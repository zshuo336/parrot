//! parrot-wasm：Parrot wasm 组件运行时（DEV_09 C 阶段 / 09 §4.2）。
//!
//! 分层：
//! - `WasmConfig` / `HostCtx` / `WasmMetrics` / `ComponentError`：纯类型
//!   （default feature——引用方零 wasmtime 成本）
//! - `runtime` feature：`WasmRuntime` / `WasmComponent`（wasmtime 集成——
//!   fuel/epoch/线性内存限额与宿主panic边界）
//!
//! WIT 契约：`wit/parrot-actor.wit`（C2 冻结件——semver 管理）。
//! 消息边界与 codec_registry 同构：type_key + bytes（组件内部序列化自由）。

pub mod wit_include {
    //! 冻结 WIT 文本嵌入（运行时校验用——防止分发目录漂移）。
    pub const PARROT_ACTOR_WIT: &str = include_str!("../wit/parrot-actor.wit");
}

// ─────────────────────────────────────────────────────────────
// 配置与上下文（纯类型——default feature）
// ─────────────────────────────────────────────────────────────

/// 沙箱限额（fuel/epoch/memory——09 §4.2 调度映射）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmConfig {
    /// 每消息 fuel 预算（耗尽返回 `ComponentError::OutOfFuel`）。
    pub fuel_per_message: u64,
    /// epoch 增量预算（默认每10ms推进一次；0按1处理；最大u32::MAX）。
    /// tick相位和宿主调度会影响实际时限，不提供硬实时保证。
    pub epoch_deadline: u64,
    /// 每个组件Store全部线性内存的合计上限（MiB），包括初始化和增长。
    pub memory_limit_mb: usize,
}

impl Default for WasmConfig {
    fn default() -> Self {
        Self {
            fuel_per_message: 100_000,
            epoch_deadline: 1,
            memory_limit_mb: 64,
        }
    }
}

/// 组件宿主上下文（能力注入——ctx 接口实现数据源）。
#[derive(Debug, Clone, Default)]
pub struct HostCtx {
    /// 自身路径（/user/{name}）。
    pub self_path: String,
    /// config overlay 合成视图（ctx.config-get 数据源）。
    pub config_overlay: toml_table::Table,
    /// Most verbose guest log level admitted: 0=error, 1=warn, 2=info,
    /// 3=debug, 4=trace. Default 0 admits error only. Host tracing filters
    /// still apply after this component-specific filter.
    pub log_level: u8,
}

/// Explicit causal parent for one guest invocation. IDs use W3C byte order;
/// neither identifier may be all zero. This is metadata, not a recorded span,
/// authentication state or sampling decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceParent {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
}

impl TraceParent {
    pub fn is_valid(&self) -> bool {
        self.trace_id != [0; 16] && self.span_id != [0; 8]
    }
}

/// toml::value::Table 的独立声明（避免 default feature 拉 toml 依赖——
/// 宿主侧组装时才需要；此处用轻量 map 形态）。
pub mod toml_table {
    use std::collections::BTreeMap;

    /// TOML 表的值形态投影（config-get 只需要 string 视图）。
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct Table(pub BTreeMap<String, String>);

    impl Table {
        pub fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).map(|s| s.as_str())
        }
        pub fn insert(&mut self, k: impl Into<String>, v: impl Into<String>) {
            self.0.insert(k.into(), v.into());
        }
        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
    }
}

/// 组件运行指标（take_metrics 取走后清零——基准门禁数据源）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WasmMetrics {
    /// 累计 fuel 消耗。
    pub fuel_consumed: u64,
    /// 消息处理次数（handle + tell）。
    pub messages: u64,
    /// OutOfFuel 次数。
    pub out_of_fuel_count: u64,
    /// trap 次数。
    pub trap_count: u64,
    /// 实例化耗时（µs——instantiate 一次填入）。
    pub instantiate_micros: u64,
}

// ─────────────────────────────────────────────────────────────
// 错误（纯类型——default feature）
// ─────────────────────────────────────────────────────────────

/// 组件调用错误（09 §4.2 行为规约映射）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComponentError {
    #[error("invalid wasm trace parent: identifiers must be nonzero")]
    InvalidTraceParent,
    /// Invalid runtime limits, rejected before compiling or allocating guest state.
    #[error("invalid wasm configuration: {0}")]
    InvalidConfig(String),
    /// Host-side artifact admission limit, separate from guest fuel/memory.
    #[error("wasm resource limit: {0}")]
    ResourceLimit(String),
    /// wasm trap（含组件 err 返回值）——不污染宿主。
    #[error("wasm trap: {0}")]
    Trap(String),
    /// fuel 耗尽（→ OverQuota 语义）。
    #[error("out of fuel")]
    OutOfFuel,
    /// epoch 超限（协作抢占）。
    #[error("epoch deadline exceeded")]
    EpochDeadline,
    /// WIT 契约不符（缺 export / ABI 版本）。
    #[error("abi/contract mismatch: {0}")]
    AbiVersion(String),
    /// 宿主侧包装 panic（catch_unwind 捕获）。
    #[error("host panic boundary: {0}")]
    Panic(String),
}

#[cfg(feature = "runtime")]
pub mod runtime {
    //! wasmtime 集成（feature gate——R1 重编译成本隔离）。

    use super::{ComponentError, HostCtx, TraceParent, WasmConfig, WasmMetrics};
    use std::path::Path;

    mod asynchronous;
    mod cache;
    mod epoch;
    mod limits;
    mod messaging;
    mod recording;
    mod services;
    pub use asynchronous::PreparedComponent;
    pub use cache::CompilationLimits;
    pub use messaging::{GuestMessaging, MessagingError, MessagingRequest, MAX_MESSAGE_BYTES};
    pub use recording::{
        GuestScope, GuestSpan, GuestSpanStatus, GuestTrace, MAX_GUEST_SPANS,
        MAX_GUEST_SPAN_TARGET_BYTES,
    };
    pub use services::{
        GuestServices, GuestSubscription, ServiceError, ServiceEvent, ServiceFuture,
    };

    #[derive(Debug, thiserror::Error)]
    #[error("host panic: {0}")]
    struct HostPanic(String);

