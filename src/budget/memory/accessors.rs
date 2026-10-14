//! Read-only accessors and the lease API on [`MemoryBudget`].

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::handle::MemoryBudget;
use crate::ReservationError;
use crate::lease::LeaseGuard;

const EXECUTOR_SPILL_TARGET_DIVISOR: u64 = 16;
const MIN_EXECUTOR_SPILL_RUN_BUFFER_BYTES: u64 = 1024 * 1024;
const MAX_EXECUTOR_SPILL_RUN_BUFFER_BYTES: u64 = 32 * 1024 * 1024;

const RESULT_CHANNEL_TARGET_DIVISOR: u64 = 32;
const MIN_RESULT_CHANNEL_BYTES: u64 = 256 * 1024;
const MAX_RESULT_CHANNEL_BYTES: u64 = 8 * 1024 * 1024;

impl MemoryBudget {
    /// Observe current process/allocator usage without running a cache tick.
    ///
    /// This is a measurement, not an allocation reservation. The maximum of
    /// process usage, allocator residency and live heap bytes protects against
    /// both compressed pages and allocations outside the configured allocator.
    #[must_use]
    pub fn observed_usage_bytes(&self) -> u64 {
        self.state
            .jemalloc
            .read()
            .observed_usage_bytes(self.state.rss.read_rss_bytes())
    }

    /// Reserve `bytes`, panicking if outstanding leases would exceed the target.
    ///
    /// Use [`Self::try_lease`] when exhaustion is an expected refusal. The
    /// reservation must precede allocation and follow its ownership to release.
    /// This bounds outstanding reservations, not unregistered allocations.
    ///
    /// # Panics
    ///
    /// Panics when the aggregate reservation would exceed the target.
    #[must_use = "dropping the guard immediately releases the lease"]
    pub fn lease(&self, bytes: u64) -> LeaseGuard {
        self.try_lease(bytes)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    /// Reserve bytes atomically without exceeding the aggregate lease target.
    ///
    /// Registered cache ceilings are reduced by active leases on the next
    /// coordination tick. Existing allocations are not synchronously evicted;
    /// callers must account for cache/baseline ownership separately before
    /// claiming an enforced whole-process allocation bound.
    ///
    /// # Errors
    ///
    /// Returns [`ReservationError`] without changing accounting when the
    /// request cannot fit. No payload is allocated and no waiter is queued.
    pub fn try_lease(&self, bytes: u64) -> Result<LeaseGuard, ReservationError> {
        let capacity = self.target_bytes();
        let mut reserved = self.state.active_lease_bytes.load(Ordering::Relaxed);
        loop {
            let Some(next) = reserved.checked_add(bytes).filter(|next| *next <= capacity) else {
                return Err(ReservationError::new(bytes, reserved, capacity));
            };
            match self.state.active_lease_bytes.compare_exchange_weak(
                reserved,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(LeaseGuard::new(bytes, Arc::clone(&self.state))),
                Err(observed) => reserved = observed,
            }
        }
    }

    /// Soft RSS target in bytes, before any lease deductions.
    #[must_use]
    pub fn target_bytes(&self) -> u64 {
        self.state.config.target_bytes()
    }

    /// Effective target after subtracting admitted active leases.
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
