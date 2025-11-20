use super::SystemError;

/// Per-wake processing admission limit, independent of the business batch size.
pub const MAX_MESSAGES_PER_BATCH: usize = parrot_config::ThreadResolved::MAX_QUEUE_CAPACITY;

pub(super) fn count(
    key: &str,
    value: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), SystemError> {
    if !(minimum..=maximum).contains(&value) {
        return Err(SystemError::ConfigError(format!(
            "{key}: must be in {minimum}..={maximum}"
        )));
    }
    Ok(())
}

pub(super) fn duration(key: &str, value: std::time::Duration) -> Result<(), SystemError> {
    parrot_config::validate_thread_duration(key, value)
        .map_err(|error| SystemError::ConfigError(error.to_string()))
}
