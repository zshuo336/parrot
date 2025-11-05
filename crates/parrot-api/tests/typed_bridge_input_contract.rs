//! Direct typed-to-dynamic references own messages even before first poll.
use futures::FutureExt;
use parrot_api::{
    address::ActorRef,
    errors::ActorError,
    message::Message,
    typed::{ParrotMsgVariant, ParrotTypedDispatch, TerminationProtocol},
    typed_channel::{typed_channel, TypedActorRef},
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
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("direct bridge input destructor");
    }
}
struct Data(Payload);
impl Message for Data {
    type Result = u64;
}
struct Target<const BAD_METADATA: bool>;
impl<const B: bool> ParrotTypedDispatch for Target<B> {
    type Msg = Data;
    type Reply = u64;
    fn termination_protocol() -> Option<TerminationProtocol<Data>> {
        assert!(!B, "termination metadata callback panic");
        None
    }
    fn dispatch(&mut self, _: Data) -> BoxedFuture<'_, ActorResult<u64>> {
        unreachable!("input tests never execute a handler")
    }
}
impl<const B: bool> ParrotMsgVariant<Target<B>> for Data {
    fn inject(message: Self) -> Self {
        let Self(payload) = message;
        Self(payload)
    }
    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}
#[derive(Clone, Copy)]
enum Operation {
    Send,
    NoneTimeout,
    ExplicitTimeout,
    Deliver,
    Options,
    DeliverOptions,
}
impl Operation {
    fn call<'a>(
        self,
        target: &'a dyn ActorRef,
        message: BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        match self {
            Self::Send => target.send(message),
            Self::Options => target.send_with_options(message, Default::default()),
            Self::DeliverOptions => target
                .deliver_with_options(message, Default::default())
                .map(|result| result.map(|_| Box::new(()) as BoxedMessage))
                .boxed(),
            Self::NoneTimeout => target.send_with_timeout(message, None),
            Self::ExplicitTimeout => {
                target.send_with_timeout(message, Some(Duration::from_secs(1)))
            }
            Self::Deliver => target
                .deliver(message)
                .map(|result| result.map(|_| Box::new(()) as BoxedMessage))
                .boxed(),
        }
    }
}
#[derive(Clone, Copy)]
enum Boundary {
    Unpolled,
    WrongType,
    Disconnected,
}
async fn check(operation: Operation, boundary: Boundary) {
    let (sender, receiver) = typed_channel::<Target<false>>(1);
    let reference =
        TypedActorRef::<Target<false>, Data>::new(sender, "/bridge-input".into()).into_dyn();
    let drops = Arc::new(AtomicUsize::new(0));
    let payload = Payload(drops.clone());
    let message: BoxedMessage = if matches!(boundary, Boundary::WrongType) {
        Box::new(payload)
    } else {
        Box::new(Data(payload))
    };
    if matches!(boundary, Boundary::Disconnected) {
        drop(receiver);
    }
    let future = operation.call(reference.as_ref(), message);
    match boundary {
        Boundary::Unpolled => {
            let dropped = std::panic::catch_unwind(AssertUnwindSafe(|| drop(future)));
            assert!(
                dropped.is_ok(),
                "unpolled direct bridge future leaked message Drop panic"
            );
        }
        Boundary::WrongType | Boundary::Disconnected => {
            let result = AssertUnwindSafe(future).catch_unwind().await;
            assert!(
                result.is_ok(),
                "direct bridge poll leaked input retirement panic"
            );
            let error = result.unwrap().expect_err("rejected input cannot succeed");
            match boundary {
                Boundary::WrongType => assert!(
                    matches!(error, ActorError::MessageHandlingError(ref text) if text.contains("type mismatch")),
                    "{error:?}"
                ),
                Boundary::Disconnected => {
                    assert!(matches!(error, ActorError::Stopped), "{error:?}")
                }
                Boundary::Unpolled => unreachable!(),
            }
        }
    }
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "message retired more than once or leaked"
    );
}
macro_rules! contract {
    ($name:ident,$op:ident,$boundary:ident) => {
        #[tokio::test]
        async fn $name() {
            check(Operation::$op, Boundary::$boundary).await;
        }
    };
}
contract!(send_unpolled, Send, Unpolled);
contract!(none_timeout_unpolled, NoneTimeout, Unpolled);
contract!(explicit_timeout_unpolled, ExplicitTimeout, Unpolled);
contract!(deliver_unpolled, Deliver, Unpolled);
contract!(send_wrong_type, Send, WrongType);
contract!(none_timeout_wrong_type, NoneTimeout, WrongType);
contract!(explicit_timeout_wrong_type, ExplicitTimeout, WrongType);
contract!(deliver_wrong_type, Deliver, WrongType);
contract!(send_disconnected, Send, Disconnected);
contract!(none_timeout_disconnected, NoneTimeout, Disconnected);
contract!(explicit_timeout_disconnected, ExplicitTimeout, Disconnected);
contract!(deliver_disconnected, Deliver, Disconnected);

contract!(options_unpolled, Options, Unpolled);
contract!(deliver_options_unpolled, DeliverOptions, Unpolled);
contract!(options_wrong_type, Options, WrongType);
contract!(deliver_options_wrong_type, DeliverOptions, WrongType);
contract!(options_disconnected, Options, Disconnected);
contract!(deliver_options_disconnected, DeliverOptions, Disconnected);

#[test]
fn termination_metadata_panic_is_an_admission_error() {
    let (sender, _receiver) = typed_channel::<Target<true>>(1);
    let reference = TypedActorRef::<Target<true>, Data>::new(sender, "/metadata".into());
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        reference.install_termination_recipient()
    }));
    assert!(
        result.is_ok(),
        "termination callback unwound across admission"
    );
    assert!(
        matches!(result.unwrap(), Err(ActorError::Panic(ref message)) if message.contains("termination metadata callback panic"))
    );
}
