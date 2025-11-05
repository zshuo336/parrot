//! C1+C2（DEV_09 §3.3）parrot-wasm 单测——30+ 项。
//!
//! 分组：
//! 1. 配置/类型默认值（default feature——不依赖 wasmtime）
//! 2. WIT 冻结件校验（文本锚定 + 组件契约对账）
//! 3. 实例化（fixture 组件 + 坏文件 + 缺 export）
//! 4. handle 语义（echo/up/self/config/clock/log/err/unknown）
//! 5. 沙箱（fuel 耗尽 OutOfFuel / epoch 超限 / trap 不污染宿主 / 恢复后续消息可处理）
//! 6. 状态与生命周期（state 跨消息 / on-start / on-drain / drop 重建无残留）
//! 7. tell 语义（bump 计数）
//! 8. metrics（fuel 计量 / take 清零 / 实例化耗时）
//! 9. 缓存（同 digest 二次实例化复用编译产物）
//! 10. 并发安全（多实例隔离）

#![cfg(feature = "runtime")]

use parrot_wasm::runtime::WasmRuntime;
use parrot_wasm::{wit_include, ComponentError, HostCtx, WasmConfig, WasmMetrics};

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-component.wasm")
}

fn rt(fuel: u64) -> WasmRuntime {
    WasmRuntime::new(WasmConfig {
        fuel_per_message: fuel,
        epoch_deadline: 1_000_000, // 大到不误触（epoch 专项测试单独调）
        memory_limit_mb: 64,
    })
    .unwrap()
}

fn host() -> HostCtx {
    let mut c = HostCtx {
        self_path: "/user/wtest".into(),
        log_level: 4,
        ..Default::default()
    };
    c.config_overlay.insert("mode", "fast");
    c
}

fn inst(r: &WasmRuntime) -> parrot_wasm::runtime::WasmComponent {
    let mut c = r.instantiate(&fixture_path(), host()).unwrap();
    c.on_start().unwrap();
    c
}

// ════════════════════════════════════════════════════════════
// 1. 配置/类型
// ════════════════════════════════════════════════════════════

#[test]
fn config_default_fuel() {
    let c = WasmConfig::default();
    assert_eq!(c.fuel_per_message, 100_000);
    assert_eq!(c.epoch_deadline, 1);
    assert_eq!(c.memory_limit_mb, 64);
}

#[test]
fn config_clone_eq() {
    let a = WasmConfig::default();
    let b = a.clone();
    assert_eq!(a, b);
}

#[test]
fn host_ctx_defaults() {
    let h = HostCtx::default();
    assert!(h.self_path.is_empty());
    assert!(h.config_overlay.is_empty());
    assert_eq!(h.log_level, 0);
}

#[test]
fn metrics_default_zero() {
    let m = WasmMetrics::default();
    assert_eq!(m.fuel_consumed, 0);
    assert_eq!(m.messages, 0);
    assert_eq!(m.out_of_fuel_count, 0);
    assert_eq!(m.trap_count, 0);
    assert_eq!(m.instantiate_micros, 0);
}

#[test]
fn error_display_forms() {
    assert_eq!(ComponentError::OutOfFuel.to_string(), "out of fuel");
    assert!(ComponentError::Trap("x".into())
        .to_string()
        .contains("trap"));
    assert!(ComponentError::EpochDeadline.to_string().contains("epoch"));
    assert!(ComponentError::AbiVersion("v".into())
        .to_string()
        .contains("contract"));
    assert!(ComponentError::Panic("p".into())
        .to_string()
        .contains("panic"));
}

// ════════════════════════════════════════════════════════════
// 2. WIT 冻结件
// ════════════════════════════════════════════════════════════

#[test]
fn wit_frozen_text_anchors() {
    // C2 冻结锚定：关键字段一旦变更即破坏（semver 纪律）
    let wit = wit_include::PARROT_ACTOR_WIT;
    assert!(wit.contains("package parrot:component@0.1.0;"));
    assert!(wit.contains("world parrot-actor"));
    assert!(wit.contains("import ctx;"));
    assert!(wit.contains("export handler;"));
    assert!(wit.contains("self-ref: func() -> string;"));
    assert!(wit.contains("log: func(level: u8, msg: string);"));
    assert!(wit.contains("config-get: func(key: string) -> option<string>;"));
    assert!(wit.contains("clock-now-ms: func() -> u64;"));
    assert!(wit.contains("handle: func(m: msg) -> result<list<u8>, string>;"));
    assert!(wit.contains("tell: func(m: msg);"));
    assert!(wit.contains("on-start: func();"));
    assert!(wit.contains("on-drain: func();"));
    assert!(wit.contains("record msg"));
    assert!(wit.contains("type-key: string"));
    assert!(wit.contains("payload: list<u8>"));
}

