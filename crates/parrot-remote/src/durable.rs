//! E2 · durable tell（DEV_03 §3 / 06 §3.2）：云 proxy WAL。
//!
//! Bounded WAL, owned outbox, authenticated durable envelopes and atomic inbox snapshots.
//! TransactionalInbox commits its own state bytes and stream history together.
//! External business effects require a transactional/idempotent adapter. CloudProxy
//! application wiring and bounded batch submission are provided by parrot-app.
//! Outbox acceptance uses bounded group commit; cross-language durable remains open.
//!
//! WAL 裁定（06 ⚠ 复核点）：文件追加 + 内存索引，不引入嵌入式 DB。
//! 段文件 64MB 滚动；段内 `[u32 len][crc32][record]`；ACK 水位推进后旧段
//! 整段删除；恢复只修复最终不完整header/body，完整CRC损坏必须报告。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// CRC32（IEEE——与 zk/etcd 同族校验；查表实现零依赖）。
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// WAL 记录（serde bincode2 standard，按固定字段顺序编码，非自描述格式）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WalRecord {
    /// 目标端点（node_id）。
    pub endpoint: String,
    /// 每发送方独立序列（06 §3.2.1：不要全局 seq——多发送方会洞）。
    pub sender: String,
    pub seq: u64,
    /// 转发路径。
    pub path: String,
    pub type_key: String,
    pub payload: bytes::Bytes,
}

/// Resource limits shared by recovery and the live writer.
#[derive(Debug, Clone, Copy)]
pub struct WalLimits {
    pub max_record_bytes: usize,
    pub max_disk_bytes: u64,
    pub max_segments: usize,
    pub max_pending_records: usize,
    pub max_pending_bytes: usize,
    pub max_streams: usize,
    pub max_watermark_bytes: usize,
    /// Retained keyed acceptance receipts, including already ACKed messages.
    pub max_submission_receipts: usize,
    pub max_submission_receipt_bytes: usize,
}
impl Default for WalLimits {
    fn default() -> Self {
        Self {
            max_record_bytes: 16 * 1024 * 1024,
            max_disk_bytes: 1024 * 1024 * 1024,
            max_segments: 4096,
            max_pending_records: 100_000,
            max_pending_bytes: 64 * 1024 * 1024,
            max_streams: 65_536,
            max_watermark_bytes: 16 * 1024 * 1024,
            max_submission_receipts: 65_536,
            max_submission_receipt_bytes: 16 * 1024 * 1024,
        }
    }
}
impl WalLimits {
    fn validate(self, segment_max: u64) -> Result<(), String> {
        if segment_max == 0
            || self.max_disk_bytes == 0
            || self.max_disk_bytes > i64::MAX as u64
            || segment_max > self.max_disk_bytes
            || self.max_record_bytes == 0
            || self.max_record_bytes > 16 * 1024 * 1024
            || self.max_segments == 0
            || self.max_pending_records == 0
            || self.max_pending_bytes == 0
            || self.max_streams == 0
            || self.max_watermark_bytes == 0
            || self.max_submission_receipts == 0
            || self.max_submission_receipt_bytes == 0
            || [
                self.max_segments,
                self.max_pending_records,
                self.max_pending_bytes,
                self.max_streams,
                self.max_watermark_bytes,
                self.max_submission_receipts,
                self.max_submission_receipt_bytes,
            ]
            .iter()
            .any(|&v| v >= isize::MAX as usize)
        {
            return Err("wal invalid segment size or limits".into());
        }
        Ok(())
    }
}

