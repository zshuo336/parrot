//! Direct ThreadActor ownership, independent of the managed processor wrapper.
use futures::FutureExt;
use parrot::thread::{
    ThreadActor, ThreadContext,
    envelope::{AskEnvelope, ControlMessage},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    io::Read,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default, Debug)]
struct Counts {
    calls: AtomicUsize,
    stops: AtomicUsize,
    drops: [AtomicUsize; 2],
    payloads: [AtomicUsize; 2],
    pointer: AtomicUsize,
    diagnostics: AtomicUsize,
}
#[derive(Debug)]
struct Value(Arc<Counts>, usize, u8);
struct Recursive(Arc<Counts>, usize);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads[self.1].fetch_add(1, Ordering::SeqCst);
        panic!("recursive direct input payload");
    }
}
impl Drop for Value {
    fn drop(&mut self) {
        self.0.drops[self.1].fetch_add(1, Ordering::SeqCst);
        match self.2 {
            0 => {}
            1 => panic!("direct value {} disposal", self.1),
            2 => std::panic::panic_any(Recursive(self.0.clone(), self.1)),
            _ => unreachable!(),
        }
    }
}
#[derive(Debug)]
struct Business(Value);
impl std::fmt::Display for Business {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.0.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("direct dispatcher must not format business errors");
    }
}
impl std::error::Error for Business {}
#[derive(Clone, Copy, Debug)]
enum Mode {
    Success,
    Error,
    Timeout,
    Pending,
}
#[derive(Debug)]
struct Probe {
    counts: Arc<Counts>,
    mode: Mode,
    output_panic: u8,
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.counts.calls.fetch_add(1, Ordering::SeqCst);
        // The post-failure admission probe must finish even in Pending mode.
        if message.is::<()>() {
            return Box::pin(async { Ok(message) });
        }
        Box::pin(async move {
            if matches!(self.mode, Mode::Pending) {
                std::future::pending::<()>().await;
            }
            drop(message);
            match self.mode {
                Mode::Success => {
                    let reply = Box::new(Value(self.counts.clone(), 1, self.output_panic));
                    self.counts
                        .pointer
                        .store(reply.as_ref() as *const Value as usize, Ordering::SeqCst);
                    Ok(reply as BoxedMessage)
                }
                Mode::Error => {
                    let error = anyhow::Error::new(Business(Value(self.counts.clone(), 1, 0)));
                    self.counts.pointer.store(
                        error.downcast_ref::<Business>().unwrap() as *const Business as usize,
                        Ordering::SeqCst,
                    );
                    Err(ActorError::Other(error))
                }
                Mode::Timeout => Err(ActorError::Timeout),
                Mode::Pending => unreachable!(),
            }
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.counts.stops.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}
fn escaped(payload: Box<dyn std::any::Any + Send>) {
    if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(secondary);
    }
}
async fn run_future<F: Future + Send>(future: F) -> F::Output {
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => {
            escaped(payload);
            panic!("direct process_message let a panic escape");
        }
    }
}
fn cancel<F: Future + Send>(future: F) {
    if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(future))) {
        escaped(payload);
        panic!("direct process_message cancellation let a panic escape");
    }
}
async fn setup(
    mode: Mode,
    output_panic: u8,
    running: bool,
) -> (ThreadActor<Probe>, ThreadContext<Probe>, Arc<Counts>) {
    let counts = Arc::new(Counts::default());
    let mut actor = ThreadActor::new_for_test(Probe {
        counts: counts.clone(),
        mode,
        output_panic,
    });
    let mut context = ThreadContext::new_for_test("direct-message");
    if running {
        actor.initialize(&mut context).await.unwrap();
        run_future(actor.process_message(Box::new(ControlMessage::Start), &mut context))
            .await
            .unwrap();
    }
    (actor, context, counts)
}
fn business_pointer(error: &ActorError) -> usize {
    let ActorError::Other(error) = error else {
        panic!("business error category changed");
    };
    error
        .chain()
        .find_map(|source| source.downcast_ref::<Business>())
        .expect("original business error missing") as *const Business as usize
}
async fn failed_instance(
    actor: &mut ThreadActor<Probe>,
    context: &mut ThreadContext<Probe>,
    counts: &Arc<Counts>,
) {
    let calls = counts.calls.load(Ordering::SeqCst);
    let rejected = run_future(actor.process_message(Box::new(()), context)).await;
    let Err(ActorError::Panic(first_message)) = rejected else {
        panic!("failed instance accepted another message or lost failure class");
    };
    assert_eq!(counts.calls.load(Ordering::SeqCst), calls);
    let later = Arc::new(Counts::default());
    let rejected =
        run_future(actor.process_message(Box::new(Value(later.clone(), 0, 1)), context)).await;
    assert!(
        matches!(rejected, Err(ActorError::Panic(ref message)) if message == &first_message),
        "later input disposal replaced the first failure"
    );
    assert_eq!(later.drops[0].load(Ordering::SeqCst), 1);
    assert_eq!(counts.calls.load(Ordering::SeqCst), calls);
    let start = run_future(actor.process_message(Box::new(ControlMessage::Start), context)).await;
    assert!(
        matches!(start, Err(ActorError::Panic(ref message)) if message == &first_message),
        "Start ignored the prior cleanup failure"
    );
    let first = actor.shutdown(context).await;
    assert!(
        matches!(first, Err(ActorError::Panic(ref message)) if message == &first_message),
        "stop lost cleanup failure"
    );
    let second = actor.shutdown(context).await;
    assert!(
        matches!(second, Err(ActorError::Panic(ref message)) if message == &first_message),
        "repeat stop lost cleanup failure"
    );
    assert_eq!(counts.stops.load(Ordering::SeqCst), 1);
}

