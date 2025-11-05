#[path = "support/codec_compatibility.rs"]
mod contract;

fn fixtures() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bincode-2.0.1")
}

#[test]
fn old_wire_messages_preserve_bytes_fields_and_rejection_contracts() {
    contract::wire(&fixtures().join("wire"), false);
}

#[tokio::test]
async fn old_durable_files_recover_continue_and_reopen_without_lost_commits_or_dedup() {
    let dir = std::env::temp_dir().join(format!("parrot-codec-compat-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = Cleanup(dir);
    for part in ["wal", "raft", "inbox"] {
        std::fs::create_dir(dir.0.join(part)).unwrap();
        for entry in std::fs::read_dir(fixtures().join(part)).unwrap() {
            let entry = entry.unwrap();
            assert!(entry.file_type().unwrap().is_file());
            std::fs::copy(entry.path(), dir.0.join(part).join(entry.file_name())).unwrap();
        }
    }
    contract::persistence(&dir.0, 1).await;
    contract::persistence(&dir.0, 2).await;
}
