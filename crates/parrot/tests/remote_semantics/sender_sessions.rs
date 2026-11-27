// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::{
    actor::{ActixEngineExt, ActorState},
    types::BoxedActorRef,
};
use parrot_remote::{RemoteActorRef, transport::RuntimeKnobs};

async fn command(target: &RemoteActorRef, msg: BoxedMessage) -> ActorResult<BoxedMessage> {
    let op = *msg.downcast::<Option<u64>>().unwrap();
    tokio::task::yield_now().await;
    let result = if let Some(value) = op {
        target
            .deliver_ordered(Box::new(RSeq(value)), Default::default())
            .await
    } else {
        target.close_ordered_session(Duration::from_secs(3)).await
    };
    Ok(Box::new(result.map_err(|error| error.to_string())))
}

struct ThreadSender(RemoteActorRef);
impl Actor for ThreadSender {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(self.0.close_ordered_session(Duration::from_secs(3)))
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(command(&self.0, msg))
    }
}
struct ActixSender(RemoteActorRef);
impl Actor for ActixSender {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(self.0.close_ordered_session(Duration::from_secs(3)))
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(command(&self.0, msg))
    }
}
impl ActixEngineExt for ActixSender {}

async fn call(actor: &dyn ActorRef, op: Option<u64>) -> Result<(), String> {
    *actor
        .send(Box::new(op))
        .await
        .unwrap()
        .downcast::<Result<(), String>>()
        .unwrap()
}

async fn verify(actix: bool, limit_at_source: bool) {
    let label = format!("senders-{actix}-{limit_at_source}");
    let facade = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    spawn_thread_actor(&facade, &ts, FifoActor { seen: vec![] }, "/user/fifo").await;
    let config = |name: &str, limit: bool| {
        let cfg = RCfg::mem(format!("{label}-{name}"));
        if limit {
            cfg.with_knobs(RuntimeKnobs {
                reorder_stream_capacity: 1,
                ..Default::default()
            })
        } else {
            cfg
        }
    };
    let start = |cfg| {
        RemoteActorSystem::new(
            cfg,
            Arc::new(FacadeLookup {
                facade: facade.clone(),
            }),
        )
        .unwrap()
    };
    let src = start(config("src", limit_at_source));
    let hub = start(config("hub", false).with_role(parrot_remote::handshake::TopologyRole::Hub));
    let end = start(config("end", !limit_at_source).with_ordered_relays([format!("{label}-hub")]));
    for node in [&src, &hub, &end] {
        node.start().await.unwrap();
    }
    src.connect_mem_pair(&hub).await.unwrap();
    end.connect_mem_pair(&hub).await.unwrap();
    let target = src
        .remote_ref(&format!("parrot://{label}-end/user/fifo"))
        .unwrap();
    let pool = if actix {
        Some(ActixActorSystem::with_arbiter_count(1).await.unwrap())
    } else {
        None
    };
    let mut actors: Vec<BoxedActorRef> = Vec::new();
    for path in ["/user/a", "/user/b"] {
        actors.push(if let Some(pool) = &pool {
            pool.spawn_root_typed(ActixSender(target.clone()), EmptyConfig)
                .await
                .unwrap()
                .clone_boxed()
        } else {
            ts.spawn_at(ThreadSender(target.clone()), path, None, Default::default())
                .await
                .unwrap()
                .clone_boxed()
        });
    }
    let (a, b) = (actors[0].as_ref(), actors[1].as_ref());
    call(a, Some(11)).await.unwrap();
    call(b, None).await.unwrap();
    let blocked = call(b, Some(99)).await;
    assert!(
        blocked.is_err_and(|e| e.contains("overloaded")),
        "second sender used another actor's ticket"
    );
    call(a, Some(12)).await.unwrap();
    let local = facade
        .get_actor(&ActorPath::placeholder("/user/fifo"))
        .await
        .unwrap();
    async fn wait_seen(local: &dyn ActorRef, expected: &[u64]) {
        eventually(Duration::from_secs(3), || async {
            local
                .send(Box::new(RGetSeen))
                .await
                .unwrap()
                .downcast::<RSeen>()
                .unwrap()
                .0
                == expected
        })
        .await;
    }
    wait_seen(local.as_ref(), &[11, 12]).await;
    a.stop().await.unwrap();
    call(b, Some(13)).await.unwrap();
    wait_seen(local.as_ref(), &[11, 12, 13]).await;
    b.stop().await.unwrap();
    for node in [&src, &hub, &end] {
        node.shutdown().await.unwrap();
    }
    if let Some(pool) = pool {
        pool.shutdown().await.unwrap();
    }
    ts.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thread_senders_own_separate_tickets_with_one_outbound_slot() {
    verify(false, true).await;
}

#[actix::test]
async fn actix_senders_own_separate_tickets_with_one_receiver_slot() {
    verify(true, false).await;
}
