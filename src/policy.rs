//! Reapportionment policies for [`MemoryBudget`](crate::MemoryBudget).
//!
//! The default [`ProportionalPolicy`] implements a hysteresis-based
//! proportional allocation algorithm:
//!
//! - Above the upper hysteresis band, each cache shrinks proportionally so the
//!   projected total fits the target — **unless** the sum of cache bytes is a
//!   small fraction of RSS, in which case shrinking caches cannot meaningfully
//!   reduce RSS (the bulk lives outside the cache subsystem, e.g. in a
//!   transient allocator or working set). In
//!   that case the policy holds with a `SkipShrinkLeanCaches` verdict.
//! - Below the lower hysteresis band, caches grow back toward their preferred
//!   ceiling. Caches with higher hit rates get more of the slack first.
//! - Inside the band, the policy does nothing — avoiding the thrashing that a
//!   hysteresis-less algorithm would produce when RSS sits within ε of the
//!   target.
//!
//! Every reapportionment produces a [`Verdict`] explaining what the policy
//! decided and why. The budget logs the verdict on each tick so post-mortem
//! analysis can distinguish "shrunk because cache N grew" from "skipped
//! shrink because RSS climbed but caches stayed lean".

use crate::config::{GROW_RATIO, SHRINK_RATIO};
use crate::resizable::ResizableStats;

/// Cache footprint as a fraction of RSS below which the shrink path is a
/// no-op. When caches account for less than this share of process RSS, the
/// growth is coming from somewhere else (transient allocator, leaked
/// non-cache buffer) and shrinking caches just trades a useful working set
/// for an unrelated runaway.
///
/// `0.25` leaves room for applications whose working set is dominated by
/// allocations outside their registered caches. Tune this threshold when
/// application-specific non-cache reporters provide more precise accounting.
pub(crate) const MIN_CACHE_SHARE_FOR_SHRINK: f64 = 0.25;

/// A single cache's view passed to the policy.
#[derive(Clone, Debug)]
pub struct CacheView {
    /// Human-readable name (forwarded from
    /// [`Resizable::name`](crate::Resizable::name)).
    pub name: String,
    /// Current soft cap, in bytes.
    pub max_bytes: u64,
    /// Cache's preferred ceiling — typically its construction-time
    /// value. The policy refuses to grow caps above this.
    pub preferred_max_bytes: u64,
    /// Currently held bytes.
    pub current_bytes: u64,
    /// Stats snapshot.
    pub stats: ResizableStats,
}

/// Inputs the policy needs to compute a new set of caps.
#[derive(Clone, Debug)]
pub struct PolicyInput {
    /// Effective target after lease deductions, in bytes.
    pub effective_target_bytes: u64,
    /// Most recent RSS reading.
    pub rss_bytes: u64,
    /// View of every registered cache, in registration order.
    pub caches: Vec<CacheView>,
    /// Sum of cache `current_bytes` at the previous tick, or `0`
    /// if this is the first tick (no history yet). The shrink
    /// branch uses this to detect "RSS climbed but caches didn't"
    /// — a signal that the growth came from outside the cache
    /// subsystem.
    pub previous_cache_bytes: u64,
    /// Number of registered caches at the previous tick. `0` on
    /// first tick.
    pub previous_cache_count: u32,
    /// RSS reading at the previous tick. `0` on first tick.
    /// The flat-caches guard requires RSS to have actually grown
    /// (`rss_bytes > previous_rss_bytes`) so a lease-induced
    /// effective-target drop — which raises the rss/target ratio
    /// without an actual RSS climb — still triggers shrink.
    pub previous_rss_bytes: u64,
}

/// The policy returns one new cap per input cache, same order, plus a
/// [`Verdict`] explaining the decision.
#[derive(Clone, Debug, Default)]
pub struct PolicyOutput {
    /// New `max_bytes` for each cache, parallel to
    /// [`PolicyInput::caches`].
    pub new_max_bytes: Vec<u64>,
    /// What the policy decided this tick and why.
    pub verdict: Verdict,
}

