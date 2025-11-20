use parrot_api::trace::TraceContext;

pub fn assert_child(child: Option<&TraceContext>, parent: Option<&TraceContext>) {
    match (child, parent) {
        (Some(child), Some(parent)) => {
            assert_eq!(child.trace_id(), parent.trace_id());
            assert_eq!(child.parent_span_id(), Some(parent.span_id()));
            assert_ne!(child.span_id(), parent.span_id());
        }
        (None, None) => {}
        mismatch => panic!("lifecycle hook lost its causal parent: {mismatch:?}"),
    }
}
