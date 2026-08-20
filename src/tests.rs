//! Unit tests for [`crate::MemoryBudget`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::{
    AtomicReporter, BudgetConfig, JemallocStats, JemallocStatsSource, MemoryBudget,
    NonCacheReporter, Resizable, ResizableStats, RssSource,
};

/// In-memory test cache: records `set_max_bytes` calls and exposes
/// configurable `hits` / `current_bytes` readings.
struct FakeCache {
    name: String,
    current_bytes: AtomicU64,
    max_bytes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl FakeCache {
    fn new(name: &str, max_bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            current_bytes: AtomicU64::new(0),
            max_bytes: AtomicU64::new(max_bytes),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    fn set_current_bytes(&self, v: u64) {
        self.current_bytes.store(v, Ordering::Relaxed);
    }

    fn set_hits(&self, hits: u64, misses: u64) {
        self.hits.store(hits, Ordering::Relaxed);
        self.misses.store(misses, Ordering::Relaxed);
    }
}

impl Resizable for FakeCache {
    fn name(&self) -> &str {
        &self.name
    }
    fn current_bytes(&self) -> u64 {
        self.current_bytes.load(Ordering::Relaxed)
    }
    fn max_bytes(&self) -> u64 {
        self.max_bytes.load(Ordering::Relaxed)
    }
    fn set_max_bytes(&self, new: u64) {
        self.max_bytes.store(new, Ordering::Relaxed);
        if self.current_bytes.load(Ordering::Relaxed) > new {
            self.current_bytes.store(new, Ordering::Relaxed);
        }
    }
    fn stats(&self) -> ResizableStats {
        ResizableStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: 0,
        }
    }
}

/// Test RSS source — returns whatever `set_rss` was last given.
struct FakeRss {
    rss: AtomicU64,
}

impl FakeRss {
    fn new(initial: u64) -> Arc<Self> {
        Arc::new(Self {
            rss: AtomicU64::new(initial),
        })
    }
}

impl RssSource for FakeRss {
    fn read_rss_bytes(&self) -> u64 {
        self.rss.load(Ordering::Relaxed)
    }
}

/// Helper: build a budget driven by a [`FakeRss`] handle the test can
/// later mutate.
fn budget_with_target(target: u64, rss: Arc<FakeRss>) -> MemoryBudget {
    struct Forward(Arc<FakeRss>);
    impl RssSource for Forward {
        fn read_rss_bytes(&self) -> u64 {
            self.0.read_rss_bytes()
        }
    }
    MemoryBudget::with_rss(
        BudgetConfig::new(target).with_tick(Duration::from_millis(10)),
        Box::new(Forward(rss)),
    )
}

#[test]
fn shrink_under_over_target() {
    // Target 100 MiB, RSS 200 MiB → factor 0.5 → caps halved.
    let target = 100 * 1024 * 1024;
    let rss = FakeRss::new(2 * target);
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 64 * 1024 * 1024);
    let b = FakeCache::new("b", 32 * 1024 * 1024);
    a.set_current_bytes(64 * 1024 * 1024);
    b.set_current_bytes(32 * 1024 * 1024);
    let dyn_a: Arc<dyn Resizable> = a.clone();
    let dyn_b: Arc<dyn Resizable> = b.clone();
    budget.register(&dyn_a);
    budget.register(&dyn_b);

    let snap = budget.tick_now();
    assert!(snap.caches.iter().any(|c| c.name == "a"));
    // factor = target / rss = 0.5
    assert_eq!(a.max_bytes(), 32 * 1024 * 1024);
    assert_eq!(b.max_bytes(), 16 * 1024 * 1024);
}

#[test]
fn hold_inside_hysteresis_band() {
    // RSS = target → in band → caps unchanged.
    let target = 100 * 1024 * 1024;
    let rss = FakeRss::new(target);
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 64 * 1024 * 1024);
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    let before = a.max_bytes();
    budget.tick_now();
    assert_eq!(a.max_bytes(), before, "caps must hold in band");
}

#[test]
fn grow_under_target() {
    // RSS = 0.5 × target (below GROW_RATIO 0.7) → cap grows toward preferred.
    let target = 200 * 1024 * 1024;
    let rss = FakeRss::new(target / 2);
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 32 * 1024 * 1024);
    a.set_hits(900, 100); // hit_rate 0.9 → gets all the headroom
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    budget.tick_now();
    // Preferred ceiling was 32 MiB at registration; growth is capped there.
    assert_eq!(a.max_bytes(), 32 * 1024 * 1024);
}

