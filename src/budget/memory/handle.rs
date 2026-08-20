//! The [`MemoryBudget`] handle and its construction APIs.

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use super::super::state::{BudgetState, Registration, VerdictStreak};
use crate::config::BudgetConfig;
use crate::jemalloc::{JemallocStatsSource, NoJemallocStats};
use crate::policy::{Policy, ProportionalPolicy};
use crate::reporter::NonCacheReporter;
use crate::resizable::Resizable;
use crate::rss::{RssSource, SystemRss};

/// Process-wide coordinator that periodically reapportions cache
/// capacities so the application stays close to its target RSS.
///
/// Construct via [`MemoryBudget::new`] or [`MemoryBudget::from_env`].
/// Register every cache via [`MemoryBudget::register`]. Start the
/// background tick via [`MemoryBudget::spawn`].
pub struct MemoryBudget {
    pub(super) state: Arc<BudgetState>,
}

impl MemoryBudget {
    /// Construct a budget from an explicit config and the default RSS
    /// source and policy. Caller is responsible for spawning the
    /// background ticker via [`MemoryBudget::spawn`].
    #[must_use]
    pub fn new(config: BudgetConfig) -> Self {
        Self::with_rss(config, Box::new(SystemRss::new()))
    }

    /// Convenience constructor that resolves config from environment
    /// using the system RAM reported by the default [`SystemRss`].
    #[must_use]
    pub fn from_env() -> Self {
        let rss = SystemRss::new();
        let config = BudgetConfig::from_env(Some(rss.total_ram_bytes()));
        Self::with_rss(config, Box::new(rss))
    }

    /// One-call bootstrap for an application using jemalloc. Builds a budget from
    /// environment, wires
    /// [`SystemJemallocStats`](crate::SystemJemallocStats), failing if
    /// mallctl initialisation is unavailable, spawns the background ticker, and
    /// emits a one-line `INFO memory_budget::budget` "initialised"
    /// breadcrumb.
    ///
    /// Returns the budget `Arc` so the caller can register caches
    /// later. The spawned ticker holds its own `Arc<BudgetState>`,
    /// so the returned `Arc` may be dropped immediately by binaries
    /// that have no caches to register yet — the tick keeps running.
    ///
    /// Only available with the `jemalloc` feature, because that's
    /// the entire point of this constructor; binaries that don't
    /// install jemalloc as their global allocator should build a
    /// budget by hand via [`MemoryBudget::from_env`].
    #[cfg(feature = "jemalloc")]
    #[must_use]
    pub fn start() -> Arc<Self> {
        // Enable jemalloc's background decay thread at runtime rather
        // than baking `background_thread:true` into MALLOC_CONF: macOS
        // jemalloc lacks pthread background-thread support, so baking
        // it makes every binary print "<jemalloc>: option
        // background_thread currently supports pthread only" and does
        // nothing. The runtime mallctl write succeeds on Linux (the
        // production target, where it converges idle RSS) and returns
        // an error on macOS, which we log at debug and ignore — no
        // startup warning either way.
        match tikv_jemalloc_ctl::background_thread::write(true) {
            Ok(()) => tracing::debug!(
                target: "memory_budget::budget",
                "jemalloc background_thread enabled"
            ),
            Err(e) => tracing::debug!(
                target: "memory_budget::budget",
                error = %e,
                "jemalloc background_thread not enabled (unsupported on this platform)"
            ),
        }
        let mut budget = Self::from_env();
        let stats = crate::SystemJemallocStats::new()
            .expect("jemalloc stats are unavailable despite the jemalloc feature");
        budget = budget.with_jemalloc_stats(Box::new(stats));
        #[cfg(feature = "heap-profiling")]
        {
            budget = budget.with_heap_profiler_from_env();
        }
        let budget = Arc::new(budget);
        // JoinHandle dropped on purpose: tokio keeps the task
        // scheduled, and the spawned closure holds an
        // `Arc<BudgetState>` so the budget survives the drop.
        let _tick = budget.spawn();
        tracing::info!(
            target: "memory_budget::budget",
            target_bytes = budget.target_bytes(),
            tick_interval_secs = budget.tick_interval().as_secs(),
            hard_ceiling_bytes = budget.hard_ceiling_bytes().unwrap_or(0),
            "memory budget initialised"
        );
        budget
    }

    /// Non-jemalloc fallback for builds that don't link tikv-jemallocator
    /// (e.g. Windows-msvc, where the tikv-jemalloc-sys build needs an
    /// MSYS2/autoconf toolchain we don't provide). Builds a budget with
    /// the default RSS source, spawns the ticker, and emits the same
    /// `initialised` breadcrumb so application startup logs stay consistent
    /// across platforms. The jemalloc-stats columns will read zero.
    #[cfg(not(feature = "jemalloc"))]
    #[must_use]
    pub fn start() -> Arc<Self> {
        let budget = Arc::new(Self::from_env());
        let _tick = budget.spawn();
        tracing::info!(
            target: "memory_budget::budget",
            target_bytes = budget.target_bytes(),
            tick_interval_secs = budget.tick_interval().as_secs(),
            hard_ceiling_bytes = budget.hard_ceiling_bytes().unwrap_or(0),
            "memory budget initialised (no jemalloc stats — feature disabled)"
        );
        budget
    }

