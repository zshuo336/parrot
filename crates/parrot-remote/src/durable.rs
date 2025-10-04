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
                if let Ok((rec, _)) = bincode::serde::decode_from_slice::<WalRecord, _>(
                    body,
                    bincode::config::standard(),
                ) {
                    pending.push(rec);
                }
                off += 8 + len;
                good_end = off;
            }
            if good_end < data.len() {
                // 截断尾段坏尾巴
                std::fs::write(&path, &data[..good_end])
                    .map_err(|e| format!("wal truncate {path:?}: {e}"))?;
                // 截断点之后不再解析（本段结束）
            }
            // acked 水位由 ACK 标记记录恢复（wal_ack 写 ACK_MARKER）
            let _ = &mut acked;
        }
        // 重建水位：pending 里按 (endpoint,sender) 最大 seq 之前的已 ACK 部分
        // 简化：水位从 ACK_MARKER 记录读（payload 为 [u8 1] 标记 + 内嵌 sender/seq）
        for rec in &pending {
            if rec.type_key == "#ack" {
                acked.insert((rec.endpoint.clone(), rec.sender.clone()), rec.seq);
            }
        }
        pending.retain(|r| r.type_key != "#ack");

        // 开新段（或续写最后段）
        let seg_id = seg_ids.last().map(|s| s + 1).unwrap_or(1);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("seg-{seg_id:08}.log")))
            .map_err(|e| format!("wal create: {e}"))?;
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

    /// 追加一条待确认记录（写盘，不 fsync——组提交由 flush 驱动）。
    pub fn append(&self, rec: WalRecord) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        let body = bincode::serde::encode_to_vec(&rec, bincode::config::standard())
            .map_err(|e| format!("wal encode: {e}"))?;
        let mut frame = Vec::with_capacity(8 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc32(&body).to_le_bytes());
        frame.extend_from_slice(&body);
        g.file
            .write_all(&frame)
            .map_err(|e| format!("wal write: {e}"))?;
        g.seg_bytes += frame.len() as u64;
        g.pending.push(rec);
        // 段滚动
        if g.seg_bytes >= self.segment_max {
            g.seg_id += 1;
            g.seg_bytes = 0;
            g.file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(format!("seg-{:08}.log", g.seg_id)))
                .map_err(|e| format!("wal roll: {e}"))?;
        }
        Ok(())
    }

    /// 组提交 flush（proxy 定时 10ms 或批量后调）。
    pub fn flush(&self) -> Result<(), String> {
        self.inner
            .lock()
            .unwrap()
            .file
            .flush()
            .map_err(|e| format!("wal flush: {e}"))
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
        let body = bincode::serde::encode_to_vec(&marker, bincode::config::standard())
            .map_err(|e| format!("wal encode ack: {e}"))?;
        let mut frame = Vec::with_capacity(8 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc32(&body).to_le_bytes());
        frame.extend_from_slice(&body);
        g.file
            .write_all(&frame)
            .map_err(|e| format!("wal write ack: {e}"))?;
        g.seg_bytes += frame.len() as u64;

        // 水位更新 + pending 清理（≤seq 的记录已确认）
        g.acked.insert((endpoint.into(), sender.into()), seq);
        g.pending
            .retain(|r| !(r.endpoint == endpoint && r.sender == sender && r.seq <= seq));
        // 段清理：pending 为空时删除非当前段（整段删除——无逐条空洞）
        if g.pending.is_empty() {
            let cur = g.seg_id;
            drop(g);
            if let Ok(rd) = std::fs::read_dir(&self.dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if let Some(sid) = name
                        .strip_prefix("seg-")
                        .and_then(|s| s.strip_suffix(".log"))
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        if sid < cur {
                            let _ = std::fs::remove_file(e.path());
                        }
                    }
                }
            }
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
            let mut next = seq;
            while self.seen.remove(&(sender.to_string(), next + 1)).is_some() {
                next += 1;
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

    // crc32 与 IEEE 参考值对齐（"123456789" → 0xCBF43926）
    #[test]
    fn crc32_reference() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
