use parrot_app::{AppManifest, ArtifactRef, ComponentSpec, EngineKind};

pub fn manifest() -> AppManifest {
    AppManifest {
        name: "manifest-output".into(),
        version: "1.0.0".into(),
        components: vec![ComponentSpec {
            name: "worker".into(),
            engine: EngineKind::Parrot,
            artifact: ArtifactRef::Props {
                factory: "test.echo".into(),
            },
            alt_artifact: None,
            instances: Default::default(),
            placement: Default::default(),
            upgrade: Default::default(),
            deps: vec![],
            config: None,
            hooks: Default::default(),
        }],
        wiring: vec![],
        mirrors: vec![],
        config_overlay: None,
    }
}

pub fn nested_table(depth: usize) -> toml::Table {
    assert!(depth >= 2);
    let mut value = toml::Value::Integer(1);
    for _ in 2..depth {
        value = toml::Value::Array(vec![value]);
    }
    toml::Table::from_iter([("value".into(), value)])
}

pub fn repeated_headers() -> AppManifest {
    let mut m = manifest();
    let children = (0..1000)
        .map(|i| (format!("child{i}"), toml::Value::Table(toml::Table::new())))
        .collect();
    m.components[0].config = Some(toml::Table::from_iter([(
        "p".repeat(8192),
        toml::Value::Table(children),
    )]));
    m
}

pub fn budget<T, E: std::fmt::Display>(result: Result<T, E>, resource: &str) {
    let error = result
        .err()
        .expect("over-budget manifest output was accepted")
        .to_string();
    assert!(
        error.contains("budget exceeded") && error.contains(resource),
        "unexpected {resource} error: {error}"
    );
}