    /// Construct with a custom [`RssSource`] (used by tests).
    #[must_use]
    pub fn with_rss(config: BudgetConfig, rss: Box<dyn RssSource>) -> Self {
        Self {
            state: Arc::new(BudgetState {
                config,
                registrations: Mutex::new(Vec::new()),
                active_lease_bytes: AtomicU64::new(0),
                previous_cache_bytes: AtomicU64::new(0),
                previous_cache_count: AtomicU64::new(0),
                previous_rss_bytes: AtomicU64::new(0),
                verdict_streak: Mutex::new(VerdictStreak::default()),
                policy: Box::new(ProportionalPolicy),
                rss,
                jemalloc: Box::new(NoJemallocStats),
                reporters: Mutex::new(Vec::new()),
                #[cfg(feature = "heap-profiling")]
                heap_profiler: None,
            }),
        }
    }

    /// Arm the [`HeapProfiler`](crate::heap_profile::HeapProfiler) from
    /// the environment so the tick dumps a jemalloc heap profile each
    /// time resident RSS crosses the next threshold. No-op when
    /// `MEMORY_BUDGET_HEAP_PROFILE_DIR` is unset. Must run before any
    /// registration or spawn.
    ///
    /// # Panics
    ///
    /// Panics if the budget state has already been shared (registered
    /// or spawned).
    #[cfg(feature = "heap-profiling")]
    #[must_use]
    fn with_heap_profiler_from_env(mut self) -> Self {
        let state = Arc::get_mut(&mut self.state)
            .expect("with_heap_profiler_from_env must be called before any registration or spawn");
        state.heap_profiler = crate::heap_profile::HeapProfiler::from_env();
        self
    }

    /// Install a [`JemallocStatsSource`] so the budget tick can split
    /// the non-cache bucket into "live allocations" vs
    /// "retained-by-jemalloc pages". Must be called before any cache
    /// is registered.
    ///
    /// # Panics
    ///
    /// Panics if any cache is already registered, or if the budget's
    /// internal state has been cloned (e.g. the ticker is already
    /// spawned).
    #[must_use]
    pub fn with_jemalloc_stats(mut self, source: Box<dyn JemallocStatsSource>) -> Self {
        let state = Arc::get_mut(&mut self.state)
            .expect("with_jemalloc_stats must be called before any registration or spawn");
        assert!(
            state
                .registrations
                .lock()
                .expect("registrations lock")
                .is_empty(),
            "with_jemalloc_stats must be called before any registration"
        );
        state.jemalloc = source;
        self
    }

    /// Replace the default policy. Mostly used in tests; production
    /// stays on [`ProportionalPolicy`] until benchmark data argues
    /// otherwise.
    ///
    /// # Panics
    ///
    /// Panics if any cache is already registered — the new policy
    /// must be installed before registration so that caches see a
    /// consistent reapportionment regime.
    #[must_use]
    pub fn with_policy(mut self, policy: Box<dyn Policy>) -> Self {
        let state = Arc::get_mut(&mut self.state)
            .expect("with_policy must be called before any registration or spawn");
        assert!(
            state
                .registrations
                .lock()
                .expect("registrations lock")
                .is_empty(),
            "with_policy must be called before any registration"
        );
        state.policy = policy;
        self
    }

    /// Register a cache. The cache's current `max_bytes` is captured
    /// as its preferred ceiling — the policy will never grow a cache
    /// above this value.
    ///
    /// # Panics
    ///
    /// Panics if the internal registrations lock is poisoned.
    pub fn register(&self, cache: &Arc<dyn Resizable>) {
        let preferred_max_bytes = cache.max_bytes();
        let mut guard = self.state.registrations.lock().expect("registrations lock");
        guard.push(Registration {
            cache: Arc::downgrade(cache),
            preferred_max_bytes,
        });
    }

    /// Register a read-only [`NonCacheReporter`] so the tick can
    /// attribute a named slice of the `non_cache_live` bucket to it.
    /// The budget holds a `Weak`, so dropping the reporter prunes it
    /// from the next tick. Unlike caches, reporters are never resized.
    ///
    /// # Panics
    ///
    /// Panics if the internal reporters lock is poisoned.
    pub fn register_reporter(&self, reporter: &Arc<dyn NonCacheReporter>) {
        let mut guard = self.state.reporters.lock().expect("reporters lock");
        guard.push(Arc::downgrade(reporter));
    }
}