#[test]
fn lease_reduces_effective_target() {
    let target = 100 * 1024 * 1024;
    let rss = FakeRss::new(target); // in band without lease
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 80 * 1024 * 1024);
    a.set_current_bytes(80 * 1024 * 1024);
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    // Without lease: cap held.
    budget.tick_now();
    assert_eq!(a.max_bytes(), 80 * 1024 * 1024);

    // With a 50 MiB lease, effective_target = 50 MiB, RSS = 100 MiB
    // → factor 0.5 → cap halved.
    let lease = budget.lease(50 * 1024 * 1024);
    assert_eq!(budget.effective_target_bytes(), 50 * 1024 * 1024);
    budget.tick_now();
    assert_eq!(a.max_bytes(), 40 * 1024 * 1024);

    // Dropping the lease restores the effective target.
    drop(lease);
    assert_eq!(budget.effective_target_bytes(), target);
}

#[test]
fn executor_spill_run_buffer_uses_effective_budget_slice() {
    let target = 64 * 1024 * 1024;
    let rss = FakeRss::new(target);
    let budget = budget_with_target(target, Arc::clone(&rss));

    assert_eq!(budget.executor_spill_run_buffer_bytes(), 4 * 1024 * 1024);

    let lease = budget.lease(32 * 1024 * 1024);
    assert_eq!(budget.executor_spill_run_buffer_bytes(), 2 * 1024 * 1024);

    drop(lease);
    assert_eq!(budget.executor_spill_run_buffer_bytes(), 4 * 1024 * 1024);
}

#[test]
fn skip_shrink_when_caches_lean_vs_rss() {
    // Target 100 MiB, RSS 1 GiB (10× over → would normally shrink hard).
    // But cache `current_bytes` sums to only 50 MiB / 1 GiB = 5% share,
    // far below MIN_CACHE_SHARE_FOR_SHRINK (0.25). The policy must
    // hold caps and emit a SkipShrinkLeanCaches verdict — shrinking
    // caches would not meaningfully reduce RSS (the bulk lives
    // outside the cache subsystem, e.g. in a transient reasoner /
    // parser scratch).
    use crate::policy::Verdict;

    let target = 100 * 1024 * 1024;
    let rss_value = 1024 * 1024 * 1024_u64; // 1 GiB
    let rss = FakeRss::new(rss_value);
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 80 * 1024 * 1024);
    a.set_current_bytes(50 * 1024 * 1024); // 50 MiB current vs 80 MiB cap
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    let before_cap = a.max_bytes();
    let snap = budget.tick_now();
    assert_eq!(
        a.max_bytes(),
        before_cap,
        "lean-cache shrink should hold caps, got {}",
        a.max_bytes()
    );
    match &snap.verdict {
        Verdict::SkipShrinkLeanCaches {
            cache_share,
            cache_bytes,
            rss_bytes,
        } => {
            assert!(
                *cache_share < 0.25,
                "expected lean cache share, got {cache_share}"
            );
            assert_eq!(*cache_bytes, 50 * 1024 * 1024);
            assert_eq!(*rss_bytes, rss_value);
        }
        other => panic!("expected SkipShrinkLeanCaches, got {other:?}"),
    }
}

#[test]
fn shrink_still_fires_when_caches_dominate_rss() {
    // Same RSS-over-target setup as `shrink_under_over_target` but
    // assert the snapshot carries a Shrink verdict (regression-fence
    // for the new verdict field).
    use crate::policy::Verdict;

    let target = 100 * 1024 * 1024;
    let rss = FakeRss::new(2 * target);
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 64 * 1024 * 1024);
    a.set_current_bytes(64 * 1024 * 1024);
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    let snap = budget.tick_now();
    match snap.verdict {
        Verdict::Shrink { factor } => {
            assert!(
                (factor - 0.5).abs() < 1e-6,
                "expected factor ≈ 0.5, got {factor}"
            );
        }
        other => panic!("expected Shrink, got {other:?}"),
    }
}

