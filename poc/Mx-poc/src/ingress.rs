//! 入站路由（05 §6）：帧 → 本地真实 parrot actor。
//! ASK/TELL/STOP → facade get_actor(path) → 本地调用 → REPLY 回程。
//!
//! P4 联邦扩展：`PrefixRouter` 前缀路由表——查不到本地 actor 时按
//! 路径前缀把帧转发给另一条链路（hub 中继模式：网关两两互访的
//! 最小机制，回复沿原路自动折返）。

use crate::codec::CodecRegistry;
use crate::frame::{frame_type, Frame};
use crate::remote_ref::{CallbackRegistry, PendingRequest};
use crate::transport::FrameSender;
use parrot::system::ParrotActorSystem;
use parrot_api::address::ActorPath;
use parrot_api::system::ActorSystem as _;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 前缀路由表：`/erl` → 发往 erlang 网关的链路。查表命中即转发。
/// POC 边界：单跳（不做多级路由环路检测——正式版路由表来自集群拓扑）。
pub struct PrefixRouter {
    state: Mutex<RoutingState>,
}

#[derive(Default)]
struct RoutingState {
    routes: HashMap<String, OwnedRoute>,
    dead: HashMap<String, u64>,
}
struct OwnedRoute {
    route: Route,
    member: Option<(String, u64)>,
    _retirement: tokio::sync::watch::Sender<bool>,
}

/// Each route owns the destination connection's callback namespace. A reply
/// from another connection cannot complete its requests, even with the same CID.
#[derive(Clone)]
pub struct Route {
    pub sender: FrameSender,
    pub callbacks: Arc<CallbackRegistry>,
    retirement: tokio::sync::watch::Receiver<bool>,
}
impl Route {
    pub fn is_retired(&self) -> bool {
        *self.retirement.borrow() || self.retirement.has_changed().is_err()
    }
    async fn retired(&self) {
        let mut receiver = self.retirement.clone();
        let _ = receiver.wait_for(|retired| *retired).await;
    }
}
impl Default for PrefixRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl PrefixRouter {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RoutingState::default()),
        }
    }
    /// 注册前缀：`add("/erl", sender)` 后，`/erl/user/x` 的帧都走该链路。
    pub fn add(
        &self,
        prefix: impl Into<String>,
        sender: FrameSender,
        callbacks: Arc<CallbackRegistry>,
    ) {
        self.insert(None, prefix.into(), sender, callbacks).unwrap();
    }
    /// Bind a route to an observed member incarnation. Replacing the prefix
    /// retires the old route; a stale death cannot withdraw a newer binding.
    pub fn add_member(
        &self,
        node: &str,
        incarnation: u64,
        prefix: impl Into<String>,
        sender: FrameSender,
        callbacks: Arc<CallbackRegistry>,
    ) -> Result<(), crate::ClusterError> {
        self.insert(
            Some((node.into(), incarnation)),
            prefix.into(),
            sender,
            callbacks,
        )
    }
    fn insert(
        &self,
        member: Option<(String, u64)>,
        prefix: String,
        sender: FrameSender,
        callbacks: Arc<CallbackRegistry>,
    ) -> Result<(), crate::ClusterError> {
        let mut state = self.state.lock().unwrap();
        if member.as_ref().is_some_and(|(node, incarnation)| {
            state.dead.get(node).is_some_and(|dead| incarnation <= dead)
        }) {
            return Err(crate::ClusterError::UnavailableMember);
        }
        let (retirement, receiver) = tokio::sync::watch::channel(false);
        state.routes.insert(
            prefix,
            OwnedRoute {
                route: Route {
                    sender,
                    callbacks,
                    retirement: receiver,
                },
                member,
                _retirement: retirement,
            },
        );
        Ok(())
    }
    pub(crate) fn apply_membership(&self, members: &[crate::Member]) {
        let mut state = self.state.lock().unwrap();
        state.dead = members
            .iter()
            .filter(|m| m.status == crate::MemberStatus::Dead)
            .map(|m| (m.node_id.clone(), m.incarnation))
            .collect();
        let dead = state.dead.clone();
        state.routes.retain(|_, entry| {
            !entry.member.as_ref().is_some_and(|(node, incarnation)| {
                dead.get(node).is_some_and(|floor| incarnation <= floor)
            })
        });
    }
    /// 最长前缀匹配；命中返回出站链路。
    pub fn resolve(&self, path: &str) -> Option<Route> {
        let r = self.state.lock().unwrap();
        r.routes
            .iter()
            .filter(|(p, _)| {
                path == p.as_str()
                    || path
                        .strip_prefix(p.as_str())
                        .is_some_and(|tail| tail.starts_with('/'))
            })
            .max_by_key(|(p, _)| p.len())
            .map(|(_, s)| s.route.clone())
    }
}

