use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    location: Option<String>,
    entered: Option<Arc<tokio::sync::Notify>>,
    release: Option<Arc<tokio::sync::Semaphore>>,
    after_body: Option<Arc<tokio::sync::Semaphore>>,
    omit_length: bool,
    encoding: Option<&'static str>,
}
impl Reply {
    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.as_bytes().to_vec(),
            location: None,
            entered: None,
            release: None,
            after_body: None,
            omit_length: false,
            encoding: None,
        }
    }
}
struct Site {
    origin: String,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
    headers: Arc<std::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Site {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Site {
    async fn new(route: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let headers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded_headers = headers.clone();
        let recorded = requests.clone();
        let route = Arc::new(route);
        let task = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        let recorded = recorded.clone();
                        let route = route.clone();
                        let recorded_headers = recorded_headers.clone();
                        clients.spawn(async move {
                            let mut request = Vec::new();
                            loop {
                                let mut byte = [0];
                                if socket.read_exact(&mut byte).await.is_err() { return; }
                                request.push(byte[0]);
                                if request[0] == 0x16 || request.ends_with(b"\r\n\r\n") { break; }
                                assert!(request.len() < 16384);
                            }
                            let wire = String::from_utf8_lossy(&request);
                            recorded_headers.lock().unwrap().push(wire.to_string());
                            let path = if request[0] == 0x16 { "TLS-on-HTTP" } else { wire.split_whitespace().nth(1).unwrap() };
                            recorded.lock().unwrap().push(path.to_owned());
                            // Gate old forced-HTTPS attempts too, so the old global
                            // cache lock can be observed while its network request waits.
                            let reply = route(if path == "TLS-on-HTTP" { "/robots.txt" } else { path });
                            if let Some(entered) = reply.entered { entered.notify_one(); }
                            if let Some(release) = reply.release { let _ = release.acquire().await.unwrap(); }
                            if path == "TLS-on-HTTP" { return; }
                            let mut headers = format!("HTTP/1.1 {} response\r\nContent-Type: text/html\r\nConnection: close\r\n", reply.status);
                            if !reply.omit_length { headers.push_str(&format!("Content-Length: {}\r\n", reply.body.len())); }
                            if let Some(encoding) = reply.encoding { headers.push_str(&format!("Content-Encoding: {encoding}\r\n")); }
                            if let Some(location) = reply.location { headers.push_str(&format!("Location: {location}\r\n")); }
                            headers.push_str("\r\n");
                            let _ = socket.write_all(headers.as_bytes()).await;
                            let _ = socket.write_all(&reply.body).await;
                            if let Some(gate) = reply.after_body { let _ = gate.acquire().await; }
                        });
                    },
                    result = clients.join_next(), if !clients.is_empty() => { result.unwrap().unwrap(); },
                }
            }
        });
        Self {
            origin,
            requests,
            headers,
            task,
        }
    }
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }
    fn paths(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
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
async fn http_origin_robots_are_fetched_at_the_actual_scheme_and_port() {
    let site = Site::new(|_| Reply::text(200, "User-agent: *\nDisallow: /private\n")).await;
    let allowed = fetcher()
        .robots_allowed(&site.url("/private/account"))
        .await;
    assert!(!allowed, "HTTP-origin policy was ignored");
    assert_eq!(site.paths(), vec!["/robots.txt"]);
}

#[tokio::test]
async fn robots_server_failure_is_not_permission_to_crawl() {
    let site = Site::new(|_| Reply::text(503, "unavailable")).await;
    assert!(!fetcher().robots_allowed(&site.url("/private")).await);
}

#[tokio::test]
async fn slow_robots_origin_does_not_lock_other_origins() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (signal, gate) = (entered.clone(), release.clone());
    let slow = Site::new(move |_| Reply {
        entered: Some(signal.clone()),
        release: Some(gate.clone()),
        ..Reply::text(404, "")
    })
    .await;
    let fast = Site::new(|_| Reply::text(404, "")).await;
    let fetcher = Arc::new(fetcher());
    let caller = fetcher.clone();
    let url = slow.url("/page");
    let task = tokio::spawn(async move { caller.robots_allowed(&url).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let other = tokio::time::timeout(
        Duration::from_millis(500),
        fetcher.robots_allowed(&fast.url("/page")),
    )
    .await;
    release.add_permits(1);
    task.await.unwrap();
    assert!(other.expect("one origin's network wait held the global cache lock"));
}

#[tokio::test]
async fn page_redirect_cannot_bypass_target_robots_policy() {
    let target = Site::new(|path| {
        if path == "/robots.txt" {
            Reply::text(200, "User-agent: *\nDisallow: /private\n")
        } else {
            Reply::text(200, "must not be fetched")
        }
    })
    .await;
    let destination = target.url("/private");
    let source = Site::new(move |path| {
        if path == "/robots.txt" {
            Reply::text(404, "")
        } else {
            Reply {
                location: Some(destination.clone()),
                ..Reply::text(302, "")
            }
        }
    })
    .await;
    let result = fetcher().fetch(&source.url("/start")).await;
    assert!(
        result.is_err(),
        "automatic redirect fetched a forbidden target"
    );
    assert_eq!(target.paths(), vec!["/robots.txt"]);
}

#[tokio::test]
async fn invalid_or_credential_bearing_urls_are_not_crawlable() {
    let fetcher = fetcher();
    for url in [
        "not a URL",
        "file:///etc/hosts",
        "http://user:password@127.0.0.1:1/private",
    ] {
        assert!(
            !fetcher.robots_allowed(url).await,
            "invalid crawl target was allowed"
        );
    }
}

#[tokio::test]
async fn concurrent_requests_share_one_origin_policy_download() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (signal, gate) = (entered.clone(), release.clone());
    let site = Site::new(move |_| Reply {
        entered: Some(signal.clone()),
        release: Some(gate.clone()),
        ..Reply::text(404, "")
    })
    .await;
    let fetcher = Arc::new(fetcher());
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..32 {
        let caller = fetcher.clone();
        let url = site.url(&format!("/page/{index}"));
        tasks.spawn(async move { caller.robots_allowed(&url).await });
    }
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    release.add_permits(1);
    while let Some(result) = tasks.join_next().await {
        assert!(result.unwrap());
    }
    assert_eq!(site.paths(), vec!["/robots.txt"]);
}

#[tokio::test]
async fn cancelled_loader_releases_origin_lock_and_global_permit() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (signal, gate) = (entered.clone(), release.clone());
    let site = Site::new(move |_| Reply {
        entered: Some(signal.clone()),
        release: Some(gate.clone()),
        ..Reply::text(404, "")
    })
    .await;
    let mut configured = fetcher();
    configured.robots = robots::RobotsCache::with_limits_allow_private(2, 4096, 1);
    let fetcher = Arc::new(configured);
    let launch = || {
        let caller = fetcher.clone();
        let url = site.url("/page");
        tokio::spawn(async move { caller.robots_allowed(&url).await })
    };
    let first = launch();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    first.abort();
    let _ = first.await;
    let second = launch();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("cancelled loader retained admission");
    release.add_permits(1);
    assert!(second.await.unwrap());
    assert!(fetcher.robots_allowed(&site.url("/later")).await);
    assert_eq!(site.paths(), vec!["/robots.txt", "/robots.txt"]);
}

