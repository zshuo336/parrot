//! parrot-wasm：Parrot wasm 组件运行时（DEV_09 C 阶段 / 09 §4.2）。
//!
//! 分层：
//! - `WasmConfig` / `HostCtx` / `WasmMetrics` / `ComponentError`：纯类型
//!   （default feature——引用方零 wasmtime 成本）
//! - `runtime` feature：`WasmRuntime` / `WasmComponent`（wasmtime 集成——
//!   fuel/epoch 沙箱、句柄表、panic 边界）
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
    /// 每消息 fuel 预算（耗尽 → `ComponentError::OutOfFuel` → 宿主转
    /// `ActorError::OverQuota` 语义交监督——与 native 同一套监督）。
    pub fuel_per_message: u64,
    /// epoch 增量预算（tokio 信号驱动递增——协作抢占边界）。
    pub epoch_deadline: u64,
    /// 线性内存上限（MB）。
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
    /// 日志级别下限（低于此级别的组件 log 吞掉）。
    pub log_level: u8,
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

    use super::{ComponentError, HostCtx, WasmConfig, WasmMetrics};
    use std::path::Path;

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

    /// Wasmtime 运行时（Engine + 预编译组件缓存——同 digest 复用）。
    pub struct WasmRuntime {
        engine: wasmtime::Engine,
        cfg: WasmConfig,
        /// 组件字节码缓存（digest → 预编译 Component）。
        cache: std::sync::Mutex<std::collections::HashMap<String, wasmtime::component::Component>>,
    }

    impl WasmRuntime {
        pub fn new(cfg: WasmConfig) -> Result<Self, ComponentError> {
            let mut ec = wasmtime::Config::new();
            ec.epoch_interruption(true)
                .consume_fuel(true)
                .wasm_memory64(false);
            let engine = wasmtime::Engine::new(&ec)
                .map_err(|e| ComponentError::AbiVersion(format!("engine init: {e}")))?;
            Ok(Self {
                engine,
                cfg,
                cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            })
        }

        pub fn config(&self) -> &WasmConfig {
            &self.cfg
        }

        /// 推进 engine epoch（宿主心跳线程/tokio 任务驱动——所有 store
        /// 的 deadline 以 engine epoch 为基准；测试中手动调用）。
        pub fn tick_epoch(&self) {
            self.engine.increment_epoch();
        }

        /// engine 引用（宿主心跳线程 clone 后自行递增 epoch）。
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
            let bytes = std::fs::read(wasm_path).map_err(|e| {
                ComponentError::AbiVersion(format!("read {}: {e}", wasm_path.display()))
            })?;
            let component = self.compile_cached(&bytes)?;
            let component = std::sync::Arc::new(component);
            let mut store = wasmtime::Store::new(&self.engine, ctx);
            store
                // 实例化预算：每消息预算 ×8，且不低于固定地板值（小预算
                // 配置下 instantiate 自身也不该被饿死）
                .set_fuel((self.cfg.fuel_per_message * 8).max(200_000))
                .map_err(|e| ComponentError::AbiVersion(format!("fuel: {e}")))?;
            store.epoch_deadline_trap();
            // 实例化阶段用宽 deadline（心跳线程可能已在跑——on_start 期间
            // 不该被 epoch 误伤）；进入消息作用域后再按配置收紧。
            store.set_epoch_deadline(1_000_000_000);
            let bindings = ParrotComponent::instantiate(&mut store, &component)
                .map_err(|e| ComponentError::AbiVersion(format!("instantiate: {e}")))?;
            Ok(WasmComponent {
                store,
                bindings,
                poisoned: false,
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
            // digest = 内容 hash（缓存键——同 digest 复用预编译产物）
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            let key = format!("{:016x}", h.finish());
            let mut cache = self.cache.lock().unwrap();
            if let Some(c) = cache.get(&key) {
                // Component: Clone 为内部 Arc 克隆——零拷贝复用
                return Ok(c.clone());
            }
            let comp = wasmtime::component::Component::new(&self.engine, bytes)
                .map_err(|e| ComponentError::AbiVersion(format!("compile: {e}")))?;
            cache.insert(key, comp.clone());
            Ok(comp)
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
            store: &mut wasmtime::Store<HostCtx>,
            comp: &wasmtime::component::Component,
        ) -> wasmtime::Result<Self> {
            let mut linker = wasmtime::component::Linker::new(store.engine());
            // ctx 能力（host functions——最小权限四件套；返回 Result 形态）
            let mut ctx = linker.instance("parrot:component/ctx@0.1.0")?;
            ctx.func_wrap(
                "self-ref",
                |caller: wasmtime::StoreContextMut<'_, HostCtx>, (): ()| {
                    Ok((caller.data().self_path.clone(),))
                },
            )?;
            ctx.func_wrap(
                "log",
                |caller: wasmtime::StoreContextMut<'_, HostCtx>, (level, msg): (u8, String)| {
                    let data = caller.data();
                    if level <= data.log_level.max(2) {
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
                |caller: wasmtime::StoreContextMut<'_, HostCtx>, (key,): (String,)| {
                    Ok((caller.data().config_overlay.get(&key).map(str::to_string),))
                },
            )?;
            ctx.func_wrap(
                "clock-now-ms",
                |_: wasmtime::StoreContextMut<'_, HostCtx>, (): ()| {
                    Ok((std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),))
                },
            )?;

            let instance = linker.instantiate(&mut *store, comp)?;
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
        store: wasmtime::Store<HostCtx>,
        bindings: ParrotComponent,
        /// trap 后 component instance 不可重入——置毒标记，下一消息
        /// 以全新 Store + 实例重建（宿主零污染；组件状态丢弃是
        /// 沙箱语义：失活组件的监督决策由宿主决定）。
        poisoned: bool,
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
            self.enter_message_scope();
            let r = self.bindings.handle_fn.call(
                &mut self.store,
                (Msg {
                    type_key: type_key.to_string(),
                    payload: payload.to_vec(),
                },),
            );
            self.exit_message_scope(&r);
            match r {
                Ok((Ok(out),)) => {
                    self.metrics.messages += 1;
                    Ok(out)
                }
                Ok((Err(e),)) => {
                    self.metrics.messages += 1;
                    Err(self.classify_trap(e))
                }
                Err(t) => Err(self.classify_trap_err(&t)),
            }
        }

        /// tell 入口（单向）。
        pub fn tell(&mut self, type_key: &str, payload: &[u8]) -> Result<(), ComponentError> {
            self.enter_message_scope();
            let r = self.bindings.tell_fn.call(
                &mut self.store,
                (Msg {
                    type_key: type_key.to_string(),
                    payload: payload.to_vec(),
                },),
            );
            self.exit_message_scope(&r);
            match r {
                Ok(()) => {
                    self.metrics.messages += 1;
                    Ok(())
                }
                Err(t) => Err(self.classify_trap_err(&t)),
            }
        }

        /// 启动钩子。
        pub fn on_start(&mut self) -> Result<(), ComponentError> {
            let r = self.bindings.on_start_fn.call(&mut self.store, ());
            match r {
                Ok(()) => Ok(()),
                Err(t) => Err(self.classify_trap_err(&t)),
            }
        }

        /// drain 钩子（升级排空语义交组件自述）。
        pub fn on_drain(&mut self) -> Result<(), ComponentError> {
            let r = self.bindings.on_drain_fn.call(&mut self.store, ());
            match r {
                Ok(()) => Ok(()),
                Err(t) => Err(self.classify_trap_err(&t)),
            }
        }

        /// 取走指标（清零——take 语义）。
        pub fn take_metrics(&mut self) -> WasmMetrics {
            std::mem::take(&mut self.metrics)
        }

        // ── 沙箱边界 ──

        fn enter_message_scope(&mut self) {
            // 每消息重置 fuel 颐算 + epoch deadline（消息间不叠加；
            // epoch 基准 = engine 当前 epoch（tick_epoch 递增）。
            // trap 后 instance 不可重入——重建 bindings 保证宿主不被污染）
            // trap 后原 Store/instance 不可重入——全新 Store 重建（宿主
            // ctx 保留；组件内部状态丢弃 = 沙箱失活语义）
            if self.poisoned {
                let host = std::mem::take(self.store.data_mut());
                let mut store = wasmtime::Store::new(&self.engine, host);
                store
                    .set_fuel((self.cfg.fuel_per_message * 8).max(200_000))
                    .expect("fuel for rebuild");
                store.epoch_deadline_trap();
                store.set_epoch_deadline(1_000_000_000);
                self.bindings = ParrotComponent::instantiate(&mut store, self.component.as_ref())
                    .expect("re-instantiate after trap");
                self.store = store;
                self.poisoned = false;
            }
            let _ = self.store.set_fuel(self.cfg.fuel_per_message);
            self.store
                .set_epoch_deadline(self.cfg.epoch_deadline.max(1));
        }

        fn exit_message_scope<T>(&mut self, _r: &Result<T, wasmtime::Error>) {
            if let Ok(f) = self.store.get_fuel() {
                self.metrics.fuel_consumed += self.cfg.fuel_per_message.saturating_sub(f);
            }
        }

        fn classify_trap(&mut self, msg: String) -> ComponentError {
            self.metrics.trap_count += 1;
            ComponentError::Trap(msg)
        }

        fn classify_trap_err(&mut self, t: &wasmtime::Error) -> ComponentError {
            use wasmtime::Trap;
            match t.downcast_ref::<Trap>() {
                Some(Trap::OutOfFuel) => {
                    self.metrics.out_of_fuel_count += 1;
                    self.poisoned = true;
                    ComponentError::OutOfFuel
                }
                Some(Trap::Interrupt) => {
                    self.poisoned = true;
                    ComponentError::EpochDeadline
                }
                _ => {
                    self.metrics.trap_count += 1;
                    self.poisoned = true;
                    ComponentError::Trap(t.to_string())
                }
            }
        }

        /// 宿主上下文只读快照（测试/探活）。
        pub fn host_ctx(&self) -> &HostCtx {
            self.store.data()
        }
    }

    /// WasmActor：parrot-api Actor 适配（Executor 接线用——B2 扩展形态）。
    /// 注：parrot-wasm 不依赖 parrot-api（分层铁律——09 §2.2）；
    /// 适配在 parrot-node 侧完成，此处仅暴露句柄语义。
    impl WasmComponent {
        pub fn is_alive(&self) -> bool {
            true // store 存活即活（drop 后句柄失效由 owner 管）
        }
    }
}

#[cfg(feature = "runtime")]
pub use runtime::{WasmComponent, WasmRuntime};
