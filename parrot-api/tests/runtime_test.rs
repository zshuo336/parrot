use parrot_api::runtime::{RuntimeConfig, SchedulerConfig, LoadBalancingStrategy, RuntimeMetrics, ActorRuntime};
use parrot_api::errors::ActorError;
use std::time::Duration;
use std::sync::{Arc, Mutex};
use async_trait::async_trait;

/// Mock implementation of ActorRuntime for testing
struct MockActorRuntime {
    config: RuntimeConfig,
    metrics: Mutex<RuntimeMetrics>,
    is_running: Mutex<bool>,
}

#[async_trait]
impl ActorRuntime for MockActorRuntime {
    async fn start(config: RuntimeConfig) -> Result<Self, ActorError> {
        let metrics = RuntimeMetrics {
            active_actors: 0,
            pending_messages: 0,
            cpu_usage: 0.0,
            memory_usage: 0,
        };
        
        Ok(Self {
            config,
            metrics: Mutex::new(metrics),
            is_running: Mutex::new(true),
        })
    }
    
    async fn shutdown(self) -> Result<(), ActorError> {
        *self.is_running.lock().unwrap() = false;
        Ok(())
    }
    
    async fn spawn<F, T>(&self, future: F) -> Result<T, ActorError>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // Simple implementation that just runs the future
        // In a real implementation, this would be scheduled on a worker
        Ok(future.await)
    }
    
    fn metrics(&self) -> RuntimeMetrics {
        self.metrics.lock().unwrap().clone()
    }
}

// Extension methods for testing runtime
impl MockActorRuntime {
    fn is_running(&self) -> bool {
        *self.is_running.lock().unwrap()
    }
    
    fn set_metrics(&self, metrics: RuntimeMetrics) {
        *self.metrics.lock().unwrap() = metrics;
    }
    
    fn get_config(&self) -> &RuntimeConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    // Test RuntimeConfig construction and defaults
    #[test]
    fn test_runtime_config() {
        // Create a custom runtime configuration
        let config = RuntimeConfig {
            worker_threads: Some(4),
            io_threads: Some(2),
            scheduler_config: SchedulerConfig {
                task_queue_capacity: 1000,
                task_timeout: Duration::from_secs(30),
                load_balancing: LoadBalancingStrategy::RoundRobin,
            },
        };
        
        // Verify configuration properties
        assert_eq!(config.worker_threads, Some(4));
        assert_eq!(config.io_threads, Some(2));
        assert_eq!(config.scheduler_config.task_queue_capacity, 1000);
        assert_eq!(config.scheduler_config.task_timeout, Duration::from_secs(30));
        
        // Test with different load balancing strategy
        let config_random = RuntimeConfig {
            worker_threads: None,  // Use CPU count
            io_threads: None,      // Use default
            scheduler_config: SchedulerConfig {
                task_queue_capacity: 500,
                task_timeout: Duration::from_secs(10),
                load_balancing: LoadBalancingStrategy::Random,
            },
        };
        
        // Verify configuration with different strategy
        assert_eq!(config_random.worker_threads, None);
        assert_eq!(config_random.io_threads, None);
        assert_eq!(config_random.scheduler_config.task_queue_capacity, 500);
        assert_eq!(config_random.scheduler_config.task_timeout, Duration::from_secs(10));
        
        // Test with least-loaded strategy
        let config_least_loaded = RuntimeConfig {
            worker_threads: Some(8),
            io_threads: Some(4),
            scheduler_config: SchedulerConfig {
                task_queue_capacity: 2000,
                task_timeout: Duration::from_secs(60),
                load_balancing: LoadBalancingStrategy::LeastLoaded,
            },
        };
        
        // Verify configuration with least-loaded strategy
        assert_eq!(config_least_loaded.worker_threads, Some(8));
        assert_eq!(config_least_loaded.io_threads, Some(4));
        assert_eq!(config_least_loaded.scheduler_config.task_queue_capacity, 2000);
        assert_eq!(config_least_loaded.scheduler_config.task_timeout, Duration::from_secs(60));
    }
    
    // Test RuntimeMetrics
    #[test]
    fn test_runtime_metrics() {
        // Create runtime metrics
        let metrics = RuntimeMetrics {
            active_actors: 10,
            pending_messages: 50,
            cpu_usage: 45.5,
            memory_usage: 1024 * 1024 * 100, // 100 MB
        };
        
        // Verify metrics properties
        assert_eq!(metrics.active_actors, 10);
        assert_eq!(metrics.pending_messages, 50);
        assert_eq!(metrics.cpu_usage, 45.5);
        assert_eq!(metrics.memory_usage, 1024 * 1024 * 100);
        
        // Create a clone and verify
        let metrics_clone = metrics.clone();
        assert_eq!(metrics_clone.active_actors, metrics.active_actors);
        assert_eq!(metrics_clone.pending_messages, metrics.pending_messages);
        assert_eq!(metrics_clone.cpu_usage, metrics.cpu_usage);
        assert_eq!(metrics_clone.memory_usage, metrics.memory_usage);
    }
    
