#![cfg(feature = "dylib")]

use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_abi::*;
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorRef, RetirementStatus},
    trace::{SpanStatus, TraceContext, TraceRecorder},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::ParrotSpawner,
    host::{
        dylib::{DylibAsk, DylibReply, NativeMessageGrant, NativeMessageOperation},
        ThreadPropsSpawner,
    },
    ArtifactRef, ParrotAppHook,
};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};

#[derive(Debug)]
struct Request(Vec<u8>);
#[derive(Debug)]
struct Response(Vec<u8>);
struct HookAlias;
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:native.request#v1", type_id: std::any::TypeId::of::<Request>(),
        encode: |value| Ok(value.downcast_ref::<Request>().unwrap().0.clone()),
        decode: |bytes| Ok(Box::new(Request(bytes.to_vec())) as BoxedMessage),
    }
}
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:native.reply#v1", type_id: std::any::TypeId::of::<Response>(),
        encode: |value| Ok(value.downcast_ref::<Response>().unwrap().0.clone()),
        decode: |bytes| Ok(Box::new(Response(bytes.to_vec())) as BoxedMessage),
    }
}
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:native.hook-alias#v1", type_id: std::any::TypeId::of::<HookAlias>(),
        encode: |_| Err("test alias only".into()),
        decode: |_| Ok(Box::new(ParrotAppHook(2)) as BoxedMessage),
    }
}
struct Target {
    calls: Arc<AtomicUsize>,
    parents: Arc<Mutex<Vec<Option<TraceContext>>>>,
    release: Arc<tokio::sync::Notify>,
    completed: Arc<AtomicUsize>,
}
impl Actor for Target {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.parents.lock().unwrap().push(TraceContext::current());
            let message = message.downcast::<Request>().unwrap();
            if message.0 == b"blocked" {
                self.release.notified().await;
            }
            if message.0 == b"slow" {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if message.0 == b"wrong" {
                return Ok(Box::new(13u64) as BoxedMessage);
            }
            let mut bytes = b"business:".to_vec();
            bytes.extend_from_slice(&message.0);
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Response(bytes)) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
mod fixture_build {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../parrot-abi/tests/common/build.rs"
    ));
}

