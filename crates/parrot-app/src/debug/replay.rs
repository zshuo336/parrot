//! F3（DEV_09 §3.6）：record-replay。
//!
//! - record：帧级 JSONL 落盘（tx/rx 全帧——复用 trace_line 数据源语义，
//!   结构化字段而非行文本；durable.rs WAL 的 cid 序思想）
//! - replay：按文件序（= cid 时间序）逐条投递到本地组件句柄
//!   （ask 重放——回执比对由调用方断言）

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 单帧记录（JSONL 一行）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonlRecord {
    /// 单调序号（落盘顺序——replay 依此序）。
    pub seq: u64,
    /// 关联号（trace_id/cid——按 cid 子集重放的数据源）。
    pub cid: u64,
    /// 方向（tx/rx）。
    pub dir: String,
    pub frame_type: u8,
    pub path: String,
    pub type_key: String,
    /// payload 十六进制（JSONL 文本安全）。
    pub payload_hex: String,
}

impl JsonlRecord {
    pub fn payload_bytes(&self) -> Vec<u8> {
        hex_decode(&self.payload_hex)
    }
}

/// 简易 hex 编解码（省 hex 依赖——payload 调试量级小）。
fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

/// replay 错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    #[error("io: {0}")]
    Io(String),
    #[error("jsonl line {line}: {reason}")]
    BadLine { line: usize, reason: String },
    #[error("record seq not monotonic: {prev} → {next}")]
    NonMonotonic { prev: u64, next: u64 },
}

/// 记录接收端（实现写盘策略——同步写/缓冲写由调用方选）。
pub trait RecordSink: Send {
    fn write(&mut self, r: &JsonlRecord) -> Result<(), ReplayError>;
}

/// 文件 JSONL sink（append 打开——一次 run 一文件）。
pub struct FileSink {
    file: std::fs::File,
    path: PathBuf,
    last_seq: u64,
}

impl FileSink {
    pub fn open(path: &Path) -> Result<Self, ReplayError> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).map_err(|e| ReplayError::Io(e.to_string()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| ReplayError::Io(e.to_string()))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            last_seq: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl RecordSink for FileSink {
    fn write(&mut self, r: &JsonlRecord) -> Result<(), ReplayError> {
        use std::io::Write;
        let line = serde_json::to_string(r).map_err(|e| ReplayError::BadLine {
            line: 0,
            reason: e.to_string(),
        })?;
        writeln!(self.file, "{line}").map_err(|e| ReplayError::Io(e.to_string()))?;
        self.last_seq = r.seq;
        Ok(())
    }
}

/// 内存 sink（测试/短录制）。
#[derive(Debug, Default)]
pub struct MemSink {
    pub records: Vec<JsonlRecord>,
}

impl RecordSink for MemSink {
    fn write(&mut self, r: &JsonlRecord) -> Result<(), ReplayError> {
        self.records.push(r.clone());
        Ok(())
    }
}

/// record 便利构造（帧字段 → JsonlRecord；seq 由 sink 侧单调保证——
/// 调用方传入自增计数器）。
pub fn record_sink(seq: u64, cid: u64, dir: &str, frame_type: u8, path: &str, type_key: &str, payload: &[u8]) -> JsonlRecord {
    JsonlRecord {
        seq,
        cid,
        dir: dir.into(),
        frame_type,
        path: path.into(),
        type_key: type_key.into(),
        payload_hex: hex_encode(payload),
    }
}

/// 录制日志读取（replay 数据源——seq 单调校验防损坏）。
#[derive(Debug, Clone, Default)]
pub struct ReplayLog {
    pub records: Vec<JsonlRecord>,
}

impl ReplayLog {
    pub fn load(path: &Path) -> Result<Self, ReplayError> {
        let text = std::fs::read_to_string(path).map_err(|e| ReplayError::Io(e.to_string()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, ReplayError> {
        let mut records = Vec::new();
        let mut prev = 0u64;
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let r: JsonlRecord = serde_json::from_str(line).map_err(|e| ReplayError::BadLine {
                line: i + 1,
                reason: e.to_string(),
            })?;
            if r.seq <= prev && !records.is_empty() {
                return Err(ReplayError::NonMonotonic { prev, next: r.seq });
            }
            prev = r.seq;
            records.push(r);
        }
        Ok(Self { records })
    }

    /// 全量记录（cid 时间序 = 落盘序）。
    pub fn all(&self) -> &[JsonlRecord] {
        &self.records
    }

    /// 按 cid 过滤子集（trace_id 视角重放）。
    pub fn by_cid(&self, cid: u64) -> Vec<&JsonlRecord> {
        self.records.iter().filter(|r| r.cid == cid).collect()
    }

    /// 重放到投递闭包（ask 语义——回执由调用方收集/断言）。
    ///
    /// 返回：成功投递数（闭包 Err 即中止——保留现场）。
    pub fn replay<F>(&self, mut deliver: F) -> Result<usize, ReplayError>
    where
        F: FnMut(&JsonlRecord) -> Result<(), String>,
    {
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

    fn rec(seq: u64, cid: u64, path: &str, payload: &[u8]) -> JsonlRecord {
        record_sink(seq, cid, "tx", 0x10, path, "m::Ask", payload)
    }

    #[test]
    fn record_roundtrip_hex() {
        let r = rec(1, 7, "/a", b"\x00\x01\xff");
        assert_eq!(r.payload_hex, "0001ff");
        assert_eq!(r.payload_bytes(), vec![0, 1, 255]);
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
        assert_eq!(log.all()[1].payload_bytes(), b"two".to_vec());
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
            records: vec![rec(1, 1, "/a", b""), rec(2, 2, "/b", b""), rec(3, 1, "/c", b"")],
        };
        assert_eq!(log.by_cid(1).len(), 2);
        assert_eq!(log.by_cid(2).len(), 1);
        assert_eq!(log.by_cid(9).len(), 0);
    }

    #[test]
    fn replay_in_order_and_abort_on_err() {
        let log = ReplayLog {
            records: vec![rec(1, 1, "/a", b""), rec(2, 1, "/b", b""), rec(3, 1, "/c", b"")],
        };
        let mut seen = Vec::new();
        let n = log.replay(|r| {
            seen.push(r.path.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 3);
        assert_eq!(seen, vec!["/a", "/b", "/c"]);
        // 中止弧：第二条失败
        let r = log.replay(|r| if r.seq == 2 { Err("boom".into()) } else { Ok(()) });
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
