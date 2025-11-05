//! D2（DEV_09 §3.4 / 09 §4.3 ②③）：dylib 宿主侧加载器。
//!
//! - `DylibLoader::load`：dlopen + `PARROT_ABI_META` 符号 + abi_version 强
//!   校验 + sha256 digest 校验 + **禁止清单扫描**（violation = 拒载）
//! - `construct`：借用有效配置字符串；插件必须在自身内部捕获panic
//! - `unload`：四步协议 Quarantine→Drain→Destroy→Dlclose
//!   （独立线程持有清理；超时/取消等待不会卸载仍在执行的代码）
//! - `scan_violations`：直接解析ELF/Mach-O；扫描错误不得当作无违例

use crate::{
    AbiComponent, AbiMeta, AbiMsg, AbiReplyBuf, AbiStr, AbiTraceParentV1, AbiTraceV1,
    ABI_META_SYMBOL, ABI_OK, ABI_TRACE_V1_SYMBOL, PARROT_ABI_VERSION,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod artifact;
pub use artifact::NativeArtifact;

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
    #[error("native artifact scan failed: {0}")]
    Scan(String),
    #[error("forbidden-list violation: {0:?}")]
    Forbidden(Vec<Violation>),
    #[error("construct failed: code={code} ({detail})")]
    Construct { code: u32, detail: String },
    #[error("handle not found: {0}")]
    NoSuchHandle(String),
    #[error("invalid native artifact limits: {0}")]
    InvalidConfig(String),
    #[error("native artifact resource limit: {0}")]
    ResourceLimit(String),
}

/// File and static-analysis budgets, independent of native runtime allocations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeArtifactLimits {
    /// Maximum artifact bytes read/hashed/scanned; default 64 MiB.
    pub max_file_bytes: usize,
    /// Maximum symbol entries visited across static and dynamic tables.
    pub max_scan_symbols: usize,
    /// Maximum sum of visited raw symbol-name lengths, including duplicates.
    pub max_scan_name_bytes: usize,
    /// Maximum object sections inspected.
    pub max_scan_sections: usize,
}
impl Default for NativeArtifactLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 64 * 1024 * 1024,
            max_scan_symbols: 262_144,
            max_scan_name_bytes: 32 * 1024 * 1024,
            max_scan_sections: 16_384,
        }
    }
}
impl NativeArtifactLimits {
    fn validate(self) -> Result<(), LoadError> {
        if self.max_file_bytes == 0 || self.max_file_bytes >= isize::MAX as usize {
            return Err(LoadError::InvalidConfig("max_file_bytes must be positive and leave room for a detection byte within isize::MAX".into()));
        }
        if [
            self.max_scan_symbols,
            self.max_scan_name_bytes,
            self.max_scan_sections,
        ]
        .iter()
        .any(|&limit| limit == 0 || limit >= isize::MAX as usize)
        {
            return Err(LoadError::InvalidConfig(
                "scan budgets must be positive and below isize::MAX".into(),
            ));
        }
        Ok(())
    }
    fn read(self, reader: impl std::io::Read) -> Result<Vec<u8>, LoadError> {
        use std::io::Read;
        self.validate()?;
        let mut bytes = Vec::new();
        reader
            .take(self.max_file_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| LoadError::Open(format!("read: {e}")))?;
        if bytes.len() > self.max_file_bytes {
            return Err(self.exceeded());
        }
        Ok(bytes)
    }
    fn exceeded(self) -> LoadError {
        LoadError::ResourceLimit(format!("artifact exceeds {} bytes", self.max_file_bytes))
    }
}

fn read_artifact(path: &Path, limits: NativeArtifactLimits) -> Result<Vec<u8>, LoadError> {
    limits.validate()?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Opening a FIFO must not wait for a writer before we can reject its
        // file type. Validate the opened descriptor, not a racy path stat.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    let file = options
        .open(path)
        .map_err(|e| LoadError::Open(format!("open: {e}")))?;
    let metadata = file
        .metadata()
        .map_err(|e| LoadError::Open(format!("metadata: {e}")))?;
    if !metadata.is_file() {
        return Err(LoadError::Open(
            "expected a regular native artifact file".into(),
        ));
    }
    if metadata.len() > limits.max_file_bytes as u64 {
        return Err(limits.exceeded());
    }
    // The read bound also applies if the file grows after metadata inspection.
    limits.read(file)
}

