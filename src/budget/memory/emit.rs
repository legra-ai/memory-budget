//! Tracing emission for budget ticks and verdict rendering.

use super::super::snapshot::BudgetSnapshot;
use super::handle::MemoryBudget;
use crate::policy::Verdict;

impl MemoryBudget {
    pub(super) fn emit(&self, snapshot: &BudgetSnapshot) {
        let (verdict_label, verdict_detail) = describe_verdict(&snapshot.verdict);
        // When the run just ended, surface the previous label +
        // length on a dedicated line so the boundary is greppable
        // ("verdict 'skip-shrink-lean-caches' ended after 480 ticks").
        if let (Some(prev_label), Some(prev_streak)) = (
            snapshot.previous_verdict_label,
            snapshot.previous_verdict_streak,
        ) {
            tracing::info!(
                target: "memory_budget::budget",
                previous_verdict = prev_label,
                previous_verdict_streak = prev_streak,
                new_verdict = verdict_label,
                "verdict run ended"
            );
        }
        // The periodic tick is steady-state telemetry: at idle the
        // verdict is `Hold` tick after tick (~250 lines per 20 min),
        // which buries everything else. Emit a `Hold` tick at DEBUG and
        // any acting verdict (Shrink / SkipShrink / Grow) at INFO, so a
        // first-pass scan surfaces every budget *decision* without the
        // steady-state stream. Verdict *boundaries* are already on the
        // greppable "verdict run ended" INFO line above; full per-tick
        // history stays one `RUST_LOG=memory_budget::budget=debug` away.
        macro_rules! emit_tick {
            ($level:expr) => {
                tracing::event!(
                    target: "memory_budget::budget",
                    $level,
                    target_bytes = snapshot.target_bytes,
                    effective_target_bytes = snapshot.effective_target_bytes,
                    rss_bytes = snapshot.rss_bytes,
                    rss_sysinfo_bytes = snapshot.rss_sysinfo_bytes,
                    lease_bytes_active = snapshot.lease_bytes_active,
                    cap_bytes = snapshot.cap_bytes_after(),
                    cache_bytes_current = snapshot.cache_bytes_current(),
                    non_cache_bytes = snapshot.non_cache_bytes(),
                    non_cache_live_bytes = snapshot.non_cache_live_bytes(),
                    non_cache_reporter_bytes = snapshot.reporter_bytes(),
                    non_cache_other_bytes = snapshot.non_cache_other_bytes(),
                    non_cache_retained_bytes = snapshot.jemalloc.retained_bytes,
                    je_allocated_bytes = snapshot.jemalloc.allocated_bytes,
                    je_active_bytes = snapshot.jemalloc.active_bytes,
                    je_metadata_bytes = snapshot.jemalloc.metadata_bytes,
                    je_resident_bytes = snapshot.jemalloc.resident_bytes,
                    je_mapped_bytes = snapshot.jemalloc.mapped_bytes,
                    je_retained_bytes = snapshot.jemalloc.retained_bytes,
                    cache_count = snapshot.caches.len(),
                    reporter_count = snapshot.reporters.len(),
                    verdict = verdict_label,
                    verdict_detail = %verdict_detail,
                    verdict_streak = snapshot.verdict_streak,
                    "memory budget tick"
                );
            };
        }
        if matches!(snapshot.verdict, Verdict::Hold) {
            emit_tick!(tracing::Level::DEBUG);
        } else {
            emit_tick!(tracing::Level::INFO);
        }
        // Per-reporter / per-cache breakdowns follow the tick's level:
        // DEBUG when the verdict is `Hold` (steady state, drill-down
        // only), INFO otherwise. The INFO case is the one that matters
        // for diagnosis — when the budget is *not* holding (memory
        // pressure / a balloon), the production log names exactly what
        // each reporter holds, so an unattributed `non_cache_other`
        // spike is immediately visible as "every reporter reads ~0 →
        // the bytes are in an un-instrumented subsystem" rather than
        // hiding behind an aggregate that only DEBUG could break down.
        macro_rules! emit_breakdown {
            ($level:expr) => {
                for reporter in &snapshot.reporters {
                    tracing::event!(
                        target: "memory_budget::budget",
                        $level,
                        reporter = %reporter.name,
                        current_bytes = reporter.current_bytes,
                        "memory budget reporter"
                    );
                }
                for cache in &snapshot.caches {
                    tracing::event!(
                        target: "memory_budget::budget",
                        $level,
                        cache = %cache.name,
                        current_bytes = cache.current_bytes,
                        previous_max_bytes = cache.previous_max_bytes,
                        new_max_bytes = cache.new_max_bytes,
                        delta_bytes = cache.delta_bytes(),
                        hit_rate = cache.hit_rate.map(|v| format!("{v:.3}")).unwrap_or_default(),
                        "memory budget cache"
                    );
                }
            };
        }
        if matches!(snapshot.verdict, Verdict::Hold) {
            emit_breakdown!(tracing::Level::DEBUG);
        } else {
            emit_breakdown!(tracing::Level::INFO);
        }
    }
}

/// Render a [`Verdict`] into a one-word label and a structured detail string
/// for log emission. The detail is empty for `Hold` (no extra context worth
/// logging) and otherwise carries the policy's numeric arguments.
fn describe_verdict(verdict: &Verdict) -> (&'static str, String) {
    match verdict {
        Verdict::Hold => (verdict.label(), String::new()),
        Verdict::Shrink { factor } => (verdict.label(), format!("factor={factor:.3}")),
        Verdict::SkipShrinkLeanCaches {
            cache_bytes,
            rss_bytes,
            cache_share,
        } => (
            verdict.label(),
            format!(
                "cache_bytes={cache_bytes} rss_bytes={rss_bytes} \
                 cache_share={cache_share:.3}",
            ),
        ),
        Verdict::SkipShrinkFlatCaches {
            cache_bytes,
            previous_cache_bytes,
            cache_count,
            previous_cache_count,
            rss_bytes,
        } => (
            verdict.label(),
            format!(
                "cache_bytes={cache_bytes} \
                 previous_cache_bytes={previous_cache_bytes} \
                 cache_count={cache_count} \
                 previous_cache_count={previous_cache_count} \
                 rss_bytes={rss_bytes}",
            ),
        ),
        Verdict::Grow { headroom_bytes } => {
            (verdict.label(), format!("headroom_bytes={headroom_bytes}"))
        }
    }
}
