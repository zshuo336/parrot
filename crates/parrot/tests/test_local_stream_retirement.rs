//! Real owner-thread actor retirement must survive independently failing streams.
use futures::Stream;
use parrot_api::{
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalStreamContext, LocalTypedDispatch},
    message::Message,
    trace::{self, TraceContext},
    typed::ParrotMsgVariant,
    typed_context::TypedContext,
    types::ActorResult,
};
use std::{
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
#[derive(Default)]
struct Stats {
    drops: Mutex<Vec<(usize, Option<TraceContext>)>>,
    actors: AtomicUsize,
}
struct Payload;
impl Drop for Payload {
    fn drop(&mut self) {
        panic!("payload destructor");
    }
}
struct Source {
    stats: Arc<Stats>,
    id: usize,
    hostile: bool,
    poll_hostile: bool,
    _local: Rc<()>,
}
impl Stream for Source {
    type Item = ();
    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<()>> {
        if self.poll_hostile {
            std::panic::panic_any(Payload);
        }
        Poll::Pending
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.stats
            .drops
            .lock()
            .unwrap()
            .push((self.id, TraceContext::current()));
        if self.id == 0 && self.hostile {
            std::panic::panic_any(Payload);
        }
        assert!(self.id == 2, "source destructor {}", self.id);
    }
}
struct Actor {
    stats: Arc<Stats>,
    parent: TraceContext,
    init_error: bool,
    hostile: bool,
    poll_hostile: bool,
    hook: flume::Sender<String>,
}
impl Drop for Actor {
    fn drop(&mut self) {
        self.stats.actors.fetch_add(1, Ordering::SeqCst);
        let _ = self.hook.send("actor dropped".into());
    }
}
struct Ping;
impl Message for Ping {
    type Result = ();
}
impl ParrotMsgVariant<LocalProtocol<Actor>> for Ping {
    fn inject(msg: Self) -> Ping {
        msg
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
impl LocalTypedDispatch for Actor {
    type Msg = Ping;
    type Reply = ();
    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        streams: &'a mut LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        for id in 0..3 {
            trace::in_scope((id != 1).then(|| self.parent.clone()), || {
                streams
                    .add_stream(Source {
                        stats: self.stats.clone(),
                        id,
                        hostile: self.hostile,
                        poll_hostile: self.poll_hostile && id == 0,
                        _local: Rc::new(()),
                    })
                    .unwrap()
            });
        }
        let _ = self.hook.send("registered".into());
        let fail = self.init_error;
        Box::pin(async move {
            if fail {
                Err(ActorError::InternalError("original init failure".into()))
            } else {
                Ok(())
            }
        })
    }
    fn dispatch<'a>(
        &'a mut self,
        _: Ping,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn local_stream_error<'a>(
        &'a mut self,
        _: u64,
        error: ActorError,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        let _ = self.hook.send(error.to_string());
        Box::pin(async { Ok(()) })
    }
}
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(8), future)
        .await
        .expect("actor cleanup deadline")
}
fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_LOCAL_ENGINE_RETIREMENT_CASE")
        .ok()
        .as_deref()
        == Some(name)
    {
        body();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_LOCAL_ENGINE_RETIREMENT_CASE", name)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "actor child {name}: {status}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("actor child deadline: {name}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn run(init_error: bool, hostile: bool, poll_hostile: bool) {
    actix::System::new().block_on(async {
        let stats = Arc::new(Stats::default());
        let parent = TraceContext::root();
        let owned_stats = stats.clone();
        let owned_parent = parent.clone();
        let (sender, receiver) = flume::unbounded();
        let system = Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        );
        let actor = bounded(system.spawn_local_typed::<Actor, Ping, _>(
            move || Actor {
                stats: owned_stats.clone(),
                parent: owned_parent.clone(),
                init_error,
                hostile,
                poll_hostile,
                hook: sender.clone(),
            },
            "/local/retirement",
        ))
        .await;
        let actor = actor.unwrap(); // Admission precedes asynchronous initialization.
        assert_eq!(bounded(receiver.recv_async()).await.unwrap(), "registered");
        if init_error {
            assert_eq!(
                bounded(receiver.recv_async()).await.unwrap(),
                "actor dropped"
            );
        } else if poll_hostile {
            let hook = bounded(receiver.recv_async()).await.unwrap();
            assert!(hook.contains("panic payload destructor panicked"), "{hook}");
            assert_eq!(
                bounded(receiver.recv_async()).await.unwrap(),
                "actor dropped"
            );
        } else {
            // A real mailbox reply proves stream initialization completed.
            bounded(actor.ask(Ping)).await.unwrap();
        }
        let first = bounded(actor.stop())
            .await
            .expect_err("source destruction must fail stop");
        assert!(matches!(&first, ActorError::Panic(_)), "{first:?}");
        let repeated = bounded(actor.stop()).await.unwrap_err();
        assert_eq!(first.to_string(), repeated.to_string());
        let error = first.to_string();
        let _ = bounded(system.as_ref().clone().shutdown()).await;
        if hostile {
            assert!(
                error.contains("panic payload destructor panicked"),
                "{error}"
            );
        } else {
            assert!(error.contains("source destructor 0"), "{error}");
        }
        let mut drops = stats.drops.lock().unwrap().clone();
        drops.sort_by_key(|(id, _)| *id);
        assert_eq!(
            drops,
            vec![(0, Some(parent.clone())), (1, None), (2, Some(parent))]
        );
        assert_eq!(stats.actors.load(Ordering::SeqCst), 1);
    });
}
#[test]
fn actor_stop_reports_first_error_after_all_streams_retire() {
    isolated(
        "actor_stop_reports_first_error_after_all_streams_retire",
        || run(false, false, false),
    );
}
#[test]
fn actor_failed_initialization_retires_all_sources() {
    isolated("actor_failed_initialization_retires_all_sources", || {
        run(true, true, false)
    });
}
#[test]
fn actor_stop_sanitizes_hostile_source_payload() {
    isolated("actor_stop_sanitizes_hostile_source_payload", || {
        run(false, true, false)
    });
}
#[test]
fn actor_poll_error_hook_survives_hostile_payload() {
    isolated("actor_poll_error_hook_survives_hostile_payload", || {
        run(false, false, true)
    });
}
