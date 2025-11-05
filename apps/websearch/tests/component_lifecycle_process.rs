//! Real application processes against controlled TCP administration peers.
//! Peers retain component state after EOF: closing a client is not retirement.
#![cfg(unix)]
use parrot_remote::admin::{decode_sys_event, SysEvent};
use parrot_remote::admin_v2::{encode_admin_reply_v2, v2_err, AdminCommandV2, AdminReplyV2};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    None,
    RejectDeploy,
    AmbiguousDeploy,
    WrongPath,
    RejectInitialStop,
    RejectRetirement,
    MissingOnRetirement,
    BadBusinessReply,
    HoldDeploy,
    HoldRetirement,
}
#[derive(Default, Debug)]
struct State {
    active: bool,
    deployments: usize,
    stops: usize,
    disconnected: bool,
    prefix: Option<String>,
    path: Option<String>,
}
struct Peer {
    address: std::net::SocketAddr,
    state: Arc<Mutex<State>>,
    release: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}
fn frame(kind: u8, cid: u64, key: &str, payload: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&(28 + key.len() as u32 + payload.len() as u32).to_le_bytes());
    data.extend_from_slice(&[1, kind, 0, 0]);
    data.extend_from_slice(&cid.to_le_bytes());
    data.extend_from_slice(&[0, 8, 0, 0, 0, 0, 0, 0]);
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&(key.len() as u32).to_le_bytes());
    data.extend_from_slice(key.as_bytes());
    data.extend_from_slice(payload);
    data
}
impl Peer {
    async fn start(id: &'static str, name: &'static str, fault: Fault) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let observed = state.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = release.clone();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            loop {
                let size = match socket.read_u32_le().await {
                    Ok(size) => size,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                        ) =>
                    {
                        break
                    }
                    Err(error) => panic!("peer read: {error}"),
                };
                assert!((28..=1048576).contains(&size));
                let mut data = vec![0; size as usize];
                socket.read_exact(&mut data).await.unwrap();
                let cid = u64::from_le_bytes(data[4..12].try_into().unwrap());
                let path_len = u32::from_le_bytes(data[20..24].try_into().unwrap()) as usize;
                let key_len =
                    u32::from_le_bytes(data[24 + path_len..28 + path_len].try_into().unwrap())
                        as usize;
                let payload = &data[28 + path_len + key_len..];
                let response = match data[1] {
                    1 => {
                        let mut hs = Vec::new();
                        for (tag, bytes) in [
                            (1, id.as_bytes()),
                            (4, &3u32.to_le_bytes()),
                            (5, &1048576u32.to_le_bytes()),
                            (6, &[0]),
                            (7, &[8]),
                            (9, b"bin"),
                        ] {
                            hs.push(tag);
                            hs.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                            hs.extend_from_slice(bytes);
                        }
                        frame(2, cid, "", &hs)
                    }
                    3 => frame(4, cid, "", &[]),
                    0x20 => {
                        let SysEvent::AdminCommandV2(command) = decode_sys_event(payload).unwrap()
                        else {
                            panic!("unexpected event")
                        };
                        assert_eq!(command.req_id(), cid);
                        let reply = match command {
                            AdminCommandV2::DeployComponent { component, .. } => {
                                let expected = observed.lock().unwrap().prefix.clone().unwrap();
                                let path = format!("/user/{}", component.name);
                                let run = path
                                    .strip_prefix(&(expected + "-"))
                                    .expect("deployment belongs to stopped namespace");
                                assert_eq!(run.len(), 64);
                                assert!(run.bytes().all(|b| b.is_ascii_hexdigit()));
                                {
                                    let mut state = observed.lock().unwrap();
                                    state.path = Some(path.clone());
                                    state.deployments += 1;
                                    state.active = !matches!(fault, Fault::RejectDeploy);
                                }
                                if matches!(fault, Fault::HoldDeploy) {
                                    gate.notified().await;
                                }
                                match fault {
                                    Fault::RejectDeploy => AdminReplyV2::Failed {
                                        req_id: cid,
                                        code: v2_err::SPAWN_FAILED,
                                        detail: "controlled deployment rejection".into(),
                                    },
                                    Fault::AmbiguousDeploy => AdminReplyV2::Stopped { req_id: cid },
                                    Fault::WrongPath => AdminReplyV2::Deployed {
                                        req_id: cid,
                                        instances: vec!["/user/unowned".into()],
                                    },
                                    _ => AdminReplyV2::Deployed {
                                        req_id: cid,
                                        instances: vec![path],
                                    },
                                }
                            }
                            AdminCommandV2::StopComponent { path_prefix, .. } => {
                                {
                                    let mut state = observed.lock().unwrap();
                                    if let Some(path) = &state.path {
                                        assert_eq!(
                                            &path_prefix, path,
                                            "retirement must use exact invocation path"
                                        );
                                    } else {
                                        let (family, run) = path_prefix.rsplit_once('-').unwrap();
                                        assert_eq!(run.len(), 64);
                                        assert!(run.bytes().all(|b| b.is_ascii_hexdigit()));
                                        let owner = family
                                            .strip_prefix("/user/ws-")
                                            .unwrap()
                                            .strip_suffix(&format!("-{name}"))
                                            .unwrap();
                                        assert_eq!(owner.len(), 64);
                                        assert!(owner.bytes().all(|b| b.is_ascii_hexdigit()));
                                        state.prefix = Some(family.to_owned());
                                    }
                                }
                                let deployed = {
                                    let mut state = observed.lock().unwrap();
                                    state.stops += 1;
                                    state.deployments > 0
                                };
                                if deployed && matches!(fault, Fault::HoldRetirement) {
                                    // Keep answering heartbeats, withholding only the Stop receipt.
                                    continue;
                                }
                                let mut state = observed.lock().unwrap();
                                if matches!(fault, Fault::RejectInitialStop)
                                    || (deployed && matches!(fault, Fault::RejectRetirement))
                                {
                                    AdminReplyV2::Failed {
                                        req_id: cid,
                                        code: v2_err::DRAIN_TIMEOUT,
                                        detail: "controlled stop rejection".into(),
                                    }
                                } else if !state.active
                                    || (deployed && matches!(fault, Fault::MissingOnRetirement))
                                {
                                    state.active = false;
                                    AdminReplyV2::Failed {
                                        req_id: cid,
                                        code: v2_err::COMPONENT_NOT_FOUND,
                                        detail: "absent".into(),
                                    }
                                } else {
                                    state.active = false;
                                    AdminReplyV2::Stopped { req_id: cid }
                                }
                            }
                            other => panic!("unexpected administration: {other:?}"),
                        };
                        frame(0x20, cid, "", &encode_admin_reply_v2(&reply))
                    }
                    0x10 => {
                        if matches!(fault, Fault::BadBusinessReply) {
                            frame(0x11, cid, "bin:ws/ClearAck", &[])
                        } else {
                            frame(
                                0x11,
                                cid,
                                "bin:ws/HealthzR",
                                br#"{"terms":0,"postings":0,"queries":0,"docs":0,"segments":0}"#,
                            )
                        }
                    }
                    other => panic!("unexpected frame {other}"),
                };
                // A timeout may close the client while the controlled reply is held.
                if socket.write_all(&response).await.is_err() {
                    break;
                }
            }
            observed.lock().unwrap().disconnected = true;
        });
        Self {
            address,
            state,
            release,
            task,
        }
    }
    async fn finish(&mut self) {
        self.release.notify_one();
        tokio::time::timeout(Duration::from_secs(3), &mut self.task)
            .await
            .unwrap()
            .unwrap();
        assert!(self.state.lock().unwrap().disconnected);
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct App {
    child: tokio::process::Child,
    directory: std::path::PathBuf,
}
impl App {
    fn start(peers: &[(&str, &Peer)], serve: bool) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "ws-components-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let log = std::fs::File::create(directory.join("app.log")).unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_websearch"));
        command
            .args(["--insecure-development", "--port", "0", "--data"])
            .arg(directory.join("data"));
        for (tag, peer) in peers {
            command.arg(format!("{tag}={}", peer.address));
        }
        if serve {
            command.arg("--serve-only");
        }
        let child = command
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Self { child, directory }
    }
    fn text(&self) -> String {
        std::fs::read_to_string(self.directory.join("app.log")).unwrap()
    }
    async fn wait_for(&mut self, marker: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self.text().contains(marker) {
                    return;
                }
                assert!(self.child.try_wait().unwrap().is_none(), "{}", self.text());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    fn signal(&self) {
        assert!(std::process::Command::new("kill")
            .args(["-TERM", &self.child.id().unwrap().to_string()])
            .status()
            .unwrap()
            .success());
    }
    async fn exited(&mut self, success: bool) {
        let status = tokio::time::timeout(Duration::from_secs(40), self.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(if success { 0 } else { 1 }),
            "{}",
            self.text()
        );
        assert_eq!(
            self.text().contains("shutdown complete"),
            success,
            "{}",
            self.text()
        );
    }
}
impl Drop for App {
    fn drop(&mut self) {
        // Keep failed-process evidence; completed successful fixtures are disposable.
        if self
            .child
            .try_wait()
            .ok()
            .flatten()
            .is_some_and(|status| status.success())
        {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
}

#[tokio::test]
async fn serve_only_stop_retires_search_before_disconnect() {
    let mut peer = Peer::start("jvm-search-1", "search", Fault::None).await;
    let mut app = App::start(&[("jvm", &peer)], true);
    app.wait_for("Web listening").await;
    app.signal();
    app.exited(true).await;
    peer.finish().await;
    let state = peer.state.lock().unwrap();
    assert!(
        !state.active,
        "component survives successful application exit: {state:?}"
    );
    assert_eq!(state.stops, 2);
}

async fn partial(fault: Fault) {
    let mut erl = Peer::start("erl-gw-1", "frontier", Fault::None).await;
    let mut ray = Peer::start("ray-gw-1", "tokenizer", fault).await;
    let mut jvm = Peer::start("jvm-search-1", "search", Fault::None).await;
    let mut app = App::start(&[("erl", &erl), ("ray", &ray), ("jvm", &jvm)], false);
    app.exited(false).await;
    for peer in [&mut erl, &mut ray, &mut jvm] {
        peer.finish().await;
    }
    assert!(app.text().contains("ray-gw-1") && app.text().contains("tokenizer"));
    assert!(
        !erl.state.lock().unwrap().active,
        "earlier deployment leaked"
    );
    assert!(
        !ray.state.lock().unwrap().active,
        "ambiguous deployment leaked"
    );
    assert_eq!(
        jvm.state.lock().unwrap().stops,
        0,
        "must not touch unattempted components"
    );
    assert_eq!(
        ray.state.lock().unwrap().stops,
        if matches!(fault, Fault::RejectInitialStop) {
            1
        } else {
            2
        }
    );
}
#[tokio::test]
async fn later_deployment_rejection_rolls_back_earlier_components() {
    partial(Fault::RejectDeploy).await;
}
#[tokio::test]
async fn unexpected_deploy_receipt_also_retires_attempted_component() {
    partial(Fault::AmbiguousDeploy).await;
}
#[tokio::test]
async fn foreign_deployment_path_is_rejected_and_only_requested_component_is_retired() {
    partial(Fault::WrongPath).await;
}
#[tokio::test]
async fn initial_stop_rejection_never_claims_that_component() {
    partial(Fault::RejectInitialStop).await;
}

async fn business_failure(fault: Fault) {
    let mut erl = Peer::start("erl-gw-1", "frontier", fault).await;
    let mut ray = Peer::start("ray-gw-1", "tokenizer", Fault::None).await;
    let mut jvm = Peer::start("jvm-search-1", "search", Fault::BadBusinessReply).await;
    let mut app = App::start(&[("erl", &erl), ("ray", &ray), ("jvm", &jvm)], false);
    if matches!(fault, Fault::HoldRetirement) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if erl.state.lock().unwrap().stops == 2
                    && ray.state.lock().unwrap().stops == 2
                    && jvm.state.lock().unwrap().stops == 2
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("one pending Stop must not delay the other nodes");
        assert!(app.child.try_wait().unwrap().is_none());
        // The failed business task has already ended, but remote retirement has not.
        // A second real application must fail locally, before it can touch any gateway.
        let contender = tokio::process::Command::new(env!("CARGO_BIN_EXE_websearch"))
            .args([
                "--insecure-development",
                "--serve-only",
                "--port",
                "0",
                "--data",
            ])
            .arg(app.directory.join("data"))
            .output()
            .await
            .unwrap();
        assert_eq!(contender.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&contender.stderr).contains("already has an owner"),
            "{}",
            String::from_utf8_lossy(&contender.stderr)
        );
    }
    app.exited(false).await;
    for peer in [&mut erl, &mut ray, &mut jvm] {
        peer.finish().await;
    }
    if matches!(fault, Fault::HoldRetirement) {
        assert!(app.text().contains("admin timeout 30s"), "{}", app.text());
    }
    assert!(
        app.text().contains("clear index failed"),
        "primary failure lost: {}",
        app.text()
    );
    for peer in [&ray, &jvm] {
        assert!(
            !peer.state.lock().unwrap().active,
            "healthy component skipped after cleanup failure"
        );
    }
    if matches!(fault, Fault::RejectRetirement | Fault::HoldRetirement) {
        assert!(
            app.text().contains(&format!(
                "component retirement failed: erl-gw-1/{}",
                erl.state
                    .lock()
                    .unwrap()
                    .path
                    .as_ref()
                    .unwrap()
                    .trim_start_matches("/user/")
            )),
            "{}",
            app.text()
        );
        assert!(erl.state.lock().unwrap().active);
    } else {
        assert!(!erl.state.lock().unwrap().active);
    }
}
#[tokio::test]
async fn bad_business_reply_still_retires_all_components() {
    business_failure(Fault::None).await;
}
#[tokio::test]
async fn stop_rejection_keeps_primary_failure_and_retires_other_nodes() {
    business_failure(Fault::RejectRetirement).await;
}
#[tokio::test]
async fn stop_timeout_does_not_prevent_other_nodes_retiring() {
    business_failure(Fault::HoldRetirement).await;
}

