//! Actual membership probes over authorized, established physical links.
use super::RemoteActorSystem;
use crate::{
    handshake::caps,
    swim::{ProbeEvent, ProbeOutcome, ProbeReply, ProbeRequest},
    transport::FrameSender,
};
use futures::{stream::FuturesUnordered, StreamExt};
use std::{collections::HashSet, sync::Weak, time::Duration};
use tokio::time::Instant;

fn eligible(system: &RemoteActorSystem, sender: &FrameSender) -> bool {
    !sender.is_closed()
        && sender.peer_capabilities() & caps::MEMBERSHIP_PROBES != 0
        && system
            .config
            .security
            .control_policy()
            .allows_cluster(sender)
}

pub(super) async fn observe(weak: Weak<RemoteActorSystem>, target: String) -> ProbeOutcome {
    let (state, direct, helpers, total, shutdown) = {
        let Some(system) = weak.upgrade() else {
            return ProbeOutcome::Unavailable;
        };
        if *system.shutdown_tx.borrow() || !system.config.membership_gossip_enabled() {
            return ProbeOutcome::Unavailable;
        }
        let direct = system
            .direct_sender_of(&target)
            .filter(|sender| eligible(&system, sender));
        let mut seen = HashSet::new();
        let mut helpers = system
            .links
            .read()
            .unwrap()
            .iter()
            .rev()
            .filter(|(id, sender, _)| {
                id != &target
                    && id != &system.config.node_id
                    && eligible(&system, sender)
                    && seen.insert(id.clone())
            })
            .map(|(id, sender, _)| (id.clone(), sender.clone()))
            .collect::<Vec<_>>();
        helpers.sort_by(|a, b| a.0.cmp(&b.0));
        if !helpers.is_empty() {
            let cursor = system
                .probe_cursor
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let offset = cursor % helpers.len();
            helpers.rotate_left(offset);
        }
        helpers.truncate(system.config.swim.indirect_probes);
        (
            system.probes.clone(),
            direct,
            helpers,
            system.config.swim.probe_timeout,
            system.shutdown_tx.subscribe(),
        )
    };
    let deadline = Instant::now() + total;
    let mut failed = false;
    if let Some(sender) = direct {
        let budget = if helpers.is_empty() { total } else { total / 2 };
        let result = state
            .clone()
            .exchange(
                sender.clone(),
                target.clone(),
                false,
                budget,
                shutdown.clone(),
            )
            .await;
        if result == ProbeOutcome::Reachable {
            return result;
        }
        // A connection replacement is not evidence that its new route failed.
        let replaced = weak
            .upgrade()
            .and_then(|system| system.direct_sender_of(&target))
            .is_some_and(|current| !current.same_connection(&sender));
        failed = result == ProbeOutcome::Failed && !replaced;
    }
    let mut requests = FuturesUnordered::new();
    for (_, sender) in helpers {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        requests.push(state.clone().exchange(
            sender,
            target.clone(),
            true,
            remaining,
            shutdown.clone(),
        ));
    }
    while let Some(result) = requests.next().await {
        match result {
            ProbeOutcome::Reachable => return result,
            ProbeOutcome::Failed => failed = true,
            ProbeOutcome::Unavailable => {}
        }
    }
    if failed {
        ProbeOutcome::Failed
    } else {
        ProbeOutcome::Unavailable
    }
}

fn reply(back: &FrameSender, request: ProbeRequest, outcome: ProbeOutcome, indirect: bool) {
    let body = ProbeReply {
        request_id: request.request_id,
        target: request.target,
        outcome,
    };
    let event = if indirect {
        ProbeEvent::PingReqAck(body)
    } else {
        ProbeEvent::Pong(body)
    };
    if let Ok(frame) = event.frame() {
        let _ = back.send_frame_sync(frame);
    }
}

/// Never await helper work in the inbound dispatch loop. Acquire a bounded slot
/// before spawning an owned task; its lifetime is also scoped to the requester.
pub(super) fn handle(weak: Weak<RemoteActorSystem>, event: ProbeEvent, back: &FrameSender) {
    let Some(system) = weak.upgrade() else {
        return;
    };
    if !system.config.membership_gossip_enabled() {
        return;
    }
    match event {
        ProbeEvent::Pong(body) => {
            system.probes.complete(body, false, back);
        }
        ProbeEvent::PingReqAck(body) => {
            system.probes.complete(body, true, back);
        }
        ProbeEvent::Ping(request) => {
            let outcome = if request.target == system.config.node_id {
                ProbeOutcome::Reachable
            } else {
                ProbeOutcome::Unavailable
            };
            reply(back, request, outcome, false);
        }
        ProbeEvent::PingReq(request) => {
            let target = system
                .direct_sender_of(&request.target)
                .filter(|sender| eligible(&system, sender) && !sender.same_connection(back));
            let permit = system.probes.helpers.clone().try_acquire_owned();
            let (Some(target), Ok(permit)) = (target, permit) else {
                reply(back, request, ProbeOutcome::Unavailable, true);
                return;
            };
            let state = system.probes.clone();
            let shutdown = system.shutdown_tx.subscribe();
            let back = back.clone();
            let deadline = Instant::now() + Duration::from_millis(u64::from(request.budget_ms));
            system.spawn_background(async move {
                let _permit = permit;
                tokio::select! {
                    biased;
                    _ = back.closed() => {},
                    _ = async {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        let mut outcome = state.exchange(target.clone(), request.target.clone(), false, remaining, shutdown).await;
                        if outcome == ProbeOutcome::Failed {
                            let replaced = weak.upgrade().and_then(|system| system.direct_sender_of(&request.target))
                                .is_some_and(|current| !current.same_connection(&target));
                            if replaced { outcome = ProbeOutcome::Unavailable; }
                        }
                        reply(&back, request, outcome, true);
                    } => {},
                }
            });
        }
    }
}
