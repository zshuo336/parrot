use parrot::thread::{config::ThreadActorConfig, context::ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    fn wait(&self) {
        let (guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(8), |open| {
                !*open
            })
            .unwrap();
        assert!(
            *guard && !timeout.timed_out(),
            "test failed to release blocking actor"
        );
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct BlockingActor {
    entered: Arc<tokio::sync::Notify>,
    gate: Arc<Gate>,
}
impl Actor for BlockingActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.gate.wait();
            Ok(msg)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
struct Echo;
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_node_assembly_consumes_thread_configuration_and_shuts_down_actors() {
    let mut config = parrot_config::ParrotConfig::builder();
    config.thread.shared_pool_size = Some(1);
    config.thread.shared_burst_workers_max = Some(0);
    config.thread.shared_queue_capacity = Some(1);
    config.thread.default_mailbox_capacity = Some(2);
    let settings = parrot_node::configuration::NodeSettings {
        resolved: config
            .remote_node_id("runtime-test")
            .remote_bind("127.0.0.1:0")
            .build()
            .unwrap(),
        actors: vec![],
        ready_file: None,
        wasm_epoch_ticks: None,
    };
    let node = parrot_node::runtime::RunningNode::start(&settings)
        .await
        .unwrap();
    let system = &node.thread;
    let entered = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocker = system
        .spawn_at(
            BlockingActor {
                entered: entered.clone(),
                gate: gate.clone(),
            },
            "/blocker",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    let blocking = tokio::spawn(async move { blocker.send(Box::new(())).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let mut actors = Vec::new();
    for n in 0..12 {
        actors.push(
            system
                .spawn_at(
                    Echo,
                    &format!("/waiting/{n}"),
                    None,
                    ThreadActorConfig::default(),
                )
                .await
                .unwrap(),
        );
    }
    let retained = actors[0].clone();
    retained.deliver(Box::new(1_u64)).await.unwrap();
    retained.deliver(Box::new(2_u64)).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), retained.deliver(Box::new(3_u64)))
            .await
            .is_err(),
        "production node must use configured mailbox capacity"
    );
    let metrics = system.scheduler_metrics().unwrap();
    let backlog = metrics.queue_length;
    let calls: Vec<_> = actors
        .into_iter()
        .enumerate()
        .map(|(n, actor)| {
            tokio::spawn(async move {
                actor
                    .send_with_timeout(Box::new(n as u64), Some(Duration::from_secs(3)))
                    .await
                    .map(|reply| *reply.downcast::<u64>().unwrap())
            })
        })
        .collect();
    gate.release();
    blocking.await.unwrap().unwrap();
    for (n, call) in calls.into_iter().enumerate() {
        assert_eq!(call.await.unwrap().unwrap(), n as u64);
    }
    node.shutdown().await.unwrap();
    assert!(!retained.is_alive().await);
    assert!(retained.send(Box::new(4_u64)).await.is_err());
    assert_eq!(metrics.queue_capacity, 1);
    assert_eq!(metrics.deferred_ready, 11);
    assert_eq!(
        backlog, 1,
        "ready queue must obey the resolved configuration even while CPU execution is blocked"
    );
}
