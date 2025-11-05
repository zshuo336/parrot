//! Sandboxed crawler business. Same Tick/Stat/Progress/Echo protocol and
//! settings as the native component; WIT retains its no-network capability set.
use crawler_lab_core::{Config, Hub, MAX_BYTES};
use std::cell::RefCell;
thread_local! { static HUB: RefCell<Option<Hub>> = const { RefCell::new(None) }; }
mod bindings {
    wit_bindgen::generate!({ path: "../../../crates/parrot-wasm/wit", world: "parrot-actor" });
    use super::*;
    use exports::parrot::component::handler::{Guest, Msg};
    use parrot::component::ctx::{config_get, log, self_ref};
    pub struct Handler;
    impl Guest for Handler {
        fn handle(message: Msg) -> Result<Vec<u8>, String> {
            if message.type_key == "crawl/Self" {
                if !message.payload.is_empty() {
                    return Err("Self requires empty payload".into());
                }
                return HUB.with(|hub| {
                    hub.borrow_mut()
                        .as_mut()
                        .ok_or("crawler not started")?
                        .handle("crawl/Echo", self_ref().as_bytes(), MAX_BYTES)
                        .map_err(|e| e.to_string())
                });
            }
            HUB.with(|hub| {
                hub.borrow_mut()
                    .as_mut()
                    .ok_or("crawler not started")?
                    .handle(&message.type_key, &message.payload, MAX_BYTES)
                    .map_err(|e| e.to_string())
            })
        }
        fn tell(message: Msg) {
            if let Err(error) = Self::handle(message) {
                log(0, &error);
            }
        }
        fn on_start() {
            let config = match config_get("settings") {
                Some(value) => {
                    Config::from_json(value.as_bytes()).expect("invalid crawler settings")
                }
                None => Config::default(),
            };
            HUB.with(|hub| {
                *hub.borrow_mut() = Some(Hub::new(config).expect("invalid crawler settings"))
            });
            log(2, "crawler business started");
        }
        fn on_drain() {
            HUB.with(|hub| {
                if let Some(hub) = hub.borrow_mut().as_mut() {
                    hub.drain();
                }
            });
        }
    }
    export!(Handler);
}
