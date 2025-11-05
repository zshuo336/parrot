//! Bounded JSONL recording and replay in strictly increasing `seq` order.
//! Correlation IDs are not clocks and are never used to reorder records.
//! Validate a complete log before invoking delivery callbacks. File appends
//! require an exclusive advisory lock and a complete existing JSONL boundary.
//! This module is not a WAL: callers must explicitly sync for durable storage.

#[path = "recording.rs"]
mod recording;
#[path = "replay_runtime.rs"]
mod runtime;
pub use recording::{Recording, RecordingReport};
pub use runtime::{ReplayDirection, ReplayOptions, ReplayReply, ReplayReport, ReplayTellMode};

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct ReplayLimits {
    pub max_file_bytes: usize,
    /// Encoded JSONL bytes per line, including a final newline when present.
    pub max_line_bytes: usize,
    pub max_records: usize,
    pub max_payload_bytes: usize,
}
impl Default for ReplayLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 64 * 1024 * 1024,
            max_line_bytes: 40 * 1024 * 1024,
            max_records: 100_000,
            max_payload_bytes: 16 * 1024 * 1024,
        }
    }
}
impl ReplayLimits {
    fn validate(self) -> Result<(), ReplayError> {
        if [
            self.max_file_bytes,
            self.max_line_bytes,
            self.max_records,
            self.max_payload_bytes,
        ]
        .iter()
        .any(|&n| n == 0 || n >= isize::MAX as usize)
            || self.max_payload_bytes > (isize::MAX as usize - 1) / 2
        {
            return Err(ReplayError::InvalidLimits);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonlRecord {
    pub seq: u64,
    pub cid: u64,
    /// Exactly `tx` or `rx`.
    pub dir: String,
    pub frame_type: u8,
    pub path: String,
    pub type_key: String,
    pub payload_hex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<parrot_remote::capture::CapturedWire>,
}
impl JsonlRecord {
    pub fn payload_bytes(&self) -> Result<Vec<u8>, ReplayError> {
        self.payload_bytes_with_limits(ReplayLimits::default())
    }
    pub fn payload_bytes_with_limits(&self, limits: ReplayLimits) -> Result<Vec<u8>, ReplayError> {
        limits.validate()?;
        validate_hex(&self.payload_hex, limits)?;
        Ok(self
            .payload_hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (nibble(pair[0]).unwrap() << 4) | nibble(pair[1]).unwrap())
            .collect())
    }
}
fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
fn validate_hex(hex: &str, limits: ReplayLimits) -> Result<(), ReplayError> {
    if hex.len() > limits.max_payload_bytes * 2 {
        return Err(limit("payload bytes"));
    }
    if !hex.len().is_multiple_of(2) {
        return Err(ReplayError::InvalidPayload("odd hex length".into()));
    }
    if let Some(index) = hex.as_bytes().iter().position(|&b| nibble(b).is_none()) {
        return Err(ReplayError::InvalidPayload(format!(
            "non-hex byte at offset {index}"
        )));
    }
    Ok(())
}
fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    #[error("replay call timeout must be positive and at most one day")]
    InvalidTimeout,
    #[error("replay seq {seq}, {completed} calls completed: {reason}")]
    Delivery {
        seq: u64,
        completed: usize,
        reason: String,
    },
    #[error("incomplete or invalid capture: {0}")]
    IncompleteCapture(String),
    #[error("io: {0}")]
    Io(String),
    #[error("jsonl line {line}: {reason}")]
    BadLine { line: usize, reason: String },
    #[error("record seq not monotonic: {prev} → {next}")]
    NonMonotonic { prev: u64, next: u64 },
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
    #[error("recording resource limit: {0}")]
    ResourceLimit(String),
    #[error("recording limits must be positive and representable")]
    InvalidLimits,
    #[error("recording writer failed; reopen only after inspecting the file")]
    Poisoned,
}
fn limit(what: &str) -> ReplayError {
    ReplayError::ResourceLimit(what.into())
}
fn io(error: impl std::fmt::Display) -> ReplayError {
    ReplayError::Io(error.to_string())
}
fn at_line(line: usize, error: ReplayError) -> ReplayError {
    match error {
        ReplayError::ResourceLimit(_) => error,
        _ => ReplayError::BadLine {
            line,
            reason: error.to_string(),
        },
    }
}
fn validate_record(record: &JsonlRecord, limits: ReplayLimits) -> Result<(), ReplayError> {
    validate_hex(&record.payload_hex, limits)?;
    if !matches!(record.dir.as_str(), "tx" | "rx") {
        return Err(ReplayError::BadLine {
            line: 0,
            reason: "direction must be tx or rx".into(),
        });
    }
    Ok(())
}
fn sequence(prev: Option<u64>, next: u64) -> Result<(), ReplayError> {
    if let Some(prev) = prev {
        if next <= prev {
            return Err(ReplayError::NonMonotonic { prev, next });
        }
    }
    Ok(())
}

// Count encoded JSON without first allocating an oversized escaped string.
struct Counter {
    bytes: usize,
    cap: usize,
}
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .checked_add(bytes.len())
            .filter(|n| *n <= self.cap)
            .ok_or_else(|| std::io::Error::other("encoded line budget"))?;
        self.bytes = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn line_len(record: &impl Serialize, limits: ReplayLimits) -> Result<usize, ReplayError> {
    let mut count = Counter {
        bytes: 1,
        cap: limits.max_line_bytes,
    }; // newline
    serde_json::to_writer(&mut count, record).map_err(|_| limit("encoded line bytes"))?;
    Ok(count.bytes)
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum CaptureLine {
    Start(CaptureStart),
    Frame(JsonlRecord),
    End(CaptureEnd),
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureStart {
    version: u8,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureEnd {
    written: usize,
    stats: parrot_remote::capture::CaptureStats,
    complete: bool,
}
fn incomplete(reason: impl Into<String>) -> ReplayError {
    ReplayError::IncompleteCapture(reason.into())
}
#[derive(Default)]
struct History {
    capture: bool,

    last: Option<u64>,
    records: usize,
    bytes: usize,
    terminated: bool,
}
fn read_history(
    reader: impl Read,
    limits: ReplayLimits,
    mut push: impl FnMut(JsonlRecord),
) -> Result<History, ReplayError> {
    limits.validate()?;
    let mut reader = std::io::BufReader::new(reader.take(limits.max_file_bytes as u64 + 1));
    let mut history = History::default();
    let mut line = Vec::new();
    let mut line_number = 0;
    let mut ended = false;
    loop {
        line.clear();
        let n = (&mut reader)
            .take(limits.max_line_bytes as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(io)?;
        if n == 0 {
            break;
        }
        line_number += 1;
        history.bytes = history
            .bytes
            .checked_add(n)
            .ok_or_else(|| limit("file bytes"))?;
        if history.bytes > limits.max_file_bytes {
            return Err(limit("file bytes"));
        }
        if n > limits.max_line_bytes {
            return Err(limit("encoded line bytes"));
        }
        history.terminated = line.ends_with(b"\n");
        let text = std::str::from_utf8(&line).map_err(|e| ReplayError::BadLine {
            line: line_number,
            reason: e.to_string(),
        })?;
        if text.trim().is_empty() {
            continue;
        }
        if ended {
            return Err(incomplete("data after capture footer"));
        }
        let record = match serde_json::from_str::<JsonlRecord>(text) {
            Ok(record) if !history.capture => record,
            Ok(_) => return Err(incomplete("legacy record in capture stream")),
            Err(legacy_error) => {
                let envelope: CaptureLine =
                    serde_json::from_str(text).map_err(|e| ReplayError::BadLine {
                        line: line_number,
                        reason: format!("{legacy_error}; capture: {e}"),
                    })?;
                match envelope {
                    CaptureLine::Start(start) => {
                        if history.capture || history.records != 0 || start.version != 1 {
                            return Err(incomplete("invalid or duplicate capture header"));
                        }
                        history.capture = true;
                        continue;
                    }
                    CaptureLine::Frame(record) => {
                        if !history.capture
                            || record.wire.is_none()
                            || record.seq != history.records as u64
                        {
                            return Err(incomplete(
                                "capture frame requires header, metadata and contiguous sequence",
                            ));
                        }
                        record
                    }
                    CaptureLine::End(end) => {
                        if !history.capture
                            || !end.complete
                            || end.stats.lost != 0
                            || end.written != history.records
                            || end.stats.queued != history.records as u64
                            || end.stats.observed != end.stats.queued
                        {
                            return Err(incomplete(
                                "missing frames, abandonment or inconsistent receipt",
                            ));
                        }
                        ended = true;
                        continue;
                    }
                }
            }
        };
        if history.records == limits.max_records {
            return Err(limit("record count"));
        }
        validate_record(&record, limits).map_err(|e| at_line(line_number, e))?;
        sequence(history.last, record.seq)?;
        history.last = Some(record.seq);
        history.records += 1;
        push(record);
    }
    if history.capture && (!ended || !history.terminated) {
        return Err(incomplete("missing or unterminated capture footer"));
    }
    Ok(history)
}
fn open_regular(
    path: &Path,
    append: bool,
    limits: ReplayLimits,
) -> Result<std::fs::File, ReplayError> {
    limits.validate()?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    if append {
        options.create(true).append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .mode(0o600);
    }
    let file = options.open(path).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(io("recording requires a regular file"));
    }
    if meta.len() > limits.max_file_bytes as u64 {
        return Err(limit("file bytes"));
    }
    Ok(file)
}

// Closing one descriptor does not release an OFD/flock lock when a concurrent
// fork inherited the same open file description. Unlock explicitly on every exit.
struct LockedFile(std::fs::File);
impl std::ops::Deref for LockedFile {
    type Target = std::fs::File;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub trait RecordSink: Send {
    fn write(&mut self, record: &JsonlRecord) -> Result<(), ReplayError>;
}

/// Single append owner. Locks are advisory: all writers must cooperate and the
/// caller must keep the file's path/inode stable. Writes reach the OS; call
/// `sync`/`finish` for a data sync. An I/O error poisons the writer, preserving
/// any incomplete tail for inspection instead of silently discarding history.
pub struct FileSink {
    file: LockedFile,
    path: PathBuf,
    history: History,
    limits: ReplayLimits,
    failed: bool,
}
impl FileSink {
    pub fn open(path: &Path) -> Result<Self, ReplayError> {
        Self::open_with_limits(path, ReplayLimits::default())
    }
    pub fn open_with_limits(path: &Path, limits: ReplayLimits) -> Result<Self, ReplayError> {
        limits.validate()?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let file = open_regular(path, true, limits)?;
        file.try_lock().map_err(io)?;
        let file = LockedFile(file);
        let history = read_history(&file.0, limits, |_| {})?;
        if history.capture {
            return Err(incomplete("cannot append to a capture interval"));
        }
        if history.bytes > 0 && !history.terminated {
            return Err(io("append requires a newline-terminated JSONL file"));
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            history,
            limits,
            failed: false,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn sync(&mut self) -> Result<(), ReplayError> {
        if self.failed {
            return Err(ReplayError::Poisoned);
        }
        self.file.sync_data().map_err(|e| {
            self.failed = true;
            io(e)
        })
    }
    pub fn finish(mut self) -> Result<PathBuf, ReplayError> {
        self.sync()?;
        self.file.unlock().map_err(io)?;
        Ok(self.path.clone())
    }
}
impl RecordSink for FileSink {
    fn write(&mut self, record: &JsonlRecord) -> Result<(), ReplayError> {
        if self.failed {
            return Err(ReplayError::Poisoned);
        }
        validate_record(record, self.limits)?;
        sequence(self.history.last, record.seq)?;
        if self.history.records == self.limits.max_records {
            return Err(limit("record count"));
        }
        let size = line_len(record, self.limits)?;
        let next_bytes = self
            .history
            .bytes
            .checked_add(size)
            .filter(|n| *n <= self.limits.max_file_bytes)
            .ok_or_else(|| limit("file bytes"))?;
        let mut line = serde_json::to_vec(record).map_err(io)?;
        line.push(b'\n');
        self.file.write_all(&line).map_err(|e| {
            self.failed = true;
            io(e)
        })?;
        self.history.last = Some(record.seq);
        self.history.records += 1;
        self.history.bytes = next_bytes;
        Ok(())
    }
}

#[derive(Default)]
pub struct MemSink {
    records: Vec<JsonlRecord>,
    limits: ReplayLimits,
    bytes: usize,
}
impl MemSink {
    pub fn with_limits(limits: ReplayLimits) -> Result<Self, ReplayError> {
        limits.validate()?;
        Ok(Self {
            limits,
            ..Self::default()
        })
    }
    pub fn records(&self) -> &[JsonlRecord] {
        &self.records
    }
    pub fn into_log(self) -> ReplayLog {
        ReplayLog {
            records: self.records,
        }
    }
}
impl RecordSink for MemSink {
    fn write(&mut self, record: &JsonlRecord) -> Result<(), ReplayError> {
        validate_record(record, self.limits)?;
        sequence(self.records.last().map(|r| r.seq), record.seq)?;
        if self.records.len() == self.limits.max_records {
            return Err(limit("record count"));
        }
        let size = line_len(record, self.limits)?;
        let next = self
            .bytes
            .checked_add(size)
            .filter(|n| *n <= self.limits.max_file_bytes)
            .ok_or_else(|| limit("file bytes"))?;
        self.records.push(record.clone());
        self.bytes = next;
        Ok(())
    }
}

/// Construct an unvalidated record. Sinks/load/replay enforce limits and validity.
pub fn record_sink(
    seq: u64,
    cid: u64,
    dir: &str,
    frame_type: u8,
    path: &str,
    type_key: &str,
    payload: &[u8],
) -> JsonlRecord {
    JsonlRecord {
        seq,
        cid,
        dir: dir.into(),
        frame_type,
        path: path.into(),
        type_key: type_key.into(),
        payload_hex: hex_encode(payload),
        wire: None,
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReplayLog {
    pub records: Vec<JsonlRecord>,
}
impl ReplayLog {
    pub fn load(path: &Path) -> Result<Self, ReplayError> {
        Self::load_with_limits(path, ReplayLimits::default())
    }
    pub fn load_with_limits(path: &Path, limits: ReplayLimits) -> Result<Self, ReplayError> {
        Self::load_inner(path, limits, false)
    }
    /// Require a complete automatic capture interval, including for an empty file.
    /// Prefer this over format autodetection when consuming Recording output.
    pub fn load_capture(path: &Path) -> Result<Self, ReplayError> {
        Self::load_capture_with_limits(path, ReplayLimits::default())
    }
    pub fn load_capture_with_limits(
        path: &Path,
        limits: ReplayLimits,
    ) -> Result<Self, ReplayError> {
        Self::load_inner(path, limits, true)
    }
    fn load_inner(
        path: &Path,
        limits: ReplayLimits,
        require_capture: bool,
    ) -> Result<Self, ReplayError> {
        let file = open_regular(path, false, limits)?;
        file.try_lock_shared().map_err(io)?;
        let file = LockedFile(file);
        let mut records = Vec::new();
        let history = read_history(&file.0, limits, |r| records.push(r))?;
        if require_capture && !history.capture {
            return Err(incomplete("capture header required"));
        }
        Ok(Self { records })
    }
    pub fn parse(text: &str) -> Result<Self, ReplayError> {
        Self::parse_with_limits(text, ReplayLimits::default())
    }
    pub fn parse_with_limits(text: &str, limits: ReplayLimits) -> Result<Self, ReplayError> {
        let mut records = Vec::new();
        read_history(text.as_bytes(), limits, |r| records.push(r))?;
        Ok(Self { records })
    }
    pub fn all(&self) -> &[JsonlRecord] {
        &self.records
    }
    pub fn by_cid(&self, cid: u64) -> Vec<&JsonlRecord> {
        self.records.iter().filter(|r| r.cid == cid).collect()
    }
    pub fn validate(&self, limits: ReplayLimits) -> Result<(), ReplayError> {
        limits.validate()?;
        if self.records.len() > limits.max_records {
            return Err(limit("record count"));
        }
        let mut bytes = 0usize;
        let mut prev = None;
        for (i, r) in self.records.iter().enumerate() {
            validate_record(r, limits).map_err(|e| at_line(i + 1, e))?;
            sequence(prev, r.seq)?;
            bytes = bytes
                .checked_add(line_len(r, limits)?)
                .filter(|n| *n <= limits.max_file_bytes)
                .ok_or_else(|| limit("file bytes"))?;
            prev = Some(r.seq);
        }
        Ok(())
    }
    /// Validate the complete input before any callback. Delivery errors stop at
    /// that record; already executed application effects are not rolled back.
    pub fn replay<F>(&self, deliver: F) -> Result<usize, ReplayError>
    where
        F: FnMut(&JsonlRecord) -> Result<(), String>,
    {
        self.replay_with_limits(ReplayLimits::default(), deliver)
    }
    pub fn replay_with_limits<F>(
        &self,
        limits: ReplayLimits,
        mut deliver: F,
    ) -> Result<usize, ReplayError>
    where
        F: FnMut(&JsonlRecord) -> Result<(), String>,
    {
        self.validate(limits)?;
        for (i, r) in self.records.iter().enumerate() {
            deliver(r).map_err(|reason| ReplayError::BadLine {
                line: i + 1,
                reason,
            })?;
        }
        Ok(self.records.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_read_budget_bounds_actual_consumption_without_metadata() {
        struct Endless {
            consumed: usize,
        }
        impl Read for Endless {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                out.fill(b' ');
                self.consumed += out.len();
                Ok(out.len())
            }
        }
        let mut reader = Endless { consumed: 0 };
        let limits = ReplayLimits {
            max_file_bytes: 32,
            max_line_bytes: 64,
            ..Default::default()
        };
        assert!(matches!(
            read_history(&mut reader, limits, |_| panic!("no record in whitespace")),
            Err(ReplayError::ResourceLimit(_))
        ));
        assert_eq!(reader.consumed, 33);
        read_history(&b"                                "[..], limits, |_| {
            panic!("whitespace")
        })
        .unwrap();
    }

    fn rec(seq: u64, cid: u64, path: &str, payload: &[u8]) -> JsonlRecord {
        record_sink(seq, cid, "tx", 0x10, path, "m::Ask", payload)
    }

    #[test]
    fn record_roundtrip_hex() {
        let r = rec(1, 7, "/a", b"\x00\x01\xff");
        assert_eq!(r.payload_hex, "0001ff");
        assert_eq!(r.payload_bytes().unwrap(), vec![0, 1, 255]);
    }

    #[test]
    fn file_sink_write_load() {
        let dir = std::env::temp_dir().join(format!("parrot-replay-{}", std::process::id()));
        let p = dir.join("run.jsonl");
        {
            let mut s = FileSink::open(&p).unwrap();
            s.write(&rec(1, 10, "/a", b"one")).unwrap();
            s.write(&rec(2, 10, "/b", b"two")).unwrap();
        }
        let log = ReplayLog::load(&p).unwrap();
        assert_eq!(log.all().len(), 2);
        assert_eq!(log.all()[0].path, "/a");
        assert_eq!(log.all()[1].payload_bytes().unwrap(), b"two".to_vec());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_rejects_non_monotonic() {
        let text = concat!(
            "{\"seq\":1,\"cid\":1,\"dir\":\"tx\",\"frame_type\":16,\"path\":\"/a\",\"type_key\":\"k\",\"payload_hex\":\"00\"}\n",
            "{\"seq\":1,\"cid\":1,\"dir\":\"tx\",\"frame_type\":16,\"path\":\"/a\",\"type_key\":\"k\",\"payload_hex\":\"00\"}\n"
        );
        assert!(matches!(
            ReplayLog::parse(text),
            Err(ReplayError::NonMonotonic { .. })
        ));
    }

    #[test]
    fn parse_bad_json_reports_line() {
        let text = "not json\n";
        match ReplayLog::parse(text) {
            Err(ReplayError::BadLine { line, .. }) => assert_eq!(line, 1),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn by_cid_filter() {
        let log = ReplayLog {
            records: vec![
                rec(1, 1, "/a", b""),
                rec(2, 2, "/b", b""),
                rec(3, 1, "/c", b""),
            ],
        };
        assert_eq!(log.by_cid(1).len(), 2);
        assert_eq!(log.by_cid(2).len(), 1);
        assert_eq!(log.by_cid(9).len(), 0);
    }

    #[test]
    fn replay_in_order_and_abort_on_err() {
        let log = ReplayLog {
            records: vec![
                rec(1, 1, "/a", b""),
                rec(2, 1, "/b", b""),
                rec(3, 1, "/c", b""),
            ],
        };
        let mut seen = Vec::new();
        let n = log
            .replay(|r| {
                seen.push(r.path.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(seen, vec!["/a", "/b", "/c"]);
        // 中止弧：第二条失败
        let r = log.replay(|r| {
            if r.seq == 2 {
                Err("boom".into())
            } else {
                Ok(())
            }
        });
        assert!(matches!(r, Err(ReplayError::BadLine { line: 2, .. })));
    }

    #[test]
    fn mem_sink_collects() {
        let mut m = MemSink::default();
        m.write(&rec(1, 1, "/x", b"")).unwrap();
        assert_eq!(m.records.len(), 1);
    }

    #[test]
    fn empty_lines_skipped() {
        let text = "\n\n";
        let log = ReplayLog::parse(text).unwrap();
        assert!(log.records.is_empty());
    }
}