#[tokio::test]
async fn failed_policy_is_cached_briefly_then_retried() {
    use std::sync::atomic::AtomicUsize;
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let site = Site::new(move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::text(503, "")
        } else {
            Reply::text(404, "")
        }
    })
    .await;
    let fetcher = fetcher();
    assert!(!fetcher.robots_allowed(&site.url("/page")).await);
    assert!(!fetcher.robots_allowed(&site.url("/page")).await);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    assert!(fetcher.robots_allowed(&site.url("/page")).await);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn successful_rules_are_refreshed_after_twenty_four_hours() {
    use std::sync::atomic::AtomicUsize;
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let site = Site::new(move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::text(200, "User-agent: *\nDisallow:\n")
        } else {
            Reply::text(200, "User-agent: *\nDisallow: /\n")
        }
    })
    .await;
    let fetcher = fetcher();
    assert!(fetcher.robots_allowed(&site.url("/page")).await);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(24 * 60 * 60 + 1)).await;
    tokio::time::resume();
    assert!(!fetcher.robots_allowed(&site.url("/page")).await);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn robots_redirect_rules_belong_to_original_origin() {
    let policy = Site::new(|path| {
        if path == "/policy" {
            Reply::text(200, "User-agent: *\nDisallow: /private\n")
        } else {
            Reply::text(404, "")
        }
    })
    .await;
    let destination = policy.url("/policy");
    let source = Site::new(move |_| Reply {
        location: Some(destination.clone()),
        ..Reply::text(302, "")
    })
    .await;
    let fetcher = fetcher();
    assert!(!fetcher.robots_allowed(&source.url("/private")).await);
    assert!(fetcher.robots_allowed(&source.url("/public")).await);
    assert!(fetcher.robots_allowed(&policy.url("/private")).await);
    assert_eq!(source.paths(), vec!["/robots.txt"]);
    assert_eq!(policy.paths(), vec!["/policy", "/robots.txt"]);
}

