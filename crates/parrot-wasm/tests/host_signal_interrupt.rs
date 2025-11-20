#![cfg(all(target_os = "macos", feature = "runtime"))]

use parrot_wasm::{WasmConfig, WasmRuntime};
use std::{
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

static SIGNALS: AtomicUsize = AtomicUsize::new(0);
extern "C" fn observe_signal(_: libc::c_int) {
    SIGNALS.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" {
    #[link_name = "mach_task_self_"]
    static TASK_SELF: libc::mach_port_t;

    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

fn threads() -> Vec<libc::pthread_t> {
    // task_threads returns owned send rights and a VM-allocated array.
    unsafe {
        let task = TASK_SELF;
        let mut ports = std::ptr::null_mut();
        let mut count = 0;
        assert_eq!(libc::task_threads(task, &mut ports, &mut count), 0);
        let rights = std::slice::from_raw_parts(ports, count as usize);
        let mut threads = Vec::new();
        for &port in rights {
            let thread = libc::pthread_from_mach_thread_np(port);
            if thread != 0 {
                threads.push(thread);
            }
            assert_eq!(mach_port_deallocate(task, port), 0);
        }
        assert_eq!(
            libc::vm_deallocate(
                task,
                ports as libc::vm_address_t,
                std::mem::size_of_val(rights) as libc::vm_size_t,
            ),
            0
        );
        threads
    }
}

fn worker(interrupt: bool) {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = observe_signal as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        assert_eq!(
            libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()),
            0
        );
        let mut signals = std::mem::zeroed();
        libc::sigemptyset(&mut signals);
        libc::sigaddset(&mut signals, libc::SIGUSR1);
        assert_eq!(
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut()),
            0
        );
    }
    let before = threads();
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
    let handlers: Vec<_> = threads()
        .into_iter()
        .filter(|thread| !before.contains(thread))
        .collect();
    assert!(
        !handlers.is_empty(),
        "engine must create its Mach exception handler"
    );
    let module = wasmtime::Module::new(runtime.engine(),
        "(module (func (export \"healthy\") (result i32) i32.const 42) (func (export \"trap\") unreachable))").unwrap();
    let mut store = wasmtime::Store::new(runtime.engine(), ());
    store.set_fuel(100_000).unwrap();
    store.set_epoch_deadline(1);
    let instance = wasmtime::Instance::new(&mut store, &module, &[]).unwrap();
    let healthy = instance
        .get_typed_func::<(), i32>(&mut store, "healthy")
        .unwrap();
    let trap = instance
        .get_typed_func::<(), ()>(&mut store, "trap")
        .unwrap();
    for round in 0..20 {
        assert_eq!(healthy.call(&mut store, ()).unwrap(), 42);
        let error = trap.call(&mut store, ()).unwrap_err();
        assert_eq!(
            error.downcast_ref::<wasmtime::Trap>(),
            Some(&wasmtime::Trap::UnreachableCodeReached)
        );
        if interrupt {
            let expected = (round + 1) * handlers.len();
            for &thread in &handlers {
                assert_eq!(unsafe { libc::pthread_kill(thread, libc::SIGUSR1) }, 0);
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while SIGNALS.load(Ordering::Relaxed) < expected {
                assert!(
                    Instant::now() < deadline,
                    "every targeted handler must receive the signal"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert_eq!(healthy.call(&mut store, ()).unwrap(), 42);
    assert_eq!(
        SIGNALS.load(Ordering::Relaxed),
        if interrupt { 20 * handlers.len() } else { 0 }
    );
}

fn isolated(name: &str, interrupt: bool) {
    const KEY: &str = "PARROT_WASM_SIGNAL_WORKER";
    if std::env::var(KEY).as_deref() == Ok(name) {
        worker(interrupt);
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(KEY, name)
        .env("RUST_BACKTRACE", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Wasm signal worker exceeded its deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success()
            && stdout.contains("running 1 test")
            && stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "Wasm signal contract must execute once: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn healthy_guest_recovers_from_traps() {
    isolated("healthy_guest_recovers_from_traps", false);
}

#[test]
fn host_signal_does_not_abort_guest_trap_handling() {
    isolated("host_signal_does_not_abort_guest_trap_handling", true);
}
