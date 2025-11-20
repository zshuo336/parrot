use super::*;
use crate::mirror::notification_contracts::{check, isolated_named, park, probe, run, Empty};
use crate::{RemoteActorSystem, RemoteConfig};
use futures::FutureExt;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

fn retire(action: u8, throwing: u8) {
    run(async {
        let system = RemoteActorSystem::new(
            RemoteConfig::tcp("segment-notification", None),
            Arc::new(Empty),
        )
        .unwrap();
        let outgoing = Arc::new(Mutex::new(Outgoing::default()));
        let mut waiters: Vec<Pin<Box<dyn Future<Output = ()>>>> = Vec::new();
        let mut probes = Vec::new();
        let mut closing = Vec::new();
        for index in 0..40 {
            let (tx, mut rx) = mpsc::channel(1);
            let sender = FrameSender::new(tx, format!("peer-{index}"));
            if action < 2 {
                let mut assemblies = system.receptionist_assemblies.lock().unwrap();
                let data = [
                    1u64.to_le_bytes().as_slice(),
                    8u32.to_le_bytes().as_slice(),
                    0u32.to_le_bytes().as_slice(),
                    &[1],
                ]
                .concat();
                assemblies.push(&sender, &data, Instant::now()).unwrap();
            } else {
                let mut outgoing = outgoing.lock().unwrap();
                outgoing.flights.insert(
                    sender.dispatch_key(),
                    Flight {
                        back: sender.clone(),
                        id: index + 1,
                        body: Bytes::from_static(b"snapshot"),
                        offset: 0,
                        progressed: tokio::time::Instant::now(),
                        routed: false,
                    },
                );
                outgoing.reserved += 8;
            }
            if action == 1 {
                closing.push(sender.clone());
            }
            drop(sender);
            let weak = Arc::downgrade(&system);
            let output = Arc::downgrade(&outgoing);
            let p = probe(throwing, move || {
                let system = weak.upgrade().unwrap();
                if system.link_gate.try_lock().is_err() || system.receptionist.try_lock().is_err() {
                    return false;
                }
                let Ok(assemblies) = system.receptionist_assemblies.try_lock() else {
                    return false;
                };
                if !assemblies.states.is_empty() || assemblies.reserved != 0 {
                    return false;
                }
                drop(assemblies);
                let outgoing = output.upgrade().unwrap();
                let Ok(outgoing) = outgoing.try_lock() else {
                    return false;
                };
                outgoing.flights.is_empty() && outgoing.reserved == 0
            });
            let mut waiter: Pin<Box<dyn Future<Output = ()>>> = Box::pin(async move {
                assert!(rx.recv().await.is_none());
            });
            park(waiter.as_mut(), &p);
            waiters.push(waiter);
            probes.push(p);
        }
        // Construct the complete cohort before closing any sender: push prunes
        // closed predecessors and would otherwise notify during fixture setup.
        for sender in closing {
            sender.close();
        }
        let cleanup = crate::ingress::unwind::Cleanup::default();
        let result = cleanup.call(|| match action {
            0 => system.begin_close(),
            1 => {
                let retired = system
                    .receptionist_assemblies
                    .lock()
                    .unwrap()
                    .prune(Instant::now());
                drop(retired);
            }
            2 => {
                let retired = std::mem::take(&mut *outgoing.lock().unwrap());
                drop(retired);
            }
            3 => {
                let retired = outgoing.lock().unwrap().prune(&HashSet::new());
                drop(retired);
            }
            _ => unreachable!(),
        });
        system.link_gate.clear_poison();
        system.receptionist_assemblies.clear_poison();
        outgoing.clear_poison();
        let mut failures = 0;
        for (waiter, p) in waiters.into_iter().zip(&probes) {
            failures += usize::from(
                cleanup
                    .call(|| {
                        assert!(
                            waiter.now_or_never().is_some(),
                            "segment owner not released"
                        );
                        check(p);
                    })
                    .is_err(),
            );
        }
        let _ = cleanup.call(|| system.begin_close());
        assert!(
            result.is_ok(),
            "segment retirement escaped its notification boundary"
        );
        assert_eq!(
            failures, 0,
            "segment retirement lost notification or exposed partial state"
        );
        system.shutdown().await.unwrap();
    });
}

macro_rules! contracts {
    ($($name:ident: $action:expr, $throwing:expr;)*) => {$ (
        #[test]
        fn $name() {
            isolated_named(concat!("system::receptionist::segments::notification_contracts::", stringify!($name)), || retire($action, $throwing));
        }
    )*};
}
contracts! {
    shutdown_releases_all_assemblies: 0, 0;
    shutdown_assembly_panicking_wakers: 0, 1;
    shutdown_assembly_panicking_payloads: 0, 2;
    prune_releases_assemblies_after_unlock: 1, 0;
    prune_assembly_panicking_wakers: 1, 1;
    prune_assembly_panicking_payloads: 1, 2;
    outgoing_drop_commits_all_owners: 2, 0;
    outgoing_drop_panicking_wakers: 2, 1;
    outgoing_drop_panicking_payloads: 2, 2;
    outgoing_prune_commits_all_owners: 3, 0;
    outgoing_prune_panicking_wakers: 3, 1;
    outgoing_prune_panicking_payloads: 3, 2;
}
