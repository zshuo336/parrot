#![cfg(all(feature = "host", unix))]
#[allow(dead_code)]
#[path = "../examples/support/replay_counter.rs"]
mod counter;
use parrot_api::{address::ActorRef, types::BoxedActorRef};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
#[test]
fn fixture_command() {
    let Some(args) = std::env::var_os("PARROT_MIRROR_TEST_ARGS") else {
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
struct Case {
    dir: tempfile::TempDir,
}
impl Case {
    fn new(policy: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = format!("name = 'mirror-cli'\nversion = '1.0.0'\n{policy}\n");
        for name in ["primary", "shadow"] {
            manifest += &format!("[[components]]\nname = '{name}'\nengine = 'parrot'\nartifact = {{ Props = {{ factory = 'example.replay-counter' }} }}\n[components.config]\njournal = '{}'\n", dir.path().join(format!("{name}.log")).display());
        }
        manifest += "[config_overlay.remote.node]\nbind = '127.0.0.1:0'\n[config_overlay.thread]\nshared_pool_size = 1\nshared_burst_workers_max = 0\n";
        std::fs::write(dir.path().join("app.toml"), manifest).unwrap();
        Self { dir }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn text(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap_or_default()
    }
    fn start(&self) -> tokio::process::Child {
        let args = vec![
            "run".to_owned(),
            "--manifest".to_owned(),
            self.path("app.toml").to_str().unwrap().to_owned(),
        ];
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_command", "--nocapture"])
            .env(
                "PARROT_MIRROR_TEST_ARGS",
                serde_json::to_string(&args).unwrap(),
            )
            .env("PARROT_APP_READY_FILE", self.path("ready"))
            .env_remove("PARROT_CONFIG")
            .env_remove("PARROT_REPO_ROOT")
            .current_dir(self.dir.path())
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
        if let Some(root) = std::env::var_os("PARROT_APP_MIRROR_EVIDENCE") {
            let target = Path::new(&root).join(self.dir.path().file_name().unwrap());
            std::fs::create_dir_all(&target).unwrap();
            for entry in std::fs::read_dir(self.dir.path()).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_file() {
                    std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
                }
            }
        }
    }
}
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
#[tokio::test]
async fn actual_cli_manifest_mirrors_tcp_and_retires_both_actors_on_signal() {
    let case =
        Case::new("[[mirrors]]\nsource = 'primary'\nshadow = 'shadow'\ndelivery_timeout_ms = 3000");
    let mut child = case.start();
    let endpoint = case.ready(&mut child).await;
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("mirror-cli-client", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Empty),
    )
    .unwrap();
    client.start().await.unwrap();
    let address = NodeAddr::tcp("app-host", endpoint.trim().parse().unwrap());
    client.connect(&address).await.unwrap();
    let actor = client
        .remote_ref(&format!("parrot://{}/user/primary", address.node_id))
        .unwrap();
    let reply = actor.send(Box::new(counter::Count(7))).await.unwrap();
    assert_eq!(reply.downcast::<counter::Count>().unwrap().0, 7);
    actor
        .deliver_with_ack(Box::new(counter::Count(5)), Duration::from_secs(3))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if case.text("shadow.log").contains("value 12") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    client.shutdown().await.unwrap();
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGTERM) },
        0
    );
    assert!(case.wait(&mut child).await, "{}", case.text("stderr.log"));
    for name in ["primary.log", "shadow.log"] {
        let log = case.text(name);
        assert_eq!(log.matches("handling ").count(), 2, "{log}");
        assert!(log.contains("stopped") && log.contains("disposed"), "{log}");
    }
}
#[tokio::test]
async fn invalid_mirror_manifest_never_publishes_ready_or_starts_actors() {
    let case = Case::new("[[mirrors]]\nsource = 'primary'\nshadow = 'missing'");
    let mut child = case.start();
    assert!(!case.wait(&mut child).await);
    assert!(!case.path("ready").exists());
    assert!(!case.path("primary.log").exists());
    assert!(!case.path("shadow.log").exists());
    assert!(case.text("stderr.log").contains("mirror"));
}
