use parrot_config::ParrotConfig;

#[test]
fn work_stealing_defaults_off_and_toml_enables_it() {
    assert!(
        !ParrotConfig::builder()
            .build()
            .unwrap()
            .thread
            .shared_work_stealing
    );
    let config: ParrotConfig = toml::from_str("[thread]\nshared_work_stealing = true\n").unwrap();
    assert!(config.build().unwrap().thread.shared_work_stealing);
}

#[test]
fn explicit_false_overrides_enabled_fallback_and_omission_inherits_it() {
    let enabled = ParrotConfig::builder().thread_shared_work_stealing(true);
    assert!(
        ParrotConfig::builder()
            .with_fallback(enabled.clone())
            .build()
            .unwrap()
            .thread
            .shared_work_stealing
    );
    assert!(
        !ParrotConfig::builder()
            .thread_shared_work_stealing(false)
            .with_fallback(enabled)
            .build()
            .unwrap()
            .thread
            .shared_work_stealing
    );
}

#[test]
fn work_stealing_requires_an_actual_boolean() {
    for value in ["1", "\"true\"", "[]"] {
        assert!(toml::from_str::<ParrotConfig>(&format!(
            "[thread]\nshared_work_stealing = {value}\n"
        ))
        .is_err());
    }
}