/// 禁止清单违例（09 §4.3：violation = 加载拒绝）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// 组件TLS存储（保守拒绝；静态符号不证明已经注册析构）。
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
    /// Reserved for compatibility; native hooks cannot be safely aborted (always zero).
    pub aborted: usize,
    /// Always false: executing native code is never force-unmapped.
    pub force_closed: bool,
}

/// 卸载错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnloadError {
    #[error("refusing native cleanup: {0} instances have active calls; library retained")]
    InstancesAlive(usize),
    #[error("native drain failed with codes {codes:?}; all instances destroyed: {report:?}")]
    DrainFailed {
        codes: Vec<u32>,
        report: UnloadReport,
    },
    #[error("native cleanup deadline elapsed; owned cleanup continues in the background")]
    TimedOut,
    #[error("native cleanup timeout exceeds the monotonic clock range; cleanup continues")]
    InvalidTimeout,
    #[error("waiting for native cleanup requires a Tokio runtime with time enabled")]
    NoRuntime,
    #[error("could not start native cleanup worker: {0}")]
    WorkerStart(String),
    #[error("native cleanup worker exited without a result")]
    WorkerLost,
}

/// 活跃实例登记（drain 计数与 destroy 追踪）。
#[derive(Default)]
struct InstanceRegistry {
    /// 存活实例（addr → in-flight 计数——Arc 便于句柄表共享递增）。
    alive: Mutex<std::collections::HashMap<usize, Arc<AtomicUsize>>>,
    drained: Mutex<std::collections::HashMap<usize, u32>>,
}

/// 已加载 dylib 句柄（libloading::Library + 实例登记 + 状态机）。
pub struct DylibHandle {
    /// 库名（诊断）。
    pub name: String,
    path: PathBuf,
    lib: Option<libloading::Library>,
    staged: Option<Arc<artifact::StagedFile>>,
    meta: *const AbiMeta,
    trace_v1: Option<AbiTraceV1>,
    registry: Arc<InstanceRegistry>,
    /// Quarantine 后为 true（新消息拒绝进入）。
    quarantined: std::sync::atomic::AtomicBool,
    /// Cleanup has started (idempotence guard), not proof of completed dlclose.
    closed: std::sync::atomic::AtomicBool,
}

// Safety: load requires immutable metadata and thread-independent native hooks;
// the library outlives all instances and every native call. Message exclusivity
// remains the responsibility of the unsafe handle_msg caller.
unsafe impl Send for DylibHandle {}
unsafe impl Sync for DylibHandle {}

