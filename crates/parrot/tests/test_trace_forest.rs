//! The forest indexes records emitted by real mixed-engine mailbox chains.
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    trace::{
        ParentRelation, SpanForest, SpanGuard, SpanKind, SpanStatus, TraceContext, TraceRecorder,
    },
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{future::Future, sync::Arc, time::Duration};

struct Request;
impl parrot_api::message::Message for Request {
    type Result = Vec<TraceContext>;
}
fn chain(next: &Option<BoxedActorRef>) -> BoxedFuture<'_, ActorResult<Vec<TraceContext>>> {
    let constructed = TraceContext::current().expect("construct without captured context");
    Box::pin(async move {
        tokio::task::yield_now().await;
        assert_eq!(TraceContext::current(), Some(constructed.clone()));
        let mut contexts = vec![constructed];
        if let Some(next) = next {
            contexts.extend(
                *next
                    .send(Box::new(Request))
                    .await?
                    .downcast::<Vec<TraceContext>>()
                    .unwrap(),
            );
        }
        Ok(contexts)
    })
}
struct Typed(Option<BoxedActorRef>);
impl ParrotTypedDispatch for Typed {
    type Msg = Request;
    type Reply = Vec<TraceContext>;
    fn dispatch(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<Self::Reply>> {
        chain(&self.0)
    }
}
impl ParrotMsgVariant<Typed> for Request {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: Vec<TraceContext>) -> ActorResult<Vec<TraceContext>> {
        Ok(reply)
    }
}
struct Thread(Option<BoxedActorRef>);
impl Actor for Thread {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert!(message.is::<Request>());
        let operation = chain(&self.0);
        Box::pin(async move { Ok(Box::new(operation.await?) as BoxedMessage) })
    }
}
struct Actix(Option<BoxedActorRef>);
impl Actor for Actix {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert!(message.is::<Request>());
        let operation = chain(&self.0);
        Box::pin(async move { Ok(Box::new(operation.await?) as BoxedMessage) })
    }
}
impl ActixEngineExt for Actix {}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("mixed-engine chain timed out")
}
async fn verify_chain(front: &dyn ActorRef) {
    let recorder = TraceRecorder::new(16).unwrap();
    let mut expected = Vec::new();
    for record_root in [true, false] {
        let root = recorder.root();
        let contexts = root
            .scope(async {
                let owner =
                    record_root.then(|| SpanGuard::current(SpanKind::Application, "/caller"));
                let reply = bounded(front.send(Box::new(Request))).await.unwrap();
                if let Some(owner) = owner {
                    owner.finish(SpanStatus::Ok);
                }
                *reply.downcast::<Vec<TraceContext>>().unwrap()
            })
            .await;
        assert_eq!(contexts.len(), 3);
        expected.push((root, contexts, record_root));
    }
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.counts.completed, 7);
    assert_eq!(snapshot.counts.overwritten, 0);
    let forest = SpanForest::new(&snapshot).unwrap();
    assert_eq!(forest.nodes().len(), 7);
    assert_eq!(forest.roots().len(), 2);
    assert_eq!(forest.diagnostics().unobserved_parent_records, 1);
    assert_eq!(forest.diagnostics().ambiguous_parent_records, 0);
    assert_eq!(forest.diagnostics().cycle_records, 0);
    for (root, chain, record_root) in expected {
        let mut parent = root.clone();
        for (hop, context) in chain.iter().enumerate() {
            let (index, node) = forest
                .nodes()
                .iter()
                .enumerate()
                .find(|(_, node)| {
                    node.record().trace_id == root.trace_id()
                        && node.record().span_id == context.span_id()
                })
                .expect("actual handler missing from forest");
            assert_eq!(node.record().parent_span_id, Some(parent.span_id()));
            assert_eq!(node.record().status, SpanStatus::Ok);
            assert_eq!(
                node.record().kind,
                if hop == 1 {
                    SpanKind::TypedHandler
                } else {
                    SpanKind::DynamicHandler
                }
            );
            if hop == 0 && !record_root {
                assert_eq!(node.parent(), ParentRelation::Unobserved);
                assert!(forest.roots().contains(&index));
            } else {
                let ParentRelation::Recorded(parent_index) = node.parent() else {
                    panic!("recorded ancestor not linked")
                };
                assert_eq!(
                    forest.nodes()[parent_index].record().span_id,
                    parent.span_id()
                );
                assert_eq!(forest.nodes()[parent_index].children(), [index]);
                // Child completion precedes parent in the raw ring; the forest
                // must not use completion order as a parent heuristic.
                let child_position = snapshot
                    .records
                    .iter()
                    .position(|r| r.span_id == context.span_id())
                    .unwrap();
                let parent_position = snapshot
                    .records
                    .iter()
                    .position(|r| r.span_id == parent.span_id())
                    .unwrap();
                assert!(child_position < parent_position);
            }
            parent = context.clone();
        }
    }
    let view = forest.render_text(8192).unwrap();
    assert_eq!((view.emitted_records, view.omitted_records), (7, 0));
    assert_eq!(
        view.text.lines().filter(|l| l.contains("target=")).count(),
        7
    );
}

#[test]
fn thread_dynamic_actix_typed_thread_dynamic_records_form_actual_tree() {
    actix::System::new().block_on(async {
        let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
        let actix = Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        );
        let leaf = bounded(thread.spawn_root_typed_thread(Thread(None), EmptyConfig))
            .await
            .unwrap();
        let middle = bounded(
            actix.spawn_typed::<Typed, Request>(Typed(Some(leaf.clone_boxed())), "/middle"),
        )
        .await
        .unwrap();
        let front = bounded(
            thread.spawn_root_typed_thread(Thread(Some(middle.clone().into_dyn())), EmptyConfig),
        )
        .await
        .unwrap();
        verify_chain(&front).await;
        bounded(front.stop()).await.unwrap();
        bounded(middle.stop()).await.unwrap();
        bounded(leaf.stop()).await.unwrap();
        bounded(thread.shutdown_internal()).await.unwrap();
        bounded(actix.as_ref().clone().shutdown()).await.unwrap();
    });
}

#[test]
fn actix_dynamic_thread_typed_actix_dynamic_records_form_actual_tree() {
    actix::System::new().block_on(async {
        let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
        let actix = Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        );
        let leaf = bounded(actix.spawn_root_typed(Actix(None), EmptyConfig))
            .await
            .unwrap();
        let middle = bounded(
            thread.spawn_typed::<Typed, Request>(Typed(Some(leaf.clone_boxed())), "/middle"),
        )
        .await
        .unwrap();
        let front =
            bounded(actix.spawn_root_typed(Actix(Some(middle.clone().into_dyn())), EmptyConfig))
                .await
                .unwrap();
        verify_chain(front.as_ref()).await;
        bounded(front.stop()).await.unwrap();
        bounded(middle.stop()).await.unwrap();
        bounded(leaf.stop()).await.unwrap();
        bounded(thread.shutdown_internal()).await.unwrap();
        bounded(actix.as_ref().clone().shutdown()).await.unwrap();
    });
}
