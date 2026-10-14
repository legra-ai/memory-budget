//! Whole-process and non-cache accounting regressions.

use super::*;

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
    // The larger whole-process reading remains visible:
    //   non_cache_bytes      = 2048 - 200 - 300 = 1548 MiB
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
    // Effective usage includes allocations outside jemalloc.
    assert_eq!(snap.rss_bytes, 2048 * 1024 * 1024);
    assert_eq!(snap.rss_sysinfo_bytes, 2 * 1024 * 1024 * 1024);
    assert_eq!(snap.non_cache_bytes(), 1548 * 1024 * 1024);
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