fn fixture() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../parrot-abi/tests/fixtures");
        let target = root.join("target");
        let output = fixture_build::cargo()
            .args(["build", "--release", "--locked", "--target-dir"])
            .arg(&target)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        target
            .join("release")
            .join(format!(
                "libparrot_abi_messagingcomp.{}",
                std::env::consts::DLL_EXTENSION
            ))
            .canonicalize()
            .unwrap()
    })
}
struct Setup {
    engine: Arc<ThreadActorSystem>,
    spawner: ThreadPropsSpawner,
    artifact: ArtifactRef,
    digest: String,
    calls: Arc<AtomicUsize>,
    parents: Arc<Mutex<Vec<Option<TraceContext>>>>,
    target: BoxedActorRef,
    release: Arc<tokio::sync::Notify>,
    completed: Arc<AtomicUsize>,
}
impl Setup {
    async fn new() -> Self {
        let engine = ThreadActorSystem::shared(Default::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let parents = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(AtomicUsize::new(0));
        let target = engine
            .spawn_at(
                Target {
                    calls: calls.clone(),
                    parents: parents.clone(),
                    release: release.clone(),
                    completed: completed.clone(),
                },
                "/user/target",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let digest = format!("{:x}", Sha256::digest(std::fs::read(fixture()).unwrap()));
        // SAFETY: owned fixture and its dependencies obey the trusted ABI.
        let spawner =
            unsafe { ThreadPropsSpawner::new(engine.clone()).with_trusted_dylib(&digest) }.unwrap();
        let artifact = ArtifactRef::Dylib {
            digest: digest.clone(),
            uri: url::Url::from_file_path(fixture()).unwrap().into(),
            abi: 1,
        };
        Self {
            engine,
            spawner,
            artifact,
            digest,
            calls,
            parents,
            target: Box::new(target),
            release,
            completed,
        }
    }
    fn grant(&self, operation: NativeMessageOperation) -> NativeMessageGrant {
        NativeMessageGrant {
            source_path: "/user/native".into(),
            artifact_digest: self.digest.clone(),
            destination: "/user/target".into(),
            request_type_key: "bin:native.request#v1".into(),
            operation,
        }
    }
    fn ask(&self) -> NativeMessageGrant {
        self.grant(NativeMessageOperation::Ask {
            reply_type_key: "bin:native.reply#v1".into(),
        })
    }
    async fn spawn(&self, spawner: &ThreadPropsSpawner, config: &str, path: &str) -> BoxedActorRef {
        spawner
            .spawn_artifact(
                &self.artifact,
                &[path.into()],
                Some(&toml::from_str(config).unwrap()),
            )
            .await
            .unwrap()
            .remove(0)
    }
    async fn finish(self, actor: BoxedActorRef) {
        actor.stop().await.unwrap();
        assert!(matches!(
            actor.retirement_status(),
            RetirementStatus::Retired(Ok(()))
        ));
        self.target.stop().await.unwrap();
        self.engine.shutdown_internal().await.unwrap();
        assert!(self.spawner.pending_cleanup().is_empty());
    }
}
async fn call(actor: &dyn ActorRef, key: &str, bytes: &[u8]) -> (u32, Vec<u8>) {
    let reply = actor
        .send(Box::new(DylibAsk {
            type_key: key.into(),
            payload: bytes.to_vec(),
        }))
        .await
        .unwrap()
        .downcast::<DylibReply>()
        .unwrap()
        .0;
    (
        u32::from_le_bytes(reply[..4].try_into().unwrap()),
        reply[4..].to_vec(),
    )
}

#[tokio::test]
async fn actual_guest_asks_and_tells_and_composes_recording_and_explicit_parent() {
    let setup = Setup::new().await;
    let spawner = setup
        .spawner
        .clone()
        .with_dylib_messaging(vec![setup.ask(), setup.grant(NativeMessageOperation::Tell)])
        .unwrap();
    let actor = setup.spawn(&spawner, "", "/user/native").await;
    let recorder = TraceRecorder::new(16).unwrap();
    let root = recorder.root();
    assert_eq!(
        root.scope(call(&*actor, "ask", b"alpha")).await,
        (ABI_MESSAGE_OK, b"business:alpha".to_vec())
    );
    assert_eq!(call(&*actor, "tell", b"beta").await.0, ABI_MESSAGE_OK);
    let barrier = setup
        .target
        .send(Box::new(Request(b"barrier".to_vec())))
        .await
        .unwrap();
    assert_eq!(
        barrier.downcast::<Response>().unwrap().0,
        b"business:barrier"
    );
    assert_eq!(setup.calls.load(Ordering::SeqCst), 3);
    let target_context = {
        let parents = setup.parents.lock().unwrap();
        assert_eq!(parents[0].as_ref().unwrap().trace_id(), root.trace_id());
        assert!(parents[1].is_none());
        parents[0].as_ref().unwrap().clone()
    };
    let snapshot = recorder.snapshot();
    let guest = snapshot
        .records
        .iter()
        .find(|record| record.target == "native-message" && record.status == SpanStatus::Ok)
        .unwrap();
    let native = snapshot
        .records
        .iter()
        .find(|record| record.span_id == guest.parent_span_id.unwrap())
        .unwrap();
    assert_eq!(native.parent_span_id, Some(root.span_id()));
    let target = snapshot
        .records
        .iter()
        .find(|record| record.span_id == target_context.span_id())
        .unwrap();
    assert_eq!(target.parent_span_id, Some(native.span_id));
    assert_eq!(target.trace_id, native.trace_id);
    setup.finish(actor).await;
}

#[tokio::test]
async fn default_denial_and_immutable_clone_policy_do_not_reach_business() {
    let setup = Setup::new().await;
    let enabled = setup
        .spawner
        .clone()
        .with_dylib_messaging(vec![setup.ask()])
        .unwrap();
    let actor = setup.spawn(&setup.spawner, "", "/user/native").await;
    for _ in 0..2 {
        assert_eq!(call(&*actor, "ask", b"denied").await.0, ABI_MESSAGE_DENIED);
    }
    assert_eq!(setup.calls.load(Ordering::SeqCst), 0);
    actor.stop().await.unwrap();
    let replacement = setup.spawn(&enabled, "", "/user/native").await;
    assert_eq!(
        call(&*replacement, "ask", b"allowed").await.0,
        ABI_MESSAGE_OK
    );
    setup.finish(replacement).await;
}

#[tokio::test]
async fn wrong_source_digest_destination_request_reply_and_operation_are_denied() {
    let setup = Setup::new().await;
    for index in 0..6 {
        let mut grant = setup.ask();
        match index {
            0 => grant.source_path = "/user/other".into(),
            1 => grant.artifact_digest = "0".repeat(64),
            2 => grant.destination = "/user/other".into(),
            3 => grant.request_type_key = "bin:other#v1".into(),
            4 => {
                grant.operation = NativeMessageOperation::Ask {
                    reply_type_key: "bin:other#v1".into(),
                }
            }
            _ => grant.operation = NativeMessageOperation::Tell,
        }
        let spawner = setup
            .spawner
            .clone()
            .with_dylib_messaging(vec![grant])
            .unwrap();
        let actor = setup.spawn(&spawner, "", "/user/native").await;
        assert_eq!(
            call(&*actor, "ask", b"denied").await.0,
            ABI_MESSAGE_DENIED,
            "case {index}"
        );
        actor.stop().await.unwrap();
    }
    assert_eq!(setup.calls.load(Ordering::SeqCst), 0);
    let actor = setup.spawn(&setup.spawner, "", "/user/native").await;
    setup.finish(actor).await;
}

#[tokio::test]
async fn timeout_wrong_reply_and_small_output_do_not_retry_business() {
    let setup = Setup::new().await;
    let spawner = setup
        .spawner
        .clone()
        .with_dylib_messaging(vec![setup.ask()])
        .unwrap();
    let actor = setup.spawn(&spawner, "", "/user/native").await;
    assert_eq!(
        call(&*actor, "timeout", b"slow").await.0,
        ABI_MESSAGE_TIMEOUT
    );
    assert_eq!(call(&*actor, "ask", b"wrong").await.0, ABI_MESSAGE_REPLY);
    assert_eq!(
        call(&*actor, "small", b"one effect").await,
        (ABI_MESSAGE_LIMIT, vec![])
    );
    assert_eq!(
        call(&*actor, "ask", b"recovered").await,
        (ABI_MESSAGE_OK, b"business:recovered".to_vec())
    );
    assert_eq!(setup.calls.load(Ordering::SeqCst), 4);
    setup.finish(actor).await;
}

#[tokio::test]
async fn call_budget_is_shared_and_resets_without_allowing_a_sixty_fifth_effect() {
    let setup = Setup::new().await;
    let spawner = setup
        .spawner
        .clone()
        .with_dylib_messaging(vec![setup.grant(NativeMessageOperation::Tell)])
        .unwrap();
    let actor = setup.spawn(&spawner, "", "/user/native").await;
    let (first, bytes) = call(&*actor, "budget", b"effect").await;
    assert_eq!(first, ABI_MESSAGE_OK);
    let codes: Vec<_> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| u32::from_le_bytes(*bytes))
        .collect();
    assert_eq!(codes.len(), 64);
    assert!(codes[..63].iter().all(|code| *code == ABI_MESSAGE_OK));
    assert_eq!(codes[63], ABI_MESSAGE_LIMIT);
    setup
        .target
        .send(Box::new(Request(b"barrier".to_vec())))
        .await
        .unwrap();
    assert_eq!(setup.calls.load(Ordering::SeqCst), 65);
    assert_eq!(
        call(&*actor, "tell", b"new session").await.0,
        ABI_MESSAGE_OK
    );
    setup
        .target
        .send(Box::new(Request(b"barrier".to_vec())))
        .await
        .unwrap();
    assert_eq!(setup.calls.load(Ordering::SeqCst), 67);
    setup.finish(actor).await;
}

#[tokio::test]
async fn invalid_views_and_lifecycle_codec_aliases_cannot_trigger_target() {
    let setup = Setup::new().await;
    let mut grant = setup.ask();
    grant.request_type_key = "bin:native.hook-alias#v1".into();
    let spawner = setup
        .spawner
        .clone()
        .with_dylib_messaging(vec![grant])
        .unwrap();
    let actor = setup
        .spawn(
            &spawner,
            "request='bin:native.hook-alias#v1'",
            "/user/native",
        )
        .await;
    assert_eq!(call(&*actor, "invalid", b"x").await.0, ABI_MESSAGE_INVALID);
    assert_eq!(call(&*actor, "ask", b"x").await.0, ABI_MESSAGE_DENIED);
    assert_eq!(setup.calls.load(Ordering::SeqCst), 0);
    setup.finish(actor).await;
}

#[tokio::test]
async fn malformed_self_overlapping_and_excessive_grants_are_rejected() {
    let setup = Setup::new().await;
    for path in [
        "/user/native",
        "relative",
        "/user/../target",
        "/user//target",
        "/user/target/",
    ] {
        let mut grant = setup.ask();
        grant.destination = path.into();
        assert!(setup
            .spawner
            .clone()
            .with_dylib_messaging(vec![grant])
            .is_err());
    }
    assert!(setup
        .spawner
        .clone()
        .with_dylib_messaging(vec![setup.ask(), setup.ask()])
        .is_err());
    let grants = (0..257)
        .map(|index| {
            let mut grant = setup.ask();
            grant.destination = format!("/user/target{index}");
            grant
        })
        .collect();
    assert!(setup.spawner.clone().with_dylib_messaging(grants).is_err());
    let actor = setup.spawn(&setup.spawner, "", "/user/native").await;
    setup.finish(actor).await;
}

#[tokio::test]
async fn cumulative_request_bytes_are_bounded_and_the_next_invocation_recovers() {
    let setup = Setup::new().await;
    let spawner = setup
        .spawner
        .clone()
        .with_dylib_messaging(vec![setup.grant(NativeMessageOperation::Tell)])
        .unwrap();
    let actor = setup.spawn(&spawner, "", "/user/native").await;
    let (first, codes) = call(&*actor, "byte-budget", &vec![5; 4 * 1024 * 1024]).await;
    assert_eq!(first, ABI_MESSAGE_OK);
    assert_eq!(
        codes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_le_bytes(*bytes))
            .collect::<Vec<_>>(),
        [
            ABI_MESSAGE_OK,
            ABI_MESSAGE_OK,
            ABI_MESSAGE_LIMIT,
            ABI_MESSAGE_LIMIT
        ]
    );
    setup
        .target
        .send(Box::new(Request(b"barrier".to_vec())))
        .await
        .unwrap();
    assert_eq!(setup.calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        call(&*actor, "tell", b"new session").await.0,
        ABI_MESSAGE_OK
    );
    setup.finish(actor).await;
}

