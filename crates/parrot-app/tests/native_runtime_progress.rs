#![cfg(all(feature = "dylib", target_os = "linux", target_env = "gnu"))]

// Public runtime progress contracts while an independent C finalizer owns the
// glibc loader lock. Each case has an external, reaped subprocess watchdog.
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

#[repr(C)]
struct AbiStr {
    ptr: *const u8,
    len: u32,
}
#[repr(C)]
struct Hooks {
    construct: unsafe extern "C" fn(AbiStr, *mut *mut std::ffi::c_void) -> u32,
    destroy: unsafe extern "C" fn(*mut std::ffi::c_void),
}
#[repr(C)]
struct Meta {
    abi: u32,
    min: u32,
    name: *const u8,
    len: u32,
    hooks: Hooks,
}

fn wait(predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "probe phase did not begin");
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct ReleaseStart(Arc<(Mutex<bool>, Condvar)>);
impl Drop for ReleaseStart {
    fn drop(&mut self) {
        *self.0 .0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.0 .1.notify_all();
    }
}
struct ReleaseNative(Arc<[AtomicU64; 9]>);
impl Drop for ReleaseNative {
    fn drop(&mut self) {
        self.0[6].store(1, Ordering::SeqCst);
    }
}

fn exercise(mode: &str, image: &std::path::Path) {
    let worker = mode == "worker";
    let current = mode == "current";
    let synchronous = matches!(
        mode,
        "oneshot" | "bounded-recv" | "unbounded-recv" | "bounded-send" | "shutdown"
    );
    assert!(worker || current || mode == "caller" || synchronous, "mode");
    let start = Arc::new((Mutex::new(false), Condvar::new()));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let mut builder = if !current {
        let mut b = tokio::runtime::Builder::new_multi_thread();
        b.worker_threads(2);
        let start = start.clone();
        b.on_thread_start(move || {
            if !worker {
                // Isolate a cold block_on caller from the separate worker
                // defect: these workers intentionally initialize their parker.
                let mutex = parking_lot::Mutex::new(());
                parking_lot::Condvar::new().wait_for(&mut mutex.lock(), Duration::ZERO);
            }
            ready_tx.send(()).unwrap();
            if worker {
                let mut open = start.0.lock().unwrap();
                while !*open {
                    open = start.1.wait(open).unwrap();
                }
            }
        });
        b
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    let runtime = builder.enable_all().build().unwrap();
    let _release_start = ReleaseStart(start.clone());
    if !current {
        for _ in 0..2 {
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }
    if worker {
        // Enter the caller before native retirement. This does not park on
        // the original runtime, but gives the candidate its normal entry point.
        runtime.block_on(async {});
    }
    if synchronous {
        // A ready receive initializes Tokio's CURRENT_PARKER without parking
        // on the old implementation. The caller never enters a runtime here.
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(()).unwrap();
        rx.blocking_recv().unwrap();
    }
    let slots: Arc<[AtomicU64; 9]> = Arc::new(std::array::from_fn(|_| AtomicU64::new(0)));
    let _release_native = ReleaseNative(slots.clone());
    slots[1].store(1, Ordering::SeqCst);
    // SAFETY: the independently compiled C fixture uses exactly this ABI and
    // only touches the owned AtomicU64 allocation, retained until join below.
    let library = unsafe { libloading::Library::new(image).unwrap() };
    let cfg = format!("probe_address = {}\n", slots.as_ptr() as usize);
    unsafe {
        let meta = library.get::<*const Meta>(b"PARROT_ABI_META\0").unwrap();
        let meta = &**meta;
        assert_eq!((meta.abi, meta.min), (1, 1));
        let mut component = std::ptr::null_mut();
        assert_eq!(
            (meta.hooks.construct)(
                AbiStr {
                    ptr: cfg.as_ptr(),
                    len: cfg.len() as u32
                },
                &mut component
            ),
            0
        );
        (meta.hooks.destroy)(component);
    }
    let unload_start = Arc::new(AtomicBool::new(false));
    let close_signal = unload_start.clone();
    let unload_slots = slots.clone();
    let closer = std::thread::spawn(move || {
        wait(|| close_signal.load(Ordering::SeqCst));
        drop(library);
        drop(unload_slots);
    });
    let timed_out = Arc::new(AtomicBool::new(false));
    let timeout = timed_out.clone();
    let timeout_slots = slots.clone();
    let watchdog_ready = Arc::new(AtomicBool::new(false));
    let ready = watchdog_ready.clone();
    let watchdog = std::thread::spawn(move || {
        ready.store(true, Ordering::SeqCst);
        wait(|| timeout_slots[5].load(Ordering::SeqCst) == 1);
        let end = Instant::now() + Duration::from_secs(3);
        while timeout_slots[6].load(Ordering::SeqCst) == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        if timeout_slots[6].load(Ordering::SeqCst) == 0 {
            timeout.store(true, Ordering::SeqCst);
            timeout_slots[6].store(1, Ordering::SeqCst);
        }
    });
    // OS thread creation can return before Rust has initialized the child.
    // The independent watchdog must enter its body before any loader lock is held.
    wait(|| watchdog_ready.load(Ordering::SeqCst));
    let begin_unload = || {
        unload_start.store(true, Ordering::SeqCst);
        wait(|| slots[5].load(Ordering::SeqCst) == 1);
        *start.0.lock().unwrap() = true;
        start.1.notify_all();
    };
    let (elapsed, completed) = if synchronous {
        let done = Arc::new(AtomicBool::new(false));
        let finished = done.clone();
        let operation: Box<dyn FnOnce()> = match mode {
            "oneshot" => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let entered = slots.clone();
                runtime.spawn(async move {
                    while entered[5].load(Ordering::SeqCst) == 0 {
                        tokio::task::yield_now().await;
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    tx.send(42).unwrap();
                    finished.store(true, Ordering::SeqCst);
                });
                Box::new(move || assert_eq!(rx.blocking_recv().unwrap(), 42))
            }
            "bounded-recv" => {
                let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                let entered = slots.clone();
                runtime.spawn(async move {
                    while entered[5].load(Ordering::SeqCst) == 0 {
                        tokio::task::yield_now().await;
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    tx.send(42).await.unwrap();
                    finished.store(true, Ordering::SeqCst);
                });
                Box::new(move || assert_eq!(rx.blocking_recv(), Some(42)))
            }
            "unbounded-recv" => {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let entered = slots.clone();
                runtime.spawn(async move {
                    while entered[5].load(Ordering::SeqCst) == 0 {
                        tokio::task::yield_now().await;
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    tx.send(42).unwrap();
                    finished.store(true, Ordering::SeqCst);
                });
                Box::new(move || assert_eq!(rx.blocking_recv(), Some(42)))
            }
            "bounded-send" => {
                let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                tx.try_send(41).unwrap();
                let entered = slots.clone();
                runtime.spawn(async move {
                    while entered[5].load(Ordering::SeqCst) == 0 {
                        tokio::task::yield_now().await;
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    assert_eq!(rx.recv().await, Some(41));
                    assert_eq!(rx.recv().await, Some(42));
                    finished.store(true, Ordering::SeqCst);
                });
                Box::new(move || tx.blocking_send(42).unwrap())
            }
            "shutdown" => {
                let entered = Arc::new(AtomicBool::new(false));
                let running = entered.clone();
                let retained = slots.clone();
                runtime.spawn_blocking(move || {
                    running.store(true, Ordering::SeqCst);
                    wait(|| retained[6].load(Ordering::SeqCst) != 0);
                    finished.store(true, Ordering::SeqCst);
                });
                wait(|| entered.load(Ordering::SeqCst));
                Box::new(move || runtime.shutdown_timeout(Duration::from_millis(150)))
            }
            _ => unreachable!(),
        };
        begin_unload();
        let started = Instant::now();
        operation();
        let result = (started.elapsed(), slots[7].load(Ordering::SeqCst));
        if mode == "shutdown" {
            assert!(
                result.0 >= Duration::from_millis(150),
                "shutdown skipped its pending worker"
            );
            assert!(
                !done.load(Ordering::SeqCst),
                "shutdown returned after the blocked worker retired"
            );
        }
        slots[6].store(1, Ordering::SeqCst);
        wait(|| done.load(Ordering::SeqCst));
        result
    } else {
        if worker {
            begin_unload();
        }
        runtime.block_on(async {
            if !worker {
                begin_unload();
            }
            let started = Instant::now();
            tokio::time::sleep(Duration::from_millis(150)).await;
            let result = (started.elapsed(), slots[7].load(Ordering::SeqCst));
            slots[6].store(1, Ordering::SeqCst);
            result
        })
    };
    closer.join().unwrap();
    watchdog.join().unwrap();
    let expired = timed_out.load(Ordering::SeqCst);
    println!(
        "mode={} timer_ms={} unload_already_completed={} watchdog={expired}",
        mode,
        elapsed.as_millis(),
        completed
    );
    assert!(
        !expired,
        "independent watchdog had to release the native finalizer"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "150ms timer lost runtime progress"
    );
    assert_eq!(
        completed, 0,
        "timer must complete while finalizer is still gated"
    );
    assert_eq!(slots[7].load(Ordering::SeqCst), 1);
    assert_eq!(
        slots[8].load(Ordering::SeqCst),
        0,
        "original fixture watchdog must not fire"
    );
}

fn contract(name: &str, mode: &str) {
    const CHILD: &str = "PARROT_NATIVE_RUNTIME_PROGRESS";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        let root = tempfile::tempdir().unwrap();
        let image = root.path().join("finalizer.so");
        let source =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_unload.c");
        let built = std::process::Command::new("cc")
            .args([
                "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC",
            ])
            .arg(source)
            .arg("-o")
            .arg(&image)
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "C fixture: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        exercise(mode, &image);
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
            .env(CHILD, name)
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            let text = std::fs::read_to_string(log.path()).unwrap();
            assert!(status.success(), "{name}: {status}\n{text}");
            assert!(
                text.contains("test result: ok. 1 passed; 0 failed;"),
                "{text}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "native progress child stalled: {}",
            std::fs::read_to_string(log.path()).unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn native_finalizer_keeps_cold_worker_timers_live() {
    contract("native_finalizer_keeps_cold_worker_timers_live", "worker");
}
#[test]
fn native_finalizer_keeps_cold_block_on_caller_live() {
    contract("native_finalizer_keeps_cold_block_on_caller_live", "caller");
}
#[test]
fn native_finalizer_keeps_current_thread_timers_live() {
    contract(
        "native_finalizer_keeps_current_thread_timers_live",
        "current",
    );
}

#[test]
fn native_finalizer_keeps_blocking_oneshot_live() {
    contract("native_finalizer_keeps_blocking_oneshot_live", "oneshot");
}
#[test]
fn native_finalizer_keeps_blocking_bounded_receive_live() {
    contract(
        "native_finalizer_keeps_blocking_bounded_receive_live",
        "bounded-recv",
    );
}
#[test]
fn native_finalizer_keeps_blocking_unbounded_receive_live() {
    contract(
        "native_finalizer_keeps_blocking_unbounded_receive_live",
        "unbounded-recv",
    );
}
#[test]
fn native_finalizer_keeps_blocking_bounded_send_live() {
    contract(
        "native_finalizer_keeps_blocking_bounded_send_live",
        "bounded-send",
    );
}
#[test]
fn native_finalizer_keeps_shutdown_timeout_live() {
    contract("native_finalizer_keeps_shutdown_timeout_live", "shutdown");
}
