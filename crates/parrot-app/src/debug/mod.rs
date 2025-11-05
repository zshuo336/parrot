//! F 阶段（DEV_09 §3.6）调试五件套之 Rust 侧：
//!
//! - [`trace`]（F1 的部分数据面）：消费 PARROT_TRACE=frame 文本，按 cid/path
//!   分组并选择最小 hop 路径为根。文本投影仍不等于因果 span；宿主 run --trace-otlp 使用因果记录与实际 HTTP 导出。
//! - [`otlp`]：导出显式本地完成快照为有界 OTLP JSON，保留身份与丢失诊断。
//! - [`replay`]（F3）：有界自动 Wire 录制 / 按 seq 顺序回放到本地 actor
//!
//! F2 镜像 actor 在 parrot 主 crate（mirror.rs 与 mirror/runtime.rs）；
//! F4 孪生门禁在 tools/federation-lab（twin.rs）。

pub mod export_worker;
mod history;
pub use history::{TraceHistory, TraceHistorySnapshot};
pub mod otlp;
pub use export_worker::{
    TraceExportAttachError, TraceExportBatch, TraceExportConfig, TraceExportEnd,
    TraceExportFailure, TraceExportJoinError, TraceExportReport, TraceExportStartError,
    TraceExportStatus, TraceExporter,
};
pub use otlp::{export_otlp_json, TraceExportError, MAX_OTLP_JSON_BYTES, MAX_SERVICE_NAME_BYTES};

pub mod replay;
pub mod trace;

pub use replay::{
    record_sink, FileSink, JsonlRecord, MemSink, RecordSink, Recording, RecordingReport,
    ReplayDirection, ReplayError, ReplayLimits, ReplayLog, ReplayOptions, ReplayReply,
    ReplayReport, ReplayTellMode,
};
pub use trace::{
    aggregate as aggregate_traces, parse_trace_line as parse_trace, SpanEvent, SpanNode, SpanTree,
};