/// What the policy decided this tick. Shared across all caches in the
/// same reapportionment (every cache gets the same regime).
#[derive(Clone, Debug, Default)]
pub enum Verdict {
    /// RSS is inside the hysteresis band — caps held steady.
    #[default]
    Hold,
    /// RSS over target × `SHRINK_RATIO`; every cap multiplied by
    /// `target / rss`.
    Shrink {
        /// `target / rss`, in `(0.0, 1.0)`.
        factor: f64,
    },
    /// RSS over target × `SHRINK_RATIO`, but the sum of cache
    /// `current_bytes` is < `MIN_CACHE_SHARE_FOR_SHRINK` of RSS —
    /// shrinking caches would not meaningfully reduce RSS, so the
    /// policy holds. The growth is coming from a non-cache allocator
    /// (transient parser/reasoner state, leaked buffer, etc.).
    SkipShrinkLeanCaches {
        /// Sum of `current_bytes` across registered caches.
        cache_bytes: u64,
        /// Process RSS at the tick.
        rss_bytes: u64,
        /// `cache_bytes / rss_bytes`, in `[0.0, 1.0]`.
        cache_share: f64,
    },
    /// RSS over target × `SHRINK_RATIO`, but the cache subsystem did
    /// not grow between the previous tick and this one (`cache_count`
    /// is flat AND `cache_bytes` is within
    /// `FLAT_CACHE_TOLERANCE` of the previous reading). The RSS
    /// climb came from outside the cache subsystem — shrinking caches
    /// would trade away a useful working set for an unrelated
    /// allocator's growth (e.g. parser, query-engine, or reasoner working
    /// memory).
    SkipShrinkFlatCaches {
        /// Sum of `current_bytes` across registered caches.
        cache_bytes: u64,
        /// Previous tick's `cache_bytes`.
        previous_cache_bytes: u64,
        /// Number of registered caches this tick.
        cache_count: u32,
        /// Previous tick's cache count.
        previous_cache_count: u32,
        /// Process RSS at the tick.
        rss_bytes: u64,
    },
    /// RSS under target × `GROW_RATIO`; caches grew toward their
    /// `preferred_max_bytes` ceilings, weighted by hit rate.
    Grow {
        /// `target - rss`, distributed across caches.
        headroom_bytes: u64,
    },
}

impl Verdict {
    /// One-word label for the verdict (used in log emissions).
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Hold => "hold",
            Verdict::Shrink { .. } => "shrink",
            Verdict::SkipShrinkLeanCaches { .. } => "skip-shrink-lean-caches",
            Verdict::SkipShrinkFlatCaches { .. } => "skip-shrink-flat-caches",
            Verdict::Grow { .. } => "grow",
        }
    }
}

/// `cache_bytes` is considered "flat" against the previous tick when
/// it has grown by less than this fraction. Chosen wide enough to
/// absorb the few-MB churn that LRU eviction + insertion produces on
/// a quiet cache, but tight enough that real cache fill (typically
/// 10s of MB per tick during a workload) breaks the threshold.
pub(crate) const FLAT_CACHE_TOLERANCE: f64 = 0.05;

/// Policy contract.
pub trait Policy: Send + Sync {
    /// Compute new caps. Implementations must return one entry per
    /// input cache, in the same order; the budget then applies each
    /// via [`Resizable::set_max_bytes`](crate::Resizable::set_max_bytes).
    fn reapportion(&self, input: &PolicyInput) -> PolicyOutput;
}

/// Default policy for proportional cache-capacity adjustment.
///
/// - **Shrink** (`rss > target × SHRINK_RATIO`): each cache shrinks
///   proportionally — `new = current_max × target / rss` — so the projected
///   total fits the target. Skipped if `sum(current_bytes) / rss` is below
///   `MIN_CACHE_SHARE_FOR_SHRINK` (caches are not the bulk of memory).
/// - **Grow** (`rss < target × GROW_RATIO`): caches with the highest hit rate
///   get the first share of the slack, capped at their `preferred_max_bytes`.
/// - **Hold** (in-band): no change.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProportionalPolicy;

