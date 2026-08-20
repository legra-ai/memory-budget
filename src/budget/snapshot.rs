//! Diagnostic snapshots emitted by the budget tick.

use crate::jemalloc::JemallocStats;
use crate::policy::Verdict;

/// Diagnostic snapshot of one budget tick.
#[derive(Clone, Debug)]
pub struct BudgetSnapshot {
    /// Configured target RSS in bytes.
    pub target_bytes: u64,
    /// Target after active-lease subtraction.
    pub effective_target_bytes: u64,
    /// **Effective** RSS the policy used this tick. When jemalloc
    /// stats are present this is `jemalloc.resident_bytes` (the true
    /// resident footprint); otherwise it falls back to the sysinfo
    /// reading in [`BudgetSnapshot::rss_sysinfo_bytes`].
    ///
    /// On some platforms, the operating-system reading can diverge
    /// materially from the allocator's resident estimate. Driving the
    /// policy off `jemalloc.resident` keeps grow/shrink decisions aligned
    /// with the allocator when those statistics are available.
    pub rss_bytes: u64,
    /// Raw sysinfo RSS reading (`phys_footprint` on macOS, RSS on
    /// Linux). Kept for the gap diagnostic — compare against
    /// [`BudgetSnapshot::rss_bytes`] to see how far the OS-reported
    /// footprint diverges from jemalloc's view. Equals `rss_bytes`
    /// when no jemalloc stats are available.
    pub rss_sysinfo_bytes: u64,
    /// Sum of bytes reserved by live [`LeaseGuard`](crate::LeaseGuard)s.
    pub lease_bytes_active: u64,
    /// jemalloc introspection at tick time. Splits the non-cache
    /// bucket into "live allocations the program holds"
    /// ([`JemallocStats::allocated_bytes`]) and "pages jemalloc has
    /// not yet returned to the kernel"
    /// ([`JemallocStats::retained_bytes`]). Zero on builds that do
    /// not install jemalloc.
    pub jemalloc: JemallocStats,
    /// What the policy decided this tick and why.
    pub verdict: Verdict,
    /// Number of consecutive ticks (including this one) that have
    /// ended in the same [`Verdict`] label. Resets to `1` when the
    /// label changes. Useful for spotting wedges in post-mortem
    /// logs ("the budget has been at `skip-shrink-lean-caches` for
    /// 480 ticks") without arithmetic on timestamps.
    pub verdict_streak: u32,
    /// When the verdict label changed at this tick, the label of
    /// the previous run; `None` otherwise. Lets post-mortem capture
    /// the length of the run that just ended via
    /// [`BudgetSnapshot::previous_verdict_streak`].
    pub previous_verdict_label: Option<&'static str>,
    /// Length of the run that just ended (`Some` when
    /// [`BudgetSnapshot::previous_verdict_label`] is `Some`).
    pub previous_verdict_streak: Option<u32>,
    /// One entry per still-live registered cache.
    pub caches: Vec<CacheSnapshot>,
    /// One entry per still-live registered [`NonCacheReporter`]
    /// (e.g. the reasoner working-set gauge). Names slices of the
    /// otherwise-opaque `non_cache_live` bucket.
    ///
    /// [`NonCacheReporter`]: crate::NonCacheReporter
    pub reporters: Vec<ReporterSnapshot>,
}

impl BudgetSnapshot {
    /// Sum of `new_max_bytes` across every cache after the tick.
    /// Useful when logging "what did the budget reapportion to".
    #[must_use]
    pub fn cap_bytes_after(&self) -> u64 {
        self.caches.iter().map(|c| c.new_max_bytes).sum()
    }

    /// Sum of `current_bytes` across every cache at tick time —
    /// i.e. live cache footprint, before the new cap is applied.
    #[must_use]
    pub fn cache_bytes_current(&self) -> u64 {
        self.caches.iter().map(|c| c.current_bytes).sum()
    }

