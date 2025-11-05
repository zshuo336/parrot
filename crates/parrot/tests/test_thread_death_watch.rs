//! DeathWatch is tied to logical actor identity, including cross-system refs.
use parrot::thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorTerminated},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

enum Command {
    Ping,
    Crash,
    SelfRef,
    Watch(BoxedActorRef),
    Unwatch(BoxedActorRef),
}
struct Probe(mpsc::UnboundedSender<ActorTerminated>);
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<ActorTerminated>() {
                let _ = self.0.send(*msg.downcast::<ActorTerminated>().unwrap());
            } else if msg.is::<parrot::thread::system::Terminated>() {
                let event = *msg
                    .downcast::<parrot::thread::system::Terminated>()
                    .unwrap();
                let _ = self.0.send(ActorTerminated {
                    generation: uuid::Uuid::nil(),
                    path: event.path,
                    reason: event.reason,
                });
            } else {
                match *msg.downcast::<Command>().unwrap() {
                    Command::Ping => {}
                    Command::Crash => panic!("watched generation failed"),
                    Command::SelfRef => return Ok(Box::new(ctx.get_self_ref()) as BoxedMessage),
                    Command::Watch(target) => ctx.watch(target).await?,
                    Command::Unwatch(target) => ctx.unwatch(target).await?,
                }
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
fn system(mode: SchedulingMode) -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_scheduling_mode: mode,
        shared_pool_size: 2,
        shutdown_timeout: Duration::from_secs(1),
        ..Default::default()
    })
}
async fn spawn(
    system: &Arc<ThreadActorSystem>,
    path: &str,
) -> (BoxedActorRef, mpsc::UnboundedReceiver<ActorTerminated>) {
    use parrot_api::address::ActorRef;
    let (sender, receiver) = mpsc::unbounded_channel();
    let actor = system
        .spawn_at(Probe(sender), path, None, Default::default())
        .await
        .unwrap();
    actor.send(Box::new(Command::Ping)).await.unwrap();
    (actor.clone_boxed(), receiver)
}
fn modes() -> [SchedulingMode; 3] {
    [
        SchedulingMode::SharedPool {
            max_messages_per_run: 8,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "watch".into(),
        },
    ]
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_and_context_references_share_death_watch_and_unwatch() {
    for mode in modes() {
        let system = system(mode);
        let (watcher, mut events) = spawn(&system, "/watcher").await;
        let (target, _) = spawn(&system, "/target").await;
        let self_ref = *watcher
            .send(Box::new(Command::SelfRef))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        let handle = watcher
            .death_watch()
            .expect("managed Thread actor must expose DeathWatch");
        assert_eq!(handle.id(), self_ref.death_watch().unwrap().id());
        watcher
            .send(Box::new(Command::Watch(target.clone_boxed())))
            .await
            .unwrap();
        target.watch(watcher.clone_boxed()).await.unwrap();
        assert_eq!(target.death_watch().unwrap().subscriptions().0, 1);
        watcher
            .send(Box::new(Command::Unwatch(target.clone_boxed())))
            .await
            .unwrap();
        assert_eq!(target.death_watch().unwrap().subscriptions().0, 0);
        target.stop().await.unwrap();
        watcher.send(Box::new(Command::Ping)).await.unwrap();
        assert!(events.try_recv().is_err());
        system.shutdown_internal().await.unwrap();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_watch_uses_foreign_reference_not_matching_local_path() {
    let mode = SchedulingMode::SharedPool {
        max_messages_per_run: 8,
    };
    let local = system(mode.clone());
    let foreign = system(mode);
    let (watcher, mut events) = spawn(&local, "/watcher").await;
    let (impostor, _) = spawn(&local, "/target").await;
    let (target, _) = spawn(&foreign, "/target").await;
    watcher
        .send(Box::new(Command::Watch(target.clone_boxed())))
        .await
        .unwrap();
    impostor.stop().await.unwrap();
    watcher.send(Box::new(Command::Ping)).await.unwrap();
    assert!(
        events.try_recv().is_err(),
        "local path collision must not cause foreign target death"
    );
    target.stop().await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("foreign target must notify its actual watcher")
        .unwrap();
    assert_eq!(event.path, "/target");
    assert_eq!(event.generation, target.death_watch().unwrap().id());
    local.shutdown_internal().await.unwrap();
    foreign.shutdown_internal().await.unwrap();
}

async fn event(events: &mut mpsc::UnboundedReceiver<ActorTerminated>) -> ActorTerminated {
    tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_unwatch_and_reused_watcher_paths_do_not_touch_new_instances() {
    for mode in modes() {
        let sys = system(mode);
        let (watcher, mut events) = spawn(&sys, "/watcher").await;
        let (old, _) = spawn(&sys, "/target").await;
        old.watch(watcher.clone_boxed()).await.unwrap();
        old.stop().await.unwrap();
        assert_eq!(
            event(&mut events).await.generation,
            old.death_watch().unwrap().id()
        );
        let (next, _) = spawn(&sys, "/target").await;
        assert_ne!(
            old.death_watch().unwrap().id(),
            next.death_watch().unwrap().id()
        );
        next.watch(watcher.clone_boxed()).await.unwrap();
        old.unwatch(watcher.clone_boxed()).await.unwrap();
        assert_eq!(next.death_watch().unwrap().subscriptions().0, 1);
        // Both legacy and common watches belong to the old watcher identity.
        sys.watch("/watcher".into(), "/target".into())
            .await
            .unwrap();
        watcher.stop().await.unwrap();
        assert_eq!(next.death_watch().unwrap().subscriptions().0, 0);
        let (new_watcher, mut new_events) = spawn(&sys, "/watcher").await;
        next.stop().await.unwrap();
        new_watcher.send(Box::new(Command::Ping)).await.unwrap();
        assert!(new_events.try_recv().is_err());
        sys.shutdown_internal().await.unwrap();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn common_watch_survives_supervised_replacement_and_notifies_once_at_final_death() {
    use parrot::thread::{SupervisorStrategy, ThreadActorConfig};
    use parrot_api::address::ActorRef;
    use std::sync::atomic::{AtomicUsize, Ordering};
    for mode in modes() {
        let sys = system(mode.clone());
        let (watcher, mut events) = spawn(&sys, "/watcher").await;
        let count = Arc::new(AtomicUsize::new(0));
        let builds = count.clone();
        let (sender, _) = mpsc::unbounded_channel();
        let target = sys
            .spawn_supervised_with_config(
                move || {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Probe(sender.clone())
                },
                "/target",
                ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    supervisor_strategy: Some(SupervisorStrategy::Restart {
                        max_retries: 1,
                        within: Duration::from_secs(10),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        target.send(Box::new(Command::Ping)).await.unwrap();
        let identity = target.death_watch().unwrap().id();
        watcher
            .send(Box::new(Command::Watch(target.clone_boxed())))
            .await
            .unwrap();
        assert!(target.send(Box::new(Command::Crash)).await.is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if count.load(Ordering::SeqCst) == 2
                    && target.send(Box::new(Command::Ping)).await.is_ok()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(target.death_watch().unwrap().id(), identity);
        watcher.send(Box::new(Command::Ping)).await.unwrap();
        assert!(events.try_recv().is_err());
        assert!(target.send(Box::new(Command::Crash)).await.is_err());
        let notice = event(&mut events).await;
        assert_eq!(notice.generation, identity);
        assert!(matches!(notice.reason, parrot_api::DeathReason::Panic(_)));
        // Notification admission may precede the supervisor's final removal.
        tokio::time::timeout(Duration::from_secs(2), async {
            while sys.get_actor_ref("/target").is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        watcher.send(Box::new(Command::Ping)).await.unwrap();
        assert!(events.try_recv().is_err());
        sys.shutdown_internal().await.unwrap();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_common_and_legacy_subscriptions_are_bounded_and_reclaimed() {
    let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 2,
        shutdown_timeout: Duration::from_secs(5),
        ..Default::default()
    });
    let (target, _) = spawn(&sys, "/target").await;
    let mut watchers = Vec::new();
    for i in 0..1025 {
        let path = format!("/watcher-{i}");
        let (watcher, _) = spawn(&sys, &path).await;
        let common = target.watch(watcher.clone_boxed()).await;
        let legacy = sys.watch(path, "/target".into()).await;
        assert_eq!(common.is_ok(), i < 1024);
        assert_eq!(legacy.is_ok(), i < 1024);
        watchers.push(watcher);
    }
    assert_eq!(target.death_watch().unwrap().subscriptions().0, 1024);
    watchers[0].stop().await.unwrap();
    assert_eq!(target.death_watch().unwrap().subscriptions().0, 1023);
    target.watch(watchers[1024].clone_boxed()).await.unwrap();
    sys.watch("/watcher-1024".into(), "/target".into())
        .await
        .unwrap();
    sys.shutdown_internal().await.unwrap();
    assert_eq!(target.death_watch().unwrap().subscriptions(), (0, 0));
}

struct ActixProbe(mpsc::UnboundedSender<ActorTerminated>);
impl parrot_api::actor::ActixEngineExt for ActixProbe {}
impl Actor for ActixProbe {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<ActorTerminated>() {
                let _ = self.0.send(*msg.downcast::<ActorTerminated>().unwrap());
            } else if let Command::Watch(target) = *msg.downcast::<Command>().unwrap() {
                ctx.watch(target).await?;
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
#[actix::test]
async fn real_context_watches_cross_both_engine_directions() {
    let threaded = system(modes()[0].clone());
    let actix = parrot::actix::ActixActorSystem::new().await.unwrap();
    let (thread_watcher, mut thread_events) = spawn(&threaded, "/thread-watcher").await;
    let (sender, mut actix_events) = mpsc::unbounded_channel();
    let actix_watcher = actix
        .spawn_root_typed(ActixProbe(sender), EmptyConfig)
        .await
        .unwrap();
    let (sender, _) = mpsc::unbounded_channel();
    let actix_target = actix
        .spawn_root_typed(ActixProbe(sender), EmptyConfig)
        .await
        .unwrap();
    let (thread_target, _) = spawn(&threaded, "/thread-target").await;
    actix_watcher
        .send(Box::new(Command::Watch(thread_target.clone_boxed())))
        .await
        .unwrap();
    thread_watcher
        .send(Box::new(Command::Watch(actix_target.clone_boxed())))
        .await
        .unwrap();
    thread_target.stop().await.unwrap();
    actix_target.stop().await.unwrap();
    assert_eq!(
        event(&mut actix_events).await.generation,
        thread_target.death_watch().unwrap().id()
    );
    assert_eq!(
        event(&mut thread_events).await.generation,
        actix_target.death_watch().unwrap().id()
    );
    threaded.shutdown_internal().await.unwrap();
    actix.shutdown().await.unwrap();
}

struct BlockingDrop {
    entered: Arc<tokio::sync::Notify>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
impl Drop for BlockingDrop {
    fn drop(&mut self) {
        self.entered.notify_one();
        let _ = self
            .release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(3));
    }
}
impl Actor for BlockingDrop {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn common_notification_waits_for_actual_disposal_after_stop_timeout() {
    use parrot_api::address::ActorRef;
    for mode in modes() {
        let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
            default_scheduling_mode: mode,
            shutdown_timeout: Duration::from_millis(60),
            shared_pool_size: 2,
            ..Default::default()
        });
        let (watcher, mut events) = spawn(&sys, "/watcher").await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let (release, gate) = std::sync::mpsc::channel();
        let target = sys
            .spawn_at(
                BlockingDrop {
                    entered: entered.clone(),
                    release: std::sync::Mutex::new(gate),
                },
                "/target",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        target.send(Box::new(())).await.unwrap();
        watcher
            .send(Box::new(Command::Watch(target.clone_boxed())))
            .await
            .unwrap();
        let stopping = target.clone();
        let stop = tokio::spawn(async move { stopping.stop().await });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        assert!(matches!(
            stop.await.unwrap(),
            Err(parrot_api::errors::ActorError::TimeoutDetail(_))
        ));
        assert!(sys.get_actor_ref("/target").is_some());
        assert!(events.try_recv().is_err());
        assert_eq!(target.death_watch().unwrap().subscriptions().0, 1);
        release.send(()).unwrap();
        let notice = event(&mut events).await;
        assert_eq!(notice.generation, target.death_watch().unwrap().id());
        assert!(matches!(notice.reason, parrot_api::DeathReason::Normal));
        tokio::time::timeout(Duration::from_secs(1), async {
            while sys.get_actor_ref("/target").is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        sys.shutdown_internal().await.unwrap();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarted_watcher_releases_old_dependencies_and_receives_on_its_new_mailbox() {
    use parrot::thread::{SupervisorStrategy, ThreadActorConfig};
    use parrot_api::address::ActorRef;
    use std::sync::atomic::{AtomicUsize, Ordering};
    for mode in modes() {
        let sys = system(mode.clone());
        let (target, _) = spawn(&sys, "/target").await;
        let (sender, mut events) = mpsc::unbounded_channel();
        let builds = Arc::new(AtomicUsize::new(0));
        let count = builds.clone();
        let watcher = sys
            .spawn_supervised_with_config(
                move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    Probe(sender.clone())
                },
                "/watcher",
                ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    supervisor_strategy: Some(SupervisorStrategy::Restart {
                        max_retries: 1,
                        within: Duration::from_secs(10),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        watcher
            .send(Box::new(Command::Watch(target.clone_boxed())))
            .await
            .unwrap();
        assert_eq!(target.death_watch().unwrap().subscriptions().0, 1);
        assert!(watcher.send(Box::new(Command::Crash)).await.is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if builds.load(Ordering::SeqCst) == 2
                    && watcher.send(Box::new(Command::Ping)).await.is_ok()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(target.death_watch().unwrap().subscriptions().0, 0);
        watcher
            .send(Box::new(Command::Watch(target.clone_boxed())))
            .await
            .unwrap();
        target.stop().await.unwrap();
        assert_eq!(
            event(&mut events).await.generation,
            target.death_watch().unwrap().id()
        );
        sys.shutdown_internal().await.unwrap();
    }
}

#[derive(Debug)]
struct BusinessPolicy;
impl parrot::thread::mailbox::admission::MessageAdmission for BusinessPolicy {
    fn prepare(&self, msg: BoxedMessage) -> Result<BoxedMessage, parrot::thread::MailboxError> {
        if msg.is::<ActorTerminated>() || msg.is::<parrot::thread::system::Terminated>() {
            return Err(parrot::thread::MailboxError::PushError(
                "system notifications must bypass business admission".into(),
            ));
        }
        Ok(msg)
    }
}
struct BatchObserver(mpsc::UnboundedSender<&'static str>);
impl Actor for BatchObserver {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn can_batch_message(&self, _: &BoxedMessage) -> bool {
        true
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        Box::pin(async move { messages.into_iter().map(Ok).collect() })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<ActorTerminated>() {
                let _ = self.0.send("common");
            } else if msg.is::<parrot::thread::system::Terminated>() {
                let _ = self.0.send("legacy");
            }
            Ok(msg)
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_notification_contracts_bypass_business_admission_and_batching() {
    use parrot_api::address::ActorRef;
    let sys = system(modes()[0].clone());
    let (sender, mut events) = mpsc::unbounded_channel();
    let watcher = sys
        .spawn_at(
            BatchObserver(sender),
            "/watcher",
            None,
            parrot::thread::ThreadActorConfig {
                message_admission: Some(Arc::new(BusinessPolicy)),
                message_batch_size: Some(8),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    watcher.send(Box::new(())).await.unwrap();
    let (target, _) = spawn(&sys, "/target").await;
    target.watch(watcher.clone_boxed()).await.unwrap();
    sys.watch("/watcher".into(), "/target".into())
        .await
        .unwrap();
    target.stop().await.unwrap();
    let mut received = Vec::new();
    for _ in 0..2 {
        received.push(
            tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    received.sort();
    assert_eq!(received, ["common", "legacy"]);
    sys.shutdown_internal().await.unwrap();
}
