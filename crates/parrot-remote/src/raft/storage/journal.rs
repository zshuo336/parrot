use super::*;
use serde::de::DeserializeOwned;

const MAGIC_V2: &[u8; 8] = b"PRAFT02\0";
const MAGIC_V3: &[u8; 8] = b"PRAFT03\0";
const HEADER: usize = 72;
const MAX_RECORD: usize = MAX_SNAPSHOT + 64;
const MAX_FILE: usize = 2 * MAX_SNAPSHOT + 4 * HEADER;

pub(super) struct Journal {
    length: usize,
    checkpoint_length: usize,
    hash: [u8; 32],
    tail: bool,
    legacy: bool,
}
impl Journal {
    pub(super) fn initial() -> Self {
        Self {
            length: 0,
            checkpoint_length: 0,
            hash: [0; 32],
            tail: false,
            legacy: true,
        }
    }
    pub(super) fn recover(&self, path: &Path) -> Result<(), RaftStorageError> {
        if self.tail {
            let file = regular(path, Kind::Recover)?;
            file.set_len(self.length as u64).map_err(storage)?;
            file.sync_all().map_err(storage)?;
        }
        Ok(())
    }
    pub(super) fn checkpoint(
        &mut self,
        directory: &Path,
        saved: &Saved,
    ) -> Result<(), RaftStorageError> {
        match &saved.snapshot {
            Some(snapshot) => {
                self.checkpoint_value(directory, &(saved, snapshot.as_ref()), MAGIC_V3)
            }
            None => self.checkpoint_value(directory, saved, MAGIC_V2),
        }
    }
    fn checkpoint_value(
        &mut self,
        directory: &Path,
        saved: &impl serde::Serialize,
        magic: &[u8; 8],
    ) -> Result<(), RaftStorageError> {
        // Keep the logical v1 capacity, including its 40-byte envelope.
        bincode::serde::encode_into_std_write(saved, &mut Counter(40), bincode::config::standard())
            .map_err(|_| RaftStorageError::Capacity)?;
        let body = encode(saved)?;
        let (header, hash) = frame(&seed(magic), &body);
        let next = directory.join("raft.next");
        let mut file = regular(&next, Kind::New)?;
        file.write_all(magic)
            .and_then(|()| file.write_all(&header))
            .and_then(|()| file.write_all(&body))
            .and_then(|()| file.sync_all())
            .map_err(storage)?;
        std::fs::rename(next, directory.join("raft.state")).map_err(storage)?;
        File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(storage)?;
        self.length = 8 + HEADER + body.len();
        self.checkpoint_length = self.length;
        self.hash = hash;
        self.legacy = false;
        self.tail = false;
        Ok(())
    }
    pub(super) fn publish(
        &mut self,
        directory: &Path,
        saved: &Saved,
        node: &RaftNode,
        delta: &impl serde::Serialize,
    ) -> Result<(), RaftStorageError> {
        let body = encode(delta)?;
        let limit = self
            .checkpoint_length
            .saturating_mul(2)
            .clamp(64 * 1024, MAX_FILE);
        if self.legacy || self.length.saturating_add(HEADER + body.len()) > limit {
            let state = (
                saved.version,
                &saved.cluster,
                &node.id,
                &node.peers,
                node.current_term,
                &node.voted_for,
                &node.log,
                node.commit_index,
            );
            return match &saved.snapshot {
                Some(snapshot) => {
                    self.checkpoint_value(directory, &(state, snapshot.as_ref()), MAGIC_V3)
                }
                None => self.checkpoint_value(directory, &state, MAGIC_V2),
            };
        }
        let (header, hash) = frame(&self.hash, &body);
        let mut file = regular(&directory.join("raft.state"), Kind::Append)?;
        if file.metadata().map_err(storage)?.len() != self.length as u64 {
            return Err(invalid("journal length changed outside its owner"));
        }
        file.write_all(&header)
            .and_then(|()| file.write_all(&body))
            .and_then(|()| file.sync_all())
            .map_err(storage)?;
        self.length += HEADER + body.len();
        self.hash = hash;
        Ok(())
    }
}

