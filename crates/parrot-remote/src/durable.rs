//! E2 · durable tell（DEV_03 §3 / 06 §3.2）：云 proxy WAL。
//!
//! CloudProxy：下行 tell 的持久化中继——写 WAL（组提交 fsync 可配）→ 转发端侧；
//! ACK：TELL 置 flags::TELL_ACK，端侧处理完成回 ACK 变体（payload=[u64 seq]）；
//! 断线：WAL 保留，重连按序重放；端侧按 (sender, seq) 去重 → 业务幂等层。
//!
//! WAL 裁定（06 ⚠ 复核点）：文件追加 + 内存索引，不引入嵌入式 DB。
//! 段文件 64MB 滚动；段内 `[u32 len][crc32][record]`；ACK 水位推进后旧段
//! 整段删除；崩溃恢复扫尾段校验 crc 截断不完整记录。

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

/// WAL 记录（wire 形态：serde bincode——Rust 侧自描述，不跨语言）。
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

/// 单端 WAL：追加 + 水位 + 崩溃恢复。
///
/// 线程模型：内部 Mutex 串行（WAL 写者是 proxy 单 actor——低竞争）。
pub struct Wal {
    dir: PathBuf,
    /// 段大小阈值滚动。
    segment_max: u64,
    inner: Mutex<WalInner>,
}

struct WalInner {
    /// 当前段文件 + 已写字节。
    file: std::fs::File,
    seg_id: u64,
    seg_bytes: u64,
    /// (endpoint, sender) → 已 ACK 最高 seq（水位）。
    acked: HashMap<(String, String), u64>,
    /// 未 ACK 记录的内存索引（重放源；段删除的依据）。
    pending: Vec<WalRecord>,
}

impl Wal {
    /// 打开/恢复：扫 dir 全部段，校验 crc，截断尾段坏记录。
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, String> {
        Self::open_with(dir, 64 * 1024 * 1024)
    }