struct Lookup(std::sync::Weak<ThreadActorSystem>);
#[async_trait::async_trait]
impl parrot_remote::LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        self.0.upgrade()?.resolve_actor(path)
    }
}

#[tokio::test]
async fn actual_guest_routes_ask_and_tell_over_tcp_and_retires_both_engines() {
    use parrot_remote::{NodeAddr, RemoteActorSystem, RemoteConfig};
    tokio::time::timeout(Duration::from_secs(30), async {
        let setup = Setup::new().await;
        let server = RemoteActorSystem::new(
            RemoteConfig::tcp("native-message-peer", Some("127.0.0.1:0".parse().unwrap())),
            Arc::new(Lookup(Arc::downgrade(&setup.engine))),
        )
        .unwrap();
        server.start().await.unwrap();
        let client_engine = ThreadActorSystem::shared(Default::default());
        let client = RemoteActorSystem::new(
            RemoteConfig::tcp("native-message-client", None),
            Arc::new(Lookup(Arc::downgrade(&client_engine))),
        )
        .unwrap();
        client.start().await.unwrap();
        client
            .connect(&NodeAddr::tcp(
                "native-message-peer",
                server.local_addr().unwrap(),
            ))
            .await
            .unwrap();
        let destination = "parrot://native-message-peer/user/target";
        let mut ask = setup.ask();
        ask.destination = destination.into();
        let mut tell = setup.grant(NativeMessageOperation::Tell);
        tell.destination = destination.into();
        // SAFETY: owned native fixture with its complete trusted dependency set.
        let spawner = unsafe {
            ThreadPropsSpawner::with_remote(client_engine.clone(), client.clone())
                .with_trusted_dylib(&setup.digest)
        }
        .unwrap()
        .with_dylib_messaging(vec![ask, tell])
        .unwrap();
        let actor = setup
            .spawn(
                &spawner,
                &format!("destination='{destination}'"),
                "/user/native",
            )
            .await;
        assert_eq!(
            call(&*actor, "ask", b"tcp ask").await,
            (ABI_MESSAGE_OK, b"business:tcp ask".to_vec())
        );
        assert_eq!(call(&*actor, "tell", b"tcp tell").await.0, ABI_MESSAGE_OK);
        assert_eq!(call(&*actor, "ask", b"tcp barrier").await.0, ABI_MESSAGE_OK);
        assert_eq!(setup.calls.load(Ordering::SeqCst), 3);
        actor.stop().await.unwrap();
        assert!(matches!(
            actor.retirement_status(),
            RetirementStatus::Retired(Ok(()))
        ));
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
        client_engine.shutdown_internal().await.unwrap();
        setup.target.stop().await.unwrap();
        setup.engine.shutdown_internal().await.unwrap();
    })
    .await
    .expect("native TCP message and retirement deadline");
}

