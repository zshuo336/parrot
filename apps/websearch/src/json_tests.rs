use super::*;
use parrot_remote::codec_registry::CodecRegistry;

#[test]
fn search_json_handles_whitespace_escapes_braces_and_exponents() {
    let json = r#"[ { "doc": -7, "score": 1e-3, "url": "https://example.test/", "title": "\u4e2d\u6587", "snippet": "literal { text" } ]"#;
    let hits: Vec<_> = json_protocol::decode::<Vec<SearchHit>>(json.as_bytes())
        .unwrap()
        .into_iter()
        .map(|h| (h.score, h.url, h.title, h.snippet))
        .collect();
    assert_eq!(
        hits,
        vec![(
            0.001,
            "https://example.test/".into(),
            "中文".into(),
            "literal { text".into()
        )]
    );
}
#[test]
fn search_links_and_display_text_cannot_create_html_attributes() {
    let url = "https://example.test/\" onmouseover=\"alert(1)";
    let html = render_results(
        "q",
        &[(
            1.0,
            url.into(),
            "<title>".into(),
            "<script>text</script>".into(),
        )],
        1,
        10,
    );
    assert!(
        !html.contains("\" onmouseover=\""),
        "untrusted URL became an attribute"
    );
    assert!(!html.contains("<title><title>"));
    assert!(html.contains("&lt;script&gt;text&lt;/script&gt;"));
}
#[test]
fn document_links_cannot_create_html_attributes() {
    let json = r#"{"hosts":1,"total_docs":1,"offset":0,"items":[{"host":"example.test","pages":1,"sample":"https://example.test/\" onmouseover=\"alert(1)"}]}"#;
    let html = render_docs_page(&json_protocol::decode(json.as_bytes()).unwrap());
    assert!(
        !html.contains("\" onmouseover=\""),
        "sample URL became an attribute"
    );
}
#[test]
fn unsafe_schemes_are_displayed_without_active_links() {
    for url in [
        "javascript:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "file:///etc/passwd",
    ] {
        let html = render_results(
            "q",
            &[(1.0, url.into(), "title".into(), "text".into())],
            1,
            10,
        );
        assert!(
            !html.contains(&format!("href=\"{url}\"")),
            "unsafe link {url}"
        );
    }
}
#[test]
fn highlighting_never_reprocesses_its_own_markup() {
    let html = render_results(
        "parrot mark",
        &[(
            1.0,
            "https://example.test/".into(),
            "title".into(),
            "parrot mark".into(),
        )],
        1,
        10,
    );
    assert!(
        html.contains("<mark>parrot</mark> <mark>mark</mark>"),
        "{html}"
    );
}
#[test]
fn document_counts_preserve_u64_precision() {
    let html = render_docs_page(
        &json_protocol::decode(
            br#"{"hosts":1,"total_docs":9007199254740993,"offset":0,"items":[]}"#,
        )
        .unwrap(),
    );
    assert!(html.contains("9007199254740993"));
}
#[test]
fn json_reply_codecs_reject_invalid_documents() {
    for key in [
        "bin:ws/SearchResult",
        "bin:ws/HealthzR",
        "bin:ws/ListDocsR",
        "bin:ws/ListTermsR",
    ] {
        for bytes in [b"{".as_slice(), b"null", b"42", b"{\"items\":[]}", b"\xff"] {
            assert!(
                CodecRegistry::global().decode_incoming(key, bytes).is_err(),
                "accepted invalid {key}"
            );
        }
    }
}

