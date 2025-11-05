use bytes::Bytes;
use parrot_app::debug::trace::{aggregate, parse_trace_line, render_tree};
use parrot_remote::frame::{flags, Frame};

fn frame(cid: u64, hop: u8, path: &str, key: &str) -> Frame {
    let mut frame = Frame::tell(path, key, Bytes::from_static(b"business payload"));
    frame.header.flags |= flags::TRACING;
    frame.header.correlation_id = cid;
    frame.header.hop_count = hop;
    frame
}

#[test]
fn real_frame_strings_round_trip_without_truncation_or_field_injection() {
    for (path, key) in [
        ("/a b", "hello world"),
        ("", ""),
        ("/a\\b\"c", "key=\" len=999 trace_id=99"),
        ("/雪/🐦", "e\u{301}\u{202e}rtl\u{200b}zero-width"),
        ("/\n\r\t\0\u{1b}[2J", "\u{7}\u{85}\u{2028}\u{2029}"),
    ] {
        let f = frame(u64::MAX, 7, path, key);
        let line = f.trace_line();
        let event = parse_trace_line(&line).expect("real traced Frame must parse");
        assert_eq!(event.path, path, "{line}");
        assert_eq!(event.type_key, key, "{line}");
        assert_eq!(event.trace_id, f.header.correlation_id);
        assert_eq!(event.frame_type, f.header.frame_type);
        assert_eq!(event.hop_count, f.header.hop_count);
        assert_eq!(event.len, f.payload.len());
        assert_eq!(event.raw, line);
    }
}

#[test]
fn real_frame_debug_escapes_round_trip_control_and_unicode_characters() {
    let text: String = (0..=255)
        .chain([0x301, 0x200b, 0x202e, 0xfeff, 0x1f426, 0x10ffff])
        .map(|n| char::from_u32(n).unwrap())
        .collect();
    let line = frame(7, 0, &text, &text).trace_line();
    let event = parse_trace_line(&line).unwrap();
    assert_eq!(event.path, text);
    assert_eq!(event.type_key, text);
}

#[test]
fn transport_tx_rx_envelopes_parse_and_preserve_audit_line() {
    let body = frame(7, 0, "/a b", "a key").trace_line();
    for direction in ["TX", "RX"] {
        let line = format!("[parrot-trace] {direction} {body}");
        let event = parse_trace_line(&line).unwrap();
        assert_eq!(event.path, "/a b");
        assert_eq!(event.type_key, "a key");
        assert_eq!(event.raw, line);
    }
    for prefix in ["unrelated ", "[parrot-trace] BAD ", "[other] TX "] {
        assert!(parse_trace_line(&format!("{prefix}{body}")).is_none());
    }
}

#[test]
fn traced_marker_and_correlation_id_must_agree() {
    let mut f = frame(7, 0, "/x", "k");
    let line = f.trace_line();
    for invalid in [
        line.replace("trace_id=7", "trace_id=8"),
        line.replace("flags=0x0002", "flags=0x0000"),
        line.replace("cid=7 ", ""),
        line.replace("flags=0x0002 ", ""),
    ] {
        assert!(parse_trace_line(&invalid).is_none(), "{invalid}");
    }
    f.header.flags = 0;
    assert!(parse_trace_line(&f.trace_line()).is_none());
    assert!(parse_trace_line(&format!("{} trace_id=7", f.trace_line())).is_none());
    f.header.flags = flags::TRACING | flags::URGENT;
    assert!(parse_trace_line(&f.trace_line()).is_some());
}

#[test]
fn duplicate_fields_are_rejected_even_when_values_match() {
    let line = frame(7, 0, "/x", "k").trace_line();
    for duplicate in [
        "ft=0x13",
        "cid=7",
        "hop=0/8",
        "flags=0x0002",
        "trace_id=7",
        "path=\"/x\"",
        "key=\"k\"",
        "len=16",
        "future=1 future=2",
    ] {
        let invalid = format!("{line} {duplicate}");
        assert!(parse_trace_line(&invalid).is_none(), "{invalid}");
    }
}

#[test]
fn malformed_debug_strings_are_not_partially_accepted() {
    let line = frame(7, 0, "/x", "k").trace_line();
    for bad in [
        "/x",
        "\"/x",
        "\"/x\"tail",
        r#""\q""#,
        r#""\u{}""#,
        r#""\u{110000}""#,
        r#""\u{d800}""#,
        r#""\u{0000000}""#,
        r#""\u{zz}""#,
        r#""\u1234""#,
        "\"raw\nnewline\"",
    ] {
        for field in ["path", "key"] {
            let original = if field == "path" { "\"/x\"" } else { "\"k\"" };
            let invalid = line.replace(&format!("{field}={original}"), &format!("{field}={bad}"));
            assert!(parse_trace_line(&invalid).is_none(), "{invalid:?}");
        }
    }
}

