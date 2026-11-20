//! Queued owner-thread work must spend the original construction deadline.
use super::*;
use parrot_api::{
    address::ActorRefExt,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalTypedDispatch},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
};
use std::{rc::Rc, sync::Condvar};

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn wait(&self) {
        self.entered.notify_one();
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(10), |open| {
                !*open
            })
            .unwrap();
        assert!(*open, "test must release the real arbiter gate");
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[derive(Default)]
struct Stats {
    calls: AtomicUsize,
    initialized: AtomicUsize,
    captures_retired: AtomicUsize,
    actors_retired: AtomicUsize,
}
struct Capture(Arc<Stats>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.captures_retired.fetch_add(1, Ordering::SeqCst);
    }
}
struct LocalProbe {
    _local: Rc<()>,
    stats: Arc<Stats>,
}
impl Drop for LocalProbe {
    fn drop(&mut self) {
        self.stats.actors_retired.fetch_add(1, Ordering::SeqCst);
    }
}
impl LocalTypedDispatch for LocalProbe {
    type Msg = Read;
    type Reply = ();
    fn init<'a>(
        &'a mut self,
        _: &'a parrot_api::typed_context::TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.stats.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn dispatch<'a>(
        &'a mut self,
        _: Read,
        _: &'a parrot_api::typed_context::TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
struct StaticProbe(Arc<Stats>);
impl Drop for StaticProbe {
    fn drop(&mut self) {
        self.0.actors_retired.fetch_add(1, Ordering::SeqCst);
    }
}
impl ParrotTypedDispatch for StaticProbe {
    type Msg = Read;
    type Reply = ();
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.0.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&mut self, _: Read) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
struct Read;
impl Message for Read {
    type Result = ();
}
impl ParrotMsgVariant<LocalProtocol<LocalProbe>> for Read {
    fn inject(message: Self) -> Read {
        message
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
impl ParrotMsgVariant<StaticProbe> for Read {
    fn inject(message: Self) -> Read {
        message
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
#[derive(Clone, Copy)]
enum Source {
    LocalOnce,
    LocalFactory,
    StaticFactory,
}

async fn queued_construction(source: Source, expire: bool) {
    let system = Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: if expire {
                Duration::from_millis(200)
            } else {
                Duration::from_secs(5)
            },
            ask_timeout: Duration::from_secs(2),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let blocked = gate.clone();
    assert!(
        system
            .arbiter_pool()
            .reserve_arbiter()
            .spawn_fn(move || async move {
                blocked.wait();
            })
    );
    gate.entered.notified().await;
    let stats = Arc::new(Stats::default());
    let capture = Capture(stats.clone());
    let made = stats.clone();
    let started = tokio::time::Instant::now();
    let reference = match source {
        Source::LocalOnce | Source::LocalFactory => {
            let factory = move || {
                std::hint::black_box(&capture);
                made.calls.fetch_add(1, Ordering::SeqCst);
                LocalProbe {
                    _local: Rc::new(()),
                    stats: made.clone(),
                }
            };
            match source {
                Source::LocalOnce => {
                    system
                        .spawn_local_typed::<_, Read, _>(factory, "/queued")
                        .await
                }
                _ => {
                    system
                        .spawn_local_typed_factory::<_, Read, _>(factory, "/queued")
                        .await
                }
            }
            .unwrap()
            .into_dyn()
        }
        Source::StaticFactory => system
            .spawn_typed_factory::<_, Read, _>(
                move || {
                    std::hint::black_box(&capture);
                    made.calls.fetch_add(1, Ordering::SeqCst);
                    StaticProbe(made.clone())
                },
                "/queued",
            )
            .await
            .unwrap()
            .into_dyn(),
    };
    let admitted = tokio::time::Instant::now();
    let control = system.registry.read().unwrap().actors["/queued"]
        .control
        .clone();
    let original_deadline = control.cleanup_deadline();
    let published_completion = control.completion.clone();
    if expire {
        // The worker cannot execute before this actual elapsed deadline.
        tokio::time::sleep_until(admitted + control.creation_timeout).await;
    }
    drop(release);
    let delivered = reference.ask(Read).await.is_ok();
    let stopped = reference.stop().await;
    let _ = await_completion(published_completion).await;
    tokio::time::timeout(Duration::from_secs(3), control.lifetime.wait())
        .await
        .unwrap()
        .unwrap();
    system.as_ref().clone().shutdown().await.unwrap();

    // Assertions run only after the worker, captures and instance have retired.
    assert_eq!(stats.captures_retired.load(Ordering::SeqCst), 1);
    assert_eq!(
        delivered, !expire,
        "expired queued constructor handled a real ASK"
    );
    assert_eq!(stats.calls.load(Ordering::SeqCst), usize::from(!expire));
    assert_eq!(
        stats.initialized.load(Ordering::SeqCst),
        usize::from(!expire)
    );
    assert_eq!(
        stats.actors_retired.load(Ordering::SeqCst),
        usize::from(!expire)
    );
    if !expire {
        stopped.unwrap();
    } else {
        assert!(
            original_deadline.is_some_and(|deadline| {
                deadline >= started && deadline <= admitted + control.creation_timeout
            }),
            "queued work did not inherit an admission-time construction deadline"
        );
    }
}

#[actix::test]
async fn expired_queued_local_one_shot_never_calls_constructor() {
    queued_construction(Source::LocalOnce, true).await;
}
#[actix::test]
async fn expired_queued_local_factory_never_calls_constructor() {
    queued_construction(Source::LocalFactory, true).await;
}
#[actix::test]
async fn expired_queued_static_factory_never_calls_constructor() {
    queued_construction(Source::StaticFactory, true).await;
}
#[actix::test]
async fn timely_queued_local_one_shot_constructs_and_serves() {
    queued_construction(Source::LocalOnce, false).await;
}
#[actix::test]
async fn timely_queued_local_factory_constructs_and_serves() {
    queued_construction(Source::LocalFactory, false).await;
}
#[actix::test]
async fn timely_queued_static_factory_constructs_and_serves() {
    queued_construction(Source::StaticFactory, false).await;
}
