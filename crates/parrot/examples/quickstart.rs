//! Run with: cargo run --locked -p parrot --example quickstart
//!
//! Both engines use the same messages. Each actor has an engine-specific
//! context; binding __parrot_engine selects the derive's context types.
use parrot_api_derive::Message;

#[derive(Debug, Clone, Message)]
#[message(result = "u32")]
struct Increment(u32);

#[derive(Debug, Clone, Message)]
#[message(result = "String")]
struct Describe;

mod actix_counter {
    use super::{Describe, Increment};
    use parrot::actix as __parrot_engine;
    use parrot::actix::ActixActorSystem;
    use parrot_api::actor::{Actor, EmptyConfig, EngineContextHandle};
    use parrot_api::address::ActorRefExt;
    use parrot_api::errors::ActorError;
    use parrot_api::types::{ActorResult, BoxedMessage};
    use parrot_api_derive::ParrotActor;

    #[derive(Debug, ParrotActor)]
    #[ParrotActor(engine = "actix", async_handler = true)]
    struct Counter(u32);

    impl Counter {
        async fn handle_message(
            &mut self,
            msg: BoxedMessage,
            _ctx: &mut <Self as Actor>::Context,
        ) -> ActorResult<BoxedMessage> {
            if let Some(Increment(amount)) = msg.downcast_ref::<Increment>() {
                self.0 += amount;
                Ok(Box::new(self.0))
            } else if msg.is::<Describe>() {
                Ok(Box::new(format!("count={}", self.0)))
            } else {
                Err(ActorError::MessageHandlingError("unknown message".into()))
            }
        }

        // Required by the Actix derive. async_handler=true selects the async
        // handler above, so this synchronous fast path is not called.
        fn handle_message_engine(
            &mut self,
            _msg: BoxedMessage,
            _ctx: &mut <Self as Actor>::Context,
            _engine_ctx: EngineContextHandle,
        ) -> Option<ActorResult<BoxedMessage>> {
            None
        }
    }

    pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
        let system = ActixActorSystem::new().await?;
        let result = async {
            let actor = system.spawn_root_typed(Counter(0), EmptyConfig).await?;
            assert_eq!(actor.ask(Increment(2)).await?, 2);
            assert_eq!(actor.ask(Increment(3)).await?, 5);
            let description = actor.ask(Describe).await?;
            assert_eq!(description, "count=5");
            println!("Actix: {description}");
            Ok(())
        }
        .await;
        system.shutdown().await?;
        result
    }
}

mod thread_counter {
    use super::{Describe, Increment};
    use parrot::thread as __parrot_engine;
    use parrot::thread::ThreadActorSystem;
    use parrot_api::actor::{Actor, EmptyConfig};
    use parrot_api::address::ActorRefExt;
    use parrot_api::errors::ActorError;
    use parrot_api::types::{ActorResult, BoxedMessage};
    use parrot_api_derive::ParrotActor;

    #[derive(Debug, ParrotActor)]
    #[ParrotActor(engine = "thread")]
    struct Counter(u32);

    impl Counter {
        async fn handle_message(
            &mut self,
            msg: BoxedMessage,
            _ctx: &mut <Self as Actor>::Context,
        ) -> ActorResult<BoxedMessage> {
            if let Some(Increment(amount)) = msg.downcast_ref::<Increment>() {
                self.0 += amount;
                Ok(Box::new(self.0))
            } else if msg.is::<Describe>() {
                Ok(Box::new(format!("count={}", self.0)))
            } else {
                Err(ActorError::MessageHandlingError("unknown message".into()))
            }
        }
    }

    pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
        // shared() installs the weak system reference used by actor contexts.
        let system = ThreadActorSystem::shared(Default::default());
        let result = async {
            let actor = system
                .spawn_root_typed_thread(Counter(0), EmptyConfig)
                .await?;
            // ThreadActorRef also has an inherent boxed ask method. Select
            // the shared typed extension explicitly to retain result types.
            assert_eq!(ActorRefExt::ask(&actor, Increment(2)).await?, 2);
            assert_eq!(ActorRefExt::ask(&actor, Increment(3)).await?, 5);
            let description = ActorRefExt::ask(&actor, Describe).await?;
            assert_eq!(description, "count=5");
            println!("Thread: {description}");
            Ok(())
        }
        .await;
        system.shutdown_internal().await?;
        result
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Actix needs its System/arbiter context. The thread engine also runs in
    // this Tokio runtime; a thread-only application can use #[tokio::main].
    actix::System::new().block_on(async {
        actix_counter::run().await?;
        thread_counter::run().await
    })
}
