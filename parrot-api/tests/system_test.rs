use parrot_api::system::{ActorSystem, ActorSystemConfig, SystemTimeouts, GuardianConfig, SystemStatus, SystemState, SystemResources, SystemError};
use parrot_api::runtime::{RuntimeConfig, SchedulerConfig, LoadBalancingStrategy};
use parrot_api::actor::{Actor, ActorState, ActorConfig};
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture, BoxedActorRef};
use parrot_api::supervisor::{SupervisorStrategyType, SupervisorStrategy, OneForOneStrategy, SupervisionDecision, BasicDecisionFn};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::context::ActorContext;
use std::time::Duration;
use std::sync::{Arc, Mutex, RwLock};
use std::any::Any;
use async_trait::async_trait;
use std::ptr::NonNull;

// Mock Context for tests
#[derive(Default)]
struct MockContext;

// Test actor config
#[derive(Default)]
struct TestActorConfig;

impl ActorConfig for TestActorConfig {}

// Test actor implementation
struct TestActor {
    state: ActorState,
    processed_messages: usize,
}

impl Default for TestActor {
    fn default() -> Self {
        Self {
            state: ActorState::Starting,
            processed_messages: 0,
        }
    }
}

impl Actor for TestActor {
    type Config = TestActorConfig;
    type Context = MockContext;

    fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.processed_messages += 1;
            Ok(msg) // Echo back the message
        })
    }

    // This is not used in the test actor
    fn receive_message_with_engine<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context, _engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        None
    }

    fn state(&self) -> ActorState {
        self.state
    }
}

// Test message
#[derive(Debug, Clone)]
struct TestMessage(String);

impl Message for TestMessage {
    type Result = String;
    
    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        if let Ok(response) = result.downcast::<String>() {
            Ok(*response)
        } else {
            Err(ActorError::MessageHandlingError("Failed to extract result".to_string()))
        }
    }
}

// Mock actor system implementation
struct MockActorSystem {
    config: ActorSystemConfig,
    actors: Arc<RwLock<Vec<BoxedActorRef>>>,
    state: Arc<RwLock<SystemState>>,
    uptime: Arc<RwLock<Duration>>,
}

impl MockActorSystem {
    fn new(config: ActorSystemConfig) -> Self {
        Self {
            config,
            actors: Arc::new(RwLock::new(Vec::new())),
            state: Arc::new(RwLock::new(SystemState::Starting)),
            uptime: Arc::new(RwLock::new(Duration::from_secs(0))),
        }
    }
    
    // Helper to stop all actors safely without holding RwLock across await points
    async fn stop_all_actors(&self) -> Result<(), SystemError> {
        // First collect all actors into a Vec while holding the lock
        let actor_refs: Vec<BoxedActorRef> = {
            let actors = self.actors.read().unwrap();
            actors.iter().map(|a| a.clone_boxed()).collect()
        };
        
        // Then stop each actor without holding the lock
        for actor in actor_refs {
            let _ = actor.stop().await;
        }
        
        Ok(())
    }
}

// Implementation of MockActorRef for testing
#[derive(Debug)]
struct MockActorRef {
    id: String,
    path: String,
    is_alive: Arc<RwLock<bool>>,
}

impl MockActorRef {
    fn new(id: &str, path: &str) -> Self {
        Self {
            id: id.to_string(),
            path: path.to_string(),
            is_alive: Arc::new(RwLock::new(true)),
        }
    }
}

#[async_trait]
impl ActorRef for MockActorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if !*self.is_alive.read().unwrap() {
                return Err(ActorError::Stopped);
            }
            // Echo back the message
            Ok(msg)
        })
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        let is_alive = self.is_alive.clone();
        Box::pin(async move {
            let mut state = is_alive.write().unwrap();
            *state = false;
            Ok(())
        })
    }

    fn path(&self) -> String {
        self.path.clone()
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        let is_alive = self.is_alive.clone();
        Box::pin(async move {
            *is_alive.read().unwrap()
        })
    }

    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            id: self.id.clone(),
            path: self.path.clone(),
            is_alive: self.is_alive.clone(),
        })
    }
}

