//! Internal mutable state shared between the budget handle, lease
//! guards, and the spawned ticker task.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::config::BudgetConfig;
use crate::jemalloc::JemallocStatsSource;
use crate::policy::{CacheView, Policy};
use crate::reporter::NonCacheReporter;
use crate::resizable::Resizable;
use crate::rss::RssSource;

/// Internal mutable state shared between the budget handle, lease
/// guards, and the spawned ticker task.
pub(crate) struct BudgetState {
    pub(super) config: BudgetConfig,
    /// Preferred ceiling per registered cache, captured at
    /// registration time. The policy uses these as upper bounds when
    /// growing caches back after a shrink.
    pub(super) registrations: Mutex<Vec<Registration>>,
    /// Active lease bytes (sum). Updated by lease take/drop and by
    /// the policy when computing effective target.
    pub(super) active_lease_bytes: AtomicU64,
    /// Sum of `current_bytes` across registered caches as observed
    /// at the previous tick. Used by the policy to detect whether
    /// the recent RSS climb came from the cache subsystem or from
    /// elsewhere. `0` until the first tick completes.
    pub(super) previous_cache_bytes: AtomicU64,
    /// Number of registered caches at the previous tick. `0` until
    /// the first tick completes.
    pub(super) previous_cache_count: AtomicU64,
    /// RSS reading at the previous tick. `0` until the first tick
    /// completes. Used by the trend guard to require a real RSS
    /// climb before classifying an over-target situation as a
    /// non-cache spike.
    pub(super) previous_rss_bytes: AtomicU64,
    /// Run-length state for the verdict streak counter. Holds the
    /// verdict label of the previous tick and the streak length so
    /// far. `&'static str` so labels can be compared and stored
    /// cheaply.
    pub(super) verdict_streak: Mutex<VerdictStreak>,
    pub(super) policy: Box<dyn Policy>,
    pub(super) rss: Box<dyn RssSource>,
    /// Source for jemalloc introspection — buckets `non_cache_bytes`
    /// into "live allocations the program holds" vs
    /// "retained-by-jemalloc pages". Defaults to
    /// [`crate::jemalloc::NoJemallocStats`] (all zeros). Binaries
    /// that install jemalloc as the global allocator wire a real
    /// source via [`super::MemoryBudget::with_jemalloc_stats`].
    pub(super) jemalloc: Box<dyn JemallocStatsSource>,
    /// Read-only non-cache memory reporters (e.g. a parser or reasoner
    /// working-set gauge). Held weakly so a dropped reporter
    /// prunes from the next tick. The budget cannot resize these — it
    /// only attributes slices of `non_cache_live` to named subsystems.
    pub(super) reporters: Mutex<Vec<Weak<dyn NonCacheReporter>>>,
    /// Threshold-driven jemalloc heap-profile dumper. `None` unless the
    /// `heap-profiling` feature is built and `MEMORY_BUDGET_HEAP_PROFILE_DIR` is
    /// set; when present the tick asks it to dump a profile each time
    /// resident RSS climbs past the next threshold, so a `non_cache_other`
    /// balloon is captured as backtrace-bearing `*.heap` files.
    #[cfg(feature = "heap-profiling")]
    pub(super) heap_profiler: Option<crate::heap_profile::HeapProfiler>,
}

impl BudgetState {
    pub(crate) fn release_lease(&self, bytes: u64) {
        // Subtract while clamping at zero. A misuse where the caller
        // requested more than the current outstanding total must not
        // underflow; we log a warning and saturate instead.
        let mut current = self.active_lease_bytes.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(bytes);
            match self.active_lease_bytes.compare_exchange(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }
}

/// Run-length state for [`BudgetState::verdict_streak`].
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VerdictStreak {
    pub(super) label: &'static str,
    pub(super) length: u32,
}

pub(super) struct Registration {
    pub(super) cache: Weak<dyn Resizable>,
    pub(super) preferred_max_bytes: u64,
}

pub(super) struct LiveViews {
    pub(super) views: Vec<CacheView>,
    pub(super) live_caches: Vec<Arc<dyn Resizable>>,
}
