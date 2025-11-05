use futures::FutureExt;
use parrot::{
    mirror::{MirrorCopier, MirrorHandle, MirrorPolicy},
    system::ParrotActorSystem,
    thread::{config::ThreadActorConfig, context::ThreadContext, system::ThreadActorSystem},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    system::ActorSystem,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Value {
    value: u64,
    shadow: bool,
    shadow_drops: Arc<AtomicUsize>,
}
impl Drop for Value {
    fn drop(&mut self) {
        if self.shadow {
            self.shadow_drops.fetch_add(1, Ordering::SeqCst);
            panic!("shadow payload disposal failed");
        }
    }
}
struct Primary(Arc<AtomicUsize>);
impl Actor for Primary {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = message.downcast::<Value>().unwrap();
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(value.value) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
struct PanicPayload(Arc<AtomicUsize>);
impl Drop for PanicPayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("copier panic payload disposal failed");
    }
}

async fn exercise(mode: u8, dispatch: u8) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let engine = ThreadActorSystem::shared(Default::default());
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let disposals = Arc::new(AtomicUsize::new(0));
    engine
        .spawn_at(
            Primary(primary_calls.clone()),
            "/primary/value",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    facade
        .register_thread_system("primary".into(), engine, true)
        .await
        .unwrap();
    let owner: Arc<Mutex<Option<MirrorHandle>>> = Arc::new(Mutex::new(None));
    let copier_owner = owner.clone();
    let panic_drops = disposals.clone();
    let measure_drops = disposals.clone();
    let weak_facade = Arc::downgrade(&facade);
    let measure_capture = Capture {
        count: Arc::new(AtomicUsize::new(0)),
        panic: mode >= 3,
    };
    let copy_capture = Capture {
        count: Arc::new(AtomicUsize::new(0)),
        panic: mode >= 3,
    };
    let measure_count = measure_capture.count.clone();
    let copy_count = copy_capture.count.clone();
    facade
        .register_mirror_copier(MirrorCopier::new(
            move |_: &Value| {
                measure_capture.touch();
                if mode == 2 {
                    std::panic::panic_any(PanicPayload(measure_drops.clone()));
                }
                8
            },
            move |value: &Value| {
                copy_capture.touch();
                if mode >= 3 {
                    weak_facade
                        .upgrade()
                        .unwrap()
                        .register_mirror_copier(MirrorCopier::new(
                            |_: &Value| 8,
                            |_: &Value| Err("replacement copier".into()),
                        ))
                        .unwrap();
                }
                if mode == 0 || mode == 3 {
                    copier_owner.lock().unwrap().as_ref().unwrap().close();
                    Ok(Value {
                        value: value.value,
                        shadow: true,
                        shadow_drops: value.shadow_drops.clone(),
                    })
                } else {
                    std::panic::panic_any(PanicPayload(panic_drops.clone()));
                }
            },
        ))
        .unwrap();
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new("/primary/", "/shadow"))
        .unwrap();
    *owner.lock().unwrap() = Some(handle.clone());
    let actor = facade
        .get_actor(&ActorPath::placeholder("/primary/value"))
        .await
        .unwrap();
    let message = Box::new(Value {
        value: 42,
        shadow: false,
        shadow_drops: disposals.clone(),
    });
    let outcome = AssertUnwindSafe(async {
        match dispatch {
            0 => actor.send(message).await.map(Some),
            1 => actor
                .send_with_timeout(message, Some(std::time::Duration::from_secs(3)))
                .await
                .map(Some),
            2 => actor
                .send_with_options(message, Default::default())
                .await
                .map(Some),
            3 => actor.deliver(message).await.map(|()| None),
            4 => actor
                .deliver_with_options(message, Default::default())
                .await
                .map(|()| None),
            _ => unreachable!(),
        }
    })
    .catch_unwind()
    .await;
    let primary_succeeded = outcome.as_ref().is_ok_and(|result| result.is_ok());
    let answer = outcome
        .ok()
        .and_then(Result::ok)
        .flatten()
        .and_then(|message| message.downcast::<u64>().ok())
        .map(|value| *value);
    if primary_succeeded {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while primary_calls.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    owner.lock().unwrap().take();
    handle.shutdown().await;
    Arc::try_unwrap(facade)
        .ok()
        .expect("sole facade owner")
        .internal_shutdown()
        .await
        .unwrap();
    assert_eq!(handle.stats().worker_failed, 0);
    assert_eq!(
        handle.stats().disposal_failed,
        match mode {
            0 | 4 => 1,
            3 => 2,
            _ => 0,
        }
    );
    assert_eq!(
        handle.stats().copy_failed,
        u64::from(mode == 1 || mode == 2 || mode == 4)
    );
    assert_eq!(
        disposals.load(Ordering::SeqCst),
        1,
        "the owned shadow resource must be disposed exactly once"
    );
    assert_eq!(
        primary_calls.load(Ordering::SeqCst),
        1,
        "shadow cleanup prevented the primary actor from running"
    );
    assert!(
        primary_succeeded,
        "shadow cleanup replaced the primary result"
    );
    assert_eq!(answer, (dispatch < 3).then_some(42));
    assert_eq!(measure_count.load(Ordering::SeqCst), 1);
    assert_eq!(copy_count.load(Ordering::SeqCst), 1);
}

