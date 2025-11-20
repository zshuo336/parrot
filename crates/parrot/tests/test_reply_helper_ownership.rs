//! Public reply helpers own each unpublished value, including before first poll.
use futures::FutureExt;
use parrot::thread::{
    envelope::AskEnvelope,
    reply::{ReplyChannel, ThreadReplyChannel},
};
use parrot_api::{
    errors::ActorError,
    types::{ActorResult, BoxedMessage},
};
use std::{
    future::Future,
    io::Read,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Counts {
    drops: [AtomicUsize; 2],
    payloads: [AtomicUsize; 2],
    diagnostics: AtomicUsize,
}
struct Recursive(Arc<Counts>, usize);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads[self.1].fetch_add(1, Ordering::SeqCst);
        panic!("recursive reply panic payload");
    }
}
#[derive(Debug)]
struct Value {
    counts: Arc<Counts>,
    index: usize,
    panic: u8,
}
impl Drop for Value {
    fn drop(&mut self) {
        self.counts.drops[self.index].fetch_add(1, Ordering::SeqCst);
        match self.panic {
            0 => {}
            1 => panic!("reply value {} disposal", self.index),
            2 => std::panic::panic_any(Recursive(self.counts.clone(), self.index)),
            _ => unreachable!(),
        }
    }
}
#[derive(Debug)]
struct BusinessError(Value);
impl std::fmt::Display for BusinessError {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.counts.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("reply helper must not format a business error");
    }
}
impl std::error::Error for BusinessError {}

