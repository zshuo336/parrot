//! One live owner per process-local memory address, including before start.
use parrot_remote::{
    admin_v2::{AdminReplyV2, ComponentDeploy, ComponentExecutor},
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}
struct Counter(AtomicUsize);
#[async_trait::async_trait]
impl ComponentExecutor for Counter {
    async fn deploy(&self, _: u64, _: &ComponentDeploy) -> AdminReplyV2 {
        unreachable!()
    }
    async fn drain(&self, _: u64, _: &str, _: u64) -> AdminReplyV2 {
        unreachable!()
    }
    async fn status(&self, _: u64, _: &str) -> AdminReplyV2 {
        unreachable!()
    }
    async fn stop(&self, req_id: u64, _: &str) -> AdminReplyV2 {
        self.0.fetch_add(1, Ordering::SeqCst);
        AdminReplyV2::Stopped { req_id }
    }
}
fn system(id: &str) -> Arc<RemoteActorSystem> {
    RemoteActorSystem::new(RemoteConfig::mem(id), Arc::new(Empty)).unwrap()
}
async fn call(client: &Arc<RemoteActorSystem>, target: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        client.connect(&NodeAddr::mem(target)).await.unwrap();
        client.stop_component(target, "/probe").await.unwrap();
    })
    .await
    .expect("memory address must route to its current owner");
}

#[tokio::test]
async fn duplicate_construction_is_rejected_without_stealing_the_existing_address() {
    let first = system("exclusive-original");
    let calls = Arc::new(Counter(AtomicUsize::new(0)));
    first.set_component_executor(Some(calls.clone()));
    // Registration is synchronous: exclusivity also applies before start.
    let duplicate =
        RemoteActorSystem::new(RemoteConfig::mem("exclusive-original"), Arc::new(Empty));
    let rejected = duplicate.is_err();
    if let Ok(duplicate) = duplicate {
        duplicate.shutdown().await.unwrap();
    }
    if !rejected {
        first.shutdown().await.unwrap();
        panic!("duplicate memory identity silently replaced a live owner");
    }
    first.start().await.unwrap();
    let client = system("exclusive-client");
    client.start().await.unwrap();
    call(&client, "exclusive-original").await;
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    client.shutdown().await.unwrap();
    first.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_constructors_have_exactly_one_owner_and_all_losers_leave_it_usable() {
    let barrier = Arc::new(tokio::sync::Barrier::new(16));
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                RemoteActorSystem::new(RemoteConfig::mem("exclusive-race"), Arc::new(Empty))
            })
        })
        .collect();
    let mut owners = Vec::new();
    for task in tasks {
        if let Ok(owner) = task.await.unwrap() {
            owners.push(owner);
        }
    }
    let count = owners.len();
    if count != 1 {
        for owner in owners {
            owner.shutdown().await.unwrap();
        }
        panic!("expected one live endpoint, obtained {count}");
    }
    let owner = owners.pop().unwrap();
    let calls = Arc::new(Counter(AtomicUsize::new(0)));
    owner.set_component_executor(Some(calls.clone()));
    owner.start().await.unwrap();
    let client = system("exclusive-race-client");
    client.start().await.unwrap();
    call(&client, "exclusive-race").await;
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    client.shutdown().await.unwrap();
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_and_drop_release_binding_without_old_cleanup_removing_a_replacement() {
    let old = system("exclusive-reuse");
    old.start().await.unwrap();
    old.shutdown().await.unwrap();
    let replacement = system("exclusive-reuse");
    let calls = Arc::new(Counter(AtomicUsize::new(0)));
    replacement.set_component_executor(Some(calls.clone()));
    replacement.start().await.unwrap();
    old.shutdown().await.unwrap();
    drop(old);
    let client = system("exclusive-reuse-client");
    client.start().await.unwrap();
    call(&client, "exclusive-reuse").await;
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    client.shutdown().await.unwrap();
    replacement.shutdown().await.unwrap();
    drop(replacement);
    // An unstarted owner's Drop must also release the reservation immediately.
    drop(system("exclusive-reuse"));
    let last = system("exclusive-reuse");
    last.shutdown().await.unwrap();
}