#[derive(Clone, Copy)]
enum Delivery {
    Live,
    Closed,
    Missing,
}
async fn ask(mode: Mode, delivery: Delivery, output_panic: u8) {
    let (mut actor, mut context, counts) = setup(mode, output_panic, true).await;
    let (mut envelope, mut receiver) = AskEnvelope::new_typed(Value(counts.clone(), 0, 0));
    if matches!(delivery, Delivery::Closed) {
        receiver.close();
    }
    if matches!(delivery, Delivery::Missing) {
        drop(envelope.take_reply());
    }
    let result = run_future(actor.process_message(Box::new(envelope), &mut context)).await;
    assert_eq!(counts.calls.load(Ordering::SeqCst), 1);
    assert_eq!(counts.drops[0].load(Ordering::SeqCst), 1);
    match mode {
        Mode::Success if output_panic != 0 => {
            assert!(
                matches!(result, Err(ActorError::Panic(_))),
                "cleanup failure hidden"
            );
        }
        Mode::Success => {
            assert!(
                result
                    .as_ref()
                    .expect("healthy cancelled receiver is not a business failure")
                    .is::<()>(),
                "unit acknowledgement"
            );
        }
        Mode::Error => assert_eq!(
            business_pointer(result.as_ref().expect_err("business failure")),
            counts.pointer.load(Ordering::SeqCst)
        ),
        Mode::Timeout => assert!(matches!(result, Err(ActorError::Timeout))),
        Mode::Pending => unreachable!(),
    }
    if matches!(delivery, Delivery::Live) {
        let reply = receiver.try_recv().expect("live reply missing");
        match mode {
            Mode::Success => {
                let value = reply.unwrap().downcast::<Value>().unwrap();
                assert_eq!(
                    value.as_ref() as *const Value as usize,
                    counts.pointer.load(Ordering::SeqCst)
                );
                drop(value);
            }
            Mode::Error => assert_eq!(
                business_pointer(reply.as_ref().expect_err("business reply failure")),
                counts.pointer.load(Ordering::SeqCst)
            ),
            Mode::Timeout => assert!(matches!(reply, Err(ActorError::Timeout))),
            Mode::Pending => unreachable!(),
        }
    }
    drop(result);
    assert_eq!(counts.diagnostics.load(Ordering::SeqCst), 0);
    if !matches!(mode, Mode::Error | Mode::Timeout) {
        assert_eq!(counts.drops[1].load(Ordering::SeqCst), 1);
    }
    assert_eq!(
        counts.payloads[1].load(Ordering::SeqCst),
        usize::from(output_panic == 2)
    );
    if output_panic != 0 {
        failed_instance(&mut actor, &mut context, &counts).await;
    } else {
        actor.shutdown(&mut context).await.unwrap();
    }
}

