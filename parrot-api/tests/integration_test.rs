use parrot_api::actor::{Actor, ActorState, ActorConfig, ActorFactory};
use parrot_api::address::{ActorRef, ActorRefExt, ActorPath};
use parrot_api::context::ActorContext;
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture};
use parrot_api::errors::ActorError;
use parrot_api::message::{Message, MessageEnvelope, MessageOptions, MessagePriority};
use parrot_api::system::{ActorSystem, ActorSystemConfig, SystemTimeouts, GuardianConfig, SystemError};
use parrot_api::runtime::{RuntimeConfig, SchedulerConfig, LoadBalancingStrategy};
use parrot_api::supervisor::{SupervisorStrategyType, OneForOneStrategy, SupervisionDecision, BasicDecisionFn, DefaultStrategy, SupervisorStrategy};
use std::time::Duration;
use std::sync::{Arc, Mutex, RwLock};
use async_trait::async_trait;

// Integration test environment
// -----------------

// Mock Context for tests
#[derive(Default)]
struct MockContext;

// Test Actor configuration
#[derive(Default)]
struct GreeterConfig {
    greeting: String,
}

impl ActorConfig for GreeterConfig {}

// Message definitions
#[derive(Debug, Clone)]
struct Greet {
    name: String,
}

impl Message for Greet {
    type Result = String;
    
    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        if let Ok(response) = result.downcast::<String>() {
            Ok(*response)
        } else {
            Err(ActorError::MessageHandlingError("Failed to extract greeting result".to_string()))
        }
    }
    
    fn priority(&self) -> MessagePriority {
        MessagePriority::HIGH
    }
}

// Message that throws error
#[derive(Debug, Clone)]
struct FailingCommand;

impl Message for FailingCommand {
    type Result = ();
    
    fn extract_result(_: BoxedMessage) -> ActorResult<Self::Result> {
        Ok(())
    }
}

// Test Actor implementation
struct GreeterActor {
    state: ActorState,
    greeting: String,
    message_count: usize,
}

impl Actor for GreeterActor {
    type Config = GreeterConfig;
    type Context = MockContext;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async {
            self.state = ActorState::Running;
            Ok(())
        })
    }

    fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.message_count += 1;
            
            if let Some(greet) = msg.downcast_ref::<Greet>() {
                let response = format!("{} {}!", self.greeting, greet.name);
                return Ok(Box::new(response) as BoxedMessage);
            } else if msg.downcast_ref::<FailingCommand>().is_some() {
                return Err(ActorError::MessageHandlingError("Simulated failure".to_string()));
            }
            
            // Echo back unknown messages
            Ok(msg)
        })
    }

    fn before_stop<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async {
            self.state = ActorState::Stopped;
            Ok(())
        })
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

// Actor Factory
struct GreeterFactory;

impl ActorFactory<GreeterActor> for GreeterFactory {
    fn create(&self, config: GreeterConfig) -> GreeterActor {
        GreeterActor {
            state: ActorState::Starting,
            greeting: config.greeting,
            message_count: 0,
        }
    }
}

// Mock Actor System
struct MockActorSystem {
    config: ActorSystemConfig,
    actors: Arc<RwLock<Vec<Box<dyn ActorRef>>>>,
    state: Arc<RwLock<bool>>, // true means running
}

impl MockActorSystem {
    fn new(config: ActorSystemConfig) -> Self {
        Self {
            config,
            actors: Arc::new(RwLock::new(Vec::new())),
            state: Arc::new(RwLock::new(true)),
        }
    }
}

#[async_trait]
impl ActorSystem for MockActorSystem {
    async fn start(config: ActorSystemConfig) -> Result<Self, SystemError> where Self: Sized {
        Ok(Self::new(config))
    }

    async fn spawn_root_typed<A: Actor>(&self, actor: A, _config: A::Config) -> Result<Box<dyn ActorRef>, SystemError> {
        // For testing we directly return a MockActorRef
        let mock_ref = MockActorRef::new(format!("actor-{}", self.actors.read().unwrap().len()));
        
        {
            let mut actors = self.actors.write().unwrap();
            actors.push(mock_ref.clone_boxed());
        }
        
        Ok(mock_ref.clone_boxed())
    }

