//! jemalloc stats split for the [`crate::BudgetSnapshot`] non-cache bucket.
//!
//! `rss − cache_bytes − lease_bytes` lumps every byte the budget cannot
//! resize into a single number. When that number grows monotonically
//! across a long-running workload it is impossible to tell whether the
//! application is retaining live objects or jemalloc is sitting on
//! retained-but-decommitted pages
//! (work for allocator tuning). This module surfaces jemalloc's own
//! introspection so the tick log can name the bucket directly.
//!
//! [`SystemJemallocStats`] is feature-gated behind `jemalloc` so the
//! memory-budget crate stays allocator-agnostic; binaries that install
//! `tikv-jemallocator` as `#[global_allocator]` enable the feature and
//! wire it into the budget via [`crate::MemoryBudget::with_jemalloc_stats`].
//! Everyone else falls back to [`NoJemallocStats`] and the budget tick
//! reports zeros for these columns.

/// Snapshot of jemalloc's process-wide accounting at one tick. All
/// values are bytes. Definitions match the `stats.*` mallctl namespace
/// (see [jemalloc(3)](https://jemalloc.net/jemalloc.3.html#stats)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JemallocStats {
    /// Bytes currently held by live application allocations
    /// (`stats.allocated`). The closest thing to "what is the live heap
    /// from the program's point of view". If this grows monotonically
    /// across runs while caches stay flat, the program is holding more
    /// memory.
    pub allocated_bytes: u64,
    /// Bytes in active jemalloc extents (`stats.active`). Always
    /// `>= allocated_bytes`; the delta is fragmentation within active
    /// extents.
    pub active_bytes: u64,
    /// Bytes used by jemalloc's own metadata structures
    /// (`stats.metadata`).
    pub metadata_bytes: u64,
    /// jemalloc's resident-set estimate (`stats.resident`): active +
    /// retained-but-resident + metadata, in pages.
    pub resident_bytes: u64,
    /// Total virtual memory mapped by jemalloc (`stats.mapped`).
    pub mapped_bytes: u64,
    /// Virtual memory retained by jemalloc after decommit
    /// (`stats.retained`). This is the bucket that explains "process
    /// RSS stays high after a workload finishes" — pages were
    /// returned to jemalloc but jemalloc has not yet returned them to
    /// the kernel. Controlled by `dirty_decay_ms` / `muzzy_decay_ms`
    /// in `MALLOC_CONF`.
    pub retained_bytes: u64,
}

/// "What does jemalloc say about our memory usage right now?"
///
/// Implementations must be cheap (the budget calls
/// [`JemallocStatsSource::read`] once per tick). Returning all-zeros
/// is a valid answer for builds that do not use jemalloc.
pub trait JemallocStatsSource: Send + Sync {
    /// Read the current stats. Implementations are free to cache the
    /// epoch handle, but should call `epoch.advance` (or equivalent)
    /// before reading so the returned values reflect the current
    /// state and not the values from the previous epoch.
    fn read(&self) -> JemallocStats;
}

/// No-op source used when the binary does not install jemalloc as
/// the global allocator. All readings are zero, which the tick log
/// distinguishes from real-but-tiny readings via the surrounding
/// `cache_bytes_current` / `rss_bytes` context.
pub struct NoJemallocStats;

impl JemallocStatsSource for NoJemallocStats {
    fn read(&self) -> JemallocStats {
        JemallocStats::default()
    }
}

#[cfg(feature = "jemalloc")]
pub use system::SystemJemallocStats;

#[cfg(feature = "jemalloc")]
mod system {
    use tikv_jemalloc_ctl::{epoch, epoch_mib, stats};

    use super::{JemallocStats, JemallocStatsSource};

    /// Production stats source backed by `tikv-jemalloc-ctl`'s mallctl
    /// wrapper. Caches the per-stat MIB handles so each tick is just
    /// six word-sized reads after an `epoch.advance`.
    ///
    /// Construct via [`SystemJemallocStats::new`]. The constructor
    /// resolves the mallctl handles once; a failure here means jemalloc
    /// was compiled without stats support and the budget has no useful
    /// information to surface, so the caller should fall back to
    /// [`super::NoJemallocStats`] instead.
    pub struct SystemJemallocStats {
        epoch: epoch_mib,
        allocated: stats::allocated_mib,
        active: stats::active_mib,
        metadata: stats::metadata_mib,
        resident: stats::resident_mib,
        mapped: stats::mapped_mib,
        retained: stats::retained_mib,
    }

    impl SystemJemallocStats {
        /// Resolve the mallctl MIB handles. Returns `None` if jemalloc
        /// was built without stats support — caller should fall back
        /// to [`super::NoJemallocStats`].
        #[must_use]
        pub fn new() -> Option<Self> {
            Some(Self {
                epoch: epoch::mib().ok()?,
                allocated: stats::allocated::mib().ok()?,
                active: stats::active::mib().ok()?,
                metadata: stats::metadata::mib().ok()?,
                resident: stats::resident::mib().ok()?,
                mapped: stats::mapped::mib().ok()?,
                retained: stats::retained::mib().ok()?,
            })
        }
    }

    impl JemallocStatsSource for SystemJemallocStats {
        fn read(&self) -> JemallocStats {
            // jemalloc's stats are sampled into a snapshot on each
            // `epoch.advance` call; without this every read returns the
            // values frozen at process start.
            if self.epoch.advance().is_err() {
                return JemallocStats::default();
            }
            JemallocStats {
                allocated_bytes: self.allocated.read().unwrap_or(0) as u64,
                active_bytes: self.active.read().unwrap_or(0) as u64,
                metadata_bytes: self.metadata.read().unwrap_or(0) as u64,
                resident_bytes: self.resident.read().unwrap_or(0) as u64,
                mapped_bytes: self.mapped.read().unwrap_or(0) as u64,
                retained_bytes: self.retained.read().unwrap_or(0) as u64,
            }
        }
    }
}
