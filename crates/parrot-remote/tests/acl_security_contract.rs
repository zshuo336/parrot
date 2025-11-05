//! Authorization policy contracts, independent of transport authentication.
//! Runtime callers must still supply a trusted identity; these helpers cannot
//! authenticate a role string or enforce an ACL at an unwired ingress point.
use parrot_remote::acl::{AclDecision, AclRules, RouteAcl};

#[test]
fn route_default_denies_cross_realm_like_deserialization() {
    for rules in [
        RouteAcl::default(),
        RouteAcl::from_json(r#"{"routes":{}}"#).unwrap(),
    ] {
        assert_eq!(
            rules.check_route("source", "parrot://destination/cluster/node"),
            AclDecision::Deny
        );
    }
}

#[test]
fn missing_source_cannot_use_open_or_wildcard_route_policy() {
    for rules in [
        RouteAcl::default(),
        RouteAcl::from_json(r#"{"routes":{},"default_deny_cross_realm":false}"#).unwrap(),
        RouteAcl::from_json(r#"{"routes":{"parrot://destination/":["*"]}}"#).unwrap(),
    ] {
        assert_eq!(
            rules.check_route("", "parrot://destination/cluster/node"),
            AclDecision::Deny
        );
    }
}

#[test]
fn route_grants_end_at_path_segment_boundaries() {
    let rules =
        RouteAcl::from_json(r#"{"routes":{"parrot://destination/eu":["source"]}}"#).unwrap();
    assert_eq!(
        rules.check_route("source", "parrot://destination/eu/node"),
        AclDecision::Allow
    );
    assert_eq!(
        rules.check_route("source", "parrot://destination/europe/node"),
        AclDecision::Deny
    );
}

#[test]
fn same_realm_uses_uri_authority_and_preserves_explicit_restrictions() {
    let rules =
        RouteAcl::from_json(r#"{"routes":{"parrot://prod/eu/admin":["operator"]}}"#).unwrap();
    for source in ["prod", "prod/eu"] {
        assert_eq!(
            rules.check_route(source, "parrot://prod/eu/node"),
            AclDecision::Allow
        );
        assert_eq!(
            rules.check_route(source, "parrot://prod/eu/admin/action"),
            AclDecision::Deny
        );
        assert_eq!(
            rules.check_route(source, "parrot://production/eu/node"),
            AclDecision::Deny
        );
    }
}

#[test]
fn receptionist_omitted_policy_and_default_reject_unknown_roles() {
    for rules in [
        AclRules::default(),
        AclRules::from_json(r#"{"roles":{"device":["edge"]}}"#).unwrap(),
    ] {
        assert_eq!(rules.check("unknown", "edge/sensor"), AclDecision::Deny);
    }
    assert_eq!(
        AclRules::permissive().check("device", "edge/sensor"),
        AclDecision::Allow
    );
}

#[test]
fn receptionist_scope_grants_do_not_authorize_sibling_namespaces() {
    let rules =
        AclRules::from_json(r#"{"roles":{"device":["edge"]},"default_deny":true}"#).unwrap();
    assert_eq!(rules.check("device", "edge/sensor"), AclDecision::Allow);
    for key in ["edge-admin/secret", "edgeless/secret"] {
        assert_eq!(rules.check("device", key), AclDecision::Deny);
        assert!(!rules.event_visible("device", key));
    }
}

#[test]
fn receptionist_nested_grants_match_complete_key_segments() {
    let rules =
        AclRules::from_json(r#"{"roles":{"device":["edge/public"]},"default_deny":true}"#).unwrap();
    for key in ["edge/public", "edge/public/sensor"] {
        assert_eq!(rules.check("device", key), AclDecision::Allow);
    }
    for key in ["edge", "edge/private", "edge/publicity/sensor"] {
        assert_eq!(rules.check("device", key), AclDecision::Deny);
    }
}

#[test]
fn misspelled_security_options_are_errors() {
    assert!(AclRules::from_json(r#"{"roles":{},"default_denyy":true}"#).is_err());
    assert!(RouteAcl::from_json(r#"{"routes":{},"default_deny_cross_relam":true}"#).is_err());
}

#[test]
fn duplicate_acl_entries_cannot_silently_replace_restrictions() {
    assert!(AclRules::from_json(r#"{"roles":{"device":[],"device":["edge"]}}"#).is_err());
    assert!(RouteAcl::from_json(
        r#"{"routes":{"parrot://prod/admin":[],"parrot://prod/admin":["*"]}}"#
    )
    .is_err());
}

#[test]
fn malformed_policy_values_are_rejected_at_load_and_direct_use() {
    for prefix in [
        "",
        "/edge",
        "edge//x",
        "edge/../cloud",
        "edge/%2e%2e",
        "edge/*",
    ] {
        let input = serde_json::json!({"roles":{"device":[prefix]}}).to_string();
        assert!(AclRules::from_json(&input).is_err(), "accepted {prefix:?}");
        let mut direct = AclRules::permissive();
        direct.roles.insert("device".into(), vec![prefix.into()]);
        assert_eq!(direct.check("device", "edge/x"), AclDecision::Deny);
    }
    for prefix in [
        "",
        "prod/admin",
        "https://prod/admin",
        "parrot://prod//admin",
    ] {
        let input = serde_json::json!({"routes":{prefix:["*"]}}).to_string();
        assert!(RouteAcl::from_json(&input).is_err(), "accepted {prefix:?}");
        let mut direct = RouteAcl {
            default_deny_cross_realm: false,
            ..Default::default()
        };
        direct.routes.insert(prefix.into(), vec!["*".into()]);
        assert_eq!(
            direct.check_route("source", "parrot://prod/admin"),
            AclDecision::Deny
        );
    }
    assert!(AclRules::from_json(r#"{"roles":{"": ["edge"]}}"#).is_err());
    assert!(RouteAcl::from_json(r#"{"routes":{"parrot://prod":[""]}}"#).is_err());
}

#[test]
fn ambiguous_request_paths_and_missing_principals_fail_even_in_open_mode() {
    let rules = AclRules::permissive();
    for key in [
        "",
        "/edge/x",
        "edge/",
        "edge//x",
        "edge/../secret",
        "edge/%2fsecret",
        "edge/x?y",
        "edge/x\n",
        "edge\\secret",
    ] {
        assert_eq!(rules.check("device", key), AclDecision::Deny, "{key:?}");
    }
    for principal in ["", " ", "prod//eu", "prod/..", "*"] {
        assert_eq!(rules.check(principal, "edge/x"), AclDecision::Deny);
    }
    let open = RouteAcl {
        default_deny_cross_realm: false,
        ..Default::default()
    };
    for target in [
        "",
        "prod/eu/node",
        "parrot://prod/../secret",
        "parrot://prod/%2fsecret",
        "parrot://prod/x#fragment",
    ] {
        assert_eq!(
            open.check_route("source", target),
            AclDecision::Deny,
            "{target:?}"
        );
    }
}

#[test]
fn equivalent_prefixes_intersect_and_more_specific_rules_override_defaults() {
    let rules = RouteAcl::from_json(
        r#"{
        "routes": {
            "parrot://prod": ["*"],
            "parrot://prod/admin": ["source"],
            "parrot://prod/admin/": [],
            "parrot://prod/admin/public/": ["source"]
        },
        "default_deny_cross_realm": false
    }"#,
    )
    .unwrap();
    assert_eq!(
        rules.check_route("source", "parrot://prod/data"),
        AclDecision::Allow
    );
    for target in [
        "parrot://prod/admin",
        "parrot://prod/admin/",
        "parrot://prod/admin/secret",
    ] {
        assert_eq!(rules.check_route("source", target), AclDecision::Deny);
    }
    assert_eq!(
        rules.check_route("source", "parrot://prod/admin/public/report"),
        AclDecision::Allow
    );
    assert_eq!(
        rules.check_route("other", "parrot://prod/admin/public/report"),
        AclDecision::Deny
    );
}

#[tokio::test]
async fn facade_is_closed_until_explicitly_authorized() {
    use parrot_api::receptionist::{ReceptionistGateway, ReceptionistKey};
    use parrot_remote::receptionist::ReceptionistFacade;
    let facade = ReceptionistFacade::new("device");
    let key = ReceptionistKey::new("edge/sensor").unwrap();
    assert!(facade
        .register(key.clone(), "parrot://device/user/x".into())
        .await
        .is_err());
    assert!(facade.subscribe(key.clone()).await.is_err());
    assert!(facade
        .deregister(key, "parrot://device/user/x".into())
        .await
        .is_err());
}

#[tokio::test]
async fn facade_rejects_namespace_escape_and_preserves_authorized_event_flow() {
    use parrot_api::receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey};
    use parrot_remote::receptionist::ReceptionistFacade;
    let rules =
        AclRules::from_json(r#"{"roles":{"device":["edge"]},"default_deny":true}"#).unwrap();
    let facade = ReceptionistFacade::new("device").with_acl(rules);
    let forbidden = ReceptionistKey::new("edge-admin/secret").unwrap();
    assert!(facade
        .register(forbidden.clone(), "parrot://device/user/x".into())
        .await
        .is_err());
    assert!(facade.subscribe(forbidden.clone()).await.is_err());
    assert!(facade
        .deregister(forbidden, "parrot://device/user/x".into())
        .await
        .is_err());
    let allowed = ReceptionistKey::new("edge/sensor").unwrap();
    let mut events = facade.subscribe(allowed.clone()).await.unwrap();
    let remote_path = "parrot://device/user/sensor".to_string();
    facade
        .register(allowed.clone(), remote_path.clone())
        .await
        .unwrap();
    assert_eq!(
        events.try_recv().unwrap(),
        ReceptionistEvent::Registered {
            key: allowed.clone(),
            remote_path: remote_path.clone(),
        }
    );
    facade
        .deregister(allowed.clone(), remote_path.clone())
        .await
        .unwrap();
    assert_eq!(
        events.try_recv().unwrap(),
        ReceptionistEvent::Unregistered {
            key: allowed,
            remote_path,
        }
    );
}
