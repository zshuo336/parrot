use super::*;
use futures::FutureExt;
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::AtomicUsize,
    task::{Context, Wake, Waker},
};

pub(crate) struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
pub(crate) struct Probe {
    pub calls: AtomicUsize,
    pub locked: AtomicBool,
    check: Box<dyn Fn() -> bool + Send + Sync>,
    throwing: u8,
    retired: Arc<AtomicUsize>,
}
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("controlled mirror panic payload disposal");
    }
}
impl Wake for Probe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let checked = Cleanup::default().call(|| (self.check)()).unwrap_or(false);
        self.locked.fetch_or(!checked, Ordering::SeqCst);
        match self.throwing {
            1 => panic!("controlled mirror notification panic"),
            2 => std::panic::panic_any(Payload(self.retired.clone())),
            _ => {}
        }
    }
}
pub(crate) fn probe(throwing: u8, check: impl Fn() -> bool + Send + Sync + 'static) -> Arc<Probe> {
    Arc::new(Probe {
        calls: 0.into(),
        locked: false.into(),
        check: Box::new(check),
        throwing,
        retired: Arc::new(0.into()),
    })
}
pub(crate) fn park<T, F: Future<Output = T> + ?Sized>(future: Pin<&mut F>, probe: &Arc<Probe>) {
    assert!(future
        .poll(&mut Context::from_waker(&Waker::from(probe.clone())))
        .is_pending());
}
pub(crate) fn check(probe: &Probe) {
    assert!(
        probe.calls.load(Ordering::SeqCst) > 0,
        "waiter lost its wake"
    );
    assert!(
        !probe.locked.load(Ordering::SeqCst),
        "notification held an owner lock or exposed an incomplete commit"
    );
    if probe.throwing == 2 {
        assert!(
            probe.retired.load(Ordering::SeqCst) > 0,
            "panic payload not retired"
        );
    }
}
pub(crate) fn isolated(name: &str, body: impl FnOnce()) {
    let name = format!("mirror::notification_contracts::{name}");
    isolated_named(&name, body);
}
pub(crate) fn isolated_named(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_MIRROR_CHILD").as_deref() == Ok(name) {
        body();
        return;
    }
    let log = tempfile::NamedTempFile::new().unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    let output = log.reopen().unwrap();
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_MIRROR_CHILD", name)
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let end = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            let output = std::fs::read_to_string(log.path()).unwrap();
            assert!(status.success(), "{name}: {status}\n{output}");
            assert!(
                output.contains("test result: ok. 1 passed; 0 failed;"),
                "contract not executed: {output}"
            );
            return;
        }
        assert!(
            std::time::Instant::now() < end,
            "mirror child stalled: {}",
            std::fs::read_to_string(log.path()).unwrap()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
pub(crate) fn run(body: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body);
}
pub(crate) fn cancelled(handle: &WireMirrorHandle) -> impl Future<Output = ()> + Send + 'static {
    handle.0.cancel.clone().cancelled_owned()
}
fn fixture() -> (Arc<dyn LocalLookup>, Arc<WireMirrorRegistry>) {
    let local: Arc<dyn LocalLookup> = Arc::new(Empty);
    let registry = Arc::new(WireMirrorRegistry::new(&local));
    (local, registry)
}
fn policy(source: &str) -> (WireMirrorPolicy, WireMirrorOptions) {
    (
        WireMirrorPolicy::new(source, "/shadow"),
        WireMirrorOptions::default(),
    )
}
fn retirement(action: u8, throwing: u8) {
    run(async {
        let (_local, registry) = fixture();
        let group = registry
            .register_group(vec![policy("/source/a"), policy("/source/b")])
            .unwrap();
        let handles: Vec<_> = ["/source/a", "/source/b"]
            .iter()
            .map(|s| group.handle_for(s).unwrap())
            .collect();
        let probes: Vec<_> = handles
            .iter()
            .map(|handle| {
                let registry = Arc::downgrade(&registry);
                let entry = Arc::downgrade(&handle.0);
                let cohort: Vec<_> = if matches!(action, 1..=3) {
                    handles.iter().map(|h| Arc::downgrade(&h.0)).collect()
                } else {
                    vec![entry.clone()]
                };
                probe(throwing, move || {
                    let registry = registry.upgrade().unwrap();
                    let entry = entry.upgrade().unwrap();
                    let Ok(state) = registry.state.try_lock() else {
                        return false;
                    };
                    if registry.is_enabled() != !state.entries.is_empty() {
                        return false;
                    }
                    if matches!(action, 1..=3)
                        && cohort.iter().any(|old| {
                            let old = old.upgrade().unwrap();
                            state
                                .entries
                                .get(&old.policy.source_prefix)
                                .is_some_and(|current| Arc::ptr_eq(current, &old))
                        })
                    {
                        return false;
                    }
                    drop(state);
                    if entry.admission.try_lock().is_err() {
                        return false;
                    }
                    for old in &cohort {
                        let old = WireMirrorHandle(old.upgrade().unwrap());
                        let before = old.stats();
                        old.offer_local(
                            &old.0.policy.source_prefix,
                            &(Box::new(()) as parrot_api::types::BoxedMessage),
                        );
                        if old.stats() != before {
                            return false;
                        }
                    }
                    WireMirrorHandle(entry).close();
                    true
                })
            })
            .collect();
        let mut waiters: Vec<_> = handles
            .iter()
            .map(|handle| Box::pin(cancelled(handle)))
            .collect();
        for (waiter, p) in waiters.iter_mut().zip(&probes) {
            park(waiter.as_mut(), p);
        }
        let successor = Cleanup::default().call(|| match action {
            0 => {
                handles[0].close();
                handles[1].close();
                None
            }
            1 => {
                group.cancel();
                None
            }
            2 => {
                registry.close();
                None
            }
            3 => Some(
                registry
                    .replace_group(Some(&group), vec![policy("/replacement")])
                    .unwrap(),
            ),
            4 => {
                registry.remove("/source/a");
                registry.remove("/source/b");
                None
            }
            5 => {
                registry
                    .register(policy("/source/a").0, WireMirrorOptions::default())
                    .unwrap();
                registry
                    .register(policy("/source/b").0, WireMirrorOptions::default())
                    .unwrap();
                None
            }
            _ => unreachable!(),
        });
        assert!(successor.is_ok(), "notification escaped retirement");
        for (waiter, p) in waiters.into_iter().zip(&probes) {
            assert!(waiter.now_or_never().is_some());
            check(p);
        }
        drop(successor);
        registry.close();
        registry.wait_closed().await;
        assert_eq!(registry.workers.available_permits(), 64);
    });
}
macro_rules! retirement_contract {
    ($name:ident, $action:literal, $throwing:literal) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || retirement($action, $throwing));
        }
    };
}
retirement_contract!(handle_cancel_releases_admission, 0, 0);
retirement_contract!(group_cancel_releases_registry, 1, 0);
retirement_contract!(registry_close_releases_registry, 2, 0);
retirement_contract!(group_replace_releases_registry, 3, 0);
retirement_contract!(registry_remove_releases_registry, 4, 0);
retirement_contract!(single_replace_releases_registry, 5, 0);
retirement_contract!(handle_cancel_contains_throwing_wakes, 0, 1);
retirement_contract!(group_cancel_contains_throwing_wakes, 1, 1);
retirement_contract!(registry_close_contains_throwing_wakes, 2, 1);
retirement_contract!(group_replace_contains_throwing_payloads, 3, 2);
retirement_contract!(registry_remove_contains_throwing_payloads, 4, 2);
retirement_contract!(single_replace_contains_throwing_payloads, 5, 2);

