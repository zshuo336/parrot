//! Repeated native load/dispatch/drain/unload behavior. RSS is a separate,
//! explicit measurement: allocator noise cannot prove instance destruction.
#![cfg(feature = "loader")]
// The fixture library is serialized across the current-thread runtime.
#![allow(clippy::await_holding_lock)]
mod common;

use parrot_abi::{AbiMsg, AbiReplyBuf, DylibLoader};
use std::time::Duration;

async fn cycles(rounds: usize) {
    for _ in 0..rounds {
        let h = unsafe { DylibLoader::load(&common::testcomp(), "") }.expect("load");
        let c = DylibLoader.construct(&h, "{}").expect("construct");
        unsafe {
            let mut buf = [0u8; 64];
            let mut rb = AbiReplyBuf::new(&mut buf);
            DylibLoader
                .handle_msg(&h, c, AbiMsg::new("echo", b"cycle"), &mut rb)
                .unwrap();
            assert_eq!(rb.written_slice(), b"cycle");
            DylibLoader
                .handle_msg(&h, c, AbiMsg::new("msgs", &[]), &mut rb)
                .unwrap();
            assert_eq!(
                rb.written_slice(),
                &2u64.to_le_bytes(),
                "fresh instance per cycle"
            );
        }
        let report = DylibLoader
            .unload(h, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(
            (report.drained, report.aborted, report.force_closed),
            (1, 0, false)
        );
    }
}

#[tokio::test]
async fn repeated_load_dispatch_unload_has_fresh_state() {
    let _lock = common::load_mutex();
    cycles(40).await;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rss_kb() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .expect("RSS measurement requires ps");
        assert!(output.status.success(), "ps failed");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("numeric RSS")
    }
    #[cfg(target_os = "linux")]
    {
        let status =
            std::fs::read_to_string("/proc/self/status").expect("RSS measurement requires procfs");
        status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .expect("VmRSS present")
            .trim()
            .trim_end_matches(" kB")
            .trim()
            .parse()
            .expect("numeric RSS")
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
#[ignore = "RSS observation: run with --release --ignored --nocapture; no controlled leak-rate baseline"]
async fn native_cycle_rss_report() {
    let _lock = common::load_mutex();
    cycles(20).await;
    let before = rss_kb();
    cycles(200).await;
    let after = rss_kb();
    eprintln!("Native cycles after warmup: {before} -> {after} KiB RSS, 200 cycles. This is not proof of leak freedom.");
}
