use parrot_api::context::{ActorContext, ActorContextMessage, LifecycleEvent, ScheduledTask, ActorSpawner, ActorSpawnerExt};
use parrot_api::address::{ActorRef, ActorPath};
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture, BoxedActorRef, WeakActorTarget};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::stream::StreamRegistry;
use parrot_api::supervisor::{SupervisorStrategyType, OneForOneStrategy, BasicDecisionFn, SupervisionDecision};
use parrot_api::actor::{Actor, ActorState, ActorConfig};

use std::time::{Duration, SystemTime};
use std::sync::{Arc, RwLock};
use std::collections::{HashMap, HashSet};
use std::any::Any;
use std::fmt::Debug;
use tokio::runtime::Runtime;
use async_trait::async_trait;
use uuid::Uuid;
use futures::Stream;
use std::ptr::NonNull;
// Test message types
#[derive(Debug, Clone, PartialEq)]
struct TestMessage {
    data: u32,
}

impl Message for TestMessage {
    type Result = u32;
    
    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        if let Ok(value) = result.downcast::<u32>() {
            Ok(*value)
        } else {
            Err(ActorError::MessageHandlingError("Failed to extract result".to_string()))
        }
    }
}

// Simple test actor implementation
struct TestActor {
    state: ActorState,
    counter: u32,
}

impl TestActor {
    fn new() -> Self {
        Self {
            state: ActorState::Starting,
            counter: 0,
        }
    }
}

// Empty config for test actor
#[derive(Debug)]
struct TestActorConfig;
impl ActorConfig for TestActorConfig {}

impl Actor for TestActor {
    type Config = TestActorConfig;
    type Context = MockActorContext;
    
    fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
                self.counter += test_msg.data;
                Ok(Box::new(self.counter) as BoxedMessage)
            } else {
                Err(ActorError::MessageHandlingError("Unknown message type".to_string()))
            }
        })
    }

    fn receive_message_with_engine<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context, _engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        None
    }
    
    fn state(&self) -> ActorState {
        self.state
    }
}

// Mock of ActorRef for testing
#[derive(Debug, Clone)]
struct MockActorRef {
    id: String,
    messages: Arc<RwLock<Vec<TestMessage>>>,
    alive: Arc<RwLock<bool>>,
}

impl MockActorRef {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            messages: Arc::new(RwLock::new(Vec::new())),
            alive: Arc::new(RwLock::new(true)),
        }
    }
    
    fn get_messages(&self) -> Vec<TestMessage> {
        self.messages.read().unwrap().clone()
    }
}

#[async_trait]
impl ActorRef for MockActorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let alive = self.alive.clone();
        let messages = self.messages.clone();
        
        Box::pin(async move {
            if !*alive.read().unwrap() {
                return Err(ActorError::Stopped);
            }
            
            if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
                messages.write().unwrap().push(test_msg.clone());
                
                // Return doubled value as response
                let response = test_msg.data * 2;
                Ok(Box::new(response) as BoxedMessage)
            } else {
                Ok(msg)
            }
        })
    }
    
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        let alive = self.alive.clone();
        Box::pin(async move {
            *alive.write().unwrap() = false;
            Ok(())
        })
    }
    
    fn path(&self) -> String {
        format!("test://actor/{}", self.id)
    }
    
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        let alive = self.alive.clone();
        Box::pin(async move {
            *alive.read().unwrap()
        })
    }
    
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
}

// Mock StreamRegistry implementation
struct MockStreamRegistry {
    registered_streams: Vec<String>, // Just store some identifier for testing
}

impl MockStreamRegistry {
    fn new() -> Self {
        Self {
            registered_streams: Vec::new(),
        }
    }
    
    fn count_streams(&self) -> usize {
        self.registered_streams.len()
    }
}

impl StreamRegistry for MockStreamRegistry {
    fn add_stream_erased(
        &mut self,
        _stream: Box<dyn Stream<Item = Box<dyn Any + Send>> + Send>,
    ) -> Result<(), ActorError> {
        self.registered_streams.push("stream".to_string());
        Ok(())
    }

