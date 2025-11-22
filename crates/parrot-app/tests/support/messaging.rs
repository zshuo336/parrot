use std::{path::PathBuf, sync::OnceLock};

mod fixture_build {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../parrot-abi/tests/common/build.rs"
    ));
}

pub fn fixture() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../parrot-abi/tests/fixtures");
        let target = root.join("target");
        let output = fixture_build::cargo()
            .args(["build", "--release", "--locked", "--target-dir"])
            .arg(&target)
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
