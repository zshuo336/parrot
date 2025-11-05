//! Per-store guest resource accounting. A component may contain several core
//! memories; each consumes the same linear-memory budget. Wasmtime/host metadata
//! and compiled code are outside this budget.

use wasmtime::ResourceLimiter;

pub(super) const MAX_CORE_INSTANCES: usize = 64;
pub(super) const MAX_MEMORIES: usize = 64;
pub(super) const MAX_TABLES: usize = 64;
pub(super) const MAX_TABLE_ELEMENTS: usize = 65_536;

#[derive(Debug)]
pub(super) struct GuestLimits {
    memory_limit: usize,
    memory_bytes: usize,
    table_elements: usize,
    pending_memory: usize,
    pending_table: usize,
}

impl GuestLimits {
    pub(super) fn new(memory_limit: usize) -> Self {
        Self {
            memory_limit,
            memory_bytes: 0,
            table_elements: 0,
            pending_memory: 0,
            pending_table: 0,
        }
    }
}

impl ResourceLimiter for GuestLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.pending_memory = 0;
        let Some(delta) = desired.checked_sub(current) else {
            return Ok(false);
        };
        let Some(total) = self.memory_bytes.checked_add(delta) else {
            return Ok(false);
        };
        if total > self.memory_limit || maximum.is_some_and(|m| desired > m) {
            return Ok(false);
        }
        // Store operations are serial. On success this reservation becomes the
        // live allocation; if Wasmtime cannot allocate, its failure callback
        // rolls back this exact reservation before another growth is attempted.
        self.memory_bytes = total;
        self.pending_memory = delta;
        Ok(true)
    }

    fn memory_grow_failed(&mut self, _error: wasmtime::Error) -> wasmtime::Result<()> {
        self.memory_bytes -= std::mem::take(&mut self.pending_memory);
        Ok(())
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.pending_table = 0;
        let Some(delta) = desired.checked_sub(current) else {
            return Ok(false);
        };
        let Some(total) = self.table_elements.checked_add(delta) else {
            return Ok(false);
        };
        if total > MAX_TABLE_ELEMENTS || maximum.is_some_and(|m| desired > m) {
            return Ok(false);
        }
        self.table_elements = total;
        self.pending_table = delta;
        Ok(true)
    }

    fn table_grow_failed(&mut self, _error: wasmtime::Error) -> wasmtime::Result<()> {
        self.table_elements -= std::mem::take(&mut self.pending_table);
        Ok(())
    }

    fn instances(&self) -> usize {
        MAX_CORE_INSTANCES
    }
    fn memories(&self) -> usize {
        MAX_MEMORIES
    }
    fn tables(&self) -> usize {
        MAX_TABLES
    }
}
