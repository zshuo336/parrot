use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LIMIT: usize = 1 << 20;

/// Hold the response open after the supplied bytes. The client must decide
/// whether it has exceeded its budget without waiting for EOF or a timeout.
async fn response(
    headers: String,
    body: Vec<u8>,
) -> (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/page", listener.local_addr().unwrap());
    let (release, done) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut socket = loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0];
                if socket.read_exact(&mut byte).await.is_err() {
                    return;
                }
                request.push(byte[0]);
                assert!(request.len() < 8192);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            if String::from_utf8_lossy(&request).split_whitespace().nth(1) == Some("/robots.txt") {
                socket
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                continue;
            }
            break socket;
        };
        let _ = socket.write_all(headers.as_bytes()).await;
        let _ = socket.write_all(&body).await;
        let _ = done.await;
    });
    (url, release, task)
}
fn fetcher() -> Fetcher {
    let mut fetcher = Fetcher::for_tests();
    fetcher.client = Fetcher::client_builder_with_policy(true)
        .no_proxy()
        .build()
        .unwrap();
    fetcher
}

#[tokio::test]
async fn declared_oversized_page_is_rejected_without_waiting_for_body() {
    let (url, release, server) = response(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
            LIMIT + 1
        ),
        vec![],
    )
    .await;
    let fetcher = fetcher();
    let result = tokio::time::timeout(Duration::from_secs(1), fetcher.fetch(&url)).await;
    let _ = release.send(());
    server.await.unwrap();
    assert!(result
        .expect("declared size must be checked before reading body")
        .unwrap_err()
        .contains("body exceeds"));
}

#[tokio::test]
async fn unknown_length_page_is_rejected_before_eof() {
    let (url, release, server) = response(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n".into(),
        vec![b'a'; LIMIT + 1],
    )
    .await;
    let fetcher = fetcher();
    let result = tokio::time::timeout(Duration::from_secs(1), fetcher.fetch(&url)).await;
    let _ = release.send(());
    server.await.unwrap();
    assert!(result
        .expect("must stop reading when the decoded body exceeds its budget")
        .unwrap_err()
        .contains("body exceeds"));
}

#[tokio::test]
async fn complete_oversized_page_is_not_silently_indexed_as_truncated_html() {
    let (url, release, server) = response(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
            LIMIT + 1
        ),
        vec![b'a'; LIMIT + 1],
    )
    .await;
    let result = fetcher().fetch(&url).await;
    let _ = release.send(());
    server.await.unwrap();
    assert!(
        matches!(result, Err(detail) if detail.contains("body exceeds")),
        "oversized HTML must be rejected, not indexed as a truncated success"
    );
}

#[tokio::test]
async fn exact_budget_page_preserves_all_bytes() {
    let mut body = vec![b'a'; LIMIT];
    body[LIMIT - 1] = b'z';
    let (url, release, server) = response(
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {LIMIT}\r\n\r\n"),
        body.clone(),
    )
    .await;
    let result = fetcher().fetch(&url).await;
    let _ = release.send(());
    server.await.unwrap();
    let (text, _, _) = result.unwrap();
    assert_eq!(text.as_bytes(), body);
}

#[tokio::test]
async fn gzip_expansion_is_checked_against_decoded_budget() {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&vec![b'a'; LIMIT + 1]).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(compressed.len() < LIMIT);
    let (url, release, server) = response(
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n", compressed.len()),
        compressed,
    ).await;
    let result = fetcher().fetch(&url).await;
    let _ = release.send(());
    server.await.unwrap();
    assert!(
        matches!(result, Err(detail) if detail.contains("body exceeds")),
        "compressed wire length must not bypass the decoded body budget"
    );
}

#[tokio::test]
async fn production_fetcher_rejects_private_literal_before_network_io() {
    let error = Fetcher::new()
        .fetch("http://127.0.0.1:9/private")
        .await
        .expect_err("production crawler must reject loopback targets");
    assert!(
        error.contains("private or reserved"),
        "unexpected error: {error}"
    );
}

#[test]
fn chinese_charset_decoding_has_no_pipe_capacity_deadlock() {
    let (tx, rx) = std::sync::mpsc::channel();
    let decoder = std::thread::spawn(move || {
        let _ = tx.send(decode_gbk(&[0xd6, 0xd0, 0xce, 0xc4].repeat(128 * 1024)));
    });
    let text = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("large GBK decoding blocked");
    decoder.join().unwrap();
    assert!(text == "中文".repeat(128 * 1024), "GBK content changed");
}

#[tokio::test]
async fn gb18030_four_byte_characters_survive_http_fetch() {
    // Independent GB18030 vector for 中文😀, including a four-byte character.
    let bytes = vec![0xd6, 0xd0, 0xce, 0xc4, 0x94, 0x39, 0xfc, 0x36];
    let (url, release, server) = response(
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=gb18030\r\nContent-Length: {}\r\n\r\n", bytes.len()),
        bytes,
    ).await;
    let result = fetcher().fetch(&url).await;
    let _ = release.send(());
    server.await.unwrap();
    assert_eq!(result.unwrap().0, "中文😀");
}
