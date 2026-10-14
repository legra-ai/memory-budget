//! Cache coordination regressions.

use super::*;

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
