//! 入站路由（05 §6）：帧 → 本地真实 parrot actor。
//! ASK/TELL/STOP → facade get_actor(path) → 本地调用 → REPLY 回程。
//!
//! P4 联邦扩展：`PrefixRouter` 前缀路由表——查不到本地 actor 时按
//! 路径前缀把帧转发给另一条链路（hub 中继模式：网关两两互访的
//! 最小机制，回复沿原路自动折返）。

use crate::codec::CodecRegistry;
use crate::frame::{frame_type, Frame};
use crate::remote_ref::CallbackRegistry;
use crate::transport::FrameSender;
use parrot::system::ParrotActorSystem;
use parrot_api::address::ActorPath;
use parrot_api::system::ActorSystem as _;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 前缀路由表：`/erl` → 发往 erlang 网关的链路。查表命中即转发。
/// POC 边界：单跳（不做多级路由环路检测——正式版路由表来自集群拓扑）。
pub struct PrefixRouter {
    routes: Mutex<HashMap<String, FrameSender>>,
}

impl PrefixRouter {
    pub fn new() -> Self {
        Self { routes: Mutex::new(HashMap::new()) }
    }
    /// 注册前缀：`add("/erl", sender)` 后，`/erl/user/x` 的帧都走该链路。
    pub fn add(&self, prefix: impl Into<String>, sender: FrameSender) {
        self.routes.lock().unwrap().insert(prefix.into(), sender);
    }
    /// 最长前缀匹配；命中返回出站链路。
    pub fn resolve(&self, path: &str) -> Option<FrameSender> {
        let r = self.routes.lock().unwrap();
        r.iter()
            .filter(|(p, _)| path.starts_with(&p[..]))
            .max_by_key(|(p, _)| p.len())
            .map(|(_, s)| s.clone())
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
                    relay_frame(f, callbacks, back.clone(), out).await;
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
async fn relay_frame(
    f: Frame,
    callbacks: &Arc<CallbackRegistry>,
    back: FrameSender,
    out: FrameSender,
) {
    let orig_cid = f.correlation_id;
    let new_cid = callbacks.next_cid();
    let (tx, rx) = tokio::sync::oneshot::channel::<Frame>();
    callbacks.insert(new_cid, tx);
    let mut relayed = f.clone();
    relayed.correlation_id = new_cid;

    if out.send(relayed).await.is_err() {
        callbacks.remove(new_cid);
        if f.frame_type == frame_type::ASK {
            let _ = back
                .send(Frame::reply_err(orig_cid, "relay: outbound link down"))
                .await;
        }
        return;
    }

    // TELL 无回执，清表直接返回；ASK 等折返。
    if f.frame_type != frame_type::ASK {
        callbacks.remove(new_cid);
        return;
    }
    match tokio::time::timeout(std::time::Duration::from_secs(8), rx).await {
        Ok(Ok(reply)) => {
            let mut orig_reply = reply;
            orig_reply.correlation_id = orig_cid;
            let _ = back.send(orig_reply).await;
        }
        _ => {
            callbacks.remove(new_cid);
            let _ = back.send(Frame::reply_err(orig_cid, "relay: timeout")).await;
        }
    }
}