    fn add_stream_with_handler_erased(
        &mut self,
        _stream: Box<dyn Stream<Item = Box<dyn Any + Send>> + Send>,
        _handler: Box<dyn Any + Send>,
    ) -> Result<(), ActorError> {
        self.registered_streams.push("stream_with_handler".to_string());
        Ok(())
    }
}

// Mock ActorSpawner implementation
struct MockActorSpawner {
    spawned_actors: Arc<RwLock<Vec<String>>>, // Just store IDs for testing
}

impl MockActorSpawner {
    fn new() -> Self {
        Self {
            spawned_actors: Arc::new(RwLock::new(Vec::new())),
        }
    }
    
    fn count_actors(&self) -> usize {
        self.spawned_actors.read().unwrap().len()
    }
}

#[async_trait]
impl ActorSpawner for MockActorSpawner {
    fn spawn<'a>(&'a self, _actor: BoxedMessage, _config: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let actor_ref = MockActorRef::new("spawned");
        self.spawned_actors.write().unwrap().push("spawned".to_string());
        Box::pin(async move { Ok(Box::new(actor_ref) as BoxedActorRef) })
    }
    
    fn spawn_with_strategy<'a>(&'a self, 
        _actor: BoxedMessage, 
        _config: BoxedMessage, 
        _strategy: SupervisorStrategyType
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        let actor_ref = MockActorRef::new("supervised");
        self.spawned_actors.write().unwrap().push("supervised".to_string());
        Box::pin(async move { Ok(Box::new(actor_ref) as BoxedActorRef) })
    }
}

// Create a default decision function for tests
fn create_default_decider() -> BasicDecisionFn {
    BasicDecisionFn::new(|error: &ActorError| match error {
        ActorError::Timeout => SupervisionDecision::Restart,
        ActorError::Stopped => SupervisionDecision::Stop,
        _ => SupervisionDecision::Escalate,
    })
}

// Mock ActorContext implementation
struct MockActorContext {
    self_ref: MockActorRef,
    parent: Option<MockActorRef>,
    children: Arc<RwLock<Vec<MockActorRef>>>,
    path: ActorPath,
    receive_timeout: Option<Duration>,
    supervisor_strategy: SupervisorStrategyType,
    stream_registry: MockStreamRegistry,
    spawner: MockActorSpawner,
    watched_actors: HashSet<String>,
}

impl MockActorContext {
    fn new(id: &str) -> Self {
        let self_ref = MockActorRef::new(id);
        
        let path = ActorPath {
            target: Arc::new(self_ref.clone()) as WeakActorTarget,
            path: format!("test://context/{}", id),
        };
        
        // Create a one-for-one strategy for testing
        let strategy = OneForOneStrategy {
            max_restarts: 3,
            within: Duration::from_secs(5),
            decider: create_default_decider(),
        };
        
        Self {
            self_ref,
            parent: None,
            children: Arc::new(RwLock::new(Vec::new())),
            path,
            receive_timeout: None,
            supervisor_strategy: SupervisorStrategyType::OneForOne(strategy),
            stream_registry: MockStreamRegistry::new(),
            spawner: MockActorSpawner::new(),
            watched_actors: HashSet::new(),
        }
    }
    
    fn with_parent(mut self, parent: MockActorRef) -> Self {
        self.parent = Some(parent);
        self
    }
    
    fn add_child(&mut self, child: MockActorRef) {
        self.children.write().unwrap().push(child);
    }
}

impl ActorContext for MockActorContext {
    fn get_self_ref(&self) -> BoxedActorRef {
        Box::new(self.self_ref.clone())
    }
    
