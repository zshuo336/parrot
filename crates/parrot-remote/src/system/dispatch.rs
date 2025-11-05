//! Bounded, scoped dispatch lanes keyed by the physical connection.
//!
//! Only operations that can await application work enter a lane. Requests and
//! replies can progress while that work waits. There is one future per active
//! lane, no spawned task, and no retained lane once its last operation finishes.
#[cfg(test)]
mod terminal_error_tests;

use std::{collections::HashMap, collections::VecDeque, sync::Arc};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use tokio::sync::mpsc;

use crate::{
    admin::sys_event_tag,
    frame::{frame_type, Frame},
    ingress::{
        unwind::{self, Cleanup},
        Ingress,
    },
    transport::{FrameSender, RuntimeKnobs},
};

type Input = (Frame, FrameSender, String);
struct Pending {
    frame: Frame,
    from: String,
    bytes: usize,
}
struct Lane {
    // Keeps the connection identity alive until all its accepted work is gone.
    back: FrameSender,
    queue: VecDeque<Pending>,
}
// Resolve waits can depend on Raft traffic from the same physical connection.
// A separate bounded lane prevents that dependency from deadlocking itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum LaneKind {
    Business,
    Resolve,
    Mirror,
}
type LaneKey = (usize, LaneKind);
type Completion = (LaneKey, usize);

fn lane_kind(frame: &Frame) -> LaneKind {
    if frame.header.frame_type == frame_type::RESOLVE_Q {
        LaneKind::Resolve
    } else if frame.header.frame_type == frame_type::SYSTEM_EVENT
        && frame.payload.first() == Some(&sys_event_tag::ADMIN_CMD_V2)
        // Read only the bounded, allocation-free enum discriminant, including
        // nonminimal varints accepted by the normal decoder. Full validation
        // and authorization remain in ingress. Mirror is appended variant 6.
        && bincode::decode_from_slice::<u32, _>(&frame.payload[1..], bincode::config::standard())
            .is_ok_and(|(variant, _)| variant == 6)
    {
        LaneKind::Mirror
    } else {
        LaneKind::Business
    }
}

fn is_slow(frame: &Frame) -> bool {
    match frame.header.frame_type {
        frame_type::TELL => frame.header.flags & crate::frame::flags::TELL_ACK == 0,
        frame_type::STOP => true,
        // Directory lookups are application supplied and may wait on Raft or
        // an external authority; they must never stall the connection's fast
        // dispatch loop.
        frame_type::RESOLVE_Q => true,
        frame_type::SYSTEM_EVENT => !matches!(
            frame.payload.first(),
            Some(&sys_event_tag::ADMIN_REPLY)
                | Some(&sys_event_tag::ADMIN_REPLY_V2)
                | Some(&sys_event_tag::MEMBERSHIP_PING)
                | Some(&sys_event_tag::MEMBERSHIP_PING_REQ)
                | Some(&sys_event_tag::MEMBERSHIP_PONG)
                | Some(&sys_event_tag::MEMBERSHIP_PING_REQ_ACK)
        ),
        _ => false,
    }
}

async fn dispatch(ingress: &Arc<Ingress>, frame: Frame, back: &FrameSender, from: &str) {
    let cleanup = Cleanup::default();
    let _close = cleanup.close_on_failure(back);
    if unwind::run(&cleanup, || Box::pin(ingress.dispatch(frame, back, from)))
        .await
        .is_err()
        || cleanup.failed()
    {
        tracing::warn!(node = %from, "inbound dispatch panicked; closing its connection");
        back.close();
    }
}

fn operation(
    ingress: Arc<Ingress>,
    back: FrameSender,
    key: LaneKey,
    pending: Pending,
) -> BoxFuture<'static, Completion> {
    Box::pin(async move {
        let Pending { frame, from, bytes } = pending;
        tokio::select! {
            biased;
            _ = back.closed() => {},
            _ = dispatch(&ingress, frame, &back, &from) => {},
        }
        (key, bytes)
    })
}