impl std::fmt::Debug for DylibHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DylibHandle")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("quarantined", &self.quarantined.load(Ordering::Relaxed))
            .field("cleanup_started", &self.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl DylibHandle {
    /// Whether this library accepts per-message causal parents. A false result
    /// means messages use ABI v1 without guest-visible causal metadata.
    pub fn supports_trace_v1(&self) -> bool {
        self.trace_v1.is_some()
    }
    /// 测试钩子：直接置/清 quarantine（状态机断言用）。
    #[doc(hidden)]
    pub fn set_quarantined(&self, v: bool) {
        self.quarantined.store(v, Ordering::Release);
    }
}

impl DylibHandle {
    /// Synchronous fallback shared by Drop and the owned unload worker.
    fn cleanup(&mut self) -> Result<UnloadReport, UnloadError> {
        self.quarantined.store(true, Ordering::Release);
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(UnloadReport::default());
        }
        let mut registry = self
            .registry
            .alive
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let busy = registry
            .values()
            .filter(|c| c.load(Ordering::Acquire) != 0)
            .count();
        if busy != 0 {
            // Consuming ownership normally rules this out. If the invariant is
            // violated, pin both code and instances; never destroy active state.
            if let Some(lib) = self.lib.take() {
                std::mem::forget(lib);
            }
            if let Some(staged) = self.staged.take() {
                std::mem::forget(staged);
            }
            return Err(UnloadError::InstancesAlive(busy));
        }
        let instances: Vec<_> = registry.drain().map(|(addr, _)| addr).collect();
        drop(registry);
        let drained = std::mem::take(
            &mut *self
                .registry
                .drained
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        );
        let mut report = UnloadReport::default();
        let mut codes = Vec::new();
        // SAFETY: load's trust contract guarantees valid unique instances,
        // non-unwinding hooks, and destroy validity even after a drain error.
        unsafe {
            for &addr in &instances {
                let comp = addr as *mut AbiComponent;
                let code = drained
                    .get(&addr)
                    .copied()
                    .unwrap_or_else(|| ((*(*comp).vt).on_drain)((*comp).self_));
                if code == ABI_OK {
                    report.drained += 1;
                } else {
                    codes.push(code);
                }
            }
            // Complete the drain phase before destroying any instance: another
            // instance's drain hook may still need shared library-owned state.
            for addr in instances {
                ((*self.meta).hooks.destroy)(addr as *mut AbiComponent);
            }
        }
        // Release this handle after destroying its instances. Finalizers invoked
        // by this dlclose finish before it returns; the platform may retain the
        // image for other handles or runtime state. This is not proof of unmapping.
        drop(self.lib.take());
        drop(self.staged.take());
        if codes.is_empty() {
            Ok(report)
        } else {
            Err(UnloadError::DrainFailed { codes, report })
        }
    }
}

impl Drop for DylibHandle {
    fn drop(&mut self) {
        // Drop cannot return drain failures. Call unload to observe them.
        // Hooks may block: applications requiring a deadline must use unload.
        let _ = self.cleanup();
    }
}

/// in-flight 消息守卫（drop 归零——drain 栅栏数据源）。
pub struct InFlightGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Release);
    }
}

/// Defensively keep lookup and increment in the same registry critical section.
/// Normal callers cannot race cleanup: it requires exclusive handle ownership,
/// while native calls borrow that handle. If an internal invariant is violated,
/// cleanup observes the active counter and permanently retains the native image
/// instead of destroying active state.
fn acquire_inflight(registry: &InstanceRegistry, addr: usize) -> Option<InFlightGuard> {
    let entries = registry.alive.lock().unwrap();
    let counter = entries.get(&addr).cloned()?;
    counter.fetch_add(1, Ordering::AcqRel);
    Some(InFlightGuard { counter })
}

/// dylib 加载器（宿主侧）。
#[derive(Debug, Clone, Copy, Default)]
pub struct DylibLoader;

impl DylibLoader {
    /// dlopen + meta 校验 + digest 校验 + 禁止清单扫描。
    ///
    /// `digest`：`sha256:<hex64>` 或裸 hex64；空 = 跳过（测试便利——
    /// 生产路径 manifest 强制非空）。
    /// # Safety
    /// Loading runs native initializers before ABI validation. The caller must
    /// trust this artifact and its dependencies, keep the path unchanged during
    /// loading, and guarantee valid ABI metadata and hooks. Hooks must not unwind
    /// across extern C, retain borrowed call buffers, or leave callbacks/threads
    /// referencing the library after unload. Metadata must remain valid for the
    /// library lifetime; constructors must return unique owned instances on
    /// success and clean their allocations on failure. They must be
    /// safe to call concurrently and from any host thread. All instances must
    /// have valid vtables and support moving between host threads. Drain and
    /// destroy run exclusively per instance, possibly on a cleanup worker;
    /// destroy must remain valid after a non-OK drain. Hooks must not require
    /// the loading thread or a Tokio runtime. If exported, `PARROT_ABI_TRACE_V1`
    /// must be immutable valid `AbiTraceV1` data for the library lifetime, and
    /// its message hook has all the same obligations as the original vtable.
    /// Static scanning is
    /// not a sandbox and a matching digest does not establish these guarantees.
    ///
    /// ```compile_fail
    /// use parrot_abi::DylibLoader;
    /// // Native code loading requires an explicit unsafe trust decision.
    /// let _ = DylibLoader::load(std::path::Path::new("plugin.so"), "");
    /// ```
    pub unsafe fn load(path: &Path, digest: &str) -> Result<DylibHandle, LoadError> {
        unsafe { Self::load_with_limits(path, digest, NativeArtifactLimits::default()) }
    }

