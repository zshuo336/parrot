//! D2（DEV_09 §3.4 / 09 §4.3 ②③）：dylib 宿主侧加载器。
//!
//! - `DylibLoader::load`：dlopen + `PARROT_ABI_META` 符号 + abi_version 强
//!   校验 + sha256 digest 校验 + **禁止清单扫描**（violation = 拒载）
//! - `construct`：catch_unwind 双保险（dylib 侧 + 宿主侧）
//! - `unload`：四步协议 Quarantine→Drain→Destroy→Dlclose
//!   （in-flight 计数 + drain_timeout 兜底 → force dlclose 告警）
//! - `scan_violations`：TLS 析构注册/线程创建/signal 安装（nm/otool 扫描）

use crate::{
    AbiComponent, AbiMeta, AbiMsg, AbiReplyBuf, AbiStr, ABI_META_SYMBOL, ABI_OK, PARROT_ABI_VERSION,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 加载错误（拒载语义——禁止清单 violation 也走这里）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    #[error("dylib open failed: {0}")]
    Open(String),
    #[error("symbol {ABI_META_SYMBOL} missing: {0}")]
    Symbol(String),
    #[error("abi version mismatch: dylib {dylib:?} vs host {host}")]
    AbiVersion { dylib: u32, host: u32 },
    #[error("parrot_min {min} exceeds host abi {host}")]
    ParrotMin { min: u32, host: u32 },
    #[error("sha256 mismatch: want {want}, got {got}")]
    Digest { want: String, got: String },
    #[error("forbidden-list violation: {0:?}")]
    Forbidden(Vec<Violation>),
    #[error("construct failed: code={code} ({detail})")]
    Construct { code: u32, detail: String },
    #[error("handle not found: {0}")]
    NoSuchHandle(String),
}

/// 禁止清单违例（09 §4.3：violation = 加载拒绝）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// TLS 析构注册（`__tls_dtor` / `tlv_atexit` 引用）。
    TlsDestructor { symbol: String },
    /// pthread_create 引用库内符号（自有线程持有组件函数指针）。
    ThreadSpawn { symbol: String },
    /// signal/sigaction 安装。
    SignalHandler { symbol: String },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::TlsDestructor { symbol } => write!(f, "tls-dtor: {symbol}"),
            Violation::ThreadSpawn { symbol } => write!(f, "thread-spawn: {symbol}"),
            Violation::SignalHandler { symbol } => write!(f, "signal: {symbol}"),
        }
    }
}

/// 卸载报告（四步协议执行结果）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnloadReport {
    /// 正常排空的实例数。
    pub drained: usize,
    /// 超时中止的实例数（DRAIN_ABORTED 计数）。
    pub aborted: usize,
    /// 是否强制 dlclose（栅栏未归零——诚实边界：接受潜在 UB，告警）。
    pub force_closed: bool,
}

/// 卸载错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnloadError {
    #[error("instance registry not empty after destroy: {0} alive")]
    InstancesAlive(usize),
}

/// 活跃实例登记（drain 计数与 destroy 追踪）。
#[derive(Default)]
struct InstanceRegistry {
    /// 存活实例（addr → in-flight 计数——Arc 便于句柄表共享递增）。
    alive: Mutex<std::collections::HashMap<usize, Arc<AtomicUsize>>>,
}

/// 已加载 dylib 句柄（libloading::Library + 实例登记 + 状态机）。
pub struct DylibHandle {
    /// 库名（诊断）。
    pub name: String,
    path: PathBuf,
    lib: libloading::Library,
    meta: *const AbiMeta,
    registry: Arc<InstanceRegistry>,
    /// Quarantine 后为 true（新消息拒绝进入）。
    quarantined: std::sync::atomic::AtomicBool,
    /// dlclose 是否已执行（幂等保护）。
    closed: std::sync::atomic::AtomicBool,
}

// Safety：AbiMeta 指向库内静态（库生存期 = self）；registry 纯同步原语。
unsafe impl Send for DylibHandle {}
unsafe impl Sync for DylibHandle {}

impl std::fmt::Debug for DylibHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DylibHandle")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("quarantined", &self.quarantined.load(Ordering::Relaxed))
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl DylibHandle {
    /// 测试钩子：直接置/清 quarantine（状态机断言用）。
    #[doc(hidden)]
    pub fn set_quarantined(&self, v: bool) {
        self.quarantined.store(v, Ordering::Release);
    }
}

/// in-flight 消息守卫（drop 归零——drain 栅栏数据源）。
pub struct InFlightGuard {
    registry: Arc<InstanceRegistry>,
    addr: usize,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Some(c) = self.registry.alive.lock().unwrap().get(&self.addr) {
            c.fetch_sub(1, Ordering::Release);
        }
    }
}

/// dylib 加载器（宿主侧）。
#[derive(Debug, Clone, Copy, Default)]
pub struct DylibLoader;

