use std::cell::Cell;
thread_local! {
    static CALLS: Cell<u64> = const { Cell::new(0) };
    static STARTED: Cell<bool> = const { Cell::new(false) };
    static DRAINED: Cell<bool> = const { Cell::new(false) };
}

mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "messaging-parrot-actor",
        with: {
            "parrot:trace/context@0.1.0": generate,
            "parrot:recording/spans@0.1.0": generate,
            "parrot:messaging/messages@0.1.0": generate,
        },
    });
    use super::*;
    use exports::parrot::component::handler::{Guest, Msg};
    use parrot::component::ctx::{config_get, self_ref};
    use parrot::messaging::messages::{ask, tell, MessagingError, Msg as WireMsg};

    fn settings() -> (String, String, u32) {
        (
            config_get("destination").unwrap_or_else(|| "/user/native".into()),
            config_get("request-key").unwrap_or_else(|| "bin:wasm.messaging.Request#v1".into()),
            config_get("timeout-ms")
                .map(|value| value.parse().unwrap())
                .unwrap_or(5000),
        )
    }
    fn reply(result: Result<WireMsg, MessagingError>) -> Vec<u8> {
        match result {
            Ok(message) => [message.type_key.into_bytes(), vec![0], message.payload].concat(),
            Err(error) => error_bytes(error),
        }
    }
    fn error_bytes(error: MessagingError) -> Vec<u8> {
        let name = match error {
            MessagingError::Disabled => "Disabled",
            MessagingError::Denied => "Denied",
            MessagingError::InvalidRequest => "InvalidRequest",
            MessagingError::Limit => "Limit",
            MessagingError::Timeout => "Timeout",
            MessagingError::UnknownTarget => "UnknownTarget",
            MessagingError::UnknownType => "UnknownType",
            MessagingError::Codec => "Codec",
            MessagingError::Failed => "Failed",
            MessagingError::UnexpectedReply => "UnexpectedReply",
        };
        format!("error:{name}").into_bytes()
    }
    fn notify(key: &str) {
        if let Some(value) = config_get(key) {
            let (destination, type_key, timeout) = settings();
            tell(
                &destination,
                &WireMsg {
                    type_key,
                    payload: vec![value.parse().unwrap()],
                },
                timeout,
            )
            .unwrap();
        }
    }
    pub struct Handler;
    impl Guest for Handler {
        fn handle(message: Msg) -> Result<Vec<u8>, String> {
            match message.type_key.as_str() {
                "count" => return Ok(CALLS.get().to_le_bytes().to_vec()),
                "started" => return Ok(vec![u8::from(STARTED.get())]),
                "drained" => return Ok(vec![u8::from(DRAINED.get())]),
                "parent" => {
                    return Ok(parrot::trace::context::current_parent()
                        .map(|parent| [parent.trace_id, parent.span_id].concat())
                        .unwrap_or_default())
                }
                "echo" => return Ok(message.payload),
                "trap" => core::arch::wasm32::unreachable(),
                _ => {}
            }
            CALLS.set(CALLS.get() + 1);
            // Intentionally unfinished: host retirement must close this span
            // on success, trap, cancellation and host panic.
            let _ = parrot::recording::spans::start(None, "guest/message-await");
            let (mut destination, mut type_key, mut timeout) = settings();
            let mut payload = message.payload;
            match message.type_key.as_str() {
                "self" => destination = self_ref(),
                "denied-target" => destination = "/user/forbidden".into(),
                "denied-type" => type_key = "bin:wasm.messaging.Forbidden#v1".into(),
                "zero-timeout" => timeout = 0,
                "big-timeout" => timeout = 30_001,
                "long-path" => destination = "x".repeat(4097),
                "long-key" => type_key = "x".repeat(1025),
                "oversize" => payload.resize(1024 * 1024 + 1, 0),
                "quota" | "disabled-quota" => {
                    for _ in 0..64 {
                        let result = ask(
                            "/user/forbidden",
                            &WireMsg {
                                type_key: type_key.clone(),
                                payload: vec![],
                            },
                            timeout,
                        );
                        assert!(match message.type_key.as_str() {
                            "disabled-quota" => matches!(result, Err(MessagingError::Disabled)),
                            _ => matches!(result, Err(MessagingError::Denied)),
                        });
                    }
                    return Ok(reply(ask(
                        &destination,
                        &WireMsg { type_key, payload },
                        timeout,
                    )));
                }
                _ => {}
            }
            let message = WireMsg { type_key, payload };
            if config_get("mode").as_deref() == Some("tell") {
                return Ok(match tell(&destination, &message, timeout) {
                    Ok(()) => b"admitted".to_vec(),
                    Err(error) => error_bytes(error),
                });
            }
            Ok(reply(ask(&destination, &message, timeout)))
        }
        fn tell(message: Msg) {
            Self::handle(message).unwrap();
        }
        fn on_start() {
            let _ = parrot::recording::spans::start(None, "guest/start-await");
            STARTED.set(true);
            notify("start-value");
        }
        fn on_drain() {
            let _ = parrot::recording::spans::start(None, "guest/drain-await");
            DRAINED.set(true);
            notify("drain-value");
        }
    }
    export!(Handler);
}