/// One cooperative writer per directory. The directory/lock inode must remain
/// stable. Explicit sync establishes durability; Drop only releases ownership.
pub struct Wal {
    dir: PathBuf,
    segment_max: u64,
    limits: WalLimits,
    inner: Mutex<WalInner>,
    // Declared last so the writer descriptor closes before directory ownership.
    _owner: WalOwner,
}
struct WalOwner {
    file: std::fs::File,
    pid: u32,
}
impl Drop for WalOwner {
    fn drop(&mut self) {
        // A forked child must not unlock its parent's inherited open description.
        if std::process::id() == self.pid {
            let _ = self.file.unlock();
        }
    }
}
struct WalInner {
    file: std::fs::File,
    seg_id: u64,
    seg_bytes: u64,
    disk_bytes: u64,
    segments: usize,
    failed: bool,
    acked: HashMap<(String, String), u64>,
    pending: Vec<causal::PendingRecord>,
    pending_bytes: usize,
    watermark_bytes: usize,
    receipts: submission::ReceiptIndex,
}
impl WalInner {
    fn healthy(&self) -> Result<(), String> {
        if self.failed {
            Err("wal poisoned by an I/O or storage mutation failure; close and recover before reuse".into())
        } else {
            Ok(())
        }
    }
}
impl Wal {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, String> {
        Self::open_with(dir, 64 * 1024 * 1024)
    }
    pub fn open_with(dir: impl AsRef<Path>, segment_max: u64) -> Result<Self, String> {
        Self::open_with_limits(dir, segment_max, WalLimits::default())
    }
    /// Recovery validates all segments before truncating an incomplete final
    /// header/body. Complete CRC/codec corruption is never automatically erased.
    pub fn open_with_limits(
        dir: impl AsRef<Path>,
        segment_max: u64,
        limits: WalLimits,
    ) -> Result<Self, String> {
        limits.validate(segment_max)?;
        let dir = dir.as_ref().to_path_buf();
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&dir)
            .map_err(|e| format!("wal mkdir: {e}"))?;
        let lock_path = dir.join(".parrot-wal.lock");
        let file = open_regular(&lock_path, OpenKind::Lock)?;
        file.try_lock()
            .map_err(|e| format!("wal writer lock: {e}"))?;
        let owner = WalOwner {
            file,
            pid: std::process::id(),
        };
        let segments = list_segments(&dir, limits.max_segments)?;
        let mut disk_bytes = 0u64;
        for &sid in &segments {
            let file = open_regular(&segment_path(&dir, sid), OpenKind::Read)?;
            disk_bytes = disk_bytes
                .checked_add(file.metadata().map_err(|e| e.to_string())?.len())
                .filter(|v| *v <= limits.max_disk_bytes)
                .ok_or("wal disk byte budget exceeded")?;
        }
        let seg_id = segments.last().copied().unwrap_or(1);
        seg_id.checked_add(1).ok_or("wal segment id exhausted")?;
        let mut acked = HashMap::new();
        let mut watermark_bytes = 0usize;
        let mut tail = None;
        let mut receipts = submission::ReceiptIndex::default();
        // Pass one validates every record and obtains all cumulative ACKs. This
        // avoids retaining acknowledged history merely to discover a later ACK.
        for &sid in &segments {
            let scan = scan_segment(
                &segment_path(&dir, sid),
                limits,
                Some(&sid) == segments.last(),
                |rec| {
                    let (rec, _) = causal::read(rec)?;
                    let (business, receipt) = submission::read(rec)?;
                    if let Some(receipt) = receipt {
                        receipts.insert(receipt, limits)?;
                    }
                    let Some(rec) = business else {
                        return Ok(());
                    };
                    if rec.type_key == "#ack" {
                        let key = (rec.endpoint, rec.sender);
                        if !acked.contains_key(&key) && acked.len() == limits.max_streams {
                            return Err("wal stream budget exceeded".into());
                        }
                        if !acked.contains_key(&key) {
                            watermark_bytes =
                                reserve_watermark(watermark_bytes, &key.0, &key.1, limits)?;
                        }
                        let watermark = acked.entry(key).or_insert(0u64);
                        *watermark = (*watermark).max(rec.seq);
                    }
                    Ok(())
                },
            )?;
            if scan.good_end != scan.length {
                tail = Some((sid, scan.good_end, scan.length));
            }
        }
        let mut pending = Vec::new();
        let mut pending_bytes = 0;
        for &sid in &segments {
            scan_segment(
                &segment_path(&dir, sid),
                limits,
                Some(&sid) == segments.last(),
                |rec| {
                    let (rec, parent) = causal::read(rec)?;
                    let (Some(rec), _) = submission::read(rec)? else {
                        return Ok(());
                    };
                    if rec.type_key != "#ack"
                        && acked
                            .get(&(rec.endpoint.clone(), rec.sender.clone()))
                            .is_none_or(|n| rec.seq > *n)
                    {
                        pending_bytes =
                            reserve_pending(&rec, pending.len(), pending_bytes, limits)?;
                        pending.push(causal::PendingRecord {
                            record: rec,
                            parent,
                        });
                    }
                    Ok(())
                },
            )?;
        }
        // No repair until the complete recovery input and resource budgets pass.
        if let Some((sid, good, length)) = tail {
            let file = open_regular(&segment_path(&dir, sid), OpenKind::Append)?;
            file.set_len(good)
                .map_err(|e| format!("wal truncate: {e}"))?;
            file.sync_all()
                .map_err(|e| format!("wal sync truncate: {e}"))?;
            disk_bytes -= length - good;
        }
        let file = open_regular(
            &segment_path(&dir, seg_id),
            if segments.is_empty() {
                OpenKind::New
            } else {
                OpenKind::Append
            },
        )?;
        let seg_bytes = file.metadata().map_err(|e| e.to_string())?.len();
        sync_dir(&dir)?;
        Ok(Self {
            dir,
            segment_max,
            limits,
            _owner: owner,
            inner: Mutex::new(WalInner {
                file,
                seg_id,
                seg_bytes,
                disk_bytes,
                segments: segments.len().max(1),
                failed: false,
                acked,
                pending,
                pending_bytes,
                watermark_bytes,
                receipts,
            }),
        })
    }
    fn state(&self) -> Result<std::sync::MutexGuard<'_, WalInner>, String> {
        if std::process::id() != self._owner.pid {
            return Err("wal handle cannot be used after fork".into());
        }
        let guard = self
            .inner
            .lock()
            .map_err(|_| "wal mutex poisoned".to_string())?;
        guard.healthy()?;
        Ok(guard)
    }
    pub fn append(&self, rec: WalRecord) -> Result<(), String> {
        self.append_submission(rec, None).map(|_| ())
    }
    /// Returns the original sequence and whether a new record was appended.
    /// This internal acceptance is not a durability receipt until sync succeeds.
    fn append_submission(
        &self,
        rec: WalRecord,
        request_id: Option<&str>,
    ) -> Result<(u64, bool), String> {
        self.append_submission_with_parent(rec, request_id, None)
    }
    fn append_submission_with_parent(
        &self,
        rec: WalRecord,
        request_id: Option<&str>,
        parent: Option<parrot_api::trace::TraceParent>,
    ) -> Result<(u64, bool), String> {
        let mut state = self.state()?;
        validate_record(&rec, false)?;
        if let Some(id) = request_id {
            if let Some(seq) = state.receipts.lookup(&rec, id)? {
                return Ok((seq, false));
            }
        }
        if state
            .acked
            .get(&(rec.endpoint.clone(), rec.sender.clone()))
            .is_some_and(|seq| rec.seq <= *seq)
        {
            if request_id.is_some() {
                return Err("new submission sequence is already ACKed".into());
            }
            return Ok((rec.seq, false));
        }
        let (stored, receipt) = match request_id {
            Some(id) => {
                let (stored, receipt) = submission::wrap(&rec, id)?;
                (stored, Some(receipt))
            }
            None => (rec.clone(), None),
        };
        if let Some(receipt) = &receipt {
            state.receipts.check(receipt, self.limits)?;
        }
        let bytes = reserve_pending(&rec, state.pending.len(), state.pending_bytes, self.limits)?;
        let stored = causal::wrap(stored, parent)?;
        let encoded = encode_record(&stored, self.limits)?;
        ensure_disk(&state, encoded.len(), self.limits)?;
        let rotate = state.seg_bytes + encoded.len() as u64 >= self.segment_max;
        if rotate && (state.seg_id == u64::MAX || state.segments == self.limits.max_segments) {
            return Err("wal segment count/id budget exhausted".into());
        }
        let rec = WalRecord {
            endpoint: rec.endpoint.clone(),
            sender: rec.sender.clone(),
            path: rec.path.clone(),
            type_key: rec.type_key.clone(),
            seq: rec.seq,
            payload: bytes::Bytes::copy_from_slice(&rec.payload),
        };
        let result = (|| {
            write_encoded(&mut state, &encoded)?;
            if let Some(receipt) = receipt {
                state.receipts.insert(receipt, self.limits)?;
            }
            let seq = rec.seq;
            state.pending.push(causal::PendingRecord {
                record: rec,
                parent,
            });
            state.pending_bytes = bytes;
            if rotate {
                state
                    .file
                    .sync_all()
                    .map_err(|e| format!("wal sync before roll: {e}"))?;
                let next = state.seg_id + 1;
                let file = open_regular(&segment_path(&self.dir, next), OpenKind::New)?;
                sync_dir(&self.dir)?;
                state.file = file;
                state.seg_id = next;
                state.seg_bytes = 0;
                state.segments += 1;
            }
            Ok((seq, true))
        })();
        if result.is_err() {
            state.failed = true;
        }
        result
    }
    pub fn flush(&self) -> Result<(), String> {
        self.sync()
    }
    pub fn sync(&self) -> Result<(), String> {
        let mut state = self.state()?;
        let result = state.file.sync_all().map_err(|e| format!("wal sync: {e}"));
        if result.is_err() {
            state.failed = true;
        }
        result
    }
    /// Caller certifies all messages through seq completed. This does not infer
    /// business execution or validate an ACK received from a remote peer.
    pub fn ack(&self, endpoint: &str, sender: &str, seq: u64) -> Result<(), String> {
        let mut state = self.state()?;
        let marker = ack_record(endpoint, sender, seq);
        validate_record(&marker, true)?;
        let key = (endpoint.to_owned(), sender.to_owned());
        if state.acked.get(&key).is_some_and(|n| seq <= *n) {
            return Ok(());
        }
        if !state.acked.contains_key(&key) && state.acked.len() == self.limits.max_streams {
            return Err("wal stream budget exceeded".into());
        }
        let watermark_bytes = if state.acked.contains_key(&key) {
            state.watermark_bytes
        } else {
            reserve_watermark(state.watermark_bytes, endpoint, sender, self.limits)?
        };
        let marker = encode_record(&marker, self.limits)?;
        let clears = state
            .pending
            .iter()
            .all(|r| r.endpoint == endpoint && r.sender == sender && r.seq <= seq);
        // Preflight all checkpoint bytes before any write or in-memory ACK update.
        let mut checkpoint_bytes = if clears {
            state.receipts.checkpoint_bytes
        } else {
            0
        };
        if clears {
            for ((e, s), n) in &state.acked {
                if e == endpoint && s == sender {
                    continue;
                }
                checkpoint_bytes = checkpoint_bytes
                    .checked_add(encoded_size(&ack_record(e, s, *n), self.limits)?)
                    .ok_or("wal checkpoint size overflow")?;
            }
        }
        ensure_disk(
            &state,
            marker
                .len()
                .checked_add(checkpoint_bytes)
                .ok_or("wal checkpoint size overflow")?,
            self.limits,
        )?;
        if clears && (state.seg_id == u64::MAX || state.segments == self.limits.max_segments) {
            return Err("wal checkpoint segment budget exhausted".into());
        }
        let result = (|| {
            if clears {
                // New checkpoint is durable and its directory entry synced before
                // deleting any old segment, including the previously active one.
                let next = state.seg_id + 1;
                let mut file = open_regular(&segment_path(&self.dir, next), OpenKind::New)?;
                file.write_all(&marker)
                    .map_err(|e| format!("wal checkpoint write: {e}"))?;
                for ((e, s), n) in &state.acked {
                    if e == endpoint && s == sender {
                        continue;
                    }
                    file.write_all(&encode_record(&ack_record(e, s, *n), self.limits)?)
                        .map_err(|e| format!("wal checkpoint write: {e}"))?;
                }
                for receipt in state.receipts.iter() {
                    file.write_all(&encode_record(&receipt.record()?, self.limits)?)
                        .map_err(|e| format!("wal receipt checkpoint write: {e}"))?;
                }
                file.sync_all()
                    .map_err(|e| format!("wal sync checkpoint: {e}"))?;
                sync_dir(&self.dir)?;
                state.file = file;
                state.seg_id = next;
                state.seg_bytes = (marker.len() + checkpoint_bytes) as u64;
                for sid in list_segments(&self.dir, self.limits.max_segments)? {
                    if sid < next {
                        std::fs::remove_file(segment_path(&self.dir, sid))
                            .map_err(|e| format!("wal gc remove: {e}"))?;
                    }
                }
                sync_dir(&self.dir)?;
                state.disk_bytes = state.seg_bytes;
                state.segments = 1;
            } else {
                write_encoded(&mut state, &marker)?;
            }
            state.acked.insert(key, seq);
            state.watermark_bytes = watermark_bytes;
            state
                .pending
                .retain(|r| !(r.endpoint == endpoint && r.sender == sender && r.seq <= seq));
            state.pending_bytes = state
                .pending
                .iter()
                .map(|pending| record_memory(&pending.record))
                .sum();
            Ok(())
        })();
        if result.is_err() {
            state.failed = true;
        }
        result
    }
    /// A poisoned handle cannot provide replay data until recovery reconciles disk.
    pub fn replay(&self, endpoint: &str) -> Result<Vec<WalRecord>, String> {
        let state = self.state()?;
        let mut out: Vec<_> = state
            .pending
            .iter()
            .filter(|r| r.endpoint == endpoint)
            .map(|pending| pending.record.clone())
            .collect();
        out.sort_by_key(|r| r.seq);
        Ok(out)
    }
    pub fn pending_len(&self) -> Result<usize, String> {
        Ok(self.state()?.pending.len())
    }
}
fn reserve_watermark(
    previous: usize,
    endpoint: &str,
    sender: &str,
    limits: WalLimits,
) -> Result<usize, String> {
    [
        endpoint.len(),
        sender.len(),
        std::mem::size_of::<((String, String), u64)>(),
    ]
    .into_iter()
    .try_fold(previous, usize::checked_add)
    .filter(|n| *n <= limits.max_watermark_bytes)
    .ok_or("wal watermark byte budget exceeded".into())
}
fn record_memory(record: &WalRecord) -> usize {
    std::mem::size_of::<causal::PendingRecord>()
        + record.endpoint.len()
        + record.sender.len()
        + record.path.len()
        + record.type_key.len()
        + record.payload.len()
}
fn reserve_pending(
    record: &WalRecord,
    count: usize,
    bytes: usize,
    limits: WalLimits,
) -> Result<usize, String> {
    if count >= limits.max_pending_records {
        return Err("wal pending count budget exceeded".into());
    }
    bytes
        .checked_add(record_memory(record))
        .filter(|n| *n <= limits.max_pending_bytes)
        .ok_or("wal pending byte budget exceeded".into())
}
fn validate_record(record: &WalRecord, allow_ack: bool) -> Result<(), String> {
    if record.seq == 0
        || record.endpoint.is_empty()
        || record.sender.is_empty()
        || record.type_key.is_empty()
        || (!allow_ack
            && (submission::reserved(&record.type_key) || causal::reserved(&record.type_key)))
        || (record.type_key == "#ack"
            && (!allow_ack || !record.path.is_empty() || !record.payload.is_empty()))
    {
        return Err("wal invalid record identity, reserved ACK or zero sequence".into());
    }
    Ok(())
}
fn ack_record(endpoint: &str, sender: &str, seq: u64) -> WalRecord {
    WalRecord {
        endpoint: endpoint.into(),
        sender: sender.into(),
        seq,
        path: String::new(),
        type_key: "#ack".into(),
        payload: bytes::Bytes::new(),
    }
}
struct SizeCounter {
    size: usize,
    cap: usize,
}
impl Write for SizeCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.size = self
            .size
            .checked_add(bytes.len())
            .filter(|n| *n <= self.cap)
            .ok_or_else(|| std::io::Error::other("record byte budget"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encoded_size(record: &WalRecord, limits: WalLimits) -> Result<usize, String> {
    let mut count = SizeCounter {
        size: 0,
        cap: limits.max_record_bytes,
    };
    bincode::serde::encode_into_std_write(record, &mut count, bincode::config::standard())
        .map_err(|e| format!("wal encode budget: {e}"))?;
    Ok(count.size + 8)
}
fn encode_record(record: &WalRecord, limits: WalLimits) -> Result<Vec<u8>, String> {
    let size = encoded_size(record, limits)?;
    let body = bincode::serde::encode_to_vec(record, bincode::config::standard())
        .map_err(|e| format!("wal encode: {e}"))?;
    let mut frame = Vec::with_capacity(size);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&crc32(&body).to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}
fn ensure_disk(state: &WalInner, bytes: usize, limits: WalLimits) -> Result<(), String> {
    state
        .disk_bytes
        .checked_add(bytes as u64)
        .filter(|n| *n <= limits.max_disk_bytes)
        .ok_or("wal disk byte budget exceeded")?;
    Ok(())
}
fn write_encoded(state: &mut WalInner, frame: &[u8]) -> Result<(), String> {
    state
        .file
        .write_all(frame)
        .map_err(|e| format!("wal write: {e}"))?;
    state.seg_bytes += frame.len() as u64;
    state.disk_bytes += frame.len() as u64;
    Ok(())
}
fn segment_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("seg-{id:08}.log"))
}
fn list_segments(dir: &Path, max: usize) -> Result<Vec<u64>, String> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("wal readdir: {e}"))? {
        let entry = entry.map_err(|e| format!("wal directory entry: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err("wal non-UTF8 directory entry".into());
        };
        if !name.starts_with("seg-") {
            continue;
        }
        let id = name
            .strip_prefix("seg-")
            .and_then(|s| s.strip_suffix(".log"))
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|id| *id > 0)
            .ok_or("wal invalid segment name")?;
        if name != format!("seg-{id:08}.log") {
            return Err("wal noncanonical segment name".into());
        }
        if ids.len() == max {
            return Err("wal segment count budget exceeded".into());
        }
        ids.push(id);
    }
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0].checked_add(1) != Some(w[1])) {
        return Err("wal segment gap detected".into());
    }
    Ok(ids)
}
enum OpenKind {
    Read,
    Append,
    New,
    Lock,
}
fn open_regular(path: &Path, kind: OpenKind) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    match kind {
        OpenKind::Read => {}
        OpenKind::Append => {
            options.append(true);
        }
        OpenKind::New => {
            options.append(true).create_new(true);
        }
        OpenKind::Lock => {
            options.write(true).create(true);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_NOCTTY);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("wal symlink entry forbidden".into());
    }
    let file = options
        .open(path)
        .map_err(|e| format!("wal open {path:?}: {e}"))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("wal requires regular files".into());
    }
    Ok(file)
}
struct Scan {
    good_end: u64,
    length: u64,
}
fn scan_segment(
    path: &Path,
    limits: WalLimits,
    tail: bool,
    mut visit: impl FnMut(WalRecord) -> Result<(), String>,
) -> Result<Scan, String> {
    use std::io::Read;
    let file = open_regular(path, OpenKind::Read)?;
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    if length > limits.max_disk_bytes {
        return Err("wal disk byte budget exceeded".into());
    }
    let mut reader = std::io::BufReader::new(file.take(length));
    let mut position = 0;
    while position < length {
        let interrupted = || {
            if tail {
                Ok(Scan {
                    good_end: position,
                    length,
                })
            } else {
                Err(format!(
                    "wal corruption in sealed segment {path:?} at {position}"
                ))
            }
        };
        if length - position < 8 {
            return interrupted();
        }
        let mut header = [0u8; 8];
        reader
            .read_exact(&mut header)
            .map_err(|e| format!("wal read header: {e}"))?;
        let size = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        if size == 0 || size > limits.max_record_bytes {
            return Err(format!("wal invalid record length {size} at {position}"));
        }
        if size as u64 > length - position - 8 {
            return interrupted();
        }
        let mut body = vec![0; size];
        reader
            .read_exact(&mut body)
            .map_err(|e| format!("wal read record: {e}"))?;
        if crc32(&body) != u32::from_le_bytes(header[4..].try_into().unwrap()) {
            return Err(format!(
                "wal checksum corruption in {}segment {path:?} at {position}",
                if tail { "" } else { "sealed " }
            ));
        }
        let record = parrot_api::message::serde_remote_deserialize_with_limits::<WalRecord>(
            &body,
            parrot_api::message::RemoteDecodeLimits {
                max_payload_bytes: limits.max_record_bytes,
                ..Default::default()
            },
        )
        .map_err(|e| format!("wal invalid record at {position}: {e}"))?;
        validate_record(&record, true)?;
        visit(record)?;
        position += 8 + size as u64;
    }
    Ok(Scan {
        good_end: position,
        length,
    })
}