#[derive(Clone, Copy)]
enum Entry {
    Success,
    Error,
    SyncSuccess,
    SyncError,
    ChannelSuccess,
    ChannelError,
}
impl Entry {
    fn channel(self) -> bool {
        matches!(self, Self::ChannelSuccess | Self::ChannelError)
    }
    fn error(self) -> bool {
        matches!(self, Self::Error | Self::SyncError | Self::ChannelError)
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Live,
    Closed,
    Missing,
    Unpolled,
}

// Report an escaped recursive payload only after disposing it independently.
// Assertion reporting itself must not double-panic over a hostile payload.
fn escaped(result: std::thread::Result<()>) -> bool {
    match result {
        Ok(()) => false,
        Err(payload) => {
            if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                std::mem::forget(secondary);
            }
            true
        }
    }
}
async fn execute<F: Future + Send>(future: F, cancel: bool) -> (bool, Option<F::Output>) {
    if cancel {
        (
            escaped(std::panic::catch_unwind(AssertUnwindSafe(|| drop(future)))),
            None,
        )
    } else {
        match AssertUnwindSafe(future).catch_unwind().await {
            Ok(result) => (false, Some(result)),
            Err(payload) => (escaped(Err(payload)), None),
        }
    }
}
fn value(counts: &Arc<Counts>, index: usize, panic: u8) -> Value {
    Value {
        counts: counts.clone(),
        index,
        panic,
    }
}
fn make_result(counts: &Arc<Counts>, error: bool, panic: u8) -> (ActorResult<BoxedMessage>, usize) {
    if error {
        let error = anyhow::Error::new(BusinessError(value(counts, 1, panic)));
        let pointer =
            error.downcast_ref::<BusinessError>().unwrap() as *const BusinessError as usize;
        (Err(ActorError::Other(error)), pointer)
    } else {
        let reply = Box::new(value(counts, 1, panic));
        let pointer = reply.as_ref() as *const Value as usize;
        (Ok(reply), pointer)
    }
}
fn not_sent(receiver: &mut parrot::thread::reply::ReplyReceiver) {
    assert!(
        matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ),
        "reply was published before its future was polled"
    );
}
async fn run(entry: Entry, delivery: Delivery, input_panic: u8, output_panic: u8) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = LogWriter(logs.clone());
    let _subscriber = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish(),
    );
    let counts = Arc::new(Counts::default());
    let (result, pointer) = make_result(&counts, entry.error(), output_panic);
    let mut transport = None;
    let mut receiver;
    let did_escape;
    if entry.channel() {
        let (sender, rx) = oneshot::channel();
        receiver = Some(parrot::thread::reply::ReplyReceiver::new(rx));
        if delivery == Delivery::Closed {
            drop(receiver.take());
        }
        let channel: Box<dyn ReplyChannel> = Box::new(ThreadReplyChannel(sender));
        let future = channel.send_reply(result);
        if let Some(rx) = &mut receiver {
            not_sent(rx);
        }
        assert_eq!(
            counts.drops[1].load(Ordering::SeqCst),
            0,
            "eager reply disposal"
        );
        let (escaped, returned) = execute(future, delivery == Delivery::Unpolled).await;
        did_escape = escaped;
        transport = returned;
    } else {
        let (mut envelope, rx) = AskEnvelope::new_typed(value(&counts, 0, input_panic));
        receiver = Some(rx);
        if delivery == Delivery::Closed {
            drop(receiver.take());
        }
        if delivery == Delivery::Missing {
            drop(envelope.take_reply());
        }
        did_escape = match entry {
            Entry::Success => {
                let future = envelope.reply_success(result.unwrap());
                if matches!(delivery, Delivery::Live | Delivery::Unpolled) {
                    not_sent(receiver.as_mut().unwrap());
                }
                assert_eq!(
                    counts.drops[0].load(Ordering::SeqCst),
                    0,
                    "eager input disposal"
                );
                assert_eq!(
                    counts.drops[1].load(Ordering::SeqCst),
                    0,
                    "eager reply disposal"
                );
                execute(future, delivery == Delivery::Unpolled).await.0
            }
            Entry::Error => {
                let future = envelope.reply_error(result.unwrap_err());
                if matches!(delivery, Delivery::Live | Delivery::Unpolled) {
                    not_sent(receiver.as_mut().unwrap());
                }
                assert_eq!(
                    counts.drops[0].load(Ordering::SeqCst),
                    0,
                    "eager input disposal"
                );
                assert_eq!(
                    counts.drops[1].load(Ordering::SeqCst),
                    0,
                    "eager reply disposal"
                );
                execute(future, delivery == Delivery::Unpolled).await.0
            }
            Entry::SyncSuccess | Entry::SyncError => {
                escaped(std::panic::catch_unwind(AssertUnwindSafe(|| {
                    envelope.reply_sync(result)
                })))
            }
            _ => unreachable!(),
        };
    }
    if delivery == Delivery::Live {
        assert_eq!(
            counts.drops[1].load(Ordering::SeqCst),
            0,
            "delivered reply was destroyed"
        );
        let received = receiver
            .as_mut()
            .unwrap()
            .try_recv()
            .expect("reply must arrive");
        let received_pointer = if entry.error() {
            match &received {
                Err(ActorError::Other(error)) => error
                    .downcast_ref::<BusinessError>()
                    .expect("business error concrete type")
                    as *const BusinessError
                    as usize,
                _ => panic!("business error variant was changed"),
            }
        } else {
            received
                .as_ref()
                .expect("successful result")
                .downcast_ref::<Value>()
                .expect("reply concrete type") as *const Value as usize
        };
        // Drop the receiver-owned value before any identity assertion can unwind.
        let consumer_panic = escaped(std::panic::catch_unwind(AssertUnwindSafe(|| {
            drop(received)
        })));
        assert_eq!(consumer_panic, output_panic != 0);
        assert_eq!(
            received_pointer, pointer,
            "original reply allocation changed"
        );
    } else if let Some(rx) = &mut receiver {
        assert!(
            matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Closed)),
            "cancelled/absent sender must not deliver"
        );
    }
    assert_eq!(
        counts.drops[0].load(Ordering::SeqCst),
        usize::from(!entry.channel()),
        "request not disposed exactly once"
    );
    assert_eq!(
        counts.drops[1].load(Ordering::SeqCst),
        1,
        "reply not disposed exactly once"
    );
    assert_eq!(
        counts.payloads[0].load(Ordering::SeqCst),
        usize::from(input_panic == 2)
    );
    assert_eq!(
        counts.payloads[1].load(Ordering::SeqCst),
        usize::from(output_panic == 2)
    );
    assert_eq!(
        counts.diagnostics.load(Ordering::SeqCst),
        0,
        "user diagnostic code ran"
    );
    if entry.channel() && delivery != Delivery::Unpolled && !did_escape {
        match (delivery, transport.unwrap()) {
            (Delivery::Live, Ok(())) => {}
            (Delivery::Closed, Err(ActorError::ReplyChannelError(message))) => {
                assert_eq!(
                    message.contains("disposal failed"),
                    output_panic != 0,
                    "lost reply cleanup failure diagnostic"
                );
                assert_eq!(
                    message.contains("panic payload destructor panicked"),
                    output_panic == 2,
                    "lost recursive cleanup diagnostic"
                );
            }
            _ => panic!("reply transport result category changed"),
        }
    }
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let expected_logs = if entry.channel() {
        usize::from(delivery == Delivery::Unpolled && output_panic != 0)
    } else {
        usize::from(input_panic != 0) + usize::from(delivery != Delivery::Live && output_panic != 0)
    };
    assert_eq!(
        logs.matches("reply helper disposal failed").count(),
        expected_logs,
        "cleanup failures must each be reported exactly once: {logs}"
    );
    assert!(
        !did_escape,
        "reply helper let a value disposal panic escape"
    );
}

