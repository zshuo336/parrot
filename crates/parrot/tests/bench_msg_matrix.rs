//! Message matrix: dynamic/typed payloads, ask, admission throughput and allocation windows.
//!
//! The six ignored experiments require release and each runs in a separate process.
//! Ordinary small semantic tests run the same workloads in every profile. Every ask
//! checks its reply; tell batches check admission and then verify a FIFO processing
//! barrier before another workload starts. Timing rows for tell measure admission;
//! allocation rows include processing and one completion barrier, and also report
//! a separate barrier-only control. Process allocation counts include runtime work;
//! they are neither payload-only costs nor exact attribution to a particular actor.
//! No zero-allocation or estimated enum-size claims are made. This matrix does not
//! replace the default-API M4 latency gate with its original 1.15 limit.
//!
//! cargo test -p parrot --release --test bench_msg_matrix -- --ignored --nocapture

use std::alloc::{GlobalAlloc, Layout, System};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt;
use parrot::thread::ThreadActorRef;
use parrot::thread::config::ThreadActorSystemConfig;
use parrot::thread::system::ThreadActorSystem;
use parrot::thread::typed::TypedActorRef;
use parrot_api::ParrotTypedActor;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::message::Message;
use parrot_api::typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive};
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use tokio::sync::Semaphore;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
struct CountingAlloc;
fn record(ptr: *mut u8) {
    if !ptr.is_null() {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
    }
}
// SAFETY: requests and pointers are forwarded unchanged to System; recording
// successful calls only uses an atomic and cannot allocate or unwind.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        record(ptr);
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        record(ptr);
        ptr
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, size) };
        record(ptr);
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static GLOBAL_ALLOC: CountingAlloc = CountingAlloc;

#[derive(Clone)]
struct Msg64([u8; 64]);
#[derive(Clone)]
struct Msg1K([u8; 1024]);
#[derive(Clone)]
struct Msg64K(Box<[u8; 65536]>);
#[derive(Clone, Copy, Debug)]
pub struct PingS(u64);
#[derive(Clone, Debug)]
pub struct PingM([u8; 64]);
#[derive(Clone, Debug)]
pub struct PingL([u8; 1024]);
impl Message for PingS {
    type Result = u64;
}
impl Message for PingM {
    type Result = u64;
}
impl Message for PingL {
    type Result = u64;
}
#[derive(Clone, Copy, Debug)]
pub struct Snapshot;
impl Message for Snapshot {
    type Result = Stats;
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    completed: u64,
    checksum: u64,
}
impl Stats {
    fn record(&mut self, value: u64) {
        self.completed += 1;
        self.checksum = self.checksum.wrapping_add(value);
    }
    fn verify_delta(self, before: Self, count: u64, checksum: u64) {
        assert_eq!(
            self.completed - before.completed,
            count,
            "processed message count"
        );
        assert_eq!(
            self.checksum.wrapping_sub(before.checksum),
            checksum,
            "processed payload checksum"
        );
    }
}
#[derive(Default)]
struct DynEcho {
    stats: Stats,
    pause: Option<Arc<Semaphore>>,
}
impl Actor for DynEcho {
    type Config = EmptyConfig;
    type Context = parrot::thread::context::ThreadContext<Self>;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<Snapshot>() {
                return Ok(Box::new(self.stats) as BoxedMessage);
            }
            if let Some(gate) = &self.pause {
                drop(gate.acquire().await.unwrap());
            }
            let value = if let Some(v) = msg.downcast_ref::<u64>() {
                *v
            } else if let Some(v) = msg.downcast_ref::<Msg64>() {
                u64::from(v.0[0])
            } else if let Some(v) = msg.downcast_ref::<Msg1K>() {
                u64::from(v.0[0])
            } else if let Some(v) = msg.downcast_ref::<Msg64K>() {
                u64::from(v.0[0])
            } else {
                panic!("unexpected matrix message")
            };
            self.stats.record(value);
            Ok(Box::new(value) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

#[derive(ParrotTypedActor, Default)]
#[ParrotTypedActor(msgs(PingS, Snapshot))]
struct EchoS {
    stats: Stats,
    pause: Option<Arc<Semaphore>>,
}
impl TypedReceive<PingS> for EchoS {
    fn receive_typed<'a>(&'a mut self, msg: PingS) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move {
            if let Some(gate) = &self.pause {
                drop(gate.acquire().await.unwrap());
            }
            self.stats.record(msg.0);
            Ok(msg.0)
        })
    }
}
#[derive(ParrotTypedActor, Default)]
#[ParrotTypedActor(msgs(PingM, Snapshot))]
struct EchoM {
    stats: Stats,
}
#[derive(ParrotTypedActor, Default)]
#[ParrotTypedActor(msgs(PingL, Snapshot))]
struct EchoL {
    stats: Stats,
}
#[derive(ParrotTypedActor, Default)]
#[ParrotTypedActor(msgs(PingS, PingL, Snapshot))]
struct EchoMixed {
    stats: Stats,
}
macro_rules! receive {
    ($actor:ty, $msg:ty, $value:expr) => {
        impl TypedReceive<$msg> for $actor {
            fn receive_typed<'a>(&'a mut self, msg: $msg) -> BoxedFuture<'a, ActorResult<u64>> {
                Box::pin(async move {
                    let value = ($value)(msg);
                    self.stats.record(value);
                    Ok(value)
                })
            }
        }
    };
}
receive!(EchoM, PingM, |m: PingM| u64::from(m.0[0]));
receive!(EchoL, PingL, |m: PingL| u64::from(m.0[0]));
receive!(EchoMixed, PingS, |m: PingS| m.0);
receive!(EchoMixed, PingL, |m: PingL| u64::from(m.0[0]));
macro_rules! snapshot {
    ($($actor:ty),+) => { $(impl TypedReceive<Snapshot> for $actor {
        fn receive_typed<'a>(&'a mut self, _: Snapshot) -> BoxedFuture<'a, ActorResult<Stats>> {
            Box::pin(async move { Ok(self.stats) })
        }
    })+ };
}
snapshot!(EchoS, EchoM, EchoL, EchoMixed);

