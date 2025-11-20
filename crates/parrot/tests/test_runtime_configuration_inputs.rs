use futures::FutureExt;
use parrot::{
    system::ParrotActorSystem,
    thread::{config::ThreadActorSystemConfig, context::ThreadContext, system::ThreadActorSystem},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use parrot_config::{ParrotConfig, ThreadResolved};
use std::panic::AssertUnwindSafe;

struct IdentityProbe(std::sync::Weak<ThreadActorSystem>);
impl Actor for IdentityProbe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let owner = context
                .system_weak_ref()
                .upgrade()
                .expect("context must own a live system identity");
            assert!(std::sync::Arc::ptr_eq(&owner, &self.0.upgrade().unwrap()));
            Ok(message)
        })
    }
}

async fn rejected(change: impl FnOnce(&mut ThreadResolved), key: &str) {
    let mut resolved = ParrotConfig::builder()
        .thread_shared_pool_size(1)
        .build()
        .unwrap();
    resolved.thread.shared_burst_workers_max = 0;
    change(&mut resolved.thread);
    let result = AssertUnwindSafe(ParrotActorSystem::from_config(&resolved))
        .catch_unwind()
        .await;
    assert!(
        result.is_ok(),
        "invalid configuration panicked instead of returning an error"
    );
    match result.unwrap() {
        Err(error) => assert!(error.to_string().contains(key), "{error}"),
        Ok(system) => {
            system.shutdown_internal().await.unwrap();
            panic!("invalid configuration was accepted: {key}");
        }
    }
}

macro_rules! rejection {
    ($name:ident, $field:ident, $value:expr) => {
        #[tokio::test]
        async fn $name() {
            rejected(
                |thread| thread.$field = $value,
                concat!("thread.", stringify!($field)),
            )
            .await;
        }
    };
}

rejection!(zero_workers_are_an_error, shared_pool_size, 0);
rejection!(
    zero_ready_capacity_is_an_error_not_a_panic,
    shared_queue_capacity,
    0
);
rejection!(
    zero_default_mailbox_is_not_silently_rounded_up,
    default_mailbox_capacity,
    0
);
rejection!(
    unrepresentable_worker_count_is_rejected_before_allocation,
    shared_pool_size,
    usize::MAX
);
rejection!(
    burst_count_cannot_wrap_the_total_worker_budget,
    shared_burst_workers_max,
    usize::MAX
);
rejection!(
    ready_capacity_has_a_finite_budget,
    shared_queue_capacity,
    usize::MAX
);
rejection!(
    default_mailbox_has_a_finite_budget,
    default_mailbox_capacity,
    usize::MAX
);
rejection!(
    dedicated_threads_have_a_finite_budget,
    max_dedicated_threads,
    usize::MAX
);
rejection!(
    context_tasks_have_a_finite_budget,
    max_context_tasks,
    usize::MAX
);

#[tokio::test]
async fn minimum_system_still_installs_its_identity_and_shuts_down() {
    let mut resolved = ParrotConfig::builder()
        .thread_shared_pool_size(1)
        .build()
        .unwrap();
    resolved.thread.shared_burst_workers_max = 0;
    resolved.thread.max_dedicated_threads = 0;
    resolved.thread.max_context_tasks = 0;
    resolved.thread.shared_queue_capacity = 1;
    resolved.thread.default_mailbox_capacity = 1;
    let system = ParrotActorSystem::from_config(&resolved).await.unwrap();
    let actor = system
        .spawn_at(
            IdentityProbe(std::sync::Arc::downgrade(&system)),
            "/input-probe",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        actor.send(Box::new(42u64)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 42);
    assert_eq!(system.config().shared_pool_size, 1);
    assert_eq!(
        ThreadActorSystemConfig::from_resolved(&resolved).max_context_tasks,
        0
    );
    system.shutdown_internal().await.unwrap();
}
