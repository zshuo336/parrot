//! C 阶段测试 fixture 组件（wit/parrot-actor.wit 冻结世界实现）。
//!
//! 行为（type_key 路由——辨识值断言用）：
//!   "echo"    → 回 payload 原样（echo 语义）
//!   "up"      → 回 payload u64 LE + 1（计算语义）
//!   "self"    → 回 ctx.self-ref() 路径
//!   "config"  → 回 ctx.config-get(key=payload utf8) 值（none → 空）
//!   "clock"   → 回 ctx.clock-now-ms() u64 LE
//!   "log"     → ctx.log(level=payload[0], msg=payload[1..]) → 回 "logged"
//!   "err"     → Err("fixture-err")——Trap 路径
//!   "spin"    → 死循环（fuel 耗尽——OutOfFuel 路径）
//!   "state"   → 内部 counter+1 后回 u64 LE（跨消息状态验证）
//!   "counter" → 回 counter 当前值（不增）
//!   "started" → 回 on-start 是否已调（1/0）
//!   "drained" → 回 on-drain 是否已调（1/0）
//!   tell("bump") → counter += payload[0]（tell 语义）

use std::cell::Cell;

thread_local! {
    static COUNTER: Cell<u64> = const { Cell::new(0) };
    static STARTED: Cell<bool> = const { Cell::new(false) };
    static DRAINED: Cell<bool> = const { Cell::new(false) };
}

mod bindings {
    // 冻结 WIT 相对路径（fixture manifest 目录 → parrot-wasm/wit）
    wit_bindgen::generate!({
        path: "../../wit",
        world: "parrot-actor",
    });

    use super::*;
    use exports::parrot::component::handler::{Guest, Msg};
    use parrot::component::ctx::{clock_now_ms, config_get, log, self_ref};

    pub struct Handler;

    impl Guest for Handler {
        fn handle(m: Msg) -> Result<Vec<u8>, String> {
            match m.type_key.as_str() {
                "echo" => Ok(m.payload),
                "up" => {
                    let n = u64::from_le_bytes(
                        m.payload.try_into().map_err(|_| "up: need 8B".to_string())?,
                    );
                    Ok((n + 1).to_le_bytes().to_vec())
                }
                "self" => Ok(self_ref().into_bytes()),
                "config" => {
                    let key = String::from_utf8(m.payload).map_err(|e| e.to_string())?;
                    Ok(config_get(&key).unwrap_or_default().into_bytes())
                }
                "clock" => Ok(clock_now_ms().to_le_bytes().to_vec()),
                "log" => {
                    let (level, rest) = m.payload.split_first().ok_or("log: empty")?;
                    log(
                        *level,
                        &String::from_utf8(rest.to_vec()).map_err(|e| e.to_string())?,
                    );
                    Ok(b"logged".to_vec())
                }
                "err" => Err("fixture-err".to_string()),
                "spin" => loop {
                    std::hint::spin_loop();
                },
                "state" => {
                    let v = COUNTER.with(|c| {
                        c.set(c.get() + 1);
                        c.get()
                    });
                    Ok(v.to_le_bytes().to_vec())
                }
                "counter" => Ok(COUNTER.with(|c| c.get()).to_le_bytes().to_vec()),
                "started" => Ok(vec![u8::from(STARTED.get())]),
                "drained" => Ok(vec![u8::from(DRAINED.get())]),
                other => Err(format!("unknown type_key: {other}")),
            }
        }

        fn tell(m: Msg) {
            match m.type_key.as_str() {
                "bump" => {
                    if let Some(b) = m.payload.first() {
                        COUNTER.with(|c| c.set(c.get() + *b as u64));
                    }
                }
                _ => {}
            }
        }

        fn on_start() {
            STARTED.with(|s| s.set(true));
        }

        fn on_drain() {
            DRAINED.with(|d| d.set(true));
        }
    }

    export!(Handler);
}