fn completed(throwing: u8, barrier: bool) {
    run(async {
        let (_local, registry) = fixture();
        let handle = registry
            .register(policy("/source").0, WireMirrorOptions::default())
            .unwrap();
        let mut waits = Vec::new();
        let mut probes = Vec::new();
        for _ in 0..40 {
            let owner = Arc::downgrade(&registry);
            let entry = Arc::downgrade(&handle.0);
            let p = probe(throwing, move || {
                owner.upgrade().unwrap().state.try_lock().is_ok()
                    && entry.upgrade().unwrap().admission.try_lock().is_ok()
            });
            let mut wait: Pin<Box<dyn Future<Output = ()>>> = if barrier {
                Box::pin(registry.wait_closed())
            } else {
                Box::pin(handle.wait_closed())
            };
            park(wait.as_mut(), &p);
            waits.push(wait);
            probes.push(p);
        }
        registry.close();
        let end = tokio::time::Instant::now() + Duration::from_secs(3);
        while probes.iter().any(|p| p.calls.load(Ordering::SeqCst) == 0) {
            assert!(
                tokio::time::Instant::now() < end,
                "completion waiter lost its wake"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        for (mut wait, p) in waits.into_iter().zip(&probes) {
            assert!(wait
                .as_mut()
                .poll(&mut Context::from_waker(&Waker::from(p.clone())))
                .is_ready());
        }
        for p in &probes {
            check(p);
        }
        assert!(handle.is_closed());
        registry.wait_closed().await;
        assert_eq!(registry.workers.available_permits(), 64);
    });
}
macro_rules! completion_contract {
    ($name:ident, $throwing:literal, $barrier:literal) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || completed($throwing, $barrier));
        }
    };
}
completion_contract!(handle_completion_reentry, 0, false);
completion_contract!(handle_completion_throwing_wakers, 1, false);
completion_contract!(handle_completion_throwing_payloads, 2, false);
completion_contract!(registry_completion_reentry, 0, true);
completion_contract!(registry_completion_throwing_wakers, 1, true);
completion_contract!(registry_completion_throwing_payloads, 2, true);

