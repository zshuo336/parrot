//! POC 核心：parrot-remote 的可运行最小实现（对应 05 文档章节）。
//!
//!   frame.rs     → §1  L0 帧格式（24B 头 + path + type_key + payload）
//!   transport.rs → § §2 L1 传输（内存对 / TCP 同一 FrameLink 抽象）
//!   codec.rs     → §3  编解码 registry（POC 显式注册）
//!   messages.rs  →     POC 消息集（跨语言 golden 共用 TYPE_KEY）
//!   remote_ref.rs→ §5  RemoteActorRef（ActorRef trait 实现）
//!   ingress.rs   → §6  入站路由（帧 → 本地真实 parrot actor）
//!   node.rs      → §7  RemoteNode 组装（endpoint_pair / spawn_endpoint）

mod frame;
mod transport;
mod codec;
mod messages;
mod remote_ref;
mod ingress;
mod node;
mod swim;
pub use frame::*;
pub use transport::*;
pub use codec::*;
pub use messages::*;
pub use remote_ref::*;
pub use ingress::*;
pub use node::*;
pub use swim::*;
