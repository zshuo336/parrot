use crate::{
    deadline, panic_error, safe_drop, SimpleEngine, TypedActorRef, TypedAskRef, TypedReceive,
};
use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, Instant};
pub type BoxedMessage = Box<dyn Any + Send>;
pub type BoxedResult = Result<BoxedMessage, String>;
pub trait DynActorRef: Send + Sync {
    fn send_dyn(&self, msg: BoxedMessage) -> BoxedResult {
        self.send_dyn_with_timeout(msg, Duration::from_secs(5))
    }
    fn send_dyn_with_timeout(&self, msg: BoxedMessage, timeout: Duration) -> BoxedResult;
    fn path(&self) -> String;
    fn generation(&self) -> u64;
    fn stop(&self);
    fn wait_stopped(&self, timeout: Duration) -> Result<(), String>;
}
type DynamicHandler = Box<dyn FnMut(BoxedMessage) -> BoxedResult + Send>;
struct DynamicActor(DynamicHandler);
impl TypedReceive<BoxedMessage> for DynamicActor {
    type Reply = BoxedMessage;
    fn receive_typed(&mut self, msg: BoxedMessage) -> BoxedResult {
        (self.0)(msg)
    }
}
impl SimpleEngine {
    pub fn spawn_dyn(
        &self,
        actor: DynamicHandler,
        path: &str,
    ) -> Result<Arc<dyn DynActorRef>, String> {
        let reference = self.spawn_typed(DynamicActor(actor), path)?;
        Ok(Arc::new(NativeDyn(reference)))
    }
}
struct NativeDyn(TypedActorRef<BoxedMessage, BoxedMessage>);
impl DynActorRef for NativeDyn {
    fn send_dyn_with_timeout(&self, msg: BoxedMessage, timeout: Duration) -> BoxedResult {
        self.0.ask_with_timeout(msg, timeout)
    }
    fn path(&self) -> String {
        self.0.path().into()
    }
    fn generation(&self) -> u64 {
        self.0.generation()
    }
    fn stop(&self) {
        self.0.stop();
    }
    fn wait_stopped(&self, timeout: Duration) -> Result<(), String> {
        self.0.wait_stopped(timeout)
    }
}
type Encode<M> = Box<dyn Fn(BoxedMessage) -> Result<M, String> + Send + Sync>;
type Decode<R> = Box<dyn Fn(R) -> BoxedMessage + Send + Sync>;
/// Dynamic conversions occur at this explicit edge, sharing the same actor
/// identity, capacity, deadline and retirement as its typed address.
pub struct TypedToDyn<M, R> {
    reference: TypedActorRef<M, R>,
    encode: Encode<M>,
    decode: Decode<R>,
}
impl<M: Send + 'static, R: Send + 'static> DynActorRef for TypedToDyn<M, R> {
    fn send_dyn_with_timeout(&self, msg: BoxedMessage, timeout: Duration) -> BoxedResult {
        let end = match deadline(timeout) {
            Ok(d) => d,
            Err(e) => {
                safe_drop(msg);
                return Err(e);
            }
        };
        let message =
            catch_unwind(AssertUnwindSafe(|| (self.encode)(msg))).map_err(panic_error)??;
        let reply = self.reference.ask_until(message, end)?;
        if Instant::now() >= end {
            safe_drop(reply);
            return Err("bridge deadline expired".into());
        }
        let value = catch_unwind(AssertUnwindSafe(|| (self.decode)(reply))).map_err(panic_error)?;
        if Instant::now() >= end {
            safe_drop(value);
            return Err("bridge deadline expired".into());
        }
        Ok(value)
    }
    fn path(&self) -> String {
        self.reference.path().into()
    }
    fn generation(&self) -> u64 {
        self.reference.generation()
    }
    fn stop(&self) {
        self.reference.stop();
    }
    fn wait_stopped(&self, timeout: Duration) -> Result<(), String> {
        self.reference.wait_stopped(timeout)
    }
}
impl<M: Send + 'static, R: Send + 'static> TypedActorRef<M, R> {
    pub fn into_dyn(
        self,
        encode: impl Fn(BoxedMessage) -> Result<M, String> + Send + Sync + 'static,
        decode: impl Fn(R) -> BoxedMessage + Send + Sync + 'static,
    ) -> Arc<dyn DynActorRef> {
        Arc::new(TypedToDyn {
            reference: self,
            encode: Box::new(encode),
            decode: Box::new(decode),
        })
    }
}