fn seed(magic: &[u8; 8]) -> [u8; 32] {
    Sha256::digest(magic).into()
}
fn frame(previous: &[u8; 32], body: &[u8]) -> ([u8; HEADER], [u8; 32]) {
    let length = (body.len() as u64).to_le_bytes();
    let mut digest = Sha256::new();
    digest.update(previous);
    digest.update(length);
    let header_hash: [u8; 32] = digest.finalize().into();
    let mut digest = Sha256::new();
    digest.update(header_hash);
    digest.update(body);
    let hash: [u8; 32] = digest.finalize().into();
    let mut header = [0; HEADER];
    header[..8].copy_from_slice(&length);
    header[8..40].copy_from_slice(&header_hash);
    header[40..].copy_from_slice(&hash);
    (header, hash)
}
fn encode(value: &impl serde::Serialize) -> Result<Vec<u8>, RaftStorageError> {
    let body = bincode::serde::encode_to_vec(value, bincode::config::standard())
        .map_err(|error| RaftStorageError::Storage(error.to_string()))?;
    if body.len() > MAX_RECORD {
        return Err(RaftStorageError::Capacity);
    }
    Ok(body)
}
fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, RaftStorageError> {
    parrot_api::message::serde_remote_deserialize_with_limits(
        body,
        parrot_api::message::RemoteDecodeLimits {
            max_payload_bytes: MAX_RECORD,
            max_values: MAX_RECORD + MAX_ENTRIES * 8,
            ..Default::default()
        },
    )
    .map_err(RaftStorageError::Invalid)
}

// A separate chained checksum protects the length before it is used to decide
// whether a body is an interrupted append. A corrupt length must never cause
// an acknowledged record to be discarded as an incomplete tail.
struct Frame<'a> {
    body: &'a [u8],
    hash: [u8; 32],
}
fn next<'a>(
    bytes: &'a [u8],
    position: usize,
    previous: &[u8; 32],
) -> Result<Option<Frame<'a>>, RaftStorageError> {
    let suffix = &bytes[position..];
    if suffix.len() < HEADER {
        return Ok(None);
    }
    let length = u64::from_le_bytes(suffix[..8].try_into().unwrap());
    let mut digest = Sha256::new();
    digest.update(previous);
    digest.update(&suffix[..8]);
    if digest.finalize()[..] != suffix[8..40] || length > MAX_RECORD as u64 {
        return Err(invalid("corrupt journal header or chain"));
    }
    let length = length as usize;
    if suffix.len() - HEADER < length {
        return Ok(None);
    }
    let body = &suffix[HEADER..HEADER + length];
    let mut digest = Sha256::new();
    digest.update(&suffix[8..40]);
    digest.update(body);
    let hash: [u8; 32] = digest.finalize().into();
    if hash[..] != suffix[40..HEADER] {
        return Err(invalid("corrupt journal body"));
    }
    Ok(Some(Frame { body, hash }))
}

#[derive(serde::Deserialize)]
struct Delta {
    term: Term,
    vote: Option<NodeId>,
    keep: u64,
    #[serde(deserialize_with = "deserialize_log")]
    entries: Vec<LogEntry>,
    commit: LogIndex,
}

