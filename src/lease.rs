//! RAII lease returned by [`MemoryBudget::lease`](crate::MemoryBudget::lease).

use std::sync::Weak;

use crate::budget::BudgetState;

/// Active short-term reservation against the budget.
///
/// While alive, the effective per-cache target is reduced by
/// `bytes()`. The reduction is released when the guard is dropped.
///
/// Issued by [`MemoryBudget::lease`](crate::MemoryBudget::lease).
/// The guard holds a `Weak` reference to the budget so that a guard
/// outliving its budget does not leak the budget's allocation.
#[must_use = "dropping the LeaseGuard immediately releases the lease"]
pub struct LeaseGuard {
    bytes: u64,
    state: Weak<BudgetState>,
}

impl LeaseGuard {
    pub(crate) fn new(bytes: u64, state: Weak<BudgetState>) -> Self {
        Self { bytes, state }
    }

    /// Bytes reserved by this guard.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.release_lease(self.bytes);
        }
    }
}
