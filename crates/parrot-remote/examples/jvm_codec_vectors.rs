//! Reproduce the independent Rust oracle for JVM codec compatibility.
#[path = "../tests/support/jvm_codec_corpus.rs"]
mod corpus;
fn main() {
    print!("{}", corpus::generate());
}