#[test]
fn malformed_headers_and_incomplete_tokens_are_rejected() {
    let line = frame(7, 0, "/x", "k").trace_line();
    for (valid, invalid) in [
        ("ft=0x13", "ft=0x0x13"),
        ("ft=0x13", "ft=13"),
        ("ft=0x13", "ft=0x100"),
        ("cid=7", "cid=+7"),
        ("trace_id=7", "trace_id=18446744073709551616"),
        ("flags=0x0002", "flags=0x10002"),
        ("hop=0/8", "hop=0"),
        ("hop=0/8", "hop=0/8/9"),
        ("hop=0/8", "hop=256/8"),
        ("hop=0/8", "hop=0/no"),
        ("len=16", "len=-1"),
        ("len=16", "len=+16"),
        ("len=16", "len=18446744073709551616"),
    ] {
        assert!(line.contains(valid));
        let invalid = line.replace(valid, invalid);
        assert!(parse_trace_line(&invalid).is_none(), "{invalid}");
    }
    for suffix in [" orphan", " =x", " future=", " future=\"open"] {
        assert!(parse_trace_line(&format!("{line}{suffix}")).is_none());
    }
    // Future well-formed metadata can coexist with the current fields.
    assert!(parse_trace_line(&format!("{line} future=1")).is_some());
}

#[test]
fn field_order_is_irrelevant_and_quoted_values_cannot_override_metadata() {
    let line = "frame key=\"key trace_id=99 len=999\" path=\"/path flags=0x0000\" len=1 trace_id=7 flags=0x0002 hop=0/8 cid=7 ft=0x13";
    let event = parse_trace_line(line).unwrap();
    assert_eq!(event.trace_id, 7);
    assert_eq!(event.len, 1);
    assert_eq!(event.path, "/path flags=0x0000");
    assert_eq!(event.type_key, "key trace_id=99 len=999");
}

#[test]
fn aggregate_chooses_lowest_hop_even_when_frames_arrive_out_of_order() {
    let frames = [
        frame(7, 2, "/dst", "k"),
        frame(7, 1, "/mid", "k"),
        frame(7, 0, "/src", "k"),
    ];
    let mut expected = None;
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let trees = aggregate(order.into_iter().map(|i| frames[i].trace_line()));
        let tree = &trees[&7];
        assert_eq!(tree.root.path, "/src");
        assert_eq!(tree.root.event_count(), 1);
        assert_eq!(tree.total_events(), 3);
        assert_eq!(tree.root.children.len(), 2);
        assert_eq!(tree.root.children["/mid"].events[0].hop_count, 1);
        assert_eq!(tree.root.children["/dst"].events[0].hop_count, 2);
        let rendered = render_tree(tree);
        assert_eq!(expected.get_or_insert_with(|| rendered.clone()), &rendered);
    }
}

#[test]
fn equal_hop_root_is_deterministic_and_empty_path_is_not_an_unset_sentinel() {
    for paths in [["/z", "", "/a"], ["/a", "/z", ""], ["", "/a", "/z"]] {
        let trees = aggregate(
            paths
                .into_iter()
                .map(|path| frame(7, 0, path, "k").trace_line()),
        );
        let tree = &trees[&7];
        assert_eq!(tree.root.path, "");
        assert_eq!(tree.root.events.len(), 1);
        assert_eq!(tree.root.events[0].path, "");
        assert_eq!(tree.root.children.len(), 2);
        assert_eq!(tree.total_events(), 3);
    }
}

#[test]
fn regrouping_preserves_each_observation_and_separates_trace_ids() {
    let lines = [
        frame(7, 2, "/dst", "first"),
        frame(7, 1, "/mid", "second"),
        frame(7, 0, "/dst", "third"),
        frame(7, 1, "/mid", "fourth"),
        frame(8, 0, "/other", "fifth"),
    ];
    let trees = aggregate(lines.iter().map(Frame::trace_line));
    assert_eq!(trees.len(), 2);
    let tree = &trees[&7];
    assert_eq!(tree.root.path, "/dst");
    assert_eq!(tree.total_events(), 4);
    let keys: Vec<_> = tree
        .root
        .events
        .iter()
        .map(|e| e.type_key.as_str())
        .collect();
    assert_eq!(keys, ["first", "third"]);
    let keys: Vec<_> = tree.root.children["/mid"]
        .events
        .iter()
        .map(|e| e.type_key.as_str())
        .collect();
    assert_eq!(keys, ["second", "fourth"]);
    assert_eq!(trees[&8].total_events(), 1);
}

