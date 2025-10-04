//! 入站路由（05 §6）：帧 → 本地真实 parrot actor。
//! ASK/TELL/STOP → facade get_actor(path) → 本地调用 → REPLY 回程。

use crate::codec::CodecRegistry;
use crate::frame::{frame_type, Frame};
use crate::remote_ref::CallbackRegistry;
use crate::transport::FrameSender;
use parrot::system::ParrotActorSystem;
use parrot_api::address::ActorPath;
use parrot_api::system::ActorSystem as _;
use std::sync::Arc;

/// 处理一帧入站。
pub async fn handle_frame(
    f: Frame,
    local: &ParrotActorSystem,
    callbacks: &Arc<CallbackRegistry>,
    back: &FrameSender,
) {
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
