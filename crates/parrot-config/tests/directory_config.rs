use parrot_config::{directory::DirectorySection, ParrotConfig};
fn config() -> ParrotConfig {
    let mut c = ParrotConfig::builder()
        .remote_node_id("a")
        .remote_topology_role("directory");
    c.remote.node.scheme = Some("mem".into());
    c.remote.directory = Some(DirectorySection {
        storage_dir: Some("/private/directory".into()),
        group: Some("g".into()),
        peers: Some(vec![]),
        ..Default::default()
    });
    c
}
#[test]
fn directory_defaults_are_explicit_and_separate_from_discovery() {
    assert!(ParrotConfig::builder()
        .build()
        .unwrap()
        .remote
        .directory
        .is_none());
    let c = config()
        .remote_seeds(vec!["mem://discovery".into()])
        .build()
        .unwrap();
    let d = c.remote.directory.unwrap();
    assert!(d.peers.is_empty());
    assert_eq!(
        (d.election_ms, d.heartbeat_ms, d.rpc_timeout_ms),
        (2000, 150, 5000)
    );
    assert_eq!(c.remote.node.seeds, ["mem://discovery"]);
}
#[test]
fn every_directory_field_merges_by_precedence_without_erasing_explicit_empty_peers() {
    let mut lower = config();
    lower.remote.directory.as_mut().unwrap().peers = Some(vec!["mem://b".into()]);
    lower.remote.directory.as_mut().unwrap().heartbeat_ms = Some(80);
    let mut higher = ParrotConfig::builder();
    higher.remote.directory = Some(DirectorySection {
        peers: Some(vec![]),
        election_ms: Some(1500),
        ..Default::default()
    });
    let d = higher
        .with_fallback(lower)
        .build()
        .unwrap()
        .remote
        .directory
        .unwrap();
    assert_eq!(d.storage_dir, "/private/directory");
    assert_eq!(d.group, "g");
    assert!(d.peers.is_empty());
    assert_eq!(d.election_ms, 1500);
    assert_eq!(d.heartbeat_ms, 80);
}
#[test]
fn membership_security_storage_and_timing_errors_are_not_defaulted_away() {
    let mut cases = Vec::new();
    let mut c = config();
    c.remote.directory.as_mut().unwrap().peers = None;
    cases.push(c);
    let mut c = config();
    c.remote.directory.as_mut().unwrap().group = Some("".into());
    cases.push(c);
    let mut c = config();
    c.remote.directory.as_mut().unwrap().storage_dir = Some("\0bad".into());
    cases.push(c);
    for peers in [
        vec!["mem://a".into()],
        vec!["mem://b".into(), "mem://b".into()],
        vec!["tcp://b@127.0.0.1:9".into()],
        (0..17).map(|i| format!("mem://p{i}")).collect(),
    ] {
        let mut c = config();
        c.remote.directory.as_mut().unwrap().peers = Some(peers);
        cases.push(c);
    }
    let mut c = config();
    c.remote.directory.as_mut().unwrap().election_ms = Some(0);
    cases.push(c);
    let mut c = config();
    c.remote.directory.as_mut().unwrap().heartbeat_ms = Some(3000);
    cases.push(c);
    let mut c = config();
    c.remote.directory.as_mut().unwrap().rpc_timeout_ms = Some(0);
    cases.push(c);
    let mut c = config();
    c.remote.node.scheme = Some("tcp".into());
    cases.push(c);
    let mut c = config();
    c.remote.security.mode = Some("insecure-development".into());
    cases.push(c);
    for c in cases {
        assert!(c.build().is_err());
    }
}
#[test]
fn toml_schema_preserves_the_directory_and_rejects_unknown_keys() {
    let text="[remote.node]\nnode_id='a'\nscheme='mem'\ntopology_role='directory'\n[remote.directory]\nstorage_dir='/private/directory'\ngroup='g'\npeers=['mem://b']\n";
    let c: ParrotConfig = toml::from_str(text).unwrap();
    assert_eq!(
        c.build().unwrap().remote.directory.unwrap().peers,
        ["mem://b"]
    );
    assert!(toml::from_str::<ParrotConfig>(&format!("{text}peerz=[]\n")).is_err());
}