#[tokio::test]
async fn cleanup_failure_changes_success_to_nonzero() {
    let mut peer = Peer::start("jvm-search-1", "search", Fault::RejectRetirement).await;
    let mut app = App::start(&[("jvm", &peer)], true);
    app.wait_for("Web listening").await;
    app.signal();
    app.exited(false).await;
    peer.finish().await;
    assert!(peer.state.lock().unwrap().active);
    assert!(
        app.text().contains(&format!(
            "component retirement failed: jvm-search-1/{}",
            peer.state
                .lock()
                .unwrap()
                .path
                .as_ref()
                .unwrap()
                .trim_start_matches("/user/")
        )),
        "{}",
        app.text()
    );
}
#[tokio::test]
async fn already_absent_on_retirement_is_success() {
    let mut peer = Peer::start("jvm-search-1", "search", Fault::MissingOnRetirement).await;
    let mut app = App::start(&[("jvm", &peer)], true);
    app.wait_for("Web listening").await;
    app.signal();
    app.exited(true).await;
    peer.finish().await;
    assert_eq!(peer.state.lock().unwrap().stops, 2);
}
#[tokio::test]
async fn signal_during_deployment_waits_for_receipt_then_retires() {
    let mut peer = Peer::start("jvm-search-1", "search", Fault::HoldDeploy).await;
    let mut app = App::start(&[("jvm", &peer)], true);
    tokio::time::timeout(Duration::from_secs(10), async {
        while peer.state.lock().unwrap().deployments == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    app.signal();
    app.wait_for("stop requested:").await;
    peer.release.notify_one();
    app.exited(true).await;
    peer.finish().await;
    assert!(!peer.state.lock().unwrap().active);
}

#[tokio::test]
async fn serve_directory_lease_outlives_http_until_remote_retirement_finishes() {
    let mut peer = Peer::start("jvm-search-1", "search", Fault::HoldRetirement).await;
    let mut app = App::start(&[("jvm", &peer)], true);
    app.wait_for("Web listening").await;
    app.signal();
    tokio::time::timeout(Duration::from_secs(5), async {
        while peer.state.lock().unwrap().stops < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let contender = tokio::process::Command::new(env!("CARGO_BIN_EXE_websearch"))
        .args([
            "--insecure-development",
            "--serve-only",
            "--port",
            "0",
            "--data",
        ])
        .arg(app.directory.join("data"))
        .output()
        .await
        .unwrap();
    assert_eq!(contender.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&contender.stderr).contains("already has an owner"),
        "{}",
        String::from_utf8_lossy(&contender.stderr)
    );
    app.exited(false).await;
    peer.finish().await;
    // Once the first process exits, a new handle can own the directory again.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(app.directory.join("data/.crawl.lock"))
        .unwrap();
    lock.try_lock().unwrap();
}
