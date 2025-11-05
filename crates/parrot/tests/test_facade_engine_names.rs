//! Registration names are opaque keys. An accepted empty key must retain the
//! same spawn, lookup, fallback and mirror behavior as a nonempty key.
use parrot::{
    actix::ActixActorSystem,
    mirror::{MirrorCopier, MirrorPolicy},
    system::ParrotActorSystem,
    thread::{context::ThreadContext, system::ThreadActorSystem},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorPath, ActorRef},
    context::ActorContext,
    system::ActorSystem,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{sync::Arc, time::Duration};

struct Identity(u64);
impl Actor for Identity {
    type Config = Box<dyn std::any::Any + Send>;
    type Context = dyn ActorContext;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(
            async move { Ok(Box::new((self.0, *msg.downcast::<u64>().unwrap())) as BoxedMessage) },
        )
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

struct ThreadIdentity {
    id: u64,
    observed: Option<tokio::sync::mpsc::UnboundedSender<u64>>,
}
impl Actor for ThreadIdentity {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let n = *msg.downcast::<u64>().unwrap();
            if let Some(tx) = &self.observed {
                tx.send(n).unwrap();
            }
            Ok(Box::new((self.id, n)) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

async fn register(facade: &ParrotActorSystem, name: &str, actix: bool, default: bool) {
    if actix {
        facade
            .register_actix_system(name.into(), ActixActorSystem::new().await.unwrap(), default)
            .await
            .unwrap();
    } else {
        facade
            .register_thread_system(
                name.into(),
                ThreadActorSystem::shared(Default::default()),
                default,
            )
            .await
            .unwrap();
    }
}
async fn spawn(facade: &ParrotActorSystem, id: u64) -> BoxedActorRef {
    facade
        .spawn_root_boxed(Box::new(Identity(id)), Box::new(()))
        .await
        .unwrap()
}
async fn answer(actor: &dyn ActorRef) -> (u64, u64) {
    *actor
        .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(3)))
        .await
        .unwrap()
        .downcast::<(u64, u64)>()
        .unwrap()
}
async fn lookup(facade: &ParrotActorSystem, path: &str, public: bool) -> Option<(u64, u64)> {
    let path = ActorPath::placeholder(path);
    let actor = if public {
        facade.get_actor(&path).await
    } else {
        facade.internal_get_actor(&path).await
    };
    match actor {
        Some(actor) => Some(answer(actor.as_ref()).await),
        None => None,
    }
}

async fn empty_default(actix: bool, explicit: bool) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    register(&facade, "", actix, explicit).await;
    let actor = spawn(&facade, 17).await;
    assert_eq!(
        facade.list_registered_systems().unwrap(),
        vec![String::new()]
    );
    let direct = answer(actor.as_ref()).await;
    let internal = lookup(&facade, &actor.path(), false).await;
    let public = lookup(&facade, &actor.path(), true).await;
    facade.internal_shutdown().await.unwrap();
    assert_eq!(direct, (17, 42));
    assert_eq!(
        internal,
        Some(direct),
        "internal lookup lost the live actor"
    );
    assert_eq!(public, Some(direct), "public lookup lost the live actor");
}
#[tokio::test]
async fn thread_empty_explicit_default() {
    empty_default(false, true).await;
}
#[tokio::test]
async fn thread_empty_automatic_default() {
    empty_default(false, false).await;
}
#[test]
fn actix_empty_explicit_default() {
    actix::System::new().block_on(empty_default(true, true));
}
#[test]
fn actix_empty_automatic_default() {
    actix::System::new().block_on(empty_default(true, false));
}

async fn switch_default(empty_is_actix: bool) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    register(&facade, "named", !empty_is_actix, true).await;
    let named = spawn(&facade, 11).await;
    register(&facade, "", empty_is_actix, false).await;
    facade.set_default_system("").unwrap();
    let empty = spawn(&facade, 22).await;
    let mut results = Vec::new();
    // Empty-name actors must work both as the default and in fallback, including
    // switching back after a rejected update to a nonexistent key.
    for default in ["", "named", ""] {
        facade.set_default_system(default).unwrap();
        assert!(facade.set_default_system("missing").is_err());
        for public in [false, true] {
            results.push(lookup(&facade, &named.path(), public).await);
            results.push(lookup(&facade, &empty.path(), public).await);
        }
    }
    facade.internal_shutdown().await.unwrap();
    assert_eq!(results, [Some((11, 42)), Some((22, 42))].repeat(6));
}
#[test]
fn empty_actix_and_named_thread_survive_default_switches() {
    actix::System::new().block_on(switch_default(true));
}
#[test]
fn empty_thread_and_named_actix_survive_default_switches() {
    actix::System::new().block_on(switch_default(false));
}

#[tokio::test]
async fn named_thread_spawn_and_concrete_lookup_precede_prefix_fallback() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    register(&facade, "", false, true).await;
    register(&facade, "fallback", false, false).await;
    let local = facade
        .spawn_root_thread_in_system(
            "",
            ThreadIdentity {
                id: 1,
                observed: None,
            },
            EmptyConfig,
        )
        .await
        .unwrap();
    let fallback = facade
        .spawn_root_thread_in_system(
            "fallback",
            ThreadIdentity {
                id: 2,
                observed: None,
            },
            EmptyConfig,
        )
        .await
        .unwrap();
    facade
        .register_prefix_handler("/user/", Arc::from(fallback))
        .unwrap();
    let concrete = lookup(&facade, &local.path(), true).await;
    let missing = lookup(&facade, "/user/missing", true).await;
    facade.internal_shutdown().await.unwrap();
    assert_eq!(
        concrete,
        Some((1, 42)),
        "a wildcard must not mask the live actor"
    );
    assert_eq!(missing, Some((2, 42)));
}

#[tokio::test]
async fn mirror_primary_and_shadow_resolve_in_empty_named_engine() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    register(&facade, "", false, true).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let primary = facade
        .spawn_root_thread(
            ThreadIdentity {
                id: 1,
                observed: None,
            },
            EmptyConfig,
        )
        .await
        .unwrap();
    let shadow = facade
        .spawn_root_thread(
            ThreadIdentity {
                id: 2,
                observed: Some(tx),
            },
            EmptyConfig,
        )
        .await
        .unwrap();
    facade
        .register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
        .unwrap();
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new(primary.path(), shadow.path()))
        .unwrap();
    let reply = lookup(&facade, &primary.path(), true).await;
    let mirrored = if reply.is_some() {
        tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    facade.internal_shutdown().await.unwrap();
    assert_eq!(reply, Some((1, 42)));
    assert_eq!(
        mirrored,
        Some(42),
        "the worker must deliver to the actual shadow"
    );
    assert!(handle.is_finished());
}

#[tokio::test]
async fn no_registered_engine_keeps_missing_and_prefix_lookup_semantics() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let owner = ParrotActorSystem::new(Default::default()).await.unwrap();
    register(&owner, "external", false, true).await;
    let actor = spawn(&owner, 99).await;
    facade
        .register_prefix_handler("/external/", Arc::from(actor))
        .unwrap();
    let missing = lookup(&facade, "/user/missing", true).await;
    let prefix = lookup(&facade, "/external/item", true).await;
    facade.internal_shutdown().await.unwrap();
    owner.internal_shutdown().await.unwrap();
    assert_eq!(missing, None);
    assert_eq!(prefix, Some((99, 42)));
}
