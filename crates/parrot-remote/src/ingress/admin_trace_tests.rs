use super::*;
use crate::{
    admin::{AdminCommand, SysEvent},
    admin_v2::{AdminCommandV2, AdminReplyV2},
    security::ControlPolicy,
    IncomingTracePolicy,
};
use parrot_api::trace::{SpanGuard, SpanKind, SpanStatus, TraceContext, TraceRecorder};
use std::sync::Mutex;

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}
#[derive(Default)]
struct Observe(Mutex<Vec<Option<TraceContext>>>);
#[async_trait::async_trait]
impl SysEventHook for Observe {
    async fn on_event(&self, _: SysEvent, _: &FrameSender, _: &str) {
        self.0.lock().unwrap().push(TraceContext::current());
        SpanGuard::child(SpanKind::ManagedTask, "test/admin").finish(SpanStatus::Ok);
    }
}
fn command() -> Frame {
    let mut frame = crate::admin_v2::reply_frame_v2(7, "", &AdminReplyV2::Stopped { req_id: 7 });
    frame.payload = crate::admin_v2::encode_admin_cmd_v2(&AdminCommandV2::StopComponent {
        req_id: 7,
        path_prefix: "/missing".into(),
    });
    frame
}
fn setup(policy: ControlPolicy) -> (Arc<Ingress>, Arc<Observe>, FrameSender) {
    let ingress = Arc::new(
        Ingress::new(Arc::new(Empty), Arc::new(CallbackRegistry::new(32)))
            .with_control_policy(policy),
    );
    let observe = Arc::new(Observe::default());
    *ingress.sys_event_slot() = Some(observe.clone());
    let (sender, _receiver) = tokio::sync::mpsc::channel(8);
    (ingress, observe, FrameSender::anon(sender))
}

#[tokio::test]
async fn default_and_propagate_only_management_do_not_inherit_ambient_identity() {
    for attach in [false, true] {
        let (ingress, observe, back) = setup(ControlPolicy::InsecureDevelopment);
        let recorder = TraceRecorder::new(8).unwrap();
        if attach {
            ingress.set_trace_recorder(Some(recorder.clone()));
        }
        recorder
            .root()
            .scope(ingress.dispatch(command(), &back, "peer"))
            .await;
        let seen = observe.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].is_none(),
            "management inherited unrelated environment"
        );
        assert_eq!(recorder.snapshot().counts.started, 0);
    }
}

#[tokio::test]
async fn authorized_start_roots_management_has_independent_receiver_identity() {
    let (ingress, observe, back) = setup(ControlPolicy::InsecureDevelopment);
    let recorder = TraceRecorder::new(8).unwrap();
    let ambient = recorder.root();
    let _attachment =
        ingress.attach_trace_recorder(recorder.clone(), IncomingTracePolicy::StartRoots);
    for _ in 0..2 {
        ambient
            .scope(ingress.dispatch(command(), &back, "peer"))
            .await;
    }
    let seen = observe.0.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let contexts: Vec<_> = seen.iter().map(|value| value.as_ref().unwrap()).collect();
    for context in &contexts {
        assert_ne!(context.trace_id(), ambient.trace_id());
        assert_eq!(context.parent_span_id(), None);
    }
    assert_ne!(contexts[0].trace_id(), contexts[1].trace_id());
    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.records.len(), 2);
    for (record, context) in snapshot.records.iter().zip(contexts) {
        assert_eq!(record.trace_id, context.trace_id());
        assert_eq!(record.parent_span_id, Some(context.span_id()));
    }
}

#[tokio::test]
async fn unauthorized_management_and_non_commands_never_start_roots() {
    for unauthorized in [false, true] {
        let (ingress, observe, back) = setup(if unauthorized {
            ControlPolicy::LocalOnly
        } else {
            ControlPolicy::InsecureDevelopment
        });
        let recorder = TraceRecorder::new(8).unwrap();
        let _attachment =
            ingress.attach_trace_recorder(recorder.clone(), IncomingTracePolicy::StartRoots);
        let frame = if unauthorized {
            command()
        } else {
            crate::admin_v2::reply_frame_v2(7, "", &AdminReplyV2::Stopped { req_id: 7 })
        };
        recorder
            .root()
            .scope(ingress.dispatch(frame, &back, "peer"))
            .await;
        let mut legacy = command();
        legacy.payload = crate::admin::encode_admin_cmd(&AdminCommand::AdminStop {
            req_id: 7,
            target_path: "/missing".into(),
            reply_to: String::new(),
        });
        recorder
            .root()
            .scope(ingress.dispatch(legacy, &back, "peer"))
            .await;
        let seen = observe.0.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(Option::is_none));
        assert_eq!(recorder.snapshot().counts.started, 0);
    }
}

#[tokio::test]
async fn mismatched_management_request_id_is_rejected_before_hook_and_recording() {
    let (ingress, observe, back) = setup(ControlPolicy::InsecureDevelopment);
    let recorder = TraceRecorder::new(8).unwrap();
    let _attachment =
        ingress.attach_trace_recorder(recorder.clone(), IncomingTracePolicy::StartRoots);
    let mut frame = command();
    frame.header.correlation_id = 8;
    recorder
        .root()
        .scope(ingress.dispatch(frame, &back, "peer"))
        .await;
    assert!(observe.0.lock().unwrap().is_empty());
    assert_eq!(recorder.snapshot().counts.started, 0);
}
