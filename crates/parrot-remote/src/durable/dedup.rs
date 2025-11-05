//! Bounded, in-memory deduplication for one logical receiver.
//!
//! Sender identifiers must include the sending session/epoch if sequences reset.
//! This table neither authenticates identifiers nor persists business effects.

use std::collections::{HashMap, HashSet};

/// Logical resource bounds. Allocator and hash-table overhead are not included
/// in sender bytes; sender and sequence counts also bound those structures.
#[derive(Debug, Clone, Copy)]
pub struct DedupLimits {
    pub max_senders: usize,
    pub max_pending_sequences: usize,
    pub max_sender_bytes: usize,
    pub max_sender_length: usize,
}

impl Default for DedupLimits {
    fn default() -> Self {
        Self {
            max_senders: 4096,
            max_pending_sequences: 65_536,
            max_sender_bytes: 1024 * 1024,
            max_sender_length: 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DedupError {
    #[error("invalid dedup limits")]
    InvalidLimits,
    #[error("dedup sender must not be empty")]
    EmptySender,
    #[error("dedup sequence must be positive")]
    ZeroSequence,
    #[error("dedup sender identity exceeds its byte limit")]
    SenderTooLong,
    #[error("dedup sender count budget exhausted")]
    SenderLimit,
    #[error("dedup sender byte budget exhausted")]
    SenderBytesLimit,
    #[error("dedup sequence gap budget exhausted")]
    SequenceLimit,
    #[error("dedup allocation failed")]
    Allocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DedupStats {
    pub senders: usize,
    /// Completed out-of-order sequences retained above the contiguous watermark.
    pub pending_sequences: usize,
    pub sender_bytes: usize,
}

#[derive(Debug, Default)]
struct SenderState {
    watermark: u64,
    gaps: HashSet<u64>,
}
impl SenderState {
    fn compact(&mut self) {
        // Without releasing historical capacity, filling one sender's gap after
        // another could retain max_pending_sequences slots per sender forever.
        if self.gaps.is_empty() {
            self.gaps = HashSet::new();
        } else if self.gaps.len() < self.gaps.capacity() / 4 {
            self.gaps.shrink_to_fit();
        }
    }
}

/// One receiver's bounded sender/session history. Never evicts completed history
/// to admit a new sender: that would silently allow a previously processed replay.
#[derive(Debug)]
pub struct DedupTable {
    limits: DedupLimits,
    senders: HashMap<String, SenderState>,
    pending_sequences: usize,
    sender_bytes: usize,
}

impl Default for DedupTable {
    fn default() -> Self {
        Self::with_limits(DedupLimits::default()).expect("valid default dedup limits")
    }
}

/// Admission distinguishes completed duplicates from work that needs processing.
#[derive(Debug)]
#[must_use = "process and commit the permit, or let it drop to allow retry"]
pub enum DedupDecision<'a> {
    Duplicate { watermark: u64 },
    Process(DedupPermit<'a>),
}

/// Exclusive reservation held across business processing. Drop (including error,
/// cancellation or unwinding) abandons the reservation. Only commit marks success.
/// The mutable borrow serializes access; wrap the table in a mutex if it is shared.
#[derive(Debug)]
#[must_use = "commit only after business success; dropping permits retry"]
pub struct DedupPermit<'a> {
    table: &'a mut DedupTable,
    sender: &'a str,
    seq: u64,
    fresh: Option<(String, SenderState)>,
    reserved_gap: bool,
    committed: bool,
}

impl DedupTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: DedupLimits) -> Result<Self, DedupError> {
        if [
            limits.max_senders,
            limits.max_pending_sequences,
            limits.max_sender_bytes,
            limits.max_sender_length,
        ]
        .iter()
        .any(|&n| n == 0 || n >= isize::MAX as usize)
            || limits.max_sender_length > limits.max_sender_bytes
        {
            return Err(DedupError::InvalidLimits);
        }
        Ok(Self {
            limits,
            senders: HashMap::new(),
            pending_sequences: 0,
            sender_bytes: 0,
        })
    }

    pub fn stats(&self) -> DedupStats {
        DedupStats {
            senders: self.senders.len(),
            pending_sequences: self.pending_sequences,
            sender_bytes: self.sender_bytes,
        }
    }

    /// Highest contiguous completed sequence, not the largest observed sequence.
    /// Each sender/session starts at one. Unknown senders return None.
    pub fn watermark(&self, sender: &str) -> Option<u64> {
        self.senders.get(sender).map(|s| s.watermark)
    }

    /// Reserve capacity before running business code. Budget/identity errors do
    /// not change logical history. Duplicates remain recognizable at capacity;
    /// filling the next contiguous gap requires no additional sequence slot.
    pub fn begin<'a>(
        &'a mut self,
        sender: &'a str,
        seq: u64,
    ) -> Result<DedupDecision<'a>, DedupError> {
        if sender.is_empty() {
            return Err(DedupError::EmptySender);
        }
        if seq == 0 {
            return Err(DedupError::ZeroSequence);
        }
        if sender.len() > self.limits.max_sender_length {
            return Err(DedupError::SenderTooLong);
        }
        let existing = self.senders.get(sender);
        let wm = existing.map_or(0, |s| s.watermark);
        if seq <= wm || existing.is_some_and(|s| s.gaps.contains(&seq)) {
            return Ok(DedupDecision::Duplicate { watermark: wm });
        }
        let new_sender = existing.is_none();
        let reserved_gap = wm.checked_add(1) != Some(seq);
        if new_sender {
            if self.senders.len() == self.limits.max_senders {
                return Err(DedupError::SenderLimit);
            }
            if self
                .sender_bytes
                .checked_add(sender.len())
                .is_none_or(|n| n > self.limits.max_sender_bytes)
            {
                return Err(DedupError::SenderBytesLimit);
            }
        }
        if reserved_gap && self.pending_sequences == self.limits.max_pending_sequences {
            return Err(DedupError::SequenceLimit);
        }
        let mut fresh = None;
        if new_sender {
            let mut key = String::new();
            key.try_reserve_exact(sender.len())
                .map_err(|_| DedupError::Allocation)?;
            key.push_str(sender);
            let mut state = SenderState::default();
            if reserved_gap {
                state
                    .gaps
                    .try_reserve(1)
                    .map_err(|_| DedupError::Allocation)?;
            }
            self.senders
                .try_reserve(1)
                .map_err(|_| DedupError::Allocation)?;
            fresh = Some((key, state));
        } else if reserved_gap {
            let state = self.senders.get_mut(sender).expect("existing sender");
            state
                .gaps
                .try_reserve(1)
                .map_err(|_| DedupError::Allocation)?;
        }
        Ok(DedupDecision::Process(DedupPermit {
            table: self,
            sender,
            seq,
            fresh,
            reserved_gap,
            committed: false,
        }))
    }

    /// Immediate mark-on-admission helper. Returns false only for a completed
    /// duplicate; resource rejection is an error, never a successful duplicate.
    /// For fallible business processing use begin and commit after success.
    /// This signature replaces the previous infallible bool API.
    pub fn filter(&mut self, sender: &str, seq: u64) -> Result<bool, DedupError> {
        match self.begin(sender, seq)? {
            DedupDecision::Duplicate { .. } => Ok(false),
            DedupDecision::Process(permit) => {
                permit.commit();
                Ok(true)
            }
        }
    }
}

