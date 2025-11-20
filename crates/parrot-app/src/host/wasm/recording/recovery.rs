use super::*;
use crate::host::wasm::{WasmActor, WasmAsk, WasmReply};
use parrot_api::trace::TraceRecorder;
use parrot_wasm::{runtime::WasmRuntime, HostCtx, WasmConfig};
use std::sync::{
    atomic::{AtomicBool, Ordering::SeqCst},
    Arc, Mutex,
};

async fn actor(asynchronous: bool, fixture: &[u8], mode: &str) -> WasmActor {
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 2_000_000,
        epoch_deadline: 10_000,
        ..Default::default()
    })
    .unwrap();
    let mut host = HostCtx {
        self_path: "/user/recovery".into(),
        ..Default::default()
    };
    host.config_overlay.insert("recording-on-start-mode", mode);
    if asynchronous {
        WasmActor::new_async(
            runtime
                .prepare_bytes(fixture)
                .unwrap()
                .instantiate_async(host, None)
                .await
                .unwrap(),
        )
        .await
        .unwrap()
    } else {
        WasmActor::new(runtime.instantiate_bytes(fixture, host).unwrap()).unwrap()
    }
}

const RECORDING: &[u8] =
    include_bytes!("../../../../../parrot-wasm/tests/fixtures/recording-component.wasm");

async fn ask(actor: &mut WasmActor, key: &str) -> parrot_api::types::ActorResult<Vec<u8>> {
    let reply = actor
        .receive(Box::new(WasmAsk {
            type_key: key.into(),
            payload: vec![],
        }))
        .await?;
    Ok(reply.downcast_ref::<WasmReply>().unwrap().0.clone())
}

async fn context_is_distinct(asynchronous: bool) {
    let mut actor = actor(asynchronous, RECORDING, "").await;
    let recorder = TraceRecorder::new(32).unwrap();
    let failed = recorder.root();
    assert!(failed.scope(ask(&mut actor, "trap")).await.is_err());
    let next = recorder.root();
    let reply = next.scope(ask(&mut actor, "nested")).await.unwrap();
    let identity = ask(&mut actor, "start-identity").await.unwrap();
    actor.drain_async().await.unwrap();
    recorder.close();
    let snapshot = recorder.snapshot();
    let recovery = snapshot
        .records
        .iter()
        .find(|record| {
            record.kind == SpanKind::WasmLifecycle
                && record.target == "wasm/recovery-start:/user/recovery"
        })
        .expect("actual recovery on_start has no independent lifecycle record");
    let startup = snapshot
        .records
        .iter()
        .find(|record| record.trace_id == next.trace_id() && record.target == "guest/on-start")
        .expect("real recovery startup did not receive its recording capability");
    let outer = snapshot
        .records
        .iter()
        .find(|record| record.trace_id == next.trace_id() && record.target == "guest/outer")
        .unwrap();
    let inner = snapshot
        .records
        .iter()
        .find(|record| record.trace_id == next.trace_id() && record.target == "guest/inner")
        .unwrap();
    assert_eq!(recovery.parent_span_id, Some(next.span_id()));
    assert_eq!(recovery.trace_id, next.trace_id());
    assert_ne!(recovery.span_id, next.span_id());
    assert_ne!(recovery.trace_id, failed.trace_id());
    assert_eq!(startup.parent_span_id, Some(recovery.span_id));
    assert_eq!(outer.parent_span_id, Some(next.span_id()));
    assert_eq!(inner.parent_span_id, Some(outer.span_id));
    assert_eq!(
        &identity[..32],
        [
            recovery.trace_id.as_slice(),
            &recovery.span_id,
            &startup.span_id
        ]
        .concat()
    );
    assert_eq!(
        reply,
        [outer.trace_id.as_slice(), &outer.span_id, &inner.span_id].concat()
    );
    for record in [recovery, startup, outer, inner] {
        assert_eq!(record.status, SpanStatus::Ok);
    }
    assert!(snapshot.terminal);
    assert_eq!(snapshot.counts.started, 5);
    assert_eq!(snapshot.counts.errors, 1);
    assert_eq!(snapshot.counts.ok, 4);
    assert_eq!(snapshot.counts.overwritten, 0);
    assert!(TraceContext::current().is_none());
}

