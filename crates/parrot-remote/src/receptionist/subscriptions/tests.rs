use super::*;
use crate::{acl::AclRules, receptionist::ReceptionistFacade};
use parrot_api::receptionist::{ReceptionistGateway, RegistrationScope};

fn key(s: &str) -> ReceptionistKey {
    ReceptionistKey::new(s).unwrap()
}
fn limits(count: usize, slots: usize) -> SubscriptionLimits {
    SubscriptionLimits {
        max_subscriptions: count,
        max_reserved_bytes: slots * RESERVED_SLOT_BYTES,
    }
}

#[test]
fn byte_reservation_includes_full_snapshot_and_realtime_capacity() {
    let k = key("edge/snapshot");
    let mut table = Receptionist::with_subscription_limits(limits(4, 66));
    for n in 0..2 {
        table.register("n", &k, &format!("/actor/{n}"));
    }
    let mut stream = table.try_subscribe(&k, 64).unwrap();
    assert_eq!(stream.max_capacity(), 66);
    assert_eq!(stream.len(), 2);
    assert!(
        table.try_subscribe(&key("edge/other"), 1).is_err(),
        "full snapshot and realtime slots must reserve bytes"
    );
    assert_eq!(
        table.subscribers.len(),
        1,
        "denied admission must not create a bucket"
    );
    assert_eq!(
        stream.try_recv().unwrap(),
        ReceptionistEvent::Registered {
            key: k.clone(),
            remote_path: "/actor/0".into()
        }
    );
    stream.close();
    stream.try_recv().unwrap();
    assert!(
        table.try_subscribe(&k, 1).is_err(),
        "drained retained receiver must keep its lease"
    );
    drop(stream);
    assert!(table.try_subscribe(&k, 64).is_ok());
    assert_eq!(table.subscription_usage.count.load(Ordering::Acquire), 0);
    assert_eq!(table.subscription_usage.bytes.load(Ordering::Acquire), 0);
}

#[test]
fn arithmetic_overflow_zero_and_one_byte_short_fail_without_mutation() {
    let k = key("edge/arithmetic");
    for budget in [
        SubscriptionLimits {
            max_subscriptions: 0,
            max_reserved_bytes: usize::MAX,
        },
        SubscriptionLimits {
            max_subscriptions: 1,
            max_reserved_bytes: 0,
        },
        SubscriptionLimits {
            max_subscriptions: 1,
            max_reserved_bytes: RESERVED_SLOT_BYTES - 1,
        },
    ] {
        let mut table = Receptionist::with_subscription_limits(budget);
        assert!(table.try_subscribe(&k, 1).is_err());
        assert!(table.subscribers.is_empty());
        assert_eq!(table.subscription_usage.count.load(Ordering::Acquire), 0);
        assert_eq!(table.subscription_usage.bytes.load(Ordering::Acquire), 0);
    }
    let mut table = Receptionist::with_subscription_limits(SubscriptionLimits {
        max_subscriptions: usize::MAX,
        max_reserved_bytes: usize::MAX,
    });
    table.register("node", &k, "/path");
    for capacity in [usize::MAX, tokio::sync::Semaphore::MAX_PERMITS] {
        assert!(table.try_subscribe(&k, capacity).is_err());
    }
    assert!(table.subscribers.is_empty());
    assert_eq!(table.lookup(&k).len(), 1);
    let mut table = Receptionist::with_subscription_limits(limits(1, 1));
    assert!(
        table.try_subscribe(&k, 0).is_ok(),
        "exact reservation boundary must be admitted"
    );
}

#[tokio::test]
async fn slow_closed_stream_retains_node_quota_across_keys_until_receiver_drop() {
    let gateway = ReceptionistFacade::with_subscription_limits("n", limits(1, 128))
        .with_acl(AclRules::permissive());
    let k = key("edge/slow");
    let mut stream = gateway.subscribe(k.clone()).await.unwrap();
    for n in 0..65 {
        gateway
            .register(k.clone(), format!("/actor/{n}"))
            .await
            .unwrap();
    }
    assert!(stream.is_closed());
    assert_eq!(stream.len(), 64);
    assert!(
        gateway.subscribe(key("edge/new")).await.is_err(),
        "closed receiver must retain node quota"
    );
    for n in 0..64 {
        assert_eq!(
            stream.recv().await,
            Some(ReceptionistEvent::Registered {
                key: k.clone(),
                remote_path: format!("/actor/{n}")
            })
        );
    }
    assert_eq!(stream.recv().await, None);
    assert!(
        gateway.subscribe(key("edge/new")).await.is_err(),
        "EOF receiver must retain node quota"
    );
    drop(stream);
    let replacement = gateway.subscribe(key("edge/new")).await.unwrap();
    assert_eq!(replacement.len(), 0);
    assert!(gateway.subscribe(k).await.is_err());
}