#[async_trait]
impl ActorSystem for MockActorSystem {
    async fn start(config: ActorSystemConfig) -> Result<Self, SystemError> where Self: Sized {
        let system = Self::new(config);
        {
            let mut state = system.state.write().unwrap();
            *state = SystemState::Running;
        }
        Ok(system)
    }

    async fn spawn_root_typed<A: Actor>(&self, _actor: A, _config: A::Config) -> Result<Box<dyn ActorRef>, SystemError> {
        let actor_ref = Box::new(MockActorRef::new("test", &format!("{}/user/test", self.config.name)));
        {
            let mut actors = self.actors.write().unwrap();
            actors.push(actor_ref.clone_boxed());
        }
        Ok(actor_ref)
    }

    async fn spawn_root_boxed(
        &self,
        _actor: Box<dyn Actor<Config = Box<dyn Any + Send>, Context = dyn ActorContext>>,
        _config: Box<dyn Any + Send>,
    ) -> Result<Box<dyn ActorRef>, SystemError> {
        let actor_ref = Box::new(MockActorRef::new("test-boxed", &format!("{}/user/test-boxed", self.config.name)));
        {
            let mut actors = self.actors.write().unwrap();
            actors.push(actor_ref.clone_boxed());
        }
        Ok(actor_ref)
    }

