use std::time::Duration;
use parrot::actix::{ActixActorSystem, ActixContext, ActixActor};
use parrot::system::ParrotActorSystem;
use parrot_api::system::{ActorSystemConfig, ActorSystem};

/// Creates and initializes a ParrotActorSystem with Actix backend for testing
pub async fn setup_test_system() -> anyhow::Result<ParrotActorSystem> {
    // Create global actor system with default configuration
    let config = ActorSystemConfig::default();
    let system = ParrotActorSystem::new(config).await?;

    // Create and register ActixActorSystem as the default system
    let actix_system = ActixActorSystem::new().await?;
    system.register_actix_system("actix".to_string(), actix_system, true).await?;

    Ok(system)
}

/// Waits for a specified duration, useful for async tests that need timing
pub async fn wait_for(duration_millis: u64) {
    tokio::time::sleep(Duration::from_millis(duration_millis)).await;
}

/// Helper to run a test function with a configured actor system
pub async fn with_test_system<F, Fut, T>(test_fn: F) -> anyhow::Result<T>
where
    F: FnOnce(ParrotActorSystem) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<(T, ParrotActorSystem)>>,
{
    let system = setup_test_system().await?;
    let (result, system) = test_fn(system).await?;
    system.shutdown().await?;
    Ok(result)
}

/// Default wait time for async operations during tests in milliseconds
pub const DEFAULT_WAIT_TIME: u64 = 100; 