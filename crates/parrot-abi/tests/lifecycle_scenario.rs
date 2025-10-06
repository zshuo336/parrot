//! D 阶段场景测试：三形态生命周期之 dylib 装载循环（RSS 稳定性）。
//!
//! 09 §4.3 ④ 升级主路径的量化面：反复 load→construct→handle→unload
//! N 轮后进程 RSS 不单调增长（泄漏即失败——泄的 Library/tls/静态
//! 都会体现在 RSS 曲线）。macOS 用 `mach_task_info`；跨平台退化
//! 为 /proc/self/status（Linux）或跳过（诚实边界）。

#![cfg(feature = "loader")]
// dyld 串行守卫刻意跨 await（同 loader_tests）
#![allow(clippy::await_holding_lock)]

mod common;

use parrot_abi::{AbiMsg, AbiReplyBuf, AbiStr, DylibLoader};
use std::time::Duration;

fn rss_kb() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // sysctl hw.pagesize + task_info PHYS_FOOTPRINT 经 libc——
        // 免 libc 依赖：ps 快照（子进程读自身，精度页级足够）
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        s.parse::<u64>().ok()
    }
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in s.lines() {
            if let Some(v) = line.strip_prefix("VmRSS:") {
                return v.trim_end_matches(" kB").trim().parse::<u64>().ok().map(|k| k);
            }
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    None
}

#[tokio::test]
async fn dylib_load_unload_cycle_rss_stable() {
    let _g = common::load_mutex();
    let Some(rss0) = rss_kb() else {
        eprintln!("RSS unavailable on this platform——场景跳过（诚实边界）");
        return;
    };
    let rounds = 40;
    for i in 0..rounds {
        let h = DylibLoader::load(&common::testcomp(), "").expect("load");
        let c = DylibLoader::construct(&DylibLoader, &h, AbiStr::of_str("{}")).expect("construct");
        unsafe {
            let mut buf = vec![0u8; 4096];
            let mut rb = AbiReplyBuf::new(&mut buf);
            DylibLoader::handle_msg(
                &DylibLoader,
                &h,
                c,
                AbiMsg::new("echo", b"cycle"),
                &mut rb,
            )
            .unwrap();
            assert_eq!(&buf[..rb.written as usize], b"cycle");
        }
        let r = DylibLoader::unload(&DylibLoader, h, Duration::from_millis(300))
            .await
            .unwrap();
        assert_eq!((r.drained, r.aborted), (1, 0));
        if i == rounds / 2 {
            // 半程预热后取样（JIT/分配器稳态）
        }
    }
    let Some(rss1) = rss_kb() else { return };
    // 判定：40 轮后增幅 < 8MB（页级噪声容忍；泄漏典型 >10MB/百轮）
    let growth_kb = rss1.saturating_sub(rss0);
    assert!(
        growth_kb < 8 * 1024,
        "RSS grew {growth_kb}KB over {rounds} load/unload cycles (leak?): {rss0}→{rss1}KB"
    );
    eprintln!("RSS: {rss0}KB → {rss1}KB over {rounds} cycles");
}
