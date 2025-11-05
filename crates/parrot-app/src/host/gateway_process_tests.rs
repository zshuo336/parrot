use super::*;
use std::time::{Duration, Instant};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

#[tokio::test]
async fn gateway_factory_default_process_admission_is_bounded() {
    let engine = ThreadActorSystem::shared(Default::default());
    let remote = RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("group-admission-test", None),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    let factory = ProcessGatewayFactory::new(root(), remote);
    let mut fixtures = Vec::new();
    let mut children = Vec::new();
    let mut rejection = None;
    // Exercise the actual production default, including unpublished startup
    // groups. No reduction of the limit in order to manufacture the failure.
    for index in 0..65 {
        let fixture = Fixture::new();
        match factory.spawn_owned(
            &format!("capacity-{index}"),
            &fixture.command("ordinary"),
            false,
            None,
        ) {
            Ok(child) => {
                fixture.ready().await;
                children.push(child);
                fixtures.push(fixture);
            }
            Err(error) => {
                rejection = Some(error);
                break;
            }
        }
    }
    let admitted = children.len();
    assert_eq!(
        factory.procs.child_count(),
        0,
        "startup groups are not published components"
    );
    drop(children);
    factory.procs.kill_all().await.unwrap();
    for fixture in &fixtures {
        fixture.assert_retired();
    }
    engine.shutdown_internal().await.unwrap();
    assert_eq!(
        admitted, 64,
        "host admitted more than its default group budget"
    );
    assert!(rejection
        .unwrap()
        .to_string()
        .contains("gateway process group budget"));
}

struct Fixture {
    directory: tempfile::TempDir,
    record: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let record = directory.path().join("child.json");
        Self { directory, record }
    }
    fn command(&self, mode: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("python3");
        command
            .arg(root().join("crates/parrot-app/src/host/gateway_process_fixture.py"))
            .arg(mode)
            .arg(&self.record);
        command
    }
    fn value(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(&self.record).unwrap()).unwrap()
    }
    async fn ready(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.record.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    fn port_released(&self) -> bool {
        std::net::TcpListener::bind(format!("127.0.0.1:{}", self.value()["port"])).is_ok()
    }
    fn assert_retired(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &self.value()["pid"].to_string()])
                .output()
                .unwrap();
            let state = String::from_utf8(state.stdout).unwrap();
            if self.port_released() && (state.trim().is_empty() || state.trim().starts_with('Z')) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "descendant alive or port occupied: {} state={state}",
                self.value()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.record.exists() {
            let _ = std::net::TcpStream::connect(format!("127.0.0.1:{}", self.value()["port"]));
        }
    }
}

#[tokio::test]
async fn shared_gateway_budget_keeps_retiring_groups_and_preserves_peers() {
    use std::os::unix::ffi::OsStringExt;
    for cancel_retirement in [false, true] {
        let engine = ThreadActorSystem::shared(Default::default());
        let remote = RemoteActorSystem::new(
            parrot_remote::RemoteConfig::tcp("shared-budget-test", None),
            Arc::new(HostLookup { ts: engine.clone() }),
        )
        .unwrap();
        let budget = Arc::new(GatewayProcessBudget::new(2).unwrap());
        let first_factory = ProcessGatewayFactory::new_with_routes_and_budget(
            root(),
            remote.clone(),
            Arc::default(),
            budget.clone(),
        );
        let peer_factory = ProcessGatewayFactory::new_with_routes_and_budget(
            root(),
            remote,
            Arc::default(),
            budget.clone(),
        );
        let mut invalid = tokio::process::Command::new("python3");
        invalid.arg(std::ffi::OsString::from_vec(vec![255]));
        assert!(first_factory
            .spawn_owned("invalid", &invalid, false, None)
            .is_err());
        assert_eq!(budget.in_use(), 0, "pre-spawn validation leaked capacity");
        let first = Fixture::new();
        let peer = Fixture::new();
        let next = Fixture::new();
        let mut child = first_factory
            .spawn_owned("first", &first.command("ordinary"), false, None)
            .unwrap();
        let other = peer_factory
            .clone()
            .spawn_owned("peer", &peer.command("ordinary"), false, None)
            .unwrap();
        first.ready().await;
        peer.ready().await;
        let mut completion = child.completion();
        if cancel_retirement {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), child.retire())
                    .await
                    .is_err()
            );
        }
        drop(child);
        assert_eq!(
            budget.in_use(),
            2,
            "caller cancellation released a live group"
        );
        assert!(peer_factory
            .spawn_owned("next", &next.command("ordinary"), false, None)
            .is_err());
        assert!(!next.record.exists(), "rejected startup executed a child");
        gateway_process::wait_for_retirement(&mut completion)
            .await
            .unwrap();
        first.assert_retired();
        assert_eq!(budget.in_use(), 1);
        assert!(
            !peer.port_released(),
            "retirement killed a peer using the same budget"
        );
        let third = first_factory
            .spawn_owned("next", &next.command("ordinary"), false, None)
            .unwrap();
        next.ready().await;
        assert_eq!(budget.in_use(), 2);
        drop((other, third));
        first_factory.procs.kill_all().await.unwrap();
        peer_factory.procs.kill_all().await.unwrap();
        peer.assert_retired();
        next.assert_retired();
        assert_eq!(budget.in_use(), 0);
        assert_eq!(budget.quarantined(), 0);
        engine.shutdown_internal().await.unwrap();
    }
}

