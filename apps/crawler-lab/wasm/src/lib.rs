//! crawler-lab parrot 组件（wasm 形态——R2 双形态之一）。
//!
//! R1/R2（应用体系架构纠正）：本 crate 是 apps/crawler-lab 的业务代码。
//! 构建产物 crawler-hub.wasm（component model）经 parrot 标准包分发：
//! crawler.app.toml 声明 `artifact = { Wasm = { uri, digest } }`，parrot
//! 网关 deploy 时经 parrot-wasm runtime 实例化（fuel/epoch 沙箱）。
//!
//! 消息契约（与 dylib 形态同键——双形态行为一致锚点）：
//!   "crawl/Echo"  → payload 原样回
//!   "crawl/Stat"  → 内部消息计数 u64 LE
//!   "crawl/Tick"  → 编排一跳（预留）→ 0u64
//!   "crawl/Self"  → ctx.self-ref()（沙箱宿主能力探针）

use std::cell::Cell;

thread_local! {
    static MSGS: Cell<u64> = const { Cell::new(0) };
}

mod bindings {
    // 冻结 WIT 相对路径（app wasm 目录 → parrot-wasm/wit）
    wit_bindgen::generate!({
        path: "../../../crates/parrot-wasm/wit",
        world: "parrot-actor",
    });

    use super::*;
    use exports::parrot::component::handler::{Guest, Msg};
    use parrot::component::ctx::{log, self_ref};

    pub struct Handler;

    impl Guest for Handler {
        fn handle(m: Msg) -> Result<Vec<u8>, String> {
            let n = MSGS.with(|c| {
                c.set(c.get() + 1);
                c.get()
            });
            match m.type_key.as_str() {
                "crawl/Echo" => Ok(m.payload),
                "crawl/Stat" => Ok(n.to_le_bytes().to_vec()),
                "crawl/Tick" => Ok(0u64.to_le_bytes().to_vec()),
                "crawl/Self" => {
                    let p = self_ref();
                    log(2, &format!("crawler-hub(wasm) at {p}"));
                    Ok(p.into_bytes())
                }
                other => Err(format!("unknown type_key: {other}")),
            }
        }

        fn tell(m: Msg) {
            if m.type_key == "crawl/Bump" {
                if let Some(b) = m.payload.first() {
                    MSGS.with(|c| c.set(c.get() + *b as u64));
                }
            }
        }

        fn on_start() {
            log(2, "crawler-hub(wasm) started");
        }

        fn on_drain() {
            log(2, "crawler-hub(wasm) drained");
        }
    }

    export!(Handler);
}