    fn guest_boundary<T>(call: impl FnOnce() -> wasmtime::Result<T>) -> wasmtime::Result<T> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)).unwrap_or_else(|payload| {
            let message = if let Some(message) = payload.downcast_ref::<String>() {
                message.clone()
            } else if let Some(message) = payload.downcast_ref::<&str>() {
                (*message).to_string()
            } else {
                "non-string host panic".to_string()
            };
            // Panic payload destructors are user code. A second hostile
            // payload cannot safely be dropped recursively during recovery.
            if let Err(secondary) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                std::mem::forget(secondary);
            }
            Err(HostPanic(message).into())
        })
    }

    fn init_error(phase: &str, error: wasmtime::Error) -> ComponentError {
        if let Some(HostPanic(message)) = error.downcast_ref::<HostPanic>() {
            ComponentError::Panic(message.clone())
        } else {
            match error.downcast_ref::<wasmtime::Trap>() {
                Some(wasmtime::Trap::OutOfFuel) => ComponentError::OutOfFuel,
                Some(wasmtime::Trap::Interrupt) => ComponentError::EpochDeadline,
                _ => ComponentError::AbiVersion(format!("{phase}: {error}")),
            }
        }
    }

    fn monotonic_millis() -> u64 {
        static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        ORIGIN
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis()
            .min(u64::MAX as u128) as u64
    }

    #[derive(Debug)]
    struct StoreState {
        host: HostCtx,
        limits: limits::GuestLimits,
        trace_parent: Option<TraceParent>,
        recording: Option<recording::Invocation>,
        next_span_handle: u64,
        messaging: Option<std::sync::Arc<dyn GuestMessaging>>,
        services: Option<std::sync::Arc<dyn GuestServices>>,
        subscriptions: std::collections::BTreeMap<u64, services::Subscription>,
        next_subscription_handle: u64,
        host_calls: usize,
        cleanup: asynchronous::CleanupFailures,
    }

    impl StoreState {
        fn charge_host_call(&mut self) -> bool {
            if self.host_calls >= 64 {
                return false;
            }
            self.host_calls += 1;
            true
        }
    }

    impl Drop for StoreState {
        fn drop(&mut self) {
            self.subscriptions.clear();
            // Unpublished initialization and final Store disposal own these
            // host capabilities too, including their last Arc destructor.
            let _ = self.cleanup.dispose(self.recording.take());
            let _ = self.cleanup.dispose(self.messaging.take());
            let _ = self.cleanup.dispose(self.services.take());
        }
    }

    fn memory_limit_bytes(cfg: &WasmConfig) -> Result<usize, ComponentError> {
        cfg.memory_limit_mb
            .checked_mul(1024 * 1024)
            .filter(|&bytes| bytes > 0 && bytes <= isize::MAX as usize)
            .ok_or_else(|| {
                ComponentError::InvalidConfig(
                    "memory_limit_mb must be positive and fit the host allocation range".into(),
                )
            })
    }

    fn create_store(
        engine: &wasmtime::Engine,
        cfg: &WasmConfig,
        host: HostCtx,
    ) -> Result<wasmtime::Store<StoreState>, ComponentError> {
        let mut store = wasmtime::Store::new(
            engine,
            StoreState {
                host,
                limits: limits::GuestLimits::new(memory_limit_bytes(cfg)?),
                trace_parent: None,
                recording: None,
                next_span_handle: 1,
                messaging: None,
                services: None,
                subscriptions: Default::default(),
                next_subscription_handle: 1,
                host_calls: 0,
                cleanup: Default::default(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(cfg.fuel_per_message.saturating_mul(8).max(200_000))
            .map_err(|e| ComponentError::AbiVersion(format!("initial fuel: {e}")))?;
        store.epoch_deadline_trap();
        store.set_epoch_deadline(cfg.epoch_deadline.max(1));
        Ok(store)
    }

    // ── WIT 绑定（wasmtime component API 手写桥——bindgen 生成器在
    //    50.0.0-rc 需 nightly 工具链，此处按 component API 显式编码：
    //    与 wit/parrot-actor.wit 冻结文本逐字段对齐）──

    // WIT record msg { type-key: string, payload: list<u8> } 的 Rust 映射
    // （derive ComponentType/Lift/Lower——字段名 kebab 映射）
    use wasmtime::component::{ComponentType, Lift, Lower};

    #[derive(ComponentType, Lift, Lower, Clone, Debug)]
    #[component(record)]
    pub struct Msg {
        #[component(name = "type-key")]
        pub type_key: String,
        #[component(name = "payload")]
        pub payload: Vec<u8>,
    }

    #[derive(ComponentType, Lower)]
    #[component(record)]
    struct WitTraceParent {
        #[component(name = "trace-id")]
        trace_id: Vec<u8>,
        #[component(name = "span-id")]
        span_id: Vec<u8>,
    }

    /// Wasmtime engine with bounded, exact-source compiled component reuse.
    pub struct WasmRuntime {
        engine: wasmtime::Engine,
        cfg: WasmConfig,
        epoch_driver: Option<std::sync::Arc<epoch::Driver>>,
        compilation_limits: CompilationLimits,
        cache: std::sync::Mutex<cache::Cache>,
        /// Serialize misses without holding the cache lock during compilation.
        compilation: std::sync::Mutex<()>,
    }

    impl WasmRuntime {
        /// Create a runtime with an owned 10ms epoch ticker. Components retain
        /// the ticker after the runtime is dropped; the last owner stops it.
        pub fn new(cfg: WasmConfig) -> Result<Self, ComponentError> {
            Self::build(cfg, true, CompilationLimits::default())
        }

        /// Disable the automatic ticker for deterministic tests or an external
        /// scheduler. The caller must drive `tick_epoch` to enforce deadlines.
        pub fn new_with_manual_epoch(cfg: WasmConfig) -> Result<Self, ComponentError> {
            Self::build(cfg, false, CompilationLimits::default())
        }

        /// Override artifact/cache admission limits; automatic epochs stay enabled.
        pub fn new_with_compilation_limits(
            cfg: WasmConfig,
            limits: CompilationLimits,
        ) -> Result<Self, ComponentError> {
            Self::build(cfg, true, limits)
        }

        fn build(
            cfg: WasmConfig,
            automatic_epoch: bool,
            compilation_limits: CompilationLimits,
        ) -> Result<Self, ComponentError> {
            memory_limit_bytes(&cfg)?;
            // Wasmtime computes current_epoch + delta without saturation.
            // Leave ample headroom instead of accepting u64::MAX and letting
            // the first tick cause a host overflow panic or a wrapped deadline.
            if cfg.epoch_deadline > u32::MAX as u64 {
                return Err(ComponentError::InvalidConfig(
                    "epoch_deadline must not exceed u32::MAX ticks".into(),
                ));
            }
            compilation_limits.validate()?;
            let mut ec = wasmtime::Config::new();
            ec.epoch_interruption(true)
                .consume_fuel(true)
                .wasm_memory64(false)
                // ResourceLimiter does not observe shared-memory growth.
                .wasm_threads(false);
            let engine = wasmtime::Engine::new(&ec)
                .map_err(|e| ComponentError::AbiVersion(format!("engine init: {e}")))?;
            let epoch_driver = if automatic_epoch {
                Some(std::sync::Arc::new(
                    epoch::Driver::start(engine.clone())
                        .map_err(|e| ComponentError::AbiVersion(format!("epoch driver: {e}")))?,
                ))
            } else {
                None
            };
            Ok(Self {
                engine,
                cfg,
                epoch_driver,
                compilation_limits,
                cache: std::sync::Mutex::new(cache::Cache::default()),
                compilation: std::sync::Mutex::new(()),
            })
        }

        pub fn config(&self) -> &WasmConfig {
            &self.cfg
        }

        /// Advance the shared engine epoch, in addition to any automatic ticks.
        pub fn tick_epoch(&self) {
            self.engine.increment_epoch();
        }

        /// Engine reference. Cloning it alone does not retain the owned ticker.
        pub fn engine(&self) -> &wasmtime::Engine {
            &self.engine
        }

        /// 实例化组件（wasm 二进制或 wat 文本均可——单测载体）。
        pub fn instantiate(
            &self,
            wasm_path: &Path,
            ctx: HostCtx,
        ) -> Result<WasmComponent, ComponentError> {
            let t0 = std::time::Instant::now();
            let mut open = std::fs::OpenOptions::new();
            open.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                open.custom_flags(libc::O_NONBLOCK);
            }
            let file = open.open(wasm_path).map_err(|e| {
                ComponentError::AbiVersion(format!("read {}: {e}", wasm_path.display()))
            })?;
            let metadata = file.metadata().map_err(|e| {
                ComponentError::AbiVersion(format!("read metadata {}: {e}", wasm_path.display()))
            })?;
            if !metadata.is_file() {
                return Err(ComponentError::AbiVersion(
                    "read component: expected a regular file".into(),
                ));
            }
            if metadata.len() > self.compilation_limits.max_input_bytes as u64 {
                return Err(ComponentError::ResourceLimit(format!(
                    "component input exceeds {} bytes",
                    self.compilation_limits.max_input_bytes
                )));
            }
            let bytes = self.compilation_limits.read(file)?;
            self.instantiate_bytes_started(&bytes, ctx, t0)
        }

        /// Instantiate the exact bytes admitted by the caller (for example after
        /// digest verification), without reopening a mutable artifact path.
        /// The same input/cache limits and sandbox apply as to [`Self::instantiate`].
        pub fn instantiate_bytes(
            &self,
            bytes: &[u8],
            ctx: HostCtx,
        ) -> Result<WasmComponent, ComponentError> {
            self.instantiate_bytes_started(bytes, ctx, std::time::Instant::now())
        }

        /// Compile on a blocking worker, then instantiate with asynchronous
        /// host imports using [`PreparedComponent::instantiate_async`].
        pub fn prepare_bytes(&self, bytes: &[u8]) -> Result<PreparedComponent, ComponentError> {
            let t0 = std::time::Instant::now();
            Ok(PreparedComponent {
                engine: self.engine.clone(),
                component: std::sync::Arc::new(self.compile_cached(bytes)?),
                cfg: self.cfg.clone(),
                epoch_driver: self.epoch_driver.clone(),
                started_at: t0,
            })
        }

        fn instantiate_bytes_started(
            &self,
            bytes: &[u8],
            ctx: HostCtx,
            t0: std::time::Instant,
        ) -> Result<WasmComponent, ComponentError> {
            let component = self.compile_cached(bytes)?;
            let component = std::sync::Arc::new(component);
            let mut store = create_store(&self.engine, &self.cfg, ctx)?;
            let bindings = guest_boundary(|| ParrotComponent::instantiate(&mut store, &component))
                .map_err(|e| init_error("instantiate", e))?;
            Ok(WasmComponent {
                store,
                bindings,
                poisoned: false,
                asynchronous: false,
                started: false,
                _epoch_driver: self.epoch_driver.clone(),
                engine: self.engine.clone(),
                component,
                cfg: self.cfg.clone(),
                metrics: WasmMetrics {
                    instantiate_micros: t0.elapsed().as_micros() as u64,
                    ..Default::default()
                },
            })
        }

        fn compile_cached(
            &self,
            bytes: &[u8],
        ) -> Result<wasmtime::component::Component, ComponentError> {
            self.compilation_limits.check_input(bytes.len())?;
            if let Some(c) = self.lock_cache()?.get(bytes) {
                return Ok(c);
            }
            let _permit = self
                .compilation
                .lock()
                .map_err(|_| ComponentError::Panic("component compilation lock poisoned".into()))?;
            // Another miss may have compiled these exact bytes while we waited.
            if let Some(c) = self.lock_cache()?.get(bytes) {
                return Ok(c);
            }
            let comp = guest_boundary(|| wasmtime::component::Component::new(&self.engine, bytes))
                .map_err(|e| init_error("compile", e))?;
            self.lock_cache()?
                .insert(bytes, comp.clone(), &self.compilation_limits);
            Ok(comp)
        }

        fn lock_cache(&self) -> Result<std::sync::MutexGuard<'_, cache::Cache>, ComponentError> {
            self.cache
                .lock()
                .map_err(|_| ComponentError::Panic("component cache lock poisoned".into()))
        }
    }

    // ── WIT 桥（parrot:component/ctx + handler——字段序对齐冻结 WIT）──

    /// handler export 句柄（typed funcs——消息热路径零查表）。
    pub struct ParrotComponent {
        instance: wasmtime::component::Instance,
        handle_fn: wasmtime::component::TypedFunc<(Msg,), (Result<Vec<u8>, String>,)>,
        tell_fn: wasmtime::component::TypedFunc<(Msg,), ()>,
        on_start_fn: wasmtime::component::TypedFunc<(), ()>,
        on_drain_fn: wasmtime::component::TypedFunc<(), ()>,
    }

    impl std::fmt::Debug for ParrotComponent {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ParrotComponent")
                .field("instance", &self.instance)
                .finish_non_exhaustive()
        }
    }

    impl ParrotComponent {
        fn instantiate(
            store: &mut wasmtime::Store<StoreState>,
            comp: &wasmtime::component::Component,
        ) -> wasmtime::Result<Self> {
            let linker = Self::linker(store.engine(), false)?;
            let instance = linker.instantiate(&mut *store, comp)?;
            Self::bindings(store, instance)
        }

        fn linker(
            engine: &wasmtime::Engine,
            asynchronous: bool,
        ) -> wasmtime::Result<wasmtime::component::Linker<StoreState>> {
            let mut linker = wasmtime::component::Linker::new(engine);
            // ctx 能力（host functions——最小权限四件套；返回 Result 形态）
            let mut ctx = linker.instance("parrot:component/ctx@0.1.0")?;
            ctx.func_wrap(
                "self-ref",
                |caller: wasmtime::StoreContextMut<'_, StoreState>, (): ()| {
                    Ok((caller.data().host.self_path.clone(),))
                },
            )?;
            ctx.func_wrap(
                "log",
                |caller: wasmtime::StoreContextMut<'_, StoreState>, (level, msg): (u8, String)| {
                    let data = &caller.data().host;
                    if level <= data.log_level {
                        match level {
                            0 => tracing::error!(target: "parrot_wasm", "{}", msg),
                            1 => tracing::warn!(target: "parrot_wasm", "{}", msg),
                            2 => tracing::info!(target: "parrot_wasm", "{}", msg),
                            3 => tracing::debug!(target: "parrot_wasm", "{}", msg),
                            _ => tracing::trace!(target: "parrot_wasm", "{}", msg),
                        }
                    }
                    Ok(())
                },
            )?;
            ctx.func_wrap(
                "config-get",
                |caller: wasmtime::StoreContextMut<'_, StoreState>, (key,): (String,)| {
                    Ok((caller
                        .data()
                        .host
                        .config_overlay
                        .get(&key)
                        .map(str::to_string),))
                },
            )?;
            ctx.func_wrap(
                "clock-now-ms",
                |_: wasmtime::StoreContextMut<'_, StoreState>, (): ()| Ok((monotonic_millis(),)),
            )?;

            let mut trace = linker.instance("parrot:trace/context@0.1.0")?;
            trace.func_wrap(
                "current-parent",
                |caller: wasmtime::StoreContextMut<'_, StoreState>, (): ()| {
                    Ok((caller.data().trace_parent.map(|parent| WitTraceParent {
                        trace_id: parent.trace_id.to_vec(),
                        span_id: parent.span_id.to_vec(),
                    }),))
                },
            )?;

            recording::link(&mut linker)?;
            if asynchronous {
                messaging::link(&mut linker)?;
                services::link(&mut linker)?;
            }
            Ok(linker)
        }

        fn bindings(
            store: &mut wasmtime::Store<StoreState>,
            instance: wasmtime::component::Instance,
        ) -> wasmtime::Result<Self> {
            let handler_idx = instance
                .get_export_index(&mut *store, None, "parrot:component/handler@0.1.0")
                .ok_or_else(|| wasmtime::Error::msg("missing export parrot:component/handler"))?;
            let handle_fn = instance
                .get_export_index(&mut *store, Some(&handler_idx), "handle")
                .and_then(|i| instance.get_func(&mut *store, i))
                .ok_or_else(|| wasmtime::Error::msg("missing handler.handle"))?
                .typed::<(Msg,), (Result<Vec<u8>, String>,)>(&mut *store)?;
            let tell_fn = instance
                .get_export_index(&mut *store, Some(&handler_idx), "tell")
                .and_then(|i| instance.get_func(&mut *store, i))
                .ok_or_else(|| wasmtime::Error::msg("missing handler.tell"))?
                .typed::<(Msg,), ()>(&mut *store)?;
            let on_start_fn = instance
                .get_export_index(&mut *store, Some(&handler_idx), "on-start")
                .and_then(|i| instance.get_func(&mut *store, i))
                .ok_or_else(|| wasmtime::Error::msg("missing handler.on-start"))?
                .typed::<(), ()>(&mut *store)?;
            let on_drain_fn = instance
                .get_export_index(&mut *store, Some(&handler_idx), "on-drain")
                .and_then(|i| instance.get_func(&mut *store, i))
                .ok_or_else(|| wasmtime::Error::msg("missing handler.on-drain"))?
                .typed::<(), ()>(&mut *store)?;
            Ok(Self {
                instance,
                handle_fn,
                tell_fn,
                on_start_fn,
                on_drain_fn,
            })
        }

        /// 原始实例引用（状态检查/扩展探针）。
        pub fn instance(&self) -> &wasmtime::component::Instance {
            &self.instance
        }
    }

    /// 组件实例（store + bindings + 指标——drop 即释放，同 digest 重建
    /// 断言无状态残留由调用方保证：每次 instantiate 全新 store）。
    #[derive(Debug)]
    pub struct WasmComponent {
        store: wasmtime::Store<StoreState>,
        bindings: ParrotComponent,
        /// trap 后 component instance 不可重入——置毒标记，下一消息
        /// 以全新 Store + 实例重建（宿主零污染；组件状态丢弃是
        /// 沙箱语义：失活组件的监督决策由宿主决定）。
        poisoned: bool,
        asynchronous: bool,
        /// A rebuilt instance must re-run a previously successful on_start.
        started: bool,
        // Ownership, not a direct caller: keeps ticking through runtime drop
        // and guest rebuild. Declared before engine so ticker is joined first.
        _epoch_driver: Option<std::sync::Arc<epoch::Driver>>,
        /// 引擎与预编译组件（毒化重建用）。
        engine: wasmtime::Engine,
        component: std::sync::Arc<wasmtime::component::Component>,
        cfg: WasmConfig,
        metrics: WasmMetrics,
    }

    impl WasmComponent {
        /// ask 入口：bytes → WIT msg → handle → bytes（与 codec_registry 同构）。
        pub fn handle(
            &mut self,
            type_key: &str,
            payload: &[u8],
        ) -> Result<Vec<u8>, ComponentError> {
            self.handle_with_trace(type_key, payload, None)
        }

        /// Invoke a guest with an explicit parent. Legacy guests keep their
        /// existing exports and simply do not import this metadata. The Store
        /// owns a copy only for this call. Plain lifecycle/rebuild is untraced;
        /// explicit lifecycle methods accept their own invocation context.
        pub fn handle_with_trace(
            &mut self,
            type_key: &str,
            payload: &[u8],
            parent: Option<&TraceParent>,
        ) -> Result<Vec<u8>, ComponentError> {
            self.handle_invocation(type_key, payload, parent, None)
        }

        /// Invoke with an explicit host recording capability. Neither ambient
        /// tracing nor plain lifecycle calls implicitly acquire this capability.
        pub fn handle_with_recording(
            &mut self,
            type_key: &str,
            payload: &[u8],
            recording: Box<dyn GuestTrace>,
        ) -> Result<Vec<u8>, ComponentError> {
            let recording = recording::Invocation::new(recording);
            let parent = recording
                .parent()
                .map_err(|e| init_error("recording parent", e))?;
            self.handle_invocation(type_key, payload, Some(&parent), Some(recording))
        }

        fn handle_invocation(
            &mut self,
            type_key: &str,
            payload: &[u8],
            parent: Option<&TraceParent>,
            recording: Option<recording::Invocation>,
        ) -> Result<Vec<u8>, ComponentError> {
            self.enter_message_scope(parent, recording.as_ref())?;
            self.store.data_mut().recording = recording;
            let r = guest_boundary(|| {
                self.bindings.handle_fn.call(
                    &mut self.store,
                    (Msg {
                        type_key: type_key.to_string(),
                        payload: payload.to_vec(),
                    },),
                )
            });
            let unfinished = matches!(&r, Ok((Err(_),))).then_some(GuestSpanStatus::Error);
            let r = self.exit_message_scope(r, unfinished);
            match r {
                Ok((Ok(out),)) => {
                    self.metrics.messages = self.metrics.messages.saturating_add(1);
                    Ok(out)
                }
                Ok((Err(e),)) => {
                    self.metrics.messages = self.metrics.messages.saturating_add(1);
                    Err(self.classify_trap(e))
                }
                Err(t) => Err(self.classify_trap_err(&t)),
            }
        }

        /// tell 入口（单向）。
        pub fn tell(&mut self, type_key: &str, payload: &[u8]) -> Result<(), ComponentError> {
            self.tell_with_trace(type_key, payload, None)
        }

        /// One-way message with the same per-invocation parent lifetime as ASK.
        pub fn tell_with_trace(
            &mut self,
            type_key: &str,
            payload: &[u8],
            parent: Option<&TraceParent>,
        ) -> Result<(), ComponentError> {
            self.tell_invocation(type_key, payload, parent, None)
        }

        /// One-way message with the same bounded recording lifetime as ASK.
        pub fn tell_with_recording(
            &mut self,
            type_key: &str,
            payload: &[u8],
            recording: Box<dyn GuestTrace>,
        ) -> Result<(), ComponentError> {
            let recording = recording::Invocation::new(recording);
            let parent = recording
                .parent()
                .map_err(|e| init_error("recording parent", e))?;
            self.tell_invocation(type_key, payload, Some(&parent), Some(recording))
        }

        fn tell_invocation(
            &mut self,
            type_key: &str,
            payload: &[u8],
            parent: Option<&TraceParent>,
            recording: Option<recording::Invocation>,
        ) -> Result<(), ComponentError> {
            self.enter_message_scope(parent, recording.as_ref())?;
            self.store.data_mut().recording = recording;
            let r = guest_boundary(|| {
                self.bindings.tell_fn.call(
                    &mut self.store,
                    (Msg {
                        type_key: type_key.to_string(),
                        payload: payload.to_vec(),
                    },),
                )
            });
            let r = self.exit_message_scope(r, None);
            match r {
                Ok(()) => {
                    self.metrics.messages = self.metrics.messages.saturating_add(1);
                    Ok(())
                }
                Err(t) => Err(self.classify_trap_err(&t)),
            }
        }

        /// Startup runs with the same finite message budget as business calls.
        pub fn on_start(&mut self) -> Result<(), ComponentError> {
            self.on_start_with_trace(None)
        }

        /// Explicit startup identity, cleared before the next invocation.
        pub fn on_start_with_trace(
            &mut self,
            parent: Option<&TraceParent>,
        ) -> Result<(), ComponentError> {
            self.start_invocation(parent, None)
        }

        /// Explicit startup recording with its own bounded invocation budget.
        pub fn on_start_with_recording(
            &mut self,
            recording: Box<dyn GuestTrace>,
        ) -> Result<(), ComponentError> {
            let recording = recording::Invocation::new(recording);
            let parent = recording
                .parent()
                .map_err(|e| init_error("recording parent", e))?;
            self.start_invocation(Some(&parent), Some(recording))
        }

        fn start_invocation(
            &mut self,
            parent: Option<&TraceParent>,
            recording: Option<recording::Invocation>,
        ) -> Result<(), ComponentError> {
            self.enter_scope(parent, false, None)?;
            self.store.data_mut().recording = recording;
            self.call_start()
        }

        fn call_start(&mut self) -> Result<(), ComponentError> {
            let result = guest_boundary(|| self.bindings.on_start_fn.call(&mut self.store, ()));
            let result = self.exit_message_scope(result, None);
            match result {
                Ok(()) => {
                    self.started = true;
                    Ok(())
                }
                Err(error) => Err(self.classify_trap_err(&error)),
            }
        }

        /// A cooperative drain hook; it receives its own finite call budget.
        pub fn on_drain(&mut self) -> Result<(), ComponentError> {
            self.on_drain_with_trace(None)
        }

        /// Explicit drain identity; a required poisoned-store restart still
        /// runs without inheriting this invocation's parent or capability.
        pub fn on_drain_with_trace(
            &mut self,
            parent: Option<&TraceParent>,
        ) -> Result<(), ComponentError> {
            self.drain_invocation(parent, None)
        }

        /// Explicit drain recording, retired on success, trap or host panic.
        pub fn on_drain_with_recording(
            &mut self,
            recording: Box<dyn GuestTrace>,
        ) -> Result<(), ComponentError> {
            let recording = recording::Invocation::new(recording);
            let parent = recording
                .parent()
                .map_err(|e| init_error("recording parent", e))?;
            self.drain_invocation(Some(&parent), Some(recording))
        }

        fn drain_invocation(
            &mut self,
            parent: Option<&TraceParent>,
            recording: Option<recording::Invocation>,
        ) -> Result<(), ComponentError> {
            self.enter_message_scope(parent, recording.as_ref())?;
            self.store.data_mut().recording = recording;
            let result = guest_boundary(|| self.bindings.on_drain_fn.call(&mut self.store, ()));
            let result = self.exit_message_scope(result, None);
            match result {
                Ok(()) => Ok(()),
                Err(error) => Err(self.classify_trap_err(&error)),
            }
        }

        /// 取走指标（清零——take 语义）。
        pub fn take_metrics(&mut self) -> WasmMetrics {
            std::mem::take(&mut self.metrics)
        }

        // ── 沙箱边界 ──

        fn enter_message_scope(
            &mut self,
            parent: Option<&TraceParent>,
            recovery: Option<&recording::Invocation>,
        ) -> Result<(), ComponentError> {
            self.enter_scope(parent, true, recovery)
        }

        fn enter_scope(
            &mut self,
            parent: Option<&TraceParent>,
            restore_start: bool,
            recovery: Option<&recording::Invocation>,
        ) -> Result<(), ComponentError> {
            if self.asynchronous {
                return Err(ComponentError::InvalidConfig(
                    "messaging-enabled components require asynchronous invocation".into(),
                ));
            }
            if parent.is_some_and(|parent| !parent.is_valid()) {
                return Err(ComponentError::InvalidTraceParent);
            }
            self.prepare_scope(restore_start, recovery)?;
            self.store.data_mut().trace_parent = parent.copied();
            Ok(())
        }

        fn prepare_scope(
            &mut self,
            restore_start: bool,
            recovery: Option<&recording::Invocation>,
        ) -> Result<(), ComponentError> {
            self.store
                .data()
                .cleanup
                .result()
                .map_err(|error| self.classify_trap_err(&error))?;
            self.retire_recording(GuestSpanStatus::Cancelled)
                .map_err(|e| self.classify_trap_err(&e))?;
            self.store.data_mut().trace_parent = None;
            if self.poisoned {
                // Retire poisoned guest allocations before allocating the next
                // instance, so rebuild cannot temporarily double the guest cap.
                // Host context remains available on failure; poisoned remains
                // true until all required exports have been validated.
                let mut store =
                    create_store(&self.engine, &self.cfg, self.store.data().host.clone())?;
                store.data_mut().next_span_handle = self.store.data().next_span_handle;
                self.store = store;
                let bindings = guest_boundary(|| {
                    ParrotComponent::instantiate(&mut self.store, self.component.as_ref())
                })
                .map_err(|e| init_error("re-instantiate after trap", e))?;
                self.bindings = bindings;
                if restore_start && self.started {
                    self.reset_budget()?;
                    self.begin_recovery_recording(recovery)?;
                    let scope = self
                        .store
                        .data()
                        .recording
                        .as_ref()
                        .map(recording::Invocation::execution_scope)
                        .transpose()
                        .map(Option::flatten);
                    let result = match scope {
                        Ok(scope) => {
                            let failures = self.store.data().cleanup.clone();
                            recording::scoped_owned(scope, failures, || {
                                self.bindings.on_start_fn.call(&mut self.store, ())
                            })
                        }
                        Err(error) => Err(error),
                    };
                    let result = self.exit_message_scope(result, None);
                    result.map_err(|error| self.classify_trap_err(&error))?;
                }
                self.poisoned = false;
            }
            self.reset_budget()
        }

        fn begin_recovery_recording(
            &mut self,
            source: Option<&recording::Invocation>,
        ) -> Result<(), ComponentError> {
            let Some(source) = source else { return Ok(()) };
            let target = format!("wasm/recovery-start:{}", self.store.data().host.self_path);
            if let Some(recording) = source
                .recovery(&target)
                .map_err(|error| self.classify_trap_err(&error))?
            {
                let parent = recording
                    .parent()
                    .map_err(|error| self.classify_trap_err(&error))?;
                self.store.data_mut().trace_parent = Some(parent);
                self.store.data_mut().recording = Some(recording);
            }
            Ok(())
        }

        fn reset_budget(&mut self) -> Result<(), ComponentError> {
            self.store.data_mut().host_calls = 0;
            self.store
                .set_fuel(self.cfg.fuel_per_message)
                .map_err(|e| ComponentError::AbiVersion(format!("message fuel: {e}")))?;
            self.store
                .set_epoch_deadline(self.cfg.epoch_deadline.max(1));
            Ok(())
        }

        fn retire_recording(&mut self, status: GuestSpanStatus) -> wasmtime::Result<()> {
            self.finish_recording(status, status)
        }

        fn finish_recording(
            &mut self,
            unfinished: GuestSpanStatus,
            completed: GuestSpanStatus,
        ) -> wasmtime::Result<()> {
            let result = self
                .store
                .data_mut()
                .recording
                .take()
                .map_or(Ok(()), |invocation| {
                    invocation.retire(unfinished, completed)
                });
            self.store.data().cleanup.record(&result);
            result
        }

        fn exit_message_scope<T>(
            &mut self,
            mut r: wasmtime::Result<T>,
            unfinished: Option<GuestSpanStatus>,
        ) -> wasmtime::Result<T> {
            if r.is_err() {
                self.store.data_mut().subscriptions.clear();
            }
            if let Err(error) = self.store.data().cleanup.result() {
                self.store.data_mut().subscriptions.clear();
                // Keep any returned host-owned value outside the unwind boundary.
                let _ = self.store.data().cleanup.dispose(r);
                r = Err(error);
            }
            let status = unfinished
                .filter(|_| r.is_ok())
                .unwrap_or_else(|| recording::outcome(&r));
            let completed = if r.is_ok() {
                unfinished.unwrap_or(GuestSpanStatus::Ok)
            } else {
                recording::outcome(&r)
            };
            if let Err(error) = self.finish_recording(status, completed) {
                r = Err(error);
            }
            self.store.data_mut().trace_parent = None;
            if let Ok(f) = self.store.get_fuel() {
                self.metrics.fuel_consumed = self
                    .metrics
                    .fuel_consumed
                    .saturating_add(self.cfg.fuel_per_message.saturating_sub(f));
            }
            r
        }

        fn classify_trap(&mut self, msg: String) -> ComponentError {
            self.metrics.trap_count = self.metrics.trap_count.saturating_add(1);
            ComponentError::Trap(msg)
        }

        fn classify_trap_err(&mut self, t: &wasmtime::Error) -> ComponentError {
            use wasmtime::Trap;
            self.store.data_mut().subscriptions.clear();
            let cleanup = self.store.data().cleanup.result();
            let t = cleanup.as_ref().err().unwrap_or(t);
            if let Some(HostPanic(message)) = t.downcast_ref::<HostPanic>() {
                self.metrics.trap_count = self.metrics.trap_count.saturating_add(1);
                self.poisoned = true;
                return ComponentError::Panic(message.clone());
            }
            match t.downcast_ref::<Trap>() {
                Some(Trap::OutOfFuel) => {
                    self.metrics.out_of_fuel_count =
                        self.metrics.out_of_fuel_count.saturating_add(1);
                    self.poisoned = true;
                    ComponentError::OutOfFuel
                }
                Some(Trap::Interrupt) => {
                    self.poisoned = true;
                    ComponentError::EpochDeadline
                }
                _ => {
                    self.metrics.trap_count = self.metrics.trap_count.saturating_add(1);
                    self.poisoned = true;
                    ComponentError::Trap(format!("{t:#}"))
                }
            }
        }

        /// 宿主上下文只读快照（测试/探活）。
        pub fn host_ctx(&self) -> &HostCtx {
            &self.store.data().host
        }
    }

    /// WasmActor：parrot-api Actor 适配（Executor 接线用——B2 扩展形态）。
    /// 注：parrot-wasm 不依赖 parrot-api（分层铁律——09 §2.2）；
    /// 适配在 parrot-node 侧完成，此处仅暴露句柄语义。
    impl WasmComponent {
        /// A physical host cleanup failure permanently prevents this component
        /// from executing again. This receipt survives cancellation; recreate
        /// the admitted actor under its owner's supervision rather than retrying
        /// an inner Store after resources failed to retire.
        pub fn cleanup_error(&self) -> Option<ComponentError> {
            self.store.data().cleanup.result().err().map(|error| {
                if let Some(HostPanic(message)) = error.downcast_ref::<HostPanic>() {
                    ComponentError::Panic(message.clone())
                } else {
                    ComponentError::Trap(format!("{error:#}"))
                }
            })
        }

        pub fn is_alive(&self) -> bool {
            self.cleanup_error().is_none()
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn hostile_panic_payload_destructor_is_contained() {
            const CHILD: &str = "PARROT_WASM_HOSTILE_PANIC_CHILD";
            if std::env::var_os(CHILD).is_none() {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "runtime::tests::hostile_panic_payload_destructor_is_contained",
                    ])
                    .env(CHILD, "1")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        assert!(status.success(), "panic boundary child failed: {status}");
                        return;
                    }
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("panic boundary child did not terminate");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            struct Secondary;
            impl Drop for Secondary {
                fn drop(&mut self) {
                    panic!("secondary payload must not be dropped");
                }
            }
            struct Payload;
            impl Drop for Payload {
                fn drop(&mut self) {
                    std::panic::panic_any(Secondary);
                }
            }
            let error = guest_boundary::<()>(|| std::panic::panic_any(Payload)).unwrap_err();
            assert_eq!(
                init_error("test", error),
                ComponentError::Panic("non-string host panic".into())
            );
            assert_eq!(guest_boundary(|| Ok(42)).unwrap(), 42);
        }

        #[test]
        fn cache_rejects_oversized_input_before_compilation() {
            let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
            let mut bytes = b"(component)".to_vec();
            bytes.resize(16 * 1024 * 1024 + 1, b' ');
            assert!(
                runtime.compile_cached(&bytes).is_err(),
                "valid but oversized input must be refused"
            );
        }

        #[test]
        fn cache_concurrent_misses_share_one_compiled_component() {
            let runtime = std::sync::Arc::new(
                WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap(),
            );
            let start = std::sync::Arc::new(std::sync::Barrier::new(8));
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    let runtime = runtime.clone();
                    let start = start.clone();
                    std::thread::spawn(move || {
                        start.wait();
                        runtime.compile_cached(b"(component)").unwrap()
                    })
                })
                .collect();
            let components: Vec<_> = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect();
            assert!(components
                .iter()
                .all(|c| wasmtime::component::Component::same(&components[0], c)));
        }

        #[test]
        fn cache_hit_can_progress_while_compilation_admission_is_busy() {
            let runtime = std::sync::Arc::new(
                WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap(),
            );
            let first = runtime.compile_cached(b"(component)").unwrap();
            let permit = runtime.compilation.lock().unwrap();
            let (sender, receiver) = std::sync::mpsc::channel();
            let worker_runtime = runtime.clone();
            let worker = std::thread::spawn(move || {
                sender
                    .send(worker_runtime.compile_cached(b"(component)"))
                    .unwrap();
            });
            let result = receiver.recv_timeout(std::time::Duration::from_secs(5));
            // Release the permit even on regression, so the worker can finish.
            drop(permit);
            worker.join().unwrap();
            let second = result
                .expect("cache hit blocked behind compilation")
                .unwrap();
            assert!(wasmtime::component::Component::same(&first, &second));
        }

        #[test]
        fn cache_evicts_oldest_at_default_entry_limit() {
            let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
            let first = runtime.compile_cached(b"(component)").unwrap();
            for i in 0..32 {
                runtime
                    .compile_cached(format!("(component (; {i} ;))").as_bytes())
                    .unwrap();
            }
            let recompiled = runtime.compile_cached(b"(component)").unwrap();
            assert!(
                !wasmtime::component::Component::same(&first, &recompiled),
                "unbounded cache retained the oldest component"
            );
        }

        #[test]
        fn cache_reuses_actual_compiled_identity_and_failed_compile_does_not_pollute_it() {
            let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
            let first = runtime.compile_cached(b"(component)").unwrap();
            let second = runtime.compile_cached(b"(component)").unwrap();
            assert!(wasmtime::component::Component::same(&first, &second));
            assert!(runtime.compile_cached(b"bad component").is_err());
            let different = runtime
                .compile_cached(b"(component (; different ;))")
                .unwrap();
            assert!(!wasmtime::component::Component::same(&first, &different));
            assert!(wasmtime::component::Component::same(
                &first,
                &runtime.compile_cached(b"(component)").unwrap()
            ));
        }

        #[test]
        fn automatic_epoch_survives_runtime_drop_and_joins_after_last_component() {
            use std::sync::atomic::Ordering;
            use std::time::{Duration, Instant};
            let runtime = WasmRuntime::new(WasmConfig {
                fuel_per_message: 100_000,
                // Setup is not the timed operation under test.
                epoch_deadline: 1_000_000,
                ..Default::default()
            })
            .unwrap();
            let fixture =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-component.wasm");
            let mut component = runtime.instantiate(&fixture, HostCtx::default()).unwrap();
            let driver = runtime.epoch_driver.as_ref().unwrap();
            let weak = std::sync::Arc::downgrade(driver);
            let ticks = driver.ticks.clone();
            let exited = driver.exited.clone();
            drop(runtime);
            assert!(weak.upgrade().is_some());

            // Observe the real owned ticker AFTER arming the actual store.
            // We deliberately avoid racing a hot spin loop against fuel.
            component.cfg.epoch_deadline = 2;
            component.enter_message_scope(None, None).unwrap();
            let target = ticks.load(Ordering::Acquire) + 2;
            let timeout = Instant::now() + Duration::from_secs(5);
            while ticks.load(Ordering::Acquire) < target {
                assert!(
                    Instant::now() < timeout,
                    "owned epoch ticker did not advance"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let error = component
                .bindings
                .handle_fn
                .call(
                    &mut component.store,
                    (Msg {
                        type_key: "spin".into(),
                        payload: vec![],
                    },),
                )
                .unwrap_err();
            assert_eq!(
                component.classify_trap_err(&error),
                ComponentError::EpochDeadline
            );
            assert!(component.store.get_fuel().unwrap() > 0);
            // Recovery keeps the same ticker lease. Use a large deadline for
            // the ordinary echo, independent of host scheduling delays.
            component.cfg.epoch_deadline = 1_000_000;
            assert_eq!(
                component.handle("echo", b"recovered").unwrap(),
                b"recovered"
            );
            drop(component);
            assert!(weak.upgrade().is_none());
            assert!(exited.load(Ordering::Acquire), "Drop must join the ticker");
        }

        #[test]
        fn unused_runtime_joins_its_driver_and_manual_mode_has_none() {
            use std::sync::atomic::Ordering;
            let runtime = WasmRuntime::new(WasmConfig::default()).unwrap();
            let exited = runtime.epoch_driver.as_ref().unwrap().exited.clone();
            drop(runtime);
            assert!(exited.load(Ordering::Acquire));
            let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
            assert!(runtime.epoch_driver.is_none());
        }

        #[test]
        fn instantiation_guest_start_has_the_configured_epoch_budget() {
            let cfg = WasmConfig {
                epoch_deadline: 2,
                ..Default::default()
            };
            let runtime = WasmRuntime::new_with_manual_epoch(cfg.clone()).unwrap();
            let mut store = create_store(runtime.engine(), &cfg, HostCtx::default()).unwrap();
            let module = wasmtime::Module::new(
                runtime.engine(),
                "(module (func $start (loop br 0)) (start $start))",
            )
            .unwrap();
            runtime.tick_epoch();
            runtime.tick_epoch();
            let error = wasmtime::Instance::new(&mut store, &module, &[]).unwrap_err();
            assert_eq!(
                error.downcast_ref::<wasmtime::Trap>(),
                Some(&wasmtime::Trap::Interrupt)
            );
            assert!(store.get_fuel().unwrap() > 0);
            assert_eq!(
                init_error("instantiate", error.context("guest initializer")),
                ComponentError::EpochDeadline
            );
        }

        #[test]
        fn instantiation_fuel_exhaustion_is_not_an_abi_mismatch() {
            let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
            let error = match runtime.instantiate_bytes(
                br"(component
                    (core module $guest
                        (func $start (loop br 0))
                        (start $start))
                    (core instance (instantiate $guest)))",
                HostCtx::default(),
            ) {
                Ok(_) => panic!("unbounded guest initializer must exhaust its finite fuel"),
                Err(error) => error,
            };
            assert_eq!(error, ComponentError::OutOfFuel);
            let error = match runtime.instantiate_bytes(b"(component)", HostCtx::default()) {
                Ok(_) => panic!("missing handler interface must remain invalid"),
                Err(error) => error,
            };
            assert!(
                matches!(error, ComponentError::AbiVersion(message) if message.contains("missing export"))
            );
        }

        #[test]
        fn all_core_memories_share_one_store_budget() {
            let cfg = WasmConfig {
                memory_limit_mb: 1,
                ..Default::default()
            };
            let runtime = WasmRuntime::new(cfg.clone()).unwrap();
            let mut store = create_store(runtime.engine(), &cfg, HostCtx::default()).unwrap();
            let first =
                wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(8, Some(9))).unwrap();
            let second =
                wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(7, None)).unwrap();
            // A module-local maximum must not consume the shared quota on failure.
            assert!(first.grow(&mut store, 2).is_err());
            assert_eq!(second.grow(&mut store, 1).unwrap(), 7);
            assert!(first.grow(&mut store, 1).is_err());
            assert!(wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(1, None)).is_err());
            assert_eq!(first.size(&store), 8);
            assert_eq!(second.size(&store), 8);
        }

        #[test]
        fn tables_share_an_element_budget_and_instances_are_bounded() {
            let cfg = WasmConfig::default();
            let runtime = WasmRuntime::new(cfg.clone()).unwrap();
            let mut store = create_store(runtime.engine(), &cfg, HostCtx::default()).unwrap();
            let first = wasmtime::Table::new(
                &mut store,
                wasmtime::TableType::new(wasmtime::RefType::FUNCREF, 32_768, Some(32_768)),
                wasmtime::Ref::Func(None),
            )
            .unwrap();
            let second = wasmtime::Table::new(
                &mut store,
                wasmtime::TableType::new(wasmtime::RefType::FUNCREF, 32_767, None),
                wasmtime::Ref::Func(None),
            )
            .unwrap();
            assert!(first
                .grow(&mut store, 1, wasmtime::Ref::Func(None))
                .is_err());
            assert_eq!(
                second
                    .grow(&mut store, 1, wasmtime::Ref::Func(None))
                    .unwrap(),
                32_767
            );
            assert!(second
                .grow(&mut store, 1, wasmtime::Ref::Func(None))
                .is_err());

            let mut store = create_store(runtime.engine(), &cfg, HostCtx::default()).unwrap();
            let module = wasmtime::Module::new(runtime.engine(), "(module)").unwrap();
            for _ in 0..limits::MAX_CORE_INSTANCES {
                wasmtime::Instance::new(&mut store, &module, &[]).unwrap();
            }
            assert!(wasmtime::Instance::new(&mut store, &module, &[]).is_err());
        }

        #[test]
        fn shared_memory_cannot_bypass_the_limiter() {
            let runtime = WasmRuntime::new(WasmConfig::default()).unwrap();
            assert!(
                wasmtime::Module::new(runtime.engine(), "(module (memory 1 2 shared))").is_err()
            );
        }

        #[test]
        fn allocator_failure_returns_reserved_guest_capacity() {
            use wasmtime::ResourceLimiter;
            let cfg = WasmConfig {
                memory_limit_mb: 1,
                ..Default::default()
            };
            let runtime = WasmRuntime::new(cfg.clone()).unwrap();
            let mut store = create_store(runtime.engine(), &cfg, HostCtx::default()).unwrap();
            // Fault injection at the allocator callback: no successful memory
            // allocation occurred, so the next real allocation must still fit.
            let limits = &mut store.data_mut().limits;
            assert!(limits.memory_growing(0, 1024 * 1024, None).unwrap());
            limits
                .memory_grow_failed(wasmtime::Error::msg("injected allocator failure"))
                .unwrap();
            assert!(limits
                .table_growing(0, limits::MAX_TABLE_ELEMENTS, None)
                .unwrap());
            limits
                .table_grow_failed(wasmtime::Error::msg("injected table failure"))
                .unwrap();
            wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(16, None)).unwrap();
            wasmtime::Table::new(
                &mut store,
                wasmtime::TableType::new(wasmtime::RefType::FUNCREF, 65_536, None),
                wasmtime::Ref::Func(None),
            )
            .unwrap();
        }

        #[derive(Debug)]
        struct RebuildRecording(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl GuestTrace for RebuildRecording {
            fn parent(&self) -> TraceParent {
                TraceParent {
                    trace_id: [1; 16],
                    span_id: [2; 8],
                }
            }
            fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
                None
            }
            fn start_recovery(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                panic!("failed rebuild must not start a lifecycle owner");
            }
        }

        #[test]
        fn failed_rebuild_returns_error_and_preserves_host_context() {
            // This test injects an export-contract failure on rebuild. A real
            // ticker must not interrupt the initial, otherwise valid fixture
            // before we reach that injection. Automatic epochs have a separate
            // ownership/interruption test above.
            let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
            let fixture =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-component.wasm");
            let mut component = runtime
                .instantiate(
                    &fixture,
                    HostCtx {
                        self_path: "/retained".into(),
                        ..Default::default()
                    },
                )
                .unwrap();
            component.on_start().unwrap();
            component.component = std::sync::Arc::new(
                wasmtime::component::Component::new(runtime.engine(), "(component)").unwrap(),
            );
            component.poisoned = true;
            let factories = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            for _ in 0..2 {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    component.handle_with_recording(
                        "echo",
                        b"retry",
                        Box::new(RebuildRecording(factories.clone())),
                    )
                }))
                .expect("rebuild failure must be returned, not panic");
                assert!(result.is_err());
                assert_eq!(component.host_ctx().self_path, "/retained");
                assert!(component.poisoned);
            }
            assert_eq!(factories.load(std::sync::atomic::Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn failed_async_rebuild_does_not_create_recovery_or_reuse_invalid_bindings() {
            let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
            let mut component = runtime
                .prepare_bytes(include_bytes!("../tests/fixtures/fixture-component.wasm"))
                .unwrap()
                .instantiate_async(
                    HostCtx {
                        self_path: "/retained".into(),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap();
            component.on_start_async(None).await.unwrap();
            component.component = std::sync::Arc::new(
                wasmtime::component::Component::new(runtime.engine(), "(component)").unwrap(),
            );
            component.poisoned = true;
            let factories = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            for _ in 0..2 {
                let result = component
                    .handle_async(
                        "echo",
                        b"retry",
                        Some(Box::new(RebuildRecording(factories.clone()))),
                    )
                    .await;
                assert!(matches!(result, Err(ComponentError::AbiVersion(_))));
                assert!(component.poisoned);
                assert_eq!(component.host_ctx().self_path, "/retained");
            }
            assert_eq!(factories.load(std::sync::atomic::Ordering::SeqCst), 0);
        }

        #[test]
        fn sandbox_epoch_deadline_is_relative_and_recovers_without_a_clock_race() {
            let fixture =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-component.wasm");
            for deadline in [1, 2, 7] {
                let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
                    fuel_per_message: 1_000,
                    epoch_deadline: deadline,
                    memory_limit_mb: 64,
                })
                .unwrap();
                let mut component = runtime.instantiate(&fixture, HostCtx::default()).unwrap();

                // Advance before entering the message: the new deadline must
                // be relative to the current engine epoch, not epoch zero.
                for _ in 0..10 {
                    runtime.tick_epoch();
                }
                assert_eq!(component.handle("echo", b"before").unwrap(), b"before");

                component.enter_message_scope(None, None).unwrap();
                for _ in 0..deadline - 1 {
                    runtime.tick_epoch();
                }
                let (before_deadline,) = component
                    .bindings
                    .handle_fn
                    .call(
                        &mut component.store,
                        (Msg {
                            type_key: "echo".into(),
                            payload: b"not expired".to_vec(),
                        },),
                    )
                    .unwrap();
                assert_eq!(before_deadline.unwrap(), b"not expired");
                runtime.tick_epoch();
                // Call the actual component after crossing the armed deadline.
                // Keep finite fuel so missing epoch wiring fails as OutOfFuel
                // instead of leaving an infinite guest running in the test.
                let error = component
                    .bindings
                    .handle_fn
                    .call(
                        &mut component.store,
                        (Msg {
                            type_key: "spin".into(),
                            payload: vec![],
                        },),
                    )
                    .unwrap_err();
                assert_eq!(
                    component.classify_trap_err(&error),
                    ComponentError::EpochDeadline,
                    "deadline={deadline}: {error:?}"
                );
                assert!(component.store.get_fuel().unwrap() > 0);
                assert_eq!(component.take_metrics().out_of_fuel_count, 0);
                assert_eq!(component.handle("echo", b"after").unwrap(), b"after");
            }
        }
    }
}

#[cfg(feature = "runtime")]
pub use runtime::{WasmComponent, WasmRuntime};