#[test]
fn gateway_reservation_survives_runtime_destruction_until_process_exit() {
    let fixture = Fixture::new();
    let budget = Arc::new(GatewayProcessBudget::new(1).unwrap());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let child = OwnedGateway::spawn_admitted(
            &fixture.command("ordinary"),
            &root(),
            false,
            None,
            Some(budget.reserve().unwrap()),
        )
        .unwrap();
        fixture.ready().await;
        drop(child);
        assert_eq!(budget.in_use(), 1);
    });
    drop(runtime);
    fixture.assert_retired();
    let deadline = Instant::now() + Duration::from_secs(5);
    while budget.in_use() != 0 {
        assert!(
            Instant::now() < deadline,
            "retirement did not return capacity"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(budget.quarantined(), 0);
}

#[tokio::test]
async fn gateway_cleanup_retires_listening_descendant() {
    let fixture = Fixture::new();
    let mut child = OwnedGateway::spawn(&fixture.command("ordinary"), &root(), false).unwrap();
    fixture.ready().await;
    assert!(!fixture.port_released());
    reap(&mut child).await.unwrap();
    fixture.assert_retired();
}

#[tokio::test]
async fn private_compilation_survives_caller_drop_until_actual_retirement() {
    let fixture = Fixture::new();
    let artifacts = tempfile::tempdir().unwrap();
    let path = artifacts.path().to_owned();
    std::fs::write(path.join("owned.beam"), b"compiled artifact").unwrap();
    let child = OwnedGateway::spawn_with_artifacts(
        &fixture.command("ordinary"),
        &root(),
        false,
        Some(Arc::new(PrivateArtifacts::new(artifacts))),
    )
    .unwrap();
    let mut completion = child.completion();
    fixture.ready().await;
    assert!(path.join("owned.beam").is_file());
    drop(child);
    gateway_process::wait_for_retirement(&mut completion)
        .await
        .unwrap();
    fixture.assert_retired();
    assert!(!path.exists(), "private artifacts leaked after retirement");
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[tokio::test]
async fn unproven_group_retirement_preserves_private_compilation() {
    const CHILD: &str = "PARROT_TEST_DENIED_GROUP_OBSERVATION";
    if std::env::var_os(CHILD).is_none() {
        let library = tempfile::tempdir().unwrap();
        let preload = library.path().join("deny-observation.so");
        let compiled = std::process::Command::new("cc")
            .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
            .arg(root().join("crates/parrot-app/src/host/gateway_observation_fault.c"))
            .args(["-ldl", "-o"])
            .arg(&preload)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let tested = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host::gateway_process_tests::unproven_group_retirement_preserves_private_compilation",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("LD_PRELOAD", preload)
            .output()
            .unwrap();
        assert!(
            tested.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&tested.stdout),
            String::from_utf8_lossy(&tested.stderr)
        );
        assert!(
            String::from_utf8_lossy(&tested.stdout)
                .contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
            "isolated artifact contract did not execute exactly once"
        );
        return;
    }

    for keep_caller_reference in [false, true] {
        let fixture = Fixture::new();
        let artifacts = Arc::new(PrivateArtifacts::new(tempfile::tempdir().unwrap()));
        let path = artifacts.path().to_owned();
        std::fs::write(path.join("owned.beam"), b"compiled artifact").unwrap();
        let budget = Arc::new(GatewayProcessBudget::new(1).unwrap());
        let mut child = OwnedGateway::spawn_admitted(
            &fixture.command("ordinary"),
            &root(),
            false,
            Some(artifacts.clone()),
            Some(budget.reserve().unwrap()),
        )
        .unwrap();
        let caller_artifacts = keep_caller_reference.then_some(artifacts);
        fixture.ready().await;
        assert!(!fixture.port_released());
        let error = child.retire().await.unwrap_err().to_string();
        assert!(error.contains("Permission denied"), "{error}");
        assert_eq!(budget.quarantined(), 1);
        assert!(budget.reserve().is_err());
        drop(child);
        drop(caller_artifacts);
        let retained = std::fs::read(path.join("owned.beam")).ok();
        // Independently establish that this fixture has actually exited before
        // removing retained diagnostic artifacts. Production only saw an error.
        fixture.assert_retired();
        if path.exists() {
            std::fs::remove_dir_all(&path).unwrap();
        }
        assert_eq!(
            retained.as_deref(),
            Some(b"compiled artifact".as_slice()),
            "unproven retirement deleted private compilation"
        );
    }
}

#[tokio::test]
async fn gateway_cancelled_cleanup_is_awaited_again_and_preserves_peer() {
    let first = Fixture::new();
    let peer = Fixture::new();
    let child = OwnedGateway::spawn(&first.command("ordinary"), &root(), false).unwrap();
    let mut other = OwnedGateway::spawn(&peer.command("ordinary"), &root(), false).unwrap();
    first.ready().await;
    peer.ready().await;
    let processes = GatewayProcesses::default();
    processes
        .retirements
        .lock()
        .unwrap()
        .push(("first".into(), child.completion()));
    processes
        .procs
        .lock()
        .unwrap()
        .insert("first".into(), ("node".into(), child));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), processes.kill_all())
            .await
            .is_err()
    );
    processes.kill_all().await.unwrap();
    first.assert_retired();
    assert!(!peer.port_released(), "retiring one owner killed its peer");
    other.retire().await.unwrap();
    peer.assert_retired();
}

