//! A declared termination protocol must be usable regardless of entry protocol.
use parrot_api::{
    Message, ParrotTypedActor,
    context::ActorTerminated,
    typed::TypedReceive,
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Notify, mpsc, oneshot};

pub struct Ping;
impl Message for Ping {
    type Result = ();
}
// An alias deliberately prevents syntax-name detection from faking support.
type Ended = ActorTerminated;
pub struct Hold(oneshot::Sender<()>, Arc<Notify>);
impl Message for Hold {
    type Result = ();
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Ping, Ended, Hold))]
struct Observer(mpsc::UnboundedSender<ActorTerminated>);

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Ping))]
struct Plain;
impl TypedReceive<Ping> for Plain {
    fn receive_typed(&mut self, _: Ping) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl TypedReceive<Ping> for Observer {
    fn receive_typed(&mut self, _: Ping) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl TypedReceive<Ended> for Observer {
    fn receive_typed(&mut self, event: Ended) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.0.send(event).unwrap();
            Ok(())
        })
    }
}
impl TypedReceive<Hold> for Observer {
    fn receive_typed(&mut self, hold: Hold) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let _ = hold.0.send(());
            hold.1.notified().await;
            Ok(())
        })
    }
}
async fn observe(
    target: BoxedActorRef,
    watcher: BoxedActorRef,
    mut events: mpsc::UnboundedReceiver<ActorTerminated>,
) {
    target
        .watch(watcher)
        .await
        .expect("declared termination protocol must install a typed watcher recipient");
    let identity = target.death_watch().unwrap().id();
    target.stop().await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.generation, identity);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_typed_watcher_receives_its_declared_system_protocol() {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let (send, events) = mpsc::unbounded_channel();
    let watcher = system
        .spawn_typed::<Observer, Ping>(Observer(send), "/watcher")
        .await
        .unwrap();
    watcher.ask(Ping).await.unwrap();
    let (send, _) = mpsc::unbounded_channel();
    let target = system
        .spawn_typed::<Observer, Ping>(Observer(send), "/target")
        .await
        .unwrap();
    observe(target.into_dyn(), watcher.into_dyn(), events).await;
    system.shutdown_internal().await.unwrap();
}
#[actix::test]
async fn actix_typed_watcher_receives_its_declared_system_protocol() {
    let system = std::sync::Arc::new(parrot::actix::ActixActorSystem::new().await.unwrap());
    let (send, events) = mpsc::unbounded_channel();
    let watcher = system
        .spawn_typed::<Observer, Ping>(Observer(send), "/watcher")
        .await
        .unwrap();
    watcher.ask(Ping).await.unwrap();
    let (send, _) = mpsc::unbounded_channel();
    let target = system
        .spawn_typed::<Observer, Ping>(Observer(send), "/target")
        .await
        .unwrap();
    observe(target.into_dyn(), watcher.into_dyn(), events).await;
    system.as_ref().clone().shutdown().await.unwrap();
}