pub(super) fn deserialize_log<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<LogEntry>, D::Error> {
    struct Log;
    impl<'de> serde::de::Visitor<'de> for Log {
        type Value = Vec<LogEntry>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded persistent Raft log")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            use serde::de::Error;
            let hint = sequence.size_hint().unwrap_or(0);
            if hint > MAX_ENTRIES {
                return Err(A::Error::custom("persistent log entry capacity exceeded"));
            }
            let mut entries = Vec::with_capacity(hint);
            while let Some(entry) = sequence.next_element::<LogEntry>()? {
                if entries.len() == MAX_ENTRIES || entry.cmd.len() > MAX_COMMAND {
                    return Err(A::Error::custom("persistent log entry capacity exceeded"));
                }
                entries.push(entry);
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_seq(Log)
}

pub(super) fn read(
    path: &Path,
    cluster: &str,
    id: &str,
    peers: &[NodeId],
    validate_command: &impl Fn(&[u8]) -> Result<(), String>,
) -> Result<(Saved, Journal), RaftStorageError> {
    let mut bytes = Vec::new();
    regular(path, Kind::Read)?
        .take(MAX_FILE as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(storage)?;
    let mut journal = Journal::initial();
    let mut saved: Saved;
    if bytes.starts_with(MAGIC) {
        if bytes.len() < 40
            || bytes.len() > MAX_SNAPSHOT
            || Sha256::digest(&bytes[40..])[..] != bytes[8..40]
        {
            return Err(invalid("corrupt snapshot"));
        }
        saved = decode(&bytes[40..])?;
    } else if (bytes.starts_with(MAGIC_V2) || bytes.starts_with(MAGIC_V3))
        && bytes.len() <= MAX_FILE
    {
        let magic = if bytes.starts_with(MAGIC_V3) {
            MAGIC_V3
        } else {
            MAGIC_V2
        };
        let Frame { body, hash } = next(&bytes, 8, &seed(magic))?
            .ok_or_else(|| invalid("incomplete journal checkpoint"))?;
        if magic == MAGIC_V3 {
            let (mut state, snapshot): (Saved, RaftSnapshot) = decode(body)?;
            state.snapshot = Some(Arc::new(snapshot));
            saved = state;
        } else {
            saved = decode(body)?;
        }
        if body.len() + 40 > MAX_SNAPSHOT {
            return Err(RaftStorageError::Capacity);
        }
        journal.length = 8 + HEADER + body.len();
        journal.checkpoint_length = journal.length;
        journal.hash = hash;
        journal.legacy = false;
    } else {
        return Err(invalid("corrupt snapshot or journal magic"));
    }
    let expected_version = if saved.snapshot.is_some() { 3 } else { 1 };
    if saved.version != expected_version
        || saved.cluster != cluster
        || saved.id != id
        || saved.peers != peers
    {
        return Err(invalid("snapshot cluster/node/membership mismatch"));
    }
    validate(&saved)?;
    for entry in &saved.log {
        validate_command(&entry.cmd).map_err(RaftStorageError::Invalid)?;
    }
    if journal.legacy {
        return Ok((saved, journal));
    }
    let mut capacity = Capacity::new(&saved)?;
    while journal.length < bytes.len() {
        let Some(Frame { body, hash }) = next(&bytes, journal.length, &journal.hash)? else {
            journal.tail = true;
            break;
        };
        let delta: Delta = decode(body)?;
        let keep = usize::try_from(delta.keep).map_err(|_| invalid("journal prefix overflow"))?;
        let base = saved.base();
        let last = base
            .checked_add(delta.keep)
            .and_then(|n| n.checked_add(delta.entries.len() as u64))
            .ok_or_else(|| invalid("journal log index overflow"))?;
        if delta.term < saved.term
            || delta.commit < saved.commit
            || delta.keep < saved.commit - base
            || keep > saved.log.len()
            || delta.commit > last
            || (delta.term == saved.term && saved.vote.is_some() && delta.vote != saved.vote)
            || (delta.term == 0 && delta.vote.is_some())
            || delta
                .vote
                .as_ref()
                .is_some_and(|vote| vote != id && !peers.contains(vote))
        {
            return Err(invalid("invalid journal term, vote, commit or prefix"));
        }
        let mut term = saved.log[..keep].last().map_or_else(
            || saved.snapshot.as_ref().map_or(0, |snapshot| snapshot.term),
            |entry| entry.term,
        );
        for (offset, entry) in delta.entries.iter().enumerate() {
            if entry.index != base + (keep + offset) as u64 + 1
                || entry.term == 0
                || entry.term < term
                || entry.term > delta.term
                || saved
                    .log
                    .get(keep + offset)
                    .is_some_and(|old| old.term == entry.term && old != entry)
            {
                return Err(invalid("invalid journal log transition"));
            }
            term = entry.term;
            validate_command(&entry.cmd).map_err(RaftStorageError::Invalid)?;
        }
        let sizes = capacity.project(keep, &delta.entries)?;
        saved.term = delta.term;
        saved.vote = delta.vote;
        saved.commit = delta.commit;
        // Validate logical snapshot capacity without rescanning the retained log.
        capacity.check_saved(&saved, keep, &sizes)?;
        capacity.replace(keep, &sizes);
        saved.log.truncate(keep);
        saved.log.extend(delta.entries);
        journal.length += HEADER + body.len();
        journal.hash = hash;
    }
    Ok((saved, journal))
}
