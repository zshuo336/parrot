//! Validation uses the real CLI and must precede storage and listener effects.
use std::{
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static SERIAL: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "websearch-validate-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn validation_succeeds_without_binding_or_creating_data() {
    let directory = Directory::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_websearch"))
        .args([
            "--validate-only",
            "--bind",
            &address.to_string(),
            "--port",
            &address.port().to_string(),
            "--data",
        ])
        .arg(directory.0.join("missing-data"))
        .args([
            "--index-data",
            "/not-created-remotely/quote \" slash \\ 中文",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(!directory.0.join("missing-data").exists());
}
#[test]
fn validation_still_rejects_invalid_values_and_duplicates() {
    let directory = Directory::new();
    for args in [
        vec!["--pages", "0"],
        vec!["--index-data", "relative/index"],
        vec!["--index-data", "/a", "--index-data", "/b"],
        vec!["--validate-only"],
        vec!["--serve-only", "https://example.test/"],
        vec![
            "--crawl-origin",
            "http://127.0.0.1:1234",
            "http://127.0.0.1:5678/a",
        ],
        vec!["--crawl-origin", "http://127.0.0.1:1234/private"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_websearch"))
            .args(["--validate-only", "--data"])
            .arg(directory.0.join("missing-data"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(!directory.0.join("missing-data").exists());
    }
}

#[test]
#[cfg(unix)]
fn launcher_mixed_help_uses_real_cli_and_never_starts_services() {
    let directory = Directory::new();
    let launcher = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("deploy/launcher.py");
    // Replace only artifact preparation. Validation is the actual Rust binary;
    // any accidental service start fails without prepared gateway specifications.
    let script = r#"
import importlib.util, pathlib, sys
spec = importlib.util.spec_from_file_location('launcher_contract', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
def prepare(self): self.binary = pathlib.Path(sys.argv[2])
module.Launcher.prepare = prepare
raise SystemExit(module.main(['direct', *sys.argv[3:]]))
"#;
    for (index, (args, code)) in [
        (vec!["--pages", "5", "--help"], 0),
        (vec!["--data", "jvm=index", "--help"], 0),
        (vec!["--data", "-h", "--help"], 0),
        (vec!["--serve-only", "--help"], 0),
        (vec!["--pages", "bad", "--help"], 2),
        (vec!["--data", "--help"], 2),
        (vec!["--", "--help"], 2),
    ]
    .into_iter()
    .enumerate()
    {
        let run = directory.0.join(index.to_string());
        let mut command = Command::new("python3");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("WS_") {
                command.env_remove(key);
            }
        }
        let output = command
            .args(["-c", script])
            .arg(&launcher)
            .arg(env!("CARGO_BIN_EXE_websearch"))
            .args(args)
            .env("WS_RUN_DIR", &run)
            .env("WS_TIMEOUT", "10")
            .current_dir(&directory.0)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.contains("Maximum fetch attempts"), code == 0);
        assert!(!stdout.contains("WEBSEARCH_READY="));
        let result: serde_json::Value =
            serde_json::from_slice(&std::fs::read(run.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["ready"], false);
        let children = result["children"].as_array().unwrap();
        // Missing --data values are rejected before even spawning validation.
        assert_eq!(children.len(), usize::from(index != 5), "case {index}");
        for child in children {
            assert_eq!(child["name"], "validate");
            assert_eq!(child["retired"], true);
        }
        assert!(!run.join("data").exists());
    }
    assert!(!directory.0.join("jvm=index").exists());
    assert!(!directory.0.join("-h").exists());
}

#[test]
fn manifest_validation_is_effect_free_and_invalid_manifests_fail_before_startup() {
    let directory = Directory::new();
    // Presence validation must be independent of prebuilt gateway artifacts.
    for name in [
        "frontier.erl",
        "runtime/tools/parrot-obs/gateway_worker.py",
        "runtime/tools/parrot-obs/ray_gateway.py",
        "runtime/interop/python/parrot_protocol/__init__.py",
        "runtime/interop/python/parrot_protocol/ray_gw.py",
        "runtime/interop/jvm/target/parrot-protocol-jvm-0.1.0.jar",
        "runtime/interop/jvm/target/dependency.jar",
        "runtime/interop/erlang/parrot_gw.erl",
        "runtime/interop/erlang/parrot_component_runtime.erl",
        "runtime/interop/erlang/parrot_beam_snapshot.erl",
        "runtime/interop/erlang/parrot_trace.erl",
        "runtime/interop/erlang/parrot_beam_guest.erl",
        "runtime/interop/erlang/parrot_beam_host.erl",
    ] {
        let path = directory.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "fixture").unwrap();
    }
    std::fs::write(
        directory.0.join("runtime/interop/jvm/target/cp.txt"),
        "dependency.jar",
    )
    .unwrap();
    let data = directory.0.join("uncreated-data");
    let source = include_str!("../websearch.app.toml");
    let mut manifest: parrot_app::manifest::AppManifest = toml::from_str(source).unwrap();
    for component in &mut manifest.components {
        use parrot_app::manifest::ArtifactRef;
        match &mut component.artifact {
            ArtifactRef::Beam { uri, .. }
            | ArtifactRef::PyModule { uri, .. }
            | ArtifactRef::Jvm { uri, .. } => {
                *uri = Some(directory.0.to_string_lossy().into_owned());
            }
            _ => {}
        }
    }
    let source = toml::to_string(&manifest).unwrap();
    let mut invalid_settings = manifest.clone();
    invalid_settings
        .components
        .iter_mut()
        .find(|c| c.name == "crawler")
        .unwrap()
        .config = Some(
        [(
            "settings".into(),
            toml::Value::String(r#"{"pages":0}"#.into()),
        )]
        .into_iter()
        .collect(),
    );
    let invalid_settings = toml::to_string(&invalid_settings).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let path = directory.0.join("app.toml");
    for (text, validate_only, code) in [
        (source.clone(), true, 0),
        (
            format!("{source}\n[config_overlay]\nunknown_key = 1\n"),
            false,
            2,
        ),
        (
            source.replace("frontier:/user/frontier", "search:/user/search"),
            false,
            2,
        ),
        (source.replace("qos = \"lan\"", "qos = \"wan\""), false, 2),
        (invalid_settings, true, 2),
    ] {
        std::fs::write(&path, text).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_websearch"));
        command
            .arg("--manifest")
            .arg(&path)
            .arg("--runtime-root")
            .arg(directory.0.join("runtime"))
            .arg("--data")
            .arg(&data)
            .args([
                "--bind",
                &address.to_string(),
                "--port",
                &address.port().to_string(),
            ]);
        if validate_only {
            command.arg("--validate-only");
        }
        let output = command.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "preflight bound Web port: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!data.exists());
    }
}
