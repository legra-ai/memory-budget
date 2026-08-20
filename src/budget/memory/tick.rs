//! The reapportionment tick loop and its supporting helpers.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::super::snapshot::{BudgetSnapshot, CacheSnapshot, ReporterSnapshot};
use super::super::state::LiveViews;
use super::handle::MemoryBudget;
use crate::policy::{CacheView, PolicyInput};
use crate::resizable::Resizable;

impl MemoryBudget {
    /// Run one reapportionment tick synchronously. Returns the
    /// snapshot used for diagnostic emission. Exposed publicly so
    /// tests can drive the budget without spawning a tokio task.
    pub fn tick_now(&self) -> BudgetSnapshot {
        let rss_sysinfo_bytes = self.state.rss.read_rss_bytes();
        let snapshot = self.run_tick(rss_sysinfo_bytes);
        self.emit(&snapshot);
        // Capture a heap profile when resident RSS crosses the next
        // threshold, so a `non_cache_other` balloon is attributed to its
        // allocation site. Inert unless the `heap-profiling` build is
        // armed via `MEMORY_BUDGET_HEAP_PROFILE_DIR`.
        #[cfg(feature = "heap-profiling")]
        if let Some(profiler) = self.state.heap_profiler.as_ref() {
            profiler.maybe_dump(snapshot.rss_bytes);
        }
        // Check the ceiling against the *effective* RSS (jemalloc
        // resident when present), not the sysinfo reading — on macOS
        // the latter is compressed-footprint and would never trip a
        // ceiling that the real allocation has already blown past.
        if let Some(ceiling) = self.state.config.hard_ceiling_bytes()
            && snapshot.rss_bytes > ceiling
        {
            tracing::error!(
                target: "memory_budget::budget",
                rss_bytes = snapshot.rss_bytes,
                rss_sysinfo_bytes = snapshot.rss_sysinfo_bytes,
                ceiling_bytes = ceiling,
                "hard ceiling exceeded — aborting process"
            );
            std::process::abort();
        }
        snapshot
    }

