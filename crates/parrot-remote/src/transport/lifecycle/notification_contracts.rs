//! Task cancellation and completion callbacks preserve every owned waiter.
use super::*;
use futures::FutureExt;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Weak,
    },
    task::{Wake, Waker},
};

const WAITERS: usize = 40;
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("controlled task notification payload disposal");
    }
}
struct Probe {
    tasks: Weak<ConnectionTasks>,
    calls: AtomicUsize,
    locked: AtomicBool,
    premature: AtomicBool,
    reentered: AtomicUsize,
    throwing: u8,
    retired: Arc<AtomicUsize>,
}
impl Wake for Probe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(tasks) = self.tasks.upgrade() {
            let unlocked = tasks.closed.try_lock().is_ok();
            self.locked.fetch_or(!unlocked, Ordering::SeqCst);
            if unlocked {
                self.premature.fetch_or(
                    !tasks.is_closed() || !tasks.tracker.is_closed(),
                    Ordering::SeqCst,
                );
                self.premature.fetch_or(
                    tasks.admit().is_ok()
                        || tasks.spawn(async {}).is_ok()
                        || !matches!(
                            tasks
                                .run(async { Ok::<(), RemoteError>(()) })
                                .now_or_never(),
                            Some(Err(_))
                        ),
                    Ordering::SeqCst,
                );
                tasks.close();
                self.reentered.fetch_add(1, Ordering::SeqCst);
            }
        }
        match self.throwing {
            1 => panic!("controlled task notification panic"),
            2 => std::panic::panic_any(Payload(self.retired.clone())),
            _ => {}
        }
    }
}
fn probe(tasks: &Arc<ConnectionTasks>, throwing: u8) -> Arc<Probe> {
    Arc::new(Probe {
        tasks: Arc::downgrade(tasks),
        calls: 0.into(),
        locked: false.into(),
        premature: false.into(),
        reentered: 0.into(),
        throwing,
        retired: Arc::new(AtomicUsize::new(0)),
    })
}
fn park<T>(future: Pin<&mut impl Future<Output = T>>, probe: &Arc<Probe>) {
    let waker = Waker::from(probe.clone());
    assert!(future.poll(&mut Context::from_waker(&waker)).is_pending());
}
fn check(probe: &Arc<Probe>) {
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        1,
        "every waiter must wake exactly once"
    );
    assert!(
        !probe.locked.load(Ordering::SeqCst),
        "notification held task admission lock"
    );
    assert!(
        !probe.premature.load(Ordering::SeqCst),
        "notification preceded admission or tracker fence"
    );
    assert_eq!(probe.reentered.load(Ordering::SeqCst), 1);
    assert_eq!(
        probe.retired.load(Ordering::SeqCst),
        usize::from(probe.throwing == 2)
    );
}
struct Operation(Arc<AtomicUsize>);
impl Drop for Operation {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl Future for Operation {
    type Output = Result<(), RemoteError>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}
fn close(tasks: &ConnectionTasks) {
    let result = crate::ingress::unwind::Cleanup::default().call(|| tasks.close());
    let poisoned = tasks.closed.is_poisoned();
    // Preserve the primary observation when exercising the old implementation.
    tasks.closed.clear_poison();
    assert!(!poisoned, "notification poisoned task admission");
    assert_eq!(result, Ok(()), "notification interrupted task closure");
}
async fn cancellation(throwing: u8) {
    let tasks = Arc::new(ConnectionTasks::default());
    let retired = Arc::new(AtomicUsize::new(0));
    let probes: Vec<_> = (0..WAITERS).map(|_| probe(&tasks, throwing)).collect();
    let mut operations: Vec<_> = (0..WAITERS)
        .map(|_| Box::pin(tasks.run(Operation(retired.clone()))))
        .collect();
    for (operation, probe) in operations.iter_mut().zip(&probes) {
        park(operation.as_mut(), probe);
    }
    close(&tasks);
    for probe in &probes {
        check(probe);
    }
    assert!(
        tasks.wait().now_or_never().is_none(),
        "wait completed before accepted operations retired"
    );
    for operation in &mut operations {
        assert!(matches!(operation.as_mut().now_or_never(), Some(Err(_))));
    }
    assert_eq!(retired.load(Ordering::SeqCst), WAITERS);
    assert!(tasks.wait().now_or_never().is_some());
    tasks.close();
}
async fn completion(throwing: u8, active: bool) {
    let tasks = Arc::new(ConnectionTasks::default());
    let retired = Arc::new(AtomicUsize::new(0));
    let mut operation = Box::pin(tasks.run(Operation(retired.clone())));
    if active {
        assert!(operation.as_mut().now_or_never().is_none());
    }
    let probes: Vec<_> = (0..WAITERS).map(|_| probe(&tasks, throwing)).collect();
    let mut waits: Vec<_> = (0..WAITERS).map(|_| Box::pin(tasks.wait())).collect();
    for (wait, probe) in waits.iter_mut().zip(&probes) {
        park(wait.as_mut(), probe);
    }
    close(&tasks);
    if active {
        assert!(probes
            .iter()
            .all(|probe| probe.calls.load(Ordering::SeqCst) == 0));
        let result =
            crate::ingress::unwind::Cleanup::default().call(|| operation.as_mut().now_or_never());
        assert!(
            matches!(result, Ok(Some(Err(_)))),
            "last operation release interrupted its caller"
        );
        assert_eq!(retired.load(Ordering::SeqCst), 1);
    }
    for probe in &probes {
        check(probe);
    }
    for wait in &mut waits {
        assert_eq!(wait.as_mut().now_or_never(), Some(()));
    }
    tasks.close();
}
async fn replacement(barrier: bool) {
    let tasks = Arc::new(ConnectionTasks::default());
    let first = probe(&tasks, 1);
    let next = probe(&tasks, 0);
    if barrier {
        let mut wait = Box::pin(tasks.wait());
        park(wait.as_mut(), &first);
        park(wait.as_mut(), &next);
        close(&tasks);
        assert_eq!(wait.as_mut().now_or_never(), Some(()));
    } else {
        let mut operation = Box::pin(tasks.run(std::future::pending::<Result<(), RemoteError>>()));
        park(operation.as_mut(), &first);
        park(operation.as_mut(), &next);
        close(&tasks);
        assert!(matches!(operation.as_mut().now_or_never(), Some(Err(_))));
    }
    assert_eq!(
        first.calls.load(Ordering::SeqCst),
        0,
        "obsolete caller Waker was retained"
    );
    check(&next);
    tasks.wait().await;
}
struct TransportWake {
    calls: AtomicUsize,
    throwing: u8,
    retired: Arc<AtomicUsize>,
}
impl Wake for TransportWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.throwing {
            1 => panic!("controlled public transport wake"),
            2 => std::panic::panic_any(Payload(self.retired.clone())),
            _ => unreachable!(),
        }
    }
}
fn transport(kind: &str, node: &str) -> Arc<dyn super::super::Transport> {
    let (inbound, _) = tokio::sync::mpsc::channel(4);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let handshake = crate::handshake::HandshakeBody {
        node_id: node.into(),
        ..Default::default()
    };
    let disconnect: super::super::OnDisconnect = Arc::new(|_| {});
    match kind {
        "tcp" => Arc::new(super::super::tcp::TcpTransport::new(
            handshake, inbound, disconnect, shutdown, None,
        )),
        "mem" => Arc::new(
            super::super::memory::MemoryTransport::new(
                handshake, inbound, disconnect, shutdown, None,
            )
            .unwrap(),
        ),
        _ => unreachable!(),
    }
}
async fn public_transport(kind: &str, throwing: u8) {
    let cleanup = crate::ingress::unwind::Cleanup::default();
    let original = cleanup.owned(transport(kind, "task-notification-peer"));
    let transport = original.value();
    transport
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = transport.local_addr();
    let probes: Vec<_> = (0..3)
        .map(|_| {
            Arc::new(TransportWake {
                calls: 0.into(),
                throwing,
                retired: Arc::new(AtomicUsize::new(0)),
            })
        })
        .collect();
    let mut accepts: Vec<_> = (0..3).map(|_| transport.accept()).collect();
    for (accept, probe) in accepts.iter_mut().zip(&probes) {
        assert!(accept
            .as_mut()
            .poll(&mut Context::from_waker(&Waker::from(probe.clone())))
            .is_pending());
    }
    let result = cleanup.call(|| transport.close());
    let mut outcomes = Vec::new();
    for mut accept in accepts {
        outcomes.push(cleanup.call(|| matches!(accept.as_mut().now_or_never(), Some(Err(_)))));
        cleanup.dispose(accept);
    }
    assert_eq!(result, Ok(()), "public close was interrupted");
    for (outcome, probe) in outcomes.into_iter().zip(&probes) {
        assert_eq!(
            outcome,
            Ok(true),
            "accept cancellation did not return its transport error"
        );
        assert!(
            probe.calls.load(Ordering::SeqCst) >= 1,
            "public accept cancellation lost its wake"
        );
        assert_eq!(
            probe.retired.load(Ordering::SeqCst),
            probe.calls.load(Ordering::SeqCst) * usize::from(throwing == 2)
        );
    }
    assert!(matches!(transport.accept().now_or_never(), Some(Err(_))));
    tokio::time::timeout(std::time::Duration::from_secs(2), transport.shutdown())
        .await
        .unwrap();
    if let Some(addr) = addr {
        assert!(transport.local_addr().is_none());
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        drop(listener);
    } else {
        let replacement = self::transport(kind, "task-notification-peer");
        replacement.shutdown().await;
    }
}
macro_rules! contract {
    ($name:ident, $exercise:expr) => {
        #[tokio::test]
        async fn $name() {
            crate::system::diagnostic_contracts::isolated(
                concat!(
                    "transport::lifecycle::notification_contracts::",
                    stringify!($name)
                ),
                &[],
                false,
                0,
                async { $exercise },
            )
            .await;
        }
    };
}
contract!(
    cancellation_fences_before_every_reentrant_wake,
    cancellation(0).await
);
contract!(
    cancellation_isolates_every_throwing_wake,
    cancellation(1).await
);
contract!(
    cancellation_isolates_every_throwing_payload,
    cancellation(2).await
);
contract!(
    empty_close_notifies_every_reentrant_barrier,
    completion(0, false).await
);
contract!(
    empty_close_isolates_every_throwing_barrier,
    completion(1, false).await
);
contract!(
    empty_close_isolates_every_throwing_barrier_payload,
    completion(2, false).await
);
contract!(
    last_operation_notifies_every_reentrant_barrier,
    completion(0, true).await
);
contract!(
    last_operation_isolates_every_throwing_barrier,
    completion(1, true).await
);
contract!(
    last_operation_isolates_every_throwing_barrier_payload,
    completion(2, true).await
);
contract!(
    cancellation_replaces_obsolete_caller_waker,
    replacement(false).await
);
contract!(
    completion_replaces_obsolete_caller_waker,
    replacement(true).await
);
contract!(
    tcp_accept_close_isolates_throwing_wakes_and_releases_listener,
    public_transport("tcp", 1).await
);
contract!(
    tcp_accept_close_isolates_throwing_payloads_and_releases_listener,
    public_transport("tcp", 2).await
);
contract!(
    mem_accept_close_isolates_throwing_wakes_and_releases_endpoint,
    public_transport("mem", 1).await
);
contract!(
    mem_accept_close_isolates_throwing_payloads_and_releases_endpoint,
    public_transport("mem", 2).await
);
