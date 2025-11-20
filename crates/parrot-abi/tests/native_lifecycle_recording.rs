#![cfg(feature = "loader")]

mod common;
use parrot_abi::*;

fn fixture() -> std::path::PathBuf {
    common::testcomp().with_file_name(format!(
        "libparrot_abi_recordingcomp.{}",
        std::env::consts::DLL_EXTENSION,
    ))
}

fn config(directory: &tempfile::TempDir, extra: &str) -> String {
    format!(
        "[config]\nlifecycle = true\nlifecycle_events = '{}'\n{extra}",
        directory.path().join("events").display()
    )
}

fn events(directory: &tempfile::TempDir) -> String {
    std::fs::read_to_string(directory.path().join("events")).unwrap_or_default()
}

fn parent() -> AbiTraceParentV1 {
    AbiTraceParentV1 {
        trace_id: [19; 16],
        span_id: [27; 8],
    }
}

#[test]
fn lifecycle_layout_is_independent_and_legacy_entries_do_not_opt_in() {
    let _lock = common::load_mutex();
    assert_eq!(PARROT_ABI_VERSION, 1);
    assert_eq!(
        std::mem::size_of::<AbiLifecycleV1>(),
        3 * std::mem::size_of::<usize>()
    );
    assert_eq!(
        std::mem::offset_of!(AbiLifecycleV1, on_drain),
        std::mem::size_of::<usize>()
    );
    assert_eq!(
        std::mem::offset_of!(AbiLifecycleV1, destroy),
        2 * std::mem::size_of::<usize>()
    );
    let directory = tempfile::tempdir().unwrap();
    // SAFETY: all libraries in this file are owned non-unwinding test fixtures.
    let handle = unsafe { DylibLoader::load(&fixture(), "") }.unwrap();
    assert!(handle.supports_lifecycle_v1());
    let component = DylibLoader
        .construct(&handle, &config(&directory, ""))
        .unwrap();
    let mut bytes = [0; 16];
    let mut reply = AbiReplyBuf::new(&mut bytes);
    unsafe {
        DylibLoader
            .handle_msg(
                &handle,
                component,
                AbiMsg::new("echo", b"legacy"),
                &mut reply,
            )
            .unwrap();
        assert_eq!(reply.written_slice(), b"legacy");
        DylibLoader.drain_instance(&handle, component).unwrap();
    }
    assert_eq!(DylibLoader.unload_blocking(handle).unwrap().drained, 1);
    assert_eq!(
        events(&directory),
        "released\n",
        "original hooks never dispatch the extension"
    );
}

