//! Production Rust sender and a separate, owned JVM process on real TCP.
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
        type_key: "bin:jvm.context",
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
                Ok(bytes) => return serde_json::from_slice(&bytes).expect("complete JVM receipt"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("JVM receipt could not be read: {error}"),
            }
        }
    })
    .await
    .expect("JVM did not publish its lifecycle receipt")
}

async fn observe_owned_exit(pid: u32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            // WNOWAIT observes the direct child without releasing the session
            // identity pinned by PinnedProcess before its last group signal.
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
            assert_eq!(status, 0, "JVM direct-child exit observation failed");
            #[cfg(target_os = "linux")]
            let observed_pid = unsafe { info.si_pid() };
            #[cfg(target_os = "macos")]
            let observed_pid = info.si_pid;
            if observed_pid != 0 {
                assert_eq!(observed_pid as u32, pid);
                assert_eq!(info.si_code, libc::CLD_EXITED, "JVM was killed or crashed");
                #[cfg(target_os = "linux")]
                let exit_code = unsafe { info.si_status() };
                #[cfg(target_os = "macos")]
                let exit_code = info.si_status;
                assert_eq!(exit_code, 0, "JVM did not finish successfully");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("JVM did not exit normally before owned cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Maven test classes and test dependency classpath; see JVM_CAUSAL_CONTEXT.md"]
async fn rust_sender_observes_actual_jvm_handler_children_and_retirement() {
    let classpath = std::env::var_os("PARROT_JVM_CP").expect("explicit JVM test classpath");
    assert!(!classpath.is_empty(), "JVM classpath must be nonempty");
    let directory = match std::env::var_os("PARROT_JVM_CAUSAL_EVIDENCE") {
        Some(parent) => {
            std::fs::create_dir_all(&parent).unwrap();
            tempfile::Builder::new()
                .prefix("jvm-causal-")
                .tempdir_in(parent)
        }
        None => tempfile::Builder::new().prefix("jvm-causal-").tempdir(),
    }
    .unwrap()
    .keep();
    let log = std::fs::File::create(directory.join("gateway.log")).unwrap();
    let mut command = Command::new("java");
    command
        .arg("-cp")
        .arg(classpath)
        .arg("parrot.protocol.jvm.RustCausalGatewayFixture")
        .arg(&directory)
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    let mut owner = PinnedProcess::spawn(command).expect("owned JVM process");
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("rust-jvm-causal-client", None),
        Arc::new(NoLookup),
    )
    .unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        let ready = receipt(&directory.join("ready.json")).await;
        let port = u16::try_from(ready["port"].as_u64().unwrap()).unwrap();
        let pid = u32::try_from(ready["pid"].as_u64().unwrap()).unwrap();
        assert_ne!(port, 0);
        assert_ne!(pid, 0);
        client.start().await.unwrap();
        client
            .connect(&NodeAddr::tcp(
                "jvm-causal-gw",
                ([127, 0, 0, 1], port).into(),
            ))
            .await
            .unwrap();
        let actor = client
            .remote_ref("parrot://jvm-causal-gw/jvm/user/context")
            .unwrap();
        let mut replies = Vec::new();
        for (body, identity) in [(1, Some(3)), (2, None), (3, Some(7))] {
            if body == 3 {
                TraceContext::remote_parent(TraceParent::new([5; 16], [6; 8]).unwrap())
                    .scope(async { actor.deliver(Box::new(Payload(vec![99]))).await })
                    .await
                    .unwrap();
            }
            let operation = async {
                actor
                    .send_with_timeout(Box::new(Payload(vec![body])), Some(Duration::from_secs(10)))
                    .await
            };
            let reply = match identity {
                Some(id) => {
                    TraceContext::remote_parent(TraceParent::new([id; 16], [id + 1; 8]).unwrap())
                        .scope(operation)
                        .await
                }
                None => operation.await,
            }
            .unwrap()
            .downcast::<Payload>()
            .expect("registered response type");
            let value: serde_json::Value = serde_json::from_slice(&reply.0).unwrap();
            assert_eq!(value["payload"], format!("{body:02x}"));
            if let Some(id) = identity {
                let trace = format!("{id:02x}").repeat(16);
                let parent = format!("{:02x}", id + 1).repeat(8);
                assert_eq!(value["parent_trace"], trace);
                assert_eq!(value["trace"], trace);
                assert_eq!(value["parent_span"], parent);
                let child = value["span"].as_str().unwrap();
                assert_eq!(child.len(), 16);
                assert!(child.bytes().all(|byte| byte.is_ascii_hexdigit()));
                assert_ne!(child, "0".repeat(16));
                assert_ne!(child, parent);
            } else {
                for field in ["trace", "span", "parent_trace", "parent_span"] {
                    assert!(value[field].is_null(), "plain request inherited {field}");
                }
            }
            assert!(TraceContext::current().is_none());
            replies.push(value);
        }
        actor.deliver(Box::new(Payload(vec![255]))).await.unwrap();
        let retired = receipt(&directory.join("retired.json")).await;
        assert_eq!(retired["stopped_without_parent"], true);
        let spans = retired["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 3);
        for (reply, id) in [(&replies[0], 3u8), (&replies[2], 7)] {
            assert!(spans.iter().any(|span| {
                span["trace"] == reply["trace"]
                    && span["span"] == reply["span"]
                    && span["parent"] == format!("{:02x}", id + 1).repeat(8)
                    && span["kind"] == "Ask"
                    && span["outcome"] == "Returned"
            }));
        }
        assert!(spans.iter().any(|span| {
            span["trace"] == "05".repeat(16)
                && span["parent"] == "06".repeat(8)
                && span["kind"] == "Tell"
                && span["status"] == "ERROR"
                && span["outcome"] == "Panicked"
        }));
        assert!(
            tokio::net::TcpStream::connect(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
                .await
                .is_err()
        );
        observe_owned_exit(pid).await;
        std::fs::write(
            directory.join("rust-replies.json"),
            serde_json::to_vec_pretty(&replies).unwrap(),
        )
        .unwrap();
    })
    .catch_unwind()
    .await;
    let shutdown = tokio::time::timeout(Duration::from_secs(10), client.shutdown()).await;
    let retirement = owner.retire(Duration::from_secs(10));
    shutdown.expect("Rust client shutdown deadline").unwrap();
    retirement.expect("JVM process group retirement");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    std::fs::write(
        directory.join("rust-retirement.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "observed_jvm_exit": 0,
            "port_closed": true,
            "owned_group_retired": true
        }))
        .unwrap(),
    )
    .unwrap();
}
