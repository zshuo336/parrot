#[cfg(target_os = "linux")]
#[test]
fn jvm_probe_checks_business_semantics_and_retires_actual_descendants() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = std::process::Command::new("python3")
        .arg(root.join("interop/tests/matrix_lifecycle.py"))
        .env(
            "PARROT_TEST_MATRIX_BINARY",
            env!("CARGO_BIN_EXE_interop-matrix"),
        )
        .status()
        .expect("launch JVM command contracts");
    assert!(status.success(), "JVM process contracts failed: {status}");
}