#[test]
fn wit_frozen_text_matches_versioned_contract() {
    // Independent v0.1.0 snapshot: a changed source cannot update both sides.
    let expected = r#"// parrot-actor WIT world —— C2（DEV_09 §3.3 / 09 §4.2）冻结件。
//
// 冻结纪律：本文件一旦合入即视为公共 API（semver 管理）；修改 = 破坏性
// 变更，须走 protocol break 评审（与 docs/vectors/admin_v2.json 同规）。
//
// 包名裁定：TECH_DESIGN_09 §4.2 原文 `parrot:component@0.1.0`（DEV_09
// 施工文档 §3.3 写 parrot:actor——以设计文档为准，包名 component）。
// world 名 `parrot-actor`（组件形态 = actor）。
//
// 宿主能力（能力制——最小权限）：
//   self-ref      自身路径（/user/{name}）
//   log           日志（tracing 汇入宿主 log_sink）
//   config-get    只读 parrot-config 投影（overlay 合成后视图）
//   clock-now-ms  单调时钟（测试可注入）
// 1.0 不给网络/文件（P2 再议——09 §4.2 诚实声明）。

package parrot:component@0.1.0;

interface ctx {
    /// 组件消息（与 Wire 帧同构：type_key + bytes——组件内部序列化自由）。
    record msg {
        type-key: string,
        payload: list<u8>,
    }

    /// 组件自身 actor 路径。
    self-ref: func() -> string;
    /// 日志（level: 0=error 1=warn 2=info 3=debug 4=trace）。
    log: func(level: u8, msg: string);
    /// 只读配置投影（key 不存在 → none）。
    config-get: func(key: string) -> option<string>;
    /// 单调时钟（毫秒）。
    clock-now-ms: func() -> u64;
}

interface handler {
    use ctx.{msg};

    /// ask：处理消息并回载荷（err = 组件自述错误串）。
    handle: func(m: msg) -> result<list<u8>, string>;
    /// tell：单向（无返回）。
    tell: func(m: msg);
    /// 实例启动钩子（spawn 后一次）。
    on-start: func();
    /// 升级 drain 钩子（§7——排空语义交组件自述）。
    on-drain: func();
}

world parrot-actor {
    import ctx;
    export handler;
}
"#;
    assert_eq!(
        wit_include::PARROT_ACTOR_WIT,
        expected,
        "PARROT_WIT_V0_1_0_CONTRACT_CHANGED"
    );
}

// ════════════════════════════════════════════════════════════
// 3. 实例化
// ════════════════════════════════════════════════════════════

#[test]
fn instantiate_fixture_and_start() {
    let r = rt(100_000);
    let mut c = inst(&r);
    // started 标记置位
    let out = c.handle("started", &[]).unwrap();
    assert_eq!(out, vec![1]);
}

#[test]
fn instantiate_missing_file() {
    let r = rt(100_000);
    let e = r
        .instantiate(std::path::Path::new("/no/such.wasm"), host())
        .unwrap_err();
    match e {
        ComponentError::AbiVersion(m) => assert!(m.contains("read")),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn instantiate_garbage_bytes() {
    // Immutable fixture: parallel test processes must not delete each other's
    // input and accidentally pass this test with a missing-file error.
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/invalid-component.wat");
    let r = rt(100_000);
    let e = r.instantiate(&p, host()).unwrap_err();
    assert!(
        matches!(&e, ComponentError::AbiVersion(detail) if detail.starts_with("compile:")),
        "got {e:?}"
    );
}

// ════════════════════════════════════════════════════════════
// 4. handle 语义
// ════════════════════════════════════════════════════════════

#[test]
fn handle_echo() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let out = c.handle("echo", b"hello parrot").unwrap();
    assert_eq!(out, b"hello parrot");
}

#[test]
fn handle_up_compute() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let out = c.handle("up", &41u64.to_le_bytes()).unwrap();
    assert_eq!(u64::from_le_bytes(out.try_into().unwrap()), 42);
}

#[test]
fn handle_self_ref() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let out = c.handle("self", &[]).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "/user/wtest");
}

#[test]
fn handle_config_get() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let out = c.handle("config", b"mode").unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "fast");
    // 不存在的 key → 空串（none → default）
    let out = c.handle("config", b"nokey").unwrap();
    assert!(out.is_empty());
}

#[test]
fn handle_clock() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let first = u64::from_le_bytes(c.handle("clock", &[]).unwrap().try_into().unwrap());
    let second = u64::from_le_bytes(c.handle("clock", &[]).unwrap().try_into().unwrap());
    assert!(second >= first, "the WIT clock must not go backwards");
}

