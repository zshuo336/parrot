use parrot_config::{ConfigError, ParrotConfig};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct File(std::path::PathBuf);
impl File {
    fn new(body: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-validation-{}-{}.toml",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, body).unwrap();
        Self(path)
    }
    fn load(&self) -> Result<ParrotConfig, ConfigError> {
        ParrotConfig::builder().load_file(&self.0)
    }
}
impl Drop for File {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn malformed_known_values_never_fall_back_to_defaults() {
    for (body, key) in [
        ("[thread]\nshared_pool_size = -1", "shared_pool_size"),
        ("[thread]\nmax_context_tasks = \"1\"", "max_context_tasks"),
        (
            "[remote.transport]\nheartbeat_interval_ms = false",
            "heartbeat_interval_ms",
        ),
        (
            "[remote.transport]\ndefault_hop_limit = 256",
            "default_hop_limit",
        ),
        (
            "[remote.node]\nseeds = [\"tcp://127.0.0.1:9800\", 23]",
            "seeds",
        ),
        ("[remote.node]\nnode_id = 123", "node_id"),
        ("thread = 5", "thread"),
        ("[remote]\nnode = \"node-1\"", "node"),
    ] {
        let error = File::new(body).load().expect_err(body).to_string();
        assert!(
            error.contains(key),
            "diagnostic must identify {key}: {error}"
        );
    }
}

#[test]
fn unknown_keys_are_rejected_at_every_level() {
    for body in [
        "threads = {}",
        "[thread]\nmax_context_task = 1",
        "[remote]\ntransports = {}",
        "[remote.node]\nnodeid = 'node-1'",
        "[remote.transport]\noutbound_queues = 4",
        "[remote.reorder]\nbufer_cap = 5",
        "[remote.codec]\nextra_cap = 1",
    ] {
        assert!(File::new(body).load().is_err(), "silently accepted: {body}");
    }
}

#[test]
fn explicit_missing_file_is_an_error() {
    let file = File::new("");
    std::fs::remove_file(&file.0).unwrap();
    assert!(matches!(file.load(), Err(ConfigError::Io { .. })));
}

#[test]
fn optional_file_skips_only_not_found() {
    let file = File::new("thread = 8");
    assert!(ParrotConfig::builder().load_optional_file(&file.0).is_err());
    assert!(ParrotConfig::builder()
        .load_optional_file(file.0.parent().unwrap())
        .is_err());
    std::fs::remove_file(&file.0).unwrap();
    ParrotConfig::builder()
        .load_optional_file(&file.0)
        .unwrap()
        .build()
        .unwrap();
}

#[test]
fn environment_path_is_required_but_default_file_is_optional() {
    const CHILD: &str = "PARROT_CONFIG_PATH_TEST";
    if let Ok(case) = std::env::var(CHILD) {
        let result = ParrotConfig::builder().load_default_locations();
        if case == "optional" {
            result.unwrap().build().unwrap();
        } else {
            assert!(matches!(result, Err(ConfigError::Io { .. })));
        }
        return;
    }
    let directory = File::new("").0.with_extension("dir");
    std::fs::create_dir(&directory).unwrap();
    for case in ["optional", "required", "empty"] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "environment_path_is_required_but_default_file_is_optional",
                "--nocapture",
            ])
            .env(CHILD, case)
            .env_remove("PARROT_CONFIG")
            .current_dir(&directory);
        if case != "optional" {
            command.env(
                "PARROT_CONFIG",
                if case == "empty" { "" } else { "missing.toml" },
            );
        }
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{case}");
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("config child timed out: {case}");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn zero_execution_and_queue_capacity_cannot_build() {
    for key in [
        "shared_pool_size",
        "shared_queue_capacity",
        "default_mailbox_capacity",
    ] {
        let file = File::new(&format!("[thread]\n{key} = 0"));
        let error = file.load().unwrap().build().expect_err(key).to_string();
        assert!(error.contains(key), "{error}");
    }
    let file = File::new("[remote.transport]\noutbound_queue = 0");
    assert!(file.load().unwrap().build().is_err());
}

#[test]
fn invalid_roles_and_schemes_do_not_select_a_different_transport_or_role() {
    for (field, value) in [("topology_role", "hbu"), ("scheme", "tpc")] {
        let file = File::new(&format!("[remote.node]\n{field} = '{value}'"));
        let error = file.load().unwrap().build().expect_err(field).to_string();
        assert!(error.contains(field), "{error}");
    }
}

#[test]
fn valid_partial_files_and_intentional_disabling_remain_supported() {
    for body in [
        "",
        "[thread]",
        "[remote.node]",
        "[thread]\nmax_context_tasks = 0\nshared_burst_workers_max = 0\nmax_dedicated_threads = 0",
    ] {
        File::new(body).load().unwrap().build().unwrap();
    }
}

