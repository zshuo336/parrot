//! # parrot-remote
//!
//! Parrot 远程层：Wire 1.0 帧 + 传输 + 编解码注册表 + 远程 ref（DEV_01）。
//!
//! 分层（E5.2）：本 crate 只依赖 `parrot-api`；与 `parrot` facade 的集成经
//! `RemoteGatewayImpl`（应用层组装注入）。所有布局/语义以 07 协议 1.0 为准。

pub mod acl;
pub mod admin;
pub mod cache;
pub mod codec;
pub mod codec_registry;
pub mod durable;
pub mod envelope;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod ingress;
pub mod livekit;
pub mod mqtt;
pub mod node;
pub mod pb;
pub mod raft;
pub mod receptionist;
pub mod ref_;
pub mod registry;
pub mod roles;
pub mod sharding;
pub mod singleton;
pub mod swim;
pub mod system;
pub mod tls;
pub mod topology;
pub mod transport;

pub mod transport_mem {
    //! mem 载体始终可用（测试确定性；不设 feature 门）
    pub use crate::transport::memory::*;
}

pub use bytes;
pub use error::{ErrCode, RemoteError};
pub use frame::{golden_vectors, Frame, FrameError, FrameHeader, GoldenVector};
pub use handshake::{negotiate_caps, HandshakeAckBody, HandshakeBody, TopologyRole};
pub use ingress::{LocalLookup, RelayMetrics, DEAD_TELL_DROPPED};
pub use node::{NodeAddr, NodeId, NodeState, NodeStatus, NodeTable};
pub use ref_::{RemoteActorRef, RemoteInner};
pub use registry::{CallbackRegistry, ReplyPayload};
pub use system::{RemoteActorSystem, RemoteConfig, RemoteGatewayImpl};
pub use transport::{
    FrameSender, Transport, HEARTBEAT_INTERVAL, HEARTBEAT_MAX_LOSS, LATE_REPLY_DROPPED,
};