#[test]
fn gateway_cleanup_survives_runtime_destruction() {
    let fixture = Fixture::new();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let _child = OwnedGateway::spawn(&fixture.command("ordinary"), &root(), false).unwrap();
        fixture.ready().await;
    });
    drop(runtime);
    fixture.assert_retired();
}

#[tokio::test]
async fn gateway_inline_setpgrp_and_fatal_exit_retire_descendants() {
    for mode in ["inline-normal", "inline-crash"] {
        let fixture = Fixture::new();
        let mut child = OwnedGateway::spawn(&fixture.command(mode), &root(), true).unwrap();
        fixture.ready().await;
        let value = fixture.value();
        assert_eq!(
            value["group"], value["parent"],
            "inline runtime escaped session leader"
        );
        if mode == "inline-crash" {
            let mut completion = child.completion();
            tokio::time::timeout(
                Duration::from_secs(10),
                gateway_process::wait_for_retirement(&mut completion),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(child.check_running().is_err());
        } else {
            child.retire().await.unwrap();
        }
        fixture.assert_retired();
    }
}

#[tokio::test]
async fn gateway_command_status_waits_for_descendants() {
    for code in [0, 7] {
        let fixture = Fixture::new();
        let mut child =
            OwnedGateway::spawn(&fixture.command(&format!("exit-{code}")), &root(), false).unwrap();
        fixture.ready().await;
        let result = tokio::time::timeout(Duration::from_secs(10), child.wait_command())
            .await
            .unwrap();
        assert_eq!(result.is_ok(), code == 0, "{result:?}");
        fixture.assert_retired();
    }
}

#[tokio::test]
async fn gateway_host_sigkill_retires_descendants() {
    const ENV: &str = "PARROT_TEST_OWNER_CRASH_RECORD";
    if let Some(record) = std::env::var_os(ENV) {
        let mut command = tokio::process::Command::new("python3");
        command
            .arg(root().join("crates/parrot-app/src/host/gateway_process_fixture.py"))
            .arg("inline-normal")
            .arg(record);
        let _owner = OwnedGateway::spawn(&command, &root(), true).unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        panic!("test host was not killed");
    }
    let fixture = Fixture::new();
    let mut host = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "host::gateway_process_tests::gateway_host_sigkill_retires_descendants",
            "--nocapture",
        ])
        .env(ENV, &fixture.record)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    fixture.ready().await;
    host.kill().await.unwrap();
    assert!(!host.wait().await.unwrap().success());
    fixture.assert_retired();
}