#[tokio::test]
async fn sync_recovery_has_independent_identity_and_exact_guest_parent() {
    context_is_distinct(false).await;
}

#[tokio::test]
async fn async_recovery_has_independent_identity_and_exact_guest_parent() {
    context_is_distinct(true).await;
}

#[derive(Clone, Debug, Default)]
struct MessagingProbe {
    gated: Arc<AtomicBool>,
    contexts: Arc<Mutex<Vec<ContextObservation>>>,
    panic_drop: Arc<AtomicBool>,
}
type ContextObservation = (&'static str, Option<TraceContext>);
impl MessagingProbe {
    fn observe(&self, phase: &'static str) {
        self.contexts
            .lock()
            .unwrap()
            .push((phase, TraceContext::current()));
    }
}
struct ImportDrop(MessagingProbe);
impl Drop for ImportDrop {
    fn drop(&mut self) {
        self.0.observe("drop");
        assert!(!self.0.panic_drop.load(SeqCst), "import disposal failed");
    }
}
struct MessagingWait(ImportDrop);
impl std::future::Future for MessagingWait {
    type Output = Result<(), parrot_wasm::runtime::MessagingError>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let probe = &self.0 .0;
        probe.observe("poll");
        if probe.gated.load(SeqCst) {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(Ok(()))
        }
    }
}
impl parrot_wasm::runtime::GuestMessaging for MessagingProbe {
    fn ask(
        &self,
        _: parrot_wasm::runtime::MessagingRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        parrot_wasm::runtime::Msg,
                        parrot_wasm::runtime::MessagingError,
                    >,
                > + Send
                + '_,
        >,
    > {
        unreachable!("startup uses TELL")
    }
    fn tell(
        &self,
        _: parrot_wasm::runtime::MessagingRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), parrot_wasm::runtime::MessagingError>>
                + Send
                + '_,
        >,
    > {
        self.observe("construct");
        Box::pin(MessagingWait(ImportDrop(self.clone())))
    }
}

async fn suspended_recovery(cancel: bool) {
    let probe = Arc::new(MessagingProbe::default());
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 2_000_000,
        epoch_deadline: 10_000,
        ..Default::default()
    })
    .unwrap();
    let mut host = HostCtx {
        self_path: "/user/recovery".into(),
        ..Default::default()
    };
    host.config_overlay.insert("start-value", "1");
    let component = runtime
        .prepare_bytes(include_bytes!(
            "../../../../../parrot-wasm/tests/fixtures/messaging-component.wasm"
        ))
        .unwrap()
        .instantiate_async(host, Some(probe.clone()))
        .await
        .unwrap();
    let mut actor = WasmActor::new_async(component).await.unwrap();
    let recorder = TraceRecorder::new(32).unwrap();
    assert!(recorder
        .root()
        .scope(ask(&mut actor, "trap"))
        .await
        .is_err());
    probe.contexts.lock().unwrap().clear();
    probe.gated.store(true, SeqCst);
    let next = recorder.root();
    let ambient = TraceContext::root();
    let mut invocation = Box::pin(next.scope(ask(&mut actor, "echo")));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    use std::future::Future;
    for _ in 0..2 {
        assert!(ambient
            .in_scope(|| invocation.as_mut().poll(&mut cx))
            .is_pending());
        assert!(TraceContext::current().is_none());
    }
    let pending = recorder.snapshot();
    assert_eq!(
        pending.counts.in_flight, 2,
        "recovery root and unfinished startup must remain owned"
    );
    assert_eq!(pending.counts.completed, 0);
    probe.gated.store(false, SeqCst);
    if cancel {
        ambient.in_scope(|| drop(invocation));
    } else {
        assert!(ambient
            .in_scope(|| invocation.as_mut().poll(&mut cx))
            .is_ready());
        drop(invocation);
    }
    assert!(TraceContext::current().is_none());
    recorder.close();
    let snapshot = recorder.snapshot();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.counts.started, 2);
    assert_eq!(snapshot.counts.completed, 2);
    assert_eq!(snapshot.counts.overwritten, 0);
    let recovery = snapshot
        .records
        .iter()
        .find(|record| record.kind == SpanKind::WasmLifecycle)
        .unwrap();
    let startup = snapshot
        .records
        .iter()
        .find(|record| record.target == "guest/start-await")
        .unwrap();
    assert_eq!(recovery.trace_id, next.trace_id());
    assert_eq!(recovery.parent_span_id, Some(next.span_id()));
    assert_eq!(
        recovery.status,
        if cancel {
            SpanStatus::Cancelled
        } else {
            SpanStatus::Ok
        }
    );
    assert_eq!(startup.parent_span_id, Some(recovery.span_id));
    assert_eq!(startup.status, SpanStatus::Cancelled);
    let observations = probe.contexts.lock().unwrap().clone();
    assert_eq!(
        observations
            .iter()
            .filter(|(phase, _)| *phase == "construct")
            .count(),
        1
    );
    assert_eq!(
        observations
            .iter()
            .filter(|(phase, _)| *phase == "drop")
            .count(),
        1
    );
    assert!(
        observations
            .iter()
            .filter(|(phase, _)| *phase == "poll")
            .count()
            >= 2
    );
    for (phase, context) in observations.iter() {
        let context = context
            .as_ref()
            .unwrap_or_else(|| panic!("missing recovery context at {phase}"));
        assert_eq!(context.trace_id(), recovery.trace_id, "{phase}");
        assert_eq!(context.span_id(), recovery.span_id, "{phase}");
        assert_eq!(context.parent_span_id(), Some(next.span_id()), "{phase}");
    }
    // A cancelled recovery poisons the Store; a subsequent plain call must
    // rebuild without leaking either the prior recorder or its identity.
    assert!(ask(&mut actor, "parent").await.unwrap().is_empty());
    actor.drain_async().await.unwrap();
}