    pub fn open_with(dir: impl AsRef<Path>, segment_max: u64) -> Result<Self, String> {
        std::fs::create_dir_all(&dir).map_err(|e| format!("wal mkdir: {e}"))?;
        let dir = dir.as_ref().to_path_buf();
        // 扫已有段（seg-00000001.log 命名——排序即序）
        let mut seg_ids: Vec<u64> = std::fs::read_dir(&dir)
            .map_err(|e| format!("wal readdir: {e}"))?
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.strip_prefix("seg-")?
                    .strip_suffix(".log")?
                    .parse()
                    .ok()
            })
            .collect();
        seg_ids.sort_unstable();

        let mut pending = Vec::new();
        let mut acked: HashMap<(String, String), u64> = HashMap::new();
        // 全段重放（重启后 pending 恢复；acked 从 ACK 记录恢复——ACK 也入 WAL）
        for sid in &seg_ids {
            let path = dir.join(format!("seg-{sid:08}.log"));
            let data = std::fs::read(&path).map_err(|e| format!("wal read {path:?}: {e}"))?;
            let mut off = 0usize;
            let mut good_end = 0usize;
            while off + 8 <= data.len() {
                let len =
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                        as usize;
                let crc = u32::from_le_bytes([
                    data[off + 4],
                    data[off + 5],
                    data[off + 6],
                    data[off + 7],
                ]);
                if off + 8 + len > data.len() {
                    break; // 尾部不完整 → 截断
                }
                let body = &data[off + 8..off + 8 + len];
                if crc32(body) != crc {
                    break; // crc 失败 → 截断（崩溃写一半）
                }
                let (rec, consumed) = bincode::serde::decode_from_slice::<WalRecord, _>(
                    body,
                    bincode::config::standard(),
                )
                .map_err(|e| format!("wal invalid record {path:?} at {off}: {e}"))?;
                if consumed != body.len() {
                    return Err(format!("wal trailing record data {path:?} at {off}"));
                }
                pending.push(rec);
                off += 8 + len;
                good_end = off;
            }
            if good_end < data.len() {
                // Only the last segment can contain an interrupted append.
                // Earlier corruption is data loss, not a recoverable tail.
                if Some(sid) != seg_ids.last() {
                    return Err(format!(
                        "wal corruption in sealed segment {path:?} at {good_end}"
                    ));
                }
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .map_err(|e| format!("wal open for truncate: {e}"))?;
                file.set_len(good_end as u64)
                    .map_err(|e| format!("wal truncate: {e}"))?;
                file.sync_all()
                    .map_err(|e| format!("wal sync truncate: {e}"))?;
                // 截断点之后不再解析（本段结束）
            }
        }
        // 重建水位：pending 里按 (endpoint,sender) 最大 seq 之前的已 ACK 部分
        // 简化：水位从 ACK_MARKER 记录读（payload 为 [u8 1] 标记 + 内嵌 sender/seq）
        for rec in &pending {
            if rec.type_key == "#ack" {
                let watermark = acked
                    .entry((rec.endpoint.clone(), rec.sender.clone()))
                    .or_insert(0);
                *watermark = (*watermark).max(rec.seq);
            }
        }
        pending.retain(|r| {
            r.type_key != "#ack"
                && acked
                    .get(&(r.endpoint.clone(), r.sender.clone()))
                    .is_none_or(|seq| r.seq > *seq)
        });

        // 开新段（或续写最后段）
        let seg_id = seg_ids.last().map(|s| s + 1).unwrap_or(1);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("seg-{seg_id:08}.log")))
            .map_err(|e| format!("wal create: {e}"))?;
        sync_dir(&dir)?;
        Ok(Self {
            dir,
            segment_max,
            inner: Mutex::new(WalInner {
                file,
                seg_id,
                seg_bytes: 0,
                acked,
                pending,
            }),
        })
    }

    /// Append a pending record. `sync`/`flush` establish the durability boundary.
    /// Sequence numbers are positive and ACKs are cumulative per endpoint/sender.
    pub fn append(&self, rec: WalRecord) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        if rec.type_key == "#ack" || rec.seq == 0 {
            return Err("wal reserved record type or zero sequence".into());
        }
        if g.acked
            .get(&(rec.endpoint.clone(), rec.sender.clone()))
            .is_some_and(|seq| rec.seq <= *seq)
        {
            return Ok(()); // Retrying an already acknowledged append is harmless.
        }
        write_record(&mut g, &rec)?;
        g.pending.push(rec);
        if g.seg_bytes >= self.segment_max {
            // A later sync on the active file must also cover all sealed files.
            g.file
                .sync_all()
                .map_err(|e| format!("wal sync before roll: {e}"))?;
            let next_id = g.seg_id.checked_add(1).ok_or("wal segment id exhausted")?;
            let next_file = std::fs::OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(self.dir.join(format!("seg-{next_id:08}.log")))
                .map_err(|e| format!("wal roll: {e}"))?;
            sync_dir(&self.dir)?;
            g.file = next_file;
            g.seg_id = next_id;
            g.seg_bytes = 0;
        }
        Ok(())
    }

    /// 组提交 flush（proxy 定时 10ms 或批量后调）。
    pub fn flush(&self) -> Result<(), String> {
        self.sync()
    }

    /// 同步落盘（崩溃安全点）。
    pub fn sync(&self) -> Result<(), String> {
        self.inner
            .lock()
            .unwrap()
            .file
            .sync_all()
            .map_err(|e| format!("wal sync: {e}"))
    }

    /// ACK 水位推进（endpoint, sender, seq）：写 ACK 标记 + 删除安全旧段。
    pub fn ack(&self, endpoint: &str, sender: &str, seq: u64) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        // ACK 标记录（恢复时重放水位）
        let marker = WalRecord {
            endpoint: endpoint.into(),
            sender: sender.into(),
            seq,
            path: String::new(),
            type_key: "#ack".into(),
            payload: bytes::Bytes::new(),
        };
        write_record(&mut g, &marker)?;
        let watermark = g.acked.entry((endpoint.into(), sender.into())).or_insert(0);
        *watermark = (*watermark).max(seq);
        let watermark = *watermark;
        g.pending
            .retain(|r| !(r.endpoint == endpoint && r.sender == sender && r.seq <= watermark));
        if g.pending.is_empty() {
            // Checkpoint ALL watermarks in the surviving file before deleting
            // any segment. Keep the lock so an append cannot race this GC.
            let checkpoints: Vec<_> = g
                .acked
                .iter()
                .map(|((endpoint, sender), seq)| WalRecord {
                    endpoint: endpoint.clone(),
                    sender: sender.clone(),
                    seq: *seq,
                    path: String::new(),
                    type_key: "#ack".into(),
                    payload: bytes::Bytes::new(),
                })
                .collect();
            for checkpoint in checkpoints {
                write_record(&mut g, &checkpoint)?;
            }
            g.file
                .sync_all()
                .map_err(|e| format!("wal sync checkpoint: {e}"))?;
            for entry in std::fs::read_dir(&self.dir).map_err(|e| format!("wal gc readdir: {e}"))? {
                let entry = entry.map_err(|e| format!("wal gc entry: {e}"))?;
                let name = entry.file_name();
                if let Some(sid) = name
                    .to_str()
                    .and_then(|s| s.strip_prefix("seg-"))
                    .and_then(|s| s.strip_suffix(".log"))
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    if sid < g.seg_id {
                        std::fs::remove_file(entry.path())
                            .map_err(|e| format!("wal gc remove: {e}"))?;
                    }
                }
            }
            sync_dir(&self.dir)?;
        }
        Ok(())
    }

    /// 重连重放：该端点的未确认记录（按 seq 序）。
    pub fn replay(&self, endpoint: &str) -> Vec<WalRecord> {
        let g = self.inner.lock().unwrap();
        let mut out: Vec<WalRecord> = g
            .pending
            .iter()
            .filter(|r| r.endpoint == endpoint)
            .cloned()
            .collect();
        out.sort_by_key(|r| r.seq);
        out
    }

    /// 诊断：pending 条数。
    pub fn pending_len(&self) -> usize {
        self.inner.lock().unwrap().pending.len()
    }
}