    fn stop<'a>(&'a mut self) -> BoxedFuture<'a, ActorResult<()>> {
        let self_ref = self.self_ref.clone();
        Box::pin(async move {
            self_ref.stop().await
        })
    }
    
    fn send<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let _ = target.send(msg).await?;
            Ok(())
        })
    }
    
    fn ask<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            target.send(msg).await
        })
    }
    
    fn schedule_once<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage, _delay: Duration) -> BoxedFuture<'a, ActorResult<()>> {
        // For testing, just send immediately
        Box::pin(async move {
            let _ = target.send(msg).await?;
            Ok(())
        })
    }
    
    fn schedule_periodic<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage, _initial_delay: Duration, _interval: Duration) -> BoxedFuture<'a, ActorResult<()>> {
        // For testing, just send once
        Box::pin(async move {
            let _ = target.send(msg).await?;
            Ok(())
        })
    }
    
    fn watch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        self.watched_actors.insert(target.path());
        Box::pin(async { Ok(()) })
    }
    
    fn unwatch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        self.watched_actors.remove(&target.path());
        Box::pin(async { Ok(()) })
    }
    
    fn parent(&self) -> Option<BoxedActorRef> {
        self.parent.as_ref().map(|p| Box::new(p.clone()) as BoxedActorRef)
    }
    
    fn children(&self) -> Vec<BoxedActorRef> {
        self.children.read().unwrap().iter().map(|c| Box::new(c.clone()) as BoxedActorRef).collect()
    }
    
    fn set_receive_timeout(&mut self, timeout: Option<Duration>) {
        self.receive_timeout = timeout;
    }
    
    fn receive_timeout(&self) -> Option<Duration> {
        self.receive_timeout
    }
    
    fn set_supervisor_strategy(&mut self, strategy: SupervisorStrategyType) {
        self.supervisor_strategy = strategy;
    }
    
    fn path(&self) -> &ActorPath {
        &self.path
    }
    
    fn stream_registry(&mut self) -> &mut dyn StreamRegistry {
        &mut self.stream_registry
    }
    
    fn spawner(&mut self) -> &mut dyn ActorSpawner {
        &mut self.spawner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::iter;
    
    // Test ActorContext basic functionality
    #[test]
    fn test_actor_context_basic() {
        let ctx = MockActorContext::new("test");
        
        // Test self reference
        let self_ref = ctx.get_self_ref();
        assert_eq!(self_ref.path(), "test://actor/test");
        
        // Test path
        assert_eq!(ctx.path().path, "test://context/test");
        
        // Test timeout
        assert_eq!(ctx.receive_timeout(), None);
        
        // Test parent/children
        assert!(ctx.parent().is_none());
        assert_eq!(ctx.children().len(), 0);
    }
    
    // Test ActorContext parent/child relationships
    #[test]
    fn test_actor_context_hierarchy() {
        let parent_ref = MockActorRef::new("parent");
        let mut ctx = MockActorContext::new("child").with_parent(parent_ref);
        
        // Test parent link
        let parent = ctx.parent().unwrap();
        assert_eq!(parent.path(), "test://actor/parent");
        
        // Test child management
        let child1 = MockActorRef::new("child1");
        let child2 = MockActorRef::new("child2");
        
        ctx.add_child(child1.clone());
        ctx.add_child(child2.clone());
        
        let children = ctx.children();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].path(), "test://actor/child1");
        assert_eq!(children[1].path(), "test://actor/child2");
    }
    
    // Test ActorContext message passing
    #[test]
    fn test_actor_context_messaging() {
        let rt = Runtime::new().unwrap();
        let ctx = MockActorContext::new("sender");
        let target = Box::new(MockActorRef::new("receiver")) as BoxedActorRef;
        
        rt.block_on(async {
            // Test send
            let msg = TestMessage { data: 5 };
            let result = ctx.send(target.clone_boxed(), Box::new(msg.clone())).await;
            assert!(result.is_ok());
            
            // Test ask
            let ask_msg = TestMessage { data: 7 };
            let response = ctx.ask(target.clone_boxed(), Box::new(ask_msg.clone())).await.unwrap();
            
            // Verify response
            let value = *response.downcast::<u32>().unwrap();
            assert_eq!(value, 14); // Should be doubled
        });
    }
    
    // Test ActorContext scheduling
    #[test]
    fn test_actor_context_scheduling() {
        let rt = Runtime::new().unwrap();
        let ctx = MockActorContext::new("scheduler");
        let target = Box::new(MockActorRef::new("target")) as BoxedActorRef;
        
        rt.block_on(async {
            // Test schedule_once
            let msg1 = TestMessage { data: 10 };
            let result = ctx.schedule_once(
                target.clone_boxed(), 
                Box::new(msg1),
                Duration::from_millis(100)
            ).await;
            assert!(result.is_ok());
            
            // Test schedule_periodic
            let msg2 = TestMessage { data: 20 };
            let result = ctx.schedule_periodic(
                target.clone_boxed(),
                Box::new(msg2),
                Duration::from_millis(50),
                Duration::from_millis(200)
            ).await;
            assert!(result.is_ok());
        });
    }
    
    // Test actor watching
    #[test]
    fn test_actor_context_watching() {
        let rt = Runtime::new().unwrap();
        let mut ctx = MockActorContext::new("watcher");
        let target1 = Box::new(MockActorRef::new("watched1")) as BoxedActorRef;
        let target2 = Box::new(MockActorRef::new("watched2")) as BoxedActorRef;
        
        rt.block_on(async {
            // Watch actors
            let _ = ctx.watch(target1.clone_boxed()).await;
            let _ = ctx.watch(target2.clone_boxed()).await;
            
            // Verify watched
            assert!(ctx.watched_actors.contains(&target1.path()));
            assert!(ctx.watched_actors.contains(&target2.path()));
            
            // Unwatch one
            let _ = ctx.unwatch(target1.clone_boxed()).await;
            
            // Verify result
            assert!(!ctx.watched_actors.contains(&target1.path()));
            assert!(ctx.watched_actors.contains(&target2.path()));
        });
    }
    
    // Test timeout settings
    #[test]
    fn test_receive_timeout() {
        let mut ctx = MockActorContext::new("timeout-test");
        
        // Default is None
        assert_eq!(ctx.receive_timeout(), None);
        
        // Set timeout
        let timeout = Duration::from_secs(5);
        ctx.set_receive_timeout(Some(timeout));
        assert_eq!(ctx.receive_timeout(), Some(timeout));
        
        // Clear timeout
        ctx.set_receive_timeout(None);
        assert_eq!(ctx.receive_timeout(), None);
    }
    
    // Test supervisor strategy
    #[test]
    fn test_supervisor_strategy() {
        let mut ctx = MockActorContext::new("supervisor-test");
        
        // Default is OneForOne
        match ctx.supervisor_strategy {
            SupervisorStrategyType::OneForOne(_) => (),
            _ => panic!("Expected OneForOne strategy"),
        }
        
        // Set to a different strategy
        let new_strategy = OneForOneStrategy {
            max_restarts: 5,
            within: Duration::from_secs(10),
            decider: create_default_decider(),
        };
        ctx.set_supervisor_strategy(SupervisorStrategyType::OneForOne(new_strategy));
        
        // Verify
        match ctx.supervisor_strategy {
            SupervisorStrategyType::OneForOne(_) => (),
            _ => panic!("Expected OneForOne strategy"),
        }
    }
    
    // Test stream registry
    #[test]
    fn test_stream_registry() {
        let mut ctx = MockActorContext::new("stream-test");
        
        // Initial state
        assert_eq!(ctx.stream_registry.count_streams(), 0);
        
        // Register a stream
        let items = vec![1, 2, 3];
        let stream = iter(items.into_iter().map(|i| Box::new(i) as Box<dyn Any + Send>));
        
        let result = ctx.stream_registry().add_stream_erased(Box::new(stream));
        assert!(result.is_ok());
        
        // Verify registration
        assert_eq!(ctx.stream_registry.count_streams(), 1);
    }
    
    // Test actor spawning
    #[test]
    fn test_actor_spawning() {
        let rt = Runtime::new().unwrap();
        let mut ctx = MockActorContext::new("spawner-test");
        
        // Initial state
        assert_eq!(ctx.spawner.count_actors(), 0);
        
        rt.block_on(async {
            // Spawn an actor
            let result = ctx.spawner().spawn(
                Box::new(TestActor::new()),
                Box::new(TestActorConfig)
            ).await;
            
            assert!(result.is_ok());
            
            // Verify actor was spawned
            assert_eq!(ctx.spawner.count_actors(), 1);
            
            // Spawn with strategy
            let strategy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 3,
                within: Duration::from_secs(5),
                decider: create_default_decider(),
            });
            let result = ctx.spawner().spawn_with_strategy(
                Box::new(TestActor::new()),
                Box::new(TestActorConfig),
                strategy
            ).await;
            
            assert!(result.is_ok());
            
            // Verify second actor was spawned
            assert_eq!(ctx.spawner.count_actors(), 2);
        });
    }
    
    // Test ActorSpawnerExt trait
    #[test]
    fn test_actor_spawner_ext() {
        let rt = Runtime::new().unwrap();
        let mut ctx = MockActorContext::new("spawner-ext-test");
        
        rt.block_on(async {
            // Use spawn_typed
            let actor = TestActor::new();
            let config = TestActorConfig;
            
            let result = ctx.spawner().spawn_typed(actor, config).await;
            assert!(result.is_ok());
            
            // Verify actor was spawned
            assert_eq!(ctx.spawner.count_actors(), 1);
            
            // Use spawn_supervised
            let actor = TestActor::new();
            let config = TestActorConfig;
            let strategy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 3,
                within: Duration::from_secs(5),
                decider: create_default_decider(),
            });
            
            let result = ctx.spawner().spawn_supervised(actor, config, strategy).await;
            assert!(result.is_ok());
            
            // Verify second actor was spawned
            assert_eq!(ctx.spawner.count_actors(), 2);
        });
    }
    
    // Test ActorContextMessage trait
    #[test]
    fn test_actor_context_message() {
        let rt = Runtime::new().unwrap();
        let ctx = MockActorContext::new("message-test");
        
        rt.block_on(async {
            // Test send_self
            let msg = TestMessage { data: 15 };
            let result = ctx.send_self(msg.clone());
            assert!(result.is_ok());
            
            // Allow async operation to complete
            tokio::time::sleep(Duration::from_millis(50)).await;
            
            // Verify message was sent to self
            let messages = ctx.self_ref.get_messages();
            assert!(messages.contains(&msg));
        });
    }
    
    // Test ScheduledTask
    #[test]
    fn test_scheduled_task() {
        // Create test actor references
        let actor_ref1 = Arc::new(MockActorRef::new("scheduled1"));
        let actor_ref2 = Arc::new(MockActorRef::new("scheduled1")); // Same path
        let actor_ref3 = Arc::new(MockActorRef::new("scheduled2")); // Different path
        
        let now = SystemTime::now();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        
        // Create tasks for testing
        let task1 = ScheduledTask {
            id: id1,
            target: actor_ref1,
            schedule_time: now,
        };
        
        let task2 = ScheduledTask {
            id: id1, // Same ID
            target: actor_ref2,
            schedule_time: now,
        };
        
        let task3 = ScheduledTask {
            id: id2, // Different ID
            target: actor_ref3,
            schedule_time: now,
        };
        
        // Test equality (should compare IDs)
        assert_eq!(task1, task2);
        assert_ne!(task1, task3);
        
        // Test hash map insertion and retrieval
        let mut map = HashMap::new();
        map.insert(task1.clone(), "task1");
        
        assert!(map.contains_key(&task2));
        assert!(!map.contains_key(&task3));
    }
    
    // Test LifecycleEvent
    #[test]
    fn test_lifecycle_event() {
        // Test different event types
        let started = LifecycleEvent::Started;
        let stopped = LifecycleEvent::Stopped;
        let receive_timeout = LifecycleEvent::ReceiveTimeout;
        
        // Test with actor references
        let actor_ref = MockActorRef::new("terminated");
        let boxed_ref = Box::new(actor_ref.clone()) as BoxedActorRef;
        let child_terminated = LifecycleEvent::ChildTerminated(boxed_ref.clone_boxed());
        let terminated = LifecycleEvent::Terminated(boxed_ref.clone_boxed());
        
        // Debug formatting
        assert!(format!("{:?}", started).contains("Started"));
        assert!(format!("{:?}", stopped).contains("Stopped"));
        assert!(format!("{:?}", receive_timeout).contains("ReceiveTimeout"));
        assert!(format!("{:?}", child_terminated).contains("ChildTerminated"));
        assert!(format!("{:?}", terminated).contains("Terminated"));
    }
} 