struct Capture {
    count: Arc<AtomicUsize>,
    panic: bool,
}
impl Capture {
    fn touch(&self) {
        assert_eq!(self.count.load(Ordering::SeqCst), 0);
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.panic {
            panic!("retired copier capture disposal failed");
        }
    }
}

macro_rules! contract {
    ($name:ident, $mode:expr, $dispatch:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            exercise($mode, $dispatch).await;
        }
    };
}
contract!(close_after_copy_disposal_cannot_cancel_primary_call, 0, 0);
contract!(close_after_copy_preserves_timeout_call, 0, 1);
contract!(close_after_copy_preserves_options_call, 0, 2);
contract!(close_after_copy_preserves_tell, 0, 3);
contract!(close_after_copy_preserves_options_tell, 0, 4);
contract!(
    copier_panic_payload_disposal_cannot_cancel_primary_call,
    1,
    0
);
contract!(copier_panic_payload_preserves_timeout_call, 1, 1);
contract!(copier_panic_payload_preserves_options_call, 1, 2);
contract!(copier_panic_payload_preserves_tell, 1, 3);
contract!(copier_panic_payload_preserves_options_tell, 1, 4);
contract!(measure_panic_payload_preserves_call, 2, 0);
contract!(measure_panic_payload_preserves_timeout_call, 2, 1);
contract!(measure_panic_payload_preserves_options_call, 2, 2);
contract!(measure_panic_payload_preserves_tell, 2, 3);
contract!(measure_panic_payload_preserves_options_tell, 2, 4);

// These independent destructor boundaries can abort a broken implementation.
// Keep that failure inside a bounded subprocess, including both original captures.
fn isolated(name: &str, mode: u8) {
    if std::env::var("PARROT_MIRROR_CAPTURE_CHILD").as_deref() == Ok(name) {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(exercise(mode, 0));
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_MIRROR_CAPTURE_CHILD", name)
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "child {name}: {status}");
            return;
        }
        if started.elapsed() > std::time::Duration::from_secs(30) {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("child {name} did not retire");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
#[test]
fn replaced_copier_captures_and_returned_copy_retire_independently() {
    isolated(
        "replaced_copier_captures_and_returned_copy_retire_independently",
        3,
    );
}
#[test]
fn replaced_copier_captures_and_panic_payload_retire_independently() {
    isolated(
        "replaced_copier_captures_and_panic_payload_retire_independently",
        4,
    );
}
