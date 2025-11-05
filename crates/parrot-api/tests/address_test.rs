use async_trait::async_trait;
use parrot_api::address::{ActorPath, ActorRef, ActorRefExt};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::any::Any;
use std::fmt::Debug;
use std::sync::{Arc, RwLock};
use std::time::Duration;
// Test message types
#[derive(Debug, Clone)]
struct TestMessage(u32);

impl Message for TestMessage {
    type Result = u32;

    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        if let Ok(value) = result.downcast::<u32>() {
            Ok(*value)
        } else {
            Err(ActorError::MessageHandlingError(
                "Failed to extract result".to_string(),
            ))
        }
    }
}

// Test actor reference implementation for testing
#[derive(Debug)]
struct TestActorRef {
    id: String,
    counter: Arc<RwLock<u32>>,
    alive: Arc<RwLock<bool>>,
}

impl TestActorRef {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            counter: Arc::new(RwLock::new(0)),
            alive: Arc::new(RwLock::new(true)),
        }
    }
}

#[async_trait]
impl ActorRef for TestActorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if !*self.alive.read().unwrap() {
                return Err(ActorError::Stopped);
            }

            if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
                let val = test_msg.0;
                // Update counter
                {
                    let mut counter = self.counter.write().unwrap();
                    *counter += val;
                }

                // Return doubled value as response
                let response = val * 2;
                Ok(Box::new(response) as BoxedMessage)
            } else {
                // Echo back unknown messages
                Ok(msg)
            }
        })
    }

    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let result = self.send(msg).await;
            result
        })
    }

    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        // Mirror the send() side effects (counter update) so tell()-based
        // tests observe the same behavior as send().
        Box::pin(async move {
            if !*self.alive.read().unwrap() {
                return Err(ActorError::Stopped);
            }
            if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
                let val = test_msg.0;
                let mut counter = self.counter.write().unwrap();
                *counter += val;
            }
            Ok(())
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        let alive = self.alive.clone();
        Box::pin(async move {
            let mut state = alive.write().unwrap();
            *state = false;
            Ok(())
        })
    }

    fn path(&self) -> String {
        format!("local://test/{}", self.id)
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        let alive = self.alive.clone();
        Box::pin(async move { *alive.read().unwrap() })
    }

    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            id: self.id.clone(),
            counter: self.counter.clone(),
            alive: self.alive.clone(),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// Mock weak actor target for ActorPath testing
#[derive(Debug)]
struct MockWeakTarget {
    path_value: String,
}

#[async_trait]
impl ActorRef for MockWeakTarget {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(msg) })
    }

    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let result = self.send(msg).await;
            result
        })
    }

    fn deliver<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn path(&self) -> String {
        self.path_value.clone()
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move { true })
    }

    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            path_value: self.path_value.clone(),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime;

    fn create_mock_target(path: &str) -> Arc<dyn ActorRef> {
        Arc::new(MockWeakTarget {
            path_value: path.to_string(),
        })
    }

    // Test ActorPath creation and equality
    #[test]
    fn test_actor_path() {
        let target1 = create_mock_target("local://system/user/actor1");
        let target2 = create_mock_target("local://system/user/actor1");
        let target3 = create_mock_target("local://system/user/actor2");

        let path1 = ActorPath {
            target: target1.clone(),
            path: "local://system/user/actor1".to_string(),
        };

        let path2 = ActorPath {
            target: target2.clone(),
            path: "local://system/user/actor1".to_string(),
        };

        let path3 = ActorPath {
            target: target3.clone(),
            path: "local://system/user/actor2".to_string(),
        };

        // Test equality
        assert_eq!(path1, path2);
        assert_ne!(path1, path3);

        // Test display formatting
        assert_eq!(path1.to_string(), "local://system/user/actor1");

        // Test path method usage
        assert_eq!(path1.target.path(), "local://system/user/actor1");
    }

    // Test ActorRef send and receive
    #[test]
    fn test_actor_ref_send() {
        let rt = Runtime::new().unwrap();
        let actor = TestActorRef::new("test_actor");

        rt.block_on(async {
            // Send a message and verify response
            let msg = Box::new(TestMessage(5)) as BoxedMessage;
            let response = actor.send(msg).await.unwrap();

            // Check response
            let response_val = response.downcast::<u32>().unwrap();
            assert_eq!(*response_val, 10); // Should be doubled

            // Verify counter was updated
            assert_eq!(*actor.counter.read().unwrap(), 5);
        });
    }

    // Test ActorRefExt ask method
    #[test]
    fn test_actor_ref_ext_ask() {
        let rt = Runtime::new().unwrap();
        let actor = TestActorRef::new("test_actor");

        rt.block_on(async {
            // Send typed message using ask
            let msg = TestMessage(7);
            let response = actor.ask(msg).await.unwrap();

            // Verify response
            assert_eq!(response, 14); // Should be doubled

            // Verify counter was updated
            assert_eq!(*actor.counter.read().unwrap(), 7);
        });
    }

    // Test ActorRefExt tell method
    #[test]
    fn test_actor_ref_ext_tell() {
        let rt = Runtime::new().unwrap();
        let actor = TestActorRef::new("test_actor");

        rt.block_on(async {
            // Send message using tell (fire and forget)
            actor.tell(TestMessage(3)).await.unwrap();

            // Verify counter was updated
            assert_eq!(*actor.counter.read().unwrap(), 3);
        });
    }

    // Test actor stopping
    #[test]
    fn test_actor_stop() {
        let rt = Runtime::new().unwrap();
        let actor = TestActorRef::new("test_actor");

        // Actor should start alive
        assert!(rt.block_on(actor.is_alive()));

        // Stop the actor
        rt.block_on(async {
            let result = actor.stop().await;
            assert!(result.is_ok());

            // Verify actor is no longer alive
            let alive = actor.is_alive().await;
            assert!(!alive);

            // Sending a message to stopped actor should fail
            let msg = Box::new(TestMessage(10)) as BoxedMessage;
            let result = actor.send(msg).await;
            assert!(result.is_err());

            if let Err(error) = result {
                match error {
                    ActorError::Stopped => {
                        // This is the expected error
                    }
                    _ => {
                        panic!("Expected Stopped error");
                    }
                }
            }
        });
    }

    // Test cloning actor references
    #[test]
    fn test_actor_ref_clone() {
        let rt = Runtime::new().unwrap();
        let actor = TestActorRef::new("test_actor");

        // Clone the actor reference
        let cloned_actor = actor.clone_boxed();

        rt.block_on(async {
            // Send message to original
            actor.tell(TestMessage(5)).await.unwrap();

            // Send message to clone
            let msg = Box::new(TestMessage(7)) as BoxedMessage;
            let _ = cloned_actor.send(msg).await;

            // Verify both updated the same counter
            assert_eq!(*actor.counter.read().unwrap(), 12);

            // Stop through the clone
            let _ = cloned_actor.stop().await;

            // Verify original is also stopped
            assert!(!actor.is_alive().await);
        });
    }
}