const TMO: Duration = Duration::from_secs(30);
fn with_system<F, Fut>(body: F)
where
    F: FnOnce(Arc<ThreadActorSystem>) -> Fut,
    Fut: Future<Output = ()>,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
        let result = tokio::time::timeout(
            Duration::from_secs(300),
            AssertUnwindSafe(body(system.clone())).catch_unwind(),
        )
        .await;
        let shutdown = system.shutdown_internal().await;
        match result {
            Ok(Ok(())) => shutdown.expect("matrix system retirement"),
            Ok(Err(payload)) => std::panic::resume_unwind(payload),
            Err(error) => panic!("matrix workload timed out: {error}; shutdown={shutdown:?}"),
        }
    });
}
fn timing_line(name: &str, count: u64, elapsed: Duration, details: &str) -> String {
    assert!(
        count > 0 && !elapsed.is_zero(),
        "timing needs completed work and a real duration"
    );
    let throughput = count as f64 / elapsed.as_secs_f64();
    assert!(throughput.is_finite());
    format!(
        "[matrix] {name} | msgs={count} | wall={:.6}s | tput={throughput:.0}/s | {details}",
        elapsed.as_secs_f64()
    )
}
fn row(name: &str, count: u64, elapsed: Duration, details: &str) {
    println!("{}", timing_line(name, count, elapsed, details));
}
fn typed_layout<A: ParrotTypedDispatch>() -> String {
    format!(
        "typed_msg_bytes={} typed_reply_bytes={} allocations=not_measured",
        std::mem::size_of::<A::Msg>(),
        std::mem::size_of::<A::Reply>()
    )
}
async fn dynamic_snapshot(r: &impl ActorRef) -> Stats {
    *r.send_with_timeout(Box::new(Snapshot), Some(TMO))
        .await
        .expect("dynamic completion barrier")
        .downcast::<Stats>()
        .expect("snapshot reply type")
}
async fn typed_snapshot<A, M>(r: &TypedActorRef<A, M>) -> Stats
where
    A: ParrotTypedDispatch,
    M: Message + ParrotMsgVariant<A>,
    Snapshot: ParrotMsgVariant<A>,
{
    r.ref_for::<Snapshot>()
        .ask(Snapshot)
        .await
        .expect("typed completion barrier")
}
async fn dynamic_asks(
    r: &ThreadActorRef<DynEcho>,
    count: u64,
    make: impl Fn(u64) -> (BoxedMessage, u64),
) -> Duration {
    let start = Instant::now();
    for i in 0..count {
        let (msg, expected) = make(i);
        let value = r
            .ask(msg)
            .await
            .expect("dynamic ask")
            .downcast::<u64>()
            .expect("u64 reply");
        assert_eq!(*value, expected, "dynamic reply payload");
    }
    start.elapsed()
}
async fn inline_asks(r: &ThreadActorRef<DynEcho>, count: u64, offset: u64) -> Duration {
    let start = Instant::now();
    for i in 0..count {
        let value = r
            .ask_inline(i + offset, TMO)
            .await
            .expect("inline ask")
            .downcast::<u64>()
            .expect("u64 reply");
        assert_eq!(*value, i + offset, "inline reply payload");
    }
    start.elapsed()
}
async fn typed_asks<A, M>(
    r: &TypedActorRef<A, M>,
    count: u64,
    make: impl Fn(u64) -> (M, u64),
) -> Duration
where
    A: ParrotTypedDispatch,
    M: Message<Result = u64> + ParrotMsgVariant<A>,
{
    let start = Instant::now();
    for i in 0..count {
        let (msg, expected) = make(i);
        assert_eq!(
            r.ask(msg).await.expect("typed ask"),
            expected,
            "typed reply payload"
        );
    }
    start.elapsed()
}
async fn admit_dynamic(
    r: &impl ActorRef,
    count: u64,
    make: impl Fn(u64) -> (BoxedMessage, u64),
) -> u64 {
    let mut checksum = 0_u64;
    for i in 0..count {
        let (msg, value) = make(i);
        r.deliver(msg).await.expect("dynamic tell admission");
        checksum = checksum.wrapping_add(value);
    }
    checksum
}
async fn admit_typed(r: &TypedActorRef<EchoS, PingS>, count: u64) {
    for _ in 0..count {
        r.tell(PingS(1)).await.expect("typed tell admission");
    }
}
async fn dynamic_tells(
    r: &impl ActorRef,
    count: u64,
    make: impl Fn(u64) -> (BoxedMessage, u64),
) -> Duration {
    let before = dynamic_snapshot(r).await;
    let start = Instant::now();
    let checksum = admit_dynamic(r, count, make).await;
    let elapsed = start.elapsed(); // Admission throughput; processing barrier is outside this timer.
    dynamic_snapshot(r)
        .await
        .verify_delta(before, count, checksum);
    elapsed
}
#[derive(Debug)]
struct AllocationSample {
    messages: u64,
    window_calls: usize,
    barrier_control_calls: usize,
    checksum: u64,
}
impl AllocationSample {
    fn line(&self, name: &str) -> String {
        assert!(self.messages > 0);
        format!(
            "[allocation] {name} | completed_messages={} | window_alloc_calls={} | window_calls_per_message={:.3} | barrier_control_calls={} | checksum={} | scope=process_including_runtime_and_one_completion_barrier",
            self.messages,
            self.window_calls,
            self.window_calls as f64 / self.messages as f64,
            self.barrier_control_calls,
            self.checksum
        )
    }
}
async fn allocation_batch<Q, QF, B, BF>(
    count: u64,
    checksum: u64,
    query: Q,
    batch: B,
) -> AllocationSample
where
    Q: Fn() -> QF,
    QF: Future<Output = Stats>,
    B: FnOnce() -> BF,
    BF: Future<Output = ()>,
{
    assert!(count > 0);
    let before = query().await; // Drain warmup before opening either measurement window.
    let control_start = ALLOCS.load(Ordering::SeqCst);
    let control = query().await;
    let barrier_control_calls = ALLOCS.load(Ordering::SeqCst) - control_start;
    assert_eq!(
        control, before,
        "barrier must not count as a payload message"
    );
    let start = ALLOCS.load(Ordering::SeqCst);
    batch().await;
    let after = query().await; // Processing, not just admission, must finish inside this window.
    let window_calls = ALLOCS.load(Ordering::SeqCst) - start;
    after.verify_delta(before, count, checksum);
    AllocationSample {
        messages: count,
        window_calls,
        barrier_control_calls,
        checksum,
    }
}