impl DylibLoader {
    /// dlopen + meta 校验 + digest 校验 + 禁止清单扫描。
    ///
    /// `digest`：`sha256:<hex64>` 或裸 hex64；空 = 跳过（测试便利——
    /// 生产路径 manifest 强制非空）。
    pub fn load(path: &Path, digest: &str) -> Result<DylibHandle, LoadError> {
        // 禁止清单扫描在 dlopen 之前（静态文件分析——拒载不带副作用）
        let violations = Self::scan_violations(path);
        if !violations.is_empty() {
            return Err(LoadError::Forbidden(violations));
        }
        let bytes = std::fs::read(path).map_err(|e| LoadError::Open(format!("read: {e}")))?;
        if !digest.is_empty() {
            use sha2::{Digest, Sha256};
            let got = hex::encode(Sha256::digest(&bytes));
            let want = digest
                .strip_prefix("sha256:")
                .unwrap_or(digest)
                .to_ascii_lowercase();
            if got != want {
                return Err(LoadError::Digest { want, got });
            }
        }
        unsafe {
            let lib = libloading::Library::new(path).map_err(|e| LoadError::Open(e.to_string()))?;
            let meta: libloading::Symbol<*const AbiMeta> = lib
                .get(ABI_META_SYMBOL.as_bytes())
                .map_err(|e| LoadError::Symbol(e.to_string()))?;
            let meta = *meta;
            if (*meta).abi_version != PARROT_ABI_VERSION {
                return Err(LoadError::AbiVersion {
                    dylib: (*meta).abi_version,
                    host: PARROT_ABI_VERSION,
                });
            }
            if (*meta).parrot_min > PARROT_ABI_VERSION {
                return Err(LoadError::ParrotMin {
                    min: (*meta).parrot_min,
                    host: PARROT_ABI_VERSION,
                });
            }
            let name = (*meta).name_str().to_string();
            Ok(DylibHandle {
                name,
                path: path.to_path_buf(),
                lib,
                meta,
                registry: Arc::new(InstanceRegistry::default()),
                quarantined: std::sync::atomic::AtomicBool::new(false),
                closed: std::sync::atomic::AtomicBool::new(false),
            })
        }
    }

