#![cfg(all(feature = "host", unix))]

#[allow(dead_code)]
#[path = "../examples/support/replay_counter.rs"]
mod counter;

use parrot_api::{address::ActorRef, types::BoxedActorRef};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
    host::ThreadPropsSpawner,
    planner::{plan, CandidateNode, LocalNodeTopology},
    AppManifest, ComponentSpec,
};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[test]
fn fixture_command() {
    let Some(args) = std::env::var_os("PARROT_METADATA_TEST_ARGS") else {
        return;
    };
    let args: Vec<String> = serde_json::from_str(args.to_str().unwrap()).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(parrot_app::cli::execute(args.into_iter().map(Into::into)));
    if let Err(error) = &result {
        eprintln!("CLI_ERROR={error}");
    }
    drop(runtime);
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}

struct Case(tempfile::TempDir);
impl Case {
    fn new(base_role: &str, overlay_role: Option<&str>, role: Option<&str>, extra: &str) -> Self {
        let case = Self(tempfile::tempdir().unwrap());
        std::fs::write(case.path("parrot.toml"), format!(
            "[remote.node]\nnode_id = 'metadata-host'\nbind = '127.0.0.1:0'\ntopology_role = '{base_role}'\n[thread]\nshared_pool_size = 1\nshared_burst_workers_max = 0\n"
        )).unwrap();
        let mut manifest = format!(
            "name = 'metadata-cli'\nversion = '1.0.0'\n[[components]]\nname = 'counter'\nengine = 'parrot'\nartifact = {{ Props = {{ factory = 'example.replay-counter' }} }}\n[components.config]\njournal = '{}'\n[components.placement]\n{extra}\n",
            case.path("actor.log").display()
        );
        if let Some(role) = role {
            manifest += &format!("role = '{role}'\n");
        }
        if let Some(role) = overlay_role {
            manifest += &format!("[config_overlay.remote.node]\ntopology_role = '{role}'\n");
        }
        std::fs::write(case.path("app.toml"), manifest).unwrap();
        case
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }
    fn text(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap_or_default()
    }
    fn start(&self) -> tokio::process::Child {
        let args = vec![
            "run".to_owned(),
            "--manifest".into(),
            self.path("app.toml").to_str().unwrap().into(),
            "--record".into(),
            self.path("capture").to_str().unwrap().into(),
        ];
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_command", "--nocapture"])
            .env(
                "PARROT_METADATA_TEST_ARGS",
                serde_json::to_string(&args).unwrap(),
            )
            .env("PARROT_APP_READY_FILE", self.path("ready"))
            .env_remove("PARROT_CONFIG")
            .env_remove("PARROT_REPO_ROOT")
            .current_dir(self.0.path())
            .stdout(std::fs::File::create(self.path("stdout.log")).unwrap())
            .stderr(std::fs::File::create(self.path("stderr.log")).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }
    async fn ready(&self, child: &mut tokio::process::Child) -> String {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let endpoint = self.text("ready");
                if !endpoint.is_empty() {
                    return endpoint;
                }
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "premature exit: {}",
                    self.text("stderr.log")
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("readiness deadline")
    }
    async fn wait(&self, child: &mut tokio::process::Child) -> bool {
        tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .expect("child exit deadline")
            .unwrap()
            .success()
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        if let Some(root) = std::env::var_os("PARROT_METADATA_EVIDENCE") {
            let target = std::path::Path::new(&root).join(self.0.path().file_name().unwrap());
            std::fs::create_dir_all(&target).unwrap();
            for entry in std::fs::read_dir(self.0.path()).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_file() {
                    std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
                }
            }
        }
    }
}

struct NoGateway;
#[async_trait::async_trait]
impl GatewayFactory for NoGateway {
    async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        Ok(())
    }
    async fn start_gateway(
        &self,
        _: &ComponentSpec,
        _: &[String],
        _: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("no gateways")
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        panic!("no gateways")
    }
}
#[async_trait::async_trait]
impl LocalLookup for NoGateway {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}

