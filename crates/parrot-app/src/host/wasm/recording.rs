use parrot_api::trace::{SpanGuard, SpanKind, SpanStatus, TraceContext};
use parrot_wasm::{
    runtime::{GuestScope, GuestSpan, GuestSpanStatus, GuestTrace, WasmComponent},
    ComponentError, TraceParent,
};

pub(super) fn on_start(component: &mut WasmComponent) -> Result<(), ComponentError> {
    lifecycle(
        component,
        "on-start",
        |component, recording| match recording {
            Some(recording) => component.on_start_with_recording(recording),
            None => component.on_start(),
        },
    )
}

pub(super) fn on_drain(component: &mut WasmComponent) -> Result<(), ComponentError> {
    lifecycle(
        component,
        "on-drain",
        |component, recording| match recording {
            Some(recording) => component.on_drain_with_recording(recording),
            None => component.on_drain(),
        },
    )
}

pub(super) async fn on_start_async(component: &mut WasmComponent) -> Result<(), ComponentError> {
    lifecycle_async(component, true).await
}

pub(super) async fn on_drain_async(component: &mut WasmComponent) -> Result<(), ComponentError> {
    lifecycle_async(component, false).await
}

struct LifecycleInvocation<'a> {
    component: &'a mut WasmComponent,
    recording: Option<SpanGuard>,
}
impl Drop for LifecycleInvocation<'_> {
    fn drop(&mut self) {
        if let Some(recording) = self.recording.take() {
            recording.finish(match self.component.cleanup_error() {
                Some(ComponentError::Panic(_)) => SpanStatus::Panicked,
                Some(_) => SpanStatus::Error,
                None => SpanStatus::Cancelled,
            });
        }
    }
}

async fn lifecycle_async(component: &mut WasmComponent, start: bool) -> Result<(), ComponentError> {
    let context = TraceContext::current().map(|context| context.child());
    parrot_api::trace::scope(context.clone(), async {
        let phase = if start { "on-start" } else { "on-drain" };
        let guard = context.as_ref().map(|_| {
            SpanGuard::current(
                SpanKind::WasmLifecycle,
                &format!("wasm/{phase}:{}", component.host_ctx().self_path),
            )
        });
        let recording = context.map(Recording::root);
        let mut owned = LifecycleInvocation {
            component,
            recording: guard,
        };
        let result = if start {
            owned.component.on_start_async(recording).await
        } else {
            owned.component.on_drain_async(recording).await
        };
        if let Some(guard) = owned.recording.take() {
            guard.finish(match &result {
                Ok(()) => SpanStatus::Ok,
                Err(ComponentError::Panic(_)) => SpanStatus::Panicked,
                Err(ComponentError::OutOfFuel | ComponentError::EpochDeadline) => {
                    SpanStatus::Cancelled
                }
                Err(_) => SpanStatus::Error,
            });
        }
        result
    })
    .await
}

fn lifecycle(
    component: &mut WasmComponent,
    phase: &str,
    call: impl FnOnce(&mut WasmComponent, Option<Box<dyn GuestTrace>>) -> Result<(), ComponentError>,
) -> Result<(), ComponentError> {
    let Some(context) = TraceContext::current().map(|context| context.child()) else {
        return call(component, None);
    };
    context.in_scope(|| {
        let guard = SpanGuard::current(
            SpanKind::WasmLifecycle,
            &format!("wasm/{phase}:{}", component.host_ctx().self_path),
        );
        let result = call(component, Some(Recording::root(context.clone())));
        guard.finish(match &result {
            Ok(()) => SpanStatus::Ok,
            Err(ComponentError::Panic(_)) => SpanStatus::Panicked,
            Err(ComponentError::OutOfFuel | ComponentError::EpochDeadline) => SpanStatus::Cancelled,
            Err(_) => SpanStatus::Error,
        });
        result
    })
}

