//! Actual deployment limits, shared gateways and retained receiver lifetimes.
use parrot_api::{
    receptionist::{ReceptionistGateway, ReceptionistKey},
    types::BoxedActorRef,
};
use parrot_config::{receptionist::SubscriptionLimits, ParrotConfig};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};
use std::{io::Write, sync::Arc};

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
fn key(s: &str) -> ReceptionistKey {
    ReceptionistKey::new(s).unwrap()
}
fn config(text: &str) -> ParrotConfig {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(text.as_bytes()).unwrap();
    ParrotConfig::builder().load_file(file.path()).unwrap()
}
fn node(budget: &str) -> Arc<RemoteActorSystem> {
    let resolved = config(&format!(
        "[remote.node]\nnode_id = 'budget'\n[remote.receptionist]\ndefault_deny = false\n{budget}"
    ))
    .build()
    .unwrap();
    RemoteActorSystem::new(
        RemoteConfig::from_resolved(&resolved).unwrap(),
        Arc::new(Empty),
    )
    .unwrap()
}

#[tokio::test]
async fn deployment_count_limit_is_shared_across_keys_and_gateway_handles() {
    let node = node("max_subscriptions = 1");
    let a = node.receptionist_gateway();
    let b = node.receptionist_gateway();
    let mut stream = a.subscribe(key("edge/first")).await.unwrap();
    assert!(
        b.subscribe(key("edge/second")).await.is_err(),
        "configured count must constrain all gateway handles"
    );
    stream.close();
    assert_eq!(stream.recv().await, None);
    assert!(
        b.subscribe(key("edge/second")).await.is_err(),
        "closed retained receiver must retain configured count"
    );
    drop(stream);
    let next = b.subscribe(key("edge/second")).await.unwrap();
    assert!(a.subscribe(key("edge/first")).await.is_err());
    drop(next);
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn deployment_byte_budget_reserves_snapshot_plus_64_realtime_slots() {
    // A one-entry snapshot needs 65 slots, which exceeds this exact 64-slot cap.
    let node = node(&format!(
        "max_subscription_bytes = {}",
        64 * parrot_remote::receptionist::RESERVED_SLOT_BYTES
    ));
    let gateway = node.receptionist_gateway();
    gateway
        .register(key("edge/full"), "/user/service".into())
        .await
        .unwrap();
    assert!(
        gateway.subscribe(key("edge/full")).await.is_err(),
        "configured bytes must include the complete snapshot"
    );
    let empty = gateway.subscribe(key("edge/empty")).await.unwrap();
    assert_eq!(empty.max_capacity(), 64);
    assert!(
        gateway.subscribe(key("edge/another")).await.is_err(),
        "reserved empty slots must consume configured bytes"
    );
    drop(empty);
    assert!(gateway.subscribe(key("edge/another")).await.is_ok());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_closes_operations_but_receiver_keeps_queued_prefix() {
    let node = node("max_subscriptions = 1");
    let gateway = node.receptionist_gateway();
    let mut stream = gateway.subscribe(key("edge/retained")).await.unwrap();
    gateway
        .register(key("edge/retained"), "/user/service".into())
        .await
        .unwrap();
    node.shutdown().await.unwrap();
    assert!(gateway.subscribe(key("edge/new")).await.is_err());
    assert!(gateway
        .register(key("edge/retained"), "/user/new".into())
        .await
        .is_err());
    assert_eq!(
        stream.len(),
        1,
        "shutdown must preserve the receiver's queued prefix"
    );
    assert!(stream.recv().await.is_some());
    drop(stream);
}

#[tokio::test]
async fn zero_in_either_deployment_dimension_disables_new_streams() {
    for setting in ["max_subscriptions = 0", "max_subscription_bytes = 0"] {
        let node = node(setting);
        let gateway = node.receptionist_gateway();
        assert!(
            gateway.subscribe(key("edge/disabled")).await.is_err(),
            "zero must disable subscriptions"
        );
        gateway
            .register(key("edge/disabled"), "/user/service".into())
            .await
            .unwrap();
        gateway
            .deregister(key("edge/disabled"), "/user/service".into())
            .await
            .unwrap();
        node.shutdown().await.unwrap();
    }
}

#[test]
fn independent_fallback_fields_defaults_and_explicit_zero_survive_conversion() {
    let defaults = RemoteConfig::from_resolved(&ParrotConfig::default().build().unwrap()).unwrap();
    assert_eq!(
        defaults.receptionist_subscriptions,
        SubscriptionLimits::default()
    );
    let lower =
        config("[remote.receptionist]\nmax_subscriptions = 7\nmax_subscription_bytes = 123456");
    let higher = config("[remote.receptionist]\nmax_subscriptions = 0");
    let resolved = higher.with_fallback(lower).build().unwrap();
    assert_eq!(
        resolved.remote.receptionist.subscriptions,
        SubscriptionLimits {
            max_subscriptions: 0,
            max_reserved_bytes: 123456
        }
    );
    let runtime = RemoteConfig::from_resolved(&resolved).unwrap();
    assert_eq!(
        runtime.receptionist_subscriptions,
        resolved.remote.receptionist.subscriptions
    );
    let reverse = config("[remote.receptionist]\nmax_subscription_bytes = 0")
        .with_fallback(config(
            "[remote.receptionist]\nmax_subscriptions = 3\nmax_subscription_bytes = 100",
        ))
        .build()
        .unwrap();
    assert_eq!(
        reverse.remote.receptionist.subscriptions,
        SubscriptionLimits {
            max_subscriptions: 3,
            max_reserved_bytes: 0
        }
    );
    for invalid in [
        "max_subscriptions = -1",
        "max_subscription_bytes = 'unlimited'",
        "max_subscription_bytes = 1.5",
    ] {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "[remote.receptionist]\n{invalid}").unwrap();
        assert!(ParrotConfig::builder().load_file(file.path()).is_err());
    }
    let changed = defaults.with_receptionist_subscriptions(SubscriptionLimits {
        max_subscriptions: 1,
        max_reserved_bytes: 2,
    });
    assert_eq!(changed.receptionist_subscriptions.max_reserved_bytes, 2);
}
