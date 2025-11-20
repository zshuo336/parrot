use anyhow::Result;
use parrot::{
    actix::{ActixActor, ActixActorSystem, ActixContext},
    system::ParrotActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRefExt,
    context::{ActorContext, ActorSpawnerExt},
    message::Message,
    system::{ActorSystem, ActorSystemConfig},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

// Simple actor implementation using Actix context (not default)
struct SimpleActor {
    state: ActorState,
    name: String,
    parent: Option<String>,
    retired: Arc<AtomicUsize>,
}

impl SimpleActor {
    fn new(name: String, retired: Arc<AtomicUsize>) -> Self {
        Self {
            state: ActorState::Starting,
            name,
            parent: None,
            retired,
        }
    }
}

impl Drop for SimpleActor {
    fn drop(&mut self) {
        self.retired.fetch_add(1, Ordering::SeqCst);
    }
}

impl Actor for SimpleActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert_eq!(
                ctx.parent().map(|parent| parent.path().to_owned()),
                self.parent
            );
            Ok(())
        })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        println!("SimpleActor '{}' received message", self.name);
        Box::pin(async move {
            // Return the same message as a response
            Ok(msg)
        })
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

// Actix-compatible actor implementation
struct ActixCompatibleActor {
    state: ActorState,
    name: String,
    retired: Arc<AtomicUsize>,
}

impl ActixCompatibleActor {
    fn new(name: String, retired: Arc<AtomicUsize>) -> Self {
        Self {
            state: ActorState::Starting,
            name,
            retired,
        }
    }
}

impl Drop for ActixCompatibleActor {
    fn drop(&mut self) {
        self.retired.fetch_add(1, Ordering::SeqCst);
    }
}

impl Actor for ActixCompatibleActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        println!("ActixCompatibleActor '{}' received message", self.name);
        Box::pin(async move {
            // Return the same message as a response
            Ok(msg)
        })
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

// Test message
#[derive(Clone)]
struct TestMessage(String);

impl Message for TestMessage {
    type Result = String;

    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        // Directly try to cast to TestMessage or String
        match result.downcast::<TestMessage>() {
            Ok(msg) => Ok(msg.0.clone()),
            Err(other_result) => match other_result.downcast::<String>() {
                Ok(string) => Ok(*string),
                Err(_) => Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "Failed to extract result - unknown response type".to_string(),
                )),
            },
        }
    }
}

// Actor capable of spawning child actors
struct ActixContextSpawner {
    state: ActorState,
    name: String,
    child_count: usize,
    retired: Arc<AtomicUsize>,
}

impl ActixContextSpawner {
    fn new(name: String, retired: Arc<AtomicUsize>) -> Self {
        Self {
            state: ActorState::Starting,
            name,
            child_count: 0,
            retired,
        }
    }
}

impl Drop for ActixContextSpawner {
    fn drop(&mut self) {
        self.retired.fetch_add(1, Ordering::SeqCst);
    }
}

impl Actor for ActixContextSpawner {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

    fn use_async_handler(&self) -> bool {
        true
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let (another, requested) =
                if let Some(request) = msg.downcast_ref::<SpawnChildMessage>() {
                    (false, &request.0)
                } else if let Some(request) = msg.downcast_ref::<ActixAnotherChildMessage>() {
                    (true, &request.0)
                } else if let Some(request) = msg.downcast_ref::<TestMessage>() {
                    assert_eq!(ctx.children().unwrap().len(), self.child_count);
                    return Ok(
                        Box::new(format!("ActixContextSpawner received: {}", request.0))
                            as BoxedMessage,
                    );
                } else {
                    return Err(parrot_api::errors::ActorError::MessageHandlingError(
                        "unknown spawner message".into(),
                    ));
                };
            let index = self.child_count + 1;
            let name = if another {
                format!("{}_another_child_{index}", self.name)
            } else {
                format!("{}_child_{index}", self.name)
            };
            let mut child = SimpleActor::new(name.clone(), self.retired.clone());
            child.parent = Some(ctx.path().path().to_owned());
            let child = ctx.spawner().spawn_typed(child, EmptyConfig).await?;
            assert_eq!(
                child.ask(TestMessage(requested.clone())).await?,
                format!("SimpleActor {name} received: {requested}")
            );
            self.child_count = index;
            assert_eq!(ctx.children().unwrap().len(), index);
            let response = if another {
                format!("Created another managed child '{name}'")
            } else {
                format!("Spawned managed child '{name}'")
            };
            Ok(Box::new(response) as BoxedMessage)
        })
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

// Specific message to trigger spawn
#[derive(Debug, Clone)]
struct SpawnChildMessage(String);

impl Message for SpawnChildMessage {
    type Result = String;

    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        // Directly try to cast to String
        if let Ok(string) = result.downcast::<String>() {
            return Ok(*string);
        }

        Err(parrot_api::errors::ActorError::MessageHandlingError(
            "Failed to extract result - expected String".to_string(),
        ))
    }
}

