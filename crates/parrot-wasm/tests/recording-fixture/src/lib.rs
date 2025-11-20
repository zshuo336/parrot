use std::cell::{Cell, RefCell};
thread_local! {
    static SAVED: Cell<u64> = const { Cell::new(0) };
    static START_IDENTITY: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static DRAIN_IDENTITY: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "recording-parrot-actor",
        with: {
            "parrot:trace/context@0.1.0": generate,
            "parrot:recording/spans@0.1.0": generate,
        },
    });
    use super::*;
    use exports::parrot::component::handler::{Guest, Msg};
    use parrot::recording::spans::{finish, start, RecordingError, Status};

    fn lifecycle(phase: &str) -> Vec<u8> {
        let span = match start(None, &format!("guest/{phase}")) {
            Ok(span) => span,
            Err(RecordingError::Disabled) => return Vec::new(),
            Err(error) => panic!("unexpected lifecycle admission: {error:?}"),
        };
        let parent = parrot::trace::context::current_parent().unwrap();
        assert_eq!(parent.trace_id, span.parent.trace_id);
        assert_ne!(parent.span_id, span.parent.span_id);
        let identity = [
            parent.trace_id,
            parent.span_id,
            span.parent.span_id,
            span.handle.to_le_bytes().to_vec(),
        ]
        .concat();
        let mode = parrot::component::ctx::config_get(&format!("recording-{phase}-mode"));
        match mode.as_deref() {
            Some("unfinished") => return identity,
            Some("trap") => core::arch::wasm32::unreachable(),
            Some("host-panic") => parrot::component::ctx::log(0, "lifecycle host panic gate"),
            Some("fuel" | "epoch") => {
                if mode.as_deref() == Some("epoch") {
                    parrot::component::ctx::log(0, "lifecycle epoch gate");
                }
                loop {
                    std::hint::spin_loop();
                }
            }
            Some("budget") => {
                finish(span.handle, Status::Ok).unwrap();
                for _ in 1..256 {
                    let span = start(None, "lifecycle/bounded").unwrap();
                    finish(span.handle, Status::Ok).unwrap();
                }
                assert_eq!(start(None, "too-many").unwrap_err(), RecordingError::Limit);
                return identity;
            }
            _ => {}
        }
        finish(span.handle, Status::Ok).unwrap();
        identity
    }

    pub struct Handler;
    impl Guest for Handler {
        fn handle(message: Msg) -> Result<Vec<u8>, String> {
            match message.type_key.as_str() {
                "disabled" => {
                    assert_eq!(
                        start(None, "disabled").unwrap_err(),
                        RecordingError::Disabled
                    );
                    return Ok(Vec::new());
                }
                "stale" => {
                    let handle = u64::from_le_bytes(message.payload.try_into().unwrap());
                    let active = start(None, "new-invocation").unwrap();
                    assert_ne!(active.handle, handle);
                    assert_eq!(
                        finish(handle, Status::Ok),
                        Err(RecordingError::UnknownHandle)
                    );
                    assert_eq!(
                        start(Some(handle), "stale-child").unwrap_err(),
                        RecordingError::UnknownHandle
                    );
                    finish(active.handle, Status::Ok).unwrap();
                    return Ok(active.handle.to_le_bytes().to_vec());
                }
                "budget" => {
                    assert_eq!(start(None, "").unwrap_err(), RecordingError::InvalidTarget);
                    assert_eq!(
                        start(None, &"x".repeat(257)).unwrap_err(),
                        RecordingError::InvalidTarget
                    );
                    assert_eq!(
                        start(None, &"界".repeat(86)).unwrap_err(),
                        RecordingError::InvalidTarget
                    );
                    for _ in 0..256 {
                        let span = start(None, "bounded").unwrap();
                        finish(span.handle, Status::Ok).unwrap();
                    }
                    assert_eq!(start(None, "too-many").unwrap_err(), RecordingError::Limit);
                    return Ok(Vec::new());
                }
                "active-budget" => {
                    for _ in 0..256 {
                        start(None, "unfinished/bounded").unwrap();
                    }
                    assert_eq!(start(None, "too-many").unwrap_err(), RecordingError::Limit);
                    return Ok(Vec::new());
                }
                "statuses" => {
                    for status in [
                        Status::Ok,
                        Status::Error,
                        Status::Cancelled,
                        Status::Panicked,
                    ] {
                        let span = start(None, &"x".repeat(256)).unwrap();
                        finish(span.handle, status).unwrap();
                        assert_eq!(
                            start(Some(span.handle), "retired-parent").unwrap_err(),
                            RecordingError::UnknownHandle
                        );
                    }
                    let span = start(None, &"界".repeat(85)).unwrap();
                    finish(span.handle, Status::Ok).unwrap();
                    return Ok(Vec::new());
                }
                "saved" => return Ok(SAVED.get().to_le_bytes().to_vec()),
                "start-identity" => return Ok(START_IDENTITY.with(|id| id.borrow().clone())),
                "drain-identity" => return Ok(DRAIN_IDENTITY.with(|id| id.borrow().clone())),
                _ => {}
            }
            let outer = start(None, "guest/outer").map_err(|e| format!("{e:?}"))?;
            SAVED.set(outer.handle);
            if message.type_key == "nested" {
                let inner = start(Some(outer.handle), "guest/inner").unwrap();
                assert_eq!(inner.parent.trace_id, outer.parent.trace_id);
                assert_ne!(inner.parent.span_id, outer.parent.span_id);
                finish(inner.handle, Status::Ok).unwrap();
                assert_eq!(
                    finish(inner.handle, Status::Ok),
                    Err(RecordingError::UnknownHandle)
                );
                finish(outer.handle, Status::Ok).unwrap();
                return Ok([
                    outer.parent.trace_id,
                    outer.parent.span_id,
                    inner.parent.span_id,
                ]
                .concat());
            }
            match message.type_key.as_str() {
                "unfinished-nested" => {
                    let inner = start(Some(outer.handle), "unfinished/inner").unwrap();
                    start(Some(inner.handle), "unfinished/leaf").unwrap();
                    Ok(outer.handle.to_le_bytes().to_vec())
                }
                "unfinished" => Ok(outer.handle.to_le_bytes().to_vec()),
                "error" => Err("guest business error".into()),
                "trap" => core::arch::wasm32::unreachable(),
                "host-panic" => {
                    parrot::component::ctx::log(0, "recording panic gate");
                    Ok(Vec::new())
                }
                "epoch" => {
                    parrot::component::ctx::log(0, "recording epoch gate");
                    loop {
                        std::hint::spin_loop();
                    }
                }
                "fuel" => loop {
                    std::hint::spin_loop();
                },
                _ => Err("unknown recording fixture message".into()),
            }
        }
        fn tell(message: Msg) {
            Self::handle(message).unwrap();
        }
        fn on_start() {
            START_IDENTITY.with(|id| *id.borrow_mut() = lifecycle("on-start"));
        }
        fn on_drain() {
            DRAIN_IDENTITY.with(|id| *id.borrow_mut() = lifecycle("on-drain"));
        }
    }
    export!(Handler);
}