    async fn spawn_root_boxed(
        &self,
        _actor: Box<dyn Actor<Config = Box<dyn std::any::Any + Send>, Context = dyn ActorContext>>,
        _config: Box<dyn std::any::Any + Send>,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        // Simplified implementation
        Err(SystemError::ActorCreationError("Not implemented in test".to_string()))
    }

    async fn get_actor(&self, _path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        None // Simplified implementation
    }

    async fn broadcast<M: Message>(&self, _msg: M) -> Result<(), SystemError> {
        // Simplified implementation
        Ok(())
    }

    fn status(&self) -> parrot_api::system::SystemStatus {
        parrot_api::system::SystemStatus {
            state: parrot_api::system::SystemState::Running,
            active_actors: self.actors.read().unwrap().len(),
            uptime: Duration::from_secs(0),
            resources: parrot_api::system::SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: 0,
            },
        }
    }

    async fn shutdown(self) -> Result<(), SystemError> {
        {
            let mut state = self.state.write().unwrap();
            *state = false;
        }
        Ok(())
    }
}

// Mock Actor Reference
#[derive(Debug, Clone)]
struct MockActorRef {
    id: String,
    state: Arc<RwLock<ActorState>>,
    last_message: Arc<RwLock<Option<String>>>,
}

impl MockActorRef {
    fn new(id: String) -> Self {
        Self {
            id,
            state: Arc::new(RwLock::new(ActorState::Running)),
            last_message: Arc::new(RwLock::new(None)),
        }
    }
}

