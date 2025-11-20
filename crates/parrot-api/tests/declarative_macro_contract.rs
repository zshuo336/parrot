//! Consumers need no implicit Message, Box, Result or Option imports.
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

mod consumer {
    use super::*;

    struct Query(u64);
    impl parrot_api::message::Message for Query {
        type Result = u64;
    }
    struct Text;
    impl parrot_api::message::Message for Text {
        type Result = String;
    }
    struct Counter(u64);
    impl Counter {
        fn query(&mut self, query: &Query) -> u64 {
            self.0 += query.0;
            self.0
        }
        async fn query_async(&mut self, query: &Query) -> u64 {
            self.query(query)
        }
        async fn text_async(&mut self, _: &Text) -> String {
            self.0.to_string()
        }
        fn sync(&mut self, msg: &BoxedMessage, mode: usize) -> ActorResult<BoxedMessage> {
            match mode {
                0 => parrot_api::match_message!(self, msg, Query => Self::query),
                1 => parrot_api::match_message!("result", self, msg, Query => Self::query),
                _ => parrot_api::match_message!("option", self, msg, Query => Self::query)
                    .expect("option dispatch always returns a result"),
            }
        }
        async fn asynchronous(
            &mut self,
            msg: &BoxedMessage,
            mode: usize,
        ) -> ActorResult<BoxedMessage> {
            match mode {
                0 => parrot_api::match_async_message!(self, msg, Query => Self::query_async),
                1 => parrot_api::match_async_message!("result", self, msg, Query => Self::query_async),
                _ => parrot_api::match_async_message!("option", self, msg, Query => Self::query_async)
                    .expect("option dispatch always returns a result"),
            }
        }
        fn deferred<'a>(
            &'a mut self,
            msg: &'a BoxedMessage,
        ) -> ActorResult<BoxedFuture<'a, ActorResult<BoxedMessage>>> {
            parrot_api::match_message!("async", self, msg,
                Query => Self::query_async,
                Text => Self::text_async,
            )
        }
    }

    #[test]
    fn response_macros_resolve_the_defining_message_trait() {
        let base: ActorResult<BoxedMessage> = parrot_api::message_response_ok!(Query, 42);
        let result: ActorResult<BoxedMessage> =
            parrot_api::message_response_ok!("result", Query, 43);
        let optional: Option<ActorResult<BoxedMessage>> =
            parrot_api::message_response_ok!("option", Query, 44);
        for (result, expected) in [(base, 42), (result, 43), (optional.unwrap(), 44)] {
            assert_eq!(*result.unwrap().downcast::<u64>().unwrap(), expected);
        }
    }

    #[test]
    fn sync_dispatch_checks_values_and_unknown_types() {
        let mut actor = Counter(0);
        let msg: BoxedMessage = Box::new(Query(5));
        let unknown: BoxedMessage = Box::new(());
        for mode in 0..3 {
            let result = actor.sync(&msg, mode).unwrap();
            assert_eq!(*result.downcast::<u64>().unwrap(), (mode as u64 + 1) * 5);
            assert!(matches!(
                actor.sync(&unknown, mode),
                Err(parrot_api::errors::ActorError::MessageHandlingError(_))
            ));
        }
        assert_eq!(actor.0, 15);
    }

    #[test]
    fn async_dispatch_returns_the_typed_result() {
        futures::executor::block_on(async {
            let mut actor = Counter(0);
            let msg: BoxedMessage = Box::new(Query(7));
            let unknown: BoxedMessage = Box::new(());
            for mode in 0..3 {
                let result = actor.asynchronous(&msg, mode).await.unwrap();
                assert_eq!(*result.downcast::<u64>().unwrap(), (mode as u64 + 1) * 7);
                assert!(matches!(
                    actor.asynchronous(&unknown, mode).await,
                    Err(parrot_api::errors::ActorError::MessageHandlingError(_))
                ));
            }
            assert_eq!(actor.0, 21);
        });
    }

    #[test]
    fn deferred_dispatch_selects_then_polls_one_handler() {
        let mut actor = Counter(0);
        let msg: BoxedMessage = Box::new(Query(9));
        drop(actor.deferred(&msg).unwrap());
        assert_eq!(actor.0, 0, "unpolled handler must not run");
        let result = futures::executor::block_on(actor.deferred(&msg).unwrap()).unwrap();
        assert_eq!(*result.downcast::<u64>().unwrap(), 9);
        let text: BoxedMessage = Box::new(Text);
        let result = futures::executor::block_on(actor.deferred(&text).unwrap()).unwrap();
        assert_eq!(*result.downcast::<String>().unwrap(), "9");
        let unknown: BoxedMessage = Box::new(());
        assert!(matches!(
            actor.deferred(&unknown),
            Err(parrot_api::errors::ActorError::MessageHandlingError(_))
        ));
    }

    #[test]
    fn response_macro_ignores_callers_with_conflicting_names() {
        struct Message;
        struct Box;
        struct Result;
        struct Option;
        mod std {}
        let _ = (Message, Box, Result, Option);
        let result: ActorResult<BoxedMessage> = parrot_api::message_response_ok!(Query, 11);
        assert_eq!(*result.unwrap().downcast::<u64>().unwrap(), 11);
    }
}
