use parrot_api::actor::{Actor, ActorState, ActorConfig, ActorFactory};
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture, BoxedActorRef};
use parrot_api::errors::ActorError;
use std::sync::{Arc, Mutex};
use std::any::Any;
use std::ptr::NonNull;

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

    fn receive_message_with_engine<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context, _engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        None
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

    fn receive_message_with_engine<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context, _engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        None
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

// EngineContext Actor implementation
struct EngineContextActor {
    state: ActorState,
    counter: u64,
    engine_counter: u64,
}

impl Default for EngineContextActor {
    fn default() -> Self {
        Self {
            state: ActorState::Running,
            counter: 0,
            engine_counter: 0,
        }
    }
}

// Custom engine context type for testing
#[derive(Debug)]
struct TestEngineContext {
    multiplier: u64,
}

impl Actor for EngineContextActor {
    type Config = TestConfig;
    type Context = MockContext;

    fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(increment) = msg.downcast_ref::<Increment>() {
                self.counter += increment.0;
                return Ok(Box::new(self.counter) as BoxedMessage);
            }
            Ok(msg)
        })
    }
    
    fn receive_message_with_engine<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context, engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        // Try to get the custom engine context
        let engine_ctx_ref = unsafe { engine_ctx.as_ref() };
        if let Some(ctx) = engine_ctx_ref.downcast_ref::<TestEngineContext>() {
            if let Some(increment) = msg.downcast_ref::<Increment>() {
                // Use the multiplier from the engine context
                let inc_amount = increment.0 * ctx.multiplier;
                self.engine_counter += inc_amount;
                return Some(Ok(Box::new(self.engine_counter) as BoxedMessage));
            }
        }
        
        None
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

// EngineErrorActor for testing error handling in receive_message_with_engine
struct EngineErrorActor {
    state: ActorState,
}

impl Default for EngineErrorActor {
    fn default() -> Self {
        Self {
            state: ActorState::Running,
        }
    }
}

impl Actor for EngineErrorActor {
    type Config = TestConfig;
    type Context = MockContext;

    fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            Ok(msg)
        })
    }
    
    fn receive_message_with_engine<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context, _engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        if msg.downcast_ref::<Panic>().is_some() {
            // Return an error for Panic messages
            return Some(Err(ActorError::MessageHandlingError("Engine context error".to_string())));
        }
        
        // For other message types, return None to fallback to regular handling
        None
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

// Additional message types for engine testing
struct Multiply(u64);
struct GetEngineCounter;
struct Reset;
struct InvalidOperation;

// Enhanced EngineContextActor with more operations
struct EnhancedEngineActor {
    state: ActorState,
    counter: u64,
    engine_counter: u64,
    last_operation: Option<String>,
}

impl Default for EnhancedEngineActor {
    fn default() -> Self {
        Self {
            state: ActorState::Running,
            counter: 0,
            engine_counter: 0,
            last_operation: None,
        }
    }
}

// Enhanced engine context for testing
struct EnhancedEngineContext {
    multiplier: u64,
    operation_mode: OperationMode,
}

// Different operation modes for testing
enum OperationMode {
    Normal,
    ReadOnly,
    ErrorProne,
}

impl Actor for EnhancedEngineActor {
    type Config = TestConfig;
    type Context = MockContext;

    fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            // Default message handling
            if let Some(increment) = msg.downcast_ref::<Increment>() {
                self.counter += increment.0;
                self.last_operation = Some("increment".to_string());
                return Ok(Box::new(self.counter) as BoxedMessage);
            } else if let Some(_) = msg.downcast_ref::<GetCounter>() {
                return Ok(Box::new(self.counter) as BoxedMessage);
            } else if let Some(_) = msg.downcast_ref::<Reset>() {
                self.counter = 0;
                self.last_operation = Some("reset".to_string());
                return Ok(Box::new(()) as BoxedMessage);
            }
            
