//! Public command contracts exercise real descendants, sockets, deadlines and
//! exit status; fixture gateways deliberately fail before valid application replies.
#[cfg(target_os = "linux")]
#[test]
fn interop_commands_own_their_gateway_lifetimes() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = std::process::Command::new("python3")
        .arg(root.join("interop/tests/probe_lifecycle.py"))
        .env("PARROT_TEST_RAY_BINARY", env!("CARGO_BIN_EXE_interop-ray"))
        .env("PARROT_TEST_ERL_BINARY", env!("CARGO_BIN_EXE_interop-erl"))
        .status()
        .expect("launch lifecycle contracts");
    assert!(
        status.success(),
        "interop lifecycle process contracts failed: {status}"
    );
}