#[test]
fn skip_shrink_when_rss_grew_but_caches_flat() {
    // First tick primes history: RSS = 90 MiB (in band), caches at
    // 60 MiB current_bytes. Second tick simulates the reasoner-burn
    // pattern: RSS jumps to 200 MiB (2× target → shrink threshold)
    // but caches haven't grown — current_bytes still 60 MiB,
    // cache_count still 1. The policy must hold caps and emit
    // SkipShrinkFlatCaches.
    use crate::policy::Verdict;

    let target = 100 * 1024 * 1024;
    let rss = FakeRss::new(90 * 1024 * 1024); // in band → priming tick
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 80 * 1024 * 1024);
    a.set_current_bytes(60 * 1024 * 1024);
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    let _ = budget.tick_now(); // primes previous_rss / cache_bytes / count

    // Bump RSS but keep caches identical.
    rss.rss.store(200 * 1024 * 1024, Ordering::Relaxed);

    let before_cap = a.max_bytes();
    let snap = budget.tick_now();
    assert_eq!(
        a.max_bytes(),
        before_cap,
        "flat-cache shrink should hold caps, got {}",
        a.max_bytes()
    );
    match &snap.verdict {
        Verdict::SkipShrinkFlatCaches {
            cache_bytes,
            previous_cache_bytes,
            cache_count,
            previous_cache_count,
            rss_bytes,
        } => {
            assert_eq!(*cache_bytes, 60 * 1024 * 1024);
            assert_eq!(*previous_cache_bytes, 60 * 1024 * 1024);
            assert_eq!(*cache_count, 1);
            assert_eq!(*previous_cache_count, 1);
            assert_eq!(*rss_bytes, 200 * 1024 * 1024);
        }
        other => panic!("expected SkipShrinkFlatCaches, got {other:?}"),
    }
}

#[test]
fn shrink_still_fires_when_caches_grew_alongside_rss() {
    // First tick: RSS in band, caches at 60 MiB. Second tick: RSS
    // jumped to 200 MiB AND caches grew to 80 MiB (33% > 5%
    // tolerance). The flat-caches guard must NOT fire — caches
    // clearly contributed to the climb, so the regular shrink path
    // takes over.
    use crate::policy::Verdict;

    let target = 100 * 1024 * 1024;
    let rss = FakeRss::new(90 * 1024 * 1024); // priming tick in band
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 80 * 1024 * 1024);
    a.set_current_bytes(60 * 1024 * 1024);
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    let _ = budget.tick_now(); // priming

    rss.rss.store(200 * 1024 * 1024, Ordering::Relaxed);
    a.set_current_bytes(80 * 1024 * 1024); // caches grew 33%

    let snap = budget.tick_now();
    match snap.verdict {
        Verdict::Shrink { factor } => {
            assert!(
                (factor - 0.5).abs() < 1e-6,
                "expected factor ≈ 0.5, got {factor}"
            );
        }
        other => panic!("expected Shrink, got {other:?}"),
    }
}

#[test]
fn non_cache_bucket_equals_rss_minus_cache_and_lease() {
    // RSS = 2 GiB, cache current_bytes = 200 MiB, lease = 300 MiB
    // → non_cache_bytes = 2048 - 200 - 300 = 1548 MiB.
    // This is the bucket that absorbs reasoner / parser / non-cache
    // allocations and is not addressable by cache resizing.
    let target = 4 * 1024 * 1024 * 1024_u64; // 4 GiB target → in band
    let rss_value = 2 * 1024 * 1024 * 1024_u64; // 2 GiB
    let rss = FakeRss::new(rss_value);
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 512 * 1024 * 1024);
    a.set_current_bytes(200 * 1024 * 1024);
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    let _lease = budget.lease(300 * 1024 * 1024);
    let snap = budget.tick_now();
    assert_eq!(snap.cache_bytes_current(), 200 * 1024 * 1024);
    assert_eq!(snap.lease_bytes_active, 300 * 1024 * 1024);
    assert_eq!(snap.non_cache_bytes(), 1548 * 1024 * 1024);
}

/// Stub stats source for tests: returns the [`JemallocStats`] handed
/// to it at construction time, every tick.
struct FixedJemalloc(JemallocStats);

impl JemallocStatsSource for FixedJemalloc {
    fn read(&self) -> JemallocStats {
        self.0
    }
}