            Ok(msg)
        })
    }
    
    fn receive_message_with_engine<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context, engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        // Try to get the engine context
        let engine_ctx_ref = unsafe { engine_ctx.as_ref() };
        
        if let Some(ctx) = engine_ctx_ref.downcast_ref::<EnhancedEngineContext>() {
            // Check operation mode first
            match ctx.operation_mode {
                OperationMode::ErrorProne => {
                    // Always return error in error-prone mode
                    return Some(Err(ActorError::MessageHandlingError("Engine is in error mode".to_string())));
                },
                OperationMode::ReadOnly => {
                    // In read-only mode, only allow read operations
                    if msg.downcast_ref::<GetCounter>().is_some() || msg.downcast_ref::<GetEngineCounter>().is_some() {
                        // Continue processing reads
                    } else {
                        // Return error for write operations
                        return Some(Err(ActorError::MessageHandlingError("Engine is in read-only mode".to_string())));
                    }
                },
                OperationMode::Normal => {
                    // Continue with normal processing
                }
            }
            
            // Process different message types
            if let Some(increment) = msg.downcast_ref::<Increment>() {
                let amount = increment.0 * ctx.multiplier;
                self.engine_counter += amount;
                self.last_operation = Some("engine_increment".to_string());
                return Some(Ok(Box::new(self.engine_counter) as BoxedMessage));
            } else if let Some(multiply) = msg.downcast_ref::<Multiply>() {
                self.engine_counter *= multiply.0 * ctx.multiplier;
                self.last_operation = Some("engine_multiply".to_string());
                return Some(Ok(Box::new(self.engine_counter) as BoxedMessage));
            } else if let Some(_) = msg.downcast_ref::<GetEngineCounter>() {
                return Some(Ok(Box::new(self.engine_counter) as BoxedMessage));
            } else if let Some(_) = msg.downcast_ref::<Reset>() {
                self.engine_counter = 0;
                self.last_operation = Some("engine_reset".to_string());
                return Some(Ok(Box::new(()) as BoxedMessage));
            } else if let Some(_) = msg.downcast_ref::<InvalidOperation>() {
                return Some(Err(ActorError::MessageHandlingError("Invalid operation requested".to_string())));
            }
        }
        
        // Return None to fall back to regular message handling
        None
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
    
    // Test receive_message_with_engine
    #[test]
    fn test_receive_message_with_engine() {
        let rt = Runtime::new().unwrap();
        let mut actor = TestActor::default();
        let mut ctx = create_mock_context();
        let engine_data = 42u32;
        
        // Create raw pointer to engine data
        let engine_ctx = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(engine_data) as Box<dyn Any>)) };
        
        rt.block_on(async {
            // Engine context handling should be None in the test actor
            let increment_msg = Box::new(Increment(5)) as BoxedMessage;
            let result = actor.receive_message_with_engine(increment_msg, &mut ctx, engine_ctx);
            assert!(result.is_none());
        });
        
        // Clean up the raw pointer to avoid memory leak
        unsafe {
            let _ = Box::from_raw(engine_ctx.as_ptr());
        }
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

    // Test receive_message_with_engine with custom engine context
    #[test]
    fn test_engine_context_handling() {
        let rt = Runtime::new().unwrap();
        let mut actor = EngineContextActor::default();
        let mut ctx = create_mock_context();
        
        // Create a test engine context with a multiplier
        let engine_data = TestEngineContext { multiplier: 2 };
        let engine_ctx = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(engine_data) as Box<dyn Any>)) };
        
        rt.block_on(async {
            // Test message with engine context
            let increment_msg = Box::new(Increment(5)) as BoxedMessage;
            let result = actor.receive_message_with_engine(increment_msg, &mut ctx, engine_ctx);
            
            // Should have a result because we implemented custom handling
            assert!(result.is_some());
            
            if let Some(Ok(boxed)) = result {
                let value = boxed.downcast::<u64>().unwrap();
                // Increment(5) * multiplier(2) = 10
                assert_eq!(*value, 10);
                assert_eq!(actor.engine_counter, 10);
                assert_eq!(actor.counter, 0); // Regular counter should be untouched
            } else {
                panic!("Expected Some(Ok) result");
            }
            
            // Test regular message handling still works
            let normal_increment_msg = Box::new(Increment(7)) as BoxedMessage;
            let normal_result = actor.receive_message(normal_increment_msg, &mut ctx).await;
            assert!(normal_result.is_ok());
            assert_eq!(actor.counter, 7);
            assert_eq!(actor.engine_counter, 10); // Engine counter should be unchanged
        });
        
        // Clean up the raw pointer
        unsafe {
            let _ = Box::from_raw(engine_ctx.as_ptr());
        }
    }

    // Test error handling in receive_message_with_engine
    #[test]
    fn test_engine_context_error_handling() {
        let rt = Runtime::new().unwrap();
        let mut actor = EngineErrorActor::default();
        let mut ctx = create_mock_context();
        
        // Create a simple engine context
        let engine_data = 42u32;
        let engine_ctx = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(engine_data) as Box<dyn Any>)) };
        
        rt.block_on(async {
            // Test error case with Panic message
            let panic_msg = Box::new(Panic) as BoxedMessage;
            let result = actor.receive_message_with_engine(panic_msg, &mut ctx, engine_ctx);
            
            // Should have Some(Err(...)) result
            assert!(result.is_some(), "Expected Some result");
            if let Some(result_inner) = result {
                assert!(result_inner.is_err(), "Expected error result");
                if let Err(ActorError::MessageHandlingError(msg)) = result_inner {
                    assert_eq!(msg, "Engine context error");
                } else {
                    panic!("Wrong error type");
                }
            }
            
            // Test fallback case with Increment message
            let increment_msg = Box::new(Increment(5)) as BoxedMessage;
            let result = actor.receive_message_with_engine(increment_msg, &mut ctx, engine_ctx);
            
            // Should return None to indicate fallback to regular message handling
            assert!(result.is_none(), "Expected None result for fallback");
            
            // Ensure regular handling still works - create a new message since the previous one was moved
            let new_increment_msg = Box::new(Increment(5)) as BoxedMessage;
            let normal_result = actor.receive_message(new_increment_msg, &mut ctx).await;
            assert!(normal_result.is_ok());
        });
        
        // Clean up the raw pointer
        unsafe {
            let _ = Box::from_raw(engine_ctx.as_ptr());
        }
    }

    // Test extended engine context handling with various message types
    #[test]
    fn test_enhanced_engine_message_handling() {
        let rt = Runtime::new().unwrap();
        let mut actor = EnhancedEngineActor::default();
        let mut ctx = create_mock_context();
        
        // Create normal mode engine context
        let engine_data = EnhancedEngineContext { 
            multiplier: 2, 
            operation_mode: OperationMode::Normal 
        };
        let engine_ctx = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(engine_data) as Box<dyn Any>)) };
        
        rt.block_on(async {
            // Test increment message
            let increment_msg = Box::new(Increment(5)) as BoxedMessage;
            let result = actor.receive_message_with_engine(increment_msg, &mut ctx, engine_ctx);
            
            assert!(result.is_some());
            if let Some(Ok(boxed)) = result {
                let value = boxed.downcast::<u64>().unwrap();
                assert_eq!(*value, 10); // 5 * 2 = 10
                assert_eq!(actor.engine_counter, 10);
                assert_eq!(actor.last_operation, Some("engine_increment".to_string()));
            } else {
                panic!("Expected Some(Ok) result for increment");
            }
            
            // Test multiply message
            let multiply_msg = Box::new(Multiply(3)) as BoxedMessage;
            let result = actor.receive_message_with_engine(multiply_msg, &mut ctx, engine_ctx);
            
            assert!(result.is_some());
            if let Some(Ok(boxed)) = result {
                let value = boxed.downcast::<u64>().unwrap();
                assert_eq!(*value, 60); // 10 * 3 * 2 = 60
                assert_eq!(actor.engine_counter, 60);
                assert_eq!(actor.last_operation, Some("engine_multiply".to_string()));
            } else {
                panic!("Expected Some(Ok) result for multiply");
            }
            
            // Test get counter message
            let get_counter_msg = Box::new(GetEngineCounter) as BoxedMessage;
            let result = actor.receive_message_with_engine(get_counter_msg, &mut ctx, engine_ctx);
            
            assert!(result.is_some());
            if let Some(Ok(boxed)) = result {
                let value = boxed.downcast::<u64>().unwrap();
                assert_eq!(*value, 60);
            } else {
                panic!("Expected Some(Ok) result for get counter");
            }
            
            // Test reset message
            let reset_msg = Box::new(Reset) as BoxedMessage;
            let result = actor.receive_message_with_engine(reset_msg, &mut ctx, engine_ctx);
            
            assert!(result.is_some());
            if let Some(Ok(_)) = result {
                assert_eq!(actor.engine_counter, 0);
                assert_eq!(actor.last_operation, Some("engine_reset".to_string()));
            } else {
                panic!("Expected Some(Ok) result for reset");
            }
            
            // Test invalid operation
            let invalid_msg = Box::new(InvalidOperation) as BoxedMessage;
            let result = actor.receive_message_with_engine(invalid_msg, &mut ctx, engine_ctx);
            
            assert!(result.is_some());
            if let Some(Err(ActorError::MessageHandlingError(msg))) = result {
                assert_eq!(msg, "Invalid operation requested");
            } else {
                panic!("Expected Some(Err) result for invalid operation");
            }
        });
        
        // Clean up the raw pointer
        unsafe {
            let _ = Box::from_raw(engine_ctx.as_ptr());
        }
    }
    
    // Test read-only mode engine context
    #[test]
    fn test_readonly_engine_context() {
        let rt = Runtime::new().unwrap();
        let mut actor = EnhancedEngineActor::default();
        let mut ctx = create_mock_context();
        
        // Set up engine counter first
        actor.engine_counter = 100;
        
        // Create read-only mode engine context
        let engine_data = EnhancedEngineContext { 
            multiplier: 2, 
            operation_mode: OperationMode::ReadOnly 
        };
        let engine_ctx = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(engine_data) as Box<dyn Any>)) };
        
        rt.block_on(async {
            // Test read operation should succeed
            let get_counter_msg = Box::new(GetEngineCounter) as BoxedMessage;
            let result = actor.receive_message_with_engine(get_counter_msg, &mut ctx, engine_ctx);
            
            assert!(result.is_some());
            if let Some(Ok(boxed)) = result {
                let value = boxed.downcast::<u64>().unwrap();
                assert_eq!(*value, 100);
            } else {
                panic!("Expected Some(Ok) result for read operation");
            }
            
            // Test write operation should fail
            let increment_msg = Box::new(Increment(5)) as BoxedMessage;
            let result = actor.receive_message_with_engine(increment_msg, &mut ctx, engine_ctx);
            
            assert!(result.is_some());
            if let Some(Err(ActorError::MessageHandlingError(msg))) = result {
                assert_eq!(msg, "Engine is in read-only mode");
                assert_eq!(actor.engine_counter, 100); // Value should not change
            } else {
                panic!("Expected Some(Err) result for write operation in read-only mode");
            }
        });
        
        // Clean up the raw pointer
        unsafe {
            let _ = Box::from_raw(engine_ctx.as_ptr());
        }
    }
    
    // Test error-prone engine context
    #[test]
    fn test_error_prone_engine_context() {
        let rt = Runtime::new().unwrap();
        let mut actor = EnhancedEngineActor::default();
        let mut ctx = create_mock_context();
        
        // Create error-prone mode engine context
        let engine_data = EnhancedEngineContext { 
            multiplier: 2, 
            operation_mode: OperationMode::ErrorProne 
        };
        let engine_ctx = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(engine_data) as Box<dyn Any>)) };
        
        rt.block_on(async {
            // All operations should fail in error-prone mode
            let operations = vec![
                Box::new(Increment(5)) as BoxedMessage,
                Box::new(GetEngineCounter) as BoxedMessage,
                Box::new(Reset) as BoxedMessage,
            ];
            
            for op in operations {
                let result = actor.receive_message_with_engine(op, &mut ctx, engine_ctx);
                
                assert!(result.is_some());
                if let Some(Err(ActorError::MessageHandlingError(msg))) = result {
                    assert_eq!(msg, "Engine is in error mode");
                } else {
                    panic!("Expected Some(Err) result for all operations in error-prone mode");
                }
            }
        });
        
        // Clean up the raw pointer
        unsafe {
            let _ = Box::from_raw(engine_ctx.as_ptr());
        }
    }
    
    // Test fallback to regular message handling when engine context type doesn't match
    #[test]
    fn test_engine_context_type_mismatch() {
        let rt = Runtime::new().unwrap();
        let mut actor = EnhancedEngineActor::default();
        let mut ctx = create_mock_context();
        
        // Create a different type of engine context 
        let engine_data = TestEngineContext { multiplier: 2 };
        let engine_ctx = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(engine_data) as Box<dyn Any>)) };
        
        rt.block_on(async {
            // Should fall back to regular message handling
            let increment_msg = Box::new(Increment(5)) as BoxedMessage;
            let result = actor.receive_message_with_engine(increment_msg, &mut ctx, engine_ctx);
            
            // Should be None to indicate fallback
            assert!(result.is_none());
            
            // Now test the regular message handling worked
            let new_increment_msg = Box::new(Increment(5)) as BoxedMessage;
            let normal_result = actor.receive_message(new_increment_msg, &mut ctx).await;
            
            assert!(normal_result.is_ok());
            assert_eq!(actor.counter, 5); // Regular counter incremented
            assert_eq!(actor.engine_counter, 0); // Engine counter unchanged
            assert_eq!(actor.last_operation, Some("increment".to_string()));
        });
        
        // Clean up the raw pointer
        unsafe {
            let _ = Box::from_raw(engine_ctx.as_ptr());
        }
    }
    
    // Test sequential processing of multiple messages
    #[test]
    fn test_sequential_engine_message_processing() {
        let rt = Runtime::new().unwrap();
        let mut actor = EnhancedEngineActor::default();
        let mut ctx = create_mock_context();
        
        // Create normal mode engine context
        let engine_data = EnhancedEngineContext { 
            multiplier: 2, 
            operation_mode: OperationMode::Normal 
        };
        let engine_ctx = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(engine_data) as Box<dyn Any>)) };
        
        rt.block_on(async {
            // Sequence of operations: Increment -> Multiply -> Get -> Reset -> Get
            let operations = vec![
                (Box::new(Increment(10)) as BoxedMessage, 20), // 0 + (10*2) = 20
                (Box::new(Multiply(3)) as BoxedMessage, 120),  // 20 * (3*2) = 120
                (Box::new(GetEngineCounter) as BoxedMessage, 120), // Still 120
                (Box::new(Reset) as BoxedMessage, 0),          // Reset to 0
                (Box::new(GetEngineCounter) as BoxedMessage, 0), // Should be 0
            ];
            
            for (i, (op, expected)) in operations.iter().enumerate() {
                // Create a new message for each iteration since we can't clone BoxedMessage
                let message: BoxedMessage = match i {
                    0 => Box::new(Increment(10)),
                    1 => Box::new(Multiply(3)),
                    2 => Box::new(GetEngineCounter),
                    3 => Box::new(Reset),
                    4 => Box::new(GetEngineCounter),
                    _ => unreachable!(),
                };
                
                let result = actor.receive_message_with_engine(message, &mut ctx, engine_ctx);
                
                assert!(result.is_some(), "Operation {} should return Some result", i);
                
                match result {
                    Some(Ok(boxed)) => {
                        if i != 3 { // Skip Reset which returns unit
                            let value = boxed.downcast::<u64>().unwrap_or_else(|_| 
                                panic!("Expected u64 value for operation {}", i)
                            );
                            assert_eq!(*value, *expected, "Operation {} should return expected value", i);
                        }
                    },
                    Some(Err(e)) => panic!("Operation {} failed: {:?}", i, e),
                    None => panic!("Operation {} returned None", i),
                }
                
                // Verify engine counter after each operation
                assert_eq!(actor.engine_counter, *expected, 
                          "Engine counter should be {} after operation {}", expected, i);
            }
        });
        
        // Clean up the raw pointer
        unsafe {
            let _ = Box::from_raw(engine_ctx.as_ptr());
        }
    }
} 