#[test]
fn rendering_keeps_untrusted_paths_on_one_safe_terminal_line() {
    let path = "/evil\ntrace 99 (999 frames)\r\t\0\u{1b}[2J\u{202e}";
    let trees = aggregate([frame(7, 0, path, "k").trace_line()].into_iter());
    let rendered = render_tree(&trees[&7]);
    assert_eq!(rendered.lines().count(), 2, "{rendered:?}");
    assert!(!rendered.contains(['\r', '\t', '\0', '\u{1b}', '\u{202e}']));
    assert!(rendered.contains(r"\ntrace 99"));
    assert!(rendered.contains(r"\u{1b}[2J"));
    assert_eq!(trees[&7].root.events[0].path, path);
}

#[test]
fn actual_transport_stderr_round_trips_into_trace_groups() {
    use parrot_remote::{
        handshake::HandshakeBody,
        transport::{run_connection, ConnParams, ConnSide, RuntimeKnobs},
    };
    use std::{
        io::{Read, Seek},
        process::{Command, Stdio},
        sync::Arc,
        time::{Duration, Instant},
    };
    const CHILD: &str = "PARROT_TRACE_TRANSPORT_PROBE";
    if std::env::var_os(CHILD).is_some() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    let (left, right) = tokio::io::duplex(4096);
                    let (shutdown, _) = tokio::sync::watch::channel(false);
                    let (in_a, mut rx_a) = tokio::sync::mpsc::channel(8);
                    let (in_b, mut rx_b) = tokio::sync::mpsc::channel(8);
                    let params = |side, name: &str, inbound| ConnParams {
                        side,
                        local_addr: None,
                        peer_addr: None,
                        scheme: "mem",
                        local_handshake: HandshakeBody {
                            node_id: name.into(),
                            ..Default::default()
                        },
                        inbound,
                        on_disconnect: Arc::new(|_| {}),
                        shutdown: shutdown.subscribe(),
                        knobs: Some(RuntimeKnobs {
                            heartbeat_enabled: false,
                            ..Default::default()
                        }),
                    };
                    let (a, b) = tokio::try_join!(
                        run_connection(left, params(ConnSide::Connect, "trace-a", in_a)),
                        run_connection(right, params(ConnSide::Accept, "trace-b", in_b))
                    )
                    .unwrap();
                    let outbound = frame(7001, 0, "/real path\n雪", "key \"one\"");
                    a.sender.send(outbound.clone()).await.unwrap();
                    let (received, _, _) = rx_b.recv().await.unwrap();
                    assert_eq!(received.path, outbound.path);
                    assert_eq!(received.type_key, outbound.type_key);
                    assert_eq!(received.payload, outbound.payload);
                    let reply = frame(7002, 0, "/return path", "key\\two");
                    b.sender.send(reply.clone()).await.unwrap();
                    let (received, _, _) = rx_a.recv().await.unwrap();
                    assert_eq!(received.path, reply.path);
                    assert_eq!(received.type_key, reply.type_key);
                    assert_eq!(received.payload, reply.payload);
                    shutdown.send_replace(true);
                    a.closed.await.unwrap();
                    b.closed.await.unwrap();
                })
                .await
                .expect("transport must deliver and retire within deadline");
            });
        return;
    }
    // Isolate PARROT_TRACE from parallel tests and capture the actual transport
    // eprintln output. A file avoids blocking on a full undrained stderr pipe.
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "actual_transport_stderr_round_trips_into_trace_groups",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("PARROT_TRACE", "frame")
        .stdout(Stdio::null())
        .stderr(stderr.try_clone().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("transport probe did not exit: {result:?}");
            }
        }
    };
    stderr.rewind().unwrap();
    let mut output = String::new();
    stderr.read_to_string(&mut output).unwrap();
    assert!(status.success(), "{status}: {output}");
    let lines: Vec<_> = output
        .lines()
        .filter(|line| line.starts_with("[parrot-trace]"))
        .collect();
    assert_eq!(lines.len(), 4, "{output}");
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("[parrot-trace] TX "))
            .count(),
        2
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("[parrot-trace] RX "))
            .count(),
        2
    );
    for line in &lines {
        let event = parse_trace_line(line).unwrap_or_else(|| panic!("actual log rejected: {line}"));
        let (path, key) = match event.trace_id {
            7001 => ("/real path\n雪", "key \"one\""),
            7002 => ("/return path", "key\\two"),
            other => panic!("unexpected cid {other}"),
        };
        assert_eq!(event.path, path);
        assert_eq!(event.type_key, key);
        assert_eq!(event.len, b"business payload".len());
    }
    let trees = aggregate(lines.into_iter().map(str::to_owned));
    assert_eq!(trees.len(), 2);
    assert_eq!(trees[&7001].total_events(), 2);
    assert_eq!(trees[&7002].total_events(), 2);
}
