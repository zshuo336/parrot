#![cfg(feature = "loader")]

mod common;
use parrot_abi::{AbiMsg, AbiReplyBuf, AbiTraceParentV1, DylibLoader, ABI_ERR_STATE};

#[test]
fn extension_layout_is_independent_of_the_frozen_v1_layouts() {
    assert_eq!(
        std::mem::size_of::<parrot_abi::AbiServicesV1>(),
        std::mem::size_of::<usize>()
    );
    assert_eq!(
        std::mem::size_of::<parrot_abi::AbiServicesHostV1>(),
        6 * std::mem::size_of::<usize>()
    );
    assert_eq!(
        std::mem::size_of::<parrot_abi::AbiMessagingV1>(),
        std::mem::size_of::<usize>()
    );
    assert_eq!(
        std::mem::size_of::<parrot_abi::AbiMessagingHostV1>(),
        3 * std::mem::size_of::<usize>()
    );
    assert_eq!(std::mem::size_of::<AbiTraceParentV1>(), 24);
    assert_eq!(std::mem::align_of::<AbiTraceParentV1>(), 1);
    assert_eq!(std::mem::offset_of!(AbiTraceParentV1, span_id), 16);
    assert_eq!(
        std::mem::size_of::<parrot_abi::AbiTraceV1>(),
        std::mem::size_of::<usize>()
    );
    assert_eq!(parrot_abi::PARROT_ABI_VERSION, 1);
    assert_eq!(
        std::mem::size_of::<parrot_abi::AbiRecordingV1>(),
        std::mem::size_of::<usize>()
    );
    assert_eq!(std::mem::offset_of!(parrot_abi::AbiSpanV1, parent), 8);
}

