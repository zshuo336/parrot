//! Production Rust sender -> real TCP -> Python execution scopes. The same
//! client can target the separately owned real Ray deployment integration gate.
#![cfg(any(target_os = "linux", target_os = "macos"))]

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
        type_key: "bin:context",
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

async fn wait_file(path: &Path) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(bytes) = std::fs::read(path) {
                return bytes;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Python gateway did not publish its lifecycle receipt")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_sender_preserves_parent_through_python_execution() {
    exercise_python(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_sender_observes_recorded_python_handler_children() {
    exercise_python(true).await;
}

async fn exercise_python(recording: bool) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let directory = if let Ok(parent) = std::env::var("PARROT_CAUSAL_RUST_EVIDENCE") {
        std::fs::create_dir_all(&parent).unwrap();
        tempfile::Builder::new()
            .prefix("causal-")
            .tempdir_in(parent)
            .unwrap()
            .keep()
    } else {
        tempfile::Builder::new()
            .prefix("parrot-causal-")
            .tempdir()
            .unwrap()
            .keep()
    };
    let mut process = None;
    let port = match std::env::var("PARROT_CAUSAL_GATEWAY_PORT") {
        Ok(port) => port.parse::<u16>().expect("explicit gateway port"),
        Err(std::env::VarError::NotPresent) => {
            let output = std::fs::File::create(directory.join("gateway.log")).unwrap();
            let mut command = Command::new("python3");
            command
                .arg(root.join("interop/python/tests/support/causal_gateway.py"))
                .arg(&directory)
                .env("PYTHONPATH", root.join("interop/python"))
                .env("PARROT_NODE_ID", "ray-gw-1")
                .env("PARROT_CAUSAL_RECORDING", if recording { "1" } else { "0" })
                .stdout(output.try_clone().unwrap())
                .stderr(output);
            process = Some(PinnedProcess::spawn(command).unwrap());
            let value: serde_json::Value =
                serde_json::from_slice(&wait_file(&directory.join("ready.json")).await).unwrap();
            value["port"].as_u64().unwrap().try_into().unwrap()
        }
        Err(error) => panic!("invalid explicit gateway port: {error}"),
    };
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("rust-causal-client", None),
        Arc::new(NoLookup),
    )
    .unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        client.start().await.unwrap();
        client
            .connect(&NodeAddr::tcp("ray-gw-1", ([127, 0, 0, 1], port).into()))
            .await
            .unwrap();
        let actor = client
            .remote_ref("parrot://ray-gw-1/ray/user/dispatcher")
            .unwrap();
        let mut receipts = Vec::new();
        for (body, identity) in [
            ("first", Some(3)),
            ("fail", Some(5)),
            ("plain", None),
            ("last", Some(7)),
        ] {
            let operation = async {
                actor
                    .send_with_timeout(
                        Box::new(Payload(body.as_bytes().to_vec())),
                        Some(Duration::from_secs(10)),
                    )
                    .await
            };
            let reply = match identity {
                Some(id) => {
                    TraceContext::remote_parent(TraceParent::new([id; 16], [id + 1; 8]).unwrap())
                        .scope(operation)
                        .await
                }
                None => operation.await,
            };
            if body == "fail" {
                assert!(
                    reply.is_err(),
                    "business failure must not become a successful reply"
                );
                continue;
            }
            let reply = reply
                .unwrap()
                .downcast::<Payload>()
                .expect("registered response type");
            let value: serde_json::Value = serde_json::from_slice(&reply.0).unwrap();
            if std::env::var_os("PARROT_CAUSAL_EXPECT_RAY").is_some() {
                assert!(value["namespace"]
                    .as_str()
                    .unwrap()
                    .starts_with("parrot-deployment-"));
                assert!(value["pid"].as_u64().unwrap() > 0);
                assert!(!value["job"].as_str().unwrap().is_empty());
            }
            let expected = identity.map(|id| {
                format!(
                    "01{}{}",
                    format!("{id:02x}").repeat(16),
                    format!("{:02x}", id + 1).repeat(8)
                )
            });
            if recording {
                let encoded = value["parent"].as_str().expect("recorded handler identity");
                assert_eq!(encoded.len(), 50);
                assert!(
                    encoded.starts_with("01") && encoded.bytes().all(|b| b.is_ascii_hexdigit())
                );
                let bytes: Vec<u8> = (2..50)
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&encoded[i..i + 2], 16).unwrap())
                    .collect();
                TraceParent::new(
                    bytes[..16].try_into().unwrap(),
                    bytes[16..].try_into().unwrap(),
                )
                .expect("nonzero handler trace and span");
                if let Some(expected) = expected {
                    assert_eq!(&encoded[..34], &expected[..34], "same causal trace");
                    assert_ne!(&encoded[34..], &expected[34..], "actual handler child");
                }
            } else {
                assert_eq!(value["parent"], serde_json::to_value(expected).unwrap());
            }
            assert_eq!(
                value["payload"],
                body.as_bytes()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            assert!(TraceContext::current().is_none());
            receipts.push(value);
        }
        std::fs::write(
            directory.join("handler-receipts.json"),
            serde_json::to_vec_pretty(&receipts).unwrap(),
        )
        .unwrap();
    });
    use futures::FutureExt;
    let result = result.catch_unwind().await;
    let shutdown = tokio::time::timeout(Duration::from_secs(10), client.shutdown()).await;
    if let Some(mut owned) = process.take() {
        let receipt: serde_json::Value =
            serde_json::from_slice(&wait_file(&directory.join("retired.json")).await).unwrap();
        assert_eq!(receipt["ok"], true);
        owned.retire(Duration::from_secs(10)).unwrap();
    }
    shutdown.expect("Rust client shutdown deadline").unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
