use super::render_results;

fn hits(count: usize) -> Vec<(f64, String, String, String)> {
    (0..count)
        .map(|index| {
            (
                1.0,
                format!("https://pagination.test/doc/{index}"),
                format!("document {index}"),
                "matched content".into(),
            )
        })
        .collect()
}

fn has_document(html: &str, index: usize) -> bool {
    html.contains(&format!("href=\"https://pagination.test/doc/{index}\""))
}

#[test]
fn actual_hit_count_controls_navigation_and_page_slice() {
    let html = render_results("alpha & beta", &hits(23), 2, 10);
    assert_eq!(html.matches("class=\"item\"").count(), 10);
    for index in 0..23 {
        assert_eq!(has_document(&html, index), (10..20).contains(&index));
    }
    assert!(html.contains(r#"<div class="meta">找到 23 条结果</div>"#));
    assert!(html.contains("/search?q=alpha%20%26%20beta&p=1\""));
    assert!(html.contains("/search?q=alpha%20%26%20beta&p=3\""));
    assert!(!html.contains("/search?q=alpha%20%26%20beta&p=4\""));
    assert!(html.contains("aria-current=\"page\">2</span>"));
}

#[test]
fn empty_and_single_results_never_offer_nonexistent_search_pages() {
    for count in [0, 1] {
        let html = render_results("single", &hits(count), usize::MAX, 10);
        assert_eq!(html.matches("class=\"item\"").count(), count);
        assert!(!html.contains("href=\"/search?"));
        assert!(!html.contains("aria-current=\"page\""));
    }
}

#[test]
fn out_of_range_requests_display_the_real_first_or_last_page() {
    let entries = hits(23);
    let first = render_results("query", &entries, 0, 10);
    let last = render_results("query", &entries, usize::MAX, 10);
    assert_eq!(first.matches("class=\"item\"").count(), 10);
    assert_eq!(last.matches("class=\"item\"").count(), 3);
    for index in 0..23 {
        assert_eq!(has_document(&first, index), index < 10);
        assert_eq!(has_document(&last, index), index >= 20);
    }
    assert!(first.contains("aria-current=\"page\">1</span>"));
    assert!(last.contains("aria-current=\"page\">3</span>"));
}

#[test]
fn bounded_window_never_renders_hits_or_pages_beyond_one_hundred() {
    let html = render_results("bounded", &hits(111), 10, 10);
    assert_eq!(html.matches("class=\"item\"").count(), 10);
    for index in 0..111 {
        assert_eq!(has_document(&html, index), (90..100).contains(&index));
    }
    assert!(!html.contains("p=11\""));
    assert!(html.contains(r#"<div class="meta">前 100 条匹配结果</div>"#));
}
