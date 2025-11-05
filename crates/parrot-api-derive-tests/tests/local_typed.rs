//! This consumer depends on parrot-api only, never a concrete actor engine.
use ::parrot_api::{
    typed_context::{TypedContext, TypedContextServices},
    types::BoxedActorRef,
};
use ::std::{cell::Cell, rc::Rc, sync::Arc};

struct Services;
impl TypedContextServices for Services {
    fn path(&self) -> &str {
        "/independent"
    }
    fn actor_ref(&self) -> Option<BoxedActorRef> {
        None
    }
    fn parent(&self) -> Option<BoxedActorRef> {
        None
    }
    fn children(&self) -> ::parrot_api::context::ReadOnlyChildrenVec {
        ::parrot_api::context::ReadOnlyChildrenVec::new(Default::default())
    }
    fn stop(&self) {}
    fn set_child_strategy(&self, _: ::parrot_api::supervisor::SupervisorStrategyType) {}
}
fn context() -> TypedContext {
    TypedContext::from_services(Arc::new(Services))
}
// Deliberately shadow unqualified generated symbols and omit receiver imports.
mod parrot_api {}
struct Box;
enum Result {
    Ok,
    Err,
}
pub struct Set(u64);
impl ::parrot_api::message::Message for Set {
    type Result = u64;
}
pub struct Read;
impl ::parrot_api::message::Message for Read {
    type Result = String;
}

#[allow(non_camel_case_types)]
#[derive(::parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(Set, Read), local, lifecycle)]
struct r#type<T>
where
    T: Clone + 'static,
{
    value: Rc<Cell<u64>>,
    owned: T,
}
impl<T: Clone + 'static> ::parrot_api::local_typed::LocalTypedReceive<Set> for r#type<T> {
    fn receive_typed(
        &mut self,
        message: Set,
    ) -> ::parrot_api::local_typed::LocalBoxedFuture<'_, ::parrot_api::types::ActorResult<u64>>
    {
        let value = self.value.clone();
        ::std::boxed::Box::pin(async move {
            tokio::task::yield_now().await;
            value.set(message.0);
            ::std::result::Result::Ok(value.get())
        })
    }
}
impl<T: Clone + 'static> ::parrot_api::local_typed::LocalTypedReceive<Read> for r#type<T> {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Read,
        ctx: &'a TypedContext,
    ) -> ::parrot_api::local_typed::LocalBoxedFuture<'a, ::parrot_api::types::ActorResult<String>>
    {
        ::std::boxed::Box::pin(async move {
            ::std::result::Result::Ok(format!("{}:{}", ctx.path(), self.value.get()))
        })
    }
}
impl<T: Clone + 'static> ::parrot_api::local_typed::LocalTypedLifecycle for r#type<T> {
    fn init(
        &mut self,
    ) -> ::parrot_api::local_typed::LocalBoxedFuture<'_, ::parrot_api::types::ActorResult<()>> {
        let owned = self.owned.clone();
        ::std::boxed::Box::pin(async move {
            tokio::task::yield_now().await;
            drop(owned);
            self.value.set(11);
            ::std::result::Result::Ok(())
        })
    }
    fn before_stop(
        &mut self,
    ) -> ::parrot_api::local_typed::LocalBoxedFuture<'_, ::parrot_api::types::ActorResult<()>> {
        ::std::boxed::Box::pin(async move {
            self.value.set(99);
            ::std::result::Result::Ok(())
        })
    }
}
impl<T: Clone + 'static> r#type<T> {
    fn receive_typed(&mut self, _: Set) -> u64 {
        999
    }
    fn init(&mut self) {
        self.value.set(888);
    }
    fn before_stop(&mut self) {
        self.value.set(777);
    }
}
type Actor = r#type<Rc<()>>;
type Protocol = ::parrot_api::local_typed::LocalProtocol<Actor>;
fn actor() -> Actor {
    r#type {
        value: Rc::new(Cell::new(0)),
        owned: Rc::new(()),
    }
}
async fn dispatch<M: ::parrot_api::typed::ParrotMsgVariant<Protocol>>(
    actor: &mut Actor,
    message: M,
) -> M::Result {
    let envelope = <M as ::parrot_api::typed::ParrotMsgVariant<Protocol>>::inject(message);
    let ctx = context();
    let reply =
        <Actor as ::parrot_api::local_typed::LocalTypedDispatch>::dispatch(actor, envelope, &ctx)
            .await
            .unwrap();
    <M as ::parrot_api::typed::ParrotMsgVariant<Protocol>>::extract(reply).unwrap()
}
#[tokio::test]
async fn local_generics_raw_identifiers_and_hygiene_use_qualified_protocol() {
    let _shadow = (Box, Result::Ok, Result::Err);
    let mut actor = actor();
    assert_eq!(actor.receive_typed(Set(1)), 999);
    assert_eq!(dispatch(&mut actor, Set(42)).await, 42);
    assert_eq!(dispatch(&mut actor, Read).await, "/independent:42");
}
#[tokio::test]
async fn local_lifecycle_defaults_forward_to_traits_without_inherent_capture() {
    let mut actor = actor();
    let ctx = context();
    actor.init();
    assert_eq!(actor.value.get(), 888);
    <Actor as ::parrot_api::local_typed::LocalTypedDispatch>::init(&mut actor, &ctx)
        .await
        .unwrap();
    assert_eq!(dispatch(&mut actor, Read).await, "/independent:11");
    actor.before_stop();
    assert_eq!(actor.value.get(), 777);
    <Actor as ::parrot_api::local_typed::LocalTypedDispatch>::before_stop(&mut actor, &ctx)
        .await
        .unwrap();
    assert_eq!(actor.value.get(), 99);
}
#[test]
fn local_reply_variant_mismatch_is_an_error() {
    assert!(
        <Set as ::parrot_api::typed::ParrotMsgVariant<Protocol>>::extract(typeReply::Read(
            "wrong".into()
        ))
        .is_err()
    );
    assert!(
        <Read as ::parrot_api::typed::ParrotMsgVariant<Protocol>>::extract(typeReply::Set(7))
            .is_err()
    );
}