impl Policy for ProportionalPolicy {
    fn reapportion(&self, input: &PolicyInput) -> PolicyOutput {
        let target = input.effective_target_bytes as f64;
        let rss = input.rss_bytes as f64;
        let shrink_threshold = target * SHRINK_RATIO;
        let grow_threshold = target * GROW_RATIO;

        let mut new_max_bytes = Vec::with_capacity(input.caches.len());

        // Pressure regime: RSS over the upper band (or pathological
        // case where target == 0). Shrink unless one of two guards
        // fires (both indicate the bulk of memory lives outside the
        // cache subsystem and shrinking caches would be ineffective):
        //
        // 1. Trend guard — RSS actually climbed since the previous tick but the cache
        //    subsystem didn't (cache_count flat AND cache_bytes within
        //    FLAT_CACHE_TOLERANCE). The new growth came from somewhere else, not the
        //    cache subsystem. Requires `rss_bytes > previous_rss_bytes` so a lease-
        //    induced effective-target drop (which can push the rss/target ratio over
        //    threshold without an actual RSS climb) still triggers shrink.
        // 2. Static guard — sum(current_bytes) is < 25% of RSS, so even halving every
        //    cap can't meaningfully reduce RSS.
        if target == 0.0 || rss >= shrink_threshold {
            let cache_bytes: u64 = input.caches.iter().map(|c| c.current_bytes).sum();
            let cache_count = input.caches.len() as u32;
            let have_history = input.previous_rss_bytes > 0;
            let rss_grew = input.rss_bytes > input.previous_rss_bytes;
            let count_flat = cache_count <= input.previous_cache_count;
            // Strictly: current cache_bytes must not exceed the
            // previous reading times the tolerance. When previous
            // is zero, any nonzero current is growth (caches just
            // started filling) — not flat. The expression handles
            // that correctly: `0 * 1.05 = 0`, so `current > 0`
            // fails the predicate.
            let bytes_flat = cache_bytes as f64
                <= (input.previous_cache_bytes as f64) * (1.0 + FLAT_CACHE_TOLERANCE);

            if have_history && rss_grew && count_flat && bytes_flat {
                for cache in &input.caches {
                    new_max_bytes.push(cache.max_bytes);
                }
                return PolicyOutput {
                    new_max_bytes,
                    verdict: Verdict::SkipShrinkFlatCaches {
                        cache_bytes,
                        previous_cache_bytes: input.previous_cache_bytes,
                        cache_count,
                        previous_cache_count: input.previous_cache_count,
                        rss_bytes: input.rss_bytes,
                    },
                };
            }

            let cache_share = if rss > 0.0 {
                cache_bytes as f64 / rss
            } else {
                1.0
            };
            if rss > 0.0 && cache_share < MIN_CACHE_SHARE_FOR_SHRINK {
                for cache in &input.caches {
                    new_max_bytes.push(cache.max_bytes);
                }
                return PolicyOutput {
                    new_max_bytes,
                    verdict: Verdict::SkipShrinkLeanCaches {
                        cache_bytes,
                        rss_bytes: input.rss_bytes,
                        cache_share,
                    },
                };
            }
            let factor = if rss > 0.0 { target / rss } else { 1.0 };
            for cache in &input.caches {
                let shrunk = ((cache.max_bytes as f64) * factor).max(0.0) as u64;
                new_max_bytes.push(shrunk);
            }
            return PolicyOutput {
                new_max_bytes,
                verdict: Verdict::Shrink { factor },
            };
        }

        if rss <= grow_threshold {
            let headroom = ((target - rss).max(0.0)) as u64;
            grow_under_pressure(input, &mut new_max_bytes);
            return PolicyOutput {
                new_max_bytes,
                verdict: Verdict::Grow {
                    headroom_bytes: headroom,
                },
            };
        }

        // Inside the hysteresis band — hold steady.
        for cache in &input.caches {
            new_max_bytes.push(cache.max_bytes);
        }
        PolicyOutput {
            new_max_bytes,
            verdict: Verdict::Hold,
        }
    }
}

/// Distribute the headroom between current RSS and the target
/// proportionally to each cache's hit rate, never exceeding the
/// preferred ceiling. Caches with no observed lookups yet get the
/// fallback rate of 0.0 (no growth).
fn grow_under_pressure(input: &PolicyInput, out: &mut Vec<u64>) {
    let target = input.effective_target_bytes as f64;
    let rss = input.rss_bytes as f64;
    let headroom = (target - rss).max(0.0);
    let weights: Vec<f64> = input
        .caches
        .iter()
        .map(|c| c.stats.hit_rate().unwrap_or(0.0))
        .collect();
    let weight_sum: f64 = weights.iter().sum();
    for (cache, weight) in input.caches.iter().zip(weights.iter()) {
        let share = if weight_sum > 0.0 {
            headroom * (weight / weight_sum)
        } else {
            // No hit-rate signal yet — distribute evenly.
            headroom / (input.caches.len().max(1) as f64)
        };
        let proposed = (cache.max_bytes as f64) + share;
        let bounded = proposed.min(cache.preferred_max_bytes as f64);
        out.push(bounded as u64);
    }
}