#[test]
fn messaging_guest_preserves_legacy_entries_and_null_capabilities_grant_nothing() {
    let _lock = common::load_mutex();
    let path = common::testcomp().with_file_name(format!(
        "libparrot_abi_messagingcomp.{}",
        std::env::consts::DLL_EXTENSION
    ));
    // SAFETY: the owned fixture provides immutable versioned descriptors and
    // synchronous non-unwinding hooks. Each call has live disjoint buffers.
    let handle = unsafe { DylibLoader::load(&path, "") }.unwrap();
    assert!(handle.supports_messaging_v1());
    assert!(handle.supports_services_v1());
    let component = DylibLoader.construct(&handle, "[config]\n").unwrap();
    let mut bytes = [0; 64];
    let mut out = AbiReplyBuf::new(&mut bytes);
    unsafe {
        for mode in 0..4 {
            let msg = AbiMsg::new("echo", b"legacy behavior");
            match mode {
                0 => DylibLoader.handle_msg(&handle, component, msg, &mut out),
                1 => DylibLoader.handle_msg_with_trace(&handle, component, msg, &mut out, None),
                2 => DylibLoader
                    .handle_msg_with_recording(&handle, component, msg, &mut out, None, None),
                _ => DylibLoader.handle_msg_with_capabilities(
                    &handle, component, msg, &mut out, None, None, None,
                ),
            }
            .unwrap();
            assert_eq!(out.written_slice(), b"legacy behavior");
        }
        assert_eq!(
            DylibLoader
                .handle_msg_with_capabilities(
                    &handle,
                    component,
                    AbiMsg::new("ask", b"no host"),
                    &mut out,
                    None,
                    None,
                    None
                )
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
        assert_eq!(
            DylibLoader
                .handle_msg_with_services(
                    &handle,
                    component,
                    AbiMsg::new("service:register", b""),
                    &mut out,
                    None,
                    None,
                    None,
                    None
                )
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
        DylibLoader.drain_instance(&handle, component).unwrap();
        assert_eq!(
            DylibLoader
                .handle_msg_with_capabilities(
                    &handle,
                    component,
                    AbiMsg::new("echo", b"retired"),
                    &mut out,
                    None,
                    None,
                    None
                )
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
    }
    DylibLoader.unload_blocking(handle).unwrap();
}

#[test]
fn old_guest_falls_back_and_keeps_original_error_and_drain_semantics() {
    let _lock = common::load_mutex();
    // SAFETY: owned fixture implements the trusted native ABI contract.
    let handle = unsafe { DylibLoader::load(&common::testcomp(), "") }.unwrap();
    assert!(!handle.supports_trace_v1());
    assert!(!handle.supports_recording_v1());
    let component = DylibLoader.construct(&handle, "").unwrap();
    let parent = AbiTraceParentV1 {
        trace_id: [17; 16],
        span_id: [23; 8],
    };
    let mut bytes = [0; 64];
    let mut out = AbiReplyBuf::new(&mut bytes);
    // SAFETY: exclusive registered instance and live disjoint borrowed buffers.
    unsafe {
        DylibLoader
            .handle_msg_with_trace(
                &handle,
                component,
                AbiMsg::new("echo", b"legacy"),
                &mut out,
                Some(&parent),
            )
            .unwrap();
        assert_eq!(out.written_slice(), b"legacy");
        assert_eq!(
            DylibLoader
                .handle_msg_with_trace(
                    &handle,
                    component,
                    AbiMsg::new("err", b""),
                    &mut out,
                    Some(&parent)
                )
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
        DylibLoader.drain_instance(&handle, component).unwrap();
        assert_eq!(
            DylibLoader
                .handle_msg_with_trace(
                    &handle,
                    component,
                    AbiMsg::new("echo", b""),
                    &mut out,
                    Some(&parent)
                )
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
    }
    DylibLoader.unload_blocking(handle).unwrap();
}

#[test]
fn recording_guest_preserves_old_entries_and_validates_parent_before_dispatch() {
    let _lock = common::load_mutex();
    let path = common::testcomp().with_file_name(format!(
        "libparrot_abi_recordingcomp.{}",
        std::env::consts::DLL_EXTENSION
    ));
    // SAFETY: owned fixture implements all three synchronous message entries.
    let handle = unsafe { DylibLoader::load(&path, "") }.unwrap();
    assert!(handle.supports_trace_v1());
    assert!(handle.supports_recording_v1());
    let component = DylibLoader.construct(&handle, "[config]\n").unwrap();
    let parent = AbiTraceParentV1 {
        trace_id: [17; 16],
        span_id: [23; 8],
    };
    let mut bytes = [0; 64];
    let mut out = AbiReplyBuf::new(&mut bytes);
    // SAFETY: exclusive live instance, original vtable and disjoint live views.
    unsafe {
        let call = (*(*component).vt).handle_msg;
        assert_eq!(
            call(
                (*component).self_,
                AbiMsg::new("echo", b"old host"),
                &mut out
            ),
            parrot_abi::ABI_OK
        );
        assert_eq!(out.written_slice(), b"old host");
        DylibLoader
            .handle_msg(&handle, component, AbiMsg::new("parent", b""), &mut out)
            .unwrap();
        assert_eq!(out.written, 0);
        for recording in [false, true] {
            if recording {
                DylibLoader
                    .handle_msg_with_recording(
                        &handle,
                        component,
                        AbiMsg::new("parent", b""),
                        &mut out,
                        Some(&parent),
                        None,
                    )
                    .unwrap();
            } else {
                DylibLoader
                    .handle_msg_with_trace(
                        &handle,
                        component,
                        AbiMsg::new("parent", b""),
                        &mut out,
                        Some(&parent),
                    )
                    .unwrap();
            }
            assert_eq!(
                out.written_slice(),
                [parent.trace_id.as_slice(), &parent.span_id].concat()
            );
        }
        let invalid = AbiTraceParentV1 {
            trace_id: [0; 16],
            ..parent
        };
        assert_eq!(
            DylibLoader
                .handle_msg_with_recording(
                    &handle,
                    component,
                    AbiMsg::new("echo", b"must not run"),
                    &mut out,
                    Some(&invalid),
                    None
                )
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
        DylibLoader
            .handle_msg_with_recording(
                &handle,
                component,
                AbiMsg::new("calls", b""),
                &mut out,
                None,
                None,
            )
            .unwrap();
        assert_eq!(out.written_slice(), 5u64.to_le_bytes());
        DylibLoader.drain_instance(&handle, component).unwrap();
        assert_eq!(
            DylibLoader
                .handle_msg_with_trace(
                    &handle,
                    component,
                    AbiMsg::new("parent", b""),
                    &mut out,
                    Some(&parent),
                )
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
        assert_eq!(
            DylibLoader
                .handle_msg_with_recording(
                    &handle,
                    component,
                    AbiMsg::new("echo", b"closed"),
                    &mut out,
                    None,
                    None
                )
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
    }
    DylibLoader.unload_blocking(handle).unwrap();
}

#[test]
fn old_host_vtable_can_still_call_new_guest_without_an_extension() {
    let _lock = common::load_mutex();
    let path = common::testcomp().with_file_name(format!(
        "libparrot_abi_hostcomp.{}",
        std::env::consts::DLL_EXTENSION
    ));
    // SAFETY: owned fixture with immutable ABI v1 and optional trace v1 metadata.
    let handle = unsafe { DylibLoader::load(&path, "") }.unwrap();
    let component = DylibLoader
        .construct(&handle, "self_path = '/legacy-host'\n[config]\n")
        .unwrap();
    let mut bytes = [0; 64];
    let mut out = AbiReplyBuf::new(&mut bytes);
    // SAFETY: exclusive live instance, valid original vtable and live buffers.
    // Deliberately invoke only the frozen v1 entry point, as an old host does.
    unsafe {
        let call = (*(*component).vt).handle_msg;
        assert_eq!(
            call(
                (*component).self_,
                AbiMsg::new("echo", b"old host"),
                &mut out
            ),
            parrot_abi::ABI_OK
        );
        assert_eq!(out.written_slice(), b"old host");
        assert_eq!(
            call(
                (*component).self_,
                AbiMsg::new("trace-parent", b""),
                &mut out
            ),
            parrot_abi::ABI_OK
        );
        assert_eq!(out.written, 0);
    }
    DylibLoader.unload_blocking(handle).unwrap();
}

#[test]
fn new_guest_observes_each_call_and_rejects_invalid_ids_without_native_effects() {
    let _lock = common::load_mutex();
    let path = common::testcomp().with_file_name(format!(
        "libparrot_abi_hostcomp.{}",
        std::env::consts::DLL_EXTENSION
    ));
    // SAFETY: owned fixture implements both versioned native contracts.
    let handle = unsafe { DylibLoader::load(&path, "") }.unwrap();
    assert!(handle.supports_trace_v1());
    let component = DylibLoader
        .construct(&handle, "self_path = '/test'\n[config]\n")
        .unwrap();
    let first = AbiTraceParentV1 {
        trace_id: [19; 16],
        span_id: [7; 8],
    };
    let second = AbiTraceParentV1 {
        trace_id: [31; 16],
        span_id: [9; 8],
    };
    for parent in [Some(first), None, Some(second), Some(first), None] {
        let mut bytes = [0; 64];
        let mut out = AbiReplyBuf::new(&mut bytes);
        // SAFETY: exclusive registered instance, live input/output/parent views.
        unsafe {
            DylibLoader
                .handle_msg_with_trace(
                    &handle,
                    component,
                    AbiMsg::new("trace-parent", b""),
                    &mut out,
                    parent.as_ref(),
                )
                .unwrap();
            let expected = parent.map_or_else(Vec::new, |p| {
                [p.trace_id.as_slice(), p.span_id.as_slice()].concat()
            });
            assert_eq!(out.written_slice(), expected);
        }
    }
    let mut bytes = [0; 64];
    let mut out = AbiReplyBuf::new(&mut bytes);
    // SAFETY: all views remain live and exclusively borrowed for each call.
    unsafe {
        for parent in [
            AbiTraceParentV1 {
                trace_id: [0; 16],
                ..first
            },
            AbiTraceParentV1 {
                span_id: [0; 8],
                ..first
            },
        ] {
            assert_eq!(
                DylibLoader
                    .handle_msg_with_trace(
                        &handle,
                        component,
                        AbiMsg::new("echo", b"must not run"),
                        &mut out,
                        Some(&parent)
                    )
                    .unwrap_err(),
                (ABI_ERR_STATE, "invalid trace parent".into())
            );
        }
        DylibLoader
            .handle_msg(&handle, component, AbiMsg::new("messages", b""), &mut out)
            .unwrap();
        assert_eq!(out.written_slice(), 6u64.to_le_bytes());
        assert_eq!(
            DylibLoader
                .handle_msg_with_trace(
                    &handle,
                    component,
                    AbiMsg::new("error", b""),
                    &mut out,
                    Some(&first)
                )
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
        DylibLoader
            .handle_msg(
                &handle,
                component,
                AbiMsg::new("trace-parent", b""),
                &mut out,
            )
            .unwrap();
        assert_eq!(
            out.written, 0,
            "legacy caller explicitly clears the context after an error"
        );
        DylibLoader.drain_instance(&handle, component).unwrap();
        assert_eq!(
            DylibLoader
                .handle_msg_with_trace(
                    &handle,
                    component,
                    AbiMsg::new("trace-parent", b""),
                    &mut out,
                    Some(&second)
                )
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
    }
    DylibLoader.unload_blocking(handle).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn dependency_extension_cannot_replace_a_legacy_components_message_hook() {
    use std::process::Command;

    let _lock = common::load_mutex();
    let directory = tempfile::tempdir().unwrap();
    let ext = std::env::consts::DLL_EXTENSION;
    let dependency = directory.path().join(format!("libtrace_dependency.{ext}"));
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/trace_symbol_owner.c");
    let compile = |path: &std::path::Path, define: &str, dependency_library: bool| {
        let mut command = Command::new("cc");
        command.args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-fPIC"]);
        if cfg!(target_os = "macos") {
            command.arg("-dynamiclib");
            if dependency_library {
                command.arg("-Wl,-install_name,@rpath/libtrace_dependency.dylib");
            } else {
                command.arg("-Wl,-rpath,@loader_path");
            }
        } else {
            command.arg("-shared");
            if !dependency_library {
                command.arg("-Wl,-rpath,$ORIGIN");
            }
        }
        if !define.is_empty() {
            command.arg(format!("-D{define}"));
        }
        command.arg(&source).arg("-o").arg(path);
        if !dependency_library {
            command
                .arg("-L")
                .arg(directory.path())
                .arg("-ltrace_dependency");
        }
        let result = command.output().expect("C fixture compiler");
        assert!(
            result.status.success(),
            "{command:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    compile(&dependency, "TRACE_DEPENDENCY", true);
    let legacy = directory.path().join(format!("liblegacy.{ext}"));
    let forwarding = directory.path().join(format!("libforwarding.{ext}"));
    compile(&legacy, "", false);
    compile(&forwarding, "TRACE_FORWARDING", false);
    if let Some(destination) = std::env::var_os("PARROT_ABI_TRACE_DEPENDENCY_EVIDENCE") {
        let destination = std::path::PathBuf::from(destination);
        std::fs::create_dir_all(&destination).unwrap();
        for path in [&dependency, &legacy, &forwarding] {
            std::fs::copy(path, destination.join(path.file_name().unwrap())).unwrap();
        }
    }
    let parent = AbiTraceParentV1 {
        trace_id: [13; 16],
        span_id: [29; 8],
    };
    for (path, owns_extension) in [(&legacy, false), (&forwarding, true)] {
        // SAFETY: owned C fixtures implement the native ABI. The foreign hook
        // deliberately ignores self, so a broken loader fails without UB.
        let handle = unsafe { DylibLoader::load(path, "") }.unwrap();
        let component = DylibLoader.construct(&handle, "").unwrap();
        let mut callbacks = 0usize;
        unsafe extern "C" fn start(
            context: *mut (),
            parent: u64,
            target: parrot_abi::AbiStr,
            out: *mut parrot_abi::AbiSpanV1,
        ) -> u32 {
            assert_eq!(parent, 0);
            assert_eq!(unsafe { target.as_str() }, "foreign recording");
            unsafe {
                *context.cast::<usize>() += 1;
                out.write(parrot_abi::AbiSpanV1 {
                    handle: 123,
                    parent: AbiTraceParentV1 {
                        trace_id: [7; 16],
                        span_id: [11; 8],
                    },
                });
            }
            0
        }
        unsafe extern "C" fn finish(context: *mut (), handle: u64, status: u32) -> u32 {
            assert_eq!((handle, status), (123, 0));
            unsafe {
                *context.cast::<usize>() += 1;
            }
            0
        }
        let host = parrot_abi::AbiRecordingHostV1 {
            context: std::ptr::from_mut(&mut callbacks).cast(),
            start,
            finish,
        };
        unsafe extern "C" fn tell(
            context: *mut (),
            destination: parrot_abi::AbiStr,
            message: AbiMsg,
            timeout: u32,
        ) -> u32 {
            if unsafe { destination.as_str() } != "/user/target"
                || unsafe { message.bytes() } != b"component reply"
                || timeout != 17
            {
                return parrot_abi::ABI_MESSAGE_INVALID;
            }
            unsafe {
                *context.cast::<usize>() += 1;
            }
            0
        }
        unsafe extern "C" fn ask(
            _: *mut (),
            _: parrot_abi::AbiStr,
            _: AbiMsg,
            _: parrot_abi::AbiStr,
            _: *mut AbiReplyBuf,
            _: u32,
        ) -> u32 {
            parrot_abi::ABI_MESSAGE_DENIED
        }
        let messaging = parrot_abi::AbiMessagingHostV1 {
            context: std::ptr::from_mut(&mut callbacks).cast(),
            ask,
            tell,
        };
        unsafe extern "C" fn register(
            context: *mut (),
            key: parrot_abi::AbiStr,
            timeout: u32,
        ) -> u32 {
            if unsafe { key.as_str() } != "native/business" || timeout != 17 {
                return 2;
            }
            unsafe {
                *context.cast::<usize>() += 1;
            }
            0
        }
        unsafe extern "C" fn subscribe(
            context: *mut (),
            key: parrot_abi::AbiStr,
            timeout: u32,
            out: *mut u64,
        ) -> u32 {
            let result = unsafe { register(context, key, timeout) };
            unsafe {
                *out = 321;
            }
            result
        }
        unsafe extern "C" fn close(context: *mut (), handle: u64) -> u32 {
            if handle != 321 {
                return 2;
            }
            unsafe {
                *context.cast::<usize>() += 1;
            }
            0
        }
        unsafe extern "C" fn next(
            context: *mut (),
            handle: u64,
            timeout: u32,
            out: *mut AbiReplyBuf,
            kind: *mut u32,
        ) -> u32 {
            if timeout != 17 {
                return 2;
            }
            let result = unsafe { close(context, handle) };
            unsafe {
                *kind = 1;
                (*out).written = 0;
            }
            result
        }
        let services = parrot_abi::AbiServicesHostV1 {
            context: std::ptr::from_mut(&mut callbacks).cast(),
            register,
            deregister: register,
            subscribe,
            next,
            close,
        };
        for mode in 0..8 {
            let mut bytes = [0; 64];
            let mut out = AbiReplyBuf::new(&mut bytes);
            // SAFETY: exclusive registered instance with live disjoint views.
            unsafe {
                if mode >= 6 {
                    DylibLoader
                        .handle_msg_with_services(
                            &handle,
                            component,
                            AbiMsg::new("echo", b"component reply"),
                            &mut out,
                            Some(&parent),
                            (mode == 7).then_some(&host),
                            (mode == 7).then_some(&messaging),
                            (mode == 7).then_some(&services),
                        )
                        .unwrap();
                } else if mode >= 4 {
                    DylibLoader
                        .handle_msg_with_capabilities(
                            &handle,
                            component,
                            AbiMsg::new("echo", b"component reply"),
                            &mut out,
                            Some(&parent),
                            (mode == 5).then_some(&host),
                            (mode == 5).then_some(&messaging),
                        )
                        .unwrap();
                } else if mode >= 2 {
                    DylibLoader
                        .handle_msg_with_recording(
                            &handle,
                            component,
                            AbiMsg::new("echo", b"component reply"),
                            &mut out,
                            Some(&parent),
                            (mode == 3).then_some(&host),
                        )
                        .unwrap();
                } else if mode == 1 {
                    DylibLoader
                        .handle_msg_with_trace(
                            &handle,
                            component,
                            AbiMsg::new("echo", b"component reply"),
                            &mut out,
                            Some(&parent),
                        )
                        .unwrap();
                } else {
                    DylibLoader
                        .handle_msg(
                            &handle,
                            component,
                            AbiMsg::new("echo", b"component reply"),
                            &mut out,
                        )
                        .unwrap();
                }
                let expected: &[u8] = if owns_extension && mode >= 6 {
                    b"foreign services"
                } else if owns_extension && mode >= 4 {
                    b"foreign messaging"
                } else if owns_extension && mode >= 2 {
                    b"foreign recording"
                } else if owns_extension {
                    b"foreign trace"
                } else {
                    b"component reply"
                };
                assert_eq!(
                    out.written_slice(),
                    expected,
                    "dependency extension must not replace the component message hook"
                );
            }
        }
        assert_eq!(handle.supports_trace_v1(), owns_extension);
        assert_eq!(handle.supports_recording_v1(), owns_extension);
        assert_eq!(handle.supports_messaging_v1(), owns_extension);
        assert_eq!(handle.supports_services_v1(), owns_extension);
        assert_eq!(handle.supports_lifecycle_v1(), owns_extension);
        assert_eq!(callbacks, if owns_extension { 13 } else { 0 });
        // The independently declared C lifecycle descriptor must also belong to
        // the component. An owned descriptor may delegate its hook code.
        unsafe {
            let constructed =
                DylibLoader.construct_with_recording(&handle, "", Some(&parent), Some(&host));
            if owns_extension {
                assert!(matches!(
                    constructed,
                    Err(parrot_abi::LoadError::Construct { code: 77, .. })
                ));
            } else {
                constructed.unwrap();
            }
            let drained = DylibLoader.drain_instance_with_recording(
                &handle,
                component,
                Some(&parent),
                Some(&host),
            );
            if owns_extension {
                assert_eq!(drained.unwrap_err().0, 76);
            } else {
                drained.unwrap();
            }
            let unloaded =
                DylibLoader.unload_blocking_with_recording(handle, Some(&parent), Some(&host));
            if owns_extension {
                match unloaded.unwrap_err() {
                    parrot_abi::UnloadError::DestroyFailed {
                        codes,
                        drain_codes,
                        report,
                    } => {
                        assert_eq!(codes, [78]);
                        assert_eq!(drain_codes, [76]);
                        assert_eq!(report.drained, 0);
                    }
                    error => panic!("wrong C cleanup result: {error}"),
                }
            } else {
                assert_eq!(unloaded.unwrap().drained, 2);
            }
        }
        assert_eq!(callbacks, if owns_extension { 19 } else { 0 });
    }
}