    // Test LoadBalancingStrategy
    #[test]
    fn test_load_balancing_strategy() {
        // Test Debug implementation
        let s1 = format!("{:?}", LoadBalancingStrategy::RoundRobin);
        let s2 = format!("{:?}", LoadBalancingStrategy::Random);
        let s3 = format!("{:?}", LoadBalancingStrategy::LeastLoaded);
        
        assert!(s1.contains("RoundRobin"));
        assert!(s2.contains("Random"));
        assert!(s3.contains("LeastLoaded"));
    }
    
    // Test ActorRuntime creation
    #[tokio::test]
    async fn test_actor_runtime_start() {
        // Create config
        let config = RuntimeConfig {
            worker_threads: Some(2),
            io_threads: Some(1),
            scheduler_config: SchedulerConfig {
                task_queue_capacity: 100,
                task_timeout: Duration::from_secs(5),
                load_balancing: LoadBalancingStrategy::RoundRobin,
            },
        };
        
        // Start runtime
        let runtime = MockActorRuntime::start(config.clone()).await;
        assert!(runtime.is_ok());
        
        let runtime = runtime.unwrap();
        
        // Verify runtime is running
        assert!(runtime.is_running());
        
        // Verify config was stored
        let stored_config = runtime.get_config();
        assert_eq!(stored_config.worker_threads, Some(2));
        assert_eq!(stored_config.scheduler_config.task_queue_capacity, 100);
        
        // Verify initial metrics
        let metrics = runtime.metrics();
        assert_eq!(metrics.active_actors, 0);
        assert_eq!(metrics.pending_messages, 0);
    }
    
    // Test ActorRuntime shutdown
    #[tokio::test]
    async fn test_actor_runtime_shutdown() {
        // Create runtime
        let config = RuntimeConfig {
            worker_threads: Some(1),
            io_threads: None,
            scheduler_config: SchedulerConfig {
                task_queue_capacity: 10,
                task_timeout: Duration::from_secs(1),
                load_balancing: LoadBalancingStrategy::Random,
            },
        };
        
        let runtime = MockActorRuntime::start(config).await.unwrap();
        
        // Shutdown runtime
        let result = runtime.shutdown().await;
        
        // Verify shutdown succeeded
        assert!(result.is_ok());
    }
    
    // Test ActorRuntime task spawn
    #[tokio::test]
    async fn test_actor_runtime_spawn() {
        // Create runtime
        let config = RuntimeConfig {
            worker_threads: Some(1),
            io_threads: None,
            scheduler_config: SchedulerConfig {
                task_queue_capacity: 10,
                task_timeout: Duration::from_secs(1),
                load_balancing: LoadBalancingStrategy::LeastLoaded,
            },
        };
        
        let runtime = MockActorRuntime::start(config).await.unwrap();
        
        // Spawn a task
        let result = runtime.spawn(async {
            // Simple computation
            let a = 5;
            let b = 7;
            a + b
        }).await;
        
        // Verify task result
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 12);
        
        // Update metrics to simulate activity
        runtime.set_metrics(RuntimeMetrics {
            active_actors: 5,
            pending_messages: 10,
            cpu_usage: 25.0,
            memory_usage: 1024 * 1024 * 10, // 10 MB
        });
        
        // Verify updated metrics
        let metrics = runtime.metrics();
        assert_eq!(metrics.active_actors, 5);
        assert_eq!(metrics.pending_messages, 10);
        assert_eq!(metrics.cpu_usage, 25.0);
        assert_eq!(metrics.memory_usage, 1024 * 1024 * 10);
    }
    
    // Test SchedulerConfig methods
    #[test]
    fn test_scheduler_config() {
        // Test creation with various parameters
        let config1 = SchedulerConfig {
            task_queue_capacity: 100,
            task_timeout: Duration::from_secs(10),
            load_balancing: LoadBalancingStrategy::RoundRobin,
        };
        
        let config2 = SchedulerConfig {
            task_queue_capacity: 200,
            task_timeout: Duration::from_secs(20),
            load_balancing: LoadBalancingStrategy::Random,
        };
        
        // Test clone
        let config1_clone = config1.clone();
        assert_eq!(config1_clone.task_queue_capacity, config1.task_queue_capacity);
        assert_eq!(config1_clone.task_timeout, config1.task_timeout);
        
        // Test debug
        let debug_str = format!("{:?}", config1);
        assert!(debug_str.contains("SchedulerConfig"));
        assert!(debug_str.contains("task_queue_capacity: 100"));
    }
} 