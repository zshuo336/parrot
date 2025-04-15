use parrot_api::address::{ActorPath, ActorRef, ActorRefExt};
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture, BoxedActorRef, WeakActorTarget};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use std::fmt::{self, Debug};
use std::sync::{Arc, Mutex, RwLock};
use std::any::Any;
use std::future::Future;
use async_trait::async_trait;

// Test message types
#[derive(Debug, Clone)]
struct TestMessage(u32);

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
        Box::pin(async move {
            *alive.read().unwrap()
        })
    }
    
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            id: self.id.clone(),
            counter: self.counter.clone(),
            alive: self.alive.clone(),
        })
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
        Box::pin(async move {
            Ok(msg)
        })
    }
    
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            Ok(())
        })
    }
    
    fn path(&self) -> String {
        self.path_value.clone()
    }
    
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move {
            true
        })
    }
    
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            path_value: self.path_value.clone(),
        })
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
            actor.tell(TestMessage(3));
            
            // Wait to allow async processing
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            
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
            actor.tell(TestMessage(5));
            
            // Small delay to ensure message is processed
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            
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