impl DedupPermit<'_> {
    /// Mark business success and return the contiguous cumulative watermark.
    /// Capacity was reserved before processing; commit cannot reject for a budget.
    /// This memory update is not atomic with external business effects or a disk
    /// transaction. A crash after the effect but before commit can repeat the effect.
    pub fn commit(mut self) -> u64 {
        if let Some((key, state)) = self.fresh.take() {
            self.table.sender_bytes += key.len();
            self.table.senders.insert(key, state);
        }
        let state = self
            .table
            .senders
            .get_mut(self.sender)
            .expect("reserved sender");
        if self.reserved_gap {
            state.gaps.insert(self.seq);
            self.table.pending_sequences += 1;
        } else {
            state.watermark = self.seq;
            while let Some(next) = state.watermark.checked_add(1) {
                if !state.gaps.remove(&next) {
                    break;
                }
                self.table.pending_sequences -= 1;
                state.watermark = next;
            }
            state.compact();
        }
        self.committed = true;
        state.watermark
    }
}

impl Drop for DedupPermit<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if self.fresh.is_none() && self.reserved_gap {
            let state = self
                .table
                .senders
                .get_mut(self.sender)
                .expect("reserved sender");
            state.compact();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_gaps_release_historical_allocation_for_each_sender() {
        let mut table = DedupTable::new();
        for n in 0..32 {
            let sender = format!("s{n}");
            for seq in 2..=1024 {
                assert!(table.filter(&sender, seq).unwrap());
            }
            assert!(table.filter(&sender, 1).unwrap());
            assert_eq!(table.senders[&sender].gaps.capacity(), 0);
        }
        assert_eq!(table.stats().pending_sequences, 0);
    }
}
