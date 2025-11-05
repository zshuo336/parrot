use parrot::thread::context::ThreadContext;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use remote_poc::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug)]
pub struct Probe;
#[derive(Debug)]
pub struct TellCount;
#[derive(Debug)]
pub struct Relay;
#[derive(Debug)]
pub struct RelayTell;
#[derive(Debug)]
pub struct Secret;

pub fn install() {
    install_poc_messages();
    macro_rules! register {
        ($ty:ident) => {
            CodecRegistry::install::<$ty>(
                concat!("bin:u:", stringify!($ty)),
                |m| {
                    if m.is::<$ty>() {
                        Ok(vec![])
                    } else {
                        Err("type mismatch".into())
                    }
                },
                |b| {
                    if b.is_empty() {
                        Ok(Box::new($ty))
                    } else {
                        Err("expected empty payload".into())
                    }
                },
            );
        };
    }
    register!(Probe);
    register!(TellCount);
    register!(Relay);
    register!(RelayTell);
    register!(Secret);
}

struct RustService {
    multiplier: u64,
    calls: Arc<Mutex<Vec<u64>>>,
}
impl Actor for RustService {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(Ping(n)) = m.downcast_ref::<Ping>() {
                self.calls.lock().unwrap().push(*n);
                Ok(Box::new(Pong(n * self.multiplier)) as BoxedMessage)
            } else {
                Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "unsupported".into(),
                ))
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

pub async fn ask(reference: &RemoteActorRef, msg: BoxedMessage) -> BoxedMessage {
    reference
        .send_with_timeout(msg, Some(Duration::from_secs(15)))
        .await
        .unwrap()
}

pub async fn exercise(ep: &RemoteEndpoint, path: &str, delta: u64, sum: u64, multiplier: u64) {
    let calls = Arc::new(Mutex::new(vec![]));
    ep.local
        .get_thread_system("main")
        .unwrap()
        .spawn_at(
            RustService {
                multiplier,
                calls: calls.clone(),
            },
            "/user/rust_service",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let remote = ep.remote_ref(path);
    assert_eq!(
        ask(&remote, Box::new(Ping(39)))
            .await
            .downcast_ref::<Pong>()
            .unwrap()
            .0,
        39 + delta
    );
    assert_eq!(
        *ask(&remote, Box::new(Add(20, 22)))
            .await
            .downcast_ref::<u64>()
            .unwrap(),
        sum
    );
    let error = remote
        .send_with_timeout(Box::new(Secret), Some(Duration::from_secs(15)))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown_service"), "{error}");
    // Both sides must execute: a gateway only completes Probe after consuming
    // the Rust actor's Pong, and the Rust actor records the actual invocation.
    assert_eq!(
        *ask(&remote, Box::new(Probe))
            .await
            .downcast_ref::<u64>()
            .unwrap(),
        100 * multiplier
    );
    assert_eq!(*calls.lock().unwrap(), vec![100]);
    assert_eq!(
        *ask(&remote, Box::new(TellCount))
            .await
            .downcast_ref::<u64>()
            .unwrap(),
        0
    );
    remote.deliver(Box::new(Ping(1))).await.unwrap();
    remote.deliver(Box::new(Add(2, 3))).await.unwrap();
    // Invalid TELL must not count as executed business work.
    remote.deliver(Box::new(Secret)).await.unwrap();
    assert_eq!(
        *ask(&remote, Box::new(TellCount))
            .await
            .downcast_ref::<u64>()
            .unwrap(),
        2
    );
}