#[tokio::test]
async fn five_robots_redirects_are_followed() {
    let site = Site::new(|path| {
        let hop = if path == "/robots.txt" {
            0
        } else {
            path.trim_start_matches('/').parse::<usize>().unwrap()
        };
        if hop == 5 {
            Reply::text(200, "User-agent: *\nDisallow: /private\n")
        } else {
            Reply {
                location: Some(format!("/{}", hop + 1)),
                ..Reply::text(302, "")
            }
        }
    })
    .await;
    assert!(!fetcher().robots_allowed(&site.url("/private")).await);
    assert_eq!(
        site.paths(),
        vec!["/robots.txt", "/1", "/2", "/3", "/4", "/5"]
    );
}

#[tokio::test]
async fn unknown_length_oversized_robots_stops_before_eof() {
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let site = Site::new(move |_| Reply {
        body: vec![b' '; 500 * 1024 + 1],
        omit_length: true,
        after_body: Some(gate.clone()),
        ..Reply::text(200, "")
    })
    .await;
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        fetcher().robots_allowed(&site.url("/page")),
    )
    .await;
    release.add_permits(1);
    assert!(!result.expect("robots budget must stop before EOF"));
}

#[tokio::test]
async fn compressed_robots_is_limited_after_decompression() {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&vec![b' '; 500 * 1024 + 1]).unwrap();
    let body = encoder.finish().unwrap();
    let site = Site::new(move |_| Reply {
        body: body.clone(),
        encoding: Some("gzip"),
        ..Reply::text(200, "")
    })
    .await;
    assert!(!fetcher().robots_allowed(&site.url("/page")).await);
}

#[tokio::test]
async fn allowed_page_redirect_returns_final_url_for_indexing_and_relative_links() {
    let site = Site::new(|path| match path {
        "/robots.txt" => Reply::text(404, ""),
        "/start" => Reply {
            location: Some("/dir/page#fragment".into()),
            ..Reply::text(302, "")
        },
        "/dir/page" => Reply::text(200, "<a href='child'>child</a>"),
        _ => panic!("unexpected path {path}"),
    })
    .await;
    let (body, _, url) = fetcher().fetch(&site.url("/start")).await.unwrap();
    assert!(body.contains("child"));
    assert_eq!(url, site.url("/dir/page"));
    assert_eq!(normalize_url(&url, "child"), Some(site.url("/dir/child")));
    assert_eq!(site.paths(), vec!["/robots.txt", "/start", "/dir/page"]);
}

#[tokio::test]
async fn redirect_loops_and_invalid_schemes_do_not_issue_extra_requests() {
    for location in [
        "/start",
        "file:///etc/hosts",
        "http://user:secret@127.0.0.1/",
    ] {
        let site = Site::new(move |path| {
            if path == "/robots.txt" {
                Reply::text(404, "")
            } else {
                Reply {
                    location: Some(location.into()),
                    ..Reply::text(302, "")
                }
            }
        })
        .await;
        assert!(fetcher().fetch(&site.url("/start")).await.is_err());
        assert_eq!(site.paths(), vec!["/robots.txt", "/start"]);
    }
}

#[tokio::test]
async fn schemes_are_separate_cache_origins() {
    let site = Site::new(|_| Reply::text(404, "")).await;
    let fetcher = fetcher();
    assert!(fetcher.robots_allowed(&site.url("/page")).await);
    assert!(
        !fetcher
            .robots_allowed(&site.url("/page").replacen("http:", "https:", 1))
            .await
    );
    assert!(fetcher.robots_allowed(&site.url("/page")).await);
    assert_eq!(site.paths(), vec!["/robots.txt", "TLS-on-HTTP"]);
}

