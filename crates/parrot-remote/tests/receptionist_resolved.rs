//! Publicly mutable resolved configuration must not bypass deployment validation.
use parrot_config::ParrotConfig;
use parrot_remote::RemoteConfig;

#[test]
fn malformed_public_resolved_grants_are_rejected_by_runtime_conversion() {
    for (node, prefix) in [
        ("server", "edge/*"),
        ("server", "edge/../private"),
        ("bad/node", "edge/public"),
        ("", "edge/public"),
    ] {
        let mut resolved = ParrotConfig::default().build().unwrap();
        resolved
            .remote
            .receptionist
            .roles
            .insert(node.into(), vec![prefix.into()]);
        assert!(
            RemoteConfig::from_resolved(&resolved).is_err(),
            "accepted {node:?}: {prefix:?}"
        );
    }
}
