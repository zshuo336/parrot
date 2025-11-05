//! Durable discovery intent and completion, bound to an index snapshot lineage.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

const MAX_LOG: u64 = 256 * 1024 * 1024;
const MAX_RECORD: u64 = 2 * 1024 * 1024;
const MAX_TASKS: usize = 100_000;
const COMPACT_STEP: u64 = 4 * 1024 * 1024;
const FORMAT_MAGIC: &[u8] = b"PRWSJ2\n";
#[path = "journal_compaction.rs"]
mod compaction;
type ResetIntent = (Vec<(String, u16)>, Vec<String>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub epoch: [u8; 16],
    pub generation: u64,
    // Absence identifies a pre-history journal. Omitting it during serialization
    // preserves the exact checksum of existing version-1 records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<[u8; 32]>,
}
impl Checkpoint {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != 56 {
            return Err("checkpoint reply must be exactly 56 bytes".into());
        }
        let state = Self {
            epoch: bytes[..16].try_into().unwrap(),
            generation: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            digest: Some(bytes[24..].try_into().unwrap()),
        };
        if state.generation > i64::MAX as u64 {
            return Err("invalid checkpoint generation".into());
        }
        Ok(state)
    }
    pub fn encode(&self) -> Vec<u8> {
        [
            self.epoch.as_slice(),
            &self.generation.to_le_bytes(),
            self.digest.as_ref().map_or(&[][..], |d| d.as_slice()),
        ]
        .concat()
    }
    pub fn accepts(&self, remote: Self) -> bool {
        // Chronology only; the JVM checks ancestry using the transmitted digest.
        self.epoch == remote.epoch && self.generation <= remote.generation
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Record {
    Snapshot {
        version: u32,
        checkpoint: Checkpoint,
        tasks: usize,
    },
    RestoreTasks {
        tasks: Vec<Task>,
    },
    Seal,
    Reset {
        seeds: Vec<(String, u16)>,
        scope: Vec<String>,
    },
    Begin {
        version: u32,
        checkpoint: Checkpoint,
        seeds: Vec<(String, u16)>,
        scope: Vec<String>,
    },
    AdoptHistory {
        checkpoint: Checkpoint,
    },
    Discover {
        tasks: Vec<(String, u16)>,
    },
    Complete {
        checkpoint: Checkpoint,
        urls: Vec<String>,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    previous: [u8; 32],
    record: Record,
    checksum: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    url: String,
    depth: u16,
    complete: bool,
}

pub(super) struct Journal {
    _owner: File,
    file: File,
    directory: PathBuf,
    replacing: bool,
    snapshot_remaining: Option<usize>,
    next_compaction: u64,
    digest: [u8; 32],
    bytes: u64,
    tasks: Vec<Task>,
    positions: HashMap<String, usize>,
    checkpoint: Option<Checkpoint>,
    reset: Option<ResetIntent>,
    poisoned: bool,
}
fn open_file(path: &Path) -> Result<File, String> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| !m.is_file()) {
        return Err(format!(
            "crawl journal path is not a regular file: {}",
            path.display()
        ));
    }
    OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
        .map_err(|e| e.to_string())
}