#[tokio::test]
async fn cache_origin_and_rule_memory_limits_are_enforced() {
    let mut fetcher = fetcher();
    fetcher.robots = robots::RobotsCache::with_limits_allow_private(2, 1024, 2);
    let mut sites = Vec::new();
    for _ in 0..4 {
        let site = Site::new(|_| Reply::text(404, "")).await;
        assert!(fetcher.robots_allowed(&site.url("/page")).await);
        assert!(fetcher.robots.stats().0 <= 2);
        sites.push(site);
    }
    assert!(fetcher.robots_allowed(&sites[0].url("/again")).await);
    assert_eq!(sites[0].paths().len(), 2, "evicted origin must reload");
    let oversized = Site::new(|_| {
        Reply::text(
            200,
            &format!("User-agent: *\nDisallow: /{}\n", "x".repeat(2048)),
        )
    })
    .await;
    assert!(!fetcher.robots_allowed(&oversized.url("/page")).await);
    assert!(fetcher.robots.stats().1 <= 1024);
}

#[tokio::test]
async fn loading_limit_rejects_without_network_and_recovers() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (signal, gate) = (entered.clone(), release.clone());
    let slow = Site::new(move |_| Reply {
        entered: Some(signal.clone()),
        release: Some(gate.clone()),
        ..Reply::text(404, "")
    })
    .await;
    let fast = Site::new(|_| Reply::text(404, "")).await;
    let mut fetcher = fetcher();
    fetcher.robots = robots::RobotsCache::with_limits_allow_private(2, 1024, 1);
    let fetcher = Arc::new(fetcher);
    let caller = fetcher.clone();
    let url = slow.url("/page");
    let first = tokio::spawn(async move { caller.robots_allowed(&url).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    assert!(!fetcher.robots_allowed(&fast.url("/page")).await);
    assert!(fast.paths().is_empty());
    release.add_permits(1);
    assert!(first.await.unwrap());
    assert!(fetcher.robots_allowed(&fast.url("/page")).await);
}

#[tokio::test]
async fn actual_request_identifies_the_robot_used_for_group_selection() {
    let site = Site::new(|_| {
        Reply::text(
            200,
            "User-agent: *\nAllow: /\nUser-agent: ParrotWebSearch\nDisallow: /private\n",
        )
    })
    .await;
    assert!(!fetcher().robots_allowed(&site.url("/private")).await);
    assert!(site.headers.lock().unwrap()[0]
        .to_ascii_lowercase()
        .contains("user-agent: parrotwebsearch/1.0\r\n"));
}

#[tokio::test]
async fn direct_fetch_enforces_denial_before_requesting_page() {
    let site = Site::new(|_| {
        Reply::text(
            200,
            "User-agent: *\nDisallow: /private\nAllow: /private/open\n",
        )
    })
    .await;
    let result = fetcher().fetch(&site.url("/private")).await;
    assert!(matches!(result, Err(detail) if detail.starts_with("robots denied")));
    assert_eq!(site.paths(), vec!["/robots.txt"]);
}

#[tokio::test]
async fn robots_and_page_redirect_chains_have_explicit_limits() {
    let site = Site::new(|path| {
        let hop = if path == "/robots.txt" {
            0
        } else {
            path.trim_start_matches('/').parse::<usize>().unwrap()
        };
        Reply {
            location: Some(format!("/{}", hop + 1)),
            ..Reply::text(302, "")
        }
    })
    .await;
    assert!(!fetcher().robots_allowed(&site.url("/page")).await);
    assert_eq!(site.paths().len(), 6);
    let page = Site::new(|path| {
        if path == "/robots.txt" {
            Reply::text(404, "")
        } else {
            let hop = path.trim_start_matches('/').parse::<usize>().unwrap();
            Reply {
                location: Some(format!("/{}", hop + 1)),
                ..Reply::text(302, "")
            }
        }
    })
    .await;
    assert!(
        matches!(fetcher().fetch(&page.url("/0")).await, Err(detail) if detail == "too many page redirects")
    );
    assert_eq!(
        page.paths(),
        vec!["/robots.txt", "/0", "/1", "/2", "/3", "/4", "/5"]
    );
}

#[tokio::test]
async fn explicit_refusal_and_rate_limit_remain_closed() {
    for status in [401, 403, 429] {
        let site = Site::new(move |_| Reply::text(status, "")).await;
        assert!(
            !fetcher().robots_allowed(&site.url("/page")).await,
            "status {status}"
        );
    }
}

#[test]
fn standard_relative_resolution_preserves_resource_identity() {
    for (base, link, expected) in [
        (
            "http://EXAMPLE.test:80/dir/page",
            "child",
            "http://example.test/dir/child",
        ),
        (
            "http://example.test/dir/page/",
            "child",
            "http://example.test/dir/page/child",
        ),
        (
            "http://example.test/dir/page",
            "../next",
            "http://example.test/next",
        ),
        (
            "http://example.test/dir/page?old=1",
            "?new=2",
            "http://example.test/dir/page?new=2",
        ),
        (
            "http://example.test/dir/page?old=1",
            "#anchor",
            "http://example.test/dir/page?old=1",
        ),
        (
            "http://example.test/dir/page",
            "//other.test/path",
            "http://other.test/path",
        ),
        (
            "https://example.test/dir/page",
            "/目录/",
            "https://example.test/%E7%9B%AE%E5%BD%95/",
        ),
        ("http://[::1]:8080/path", "/next", "http://[::1]:8080/next"),
        (
            "http://example.test/",
            "/?q=a%2Fb&x=+&UTM_source=s#anchor",
            "http://example.test/?q=a%2Fb&x=+",
        ),
    ] {
        assert_eq!(
            normalize_url(base, link).as_deref(),
            Some(expected),
            "{base} + {link}"
        );
    }
    for target in [
        "javascript:alert(1)",
        "mailto:user@example.test",
        "file:///etc/hosts",
        "http://user:secret@example.test/",
    ] {
        assert!(
            normalize_url("http://example.test/", target).is_none(),
            "{target}"
        );
    }
    assert_eq!(host_of("http://EXAMPLE.test:80?query=1"), "example.test");
    assert_eq!(host_of("http://[::1]:8080/page"), "[::1]:8080");
}

#[tokio::test]
async fn scoped_ip_origin_fetches_local_corpus_but_never_another_port() {
    let site = Site::new(|path| {
        Reply::text(
            200,
            if path == "/robots.txt" {
                "User-agent: *\nAllow: /\n"
            } else {
                "<html><title>scope</title><body>local corpus</body></html>"
            },
        )
    })
    .await;
    let outside = Site::new(|_| Reply::text(200, "unexpected")).await;
    let fetcher = Fetcher::for_origin(url::Url::parse(&site.origin).unwrap());
    let (body, content_type, final_url) = fetcher.fetch(&site.url("/page")).await.unwrap();
    assert_eq!(
        body,
        "<html><title>scope</title><body>local corpus</body></html>"
    );
    assert_eq!(content_type, "text/html");
    assert_eq!(final_url, site.url("/page"));
    assert!(fetcher
        .fetch(&outside.url("/page"))
        .await
        .unwrap_err()
        .contains("outside --crawl-origin"));
    assert!(!fetcher.robots_allowed(&outside.url("/robots.txt")).await);
    assert!(outside.requests.lock().unwrap().is_empty());
    assert_eq!(*site.requests.lock().unwrap(), ["/robots.txt", "/page"]);
}

#[tokio::test]
async fn scoped_origin_rejects_page_and_robots_redirects_before_contacting_destination() {
    for robots_redirect in [false, true] {
        let outside = Site::new(|_| Reply::text(200, "unexpected")).await;
        let destination = outside.url("/escaped");
        let site = Site::new(move |path| {
            if (path == "/robots.txt") == robots_redirect {
                let mut reply = Reply::text(302, "");
                reply.location = Some(destination.clone());
                reply
            } else {
                Reply::text(200, "User-agent: *\nAllow: /\n")
            }
        })
        .await;
        let fetcher = Fetcher::for_origin(url::Url::parse(&site.origin).unwrap());
        assert!(fetcher.fetch(&site.url("/page")).await.is_err());
        assert!(
            outside.requests.lock().unwrap().is_empty(),
            "robots redirect={robots_redirect}"
        );
        let expected = if robots_redirect {
            vec!["/robots.txt"]
        } else {
            vec!["/robots.txt", "/page"]
        };
        assert_eq!(*site.requests.lock().unwrap(), expected);
    }
}

#[tokio::test]
async fn scoped_hostname_does_not_grant_private_dns_resolution() {
    let site = Site::new(|_| Reply::text(200, "unexpected")).await;
    let address = site.origin.replace("127.0.0.1", "localhost");
    let fetcher = Fetcher::for_origin(url::Url::parse(&address).unwrap());
    assert!(fetcher.fetch(&(address + "/page")).await.is_err());
    assert!(site.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn scoped_origin_allows_same_origin_page_and_robots_redirects() {
    let site = Site::new(|path| match path {
        "/robots.txt" | "/page" => {
            let mut reply = Reply::text(302, "");
            reply.location = Some(
                if path == "/robots.txt" {
                    "/policy"
                } else {
                    "/final"
                }
                .into(),
            );
            reply
        }
        "/policy" => Reply::text(200, "User-agent: *\nAllow: /\n"),
        _ => Reply::text(
            200,
            "<html><title>accepted</title><body>same origin</body></html>",
        ),
    })
    .await;
    let fetcher = Fetcher::for_origin(url::Url::parse(&site.origin).unwrap());
    let (body, content_type, final_url) = fetcher.fetch(&site.url("/page")).await.unwrap();
    assert_eq!(
        body,
        "<html><title>accepted</title><body>same origin</body></html>"
    );
    assert_eq!(content_type, "text/html");
    assert_eq!(final_url, site.url("/final"));
    assert_eq!(
        *site.requests.lock().unwrap(),
        ["/robots.txt", "/policy", "/page", "/final"]
    );
}

#[tokio::test]
async fn multiple_origins_allow_authorized_redirects_but_do_not_widen_dns_or_ports() {
    for robots_redirect in [false, true] {
        let outside = Site::new(|_| Reply::text(200, "unexpected")).await;
        let target = Site::new(|path| {
            Reply::text(
                200,
                if path == "/policy" || path == "/robots.txt" {
                    "User-agent: *\nAllow: /\n"
                } else {
                    "<html><body>authorized second origin</body></html>"
                },
            )
        })
        .await;
        let redirect = target.url(if robots_redirect { "/policy" } else { "/final" });
        let source = Site::new(move |path| {
            if (path == "/robots.txt") == robots_redirect {
                let mut reply = Reply::text(302, "");
                reply.location = Some(redirect.clone());
                reply
            } else {
                Reply::text(
                    200,
                    if path == "/robots.txt" {
                        "User-agent: *\nAllow: /\n"
                    } else {
                        "<html>source</html>"
                    },
                )
            }
        })
        .await;
        let scoped_hostname = outside.origin.replace("127.0.0.1", "localhost");
        let fetcher = Fetcher::for_origins(vec![
            url::Url::parse(&source.origin).unwrap(),
            url::Url::parse(&target.origin).unwrap(),
            url::Url::parse(&scoped_hostname).unwrap(),
        ]);
        let (_, _, final_url) = fetcher.fetch(&source.url("/page")).await.unwrap();
        assert_eq!(
            final_url,
            if robots_redirect {
                source.url("/page")
            } else {
                target.url("/final")
            }
        );
        assert_eq!(*source.requests.lock().unwrap(), ["/robots.txt", "/page"]);
        assert_eq!(
            *target.requests.lock().unwrap(),
            if robots_redirect {
                vec!["/policy"]
            } else {
                vec!["/robots.txt", "/final"]
            }
        );
        // An authorized literal IP must not switch the shared DNS resolver to
        // allow-private mode for any of the authorized hostname origins.
        assert!(fetcher.fetch(&(scoped_hostname + "/page")).await.is_err());
        assert!(fetcher.fetch(&outside.url("/page")).await.is_err());
        assert!(!fetcher.robots_allowed(&outside.url("/robots.txt")).await);
        assert!(outside.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn multiple_origins_still_reject_page_and_robots_escape_redirects() {
    for robots_redirect in [false, true] {
        let outside = Site::new(|_| Reply::text(200, "unexpected")).await;
        let unused = Site::new(|_| Reply::text(200, "unused")).await;
        let destination = outside.url("/escaped");
        let source = Site::new(move |path| {
            if (path == "/robots.txt") == robots_redirect {
                let mut reply = Reply::text(302, "");
                reply.location = Some(destination.clone());
                reply
            } else {
                Reply::text(200, "User-agent: *\nAllow: /\n")
            }
        })
        .await;
        let fetcher = Fetcher::for_origins(vec![
            url::Url::parse(&source.origin).unwrap(),
            url::Url::parse(&unused.origin).unwrap(),
        ]);
        assert!(fetcher.fetch(&source.url("/page")).await.is_err());
        assert!(outside.requests.lock().unwrap().is_empty());
        assert!(unused.requests.lock().unwrap().is_empty());
        assert_eq!(
            *source.requests.lock().unwrap(),
            if robots_redirect {
                vec!["/robots.txt"]
            } else {
                vec!["/robots.txt", "/page"]
            }
        );
    }
}
