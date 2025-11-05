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
/// v2（DEV_09 B1）：0x03=AdminCommandV2, 0x04=AdminReplyV2（组件级部署协议）。
pub mod sys_event_tag {
    pub const ADMIN_CMD: u8 = 0x01;
    pub const ADMIN_REPLY: u8 = 0x02;
    pub const ADMIN_CMD_V2: u8 = 0x03;
    pub const ADMIN_REPLY_V2: u8 = 0x04;
    pub const MEMBERSHIP_GOSSIP: u8 = 0x10;
    pub const MEMBERSHIP_PING: u8 = 0x11;
    pub const MEMBERSHIP_PING_REQ: u8 = 0x12;
    pub const MEMBERSHIP_PONG: u8 = 0x13;
    pub const MEMBERSHIP_PING_REQ_ACK: u8 = 0x14;
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
    Spawned {
        req_id: u64,
        path: String,
    },
    Failed {
        req_id: u64,
        code: u16,
        detail: String,
    },
    Stopped {
        req_id: u64,
    },
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
            parrot_api::message::serde_remote_deserialize(body)
                .map_err(|e| RemoteError::Codec(format!("admin cmd decode: {e}")))?,
        )),
        sys_event_tag::ADMIN_REPLY => Ok(SysEvent::AdminReply(
            parrot_api::message::serde_remote_deserialize(body)
                .map_err(|e| RemoteError::Codec(format!("admin reply decode: {e}")))?,
        )),
        sys_event_tag::MEMBERSHIP_GOSSIP => Ok(SysEvent::MembershipGossip(body.to_vec().into())),
        tag @ (sys_event_tag::MEMBERSHIP_PING
        | sys_event_tag::MEMBERSHIP_PING_REQ
        | sys_event_tag::MEMBERSHIP_PONG
        | sys_event_tag::MEMBERSHIP_PING_REQ_ACK) => Ok(SysEvent::MembershipProbe(
            crate::swim::ProbeEvent::decode(tag, body)?,
        )),
        sys_event_tag::RECEPTIONIST_SYNC => Ok(SysEvent::ReceptionistSync(body.to_vec().into())),
        sys_event_tag::ADMIN_CMD_V2 => Ok(SysEvent::AdminCommandV2(
            parrot_api::message::serde_remote_deserialize(body)
                .map_err(|e| RemoteError::Codec(format!("admin cmd v2 decode: {e}")))?,
        )),
        sys_event_tag::ADMIN_REPLY_V2 => Ok(SysEvent::AdminReplyV2(
            parrot_api::message::serde_remote_deserialize(body)
                .map_err(|e| RemoteError::Codec(format!("admin reply v2 decode: {e}")))?,
        )),
        other => Err(RemoteError::Codec(format!(
            "unknown SYSTEM_EVENT tag 0x{other:02X}"
        ))),
    }
}

/// SYSTEM_EVENT 载荷形态统一视图（ingress 分发用）。
#[derive(Debug, Clone)]
pub enum SysEvent {
    AdminCommand(AdminCommand),
    AdminReply(AdminReply),
    /// v2（DEV_09 B1）——组件级部署协议（admin_v2 模块）。
    AdminCommandV2(crate::admin_v2::AdminCommandV2),
    AdminReplyV2(crate::admin_v2::AdminReplyV2),
    MembershipGossip(bytes::Bytes),
    MembershipProbe(crate::swim::ProbeEvent),
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
    parrot_api::message::inventory::iter::<PropsFactory>().find(|f| f.name == name)
}

/// Bounded administrative callbacks with optional physical connection binding.
pub type AdminPending = crate::admin_pending::Pending<AdminReply>;

impl AdminReply {
    pub fn req_id(&self) -> u64 {
        match self {
            AdminReply::Spawned { req_id, .. }
            | AdminReply::Failed { req_id, .. }
            | AdminReply::Stopped { req_id } => *req_id,
        }
    }
}

/// Build a typed management response; request IDs must agree with the envelope.
pub fn reply_frame(req_id: u64, reply_to: &str, reply: &AdminReply) -> Frame {
    Frame {
        header: crate::frame::FrameHeader {
            frame_len: 0,
            version: crate::frame::PROTOCOL_VERSION,
            frame_type: frame_type::SYSTEM_EVENT,
            flags: 0,
            correlation_id: req_id,
            hop_count: 0,
            hop_limit: 8,
            seq: crate::frame::SEQ_NONE,
        },
        path: reply_to.into(),
        type_key: String::new(),
        payload: encode_admin_reply(reply),
    }
}