    /// Load a trusted regular artifact with an explicit input byte budget.
    /// The budget limits file reads, not mapped-library/dependency/native-heap size.
    /// # Safety
    /// All trust, path stability, ABI and hook obligations of [`Self::load`] apply.
    pub unsafe fn load_with_limits(
        path: &Path,
        digest: &str,
        limits: NativeArtifactLimits,
    ) -> Result<DylibHandle, LoadError> {
        let bytes = read_artifact(path, limits)?;
        let violations = scan_bytes(&bytes, limits)?;
        if !violations.is_empty() {
            return Err(LoadError::Forbidden(violations));
        }
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
        unsafe { Self::open(path, None) }
    }

    unsafe fn open(
        path: &Path,
        staged: Option<Arc<artifact::StagedFile>>,
    ) -> Result<DylibHandle, LoadError> {
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
            let trace_v1 = lib
                .get::<*const AbiTraceV1>(ABI_TRACE_V1_SYMBOL.as_bytes())
                .ok()
                .filter(|extension| same_symbol_image(meta.cast(), (**extension).cast()))
                .map(|extension| **extension);
            Ok(DylibHandle {
                name,
                path: path.to_path_buf(),
                lib: Some(lib),
                staged,
                meta,
                trace_v1,
                registry: Arc::new(InstanceRegistry::default()),
                quarantined: std::sync::atomic::AtomicBool::new(false),
                closed: std::sync::atomic::AtomicBool::new(false),
            })
        }
    }

    /// Construct using a borrowed configuration valid throughout the native call.
    /// The trusted plugin must catch panics internally: unwinding through an
    /// extern C boundary aborts before a host catch_unwind can recover it.
    ///
    /// ```compile_fail
    /// use parrot_abi::{AbiStr, DylibHandle, DylibLoader};
    /// fn forged_config(handle: &DylibHandle) {
    ///     let raw = AbiStr { ptr: std::ptr::null(), len: 100 };
    ///     let _ = DylibLoader.construct(handle, raw);
    /// }
    /// ```
    pub fn construct(&self, h: &DylibHandle, cfg: &str) -> Result<*mut AbiComponent, LoadError> {
        if h.quarantined.load(Ordering::Acquire) {
            return Err(LoadError::Construct {
                code: crate::ABI_ERR_STATE,
                detail: "quarantined: no new instances".into(),
            });
        }
        let len = u32::try_from(cfg.len()).map_err(|_| LoadError::Construct {
            code: crate::ABI_ERR_OVERFLOW,
            detail: "configuration exceeds ABI length limit".into(),
        })?;
        // Borrowed input remains alive until the synchronous native call returns.
        let cfg = AbiStr {
            ptr: cfg.as_ptr(),
            len,
        };
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
    /// `comp` must be a live instance constructed by this handle. The caller
    /// must ensure exclusive access to its state throughout this synchronous
    /// call. Message pointers must remain readable for their stated lengths
    /// (type_key is UTF-8); the output must point to exclusively writable
    /// capacity bytes, disjoint from input/state, throughout the call. Native
    /// code must honor these bounds and must not retain any borrowed buffers.
    pub unsafe fn handle_msg(
        &self,
        h: &DylibHandle,
        comp: *mut AbiComponent,
        msg: AbiMsg,
        out: &mut AbiReplyBuf,
    ) -> Result<(), (u32, String)> {
        self.handle_msg_with_trace(h, comp, msg, out, None)
    }

    /// Invoke the optional trace v1 message hook, or the original vtable when
    /// unsupported. No process/thread-local context is inferred. A null parent
    /// explicitly marks an untraced invocation, including legacy API calls.
    /// Invalid all-zero IDs are rejected before invoking either hook.
    ///
    /// # Safety
    /// All pointer, exclusivity and buffer requirements of [`Self::handle_msg`]
    /// apply. The extension may borrow `parent` only until this call returns.
    pub unsafe fn handle_msg_with_trace(
        &self,
        h: &DylibHandle,
        comp: *mut AbiComponent,
        msg: AbiMsg,
        out: &mut AbiReplyBuf,
        parent: Option<&AbiTraceParentV1>,
    ) -> Result<(), (u32, String)> {
        if parent.is_some_and(|parent| !parent.is_valid()) {
            return Err((crate::ABI_ERR_STATE, "invalid trace parent".into()));
        }
        if h.quarantined.load(Ordering::Acquire) {
            return Err((crate::ABI_ERR_STATE, "quarantined: no new messages".into()));
        }
        // Preserve atomic registry admission as a defensive invariant in
        // addition to the shared-borrow/exclusive-ownership lifecycle boundary.
        let _guard = acquire_inflight(&h.registry, comp as usize)
            .ok_or((crate::ABI_ERR_STATE, "component not registered".into()))?;
        if h.registry
            .drained
            .lock()
            .unwrap()
            .contains_key(&(comp as usize))
        {
            return Err((crate::ABI_ERR_STATE, "component is drained".into()));
        }
        let vt = (*comp).vt;
        if vt.is_null() || (*comp).self_.is_null() {
            return Err((crate::ABI_ERR_STATE, "null vtable or self".into()));
        }
        let f = (*vt).handle_msg;
        let self_ = (*comp).self_;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(extension) = h.trace_v1 {
                (extension.handle_msg)(
                    self_,
                    msg,
                    out as *mut AbiReplyBuf,
                    parent.map_or(std::ptr::null(), std::ptr::from_ref),
                )
            } else {
                f(self_, msg, out as *mut AbiReplyBuf)
            }
        }));
        match r {
            Ok(code) if code == ABI_OK => Ok(()),
            Ok(code) => Err((code, format!("handle_msg code={code}"))),
            Err(p) => Err((crate::ABI_ERR_PANIC, format!("dylib panic: {p:?}"))),
        }
    }

    /// Close an instance to business calls and run its drain hook once. Repeated
    /// calls return the original code, including failure. Later unload destroys
    /// it without repeating the hook; sibling instances remain usable.
    ///
    /// # Safety
    /// `comp` must be live and constructed by `h`. The caller must ensure
    /// exclusive access to that instance across message and drain calls. All
    /// native trust obligations of [`Self::load`] continue to apply.
    pub unsafe fn drain_instance(
        &self,
        h: &DylibHandle,
        comp: *mut AbiComponent,
    ) -> Result<(), (u32, String)> {
        let addr = comp as usize;
        let _guard = acquire_inflight(&h.registry, addr)
            .ok_or((crate::ABI_ERR_STATE, "component not registered".into()))?;
        let cached = h.registry.drained.lock().unwrap().get(&addr).copied();
        let code = match cached {
            Some(code) => code,
            None => {
                let code = unsafe { ((*(*comp).vt).on_drain)((*comp).self_) };
                h.registry.drained.lock().unwrap().insert(addr, code);
                code
            }
        };
        if code == ABI_OK {
            Ok(())
        } else {
            Err((code, format!("on_drain code={code}")))
        }
    }

    /// Quarantine, call each on_drain, destroy each instance, then close the library.
    /// This synchronous variant is intended for an already owned native worker.
    /// It can block indefinitely in trusted native code.
    pub fn unload_blocking(&self, mut h: DylibHandle) -> Result<UnloadReport, UnloadError> {
        h.cleanup()
    }

    /// Quarantine, call each on_drain, destroy each instance, then close the library.
    ///
    /// Cleanup starts on an owned OS thread immediately, even if the returned
    /// future is never polled. Dropping it or reaching the deadline only stops
    /// waiting; cleanup retains the library until native calls have returned.
    /// A stuck native hook therefore retains its worker and resources. Use a
    /// process boundary when hard termination of untrusted code is required.
    ///
    /// The deadline covers the entire cleanup, starting at this call. Await
    /// inside a Tokio runtime **with time enabled**. A handle dropped directly
    /// instead runs the same cleanup synchronously, without a runtime.
    ///
    /// Ownership prevents unloading while a native caller still borrows the handle:
    ///
    /// ```compile_fail,E0505
    /// use parrot_abi::{DylibHandle, DylibLoader};
    /// fn unload_during_borrow(handle: DylibHandle) {
    ///     let borrowed = &handle;
    ///     let _cleanup = DylibLoader.unload(handle, std::time::Duration::from_secs(1));
    ///     let _ = DylibLoader.construct(borrowed, "{}");
    /// }
    /// ```
    pub fn unload(
        &self,
        mut h: DylibHandle,
        drain_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<UnloadReport, UnloadError>> + Send + 'static {
        h.quarantined.store(true, Ordering::Release);
        let deadline = Instant::now().checked_add(drain_timeout);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("parrot-native-cleanup".into())
            .spawn(move || {
                let result = h.cleanup();
                let _ = sender.send(result);
            });
        async move {
            // If spawning failed, dropping the closure already ran RAII cleanup.
            let _worker = worker.map_err(|e| UnloadError::WorkerStart(e.to_string()))?;
            let deadline = deadline.ok_or(UnloadError::InvalidTimeout)?;
            tokio::runtime::Handle::try_current().map_err(|_| UnloadError::NoRuntime)?;
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), receiver).await
            {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(UnloadError::WorkerLost),
                Err(_) => Err(UnloadError::TimedOut),
            }
        }
    }

    /// Inspect ELF/Mach-O symbols without executing code or spawning `nm`.
    /// A missing, malformed, unsupported or insufficiently inspectable artifact
    /// is an error, never a clean result. This conservative diagnostic rejects
    /// component TLS storage; it cannot prove whether a destructor is registered.
    /// Rust std's own named TLS is exempt. Native code still requires trust.
    pub fn scan_violations(path: &Path) -> Result<Vec<Violation>, LoadError> {
        Self::scan_violations_with_limits(path, NativeArtifactLimits::default())
    }

    /// Scan a regular file using an explicit input byte budget.
    pub fn scan_violations_with_limits(
        path: &Path,
        limits: NativeArtifactLimits,
    ) -> Result<Vec<Violation>, LoadError> {
        let bytes = read_artifact(path, limits).map_err(|e| match e {
            LoadError::Open(message) => LoadError::Scan(message),
            other => other,
        })?;
        scan_bytes(&bytes, limits)
    }
}

