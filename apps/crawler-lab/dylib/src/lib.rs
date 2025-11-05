//! Trusted native crawler business component. Tick performs deterministic
//! synthesis, tokenization, deduplication and outlink calculation. The host
//! executes the returned page/term/frontier batches over its owned gateways.
//! Configuration is a TOML `settings` string containing the shared JSON config.
use crawler_lab_core::{Config, Error, Hub};
use parrot_abi::{
    AbiComponent, AbiHooks, AbiMeta, AbiMsg, AbiReplyBuf, AbiStr, AbiVt, ABI_ERR_OVERFLOW,
    ABI_ERR_PANIC, ABI_ERR_STATE, ABI_OK, PARROT_ABI_VERSION,
};

unsafe extern "C" fn construct(cfg: AbiStr, out: *mut *mut AbiComponent) -> u32 {
    *out = std::ptr::null_mut();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if cfg.len > 65536 {
            return Err(());
        }
        let text = cfg.as_str();
        let config = if text.is_empty() || text == "{}" {
            Config::default()
        } else {
            let table = toml::from_str::<toml::Table>(text).map_err(|_| ())?;
            // Public native hosts wrap application values in [config] and
            // reserve actor_path alongside it. Also accept direct settings for
            // standalone ABI callers using the historical constructor surface.
            let settings = match table.get("config") {
                Some(value) => value.as_table().ok_or(())?,
                None => &table,
            };
            match settings.get("settings") {
                Some(value) => {
                    Config::from_json(value.as_str().ok_or(())?.as_bytes()).map_err(|_| ())?
                }
                None => Config::default(),
            }
        };
        let hub = Box::new(Hub::new(config).map_err(|_| ())?);
        let component = Box::new(AbiComponent {
            vt: &VT,
            self_: Box::into_raw(hub).cast(),
        });
        *out = Box::into_raw(component);
        Ok::<_, ()>(())
    }));
    match result {
        Ok(Ok(())) => ABI_OK,
        Ok(Err(())) => ABI_ERR_STATE,
        Err(_) => ABI_ERR_PANIC,
    }
}
unsafe extern "C" fn destroy(component: *mut AbiComponent) {
    if !component.is_null() {
        drop(Box::from_raw((*component).self_.cast::<Hub>()));
        drop(Box::from_raw(component));
    }
}
unsafe extern "C" fn handle_msg(this: *mut (), message: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    (*out).written = 0;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let hub = &mut *this.cast::<Hub>();
        let bytes = hub.handle(message.key(), message.bytes(), (*out).cap as usize)?;
        if !(*out).write(&bytes) {
            return Err(Error::Overflow);
        }
        Ok(())
    }));
    match result {
        Ok(Ok(())) => ABI_OK,
        Ok(Err(Error::Overflow)) => ABI_ERR_OVERFLOW,
        Ok(Err(_)) => ABI_ERR_STATE,
        Err(_) => ABI_ERR_PANIC,
    }
}
unsafe extern "C" fn on_drain(this: *mut ()) -> u32 {
    (&mut *this.cast::<Hub>()).drain();
    ABI_OK
}
static VT: AbiVt = AbiVt {
    handle_msg,
    on_drain,
};
#[no_mangle]
pub static PARROT_ABI_META: AbiMeta = AbiMeta {
    abi_version: PARROT_ABI_VERSION,
    parrot_min: 1,
    name: c"crawler-hub".as_ptr().cast(),
    name_len: 11,
    hooks: AbiHooks { construct, destroy },
};

#[cfg(test)]
mod tests {
    use super::*;

    struct Instance(*mut AbiComponent);
    impl Instance {
        fn new() -> Self {
            let mut ptr = std::ptr::null_mut();
            assert_eq!(unsafe { construct(AbiStr::of_str("{}"), &mut ptr) }, ABI_OK);
            assert!(!ptr.is_null());
            Self(ptr)
        }
    }
    impl Drop for Instance {
        fn drop(&mut self) {
            unsafe {
                on_drain((*self.0).self_);
                destroy(self.0);
            }
        }
    }