/// All futures and retained frames live inside the caller's tracked task.
/// Dropping this future cancels active lookups/delivery/control work immediately.
pub(super) async fn run(
    ingress: Arc<Ingress>,
    rx: &mut mpsc::Receiver<Input>,
    knobs: RuntimeKnobs,
) {
    let mut lanes: HashMap<LaneKey, Lane> = HashMap::new();
    let mut active: FuturesUnordered<BoxFuture<'static, Completion>> = FuturesUnordered::new();
    let mut count = 0usize;
    let mut bytes = 0usize;
    loop {
        tokio::task::consume_budget().await;
        tokio::select! {
            biased;
            Some((key, completed_bytes)) = active.next(), if !active.is_empty() => {
                count -= 1;
                bytes -= completed_bytes;
                let lane = lanes.get_mut(&key).expect("active dispatch lane");
                if !lane.back.is_closed() {
                    if let Some(next) = lane.queue.pop_front() {
                        active.push(operation(ingress.clone(), lane.back.clone(), key, next));
                        continue;
                    }
                }
                let lane = lanes.remove(&key).expect("completed dispatch lane");
                count -= lane.queue.len();
                bytes -= lane.queue.iter().map(|pending| pending.bytes).sum::<usize>();
            },
            input = rx.recv() => {
                let Some((frame, back, from)) = input else { return; };
                if back.is_closed() { continue; }
                if !is_slow(&frame) {
                    // The installed system hook handles management replies by
                    // completing a pending slot, without awaiting user work.
                    // Full decoding, envelope/source checks still run in ingress.
                    dispatch(&ingress, frame, &back, &from).await;
                    continue;
                }
                let connection = back.dispatch_key();
                let key = (connection, lane_kind(&frame));
                let connection_pending: usize = lanes.iter().filter(|(key, _)| key.0 == connection)
                    .map(|(_, lane)| lane.queue.len() + 1).sum();
                let size = frame.payload.len().checked_add(frame.path.len())
                    .and_then(|n| n.checked_add(frame.type_key.len()))
                    .and_then(|n| n.checked_add(from.len()));
                let fits = size.is_some_and(|size| size <= knobs.inbound_dispatch_bytes.saturating_sub(bytes))
                    && count < knobs.inbound_dispatch_capacity
                    && connection_pending < knobs.inbound_dispatch_per_connection;
                if !fits {
                    // Waiting here would recreate global HOL. Closing the exact
                    // offending connection makes refusal visible to pending
                    // callers and cancels its active/queued dispatch work.
                    tracing::warn!(node = %from, "inbound dispatch budget exhausted; closing its connection");
                    back.close();
                    continue;
                }
                let size = size.expect("checked dispatch size");
                count += 1;
                bytes += size;
                let pending = Pending { frame, from, bytes: size };
                if let Some(lane) = lanes.get_mut(&key) {
                    lane.queue.push_back(pending);
                } else {
                    active.push(operation(ingress.clone(), back.clone(), key, pending));
                    lanes.insert(key, Lane { back, queue: VecDeque::new() });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ingress::{LocalLookup, SysEventHook},
        registry::CallbackRegistry,
        security::ControlPolicy,
    };
    struct Dependency {
        entered: tokio::sync::Notify,
        progress: tokio::sync::Notify,
    }
    #[async_trait::async_trait]
    impl LocalLookup for Dependency {
        async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
            None
        }
        async fn resolve_directory(
            &self,
            _: &str,
            _: u64,
        ) -> Result<Option<crate::roles::directory::DirEntry>, String> {
            self.entered.notify_one();
            self.progress.notified().await;
            Ok(None)
        }
    }
    #[async_trait::async_trait]
    impl SysEventHook for Dependency {
        async fn on_event(&self, _: crate::admin::SysEvent, _: &FrameSender, _: &str) {
            self.progress.notify_one();
        }
    }
    #[tokio::test]
    async fn waiting_resolve_does_not_block_its_same_connection_control_dependency() {
        let dependency = Arc::new(Dependency {
            entered: tokio::sync::Notify::new(),
            progress: tokio::sync::Notify::new(),
        });
        let ingress = Arc::new(
            Ingress::new(dependency.clone(), Arc::new(CallbackRegistry::default()))
                .with_control_policy(ControlPolicy::InsecureDevelopment),
        );
        *ingress.sys_event_slot() = Some(dependency.clone());
        let (tx, mut rx) = mpsc::channel(4);
        let (writer, mut replies) = mpsc::channel(4);
        let source = FrameSender::new(writer, "source");
        let mut query = Frame::reply_err(17, "key", crate::ErrCode::Timeout, "");
        query.header.frame_type = frame_type::RESOLVE_Q;
        query.type_key = crate::resolve::RESPONSE_KEY.into();
        query.payload = crate::resolve::encode_query(&crate::resolve::Query {
            key: "key".into(),
            min_version: 0,
        })
        .unwrap()
        .into();
        let task = tokio::spawn(async move {
            run(ingress, &mut rx, RuntimeKnobs::default()).await;
        });
        tx.send((query.clone(), source.clone(), "source".into()))
            .await
            .unwrap();
        dependency.entered.notified().await;
        query.header.frame_type = frame_type::SYSTEM_EVENT;
        query.payload = bytes::Bytes::from_static(&[sys_event_tag::MEMBERSHIP_GOSSIP]);
        tx.send((query, source, "source".into())).await.unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(1), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.header.frame_type, frame_type::RESOLVE_R);
        assert!(crate::resolve::decode_response(&response.payload)
            .unwrap()
            .entry
            .is_none());
        drop(tx);
        task.await.unwrap();
    }

    struct MirrorWait {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        tell_lookup: tokio::sync::Notify,
    }
    struct Marker;
    parrot_api::message::inventory::submit! {
        parrot_api::message::CodecRegistration {
            type_key: "bin:mirror-dispatch-marker#v1",type_id:std::any::TypeId::of::<Marker>(),
            encode: |_| Ok(vec![]),decode: |_| Ok(Box::new(Marker) as parrot_api::types::BoxedMessage),
        }
    }
    #[async_trait::async_trait]
    impl LocalLookup for MirrorWait {
        async fn lookup(&self, path: &str) -> Option<parrot_api::types::BoxedActorRef> {
            assert_eq!(path, "/source");
            self.tell_lookup.notify_one();
            None
        }
    }
    #[async_trait::async_trait]
    impl SysEventHook for MirrorWait {
        async fn on_event(&self, event: crate::admin::SysEvent, _: &FrameSender, _: &str) {
            assert!(matches!(
                event,
                crate::admin::SysEvent::AdminCommandV2(
                    crate::admin_v2::AdminCommandV2::MirrorControl { .. }
                )
            ));
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
    #[tokio::test]
    async fn mirror_control_retirement_does_not_block_plain_tell_lookup_on_same_connection() {
        let waiting = Arc::new(MirrorWait {
            entered: Default::default(),
            release: Default::default(),
            tell_lookup: Default::default(),
        });
        let ingress = Arc::new(
            Ingress::new(waiting.clone(), Arc::new(CallbackRegistry::default()))
                .with_control_policy(ControlPolicy::InsecureDevelopment),
        );
        *ingress.sys_event_slot() = Some(waiting.clone());
        let (tx, mut rx) = mpsc::channel(4);
        let (writer, _replies) = mpsc::channel(4);
        let source = FrameSender::new(writer, "source");
        let mut frame = Frame::reply_err(42, "", crate::ErrCode::Timeout, "");
        frame.header.frame_type = frame_type::SYSTEM_EVENT;
        frame.payload =
            crate::admin_v2::encode_admin_cmd_v2(&crate::admin_v2::AdminCommandV2::MirrorControl {
                req_id: 42,
                command: crate::mirror_control::MirrorControl::Close {
                    session: crate::mirror_control::MirrorSessionId {
                        incarnation: [1; 16],
                        sequence: 1,
                    },
                },
            });
        let kept = ingress.clone();
        let task =
            tokio::spawn(async move { run(ingress, &mut rx, RuntimeKnobs::default()).await });
        tx.send((frame.clone(), source.clone(), "source".into()))
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            waiting.entered.notified(),
        )
        .await
        .unwrap();
        frame.header.frame_type = frame_type::TELL;
        frame.header.correlation_id = 0;
        frame.header.seq = crate::frame::SEQ_NONE;
        frame.path = "/source".into();
        frame.type_key = "bin:mirror-dispatch-marker#v1".into();
        frame.payload = bytes::Bytes::new();
        tx.send((frame, source, "source".into())).await.unwrap();
        let progress = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            waiting.tell_lookup.notified(),
        )
        .await;
        waiting.release.notify_one();
        drop(tx);
        task.await.unwrap();
        kept.close();
        kept.wait_closed().await;
        assert!(
            progress.is_ok(),
            "mirror retirement must not block ordinary business TELL dispatch"
        );
    }
}