#[tokio::test]
async fn recovery_scopes_import_construction_polls_and_successful_disposal() {
    suspended_recovery(false).await;
}

#[tokio::test]
async fn recovery_scopes_import_cancellation_and_retires_actual_records() {
    suspended_recovery(true).await;
}

#[tokio::test]
async fn lifecycle_cancellation_records_physical_cleanup_panic_and_keeps_the_receipt() {
    use std::{
        future::Future,
        panic::{catch_unwind, AssertUnwindSafe},
    };
    for phase in ["start", "drain", "recovery-drain"] {
        let probe = Arc::new(MessagingProbe::default());
        let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
            fuel_per_message: 2_000_000,
            epoch_deadline: 10_000,
            ..Default::default()
        })
        .unwrap();
        let mut host = HostCtx {
            self_path: "/user/cleanup".into(),
            ..Default::default()
        };
        host.config_overlay.insert(
            if phase == "drain" {
                "drain-value"
            } else {
                "start-value"
            },
            "1",
        );
        let mut component = runtime
            .prepare_bytes(include_bytes!(
                "../../../../../parrot-wasm/tests/fixtures/messaging-component.wasm"
            ))
            .unwrap()
            .instantiate_async(host, Some(probe.clone()))
            .await
            .unwrap();
        if phase != "start" {
            component.on_start_async(None).await.unwrap();
        }
        if phase == "recovery-drain" {
            assert!(component.handle_async("trap", b"", None).await.is_err());
        }
        probe.contexts.lock().unwrap().clear();
        probe.gated.store(true, SeqCst);
        probe.panic_drop.store(true, SeqCst);
        let recorder = TraceRecorder::new(16).unwrap();
        let root = recorder.root();
        let mut invocation = Box::pin(root.scope(async {
            if phase == "start" {
                on_start_async(&mut component).await
            } else {
                on_drain_async(&mut component).await
            }
        }));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(invocation.as_mut().poll(&mut cx).is_pending());
        assert!(catch_unwind(AssertUnwindSafe(|| drop(invocation))).is_ok());
        assert!(TraceContext::current().is_none());
        assert!(
            matches!(component.cleanup_error(), Some(ComponentError::Panic(ref message)) if message == "import disposal failed")
        );
        assert!(matches!(
            on_drain_async(&mut component).await,
            Err(ComponentError::Panic(_))
        ));
        assert_eq!(
            probe
                .contexts
                .lock()
                .unwrap()
                .iter()
                .filter(|(stage, _)| *stage == "construct")
                .count(),
            1
        );
        recorder.close();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(
            snapshot.counts.started,
            if phase == "recovery-drain" { 3 } else { 2 }
        );
        assert_eq!(snapshot.counts.started, snapshot.counts.completed);
        assert_eq!(snapshot.counts.overwritten, 0);
        assert!(
            snapshot
                .records
                .iter()
                .all(|record| record.status == SpanStatus::Panicked),
            "{snapshot:?}"
        );
    }
}

