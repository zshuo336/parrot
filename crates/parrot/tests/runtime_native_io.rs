//! Explicit native I/O workers must be constructed on every startup entry.
use parrot::{
    actix::ActixSystemSettings,
    system::ParrotActorSystem,
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    runtime::RuntimeConfig,
    system::{ActorSystem, ActorSystemConfig},
};

fn config() -> ActorSystemConfig {
    ActorSystemConfig {
        runtime_config: RuntimeConfig {
            worker_threads: Some(1),
            io_threads: Some(2),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn thread_entry(entry: usize) {
    if entry == 0 {
        let system = <ThreadActorSystem as ActorSystem>::start(config())
            .await
            .expect("native Thread startup must support an independent I/O runtime");
        system.shutdown().await.unwrap();
    } else {
        let system = match entry {
            1 => ParrotActorSystem::start(config()),
            2 => ParrotActorSystem::with_thread(
                config(),
                ThreadActorSystemConfig {
                    shared_burst_workers_max: 0,
                    ..Default::default()
                },
            ),
            3 => <ParrotActorSystem as ActorSystem>::start(config()),
            _ => unreachable!(),
        }
        .await
        .expect("facade Thread startup must support an independent I/O runtime");
        system.shutdown_runtime().await.unwrap();
    }
}

#[tokio::test]
async fn native_thread_accepts_independent_io_workers() {
    thread_entry(0).await;
}

#[tokio::test]
async fn default_facade_accepts_independent_io_workers() {
    thread_entry(1).await;
}

#[tokio::test]
async fn thread_facade_accepts_independent_io_workers() {
    thread_entry(2).await;
}

#[tokio::test]
async fn trait_facade_accepts_independent_io_workers() {
    thread_entry(3).await;
}

#[actix::test]
async fn actix_facade_accepts_independent_io_workers() {
    let system = ParrotActorSystem::with_actix(config(), ActixSystemSettings::default())
        .await
        .expect("Actix startup must support an independent I/O runtime");
    system.shutdown_runtime().await.unwrap();
}