    /// `rss_bytes − cache_bytes_current − lease_bytes_active`,
    /// saturating at zero. This is the **non-cache bucket**: heap
    /// that the budget knows about but cannot resize — typically
    /// transient allocator state (parser context, query result buffers,
    /// or other working memory) plus the
    /// non-reclaimable parts of jemalloc's retained-decommitted
    /// pages.
    ///
    /// Surfaced on every tick emission as `non_cache_bytes` so
    /// post-mortem analysis can attribute RSS growth to a specific
    /// bucket (cache pressure vs. non-cache spike) instead of
    /// inferring it from the cache breakdown. See
    /// [`BudgetSnapshot::non_cache_live_bytes`] for the further
    /// jemalloc-attributed split.
    #[must_use]
    pub fn non_cache_bytes(&self) -> u64 {
        self.rss_bytes
            .saturating_sub(self.cache_bytes_current())
            .saturating_sub(self.lease_bytes_active)
    }

    /// Non-cache bytes attributed to **live application
    /// allocations** by jemalloc: `je_allocated_bytes −
    /// cache_bytes_current − lease_bytes_active`, saturating at
    /// zero.
    ///
    /// Reading this alongside `non_cache_bytes` is the diagnostic
    /// that separates real leaks from jemalloc retention. If
    /// `non_cache_live_bytes` is flat across a long-running soak but
    /// `non_cache_bytes` keeps growing, the growth is jemalloc
    /// retained pages (`jemalloc.retained_bytes`) — an allocator
    /// tuning issue. If `non_cache_live_bytes` itself grows, the
    /// program is holding more memory (a leak somewhere outside
    /// the registered caches).
    ///
    /// Zero on builds that do not install jemalloc.
    #[must_use]
    pub fn non_cache_live_bytes(&self) -> u64 {
        self.jemalloc
            .allocated_bytes
            .saturating_sub(self.cache_bytes_current())
            .saturating_sub(self.lease_bytes_active)
    }

    /// Sum of `current_bytes` across all live non-cache reporters —
    /// the **attributed** portion of `non_cache_live_bytes`.
    #[must_use]
    pub fn reporter_bytes(&self) -> u64 {
        self.reporters
            .iter()
            .map(|r| r.current_bytes)
            .fold(0u64, u64::saturating_add)
    }

    /// `non_cache_live_bytes − reporter_bytes`, saturating at zero —
    /// the **unattributed** remainder of the live non-cache bucket
    /// (parser context, query buffers, transient allocator state, and
    /// any subsystem that hasn't registered a reporter yet).
    #[must_use]
    pub fn non_cache_other_bytes(&self) -> u64 {
        self.non_cache_live_bytes()
            .saturating_sub(self.reporter_bytes())
    }
}

/// Per-reporter slice of a [`BudgetSnapshot`].
#[derive(Clone, Debug)]
pub struct ReporterSnapshot {
    /// Reporter name (`NonCacheReporter::name`).
    pub name: String,
    /// Bytes the reporter said it was holding when the tick fired.
    pub current_bytes: u64,
}

/// Per-cache slice of a [`BudgetSnapshot`].
#[derive(Clone, Debug)]
pub struct CacheSnapshot {
    /// Cache name (`Resizable::name`).
    pub name: String,
    /// Bytes currently held by the cache when the tick fired.
    pub current_bytes: u64,
    /// Cap before this tick reapportioned.
    pub previous_max_bytes: u64,
    /// Cap installed by this tick.
    pub new_max_bytes: u64,
    /// Most recent hit rate; `None` if no lookups yet.
    pub hit_rate: Option<f64>,
}

impl CacheSnapshot {
    /// Signed cap change this tick (`new_max_bytes − previous_max_bytes`).
    /// Negative on shrink, positive on grow, zero on hold or skip.
    ///
    /// Logged as `delta_bytes` on each per-cache emission so
    /// post-mortem analysis can grep for nonzero deltas directly
    /// without doing arithmetic.
    ///
    /// Returns `i64::MAX` / `i64::MIN` on extreme cases where the
    /// difference would not fit in `i64` — practically unreachable
    /// (caps are bounded by `MEMORY_BUDGET_TARGET_MIB`, not `u64::MAX`)
    /// but the saturation keeps the math total.
    #[must_use]
    pub fn delta_bytes(&self) -> i64 {
        let prev = i64::try_from(self.previous_max_bytes).unwrap_or(i64::MAX);
        let new = i64::try_from(self.new_max_bytes).unwrap_or(i64::MAX);
        new.saturating_sub(prev)
    }
}