#[test]
fn non_cache_live_bytes_subtracts_cache_and_lease_from_allocated() {
    // sysinfo RSS 2 GiB, cache_bytes 200 MiB, lease 300 MiB, jemalloc
    // says allocated = 1 GiB, resident = 1900 MiB, retained = 900 MiB.
    //
    // Because jemalloc stats are present, the policy/snapshot RSS is
    // je_resident (1900 MiB), NOT the sysinfo 2048 MiB:
    //   non_cache_bytes      = 1900 - 200 - 300 = 1400 MiB
    //   non_cache_live_bytes = 1024 - 200 - 300 =  524 MiB
    // The sysinfo reading is preserved separately in rss_sysinfo_bytes.
    let target = 4 * 1024 * 1024 * 1024_u64;
    let rss = FakeRss::new(2 * 1024 * 1024 * 1024_u64);
    let budget = budget_with_target(target, Arc::clone(&rss)).with_jemalloc_stats(Box::new(
        FixedJemalloc(JemallocStats {
            allocated_bytes: 1024 * 1024 * 1024,
            active_bytes: 1100 * 1024 * 1024,
            metadata_bytes: 64 * 1024 * 1024,
            resident_bytes: 1900 * 1024 * 1024,
            mapped_bytes: 2200 * 1024 * 1024,
            retained_bytes: 900 * 1024 * 1024,
        }),
    ));

    let a = FakeCache::new("a", 512 * 1024 * 1024);
    a.set_current_bytes(200 * 1024 * 1024);
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    let _lease = budget.lease(300 * 1024 * 1024);
    let snap = budget.tick_now();
    // Effective RSS follows je_resident; sysinfo kept for the gap.
    assert_eq!(snap.rss_bytes, 1900 * 1024 * 1024);
    assert_eq!(snap.rss_sysinfo_bytes, 2 * 1024 * 1024 * 1024);
    assert_eq!(snap.non_cache_bytes(), 1400 * 1024 * 1024);
    assert_eq!(snap.non_cache_live_bytes(), 524 * 1024 * 1024);
    assert_eq!(snap.jemalloc.retained_bytes, 900 * 1024 * 1024);
}

#[test]
fn reporter_attributes_a_slice_of_non_cache_live() {
    // je_allocated = 10 GiB, no caches, no lease → non_cache_live =
    // 10 GiB. A registered reporter claims 6 GiB → other = 4 GiB.
    let target = 16 * 1024 * 1024 * 1024_u64;
    let rss = FakeRss::new(8 * 1024 * 1024 * 1024_u64);
    let budget = budget_with_target(target, Arc::clone(&rss)).with_jemalloc_stats(Box::new(
        FixedJemalloc(JemallocStats {
            allocated_bytes: 10 * 1024 * 1024 * 1024,
            active_bytes: 10 * 1024 * 1024 * 1024,
            metadata_bytes: 0,
            resident_bytes: 10 * 1024 * 1024 * 1024,
            mapped_bytes: 10 * 1024 * 1024 * 1024,
            retained_bytes: 0,
        }),
    ));

    let reporter = Arc::new(AtomicReporter::new("reasoner"));
    let gauge = reporter.gauge();
    let dyn_reporter: Arc<dyn NonCacheReporter> = reporter.clone();
    budget.register_reporter(&dyn_reporter);
    gauge.set(6 * 1024 * 1024 * 1024);

    let snap = budget.tick_now();
    assert_eq!(snap.reporters.len(), 1);
    assert_eq!(snap.reporter_bytes(), 6 * 1024 * 1024 * 1024);
    assert_eq!(snap.non_cache_live_bytes(), 10 * 1024 * 1024 * 1024);
    assert_eq!(snap.non_cache_other_bytes(), 4 * 1024 * 1024 * 1024);
}

#[test]
fn dropped_reporter_prunes_from_next_tick() {
    let target = 4 * 1024 * 1024 * 1024_u64;
    let rss = FakeRss::new(target);
    let budget = budget_with_target(target, Arc::clone(&rss));
    let reporter = Arc::new(AtomicReporter::new("reasoner"));
    let dyn_reporter: Arc<dyn NonCacheReporter> = reporter.clone();
    budget.register_reporter(&dyn_reporter);
    assert_eq!(budget.tick_now().reporters.len(), 1);
    drop(reporter);
    drop(dyn_reporter);
    // Weak registration prunes once the strong refs are gone.
    assert_eq!(budget.tick_now().reporters.len(), 0);
}

#[test]
fn rss_falls_back_to_sysinfo_without_jemalloc() {
    // No jemalloc source → resident reads 0 → effective RSS is the
    // sysinfo reading, and rss_bytes == rss_sysinfo_bytes.
    let target = 1024 * 1024 * 1024;
    let rss = FakeRss::new(512 * 1024 * 1024);
    let budget = budget_with_target(target, Arc::clone(&rss));
    let snap = budget.tick_now();
    assert_eq!(snap.rss_bytes, 512 * 1024 * 1024);
    assert_eq!(snap.rss_sysinfo_bytes, 512 * 1024 * 1024);
}

#[test]
fn non_cache_live_bytes_zero_without_jemalloc_source() {
    // Default is NoJemallocStats → allocated_bytes=0 → live=0 even
    // though non_cache_bytes is nonzero. Documents the fallback.
    let target = 1024 * 1024 * 1024;
    let rss = FakeRss::new(512 * 1024 * 1024);
    let budget = budget_with_target(target, Arc::clone(&rss));
    let snap = budget.tick_now();
    assert_eq!(snap.jemalloc.allocated_bytes, 0);
    assert_eq!(snap.non_cache_live_bytes(), 0);
    assert_eq!(snap.non_cache_bytes(), 512 * 1024 * 1024);
}

