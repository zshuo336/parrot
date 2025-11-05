//! Actual CLI failures must be ordinary diagnostics before network side effects.
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run(args: &[&str]) -> (Option<i32>, String, bool) {
    let binary = std::env::var_os("CRAWLER_CLI_BINARY")
        .or_else(|| option_env!("CARGO_BIN_EXE_crawler-lab").map(Into::into))
        .expect("crawler binary must be provided");
    let mut child = Command::new(binary)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut readers = Vec::new();
    for pipe in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let sender = sender.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                sender.send(line.unwrap()).unwrap();
            }
        }));
    }
    drop(sender);
    // Cargo-launched macOS binaries can remain in _dyld_start beyond 3s.
    // This is a finite process-start watchdog, not an application latency SLA.
    let mut deadline = Instant::now() + Duration::from_secs(15);
    let mut listening = false;
    let mut output = String::new();
    let mut expired = false;
    loop {
        for line in receiver.try_iter() {
            // Cold OS loading of the Wasm-enabled executable is distinct from
            // registry waiting. Once listening, the real --wait 1 scenario still
            // has a separate 3-second bound.
            if !listening && line.starts_with("PARROT_CRAWLER_PORT=") {
                listening = true;
                deadline = Instant::now() + Duration::from_secs(3);
            }
            output.push_str(&line);
            output.push('\n');
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            expired = true;
            child.kill().unwrap();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = child.wait().unwrap();
    for reader in readers {
        reader.join().unwrap();
    }
    for line in receiver {
        output.push_str(&line);
        output.push('\n');
    }
    (status.code(), output, expired)
}

fn invalid(args: &[&str]) {
    let (code, output, expired) = run(args);
    assert!(!expired, "CLI did not reject {args:?}: {output}");
    assert_eq!(code, Some(2), "{args:?}: {output}");
    assert!(
        !output.contains("panicked") && !output.contains("crawler-lab PASS"),
        "{output}"
    );
    assert!(output.contains("crawler-lab:"), "no diagnostic: {output}");
}

#[test]
fn help_is_successful_without_runtime_or_gateways() {
    for flag in ["--help", "-h"] {
        let (code, text, expired) = run(&[flag]);
        assert!(!expired && code == Some(0), "{text}");
        assert!(
            text.contains("--pages")
                && text.contains("--bind")
                && text.contains("--insecure-development")
        );
    }
}

#[test]
fn missing_and_malformed_options_do_not_panic() {
    for flag in [
        "--pages", "--depth", "--fanout", "--batch", "--bind", "--wait",
    ] {
        invalid(&[flag]);
        invalid(&[flag, "bad"]);
    }
    for args in [
        vec![],
        vec!["--unknown"],
        vec!["anything"],
        vec!["--pages", "-1"],
        vec!["--pages", "18446744073709551616"],
        vec!["--batch", "0"],
        vec!["--batch", "257"],
        vec!["--pages", "1000001"],
        vec!["--fanout", "257"],
        vec!["--depth", "65536"],
        vec!["--wait", "0"],
        vec!["--wait", "3601"],
    ] {
        invalid(&args);
    }
}

#[test]
fn duplicate_and_mixed_modes_are_rejected() {
    for args in [
        vec!["--bind", "127.0.0.1:0", "erl=127.0.0.1:1"],
        vec!["--bind", "127.0.0.1:0", "--bind", "127.0.0.1:1"],
        vec!["--pages", "1", "--pages", "2"],
        vec!["--insecure-development", "--insecure-development"],
        vec!["erl=127.0.0.1:1", "erl=127.0.0.1:2"],
        vec!["erl=127.0.0.1:1", "ray=127.0.0.1:1", "jvm=127.0.0.1:3"],
    ] {
        invalid(&args);
    }
}

#[test]
fn malformed_or_incomplete_endpoints_are_rejected() {
    for args in [
        vec!["erl=127.0.0.1:1"],
        vec!["other=127.0.0.1:1"],
        vec!["erl="],
        vec!["erl=host"],
        vec!["erl=host:0"],
        vec!["erl=host:65536"],
        vec!["erl=::1:5"],
        vec!["erl=http://host:5"],
        vec!["erl=bad host:5"],
        vec!["erl=host:5/extra"],
    ] {
        invalid(&args);
    }
}

#[test]
fn argument_error_precedes_all_network_activity() {
    let listeners: Vec<_> = (0..3)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    for listener in &listeners {
        listener.set_nonblocking(true).unwrap();
    }
    let endpoints: Vec<_> = ["erl", "ray", "jvm"]
        .iter()
        .zip(&listeners)
        .map(|(name, listener)| format!("{name}={}", listener.local_addr().unwrap()))
        .collect();
    for invalid_options in [
        vec!["--unknown"],
        vec!["--batch", "0"],
        vec!["--batch", "257"],
        vec!["--pages", "1000001"],
        vec!["--fanout", "257"],
    ] {
        let mut args = vec![
            "--insecure-development",
            &endpoints[0],
            &endpoints[1],
            &endpoints[2],
        ];
        args.extend(invalid_options);
        invalid(&args);
    }
    for listener in listeners {
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "invalid CLI reached a gateway"
        );
    }
}