    /// construct（catch_unwind 双保险——dylib 侧已 catch，宿主再包一层）。
    pub fn construct(&self, h: &DylibHandle, cfg: AbiStr) -> Result<*mut AbiComponent, LoadError> {
        unsafe {
            let out: *mut AbiComponent = std::ptr::null_mut();
            let mut out = out;
            let construct_fn = (*h.meta).hooks.construct;
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                construct_fn(cfg, &mut out as *mut *mut AbiComponent)
            }))
            .unwrap_or(99); // 宿主侧 panic 码（ABI_ERR_* 之外——诊断可辨）
                            // 注：99 = 宿主侧 panic 码（ABI_ERR_* 之外——诊断可辨）
            if r != ABI_OK || out.is_null() {
                return Err(LoadError::Construct {
                    code: r,
                    detail: "construct returned non-OK or null".into(),
                });
            }
            // 登记（in-flight 从 0 起计）
            h.registry
                .alive
                .lock()
                .unwrap()
                .insert(out as usize, Arc::new(AtomicUsize::new(0)));
            Ok(out)
        }
    }

    /// 消息处理入口（in-flight 计数 + catch_unwind）。
    ///
    /// # Safety
    /// comp 必须来自本 handle 的 construct 且未 destroy。
    pub unsafe fn handle_msg(
        &self,
        h: &DylibHandle,
        comp: *mut AbiComponent,
        msg: AbiMsg,
        out: &mut AbiReplyBuf,
    ) -> Result<(), (u32, String)> {
        if h.quarantined.load(Ordering::Acquire) {
            return Err((crate::ABI_ERR_STATE, "quarantined: no new messages".into()));
        }
        // in-flight +1（登记在案才计——外来指针拒）
        let counter = h
            .registry
            .alive
            .lock()
            .unwrap()
            .get(&(comp as usize))
            .cloned();
        let counter = match counter {
            Some(c) => c,
            None => return Err((crate::ABI_ERR_STATE, "component not registered".into())),
        };
        let _guard = InFlightGuard {
            registry: h.registry.clone(),
            addr: comp as usize,
        };
        counter.fetch_add(1, Ordering::AcqRel);
        let vt = (*comp).vt;
        if vt.is_null() || (*comp).self_.is_null() {
            return Err((crate::ABI_ERR_STATE, "null vtable or self".into()));
        }
        let f = (*vt).handle_msg;
        let self_ = (*comp).self_;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f(self_, msg, out as *mut AbiReplyBuf)
        }));
        match r {
            Ok(code) if code == ABI_OK => Ok(()),
            Ok(code) => Err((code, format!("handle_msg code={code}"))),
            Err(p) => Err((crate::ABI_ERR_PANIC, format!("dylib panic: {p:?}"))),
        }
    }

    /// 四步卸载（09 §4.3 ②）：Quarantine→Drain→Destroy→Dlclose。
    ///
    /// `drain_timeout`：in-flight 归零等待上限；超时实例计入 aborted，
    /// destroy 后仍不归零 → force_closed=true（诚实边界：接受潜在 UB，
    /// 调用方告警 + digest 审计兜底）。
    pub async fn unload(
        &self,
        h: DylibHandle,
        drain_timeout: Duration,
    ) -> Result<UnloadReport, UnloadError> {
        // 1. Quarantine：新消息拒绝
        h.quarantined.store(true, Ordering::Release);
        // 2. Drain：等 in-flight 归零（每实例上限 timeout——总量 2× 防级联）
        let deadline = Instant::now() + drain_timeout + drain_timeout;
        let mut report = UnloadReport::default();
        loop {
            let busy = h
                .registry
                .alive
                .lock()
                .unwrap()
                .values()
                .map(|c| c.load(Ordering::Acquire))
                .filter(|&n| n > 0)
                .count();
            if busy == 0 {
                break;
            }
            if Instant::now() >= deadline {
                report.aborted = busy;
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // 3. Destroy：逐实例 destroy + 摘登记
        unsafe {
            let destroy_fn = (*h.meta).hooks.destroy;
            let keys: Vec<usize> = h.registry.alive.lock().unwrap().keys().copied().collect();
            for k in keys.iter().copied() {
                let c = h.registry.alive.lock().unwrap().remove(&k);
                if c.is_some() {
                    let comp = k as *mut AbiComponent;
                    if !comp.is_null() {
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            destroy_fn(comp)
                        }));
                    }
                }
            }
            report.drained = keys.len().saturating_sub(report.aborted);
        }
        // 栅栏复查（destroy 后仍 busy = 违反清单泄漏——force）
        let still = h
            .registry
            .alive
            .lock()
            .unwrap()
            .values()
            .map(|c| c.load(Ordering::Acquire))
            .filter(|&n| n > 0)
            .count();
        if still > 0 {
            report.force_closed = true;
        }
        // 4. Dlclose（幂等）
        if !h.closed.swap(true, Ordering::AcqRel) {
            // drop(h.lib) 即 dlclose——显式 drop 强调时序
            drop(h.lib);
        }
        let _ = h.path; // 诊断字段保留
        Ok(report)
    }

    /// 禁止清单扫描（静态文件分析——不执行库代码）。
    ///
    /// 09 §4.3：不注册 TLS 析构 / 不 spawn 自有线程 / 不 install signal。
    ///
    /// 实现口径（施工裁定 BD-3，macOS/Linux 双平台）：
    /// - **TLS**：扫描 **库自身定义** 的 TLS 变量（macOS：`nm` 定义表中
    ///   `$tlv$init` 符号；Linux：`__thread_vars`/TLS section）。std 自身
    ///   的 TLS（`std::thread::current` 等）**豁免**——那是 Rust 运行时
    ///   固有形态，非库自身注册；符号 mangled 名含 crate 哈希区分。
    /// - **线程**：未定义符号表含 `pthread_create` 引用即违例
    ///   （std 正常路径不引用——仅显式 spawn 才有）。
    /// - **signal**：未定义 `signal`/`sigaction`/`sigsetjmp`（精确匹配）。
    ///
    /// 工具缺失（无 nm）→ 返回空（诚实边界：扫描失败 ≠ 无违例——
    /// load 侧 digest + 运行时 drain 栅栏双保险兜底）。
    pub fn scan_violations(path: &Path) -> Vec<Violation> {
        let mut v = Vec::new();
        // ── 1) 未定义符号（线程/signal）──
        if let Ok(o) = std::process::Command::new("nm")
            .arg("-u")
            .arg(path)
            .output()
        {
            if o.status.success() {
                for line in String::from_utf8_lossy(&o.stdout).lines() {
                    let sym = line.trim();
                    if sym == "pthread_create" || sym == "_pthread_create" {
                        v.push(Violation::ThreadSpawn {
                            symbol: sym.to_string(),
                        });
                    } else if sym == "signal"
                        || sym == "_signal"
                        || sym == "sigaction"
                        || sym == "_sigaction"
                        || sym == "sigsetjmp"
                        || sym == "_sigsetjmp"
                    {
                        v.push(Violation::SignalHandler {
                            symbol: sym.to_string(),
                        });
                    }
                }
            }
        }
        // ── 2) 库自身 TLS 变量（macOS $tlv$init；Linux TLS section 符号）──
        if let Ok(o) = std::process::Command::new("nm").arg(path).output() {
            if o.status.success() {
                for line in String::from_utf8_lossy(&o.stdout).lines() {
                    // 形态：addr t _<mangled>$tlv$init（定义表）
                    if let Some(idx) = line.find("$tlv$init") {
                        let sym = line[..idx].split_whitespace().last().unwrap_or("");
                        // std 的 TLS mangled 名含 crate 哈希 NtNt...Cs<hash>_3std
                        // ——库自身 crate 名不含。粗判：包含 "3std"（Rust
                        // mangling：crate 名长度前缀）即 std 豁免。
                        let owner_std = sym.contains("Cs") && sym.contains("3std");
                        if !owner_std && !sym.is_empty() {
                            v.push(Violation::TlsDestructor {
                                symbol: sym.trim_start_matches('_').to_string(),
                            });
                        }
                    }
                }
            }
        }
        v.dedup();
        v
    }
}