#[tokio::test]
async fn fully_qualified_local_source_alias_is_denied_without_self_ask() {
    use parrot_remote::{RemoteActorSystem, RemoteConfig};
    let setup = Setup::new().await;
    let remote = RemoteActorSystem::new(
        RemoteConfig::tcp("native-self", None),
        Arc::new(Lookup(Arc::downgrade(&setup.engine))),
    )
    .unwrap();
    remote.start().await.unwrap();
    let destination = "parrot://native-self/user/native";
    let mut grant = setup.ask();
    grant.destination = destination.into();
    // SAFETY: owned native fixture and trusted dependencies.
    let spawner = unsafe {
        ThreadPropsSpawner::with_remote(setup.engine.clone(), remote.clone())
            .with_trusted_dylib(&setup.digest)
    }
    .unwrap()
    .with_dylib_messaging(vec![grant])
    .unwrap();
    let actor = setup
        .spawn(
            &spawner,
            &format!("destination='{destination}'"),
            "/user/native",
        )
        .await;
    assert_eq!(call(&*actor, "ask", b"self").await.0, ABI_MESSAGE_DENIED);
    actor.stop().await.unwrap();
    remote.shutdown().await.unwrap();
    setup.finish(actor).await;
}

#[tokio::test]
async fn cancelled_waiter_and_stop_keep_native_call_owned_until_target_completes() {
    let setup = Setup::new().await;
    let spawner = setup
        .spawner
        .clone()
        .with_dylib_messaging(vec![setup.ask()])
        .unwrap();
    let actor = setup.spawn(&spawner, "", "/user/native").await;
    let sender = actor.clone_boxed();
    let waiter = tokio::spawn(async move { call(&*sender, "ask", b"blocked").await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while setup.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    {
        let stop = actor.stop();
        tokio::pin!(stop);
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut stop)
            .await
            .is_err());
        assert!(matches!(
            actor.retirement_status(),
            RetirementStatus::Pending
        ));
        assert_eq!(setup.completed.load(Ordering::SeqCst), 0);
        setup.release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), stop)
            .await
            .unwrap()
            .unwrap();
    }
    assert!(matches!(
        actor.retirement_status(),
        RetirementStatus::Retired(Ok(()))
    ));
    assert_eq!(setup.completed.load(Ordering::SeqCst), 1);
    setup.finish(actor).await;
}

#[tokio::test]
async fn explicit_messaging_grants_do_not_authorize_native_execution() {
    let setup = Setup::new().await;
    let spawner = ThreadPropsSpawner::new(setup.engine.clone())
        .with_dylib_messaging(vec![setup.ask()])
        .unwrap();
    let result = spawner
        .spawn_artifact(&setup.artifact, &["/user/native".into()], None)
        .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("trust authorization"));
    assert!(setup.engine.get_actor_ref("/user/native").is_none());
    let actor = setup.spawn(&setup.spawner, "", "/user/native").await;
    setup.finish(actor).await;
}