#[test]
fn plaintext_requires_explicit_development_opt_in() {
    invalid(&["erl=127.0.0.1:1", "ray=127.0.0.1:2", "jvm=127.0.0.1:3"]);
    invalid(&["--bind", "127.0.0.1:0"]);
}

#[test]
fn valid_zero_workload_and_numeric_boundaries_reach_registry_timeout() {
    let (code, text, expired) = run(&[
        "--insecure-development",
        "--bind",
        "127.0.0.1:0",
        "--pages",
        "0",
        "--depth",
        "65535",
        "--fanout",
        "0",
        "--batch",
        "256",
        "--wait",
        "1",
    ]);
    assert!(!expired && code == Some(1), "{text}");
    assert!(text.contains("registry") && text.contains("crawler-lab:"));
    let port = text
        .lines()
        .find_map(|line| line.strip_prefix("PARROT_CRAWLER_PORT="))
        .expect("successful registry bind must publish its actual port")
        .parse::<u16>()
        .unwrap();
    assert_ne!(port, 0);
    let _released = TcpListener::bind(("127.0.0.1", port)).expect("registry listener leaked");
    assert!(!text.contains("panicked") && !text.contains("crawler-lab PASS"));
}

#[test]
fn occupied_listener_is_a_regular_startup_error() {
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = occupied.local_addr().unwrap().to_string();
    let (code, text, expired) =
        run(&["--insecure-development", "--bind", &endpoint, "--wait", "1"]);
    assert!(!expired && code == Some(1), "{text}");
    assert!(text.contains("crawler-lab:") && !text.contains("panicked"));
    assert!(!text.contains("PARROT_CRAWLER_PORT="));
}

#[test]
fn hub_selection_requires_explicit_trust_and_unambiguous_digest() {
    let digest = "a".repeat(64);
    for options in [
        vec!["--hub-dylib", "x"],
        vec!["--hub-wasm", "x"],
        vec!["--trust-native-sha256", &digest],
        vec!["--hub-sha256", &digest],
        vec!["--hub-dylib", "x", "--hub-sha256", &digest],
        vec!["--hub-wasm", "x", "--trust-native-sha256", &digest],
        vec!["--hub-dylib", "", "--trust-native-sha256", &digest],
        vec!["--hub-dylib", "x", "--trust-native-sha256", "xyz"],
        vec![
            "--hub-wasm",
            "x",
            "--hub-sha256",
            &digest,
            "--hub-dylib",
            "y",
            "--trust-native-sha256",
            &digest,
        ],
    ] {
        let mut args = vec!["--insecure-development", "--bind", "127.0.0.1:0"];
        args.extend(options);
        invalid(&args);
    }
}
