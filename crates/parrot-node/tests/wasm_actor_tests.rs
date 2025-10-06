//! C 阶段接线测试（DEV_09 §3.3）：WasmActor 在真实 thread 引擎上的
//! 消息往返 + 沙箱语义（trap → MessageHandlingError；OutOfFuel 前缀辨识）。

#![cfg(feature = "wasm")]

use parrot::thread::system::ThreadActorSystem;
use parrot_api::address::ActorRefExt;
use parrot_node::{WasmActor, WasmAsk, WasmReply};
use parrot_wasm::runtime::WasmRuntime;
use parrot_wasm::{HostCtx, WasmConfig};
use std::sync::Arc;

fn fixture_path() -> std::path::PathBuf {
    // parrot-wasm 测试 fixture（crate 间共享——路径经 CARGO_MANIFEST_DIR 回溯）
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../parrot-wasm/tests/fixtures/fixture-component.wasm")
}

async fn spawn_wasm(
    ts: &Arc<ThreadActorSystem>,
    path: &str,
    fuel: u64,
) -> parrot_api::types::BoxedActorRef {
    let rt = WasmRuntime::new(WasmConfig {
        fuel_per_message: fuel,
        ..Default::default()
    })
    .unwrap();
    let ctx = HostCtx {
        self_path: path.to_string(),
        ..Default::default()
    };
    let comp = rt.instantiate(&fixture_path(), ctx).unwrap();
    let actor = WasmActor::new(comp).unwrap();
    let r = ts
        .spawn_at(
            actor,
            path,
            None,
            parrot::thread::config::ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    Box::new(r)
}

async fn ask(
    r: &parrot_api::types::BoxedActorRef,
    type_key: &str,
    payload: &[u8],
) -> Result<WasmReply, parrot_api::errors::ActorError> {
    r.ask(WasmAsk {
        type_key: type_key.into(),
        payload: payload.to_vec(),
    })
    .await
}

#[tokio::test]
async fn wasm_actor_echo_roundtrip() {
    let ts = ThreadActorSystem::shared(Default::default());
    let r = spawn_wasm(&ts, "/user/wasm-echo", 100_000).await;
    let reply = ask(&r, "echo", b"hello wasm actor").await.unwrap();
    assert_eq!(reply.0, b"hello wasm actor");
    ts.stop_actor("/user/wasm-echo").await.unwrap();
}

#[tokio::test]
async fn wasm_actor_state_across_messages() {
    let ts = ThreadActorSystem::shared(Default::default());
    let r = spawn_wasm(&ts, "/user/wasm-state", 100_000).await;
    let v1 = ask(&r, "state", &[]).await.unwrap();
    let v2 = ask(&r, "state", &[]).await.unwrap();
    assert_eq!(v1.0, 1u64.to_le_bytes());
    assert_eq!(v2.0, 2u64.to_le_bytes());
    ts.stop_actor("/user/wasm-state").await.unwrap();
}

#[tokio::test]
async fn wasm_actor_trap_reported_not_crash() {
    let ts = ThreadActorSystem::shared(Default::default());
    let r = spawn_wasm(&ts, "/user/wasm-err", 100_000).await;
    let e = ask(&r, "err", &[]).await.unwrap_err();
    assert!(e.to_string().contains("wasm trap"), "got: {e}");
    // 引擎仍活：后续消息照常
    let ok = ask(&r, "echo", b"after-trap").await.unwrap();
    assert_eq!(ok.0, b"after-trap");
    ts.stop_actor("/user/wasm-err").await.unwrap();
}

#[tokio::test]
async fn wasm_actor_out_of_fuel_identifiable() {
    let ts = ThreadActorSystem::shared(Default::default());
    // fuel=1000：echo 可能勉强够，spin 死循环必耗尽
    let r = spawn_wasm(&ts, "/user/wasm-fuel", 1_000).await;
    let e = ask(&r, "spin", &[]).await.unwrap_err();
    assert!(e.to_string().contains("OverQuota"), "got: {e}");
    // 毒化重建后引擎仍可服务（宿主不污染）
    let ok = ask(&r, "echo", b"recovered").await.unwrap();
    assert_eq!(ok.0, b"recovered");
    ts.stop_actor("/user/wasm-fuel").await.unwrap();
}

#[tokio::test]
async fn wasm_actor_self_path_visible() {
    let ts = ThreadActorSystem::shared(Default::default());
    let r = spawn_wasm(&ts, "/user/wasm-self", 100_000).await;
    let out = ask(&r, "self", &[]).await.unwrap();
    assert_eq!(out.0, b"/user/wasm-self");
    ts.stop_actor("/user/wasm-self").await.unwrap();
}