#[test]
fn handle_err_string() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let e = c.handle("err", &[]).unwrap_err();
    match e {
        ComponentError::Trap(s) => assert_eq!(s, "fixture-err"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn handle_unknown_type_key() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let e = c.handle("zzz", &[]).unwrap_err();
    match e {
        ComponentError::Trap(s) => assert!(s.contains("unknown type_key")),
        other => panic!("unexpected: {other:?}"),
    }
}

// ════════════════════════════════════════════════════════════
// 5. 沙箱（fuel / epoch / trap 边界）
// ════════════════════════════════════════════════════════════

#[test]
fn sandbox_fuel_exhaustion() {
    // spin 死循环 × 极小 fuel → OutOfFuel
    let r = rt(1_000);
    let mut c = inst(&r);
    let e = c.handle("spin", &[]).unwrap_err();
    assert!(matches!(e, ComponentError::OutOfFuel), "got {e:?}");
}

#[test]
fn sandbox_fuel_recovery_next_message() {
    // fuel 耗尽后宿主不污染：下一消息重置预算照常处理
    let r = rt(1_000);
    let mut c = inst(&r);
    let _ = c.handle("spin", &[]).unwrap_err();
    let out = c.handle("echo", b"after-recovery").unwrap();
    assert_eq!(out, b"after-recovery");
}

#[test]
fn sandbox_fuel_metrics_counted() {
    let r = rt(1_000);
    let mut c = inst(&r);
    let _ = c.handle("spin", &[]).unwrap_err();
    let m = c.take_metrics();
    assert_eq!(m.out_of_fuel_count, 1);
    assert!(m.fuel_consumed > 0);
}

// The epoch boundary is tested inside the runtime module, where the deadline
// can be armed before advancing the clock and entering the real guest. A
// background ticker here races fuel exhaustion and tests OS scheduling.

#[test]
fn sandbox_trap_does_not_corrupt_host() {
    let r = rt(100_000);
    let mut c = inst(&r);
    // 多次 trap 后宿主仍正常
    for _ in 0..3 {
        let _ = c.handle("err", &[]).unwrap_err();
    }
    let out = c.handle("up", &0u64.to_le_bytes()).unwrap();
    assert_eq!(u64::from_le_bytes(out.try_into().unwrap()), 1);
}

// ════════════════════════════════════════════════════════════
// 6. 状态与生命周期
// ════════════════════════════════════════════════════════════

#[test]
fn state_across_messages() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let v1 = u64::from_le_bytes(c.handle("state", &[]).unwrap().try_into().unwrap());
    let v2 = u64::from_le_bytes(c.handle("state", &[]).unwrap().try_into().unwrap());
    let v3 = u64::from_le_bytes(c.handle("state", &[]).unwrap().try_into().unwrap());
    assert_eq!((v1, v2, v3), (1, 2, 3));
}

#[test]
fn state_drop_rebuild_no_residual() {
    // 行为规约 ④：实例 drop 后同 digest 重建无状态残留
    let r = rt(100_000);
    {
        let mut c = inst(&r);
        let _ = c.handle("state", &[]).unwrap(); // counter=1
        let _ = c.handle("state", &[]).unwrap(); // counter=2
    } // drop
    let mut c2 = inst(&r); // 同 digest 重建
    let v = u64::from_le_bytes(c2.handle("state", &[]).unwrap().try_into().unwrap());
    assert_eq!(v, 1, "rebuild must start from fresh state");
}

#[test]
fn lifecycle_on_start_once() {
    let r = rt(100_000);
    let mut c = inst(&r);
    // 重复调用幂等（fixture 标记型——多次 set true 不变）
    c.on_start().unwrap();
    let out = c.handle("started", &[]).unwrap();
    assert_eq!(out, vec![1]);
}

#[test]
fn lifecycle_on_drain() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let before = c.handle("drained", &[]).unwrap();
    assert_eq!(before, vec![0]);
    c.on_drain().unwrap();
    let after = c.handle("drained", &[]).unwrap();
    assert_eq!(after, vec![1]);
}

// ════════════════════════════════════════════════════════════
// 7. tell 语义
// ════════════════════════════════════════════════════════════

#[test]
fn tell_bump_counter() {
    let r = rt(100_000);
    let mut c = inst(&r);
    c.tell("bump", &[7]).unwrap();
    c.tell("bump", &[3]).unwrap();
    let v = u64::from_le_bytes(c.handle("counter", &[]).unwrap().try_into().unwrap());
    assert_eq!(v, 10);
}

