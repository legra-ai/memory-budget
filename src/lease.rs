//! RAII lease returned by [`MemoryBudget::lease`](crate::MemoryBudget::lease).

use std::sync::Arc;

use crate::budget::BudgetState;

/// Active short-term reservation against the budget.
///
/// While alive, the effective per-cache target is reduced by
/// `bytes()`. The reduction is released when the guard is dropped.
///
/// Issued by [`MemoryBudget::lease`](crate::MemoryBudget::lease).
/// The guard owns the shared accounting state until release, even when the
/// public budget handle has been dropped. It never releases a reservation
/// merely because work moved to another task or queue.
#[must_use = "dropping the LeaseGuard immediately releases the lease"]
pub struct LeaseGuard {
    bytes: u64,
    state: Arc<BudgetState>,
}

impl LeaseGuard {
    pub(crate) fn new(bytes: u64, state: Arc<BudgetState>) -> Self {
        Self { bytes, state }
    }

    /// Bytes reserved by this guard.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    #[cfg(test)]
    pub(crate) fn accounting_is_live(&self) -> bool {
        Arc::strong_count(&self.state) > 0
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.state.release_lease(self.bytes);
    }
}
