//! Whole-process accounting cannot discard memory outside jemalloc.

use crate::{BudgetConfig, JemallocStats, JemallocStatsSource, MemoryBudget, RssSource};

struct FixedRss;

impl RssSource for FixedRss {
    fn read_rss_bytes(&self) -> u64 {
        80
    }
}

struct FixedAllocator;

impl JemallocStatsSource for FixedAllocator {
    fn read(&self) -> JemallocStats {
        JemallocStats {
            resident_bytes: 16,
            ..JemallocStats::default()
        }
    }
}

#[test]
fn allocator_stats_cannot_hide_larger_os_process_usage() {
    let budget = MemoryBudget::with_rss(BudgetConfig::new(100), Box::new(FixedRss))
        .with_jemalloc_stats(Box::new(FixedAllocator));
    assert_eq!(budget.tick_now().rss_bytes, 80);
    assert_eq!(budget.observed_usage_bytes(), 80);
}

#[test]
fn compressed_or_nonresident_live_allocations_remain_visible() {
    let stats = JemallocStats {
        allocated_bytes: 100,
        resident_bytes: 40,
        ..JemallocStats::default()
    };
    assert_eq!(stats.observed_usage_bytes(20), 100);
    assert_eq!(stats.observed_usage_bytes(200), 200);
}
