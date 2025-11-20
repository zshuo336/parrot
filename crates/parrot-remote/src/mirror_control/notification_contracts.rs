use super::*;
use crate::{
    ingress::{unwind::Cleanup, LocalLookup},
    mirror::notification_contracts::{cancelled, check, isolated_named, park, probe, run, Empty},
};
use futures::FutureExt;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Weak,
    },
    task::{Context, Waker},
};

fn retirement(action: u8, throwing: u8) {
    run(async {
        let local: Arc<dyn LocalLookup> = Arc::new(Empty);
        let registry = Arc::new(WireMirrorRegistry::new(&local));
        let store = Arc::new(MirrorControlStore::new());
        let id = store
            .execute(
                &registry,
                MirrorControl::Open {
                    owner: "notification-owner".into(),
                },
            )
            .await
            .unwrap()
            .session();
        store
            .execute(
                &registry,
                MirrorControl::Apply {
                    session: id,
                    revision: 1,
                    policies: vec![
                        MirrorPolicySpec::new("/source/a", "/shadow"),
                        MirrorPolicySpec::new("/source/b", "/shadow"),
                    ],
                },
            )
            .await
            .unwrap();
        let group = store.state.lock().unwrap().sessions[&id.sequence]
            .group
            .clone()
            .unwrap();
        let handles: Vec<_> = ["/source/a", "/source/b"]
            .iter()
            .map(|source| group.handle_for(source).unwrap())
            .collect();
        let probes: Vec<_> = handles.iter().map(|_| {
            let owner: Weak<MirrorControlStore> = Arc::downgrade(&store);
            let registry = Arc::downgrade(&registry);
            probe(throwing, move || {
                let store = owner.upgrade().unwrap();
                let Ok(state) = store.state.try_lock() else { return false; };
                let value = &state.sessions[&id.sequence];
                if action == 0 {
                    if value.revision != 2 || value.policies.len() != 1 || value.policies[0].source_prefix != "/replacement" || value.retiring.len() != 1 { return false; }
                } else if !value.closing || (action == 2 && state.open) { return false; }
                drop(state);
                // Status really reenters both ownership layers. Re-polling an
                // async request must observe a fully committed revision/phase.
                let registry = registry.upgrade().unwrap();
                let reply = store.execute(&registry, MirrorControl::Status { session: id }).now_or_never();
                if action == 2 { return reply.is_some_and(|r| r.is_err()); }
                matches!(reply, Some(Ok(MirrorControlReply::Session { revision, phase, .. })) if (action == 0 && revision == 2 && phase == MirrorSessionPhase::Active) || (action == 1 && revision == 1 && phase == MirrorSessionPhase::Closing))
            })
        }).collect();
        let mut waiters: Vec<_> = handles.iter().map(|h| Box::pin(cancelled(h))).collect();
        for (waiter, p) in waiters.iter_mut().zip(&probes) {
            park(waiter.as_mut(), p);
        }
        let mut pending_close = None;
        let result = Cleanup::default().call(|| match action {
            0 => {
                let reply = store
                    .execute(
                        &registry,
                        MirrorControl::Apply {
                            session: id,
                            revision: 2,
                            policies: vec![MirrorPolicySpec::new("/replacement", "/shadow")],
                        },
                    )
                    .now_or_never()
                    .unwrap()
                    .unwrap();
                assert!(matches!(
                    reply,
                    MirrorControlReply::Session {
                        revision: 2,
                        phase: MirrorSessionPhase::Active,
                        ..
                    }
                ));
            }
            1 => {
                let p = probe(throwing, || true);
                let mut close =
                    Box::pin(store.execute(&registry, MirrorControl::Close { session: id }));
                park(close.as_mut(), &p);
                pending_close = Some((close, p));
            }
            2 => store.close(),
            _ => unreachable!(),
        });
        assert!(result.is_ok(), "control retirement notification escaped");
        for (waiter, p) in waiters.into_iter().zip(&probes) {
            assert!(waiter.now_or_never().is_some());
            check(p);
        }
        if let Some((mut close, p)) = pending_close {
            let end = tokio::time::Instant::now() + Duration::from_secs(3);
            while !group.is_finished() {
                assert!(tokio::time::Instant::now() < end);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            check(&p);
            assert!(
                matches!(close.as_mut().poll(&mut Context::from_waker(&Waker::from(p.clone()))), std::task::Poll::Ready(Ok(MirrorControlReply::Retired { session })) if session == id)
            );
        } else {
            store.close();
        }
        registry.close();
        registry.wait_closed().await;
        assert!(handles.iter().all(|h| h.is_closed()));
    });
}
macro_rules! contract {
    ($name:ident, $action:literal, $throwing:literal) => {
        #[test]
        fn $name() {
            isolated_named(
                concat!(
                    "mirror_control::notification_contracts::",
                    stringify!($name)
                ),
                || retirement($action, $throwing),
            );
        }
    };
}
contract!(apply_commits_revision_before_reentry, 0, 0);
contract!(apply_contains_throwing_wakers, 0, 1);
contract!(apply_contains_throwing_payloads, 0, 2);
contract!(close_commits_phase_before_reentry, 1, 0);
contract!(close_contains_throwing_wakers, 1, 1);
contract!(close_contains_throwing_payloads, 1, 2);
contract!(shutdown_commits_all_phases_before_reentry, 2, 0);
contract!(shutdown_contains_throwing_wakers, 2, 1);
contract!(shutdown_contains_throwing_payloads, 2, 2);

#[test]
fn cancelled_handle_cannot_acknowledge_or_replace_active_revision() {
    isolated_named("mirror_control::notification_contracts::cancelled_handle_cannot_acknowledge_or_replace_active_revision", || run(async {
        let local: Arc<dyn LocalLookup> = Arc::new(Empty);
        let registry = WireMirrorRegistry::new(&local);
        let store = MirrorControlStore::new();
        let id = store.execute(&registry, MirrorControl::Open { owner: "owner".into() }).await.unwrap().session();
        let policies = vec![MirrorPolicySpec::new("/source", "/shadow")];
        let apply = MirrorControl::Apply { session: id, revision: 1, policies: policies.clone() };
        store.execute(&registry, apply.clone()).await.unwrap();
        let group = store.state.lock().unwrap().sessions[&id.sequence].group.clone().unwrap();
        group.handle_for("/source").unwrap().close();
        // Do not yield to the worker: cancellation fences ownership immediately.
        assert!(matches!(store.execute(&registry, MirrorControl::Status { session: id }).await.unwrap(), MirrorControlReply::Session { phase: MirrorSessionPhase::Superseded, .. }));
        assert!(store.execute(&registry, apply).await.is_err());
        assert!(store.execute(&registry, MirrorControl::Apply { session: id, revision: 2, policies }).await.is_err());
        assert_eq!(store.state.lock().unwrap().sessions[&id.sequence].revision, 1);
        store.execute(&registry, MirrorControl::Close { session: id }).await.unwrap();
        registry.close(); registry.wait_closed().await;
    }));
}

#[test]
fn shutdown_fences_every_session_before_first_notification() {
    isolated_named("mirror_control::notification_contracts::shutdown_fences_every_session_before_first_notification", || run(async {
        let local: Arc<dyn LocalLookup> = Arc::new(Empty);
        let registry = WireMirrorRegistry::new(&local);
        let store = Arc::new(MirrorControlStore::new());
        let mut handles = Vec::new();
        for i in 0..3 {
            let id = store.execute(&registry, MirrorControl::Open { owner: format!("owner-{i}") }).await.unwrap().session();
            let source = format!("/source/{i}");
            store.execute(&registry, MirrorControl::Apply { session: id, revision: 1, policies: vec![MirrorPolicySpec::new(&source, "/shadow")] }).await.unwrap();
            handles.push(store.state.lock().unwrap().sessions[&id.sequence].group.as_ref().unwrap().handle_for(&source).unwrap());
        }
        let owner = Arc::downgrade(&store);
        let called = Arc::new(AtomicBool::new(false));
        let receipt = called.clone();
        let p = probe(0, move || {
            receipt.store(true, Ordering::SeqCst);
            let store = owner.upgrade().unwrap();
            let Ok(state) = store.state.try_lock() else { return false; };
            if state.open || state.sessions.values().any(|s| !s.closing) { return false; }
            let groups: Vec<_> = state.sessions.values().map(|s| s.group.clone().unwrap()).collect();
            drop(state);
            groups.iter().all(|g| !g.is_current())
        });
        let mut waiter = Box::pin(cancelled(&handles[0])); park(waiter.as_mut(), &p);
        store.close(); check(&p); assert!(called.load(Ordering::SeqCst));
        registry.close(); registry.wait_closed().await;
    }));
}
