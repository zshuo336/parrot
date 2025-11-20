#![cfg(feature = "host")]

use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::ActorTerminated,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
    host::{ApplicationLookup, GatewayRoutes, ThreadPropsFactory, ThreadPropsSpawner},
    manifest::{AppManifest, ComponentSpec},
    planner::{plan, LocalTopology},
};
use parrot_remote::{RemoteActorSystem, RemoteConfig};
use std::{sync::Arc, time::Duration};

#[derive(Default)]
struct Observer(Vec<ActorTerminated>);
impl Actor for Observer {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(event) = message.downcast_ref::<ActorTerminated>() {
                self.0.push(event.clone());
            }
            Ok(Box::new(self.0.clone()) as BoxedMessage)
        })
    }
}
fn spawn(
    engine: Arc<ThreadActorSystem>,
    path: String,
    _: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        engine
            .spawn_at(Observer::default(), &path, None, Default::default())
            .await
            .map(|actor| Box::new(actor) as BoxedActorRef)
            .map_err(|e| ActorError::InternalError(e.to_string()))
    })
}
parrot_api::message::inventory::submit! {
    ThreadPropsFactory { name: "app.death-watch-test", spawn }
}
struct NoGateway;
#[async_trait::async_trait]
impl GatewayFactory for NoGateway {
    async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        Ok(())
    }
    async fn start_gateway(
        &self,
        _: &ComponentSpec,
        _: &[String],
        _: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("unexpected gateway")
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        panic!("unexpected gateway")
    }
}
struct Host {
    engine: Arc<ThreadActorSystem>,
    remote: Arc<RemoteActorSystem>,
    context: AssemblingContext,
    spawner: ThreadPropsSpawner,
    manifest: AppManifest,
}
impl Host {
    async fn new(mirrored: bool) -> Self {
        let engine = ThreadActorSystem::shared(Default::default());
        let remote = RemoteActorSystem::new(
            RemoteConfig::tcp("application-watch", Some("127.0.0.1:0".parse().unwrap())),
            Arc::new(ApplicationLookup {
                ts: engine.clone(),
                routes: Arc::new(GatewayRoutes::default()),
            }),
        )
        .unwrap();
        let mut manifest = AppManifest::from_toml(
            "name = 'watch-app'\nversion = '1.0.0'\n\
             [[components]]\nname = 'primary'\nengine = 'parrot'\n\
             artifact = { Props = { factory = 'app.death-watch-test' } }\n\
             [[components]]\nname = 'shadow'\nengine = 'parrot'\n\
             artifact = { Props = { factory = 'app.death-watch-test' } }\n\
             [[wiring]]\nfrom = 'shadow:/user/out'\nto = 'primary:/user/primary'\n",
        )
        .unwrap();
        if mirrored {
            manifest
                .mirrors
                .push(parrot_app::mirror::MirrorSpec::new("primary", "shadow"));
        }
        let context = AssemblingContext::new().with_mirror_host(&remote);
        let spawner = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
        let mut host = Self {
            engine,
            remote,
            context,
            spawner,
            manifest,
        };
        host.assemble().await;
        host
    }
    async fn assemble(&mut self) {
        let deployer = LocalDeployer {
            topology: &LocalTopology,
            gateway_factory: &NoGateway,
            parrot_spawner: &self.spawner,
        };
        self.context
            .assemble(
                &plan(&self.manifest, &LocalTopology).unwrap(),
                &deployer,
                &self.manifest,
                &Default::default(),
            )
            .await
            .unwrap();
    }
    async fn teardown(&mut self) {
        let deployer = LocalDeployer {
            topology: &LocalTopology,
            gateway_factory: &NoGateway,
            parrot_spawner: &self.spawner,
        };
        self.context.teardown(&deployer).await.unwrap();
    }
    async fn close(mut self) {
        self.teardown().await;
        self.remote.shutdown().await.unwrap();
        self.engine.shutdown_internal().await.unwrap();
    }
}
async fn events(actor: &dyn ActorRef, count: usize) -> Vec<ActorTerminated> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = *actor
                .send(Box::new(()))
                .await
                .unwrap()
                .downcast::<Vec<ActorTerminated>>()
                .unwrap();
            if events.len() >= count {
                assert_eq!(events.len(), count, "duplicate termination notification");
                return events;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("termination notification deadline")
}

