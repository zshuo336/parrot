use parrot_config::{ParrotConfig, RemoteSecurityResolved, RemoteSecuritySection};
fn parse(text: &str) -> Result<parrot_config::Resolved, String> {
    toml::from_str::<ParrotConfig>(text)
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())
}
#[test]
fn security_defaults_to_local_and_rejects_ignored_credentials() {
    assert!(matches!(
        parse("").unwrap().remote.security,
        RemoteSecurityResolved::Local
    ));
    for text in [
        "[remote.security]\nmode='typo'",
        "[remote.security]\nmode='local'\nadmin_nodes=['admin']",
        "[remote.security]\nmode='insecure-development'\nca_path='ca.pem'",
        "[remote.security]\nmode='mtls'\ncert_path='cert.pem'\nkey_path='key.pem'",
        "[remote.security]\nmode='mtls'\ncert_path=''\nkey_path='key.pem'\nca_path='ca.pem'",
        "[remote.security]\nmode='mtls'\ncert_path='cert.pem'\nkey_path='key.pem'\nca_path='ca.pem'\nadmin_nodes=['bad/name']",
        "[remote.security]\nmode='mtls'\ncert='misspelled'",
    ] { assert!(parse(text).is_err(), "accepted {text}"); }
}
#[test]
fn security_layers_keep_credentials_and_explicit_empty_admin_allowlist() {
    let mut lower = ParrotConfig::default();
    lower.remote.security = RemoteSecuritySection {
        mode: Some("mtls".into()),
        cert_path: Some("cert.pem".into()),
        key_path: Some("key.pem".into()),
        ca_path: Some("ca.pem".into()),
        admin_nodes: Some(vec!["operator".into()]),
    };
    let mut higher = ParrotConfig::default();
    higher.remote.security.admin_nodes = Some(Vec::new());
    let result = higher.with_fallback(lower.clone()).build().unwrap();
    let RemoteSecurityResolved::MutualTls {
        cert_path,
        key_path,
        ca_path,
        admin_nodes,
    } = result.remote.security
    else {
        panic!("lost security mode")
    };
    assert_eq!(
        (cert_path.as_str(), key_path.as_str(), ca_path.as_str()),
        ("cert.pem", "key.pem", "ca.pem")
    );
    assert!(
        admin_nodes.is_empty(),
        "empty allowlist must revoke lower-layer authorization"
    );
    let mut incompatible = ParrotConfig::default();
    incompatible.remote.security.mode = Some("insecure-development".into());
    assert!(
        incompatible.with_fallback(lower).build().is_err(),
        "mode change silently ignored inherited credentials"
    );
}
