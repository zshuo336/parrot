//! A real owner-thread actor with Rc state and a Send protocol reference.
//! Run: cargo run --locked -p parrot --example local_runtime
use parrot::{actix::ActixActorSystem, typed_context::TypedContextExt};
use parrot_api::{
    ParrotTypedActor,
    address::ActorRefExt,
    local_typed::{LocalBoxedFuture, LocalTypedReceive},
    message::Message,
    typed_context::TypedContext,
    types::ActorResult,
};
use std::{cell::RefCell, rc::Rc, sync::Arc, thread::ThreadId};

pub struct Add(u64);
impl Message for Add {
    type Result = u64;
}
pub struct Fail;
impl Message for Fail {
    type Result = ();
}
pub struct Context;
impl Message for Context {
    type Result = TypedContext;
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Add, Context, Fail), local)]
struct Counter {
    value: Rc<RefCell<u64>>,
    owner: ThreadId,
}
impl LocalTypedReceive<Add> for Counter {
    fn receive_typed<'a>(&'a mut self, Add(amount): Add) -> LocalBoxedFuture<'a, ActorResult<u64>> {
        let value = self.value.clone();
        Box::pin(async move {
            assert_eq!(std::thread::current().id(), self.owner);
            // This future holds Rc across an await; it cannot be sent to Tokio's
            // general executor. The local actor runtime polls it on its owner.
            tokio::task::yield_now().await;
            assert_eq!(std::thread::current().id(), self.owner);
            *value.borrow_mut() += amount;
            Ok(*value.borrow())
        })
    }
}
impl LocalTypedReceive<Fail> for Counter {
    fn receive_typed(&mut self, _: Fail) -> LocalBoxedFuture<'_, ActorResult<()>> {
        Box::pin(async {
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "example failure".into(),
            ))
        })
    }
}
impl LocalTypedReceive<Context> for Counter {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Context,
        ctx: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<TypedContext>> {
        Box::pin(async move { Ok(ctx.clone()) })
    }
}
impl Drop for Counter {
    fn drop(&mut self) {
        assert_eq!(std::thread::current().id(), self.owner);
    }
}
#[actix::main]
async fn main() -> anyhow::Result<()> {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(2).await?);
    let caller = std::thread::current().id();
    let work = async {
        let counter = system
            .spawn_local_typed_factory::<Counter, Add, _>(
                move || {
                    let owner = std::thread::current().id();
                    assert_ne!(owner, caller);
                    Counter {
                        // Construct Rc here, after the Send closure reaches its
                        // owner. Do not capture an already-created Rc actor.
                        value: Rc::new(RefCell::new(0)),
                        owner,
                    }
                },
                "/user/local-counter",
            )
            .await?;
        // Admission succeeded. The first request waits for initialization.
        let other_thread = counter.clone();
        let answer = tokio::task::spawn_blocking(move || {
            futures::executor::block_on(other_thread.ask(Add(7)))
        })
        .await??;
        anyhow::ensure!(answer == 7, "incorrect reply from another thread");
        let found = system
            .get_actor(&"/user/local-counter".into())
            .await
            .ok_or_else(|| anyhow::anyhow!("missing local actor registration"))?;
        anyhow::ensure!(found.ask(Add(5)).await? == 12, "incorrect dynamic reply");
        anyhow::ensure!(
            found.death_watch().unwrap().id()
                == counter.clone().into_dyn().death_watch().unwrap().id(),
            "lookup returned a different generation"
        );
        let context = counter.ref_for::<Context>().ask(Context).await?;
        let child = context
            .spawn_local_typed_child::<Counter, Add, _>(|| Counter {
                value: Rc::new(RefCell::new(100)),
                owner: std::thread::current().id(),
            })
            .await?;
        anyhow::ensure!(
            child.ask(Add(2)).await? == 102,
            "incorrect Local child reply"
        );
        let child_context = child.ref_for::<Context>().ask(Context).await?;
        anyhow::ensure!(
            child_context.parent().unwrap().path() == counter.path(),
            "incorrect parent identity"
        );
        anyhow::ensure!(
            context.children().len() == 1,
            "child not owned by parent scope"
        );
        // A failed request is reported once. The same logical reference then
        // reaches a fresh instance, after the old generation's child retired.
        anyhow::ensure!(
            counter.ref_for::<Fail>().ask(Fail).await.is_err(),
            "failed request unexpectedly succeeded"
        );
        anyhow::ensure!(
            counter.ask(Add(1)).await? == 1,
            "factory did not reset state"
        );
        anyhow::ensure!(
            !child.is_alive() && context.children().is_empty(),
            "replacement overtook old child cleanup"
        );
        let current = counter.ref_for::<Context>().ask(Context).await?;
        let replacement = current
            .spawn_local_typed_child_factory::<Counter, Add, _>(|| Counter {
                value: Rc::new(RefCell::new(200)),
                owner: std::thread::current().id(),
            })
            .await?;
        anyhow::ensure!(
            replacement.ask(Add(2)).await? == 202,
            "replacement child failed"
        );
        found.stop().await?;
        anyhow::ensure!(
            !replacement.is_alive() && current.children().is_empty(),
            "final stop left replacement child alive"
        );
        anyhow::ensure!(
            !child.is_alive() && context.children().is_empty(),
            "parent did not join child retirement"
        );
        anyhow::ensure!(!counter.is_alive(), "local actor remained alive after stop");
        anyhow::ensure!(
            system
                .get_actor(&"/user/local-counter".into())
                .await
                .is_none(),
            "local actor remained registered after stop"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    // Always join cleanup, including when application work fails.
    let stopped = system.as_ref().clone().shutdown().await;
    work?;
    stopped?;
    println!(
        "local: Rc across await, cross-thread ask, dynamic lookup, Local factory replacement, parent-owned children and owner cleanup verified"
    );
    Ok(())
}