// ActixAnotherChildMessage for creating another type of child actor
#[derive(Debug, Clone)]
struct ActixAnotherChildMessage(String);

impl Message for ActixAnotherChildMessage {
    type Result = String;

    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        // Directly try to cast to String
        if let Ok(string) = result.downcast::<String>() {
            return Ok(*string);
        }

        Err(parrot_api::errors::ActorError::MessageHandlingError(
            "Failed to extract result - expected String".to_string(),
        ))
    }
}

fn main() -> Result<()> {
    // Use actix::System to wrap our application
    actix::System::new().block_on(async { run_example().await })
}

async fn run_example() -> Result<()> {
    // Create system configuration
    let config = ActorSystemConfig::default();

    // Create Parrot actor system
    let system = ParrotActorSystem::new(config).await?;

    // Create and register the Actix system
    let actix_system = ActixActorSystem::new().await?;
    system
        .register_actix_system("actix".to_string(), actix_system, true)
        .await?;

    let retired = Arc::new(AtomicUsize::new(0));
    let result = async {
        let simple = system
            .spawn_root_actix(SimpleActor::new("simple".into(), retired.clone()), EmptyConfig)
            .await?;
        assert_eq!(
            simple.ask(TestMessage("Hello Simple with ask".into())).await?,
            "SimpleActor simple received: Hello Simple with ask"
        );
        // send waits for a business reply; deliver is the admission-only operation.
        let reply = simple.send(Box::new(TestMessage("Hello Simple with send".into()))).await?;
        assert_eq!(TestMessage::extract_result(reply)?, "SimpleActor simple received: Hello Simple with send");

        let compatible = system
            .spawn_root_actix(ActixCompatibleActor::new("actix-compatible".into(), retired.clone()), EmptyConfig)
            .await?;
        assert_eq!(compatible.ask(TestMessage("Hello Actix with ask".into())).await?, "Response from actix-compatible: Hello Actix with ask");
        let reply = compatible.send(Box::new(TestMessage("Hello Actix with send".into()))).await?;
        assert_eq!(TestMessage::extract_result(reply)?, "Response from actix-compatible: Hello Actix with send");

        let spawner = system
            .spawn_root_actix(ActixContextSpawner::new("main-spawner".into(), retired.clone()), EmptyConfig)
            .await?;
        assert_eq!(spawner.ask(SpawnChildMessage("test-child".into())).await?, "Spawned managed child 'main-spawner_child_1'");
        assert_eq!(spawner.ask(ActixAnotherChildMessage("another-test-child".into())).await?, "Created another managed child 'main-spawner_another_child_2'");
        assert_eq!(spawner.ask(SpawnChildMessage("another-child".into())).await?, "Spawned managed child 'main-spawner_child_3'");
        assert_eq!(spawner.ask(TestMessage("Test message to spawner".into())).await?, "ActixContextSpawner received: Test message to spawner");
        assert_eq!(system.status().active_actors, 6);
        assert_eq!(retired.load(Ordering::SeqCst), 0);
        spawner.stop().await?;
        assert!(!spawner.is_alive().await);
        assert_eq!(retired.load(Ordering::SeqCst), 4);
        assert_eq!(system.status().active_actors, 2);
        simple.stop().await?;
        compatible.stop().await?;
        assert_eq!(retired.load(Ordering::SeqCst), 6);
        assert_eq!(system.status().active_actors, 0);
        println!("facade replies verified; parent and three managed children retired; six actors disposed");
        Ok(())
    }.await;
    system.shutdown().await?;
    result
}

// M6: actix 同步快路径移至引擎侧扩展 trait（ActixEngineExt）。
impl parrot_api::actor::ActixEngineExt for SimpleActor {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine_ctx: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        // Add processing logic to avoid returning None
        println!("SimpleActor '{}' processing message via engine", self.name);

        if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
            println!("Got TestMessage: {}", test_msg.0);
            let response = Box::new(TestMessage(format!(
                "SimpleActor {} received: {}",
                self.name, test_msg.0
            ))) as BoxedMessage;
            Some(Ok(response))
        } else {
            // Return generic response
            Some(Ok(
                Box::new(TestMessage("Unknown message processed".to_string())) as BoxedMessage,
            ))
        }
    }
}

// M6: actix 同步快路径移至引擎侧扩展 trait（ActixEngineExt）。
impl parrot_api::actor::ActixEngineExt for ActixCompatibleActor {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine_ctx: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        // Properly handle messages and return results
        println!(
            "ActixCompatibleActor '{}' received message via engine",
            self.name
        );

        // Check if it's a TestMessage type
        if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
            println!("Processing TestMessage: {}", test_msg.0);

            // Return response
            let response = Box::new(TestMessage(format!(
                "Response from {}: {}",
                self.name, test_msg.0
            ))) as BoxedMessage;
            Some(Ok(response))
        } else {
            // For unknown messages, still return the original message
            Some(Ok(msg))
        }
    }
}

impl parrot_api::actor::ActixEngineExt for ActixContextSpawner {}
