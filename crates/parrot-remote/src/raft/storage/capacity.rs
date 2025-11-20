use super::*;

pub(super) struct Capacity {
    // The sum at slot n covers exactly the first n encoded entries.
    prefix: Vec<usize>,
    binding: usize,
    reserved: usize,
}
impl Capacity {
    pub(super) fn new(saved: &Saved) -> Result<Self, RaftStorageError> {
        let longest = std::iter::once(&saved.id)
            .chain(&saved.peers)
            .max_by_key(|id| id.len());
        let binding = size(&(saved.version, &saved.cluster, &saved.id, &saved.peers))?
            + saved
                .snapshot
                .as_ref()
                .map_or(Ok(0), |snapshot| size(snapshot.as_ref()))?;
        let reserved = binding + size(&(u64::MAX, longest, &[] as &[LogEntry], u64::MAX))? + 40 - 1;
        let mut this = Self {
            prefix: vec![0],
            binding,
            reserved,
        };
        let sizes = this.project(0, &saved.log)?;
        this.replace(0, &sizes);
        Ok(this)
    }

    pub(super) fn project(
        &self,
        keep: usize,
        suffix: &[LogEntry],
    ) -> Result<Vec<usize>, RaftStorageError> {
        if keep >= self.prefix.len() || keep.saturating_add(suffix.len()) > MAX_ENTRIES {
            return Err(RaftStorageError::Capacity);
        }
        let mut total = self.prefix[keep];
        let mut sizes = Vec::with_capacity(suffix.len());
        for entry in suffix {
            if entry.cmd.len() > MAX_COMMAND {
                return Err(RaftStorageError::Capacity);
            }
            total = total
                .checked_add(size(entry)?)
                .ok_or(RaftStorageError::Capacity)?;
            if total > MAX_SNAPSHOT {
                return Err(RaftStorageError::Capacity);
            }
            sizes.push(total);
        }
        Ok(sizes)
    }

    pub(super) fn check(&self, keep: usize, suffix: &[LogEntry]) -> Result<(), RaftStorageError> {
        let sizes = self.project(keep, suffix)?;
        self.check_size(self.reserved, keep, &sizes)
    }

    pub(super) fn check_actual(
        &self,
        node: &RaftNode,
        keep: usize,
        sizes: &[usize],
    ) -> Result<(), RaftStorageError> {
        let overhead = self.binding
            + size(&(
                node.current_term,
                &node.voted_for,
                &[] as &[LogEntry],
                node.commit_index,
            ))?
            + 40
            - 1;
        self.check_size(overhead, keep, sizes)
    }

    pub(super) fn check_saved(
        &self,
        saved: &Saved,
        keep: usize,
        sizes: &[usize],
    ) -> Result<(), RaftStorageError> {
        let overhead =
            self.binding + size(&(saved.term, &saved.vote, &[] as &[LogEntry], saved.commit))? + 40
                - 1;
        self.check_size(overhead, keep, sizes)
    }

    fn check_size(
        &self,
        overhead: usize,
        keep: usize,
        sizes: &[usize],
    ) -> Result<(), RaftStorageError> {
        let entries = sizes.last().copied().unwrap_or(self.prefix[keep]);
        let length = size(&((keep + sizes.len()) as u64))?;
        if overhead.saturating_add(length).saturating_add(entries) > MAX_SNAPSHOT {
            return Err(RaftStorageError::Capacity);
        }
        Ok(())
    }

    pub(super) fn replace(&mut self, keep: usize, sizes: &[usize]) {
        self.prefix.truncate(keep + 1);
        self.prefix.extend_from_slice(sizes);
    }
}

fn size(value: &impl serde::Serialize) -> Result<usize, RaftStorageError> {
    let mut counter = Counter(0);
    bincode::serde::encode_into_std_write(value, &mut counter, bincode::config::standard())
        .map_err(|_| RaftStorageError::Capacity)?;
    Ok(counter.0)
}