pub(super) struct Recording {
    context: TraceContext,
    guard: Option<SpanGuard>,
}
impl std::fmt::Debug for Recording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recording")
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}
impl Recording {
    pub(super) fn root(context: TraceContext) -> Box<dyn GuestTrace> {
        Box::new(Self {
            context,
            guard: None,
        })
    }
}

#[derive(Debug)]
struct ExecutionScope(TraceContext);
impl GuestScope for ExecutionScope {
    fn in_scope(&self, operation: &mut dyn FnMut()) {
        self.0.in_scope(operation);
    }
}

impl GuestTrace for Recording {
    fn parent(&self) -> TraceParent {
        TraceParent {
            trace_id: self.context.trace_id(),
            span_id: self.context.span_id(),
        }
    }

    fn start(&self, target: &str) -> Option<Box<dyn GuestSpan>> {
        let context = self.context.child();
        let guard = context.in_scope(|| SpanGuard::current(SpanKind::WasmGuest, target));
        guard.is_recording().then(|| {
            Box::new(Self {
                context,
                guard: Some(guard),
            }) as Box<dyn GuestSpan>
        })
    }

    fn start_recovery(&self, target: &str) -> Option<Box<dyn GuestSpan>> {
        let context = self.context.child();
        let guard = context.in_scope(|| SpanGuard::current(SpanKind::WasmLifecycle, target));
        Some(Box::new(Self {
            context,
            guard: Some(guard),
        }))
    }

    fn execution_scope(&self) -> Option<std::sync::Arc<dyn GuestScope>> {
        Some(std::sync::Arc::new(ExecutionScope(self.context.clone())))
    }
}
impl GuestSpan for Recording {
    fn finish(mut self: Box<Self>, status: GuestSpanStatus) {
        if let Some(guard) = self.guard.take() {
            guard.finish(match status {
                GuestSpanStatus::Ok => SpanStatus::Ok,
                GuestSpanStatus::Error => SpanStatus::Error,
                GuestSpanStatus::Cancelled => SpanStatus::Cancelled,
                GuestSpanStatus::Panicked => SpanStatus::Panicked,
            });
        }
    }
}