#[test]
fn json_schema_preserves_postings_without_metadata_and_exact_counters() {
    let hits: Vec<SearchHit> = json_protocol::decode(br#"[{"doc":-1,"score":1.25}]"#).unwrap();
    assert_eq!(hits[0].doc, -1);
    assert!(hits[0].url.is_empty());
    let health: Health = json_protocol::decode(
        br#"{"terms":0,"postings":18446744073709551615,"queries":0,"docs":0,"segments":0}"#,
    )
    .unwrap();
    assert_eq!(health.postings, u64::MAX);
    assert!(json_protocol::decode::<Health>(
        br#"{"terms":-1,"postings":0,"queries":0,"docs":0,"segments":0}"#
    )
    .is_err());
    for json in [
        br#"[{"doc":1,"score":1e999}]"#.as_slice(),
        br#"[{"doc":1,"score":1,"url":null}]"#,
        br#"[{"doc":1,"score":1}] null"#,
    ] {
        assert!(json_protocol::decode::<Vec<SearchHit>>(json).is_err());
    }
}
#[test]
fn highlighting_escapes_raw_matches_and_handles_unicode_and_overlaps() {
    assert_eq!(
        highlight("<img onerror=x> &lt;", "<img &lt;"),
        "<mark>&lt;img</mark> onerror=x&gt; <mark>&amp;lt;</mark>"
    );
    assert_eq!(
        highlight("中文🙂中文", "中文 中 🙂"),
        "<mark>中文</mark><mark>🙂</mark><mark>中文</mark>"
    );
    assert_eq!(highlight("parrot", "par parrot"), "<mark>parrot</mark>");
}
#[test]
fn listing_rows_use_response_offsets_and_large_counts_without_overflow() {
    let page: DocumentPage = json_protocol::decode(br#"{"hosts":9007199254740993,"total_docs":9007199254740993,"offset":100,"items":[{"host":"<host>","pages":18446744073709551615,"sample":"javascript:alert(1)"}]}"#).unwrap();
    let html = render_docs_page(&page);
    assert!(html.contains(">101</td>"));
    assert!(html.contains("18446744073709551615"));
    assert!(html.contains("&lt;host&gt;"));
    assert!(!html.contains("href=\"javascript:"));
    let terms: TermPage = json_protocol::decode(br#"{"total":9007199254740993,"offset":300,"items":[{"term":"<mark>","df":9007199254740993}]}"#).unwrap();
    let html = render_terms_page(&terms);
    assert!(html.contains(">301</td>"));
    assert!(html.contains("9007199254740993"));
}
#[tokio::test]
async fn malformed_json_is_a_tcp_error_and_following_valid_json_succeeds() {
    let (bad, good) = reply_peer::malformed_then_valid(
        || Box::new(WsSearch(enc_search(10, "term"))),
        "bin:ws/SearchResult",
        b"[{\"doc\":1,\"score\":1},".to_vec(),
        b"[{\"doc\":-1,\"score\":1}]".to_vec(),
    )
    .await;
    assert!(bad.is_err());
    let WsSearchResult(hits) = expect_reply(good.unwrap()).unwrap();
    assert_eq!(hits[0].doc, -1);
}

#[tokio::test]
async fn http_reports_invalid_upstream_json_and_recovers_on_the_same_connection() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (route, key, valid, expected) in [
        (
            "/stats",
            "bin:ws/HealthzR",
            r#"{"terms":2,"postings":3,"queries":0,"docs":1,"segments":1}"#,
            "\"docs\":1",
        ),
        (
            "/search?q=mark",
            "bin:ws/SearchResult",
            r#"[{"doc":-1,"score":1,"url":"javascript:alert(1)","title":"<title>","snippet":"mark"}]"#,
            "&lt;title&gt;",
        ),
        (
            "/docs",
            "bin:ws/ListDocsR",
            r#"{"hosts":1,"total_docs":1,"offset":0,"items":[{"host":"example.test","pages":1,"sample":"https://example.test/\" onmouseover=\"alert(1)"}]}"#,
            "&quot;",
        ),
        (
            "/terms",
            "bin:ws/ListTermsR",
            r#"{"total":1,"offset":0,"items":[{"term":"<mark>","df":1}]}"#,
            "&lt;mark&gt;",
        ),
    ] {
        reply_peer::with_replies(key, b"{".to_vec(), valid.as_bytes().to_vec(), |client, target| async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let web = tokio::spawn(serve_web(listener, client, Box::new(target), shutdown::Stop::new()));
            for bad in [true, false] {
                let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                stream.write_all(format!("GET {route} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                let mut response = String::new();
                tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await.unwrap().unwrap();
                if bad {
                    assert!(response.starts_with("HTTP/1.1 50"), "{route}: {response}");
                    assert!(response.contains("codec:"), "{response}");
                } else {
                    assert!(response.starts_with("HTTP/1.1 200 OK"), "{route}: {response}");
                    assert!(response.contains(expected), "{route}: {response}");
                    assert!(!response.contains("href=\"javascript:"));
                    assert!(!response.contains("\" onmouseover=\""));
                }
            }
            web.abort();
            assert!(web.await.unwrap_err().is_cancelled());
        }).await;
    }
}

#[tokio::test]
async fn stop_retires_http_listener_and_idle_connections() {
    retire_http(false).await;
}

#[tokio::test]
async fn aborted_server_cannot_leave_detached_http_connections() {
    retire_http(true).await;
}

async fn retire_http(abort: bool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    reply_peer::with_script("bin:ws/HealthzR", vec![], |client, target| async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = shutdown::Stop::new();
        let web = tokio::spawn(serve_web(listener, client, Box::new(target), stop.clone()));
        let mut idle = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut active = tokio::net::TcpStream::connect(addr).await.unwrap();
        active
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), active.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        if abort {
            web.abort();
            assert!(web.await.unwrap_err().is_cancelled());
        } else {
            stop.request();
            tokio::time::timeout(Duration::from_secs(2), web)
                .await
                .unwrap()
                .unwrap();
        }
        let mut byte = [0];
        let read = tokio::time::timeout(Duration::from_secs(1), idle.read(&mut byte))
            .await
            .unwrap();
        assert!(
            matches!(read, Ok(0))
                || matches!(read, Err(ref e) if e.kind()==std::io::ErrorKind::ConnectionReset)
        );
        let _rebound = tokio::net::TcpListener::bind(addr).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn http_connection_budget_releases_capacity_after_idle_peer_closes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    reply_peer::with_script("bin:ws/HealthzR", vec![], |client, target| async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = shutdown::Stop::new();
        let web = tokio::spawn(serve_web(listener, client, Box::new(target), stop.clone()));
        let mut idle = Vec::new();
        for _ in 0..128 {
            idle.push(tokio::net::TcpStream::connect(addr).await.unwrap());
        }
        let mut waiting = tokio::net::TcpStream::connect(addr).await.unwrap();
        waiting
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut first = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(150), waiting.read(&mut first))
                .await
                .is_err(),
            "a request beyond the active-connection budget was admitted"
        );
        drop(idle.pop());
        let mut body = String::new();
        tokio::time::timeout(Duration::from_secs(2), waiting.read_to_string(&mut body))
            .await
            .unwrap()
            .unwrap();
        assert!(body.starts_with("HTTP/1.1 200"));
        stop.request();
        tokio::time::timeout(Duration::from_secs(2), web)
            .await
            .unwrap()
            .unwrap();
    })
    .await;
}