/// 目标节点侧：处理入站 AdminCommand（ingress SYSTEM_EVENT 分支调起）。
pub async fn handle_admin_command(
    cmd: AdminCommand,
    local: &Arc<dyn LocalLookup>,
    back: &crate::transport::FrameSender,
) {
    use crate::ingress::unwind::{self, Cleanup};
    let cleanup = Cleanup::default();
    // Cancellation cannot send a terminal reply. If its cleanup fails, close
    // this connection; a normally completed failure reply disarms this guard.
    let mut close = cleanup.close_on_failure(back);
    let (req_id, reply_to, reply) = match cmd {
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
                Some(factory) => match unwind::run(&cleanup, || (factory.spawn)(&path)).await {
                    Ok(Ok(actor)) => {
                        cleanup.dispose(actor);
                        AdminReply::Spawned { req_id, path }
                    }
                    Ok(Err(error)) => admin_failure(&cleanup, req_id, "spawn", error),
                    Err(()) => admin_panic(req_id, "spawn handler panicked"),
                },
            };
            (req_id, reply_to, reply)
        }
        AdminCommand::AdminStop {
            req_id,
            target_path,
            reply_to,
        } => {
            let reply = match unwind::run(&cleanup, || local.lookup(&target_path)).await {
                Ok(Some(actor)) => {
                    let mut actor = cleanup.owned(actor);
                    let reply = if cleanup.failed() {
                        admin_panic(req_id, "admin lookup cleanup panicked")
                    } else {
                        match unwind::run(&cleanup, || actor.value().stop()).await {
                            Ok(Ok(())) => AdminReply::Stopped { req_id },
                            Ok(Err(error)) => admin_failure(&cleanup, req_id, "stop", error),
                            Err(()) => admin_panic(req_id, "admin stop handler panicked"),
                        }
                    };
                    actor.retire();
                    reply
                }
                Ok(None) if !cleanup.failed() => AdminReply::Failed {
                    req_id,
                    code: ErrCode::ActorNotFound as u16,
                    detail: target_path,
                },
                _ => admin_panic(req_id, "admin lookup panicked"),
            };
            (req_id, reply_to, reply)
        }
    };
    let reply = if cleanup.failed() && !matches!(reply, AdminReply::Failed { .. }) {
        admin_panic(req_id, "administrative request cleanup panicked")
    } else {
        reply
    };
    close.disarm();
    back.send_reply(reply_frame(req_id, &reply_to, &reply));
}

fn admin_panic(req_id: u64, detail: &str) -> AdminReply {
    AdminReply::Failed {
        req_id,
        code: ErrCode::ProtocolViolation as u16,
        detail: detail.into(),
    }
}
fn admin_failure(
    cleanup: &crate::ingress::unwind::Cleanup,
    req_id: u64,
    operation: &str,
    error: parrot_api::errors::ActorError,
) -> AdminReply {
    let mut error = cleanup.owned(error);
    let detail = cleanup
        .call(|| format!("{operation} failed: {}", error.value()))
        .unwrap_or_else(|()| format!("{operation} error formatter panicked"));
    error.retire();
    admin_panic(req_id, &detail)
}

/// 发起方侧：SYSTEM_EVENT 回执帧 → AdminPending（cid 即 req_id）。
pub fn handle_admin_reply_frame(frame: &Frame) -> Result<Option<AdminReply>, RemoteError> {
    if frame.header.frame_type != frame_type::SYSTEM_EVENT {
        return Ok(None);
    }
    match decode_sys_event(&frame.payload)? {
        SysEvent::AdminReply(r) if r.req_id() == frame.header.correlation_id => Ok(Some(r)),
        SysEvent::AdminReply(_) => Err(RemoteError::Codec(
            "admin reply correlation_id disagrees with req_id".into(),
        )),
        _ => Ok(None),
    }
}

/// Authorize using the transport-verified peer identity, never a claimed name alone.
pub fn admin_allowed(
    policy: &crate::security::ControlPolicy,
    peer: &crate::transport::FrameSender,
) -> bool {
    policy.allows(peer)
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
    fn admin_decode_rejects_trailing_bytes_for_all_four_tags() {
        let mut payloads = vec![
            encode_admin_cmd(&AdminCommand::AdminStop {
                req_id: 1,
                target_path: "/target".into(),
                reply_to: String::new(),
            }),
            encode_admin_reply(&AdminReply::Stopped { req_id: 2 }),
            crate::admin_v2::encode_admin_cmd_v2(&crate::admin_v2::AdminCommandV2::StopComponent {
                req_id: 3,
                path_prefix: "/target".into(),
            }),
            crate::admin_v2::encode_admin_reply_v2(&crate::admin_v2::AdminReplyV2::Stopped {
                req_id: 4,
            }),
        ];
        for payload in payloads.drain(..) {
            assert!(decode_sys_event(&payload).is_ok());
            let mut extra = payload.to_vec();
            extra.push(0);
            assert!(
                decode_sys_event(&extra).is_err(),
                "tag {} accepted trailing bytes",
                extra[0]
            );
        }
    }

    #[test]
    fn admin_decode_rejects_impossible_string_length_without_panicking() {
        let mut payload = encode_admin_reply(&AdminReply::Spawned {
            req_id: 1,
            path: String::new(),
        })
        .to_vec();
        assert_eq!(payload.pop(), Some(0));
        payload
            .extend(bincode::serde::encode_to_vec(u64::MAX, bincode::config::standard()).unwrap());
        let result = std::panic::catch_unwind(|| decode_sys_event(&payload));
        assert!(
            result.is_ok(),
            "untrusted admin length panicked before validating input"
        );
        assert!(result.unwrap().is_err());
    }

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
    fn noop_spawn(_path: &str) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
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
        p.insert(42, tx).unwrap();
        assert!(p.complete(AdminReply::Stopped { req_id: 42 }));
        assert!(matches!(
            rx.blocking_recv(),
            Ok(AdminReply::Stopped { req_id: 42 })
        ));
        // 迟到回执 false
        assert!(!p.complete(AdminReply::Stopped { req_id: 42 }));
    }
}
