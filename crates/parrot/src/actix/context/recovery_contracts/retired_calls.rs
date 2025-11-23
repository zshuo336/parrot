use super::*;
use parrot_api::{address::ActorRef, trace::TraceContext};
use std::{
    future::Future,
    pin::Pin,
    sync::Mutex,
    task::{Context as PollContext, Poll},
};

#[derive(Clone, Debug, PartialEq)]
struct Event {
    stage: &'static str,
    thread: std::thread::ThreadId,
    trace: Option<TraceContext>,
}
type Events = Arc<Mutex<Vec<Event>>>;
fn record(events: &Events, stage: &'static str) {
    events.lock().unwrap().push(Event {
        stage,
        thread: std::thread::current().id(),
        trace: TraceContext::current(),
    });
}
struct MessageProbe {
    events: Events,
    panic: bool,
}
impl Message for MessageProbe {
    type Result = ();
}
impl Clone for MessageProbe {
    fn clone(&self) -> Self {
        panic!("a rejected timer must not clone its message");
    }
}
impl Drop for MessageProbe {
    fn drop(&mut self) {
        record(&self.events, "message disposal");
        assert!(!self.panic, "rejected timer message disposal witness");
    }
}
struct Target {
    events: Events,
    panic: bool,
}
impl std::fmt::Debug for Target {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RetiredContextTarget")
    }
}
impl Drop for Target {
    fn drop(&mut self) {
        record(&self.events, "target disposal");
        assert!(!self.panic, "retired target disposal witness");
    }
}
struct Rejection<'a, T> {
    target: &'a Target,
    input: Option<BoxedMessage>,
    output: std::marker::PhantomData<T>,
}
impl<T: Unpin> Future for Rejection<'_, T> {
    type Output = ActorResult<T>;
    fn poll(self: Pin<&mut Self>, _: &mut PollContext<'_>) -> Poll<Self::Output> {
        record(&self.target.events, "delegate poll");
        Poll::Ready(Err(ActorError::Stopped))
    }
}
impl<T> Drop for Rejection<'_, T> {
    fn drop(&mut self) {
        record(&self.target.events, "delegate disposal");
        drop(self.input.take());
        assert!(!self.target.panic, "retired delegate disposal witness");
    }
}
impl Target {
    fn reject<T: Send + Unpin + 'static>(
        &self,
        input: BoxedMessage,
    ) -> BoxedFuture<'_, ActorResult<T>> {
        record(&self.events, "delegate invocation");
        Box::pin(Rejection {
            target: self,
            input: Some(input),
            output: std::marker::PhantomData,
        })
    }
}
impl ActorRef for Target {
    fn send<'a>(&'a self, input: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.reject(input)
    }
    fn send_with_timeout<'a>(
        &'a self,
        input: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.reject(input)
    }
    fn deliver<'a>(&'a self, input: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.reject(input)
    }
    fn watch<'a>(&'a self, watcher: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        self.reject(Box::new(watcher))
    }
    fn unwatch<'a>(&'a self, watcher: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        self.reject(Box::new(watcher))
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("context delegation must not stop its target");
    }
    fn path(&self) -> String {
        "/retired-context-target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        panic!("context delegation must not query its target");
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        panic!("a rejected timer must not clone its target");
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn check(entry: u8) {
    for hostile in [false, true] {
        let (mut context, slots) = std::thread::spawn(expired).join().unwrap();
        assert!(tokio::runtime::Handle::try_current().is_err());
        assert!(actix::System::try_current().is_none());
        let before = pending(&slots);
        let events = Events::default();
        let target = || {
            Box::new(Target {
                events: events.clone(),
                panic: hostile,
            })
        };
        let message = || MessageProbe {
            events: events.clone(),
            panic: hostile && entry >= 4,
        };
        let invocation = TraceContext::root();
        let polling = TraceContext::root();
        let request: BoxedFuture<'_, ActorResult<()>> = invocation.in_scope(|| match entry {
            0 => context.send(target(), Box::new(message())),
            1 => {
                use futures::TryFutureExt;
                Box::pin(context.ask(target(), Box::new(message())).map_ok(|_| ()))
            }
            2 | 3 => {
                if entry == 2 {
                    context.watch(target())
                } else {
                    context.unwatch(target())
                }
            }
            4 => context.schedule_once(target(), Box::new(message()), WAIT),
            5 => context.schedule_periodic(
                target(),
                CloneableMessage::from_message(message()),
                WAIT,
                WAIT,
            ),
            6 | 7 => {
                use futures::TryFutureExt;
                Box::pin(
                    context
                        .schedule(message(), WAIT, (entry == 7).then_some(WAIT))
                        .map_ok(|_| ()),
                )
            }
            _ => unreachable!(),
        });
        assert!(events.lock().unwrap().is_empty());
        let result = polling.in_scope(|| futures::executor::block_on(request));
        assert!(TraceContext::current().is_none());
        if hostile && entry >= 4 {
            assert!(
                matches!(result, Err(ActorError::Panic(ref text)) if text.contains("disposal witness"))
            );
        } else {
            assert!(matches!(result, Err(ActorError::Stopped)));
        }
        let expected_stages = match entry {
            0 | 1 => vec![
                "delegate invocation",
                "delegate poll",
                "delegate disposal",
                "message disposal",
                "target disposal",
            ],
            2 | 3 => vec![
                "delegate invocation",
                "delegate poll",
                "delegate disposal",
                "target disposal",
            ],
            4 | 5 => vec!["target disposal", "message disposal"],
            _ => vec!["message disposal"],
        };
        let actual = events.lock().unwrap().clone();
        let mut stages = actual.iter().map(|event| event.stage).collect::<Vec<_>>();
        let mut expected_stages = expected_stages;
        if entry >= 4 {
            stages.sort_unstable();
            expected_stages.sort_unstable();
        }
        assert_eq!(stages, expected_stages);
        for event in actual {
            assert_eq!(event.thread, std::thread::current().id());
            assert_eq!(event.trace, Some(invocation.clone()));
        }
        for _ in 0..2 {
            let receipt = futures::executor::block_on(context.tasks.join());
            if hostile && entry < 4 {
                assert!(
                    matches!(receipt, Err(ActorError::Panic(ref text)) if text.contains("delegate disposal witness"))
                );
            } else {
                receipt.unwrap();
            }
        }
        assert_eq!(slots.available_permits(), 1);
        assert_eq!(pending(&slots), before);
        assert!(!crate::owned_payload::DisposalScope::active());
    }
}
macro_rules! contract {
    ($name:ident, $entry:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!(
                    "actix::context::recovery_contracts::retired_calls::",
                    stringify!($name)
                ),
                || check($entry),
            );
        }
    };
}
contract!(send_keeps_rejection_after_delegate_and_target_disposal, 0);
contract!(ask_keeps_rejection_after_delegate_and_target_disposal, 1);
contract!(watch_keeps_rejection_after_delegate_and_target_disposal, 2);
contract!(
    unwatch_keeps_rejection_after_delegate_and_target_disposal,
    3
);
contract!(once_rejection_retires_both_inputs_without_a_host, 4);
contract!(periodic_rejection_retires_both_inputs_without_a_host, 5);
contract!(typed_once_rejection_retires_input_without_a_host, 6);
contract!(typed_periodic_rejection_retires_input_without_a_host, 7);
