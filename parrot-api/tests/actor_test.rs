use parrot_api::actor::{Actor, ActorState, ActorConfig, ActorFactory};
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture, BoxedActorRef};
use parrot_api::errors::ActorError;
use std::sync::{Arc, Mutex};
use std::any::Any;

// Empty Config implementation for test actors
#[derive(Default)]
struct TestConfig;
impl ActorConfig for TestConfig {}

// Mock Context implementation for testing
#[derive(Default)]
struct MockContext;

// Test actor implementation
struct TestActor {
    state: ActorState,
    counter: u64,
    process_count: u64,
    initialized: bool,
    stopped: bool,
}

impl Default for TestActor {
    fn default() -> Self {
        Self {
            state: ActorState::Starting,
            counter: 0,
            process_count: 0,
            initialized: false,
            stopped: false,
        }
    }
}

// Messages for testing
struct Increment(u64);
struct GetCounter;
struct Panic;

impl Actor for TestActor {
    type Config = TestConfig;
    type Context = MockContext;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async {
            self.initialized = true;
            self.state = ActorState::Running;
            Ok(())
        })
    }

    fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.process_count += 1;
            
            if let Some(increment) = msg.downcast_ref::<Increment>() {
                self.counter += increment.0;
                return Ok(Box::new(()) as BoxedMessage);
            } else if msg.downcast_ref::<GetCounter>().is_some() {
                return Ok(Box::new(self.counter) as BoxedMessage);
            } else if msg.downcast_ref::<Panic>().is_some() {
                return Err(ActorError::MessageHandlingError("Panic message received".to_string()));
            } else {
                return Ok(msg); // Echo back unknown messages
            }
        })
    }

    fn before_stop<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async {
            self.stopped = true;
            self.state = ActorState::Stopped;
            Ok(())
        })
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

// Factory implementation for test actor
struct TestActorFactory;

impl ActorFactory<TestActor> for TestActorFactory {
    fn create(&self, _config: TestConfig) -> TestActor {
        TestActor::default()
    }
}

// Stream processing test actor
struct StreamTestActor {
    state: ActorState,
    stream_started: bool,
    stream_finished: bool,
    stream_error: Option<String>,
    item_count: u64,
}

impl Default for StreamTestActor {
    fn default() -> Self {
        Self {
            state: ActorState::Running,
            stream_started: false,
            stream_finished: false,
            stream_error: None,
            item_count: 0,
        }
    }
}

impl Actor for StreamTestActor {
    type Config = TestConfig;
    type Context = MockContext;

    fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            Ok(msg) // Simply echo back messages
        })
    }

    fn handle_stream<'a>(&'a mut self, item: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.item_count += 1;
            Ok(item)
        })
    }

    fn stream_started<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async {
            self.stream_started = true;
            Ok(())
        })
    }

    fn stream_finished<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async {
            self.stream_finished = true;
            Ok(())
        })
    }

    fn stream_error<'a>(&'a mut self, err: ActorError, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.stream_error = Some(err.to_string());
            Ok(())
        })
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

// Tests for Actor lifecycle
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::runtime::Runtime;

    // Helper function to create a mock context
    fn create_mock_context() -> MockContext {
        MockContext::default()
    }

    // Test actor initialization
    #[test]
    fn test_actor_initialization() {
        let rt = Runtime::new().unwrap();
        let mut actor = TestActor::default();
        let mut ctx = create_mock_context();
        
        assert_eq!(actor.state(), ActorState::Starting);
        assert!(!actor.initialized);
        
        rt.block_on(async {
            let result = actor.init(&mut ctx).await;
            assert!(result.is_ok());
            assert!(actor.initialized);
            assert_eq!(actor.state(), ActorState::Running);
        });
    }
    
    // Test message handling
    #[test]
    fn test_message_handling() {
        let rt = Runtime::new().unwrap();
        let mut actor = TestActor::default();
        let mut ctx = create_mock_context();
        
        rt.block_on(async {
            // Initialize first
            let _ = actor.init(&mut ctx).await;
            
            // Test increment message
            let increment_msg = Box::new(Increment(5)) as BoxedMessage;
            let result = actor.receive_message(increment_msg, &mut ctx).await;
            assert!(result.is_ok());
            assert_eq!(actor.counter, 5);
            
            // Test get counter message
            let get_counter_msg = Box::new(GetCounter) as BoxedMessage;
            let result = actor.receive_message(get_counter_msg, &mut ctx).await;
            assert!(result.is_ok());
            let counter = result.unwrap().downcast::<u64>();
            assert!(counter.is_ok());
            assert_eq!(*counter.unwrap(), 5);
            
            // Test error handling
            let panic_msg = Box::new(Panic) as BoxedMessage;
            let result = actor.receive_message(panic_msg, &mut ctx).await;
            assert!(result.is_err());
            if let Err(ActorError::MessageHandlingError(msg)) = result {
                assert_eq!(msg, "Panic message received");
            } else {
                panic!("Expected MessageHandlingError");
            }
            
            // Verify process count
            assert_eq!(actor.process_count, 3);
        });
    }
    
    // Test actor stopping
    #[test]
    fn test_actor_stopping() {
        let rt = Runtime::new().unwrap();
        let mut actor = TestActor::default();
        let mut ctx = create_mock_context();
        
        rt.block_on(async {
            // Initialize first
            let _ = actor.init(&mut ctx).await;
            
            // Stop the actor
            actor.state = ActorState::Stopping;
            let result = actor.before_stop(&mut ctx).await;
            assert!(result.is_ok());
            assert!(actor.stopped);
            assert_eq!(actor.state(), ActorState::Stopped);
        });
    }
    
    // Test actor factory
    #[test]
    fn test_actor_factory() {
        let factory = TestActorFactory;
        let actor = factory.create(TestConfig::default());
        
        assert_eq!(actor.state(), ActorState::Starting);
        assert_eq!(actor.counter, 0);
    }
    
    // Test stream handling
    #[test]
    fn test_stream_handling() {
        let rt = Runtime::new().unwrap();
        let mut actor = StreamTestActor::default();
        let mut ctx = create_mock_context();
        
        rt.block_on(async {
            // Test stream started
            let result = actor.stream_started(&mut ctx).await;
            assert!(result.is_ok());
            assert!(actor.stream_started);
            
            // Test stream item processing
            let item1 = Box::new(42) as BoxedMessage;
            let item2 = Box::new("test") as BoxedMessage;
            
            let _ = actor.handle_stream(item1, &mut ctx).await;
            let _ = actor.handle_stream(item2, &mut ctx).await;
            assert_eq!(actor.item_count, 2);
            
            // Test stream finished
            let result = actor.stream_finished(&mut ctx).await;
            assert!(result.is_ok());
            assert!(actor.stream_finished);
            
            // Test stream error
            let error = ActorError::Timeout;
            let result = actor.stream_error(error, &mut ctx).await;
            assert!(result.is_ok());
            assert_eq!(actor.stream_error, Some("Timeout".to_string()));
        });
    }
    
    // Test different actor states
    #[test]
    fn test_actor_states() {
        let mut actor = TestActor::default();
        
        assert_eq!(actor.state(), ActorState::Starting);
        
        actor.state = ActorState::Running;
        assert_eq!(actor.state(), ActorState::Running);
        
        actor.state = ActorState::Stopping;
        assert_eq!(actor.state(), ActorState::Stopping);
        
        actor.state = ActorState::Stopped;
        assert_eq!(actor.state(), ActorState::Stopped);
    }
} 