impl Journal {
    /// Keep directory ownership through remote retirement after the journal closes.
    pub fn retain_lease(&self) -> std::io::Result<File> {
        self._owner.try_clone()
    }
    pub fn lease(directory: &Path) -> Result<File, String> {
        let owner = open_file(&directory.join(".crawl.lock"))?;
        owner
            .try_lock()
            .map_err(|e| format!("crawl data directory already has an owner: {e}"))?;
        Ok(owner)
    }
    pub fn open(directory: &Path) -> Result<Self, String> {
        let owner = Self::lease(directory)?;
        let mut journal = Self {
            _owner: owner,
            file: open_file(&directory.join("crawl.journal"))?,
            directory: directory.to_owned(),
            replacing: false,
            snapshot_remaining: None,
            next_compaction: COMPACT_STEP,
            digest: [0; 32],
            bytes: 0,
            tasks: Vec::new(),
            positions: HashMap::new(),
            checkpoint: None,
            reset: None,
            poisoned: false,
        };
        if journal.file.metadata().map_err(|e| e.to_string())?.len() > MAX_LOG {
            return Err("crawl journal exceeds file budget".into());
        }
        let mut input = BufReader::new(journal.file.try_clone().map_err(|e| e.to_string())?);
        loop {
            let mut line = Vec::new();
            let n = input
                .by_ref()
                .take(MAX_RECORD + 1)
                .read_until(b'\n', &mut line)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            if n as u64 > MAX_LOG.saturating_sub(journal.bytes) {
                return Err("crawl journal exceeds file budget".into());
            }
            if n as u64 > MAX_RECORD {
                return Err("crawl journal record exceeds byte budget".into());
            }
            if journal.bytes == 0 && line == FORMAT_MAGIC {
                journal.replacing = true;
                journal.bytes += line.len() as u64;
                continue;
            }
            if journal.bytes == 0 && line.first() == Some(&b'P') {
                return Err("invalid compact journal header".into());
            }
            if line.last() != Some(&b'\n') {
                if journal.replacing {
                    return Err("incomplete compact journal; missing seal".into());
                }

                // Only a torn final append is recoverable. Complete bad records
                // are corruption and never silently become an empty queue.
                journal
                    .file
                    .set_len(journal.bytes)
                    .map_err(|e| e.to_string())?;
                journal.file.sync_all().map_err(|e| e.to_string())?;
                break;
            }
            let frame: Frame =
                serde_json::from_slice(&line).map_err(|e| format!("invalid crawl journal: {e}"))?;
            if frame.previous != journal.digest
                || frame.checksum != Self::checksum(frame.previous, &frame.record)?
            {
                return Err("crawl journal checksum/chain mismatch".into());
            }
            journal.validate(&frame.record)?;
            journal.apply(&frame.record);
            journal.digest = frame.checksum;
            journal.bytes += n as u64;
            if matches!(frame.record, Record::Seal) {
                journal.next_compaction = journal.bytes.saturating_add(COMPACT_STEP);
            }
        }
        if journal.replacing {
            return Err("incomplete compact journal; missing seal".into());
        }
        journal.discard_stale_compaction()?;
        File::open(directory)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("sync crawl directory: {e}"))?;
        Ok(journal)
    }
    fn checksum(previous: [u8; 32], record: &Record) -> Result<[u8; 32], String> {
        let mut hash = Sha256::new();
        hash.update(previous);
        hash.update(serde_json::to_vec(record).map_err(|e| e.to_string())?);
        Ok(hash.finalize().into())
    }
    fn validate_tasks(&self, tasks: &[(String, u16)], fresh: bool) -> Result<(), String> {
        if tasks.len() > MAX_TASKS.saturating_sub(if fresh { self.tasks.len() } else { 0 }) {
            return Err("crawl task budget exhausted".into());
        }
        let mut urls = HashSet::new();
        for (url, _) in tasks {
            if super::robots::crawl_url(url)?.as_str() != url
                || !urls.insert(url)
                || (fresh && self.positions.contains_key(url))
            {
                return Err("invalid or duplicate journal task".into());
            }
        }
        Ok(())
    }
    fn validate(&self, record: &Record) -> Result<(), String> {
        if self.replacing
            && self.bytes != FORMAT_MAGIC.len() as u64
            && !matches!(record, Record::RestoreTasks { .. } | Record::Seal)
        {
            return Err("unexpected record inside compact journal".into());
        }
        match record {
            Record::Snapshot {
                version,
                checkpoint,
                tasks,
            } => {
                if !self.replacing
                    || self.bytes != FORMAT_MAGIC.len() as u64
                    || *version != 2
                    || *tasks > MAX_TASKS
                    || checkpoint.generation > i64::MAX as u64
                {
                    return Err("invalid compact journal snapshot".into());
                }
                Ok(())
            }
            Record::RestoreTasks { tasks } => {
                if !self.replacing
                    || self.snapshot_remaining.is_none()
                    || tasks.is_empty()
                    || tasks.len() > self.snapshot_remaining.unwrap()
                {
                    return Err("invalid compact journal task chunk".into());
                }
                self.validate_tasks(
                    &tasks
                        .iter()
                        .map(|t| (t.url.clone(), t.depth))
                        .collect::<Vec<_>>(),
                    true,
                )
            }
            Record::Seal => {
                if !self.replacing
                    || self.snapshot_remaining.is_some_and(|n| n != 0)
                    || (self.checkpoint.is_none() && self.reset.is_none())
                {
                    return Err("invalid compact journal seal".into());
                }
                Ok(())
            }
            Record::Reset { seeds, .. } => self.validate_tasks(seeds, false),
            Record::Begin {
                version,
                checkpoint,
                seeds,
                ..
            } => {
                if *version != 1
                    || (self.checkpoint.is_some() && self.reset.is_none())
                    || checkpoint.generation > i64::MAX as u64
                {
                    return Err("invalid journal begin".into());
                }
                self.validate_tasks(seeds, false)
            }
            Record::AdoptHistory { checkpoint } => {
                let old = self.checkpoint.ok_or("missing journal checkpoint")?;
                if old.digest.is_some()
                    || checkpoint.digest.is_none()
                    || !old.accepts(*checkpoint)
                    || checkpoint.generation > i64::MAX as u64
                    || self.reset.is_some()
                {
                    return Err("invalid crawl history adoption".into());
                }
                Ok(())
            }
            Record::Discover { tasks } => {
                if self.checkpoint.is_none() || self.reset.is_some() {
                    return Err("journal is not initialized".into());
                }
                self.validate_tasks(tasks, true)
            }
            Record::Complete { checkpoint, urls } => {
                let old = self.checkpoint.ok_or("missing journal checkpoint")?;
                if checkpoint.generation > i64::MAX as u64
                    || !old.accepts(*checkpoint)
                    || (old.digest.is_some() && checkpoint.digest.is_none())
                    || old.generation == checkpoint.generation
                    || self.reset.is_some()
                {
                    return Err("invalid journal completion checkpoint".into());
                }
                let mut seen = HashSet::new();
                for url in urls {
                    let index = *self
                        .positions
                        .get(url)
                        .ok_or("completion for undiscovered URL")?;
                    if self.tasks[index].complete || !seen.insert(url) {
                        return Err("duplicate journal completion".into());
                    }
                }
                Ok(())
            }
        }
    }
    fn apply(&mut self, record: &Record) {
        match record {
            Record::Snapshot {
                checkpoint, tasks, ..
            } => {
                self.checkpoint = Some(*checkpoint);
                self.snapshot_remaining = Some(*tasks);
            }
            Record::RestoreTasks { tasks } => {
                for task in tasks {
                    self.positions.insert(task.url.clone(), self.tasks.len());
                    self.tasks.push(task.clone());
                }
                *self.snapshot_remaining.as_mut().unwrap() -= tasks.len();
            }
            Record::Seal => {
                self.replacing = false;
                self.snapshot_remaining = None;
            }
            Record::Reset { seeds, scope } => {
                self.reset = Some((seeds.clone(), scope.clone()));
            }
            Record::Begin {
                checkpoint, seeds, ..
            } => {
                self.tasks.clear();
                self.positions.clear();
                self.checkpoint = Some(*checkpoint);
                self.reset = None;
                self.add(seeds);
            }
            Record::AdoptHistory { checkpoint } => {
                // Old completions have no history proof. Keep URL/depth/order,
                // but conservatively replay them before certifying completion.
                for task in &mut self.tasks {
                    task.complete = false;
                }
                self.checkpoint = Some(*checkpoint);
            }
            Record::Discover { tasks } => self.add(tasks),
            Record::Complete { checkpoint, urls } => {
                for url in urls {
                    self.tasks[*self.positions.get(url).unwrap()].complete = true;
                }
                self.checkpoint = Some(*checkpoint);
            }
        }
    }
    fn add(&mut self, tasks: &[(String, u16)]) {
        for (url, depth) in tasks {
            self.positions.insert(url.clone(), self.tasks.len());
            self.tasks.push(Task {
                url: url.clone(),
                depth: *depth,
                complete: false,
            });
        }
    }
    fn append(&mut self, record: Record) -> Result<(), String> {
        if self.poisoned {
            return Err("crawl journal failed; reopen before further writes".into());
        }
        self.validate(&record)?;
        // New epochs and durable reset intents supersede the old journal in one
        // publication. They must remain possible even when append space is gone.
        if matches!(record, Record::Reset { .. } | Record::Begin { .. }) {
            return self.publish_compaction(Some(&record));
        }
        let (mut digest, mut bytes) = Self::encode(self.digest, &record)?;
        if bytes.len() as u64 > MAX_RECORD {
            return Err("crawl journal byte budget exhausted".into());
        }
        if self.bytes.saturating_add(bytes.len() as u64) > self.next_compaction
            || bytes.len() as u64 > MAX_LOG.saturating_sub(self.bytes)
        {
            self.publish_compaction(None)?;
            (digest, bytes) = Self::encode(self.digest, &record)?;
        }
        // The new chain changes decimal JSON hash lengths as well as checksum.
        // Recheck both budgets before publishing the reencoded append.
        if bytes.len() as u64 > MAX_RECORD
            || bytes.len() as u64 > MAX_LOG.saturating_sub(self.bytes)
        {
            return Err("crawl journal byte budget exhausted".into());
        }
        self.poisoned = true;
        self.file
            .write_all(&bytes)
            .and_then(|_| self.file.sync_all())
            .map_err(|e| format!("persist crawl journal: {e}"))?;
        self.apply(&record);
        self.digest = digest;
        self.bytes += bytes.len() as u64;
        self.poisoned = false;
        Ok(())
    }
    pub fn checkpoint(&self) -> Option<Checkpoint> {
        self.checkpoint
    }
    /// A durable Reset must be representable as Begin after remote Clear.
    /// Reserve the largest valid checkpoint and checksum before destructive work.
    pub fn check_reset_budget(seeds: &[(String, u16)], scope: &[String]) -> Result<(), String> {
        let frame = Frame {
            previous: [0; 32],
            record: Record::Begin {
                version: 1,
                checkpoint: Checkpoint {
                    epoch: [255; 16],
                    generation: i64::MAX as u64,
                    digest: Some([255; 32]),
                },
                seeds: seeds.to_vec(),
                scope: scope.to_vec(),
            },
            checksum: [255; 32],
        };
        if serde_json::to_vec(&frame).map_err(|e| e.to_string())?.len() as u64 + 1 > MAX_RECORD {
            return Err(
                "crawl reset byte budget cannot represent the next Begin; index was not cleared"
                    .into(),
            );
        }
        Ok(())
    }
    pub fn reset_intent(
        &mut self,
        seeds: Vec<(String, u16)>,
        scope: Vec<String>,
    ) -> Result<(), String> {
        Self::check_reset_budget(&seeds, &scope)?;
        self.append(Record::Reset { seeds, scope })
    }
    pub fn interrupted_reset(&self) -> Option<ResetIntent> {
        self.reset.clone()
    }
    pub fn begin(
        &mut self,
        checkpoint: Checkpoint,
        seeds: Vec<(String, u16)>,
        scope: Vec<String>,
    ) -> Result<(), String> {
        self.append(Record::Begin {
            version: 1,
            checkpoint,
            seeds,
            scope,
        })
    }
    pub fn discover(&mut self, tasks: Vec<(String, u16)>) -> Result<Vec<(String, u16)>, String> {
        let mut seen = HashSet::new();
        let tasks: Vec<_> = tasks
            .into_iter()
            .filter(|(u, _)| !self.positions.contains_key(u) && seen.insert(u.clone()))
            .collect();
        if !tasks.is_empty() {
            self.append(Record::Discover {
                tasks: tasks.clone(),
            })?;
        }
        Ok(tasks)
    }
    pub fn complete(&mut self, checkpoint: Checkpoint, urls: Vec<String>) -> Result<(), String> {
        self.append(Record::Complete { checkpoint, urls })
    }
    pub fn adopt_history(&mut self, checkpoint: Checkpoint) -> Result<(), String> {
        self.append(Record::AdoptHistory { checkpoint })
    }
    pub fn pending(&self) -> Vec<(String, u16)> {
        self.tasks
            .iter()
            .filter(|t| !t.complete)
            .map(|t| (t.url.clone(), t.depth))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    // A spawned child can briefly inherit another test's flock descriptor before
    // exec closes it. Keep process-boundary probes exclusive against file-lock
    // tests, so dropping a Journal can be checked without an unrelated child
    // temporarily retaining its lease. This never retries product lock failures.
    static PROCESS_BOUNDARY: std::sync::RwLock<()> = std::sync::RwLock::new(());
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    pub(super) fn publication_stage(directory: &Path, stage: &str) {
        if std::env::var_os("PARROT_JOURNAL_TEST_DIR").as_deref() == Some(directory.as_os_str())
            && std::env::var("PARROT_JOURNAL_TEST_EXIT_STAGE").as_deref() == Ok(stage)
        {
            // Test subprocess exits without destructors at the actual filesystem boundary.
            std::process::exit(86);
        }
    }
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "ws-journal-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
        fn bytes(&self) -> Vec<u8> {
            std::fs::read(self.0.join("crawl.journal")).unwrap()
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn state(n: u64) -> Checkpoint {
        Checkpoint {
            epoch: [7; 16],
            generation: n,
            digest: Some([n as u8; 32]),
        }
    }
    fn task(name: &str, depth: u16) -> (String, u16) {
        (format!("https://example.test/{name}"), depth)
    }
    fn start(d: &Directory) -> Journal {
        let mut j = Journal::open(&d.0).unwrap();
        j.begin(state(2), vec![], vec![]).unwrap();
        j
    }

    #[test]
    fn discovery_intent_recovers_depth_and_fifo_before_any_remote_ack() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut j = start(&d);
        assert_eq!(
            j.discover(vec![task("z", 2), task("a", 0), task("z", 9)])
                .unwrap(),
            vec![task("z", 2), task("a", 0)]
        );
        drop(j);
        let mut j = Journal::open(&d.0).unwrap();
        assert_eq!(j.pending(), vec![task("z", 2), task("a", 0)]);
        assert!(j.discover(vec![task("a", 4)]).unwrap().is_empty());
    }
    #[test]
    fn completion_needs_a_new_snapshot_of_the_same_epoch_and_known_unique_tasks() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut j = start(&d);
        j.discover(vec![task("a", 1), task("b", 2)]).unwrap();
        let original = d.bytes();
        for (s, urls) in [
            (state(2), vec![task("a", 0).0]),
            (
                Checkpoint {
                    epoch: [8; 16],
                    generation: 3,
                    digest: Some([3; 32]),
                },
                vec![task("a", 0).0],
            ),
            (state(3), vec![task("unknown", 0).0]),
            (state(3), vec![task("a", 0).0, task("a", 0).0]),
        ] {
            assert!(j.complete(s, urls).is_err());
            assert_eq!(d.bytes(), original);
        }
        j.complete(state(4), vec![task("a", 0).0]).unwrap();
        drop(j);
        let j = Journal::open(&d.0).unwrap();
        assert_eq!(j.pending(), vec![task("b", 2)]);
        assert_eq!(j.checkpoint(), Some(state(4)));
        assert!(j.checkpoint().unwrap().accepts(state(5)));
        assert!(!j.checkpoint().unwrap().accepts(state(3)));
    }
    #[test]
    fn index_commit_without_local_completion_replays_instead_of_skipping() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut j = start(&d);
        j.discover(vec![task("a", 4)]).unwrap();
        drop(j);
        let j = Journal::open(&d.0).unwrap();
        assert!(j.checkpoint().unwrap().accepts(state(3)));
        assert_eq!(j.pending(), vec![task("a", 4)]);
    }
    #[test]
    fn every_torn_final_record_recovers_only_the_preceding_complete_records() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let source = Directory::new();
        let mut j = start(&source);
        let prefix = source.bytes();
        j.discover(vec![task("a", 3)]).unwrap();
        let full = source.bytes();
        drop(j);
        for n in prefix.len()..full.len() {
            let d = Directory::new();
            std::fs::write(d.0.join("crawl.journal"), &full[..n]).unwrap();
            let mut j = Journal::open(&d.0).unwrap();
            assert!(j.pending().is_empty(), "prefix {n}");
            assert_eq!(d.bytes(), prefix);
            j.discover(vec![task("b", 2)]).unwrap();
            drop(j);
            assert_eq!(Journal::open(&d.0).unwrap().pending(), vec![task("b", 2)]);
        }
    }
    #[test]
    fn complete_corruption_reordering_and_duplicate_records_are_rejected() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let source = Directory::new();
        let mut j = start(&source);
        j.discover(vec![task("a", 3)]).unwrap();
        j.discover(vec![task("b", 4)]).unwrap();
        drop(j);
        let full = source.bytes();
        let lines: Vec<_> = full.split_inclusive(|b| *b == b'\n').collect();
        let mut changed = full.clone();
        let pos = changed.windows(4).position(|w| w == b"test").unwrap();
        changed[pos] = b'X';
        for bytes in [
            changed,
            [lines[0], lines[2], lines[1]].concat(),
            [full.as_slice(), lines[2]].concat(),
            [full.as_slice(), b"bad\n"].concat(),
        ] {
            let d = Directory::new();
            std::fs::write(d.0.join("crawl.journal"), &bytes).unwrap();
            assert!(Journal::open(&d.0).is_err());
            assert_eq!(d.bytes(), bytes);
        }
    }
    #[test]
    fn reset_intent_is_durable_and_new_begin_replaces_old_task_generation() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut j = start(&d);
        j.discover(vec![task("old", 5)]).unwrap();
        j.reset_intent(vec![task("new", 0)], vec!["https://example.test".into()])
            .unwrap();
        drop(j);
        let mut j = Journal::open(&d.0).unwrap();
        let (tasks, scope) = j.interrupted_reset().unwrap();
        assert_eq!(tasks, vec![task("new", 0)]);
        assert!(j.discover(vec![task("wrong", 0)]).is_err());
        j.begin(
            Checkpoint {
                epoch: [8; 16],
                generation: 9,
                digest: Some([9; 32]),
            },
            tasks,
            scope,
        )
        .unwrap();
        drop(j);
        let j = Journal::open(&d.0).unwrap();
        assert!(j.interrupted_reset().is_none());
        assert_eq!(j.pending(), vec![task("new", 0)]);
        assert_eq!(j.checkpoint().unwrap().epoch, [8; 16]);
    }
    #[test]
    fn directory_owner_is_exclusive_and_release_allows_recovery() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let j = start(&d);
        assert!(Journal::open(&d.0).is_err());
        drop(j);
        assert!(Journal::open(&d.0).is_ok());
    }
    #[test]
    fn failed_append_poisons_owner_without_advancing_memory_and_reopen_recovers() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut j = start(&d);
        j.file = File::open(d.0.join("crawl.journal")).unwrap();
        assert!(j
            .discover(vec![task("a", 0)])
            .unwrap_err()
            .contains("persist crawl journal"));
        assert!(j.pending().is_empty());
        assert!(j
            .discover(vec![task("b", 0)])
            .unwrap_err()
            .contains("reopen"));
        drop(j);
        assert!(Journal::open(&d.0).unwrap().pending().is_empty());
    }
    #[test]
    fn file_budget_and_wire_width_fail_before_unbounded_work() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let file = File::create(d.0.join("crawl.journal")).unwrap();
        file.set_len(MAX_LOG + 1).unwrap();
        assert!(Journal::open(&d.0).is_err());
        let bytes = state(3).encode();
        assert_eq!(Checkpoint::decode(&bytes).unwrap(), state(3));
        for n in 0..56 {
            assert!(Checkpoint::decode(&bytes[..n]).is_err());
        }
        assert!(Checkpoint::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
        assert!(Checkpoint::decode(
            &[&[7; 16][..], &u64::MAX.to_le_bytes()[..], &[3; 32][..]].concat()
        )
        .is_err());
    }
    #[test]
    fn legacy_journal_adoption_replays_completed_tasks_with_original_depth_and_order() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut j = Journal::open(&d.0).unwrap();
        let legacy = |n| Checkpoint {
            digest: None,
            ..state(n)
        };
        j.begin(legacy(1), vec![task("z", 4), task("a", 1)], vec![])
            .unwrap();
        j.complete(legacy(2), vec![task("z", 0).0]).unwrap();
        assert!(!String::from_utf8(d.bytes()).unwrap().contains("digest"));
        drop(j);
        let mut j = Journal::open(&d.0).unwrap();
        assert_eq!(j.pending(), vec![task("a", 1)]);
        let before = d.bytes();
        for bad in [
            state(1),
            Checkpoint {
                epoch: [8; 16],
                ..state(3)
            },
            legacy(3),
        ] {
            assert!(j.adopt_history(bad).is_err());
            assert_eq!(d.bytes(), before);
        }
        j.adopt_history(state(3)).unwrap();
        assert!(j.adopt_history(state(4)).is_err());
        assert_eq!(j.pending(), vec![task("z", 4), task("a", 1)]);
        assert!(j.complete(legacy(4), vec![task("z", 0).0]).is_err());
        drop(j);
        let mut j = Journal::open(&d.0).unwrap();
        assert_eq!(j.checkpoint(), Some(state(3)));
        assert_eq!(j.pending(), vec![task("z", 4), task("a", 1)]);
        j.complete(state(4), vec![task("z", 0).0]).unwrap();
        drop(j);
        assert_eq!(Journal::open(&d.0).unwrap().pending(), vec![task("a", 1)]);
    }

    #[test]
    fn torn_legacy_adoption_reopens_old_state_without_losing_tasks() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let source = Directory::new();
        let mut j = Journal::open(&source.0).unwrap();
        j.begin(
            Checkpoint {
                digest: None,
                ..state(2)
            },
            vec![task("a", 3)],
            vec![],
        )
        .unwrap();
        let prefix = source.bytes();
        j.adopt_history(state(4)).unwrap();
        let full = source.bytes();
        drop(j);
        for n in prefix.len()..full.len() {
            let d = Directory::new();
            std::fs::write(d.0.join("crawl.journal"), &full[..n]).unwrap();
            let mut j = Journal::open(&d.0).unwrap();
            assert!(j.checkpoint().unwrap().digest.is_none());
            assert_eq!(j.pending(), vec![task("a", 3)]);
            j.adopt_history(state(5)).unwrap();
            assert_eq!(j.pending(), vec![task("a", 3)]);
        }
    }
    #[test]
    fn compaction_retains_order_depth_completions_lineage_and_directory_lease() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut j = start(&d);
        let tasks: Vec<_> = (0..150)
            .map(|i| task(&format!("task-{i}"), i % 9))
            .collect();
        j.discover(tasks.clone()).unwrap();
        j.complete(
            state(3),
            tasks.iter().step_by(3).map(|t| t.0.clone()).collect(),
        )
        .unwrap();
        let before = j.tasks.clone();
        let lease = j.retain_lease().unwrap();
        j.publish_compaction(None).unwrap();
        assert!(Journal::lease(&d.0).is_err());
        let bytes = d.bytes();
        assert!(bytes.starts_with(FORMAT_MAGIC));
        drop(j);
        assert!(Journal::open(&d.0).is_err());
        drop(lease);
        let mut j = Journal::open(&d.0).unwrap();
        assert_eq!(j.tasks, before);
        assert_eq!(j.checkpoint(), Some(state(3)));
        assert!(j.discover(tasks).unwrap().is_empty());
        assert!(j.complete(state(4), vec![task("task-0", 0).0]).is_err());
        j.complete(state(4), vec![task("task-1", 0).0]).unwrap();
        assert!(!j.pending().iter().any(|(u, _)| u.ends_with("task-1")));
    }

    #[test]
    fn periodic_compaction_reclaims_real_legacy_metadata_without_reducing_budgets() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut file = File::create(d.0.join("crawl.journal")).unwrap();
        let mut previous = [0; 32];
        let mut total = 0;
        let mut emit = |record| {
            let (digest, bytes) = Journal::encode(previous, &record).unwrap();
            file.write_all(&bytes).unwrap();
            previous = digest;
            total += bytes.len() as u64;
            total
        };
        emit(Record::Begin {
            version: 1,
            checkpoint: state(1),
            seeds: vec![task("pending", 7), task("done", 2)],
            scope: vec![],
        });
        emit(Record::Complete {
            checkpoint: state(2),
            urls: vec![task("done", 0).0],
        });
        let mut generation = 3;
        loop {
            if emit(Record::Complete {
                checkpoint: state(generation),
                urls: vec![],
            }) > COMPACT_STEP
            {
                break;
            }
            generation += 1;
        }
        file.sync_all().unwrap();
        drop(file);
        let mut j = Journal::open(&d.0).unwrap();
        j.complete(state(generation + 1), vec![]).unwrap();
        assert!(j.bytes < 4096);
        assert_eq!(j.pending(), vec![task("pending", 7)]);
        assert!(j.discover(vec![task("done", 2)]).unwrap().is_empty());
        drop(j);
        let j = Journal::open(&d.0).unwrap();
        assert_eq!(j.pending(), vec![task("pending", 7)]);
        assert_eq!(j.checkpoint(), Some(state(generation + 1)));
    }

    #[test]
    fn interrupted_compaction_never_publishes_a_partial_state() {
        let _process_boundary = PROCESS_BOUNDARY.write().unwrap();
        const NAME: &str =
            "crawl_journal::tests::interrupted_compaction_never_publishes_a_partial_state";
        if let Some(path) = std::env::var_os("PARROT_JOURNAL_TEST_DIR") {
            let mut j = Journal::open(Path::new(&path)).unwrap();
            j.publish_compaction(None).unwrap();
            panic!("publication exit stage was not reached");
        }
        for stage in [
            "header",
            "snapshot",
            "synced",
            "renamed",
            "directory-synced",
        ] {
            let d = Directory::new();
            let mut j = start(&d);
            j.discover(vec![task("done", 1), task("pending", 6)])
                .unwrap();
            j.complete(state(3), vec![task("done", 0).0]).unwrap();
            drop(j);
            let before = d.bytes();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env("PARROT_JOURNAL_TEST_DIR", &d.0)
                .env("PARROT_JOURNAL_TEST_EXIT_STAGE", stage)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(86),
                "{stage}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if matches!(stage, "header" | "snapshot" | "synced") {
                assert_eq!(d.bytes(), before);
            }
            let mut j = Journal::open(&d.0).unwrap();
            assert_eq!(j.pending(), vec![task("pending", 6)]);
            assert_eq!(j.checkpoint(), Some(state(3)));
            assert!(j.discover(vec![task("done", 1)]).unwrap().is_empty());
            assert!(!d.0.join(".crawl.journal.next").exists());
            j.publish_compaction(None).unwrap();
            j.discover(vec![task("later", 4)]).unwrap();
            drop(j);
            assert_eq!(
                Journal::open(&d.0).unwrap().pending(),
                vec![task("pending", 6), task("later", 4)]
            );
        }
    }

    #[test]
    fn unsealed_snapshot_or_partial_header_is_corruption_not_an_empty_queue() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let source = Directory::new();
        let mut j = start(&source);
        j.discover(vec![task("a", 5), task("b", 7)]).unwrap();
        j.publish_compaction(None).unwrap();
        drop(j);
        let full = source.bytes();
        let lines: Vec<_> = full.split_inclusive(|b| *b == b'\n').collect();
        let mut cuts: Vec<_> = (1..=FORMAT_MAGIC.len()).collect();
        let mut end = FORMAT_MAGIC.len();
        for line in &lines[1..] {
            cuts.push(end + line.len() / 2);
            end += line.len();
            if end < full.len() {
                cuts.push(end);
            }
        }
        for n in cuts {
            let d = Directory::new();
            std::fs::write(d.0.join("crawl.journal"), &full[..n]).unwrap();
            assert!(
                Journal::open(&d.0).is_err(),
                "accepted incomplete compact snapshot prefix {n}"
            );
            assert_eq!(d.bytes(), full[..n]);
        }
    }

    #[test]
    fn valid_checksums_do_not_excuse_invalid_snapshot_structure() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let entry = Task {
            url: task("a", 2).0,
            depth: 2,
            complete: true,
        };
        let header = |tasks| Record::Snapshot {
            version: 2,
            checkpoint: state(3),
            tasks,
        };
        for records in [
            vec![header(1), Record::Seal],
            vec![
                header(0),
                Record::RestoreTasks {
                    tasks: vec![entry.clone()],
                },
                Record::Seal,
            ],
            vec![
                header(2),
                Record::RestoreTasks {
                    tasks: vec![entry.clone(), entry.clone()],
                },
                Record::Seal,
            ],
            vec![
                header(0),
                Record::RestoreTasks { tasks: vec![] },
                Record::Seal,
            ],
            vec![Record::Seal],
            vec![header(MAX_TASKS + 1), Record::Seal],
            vec![
                header(0),
                Record::Seal,
                Record::RestoreTasks { tasks: vec![entry] },
            ],
            vec![
                Record::Snapshot {
                    version: 3,
                    checkpoint: state(3),
                    tasks: 0,
                },
                Record::Seal,
            ],
        ] {
            let d = Directory::new();
            let mut bytes = FORMAT_MAGIC.to_vec();
            let mut previous = [0; 32];
            for record in records {
                let (digest, encoded) = Journal::encode(previous, &record).unwrap();
                bytes.extend(encoded);
                previous = digest;
            }
            std::fs::write(d.0.join("crawl.journal"), &bytes).unwrap();
            assert!(Journal::open(&d.0).is_err());
            assert_eq!(d.bytes(), bytes);
        }
    }

    #[test]
    fn failed_publication_preserves_original_and_requires_reopen() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut j = start(&d);
        j.discover(vec![task("old", 4)]).unwrap();
        let before = d.bytes();
        std::fs::create_dir(d.0.join(".crawl.journal.next")).unwrap();
        assert!(j
            .publish_compaction(None)
            .unwrap_err()
            .contains("create compact journal"));
        assert_eq!(d.bytes(), before);
        assert!(j
            .discover(vec![task("later", 2)])
            .unwrap_err()
            .contains("reopen"));
        drop(j);
        std::fs::remove_dir(d.0.join(".crawl.journal.next")).unwrap();
        let j = Journal::open(&d.0).unwrap();
        assert_eq!(j.pending(), vec![task("old", 4)]);
    }

    #[test]
    fn compact_legacy_completion_still_requires_conservative_history_adoption() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut j = Journal::open(&d.0).unwrap();
        j.begin(
            Checkpoint {
                digest: None,
                ..state(1)
            },
            vec![task("old", 5), task("new", 2)],
            vec![],
        )
        .unwrap();
        j.complete(
            Checkpoint {
                digest: None,
                ..state(2)
            },
            vec![task("old", 0).0],
        )
        .unwrap();
        j.publish_compaction(None).unwrap();
        drop(j);
        let mut j = Journal::open(&d.0).unwrap();
        assert_eq!(j.pending(), vec![task("new", 2)]);
        j.adopt_history(state(3)).unwrap();
        j.publish_compaction(None).unwrap();
        drop(j);
        assert_eq!(
            Journal::open(&d.0).unwrap().pending(),
            vec![task("old", 5), task("new", 2)]
        );
    }

    #[test]
    fn reset_and_begin_interruption_keep_an_entire_old_or_new_transaction() {
        let _process_boundary = PROCESS_BOUNDARY.write().unwrap();
        const NAME: &str = "crawl_journal::tests::reset_and_begin_interruption_keep_an_entire_old_or_new_transaction";
        let seeds = || vec![task("new", 0)];
        let scope = || vec!["https://example.test".into()];
        let new_checkpoint = Checkpoint {
            epoch: [8; 16],
            ..state(9)
        };
        if let Some(path) = std::env::var_os("PARROT_JOURNAL_TEST_DIR") {
            let mut j = Journal::open(Path::new(&path)).unwrap();
            if std::env::var("PARROT_JOURNAL_TEST_ACTION").as_deref() == Ok("begin") {
                j.begin(new_checkpoint, seeds(), scope()).unwrap();
            } else {
                j.reset_intent(seeds(), scope()).unwrap();
            }
            panic!("replacement exit stage was not reached");
        }
        for action in ["reset", "begin"] {
            for stage in ["header", "synced", "renamed", "directory-synced"] {
                let d = Directory::new();
                let mut j = start(&d);
                j.discover(vec![task("old", 5)]).unwrap();
                if action == "begin" {
                    j.reset_intent(seeds(), scope()).unwrap();
                }
                let old_pending = j.pending();
                let old_checkpoint = j.checkpoint();
                let old_reset = j.interrupted_reset();
                drop(j);
                let before = d.bytes();
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", NAME, "--nocapture"])
                    .env("PARROT_JOURNAL_TEST_DIR", &d.0)
                    .env("PARROT_JOURNAL_TEST_ACTION", action)
                    .env("PARROT_JOURNAL_TEST_EXIT_STAGE", stage)
                    .output()
                    .unwrap();
                assert_eq!(output.status.code(), Some(86));
                let j = Journal::open(&d.0).unwrap();
                if matches!(stage, "header" | "synced") {
                    assert_eq!(d.bytes(), before);
                    assert_eq!(j.pending(), old_pending);
                    assert_eq!(j.checkpoint(), old_checkpoint);
                    assert_eq!(j.interrupted_reset(), old_reset);
                } else if action == "reset" {
                    assert_eq!(j.interrupted_reset(), Some((seeds(), scope())));
                    assert!(j.pending().is_empty());
                    assert!(j.checkpoint().is_none());
                } else {
                    assert_eq!(j.pending(), seeds());
                    assert_eq!(j.checkpoint(), Some(new_checkpoint));
                    assert!(j.interrupted_reset().is_none());
                }
            }
        }
    }

    #[test]
    fn oversized_reset_does_not_replace_or_poison_the_current_generation() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut j = start(&d);
        j.discover(vec![task("retained", 3)]).unwrap();
        let before = d.bytes();
        let seeds = (0..512)
            .map(|n| task(&format!("{n}/{}", "x".repeat(8000)), 0))
            .collect();
        assert!(j
            .reset_intent(seeds, vec![])
            .unwrap_err()
            .contains("byte budget"));
        assert_eq!(d.bytes(), before);
        assert!(!d.0.join(".crawl.journal.next").exists());
        j.discover(vec![task("later", 1)]).unwrap();
        drop(j);
        assert_eq!(
            Journal::open(&d.0).unwrap().pending(),
            vec![task("retained", 3), task("later", 1)]
        );
    }
    #[test]
    fn reset_admission_reserves_checkpoint_space_before_destructive_clear() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let mut j = start(&d);
        let before = d.bytes();
        let mut seeds: Vec<_> = (0..257)
            .map(|n| task(&format!("{n}/{}", "x".repeat(8100)), 0))
            .collect();
        seeds.push(task("tail", 0));
        let base = serde_json::to_vec(&Frame {
            previous: [0; 32],
            record: Record::Reset {
                seeds: seeds.clone(),
                scope: vec![],
            },
            checksum: [255; 32],
        })
        .unwrap()
        .len()
            + 1;
        let padding = MAX_RECORD as usize - base - 16;
        assert!(padding + seeds.last().unwrap().0.len() <= 8192);
        seeds.last_mut().unwrap().0.push_str(&"x".repeat(padding));
        assert!(
            Journal::encode(
                [0; 32],
                &Record::Reset {
                    seeds: seeds.clone(),
                    scope: vec![]
                }
            )
            .unwrap()
            .1
            .len() as u64
                <= MAX_RECORD
        );
        assert!(j
            .reset_intent(seeds, vec![])
            .unwrap_err()
            .contains("next Begin"));
        assert_eq!(d.bytes(), before);
        assert!(!j.poisoned);
    }
    #[test]
    fn reencoded_append_cannot_exceed_the_reader_record_budget() {
        let _process_boundary = PROCESS_BOUNDARY.read().unwrap();
        let d = Directory::new();
        let snapshot = Record::Snapshot {
            version: 2,
            checkpoint: state(2),
            tasks: 0,
        };
        let compact_digest = Journal::encode(
            Journal::encode([0; 32], &snapshot).unwrap().0,
            &Record::Seal,
        )
        .unwrap()
        .0;
        let digits = |digest: [u8; 32]| serde_json::to_vec(&digest).unwrap().len();
        let reset = Record::Reset {
            seeds: vec![],
            scope: vec![],
        };
        let mut previous = [0; 32];
        let mut file = File::create(d.0.join("crawl.journal")).unwrap();
        // Legal legacy history crosses the real compaction threshold.
        for _ in 0..3 {
            let (digest, mut bytes) = Journal::encode(previous, &reset).unwrap();
            bytes.pop();
            bytes.resize(MAX_RECORD as usize - 1, b' ');
            bytes.push(b'\n');
            file.write_all(&bytes).unwrap();
            previous = digest;
        }
        let (old_digest, begin) = (0..10000)
            .find_map(|n| {
                let record = Record::Begin {
                    version: 1,
                    checkpoint: state(2),
                    seeds: vec![],
                    scope: vec![format!("https://example.test/{n}")],
                };
                let (digest, bytes) = Journal::encode(previous, &record).unwrap();
                (digits(compact_digest) >= digits(digest) + 8).then_some((digest, bytes))
            })
            .expect("fixture needs a longer compact chain hash encoding");
        file.write_all(&begin).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let mut tasks: Vec<_> = (0..257)
            .map(|n| task(&format!("{n}/{}", "x".repeat(8100)), 0))
            .collect();
        tasks.push(task("tail", 0));
        let base = Journal::encode(
            old_digest,
            &Record::Discover {
                tasks: tasks.clone(),
            },
        )
        .unwrap()
        .1
        .len();
        let padding = MAX_RECORD as usize - base - 20;
        tasks.last_mut().unwrap().0.push_str(&"x".repeat(padding));
        let tasks = (0..80)
            .find_map(|_| {
                tasks.last_mut().unwrap().0.push('x');
                let record = Record::Discover {
                    tasks: tasks.clone(),
                };
                let old_len = Journal::encode(old_digest, &record).unwrap().1.len();
                let new_len = Journal::encode(compact_digest, &record).unwrap().1.len();
                (old_len <= MAX_RECORD as usize && new_len > MAX_RECORD as usize)
                    .then(|| tasks.clone())
            })
            .expect("fixture must cross only after reencoding");
        assert!(tasks.iter().all(|(url, _)| url.len() <= 8192));
        let mut j = Journal::open(&d.0).unwrap();
        let result = j.discover(tasks);
        assert!(
            matches!(result, Err(ref e) if e.contains("byte budget")),
            "append must reject a frame that exceeds the reader budget after compaction"
        );
        assert!(j.pending().is_empty());
        // Compaction itself may have succeeded, but no oversized append is durable.
        j.discover(vec![task("later", 1)]).unwrap();
        drop(j);
        assert_eq!(
            Journal::open(&d.0).unwrap().pending(),
            vec![task("later", 1)]
        );
    }
}