#[tokio::test]
async fn interrupted_actor_drain_caches_physical_cleanup_error_without_retrying_guest() {
    use std::{
        future::Future,
        panic::{catch_unwind, AssertUnwindSafe},
    };
    let probe = Arc::new(MessagingProbe::default());
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 2_000_000,
        epoch_deadline: 10_000,
        ..Default::default()
    })
    .unwrap();
    let mut host = HostCtx {
        self_path: "/user/drain-cleanup".into(),
        ..Default::default()
    };
    host.config_overlay.insert("drain-value", "1");
    let component = runtime
        .prepare_bytes(include_bytes!(
            "../../../../../parrot-wasm/tests/fixtures/messaging-component.wasm"
        ))
        .unwrap()
        .instantiate_async(host, Some(probe.clone()))
        .await
        .unwrap();
    let mut actor = WasmActor::new_async(component).await.unwrap();
    probe.gated.store(true, SeqCst);
    probe.panic_drop.store(true, SeqCst);
    let recorder = TraceRecorder::new(8).unwrap();
    let root = recorder.root();
    let mut call = Box::pin(root.scope(actor.drain_async()));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(call.as_mut().poll(&mut cx).is_pending());
    assert!(catch_unwind(AssertUnwindSafe(|| drop(call))).is_ok());
    for _ in 0..2 {
        assert_eq!(
            actor.drain_async().await,
            Err(ComponentError::Panic("import disposal failed".into()))
        );
    }
    assert_eq!(
        probe
            .contexts
            .lock()
            .unwrap()
            .iter()
            .filter(|(stage, _)| *stage == "construct")
            .count(),
        1
    );
    recorder.close();
    let snapshot = recorder.snapshot();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.counts.completed, 2);
    assert!(snapshot
        .records
        .iter()
        .all(|record| record.status == SpanStatus::Panicked));
}

async fn optional_identity(asynchronous: bool) {
    for mode in ["identity-only", "closed", "none"] {
        let mut actor = actor(
            asynchronous,
            include_bytes!("../../../../../parrot-wasm/tests/fixtures/trace-component.wasm"),
            "",
        )
        .await;
        assert!(ask(&mut actor, "trap").await.is_err());
        let recorder = TraceRecorder::new(16).unwrap();
        let context = if mode == "identity-only" {
            TraceContext::root()
        } else {
            recorder.root()
        };
        if mode == "closed" {
            recorder.close();
        }
        if mode == "none" {
            context
                .scope(parrot_api::trace::scope(None, ask(&mut actor, "parent")))
                .await
                .unwrap();
        } else {
            let parent = context.scope(ask(&mut actor, "parent")).await.unwrap();
            assert_eq!(
                parent,
                [context.trace_id().as_slice(), &context.span_id()].concat()
            );
        }
        let startup = ask(&mut actor, "start-parent").await.unwrap();
        if mode == "none" {
            assert!(startup.is_empty());
        } else {
            assert_eq!(startup.len(), 24);
            assert_eq!(&startup[..16], context.trace_id());
            assert_ne!(&startup[16..], context.span_id());
        }
        assert!(ask(&mut actor, "parent").await.unwrap().is_empty());
        actor.drain_async().await.unwrap();
        recorder.close();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 0);
        assert_eq!(
            snapshot.counts.rejected_after_close,
            u64::from(mode == "closed")
        );
        assert!(TraceContext::current().is_none());
    }
}

#[tokio::test]
async fn sync_recovery_preserves_identity_only_closed_and_explicit_none() {
    optional_identity(false).await;
}

#[tokio::test]
async fn async_recovery_preserves_identity_only_closed_and_explicit_none() {
    optional_identity(true).await;
}