fn run_a(n: u64, warmup: u64, large: u64, large_warmup: u64) {
    with_system(|ts| async move {
        let r = ts
            .spawn_root_typed_thread(DynEcho::default(), EmptyConfig)
            .await
            .unwrap();
        inline_asks(&r, warmup, 0).await;
        row(
            "A1 dyn-ask-inline-u64",
            n,
            inline_asks(&r, n, 0).await,
            "payload=inline-u64 allocations=not_measured",
        );
        dynamic_asks(&r, warmup, |i| (Box::new(i), i)).await;
        row(
            "A2 dyn-ask-boxed-u64",
            n,
            dynamic_asks(&r, n, |i| (Box::new(i), i)).await,
            "payload=boxed-u64 allocations=not_measured",
        );
        dynamic_asks(&r, warmup, |_| (Box::new(Msg64([7; 64])), 7)).await;
        row(
            "A3 dyn-ask-boxed-64B",
            n,
            dynamic_asks(&r, n, |_| (Box::new(Msg64([7; 64])), 7)).await,
            "payload=64B allocations=not_measured",
        );
        dynamic_asks(&r, warmup, |_| (Box::new(Msg1K([7; 1024])), 7)).await;
        row(
            "A4 dyn-ask-boxed-1KB",
            n,
            dynamic_asks(&r, n, |_| (Box::new(Msg1K([7; 1024])), 7)).await,
            "payload=1KB allocations=not_measured",
        );
        let big = Msg64K(Box::new([7; 65536]));
        dynamic_asks(&r, large_warmup, |_| (Box::new(big.clone()), 7)).await;
        row(
            "A5 dyn-ask-boxed-64KB",
            large,
            dynamic_asks(&r, large, |_| (Box::new(big.clone()), 7)).await,
            "payload=64KB plus outer Box allocations=not_measured",
        );
    });
}
fn run_b(n: u64, warmup: u64) {
    with_system(|ts| async move {
        let r = ts
            .spawn_root_typed_thread(DynEcho::default(), EmptyConfig)
            .await
            .unwrap();
        dynamic_tells(&r, warmup, |_| (Box::new(1_u64), 1)).await;
        row(
            "B1 dyn-tell-u64",
            n,
            dynamic_tells(&r, n, |_| (Box::new(1_u64), 1)).await,
            "admission_throughput; processing_verified; allocations=not_measured",
        );
        row(
            "B2 dyn-tell-64B",
            n,
            dynamic_tells(&r, n, |_| (Box::new(Msg64([7; 64])), 7)).await,
            "admission_throughput; processing_verified; allocations=not_measured",
        );
        row(
            "B3 dyn-tell-1KB",
            n,
            dynamic_tells(&r, n, |_| (Box::new(Msg1K([7; 1024])), 7)).await,
            "admission_throughput; processing_verified; allocations=not_measured",
        );
    });
}
fn run_c(n: u64, warmup: u64) {
    with_system(|ts| async move {
        let rs = ts
            .spawn_typed::<EchoS, PingS>(EchoS::default(), "/mx/s")
            .await
            .unwrap();
        typed_asks(&rs, warmup, |i| (PingS(i), i)).await;
        row(
            "C1 static-ask-u64",
            n,
            typed_asks(&rs, n, |i| (PingS(i), i)).await,
            &typed_layout::<EchoS>(),
        );
        let rm = ts
            .spawn_typed::<EchoM, PingM>(EchoM::default(), "/mx/m")
            .await
            .unwrap();
        typed_asks(&rm, warmup, |_| (PingM([7; 64]), 7)).await;
        row(
            "C2 static-ask-64B",
            n,
            typed_asks(&rm, n, |_| (PingM([7; 64]), 7)).await,
            &typed_layout::<EchoM>(),
        );
        let rl = ts
            .spawn_typed::<EchoL, PingL>(EchoL::default(), "/mx/l")
            .await
            .unwrap();
        typed_asks(&rl, warmup, |_| (PingL([7; 1024]), 7)).await;
        row(
            "C3 static-ask-1KB",
            n,
            typed_asks(&rl, n, |_| (PingL([7; 1024]), 7)).await,
            &typed_layout::<EchoL>(),
        );
    });
}
fn run_d(per: u64, warmup: u64) {
    with_system(|ts| async move {
        let r = Arc::new(
            ts.spawn_root_typed_thread(DynEcho::default(), EmptyConfig)
                .await
                .unwrap(),
        );
        inline_asks(&r, warmup, 0).await;
        let before = dynamic_snapshot(r.as_ref()).await;
        let start = Instant::now();
        let mut jobs = Vec::new();
        for c in 0..8 {
            let r = r.clone();
            jobs.push(tokio::spawn(async move {
                inline_asks(&r, per, c).await;
            }));
        }
        for job in jobs {
            job.await.unwrap();
        }
        let elapsed = start.elapsed();
        dynamic_snapshot(r.as_ref()).await.verify_delta(
            before,
            per * 8,
            8 * (per * (per - 1) / 2) + 28 * per,
        );
        row("D1 dyn-ask-inline-c8", per * 8, elapsed, "concurrency=8");
        let start = Instant::now();
        let mut jobs = Vec::new();
        for _ in 0..8 {
            let r = r.clone();
            jobs.push(tokio::spawn(async move {
                dynamic_asks(&r, per, |_| (Box::new(Msg1K([7; 1024])), 7)).await;
            }));
        }
        for job in jobs {
            job.await.unwrap();
        }
        row(
            "D2 dyn-ask-boxed-1KB-c8",
            per * 8,
            start.elapsed(),
            "concurrency=8 payload=1KB",
        );
        let rl = Arc::new(
            ts.spawn_typed::<EchoL, PingL>(EchoL::default(), "/mx/c8")
                .await
                .unwrap(),
        );
        typed_asks(&rl, warmup, |_| (PingL([7; 1024]), 7)).await;
        let start = Instant::now();
        let mut jobs = Vec::new();
        for _ in 0..8 {
            let r = rl.clone();
            jobs.push(tokio::spawn(async move {
                typed_asks(&r, per, |_| (PingL([7; 1024]), 7)).await;
            }));
        }
        for job in jobs {
            job.await.unwrap();
        }
        row(
            "D3 static-ask-1KB-c8",
            per * 8,
            start.elapsed(),
            &typed_layout::<EchoL>(),
        );
    });
}
fn run_e(n: u64, warmup: u64) {
    with_system(|ts| async move {
        let r = ts
            .spawn_root_typed_thread(DynEcho::default(), EmptyConfig)
            .await
            .unwrap();
        inline_asks(&r, warmup, 0).await;
        let sample = allocation_batch(
            n,
            n * (n - 1) / 2,
            || dynamic_snapshot(&r),
            || async {
                inline_asks(&r, n, 0).await;
            },
        )
        .await;
        println!("{}", sample.line("E1 dyn-ask-inline"));
        dynamic_asks(&r, warmup, |i| (Box::new(i), i)).await;
        let sample = allocation_batch(
            n,
            n * 7,
            || dynamic_snapshot(&r),
            || async {
                dynamic_asks(&r, n, |_| (Box::new(7_u64), 7)).await;
            },
        )
        .await;
        println!("{}", sample.line("E2 dyn-ask-boxed-u64"));
        let rs = ts
            .spawn_typed::<EchoS, PingS>(EchoS::default(), "/mx/e")
            .await
            .unwrap();
        typed_asks(&rs, warmup, |i| (PingS(i), i)).await;
        let sample = allocation_batch(
            n,
            n * 7,
            || typed_snapshot(&rs),
            || async {
                typed_asks(&rs, n, |_| (PingS(7), 7)).await;
            },
        )
        .await;
        println!("{}", sample.line("E3 static-ask-u64"));
        admit_dynamic(&r, warmup, |_| (Box::new(1_u64), 1)).await;
        let sample = allocation_batch(
            n,
            n,
            || dynamic_snapshot(&r),
            || async {
                assert_eq!(admit_dynamic(&r, n, |_| (Box::new(1_u64), 1)).await, n);
            },
        )
        .await;
        println!("{}", sample.line("E4 dyn-tell-u64"));
        admit_typed(&rs, warmup).await;
        let sample = allocation_batch(
            n,
            n,
            || typed_snapshot(&rs),
            || async {
                admit_typed(&rs, n).await;
            },
        )
        .await;
        println!("{}", sample.line("E5 static-tell"));
    });
}
fn run_f(n: u64, warmup: u64, mixed: u64) {
    with_system(|ts| async move {
        let rm = ts
            .spawn_typed::<EchoMixed, PingS>(EchoMixed::default(), "/mx/mixed")
            .await
            .unwrap();
        typed_asks(&rm, warmup, |i| (PingS(i), i)).await;
        row(
            "F1 static-ask-SMALL-in-MIXED-enum",
            n,
            typed_asks(&rm, n, |i| (PingS(i), i)).await,
            &typed_layout::<EchoMixed>(),
        );
        let rl = rm.ref_for::<PingL>();
        typed_asks(&rl, warmup, |_| (PingL([7; 1024]), 7)).await;
        row(
            "F1b static-ask-LARGE-in-MIXED-enum",
            n,
            typed_asks(&rl, n, |_| (PingL([7; 1024]), 7)).await,
            &typed_layout::<EchoMixed>(),
        );
        let r = ts
            .spawn_root_typed_thread(DynEcho::default(), EmptyConfig)
            .await
            .unwrap();
        dynamic_tells(&r, warmup, |_| (Box::new(1_u64), 1)).await;
        let big = Msg64K(Box::new([7; 65536]));
        let elapsed = dynamic_tells(&r, mixed, |i| {
            if i % 20 == 0 {
                (Box::new(big.clone()), 7)
            } else {
                (Box::new(1_u64), 1)
            }
        })
        .await;
        row(
            "F2 dyn-tell-mixed-95small-5big",
            mixed,
            elapsed,
            "admission_throughput; processing_verified; 95% u64 + 5% 64KB",
        );
    });
}

