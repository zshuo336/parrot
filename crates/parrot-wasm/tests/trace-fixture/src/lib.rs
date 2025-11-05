use std::cell::{Cell, RefCell};

thread_local! {
    static CALLS: Cell<u64> = const { Cell::new(0) };
    static SAVED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static START_PARENT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static DRAIN_PARENT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "traced-parrot-actor",
        with: { "parrot:trace/context@0.1.0": generate },
    });

    use super::*;
    use exports::parrot::component::handler::{Guest, Msg};
    use parrot::trace::context::current_parent;

    fn parent_bytes() -> Vec<u8> {
        current_parent().map_or_else(Vec::new, |parent| {
            assert_eq!(parent.trace_id.len(), 16);
            assert_eq!(parent.span_id.len(), 8);
            [parent.trace_id, parent.span_id].concat()
        })
    }

    pub struct Handler;

    impl Guest for Handler {
        fn handle(message: Msg) -> Result<Vec<u8>, String> {
            if message.type_key == "calls" {
                return Ok(CALLS.get().to_le_bytes().to_vec());
            }
            CALLS.set(CALLS.get() + 1);
            match message.type_key.as_str() {
                "parent" => Ok(parent_bytes()),
                "saved-parent" => Ok(SAVED.with(|saved| saved.borrow().clone())),
                "start-parent" => Ok(START_PARENT.with(|saved| saved.borrow().clone())),
                "drain-parent" => Ok(DRAIN_PARENT.with(|saved| saved.borrow().clone())),
                "echo" => Ok(message.payload),
                "remember" | "err" | "trap" | "host-panic" | "epoch" => {
                    SAVED.with(|saved| *saved.borrow_mut() = parent_bytes());
                    match message.type_key.as_str() {
                        "remember" => Ok(Vec::new()),
                        "err" => Err("fixture-trace-error".into()),
                        "host-panic" => {
                            parrot::component::ctx::log(0, "trace fixture host panic");
                            Ok(Vec::new())
                        }
                        "epoch" => {
                            parrot::component::ctx::log(0, "trace fixture epoch gate");
                            loop {
                                std::hint::spin_loop();
                            }
                        }
                        _ => core::arch::wasm32::unreachable(),
                    }
                }
                "spin" => loop {
                    std::hint::spin_loop();
                },
                other => Err(format!("unknown trace fixture message: {other}")),
            }
        }

        fn tell(message: Msg) {
            CALLS.set(CALLS.get() + 1);
            assert_eq!(message.type_key, "remember");
            SAVED.with(|saved| *saved.borrow_mut() = parent_bytes());
        }

        fn on_start() {
            START_PARENT.with(|saved| *saved.borrow_mut() = parent_bytes());
        }

        fn on_drain() {
            DRAIN_PARENT.with(|saved| *saved.borrow_mut() = parent_bytes());
        }
    }

    export!(Handler);
}