async fn full_recording_budget(asynchronous: bool) {
    let mut actor = actor(asynchronous, RECORDING, "budget").await;
    let recorder = TraceRecorder::new(1024).unwrap();
    assert!(recorder
        .root()
        .scope(ask(&mut actor, "trap"))
        .await
        .is_err());
    let context = recorder.root();
    context.scope(ask(&mut actor, "budget")).await.unwrap();
    recorder.close();
    let snapshot = recorder.snapshot();
    let recovery = snapshot
        .records
        .iter()
        .find(|record| record.kind == SpanKind::WasmLifecycle)
        .unwrap();
    assert_eq!(recovery.parent_span_id, Some(context.span_id()));
    assert_eq!(recovery.status, SpanStatus::Ok);
    let startup: Vec<_> = snapshot
        .records
        .iter()
        .filter(|record| record.parent_span_id == Some(recovery.span_id))
        .collect();
    let business: Vec<_> = snapshot
        .records
        .iter()
        .filter(|record| {
            record.parent_span_id == Some(context.span_id()) && record.kind == SpanKind::WasmGuest
        })
        .collect();
    assert_eq!(startup.len(), 256);
    assert_eq!(business.len(), 256);
    for record in startup.iter().chain(&business) {
        assert_eq!(record.trace_id, context.trace_id());
        assert_eq!(record.status, SpanStatus::Ok);
    }
    assert_eq!(snapshot.counts.started, 514);
    assert_eq!(snapshot.counts.completed, 514);
    assert_eq!(snapshot.counts.ok, 513);
    assert_eq!(snapshot.counts.errors, 1);
    assert_eq!(snapshot.counts.overwritten, 0);
    assert!(snapshot.terminal);
    actor.drain_async().await.unwrap();
}

#[tokio::test]
async fn sync_recovery_and_business_each_record_their_full_independent_budget() {
    full_recording_budget(false).await;
}

#[tokio::test]
async fn async_recovery_and_business_each_record_their_full_independent_budget() {
    full_recording_budget(true).await;
}

async fn failed_start_before_message_and_drain(asynchronous: bool) {
    for mode in ["trap", "fuel"] {
        let mut actor = actor(asynchronous, RECORDING, mode).await;
        let recorder = TraceRecorder::new(32).unwrap();
        assert!(recorder
            .root()
            .scope(ask(&mut actor, "trap"))
            .await
            .is_err());
        let message = recorder.root();
        assert!(message.scope(ask(&mut actor, "nested")).await.is_err());
        let draining = recorder.root();
        let failure = draining.scope(actor.drain_async()).await.unwrap_err();
        if mode == "fuel" {
            assert_eq!(failure, ComponentError::OutOfFuel);
        } else {
            assert!(matches!(failure, ComponentError::Trap(_)));
        }
        assert_eq!(actor.drain_async().await.unwrap_err(), failure);
        recorder.close();
        let snapshot = recorder.snapshot();
        let status = if mode == "fuel" {
            SpanStatus::Cancelled
        } else {
            SpanStatus::Error
        };
        let drain = snapshot
            .records
            .iter()
            .find(|record| record.target == "wasm/on-drain:/user/recovery")
            .unwrap();
        assert_eq!(drain.parent_span_id, Some(draining.span_id()));
        assert_eq!(drain.status, status);
        for (trace, parent) in [
            (message.trace_id(), message.span_id()),
            (drain.trace_id, drain.span_id),
        ] {
            let recovery = snapshot
                .records
                .iter()
                .find(|record| {
                    record.trace_id == trace
                        && record.target == "wasm/recovery-start:/user/recovery"
                })
                .unwrap();
            assert_eq!(recovery.parent_span_id, Some(parent));
            assert_eq!(recovery.status, status);
            let startup = snapshot
                .records
                .iter()
                .find(|record| record.parent_span_id == Some(recovery.span_id))
                .unwrap();
            assert_eq!(startup.target, "guest/on-start");
            assert_eq!(startup.status, status);
            assert!(!snapshot
                .records
                .iter()
                .any(|record| record.trace_id == trace
                    && matches!(
                        record.target.as_str(),
                        "guest/outer" | "guest/inner" | "guest/on-drain"
                    )));
        }
        assert_eq!(snapshot.counts.started, 6);
        assert_eq!(snapshot.counts.completed, 6);
        assert_eq!(snapshot.counts.overwritten, 0);
        assert!(snapshot.terminal);
    }
}

#[tokio::test]
async fn sync_failed_recovery_never_records_unexecuted_business_or_drain() {
    failed_start_before_message_and_drain(false).await;
}

#[tokio::test]
async fn async_failed_recovery_never_records_unexecuted_business_or_drain() {
    failed_start_before_message_and_drain(true).await;
}
