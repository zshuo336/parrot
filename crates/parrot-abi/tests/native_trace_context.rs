#![cfg(feature = "loader")]

mod common;
use parrot_abi::{AbiMsg, AbiReplyBuf, AbiTraceParentV1, DylibLoader, ABI_ERR_STATE};

#[test]
fn extension_layout_is_independent_of_the_frozen_v1_layouts() {
    assert_eq!(std::mem::size_of::<AbiTraceParentV1>(), 24);
    assert_eq!(std::mem::align_of::<AbiTraceParentV1>(), 1);
    assert_eq!(std::mem::offset_of!(AbiTraceParentV1, span_id), 16);
    assert_eq!(
        std::mem::size_of::<parrot_abi::AbiTraceV1>(),
        std::mem::size_of::<usize>()
    );
    assert_eq!(parrot_abi::PARROT_ABI_VERSION, 1);
}

#[test]
fn old_guest_falls_back_and_keeps_original_error_and_drain_semantics() {
    let _lock = common::load_mutex();
    // SAFETY: owned fixture implements the trusted native ABI contract.
    let handle = unsafe { DylibLoader::load(&common::testcomp(), "") }.unwrap();
    assert!(!handle.supports_trace_v1());
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
        for traced in [false, true] {
            let mut bytes = [0; 64];
            let mut out = AbiReplyBuf::new(&mut bytes);
            // SAFETY: exclusive registered instance with live disjoint views.
            unsafe {
                if traced {
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
                let expected: &[u8] = if owns_extension {
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
        DylibLoader.unload_blocking(handle).unwrap();
    }
}