async fn connection_identity_and_notification(mirrored: bool) {
    let mut host = Host::new(mirrored).await;
    let watcher = spawn(host.engine.clone(), "/user/watcher".into(), None)
        .await
        .unwrap();
    let wire = host.context.wired_ref("shadow:/user/out").unwrap();
    let cloned = wire.clone_boxed();
    let component = host.context.component_ref("primary").unwrap().clone_boxed();
    let raw = host.engine.get_actor_ref("/user/primary").unwrap();
    let identity = raw.death_watch().unwrap();
    wire.watch(watcher.clone_boxed()).await.unwrap();
    cloned.watch(watcher.clone_boxed()).await.unwrap();
    component.watch(watcher.clone_boxed()).await.unwrap();
    for actor in [&*wire, &*cloned, &*component] {
        assert_eq!(actor.death_watch().unwrap().id(), identity.id());
    }
    assert_eq!(identity.subscriptions(), (1, 0));
    assert!(wire.stop().await.is_err());
    assert!(raw.is_alive().await);
    host.teardown().await;
    let notifications = events(&*watcher, 1).await;
    assert_eq!(notifications[0].generation, identity.id());
    assert_eq!(notifications[0].path, "/user/primary");
    assert_eq!(identity.subscriptions(), (0, 0));

    host.assemble().await;
    let replacement = host.context.wired_ref("shadow:/user/out").unwrap();
    assert_ne!(replacement.death_watch().unwrap().id(), identity.id());
    assert!(matches!(
        wire.watch(watcher.clone_boxed()).await,
        Err(ActorError::Stopped)
    ));
    replacement.watch(watcher.clone_boxed()).await.unwrap();
    host.teardown().await;
    let notifications = events(&*watcher, 2).await;
    assert_ne!(notifications[0].generation, notifications[1].generation);
    assert_eq!(wire.death_watch().unwrap().id(), identity.id());
    watcher.stop().await.unwrap();
    host.close().await;
}

#[tokio::test]
async fn connection_preserves_identity_notifications_and_same_path_isolation() {
    connection_identity_and_notification(false).await;
}
#[tokio::test]
async fn mirrored_connection_and_component_preserve_identity_and_notifications() {
    connection_identity_and_notification(true).await;
}

#[tokio::test]
async fn connection_clones_can_cancel_an_existing_watch() {
    let mut host = Host::new(true).await;
    let watcher = spawn(host.engine.clone(), "/user/watcher".into(), None)
        .await
        .unwrap();
    let wire = host.context.wired_ref("shadow:/user/out").unwrap();
    let raw = host.engine.get_actor_ref("/user/primary").unwrap();
    // Establish through the raw reference so this independently tests unwatch.
    raw.watch(watcher.clone_boxed()).await.unwrap();
    wire.clone_boxed()
        .unwatch(watcher.clone_boxed())
        .await
        .unwrap();
    assert_eq!(raw.death_watch().unwrap().subscriptions(), (0, 0));
    host.teardown().await;
    assert!(events(&*watcher, 0).await.is_empty());
    watcher.stop().await.unwrap();
    host.close().await;
}

#[tokio::test]
async fn mirrored_component_can_receive_notifications_as_a_watcher() {
    let host = Host::new(true).await;
    let external = spawn(host.engine.clone(), "/user/external".into(), None)
        .await
        .unwrap();
    let watcher = host.context.component_ref("primary").unwrap().clone_boxed();
    let generation = external.death_watch().unwrap().id();
    external.watch(watcher.clone_boxed()).await.unwrap();
    external.stop().await.unwrap();
    let notifications = events(&*watcher, 1).await;
    assert_eq!(notifications[0].generation, generation);
    assert_eq!(notifications[0].path, "/user/external");
    assert_eq!(watcher.death_watch().unwrap().subscriptions(), (0, 0));
    let shadow = host.context.component_ref("shadow").unwrap();
    assert!(events(&**shadow, 0).await.is_empty());
    host.close().await;
}
