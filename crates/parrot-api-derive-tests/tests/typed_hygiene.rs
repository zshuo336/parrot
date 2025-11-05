//! Independent consumer regressions: these modules intentionally do not import
//! TypedReceive. All calls exercise the generated inject/dispatch/extract path.

async fn round_trip<A, M>(actor: &mut A, message: M) -> M::Result
where
    A: ::parrot_api::typed::ParrotTypedDispatch,
    M: ::parrot_api::typed::ParrotMsgVariant<A>,
{
    let envelope = <M as ::parrot_api::typed::ParrotMsgVariant<A>>::inject(message);
    let reply = <A as ::parrot_api::typed::ParrotTypedDispatch>::dispatch(actor, envelope)
        .await
        .unwrap();
    <M as ::parrot_api::typed::ParrotMsgVariant<A>>::extract(reply).unwrap()
}

mod no_implicit_trait {
    pub struct Query;
    impl ::parrot_api::message::Message for Query {
        type Result = u64;
    }
    #[derive(::parrot_api::ParrotTypedActor)]
    #[ParrotTypedActor(msgs(Query))]
    pub struct Service;
    impl ::parrot_api::typed::TypedReceive<Query> for Service {
        fn receive_typed<'a>(
            &'a mut self,
            _: Query,
        ) -> ::parrot_api::types::BoxedFuture<'a, ::parrot_api::types::ActorResult<u64>> {
            ::std::boxed::Box::pin(async { ::std::result::Result::Ok(42) })
        }
    }
    #[tokio::test]
    async fn protocol_trait_need_not_be_imported() {
        assert_eq!(super::round_trip(&mut Service, Query).await, 42);
    }
}

mod raw_actor {
    pub struct Query;
    impl ::parrot_api::message::Message for Query {
        type Result = u64;
    }
    #[allow(non_camel_case_types)]
    #[derive(::parrot_api::ParrotTypedActor)]
    #[ParrotTypedActor(msgs(Query))]
    pub struct r#type;
    impl ::parrot_api::typed::TypedReceive<Query> for r#type {
        fn receive_typed<'a>(
            &'a mut self,
            _: Query,
        ) -> ::parrot_api::types::BoxedFuture<'a, ::parrot_api::types::ActorResult<u64>> {
            ::std::boxed::Box::pin(async { ::std::result::Result::Ok(42) })
        }
    }
    #[tokio::test]
    async fn raw_actor_identifier_generates_usable_envelopes() {
        assert_eq!(super::round_trip(&mut r#type, Query).await, 42);
    }
}

mod shadowed_standard_names {
    struct Box;
    enum LocalResult<T, E> {
        Ok(T),
        Err(E),
    }
    use LocalResult::{Err, Ok};
    pub struct Query;
    impl ::parrot_api::message::Message for Query {
        type Result = u64;
    }
    #[derive(::parrot_api::ParrotTypedActor)]
    #[ParrotTypedActor(msgs(Query))]
    pub struct Service;
    impl ::parrot_api::typed::TypedReceive<Query> for Service {
        fn receive_typed<'a>(
            &'a mut self,
            _: Query,
        ) -> ::parrot_api::types::BoxedFuture<'a, ::parrot_api::types::ActorResult<u64>> {
            ::std::boxed::Box::pin(async { ::std::result::Result::Ok(42) })
        }
    }
    #[tokio::test]
    async fn caller_box_and_result_variants_cannot_replace_generated_symbols() {
        let _local_box = Box;
        let success: LocalResult<u64, ()> = Ok(7);
        let failure: LocalResult<(), u64> = Err(9);
        assert!(matches!(success, Ok(7)));
        assert!(matches!(failure, Err(9)));
        assert_eq!(super::round_trip(&mut Service, Query).await, 42);
    }
}

mod shadowed_crate {
    mod parrot_api {}
    pub struct Query;
    impl ::parrot_api::message::Message for Query {
        type Result = u64;
    }
    #[derive(::parrot_api::ParrotTypedActor)]
    #[ParrotTypedActor(msgs(Query))]
    pub struct Service;
    impl ::parrot_api::typed::TypedReceive<Query> for Service {
        fn receive_typed<'a>(
            &'a mut self,
            _: Query,
        ) -> ::parrot_api::types::BoxedFuture<'a, ::parrot_api::types::ActorResult<u64>> {
            ::std::boxed::Box::pin(async { ::std::result::Result::Ok(42) })
        }
    }
    #[tokio::test]
    async fn local_module_does_not_shadow_the_external_api() {
        assert_eq!(super::round_trip(&mut Service, Query).await, 42);
    }
}