#[async_trait]
impl ActorRef for MockActorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if *self.state.read().unwrap() != ActorState::Running {
                return Err(ActorError::Stopped);
            }
            
            if let Some(greet) = msg.downcast_ref::<Greet>() {
                // Store last received message
                {
                    let mut last_message = self.last_message.write().unwrap();
                    *last_message = Some(greet.name.clone());
                }
                
                let response = format!("Hello {}!", greet.name);
                return Ok(Box::new(response) as BoxedMessage);
            } else if msg.downcast_ref::<FailingCommand>().is_some() {
                return Err(ActorError::MessageHandlingError("Simulated failure".to_string()));
            }
            
            // Echo back unknown messages
            Ok(msg)
        })
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        let state = self.state.clone();
        Box::pin(async move {
            let mut write_state = state.write().unwrap();
            *write_state = ActorState::Stopped;
            Ok(())
        })
    }

    fn path(&self) -> String {
        format!("test://user/{}", self.id)
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        let state = self.state.clone();
        Box::pin(async move {
            *state.read().unwrap() == ActorState::Running
        })
    }

    fn clone_boxed(&self) -> Box<dyn ActorRef> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime;
    use tokio::time::sleep;
    use std::time::Instant;
    use std::future::Future;
    use std::collections::HashMap;
    use std::pin::Pin;
    use futures::future::join_all;

    // Create test system configuration
    fn create_test_config() -> ActorSystemConfig {
        // Create decision function
        let decider = BasicDecisionFn::new(|_| SupervisionDecision::Restart);
        
        // Create strategy
        let strategy = OneForOneStrategy {
            max_restarts: 3,
            within: Duration::from_secs(30),
            decider,
        };
        
        ActorSystemConfig {
            name: "test-system".to_string(),
            runtime_config: RuntimeConfig {
                worker_threads: Some(2),
                io_threads: Some(1),
                scheduler_config: SchedulerConfig {
                    task_queue_capacity: 100,
                    task_timeout: Duration::from_secs(10),
                    load_balancing: LoadBalancingStrategy::RoundRobin,
                },
            },
            guardian_config: GuardianConfig {
                max_restarts: 3,
                restart_window: Duration::from_secs(30),
                supervision_strategy: SupervisorStrategyType::OneForOne(strategy),
            },
            timeouts: SystemTimeouts {
                actor_creation: Duration::from_secs(5),
                message_handling: Duration::from_secs(10),
                system_shutdown: Duration::from_secs(5),
            },
        }
    }

    // Test basic Actor interaction
    #[test]
    fn test_actor_interaction() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            // Initialize system
            let config = create_test_config();
            let system = MockActorSystem::start(config).await.unwrap();
            
            // Create GreeterActor
            let greeter_config = GreeterConfig {
                greeting: "Hello".to_string(),
            };
            let factory = GreeterFactory;
            let greeter = factory.create(greeter_config);
            
            // Get Actor reference
            let greeter_ref = system.spawn_root_typed(greeter, GreeterConfig::default()).await.unwrap();
            
            // Send and receive message
            let result = greeter_ref.ask(Greet { name: "World".to_string() }).await;
            assert!(result.is_ok());
            
            if let Ok(greeting) = result {
                assert!(greeting.contains("World"));
            }
            
            // Send error message
            let error_result = greeter_ref.ask(FailingCommand).await;
            assert!(error_result.is_err());
            
            // Shutdown system
            let shutdown_result = system.shutdown().await;
            assert!(shutdown_result.is_ok());
        });
    }
    
    // Test Actor lifecycle
    #[test]
    fn test_actor_lifecycle() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            // Create Actor reference
            let actor_ref = MockActorRef::new("lifecycle-test".to_string());
            
            // Test initial state
            assert!(actor_ref.is_alive().await);
            
            // Send message
            let msg = Greet { name: "Tester".to_string() };
            let result = actor_ref.ask(msg).await;
            assert!(result.is_ok());
            
            // Stop Actor
            let stop_result = actor_ref.stop().await;
            assert!(stop_result.is_ok());
            
            // Verify state
            assert!(!actor_ref.is_alive().await);
            
            // Send message to stopped Actor
            let msg = Greet { name: "Tester".to_string() };
            let result = actor_ref.ask(msg).await;
            assert!(result.is_err());
            
            if let Err(error) = result {
                match error {
                    ActorError::Stopped => {
                        // Expected
                    }
                    _ => panic!("Expected Stopped error"),
                }
            }
        });
    }
    
    // Test message priorities and options
    #[test]
    fn test_message_priorities() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            // Create message with priority
            let msg = Greet { name: "VIP".to_string() };
            assert_eq!(msg.priority(), MessagePriority::HIGH);
            
            // Create message envelope
            let options = MessageOptions {
                priority: MessagePriority::CRITICAL,
                timeout: Some(Duration::from_secs(1)),
                retry_policy: None,
            };
            
            let envelope = MessageEnvelope::new(
                msg,
                None,
                Some(options)
            );
            
            // Verify priority is set correctly
            assert_eq!(envelope.options.priority, MessagePriority::CRITICAL);
            assert_eq!(envelope.options.timeout, Some(Duration::from_secs(1)));
            
            // Check message content
            let payload = envelope.payload::<Greet>();
            assert!(payload.is_some());
            assert_eq!(payload.unwrap().name, "VIP");
        });
    }
    
    // Test supervision strategy
    #[test]
    fn test_supervision_strategy() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            let restart_strategy = DefaultStrategy::RestartOnFailure;
            let stop_strategy = DefaultStrategy::StopOnFailure;
            
            // Test restart strategy
            let restart_decision = SupervisorStrategy::handle_failure(
                &restart_strategy,
                Box::new(MockActorRef::new("test".to_string())),
                &ActorError::MessageHandlingError("Test error".to_string()),
                1
            ).await;
            
            assert_eq!(restart_decision, SupervisionDecision::Restart);
            
            // Test stop strategy
            let stop_decision = SupervisorStrategy::handle_failure(
                &stop_strategy,
                Box::new(MockActorRef::new("test".to_string())),
                &ActorError::MessageHandlingError("Test error".to_string()),
                1
            ).await;
            
            assert_eq!(stop_decision, SupervisionDecision::Stop);
        });
    }

    // Test concurrent message handling
    #[test]
    fn test_concurrent_message_handling() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            // Initialize system
            let config = create_test_config();
            let system = MockActorSystem::start(config).await.unwrap();
            
            // Create actor
            let greeter_config = GreeterConfig {
                greeting: "Hello".to_string(),
            };
            let factory = GreeterFactory;
            let greeter = factory.create(greeter_config);
            
            // Get Actor reference
            let greeter_ref = system.spawn_root_typed(greeter, GreeterConfig::default()).await.unwrap();
            
            // Send multiple messages concurrently
            let mut futures = Vec::new();
            for i in 0..100 {
                let actor_ref = greeter_ref.clone_boxed();
                let future = async move {
                    let msg = Greet { name: format!("Concurrent-{}", i) };
                    actor_ref.ask(msg).await
                };
                futures.push(future);
            }
            
            // Wait for all futures to complete
            let results = join_all(futures).await;
            
            // Verify all messages were processed
            let success_count = results.iter().filter(|r| r.is_ok()).count();
            assert_eq!(success_count, 100);
            
            // Shutdown system
            let _ = system.shutdown().await;
        });
    }
    
    // Test error recovery
    #[test]
    fn test_error_recovery() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            // Initialize system with resilient supervision strategy
            let mut config = create_test_config();
            // Create a more resilient supervision strategy
            let decider = BasicDecisionFn::new(|_| SupervisionDecision::Restart);
            let strategy = OneForOneStrategy {
                max_restarts: 10, // Allow more restarts
                within: Duration::from_secs(10),
                decider,
            };
            config.guardian_config.supervision_strategy = SupervisorStrategyType::OneForOne(strategy);
            
            let system = MockActorSystem::start(config).await.unwrap();
            
            // Create actor
            let greeter_ref = system.spawn_root_typed(
                GreeterActor {
                    state: ActorState::Starting,
                    greeting: "Hello".to_string(),
                    message_count: 0,
                }, 
                GreeterConfig::default()
            ).await.unwrap();
            
            // Send failing command followed by normal message
            let _ = greeter_ref.ask(FailingCommand).await; // This should fail
            
            // Wait a moment for recovery
            sleep(Duration::from_millis(100)).await;
            
            // Send normal message after failure
            let result = greeter_ref.ask(Greet { name: "After-Failure".to_string() }).await;
            
            // Verify system recovered
            assert!(result.is_ok());
            
            // Shutdown system
            let _ = system.shutdown().await;
        });
    }
    
    // Test system configuration impact
    #[test]
    fn test_system_configuration() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            // Test with different configurations
            let configs = vec![
                // Minimal configuration
                ActorSystemConfig {
                    name: "minimal-system".to_string(),
                    runtime_config: RuntimeConfig {
                        worker_threads: Some(1),
                        io_threads: Some(1),
                        scheduler_config: SchedulerConfig {
                            task_queue_capacity: 10,
                            task_timeout: Duration::from_secs(5),
                            load_balancing: LoadBalancingStrategy::RoundRobin,
                        },
                    },
                    guardian_config: GuardianConfig {
                        max_restarts: 1,
                        restart_window: Duration::from_secs(10),
                        supervision_strategy: SupervisorStrategyType::OneForOne(
                            OneForOneStrategy {
                                max_restarts: 1,
                                within: Duration::from_secs(10),
                                decider: BasicDecisionFn::new(|_| SupervisionDecision::Stop),
                            }
                        ),
                    },
                    timeouts: SystemTimeouts {
                        actor_creation: Duration::from_secs(1),
                        message_handling: Duration::from_secs(2),
                        system_shutdown: Duration::from_secs(1),
                    },
                },
                
                // Optimal configuration
                ActorSystemConfig {
                    name: "optimal-system".to_string(),
                    runtime_config: RuntimeConfig {
                        worker_threads: Some(4),
                        io_threads: Some(2),
                        scheduler_config: SchedulerConfig {
                            task_queue_capacity: 1000,
                            task_timeout: Duration::from_secs(30),
                            load_balancing: LoadBalancingStrategy::LeastLoaded,
                        },
                    },
                    guardian_config: GuardianConfig {
                        max_restarts: 5,
                        restart_window: Duration::from_secs(60),
                        supervision_strategy: SupervisorStrategyType::OneForOne(
                            OneForOneStrategy {
                                max_restarts: 5,
                                within: Duration::from_secs(60),
                                decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
                            }
                        ),
                    },
                    timeouts: SystemTimeouts {
                        actor_creation: Duration::from_secs(10),
                        message_handling: Duration::from_secs(20),
                        system_shutdown: Duration::from_secs(10),
                    },
                },
            ];
            
            // Test each configuration
            for config in configs {
                let system = MockActorSystem::start(config.clone()).await.unwrap();
                
                // Create actor
                let greeter_ref = system.spawn_root_typed(
                    GreeterActor {
                        state: ActorState::Starting,
                        greeting: "Hello".to_string(),
                        message_count: 0,
                    }, 
                    GreeterConfig::default()
                ).await.unwrap();
                
                // Verify basic functionality works
                let result = greeter_ref.ask(Greet { name: "Config-Test".to_string() }).await;
                assert!(result.is_ok());
                
                // Shutdown system
                let _ = system.shutdown().await;
            }
        });
    }
    
    // Test message routing patterns
    #[test]
    fn test_message_routing() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            // Initialize system
            let config = create_test_config();
            let system = MockActorSystem::start(config).await.unwrap();
            
            // Create multiple actors
            let actor_count = 5;
            let mut actor_refs = Vec::new();
            
            for i in 0..actor_count {
                let greeter = GreeterActor {
                    state: ActorState::Starting,
                    greeting: format!("Hello from Actor {}", i),
                    message_count: 0,
                };
                
                let actor_ref = system.spawn_root_typed(greeter, GreeterConfig::default()).await.unwrap();
                actor_refs.push(actor_ref);
            }
            
            // Implement different routing patterns
            
            // 1. Broadcast to all actors
            for actor_ref in &actor_refs {
                let result = actor_ref.ask(Greet { name: "Broadcast".to_string() }).await;
                assert!(result.is_ok());
            }
            
            // 2. Round-robin routing
            for i in 0..10 {
                let actor_index = i % actor_count;
                let actor_ref = &actor_refs[actor_index];
                let result = actor_ref.ask(Greet { name: format!("Round-Robin-{}", i) }).await;
                assert!(result.is_ok());
            }
            
            // 3. Content-based routing (simplified)
            let content_router = move |content: &str| -> usize {
                match content.len() % 3 {
                    0 => 0,
                    1 => 1,
                    _ => 2,
                }
            };
            
            let messages = vec!["Short", "Medium message", "This is a longer message"];
            
            for msg in messages {
                let actor_index = content_router(msg);
                let actor_ref = &actor_refs[actor_index];
                let result = actor_ref.ask(Greet { name: msg.to_string() }).await;
                assert!(result.is_ok());
            }
            
            // Shutdown system
            let _ = system.shutdown().await;
        });
    }
    
    // Test performance benchmarking
    #[test]
    fn test_performance_benchmarking() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            // Initialize system
            let config = create_test_config();
            let system = MockActorSystem::start(config).await.unwrap();
            
            // Create actor
            let greeter_ref = system.spawn_root_typed(
                GreeterActor {
                    state: ActorState::Starting,
                    greeting: "Hello".to_string(),
                    message_count: 0,
                }, 
                GreeterConfig::default()
            ).await.unwrap();
            
            // Measure throughput - how many messages can be processed per second
            let message_count = 1000;
            let start_time = Instant::now();
            
            // Send messages sequentially
            for i in 0..message_count {
                let result = greeter_ref.ask(Greet { name: format!("Perf-{}", i) }).await;
                assert!(result.is_ok());
            }
            
            let elapsed = start_time.elapsed();
            let throughput = message_count as f64 / elapsed.as_secs_f64();
            
            // Log performance metrics (in a real test, we might assert against expected values)
            println!("Sequential throughput: {:.2} messages/sec", throughput);
            
            // Measure latency
            let start_time = Instant::now();
            let result = greeter_ref.ask(Greet { name: "Latency-Test".to_string() }).await;
            let latency = start_time.elapsed();
            assert!(result.is_ok());
            
            println!("Message latency: {:?}", latency);
            
            // Shutdown system
            let _ = system.shutdown().await;
        });
    }
    
    // Test actor hierarchy and supervision
    #[test]
    fn test_actor_hierarchy() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            // Initialize system
            let config = create_test_config();
            let system = MockActorSystem::start(config).await.unwrap();
            
            // Create parent actor
            let parent_ref = system.spawn_root_typed(
                GreeterActor {
                    state: ActorState::Starting,
                    greeting: "Hello from Parent".to_string(),
                    message_count: 0,
                }, 
                GreeterConfig::default()
            ).await.unwrap();
            
            // In a real system, the parent would spawn children
            // We'll simulate this with our mock system
            let child_refs = vec![
                MockActorRef::new("child-1".to_string()),
                MockActorRef::new("child-2".to_string()),
                MockActorRef::new("child-3".to_string()),
            ];
            
            // Test parent-child communication patterns
            
            // 1. Parent broadcasts to all children
            for child_ref in &child_refs {
                let msg = Greet { name: "Child".to_string() };
                let result = child_ref.ask(msg).await;
                assert!(result.is_ok());
            }
            
            // 2. Children report to parent
            let parent_msg = Greet { name: "Parent".to_string() };
            let result = parent_ref.ask(parent_msg).await;
            assert!(result.is_ok());
            
            // 3. Test child failure and supervision
            // Simulate child failure
            let failing_child = child_refs[0].clone();
            let _ = failing_child.ask(FailingCommand).await;
            
            // Verify child was "restarted" (in a mock way)
            let new_message = Greet { name: "After-Restart".to_string() };
            let result = failing_child.ask(new_message).await;
            
            // In our mock, the actor won't really restart but we can check basic functionality
            assert!(result.is_ok());
            
            // Shutdown system
            let _ = system.shutdown().await;
        });
    }
    
    // Test error conditions
    #[test]
    fn test_error_conditions() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            // Create Actor reference
            let actor_ref = MockActorRef::new("error-test".to_string());
            
            // Test timeout error (simulation)
            // In a real implementation, we would use actual timeouts
            let timeout_simulation = async {
                sleep(Duration::from_millis(10)).await;
                Err(ActorError::Timeout) as Result<(), ActorError>
            };
            
            let timeout_result: Result<(), ActorError> = timeout_simulation.await;
            assert!(matches!(timeout_result, Err(ActorError::Timeout)));
            
            // Test invalid state error
            let _ = actor_ref.stop().await;
            let result = actor_ref.ask(Greet { name: "Invalid".to_string() }).await;
            assert!(matches!(result, Err(ActorError::Stopped)));
            
            // Test message handling error
            let system_error_simulation = async {
                sleep(Duration::from_millis(10)).await;
                Err(ActorError::MessageHandlingError("Simulated system error".to_string())) as Result<(), ActorError>
            };
            
            let system_error_result: Result<(), ActorError> = system_error_simulation.await;
            assert!(matches!(system_error_result, Err(ActorError::MessageHandlingError(_))));
        });
    }

    // Create custom message type for testing different message handling scenarios
    #[derive(Debug, Clone)]
    struct DelayedMessage {
        name: String,
        delay_ms: u64,
    }
    
    impl Message for DelayedMessage {
        type Result = String;
        
        fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
            if let Ok(response) = result.downcast::<String>() {
                Ok(*response)
            } else {
                Err(ActorError::MessageHandlingError("Failed to extract result".to_string()))
            }
        }
    }
    
    // Test custom message handling
    #[test]
    fn test_custom_message_types() {
        let rt = Runtime::new().unwrap();
        
        rt.block_on(async {
            // Create Actor reference
            let actor_ref = MockActorRef::new("custom-message-test".to_string());
            
            // Test structured message
            let greet_msg = Greet { name: "Structure".to_string() };
            let result = actor_ref.ask(greet_msg).await;
            assert!(result.is_ok());
            
            // Create wrapped message
            let envelope = MessageEnvelope::new(
                Greet { name: "Wrapped".to_string() },
                None,
                Some(MessageOptions {
                    priority: MessagePriority::HIGH,
                    timeout: Some(Duration::from_secs(5)),
                    retry_policy: None,
                })
            );
            
            // In a real system, we would process this envelope, here we just demonstrate the API
            assert_eq!(envelope.options.priority, MessagePriority::HIGH);
            
            // Stop Actor
            let _ = actor_ref.stop().await;
        });
    }
} 