fn isolated(name: &str, ignored: bool, body: impl FnOnce()) {
    const CHILD: &str = "PARROT_MESSAGE_MATRIX_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        body();
        return;
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, name);
    if ignored {
        command.arg("--ignored");
    }
    let output = command.output().expect("matrix subprocess");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "matrix subprocess failed: {}",
        output.status
    );
}
macro_rules! experiment {
    ($name:ident,$body:expr) => {
        #[test]
        #[ignore = "release-only isolated matrix experiment"]
        fn $name() {
            assert!(
                !cfg!(debug_assertions),
                "matrix timing experiments require --release"
            );
            isolated(stringify!($name), true, || $body);
        }
    };
}
experiment!(matrix_a_dyn_ask_size_ladder, run_a(10_000, 500, 2_000, 200));
experiment!(matrix_b_dyn_tell_size_ladder, run_b(100_000, 1_000));
experiment!(matrix_c_static_ask_size_ladder, run_c(10_000, 500));
experiment!(matrix_d_concurrent_c8, run_d(10_000, 500));
experiment!(matrix_e_alloc_counting, run_e(2_000, 300));
experiment!(matrix_f_combo, run_f(10_000, 500, 20_000));
macro_rules! smoke {
    ($name:ident,$body:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), false, || $body);
        }
    };
}
smoke!(matrix_a_semantics, run_a(8, 2, 4, 2));
smoke!(matrix_b_semantics, run_b(8, 2));
smoke!(matrix_c_semantics, run_c(8, 2));
smoke!(matrix_d_semantics, run_d(4, 2));
smoke!(matrix_e_semantics, run_e(8, 2));
smoke!(matrix_f_semantics, run_f(8, 2, 40));