mod inherent_method {
    pub struct Query;
    impl ::parrot_api::message::Message for Query {
        type Result = u64;
    }
    #[derive(::parrot_api::ParrotTypedActor)]
    #[ParrotTypedActor(msgs(Query))]
    pub struct Service;
    impl Service {
        fn receive_typed(
            &mut self,
            _: Query,
        ) -> ::parrot_api::types::BoxedFuture<'_, ::parrot_api::types::ActorResult<u64>> {
            ::std::boxed::Box::pin(async { ::std::result::Result::Ok(999) })
        }
    }
    impl ::parrot_api::typed::TypedReceive<Query> for Service {
        fn receive_typed<'a>(
            &'a mut self,
            _: Query,
        ) -> ::parrot_api::types::BoxedFuture<'a, ::parrot_api::types::ActorResult<u64>> {
            ::std::boxed::Box::pin(async { ::std::result::Result::Ok(42) })
        }
    }
    #[tokio::test]
    async fn dispatch_uses_protocol_even_when_an_inherent_method_has_the_same_name() {
        let mut service = Service;
        assert_eq!(service.receive_typed(Query).await.unwrap(), 999);
        assert_eq!(super::round_trip(&mut service, Query).await, 42);
    }
}

mod explicit_lifecycle {
    pub struct Query;
    impl ::parrot_api::message::Message for Query {
        type Result = usize;
    }
    #[derive(::parrot_api::ParrotTypedActor)]
    #[ParrotTypedActor(msgs(Query), lifecycle)]
    pub struct Service<T: Send + 'static> {
        value: usize,
        owned: T,
    }
    impl<T: Send + 'static> Service<T> {
        fn init(&mut self) {
            self.value = 999;
        }
        fn before_stop(&mut self) {
            self.value = 888;
        }
    }
    impl<T: Send + 'static> ::parrot_api::typed::TypedLifecycle for Service<T> {
        fn init(
            &mut self,
        ) -> ::parrot_api::types::BoxedFuture<'_, ::parrot_api::types::ActorResult<()>> {
            ::std::boxed::Box::pin(async move {
                self.value = 42;
                ::std::result::Result::Ok(())
            })
        }
        fn before_stop(
            &mut self,
        ) -> ::parrot_api::types::BoxedFuture<'_, ::parrot_api::types::ActorResult<()>> {
            ::std::boxed::Box::pin(async move {
                self.value = 84;
                ::std::result::Result::Ok(())
            })
        }
    }
    impl<T: Send + 'static> ::parrot_api::typed::TypedReceive<Query> for Service<T> {
        fn receive_typed(
            &mut self,
            _: Query,
        ) -> ::parrot_api::types::BoxedFuture<'_, ::parrot_api::types::ActorResult<usize>> {
            ::std::boxed::Box::pin(async move { ::std::result::Result::Ok(self.value) })
        }
    }
    #[tokio::test]
    async fn generic_lifecycle_uses_qualified_trait_without_import_or_inherent_capture() {
        let mut actor = Service {
            value: 0,
            owned: String::from("owned"),
        };
        actor.init();
        assert_eq!(actor.value, 999);
        <Service<String> as ::parrot_api::typed::ParrotTypedDispatch>::init(&mut actor)
            .await
            .unwrap();
        assert_eq!(super::round_trip(&mut actor, Query).await, 42);
        actor.before_stop();
        assert_eq!(actor.value, 888);
        <Service<String> as ::parrot_api::typed::ParrotTypedDispatch>::before_stop(&mut actor)
            .await
            .unwrap();
        assert_eq!(actor.value, 84);
        assert_eq!(actor.owned, "owned");
    }
}
mod lifecycle_without_opt_in {
    pub struct Query;
    impl ::parrot_api::message::Message for Query {
        type Result = ();
    }
    #[derive(::parrot_api::ParrotTypedActor)]
    #[ParrotTypedActor(msgs(Query))]
    struct Service;
    impl ::parrot_api::typed::TypedLifecycle for Service {
        fn init(
            &mut self,
        ) -> ::parrot_api::types::BoxedFuture<'_, ::parrot_api::types::ActorResult<()>> {
            panic!("not opted in")
        }
        fn before_stop(
            &mut self,
        ) -> ::parrot_api::types::BoxedFuture<'_, ::parrot_api::types::ActorResult<()>> {
            panic!("not opted in")
        }
    }
    impl ::parrot_api::typed::TypedReceive<Query> for Service {
        fn receive_typed(
            &mut self,
            _: Query,
        ) -> ::parrot_api::types::BoxedFuture<'_, ::parrot_api::types::ActorResult<()>> {
            ::std::boxed::Box::pin(async { ::std::result::Result::Ok(()) })
        }
    }
    #[tokio::test]
    async fn adding_lifecycle_trait_does_not_silently_change_non_opted_in_actors() {
        <Service as ::parrot_api::typed::ParrotTypedDispatch>::init(&mut Service)
            .await
            .unwrap();
        <Service as ::parrot_api::typed::ParrotTypedDispatch>::before_stop(&mut Service)
            .await
            .unwrap();
    }
}
