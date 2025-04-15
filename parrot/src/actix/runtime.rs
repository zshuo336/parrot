use std::future::Future;
use std::sync::Arc;
use actix::System as ActixSystem;
use async_trait::async_trait;
use parrot_api::{
    runtime::{ActorRuntime, RuntimeConfig, RuntimeMetrics},
    errors::ActorError,
};
use tokio::runtime::Runtime;

pub struct ActixRuntime {
    system: Arc<ActixSystem>,
    config: RuntimeConfig,
    runtime: Runtime,
}

#[async_trait]
impl ActorRuntime for ActixRuntime {
    async fn start(config: RuntimeConfig) -> Result<Self, ActorError> {
        // Create tokio runtime with configured thread counts
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(config.worker_threads.unwrap_or_else(num_cpus::get))
            .enable_all()
            .build()
            .map_err(|e| ActorError::InitializationError(e.to_string()))?;

        // Create actix system
        let system = ActixSystem::new();
        
        Ok(Self { 
            system: Arc::new(system),
            config,
            runtime,
        })
    }

    async fn shutdown(self) -> Result<(), ActorError> {
        Arc::try_unwrap(self.system)
            .map_err(|_| ActorError::RuntimeError("System still has references".to_string()))?
            .stop();
        self.runtime.shutdown_timeout(std::time::Duration::from_secs(5));
        Ok(())
    }

    async fn spawn<F, T>(&self, future: F) -> Result<T, ActorError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // Spawn future on the runtime
        match self.runtime.spawn(future).await {
            Ok(result) => Ok(result),
            Err(e) => Err(ActorError::RuntimeError(format!(
                "Failed to spawn future: {}", e
            ))),
        }
    }

    fn metrics(&self) -> RuntimeMetrics {
        RuntimeMetrics {
            active_actors: 0, // TODO: Implement actor counting
            pending_messages: 0, // TODO: Implement message queue metrics
            cpu_usage: 0.0,     // TODO: Implement CPU usage tracking
            memory_usage: 0,     // TODO: Implement memory usage tracking
        }
    }
}

// Helper functions for runtime management
impl ActixRuntime {
    pub fn current() -> &'static ActixSystem {
        ActixSystem::current()
    }

    pub fn block_on<F, T>(future: F) -> T
    where
        F: Future<Output = T>,
    {
        // Run future to completion on the runtime
        let rt = tokio::runtime::Runtime::new()
            .expect("Failed to create runtime");
        rt.block_on(future)
    }
} 