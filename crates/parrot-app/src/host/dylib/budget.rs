use std::{cell::Cell, rc::Rc};

/// One native invocation's messaging and discovery callback budget.
#[derive(Clone, Default)]
pub(super) struct Budget {
    calls: Rc<Cell<usize>>,
    bytes: Rc<Cell<usize>>,
    panicked: Rc<Cell<bool>>,
}
impl Budget {
    pub(super) fn attempt(&self) -> bool {
        let calls = self.calls.get();
        if calls >= 64 {
            return false;
        }
        self.calls.set(calls + 1);
        true
    }
    pub(super) fn charge(&self, bytes: usize) -> bool {
        let used = self.bytes.get();
        if bytes > 16 * 1024 * 1024 - used {
            return false;
        }
        self.bytes.set(used + bytes);
        true
    }
    pub(super) fn panic(&self, payload: Box<dyn std::any::Any + Send>) {
        self.panicked.set(true);
        crate::disposal::drop_catching(payload);
    }
    pub(super) fn complete(&self, result: &mut Result<(), (u32, String)>) {
        if self.panicked.get() {
            *result = Err((
                parrot_abi::ABI_ERR_PANIC,
                "native host callback panicked".into(),
            ));
        }
    }
    #[cfg(test)]
    pub(super) fn calls(&self) -> usize {
        self.calls.get()
    }
}
