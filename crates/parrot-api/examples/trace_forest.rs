//! Record explicitly owned application operations and print a bounded snapshot.
//! Managed engine message handlers create their own child records automatically.
use parrot_api::trace::{SpanForest, SpanGuard, SpanKind, SpanStatus, TraceRecorder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let recorder = TraceRecorder::new(16)?;
    let root = recorder.root();
    root.in_scope(|| {
        let request = SpanGuard::current(SpanKind::Application, "/request");
        root.child().in_scope(|| {
            SpanGuard::current(SpanKind::Application, "/lookup").finish(SpanStatus::Ok);
        });
        root.child().in_scope(|| {
            SpanGuard::current(SpanKind::Application, "/optional-enrichment")
                .finish(SpanStatus::Error);
        });
        request.finish(SpanStatus::Ok);
    });
    recorder.close();
    let snapshot = recorder.drain();
    let view = SpanForest::new(&snapshot)?.render_text(4096)?;
    print!("{}", view.text);
    assert_eq!(view.emitted_records, 3);
    assert_eq!(view.omitted_records, 0);
    Ok(())
}