#[test]
fn non_cache_bucket_saturates_when_caches_exceed_rss() {
    // Degenerate case: cache_bytes_current > rss (can happen mid-tick
    // if the RSS sample races a cache insert). Must saturate at zero
    // instead of underflowing.
    let target = 100 * 1024 * 1024;
    let rss = FakeRss::new(50 * 1024 * 1024);
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 80 * 1024 * 1024);
    a.set_current_bytes(80 * 1024 * 1024); // > rss
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    let snap = budget.tick_now();
    assert_eq!(
        snap.non_cache_bytes(),
        0,
        "non_cache_bytes must saturate, got {}",
        snap.non_cache_bytes()
    );
}

#[test]
fn verdict_streak_increments_and_resets_on_label_change() {
    use crate::policy::Verdict;

    let target = 100 * 1024 * 1024;
    let rss = FakeRss::new(target); // in band → hold
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 128 * 1024 * 1024);
    a.set_current_bytes(50 * 1024 * 1024);
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    // Two ticks of hold to grow a streak.
    let s1 = budget.tick_now();
    assert!(matches!(s1.verdict, Verdict::Hold));
    assert_eq!(s1.verdict_streak, 1);
    assert!(s1.previous_verdict_label.is_none());

    let s2 = budget.tick_now();
    assert!(matches!(s2.verdict, Verdict::Hold));
    assert_eq!(s2.verdict_streak, 2);
    assert!(s2.previous_verdict_label.is_none());

    // Push into shrink. Grow caches alongside RSS so the trend
    // guard doesn't intercept (50 → 150 MiB clears the 5%
    // tolerance; 150 MiB vs. 200 MiB RSS clears the 25%
    // lean-cache threshold).
    rss.rss.store(2 * target, Ordering::Relaxed);
    a.set_current_bytes(150 * 1024 * 1024);
    let s3 = budget.tick_now();
    assert!(
        matches!(s3.verdict, Verdict::Shrink { .. }),
        "expected Shrink, got {:?}",
        s3.verdict
    );
    assert_eq!(s3.verdict_streak, 1, "streak resets on label change");
    assert_eq!(s3.previous_verdict_label, Some("hold"));
    assert_eq!(s3.previous_verdict_streak, Some(2));

    // Same shrink next tick — streak counts again, boundary fields clear.
    let s4 = budget.tick_now();
    assert!(matches!(s4.verdict, Verdict::Shrink { .. }));
    assert_eq!(s4.verdict_streak, 2);
    assert!(s4.previous_verdict_label.is_none());
    assert!(s4.previous_verdict_streak.is_none());
}

#[test]
fn cache_snapshot_delta_bytes_signed() {
    // Shrink fires when caches dominate RSS. The per-cache snapshot's
    // delta_bytes must be negative (cap went down). Hold paths
    // produce a zero delta.
    let target = 100 * 1024 * 1024;
    let rss = FakeRss::new(2 * target); // shrink threshold
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 64 * 1024 * 1024);
    a.set_current_bytes(64 * 1024 * 1024);
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);

    let snap = budget.tick_now();
    let cache = snap
        .caches
        .iter()
        .find(|c| c.name == "a")
        .expect("cache snapshot present");
    let delta = cache.delta_bytes();
    assert!(delta < 0, "expected negative delta on shrink, got {delta}");
    // factor = 0.5 → 64 MiB → 32 MiB → delta = -32 MiB.
    assert_eq!(delta, -(32 * 1024 * 1024));

    // Next tick: rss back in band, caches stayed flat → hold → delta 0.
    rss.rss.store(target, Ordering::Relaxed);
    a.set_current_bytes(32 * 1024 * 1024);
    let snap = budget.tick_now();
    let cache = snap.caches.iter().find(|c| c.name == "a").unwrap();
    assert_eq!(cache.delta_bytes(), 0, "hold should produce zero delta");
}

#[test]
fn registration_survives_weak_drop() {
    // Caches registered through Arc<dyn Resizable> are held weakly;
    // dropping the last strong handle silently removes the entry.
    let target = 100 * 1024 * 1024;
    let rss = FakeRss::new(target);
    let budget = budget_with_target(target, Arc::clone(&rss));

    let a = FakeCache::new("a", 16 * 1024 * 1024);
    let dyn_a: Arc<dyn Resizable> = a.clone();
    budget.register(&dyn_a);
    drop(a);
    drop(dyn_a);

    let snap = budget.tick_now();
    assert!(snap.caches.is_empty(), "dead caches should be pruned");
}
