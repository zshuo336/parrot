//! Production Rust sender, actual SDK spans, native workers and a separate guest VM.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use futures::FutureExt;
use parrot_api::{
    address::ActorRef,
    trace::{TraceContext, TraceParent},
    types::BoxedMessage,
};
use parrot_remote::{
    process_group::PinnedProcess, LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{path::Path, process::Command, sync::Arc, time::Duration};

struct Payload(Vec<u8>);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:erlang.context",
        type_id: std::any::TypeId::of::<Payload>(),
        encode: |message: &BoxedMessage| {
            Ok(message.downcast_ref::<Payload>().ok_or("wrong payload")?.0.clone())
        },
        decode: |bytes: &[u8]| Ok(Box::new(Payload(bytes.to_vec())) as BoxedMessage),
    }
}
struct NoLookup;
#[async_trait::async_trait]
impl LocalLookup for NoLookup {
    async fn lookup(&self, _: &str) -> Option<Box<dyn ActorRef>> {
        None
    }
}

async fn receipt(path: &Path) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match std::fs::read(path) {
                Ok(bytes) => {
                    return serde_json::from_slice(&bytes).expect("complete Erlang receipt")
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(error) => panic!("Erlang receipt could not be read: {error}"),
            }
        }
    })
    .await
    .expect("Erlang did not publish its lifecycle receipt")
}

