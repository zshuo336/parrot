//! Context calls capture at construction; owned timers preserve that parent.
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContextScheduler, ScheduledTask},
    message::{CloneableMessage, Message},
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{future::Future, sync::Arc, time::Duration};

#[derive(Clone)]
struct Payload(flume::Sender<Option<TraceContext>>);
impl Message for Payload {
    type Result = ();
}
#[derive(Clone, Copy)]
enum Mode {
    Ask,
    Send,
    Once,
    Periodic,
    SchedulerOnce,
    SchedulerPeriodic,
}
struct Request {
    payload: Payload,
    traced: bool,
}
struct Echo;
impl Actor for Echo {
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
        let payload = message.downcast::<Payload>().unwrap();
        let context = TraceContext::current();
        Box::pin(async move {
            let _ = payload.0.send_async(context).await;
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
fn invoke<'a>(
    context: &'a impl ActorContextScheduler,
    target: BoxedActorRef,
    mode: Mode,
    request: Request,
) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
    let expected = request.traced.then(TraceContext::current).flatten();
    let poller = TraceContext::root();
    let make = || -> BoxedFuture<'a, ActorResult<Option<ScheduledTask>>> {
        match mode {
            Mode::Ask => {
                let future = context.ask(target, Box::new(request.payload));
                Box::pin(async move { future.await.map(|_| None) })
            }
            Mode::Send => {
                let future = context.send(target, Box::new(request.payload));
                Box::pin(async move { future.await.map(|()| None) })
            }
            Mode::Once => {
                let future =
                    context.schedule_once(target, Box::new(request.payload), Duration::ZERO);
                Box::pin(async move { future.await.map(|()| None) })
            }
            Mode::Periodic => {
                let future = context.schedule_periodic(
                    target,
                    CloneableMessage::from_message(request.payload),
                    Duration::ZERO,
                    Duration::from_millis(10),
                );
                Box::pin(async move { future.await.map(|()| None) })
            }
            Mode::SchedulerOnce | Mode::SchedulerPeriodic => {
                let future = context.schedule(
                    request.payload,
                    Duration::ZERO,
                    matches!(mode, Mode::SchedulerPeriodic).then_some(Duration::from_millis(10)),
                );
                Box::pin(async move { future.await.map(Some) })
            }
        }
    };
    let future = if request.traced {
        make()
    } else {
        trace::in_scope(None, make)
    };
    Box::pin(async move {
        let task = poller.scope(future).await?;
        Ok(Box::new((expected, task)) as BoxedMessage)
    })
}
struct ThreadFront {
    target: BoxedActorRef,
    mode: Mode,
}
impl Actor for ThreadFront {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        if message.is::<Payload>() {
            let payload = message.downcast::<Payload>().unwrap();
            let current = TraceContext::current();
            return Box::pin(async move {
                let _ = payload.0.send_async(current).await;
                Ok(Box::new(()) as BoxedMessage)
            });
        }
        invoke(
            context,
            self.target.clone_boxed(),
            self.mode,
            *message.downcast::<Request>().unwrap(),
        )
    }
}
struct ActixFront {
    target: BoxedActorRef,
    mode: Mode,
}
impl Actor for ActixFront {
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
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        if message.is::<Payload>() {
            let payload = message.downcast::<Payload>().unwrap();
            let current = TraceContext::current();
            return Box::pin(async move {
                let _ = payload.0.send_async(current).await;
                Ok(Box::new(()) as BoxedMessage)
            });
        }
        invoke(
            context,
            self.target.clone_boxed(),
            self.mode,
            *message.downcast::<Request>().unwrap(),
        )
    }
}
impl ActixEngineExt for ActixFront {}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("context trace deadline")
}
fn exercise(use_actix: bool, mode: Mode) {
    actix::System::new().block_on(async move {
        let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
        let actix = Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        );
        let leaf = bounded(thread.spawn_root_typed_thread(Echo, EmptyConfig))
            .await
            .unwrap();
        let front: BoxedActorRef = if use_actix {
            bounded(actix.spawn_root_typed(
                ActixFront {
                    target: leaf.clone_boxed(),
                    mode,
                },
                EmptyConfig,
            ))
            .await
            .unwrap()
            .clone_boxed()
        } else {
            bounded(thread.spawn_root_typed_thread(
                ThreadFront {
                    target: leaf.clone_boxed(),
                    mode,
                },
                EmptyConfig,
            ))
            .await
            .unwrap()
            .clone_boxed()
        };
        let root = TraceContext::root();
        let mut receipts = Vec::new();
        for traced in [true, false] {
            let (sender, receiver) = flume::unbounded();
            let future = root.in_scope(|| {
                front.send(Box::new(Request {
                    payload: Payload(sender),
                    traced,
                }))
            });
            let (expected, task) = *bounded(future)
                .await
                .unwrap()
                .downcast::<(Option<TraceContext>, Option<ScheduledTask>)>()
                .unwrap();
            let mut observed = vec![bounded(receiver.recv_async()).await.unwrap()];
            if matches!(mode, Mode::Periodic | Mode::SchedulerPeriodic) {
                observed.push(bounded(receiver.recv_async()).await.unwrap());
            }
            if let Some(task) = &task {
                task.cancel();
            }
            receipts.push((expected, observed, task));
        }
        bounded(front.stop()).await.unwrap();
        bounded(leaf.stop()).await.unwrap();
        bounded(thread.shutdown_internal()).await.unwrap();
        bounded(actix.as_ref().clone().shutdown()).await.unwrap();
        if matches!(mode, Mode::SchedulerOnce | Mode::SchedulerPeriodic) {
            assert_ne!(
                receipts[0].2.as_ref().unwrap().id,
                receipts[1].2.as_ref().unwrap().id
            );
            for (_, _, task) in &receipts {
                assert_eq!(task.as_ref().unwrap().target.path(), front.path());
            }
        }
        for (expected, observations, _) in receipts {
            if expected.is_some() && observations.len() == 2 {
                assert_ne!(
                    observations[0].as_ref().unwrap().span_id(),
                    observations[1].as_ref().unwrap().span_id()
                );
            }
            for observed in observations {
                match &expected {
                    Some(parent) => {
                        let child = observed.expect("scheduled recipient lost the caller parent");
                        assert_eq!(
                            child.trace_id(),
                            parent.trace_id(),
                            "context operation adopted its poller identity"
                        );
                        assert_eq!(child.parent_span_id(), Some(parent.span_id()));
                        assert_ne!(child.span_id(), parent.span_id());
                    }
                    None => assert!(
                        observed.is_none(),
                        "untraced operation adopted its poller identity"
                    ),
                }
            }
        }
    });
}
#[test]
fn thread_context_ask_keeps_call_time_parent() {
    exercise(false, Mode::Ask);
}
#[test]
fn actix_context_ask_keeps_call_time_parent() {
    exercise(true, Mode::Ask);
}
#[test]
fn thread_context_send_keeps_call_time_parent() {
    exercise(false, Mode::Send);
}
#[test]
fn actix_context_send_keeps_call_time_parent() {
    exercise(true, Mode::Send);
}
#[test]
fn thread_context_once_keeps_call_time_parent() {
    exercise(false, Mode::Once);
}
#[test]
fn actix_context_once_keeps_call_time_parent() {
    exercise(true, Mode::Once);
}
#[test]
fn thread_context_periodic_keeps_call_time_parent() {
    exercise(false, Mode::Periodic);
}
#[test]
fn actix_context_periodic_keeps_call_time_parent() {
    exercise(true, Mode::Periodic);
}

#[test]
fn thread_scheduler_once_keeps_call_time_parent() {
    exercise(false, Mode::SchedulerOnce);
}
#[test]
fn actix_scheduler_once_keeps_call_time_parent() {
    exercise(true, Mode::SchedulerOnce);
}
#[test]
fn thread_scheduler_periodic_keeps_call_time_parent() {
    exercise(false, Mode::SchedulerPeriodic);
}
#[test]
fn actix_scheduler_periodic_keeps_call_time_parent() {
    exercise(true, Mode::SchedulerPeriodic);
}
