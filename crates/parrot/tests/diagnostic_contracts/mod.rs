use super::*;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    time::Instant,
};

struct DiagnosticPanic(Arc<AtomicUsize>);
impl Drop for DiagnosticPanic {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("Actix diagnostic panic payload destructor");
    }
}
struct Subscriber {
    events: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl tracing::Subscriber for Subscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == "parrot::actix::message" && *metadata.level() == tracing::Level::ERROR
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        self.events.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(DiagnosticPanic(self.payloads.clone()));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
struct Diagnostics {
    events: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl Diagnostics {
    fn new() -> Self {
        std::panic::set_hook(Box::new(|_| {}));
        let events = Arc::new(AtomicUsize::new(0));
        let payloads = Arc::new(AtomicUsize::new(0));
        tracing::subscriber::set_global_default(Subscriber {
            events: events.clone(),
            payloads: payloads.clone(),
        })
        .unwrap();
        Self { events, payloads }
    }
    fn assert_once(&self) {
        assert_eq!(self.events.load(Ordering::SeqCst), 1);
        assert_eq!(self.payloads.load(Ordering::SeqCst), 1);
    }
}
struct OnDrop<F: FnOnce()>(Option<F>);
impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        self.0.take().unwrap()();
    }
}
fn protected(during_unwind: bool, cleanup: impl FnOnce()) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if during_unwind {
            let _cleanup = OnDrop(Some(cleanup));
            panic!("original Actix caller unwind");
        } else {
            cleanup();
        }
    }));
    if during_unwind {
        let payload = result.expect_err("original caller unwind was swallowed");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"original Actix caller unwind")
        );
    } else if let Err(payload) = result {
        // A regressed diagnostic may itself have a recursive panic destructor.
        std::mem::forget(payload);
        panic!("Actix cleanup leaked a diagnostic panic");
    }
}

fn reply(error: bool, during_unwind: bool) {
    let diagnostic = Diagnostics::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let panic_payloads = Arc::new(AtomicUsize::new(0));
    let payload = ReplyPayload {
        drops: drops.clone(),
        panic_payloads: panic_payloads.clone(),
        hostile: true,
    };
    let result = if error {
        Err(ActorError::Other(anyhow::Error::new(payload)))
    } else {
        Ok(Box::new(payload) as BoxedMessage)
    };
    let reply = ActixReply::new(Some(result));
    protected(during_unwind, || drop(reply));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(panic_payloads.load(Ordering::SeqCst), 1);
    diagnostic.assert_once();
}

fn unpolled(deliver: bool, during_unwind: bool) {
    let diagnostic = Diagnostics::new();
    actix::System::new().block_on(async {
        let address = ActixActor::new(Business::<true> {
            drops: Default::default(),
            panic_payloads: Default::default(),
            hostile: false,
            entered: Default::default(),
            release: None,
        })
        .start();
        let reference = ActixActorRef::new(address, "/diagnostic-input".into());
        let drops = Arc::new(AtomicUsize::new(0));
        let panic_payloads = Arc::new(AtomicUsize::new(0));
        let input = Box::new(ReplyPayload {
            drops: drops.clone(),
            panic_payloads: panic_payloads.clone(),
            hostile: true,
        });
        if deliver {
            let request = reference.deliver(input);
            protected(during_unwind, || drop(request));
        } else {
            let request = reference.send(input);
            protected(during_unwind, || drop(request));
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(panic_payloads.load(Ordering::SeqCst), 1);
        diagnostic.assert_once();
        barrier(&reference).await;
        reference.stop().await.unwrap();
    });
}

fn cancelled(native: bool) {
    let diagnostic = Diagnostics::new();
    actix::System::new().block_on(published::<true>(native, true));
    diagnostic.assert_once();
}

fn isolated(name: &str, body: impl FnOnce()) {
    const FLAG: &str = "PARROT_ACTIX_DIAGNOSTIC_CASE";
    let marker = format!("PARROT_ACTIX_DIAGNOSTIC_OK {name}");
    if std::env::var(FLAG).as_deref() == Ok(name) {
        body();
        println!("{marker}");
        return;
    }
    let log = tempfile::NamedTempFile::new().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(FLAG, name)
        .stdout(log.as_file().try_clone().unwrap())
        .stderr(log.as_file().try_clone().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = std::fs::read_to_string(log.path()).unwrap();
    print!("{output}");
    assert!(
        status.is_some_and(|status| status.success()),
        "{name}: {status:?}"
    );
    assert_eq!(
        output
            .lines()
            .filter(|line| line.trim_end().ends_with(&marker))
            .count(),
        1,
        "isolated child did not execute exactly once"
    );
    assert!(output.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"));
}
macro_rules! contract {
    ($name:ident, $body:expr) => {
        #[test]
        fn $name() {
            isolated(concat!("diagnostic_contracts::", stringify!($name)), || {
                $body
            });
        }
    };
}
contract!(
    unpolled_send_contains_diagnostic_panic,
    unpolled(false, false)
);
contract!(
    unpolled_deliver_contains_diagnostic_panic,
    unpolled(true, false)
);
contract!(
    unpolled_send_preserves_original_unwind,
    unpolled(false, true)
);
contract!(
    unpolled_deliver_preserves_original_unwind,
    unpolled(true, true)
);
contract!(success_reply_contains_diagnostic_panic, reply(false, false));
contract!(
    business_error_reply_contains_diagnostic_panic,
    reply(true, false)
);
contract!(success_reply_preserves_original_unwind, reply(false, true));
contract!(
    business_error_reply_preserves_original_unwind,
    reply(true, true)
);
contract!(
    cancelled_native_published_reply_contains_diagnostic_panic,
    cancelled(true)
);
contract!(
    cancelled_reference_published_reply_contains_diagnostic_panic,
    cancelled(false)
);
