//! Internal unwind boundary for a single owned user value, never a container.
pub(crate) fn drop_catching<T>(value: T) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value))) {
        Ok(()) => true,
        Err(payload) => {
            // A panic_any payload can itself panic during destruction. Bound
            // recursion by leaking only the second throwing payload. This does
            // not recover panic=abort or a double panic inside one value.
            if let Err(nested) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                std::mem::forget(nested);
            }
            false
        }
    }
}
