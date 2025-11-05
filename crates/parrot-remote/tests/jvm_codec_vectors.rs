#[path = "support/jvm_codec_corpus.rs"]
mod corpus;
#[test]
fn jvm_shared_vectors_match_production_rust_codecs() {
    let expected = include_str!("../../../docs/vectors/jvm-codecs.tsv");
    assert_eq!(
        corpus::generate(),
        expected,
        "regenerate intentionally with the jvm_codec_vectors example and review protocol changes"
    );
}
