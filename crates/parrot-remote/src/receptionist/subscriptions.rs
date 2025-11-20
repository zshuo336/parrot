use super::{Notifications, Receptionist, ReceptionistEvent, ReceptionistKey, ReceptionistStream};
use parrot_api::{errors::ActorError, types::ActorResult};
use parrot_config::receptionist::SubscriptionLimits;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

/// Maximum event envelope: enum storage plus UTF-8 key and path bytes.
pub const MAX_EVENT_BYTES: usize = 16 * 1024;
/// Declared queue reservation per slot, including an allocation allowance.
/// This is admission accounting, not a bound on allocator RSS or the service table.
pub const RESERVED_SLOT_BYTES: usize = MAX_EVENT_BYTES + 256;

pub fn validate_event(key: &ReceptionistKey, path: &str) -> ActorResult<()> {
    if std::mem::size_of::<ReceptionistEvent>()
        .checked_add(key.as_str().len())
        .and_then(|size| size.checked_add(path.len()))
        .is_none_or(|size| size > MAX_EVENT_BYTES)
    {
        return Err(ActorError::InternalError(
            "receptionist event exceeds 16 KiB envelope".into(),
        ));
    }
    Ok(())
}

#[derive(Default)]
pub(super) struct Usage {
    count: AtomicUsize,
    bytes: AtomicUsize,
}

struct Lease {
    usage: Arc<Usage>,
    bytes: usize,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.usage.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
        self.usage.count.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Receptionist {
    /// Fallible bounded alternative to the unchecked low-level `subscribe`.
    /// Reserves the entire snapshot plus realtime queue until receiver drop.
    pub fn try_subscribe(
        &mut self,
        key: &ReceptionistKey,
        buf: usize,
    ) -> ActorResult<ReceptionistStream> {
        let (stream, notifications) = self.try_subscribe_deferred(key, buf)?;
        notifications.dispatch()?;
        Ok(stream)
    }

    pub(crate) fn try_subscribe_deferred(
        &mut self,
        key: &ReceptionistKey,
        buf: usize,
    ) -> ActorResult<(ReceptionistStream, Notifications)> {
        validate_event(key, "")?;
        let snapshot = self.entries.get(key.as_str());
        if let Some(entries) = snapshot {
            for entry in entries {
                validate_event(&entry.key, &entry.remote_path)?;
            }
        }
        let denied =
            || ActorError::InternalError("receptionist subscription budget exhausted".into());
        let capacity = snapshot
            .map_or(0, Vec::len)
            .checked_add(buf.max(1))
            .ok_or_else(denied)?;
        if capacity > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(denied());
        }
        let bytes = capacity
            .checked_mul(RESERVED_SLOT_BYTES)
            .ok_or_else(denied)?;
        let count = self
            .subscription_usage
            .count
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or_else(denied)?;
        let reserved = self
            .subscription_usage
            .bytes
            .load(Ordering::Acquire)
            .checked_add(bytes)
            .ok_or_else(denied)?;
        if count > self.subscription_limits.max_subscriptions
            || reserved > self.subscription_limits.max_reserved_bytes
        {
            return Err(denied());
        }
        // Admission is serialized by &mut/table mutex; concurrent receiver drops
        // only decrease counters. Use additions so those drops cannot be lost.
        self.subscription_usage.count.fetch_add(1, Ordering::AcqRel);
        self.subscription_usage
            .bytes
            .fetch_add(bytes, Ordering::AcqRel);
        let lease = Lease {
            usage: self.subscription_usage.clone(),
            bytes,
        };
        let (stream, notifications) = self.subscribe_deferred(key, buf);
        Ok((stream.with_lifetime_guard(lease), notifications))
    }

    /// Configure before exposing the table or admitting any subscriptions.
    pub fn with_subscription_limits(limits: SubscriptionLimits) -> Self {
        Self::with_limits(limits, Default::default())
    }

    pub fn with_limits(
        limits: SubscriptionLimits,
        registrations: parrot_config::receptionist::RegistrationLimits,
    ) -> Self {
        Self {
            registrations: super::registrations::Budget::new(registrations),
            subscription_limits: limits,
            subscription_usage: Default::default(),
            entries: Default::default(),
            subscribers: Default::default(),
            owners: Default::default(),
        }
    }

    pub(crate) fn retire_contents(&mut self) -> Self {
        let mut empty = Self::with_limits(self.subscription_limits, self.registrations.limits);
        empty.subscription_usage = self.subscription_usage.clone();
        std::mem::replace(self, empty)
    }
}

#[cfg(test)]
mod tests;