async fn observe_owned_exit(pid: u32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let status = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if status != 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            assert_eq!(status, 0, "Erlang direct-child exit observation failed");
            #[cfg(target_os = "linux")]
            let observed_pid = unsafe { info.si_pid() };
            #[cfg(target_os = "macos")]
            let observed_pid = info.si_pid;
            if observed_pid != 0 {
                assert_eq!(observed_pid as u32, pid);
                assert_eq!(
                    info.si_code,
                    libc::CLD_EXITED,
                    "Erlang was killed or crashed"
                );
                #[cfg(target_os = "linux")]
                let exit_code = unsafe { info.si_status() };
                #[cfg(target_os = "macos")]
                let exit_code = info.si_status;
                assert_eq!(exit_code, 0, "Erlang did not finish successfully");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Erlang did not exit normally before owned cleanup");
}

async fn ask(actor: &dyn ActorRef, body: u8) -> serde_json::Value {
    let reply = actor
        .send_with_timeout(Box::new(Payload(vec![body])), Some(Duration::from_secs(10)))
        .await
        .unwrap()
        .downcast::<Payload>()
        .expect("registered response type");
    serde_json::from_slice(&reply.0).expect("actual Erlang callback reply")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires actual Erlang SDK/fixture; see ERLANG_CAUSAL_CONTEXT.md"]
async fn rust_sender_observes_actual_erlang_handlers_and_guest_retirement() {
    let classes =
        std::env::var_os("PARROT_ERLANG_CAUSAL_CLASSES").expect("explicit Erlang fixture classes");
    let artifact =
        std::env::var_os("PARROT_CAUSAL_ARTIFACT").expect("component-owned SDK artifact");
    let parent =
        std::env::var_os("PARROT_ERLANG_CAUSAL_EVIDENCE").expect("explicit evidence directory");
    std::fs::create_dir_all(&parent).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("rust-erlang-")
        .tempdir_in(parent)
        .unwrap()
        .keep();
    let log = std::fs::File::create(directory.join("gateway.log")).unwrap();
    let mut command = Command::new("erl");
    command
        .args(["-noshell", "-pa"])
        .arg(classes)
        .args(["-eval", "causal_rust_fixture:run()."])
        .env("PARROT_NODE_ID", "erlang-causal-gw")
        .env("PARROT_CAUSAL_ARTIFACT", artifact)
        .env("PARROT_ERLANG_CAUSAL_RECEIPT", &directory)
        .env("ERL_CRASH_DUMP", directory.join("crash.dump"))
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    let mut owner = PinnedProcess::spawn(command).expect("owned Erlang process");
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("rust-erlang-causal-client", None),
        Arc::new(NoLookup),
    )
    .unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        let ready = receipt(&directory.join("ready.json")).await;
        let port = u16::try_from(ready["port"].as_u64().unwrap()).unwrap();
        let pid = u32::try_from(ready["pid"].as_u64().unwrap()).unwrap();
        let guest = ready["guest_pid"].as_u64().unwrap();
        assert_ne!(port, 0);
        assert_ne!(pid, 0);
        assert_ne!(guest, u64::from(pid));
        client.start().await.unwrap();
        client
            .connect(&NodeAddr::tcp(
                "erlang-causal-gw",
                ([127, 0, 0, 1], port).into(),
            ))
            .await
            .unwrap();
        let mut observations = Vec::new();
        for (name, expected_pid) in [("rust-native", u64::from(pid)), ("rust-guest", guest)] {
            let actor = client
                .remote_ref(&format!("parrot://erlang-causal-gw/user/{name}"))
                .unwrap();
            let mut replies = Vec::new();
            for identity in [Some(3u8), None, Some(7)] {
                let value = match identity {
                    Some(id) => {
                        TraceContext::remote_parent(
                            TraceParent::new([id; 16], [id + 1; 8]).unwrap(),
                        )
                        .scope(ask(&actor, 1))
                        .await
                    }
                    None => ask(&actor, 1).await,
                };
                assert_eq!(value["pid"], expected_pid);
                assert_eq!(value["operation"], "ask");
                if let Some(id) = identity {
                    assert_eq!(value["trace"], format!("{id:02x}").repeat(16));
                    let span = value["span"].as_str().unwrap();
                    assert_eq!(span.len(), 16);
                    assert!(span.bytes().all(|byte| byte.is_ascii_hexdigit()));
                    assert_ne!(span, "0".repeat(16));
                    assert_ne!(span, format!("{:02x}", id + 1).repeat(8));
                } else {
                    assert!(value["trace"].is_null());
                    assert!(value["span"].is_null());
                }
                assert!(TraceContext::current().is_none());
                replies.push(value);
            }
            TraceContext::remote_parent(TraceParent::new([5; 16], [6; 8]).unwrap())
                .scope(async { actor.deliver(Box::new(Payload(vec![99]))).await })
                .await
                .unwrap();
            let snapshot = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let value = ask(&actor, 254).await;
                    if value["spans"].as_array().unwrap().len() >= 3 {
                        break value;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("actual SDK handler spans were not exported");
            let spans = snapshot["spans"].as_array().unwrap();
            assert_eq!(spans.len(), 3);
            for (reply, id) in [(&replies[0], 3u8), (&replies[2], 7)] {
                assert!(spans.iter().any(|span| span["trace"] == reply["trace"]
                    && span["span"] == reply["span"]
                    && span["parent"] == format!("{:02x}", id + 1).repeat(8)
                    && span["outcome"] == "Returned"));
            }
            assert!(spans.iter().any(|span| span["trace"] == "05".repeat(16)
                && span["parent"] == "06".repeat(8)
                && span["status"] == "error"
                && span["outcome"] == "Panicked"));
            assert!(!snapshot.to_string().contains("private-exception-text"));
            observations
                .push(serde_json::json!({"component":name,"replies":replies,"snapshot":snapshot}));
        }
        std::fs::write(directory.join("stop"), []).unwrap();
        let retired = receipt(&directory.join("retired.json")).await;
        assert_eq!(retired["guest_exited_before_cleanup"], true);
        assert_eq!(retired["snapshot_removed"], true);
        assert_eq!(retired["port_closed"], true);
        assert!(
            tokio::net::TcpStream::connect(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
                .await
                .is_err()
        );
        observe_owned_exit(pid).await;
        std::fs::write(
            directory.join("rust-observations.json"),
            serde_json::to_vec_pretty(&observations).unwrap(),
        )
        .unwrap();
    })
    .catch_unwind()
    .await;
    let shutdown = tokio::time::timeout(Duration::from_secs(10), client.shutdown()).await;
    let retirement = owner.retire(Duration::from_secs(10));
    shutdown.expect("Rust client shutdown deadline").unwrap();
    retirement.expect("Erlang session retirement");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    std::fs::write(directory.join("rust-retirement.json"),
        serde_json::to_vec_pretty(&serde_json::json!({"observed_erlang_exit":0,"port_closed":true,"owned_session_retired":true})).unwrap()).unwrap();
}
