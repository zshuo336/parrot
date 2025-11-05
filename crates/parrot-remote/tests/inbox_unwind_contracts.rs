use bytes::Bytes;
use futures::FutureExt;
use parrot_remote::durable::{DurableEnvelope, InboxError, InboxLimits, TransactionalInbox};
use std::{
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Healthy,
    Payload(u8),
    CloseDrop(u8),
    Unpolled,
    Invalid,
    NoRuntime,
    StorageFailure,
    OwnerDrop,
    CancelClose,
}
#[derive(Default)]
struct State {
    calls: AtomicUsize,
    drops: AtomicUsize,
    payloads: AtomicUsize,
    retiring: AtomicUsize,
    released: Mutex<bool>,
    wake: Condvar,
}
struct Release(Arc<State>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}
fn count(n: &AtomicUsize) -> usize {
    n.load(Ordering::SeqCst)
}
struct Payload {
    depth: u8,
    state: Arc<State>,
}
impl Drop for Payload {
    fn drop(&mut self) {
        self.state.payloads.fetch_add(1, Ordering::SeqCst);
        if self.depth == 0 {
            panic!("payload destructor");
        }
        std::panic::panic_any(Payload {
            depth: self.depth - 1,
            state: self.state.clone(),
        });
    }
}
struct CallbackOwner {
    case: Case,
    state: Arc<State>,
}
impl CallbackOwner {
    fn call(&self, _: &[u8], message: &DurableEnvelope) -> Result<Bytes, String> {
        let calls = self.state.calls.fetch_add(1, Ordering::SeqCst);
        if let Case::Payload(depth) = self.case {
            if calls == 0 {
                std::panic::panic_any(Payload {
                    depth,
                    state: self.state.clone(),
                });
            }
        }
        Ok(Bytes::copy_from_slice(&message.payload))
    }
}
impl Drop for CallbackOwner {
    fn drop(&mut self) {
        self.state.drops.fetch_add(1, Ordering::SeqCst);
        if self.case == Case::CancelClose {
            self.state.retiring.store(1, Ordering::SeqCst);
            let mut released = self.state.released.lock().unwrap();
            while !*released {
                released = self.state.wake.wait(released).unwrap();
            }
        }
        let depth = match self.case {
            Case::CloseDrop(depth) => depth,
            Case::Unpolled
            | Case::Invalid
            | Case::NoRuntime
            | Case::StorageFailure
            | Case::OwnerDrop
            | Case::CancelClose => 8,
            _ => return,
        };
        std::panic::panic_any(Payload {
            depth,
            state: self.state.clone(),
        });
    }
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("inbox-unwind-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn message() -> DurableEnvelope {
    DurableEnvelope {
        source: "source".into(),
        session: "session".into(),
        receiver: "receiver".into(),
        path: "/state".into(),
        sequence: 1,
        type_key: "bin:probe#v1".into(),
        payload: Bytes::from_static(&[7]),
    }
}

async fn exercise(case: Case, state: Arc<State>) {
    let root = Scratch::new();
    let directory = root.0.join("inbox");
    let owner = CallbackOwner {
        case,
        state: state.clone(),
    };
    let transition = move |s: &[u8], m: &DurableEnvelope| owner.call(s, m);
    if case == Case::Unpolled {
        let future = TransactionalInbox::open(
            &directory,
            "receiver",
            "/state",
            Bytes::from_static(&[0]),
            InboxLimits::default(),
            transition,
        );
        assert!(
            std::panic::catch_unwind(AssertUnwindSafe(|| drop(future))).is_ok(),
            "dropping unpolled open escaped"
        );
        assert_eq!(count(&state.drops), 1);
        assert_eq!(
            count(&state.payloads),
            1,
            "retire first payload, bound its throwing successor"
        );
        assert_eq!(count(&state.calls), 0);
        assert!(!directory.exists());
        return;
    }
    let mut limits = InboxLimits::default();
    if case == Case::Invalid {
        limits.command_capacity = 0;
    }
    if case == Case::StorageFailure {
        std::fs::write(&directory, b"not a directory").unwrap();
    }
    let opened = AssertUnwindSafe(TransactionalInbox::open(
        &directory,
        "receiver",
        "/state",
        Bytes::from_static(&[0]),
        limits,
        transition,
    ))
    .catch_unwind()
    .await;
    assert!(opened.is_ok(), "open leaked a user destructor panic");
    let opened = opened.unwrap();
    if matches!(case, Case::Invalid | Case::NoRuntime | Case::StorageFailure) {
        let error = match opened {
            Ok(inbox) => {
                drop(inbox);
                panic!("invalid open succeeded")
            }
            Err(e) => e,
        };
        assert!(
            matches!(
                (&case, &error),
                (Case::Invalid | Case::NoRuntime, InboxError::Invalid(_))
                    | (Case::StorageFailure, InboxError::Storage(_))
            ),
            "{error:?}"
        );
        assert_eq!(count(&state.calls), 0);
        assert_eq!(count(&state.drops), 1);
        assert_eq!(count(&state.payloads), 1);
        if case != Case::StorageFailure {
            assert!(!directory.exists());
        }
        return;
    }
    let mut inbox = opened.unwrap();
    let handle = inbox.handle();
    if let Case::Payload(depth) = case {
        let error = tokio::time::timeout(Duration::from_secs(3), handle.apply(message()))
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(error, InboxError::Business(ref s) if s == "transition panicked"),
            "{error:?}"
        );
        let view = handle.snapshot();
        assert!(!view.closed && view.terminal_error.is_none());
        assert_eq!((view.revision, view.streams), (0, 0));
        assert_eq!(view.state.as_ref(), &[0]);
        assert_eq!(count(&state.payloads), if depth == 0 { 1 } else { 2 });
    }
    tokio::time::timeout(Duration::from_secs(3), handle.apply(message()))
        .await
        .unwrap()
        .unwrap();
    // Retry the same committed identity: no second transition or revision.
    handle.apply(message()).await.unwrap();
    let view = handle.snapshot();
    assert_eq!((view.revision, view.streams), (1, 1));
    assert_eq!(view.state.as_ref(), &[7]);
    assert_eq!(
        count(&state.calls),
        if matches!(case, Case::Payload(_)) {
            2
        } else {
            1
        }
    );
    if case == Case::OwnerDrop {
        assert!(std::panic::catch_unwind(AssertUnwindSafe(|| drop(inbox))).is_ok());
    } else {
        if case == Case::CancelClose {
            let release = Release(state.clone());
            let mut closing = Box::pin(inbox.close());
            assert!(futures::poll!(&mut closing).is_pending());
            tokio::time::timeout(Duration::from_secs(3), async {
                while count(&state.retiring) == 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            let premature = handle.snapshot().closed;
            drop(closing);
            drop(release);
            assert!(
                !premature,
                "closed was published before callback retirement completed"
            );
        }
        let close = tokio::time::timeout(Duration::from_secs(3), inbox.close())
            .await
            .unwrap();
        let again = inbox.close().await;
        if matches!(case, Case::CloseDrop(_) | Case::CancelClose) {
            assert!(matches!(close, Err(InboxError::Panicked)), "{close:?}");
            assert!(matches!(again, Err(InboxError::Panicked)), "{again:?}");
        } else {
            close.unwrap();
            again.unwrap();
        }
    }
    let closed = handle.snapshot();
    assert!(closed.closed);
    if matches!(
        case,
        Case::CloseDrop(_) | Case::OwnerDrop | Case::CancelClose
    ) {
        assert!(
            matches!(closed.terminal_error, Some(InboxError::Panicked)),
            "{closed:?}"
        );
        assert_eq!(count(&state.payloads), 1);
    } else {
        assert!(closed.terminal_error.is_none());
    }
    assert_eq!(count(&state.drops), 1);
    assert_eq!(closed.revision, 1);
    assert_eq!(closed.state.as_ref(), &[7]);
    assert!(matches!(
        handle.apply(message()).await,
        Err(InboxError::Closed)
    ));
    let mut reopened = TransactionalInbox::open(
        &directory,
        "receiver",
        "/state",
        Bytes::from_static(&[99]),
        InboxLimits::default(),
        |_, _| panic!("dedup must survive retirement"),
    )
    .await
    .unwrap();
    reopened.handle().apply(message()).await.unwrap();
    assert_eq!(reopened.handle().snapshot().state.as_ref(), &[7]);
    assert_eq!(reopened.handle().snapshot().revision, 1);
    reopened.close().await.unwrap();
}

fn run(name: &str, case: Case) {
    use std::{
        io::{Read, Seek},
        process::{Command, Stdio},
        time::Instant,
    };
    const CHILD: &str = "PARROT_INBOX_UNWIND_CASE";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        let state = Arc::new(State::default());
        if case == Case::NoRuntime {
            futures::executor::block_on(exercise(case, state));
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(exercise(case, state));
        }
        return;
    }
    let path = std::env::temp_dir().join(format!("inbox-unwind-log-{}-{name}", std::process::id()));
    let mut log = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .stdout(log.try_clone().unwrap())
        .stderr(Stdio::from(log.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("bounded inbox child failed: {result:?}");
            }
        }
    };
    log.rewind().unwrap();
    let mut text = String::new();
    log.read_to_string(&mut text).unwrap();
    drop(log);
    std::fs::remove_file(path).unwrap();
    assert!(status.success(), "{name}: {status}\n{text}");
    assert!(
        text.contains("test result: ok. 1 passed; 0 failed;"),
        "{text}"
    );
}
macro_rules! contract {
    ($name:ident, $case:expr) => {
        #[test]
        fn $name() {
            run(stringify!($name), $case);
        }
    };
}
contract!(
    healthy_transition_retirement_preserves_exact_dedup_state,
    Case::Healthy
);
contract!(
    throwing_panic_payload_preserves_worker_and_retry,
    Case::Payload(0)
);
contract!(
    nested_panic_payload_cannot_bypass_close_publication,
    Case::Payload(1)
);
contract!(
    recursive_panic_payload_is_retired_with_a_bounded_budget,
    Case::Payload(8)
);
contract!(
    callback_drop_failure_is_published_before_close_returns,
    Case::CloseDrop(0)
);
contract!(
    nested_callback_drop_failure_is_stable_across_repeated_close,
    Case::CloseDrop(8)
);
contract!(
    unpolled_open_owns_callback_at_a_safe_boundary,
    Case::Unpolled
);
contract!(
    invalid_open_preserves_primary_error_despite_callback_drop,
    Case::Invalid
);
contract!(
    missing_runtime_preserves_primary_error_despite_callback_drop,
    Case::NoRuntime
);
contract!(
    storage_open_failure_preserves_primary_error_despite_callback_drop,
    Case::StorageFailure
);
contract!(
    owner_drop_contains_callback_failure_and_preserves_committed_state,
    Case::OwnerDrop
);
contract!(
    cancelled_close_keeps_retirement_owned_until_real_completion,
    Case::CancelClose
);
