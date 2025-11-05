//! Contract tests for the shared group observer.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use parrot_remote::process_group::has_live_group;
    use std::time::Duration;
    use std::time::Instant;

    #[test]
    fn deadline_is_failure_and_current_group_is_live() {
        let group = unsafe { libc::getpgrp() } as u32;
        assert!(has_live_group(group, Instant::now()).is_err());
        assert!(has_live_group(group, Instant::now() + Duration::from_secs(5)).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dead_process_leader_does_not_hide_a_listening_worker_thread() {
        use std::os::unix::process::CommandExt;
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let record = directory.path().join("worker.json");
        let child = Child(
            std::process::Command::new("python3")
                .arg(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("src/host/gateway_process_fixture.py"),
                )
                .arg("dead-leader")
                .arg(&record)
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let path = std::path::PathBuf::from(format!("/proc/{}", child.0.id()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !record.is_file()
            || !std::fs::read_to_string(path.join("stat"))
                .unwrap()
                .rsplit(')')
                .next()
                .unwrap()
                .trim_start()
                .starts_with('Z')
        {
            assert!(
                Instant::now() < deadline,
                "fixture leader did not become a zombie"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
        let port = value["port"].as_u64().unwrap() as u16;
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err());
        assert!(
            has_live_group(child.0.id(), deadline).unwrap(),
            "live thread hidden by zombie leader"
        );
        drop(child);
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok());
    }
}
