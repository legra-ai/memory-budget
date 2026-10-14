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

mod cache_policy;
mod diagnostics;