#[test]
fn timing_rejects_zero_duration() {
    assert!(std::panic::catch_unwind(|| timing_line("invalid", 1, Duration::ZERO, "")).is_err());
}
#[test]
fn allocation_output_has_no_fake_throughput() {
    let line = AllocationSample {
        messages: 4,
        window_calls: 20,
        barrier_control_calls: 3,
        checksum: 4,
    }
    .line("sample");
    assert!(!line.contains("tput=") && !line.contains("wall="));
    assert!(
        line.contains("window_alloc_calls=20")
            && line.contains("barrier_control_calls=3")
            && line.contains("scope=process_including_runtime_and_one_completion_barrier")
    );
}
#[test]
fn snapshot_rejects_incomplete_processing() {
    assert!(
        std::panic::catch_unwind(|| Stats {
            completed: 3,
            checksum: 4
        }
        .verify_delta(Stats::default(), 4, 4))
        .is_err()
    );
}
#[test]
fn snapshot_rejects_wrong_payload_checksum() {
    assert!(
        std::panic::catch_unwind(|| Stats {
            completed: 4,
            checksum: 3
        }
        .verify_delta(Stats::default(), 4, 4))
        .is_err()
    );
}
#[test]
fn stopped_target_rejects_admission() {
    with_system(|ts| async move {
        let r = ts
            .spawn_root_typed_thread(DynEcho::default(), EmptyConfig)
            .await
            .unwrap();
        r.stop().await.unwrap();
        assert!(
            AssertUnwindSafe(admit_dynamic(&r, 4, |_| (Box::new(1_u64), 1)))
                .catch_unwind()
                .await
                .is_err()
        );
    });
}
struct Unblock(Arc<Semaphore>);
impl Drop for Unblock {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}
#[test]
fn dynamic_allocation_waits_for_processing() {
    with_system(|ts| async move {
        let gate = Arc::new(Semaphore::new(0));
        let _unblock = Unblock(gate.clone());
        let r = ts
            .spawn_root_typed_thread(
                DynEcho {
                    stats: Stats::default(),
                    pause: Some(gate.clone()),
                },
                EmptyConfig,
            )
            .await
            .unwrap();
        let admitted = std::cell::Cell::new(false);
        let future = allocation_batch(
            4,
            4,
            || dynamic_snapshot(&r),
            || async {
                admit_dynamic(&r, 4, |_| (Box::new(1_u64), 1)).await;
                admitted.set(true);
            },
        );
        tokio::pin!(future);
        let waiting = std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
            std::task::Poll::Ready(_) => std::task::Poll::Ready(false),
            std::task::Poll::Pending if admitted.get() => std::task::Poll::Ready(true),
            std::task::Poll::Pending => std::task::Poll::Pending,
        })
        .await;
        assert!(waiting, "allocation measurement must wait beyond admission");
        gate.add_permits(1);
        assert_eq!(future.await.messages, 4);
    });
}
#[test]
fn typed_allocation_waits_for_processing() {
    with_system(|ts| async move {
        let gate = Arc::new(Semaphore::new(0));
        let _unblock = Unblock(gate.clone());
        let r = ts
            .spawn_typed::<EchoS, PingS>(
                EchoS {
                    stats: Stats::default(),
                    pause: Some(gate.clone()),
                },
                "/blocked",
            )
            .await
            .unwrap();
        let admitted = std::cell::Cell::new(false);
        let future = allocation_batch(
            4,
            4,
            || typed_snapshot(&r),
            || async {
                admit_typed(&r, 4).await;
                admitted.set(true);
            },
        );
        tokio::pin!(future);
        let waiting = std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
            std::task::Poll::Ready(_) => std::task::Poll::Ready(false),
            std::task::Poll::Pending if admitted.get() => std::task::Poll::Ready(true),
            std::task::Poll::Pending => std::task::Poll::Pending,
        })
        .await;
        assert!(
            waiting,
            "typed allocation measurement must wait beyond admission"
        );
        gate.add_permits(1);
        assert_eq!(future.await.messages, 4);
    });
}