// Ordering preserves the public diagnostic order without allocating sort keys.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ViolationKind {
    Signal,
    Thread,
    Tls,
}

// Unix dlsym also searches dependencies. Only the component that owns the ABI
// metadata can opt in; the hook's code may intentionally live in a dependency.
#[cfg(unix)]
unsafe fn same_symbol_image(first: *const (), second: *const ()) -> bool {
    let mut first_info = std::mem::MaybeUninit::<libc::Dl_info>::uninit();
    let mut second_info = std::mem::MaybeUninit::<libc::Dl_info>::uninit();
    if libc::dladdr(first.cast(), first_info.as_mut_ptr()) == 0
        || libc::dladdr(second.cast(), second_info.as_mut_ptr()) == 0
    {
        return false;
    }
    let first_base = first_info.assume_init().dli_fbase;
    !first_base.is_null() && first_base == second_info.assume_init().dli_fbase
}

// Windows GetProcAddress resolves exports from the specified module only.
#[cfg(not(unix))]
unsafe fn same_symbol_image(_first: *const (), _second: *const ()) -> bool {
    true
}

fn is_std_tls(name: &str) -> bool {
    use std::fmt::Write;
    struct Prefix {
        bytes: [u8; 6],
        used: usize,
    }
    impl std::fmt::Write for Prefix {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            let n = (6 - self.used).min(text.len());
            self.bytes[self.used..self.used + n].copy_from_slice(&text.as_bytes()[..n]);
            self.used += n;
            if self.used == 6 {
                Err(std::fmt::Error)
            } else {
                Ok(())
            }
        }
    }
    let mut prefix = Prefix {
        bytes: [0; 6],
        used: 0,
    };
    // Deliberately stop formatting after six bytes. No expanded owned string
    // is needed merely to recognize std's root path. rustc-demangle validates
    // v0 backrefs without expanding their targets before formatting.
    let _ = write!(
        &mut prefix,
        "{:#}",
        rustc_demangle::demangle(name.strip_suffix("$tlv$init").unwrap_or(name))
    );
    prefix.bytes[..5] == *b"std::" || prefix.bytes == *b"<std::"
}

