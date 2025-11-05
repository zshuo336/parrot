use parrot_config::{ParrotConfig, Resolved};

fn parse(text: &str) -> Result<Resolved, Box<dyn std::error::Error>> {
    Ok(toml::from_str::<ParrotConfig>(text)?.build()?)
}

#[test]
fn defaults_and_explicit_permissive_policy_are_distinct() {
    let default = parse("").unwrap().remote.receptionist;
    assert!(default.default_deny);
    assert!(default.roles.is_empty());
    let explicit = parse("[remote.receptionist]\ndefault_deny = false").unwrap();
    assert!(!explicit.remote.receptionist.default_deny);
}

#[test]
fn higher_priority_grant_map_replaces_inherited_grants_even_when_empty() {
    let lower: ParrotConfig = toml::from_str(
        "[remote.receptionist]\ndefault_deny = false\n[remote.receptionist.roles]\nserver = ['edge/public/']\nclient = ['cloud/query']",
    ).unwrap();
    let higher: ParrotConfig =
        toml::from_str("[remote.receptionist]\ndefault_deny = true\nroles = {}").unwrap();
    let result = higher.with_fallback(lower.clone()).build().unwrap();
    assert!(result.remote.receptionist.default_deny);
    assert!(result.remote.receptionist.roles.is_empty());

    let higher: ParrotConfig =
        toml::from_str("[remote.receptionist.roles]\nserver = ['edge/private']").unwrap();
    let result = higher.with_fallback(lower).build().unwrap();
    assert!(!result.remote.receptionist.default_deny);
    assert_eq!(result.remote.receptionist.roles.len(), 1);
    assert_eq!(result.remote.receptionist.roles["server"], ["edge/private"]);
}

#[test]
fn malformed_grants_and_unknown_fields_are_rejected_before_runtime() {
    for node in [
        "",
        "bad/node",
        "..",
        "bad*node",
        "bad%node",
        "bad\\node",
        "bad node",
    ] {
        let mut config = ParrotConfig::default();
        config.remote.receptionist.roles = Some([(node.into(), vec!["edge/public".into()])].into());
        assert!(config.build().is_err(), "accepted node {node:?}");
    }
    for prefix in [
        "",
        "/",
        "edge//public",
        "edge/../private",
        "edge/./public",
        "edge/*",
        "edge/public//",
        "edge/public?",
        "edge/white space",
    ] {
        let mut config = ParrotConfig::default();
        config.remote.receptionist.roles = Some([("server".into(), vec![prefix.into()])].into());
        assert!(config.build().is_err(), "accepted prefix {prefix:?}");
    }
    for text in [
        "[remote.receptionist]\ndefaut_deny = false",
        "[remote.receptionist.roles]\nserver = ['edge']\nserver = ['cloud']",
    ] {
        assert!(parse(text).is_err(), "accepted {text}");
    }
    parse("[remote.receptionist.roles]\nserver = ['edge/public/', 'cloud/query']").unwrap();
}