// Existing manual protocol mappings, including generic local actors, coexist
// with the opt-in actor-side adapter without acquiring new trait requirements.
struct Legacy<T: 'static>(T);
impl<T: 'static> ::parrot_api::local_typed::LocalTypedDispatch for Legacy<T> {
    type Msg = Set;
    type Reply = u64;
    fn dispatch<'a>(
        &'a mut self,
        message: Set,
        _: &'a TypedContext,
    ) -> ::parrot_api::local_typed::LocalBoxedFuture<'a, ::parrot_api::types::ActorResult<u64>>
    {
        ::std::boxed::Box::pin(async move { ::std::result::Result::Ok(message.0) })
    }
}
impl<T: 'static>
    ::parrot_api::typed::ParrotMsgVariant<::parrot_api::local_typed::LocalProtocol<Legacy<T>>>
    for Set
{
    fn inject(message: Self) -> Set {
        message
    }
    fn extract(reply: u64) -> ::parrot_api::types::ActorResult<u64> {
        ::std::result::Result::Ok(reply)
    }
}
#[tokio::test]
async fn existing_generic_manual_mappings_still_compile_and_dispatch() {
    type P = ::parrot_api::local_typed::LocalProtocol<Legacy<Rc<()>>>;
    let mut actor = Legacy(Rc::new(()));
    let ctx = context();
    let message = <Set as ::parrot_api::typed::ParrotMsgVariant<P>>::inject(Set(51));
    let reply = <Legacy<Rc<()>> as ::parrot_api::local_typed::LocalTypedDispatch>::dispatch(
        &mut actor, message, &ctx,
    )
    .await
    .unwrap();
    assert_eq!(
        <Set as ::parrot_api::typed::ParrotMsgVariant<P>>::extract(reply).unwrap(),
        51
    );
}