fn scan_bytes(bytes: &[u8], limits: NativeArtifactLimits) -> Result<Vec<Violation>, LoadError> {
    use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind};
    let file = object::File::parse(bytes).map_err(|error| LoadError::Scan(error.to_string()))?;
    let mut violations = std::collections::BTreeSet::new();
    let mut tls_symbols = std::collections::HashSet::new();
    let mut tls_sections = std::collections::HashSet::new();
    for (index, section) in file.sections().enumerate() {
        if index >= limits.max_scan_sections {
            return Err(LoadError::ResourceLimit(
                "section analysis budget exhausted".into(),
            ));
        }
        if section.size() > 0
            && matches!(
                section.kind(),
                SectionKind::Tls | SectionKind::UninitializedTls | SectionKind::TlsVariables
            )
        {
            tls_sections.insert(section.index());
        }
    }
    let mut name_bytes_left = limits.max_scan_name_bytes;
    for (index, symbol) in file.symbols().chain(file.dynamic_symbols()).enumerate() {
        if index >= limits.max_scan_symbols {
            return Err(LoadError::ResourceLimit(
                "symbol analysis budget exhausted".into(),
            ));
        }
        let name = symbol
            .name()
            .map_err(|error| LoadError::Scan(error.to_string()))?;
        name_bytes_left = name_bytes_left.checked_sub(name.len()).ok_or_else(|| {
            LoadError::ResourceLimit("symbol-name analysis budget exhausted".into())
        })?;
        if name.is_empty() {
            continue;
        }
        // ELF symbol version suffixes and Mach-O's C underscore are decoration.
        let base = name.split('@').next().unwrap_or(name);
        let base = if file.format() == object::BinaryFormat::MachO {
            base.strip_prefix('_').unwrap_or(base)
        } else {
            base
        };
        if symbol.is_undefined() {
            let violation = match base {
                "pthread_create" => Some(ViolationKind::Thread),
                "signal" | "sigaction" | "sigsetjmp" | "__sigsetjmp" => Some(ViolationKind::Signal),
                _ => None,
            };
            if let Some(violation) = violation {
                violations.insert((violation, name));
            }
        } else {
            let section_tls = symbol
                .section_index()
                .map(|index| {
                    file.section_by_index(index).map(|section| {
                        matches!(
                            section.kind(),
                            SectionKind::Tls
                                | SectionKind::UninitializedTls
                                | SectionKind::TlsVariables
                        )
                    })
                })
                .transpose()
                .map_err(|error| LoadError::Scan(error.to_string()))?
                .unwrap_or(false);
            // ELF section/mapping labels (e.g. AArch64 $d) are not variables.
            // The zero-sized TLS module base denotes the whole TLS block, not
            // an additional user-owned TLS allocation.
            let storage = matches!(symbol.kind(), SymbolKind::Data | SymbolKind::Tls)
                && !(symbol.size() == 0
                    && (name == "_TLS_MODULE_BASE_"
                        || (matches!(
                            file.architecture(),
                            object::Architecture::Aarch64 | object::Architecture::Arm
                        ) && (name == "$d" || name.starts_with("$d.")))));
            if storage
                && (symbol.kind() == SymbolKind::Tls || section_tls || name.contains("$tlv$init"))
            {
                if let Some(index) = symbol.section_index() {
                    tls_symbols.insert(index);
                }
                if !is_std_tls(name) {
                    // Borrow names while scanning; copy only distinct findings
                    // after all budgets and parse checks have succeeded.
                    violations.insert((ViolationKind::Tls, name));
                }
            }
        }
    }
    // A stripped TLS section cannot be classified as runtime-owned. Fail closed
    // instead of treating the absence of local symbol names as proof of safety.
    if !tls_sections.is_subset(&tls_symbols) {
        return Err(LoadError::Scan(
            "TLS storage has no inspectable owner symbols".into(),
        ));
    }
    Ok(violations
        .into_iter()
        .map(|(kind, name)| match kind {
            ViolationKind::Signal => Violation::SignalHandler {
                symbol: name.into(),
            },
            ViolationKind::Thread => Violation::ThreadSpawn {
                symbol: name.into(),
            },
            ViolationKind::Tls => Violation::TlsDestructor {
                symbol: name.into(),
            },
        })
        .collect())
}