fn sync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("wal sync directory: {e}"))?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

mod causal;
mod envelope;
mod submission;
pub use envelope::{DurableEnvelope, DURABLE_KEY, DURABLE_RECEIPT_KEY};
mod inbox;
pub use inbox::{
    DurableReceiver, InboxError, InboxHandle, InboxLimits, InboxSnapshot, TransactionalInbox,
};

mod outbox;
pub use outbox::{DurableOutbox, OutboxConfig, OutboxError, OutboxHandle, OutboxStatus};

mod dedup;
pub use dedup::{DedupDecision, DedupError, DedupLimits, DedupPermit, DedupStats, DedupTable};

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(endpoint: &str, sender: &str, seq: u64) -> WalRecord {
        WalRecord {
            endpoint: endpoint.into(),
            sender: sender.into(),
            seq,
            path: "/user/dev".into(),
            type_key: "bin:t::T".into(),
            payload: bytes::Bytes::from_static(b"hello"),
        }
    }

    // wal_crash_recovery：写一半（模拟截断）→ 重开扫描截断正确
    #[test]
    fn wal_crash_recovery() {
        let dir = std::env::temp_dir().join(format!("parrot-wal-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let wal = Wal::open(&dir).unwrap();
            wal.append(rec("edge-1", "cloud", 1)).unwrap();
            wal.append(rec("edge-1", "cloud", 2)).unwrap();
            wal.flush().unwrap();
        }
        // 第一次 open 的段是 seg-00000001（seg_ids 空 → 起始 1）
        let seg = dir.join("seg-00000001.log");
        let data = std::fs::read(&seg).unwrap();
        // 模拟崩溃：最后一条写一半（截到第一条记录结束——保留完整记录 1，
        // 记录 2 部分写入被丢弃；截 4B 会破坏记录 2 的 crc 属“坏记录”同语义）
        // 找第一条记录边界：8 + len1
        let len1 = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let cut = 8 + len1 + 4; // 进入第二条记录头内（半条）
        std::fs::write(&seg, &data[..cut]).unwrap();
        // 重开：恢复出完整一条（第二条半写截掉——零部分写入）
        let wal = Wal::open(&dir).unwrap();
        let replay = wal.replay("edge-1").unwrap();
        assert_eq!(replay.len(), 1, "only complete record survives");
        assert_eq!(replay[0].seq, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ack 水位推进 + pending 清理 + 段删除
    #[test]
    fn wal_ack_watermark_and_gc() {
        let dir = std::env::temp_dir().join(format!("parrot-wal-ack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let wal = Wal::open(&dir).unwrap();
        wal.append(rec("e1", "s1", 1)).unwrap();
        wal.append(rec("e1", "s1", 2)).unwrap();
        wal.append(rec("e1", "s2", 1)).unwrap(); // 不同 sender 独立序列
        assert_eq!(wal.pending_len().unwrap(), 3);
        wal.ack("e1", "s1", 2).unwrap();
        assert_eq!(wal.pending_len().unwrap(), 1, "s1's both acked");
        let r = wal.replay("e1").unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].sender, "s2");
        // 全清 → 旧段删除
        wal.ack("e1", "s2", 1).unwrap();
        assert_eq!(wal.pending_len().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wal_ack_survives_reopen_and_is_cumulative() {
        let dir = std::env::temp_dir().join(format!("parrot-wal-reopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let wal = Wal::open_with(&dir, 1024).unwrap();
            wal.append(rec("e", "s", 1)).unwrap();
            wal.append(rec("e", "s", 2)).unwrap();
            wal.ack("e", "s", 2).unwrap();
            wal.ack("e", "s", 1).unwrap(); // must not regress the watermark
            wal.sync().unwrap();
        }
        let wal = Wal::open_with(&dir, 1024).unwrap();
        assert!(
            wal.replay("e").unwrap().is_empty(),
            "acked records must not replay after restart"
        );
        wal.append(rec("e", "s", 3)).unwrap();
        assert_eq!(
            wal.replay("e")
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            vec![3]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wal_partial_ack_and_gc_preserve_all_stream_watermarks() {
        let dir = std::env::temp_dir().join(format!("parrot-wal-segments-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let wal = Wal::open_with(&dir, 1).unwrap(); // every append seals a segment
            for record in [rec("e", "s", 1), rec("e", "s", 2), rec("e", "other", 1)] {
                wal.append(record).unwrap();
            }
            wal.ack("e", "s", 1).unwrap();
            wal.sync().unwrap();
        }
        {
            let wal = Wal::open_with(&dir, 1).unwrap();
            assert_eq!(
                wal.replay("e").unwrap(),
                vec![rec("e", "other", 1), rec("e", "s", 2)]
            );
            wal.ack("e", "s", 2).unwrap();
            wal.ack("e", "other", 1).unwrap();
            assert_eq!(wal.pending_len().unwrap(), 0);
        }
        let wal = Wal::open_with(&dir, 1).unwrap();
        for record in [rec("e", "s", 1), rec("e", "s", 2), rec("e", "other", 1)] {
            wal.append(record).unwrap();
        }
        assert_eq!(
            wal.pending_len().unwrap(),
            0,
            "GC must retain ACK watermarks for every stream"
        );
        wal.append(rec("e", "s", 3)).unwrap();
        assert_eq!(wal.replay("e").unwrap(), vec![rec("e", "s", 3)]);
        drop(wal);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wal_sealed_corruption_is_reported_without_modifying_files() {
        let dir = std::env::temp_dir().join(format!("parrot-wal-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let wal = Wal::open_with(&dir, 1).unwrap();
            wal.append(rec("e", "s", 1)).unwrap();
            wal.append(rec("e", "s", 2)).unwrap();
            wal.sync().unwrap();
        }
        let segment = dir.join("seg-00000001.log");
        let mut damaged = std::fs::read(&segment).unwrap();
        damaged[8] ^= 0xff;
        std::fs::write(&segment, &damaged).unwrap();
        assert!(Wal::open(&dir).err().unwrap().contains("sealed segment"));
        assert_eq!(std::fs::read(&segment).unwrap(), damaged);
        std::fs::remove_dir_all(dir).unwrap();
    }

    // 去重表：重放重复不二次处理
    #[test]
    fn dedup_table_filters() {
        let mut d = DedupTable::new();
        assert!(d.filter("s1", 1).unwrap());
        assert!(!d.filter("s1", 1).unwrap(), "duplicate dropped");
        assert!(d.filter("s1", 2).unwrap());
        assert!(d.filter("s1", 3).unwrap());
        assert!(!d.filter("s1", 2).unwrap(), "replayed old dropped");
        assert!(d.filter("s2", 1).unwrap(), "independent sender");
    }

    #[test]
    fn dedup_out_of_order_does_not_advance_past_a_gap() {
        let mut d = DedupTable::new();
        assert!(d.filter("s", 3).unwrap());
        assert!(d.filter("s", 1).unwrap());
        assert!(d.filter("s", 2).unwrap());
        assert!(!d.filter("s", 3).unwrap());
        assert!(!d.filter("s", 1).unwrap());
    }

    // crc32 与 IEEE 参考值对齐（"123456789" → 0xCBF43926）
    #[test]
    fn crc32_reference() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}

pub(crate) mod timing;