mod metadata_ownership {
    use super::*;
    use futures::FutureExt;
    use parrot_api::{
        message::{MessageOptions, MessagePriority},
        typed::{ParrotMsgVariant, ParrotTypedDispatch},
        typed_channel::TypedActorRef,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Options,
        Priority,
        Recursive,
        Reject,
        Healthy,
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Api {
        Ask,
        Unbounded,
        Timeout,
        Tell,
        DynamicAsk,
        DynamicTell,
        Retrying,
    }
    struct Payload;
    impl Drop for Payload {
        fn drop(&mut self) {
            panic!("metadata panic payload disposal");
        }
    }
    #[derive(Debug, Default)]
    struct Counts {
        messages: AtomicUsize,
        options: AtomicUsize,
        priority: AtomicUsize,
        sends: AtomicUsize,
    }
    #[derive(Clone)]
    struct Input {
        mode: Mode,
        hostile: bool,
        counts: Arc<Counts>,
    }
    impl Drop for Input {
        fn drop(&mut self) {
            self.counts.messages.fetch_add(1, Ordering::SeqCst);
            assert!(!self.hostile, "later metadata message disposal");
        }
    }
    impl Message for Input {
        type Result = usize;
        fn message_options(&self) -> Option<MessageOptions> {
            self.counts.options.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                Mode::Options => panic!("first message_options failure"),
                Mode::Recursive => std::panic::panic_any(Payload),
                Mode::Priority => None,
                Mode::Reject => Some(MessageOptions {
                    priority: MessagePriority::new(70).unwrap(),
                    ..Default::default()
                }),
                Mode::Healthy => Some(MessageOptions::default()),
            }
        }
        fn priority(&self) -> MessagePriority {
            self.counts.priority.fetch_add(1, Ordering::SeqCst);
            panic!("first priority failure");
        }
    }
    struct Actor;
    impl ParrotTypedDispatch for Actor {
        type Msg = Input;
        type Reply = usize;
        fn dispatch(&mut self, _: Input) -> BoxedFuture<'_, ActorResult<usize>> {
            panic!("no actor worker in direct admission contract")
        }
    }
    impl ParrotMsgVariant<Actor> for Input {
        fn inject(value: Self) -> Self {
            value
        }
        fn extract(value: usize) -> ActorResult<usize> {
            Ok(value)
        }
    }
    #[derive(Debug, Clone)]
    struct Target(Arc<Counts>);
    #[async_trait]
    impl ActorRef for Target {
        fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                self.0.sends.fetch_add(1, Ordering::SeqCst);
                drop(msg);
                Err(ActorError::Stopped)
            })
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(msg)
        }
        fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.0.sends.fetch_add(1, Ordering::SeqCst);
                drop(msg);
                Err(ActorError::Stopped)
            })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            "/metadata".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { false })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(self.clone())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    async fn exercise(api: Api, mode: Mode, hostile: bool) {
        let counts = Arc::new(Counts::default());
        let input = Input {
            mode,
            hostile,
            counts: counts.clone(),
        };
        let (tx, rx) = parrot_api::typed_channel::typed_channel::<Actor>(1);
        drop(rx);
        let typed = TypedActorRef::<Actor, Input>::new(tx, "/metadata".into());
        let target = Target(counts.clone());
        let observed = std::panic::AssertUnwindSafe(async {
            match api {
                Api::Ask => typed.ask(input).await.map(|_| ()),
                Api::Unbounded => typed.ask_unbounded(input).await.map(|_| ()),
                Api::Timeout => typed
                    .ask_with_timeout(input, Duration::from_secs(1))
                    .await
                    .map(|_| ()),
                Api::Tell => typed.tell(input).await,
                Api::DynamicAsk => ActorRefExt::ask(&target, input).await.map(|_| ()),
                Api::DynamicTell => ActorRefExt::tell(&target, input).await,
                Api::Retrying => ActorRefExt::ask_retrying(&target, input, |_| false)
                    .await
                    .map(|_| ()),
            }
        })
        .catch_unwind()
        .await;
        let result = match observed {
            Ok(result) => result,
            Err(payload) => {
                if let Err(second) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
                {
                    std::mem::forget(second);
                }
                panic!("message metadata or input disposal escaped its public API boundary");
            }
        };
        match mode {
            Mode::Healthy => assert!(
                matches!(result, Err(ActorError::Stopped)),
                "healthy metadata changed delivery: {result:?}"
            ),
            Mode::Reject => assert!(
                matches!(result, Err(ActorError::MessageHandlingError(_))),
                "rejection changed: {result:?}"
            ),
            _ => {
                let expected = match mode {
                    Mode::Options => "first message_options failure",
                    Mode::Priority => "first priority failure",
                    Mode::Recursive => "panic payload destructor panicked",
                    _ => unreachable!(),
                };
                assert!(
                    matches!(result,Err(ActorError::Panic(ref text)) if text.contains(expected)),
                    "metadata first failure lost: {result:?}"
                );
            }
        }
        assert_eq!(
            counts.options.load(Ordering::SeqCst),
            1,
            "metadata replayed"
        );
        assert_eq!(
            counts.priority.load(Ordering::SeqCst),
            usize::from(mode == Mode::Priority)
        );
        let retried_copy = api == Api::Retrying && matches!(mode, Mode::Healthy | Mode::Reject);
        assert_eq!(
            counts.messages.load(Ordering::SeqCst),
            1 + usize::from(retried_copy),
            "input leak or duplicate disposal"
        );
        let delivered = matches!(api, Api::DynamicAsk | Api::DynamicTell | Api::Retrying)
            && mode == Mode::Healthy;
        assert_eq!(
            counts.sends.load(Ordering::SeqCst),
            usize::from(delivered),
            "failed metadata must not enter target"
        );
    }
    fn isolated(name: &str, body: impl FnOnce()) {
        const KEY: &str = "PARROT_DIRECT_METADATA_CASE";
        if std::env::var(KEY).as_deref() == Ok(name) {
            body();
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(KEY, name)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{name}: {status}");
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{name}: deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    macro_rules! case {
        ($name:ident,$api:ident,$mode:ident,$drop:expr) => {
            #[test]
            fn $name() {
                isolated(
                    &format!("metadata_ownership::{}", stringify!($name)),
                    || {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap()
                            .block_on(exercise(Api::$api, Mode::$mode, $drop));
                    },
                );
            }
        };
    }
    case!(ask_options_plain, Ask, Options, false);
    case!(ask_options_hostile, Ask, Options, true);
    case!(ask_priority_plain, Ask, Priority, false);
    case!(ask_priority_hostile, Ask, Priority, true);
    case!(ask_recursive_plain, Ask, Recursive, false);
    case!(ask_recursive_hostile, Ask, Recursive, true);
    case!(ask_reject_plain, Ask, Reject, false);
    case!(ask_healthy_plain, Ask, Healthy, false);
    case!(unbounded_options_plain, Unbounded, Options, false);
    case!(unbounded_options_hostile, Unbounded, Options, true);
    case!(unbounded_priority_plain, Unbounded, Priority, false);
    case!(unbounded_priority_hostile, Unbounded, Priority, true);
    case!(unbounded_recursive_plain, Unbounded, Recursive, false);
    case!(unbounded_recursive_hostile, Unbounded, Recursive, true);
    case!(unbounded_reject_plain, Unbounded, Reject, false);
    case!(unbounded_healthy_plain, Unbounded, Healthy, false);
    case!(timeout_options_plain, Timeout, Options, false);
    case!(timeout_options_hostile, Timeout, Options, true);
    case!(timeout_priority_plain, Timeout, Priority, false);
    case!(timeout_priority_hostile, Timeout, Priority, true);
    case!(timeout_recursive_plain, Timeout, Recursive, false);
    case!(timeout_recursive_hostile, Timeout, Recursive, true);
    case!(timeout_reject_plain, Timeout, Reject, false);
    case!(timeout_healthy_plain, Timeout, Healthy, false);
    case!(tell_options_plain, Tell, Options, false);
    case!(tell_options_hostile, Tell, Options, true);
    case!(tell_priority_plain, Tell, Priority, false);
    case!(tell_priority_hostile, Tell, Priority, true);
    case!(tell_recursive_plain, Tell, Recursive, false);
    case!(tell_recursive_hostile, Tell, Recursive, true);
    case!(tell_reject_plain, Tell, Reject, false);
    case!(tell_healthy_plain, Tell, Healthy, false);
    case!(dynamicask_options_plain, DynamicAsk, Options, false);
    case!(dynamicask_options_hostile, DynamicAsk, Options, true);
    case!(dynamicask_priority_plain, DynamicAsk, Priority, false);
    case!(dynamicask_priority_hostile, DynamicAsk, Priority, true);
    case!(dynamicask_recursive_plain, DynamicAsk, Recursive, false);
    case!(dynamicask_recursive_hostile, DynamicAsk, Recursive, true);
    case!(dynamicask_reject_plain, DynamicAsk, Reject, false);
    case!(dynamicask_healthy_plain, DynamicAsk, Healthy, false);
    case!(dynamictell_options_plain, DynamicTell, Options, false);
    case!(dynamictell_options_hostile, DynamicTell, Options, true);
    case!(dynamictell_priority_plain, DynamicTell, Priority, false);
    case!(dynamictell_priority_hostile, DynamicTell, Priority, true);
    case!(dynamictell_recursive_plain, DynamicTell, Recursive, false);
    case!(dynamictell_recursive_hostile, DynamicTell, Recursive, true);
    case!(dynamictell_reject_plain, DynamicTell, Reject, false);
    case!(dynamictell_healthy_plain, DynamicTell, Healthy, false);
    case!(retrying_options_plain, Retrying, Options, false);
    case!(retrying_options_hostile, Retrying, Options, true);
    case!(retrying_priority_plain, Retrying, Priority, false);
    case!(retrying_priority_hostile, Retrying, Priority, true);
    case!(retrying_recursive_plain, Retrying, Recursive, false);
    case!(retrying_recursive_hostile, Retrying, Recursive, true);
    case!(retrying_reject_plain, Retrying, Reject, false);
    case!(retrying_healthy_plain, Retrying, Healthy, false);
}

mod input_composition {
    use super::*;
    use parrot_api::{
        message::{BackoffStrategy, MessageOptions, RetryPolicy},
        typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedAskRef},
        typed_channel::TypedActorRef,
    };
    use std::{
        cell::Cell,
        sync::atomic::{AtomicUsize, Ordering},
    };
    #[derive(Debug, Default)]
    struct State {
        messages: AtomicUsize,
        options: AtomicUsize,
        message_clones: AtomicUsize,
        targets: AtomicUsize,
        target_clones: AtomicUsize,
        sends: AtomicUsize,
        predicates: AtomicUsize,
        captures: AtomicUsize,
        errors: AtomicUsize,
        replies: AtomicUsize,
    }
    struct Recursive;
    impl Drop for Recursive {
        fn drop(&mut self) {
            panic!("clone panic payload disposal");
        }
    }
    struct Input {
        state: Arc<State>,
        drop_bomb: bool,
        clone_bomb: bool,
        metadata_bomb: bool,
        retry: bool,
    }
    impl Clone for Input {
        fn clone(&self) -> Self {
            self.state.message_clones.fetch_add(1, Ordering::SeqCst);
            assert!(!self.clone_bomb, "first message clone failure");
            Self {
                state: self.state.clone(),
                drop_bomb: false,
                clone_bomb: false,
                metadata_bomb: false,
                retry: self.retry,
            }
        }
    }
    impl Drop for Input {
        fn drop(&mut self) {
            self.state.messages.fetch_add(1, Ordering::SeqCst);
            assert!(!self.drop_bomb, "first original message disposal");
        }
    }
    #[derive(Debug)]
    struct Reply {
        state: Arc<State>,
        hostile: bool,
    }
    impl Drop for Reply {
        fn drop(&mut self) {
            self.state.replies.fetch_add(1, Ordering::SeqCst);
            assert!(!self.hostile, "later suppressed reply disposal");
        }
    }
    impl Message for Input {
        type Result = Reply;
        fn message_options(&self) -> Option<MessageOptions> {
            self.state.options.fetch_add(1, Ordering::SeqCst);
            assert!(!self.metadata_bomb, "first composition metadata failure");
            Some(MessageOptions {
                retry_policy: self.retry.then_some(RetryPolicy {
                    max_attempts: 1,
                    retry_interval: Duration::from_millis(1),
                    backoff_strategy: BackoffStrategy::Fixed,
                }),
                ..Default::default()
            })
        }
    }
    struct Actor;
    impl ParrotTypedDispatch for Actor {
        type Msg = Input;
        type Reply = Reply;
        fn dispatch(&mut self, _: Input) -> BoxedFuture<'_, ActorResult<Reply>> {
            panic!("no worker in unpolled contract")
        }
    }
    impl ParrotMsgVariant<Actor> for Input {
        fn inject(value: Self) -> Self {
            value
        }
        fn extract(value: Reply) -> ActorResult<Reply> {
            Ok(value)
        }
    }
    #[derive(Debug)]
    struct Source(Arc<State>);
    impl std::fmt::Display for Source {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("original retry failure")
        }
    }
    impl std::error::Error for Source {}
    impl Drop for Source {
        fn drop(&mut self) {
            self.0.errors.fetch_add(1, Ordering::SeqCst);
            panic!("first retry error disposal");
        }
    }
    #[derive(Debug)]
    struct Target {
        state: Arc<State>,
        owned: bool,
        drop_bomb: bool,
        clone_bomb: u8,
        fail_first: bool,
        error_bomb: bool,
        reply_bomb: bool,
    }
    impl Drop for Target {
        fn drop(&mut self) {
            if self.owned {
                self.state.targets.fetch_add(1, Ordering::SeqCst);
                assert!(!self.drop_bomb, "first cloned target disposal");
            }
        }
    }
    #[async_trait]
    impl ActorRef for Target {
        fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                let attempt = self.state.sends.fetch_add(1, Ordering::SeqCst);
                drop(msg);
                if self.fail_first && attempt == 0 {
                    if self.error_bomb {
                        return Err(ActorError::Other(anyhow::Error::new(Source(
                            self.state.clone(),
                        ))));
                    }
                    return Err(ActorError::MessageHandlingError(
                        "original business failure".into(),
                    ));
                }
                Ok(Box::new(Reply {
                    state: self.state.clone(),
                    hostile: self.reply_bomb,
                }) as BoxedMessage)
            })
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(msg)
        }
        fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.state.sends.fetch_add(1, Ordering::SeqCst);
                drop(msg);
                Ok(())
            })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            "/composition".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            self.state.target_clones.fetch_add(1, Ordering::SeqCst);
            match self.clone_bomb {
                1 => panic!("first target clone failure"),
                2 => std::panic::panic_any(Recursive),
                _ => {}
            }
            Box::new(Self {
                state: self.state.clone(),
                owned: true,
                drop_bomb: self.drop_bomb,
                clone_bomb: 0,
                fail_first: self.fail_first,
                error_bomb: self.error_bomb,
                reply_bomb: self.reply_bomb,
            })
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    struct Capture {
        state: Arc<State>,
        calls: Cell<usize>,
        drop_bomb: bool,
        panic: bool,
        retry: bool,
    }
    impl Capture {
        fn decide(&self, _: &ActorError) -> bool {
            self.calls.set(self.calls.get() + 1);
            self.state.predicates.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "first retry predicate failure");
            self.retry
        }
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            self.state.captures.fetch_add(1, Ordering::SeqCst);
            assert!(!self.drop_bomb, "first retry capture disposal");
        }
    }
    fn input(state: &Arc<State>, hostile: bool, retry: bool) -> Input {
        Input {
            state: state.clone(),
            drop_bomb: hostile,
            clone_bomb: false,
            metadata_bomb: false,
            retry,
        }
    }
    fn target(state: &Arc<State>) -> Target {
        Target {
            state: state.clone(),
            owned: false,
            drop_bomb: false,
            clone_bomb: 0,
            fail_first: false,
            error_bomb: false,
            reply_bomb: false,
        }
    }
    fn capture(state: &Arc<State>, hostile: bool) -> Capture {
        Capture {
            state: state.clone(),
            calls: Cell::new(0),
            drop_bomb: hostile,
            panic: false,
            retry: true,
        }
    }
    fn discard<T>(value: T) -> bool {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value))) {
            Ok(()) => false,
            Err(payload) => {
                if let Err(second) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
                {
                    std::mem::forget(second);
                }
                true
            }
        }
    }
    fn panic_error(result: ActorResult<Reply>, expected: &str) {
        let error = match result {
            Err(error) => error,
            Ok(reply) => {
                discard(reply);
                panic!("unexpected successful reply");
            }
        };
        assert!(
            matches!(error,ActorError::Panic(ref text) if text.contains(expected)),
            "wrong primary error: {error:?}"
        );
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Api {
        Ask,
        Unbounded,
        Timeout,
        Tell,
        TraitAsk,
        TraitTell,
        DynamicAsk,
        DynamicTell,
        Retrying,
    }
    fn unpolled(api: Api, hostile: bool) {
        let state = Arc::new(State::default());
        let msg = input(&state, hostile, false);
        let (tx, rx) = parrot_api::typed_channel::typed_channel::<Actor>(1);
        let typed = TypedActorRef::<Actor, Input>::new(tx, "/unpolled".into());
        let mut reference = target(&state);
        reference.drop_bomb = hostile;
        let constructed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let future: BoxedFuture<'_, ActorResult<Reply>> = match api {
                Api::Ask => Box::pin(typed.ask(msg)),
                Api::Unbounded => Box::pin(typed.ask_unbounded(msg)),
                Api::Timeout => Box::pin(typed.ask_with_timeout(msg, Duration::from_secs(1))),
                Api::Tell => {
                    let future = typed.tell(msg);
                    Box::pin(async move {
                        future.await?;
                        unreachable!()
                    })
                }
                Api::TraitAsk => TypedAskRef::ask(&typed, msg),
                Api::TraitTell => {
                    let future = TypedAskRef::tell(&typed, msg);
                    Box::pin(async move {
                        future.await?;
                        unreachable!()
                    })
                }
                Api::DynamicAsk => ActorRefExt::ask(&reference, msg),
                Api::DynamicTell => {
                    let future = ActorRefExt::tell(&reference, msg);
                    Box::pin(async move {
                        future.await?;
                        unreachable!()
                    })
                }
                Api::Retrying => {
                    let predicate = capture(&state, hostile);
                    ActorRefExt::ask_retrying(&reference, msg, move |error| predicate.decide(error))
                }
            };
            let eager = matches!(api, Api::DynamicAsk | Api::DynamicTell);
            assert_eq!(
                state.options.load(Ordering::SeqCst),
                usize::from(eager),
                "metadata callback timing changed"
            );
            future
        }));
        let future = constructed.expect("unpolled future construction escaped");
        assert!(
            !discard(future),
            "abandoned input disposal escaped API future"
        );
        assert!(rx.is_empty());
        assert_eq!(state.sends.load(Ordering::SeqCst), 0);
        assert_eq!(state.messages.load(Ordering::SeqCst), 1);
        assert_eq!(
            state.targets.load(Ordering::SeqCst),
            usize::from(api == Api::DynamicAsk)
        );
        assert_eq!(
            state.captures.load(Ordering::SeqCst),
            usize::from(api == Api::Retrying)
        );
        assert_eq!(state.message_clones.load(Ordering::SeqCst), 0);
    }
    async fn clone_failure(recursive: bool) {
        let state = Arc::new(State::default());
        let msg = input(&state, true, false);
        let mut reference = target(&state);
        reference.clone_bomb = if recursive { 2 } else { 1 };
        let result = ActorRefExt::ask(&reference, msg).await;
        panic_error(
            result,
            if recursive {
                "panic payload destructor panicked"
            } else {
                "first target clone failure"
            },
        );
        assert_eq!(state.options.load(Ordering::SeqCst), 0);
        assert_eq!(state.messages.load(Ordering::SeqCst), 1);
        assert_eq!(state.sends.load(Ordering::SeqCst), 0);
    }
    async fn metadata_with_clone() {
        let state = Arc::new(State::default());
        let mut msg = input(&state, true, false);
        msg.metadata_bomb = true;
        let mut reference = target(&state);
        reference.drop_bomb = true;
        panic_error(
            ActorRefExt::ask(&reference, msg).await,
            "first composition metadata failure",
        );
        assert_eq!(state.messages.load(Ordering::SeqCst), 1);
        assert_eq!(state.targets.load(Ordering::SeqCst), 1);
        assert_eq!(state.sends.load(Ordering::SeqCst), 0);
    }
    async fn dynamic_response(clone_bomb: bool, reply_bomb: bool, business: bool) {
        let state = Arc::new(State::default());
        let msg = input(&state, false, false);
        let mut reference = target(&state);
        reference.drop_bomb = clone_bomb;
        reference.reply_bomb = reply_bomb;
        reference.fail_first = business;
        let result = ActorRefExt::ask(&reference, msg).await;
        if business {
            assert!(
                matches!(result,Err(ActorError::MessageHandlingError(ref text)) if text=="original business failure")
            );
        } else if clone_bomb {
            panic_error(result, "first cloned target disposal");
            assert_eq!(state.replies.load(Ordering::SeqCst), 1);
        } else {
            let reply = result.unwrap();
            assert_eq!(state.replies.load(Ordering::SeqCst), 0);
            drop(reply);
            assert_eq!(state.replies.load(Ordering::SeqCst), 1);
        }
        assert_eq!(state.messages.load(Ordering::SeqCst), 1);
        assert_eq!(state.targets.load(Ordering::SeqCst), 1);
        assert_eq!(state.sends.load(Ordering::SeqCst), 1);
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RetryCase {
        Clone,
        Predicate,
        Refuse,
        ErrorDrop,
        MessageDrop,
        CaptureDrop,
        Healthy,
    }
    async fn retry(case: RetryCase) {
        let state = Arc::new(State::default());
        let mut msg = input(
            &state,
            matches!(
                case,
                RetryCase::Clone
                    | RetryCase::Predicate
                    | RetryCase::Refuse
                    | RetryCase::MessageDrop
            ),
            true,
        );
        msg.clone_bomb = case == RetryCase::Clone;
        let mut reference = target(&state);
        reference.fail_first = true;
        reference.error_bomb = matches!(case, RetryCase::Predicate | RetryCase::ErrorDrop);
        reference.reply_bomb = matches!(case, RetryCase::MessageDrop | RetryCase::CaptureDrop);
        let mut predicate = capture(
            &state,
            matches!(
                case,
                RetryCase::Clone
                    | RetryCase::Predicate
                    | RetryCase::Refuse
                    | RetryCase::MessageDrop
                    | RetryCase::CaptureDrop
            ),
        );
        predicate.panic = case == RetryCase::Predicate;
        predicate.retry = case != RetryCase::Refuse;
        // Capture contains Cell: it is Send but not Sync. The existing public
        // contract must continue to accept this Fn and return a Send future.
        let future: BoxedFuture<'_, ActorResult<Reply>> =
            ActorRefExt::ask_retrying(&reference, msg, move |error| predicate.decide(error));
        let result = future.await;
        let success = case == RetryCase::Healthy;
        match case {
            RetryCase::Clone => panic_error(result, "first message clone failure"),
            RetryCase::Predicate => panic_error(result, "first retry predicate failure"),
            RetryCase::Refuse => assert!(
                matches!(result,Err(ActorError::MessageHandlingError(ref text)) if text=="original business failure")
            ),
            RetryCase::ErrorDrop => panic_error(result, "first retry error disposal"),
            RetryCase::MessageDrop => panic_error(result, "first original message disposal"),
            RetryCase::CaptureDrop => panic_error(result, "first retry capture disposal"),
            RetryCase::Healthy => {
                let reply = result.unwrap();
                assert_eq!(state.replies.load(Ordering::SeqCst), 0);
                drop(reply);
            }
        }
        let attempts = match case {
            RetryCase::Clone => 0,
            RetryCase::Predicate | RetryCase::Refuse | RetryCase::ErrorDrop => 1,
            _ => 2,
        };
        assert_eq!(state.sends.load(Ordering::SeqCst), attempts);
        assert_eq!(state.messages.load(Ordering::SeqCst), attempts + 1);
        assert_eq!(
            state.message_clones.load(Ordering::SeqCst),
            attempts + usize::from(case == RetryCase::Clone)
        );
        assert_eq!(state.options.load(Ordering::SeqCst), 1);
        assert_eq!(state.captures.load(Ordering::SeqCst), 1);
        assert_eq!(
            state.predicates.load(Ordering::SeqCst),
            usize::from(case != RetryCase::Clone)
        );
        assert_eq!(
            state.errors.load(Ordering::SeqCst),
            usize::from(reference.error_bomb)
        );
        assert_eq!(
            state.replies.load(Ordering::SeqCst),
            usize::from(success || matches!(case, RetryCase::MessageDrop | RetryCase::CaptureDrop))
        );
    }
    fn isolated(name: &str, body: impl FnOnce()) {
        const KEY: &str = "PARROT_MESSAGE_COMPOSITION_CASE";
        if std::env::var(KEY).as_deref() == Ok(name) {
            body();
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(KEY, name)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{name}: {status}");
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{name}: deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    macro_rules! case {
        ($name:ident,$body:expr) => {
            #[test]
            fn $name() {
                isolated(&format!("input_composition::{}", stringify!($name)), || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(async { $body });
                });
            }
        };
    }
    case!(unpolled_ask_healthy, unpolled(Api::Ask, false));
    case!(unpolled_ask_hostile, unpolled(Api::Ask, true));
    case!(unpolled_unbounded_healthy, unpolled(Api::Unbounded, false));
    case!(unpolled_unbounded_hostile, unpolled(Api::Unbounded, true));
    case!(unpolled_timeout_healthy, unpolled(Api::Timeout, false));
    case!(unpolled_timeout_hostile, unpolled(Api::Timeout, true));
    case!(unpolled_tell_healthy, unpolled(Api::Tell, false));
    case!(unpolled_tell_hostile, unpolled(Api::Tell, true));
    case!(unpolled_traitask_healthy, unpolled(Api::TraitAsk, false));
    case!(unpolled_traitask_hostile, unpolled(Api::TraitAsk, true));
    case!(unpolled_traittell_healthy, unpolled(Api::TraitTell, false));
    case!(unpolled_traittell_hostile, unpolled(Api::TraitTell, true));
    case!(
        unpolled_dynamicask_healthy,
        unpolled(Api::DynamicAsk, false)
    );
    case!(unpolled_dynamicask_hostile, unpolled(Api::DynamicAsk, true));
    case!(
        unpolled_dynamictell_healthy,
        unpolled(Api::DynamicTell, false)
    );
    case!(
        unpolled_dynamictell_hostile,
        unpolled(Api::DynamicTell, true)
    );
    case!(unpolled_retrying_healthy, unpolled(Api::Retrying, false));
    case!(unpolled_retrying_hostile, unpolled(Api::Retrying, true));
    case!(clone_callback_panic, clone_failure(false).await);
    case!(clone_callback_recursive_panic, clone_failure(true).await);
    case!(
        metadata_and_two_owned_destructors,
        metadata_with_clone().await
    );
    case!(
        dynamic_healthy_reply,
        dynamic_response(false, false, false).await
    );
    case!(
        dynamic_clone_disposal,
        dynamic_response(true, false, false).await
    );
    case!(
        dynamic_clone_and_reply_disposal,
        dynamic_response(true, true, false).await
    );
    case!(
        dynamic_business_error_survives_clone_disposal,
        dynamic_response(true, false, true).await
    );
    case!(retry_clone, retry(RetryCase::Clone).await);
    case!(retry_predicate, retry(RetryCase::Predicate).await);
    case!(retry_refuse, retry(RetryCase::Refuse).await);
    case!(retry_errordrop, retry(RetryCase::ErrorDrop).await);
    case!(retry_messagedrop, retry(RetryCase::MessageDrop).await);
    case!(retry_capturedrop, retry(RetryCase::CaptureDrop).await);
    case!(retry_healthy, retry(RetryCase::Healthy).await);
}

mod transport_ownership {
    use super::*;
    use futures::FutureExt;
    use parrot_api::{
        message::MessageOptions,
        typed::{ParrotMsgVariant, ParrotTypedDispatch},
        typed_channel::{TypedActorLifecycle, TypedActorRef, TypedChannelState, TypedEnvelope},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Default)]
    struct Counts {
        messages: AtomicUsize,
        replies: AtomicUsize,
        metadata: AtomicUsize,
        injects: AtomicUsize,
        extracts: AtomicUsize,
    }
    struct Input {
        counts: Arc<Counts>,
        hostile: bool,
        timeout: bool,
    }
    impl Drop for Input {
        fn drop(&mut self) {
            self.counts.messages.fetch_add(1, Ordering::SeqCst);
            assert!(!self.hostile, "transport message disposal");
        }
    }
    struct Reply {
        counts: Arc<Counts>,
        hostile: bool,
    }
    impl Drop for Reply {
        fn drop(&mut self) {
            self.counts.replies.fetch_add(1, Ordering::SeqCst);
            assert!(!self.hostile, "transport reply disposal");
        }
    }
    impl Message for Input {
        type Result = Reply;
        fn message_options(&self) -> Option<MessageOptions> {
            self.counts.metadata.fetch_add(1, Ordering::SeqCst);
            Some(MessageOptions {
                timeout: self.timeout.then_some(Duration::from_millis(10)),
                ..Default::default()
            })
        }
    }
    struct Actor;
    impl ParrotTypedDispatch for Actor {
        type Msg = Input;
        type Reply = Reply;
        fn dispatch(&mut self, _: Input) -> BoxedFuture<'_, ActorResult<Reply>> {
            panic!("no worker in transport ownership contract")
        }
    }
    impl ParrotMsgVariant<Actor> for Input {
        fn inject(value: Self) -> Self {
            value.counts.injects.fetch_add(1, Ordering::SeqCst);
            value
        }
        fn extract(value: Reply) -> ActorResult<Reply> {
            value.counts.extracts.fetch_add(1, Ordering::SeqCst);
            Ok(value)
        }
    }
    #[derive(Default)]
    struct Lifecycle(TypedChannelState);
    impl TypedActorLifecycle for Lifecycle {
        fn channel(&self) -> &TypedChannelState {
            &self.0
        }
        fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async {
                self.0.seal();
                Ok(())
            })
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Api {
        Ask,
        Unbounded,
        Explicit,
        Tell,
        BridgeAsk,
        BridgeTell,
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Case {
        Disconnected,
        Sealed,
        CancelFull,
        TimeoutFull,
        CancelReply,
        Success,
        SealedReply,
    }
    async fn exercise(api: Api, case: Case, managed: bool, hostile: bool) {
        let counts = Arc::new(Counts::default());
        let filler_counts = Arc::new(Counts::default());
        let (tx, receiver) = parrot_api::typed_channel::typed_channel::<Actor>(1);
        let full = matches!(case, Case::CancelFull | Case::TimeoutFull);
        if full {
            tx.try_send(TypedEnvelope {
                msg: Input {
                    counts: filler_counts.clone(),
                    hostile: false,
                    timeout: false,
                },
                reply: None,
                trace_context: None,
            })
            .unwrap_or_else(|_| panic!("fill queue"));
        }
        let lifecycle = Arc::new(Lifecycle::default());
        if case == Case::Sealed {
            lifecycle.0.seal();
        }
        let typed = TypedActorRef::<Actor, Input>::new(tx, "/transport".into());
        let typed = if managed {
            typed.with_lifecycle(lifecycle.clone(), Duration::from_millis(10))
        } else {
            typed
        };
        let bridge = typed.clone().into_dyn();
        let mut rx = Some(receiver);
        if case == Case::Disconnected {
            drop(rx.take());
        }
        let reply_stage = matches!(case, Case::CancelReply | Case::Success | Case::SealedReply);
        let input = Input {
            counts: counts.clone(),
            hostile: hostile && !reply_stage,
            timeout: case == Case::TimeoutFull,
        };
        let future: BoxedFuture<'_, ActorResult<Reply>> = match api {
            Api::Ask => Box::pin(typed.ask(input)),
            Api::Unbounded => Box::pin(typed.ask_unbounded(input)),
            Api::Explicit => Box::pin(typed.ask_with_timeout(input, Duration::from_millis(10))),
            Api::Tell => Box::pin(async {
                typed.tell(input).await?;
                panic!("tell unexpectedly admitted")
            }),
            Api::BridgeAsk => Box::pin(async {
                let result = bridge
                    .send_with_timeout(Box::new(input), Some(Duration::from_millis(10)))
                    .await?;
                Ok(*result
                    .downcast::<Reply>()
                    .unwrap_or_else(|_| panic!("reply type")))
            }),
            Api::BridgeTell => Box::pin(async {
                bridge.deliver(Box::new(input)).await?;
                panic!("tell unexpectedly admitted")
            }),
        };
        let mut future = Some(future);
        if full || reply_stage {
            assert!(
                futures::poll!(future.as_mut().unwrap()).is_pending(),
                "must reach transport wait"
            );
        }
        if reply_stage {
            let envelope = rx
                .as_ref()
                .unwrap()
                .try_recv()
                .unwrap_or_else(|_| panic!("message missing"));
            drop(envelope.msg);
            let reply = Reply {
                counts: counts.clone(),
                hostile,
            };
            envelope
                .reply
                .as_ref()
                .unwrap()
                .try_send(Ok(reply))
                .unwrap_or_else(|_| panic!("reply queue unexpectedly closed"));
            drop(envelope.reply);
            if case == Case::SealedReply {
                lifecycle.0.seal();
            }
        }
        if matches!(case, Case::CancelFull | Case::CancelReply) {
            let dropped =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future.take())));
            assert!(
                dropped.is_ok(),
                "public cancellation leaked a transport destructor panic"
            );
        } else {
            let observed = std::panic::AssertUnwindSafe(future.take().unwrap())
                .catch_unwind()
                .await;
            let result =
                observed.unwrap_or_else(|_| panic!("transport cleanup escaped public API"));
            match case {
                Case::Disconnected | Case::Sealed => assert!(
                    matches!(result, Err(ActorError::Stopped)),
                    "admission failure was replaced"
                ),
                Case::TimeoutFull => assert!(
                    matches!(
                        result,
                        Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
                    ),
                    "deadline was replaced"
                ),
                Case::Success | Case::SealedReply => {
                    let reply = result.unwrap_or_else(|_| panic!("reply not delivered"));
                    assert_eq!(
                        counts.replies.load(Ordering::SeqCst),
                        0,
                        "successful output disposed by API"
                    );
                    let disposed =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(reply)));
                    assert_eq!(
                        disposed.is_err(),
                        hostile,
                        "successful output belongs to caller"
                    );
                }
                _ => unreachable!(),
            }
        }
        assert_eq!(
            counts.messages.load(Ordering::SeqCst),
            1,
            "message leak or double drop"
        );
        assert_eq!(
            counts.replies.load(Ordering::SeqCst),
            usize::from(reply_stage)
        );
        assert_eq!(
            counts.injects.load(Ordering::SeqCst),
            1,
            "transport changed inject count"
        );
        assert_eq!(
            counts.extracts.load(Ordering::SeqCst),
            usize::from(matches!(case, Case::Success | Case::SealedReply))
        );
        assert_eq!(
            counts.metadata.load(Ordering::SeqCst),
            usize::from(!matches!(api, Api::BridgeAsk | Api::BridgeTell))
        );
        if full {
            let receiver = rx.as_ref().unwrap();
            assert_eq!(
                receiver.len(),
                1,
                "cancelled or timed out message entered queue"
            );
            let filler = receiver
                .try_recv()
                .unwrap_or_else(|_| panic!("filler lost"));
            drop(filler);
            assert!(receiver.is_empty());
            assert_eq!(filler_counts.messages.load(Ordering::SeqCst), 1);
        }
    }
    fn isolated(name: &str, body: impl FnOnce()) {
        const KEY: &str = "PARROT_TRANSPORT_OWNERSHIP_CASE";
        if std::env::var(KEY).as_deref() == Ok(name) {
            body();
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(KEY, name)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{name}: {status}");
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{name}: deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    macro_rules! case {
        ($name:ident, $api:ident, $case:ident, $managed:expr, $hostile:expr) => {
            #[test]
            fn $name() {
                isolated(
                    &format!("transport_ownership::{}", stringify!($name)),
                    || {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap()
                            .block_on(exercise(Api::$api, Case::$case, $managed, $hostile));
                    },
                );
            }
        };
    }
    case!(
        ask_disconnected_raw_healthy,
        Ask,
        Disconnected,
        false,
        false
    );
    case!(ask_disconnected_raw_hostile, Ask, Disconnected, false, true);
    case!(ask_cancelfull_raw_healthy, Ask, CancelFull, false, false);
    case!(ask_cancelfull_raw_hostile, Ask, CancelFull, false, true);
    case!(ask_timeoutfull_raw_healthy, Ask, TimeoutFull, false, false);
    case!(ask_timeoutfull_raw_hostile, Ask, TimeoutFull, false, true);
    case!(ask_cancelreply_raw_healthy, Ask, CancelReply, false, false);
    case!(ask_cancelreply_raw_hostile, Ask, CancelReply, false, true);
    case!(ask_success_raw_healthy, Ask, Success, false, false);
    case!(ask_success_raw_hostile, Ask, Success, false, true);
    case!(
        ask_disconnected_managed_healthy,
        Ask,
        Disconnected,
        true,
        false
    );
    case!(
        ask_disconnected_managed_hostile,
        Ask,
        Disconnected,
        true,
        true
    );
    case!(ask_sealed_managed_healthy, Ask, Sealed, true, false);
    case!(ask_sealed_managed_hostile, Ask, Sealed, true, true);
    case!(ask_cancelfull_managed_healthy, Ask, CancelFull, true, false);
    case!(ask_cancelfull_managed_hostile, Ask, CancelFull, true, true);
    case!(
        ask_timeoutfull_managed_healthy,
        Ask,
        TimeoutFull,
        true,
        false
    );
    case!(
        ask_timeoutfull_managed_hostile,
        Ask,
        TimeoutFull,
        true,
        true
    );
    case!(
        ask_cancelreply_managed_healthy,
        Ask,
        CancelReply,
        true,
        false
    );
    case!(
        ask_cancelreply_managed_hostile,
        Ask,
        CancelReply,
        true,
        true
    );
    case!(ask_success_managed_healthy, Ask, Success, true, false);
    case!(ask_success_managed_hostile, Ask, Success, true, true);
    case!(
        ask_sealedreply_managed_healthy,
        Ask,
        SealedReply,
        true,
        false
    );
    case!(
        ask_sealedreply_managed_hostile,
        Ask,
        SealedReply,
        true,
        true
    );
    case!(
        unbounded_disconnected_raw_healthy,
        Unbounded,
        Disconnected,
        false,
        false
    );
    case!(
        unbounded_disconnected_raw_hostile,
        Unbounded,
        Disconnected,
        false,
        true
    );
    case!(
        unbounded_cancelfull_raw_healthy,
        Unbounded,
        CancelFull,
        false,
        false
    );
    case!(
        unbounded_cancelfull_raw_hostile,
        Unbounded,
        CancelFull,
        false,
        true
    );
    case!(
        unbounded_cancelreply_raw_healthy,
        Unbounded,
        CancelReply,
        false,
        false
    );
    case!(
        unbounded_cancelreply_raw_hostile,
        Unbounded,
        CancelReply,
        false,
        true
    );
    case!(
        unbounded_success_raw_healthy,
        Unbounded,
        Success,
        false,
        false
    );
    case!(
        unbounded_success_raw_hostile,
        Unbounded,
        Success,
        false,
        true
    );
    case!(
        unbounded_disconnected_managed_healthy,
        Unbounded,
        Disconnected,
        true,
        false
    );
    case!(
        unbounded_disconnected_managed_hostile,
        Unbounded,
        Disconnected,
        true,
        true
    );
    case!(
        unbounded_sealed_managed_healthy,
        Unbounded,
        Sealed,
        true,
        false
    );
    case!(
        unbounded_sealed_managed_hostile,
        Unbounded,
        Sealed,
        true,
        true
    );
    case!(
        unbounded_cancelfull_managed_healthy,
        Unbounded,
        CancelFull,
        true,
        false
    );
    case!(
        unbounded_cancelfull_managed_hostile,
        Unbounded,
        CancelFull,
        true,
        true
    );
    case!(
        unbounded_cancelreply_managed_healthy,
        Unbounded,
        CancelReply,
        true,
        false
    );
    case!(
        unbounded_cancelreply_managed_hostile,
        Unbounded,
        CancelReply,
        true,
        true
    );
    case!(
        unbounded_success_managed_healthy,
        Unbounded,
        Success,
        true,
        false
    );
    case!(
        unbounded_success_managed_hostile,
        Unbounded,
        Success,
        true,
        true
    );
    case!(
        unbounded_sealedreply_managed_healthy,
        Unbounded,
        SealedReply,
        true,
        false
    );
    case!(
        unbounded_sealedreply_managed_hostile,
        Unbounded,
        SealedReply,
        true,
        true
    );
    case!(
        explicit_disconnected_raw_healthy,
        Explicit,
        Disconnected,
        false,
        false
    );
    case!(
        explicit_disconnected_raw_hostile,
        Explicit,
        Disconnected,
        false,
        true
    );
    case!(
        explicit_cancelfull_raw_healthy,
        Explicit,
        CancelFull,
        false,
        false
    );
    case!(
        explicit_cancelfull_raw_hostile,
        Explicit,
        CancelFull,
        false,
        true
    );
    case!(
        explicit_timeoutfull_raw_healthy,
        Explicit,
        TimeoutFull,
        false,
        false
    );
    case!(
        explicit_timeoutfull_raw_hostile,
        Explicit,
        TimeoutFull,
        false,
        true
    );
    case!(
        explicit_cancelreply_raw_healthy,
        Explicit,
        CancelReply,
        false,
        false
    );
    case!(
        explicit_cancelreply_raw_hostile,
        Explicit,
        CancelReply,
        false,
        true
    );
    case!(
        explicit_success_raw_healthy,
        Explicit,
        Success,
        false,
        false
    );
    case!(explicit_success_raw_hostile, Explicit, Success, false, true);
    case!(
        explicit_disconnected_managed_healthy,
        Explicit,
        Disconnected,
        true,
        false
    );
    case!(
        explicit_disconnected_managed_hostile,
        Explicit,
        Disconnected,
        true,
        true
    );
    case!(
        explicit_sealed_managed_healthy,
        Explicit,
        Sealed,
        true,
        false
    );
    case!(
        explicit_sealed_managed_hostile,
        Explicit,
        Sealed,
        true,
        true
    );
    case!(
        explicit_cancelfull_managed_healthy,
        Explicit,
        CancelFull,
        true,
        false
    );
    case!(
        explicit_cancelfull_managed_hostile,
        Explicit,
        CancelFull,
        true,
        true
    );
    case!(
        explicit_timeoutfull_managed_healthy,
        Explicit,
        TimeoutFull,
        true,
        false
    );
    case!(
        explicit_timeoutfull_managed_hostile,
        Explicit,
        TimeoutFull,
        true,
        true
    );
    case!(
        explicit_cancelreply_managed_healthy,
        Explicit,
        CancelReply,
        true,
        false
    );
    case!(
        explicit_cancelreply_managed_hostile,
        Explicit,
        CancelReply,
        true,
        true
    );
    case!(
        explicit_success_managed_healthy,
        Explicit,
        Success,
        true,
        false
    );
    case!(
        explicit_success_managed_hostile,
        Explicit,
        Success,
        true,
        true
    );
    case!(
        explicit_sealedreply_managed_healthy,
        Explicit,
        SealedReply,
        true,
        false
    );
    case!(
        explicit_sealedreply_managed_hostile,
        Explicit,
        SealedReply,
        true,
        true
    );
    case!(
        tell_disconnected_raw_healthy,
        Tell,
        Disconnected,
        false,
        false
    );
    case!(
        tell_disconnected_raw_hostile,
        Tell,
        Disconnected,
        false,
        true
    );
    case!(tell_cancelfull_raw_healthy, Tell, CancelFull, false, false);
    case!(tell_cancelfull_raw_hostile, Tell, CancelFull, false, true);
    case!(
        tell_timeoutfull_raw_healthy,
        Tell,
        TimeoutFull,
        false,
        false
    );
    case!(tell_timeoutfull_raw_hostile, Tell, TimeoutFull, false, true);
    case!(
        tell_disconnected_managed_healthy,
        Tell,
        Disconnected,
        true,
        false
    );
    case!(
        tell_disconnected_managed_hostile,
        Tell,
        Disconnected,
        true,
        true
    );
    case!(tell_sealed_managed_healthy, Tell, Sealed, true, false);
    case!(tell_sealed_managed_hostile, Tell, Sealed, true, true);
    case!(
        tell_cancelfull_managed_healthy,
        Tell,
        CancelFull,
        true,
        false
    );
    case!(
        tell_cancelfull_managed_hostile,
        Tell,
        CancelFull,
        true,
        true
    );
    case!(
        tell_timeoutfull_managed_healthy,
        Tell,
        TimeoutFull,
        true,
        false
    );
    case!(
        tell_timeoutfull_managed_hostile,
        Tell,
        TimeoutFull,
        true,
        true
    );
    case!(
        bridgeask_disconnected_raw_healthy,
        BridgeAsk,
        Disconnected,
        false,
        false
    );
    case!(
        bridgeask_disconnected_raw_hostile,
        BridgeAsk,
        Disconnected,
        false,
        true
    );
    case!(
        bridgeask_cancelfull_raw_healthy,
        BridgeAsk,
        CancelFull,
        false,
        false
    );
    case!(
        bridgeask_cancelfull_raw_hostile,
        BridgeAsk,
        CancelFull,
        false,
        true
    );
    case!(
        bridgeask_timeoutfull_raw_healthy,
        BridgeAsk,
        TimeoutFull,
        false,
        false
    );
    case!(
        bridgeask_timeoutfull_raw_hostile,
        BridgeAsk,
        TimeoutFull,
        false,
        true
    );
    case!(
        bridgeask_cancelreply_raw_healthy,
        BridgeAsk,
        CancelReply,
        false,
        false
    );
    case!(
        bridgeask_cancelreply_raw_hostile,
        BridgeAsk,
        CancelReply,
        false,
        true
    );
    case!(
        bridgeask_success_raw_healthy,
        BridgeAsk,
        Success,
        false,
        false
    );
    case!(
        bridgeask_success_raw_hostile,
        BridgeAsk,
        Success,
        false,
        true
    );
    case!(
        bridgeask_disconnected_managed_healthy,
        BridgeAsk,
        Disconnected,
        true,
        false
    );
    case!(
        bridgeask_disconnected_managed_hostile,
        BridgeAsk,
        Disconnected,
        true,
        true
    );
    case!(
        bridgeask_sealed_managed_healthy,
        BridgeAsk,
        Sealed,
        true,
        false
    );
    case!(
        bridgeask_sealed_managed_hostile,
        BridgeAsk,
        Sealed,
        true,
        true
    );
    case!(
        bridgeask_cancelfull_managed_healthy,
        BridgeAsk,
        CancelFull,
        true,
        false
    );
    case!(
        bridgeask_cancelfull_managed_hostile,
        BridgeAsk,
        CancelFull,
        true,
        true
    );
    case!(
        bridgeask_timeoutfull_managed_healthy,
        BridgeAsk,
        TimeoutFull,
        true,
        false
    );
    case!(
        bridgeask_timeoutfull_managed_hostile,
        BridgeAsk,
        TimeoutFull,
        true,
        true
    );
    case!(
        bridgeask_cancelreply_managed_healthy,
        BridgeAsk,
        CancelReply,
        true,
        false
    );
    case!(
        bridgeask_cancelreply_managed_hostile,
        BridgeAsk,
        CancelReply,
        true,
        true
    );
    case!(
        bridgeask_success_managed_healthy,
        BridgeAsk,
        Success,
        true,
        false
    );
    case!(
        bridgeask_success_managed_hostile,
        BridgeAsk,
        Success,
        true,
        true
    );
    case!(
        bridgeask_sealedreply_managed_healthy,
        BridgeAsk,
        SealedReply,
        true,
        false
    );
    case!(
        bridgeask_sealedreply_managed_hostile,
        BridgeAsk,
        SealedReply,
        true,
        true
    );
    case!(
        bridgetell_disconnected_raw_healthy,
        BridgeTell,
        Disconnected,
        false,
        false
    );
    case!(
        bridgetell_disconnected_raw_hostile,
        BridgeTell,
        Disconnected,
        false,
        true
    );
    case!(
        bridgetell_cancelfull_raw_healthy,
        BridgeTell,
        CancelFull,
        false,
        false
    );
    case!(
        bridgetell_cancelfull_raw_hostile,
        BridgeTell,
        CancelFull,
        false,
        true
    );
    case!(
        bridgetell_disconnected_managed_healthy,
        BridgeTell,
        Disconnected,
        true,
        false
    );
    case!(
        bridgetell_disconnected_managed_hostile,
        BridgeTell,
        Disconnected,
        true,
        true
    );
    case!(
        bridgetell_sealed_managed_healthy,
        BridgeTell,
        Sealed,
        true,
        false
    );
    case!(
        bridgetell_sealed_managed_hostile,
        BridgeTell,
        Sealed,
        true,
        true
    );
    case!(
        bridgetell_cancelfull_managed_healthy,
        BridgeTell,
        CancelFull,
        true,
        false
    );
    case!(
        bridgetell_cancelfull_managed_hostile,
        BridgeTell,
        CancelFull,
        true,
        true
    );
}
