use parrot_config::ParrotConfig;

#[test]
fn default_node_reorder_limits_are_finite() {
    let resolved = ParrotConfig::builder().build().unwrap();
    assert_eq!(resolved.remote.reorder.stream_capacity, 256);
    assert_eq!(resolved.remote.reorder.frame_capacity, 4096);
    assert_eq!(resolved.remote.reorder.byte_capacity, 64 * 1024 * 1024);
    assert!(resolved.remote.reorder.receiver_state.is_none());
}

#[test]
fn receiver_state_binding_merges_atomically_and_roundtrips() {
    let source = "[remote.reorder.receiver_state]\ndirectory='/var/lib/parrot/receiver-a'\nrealm='cluster-a'\n";
    let fallback: ParrotConfig = toml::from_str(source).unwrap();
    let mut override_config = ParrotConfig::builder();
    override_config.remote.reorder.receiver_state = Some(parrot_config::OrderedReceiverState {
        directory: "/var/lib/parrot/receiver-b".into(),
        realm: "cluster-b".into(),
    });
    let config = override_config.with_fallback(fallback);
    let copied: ParrotConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
    assert_eq!(
        copied.build().unwrap().remote.reorder.receiver_state,
        config.remote.reorder.receiver_state
    );
    for invalid in [
        "directory='relative'\nrealm='cluster'",
        "directory='/absolute'\nrealm=''",
        "directory='/absolute'\nrealm='has/slash'",
    ] {
        let config: ParrotConfig =
            toml::from_str(&format!("[remote.reorder.receiver_state]\n{invalid}\n")).unwrap();
        assert!(config
            .build()
            .unwrap_err()
            .to_string()
            .contains("remote.reorder.receiver_state"));
    }
    for invalid in [
        "directory='/absolute'",
        "realm='cluster'",
        "directory='/absolute'\nrealm='cluster'\nunknown=1",
    ] {
        assert!(toml::from_str::<ParrotConfig>(&format!(
            "[remote.reorder.receiver_state]\n{invalid}\n"
        ))
        .is_err());
    }
}

#[test]
fn file_limits_merge_independently_and_serialize_losslessly() {
    let mut file: ParrotConfig = toml::from_str(
        "[remote.reorder]\nstream_capacity=7\nframe_capacity=19\nbyte_capacity=8192\n",
    )
    .unwrap();
    let mut override_config = ParrotConfig::builder();
    override_config.remote.reorder.frame_capacity = Some(3);
    file = override_config.with_fallback(file);
    let copied: ParrotConfig = toml::from_str(&toml::to_string(&file).unwrap()).unwrap();
    let resolved = copied.build().unwrap();
    assert_eq!(resolved.remote.reorder.stream_capacity, 7);
    assert_eq!(resolved.remote.reorder.frame_capacity, 3);
    assert_eq!(resolved.remote.reorder.byte_capacity, 8192);
}

#[test]
fn zero_and_overflowing_node_limits_fail_before_runtime() {
    for key in ["stream_capacity", "frame_capacity", "byte_capacity"] {
        for value in [0, usize::MAX] {
            let source = format!("[remote.reorder]\n{key}={value}\n");
            // TOML signed integer range is narrower than usize on a 64-bit host.
            if let Ok(config) = toml::from_str::<ParrotConfig>(&source) {
                let error = config.build().unwrap_err().to_string();
                assert!(error.contains(&format!("remote.reorder.{key}")), "{error}");
            } else {
                assert_eq!(value, usize::MAX);
            }
            let mut config = ParrotConfig::builder();
            match key {
                "stream_capacity" => config.remote.reorder.stream_capacity = Some(value),
                "frame_capacity" => config.remote.reorder.frame_capacity = Some(value),
                _ => config.remote.reorder.byte_capacity = Some(value),
            }
            assert!(config.build().is_err());
        }
    }
}
