use super::extract_html;

#[test]
fn title_and_body_are_separate_search_text_regions() {
    let page = extract_html("<html><head><title>/final</title></head><body>needle</body></html>");
    assert_eq!(page.title, "/final");
    assert_eq!(page.text, "/final needle");
}
#[test]
fn block_boundaries_include_closing_tags_and_numbered_headings() {
    assert_eq!(
        extract_html("before<h2>heading</h2>after<p>paragraph</p>end").text,
        "before heading after paragraph end"
    );
    assert_eq!(
        extract_html("a<table><tr><td>b</td><td>c</td></tr></table>d").text,
        "a b c d"
    );
}
#[test]
fn inline_markup_preserves_word_boundaries_defined_by_the_document() {
    assert_eq!(
        extract_html("<body>hel<em>lo</em> <b>wo</b>rld</body>").text,
        "hello world"
    );
}
#[test]
fn ignored_raw_text_cannot_inject_words_through_other_closing_tags() {
    assert_eq!(
        extract_html(
            "left<script>evil</scripture><div>hidden</div></script>right<style>x</style>tail"
        )
        .text,
        "left right tail"
    );
}
