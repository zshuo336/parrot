use parrot_api::actor::{Actor, ActorConfig, ActorFactory, ActorState, EmptyConfig};
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture};
use parrot_api::context::ActorContext;
use std::time::Duration;
use std::sync::Arc;
use std::any::Any;
use std::ptr::NonNull;

// Simple mock context for testing
#[derive(Default)]
struct MockContext;

// Test actor configuration
#[derive(Debug, Clone, PartialEq)]
pub struct TestActorConfig {
    pub name: String,
    pub process_delay: Duration,
    pub max_messages: usize,
}

// Implement Default for TestActorConfig
impl Default for TestActorConfig {
    fn default() -> Self {
        Self {
            name: "test-actor".to_string(),
            process_delay: Duration::from_millis(100),
            max_messages: 1000,
        }
    }
}

// Implement ActorConfig for TestActorConfig
impl ActorConfig for TestActorConfig {}

// Test actor implementation
struct TestActor {
    state: ActorState,
    config: TestActorConfig,
    processed_count: usize,
}

impl Actor for TestActor {
    type Config = TestActorConfig;
    type Context = MockContext;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.processed_count += 1;

        // Check if processed_count exceeds max_messages
        if self.processed_count > self.config.max_messages {
            self.state = ActorState::Stopping;
        }

        Box::pin(async move {
            // Simulate processing delay
            if !self.config.process_delay.is_zero() {
                tokio::time::sleep(self.config.process_delay).await;
            }
            // Return the original message
            Ok(msg)
        })
    }

    fn receive_message_with_engine<'a>(
        &'a mut self,
        _msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine_ctx: NonNull<dyn Any>,
    ) -> Option<ActorResult<BoxedMessage>> {
        None
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

// Implement Default for TestActor using default configuration
impl Default for TestActor {
    fn default() -> Self {
        Self {
            state: ActorState::Starting,
            config: TestActorConfig::default(),
            processed_count: 0,
        }
    }
}

// Default ActorFactory implementation for TestActor
#[derive(Clone)]
struct DefaultTestActorFactory;

impl ActorFactory<TestActor> for DefaultTestActorFactory {
    fn create(&self, config: TestActorConfig) -> TestActor {
        TestActor {
            state: ActorState::Starting,
            config,
            processed_count: 0,
        }
    }
}

// Custom ActorFactory that overrides the default name if not set
#[derive(Clone)]
struct CustomTestActorFactory {
    default_name: String,
}

impl ActorFactory<TestActor> for CustomTestActorFactory {
    fn create(&self, mut config: TestActorConfig) -> TestActor {
        // Use factory default_name if config.name is the default value
        if config.name == TestActorConfig::default().name {
            config.name = self.default_name.clone();
        }

        TestActor {
            state: ActorState::Starting,
            config,
            processed_count: 0,
        }
    }
}

// Shared ActorFactory using Arc for thread safety
struct SharedActorFactory<F>
where
    F: ActorFactory<TestActor> + Send + Sync + 'static,
{
    factory: Arc<F>,
}

impl<F> ActorFactory<TestActor> for SharedActorFactory<F>
where
    F: ActorFactory<TestActor> + Send + Sync + 'static,
{
    fn create(&self, config: TestActorConfig) -> TestActor {
        self.factory.create(config)
    }
}

// Example actor with EmptyConfig
struct EmptyConfigActor {
    state: ActorState,
}

impl Actor for EmptyConfigActor {
    type Config = EmptyConfig;
    type Context = MockContext;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(msg) })
    }

    fn receive_message_with_engine<'a>(
        &'a mut self,
        _msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine_ctx: NonNull<dyn Any>,
    ) -> Option<ActorResult<BoxedMessage>> {
        None
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

// Factory implementation for EmptyConfigActor
#[derive(Clone)]
struct EmptyConfigActorFactory;

impl ActorFactory<EmptyConfigActor> for EmptyConfigActorFactory {
    fn create(&self, _config: EmptyConfig) -> EmptyConfigActor {
        EmptyConfigActor {
            state: ActorState::Starting,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_factory_with_default_config() {
        let factory = DefaultTestActorFactory;
        let actor = factory.create(TestActorConfig::default());

        // Verify the actor uses the default configuration
        assert_eq!(actor.config.name, "test-actor");
        assert_eq!(actor.config.process_delay, Duration::from_millis(100));
        assert_eq!(actor.config.max_messages, 1000);
        assert_eq!(actor.processed_count, 0);
        assert_eq!(actor.state(), ActorState::Starting);
    }

    #[test]
    fn test_default_factory_with_custom_config() {
        let factory = DefaultTestActorFactory;
        let custom_config = TestActorConfig {
            name: "custom-actor".to_string(),
            process_delay: Duration::from_millis(50),
            max_messages: 500,
        };

        let actor = factory.create(custom_config.clone());

        // Verify the actor uses the provided custom configuration
        assert_eq!(actor.config, custom_config);
        assert_eq!(actor.processed_count, 0);
        assert_eq!(actor.state(), ActorState::Starting);
    }

    #[test]
    fn test_custom_factory_with_default_config() {
        let factory = CustomTestActorFactory {
            default_name: "factory-default-name".to_string(),
        };

        let actor = factory.create(TestActorConfig::default());

        // Verify the factory overrides the default name but retains other default values
        assert_eq!(actor.config.name, "factory-default-name");
        assert_eq!(actor.config.process_delay, Duration::from_millis(100));
        assert_eq!(actor.config.max_messages, 1000);
    }

    #[test]
    fn test_custom_factory_with_custom_config() {
        let factory = CustomTestActorFactory {
            default_name: "factory-default-name".to_string(),
        };

        let custom_config = TestActorConfig {
            name: "custom-actor".to_string(),
            process_delay: Duration::from_millis(50),
            max_messages: 500,
        };

        let actor = factory.create(custom_config.clone());

        // Verify the factory respects the custom name
        assert_eq!(actor.config.name, "custom-actor");
        assert_eq!(actor.config.process_delay, Duration::from_millis(50));
        assert_eq!(actor.config.max_messages, 500);
    }

    #[test]
    fn test_shared_factory() {
        let base_factory = DefaultTestActorFactory;
        let shared_factory = SharedActorFactory {
            factory: Arc::new(base_factory),
        };

        let actor = shared_factory.create(TestActorConfig::default());

        // Verify the Arc-wrapped factory works correctly
        assert_eq!(actor.config.name, "test-actor");
        assert_eq!(actor.config.process_delay, Duration::from_millis(100));
        assert_eq!(actor.config.max_messages, 1000);
    }

    #[test]
    fn test_empty_config_actor_factory() {
        let factory = EmptyConfigActorFactory;
        let config = EmptyConfig::default();

        let actor = factory.create(config);

        // Verify the empty-config actor is initialized correctly
        assert_eq!(actor.state(), ActorState::Starting);
    }
}