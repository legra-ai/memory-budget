//! Read-only accessors and the lease API on [`MemoryBudget`].

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::handle::MemoryBudget;
use crate::lease::LeaseGuard;

const EXECUTOR_SPILL_TARGET_DIVISOR: u64 = 16;
const MIN_EXECUTOR_SPILL_RUN_BUFFER_BYTES: u64 = 1024 * 1024;
const MAX_EXECUTOR_SPILL_RUN_BUFFER_BYTES: u64 = 32 * 1024 * 1024;

const RESULT_CHANNEL_TARGET_DIVISOR: u64 = 32;
const MIN_RESULT_CHANNEL_BYTES: u64 = 256 * 1024;
const MAX_RESULT_CHANNEL_BYTES: u64 = 8 * 1024 * 1024;

impl MemoryBudget {
    /// Borrow `bytes` of slack from the budget. While the returned
    /// guard is alive, the effective target is reduced by `bytes`.
    /// Releases on drop.
    #[must_use = "dropping the guard immediately releases the lease"]
    pub fn lease(&self, bytes: u64) -> LeaseGuard {
        self.state
            .active_lease_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        LeaseGuard::new(bytes, Arc::downgrade(&self.state))
    }

    /// Soft RSS target in bytes, before any lease deductions.
    #[must_use]
    pub fn target_bytes(&self) -> u64 {
        self.state.config.target_bytes()
    }

    /// Effective target after subtracting active leases. Saturates
    /// at zero if leases ever exceed the target (a misuse, but the
    /// budget refuses to underflow).
    #[must_use]
    pub fn effective_target_bytes(&self) -> u64 {
        self.state
            .config
            .target_bytes()
            .saturating_sub(self.state.active_lease_bytes.load(Ordering::Relaxed))
    }

    /// Per-operator external-sort run buffer for executor spill paths.
    ///
    /// Derived from the effective process target, so active leases shrink
    /// executor spill buffers before an operator starts rather than letting
    /// every operator size itself against the whole process budget.
    ///
    /// # Panics
    ///
    /// Panics if the bounded spill-buffer size cannot fit in `usize`.
    #[must_use]
    pub fn executor_spill_run_buffer_bytes(&self) -> usize {
        let bytes = (self.effective_target_bytes() / EXECUTOR_SPILL_TARGET_DIVISOR).clamp(
            MIN_EXECUTOR_SPILL_RUN_BUFFER_BYTES,
            MAX_EXECUTOR_SPILL_RUN_BUFFER_BYTES,
        );
        usize::try_from(bytes).expect("executor spill run buffer fits usize")
    }

    /// Byte budget for one result stream's producer→consumer channel
    /// (BME-22): the ceiling on encoded result bytes in flight between
    /// the encoder and its consumer/spool tee. A **byte** bound, not a
    /// frame count — frames vary in size by orders of magnitude, so
    /// counting them would bound the wrong quantity (BME-12's lesson).
    ///
    /// Derived from the effective process target so active leases shrink
    /// in-flight result buffering the same way they shrink spill
    /// buffers.
    ///
    /// # Panics
    ///
    /// Panics if the bounded channel size cannot fit in `usize`.
    #[must_use]
    pub fn result_channel_bytes(&self) -> usize {
        let bytes = (self.effective_target_bytes() / RESULT_CHANNEL_TARGET_DIVISOR)
            .clamp(MIN_RESULT_CHANNEL_BYTES, MAX_RESULT_CHANNEL_BYTES);
        usize::try_from(bytes).expect("result channel size fits usize")
    }

    /// Tick cadence used by the spawned background loop.
    #[must_use]
    pub fn tick_interval(&self) -> Duration {
        self.state.config.tick()
    }

    /// Optional hard ceiling. When set, the budget aborts the
    /// process if RSS exceeds this value — a backstop for runaway
    /// growth in production.
    #[must_use]
    pub fn hard_ceiling_bytes(&self) -> Option<u64> {
        self.state.config.hard_ceiling_bytes()
    }
}