fn write_record(inner: &mut WalInner, record: &WalRecord) -> Result<(), String> {
    let body = bincode::serde::encode_to_vec(record, bincode::config::standard())
        .map_err(|e| format!("wal encode: {e}"))?;
    let len = u32::try_from(body.len()).map_err(|_| "wal record too large")?;
    let mut frame = Vec::with_capacity(8 + body.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&crc32(&body).to_le_bytes());
    frame.extend_from_slice(&body);
    inner
        .file
        .write_all(&frame)
        .map_err(|e| format!("wal write: {e}"))?;
    inner.seg_bytes += frame.len() as u64;
    Ok(())
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

/// 端侧去重表：(sender, seq) 已处理集合——滑动窗口（按 sender 水位裁剪）。
#[derive(Default)]
pub struct DedupTable {
    seen: HashMap<(String, u64), ()>,
    watermark: HashMap<String, u64>,
}

impl DedupTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 首见返回 true（应处理）；重复返回 false（丢弃）。
    pub fn filter(&mut self, sender: &str, seq: u64) -> bool {
        let wm = self.watermark.get(sender).copied().unwrap_or(0);
        if seq <= wm {
            return false; // 水位下必重
        }
        if self.seen.insert((sender.to_string(), seq), ()).is_none() {
            // 推进水位：连续段确认后裁剪 seen（防无限增长）
            let mut next = wm;
            while let Some(candidate) = next.checked_add(1) {
                if self.seen.remove(&(sender.to_string(), candidate)).is_none() {
                    break;
                }
                next = candidate;
            }
            self.watermark.insert(sender.to_string(), next);
            true
        } else {
            false
        }
    }
}

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
        let replay = wal.replay("edge-1");
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
        assert_eq!(wal.pending_len(), 3);
        wal.ack("e1", "s1", 2).unwrap();
        assert_eq!(wal.pending_len(), 1, "s1's both acked");
        let r = wal.replay("e1");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].sender, "s2");
        // 全清 → 旧段删除
        wal.ack("e1", "s2", 1).unwrap();
        assert_eq!(wal.pending_len(), 0);
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
            wal.replay("e").is_empty(),
            "acked records must not replay after restart"
        );
        wal.append(rec("e", "s", 3)).unwrap();
        assert_eq!(
            wal.replay("e").iter().map(|r| r.seq).collect::<Vec<_>>(),
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
                wal.replay("e"),
                vec![rec("e", "other", 1), rec("e", "s", 2)]
            );
            wal.ack("e", "s", 2).unwrap();
            wal.ack("e", "other", 1).unwrap();
            assert_eq!(wal.pending_len(), 0);
        }
        let wal = Wal::open_with(&dir, 1).unwrap();
        for record in [rec("e", "s", 1), rec("e", "s", 2), rec("e", "other", 1)] {
            wal.append(record).unwrap();
        }
        assert_eq!(
            wal.pending_len(),
            0,
            "GC must retain ACK watermarks for every stream"
        );
        wal.append(rec("e", "s", 3)).unwrap();
        assert_eq!(wal.replay("e"), vec![rec("e", "s", 3)]);
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
        assert!(d.filter("s1", 1));
        assert!(!d.filter("s1", 1), "duplicate dropped");
        assert!(d.filter("s1", 2));
        assert!(d.filter("s1", 3));
        assert!(!d.filter("s1", 2), "replayed old dropped");
        assert!(d.filter("s2", 1), "independent sender");
    }

    #[test]
    fn dedup_out_of_order_does_not_advance_past_a_gap() {
        let mut d = DedupTable::new();
        assert!(d.filter("s", 3));
        assert!(d.filter("s", 1));
        assert!(d.filter("s", 2));
        assert!(!d.filter("s", 3));
        assert!(!d.filter("s", 1));
    }

    // crc32 与 IEEE 参考值对齐（"123456789" → 0xCBF43926）
    #[test]
    fn crc32_reference() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
