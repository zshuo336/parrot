//! Deployment configuration must control the shared local/network service table.
use parrot_api::{
    receptionist::{ReceptionistGateway, ReceptionistKey},
    types::BoxedActorRef,
};
use parrot_config::{receptionist::RegistrationLimits, ParrotConfig};
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
async fn deployment_count_constrains_all_keys_and_gateway_handles() {
    let system = node("max_registrations = 1");
    let a = system.receptionist_gateway();
    let b = system.receptionist_gateway();
    a.register(key("edge/first"), "/user/a".into())
        .await
        .unwrap();
    b.register(key("edge/first"), "/user/a".into())
        .await
        .unwrap();
    assert!(
        b.register(key("edge/second"), "/user/b".into())
            .await
            .is_err(),
        "configured registration count must constrain all gateway handles"
    );
    assert_eq!(b.subscribe(key("edge/second")).await.unwrap().len(), 0);
    a.deregister(key("edge/first"), "/user/a".into())
        .await
        .unwrap();
    b.register(key("edge/second"), "/user/b".into())
        .await
        .unwrap();
    system.shutdown().await.unwrap();
    system.shutdown().await.unwrap();
    assert!(a
        .register(key("edge/third"), "/user/c".into())
        .await
        .is_err());
}

#[tokio::test]
async fn deployment_bytes_charge_qualified_paths_at_exact_boundary() {
    let k = key("edge/bytes");
    let declared =
        parrot_remote::receptionist::registration_bytes("budget", &k, "parrot://budget/user/a");
    let system = node(&format!("max_registration_bytes = {}", declared - 1));
    assert!(
        system
            .receptionist_gateway()
            .register(k.clone(), "/user/a".into())
            .await
            .is_err(),
        "runtime bytes must include the qualified path"
    );
    system.shutdown().await.unwrap();
    let system = node(&format!("max_registration_bytes = {declared}"));
    let gateway = system.receptionist_gateway();
    gateway.register(k.clone(), "/user/a".into()).await.unwrap();
    gateway.register(k.clone(), "/user/a".into()).await.unwrap();
    assert!(gateway.register(k.clone(), "/user/b".into()).await.is_err());
    gateway
        .deregister(k.clone(), "/user/a".into())
        .await
        .unwrap();
    gateway.register(k, "/user/b".into()).await.unwrap();
    system.shutdown().await.unwrap();
}

#[tokio::test]
async fn zero_registration_dimensions_allow_subscriptions_and_empty_withdrawal() {
    for setting in ["max_registrations = 0", "max_registration_bytes = 0"] {
        let system = node(setting);
        let gateway = system.receptionist_gateway();
        let mut stream = gateway.subscribe(key("edge/disabled")).await.unwrap();
        assert!(gateway
            .register(key("edge/disabled"), "/user/a".into())
            .await
            .is_err());
        assert_eq!(
            stream.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
        gateway
            .deregister(key("edge/disabled"), "/user/a".into())
            .await
            .unwrap();
        system.shutdown().await.unwrap();
    }
}

#[test]
fn independent_registration_fallback_defaults_and_zero_survive_runtime_conversion() {
    let defaults = RemoteConfig::from_resolved(&ParrotConfig::default().build().unwrap()).unwrap();
    assert_eq!(
        defaults.receptionist_registrations,
        RegistrationLimits::default()
    );
    for (higher, expected) in [
        (
            "max_registrations = 0",
            RegistrationLimits {
                max_registrations: 0,
                max_reserved_bytes: 123456,
            },
        ),
        (
            "max_registration_bytes = 0",
            RegistrationLimits {
                max_registrations: 7,
                max_reserved_bytes: 0,
            },
        ),
    ] {
        let lower =
            config("[remote.receptionist]\nmax_registrations = 7\nmax_registration_bytes = 123456");
        let resolved = config(&format!("[remote.receptionist]\n{higher}"))
            .with_fallback(lower)
            .build()
            .unwrap();
        assert_eq!(resolved.remote.receptionist.registrations, expected);
        assert_eq!(
            RemoteConfig::from_resolved(&resolved)
                .unwrap()
                .receptionist_registrations,
            expected
        );
    }
    for invalid in [
        "max_registrations = -1",
        "max_registration_bytes = 'unlimited'",
        "max_registration_bytes = 1.5",
    ] {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "[remote.receptionist]\n{invalid}").unwrap();
        assert!(ParrotConfig::builder().load_file(file.path()).is_err());
    }
    assert_eq!(
        defaults
            .with_receptionist_registrations(RegistrationLimits {
                max_registrations: 1,
                max_reserved_bytes: 2
            })
            .receptionist_registrations
            .max_reserved_bytes,
        2
    );
}
