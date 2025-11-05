use crate::{deadline, safe_drop, SendError, TypedActorRef, TypedAskRef};
use std::marker::PhantomData;
use std::time::Duration;
/// A statically typed variant of a shared actor protocol. The macro generates
/// these mappings. Custom implementations must recover their own packed value.
pub trait ProtocolMessage<P, Q>: Send + 'static {
    type Reply: Send + 'static;
    fn pack(self) -> P;
    fn recover(message: P) -> Self;
    fn reply(reply: Q) -> Result<Self::Reply, String>;
}
pub struct ProtocolRef<M, P, Q> {
    inner: TypedActorRef<P, Q>,
    marker: PhantomData<fn(M)>,
}
impl<M, P, Q> Clone for ProtocolRef<M, P, Q> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            marker: PhantomData,
        }
    }
}
impl<M, P, Q> ProtocolRef<M, P, Q> {
    pub(crate) fn new(inner: TypedActorRef<P, Q>) -> Self {
        Self {
            inner,
            marker: PhantomData,
        }
    }
    pub fn path(&self) -> &str {
        self.inner.path()
    }
    pub fn generation(&self) -> u64 {
        self.inner.generation()
    }
    pub fn stop(&self) {
        self.inner.stop();
    }
    pub fn wait_stopped(&self, timeout: Duration) -> Result<(), String> {
        self.inner.wait_stopped(timeout)
    }
}
impl<M, P, Q> TypedAskRef<M> for ProtocolRef<M, P, Q>
where
    M: ProtocolMessage<P, Q>,
    P: Send + 'static,
    Q: Send + 'static,
{
    type Reply = M::Reply;
    fn ask_with_timeout(&self, msg: M, timeout: Duration) -> Result<Self::Reply, String> {
        let end = match deadline(timeout) {
            Ok(d) => d,
            Err(e) => {
                safe_drop(msg);
                return Err(e);
            }
        };
        self.inner.ask_until(msg.pack(), end).and_then(M::reply)
    }
    fn tell(&self, msg: M) -> Result<(), SendError<M>> {
        self.inner.tell(msg.pack()).map_err(|error| SendError {
            reason: error.reason,
            message: M::recover(error.message),
        })
    }
}
/// Generate a request enum, reply enum and adapter that dispatches several
/// TypedReceive implementations against ONE actor instance, without Any.
///
/// ```
/// use typed_static::{typed_actor, SimpleEngine, TypedReceive, TypedAskRef};
/// struct Add(u64); struct Get;
/// struct Counter(u64);
/// impl TypedReceive<Add> for Counter {
///     type Reply = u64;
///     fn receive_typed(&mut self, m: Add) -> Result<u64,String> {self.0 += m.0; Ok(self.0)}
/// }
/// impl TypedReceive<Get> for Counter {
///     type Reply = u64;
///     fn receive_typed(&mut self, _: Get) -> Result<u64,String> {Ok(self.0)}
/// }
/// typed_actor! { CounterRequest, CounterReply, CounterActor { Add(Add) => u64, Get(Get) => u64 } }
/// let engine = SimpleEngine::new();
/// let actor = engine.actor(CounterActor(Counter(0)), "counter").unwrap();
/// let add = actor.for_message::<Add>(); let get = actor.for_message::<Get>();
/// add.tell(Add(42)).unwrap(); assert_eq!(get.ask(Get).unwrap(), 42);
/// engine.shutdown().unwrap();
/// ```
#[macro_export]
macro_rules! typed_actor {
    ($vis:vis $request:ident, $reply:ident, $adapter:ident { $($variant:ident($message:ty) => $response:ty),+ $(,)? }) => {
        $vis enum $request { $($variant($message)),+ }
        $vis enum $reply { $($variant($response)),+ }
        $vis struct $adapter<A>(pub A);
        impl<A> $crate::TypedReceive<$request> for $adapter<A>
        where A: $($crate::TypedReceive<$message, Reply = $response> +)+ 'static {
            type Reply = $reply;
            fn receive_typed(&mut self, message: $request) -> ::std::result::Result<$reply, ::std::string::String> {
                match message { $($request::$variant(value) => <A as $crate::TypedReceive<$message>>::receive_typed(&mut self.0, value).map($reply::$variant)),+ }
            }
        }
        $(impl $crate::ProtocolMessage<$request, $reply> for $message {
            type Reply = $response;
            fn pack(self) -> $request { $request::$variant(self) }
            fn recover(value: $request) -> Self {
                #[allow(unreachable_patterns)]
                match value { $request::$variant(value) => value, _ => unreachable!("rejected request changed its variant") }
            }
            fn reply(value: $reply) -> ::std::result::Result<Self::Reply, ::std::string::String> {
                #[allow(unreachable_patterns)]
                match value { $reply::$variant(value) => Ok(value), _ => Err("actor replied with the wrong protocol variant".into()) }
            }
        })+
    };
}
