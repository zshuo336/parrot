use super::*;
use crate::owned_payload::diagnostic_contracts::contract as diagnostic_contract;

struct DiagnosticPayload(Arc<AtomicUsize>);
impl Drop for DiagnosticPayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Self(self.0.clone()));
    }
}
struct Subscriber {
    events: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl tracing::Subscriber for Subscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::ERROR && metadata.target() == "parrot::children"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        self.events.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(DiagnosticPayload(self.payloads.clone()));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
fn install() -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
    std::panic::set_hook(Box::new(|_| {}));
    let events = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(Subscriber {
        events: events.clone(),
        payloads: payloads.clone(),
    })
    .unwrap();
    (events, payloads)
}
fn check(mode: u8, kind: u8, expected: usize) {
    let (events, payloads) = install();
    exercise(mode, kind);
    assert_eq!(events.load(Ordering::SeqCst), expected);
    assert_eq!(payloads.load(Ordering::SeqCst), expected);
}
fn scope_unwind() {
    let (events, payloads) = install();
    let (scope, counts) = fixture();
    let slots = scope.slots.clone();
    let token = Arc::new(());
    let first = admit(&scope, &counts, &token, 2, 2);
    let second = admit(&scope, &counts, &token, 2, 2);
    let references = counts.references.load(Ordering::SeqCst);
    counts.active.store(true, Ordering::SeqCst);
    let escaped = caught(move || {
        let _scope = scope;
        panic!("original child scope unwind");
    });
    assert_eq!(escaped.as_deref(), Some("original child scope unwind"));
    assert_eq!(counts.references.load(Ordering::SeqCst) - references, 2);
    assert_eq!(counts.stops.load(Ordering::SeqCst), 2);
    assert_eq!(counts.payloads.load(Ordering::SeqCst), 4);
    assert_eq!(events.load(Ordering::SeqCst), 4);
    assert_eq!(payloads.load(Ordering::SeqCst), 4);
    assert_eq!(slots.available_permits(), MAX_CHILDREN);
    assert!(first.parent.upgrade().is_none());
    assert!(second.parent.upgrade().is_none());
}

diagnostic_contract!(replacement_diagnostic_cannot_escape, check(0, 2, 1));
diagnostic_contract!(rejected_member_diagnostic_cannot_escape, check(1, 2, 1));
diagnostic_contract!(rejected_parent_diagnostic_cannot_escape, check(2, 2, 1));
diagnostic_contract!(scope_drop_diagnostics_isolate_each_input, check(3, 2, 4));
diagnostic_contract!(partial_view_unwind_preserves_original_panic, check(5, 2, 3));
diagnostic_contract!(scope_unwind_preserves_original_panic, scope_unwind());
diagnostic_contract!(healthy_scope_does_not_emit_cleanup_errors, check(3, 0, 0));
