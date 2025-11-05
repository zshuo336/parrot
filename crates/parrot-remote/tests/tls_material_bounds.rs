//! TLS configuration must bound input before parsing or starting a listener.
use parrot_remote::tls::{client_config, server_config, TlsConfig};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const LIMIT: usize = 1024 * 1024;
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "parrot-tls-input-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn load(&self) -> Result<TlsConfig, parrot_remote::error::RemoteError> {
        TlsConfig::from_paths(
            self.0.join("cert").to_str().unwrap(),
            self.0.join("key").to_str().unwrap(),
            self.0.join("ca").to_str().unwrap(),
        )
    }
    fn save(&self, cfg: &TlsConfig) {
        for (name, data) in [
            ("cert", &cfg.cert_pem),
            ("key", &cfg.key_pem),
            ("ca", &cfg.ca_pem),
        ] {
            std::fs::write(self.0.join(name), data).unwrap();
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn credentials() -> TlsConfig {
    let cert = rcgen::generate_simple_self_signed(vec!["node.test".into()]).unwrap();
    TlsConfig {
        cert_pem: cert.cert.pem().into_bytes(),
        key_pem: cert.signing_key.serialize_pem().into_bytes(),
        ca_pem: cert.cert.pem().into_bytes(),
    }
}
fn assert_limit<T>(result: Result<T, parrot_remote::error::RemoteError>) {
    let error = match result {
        Ok(_) => panic!("unbounded TLS material accepted"),
        Err(e) => e.to_string(),
    };
    assert!(
        error.contains("TLS material limit"),
        "wrong rejection: {error}"
    );
}
#[test]
fn each_credential_file_rejects_oversize_before_parsing() {
    let scratch = Scratch::new();
    for name in ["cert", "key", "ca"] {
        scratch.save(&credentials());
        std::fs::File::create(scratch.0.join(name))
            .unwrap()
            .set_len(LIMIT as u64 + 1)
            .unwrap();
        assert_limit(scratch.load());
    }
}
#[test]
fn inline_credentials_cannot_bypass_file_limits() {
    for field in 0..3 {
        let mut cfg = credentials();
        let pem = match field {
            0 => &mut cfg.cert_pem,
            1 => &mut cfg.key_pem,
            _ => &mut cfg.ca_pem,
        };
        pem.resize(LIMIT + 1, b'\n');
        assert_limit(server_config(&cfg));
        assert_limit(client_config(&cfg));
    }
}
#[test]
fn real_credentials_at_exact_byte_limit_remain_usable() {
    let scratch = Scratch::new();
    let mut cfg = credentials();
    for pem in [&mut cfg.cert_pem, &mut cfg.key_pem, &mut cfg.ca_pem] {
        pem.resize(LIMIT, b'\n');
    }
    scratch.save(&cfg);
    let loaded = scratch.load().unwrap();
    assert_eq!(loaded.cert_pem, cfg.cert_pem);
    assert_eq!(loaded.key_pem, cfg.key_pem);
    assert_eq!(loaded.ca_pem, cfg.ca_pem);
    server_config(&loaded).unwrap();
    client_config(&loaded).unwrap();
}
#[test]
fn certificate_count_is_bounded_independently_of_pem_bytes() {
    for field in [0, 1] {
        let mut cfg = credentials();
        let cert = cfg.cert_pem.clone();
        let bundle = if field == 0 {
            &mut cfg.cert_pem
        } else {
            &mut cfg.ca_pem
        };
        *bundle = cert.repeat(65);
        assert!(bundle.len() < LIMIT);
        assert_limit(server_config(&cfg));
        assert_limit(client_config(&cfg));
    }
    let mut cfg = credentials();
    cfg.ca_pem = cfg.ca_pem.repeat(64);
    server_config(&cfg).unwrap();
    client_config(&cfg).unwrap();
}
#[test]
fn directories_are_rejected_as_non_regular_material() {
    let scratch = Scratch::new();
    scratch.save(&credentials());
    std::fs::remove_file(scratch.0.join("cert")).unwrap();
    std::fs::create_dir(scratch.0.join("cert")).unwrap();
    let error = scratch
        .load()
        .err()
        .expect("directory accepted")
        .to_string();
    assert!(error.contains("regular file"), "wrong rejection: {error}");
}

#[cfg(unix)]
#[test]
fn regular_symlinks_work_but_fifo_and_fifo_symlink_need_no_writer() {
    use std::io::Write;
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    use std::time::{Duration, Instant};
    let scratch = Scratch::new();
    scratch.save(&credentials());
    let path = scratch.0.join("cert");
    let real = scratch.0.join("real");
    std::fs::rename(&path, &real).unwrap();
    std::os::unix::fs::symlink(&real, &path).unwrap();
    let loaded = scratch.load().unwrap();
    server_config(&loaded).unwrap();
    client_config(&loaded).unwrap();
    std::fs::remove_file(&path).unwrap();
    let fifo = scratch.0.join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    for linked in [false, true] {
        if linked {
            std::os::unix::fs::symlink(&fifo, &path).unwrap();
        }
        let input = if linked { &path } else { &fifo };
        let (done, wait) = std::sync::mpsc::channel();
        let writer_path = fifo.clone();
        let rescuer = std::thread::spawn(move || {
            if wait.recv_timeout(Duration::from_millis(250)).is_ok() {
                return false;
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if wait.try_recv().is_ok() {
                    return false;
                }
                if let Ok(mut writer) = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&writer_path)
                {
                    let _ = writer.write_all(b"not PEM");
                    return true;
                }
                assert!(
                    Instant::now() < deadline,
                    "failed to rescue blocked FIFO reader"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let result = TlsConfig::from_paths(
            input.to_str().unwrap(),
            real.to_str().unwrap(),
            real.to_str().unwrap(),
        );
        let _ = done.send(());
        assert!(
            !rescuer.join().unwrap(),
            "TLS loading waited for a FIFO writer"
        );
        let error = result.err().expect("FIFO accepted").to_string();
        assert!(error.contains("regular file"), "wrong rejection: {error}");
    }
}
