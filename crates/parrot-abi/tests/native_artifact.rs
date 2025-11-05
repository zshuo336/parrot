#![cfg(feature = "loader")]
#![allow(clippy::await_holding_lock)]
mod common;
use parrot_abi::{
    AbiMsg, AbiReplyBuf, DylibLoader, LoadError, NativeArtifact, NativeArtifactLimits,
    ABI_ERR_STATE,
};
use sha2::{Digest, Sha256};

fn echo(
    handle: &parrot_abi::DylibHandle,
    comp: *mut parrot_abi::AbiComponent,
) -> Result<Vec<u8>, (u32, String)> {
    let mut bytes = vec![0; 64];
    let mut out = AbiReplyBuf::new(&mut bytes);
    unsafe {
        DylibLoader.handle_msg(
            handle,
            comp,
            AbiMsg::new("echo", b"exact snapshot"),
            &mut out,
        )?;
    }
    bytes.truncate(out.written as usize);
    Ok(bytes)
}

#[test]
fn snapshot_loads_verified_bytes_after_source_is_replaced_and_removed() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("replaceable-input");
    let bytes = std::fs::read(common::testcomp()).unwrap();
    std::fs::write(&source, &bytes).unwrap();
    let digest = format!("sha256:{:X}", Sha256::digest(&bytes));
    let artifact = NativeArtifact::prepare(&source, &digest, Default::default()).unwrap();
    std::fs::write(&source, b"not an executable").unwrap();
    let first = unsafe { artifact.load() }.unwrap();
    std::fs::remove_file(&source).unwrap();
    let second = unsafe { artifact.clone().load() }.unwrap();
    drop(artifact);
    for handle in [first, second] {
        let comp = DylibLoader.construct(&handle, "{}").unwrap();
        assert_eq!(echo(&handle, comp).unwrap(), b"exact snapshot");
        let report = DylibLoader.unload_blocking(handle).unwrap();
        assert_eq!(report.drained, 1);
    }
}

#[test]
fn snapshot_requires_digest_and_enforces_bytes_scan_and_regular_file_limits() {
    let bytes = std::fs::read(common::testcomp()).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    for bad in ["", "abc", &"g".repeat(64)] {
        assert!(matches!(
            NativeArtifact::prepare_bytes(&bytes, bad, Default::default()),
            Err(LoadError::InvalidConfig(_))
        ));
    }
    assert!(matches!(
        NativeArtifact::prepare_bytes(&bytes, &"0".repeat(64), Default::default()),
        Err(LoadError::Digest { .. })
    ));
    assert!(matches!(
        NativeArtifact::prepare_bytes(
            &bytes,
            &digest,
            NativeArtifactLimits {
                max_file_bytes: bytes.len() - 1,
                ..Default::default()
            }
        ),
        Err(LoadError::ResourceLimit(_))
    ));
    assert!(NativeArtifact::prepare_bytes(
        &bytes,
        &digest,
        NativeArtifactLimits {
            max_file_bytes: bytes.len(),
            ..Default::default()
        }
    )
    .is_ok());
    assert!(matches!(
        NativeArtifact::prepare_bytes(
            &bytes,
            &digest,
            NativeArtifactLimits {
                max_scan_symbols: 1,
                ..Default::default()
            }
        ),
        Err(LoadError::ResourceLimit(_))
    ));
    let temp = tempfile::tempdir().unwrap();
    assert!(matches!(
        NativeArtifact::prepare(temp.path(), &digest, Default::default()),
        Err(LoadError::Open(_))
    ));
    let bad = std::fs::read(common::violating()).unwrap();
    assert!(matches!(
        NativeArtifact::prepare_bytes(
            &bad,
            &format!("{:x}", Sha256::digest(&bad)),
            Default::default()
        ),
        Err(LoadError::Forbidden(_))
    ));
}

#[test]
fn per_instance_drain_is_idempotent_and_keeps_siblings_usable() {
    let _lock = common::load_mutex();
    let handle = unsafe { DylibLoader::load(&common::testcomp(), "") }.unwrap();
    let first = DylibLoader.construct(&handle, "{}").unwrap();
    let second = DylibLoader.construct(&handle, "{}").unwrap();
    for _ in 0..3 {
        unsafe { DylibLoader.drain_instance(&handle, first) }.unwrap();
    }
    assert_eq!(echo(&handle, first).unwrap_err().0, ABI_ERR_STATE);
    assert_eq!(echo(&handle, second).unwrap(), b"exact snapshot");
    assert_eq!(DylibLoader.unload_blocking(handle).unwrap().drained, 2);
}

#[test]
fn failed_drain_is_cached_and_destroy_still_runs_once() {
    let _lock = common::load_mutex();
    let observer = unsafe { libloading::Library::new(common::testcomp()) }.unwrap();
    let reset = unsafe { observer.get::<unsafe extern "C" fn()>(b"lifecycle_reset") }.unwrap();
    let observe =
        unsafe { observer.get::<unsafe extern "C" fn(u32) -> u64>(b"lifecycle_observe") }.unwrap();
    unsafe {
        reset();
    }
    let handle = unsafe { DylibLoader::load(&common::testcomp(), "") }.unwrap();
    let comp = DylibLoader.construct(&handle, "error").unwrap();
    for _ in 0..3 {
        assert_eq!(
            unsafe { DylibLoader.drain_instance(&handle, comp) }
                .unwrap_err()
                .0,
            ABI_ERR_STATE
        );
    }
    assert_eq!(unsafe { observe(0) }, 1);
    assert_eq!(unsafe { observe(1) }, 0);
    assert!(matches!(
        DylibLoader.unload_blocking(handle),
        Err(parrot_abi::UnloadError::DrainFailed { .. })
    ));
    assert_eq!(unsafe { observe(0) }, 1);
    assert_eq!(unsafe { observe(1) }, 1);
    assert_eq!(unsafe { observe(2) }, 0);
}
