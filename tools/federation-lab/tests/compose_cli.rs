use std::{
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "parrot-compose-contract-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
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

fn invoke(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_federation-lab"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn invalid_compose_arguments_fail_without_panic_or_default_workload() {
    for args in [
        vec!["--clusters"],
        vec!["--nodes-per-cluster"],
        vec!["--fold"],
        vec!["--out"],
        vec!["--realm"],
        vec!["--clusters", "invalid"],
        vec!["--clusters", "0"],
        vec!["--nodes-per-cluster", "0"],
        vec!["--fold", "0"],
        vec!["--fold", "5"],
        vec!["--clusters", "+1"],
        vec!["--clusters", " 1"],
        vec!["--clusters", "18446744073709551616"],
        vec!["--nodes-per-cluster", "18446744073709551615"],
        vec!["--clusters", "58537"],
        vec!["--clusters", "10001", "--nodes-per-cluster", "1"],
        vec!["--realm", "a: b\ninjected: true"],
        vec!["--realm", "a/b"],
        vec!["--realm", ""],
        vec!["--out", ""],
        vec!["--out", "--clusters"],
        vec!["--clusters", "1", "--clusters", "2"],
        vec!["--clustrs", "1"],
        vec!["stray"],
    ] {
        let output = invoke(&[vec!["composegen"], args.clone()].concat());
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    }
}

#[test]
fn malformed_options_cannot_truncate_an_existing_output() {
    let directory = Directory::new();
    let path = directory.0.join("existing.yaml");
    std::fs::write(&path, b"existing configuration").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_federation-lab"))
        .args(["composegen", "--out"])
        .arg(&path)
        .args(["--clusters", "1", "--unknown", "1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"existing configuration");
}

fn check_small_plan(text: &[u8]) {
    let value: serde_json::Value = serde_json::from_slice(text).unwrap();
    assert_eq!(value["x-parrot-model"]["scope"], "topology-plan-only");
    let services = value["services"].as_object().unwrap();
    assert_eq!(services.len(), 4);
    for cluster in 0..2 {
        let hub = format!("fed-c{cluster}-n0");
        let worker = format!("fed-c{cluster}-n1");
        let environment = &services[&hub]["environment"];
        assert_eq!(environment["PARROT_CLUSTER"], format!("c{cluster}"));
        assert_eq!(environment["PARROT_NODES"], "4");
        assert_eq!(environment["PARROT_REALM"], "fed");
        assert_eq!(services[&worker]["environment"]["PARROT_NODES"], "1");
        assert_eq!(services[&worker]["depends_on"], serde_json::json!([hub]));
        let port = 7000 + cluster;
        assert_eq!(
            services[&format!("fed-c{cluster}-n0")]["ports"],
            serde_json::json!([format!("{port}:{port}")])
        );
    }
}

#[test]
fn stdout_contains_the_requested_parseable_plan() {
    let output = invoke(&["composegen", "--clusters", "2", "--nodes-per-cluster", "5"]);
    assert!(output.status.success(), "{output:?}");
    check_small_plan(&output.stdout);
}

#[test]
fn file_output_has_the_same_plan_and_no_stdout_contamination() {
    let directory = Directory::new();
    let path = directory.0.join("plan with spaces.yaml");
    let output = Command::new(env!("CARGO_BIN_EXE_federation-lab"))
        .args([
            "composegen",
            "--clusters",
            "2",
            "--nodes-per-cluster",
            "5",
            "--out",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    check_small_plan(&std::fs::read(path).unwrap());
}

#[test]
fn twin_app_rejects_unknown_missing_or_duplicate_options() {
    for args in [
        vec!["twin-app", "--manifest"],
        vec!["twin-app", "--manifest", ""],
        vec!["twin-app", "--manfiest", "ignored.toml"],
        vec!["twin-app", "--manifest", "a.toml", "--manifest", "b.toml"],
        vec!["help", "extra"],
    ] {
        let output = invoke(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    }
}

#[test]
fn twin_app_default_manifest_matches_the_repository_entrypoint() {
    let output = Command::new(env!("CARGO_BIN_EXE_federation-lab"))
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .arg("twin-app")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("App model passed [crawler-lab]"));
}

#[test]
fn help_lists_implemented_commands() {
    let output = invoke(&["help"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("composegen") && text.contains("twin-app"));
    assert!(!text.contains("report"));
}

#[cfg(unix)]
#[test]
fn output_paths_preserve_platform_native_names() {
    use std::ffi::OsString;
    let directory = Directory::new();
    #[cfg(target_os = "linux")]
    let name = {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(b"plan-\xff.yaml".to_vec())
    };
    #[cfg(not(target_os = "linux"))]
    let name = OsString::from("plan-\u{e9}.yaml");
    let path = directory.0.join(name);
    let output = Command::new(env!("CARGO_BIN_EXE_federation-lab"))
        .args([
            "composegen",
            "--clusters",
            "2",
            "--nodes-per-cluster",
            "5",
            "--out",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    check_small_plan(&std::fs::read(path).unwrap());
}
