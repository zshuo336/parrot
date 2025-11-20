use std::cell::Cell;
thread_local! {
    static HANDLE: Cell<u64> = const { Cell::new(0) };
}

mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "services-parrot-actor",
        with: {
            "parrot:trace/context@0.1.0": generate,
            "parrot:recording/spans@0.1.0": generate,
            "parrot:messaging/messages@0.1.0": generate,
            "parrot:services/discovery@0.1.0": generate,
        },
    });
    use super::*;
    use exports::parrot::component::handler::{Guest, Msg};
    use parrot::component::ctx::config_get;
    use parrot::services::discovery::{
        close, deregister, next, register, subscribe, Event, ServiceError,
    };

    fn timeout() -> u32 {
        config_get("timeout-ms")
            .map(|v| v.parse().unwrap())
            .unwrap_or(5000)
    }
    fn key(payload: &[u8]) -> String {
        if payload.is_empty() {
            config_get("key").unwrap_or_else(|| "edge/guest-service".into())
        } else {
            String::from_utf8(payload.to_vec()).unwrap()
        }
    }
    fn error(error: ServiceError) -> Vec<u8> {
        let name = match error {
            ServiceError::Disabled => "Disabled",
            ServiceError::Denied => "Denied",
            ServiceError::InvalidRequest => "InvalidRequest",
            ServiceError::InvalidHandle => "InvalidHandle",
            ServiceError::Limit => "Limit",
            ServiceError::Timeout => "Timeout",
            ServiceError::Closed => "Closed",
            ServiceError::Failed => "Failed",
        };
        format!("error:{name}").into_bytes()
    }
    fn result(value: Result<(), ServiceError>) -> Vec<u8> {
        value.map(|()| b"ok".to_vec()).unwrap_or_else(error)
    }
    fn handle_id(payload: &[u8]) -> u64 {
        if payload.is_empty() {
            HANDLE.get()
        } else {
            u64::from_le_bytes(payload.try_into().unwrap())
        }
    }
    fn next_result(value: Result<Option<Event>, ServiceError>) -> Vec<u8> {
        match value {
            Ok(Some(event)) => format!(
                "{}:{}",
                if event.registered {
                    "registered"
                } else {
                    "unregistered"
                },
                event.path
            )
            .into_bytes(),
            Ok(None) => b"closed".to_vec(),
            Err(e) => error(e),
        }
    }
    fn next_then_write(handle: u64, timeout: u32) -> Vec<u8> {
        let event = next(handle, timeout);
        register("edge/after-next", timeout).unwrap();
        parrot::messaging::messages::tell(
            "/user/after-next",
            &parrot::messaging::messages::Msg {
                type_key: "bin:after-next#v1".into(),
                payload: vec![42],
            },
            timeout,
        )
        .unwrap();
        next_result(event)
    }
    pub struct Handler;
    impl Guest for Handler {
        fn handle(message: Msg) -> Result<Vec<u8>, String> {
            let timeout = timeout();
            Ok(match message.type_key.as_str() {
                "register" => result(register(&key(&message.payload), timeout)),
                "deregister" => result(deregister(&key(&message.payload), timeout)),
                "subscribe" => match subscribe(&key(&message.payload), timeout) {
                    Ok(handle) => {
                        HANDLE.set(handle);
                        handle.to_le_bytes().to_vec()
                    }
                    Err(e) => error(e),
                },
                "next" | "poll" => next_result(next(
                    handle_id(&message.payload),
                    if message.type_key == "poll" {
                        1
                    } else {
                        timeout
                    },
                )),
                "next-then-write" => next_then_write(handle_id(&message.payload), timeout),
                "close" => result(close(handle_id(&message.payload))),
                "discover-ask" => {
                    let handle = subscribe(&key(&[]), timeout).map_err(|e| format!("{e:?}"))?;
                    let event = next(handle, timeout);
                    close(handle).map_err(|e| format!("{e:?}"))?;
                    let event = event.map_err(|e| format!("{e:?}"))?.ok_or("no service")?;
                    if !event.registered {
                        return Err("service withdrawn".into());
                    }
                    match parrot::messaging::messages::ask(
                        &event.path,
                        &parrot::messaging::messages::Msg {
                            type_key: "bin:parrot.wasm.Ask#v1".into(),
                            payload: message.payload,
                        },
                        timeout,
                    ) {
                        Ok(reply) if reply.type_key == "bin:parrot.wasm.Reply#v1" => reply.payload,
                        Ok(_) => return Err("unexpected discovered reply".into()),
                        Err(error) => return Err(format!("discovered ask: {error:?}")),
                    }
                }
                "handle" => HANDLE.get().to_le_bytes().to_vec(),
                "quota" => {
                    for _ in 0..64 {
                        let _ = register("edge/forbidden", timeout);
                    }
                    result(register(&key(&message.payload), timeout))
                }
                "mixed-quota" => {
                    for _ in 0..32 {
                        let _ = register("edge/forbidden", timeout);
                        let _ = parrot::messaging::messages::tell(
                            "/user/forbidden",
                            &parrot::messaging::messages::Msg {
                                type_key: "bin:forbidden#v1".into(),
                                payload: vec![],
                            },
                            timeout,
                        );
                    }
                    result(register(&key(&message.payload), timeout))
                }
                "zero-timeout" => result(register(&key(&message.payload), 0)),
                "big-timeout" => result(register(&key(&message.payload), 30_001)),
                "oversize-key" => result(register(&"x".repeat(1025), timeout)),
                "trap" => core::arch::wasm32::unreachable(),
                "echo" => message.payload,
                _ => return Err("unknown fixture command".into()),
            })
        }
        fn tell(message: Msg) {
            Self::handle(message).unwrap();
        }
        fn on_start() {
            if let Some(key) = config_get("register-on-start") {
                register(&key, timeout()).unwrap();
            }
            if let Some(key) = config_get("subscribe-on-start") {
                HANDLE.set(subscribe(&key, timeout()).unwrap());
            }
            if config_get("next-on-start").as_deref() == Some("true") {
                next_then_write(HANDLE.get(), timeout());
            }
            if config_get("trap-on-start").as_deref() == Some("true") {
                core::arch::wasm32::unreachable();
            }
        }
        fn on_drain() {
            if config_get("next-on-drain").as_deref() == Some("true") {
                next_then_write(HANDLE.get(), timeout());
            }
            if let Some(key) = config_get("deregister-on-drain") {
                deregister(&key, timeout()).unwrap();
            }
        }
    }
    export!(Handler);
}
