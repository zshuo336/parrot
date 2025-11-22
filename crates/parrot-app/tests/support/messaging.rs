use std::{path::PathBuf, sync::OnceLock};

pub fn fixture() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../parrot-abi/tests/fixtures");
        let target = root.join("target");
        let rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
        let rustflags = if rustflags
            .split_whitespace()
            .any(|flag| flag == "prefer-dynamic")
        {
            rustflags
        } else if rustflags.is_empty() {
            "-C prefer-dynamic".to_owned()
        } else {
            format!("{rustflags} -C prefer-dynamic")
        };
        let output = std::process::Command::new("cargo")
            .args(["build", "--release", "--locked", "--target-dir"])
            .arg(&target)
            .env("RUSTFLAGS", rustflags)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        target
            .join("release")
            .join(format!(
                "libparrot_abi_messagingcomp.{}",
                std::env::consts::DLL_EXTENSION
            ))
            .canonicalize()
            .unwrap()
    })
}
