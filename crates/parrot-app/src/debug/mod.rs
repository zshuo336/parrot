//! F 阶段（DEV_09 §3.6）调试五件套之 Rust 侧：
//!
//! - [`trace`]（F1）：frame trace_line JSONL 消费 → cid→span 树聚合
//!   （`parrot app trace <app>` 数据面；帧产生端 PARROT_TRACE=frame）
//! - [`replay`]（F3）：record（JSONL 落盘）/ replay（按 cid 序重放）
//!
//! F2 镜像 actor 在 parrot 主 crate（system.rs prefix_handlers 锚点）；
//! F4 孪生门禁在 tools/federation-lab（twin.rs）。

pub mod replay;
pub mod trace;

pub use replay::{record_sink, JsonlRecord, ReplayError, ReplayLog};
pub use trace::{SpanEvent, SpanNode, SpanTree};