#[test]
fn code_priority_does_not_hide_malformed_documents() {
    let file = File::new("[thread]\nshared_pool_size = 'bad'");
    assert!(ParrotConfig::builder()
        .thread_shared_pool_size(2)
        .load_file(&file.0)
        .is_err());
    let file = File::new("[thread]\nshared_pool_size = 0");
    for before in [true, false] {
        let builder = if before {
            ParrotConfig::builder()
                .thread_shared_pool_size(2)
                .load_file(&file.0)
                .unwrap()
        } else {
            file.load().unwrap().thread_shared_pool_size(2)
        };
        assert_eq!(builder.build().unwrap().thread.shared_pool_size, 2);
    }
}

#[test]
fn endpoints_are_validated_before_runtime_and_keep_dns_unresolved() {
    for seed in [
        "",
        "tcp://host:0",
        "tcp://host:65536",
        "udp://host:9",
        "tcp://user@host",
        "tcp://[::1:9",
        "tcp://a@b@host:9",
        "mem://peer",
    ] {
        let error = ParrotConfig::builder()
            .remote_seeds(vec![seed.into()])
            .build()
            .unwrap_err()
            .to_string();
        assert!(error.contains("remote.node.seeds[0]"), "{error}");
    }
    for (scheme, seed) in [
        ("tcp", "peer@localhost:9800"),
        ("quic", "quic://peer@[::1]:9800"),
        ("mem", "mem://peer"),
    ] {
        let mut cfg = ParrotConfig::builder().remote_seeds(vec![seed.into()]);
        cfg.remote.node.scheme = Some(scheme.into());
        let resolved = cfg.build().unwrap();
        assert_eq!(resolved.remote.node.seeds, vec![seed]);
    }
    for node in ["", "peer/name", "peer name", "peer@host", "peer\n"] {
        assert!(ParrotConfig::builder()
            .remote_node_id(node)
            .build()
            .is_err());
    }
}

#[test]
fn inbound_ask_limits_validate_and_merge_without_losing_explicit_values() {
    for name in ["inbound_ask_capacity", "inbound_ask_bytes"] {
        let invalid = File::new(&format!("[remote.transport]\n{name} = 0"));
        assert!(invalid
            .load()
            .unwrap()
            .build()
            .unwrap_err()
            .to_string()
            .contains(name));
        let mut config = ParrotConfig::builder();
        if name == "inbound_ask_capacity" {
            config.remote.transport.inbound_ask_capacity = Some(usize::MAX);
        } else {
            config.remote.transport.inbound_ask_bytes = Some(usize::MAX);
        }
        assert!(config.build().unwrap_err().to_string().contains(name));
    }
    let file =
        File::new("[remote.transport]\ninbound_ask_capacity = 17\ninbound_ask_bytes = 32768");
    let resolved = file.load().unwrap().build().unwrap();
    assert_eq!(resolved.remote.transport.inbound_ask_capacity, 17);
    assert_eq!(resolved.remote.transport.inbound_ask_bytes, 32768);
    let merged = ParrotConfig::builder()
        .remote_inbound_ask_capacity(3)
        .load_file(&file.0)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(merged.remote.transport.inbound_ask_capacity, 3);
    assert_eq!(merged.remote.transport.inbound_ask_bytes, 32768);
    let defaults = ParrotConfig::builder().build().unwrap();
    assert_eq!(defaults.remote.transport.inbound_ask_capacity, 1024);
    assert_eq!(
        defaults.remote.transport.inbound_ask_bytes,
        64 * 1024 * 1024
    );
}

#[test]
fn dispatch_limits_validate_merge_and_document_the_effective_values() {
    for name in [
        "inbound_dispatch_capacity",
        "inbound_dispatch_per_connection",
        "inbound_dispatch_bytes",
    ] {
        for value in [0, u32::MAX as u64 + 1] {
            let file = File::new(&format!("[remote.transport]\n{name} = {value}"));
            assert!(file
                .load()
                .unwrap()
                .build()
                .unwrap_err()
                .to_string()
                .contains(name));
        }
        assert!(
            ParrotConfig::documented().contains_key(format!("remote.transport.{name}").as_str())
        );
    }
    let file = File::new("[remote.transport]\ninbound_dispatch_capacity = 17\ninbound_dispatch_per_connection = 3\ninbound_dispatch_bytes = 4096");
    let mut code = ParrotConfig::builder();
    code.remote.transport.inbound_dispatch_capacity = Some(11);
    let resolved = code.load_file(&file.0).unwrap().build().unwrap();
    assert_eq!(resolved.remote.transport.inbound_dispatch_capacity, 11);
    assert_eq!(resolved.remote.transport.inbound_dispatch_per_connection, 3);
    assert_eq!(resolved.remote.transport.inbound_dispatch_bytes, 4096);
    let defaults = ParrotConfig::builder().build().unwrap();
    assert_eq!(defaults.remote.transport.inbound_dispatch_capacity, 1024);
    assert_eq!(
        defaults.remote.transport.inbound_dispatch_per_connection,
        64
    );
    assert_eq!(
        defaults.remote.transport.inbound_dispatch_bytes,
        64 * 1024 * 1024
    );
}