struct CopyMessage(Arc<AtomicUsize>);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:mirror-notification-copy#v1",
        type_id: std::any::TypeId::of::<CopyMessage>(),
        encode: |message| {
            message.downcast_ref::<CopyMessage>().unwrap().0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![17, 23])
        },
        decode: |_| Ok(Box::new(()) as parrot_api::types::BoxedMessage),
    }
}

fn detached(registry: &WireMirrorRegistry) -> (WireMirrorHandle, mpsc::Receiver<Job>) {
    let options = WireMirrorOptions {
        capacity: 2,
        ..Default::default()
    };
    let (sender, receiver) = mpsc::channel(options.capacity);
    let (done, closed) = watch::channel(false);
    let entry = Arc::new(Entry {
        policy: policy("/source").0,
        admission: Mutex::new(Admission {
            permit: Some(registry.workers.clone().try_acquire_owned().unwrap()),
            ..Default::default()
        }),
        options,
        sender,
        slots: Arc::new(Semaphore::new(options.capacity)),
        bytes: Arc::new(Semaphore::new(options.max_pending_bytes)),
        cancel: CancellationToken::new(),
        closed,
        done,
        counters: Arc::new(Counters::default()),
    });
    registry
        .state
        .lock()
        .unwrap()
        .entries
        .insert("/source".into(), entry.clone());
    registry.enabled.store(true, Ordering::Release);
    (WireMirrorHandle(entry), receiver)
}
fn queued(local_copy: bool, throwing: u8) {
    run(async {
        let (_local, registry) = fixture();
        let (handle, mut receiver) = detached(&registry);
        let entry = Arc::downgrade(&handle.0);
        let owner = Arc::downgrade(&registry);
        let p = probe(throwing, move || {
            let registry = owner.upgrade().unwrap();
            let entry = entry.upgrade().unwrap();
            if registry.state.try_lock().is_err() || entry.admission.try_lock().is_err() {
                return false;
            }
            if entry.counters.enqueued.load(Ordering::Relaxed) != 1 {
                return false;
            }
            // Actual reentry, after the committed item and counter are visible.
            WireMirrorHandle(entry).close();
            true
        });
        let mut recv = Box::pin(receiver.recv());
        park(recv.as_mut(), &p);
        let copies = Arc::new(AtomicUsize::new(0));
        let offered = Cleanup::default().call(|| {
            if local_copy {
                handle.offer_local(
                    "/source",
                    &(Box::new(CopyMessage(copies.clone())) as parrot_api::types::BoxedMessage),
                );
            } else {
                registry.offer("/source", "bin:test#v1", &[17, 23]);
            }
        });
        assert!(offered.is_ok(), "queue wake escaped offer");
        check(&p);
        let job = recv.now_or_never().flatten().expect("accepted job lost");
        assert_eq!(job.payload, [17, 23]);
        assert_eq!(
            job.type_key,
            if local_copy {
                "bin:mirror-notification-copy#v1"
            } else {
                "bin:test#v1"
            }
        );
        assert_eq!(copies.load(Ordering::SeqCst), usize::from(local_copy));
        assert_eq!(handle.stats().enqueued, 1);
        assert_eq!(handle.stats().full, 0);
        drop(job);
        let worker = Worker {
            entry: handle.0.clone(),
            local: registry.local.clone(),
            remote: Weak::new(),
            receiver,
            active: None,
        };
        drop(worker);
        registry.close();
        registry.wait_closed().await;
        assert_eq!(registry.workers.available_permits(), 64);
    });
}
macro_rules! queue_contract {
    ($name:ident, $local:literal, $throwing:literal) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || queued($local, $throwing));
        }
    };
}
queue_contract!(wire_queue_reentry, false, 0);
queue_contract!(wire_queue_throwing_waker, false, 1);
queue_contract!(wire_queue_throwing_payload, false, 2);
queue_contract!(local_queue_reentry, true, 0);
queue_contract!(local_queue_throwing_waker, true, 1);
queue_contract!(local_queue_throwing_payload, true, 2);

