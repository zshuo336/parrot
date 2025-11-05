// Each doctest includes a complete consumer fixture. Both actor handler shapes
// are present so a silently discarded engine option cannot accidentally fail
// compilation just because the fallback engine needs a different handler.
#[allow(unused_macros)]
macro_rules! actor_case {
    ($($attributes:tt)*) => {
        mod __parrot_engine {
            pub type EngineContext<A> = ::std::marker::PhantomData<A>;
        }
        #[derive(parrot_api_derive::ParrotActor)]
        $($attributes)*
        struct Sample;
        #[allow(dead_code)]
        impl Sample {
            async fn handle_message(
                &mut self,
                _msg: parrot_api::types::BoxedMessage,
                _ctx: &mut <Self as parrot_api::actor::Actor>::Context,
            ) -> parrot_api::types::ActorResult<parrot_api::types::BoxedMessage> {
                ::std::result::Result::Ok(::std::boxed::Box::new(()))
            }
            fn handle_message_engine(
                &mut self,
                _msg: parrot_api::types::BoxedMessage,
                _ctx: &mut <Self as parrot_api::actor::Actor>::Context,
                _engine_ctx: parrot_api::actor::EngineContextHandle,
            ) -> ::std::option::Option<parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>> {
                ::std::option::Option::Some(::std::result::Result::Ok(::std::boxed::Box::new(())))
            }
        }
    };
}

#[allow(unused_macros)]
macro_rules! message_case {
    ($($attributes:tt)*) => {
        use parrot_api::message::Message as _;
        #[derive(parrot_api_derive::Message)]
        $($attributes)*
        struct Sample;
    };
}