#[cfg(test)]
mod recovery;

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_api::trace::TraceRecorder;
    use parrot_wasm::{
        runtime::{WasmComponent, WasmRuntime},
        ComponentError, HostCtx, WasmConfig,
    };

    #[test]
    fn host_panic_preserves_actor_panic_classification() {
        let error = super::super::guest_error(ComponentError::Panic("host failed".into()));
        assert!(matches!(error, parrot_api::errors::ActorError::Panic(_)));
    }

    fn guest() -> (WasmRuntime, WasmComponent) {
        let (runtime, mut guest) = lifecycle_guest("", "");
        guest.on_start().unwrap();
        (runtime, guest)
    }

    fn lifecycle_guest(phase: &str, mode: &str) -> (WasmRuntime, WasmComponent) {
        let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
            fuel_per_message: 2_000_000,
            epoch_deadline: 1000,
            ..Default::default()
        })
        .unwrap();
        let mut host = HostCtx {
            self_path: "/user/lifecycle".into(),
            ..Default::default()
        };
        host.config_overlay
            .insert(format!("recording-{phase}-mode"), mode);
        let guest = runtime
            .instantiate_bytes(
                include_bytes!("../../../../parrot-wasm/tests/fixtures/recording-component.wasm"),
                host,
            )
            .unwrap();
        (runtime, guest)
    }

    #[test]
    fn actual_actor_lifecycle_has_distinct_identities_and_drain_records_once() {
        let (_runtime, guest) = lifecycle_guest("", "");
        let recorder = TraceRecorder::new(16).unwrap();
        let root = recorder.root();
        let mut actor = root
            .in_scope(|| super::super::WasmActor::new(guest))
            .unwrap();
        root.in_scope(|| actor.drain()).unwrap();
        root.in_scope(|| actor.drain()).unwrap();
        recorder.close();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 4);
        assert_eq!(snapshot.counts.ok, 4);
        for (phase, [guest, lifecycle]) in ["on-start", "on-drain"]
            .into_iter()
            .zip(snapshot.records.as_chunks::<2>().0)
        {
            assert_eq!(lifecycle.kind, SpanKind::WasmLifecycle);
            assert_eq!(lifecycle.target, format!("wasm/{phase}:/user/lifecycle"));
            assert_eq!(lifecycle.parent_span_id, Some(root.span_id()));
            assert_ne!(lifecycle.span_id, root.span_id());
            assert_eq!(guest.kind, SpanKind::WasmGuest);
            assert_eq!(guest.parent_span_id, Some(lifecycle.span_id));
            let identity = actor
                .component
                .handle(
                    if phase == "on-start" {
                        "start-identity"
                    } else {
                        "drain-identity"
                    },
                    b"",
                )
                .unwrap();
            assert_eq!(
                &identity[..32],
                [
                    lifecycle.trace_id.as_slice(),
                    &lifecycle.span_id,
                    &guest.span_id
                ]
                .concat()
            );
            assert_eq!(lifecycle.trace_id, root.trace_id());
            assert_eq!(guest.trace_id, root.trace_id());
        }
        actor.component.handle("disabled", b"").unwrap();
        assert!(TraceContext::current().is_none());
    }

    #[test]
    fn lifecycle_recording_budget_is_independent_and_handles_do_not_cross_invocations() {
        let (_runtime, mut guest) = lifecycle_guest("on-start", "budget");
        let recorder = TraceRecorder::new(1024).unwrap();
        let root = recorder.root();
        root.in_scope(|| on_start(&mut guest)).unwrap();
        assert_eq!(guest.take_metrics().messages, 0);
        let identity = guest.handle("start-identity", b"").unwrap();
        let handle = u64::from_le_bytes(identity[32..].try_into().unwrap());
        let reply = guest
            .handle_with_recording(
                "stale",
                &handle.to_le_bytes(),
                Recording::root(root.clone()),
            )
            .unwrap();
        assert!(u64::from_le_bytes(reply.try_into().unwrap()) > handle);
        root.in_scope(|| on_drain(&mut guest)).unwrap();
        guest
            .handle_with_recording("budget", b"", Recording::root(root))
            .unwrap();
        recorder.close();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 516);
        assert_eq!(snapshot.counts.started, snapshot.counts.ok);
        assert_eq!(snapshot.counts.overwritten, 0);
    }

    #[test]
    fn failed_actor_drain_caches_error_without_reexecution_or_a_second_trace() {
        let (_runtime, guest) = lifecycle_guest("on-drain", "trap");
        let mut actor = super::super::WasmActor::new(guest).unwrap();
        let first = TraceRecorder::new(8).unwrap();
        let error = first.root().in_scope(|| actor.drain()).unwrap_err();
        assert!(matches!(error, ComponentError::Trap(_)));
        let second = TraceRecorder::new(8).unwrap();
        assert_eq!(second.root().in_scope(|| actor.drain()), Err(error));
        first.close();
        second.close();
        let snapshot = first.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 2);
        assert_eq!(snapshot.counts.errors, 2);
        assert!(second.snapshot().terminal);
        assert_eq!(second.snapshot().counts.started, 0);
    }

    #[test]
    fn lifecycle_unfinished_trap_and_fuel_record_actual_outcomes_then_clear_capabilities() {
        for phase in ["on-start", "on-drain"] {
            for (mode, status) in [
                ("unfinished", SpanStatus::Cancelled),
                ("trap", SpanStatus::Error),
                ("fuel", SpanStatus::Cancelled),
            ] {
                let (_runtime, mut guest) = lifecycle_guest(phase, mode);
                if phase == "on-drain" {
                    guest.on_start().unwrap();
                }
                let recorder = TraceRecorder::new(8).unwrap();
                let result = recorder.root().in_scope(|| {
                    if phase == "on-start" {
                        on_start(&mut guest)
                    } else {
                        on_drain(&mut guest)
                    }
                });
                match mode {
                    "unfinished" => assert!(result.is_ok()),
                    "fuel" => assert_eq!(result, Err(ComponentError::OutOfFuel)),
                    _ => assert!(matches!(result, Err(ComponentError::Trap(_)))),
                }
                let snapshot = recorder.snapshot();
                assert_eq!(snapshot.counts.started, 2);
                assert_eq!(snapshot.counts.in_flight, 0);
                assert_eq!(snapshot.records[0].status, status);
                assert_eq!(
                    snapshot.records[1].status,
                    if mode == "unfinished" {
                        SpanStatus::Ok
                    } else {
                        status
                    }
                );
                guest.handle("disabled", b"").unwrap();
                assert!(
                    guest.handle("start-identity", b"").unwrap().is_empty() || mode == "unfinished"
                );
                recorder.close();
                assert!(recorder.snapshot().terminal);
                assert_eq!(recorder.snapshot().counts.started, 2);
            }
        }
    }

    #[test]
    fn real_guest_records_nested_fresh_children_with_explicit_identity() {
        let (_runtime, mut guest) = guest();
        let recorder = TraceRecorder::new(8).unwrap();
        let root = recorder.root();
        let bytes = guest
            .handle_with_recording("nested", b"", Recording::root(root.clone()))
            .unwrap();
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.started, 2);
        assert_eq!(snapshot.counts.in_flight, 0);
        let inner = &snapshot.records[0];
        let outer = &snapshot.records[1];
        assert_eq!(outer.parent_span_id, Some(root.span_id()));
        assert_eq!(inner.parent_span_id, Some(outer.span_id));
        assert_ne!(outer.span_id, root.span_id());
        assert_ne!(inner.span_id, outer.span_id);
        assert_eq!(
            bytes,
            [root.trace_id().as_slice(), &outer.span_id, &inner.span_id].concat()
        );
        for record in &snapshot.records {
            assert_eq!(record.trace_id, root.trace_id());
            assert_eq!(record.kind, SpanKind::WasmGuest);
            assert_eq!(record.status, SpanStatus::Ok);
        }
    }

    #[test]
    fn absent_identity_only_closed_and_lifecycle_recording_are_disabled() {
        let (_runtime, mut guest) = guest();
        let recorder = TraceRecorder::new(8).unwrap();
        let root = recorder.root();
        root.in_scope(|| guest.handle("disabled", b"")).unwrap();
        let parent = TraceParent {
            trace_id: root.trace_id(),
            span_id: root.span_id(),
        };
        guest
            .handle_with_trace("disabled", b"", Some(&parent))
            .unwrap();
        guest
            .handle_with_recording("disabled", b"", Recording::root(TraceContext::root()))
            .unwrap();
        root.in_scope(|| guest.on_start()).unwrap();
        root.in_scope(|| guest.on_drain()).unwrap();
        assert_eq!(recorder.snapshot().counts.started, 0);
        recorder.close();
        guest
            .handle_with_recording("disabled", b"", Recording::root(root))
            .unwrap();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 0);
        assert_eq!(snapshot.counts.rejected_after_close, 1);
    }

    #[test]
    fn completed_spans_consume_message_budget_and_next_message_recovers() {
        let (_runtime, mut guest) = guest();
        let recorder = TraceRecorder::new(512).unwrap();
        guest
            .handle_with_recording("budget", b"", Recording::root(recorder.root()))
            .unwrap();
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.started, 256);
        assert_eq!(snapshot.counts.completed, 256);
        assert_eq!(snapshot.counts.in_flight, 0);
        assert_eq!(snapshot.counts.overwritten, 0);
        guest
            .handle_with_recording("nested", b"", Recording::root(recorder.root()))
            .unwrap();
        assert_eq!(recorder.snapshot().counts.completed, 258);
    }

    #[test]
    fn active_budget_is_bounded_and_unfinished_spans_retire_at_return() {
        let (_runtime, mut guest) = guest();
        let recorder = TraceRecorder::new(512).unwrap();
        guest
            .handle_with_recording("active-budget", b"", Recording::root(recorder.root()))
            .unwrap();
        recorder.close();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 256);
        assert_eq!(snapshot.counts.cancelled, 256);
        assert_eq!(snapshot.counts.overwritten, 0);
    }

    #[test]
    fn explicit_statuses_and_utf8_byte_boundaries_survive_the_wit_bridge() {
        let (_runtime, mut guest) = guest();
        let recorder = TraceRecorder::new(8).unwrap();
        guest
            .handle_with_recording("statuses", b"", Recording::root(recorder.root()))
            .unwrap();
        let snapshot = recorder.snapshot();
        let statuses: Vec<_> = snapshot.records.iter().map(|r| r.status).collect();
        assert_eq!(
            statuses,
            [
                SpanStatus::Ok,
                SpanStatus::Error,
                SpanStatus::Cancelled,
                SpanStatus::Panicked,
                SpanStatus::Ok
            ]
        );
        assert_eq!(snapshot.records[0].target.len(), 256);
        assert_eq!(snapshot.records[4].target.len(), 255);
        assert!(!snapshot.records.iter().any(|r| r.target_truncated));
        assert_eq!(snapshot.counts.in_flight, 0);
    }

    #[test]
    fn unfinished_return_error_trap_and_fuel_retire_once_before_recovery() {
        let (_runtime, mut guest) = guest();
        let recorder = TraceRecorder::new(32).unwrap();
        let mut previous = 0u64;
        let mut recovery_parents = Vec::new();
        for (message, status) in [
            ("unfinished", SpanStatus::Cancelled),
            ("error", SpanStatus::Error),
            ("trap", SpanStatus::Error),
            ("fuel", SpanStatus::Cancelled),
        ] {
            let result =
                guest.handle_with_recording(message, b"", Recording::root(recorder.root()));
            match message {
                "unfinished" => {
                    previous = u64::from_le_bytes(result.unwrap().try_into().unwrap());
                }
                "fuel" => assert_eq!(result.unwrap_err(), ComponentError::OutOfFuel),
                _ => assert!(matches!(result, Err(ComponentError::Trap(_)))),
            }
            let snapshot = recorder.snapshot();
            assert_eq!(snapshot.counts.in_flight, 0);
            assert_eq!(snapshot.records.last().unwrap().status, status);
            // A guest-supplied old handle cannot finish or parent new work,
            // including the message which triggers poisoned-store recovery.
            let next_root = recorder.root();
            if matches!(message, "trap" | "fuel") {
                recovery_parents.push(next_root.clone());
            }
            let bytes = guest
                .handle_with_recording("stale", &previous.to_le_bytes(), Recording::root(next_root))
                .unwrap();
            let next = u64::from_le_bytes(bytes.try_into().unwrap());
            assert!(next > previous);
            previous = next;
        }
        guest
            .tell_with_recording("nested", b"", Recording::root(recorder.root()))
            .unwrap();
        guest
            .tell_with_recording("unfinished", b"", Recording::root(recorder.root()))
            .unwrap();
        recorder.close();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, snapshot.counts.completed);
        assert_eq!(snapshot.counts.completed, 15);
        for parent in recovery_parents {
            let recovery = snapshot
                .records
                .iter()
                .find(|record| {
                    record.trace_id == parent.trace_id() && record.kind == SpanKind::WasmLifecycle
                })
                .unwrap();
            assert_eq!(recovery.parent_span_id, Some(parent.span_id()));
            assert_eq!(recovery.status, SpanStatus::Ok);
            let startup = snapshot
                .records
                .iter()
                .find(|record| record.parent_span_id == Some(recovery.span_id))
                .unwrap();
            assert_eq!(startup.target, "guest/on-start");
            assert_eq!(startup.status, SpanStatus::Ok);
        }
        assert_eq!(
            snapshot
                .records
                .iter()
                .filter(|record| record.kind == SpanKind::WasmLifecycle)
                .count(),
            2
        );
        assert_eq!(
            snapshot.records.last().unwrap().status,
            SpanStatus::Cancelled
        );
    }

    struct GuestLog(Box<dyn Fn() + Send + Sync>);
    impl tracing::Subscriber for GuestLog {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.target() == "parrot_wasm"
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().target() == "parrot_wasm" {
                (self.0)();
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn synchronous_recovery_host_callbacks_use_the_owned_lifecycle_context() {
        let (_runtime, mut guest) = lifecycle_guest("on-start", "host-panic");
        guest.on_start().unwrap();
        let recorder = TraceRecorder::new(16).unwrap();
        assert!(guest
            .handle_with_recording("trap", b"", Recording::root(recorder.root()))
            .is_err());
        let next = recorder.root();
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = observed.clone();
        let ambient = TraceContext::root();
        ambient
            .in_scope(|| {
                tracing::subscriber::with_default(
                    GuestLog(Box::new(move || {
                        capture.lock().unwrap().push(TraceContext::current())
                    })),
                    || guest.handle_with_recording("nested", b"", Recording::root(next.clone())),
                )
            })
            .unwrap();
        assert!(TraceContext::current().is_none());
        let snapshot = recorder.snapshot();
        let recovery = snapshot
            .records
            .iter()
            .find(|record| record.kind == SpanKind::WasmLifecycle)
            .unwrap();
        let contexts = observed.lock().unwrap();
        assert_eq!(contexts.len(), 1);
        let context = contexts[0].as_ref().unwrap();
        assert_eq!(context.trace_id(), next.trace_id());
        assert_eq!(context.span_id(), recovery.span_id);
        assert_eq!(context.parent_span_id(), Some(next.span_id()));
        assert_eq!(recovery.status, SpanStatus::Ok);
        assert_eq!(snapshot.counts.in_flight, 0);
        assert_eq!(snapshot.counts.completed, 5);
    }

    #[test]
    fn caught_host_panic_and_epoch_interrupt_retire_with_the_actual_outcome() {
        let (runtime, mut guest) = guest();
        let recorder = TraceRecorder::new(8).unwrap();
        let panic = tracing::subscriber::with_default(
            GuestLog(Box::new(|| panic!("recording host panic"))),
            || guest.handle_with_recording("host-panic", b"", Recording::root(recorder.root())),
        );
        assert_eq!(
            panic.unwrap_err(),
            ComponentError::Panic("recording host panic".into())
        );
        assert_eq!(recorder.snapshot().records[0].status, SpanStatus::Panicked);
        let engine = runtime.engine().clone();
        let epoch_root = recorder.root();
        let epoch = tracing::subscriber::with_default(
            GuestLog(Box::new(move || {
                for _ in 0..1000 {
                    engine.increment_epoch();
                }
            })),
            || guest.handle_with_recording("epoch", b"", Recording::root(epoch_root.clone())),
        );
        assert_eq!(epoch.unwrap_err(), ComponentError::EpochDeadline);
        let snapshot = recorder.snapshot();
        let epoch_child = snapshot
            .records
            .iter()
            .find(|record| {
                record.trace_id == epoch_root.trace_id()
                    && record.parent_span_id == Some(epoch_root.span_id())
                    && record.kind == SpanKind::WasmGuest
            })
            .unwrap();
        assert_eq!(epoch_child.status, SpanStatus::Cancelled);
        guest
            .handle_with_recording("nested", b"", Recording::root(recorder.root()))
            .unwrap();
        recorder.close();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 8);
        assert_eq!(snapshot.counts.completed, 8);
        assert_eq!(
            snapshot
                .records
                .iter()
                .filter(|record| record.kind == SpanKind::WasmLifecycle
                    && record.status == SpanStatus::Ok)
                .count(),
            2
        );
        assert_eq!(
            snapshot
                .records
                .iter()
                .filter(
                    |record| record.target == "guest/on-start" && record.status == SpanStatus::Ok
                )
                .count(),
            2
        );
    }

    #[test]
    fn lifecycle_host_panic_and_epoch_retire_guest_and_host_with_matching_statuses() {
        for phase in ["on-start", "on-drain"] {
            for mode in ["host-panic", "epoch"] {
                let (runtime, mut guest) = lifecycle_guest(phase, mode);
                if phase == "on-drain" {
                    guest.on_start().unwrap();
                }
                let recorder = TraceRecorder::new(8).unwrap();
                let engine = runtime.engine().clone();
                let result = tracing::subscriber::with_default(
                    GuestLog(Box::new(move || {
                        if mode == "host-panic" {
                            panic!("lifecycle host failure");
                        }
                        for _ in 0..1000 {
                            engine.increment_epoch();
                        }
                    })),
                    || {
                        recorder.root().in_scope(|| {
                            if phase == "on-start" {
                                on_start(&mut guest)
                            } else {
                                on_drain(&mut guest)
                            }
                        })
                    },
                );
                let (error, status) = if mode == "host-panic" {
                    (
                        ComponentError::Panic("lifecycle host failure".into()),
                        SpanStatus::Panicked,
                    )
                } else {
                    (ComponentError::EpochDeadline, SpanStatus::Cancelled)
                };
                assert_eq!(result, Err(error));
                recorder.close();
                let snapshot = recorder.snapshot();
                assert!(snapshot.terminal);
                assert_eq!(snapshot.counts.started, 2);
                assert!(snapshot
                    .records
                    .iter()
                    .all(|record| record.status == status));
                guest.handle("disabled", b"").unwrap();
                assert_eq!(recorder.snapshot().counts.started, 2);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_initialization_waiter_does_not_retire_running_lifecycle_owners() {
        use std::{sync::Arc, time::Duration};
        let (_runtime, guest) = lifecycle_guest("on-start", "host-panic");
        let recorder = TraceRecorder::new(8).unwrap();
        let context = recorder.root();
        let entered = Arc::new(tokio::sync::Notify::new());
        let gate = entered.clone();
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let released = std::sync::Mutex::new(released);
        let (finished, done) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let result = tracing::subscriber::with_default(
                GuestLog(Box::new(move || {
                    gate.notify_one();
                    released
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                })),
                || context.in_scope(|| super::super::WasmActor::new(guest)),
            );
            assert!(result.is_ok());
            drop(result);
            finished.send(()).unwrap();
        });
        let caller = tokio::spawn(async move { worker.await.unwrap() });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        recorder.close();
        let pending = recorder.snapshot();
        assert!(!pending.terminal);
        assert_eq!(pending.counts.in_flight, 2);
        assert_eq!(pending.counts.completed, 0);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .unwrap()
            .unwrap();
        let complete = recorder.snapshot();
        assert!(complete.terminal);
        assert_eq!(complete.counts.started, 2);
        assert_eq!(complete.counts.ok, 2);
    }

    #[derive(Debug)]
    struct PanicRoot(TraceContext);
    #[derive(Debug)]
    struct PanicDropRoot(TraceContext);
    impl GuestTrace for PanicDropRoot {
        fn parent(&self) -> TraceParent {
            Recording::root(self.0.clone()).parent()
        }
        fn start(&self, target: &str) -> Option<Box<dyn GuestSpan>> {
            Recording::root(self.0.clone()).start(target)
        }
    }
    impl Drop for PanicDropRoot {
        fn drop(&mut self) {
            panic!("host recording capability destructor");
        }
    }

    #[test]
    fn admitted_capability_destructor_panic_is_retained_and_vetoes_reuse() {
        for operation in 0..4 {
            let (_runtime, mut guest) = guest();
            let recorder = TraceRecorder::new(8).unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let capability = Box::new(PanicDropRoot(recorder.root()));
                match operation {
                    0 => guest
                        .handle_with_recording("nested", b"", capability)
                        .map(|_| ()),
                    1 => guest.tell_with_recording("nested", b"", capability),
                    2 => guest.on_start_with_recording(capability),
                    _ => guest.on_drain_with_recording(capability),
                }
            }));
            assert!(
                result.is_ok(),
                "admitted capability destructor escaped the host boundary"
            );
            assert_eq!(
                result.unwrap(),
                Err(ComponentError::Panic(
                    "host recording capability destructor".into()
                ))
            );
            recorder.close();
            assert!(recorder.snapshot().terminal);
            let error = ComponentError::Panic("host recording capability destructor".into());
            assert_eq!(guest.cleanup_error(), Some(error.clone()));
            assert!(!guest.is_alive());
            assert_eq!(guest.handle("disabled", b""), Err(error));
        }
    }
    #[derive(Debug)]
    struct PanicFinish(Box<dyn GuestSpan>);
    impl GuestTrace for PanicRoot {
        fn parent(&self) -> TraceParent {
            Recording::root(self.0.clone()).parent()
        }
        fn start(&self, target: &str) -> Option<Box<dyn GuestSpan>> {
            Recording::root(self.0.clone())
                .start(target)
                .map(|span| Box::new(PanicFinish(span)) as Box<dyn GuestSpan>)
        }
    }
    impl GuestTrace for PanicFinish {
        fn parent(&self) -> TraceParent {
            self.0.parent()
        }
        fn start(&self, target: &str) -> Option<Box<dyn GuestSpan>> {
            self.0
                .start(target)
                .map(|span| Box::new(Self(span)) as Box<dyn GuestSpan>)
        }
    }
    impl GuestSpan for PanicFinish {
        fn finish(self: Box<Self>, _: GuestSpanStatus) {
            panic!("host finish callback");
        }
    }

    #[test]
    fn recording_finish_panics_retire_every_owner_and_veto_reuse() {
        let (_runtime, mut guest) = guest();
        let recorder = TraceRecorder::new(8).unwrap();
        let result = guest.handle_with_recording(
            "unfinished-nested",
            b"",
            Box::new(PanicRoot(recorder.root())),
        );
        assert_eq!(
            result.unwrap_err(),
            ComponentError::Panic("host finish callback".into())
        );
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.started, 3);
        assert_eq!(snapshot.counts.panicked, 3);
        assert_eq!(snapshot.counts.in_flight, 0);
        let error = ComponentError::Panic("host finish callback".into());
        assert_eq!(guest.cleanup_error(), Some(error.clone()));
        assert!(!guest.is_alive());
        assert_eq!(
            guest.handle_with_recording("nested", b"", Recording::root(recorder.root())),
            Err(error)
        );
        recorder.close();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 3);
        assert_eq!(snapshot.counts.completed, 3);
        assert_eq!(snapshot.counts.panicked, 3);
        assert!(!snapshot
            .records
            .iter()
            .any(|record| record.kind == SpanKind::WasmLifecycle));
    }

    #[derive(Debug)]
    struct HostFailure {
        parent_panic: bool,
    }
    impl GuestTrace for HostFailure {
        fn parent(&self) -> TraceParent {
            if self.parent_panic {
                panic!("host parent callback");
            }
            TraceParent {
                trace_id: [1; 16],
                span_id: [2; 8],
            }
        }
        fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
            panic!("host start callback");
        }
    }

    #[test]
    fn host_parent_and_start_panics_return_errors_for_messages_and_lifecycle() {
        let (_runtime, mut guest) = guest();
        for parent_panic in [true, false] {
            for operation in 0..4 {
                let capability = Box::new(HostFailure { parent_panic });
                let result = match operation {
                    0 => guest.tell_with_recording("unfinished", b"", capability),
                    1 => guest
                        .handle_with_recording("unfinished", b"", capability)
                        .map(|_| ()),
                    2 => guest.on_start_with_recording(capability),
                    _ => guest.on_drain_with_recording(capability),
                };
                let message = if parent_panic {
                    "host parent callback"
                } else {
                    "host start callback"
                };
                assert_eq!(result.unwrap_err(), ComponentError::Panic(message.into()));
                guest.handle("disabled", b"").unwrap();
            }
        }
    }
}