#[cfg(test)]
mod artifact_read_tests {
    use super::*;
    #[test]
    fn bounded_read_handles_growth_without_consuming_the_rest() {
        struct Infinite {
            read: usize,
        }
        impl std::io::Read for Infinite {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(0);
                self.read += buf.len();
                Ok(buf.len())
            }
        }
        for max_file_bytes in [1, 2, 31, 4096] {
            let limits = NativeArtifactLimits {
                max_file_bytes,
                ..Default::default()
            };
            let mut source = Infinite { read: 0 };
            assert!(matches!(
                limits.read(&mut source),
                Err(LoadError::ResourceLimit(_))
            ));
            assert_eq!(source.read, max_file_bytes + 1);
            let exact = vec![42; max_file_bytes];
            assert_eq!(limits.read(exact.as_slice()).unwrap(), exact);
        }
    }
}

#[cfg(test)]
mod inflight_tests {
    use super::*;

    #[test]
    fn standard_library_method_tls_is_distinguished_from_component_tls() {
        let symbol = "__RNvNCNKNvNvMNtNtCs82bWklYMk3w_3std4hash6randomNtBa_11RandomState3new4KEYS0s_023___RUST_STD_INTERNAL_VAL";
        assert!(is_std_tls(symbol), "{:#}", rustc_demangle::demangle(symbol));
        assert!(is_std_tls(&format!("{symbol}$tlv$init")));
        for name in [
            "guest::std::TLS",
            "<guest::Foo as std::Trait>::TLS",
            "std_like::TLS",
            "<std_like::Foo>::TLS",
        ] {
            assert!(!is_std_tls(name), "{name}");
        }
    }

    #[test]
    fn cleanup_observes_an_admitted_call_before_draining() {
        let registry = InstanceRegistry::default();
        let addr = 7usize;
        registry
            .alive
            .lock()
            .unwrap()
            .insert(addr, Arc::new(AtomicUsize::new(0)));
        let guard = acquire_inflight(&registry, addr).expect("registered instance");
        let entries = registry.alive.lock().unwrap();
        assert_eq!(entries.get(&addr).unwrap().load(Ordering::Acquire), 1);
        drop(entries);
        drop(guard);
        assert_eq!(
            registry
                .alive
                .lock()
                .unwrap()
                .get(&addr)
                .unwrap()
                .load(Ordering::Acquire),
            0
        );
    }

    #[test]
    fn cleanup_removal_wins_before_a_late_admission() {
        let registry = InstanceRegistry::default();
        let addr = 9usize;
        registry
            .alive
            .lock()
            .unwrap()
            .insert(addr, Arc::new(AtomicUsize::new(0)));
        registry.alive.lock().unwrap().remove(&addr);
        assert!(acquire_inflight(&registry, addr).is_none());
    }
}
