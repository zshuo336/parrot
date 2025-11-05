use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Wake, Waker},
};

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static CAPTURE: Cell<bool> = const { Cell::new(false) };
    static LARGEST: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    static TRACK: Cell<usize> = const { Cell::new(0) };
    static FREED: Cell<bool> = const { Cell::new(false) };
    static ZST_DROPS: Cell<usize> = const { Cell::new(0) };
}
struct Allocator;
// SAFETY: all requests are forwarded to System unchanged. Const TLS recording
// does not allocate, panic, or change the requested ownership/layout.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        let _ = CAPTURE.try_with(|capture| {
            if capture.get() && !pointer.is_null() {
                ALLOCATIONS.with(|value| value.set(value.get() + 1));
                LARGEST.with(|largest| {
                    if layout.size() > largest.get().1 {
                        largest.set((pointer as usize, layout.size()));
                    }
                });
            }
        });
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let _ = TRACK.try_with(|tracked| {
            if tracked.get() == pointer as usize {
                tracked.set(0);
                FREED.with(|freed| freed.set(true));
            }
        });
        unsafe { System.dealloc(pointer, layout) };
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn allocate<T>(make: impl FnOnce() -> T) -> T {
    ALLOCATIONS.with(|value| value.set(0));
    LARGEST.with(|value| value.set((0, 0)));
    FREED.with(|value| value.set(false));
    CAPTURE.with(|value| value.set(true));
    let value = make();
    CAPTURE.with(|value| value.set(false));
    TRACK.with(|tracked| tracked.set(LARGEST.with(|largest| largest.get().0)));
    value
}
fn check_block(envelope: &SingleAllocEnvelope) {
    assert_eq!(envelope.block_size(), LARGEST.with(|value| value.get().1));
}
fn was_freed() -> bool {
    FREED.with(Cell::get)
}
struct Payload {
    drops: Arc<AtomicUsize>,
    panic: bool,
    _bytes: [u8; 8192],
}
impl Payload {
    fn new(drops: &Arc<AtomicUsize>, panic: bool) -> Self {
        Self {
            drops: drops.clone(),
            panic,
            _bytes: [0; 8192],
        }
    }
}
impl Drop for Payload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "payload disposal failed");
    }
}
struct PanicWake;
impl Wake for PanicWake {
    fn wake(self: Arc<Self>) {
        panic!("reply wake failed");
    }
}
fn install_panicking_waker<T>(receiver: &mut tokio::sync::oneshot::Receiver<T>) {
    let waker = Waker::from(Arc::new(PanicWake));
    assert!(
        Pin::new(receiver)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
}
fn panics(operation: impl FnOnce()) {
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)).is_err());
}

#[test]
fn payload_panic_retires_reply_and_exact_backing_allocation() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (envelope, mut reply) =
        allocate(|| SingleAllocEnvelope::new_ask(Payload::new(&drops, true)));
    check_block(&envelope);
    panics(|| drop(envelope));
    assert!(matches!(
        reply.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
    assert!(was_freed(), "envelope allocation leaked");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
#[test]
fn wake_panic_during_drop_still_frees_backing_allocation() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (envelope, mut reply) =
        allocate(|| SingleAllocEnvelope::new_ask(Payload::new(&drops, false)));
    check_block(&envelope);
    install_panicking_waker(&mut reply);
    panics(|| drop(envelope));
    assert!(was_freed(), "wake panic skipped deallocation");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(matches!(
        reply.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
}
#[test]
fn wake_panic_during_downcast_retires_moved_payload_and_storage() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (envelope, mut reply) =
        allocate(|| SingleAllocEnvelope::new_ask(Payload::new(&drops, false)));
    check_block(&envelope);
    install_panicking_waker(&mut reply);
    panics(|| {
        let _ = envelope.downcast::<Payload>();
    });
    assert!(was_freed(), "downcast wake panic skipped deallocation");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(matches!(
        reply.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
}
#[test]
fn transfer_preserves_payload_and_independent_reply_ownership() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (mut envelope, mut reply) =
        allocate(|| SingleAllocEnvelope::new_ask(Payload::new(&drops, false)));
    check_block(&envelope);
    let sender = envelope.take_reply().unwrap();
    assert!(envelope.take_reply().is_none());
    let payload = envelope.downcast::<Payload>().ok().unwrap();
    assert!(was_freed());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(matches!(
        reply.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    sender.send(Ok(Box::new(71u64))).unwrap();
    assert_eq!(
        *reply
            .try_recv()
            .unwrap()
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        71
    );
    drop(payload);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
#[test]
fn mismatched_downcast_retains_owner_until_panicking_disposal() {
    let drops = Arc::new(AtomicUsize::new(0));
    let envelope = allocate(|| SingleAllocEnvelope::new(Payload::new(&drops, true)));
    check_block(&envelope);
    let envelope = envelope.downcast::<String>().err().unwrap();
    assert!(!was_freed());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    panics(|| drop(envelope));
    assert!(was_freed());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
#[repr(align(256))]
struct AlignedZst;
impl Drop for AlignedZst {
    fn drop(&mut self) {
        ZST_DROPS.with(|value| value.set(value.get() + 1));
        panic!("aligned zero-size payload");
    }
}
#[test]
fn aligned_zero_size_payload_unwind_releases_correct_layout() {
    ZST_DROPS.with(|value| value.set(0));
    let (envelope, mut reply) = allocate(|| SingleAllocEnvelope::new_ask(AlignedZst));
    check_block(&envelope);
    panics(|| drop(envelope));
    assert!(was_freed());
    assert_eq!(ZST_DROPS.with(Cell::get), 1);
    assert!(matches!(
        reply.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
}

#[test]
fn constructor_cost_includes_separate_reply_channel_state() {
    let envelope = allocate(|| SingleAllocEnvelope::new(13u64));
    assert_eq!(ALLOCATIONS.with(Cell::get), 1);
    drop(envelope);
    let (envelope, reply) = allocate(|| SingleAllocEnvelope::new_ask(13u64));
    assert_eq!(ALLOCATIONS.with(Cell::get), 2);
    drop(envelope);
    drop(reply);
}