    async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        let actors = self.actors.read().unwrap();
        for actor in actors.iter() {
            if actor.path() == path.path {
                return Some(actor.clone_boxed());
            }
        }
        None
    }

    async fn broadcast<M: Message>(&self, _msg: M) -> Result<(), SystemError> {
        let actors = self.actors.read().unwrap();
        if actors.is_empty() {
            return Err(SystemError::ActorCreationError("No actors to broadcast to".to_string()));
        }
        Ok(())
    }

    fn status(&self) -> SystemStatus {
        SystemStatus {
            state: *self.state.read().unwrap(),
            active_actors: self.actors.read().unwrap().len(),
            uptime: *self.uptime.read().unwrap(),
            resources: SystemResources {
                cpu_usage: 10.0,
                memory_usage: 1024 * 1024,  // 1MB
                thread_count: 4,
            },
        }
    }

    async fn shutdown(self) -> Result<(), SystemError> {
        // Stop all actors safely
        self.stop_all_actors().await?;
        
        // Update system state
        {
            let mut state = self.state.write().unwrap();
            *state = SystemState::Stopped;
        }
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime;

    // Helper to create system config
    fn create_system_config() -> ActorSystemConfig {
        // 创建一个基础的决策函数
        let decider = BasicDecisionFn::new(|error: &ActorError| {
            SupervisionDecision::Restart
        });

        // 创建一个简单的OneForOneStrategy
        let one_for_one = OneForOneStrategy {
            max_restarts: 3,
            within: Duration::from_secs(30),
            decider,
        };
        
        ActorSystemConfig {
            name: "test-system".to_string(),
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
                max_restarts: 3,
                restart_window: Duration::from_secs(30),
                supervision_strategy: SupervisorStrategyType::OneForOne(one_for_one),
            },
            timeouts: SystemTimeouts {
                actor_creation: Duration::from_secs(5),
                message_handling: Duration::from_secs(30),
                system_shutdown: Duration::from_secs(10),
            },
        }
    }

    // Test system creation
    #[test]
    fn test_system_creation() {
        let rt = Runtime::new().unwrap();
        let config = create_system_config();
        
        let system = rt.block_on(async {
            MockActorSystem::start(config).await
        });
        
        assert!(system.is_ok());
        let system = system.unwrap();
        
        // Verify system is in running state
        let status = system.status();
        assert_eq!(status.state, SystemState::Running);
        assert_eq!(status.active_actors, 0);
    }

    // Test actor spawning
    #[test]
    fn test_actor_spawn() {
        let rt = Runtime::new().unwrap();
        let config = create_system_config();
        
        let result = rt.block_on(async {
            let system = MockActorSystem::start(config).await?;
            
            // Spawn an actor
            let actor = system.spawn_root_typed(TestActor::default(), TestActorConfig::default()).await?;
            
            // Verify actor is created
            assert!(actor.is_alive().await);
            
            let status = system.status();
            assert_eq!(status.active_actors, 1);
            
            Ok::<_, SystemError>(actor)
        });
        
        assert!(result.is_ok());
    }

    // Test actor message passing
    #[test]
    fn test_actor_messaging() {
        let rt = Runtime::new().unwrap();
        let config = create_system_config();
        
        let result = rt.block_on(async {
            let system = MockActorSystem::start(config).await?;
            
            // Spawn an actor
            let actor = system.spawn_root_typed(TestActor::default(), TestActorConfig::default()).await?;
            
            // Send a message
            let msg = Box::new(TestMessage("Hello".to_string())) as BoxedMessage;
            let response = actor.send(msg).await?;
            
            // Verify response
            let unpacked = response.downcast::<TestMessage>();
            assert!(unpacked.is_ok());
            assert_eq!(unpacked.unwrap().0, "Hello");
            
            Ok::<_, SystemError>(())
        });
        
        assert!(result.is_ok());
    }

    // Test system shutdown
    #[test]
    fn test_system_shutdown() {
        let rt = Runtime::new().unwrap();
        let config = create_system_config();
        
        let result = rt.block_on(async {
            let system = MockActorSystem::start(config).await?;
            
            // Spawn some actors
            let actor1 = system.spawn_root_typed(TestActor::default(), TestActorConfig::default()).await?;
            let actor2 = system.spawn_root_typed(TestActor::default(), TestActorConfig::default()).await?;
            
            // Verify actors are alive
            assert!(actor1.is_alive().await);
            assert!(actor2.is_alive().await);
            
            // Shutdown the system
            system.shutdown().await?;
            
            // Verify actors are stopped (this would need to be checked in a real system)
            
            Ok::<_, SystemError>(())
        });
        
        assert!(result.is_ok());
    }

    // Test system status
    #[test]
    fn test_system_status() {
        let rt = Runtime::new().unwrap();
        let config = create_system_config();
        
        let result = rt.block_on(async {
            let system = MockActorSystem::start(config).await?;
            
            // Spawn some actors
            let _actor1 = system.spawn_root_typed(TestActor::default(), TestActorConfig::default()).await?;
            let _actor2 = system.spawn_root_typed(TestActor::default(), TestActorConfig::default()).await?;
            
            // Check status
            let status = system.status();
            assert_eq!(status.state, SystemState::Running);
            assert_eq!(status.active_actors, 2);
            assert!(status.resources.cpu_usage > 0.0);
            assert!(status.resources.memory_usage > 0);
            assert!(status.resources.thread_count > 0);
            
            Ok::<_, SystemError>(())
        });
        
        assert!(result.is_ok());
    }

    // Test actor with default configuration
    #[test]
    fn test_actor_with_default_config() {
        let rt = Runtime::new().unwrap();
        let config = create_system_config();
        
        let result = rt.block_on(async {
            let system = MockActorSystem::start(config).await?;
            
            // Create an actor using the default configuration
            let actor = TestActor::default();
            
            // Spawn a root actor with the default configuration
            let config = TestActorConfig::default();
            let actor_ref = system.spawn_root_typed(actor, config).await?;
            
            // Verify that the actor is alive
            assert!(actor_ref.is_alive().await);
            
            // Check that only one actor is active in the system
            let status = system.status();
            assert_eq!(status.active_actors, 1);
            
            // Send a test message and await the response
            let msg = Box::new(TestMessage("DefaultConfigTest".to_string())) as BoxedMessage;
            let response = actor_ref.send(msg).await?;
            
            // Unpack and verify the response
            let unpacked = response.downcast::<TestMessage>();
            assert!(unpacked.is_ok());
            assert_eq!(unpacked.unwrap().0, "DefaultConfigTest");
            
            Ok::<_, SystemError>(())
        });
        
        assert!(result.is_ok());
    }
} 