    /// Spawn a background tokio task that calls [`MemoryBudget::tick_now`]
    /// at the configured cadence. Returns a handle that aborts the
    /// task on drop.
    pub fn spawn(&self) -> tokio::task::JoinHandle<()> {
        let state = Arc::clone(&self.state);
        let interval = state.config.tick();
        tokio::spawn(async move {
            let handle = MemoryBudget { state };
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                handle.tick_now();
            }
        })
    }

    /// Apply one reapportionment. `rss_sysinfo_bytes` is the raw
    /// sysinfo reading; the policy runs off the **effective** RSS —
    /// `jemalloc.resident_bytes` when jemalloc stats are present,
    /// else the sysinfo value (see [`BudgetSnapshot::rss_bytes`]).
    fn run_tick(&self, rss_sysinfo_bytes: u64) -> BudgetSnapshot {
        let effective_target = self
            .state
            .config
            .target_bytes()
            .saturating_sub(self.state.active_lease_bytes.load(Ordering::Relaxed));
        let jemalloc = self.state.jemalloc.read();
        // Prefer jemalloc's resident estimate as the RSS signal: on
        // macOS sysinfo's phys_footprint runs far below the real
        // allocation (compression), which would leave the policy
        // permanently in `grow`. Fall back to sysinfo when jemalloc
        // stats are absent (non-jemalloc binaries / mallctl init
        // failure → resident reads 0).
        let rss_bytes = if jemalloc.resident_bytes > 0 {
            jemalloc.resident_bytes
        } else {
            rss_sysinfo_bytes
        };
        let live = self.live_views();
        let previous_cache_bytes = self.state.previous_cache_bytes.load(Ordering::Relaxed);
        let previous_cache_count = self.state.previous_cache_count.load(Ordering::Relaxed) as u32;
        let previous_rss_bytes = self.state.previous_rss_bytes.load(Ordering::Relaxed);
        let input = PolicyInput {
            effective_target_bytes: effective_target,
            rss_bytes,
            caches: live.views,
            previous_cache_bytes,
            previous_cache_count,
            previous_rss_bytes,
        };
        let output = self.state.policy.reapportion(&input);
        for (cache, new_max) in live.live_caches.iter().zip(output.new_max_bytes.iter()) {
            cache.set_max_bytes(*new_max);
        }
        // Update the trend history *after* the policy has read it.
        // Use the pre-reapportionment `current_bytes` since
        // `set_max_bytes` above may have triggered eviction, and we
        // want the next tick to compare against what this tick saw
        // before its decision took effect.
        let current_cache_bytes: u64 = input.caches.iter().map(|c| c.current_bytes).sum();
        self.state
            .previous_cache_bytes
            .store(current_cache_bytes, Ordering::Relaxed);
        self.state
            .previous_cache_count
            .store(input.caches.len() as u64, Ordering::Relaxed);
        self.state
            .previous_rss_bytes
            .store(rss_bytes, Ordering::Relaxed);
        let verdict_label = output.verdict.label();
        let (verdict_streak, previous_verdict_label, previous_verdict_streak) =
            self.advance_verdict_streak(verdict_label);
        let reporters = self.live_reporters();
        BudgetSnapshot {
            target_bytes: self.state.config.target_bytes(),
            effective_target_bytes: effective_target,
            rss_bytes,
            rss_sysinfo_bytes,
            lease_bytes_active: self.state.active_lease_bytes.load(Ordering::Relaxed),
            jemalloc,
            verdict: output.verdict,
            verdict_streak,
            previous_verdict_label,
            previous_verdict_streak,
            reporters,
            caches: input
                .caches
                .into_iter()
                .zip(output.new_max_bytes)
                .map(|(view, new_max)| CacheSnapshot {
                    name: view.name,
                    current_bytes: view.current_bytes,
                    previous_max_bytes: view.max_bytes,
                    new_max_bytes: new_max,
                    hit_rate: view.stats.hit_rate(),
                })
                .collect(),
        }
    }

    /// Update the verdict-streak run-length state for `label`. Returns
    /// `(streak_now, previous_label, previous_streak)`. `previous_*`
    /// are `Some` only when the label changed at this tick — i.e.
    /// the previous run ended.
    fn advance_verdict_streak(
        &self,
        label: &'static str,
    ) -> (u32, Option<&'static str>, Option<u32>) {
        let mut guard = self
            .state
            .verdict_streak
            .lock()
            .expect("verdict streak lock");
        if guard.label == label && guard.length > 0 {
            guard.length = guard.length.saturating_add(1);
            (guard.length, None, None)
        } else {
            let prev_label = if guard.length > 0 {
                Some(guard.label)
            } else {
                None
            };
            let prev_streak = if guard.length > 0 {
                Some(guard.length)
            } else {
                None
            };
            guard.label = label;
            guard.length = 1;
            (1, prev_label, prev_streak)
        }
    }

    /// Walk the weak registrations, drop dead entries, and produce
    /// the parallel (Arc, view) lists used by the tick.
    fn live_views(&self) -> LiveViews {
        let mut guard = self.state.registrations.lock().expect("registrations lock");
        let mut views = Vec::with_capacity(guard.len());
        let mut live_caches: Vec<Arc<dyn Resizable>> = Vec::with_capacity(guard.len());
        guard.retain(|reg| {
            let Some(cache) = reg.cache.upgrade() else {
                return false;
            };
            views.push(CacheView {
                name: cache.name().to_owned(),
                max_bytes: cache.max_bytes(),
                preferred_max_bytes: reg.preferred_max_bytes,
                current_bytes: cache.current_bytes(),
                stats: cache.stats(),
            });
            live_caches.push(cache);
            true
        });
        LiveViews { views, live_caches }
    }

    /// Walk the weak reporter registrations, drop dead entries, and
    /// snapshot each live reporter's current byte count.
    fn live_reporters(&self) -> Vec<ReporterSnapshot> {
        let mut guard = self.state.reporters.lock().expect("reporters lock");
        let mut out = Vec::with_capacity(guard.len());
        guard.retain(|weak| {
            let Some(reporter) = weak.upgrade() else {
                return false;
            };
            out.push(ReporterSnapshot {
                name: reporter.name().to_owned(),
                current_bytes: reporter.current_bytes(),
            });
            true
        });
        out
    }
}