#[tokio::test]
async fn cli_runs_matching_roles_from_effective_config_and_retires_real_actors() {
    for (base, overlay, role) in [
        ("normal", None, "normal"),
        ("hub", None, "hub"),
        ("normal", Some("hub"), "hub"),
        ("hub", Some("normal"), "normal"),
        ("border", None, "border"),
    ] {
        let case = Case::new(base, overlay, Some(role), "");
        let mut child = case.start();
        let endpoint = case.ready(&mut child).await;
        let client = RemoteActorSystem::new(
            RemoteConfig::tcp("metadata-client", Some("127.0.0.1:0".parse().unwrap())),
            Arc::new(NoGateway),
        )
        .unwrap();
        client.start().await.unwrap();
        client
            .connect(&NodeAddr::tcp(
                "metadata-host",
                endpoint.trim().parse().unwrap(),
            ))
            .await
            .unwrap();
        let actor = client
            .remote_ref("parrot://metadata-host/user/counter")
            .unwrap();
        let reply = actor.send(Box::new(counter::Count(7))).await.unwrap();
        assert_eq!(reply.downcast::<counter::Count>().unwrap().0, 7);
        client.shutdown().await.unwrap();
        // SAFETY: this is the owned child process, still running after readiness.
        assert_eq!(
            unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGTERM) },
            0
        );
        assert!(case.wait(&mut child).await, "{}", case.text("stderr.log"));
        let log = case.text("actor.log");
        assert_eq!(log.matches("started\n").count(), 1, "{log}");
        assert!(
            log.contains("value 7\n") && log.contains("stopped\n") && log.contains("disposed\n"),
            "{log}"
        );
    }
}

#[tokio::test]
async fn cli_rejects_unmatched_metadata_before_recording_or_starting_actors() {
    for (base, overlay, role, extra) in [
        ("normal", None, Some("hub"), ""),
        ("hub", Some("normal"), Some("hub"), ""),
        ("hub", None, Some("hub-backup"), ""),
        ("normal", None, None, "realm = 'west'"),
        ("normal", None, None, "label = 'gpu'"),
    ] {
        let case = Case::new(base, overlay, role, extra);
        let mut child = case.start();
        assert!(!case.wait(&mut child).await);
        assert!(!case.path("ready").exists());
        assert!(!case.path("capture").exists());
        assert!(!case.path("actor.log").exists());
        assert!(
            case.text("stderr.log").contains("unplaceable"),
            "{}",
            case.text("stderr.log")
        );
    }
}

#[tokio::test]
async fn explicit_realm_and_label_topology_deploys_and_retires_a_real_actor() {
    let case = Case::new("hub", None, Some("hub"), "realm = 'west'\nlabel = 'gpu'");
    let manifest = AppManifest::from_file(&case.path("app.toml")).unwrap();
    let topology = LocalNodeTopology::new(
        CandidateNode {
            node_id: "metadata-host".into(),
            role: "hub".into(),
            labels: vec!["gpu".into()],
            load: 0.0,
        },
        Some("west".into()),
    );
    let plan = plan(&manifest, &topology).unwrap();
    assert_eq!(plan.order[0].nodes, ["metadata-host"]);
    let config = parrot_config::ParrotConfig::builder()
        .load_file(case.path("parrot.toml"))
        .unwrap();
    let resolved = parrot_app::assemble::effective_config(&manifest, &config).unwrap();
    let engine = parrot::thread::system::ThreadActorSystem::shared(
        parrot::thread::config::ThreadActorSystemConfig::from_resolved(&resolved),
    );
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &topology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    context
        .assemble(&plan, &deployer, &manifest, &config)
        .await
        .unwrap();
    let actor = context.component_ref("counter").unwrap().clone_boxed();
    let reply = actor.send(Box::new(counter::Count(9))).await.unwrap();
    assert_eq!(reply.downcast::<counter::Count>().unwrap().0, 9);
    context.teardown(&deployer).await.unwrap();
    assert!(context.started().is_empty());
    assert!(actor.send(Box::new(counter::Count(1))).await.is_err());
    engine.shutdown_internal().await.unwrap();
    let log = case.text("actor.log");
    assert!(
        log.contains("value 9\n") && log.contains("stopped\n") && log.contains("disposed\n"),
        "{log}"
    );
}