async fn input(running: bool, unpolled: bool, envelope: bool, panic: u8, pending: bool) {
    let (mut actor, mut context, counts) = setup(
        if pending {
            Mode::Pending
        } else {
            Mode::Success
        },
        0,
        running,
    )
    .await;
    let request = Value(counts.clone(), 0, panic);
    let message: BoxedMessage = if envelope {
        Box::new(AskEnvelope::new_typed(request).0)
    } else {
        Box::new(request)
    };
    let future = actor.process_message(message, &mut context);
    if unpolled {
        cancel(future);
    } else if pending {
        let mut future = Box::pin(future);
        assert!(futures::poll!(&mut future).is_pending());
        cancel(future);
    } else {
        let result = run_future(future).await;
        assert!(result.is_err(), "unstarted actor accepted input");
        if panic != 0 {
            assert!(
                matches!(result, Err(ActorError::Panic(_))),
                "rejection lost cleanup failure"
            );
        }
    }
    assert_eq!(counts.calls.load(Ordering::SeqCst), usize::from(pending));
    assert_eq!(counts.drops[0].load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.payloads[0].load(Ordering::SeqCst),
        usize::from(panic == 2)
    );
    if panic != 0 && running {
        failed_instance(&mut actor, &mut context, &counts).await;
    } else if panic != 0 {
        assert!(
            actor.initialize(&mut context).await.is_err(),
            "failed input allowed initialization"
        );
        assert!(
            run_future(actor.process_message(Box::new(ControlMessage::Start), &mut context))
                .await
                .is_err()
        );
        for _ in 0..2 {
            assert!(matches!(
                actor.shutdown(&mut context).await,
                Err(ActorError::Panic(_))
            ));
        }
        assert_eq!(counts.stops.load(Ordering::SeqCst), 0);
    } else if running && !pending {
        run_future(actor.process_message(Box::new(()), &mut context))
            .await
            .unwrap();
        assert_eq!(counts.calls.load(Ordering::SeqCst), 1);
        actor.shutdown(&mut context).await.unwrap();
    } else if running {
        actor.shutdown(&mut context).await.unwrap();
        assert_eq!(counts.stops.load(Ordering::SeqCst), 1);
    }
}

fn isolated(name: &str) -> bool {
    const KEY: &str = "PARROT_DIRECT_MESSAGE_CHILD";
    if std::env::var(KEY).as_deref() == Ok(name) {
        return false;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(KEY, name)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            child.kill().unwrap();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let out = out.join().unwrap();
    let err = err.join().unwrap();
    assert!(
        status.success() && !timed_out,
        "status={status}; timed_out={timed_out}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    true
}
macro_rules! case {
    ($name:ident, $body:expr) => {
        #[test]
        fn $name() {
            if isolated(stringify!($name)) {
                return;
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on($body);
        }
    };
}
case!(success_live, ask(Mode::Success, Delivery::Live, 0));
case!(success_closed, ask(Mode::Success, Delivery::Closed, 0));
case!(success_missing, ask(Mode::Success, Delivery::Missing, 0));
case!(error_live, ask(Mode::Error, Delivery::Live, 0));
case!(error_closed, ask(Mode::Error, Delivery::Closed, 0));
case!(error_missing, ask(Mode::Error, Delivery::Missing, 0));
case!(timeout_live, ask(Mode::Timeout, Delivery::Live, 0));
case!(timeout_closed, ask(Mode::Timeout, Delivery::Closed, 0));
case!(timeout_missing, ask(Mode::Timeout, Delivery::Missing, 0));
case!(
    closed_hostile_reply,
    ask(Mode::Success, Delivery::Closed, 1)
);
case!(
    closed_recursive_reply,
    ask(Mode::Success, Delivery::Closed, 2)
);
case!(
    missing_hostile_reply,
    ask(Mode::Success, Delivery::Missing, 1)
);
case!(
    missing_recursive_reply,
    ask(Mode::Success, Delivery::Missing, 2)
);
case!(unstarted_plain, input(false, false, false, 0, false));
case!(
    unstarted_hostile_plain,
    input(false, false, false, 1, false)
);
case!(
    unstarted_recursive_envelope,
    input(false, false, true, 2, false)
);
case!(unpolled_plain, input(true, true, false, 0, false));
case!(unpolled_envelope, input(true, true, true, 0, false));
case!(unpolled_hostile_plain, input(true, true, false, 1, false));
case!(unpolled_recursive_plain, input(true, true, false, 2, false));
case!(unpolled_hostile_envelope, input(true, true, true, 1, false));
case!(
    unpolled_recursive_envelope,
    input(true, true, true, 2, false)
);
case!(pending_plain, input(true, false, false, 0, true));
case!(pending_envelope, input(true, false, true, 0, true));
case!(pending_hostile_plain, input(true, false, false, 1, true));
case!(
    pending_recursive_envelope,
    input(true, false, true, 2, true)
);
