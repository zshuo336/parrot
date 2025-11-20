use parrot_config::ParrotConfig;

fn config(text: &str) -> ParrotConfig {
    toml::from_str(text).unwrap()
}

#[test]
fn default_is_inert_and_endpoint_explicitly_enables_recording() {
    let defaults = ParrotConfig::default().build().unwrap().tracing;
    assert!(!defaults.enabled);
    assert!(defaults.otlp_endpoint.is_none());
    assert!(defaults.socket.is_none());
    assert!(!defaults.allow_insecure_http);
    assert_eq!(defaults.capacity, 4096);
    assert_eq!(defaults.interval_ms, 1000);
    assert_eq!(defaults.request_timeout_ms, 5000);
    assert_eq!(defaults.max_attempts, 8);
    assert!(
        config("[tracing]\notlp_endpoint = 'https://collector.example/v1/traces'")
            .build()
            .unwrap()
            .tracing
            .enabled
    );
}

#[test]
fn higher_layer_preserves_explicit_false_and_merges_every_other_field() {
    let lower = config("[tracing]\nenabled = true\notlp_endpoint = 'http://collector.example/v1/traces'\nallow_insecure_http = true\ncapacity = 7\nsocket = '/private/query.sock'\ninterval_ms = 11\nrequest_timeout_ms = 29\nmax_attempts = 3");
    let higher = config("[tracing]\nenabled = false\nallow_insecure_http = false\ncapacity = 13");
    let resolved = higher.with_fallback(lower).build().unwrap().tracing;
    assert!(!resolved.enabled);
    assert!(!resolved.allow_insecure_http);
    assert_eq!(resolved.capacity, 13);
    assert_eq!(
        resolved.otlp_endpoint.as_deref(),
        Some("http://collector.example/v1/traces")
    );
    assert_eq!(
        resolved.socket.as_deref(),
        Some(std::path::Path::new("/private/query.sock"))
    );
    assert_eq!(resolved.interval_ms, 11);
    assert_eq!(resolved.request_timeout_ms, 29);
    assert_eq!(resolved.max_attempts, 3);
}

#[test]
fn malformed_and_out_of_budget_values_fail_with_secret_free_errors() {
    for setting in [
        "enabled = true",
        "capacity = 0",
        "capacity = 65537",
        "interval_ms = 0",
        "interval_ms = 3600001",
        "request_timeout_ms = 0",
        "request_timeout_ms = 3600001",
        "max_attempts = 0",
        "max_attempts = 1025",
        "socket = ''",
        "otlp_endpoint = ''",
        "socket = \"secret\\npath\"",
        "otlp_endpoint = \"secret\\nurl\"",
    ] {
        let prefix = if setting.starts_with("otlp_endpoint") || setting == "enabled = true" {
            ""
        } else {
            "otlp_endpoint = 'https://private-token.example/v1/traces'\n"
        };
        let error = config(&format!("[tracing]\n{prefix}{setting}"))
            .build()
            .unwrap_err()
            .to_string();
        assert!(error.contains("tracing."), "{setting}: {error}");
        assert!(
            !error.contains("secret") && !error.contains("private-token"),
            "{error}"
        );
    }
    for (key, limit) in [("otlp_endpoint", 8192), ("socket", 4096)] {
        let prefix = if key == "socket" {
            "otlp_endpoint = 'https://collector.example/v1/traces'\n"
        } else {
            ""
        };
        assert!(config(&format!(
            "[tracing]\n{prefix}{key} = '{}'",
            "x".repeat(limit + 1)
        ))
        .build()
        .is_err());
    }
    for text in [
        "[tracing]\nunknown = 1",
        "[tracing]\nenabled = 'false'",
        "[tracing]\ncapacity = -1",
    ] {
        assert!(toml::from_str::<ParrotConfig>(text).is_err());
    }
}

#[test]
fn orphan_options_need_an_endpoint_or_an_explicit_disable() {
    for option in [
        "capacity = 8",
        "allow_insecure_http = false",
        "socket = 'trace.sock'",
        "interval_ms = 3",
        "request_timeout_ms = 7",
        "max_attempts = 2",
    ] {
        assert!(
            config(&format!("[tracing]\n{option}")).build().is_err(),
            "{option}"
        );
        assert!(
            !config(&format!("[tracing]\nenabled = false\n{option}"))
                .build()
                .unwrap()
                .tracing
                .enabled
        );
    }
    // Disabling activation does not bypass malformed resource-budget validation.
    assert!(config("[tracing]\nenabled = false\ncapacity = 0")
        .build()
        .is_err());
}

#[test]
fn resource_bounds_accept_their_endpoints() {
    for (capacity, interval, timeout, attempts) in [(1, 1, 1, 1), (65536, 3600000, 3600000, 1024)] {
        let value = config(&format!("[tracing]\notlp_endpoint = 'https://collector.example/v1/traces'\ncapacity = {capacity}\ninterval_ms = {interval}\nrequest_timeout_ms = {timeout}\nmax_attempts = {attempts}")).build().unwrap().tracing;
        assert_eq!(value.capacity, capacity);
        assert_eq!(value.interval_ms, interval);
        assert_eq!(value.request_timeout_ms, timeout);
        assert_eq!(value.max_attempts, attempts);
    }
}