/// 处理一帧入站（无路由——P1/P3 兼容路径）。
pub async fn handle_frame(
    f: Frame,
    local: &ParrotActorSystem,
    callbacks: &Arc<CallbackRegistry>,
    back: &FrameSender,
) {
    handle_frame_routed(f, local, callbacks, back, None).await
}

/// 带前缀路由的入站处理（P4）：本地未命中 → 前缀匹配 → 转发。
/// 转发用**新 cid**（避免与 hub 自身出站 ask 的 cid 撞车）；对端
/// REPLY 折返后按原 cid 回给原始请求方。
pub async fn handle_frame_routed(
    f: Frame,
    local: &ParrotActorSystem,
    callbacks: &Arc<CallbackRegistry>,
    back: &FrameSender,
    router: Option<&Arc<PrefixRouter>>,
) {
    // STOP 不中继（POC 边界：停止语义限于本节点）。
    if f.frame_type == frame_type::ASK || f.frame_type == frame_type::TELL {
        let local_hit = local
            .get_actor(&ActorPath::placeholder(&f.path))
            .await
            .is_some();
        if !local_hit {
            if let Some(r) = router {
                if let Some(out) = r.resolve(&f.path) {
                    relay_frame(f, back.clone(), out).await;
                    return;
                }
            }
        }
    }

    match f.frame_type {
        frame_type::ASK => {
            let decoded = CodecRegistry::decode_incoming(&f.type_key, &f.payload);
            match decoded {
                Err(e) => {
                    let _ = back
                        .send(Frame::reply_err(f.correlation_id, format!("decode: {e}")))
                        .await;
                }
                Ok(msg) => {
                    let found = local.get_actor(&ActorPath::placeholder(&f.path)).await;
                    match found {
                        None => {
                            let _ = back
                                .send(Frame::reply_err(
                                    f.correlation_id,
                                    format!("ActorNotFound: {}", f.path),
                                ))
                                .await;
                        }
                        Some(r) => {
                            // 远端不二次超时（05 §6.1 双超时竞态规避）
                            match r.send(msg).await {
                                Ok(reply) => match CodecRegistry::encode_outgoing(&reply) {
                                    Ok((key, payload)) => {
                                        let _ = back
                                            .send(Frame::reply(f.correlation_id, key, payload))
                                            .await;
                                    }
                                    Err(e) => {
                                        let _ = back
                                            .send(Frame::reply_err(
                                                f.correlation_id,
                                                format!("reply not remotable: {e}"),
                                            ))
                                            .await;
                                    }
                                },
                                Err(e) => {
                                    let _ = back
                                        .send(Frame::reply_err(f.correlation_id, e.to_string()))
                                        .await;
                                }
                            }
                        }
                    }
                }
            }
        }
        frame_type::TELL => {
            if let Ok(msg) = CodecRegistry::decode_incoming(&f.type_key, &f.payload) {
                if let Some(r) = local.get_actor(&ActorPath::placeholder(&f.path)).await {
                    // deliver 挂起 = 读循环挂起 = 对端背压（05 §6.3）
                    let _ = r.deliver(msg).await;
                }
                // ActorNotFound：静默（tell at-most-once 无回执）
            }
        }
        frame_type::STOP => {
            if let Some(r) = local.get_actor(&ActorPath::placeholder(&f.path)).await {
                let _ = r.stop().await;
            }
        }
        frame_type::REPLY | frame_type::REPLY_ERR => {
            callbacks.complete(f);
        }
        other => {
            eprintln!("[ingress] unhandled frame_type={other:#x}");
        }
    }
}

/// 中继一帧：换新 cid 发往出站链路；REPLY 折返映射回原 cid 后从 back 回给请求方。
async fn relay_frame(f: Frame, back: FrameSender, out: Route) {
    if f.frame_type == frame_type::TELL {
        tokio::select! {
            biased;
            _ = out.retired() => {},
            _ = out.sender.send(f) => {},
        }
        return;
    }
    let callbacks = &out.callbacks;
    let orig_cid = f.correlation_id;
    let new_cid = callbacks.next_cid();
    let (tx, rx) = tokio::sync::oneshot::channel::<Frame>();
    callbacks.insert(new_cid, tx);
    let _pending = PendingRequest::new(callbacks, new_cid);
    let mut relayed = f;
    relayed.correlation_id = new_cid;
    let exchange = async {
        out.sender.send(relayed).await?;
        rx.await.map_err(|_| "relay: connection lost".to_string())
    };
    let mut reply = tokio::select! {
        biased;
        _ = out.retired() => Frame::reply_err(orig_cid, "relay: route retired"),
        result = tokio::time::timeout(std::time::Duration::from_secs(8), exchange) => match result {
            Ok(Ok(reply)) => reply,
            Ok(Err(error)) => Frame::reply_err(orig_cid, error),
            Err(_) => Frame::reply_err(orig_cid, "relay: timeout"),
        },
    };
    reply.correlation_id = orig_cid;
    let _ = back.send(reply).await;
}