#[test]
fn concurrent_receiver_drops_do_not_lose_released_or_new_reservations() {
    let k = key("edge/concurrent");
    let mut table = Receptionist::with_subscription_limits(limits(32, 32));
    for _ in 0..50 {
        let streams: Vec<_> = (0..32)
            .map(|_| table.try_subscribe(&k, 1).unwrap())
            .collect();
        assert!(table.try_subscribe(&k, 1).is_err());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            let other = barrier.clone();
            scope.spawn(move || {
                other.wait();
                drop(streams);
            });
            barrier.wait();
            let mut admitted = Vec::new();
            for _ in 0..32 {
                if let Ok(stream) = table.try_subscribe(&k, 1) {
                    admitted.push(stream);
                }
            }
            drop(admitted);
        });
        assert_eq!(table.subscription_usage.count.load(Ordering::Acquire), 0);
        assert_eq!(table.subscription_usage.bytes.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
async fn oversized_publications_do_not_replace_owners_or_consume_generation_bindings() {
    let gateway = ReceptionistFacade::new("n").with_acl(AclRules::permissive());
    let k = key("edge/oversized");
    let original = Arc::new(RegistrationScope::default());
    gateway
        .register_scoped(k.clone(), "/keep".into(), original.clone())
        .await
        .unwrap();
    let mut stream = gateway.subscribe(k.clone()).await.unwrap();
    stream.try_recv().unwrap();
    let scope = Arc::new(RegistrationScope::default());
    let huge = "x".repeat(MAX_EVENT_BYTES);
    for n in 0..RegistrationScope::MAX_BINDINGS {
        assert!(
            gateway
                .register_scoped(k.clone(), format!("/{n}/{huge}"), scope.clone())
                .await
                .is_err(),
            "oversized scoped publication must fail before binding commit"
        );
    }
    assert!(
        gateway.register(k.clone(), huge.clone()).await.is_err(),
        "oversized manual publication must be rejected"
    );
    let huge_key = key(&format!("edge/{huge}"));
    assert!(gateway.subscribe(huge_key.clone()).await.is_err());
    assert!(gateway
        .register_scoped(huge_key, "/keep".into(), original.clone())
        .await
        .is_err());
    assert_eq!(gateway.inner.lock().unwrap().owners.len(), 1);
    assert_eq!(
        stream.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    for n in 0..RegistrationScope::MAX_BINDINGS {
        gateway
            .register_scoped(k.clone(), format!("/valid/{n}"), scope.clone())
            .await
            .expect("rejected events must not consume generation bindings");
    }
    assert!(gateway
        .register_scoped(k.clone(), "/one-too-many".into(), scope.clone())
        .await
        .is_err());
    drop(stream);
    scope.retire();
    scope.join().await.unwrap();
    original.retire();
    original.join().await.unwrap();
    assert!(gateway.inner.lock().unwrap().lookup(&k).is_empty());
}

#[test]
fn event_boundary_and_legacy_oversized_cleanup_preserve_reservation_contract() {
    let k = key("edge/envelope");
    let allowed = MAX_EVENT_BYTES - std::mem::size_of::<ReceptionistEvent>() - k.as_str().len();
    assert!(validate_event(&k, &"x".repeat(allowed)).is_ok());
    assert!(validate_event(&k, &"x".repeat(allowed + 1)).is_err());
    let mut table = Receptionist::with_subscription_limits(limits(1, 64));
    let mut stream = table.try_subscribe(&k, 1).unwrap();
    let path = "x".repeat(MAX_EVENT_BYTES);
    let event = table.register("n", &k, &path);
    table.broadcast(&event);
    assert!(
        stream.is_closed(),
        "unchecked oversized event must not enter reserved stream"
    );
    assert_eq!(
        stream.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    );
    assert!(table.try_subscribe(&k, 1).is_err());
    let event = table.deregister(&k, &path);
    table.broadcast(&event);
    assert!(
        table.lookup(&k).is_empty(),
        "oversized historical entry must remain removable"
    );
    drop(stream);
    assert!(table.try_subscribe(&k, 1).is_ok());
}

#[test]
fn legacy_subscribe_does_not_replay_an_oversized_historical_snapshot() {
    let k = key("edge/historical-envelope");
    let mut table = Receptionist::new();
    table.register("legacy", &k, &"x".repeat(MAX_EVENT_BYTES));

    let mut stream = table.subscribe(&k, 1);
    assert!(stream.is_closed());
    assert_eq!(
        stream.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    );
}

#[tokio::test]
async fn shutdown_does_not_release_retained_receiver_lease() {
    struct Empty;
    #[async_trait::async_trait]
    impl crate::LocalLookup for Empty {
        async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
            None
        }
    }
    let config = crate::RemoteConfig::tcp("budget-shutdown", Some("127.0.0.1:0".parse().unwrap()))
        .with_receptionist_acl(AclRules::permissive())
        .with_receptionist_subscriptions(limits(1, 64));
    let node = crate::RemoteActorSystem::new(config, Arc::new(Empty)).unwrap();
    let stream = node
        .receptionist_gateway()
        .subscribe(key("edge/retained"))
        .await
        .unwrap();
    node.shutdown().await.unwrap();
    node.shutdown().await.unwrap();
    let (count, refused) = {
        let mut table = node.receptionist.lock().unwrap();
        (
            table.subscription_usage.count.load(Ordering::Acquire),
            table.try_subscribe(&key("edge/other"), 1).is_err(),
        )
    };
    assert_eq!(
        count, 1,
        "shutdown must retain receiver lease while receiver survives"
    );
    assert!(refused);
    assert!(stream.is_closed());
    drop(stream);
    let (count, bytes, admitted) = {
        let mut table = node.receptionist.lock().unwrap();
        (
            table.subscription_usage.count.load(Ordering::Acquire),
            table.subscription_usage.bytes.load(Ordering::Acquire),
            table.try_subscribe(&key("edge/other"), 64).is_ok(),
        )
    };
    assert_eq!(count, 0);
    assert_eq!(bytes, 0);
    assert!(admitted);
}
