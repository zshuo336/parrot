//! Capability probes for integration tests that require local sockets.

#![allow(dead_code)]

pub fn tcp_bind_available() -> bool {
    bind_available(std::net::TcpListener::bind("127.0.0.1:0").map(drop))
}

pub fn udp_bind_available() -> bool {
    bind_available(std::net::UdpSocket::bind("127.0.0.1:0").map(drop))
}

fn bind_available(result: std::io::Result<()>) -> bool {
    match result {
        Ok(()) => true,
        Err(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied
                && std::env::var_os("PARROT_ALLOW_NETWORK_TEST_SKIP").as_deref()
                    == Some(std::ffi::OsStr::new("1")) =>
        {
            false
        }
        Err(error) => panic!("local socket capability probe failed: {error}"),
    }
}

pub fn skip_tcp(test_name: &str) -> bool {
    if tcp_bind_available() {
        return false;
    }
    eprintln!("skipping {test_name}: local TCP bind is unavailable");
    true
}

pub fn skip_udp(test_name: &str) -> bool {
    if udp_bind_available() {
        return false;
    }
    eprintln!("skipping {test_name}: local UDP bind is unavailable");
    true
}
