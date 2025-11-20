use parrot_api::trace::TraceContext;
use std::sync::Mutex;

#[derive(Default)]
struct State {
    claimed: bool,
    context: Option<TraceContext>,
}

/// An untraced request also claims ownership. Taking the hook's context releases
/// the recorder without allowing a later waiter to replace the original caller.
#[derive(Default)]
pub(crate) struct StopContext(Mutex<State>);
impl StopContext {
    pub(crate) fn claim_current(&self) -> Option<TraceContext> {
        let mut state = self.0.lock().unwrap();
        if !state.claimed {
            state.context = TraceContext::current();
            state.claimed = true;
        }
        state.context.clone()
    }
    pub(crate) fn get(&self) -> Option<TraceContext> {
        self.0.lock().unwrap().context.clone()
    }
    pub(crate) fn take(&self) -> Option<TraceContext> {
        self.take_or_else(|| None)
    }
    pub(crate) fn take_or_else(
        &self,
        fallback: impl FnOnce() -> Option<TraceContext>,
    ) -> Option<TraceContext> {
        let mut state = self.0.lock().unwrap();
        if !state.claimed {
            state.context = fallback();
            state.claimed = true;
        }
        state.context.take()
    }
}
