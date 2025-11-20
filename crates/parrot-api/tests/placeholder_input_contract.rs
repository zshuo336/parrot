use futures::{executor::block_on, FutureExt};
use parrot_api::{
    address::{ActorPath, ActorRef},
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

struct Input {
    drops: Arc<AtomicUsize>,
    panic_on_drop: bool,
}

impl Drop for Input {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_on_drop, "placeholder input destructor");
    }
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    Send,
    NoneTimeout,
    ExplicitTimeout,
    Deliver,
}

impl Operation {
    fn call(
        self,
        reference: &dyn ActorRef,
        message: BoxedMessage,
    ) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        match self {
            Self::Send => reference.send(message),
            Self::NoneTimeout => reference.send_with_timeout(message, None),
            Self::ExplicitTimeout => {
                reference.send_with_timeout(message, Some(Duration::from_secs(1)))
            }
            Self::Deliver => reference
                .deliver(message)
                .map(|result| result.map(|_| Box::new(()) as BoxedMessage))
                .boxed(),
        }
    }
}

fn check(cancel: bool, panic_on_drop: bool) {
    for operation in [
        Operation::Send,
        Operation::NoneTimeout,
        Operation::ExplicitTimeout,
        Operation::Deliver,
    ] {
        let path = ActorPath::placeholder("local://missing/user/actor");
        let drops = Arc::new(AtomicUsize::new(0));
        let input = Box::new(Input {
            drops: drops.clone(),
            panic_on_drop,
        });
        let constructed = std::panic::catch_unwind(AssertUnwindSafe(|| {
            operation.call(path.target.as_ref(), input)
        }));
        assert!(constructed.is_ok(), "{operation:?} leaked input Drop panic");
        let future = constructed.unwrap();
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "{operation:?} retired early"
        );
        if cancel {
            assert!(
                std::panic::catch_unwind(AssertUnwindSafe(|| drop(future))).is_ok(),
                "{operation:?} cancellation leaked input Drop panic"
            );
        } else {
            // An inert target must reject even outside a Tokio runtime.
            let result = block_on(AssertUnwindSafe(future).catch_unwind());
            assert!(result.is_ok(), "{operation:?} rejection unwound");
            assert!(
                matches!(result.unwrap(), Err(ActorError::ActorNotFound(ref path)) if path == "dead://placeholder"),
                "{operation:?} lost the original rejection"
            );
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1, "{operation:?} lost input");
        assert!(!block_on(path.target.is_alive()));
    }
}

#[test]
fn placeholder_rejection_retires_each_message_once() {
    check(false, false);
}

#[test]
fn placeholder_rejection_isolates_message_destructor_panics() {
    check(false, true);
}

#[test]
fn unpolled_placeholder_cancellation_retires_each_message_once() {
    check(true, false);
}

#[test]
fn unpolled_placeholder_cancellation_isolates_message_destructor_panics() {
    check(true, true);
}
