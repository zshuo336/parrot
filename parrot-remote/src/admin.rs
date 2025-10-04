//! 职责：远程 spawn/stop 管理协议——SYSTEM_EVENT 管理载荷 + 目标节点 AdminService（DEV_02 §0）。
//!
//! 权限域：运维面（证书 role=admin 校验——K4 mTLS 后启用；P2 前期 mem/tcp
//! 信任连接边界），与 receptionist ACL（数据面）不同轨。
//!
//! PropsRef 原则（DEV_02 §0.2）：actor 本体（闭包/结构体）不可序列化，
//! 跨线传"构造器注册名"——inventory 自注册，与 RemoteMessage 宏同族机制。

use std::sync::Arc;

use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture};

use crate::error::{decode_err_payload, encode_err_payload, ErrCode, RemoteError};
use crate::frame::{frame_type, Frame};
use crate::ingress::LocalLookup;

/// SYSTEM_EVENT (0x20) payload 第三形态（前两形态：MembershipGossip/ReceptionistSync
/// ——K1/K2 各自定义，本模块只管 admin 标签）。
///
/// 布局：`[u8 tag][bincode(body)]`；tag: 0x01=AdminCommand, 0x02=AdminReply,
/// 0x10=MembershipGossip(K1), 0x20=ReceptionistSync(K2)。
pub mod sys_event_tag {
    pub const ADMIN_CMD: u8 = 0x01;
    pub const ADMIN_REPLY: u8 = 0x02;
    pub const MEMBERSHIP_GOSSIP: u8 = 0x10;
    pub const RECEPTIONIST_SYNC: u8 = 0x20;
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AdminCommand {
    /// 目标节点收到后在本节点执行 spawn（target_node=自己；非自己则拒绝）。
    SpawnLocal {
        req_id: u64,
        props: String,
        path: String,
        reply_to: String,
    },
    /// 管理性远程 stop（比数据面 STOP 帧多回执）。
    AdminStop {
        req_id: u64,
        target_path: String,
        reply_to: String,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AdminReply {
    Spawned { req_id: u64, path: String },
    Failed { req_id: u64, code: u16, detail: String },
    Stopped { req_id: u64 },
}

impl AdminCommand {
    pub fn set_req_id(&mut self, id: u64) {
        match self {
            AdminCommand::SpawnLocal { req_id, .. } | AdminCommand::AdminStop { req_id, .. } => {
                *req_id = id;
            }
        }
    }

    pub fn req_id(&self) -> u64 {
        match self {
            AdminCommand::SpawnLocal { req_id, .. } | AdminCommand::AdminStop { req_id, .. } => {
                *req_id
            }
        }
    }
}

pub fn encode_admin_cmd(cmd: &AdminCommand) -> bytes::Bytes {
    let mut b = bytes::BytesMut::new();
    b.extend_from_slice(&[sys_event_tag::ADMIN_CMD]);
    b.extend_from_slice(&bincode::serde::encode_to_vec(cmd, bincode::config::standard()).unwrap());
    b.freeze()
}

pub fn encode_admin_reply(r: &AdminReply) -> bytes::Bytes {
    let mut b = bytes::BytesMut::new();
    b.extend_from_slice(&[sys_event_tag::ADMIN_REPLY]);
    b.extend_from_slice(&bincode::serde::encode_to_vec(r, bincode::config::standard()).unwrap());
    b.freeze()
}

/// 解 SYSTEM_EVENT payload 首字节分发（admin/gossip/receptionist 同帧不同标签）。
pub fn decode_sys_event(payload: &[u8]) -> Result<SysEvent, RemoteError> {
    let Some((&tag, body)) = payload.split_first() else {
        return Err(RemoteError::Codec("empty SYSTEM_EVENT payload".into()));
    };
    match tag {
        sys_event_tag::ADMIN_CMD => Ok(SysEvent::AdminCommand(
            bincode::serde::decode_from_slice(body, bincode::config::standard())
                .map_err(|e| RemoteError::Codec(format!("admin cmd decode: {e}")))?
                .0,
        )),
        sys_event_tag::ADMIN_REPLY => Ok(SysEvent::AdminReply(
            bincode::serde::decode_from_slice(body, bincode::config::standard())
                .map_err(|e| RemoteError::Codec(format!("admin reply decode: {e}")))?
                .0,
        )),
        sys_event_tag::MEMBERSHIP_GOSSIP => Ok(SysEvent::MembershipGossip(body.to_vec().into())),
        sys_event_tag::RECEPTIONIST_SYNC => Ok(SysEvent::ReceptionistSync(body.to_vec().into())),
        other => Err(RemoteError::Codec(format!(
            "unknown SYSTEM_EVENT tag 0x{other:02X}"
        ))),
    }
}

/// SYSTEM_EVENT 载荷四形态统一视图（ingress 分发用）。
#[derive(Debug, Clone)]
pub enum SysEvent {
    AdminCommand(AdminCommand),
    AdminReply(AdminReply),
    MembershipGossip(bytes::Bytes),
    ReceptionistSync(bytes::Bytes),
}

/// 本地可远程 spawn 的工厂（inventory 自注册——DEV_02 §0.2）。
pub struct PropsFactory {
    pub name: &'static str,
    /// 在当前节点 spawn 一个 actor，返回其 ref（路径由调用方指定）。
    pub spawn: fn(path: &str) -> BoxedFuture<'static, ActorResult<BoxedActorRef>>,
}

parrot_api::message::inventory::collect!(PropsFactory);

/// 按名查工厂（目标节点侧）。
pub fn find_factory(name: &str) -> Option<&'static PropsFactory> {
    parrot_api::message::inventory::iter::<PropsFactory>()
        .find(|f| f.name == name)
}

/// admin 回执路由：req_id → oneshot（发起方挂起等待）。
#[derive(Default)]
pub struct AdminPending {
    inner: std::sync::Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<AdminReply>>>,
}

impl AdminPending {
    pub fn insert(&self, req_id: u64, tx: tokio::sync::oneshot::Sender<AdminReply>) {
        self.inner.lock().unwrap().insert(req_id, tx);
    }
    pub fn complete(&self, r: AdminReply) -> bool {
        self.inner.lock().unwrap().remove(&r.req_id()).is_some_and(|tx| tx.send(r).is_ok())
    }
    pub fn fail_all(&self, detail: &str) {
        for (_, tx) in self.inner.lock().unwrap().drain() {
            let _ = tx.send(AdminReply::Failed {
                req_id: 0,
                code: ErrCode::ConnectionLost as u16,
                detail: detail.into(),
            });
        }
    }
}

impl AdminReply {
    pub fn req_id(&self) -> u64 {
        match self {
            AdminReply::Spawned { req_id, .. }
            | AdminReply::Failed { req_id, .. }
            | AdminReply::Stopped { req_id } => *req_id,
        }
    }
}

/// 目标节点侧：处理入站 AdminCommand（ingress SYSTEM_EVENT 分支调起）。
pub async fn handle_admin_command(
    cmd: AdminCommand,
    local: &Arc<dyn LocalLookup>,
    back: &crate::transport::FrameSender,
) {
    match cmd {
        AdminCommand::SpawnLocal {
            req_id,
            props,
            path,
            reply_to,
        } => {
            let reply = match find_factory(&props) {
                None => AdminReply::Failed {
                    req_id,
                    code: ErrCode::NotRemotable as u16,
                    detail: format!("props '{props}' not registered"),
                },
                Some(f) => match (f.spawn)(&path).await {
                    Ok(_) => AdminReply::Spawned { req_id, path },
                    Err(e) => AdminReply::Failed {
                        req_id,
                        code: ErrCode::ProtocolViolation as u16,
                        detail: format!("spawn failed: {e}"),
                    },
                },
            };
            let _ = back
                .send(Frame {
                    header: crate::frame::FrameHeader {
                        frame_len: 0,
                        version: crate::frame::PROTOCOL_VERSION,
                        frame_type: frame_type::SYSTEM_EVENT,
                        flags: 0,
                        correlation_id: req_id,
                        hop_count: 0,
                        hop_limit: 8,
                    },
                    path: reply_to,
                    type_key: String::new(),
                    payload: encode_admin_reply(&reply),
                })
                .await;
        }
        AdminCommand::AdminStop {
            req_id,
            target_path,
            reply_to,
        } => {
            let reply = match local.lookup(&target_path).await {
                Some(r) => match r.stop().await {
                    Ok(()) => AdminReply::Stopped { req_id },
                    Err(e) => AdminReply::Failed {
                        req_id,
                        code: ErrCode::ProtocolViolation as u16,
                        detail: format!("stop failed: {e}"),
                    },
                },
                None => AdminReply::Failed {
                    req_id,
                    code: ErrCode::ActorNotFound as u16,
                    detail: target_path,
                },
            };
            let _ = back
                .send(Frame {
                    header: crate::frame::FrameHeader {
                        frame_len: 0,
                        version: crate::frame::PROTOCOL_VERSION,
                        frame_type: frame_type::SYSTEM_EVENT,
                        flags: 0,
                        correlation_id: req_id,
                        hop_count: 0,
                        hop_limit: 8,
                    },
                    path: reply_to,
                    type_key: String::new(),
                    payload: encode_admin_reply(&reply),
                })
                .await;
        }
    }
}

/// 发起方侧：SYSTEM_EVENT 回执帧 → AdminPending（cid 即 req_id）。
pub fn handle_admin_reply_frame(frame: &Frame) -> Result<Option<AdminReply>, RemoteError> {
    if frame.header.frame_type != frame_type::SYSTEM_EVENT {
        return Ok(None);
    }
    match decode_sys_event(&frame.payload)? {
        SysEvent::AdminReply(r) => Ok(Some(r)),
        _ => Ok(None),
    }
}

/// 便捷：非 admin 语义拒绝（K4 mTLS 前占位——权限检查钩子）。
pub fn admin_allowed(_from_node: &str) -> bool {
    // P2 前期：信任连接边界（mem/tcp 均为显式配置对端）。
    // K4 后：证书 role=admin 校验，不匹配 → false → 回 REPLY_ERR(Forbidden)。
    true
}

/// 错误体解码重导出（admin 路径校验测试用）。
pub fn decode_err(code_detail: &[u8]) -> Result<(ErrCode, String), String> {
    decode_err_payload(code_detail)
}

pub fn encode_err(code: ErrCode, detail: &str) -> Vec<u8> {
    encode_err_payload(code, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_cmd_roundtrip() {
        let cmd = AdminCommand::SpawnLocal {
            req_id: 7,
            props: "crawler.v2".into(),
            path: "/user/c1".into(),
            reply_to: "parrot://n1/_admin".into(),
        };
        let p = encode_admin_cmd(&cmd);
        assert_eq!(p[0], sys_event_tag::ADMIN_CMD);
        match decode_sys_event(&p).unwrap() {
            SysEvent::AdminCommand(got) => assert_eq!(got, cmd),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn admin_reply_roundtrip() {
        let r = AdminReply::Failed {
            req_id: 9,
            code: ErrCode::NotRemotable as u16,
            detail: "props 'x' not registered".into(),
        };
        let p = encode_admin_reply(&r);
        match decode_sys_event(&p).unwrap() {
            SysEvent::AdminReply(got) => assert_eq!(got, r),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn sys_event_unknown_tag_rejected() {
        let err = decode_sys_event(&[0x99, 0, 0]).unwrap_err();
        assert!(err.to_string().contains("unknown SYSTEM_EVENT tag"));
    }

    #[test]
    fn empty_payload_rejected() {
        assert!(decode_sys_event(&[]).is_err());
    }

    // PropsFactory inventory 注册与查找
    fn noop_spawn(
        _path: &str,
    ) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
        Box::pin(async { Err(parrot_api::errors::ActorError::InternalError("noop".into())) })
    }

    parrot_api::message::inventory::submit! {
        PropsFactory {
            name: "test.noop",
            spawn: noop_spawn,
        }
    }

    #[test]
    fn factory_lookup() {
        assert!(find_factory("test.noop").is_some());
        assert!(find_factory("nowhere").is_none());
    }

    #[test]
    fn pending_complete_routing() {
        let p = AdminPending::default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        p.insert(42, tx);
        assert!(p.complete(AdminReply::Stopped { req_id: 42 }));
        assert!(matches!(rx.blocking_recv(), Ok(AdminReply::Stopped { req_id: 42 })));
        // 迟到回执 false
        assert!(!p.complete(AdminReply::Stopped { req_id: 42 }));
    }
}