fn isolated(name: &str) -> bool {
    const KEY: &str = "PARROT_REPLY_HELPER_CHILD";
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
    ($name:ident, $entry:ident, $delivery:ident, $input:expr, $output:expr) => {
        #[test]
        fn $name() {
            if isolated(stringify!($name)) {
                return;
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run(Entry::$entry, Delivery::$delivery, $input, $output));
        }
    };
}
case!(success_live, Success, Live, 0, 0);
case!(error_live, Error, Live, 0, 0);
case!(sync_success_live, SyncSuccess, Live, 0, 0);
case!(sync_error_live, SyncError, Live, 0, 0);
case!(channel_success_live, ChannelSuccess, Live, 0, 0);
case!(channel_error_live, ChannelError, Live, 0, 0);
case!(success_closed, Success, Closed, 0, 0);
case!(error_closed, Error, Closed, 0, 0);
case!(sync_success_closed, SyncSuccess, Closed, 0, 0);
case!(sync_error_closed, SyncError, Closed, 0, 0);
case!(channel_success_closed, ChannelSuccess, Closed, 0, 0);
case!(channel_error_closed, ChannelError, Closed, 0, 0);
case!(error_missing, Error, Missing, 0, 0);
case!(sync_success_missing, SyncSuccess, Missing, 0, 0);
case!(success_unpolled, Success, Unpolled, 0, 0);
case!(error_unpolled, Error, Unpolled, 0, 0);
case!(channel_success_unpolled, ChannelSuccess, Unpolled, 0, 0);
case!(channel_error_unpolled, ChannelError, Unpolled, 0, 0);
case!(success_live_hostile_input, Success, Live, 1, 0);
case!(error_live_hostile_input, Error, Live, 2, 0);
case!(sync_success_live_hostile_input, SyncSuccess, Live, 2, 0);
case!(sync_error_live_hostile_input, SyncError, Live, 1, 0);
case!(success_closed_double, Success, Closed, 1, 1);
case!(error_closed_double, Error, Closed, 2, 1);
case!(sync_success_closed_double, SyncSuccess, Closed, 1, 2);
case!(sync_error_closed_double, SyncError, Closed, 2, 2);
case!(success_missing_double, Success, Missing, 2, 2);
case!(sync_error_missing_double, SyncError, Missing, 1, 1);
case!(success_unpolled_double, Success, Unpolled, 1, 2);
case!(error_unpolled_double, Error, Unpolled, 2, 1);
case!(channel_success_closed_hostile, ChannelSuccess, Closed, 0, 1);
case!(channel_error_closed_hostile, ChannelError, Closed, 0, 2);
case!(
    channel_success_unpolled_hostile,
    ChannelSuccess,
    Unpolled,
    0,
    2
);
case!(channel_error_unpolled_hostile, ChannelError, Unpolled, 0, 1);