fn worker_retirement(throwing: u8, encoder: bool) {
    run(async {
        let (_local, registry) = fixture();
        let (handle, receiver) = detached(&registry);
        registry.offer("/source", "bin:test#v1", &[17, 23]);
        registry.offer("/source", "bin:test#v1", &[17, 23]);
        let mut receiver = receiver;
        let active = receiver.try_recv().ok();
        let worker = Worker {
            entry: handle.0.clone(),
            local: registry.local.clone(),
            remote: Weak::new(),
            receiver,
            active,
        };
        let encoding = encoder.then(|| {
            handle.0.admission.lock().unwrap().encoders += 1;
            Encoding(&handle.0)
        });
        let entry = Arc::downgrade(&handle.0);
        let p = probe(throwing, move || {
            let entry = entry.upgrade().unwrap();
            if entry.admission.try_lock().is_err() {
                return false;
            }
            WireMirrorHandle(entry).close();
            true
        });
        let mut cancelled = Box::pin(cancelled(&handle));
        park(cancelled.as_mut(), &p);
        let completed = probe(throwing, || true);
        let mut wait = Box::pin(handle.wait_closed());
        park(wait.as_mut(), &completed);
        assert!(
            Cleanup::default().call(|| drop(worker)).is_ok(),
            "worker destruction unwound"
        );
        check(&p);
        assert_eq!(handle.stats().enqueued, 2);
        assert_eq!(handle.stats().cancelled, 2);
        assert_eq!(handle.0.slots.available_permits(), 2);
        assert_eq!(
            handle.0.bytes.available_permits(),
            handle.0.options.max_pending_bytes
        );
        if encoder {
            assert!(!handle.is_closed(), "encoder ownership retired prematurely");
            assert_eq!(registry.workers.available_permits(), 63);
            assert_eq!(completed.calls.load(Ordering::SeqCst), 0);
        }
        assert!(
            Cleanup::default().call(|| drop(encoding)).is_ok(),
            "encoder completion unwound"
        );
        check(&completed);
        assert!(wait.now_or_never().is_some());
        assert!(handle.is_closed());
        assert_eq!(registry.workers.available_permits(), 64);
        registry.close();
        registry.wait_closed().await;
    });
}
macro_rules! worker_contract {
    ($name:ident, $throwing:literal, $encoder:literal) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || worker_retirement($throwing, $encoder));
        }
    };
}
worker_contract!(worker_drop_reentry, 0, false);
worker_contract!(worker_drop_throwing_waker, 1, false);
worker_contract!(worker_drop_throwing_payload, 2, false);
worker_contract!(last_encoder_reentry, 0, true);
worker_contract!(last_encoder_throwing_waker, 1, true);
worker_contract!(last_encoder_throwing_payload, 2, true);