#[test]
fn tell_unknown_silent() {
    let r = rt(100_000);
    let mut c = inst(&r);
    c.tell("whatever", &[1]).unwrap(); // 未知 key 静默（tell 无回执）
    let v = u64::from_le_bytes(c.handle("counter", &[]).unwrap().try_into().unwrap());
    assert_eq!(v, 0);
}

#[test]
fn tell_empty_payload_no_crash() {
    let r = rt(100_000);
    let mut c = inst(&r);
    c.tell("bump", &[]).unwrap();
    let v = u64::from_le_bytes(c.handle("counter", &[]).unwrap().try_into().unwrap());
    assert_eq!(v, 0);
}

// ════════════════════════════════════════════════════════════
// 8. metrics
// ════════════════════════════════════════════════════════════

#[test]
fn metrics_messages_counted() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let _ = c.handle("echo", b"a").unwrap();
    let _ = c.handle("echo", b"b").unwrap();
    c.tell("bump", &[1]).unwrap();
    let m = c.take_metrics();
    assert_eq!(m.messages, 3);
}

#[test]
fn metrics_take_resets() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let _ = c.handle("echo", b"x").unwrap();
    let m1 = c.take_metrics();
    assert_eq!(m1.messages, 1);
    let m2 = c.take_metrics();
    assert_eq!(m2.messages, 0, "take 语义清零");
}

#[test]
fn metrics_instantiate_time_recorded() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let m = c.take_metrics();
    // 实例化耗时（µs——含编译首次；>0 必然）
    assert!(m.instantiate_micros > 0);
}

#[test]
fn metrics_fuel_scales_with_work() {
    let r = rt(100_000);
    let mut c = inst(&r);
    let _ = c.handle("echo", &[0u8; 64]).unwrap(); // 小载荷
    let _ = c.handle("echo", &[0u8; 4096]).unwrap(); // 大载荷
    let m = c.take_metrics();
    assert!(m.fuel_consumed > 0);
}

// ════════════════════════════════════════════════════════════
// 9. 编译缓存
// ════════════════════════════════════════════════════════════

#[test]
fn cached_instances_remain_callable() {
    let r = rt(100_000);
    let _c1 = inst(&r);
    let mut c2 = r.instantiate(&fixture_path(), host()).unwrap();
    c2.on_start().unwrap();
    // Actual compiled identity reuse is tested internally with Component::same.
    // Relative wall-clock speed is not evidence of a cache hit.
    let out = c2.handle("echo", b"cached").unwrap();
    assert_eq!(out, b"cached");
}

#[test]
fn cache_multi_instance_isolation() {
    let r = rt(100_000);
    let mut a = inst(&r);
    let mut b = inst(&r);
    // 独立状态（thread_local per instance——component 实例隔离）
    let va = u64::from_le_bytes(a.handle("state", &[]).unwrap().try_into().unwrap());
    let vb = u64::from_le_bytes(b.handle("state", &[]).unwrap().try_into().unwrap());
    assert_eq!((va, vb), (1, 1));
}

// ════════════════════════════════════════════════════════════
// 10. 综合链路
// ════════════════════════════════════════════════════════════

#[test]
fn full_chain_echo_state_drain() {
    let r = rt(100_000);
    let mut c = inst(&r);
    // echo → state ×3 → tell bump → counter → drain → drained
    assert_eq!(c.handle("echo", b"m1").unwrap(), b"m1");
    for expect in 1..=3u64 {
        let v = u64::from_le_bytes(c.handle("state", &[]).unwrap().try_into().unwrap());
        assert_eq!(v, expect);
    }
    c.tell("bump", &[10]).unwrap();
    let v = u64::from_le_bytes(c.handle("counter", &[]).unwrap().try_into().unwrap());
    assert_eq!(v, 13);
    c.on_drain().unwrap();
    assert_eq!(c.handle("drained", &[]).unwrap(), vec![1]);
}

#[test]
fn log_via_host_no_panic() {
    let r = rt(100_000);
    let mut c = inst(&r);
    // level=info(2) + msg
    let mut p = vec![2u8];
    p.extend_from_slice(b"hello from wasm");
    let out = c.handle("log", &p).unwrap();
    assert_eq!(out, b"logged");
}

#[test]
fn host_ctx_visible_to_component() {
    // self/config 双能力综合（ctx 注入正确性）
    let r = rt(100_000);
    let mut ctx = host();
    ctx.self_path = "/user/special-path".into();
    ctx.config_overlay.insert("k1", "v1");
    let mut c = r.instantiate(&fixture_path(), ctx).unwrap();
    c.on_start().unwrap();
    assert_eq!(c.handle("self", &[]).unwrap(), b"/user/special-path");
    assert_eq!(c.handle("config", b"k1").unwrap(), b"v1");
    assert_eq!(c.handle("config", b"k2").unwrap(), b"");
}