#[tokio::test]
async fn gateway_factory_startup_timeout_and_cancel_retain_cleanup() {
    for cancel in [false, true] {
        let fixture = Fixture::new();
        let repo = fixture.directory.path();
        std::fs::create_dir_all(repo.join("tools/parrot-obs")).unwrap();
        std::fs::create_dir_all(repo.join("interop/python")).unwrap();
        std::fs::copy(
            root().join("tools/parrot-obs/gateway_worker.py"),
            repo.join("tools/parrot-obs/gateway_worker.py"),
        )
        .unwrap();
        let script = format!("import runpy,sys\nsys.argv=[{},'inline-normal',{}]\nrunpy.run_path(sys.argv[0],run_name='__main__')\n",
            serde_json::to_string(&root().join("crates/parrot-app/src/host/gateway_process_fixture.py")).unwrap(),
            serde_json::to_string(&fixture.record).unwrap());
        std::fs::write(repo.join("tools/parrot-obs/ray_gateway.py"), script).unwrap();
        let engine = ThreadActorSystem::shared(Default::default());
        let remote = RemoteActorSystem::new(
            parrot_remote::RemoteConfig::tcp(
                "ownership-test",
                Some("127.0.0.1:0".parse().unwrap()),
            ),
            Arc::new(HostLookup { ts: engine.clone() }),
        )
        .unwrap();
        remote.start().await.unwrap();
        let budget = Arc::new(GatewayProcessBudget::new(1).unwrap());
        let factory = ProcessGatewayFactory::new_with_routes_and_budget(
            repo.to_owned(),
            remote.clone(),
            Arc::default(),
            budget.clone(),
        )
        .with_startup_timeout(Duration::from_secs(1));
        let component = ComponentSpec {
            name: "fixture".into(),
            engine: EngineKind::Ray,
            artifact: ArtifactRef::PyModule {
                module: "fixture".into(),
                runtime_env: None,
                uri: None,
            },
            alt_artifact: None,
            instances: Default::default(),
            placement: Default::default(),
            upgrade: Default::default(),
            deps: vec![],
            config: None,
            hooks: Default::default(),
        };
        let owned_factory = factory.clone();
        let task = tokio::spawn(async move {
            owned_factory
                .start_gateway(&component, &["/user/fixture".into()], "1.0.0")
                .await
        });
        fixture.ready().await;
        assert_eq!(budget.in_use(), 1);
        if cancel {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(
                budget.in_use(),
                1,
                "cancelled startup released a live group"
            );
        } else {
            assert!(task.await.unwrap().is_err());
        }
        factory.procs.kill_all().await.unwrap();
        fixture.assert_retired();
        assert_eq!(budget.in_use(), 0);
        remote.shutdown().await.unwrap();
        engine.shutdown_internal().await.unwrap();
    }
}