    fn check_reply(key: &str, payload: &[u8], expected: &[u8]) {
        for capacity in 0..=expected.len() + 1 {
            let instance = Instance::new();
            let mut storage = vec![0xa5; capacity + 2];
            let mut reply = AbiReplyBuf::new(&mut storage[1..capacity + 1]);
            let status = unsafe {
                ((*(*instance.0).vt).handle_msg)(
                    (*instance.0).self_,
                    AbiMsg::new(key, payload),
                    &mut reply,
                )
            };
            assert_eq!(storage[0], 0xa5);
            assert_eq!(storage[capacity + 1], 0xa5);
            if capacity < expected.len() {
                assert_eq!(status, ABI_ERR_OVERFLOW, "{key} at capacity {capacity}");
                assert_eq!(reply.written, 0);
                assert!(storage.iter().all(|&byte| byte == 0xa5));
            } else {
                assert_eq!(status, ABI_OK, "{key} at capacity {capacity}");
                assert_eq!(reply.written as usize, expected.len());
                assert_eq!(&storage[1..1 + expected.len()], expected);
            }
        }
    }

    #[test]
    fn stat_reply_obeys_capacity_and_returns_real_counter() {
        check_reply("crawl/Stat", &[], &1u64.to_le_bytes());
    }
    #[test]
    fn tick_reply_obeys_capacity_without_committing_on_failure() {
        let request =
            crawler_lab_core::encode_request(1, &[(0, crawler_lab_core::url_of(0), 0)]).unwrap();
        let mut hub = crawler_lab_core::Hub::new(Default::default()).unwrap();
        let expected = hub
            .handle("crawl/Tick", &request, crawler_lab_core::MAX_BYTES)
            .unwrap();
        check_reply("crawl/Tick", &request, &expected);
    }
    #[test]
    fn echo_reply_obeys_capacity_and_preserves_payload() {
        check_reply("crawl/Echo", b"hello", b"hello");
    }

    #[test]
    fn constructor_reads_host_config_and_rejects_invalid_settings() {
        for config in [
            "config = 1",
            "settings = 1",
            "[config]\nsettings = 'invalid json'",
        ] {
            let mut pointer = std::ptr::null_mut();
            assert_eq!(
                unsafe { construct(AbiStr::of_str(config), &mut pointer) },
                ABI_ERR_STATE
            );
            assert!(pointer.is_null());
        }
        let mut pointer = std::ptr::null_mut();
        let config = r#"actor_path = "/user/hub"
[config]
settings = '{"pages":0,"depth":0,"fanout":0,"batch":1}'
"#;
        assert_eq!(
            unsafe { construct(AbiStr::of_str(config), &mut pointer) },
            ABI_OK
        );
        let instance = Instance(pointer);
        let request =
            crawler_lab_core::encode_request(1, &[(0, crawler_lab_core::url_of(0), 0)]).unwrap();
        let mut bytes = [0; 4096];
        let mut reply = AbiReplyBuf::new(&mut bytes);
        assert_eq!(
            unsafe {
                handle_msg(
                    (*instance.0).self_,
                    AbiMsg::new("crawl/Tick", &request),
                    &mut reply,
                )
            },
            ABI_ERR_STATE
        );
        assert_eq!(reply.written, 0);
    }

    #[test]
    fn rejected_reply_capacity_does_not_commit_handled_state() {
        let instance = Instance::new();
        let mut small = [0; 1];
        let mut reply = AbiReplyBuf::new(&mut small);
        assert_eq!(
            unsafe {
                handle_msg(
                    (*instance.0).self_,
                    AbiMsg::new("crawl/Echo", b"hello"),
                    &mut reply,
                )
            },
            ABI_ERR_OVERFLOW
        );
        let mut bytes = [0; 8];
        let mut reply = AbiReplyBuf::new(&mut bytes);
        assert_eq!(
            unsafe {
                handle_msg(
                    (*instance.0).self_,
                    AbiMsg::new("crawl/Stat", &[]),
                    &mut reply,
                )
            },
            ABI_OK
        );
        assert_eq!(
            u64::from_le_bytes(bytes),
            1,
            "rejected Echo was committed before reply capacity was checked"
        );
    }
}