type ObserverRef = parrot_api::typed_channel::TypedActorRef<Observer, Ping>;
struct Systems {
    thread: Arc<parrot::thread::ThreadActorSystem>,
    actix: Arc<parrot::actix::ActixActorSystem>,
}
impl Systems {
    async fn new() -> Self {
        Self {
            thread: parrot::thread::ThreadActorSystem::shared(
                parrot::thread::ThreadActorSystemConfig {
                    default_mailbox_capacity: 1,
                    shared_pool_size: 2,
                    shutdown_timeout: Duration::from_secs(1),
                    ..Default::default()
                },
            ),
            actix: Arc::new(
                parrot::actix::ActixActorSystem::with_settings(
                    parrot::actix::system::ActixSystemSettings {
                        mailbox_capacity: 1,
                        arbiter_count: 2,
                        shutdown_timeout: Duration::from_secs(1),
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            ),
        }
    }
    async fn spawn(
        &self,
        threaded: bool,
        path: &str,
    ) -> (ObserverRef, mpsc::UnboundedReceiver<ActorTerminated>) {
        let (send, events) = mpsc::unbounded_channel();
        let actor = if threaded {
            self.thread
                .spawn_typed::<Observer, Ping>(Observer(send), path)
                .await
        } else {
            self.actix
                .spawn_typed::<Observer, Ping>(Observer(send), path)
                .await
        }
        .unwrap();
        actor.ask(Ping).await.unwrap();
        (actor, events)
    }
    async fn shutdown(self) {
        self.thread.shutdown_internal().await.unwrap();
        self.actix.as_ref().clone().shutdown().await.unwrap();
    }
}
async fn event(events: &mut mpsc::UnboundedReceiver<ActorTerminated>) -> ActorTerminated {
    tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn no_outgoing(actor: &ObserverRef) {
    let handle = actor.clone().into_dyn().death_watch().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle.subscriptions().1 != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[actix::test]
async fn protocol_views_cross_engines_and_reused_paths_keep_exact_identity() {
    for threaded in [false, true] {
        let systems = Systems::new().await;
        let (watcher, _) = systems.spawn(threaded, "/watcher").await;
        let (target, _) = systems.spawn(!threaded, "/target").await;
        let target = target.into_dyn();
        let ping = watcher.clone().into_dyn();
        let ended = watcher.ref_for::<Ended>().into_dyn();
        assert_eq!(
            ping.death_watch().unwrap().id(),
            ended.death_watch().unwrap().id()
        );
        target.watch(ping.clone_boxed()).await.unwrap();
        target.watch(ended.clone_boxed()).await.unwrap();
        assert_eq!(target.death_watch().unwrap().subscriptions().0, 1);
        target.unwatch(ended).await.unwrap();
        assert_eq!(target.death_watch().unwrap().subscriptions().0, 0);
        target.watch(ping.clone_boxed()).await.unwrap();
        watcher.stop().await.unwrap();
        assert_eq!(target.death_watch().unwrap().subscriptions().0, 0);
        let (replacement, mut events) = systems.spawn(threaded, "/watcher").await;
        let next = replacement.clone().into_dyn();
        assert_ne!(
            ping.death_watch().unwrap().id(),
            next.death_watch().unwrap().id()
        );
        target.watch(next).await.unwrap();
        target.unwatch(ping.clone_boxed()).await.unwrap();
        assert_eq!(target.death_watch().unwrap().subscriptions().0, 1);
        assert!(matches!(
            target.watch(ping).await,
            Err(parrot_api::errors::ActorError::Stopped)
        ));
        target.stop().await.unwrap();
        assert_eq!(
            event(&mut events).await.generation,
            target.death_watch().unwrap().id()
        );
        no_outgoing(&replacement).await;
        replacement.ask(Ping).await.unwrap();
        assert!(events.try_recv().is_err());
        systems.shutdown().await;
    }
}

#[actix::test]
async fn single_protocol_adapters_receive_cross_engine_termination() {
    for threaded in [false, true] {
        let systems = Systems::new().await;
        let (send, events) = mpsc::unbounded_channel();
        let watcher = if threaded {
            systems
                .thread
                .spawn_typed_single::<Observer, Ended>(Observer(send), "/single")
                .await
        } else {
            systems
                .actix
                .spawn_typed_single::<Observer, Ended>(Observer(send), "/single")
                .await
        }
        .unwrap();
        let (target, _) = systems.spawn(!threaded, "/target").await;
        observe(target.into_dyn(), watcher.into_dyn(), events).await;
        systems.shutdown().await;
    }
}

#[actix::test]
async fn undeclared_system_protocol_is_rejected_without_registering_a_dead_edge() {
    for threaded in [false, true] {
        let systems = Systems::new().await;
        let watcher = if threaded {
            systems
                .thread
                .spawn_typed::<Plain, Ping>(Plain, "/plain")
                .await
        } else {
            systems
                .actix
                .spawn_typed::<Plain, Ping>(Plain, "/plain")
                .await
        }
        .unwrap();
        let (target, _) = systems.spawn(!threaded, "/target").await;
        let target = target.into_dyn();
        let watcher = watcher.into_dyn();
        let error = target.watch(watcher.clone_boxed()).await.unwrap_err();
        assert!(error.to_string().contains("no termination recipient"));
        assert_eq!(target.death_watch().unwrap().subscriptions(), (0, 0));
        assert_eq!(watcher.death_watch().unwrap().subscriptions(), (0, 0));
        target.stop().await.unwrap();
        assert_eq!(target.death_watch().unwrap().delivery_failures(), 0);
        systems.shutdown().await;
    }
}

#[actix::test]
async fn full_typed_mailboxes_wait_cancel_stop_and_expire_without_stranding_peers() {
    for threaded in [false, true] {
        // Release capacity, unwatch, stop the watcher, or expire delivery.
        for action in 0..4 {
            let systems = Systems::new().await;
            let (slow, mut slow_events) = systems.spawn(threaded, "/slow").await;
            let (healthy, mut healthy_events) = systems.spawn(threaded, "/healthy").await;
            let (target, _) = systems.spawn(!threaded, "/target").await;
            let target = target.into_dyn();
            let handle = target.death_watch().unwrap();
            let gate = Arc::new(Notify::new());
            let (entered, ready) = oneshot::channel();
            slow.ref_for::<Hold>()
                .tell(Hold(entered, gate.clone()))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), ready)
                .await
                .unwrap()
                .unwrap();
            slow.tell(Ping).await.unwrap();
            target.watch(slow.clone().into_dyn()).await.unwrap();
            target.watch(healthy.clone().into_dyn()).await.unwrap();
            let stopping = target.clone_boxed();
            let stopped = tokio::spawn(async move { stopping.stop().await });
            assert_eq!(event(&mut healthy_events).await.generation, handle.id());
            assert_eq!(
                slow.clone()
                    .into_dyn()
                    .death_watch()
                    .unwrap()
                    .subscriptions()
                    .1,
                1
            );
            match action {
                0 => {
                    gate.notify_one();
                    assert_eq!(event(&mut slow_events).await.generation, handle.id());
                }
                1 => {
                    target
                        .unwatch(slow.ref_for::<Ended>().into_dyn())
                        .await
                        .unwrap();
                    gate.notify_one();
                }
                2 => slow.stop().await.unwrap(),
                _ => {
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while handle.delivery_failures() == 0 {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    gate.notify_one();
                }
            }
            no_outgoing(&slow).await;
            if action != 2 {
                slow.ask(Ping).await.unwrap();
            }
            assert!(slow_events.try_recv().is_err());
            assert_eq!(handle.delivery_failures(), u64::from(action == 3));
            assert_eq!(handle.subscriptions(), (0, 0));
            tokio::time::timeout(Duration::from_secs(2), stopped)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            healthy.ask(Ping).await.unwrap();
            assert!(healthy_events.try_recv().is_err());
            systems.shutdown().await;
        }
    }
}