#[test]
fn lifecycle_entries_fall_back_for_old_guests_and_preserve_business_and_cached_drain() {
    let _lock = common::load_mutex();
    let handle = unsafe { DylibLoader::load(&common::testcomp(), "") }.unwrap();
    assert!(!handle.supports_lifecycle_v1());
    unsafe {
        let component = DylibLoader
            .construct_with_recording(&handle, "", Some(&parent()), None)
            .unwrap();
        let mut bytes = [0; 16];
        let mut reply = AbiReplyBuf::new(&mut bytes);
        DylibLoader
            .handle_msg(
                &handle,
                component,
                AbiMsg::new("echo", b"fallback"),
                &mut reply,
            )
            .unwrap();
        assert_eq!(reply.written_slice(), b"fallback");
        DylibLoader
            .drain_instance_with_recording(&handle, component, Some(&parent()), None)
            .unwrap();
        DylibLoader
            .drain_instance_with_recording(&handle, component, None, None)
            .unwrap();
        assert_eq!(
            DylibLoader
                .handle_msg(
                    &handle,
                    component,
                    AbiMsg::new("echo", b"closed"),
                    &mut reply
                )
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
        assert_eq!(
            DylibLoader
                .unload_blocking_with_recording(handle, None, None)
                .unwrap()
                .drained,
            1
        );
    }
}

#[test]
fn invalid_parent_rejects_construction_and_drain_without_guest_effects() {
    let _lock = common::load_mutex();
    let directory = tempfile::tempdir().unwrap();
    let handle = unsafe { DylibLoader::load(&fixture(), "") }.unwrap();
    let config = config(&directory, "");
    unsafe {
        for invalid in [
            AbiTraceParentV1 {
                trace_id: [0; 16],
                ..parent()
            },
            AbiTraceParentV1 {
                span_id: [0; 8],
                ..parent()
            },
        ] {
            assert!(matches!(
                DylibLoader.construct_with_recording(&handle, &config, Some(&invalid), None),
                Err(LoadError::Construct {
                    code: ABI_ERR_STATE,
                    ..
                })
            ));
        }
        assert!(events(&directory).is_empty());
        let component = DylibLoader
            .construct_with_recording(&handle, &config, None, None)
            .unwrap();
        let before = events(&directory);
        let invalid = AbiTraceParentV1 {
            span_id: [0; 8],
            ..parent()
        };
        assert_eq!(
            DylibLoader
                .drain_instance_with_recording(&handle, component, Some(&invalid), None)
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
        assert_eq!(events(&directory), before);
        let mut bytes = [0; 8];
        let mut reply = AbiReplyBuf::new(&mut bytes);
        DylibLoader
            .handle_msg(&handle, component, AbiMsg::new("echo", b"live"), &mut reply)
            .unwrap();
        assert_eq!(reply.written_slice(), b"live");
        assert_eq!(
            DylibLoader
                .unload_blocking_with_recording(handle, Some(&invalid), None)
                .unwrap_err(),
            UnloadError::InvalidTraceParent
        );
        assert_eq!(
            events(&directory),
            format!("{before}released\n"),
            "invalid unload still completes mandatory legacy cleanup"
        );
    }
}

#[test]
fn cached_failed_drain_is_not_reinvoked_and_destroy_failure_retains_both_codes() {
    let _lock = common::load_mutex();
    let directory = tempfile::tempdir().unwrap();
    let handle = unsafe { DylibLoader::load(&fixture(), "") }.unwrap();
    unsafe {
        let component = DylibLoader
            .construct_with_recording(
                &handle,
                &config(&directory, "drain_error = true\ndestroy_panic = true\n"),
                Some(&parent()),
                None,
            )
            .unwrap();
        assert_eq!(
            DylibLoader
                .drain_instance_with_recording(&handle, component, Some(&parent()), None)
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
        let before = events(&directory);
        assert_eq!(
            DylibLoader
                .drain_instance(&handle, component)
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
        assert_eq!(
            DylibLoader
                .drain_instance_with_recording(&handle, component, None, None)
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
        assert_eq!(events(&directory), before);
        match DylibLoader
            .unload_blocking_with_recording(handle, None, None)
            .unwrap_err()
        {
            UnloadError::DestroyFailed {
                codes,
                drain_codes,
                report,
            } => {
                assert_eq!(codes, [ABI_ERR_PANIC]);
                assert_eq!(drain_codes, [ABI_ERR_STATE]);
                assert_eq!(report, UnloadReport::default());
            }
            error => panic!("wrong cleanup outcome: {error}"),
        }
    }
    let events = events(&directory);
    assert_eq!(
        events
            .lines()
            .filter(|line| line.starts_with("drain|"))
            .count(),
        1
    );
    assert_eq!(
        events
            .lines()
            .filter(|line| line.starts_with("destroy|"))
            .count(),
        1
    );
    assert_eq!(events.lines().filter(|line| *line == "released").count(), 1);
}

#[test]
fn cleanup_drains_all_instances_before_any_destroy_and_releases_every_failed_instance() {
    let _lock = common::load_mutex();
    let directory = tempfile::tempdir().unwrap();
    let handle = unsafe { DylibLoader::load(&fixture(), "") }.unwrap();
    unsafe {
        DylibLoader
            .construct_with_recording(
                &handle,
                &config(&directory, "drain_error = true\n"),
                None,
                None,
            )
            .unwrap();
        DylibLoader
            .construct_with_recording(
                &handle,
                &config(&directory, "destroy_error = true\n"),
                None,
                None,
            )
            .unwrap();
        match DylibLoader
            .unload_blocking_with_recording(handle, Some(&parent()), None)
            .unwrap_err()
        {
            UnloadError::DestroyFailed {
                codes,
                drain_codes,
                report,
            } => {
                assert_eq!(codes, [ABI_ERR_STATE]);
                assert_eq!(drain_codes, [ABI_ERR_STATE]);
                assert_eq!(report.drained, 1);
            }
            error => panic!("wrong cleanup outcome: {error}"),
        }
    }
    let events = events(&directory);
    let phases: Vec<_> = events
        .lines()
        .map(|line| line.split('|').next().unwrap())
        .collect();
    assert_eq!(
        phases,
        [
            "construct",
            "construct",
            "drain",
            "drain",
            "destroy",
            "released",
            "destroy",
            "released"
        ]
    );
}

#[test]
fn drop_never_reuses_a_borrowed_lifecycle_recording_capability() {
    let _lock = common::load_mutex();
    let directory = tempfile::tempdir().unwrap();
    let handle = unsafe { DylibLoader::load(&fixture(), "") }.unwrap();
    let mut callbacks = 0usize;
    unsafe extern "C" fn start(context: *mut (), _: u64, _: AbiStr, _: *mut AbiSpanV1) -> u32 {
        unsafe {
            *context.cast::<usize>() += 1;
        }
        ABI_RECORDING_DISABLED
    }
    unsafe extern "C" fn finish(_: *mut (), _: u64, _: u32) -> u32 {
        ABI_RECORDING_UNKNOWN_HANDLE
    }
    {
        let recording = AbiRecordingHostV1 {
            context: std::ptr::from_mut(&mut callbacks).cast(),
            start,
            finish,
        };
        unsafe {
            DylibLoader
                .construct_with_recording(
                    &handle,
                    &config(&directory, ""),
                    Some(&parent()),
                    Some(&recording),
                )
                .unwrap();
        }
    }
    assert_eq!(callbacks, 1);
    drop(handle);
    assert_eq!(callbacks, 1);
    assert_eq!(
        events(&directory).lines().count(),
        2,
        "only construct and actual legacy release"
    );
}
