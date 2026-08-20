//! Read-only attribution of non-cache memory to named subsystems.
//!
//! The budget tick's `non_cache_live_bytes` (jemalloc-allocated minus
//! registered caches and leases) is the program's live heap outside
//! the resizable caches — typically dominated by the reasoner working
//! set. It is one opaque number. A [`NonCacheReporter`] lets a
//! subsystem publish "how many bytes am I holding right now" so the
//! tick can *name* slices of that bucket and leave the rest as a
//! `non_cache_other` residual.
//!
//! Unlike [`Resizable`](crate::Resizable), reporters are **read-only**:
//! the budget observes but cannot shrink them (the reasoner's working
//! set isn't evictable mid-pass). The budget holds `Weak` handles, so
//! a dropped reporter prunes itself from the next tick.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A subsystem that reports its current non-cache memory footprint.
pub trait NonCacheReporter: Send + Sync {
    /// Short label surfaced on the budget tick (e.g. `reasoner`).
    fn name(&self) -> &str;

    /// Bytes this subsystem is currently holding. Best-effort — an
    /// estimate is fine; the budget uses it for attribution, not
    /// eviction.
    fn current_bytes(&self) -> u64;
}

/// A [`NonCacheReporter`] backed by a shared atomic gauge.
///
/// The budget registers the `AtomicReporter`; the producing subsystem
/// holds a [`MemoryGauge`] clone (via [`AtomicReporter::gauge`]) and
/// updates it as its footprint changes. Construct one per subsystem
/// instance (e.g. a parser or reasoner), register it, and thread
/// the gauge into the subsystem.
pub struct AtomicReporter {
    name: String,
    gauge: Arc<AtomicU64>,
}

impl AtomicReporter {
    /// Create a reporter with a zeroed gauge.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            gauge: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Producer-side handle used to update the reported byte count.
    #[must_use]
    pub fn gauge(&self) -> MemoryGauge {
        MemoryGauge {
            inner: Arc::clone(&self.gauge),
        }
    }
}

impl NonCacheReporter for AtomicReporter {
    fn name(&self) -> &str {
        &self.name
    }

    fn current_bytes(&self) -> u64 {
        self.gauge.load(Ordering::Relaxed)
    }
}

/// Producer-side handle to an [`AtomicReporter`]'s gauge.
///
/// Cheap to clone (one `Arc`). Threaded into a subsystem so it can
/// publish its live footprint. A [`MemoryGauge::disconnected`] gauge
/// writes to a private atomic nobody reads — handy so subsystem code
/// always has a gauge to write to even when not running under a
/// registered budget (tests or applications without a budget).
#[derive(Clone, Debug)]
pub struct MemoryGauge {
    inner: Arc<AtomicU64>,
}

impl MemoryGauge {
    /// A gauge not wired to any reporter; updates go nowhere.
    #[must_use]
    pub fn disconnected() -> Self {
        Self {
            inner: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Replace the reported byte count.
    pub fn set(&self, bytes: u64) {
        self.inner.store(bytes, Ordering::Relaxed);
    }

    /// Add to the reported byte count.
    pub fn add(&self, bytes: u64) {
        self.inner.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Subtract from the reported byte count, saturating at zero.
    pub fn sub(&self, bytes: u64) {
        let mut current = self.inner.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(bytes);
            match self.inner.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Current reported byte count.
    #[must_use]
    pub fn current(&self) -> u64 {
        self.inner.load(Ordering::Relaxed)
    }

    /// RAII attribution: add `bytes` to the gauge now and subtract the
    /// same amount when the returned [`GaugeGuard`] drops. Hold one
    /// alongside a resident allocation to attribute it to this gauge's
    /// reporter for exactly that allocation's lifetime. Uses `add`/`sub`
    /// so concurrent guards on the same reporter compose into one total.
    #[must_use]
    pub fn guard(&self, bytes: u64) -> GaugeGuard {
        self.add(bytes);
        GaugeGuard {
            gauge: self.clone(),
            bytes,
        }
    }
}

/// RAII guard that publishes a fixed byte count to a [`MemoryGauge`]
/// for its lifetime. Created by [`MemoryGauge::guard`]; `Drop` removes
/// the contribution again.
#[derive(Debug)]
pub struct GaugeGuard {
    gauge: MemoryGauge,
    bytes: u64,
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        self.gauge.sub(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{AtomicReporter, NonCacheReporter};

    #[test]
    fn gauge_updates_are_visible_through_reporter() {
        let reporter = AtomicReporter::new("reasoner");
        let gauge = reporter.gauge();
        assert_eq!(reporter.current_bytes(), 0);
        gauge.add(1000);
        assert_eq!(reporter.current_bytes(), 1000);
        gauge.set(4096);
        assert_eq!(reporter.current_bytes(), 4096);
        gauge.sub(100);
        assert_eq!(reporter.current_bytes(), 3996);
        // Saturating subtraction never underflows.
        gauge.sub(u64::MAX);
        assert_eq!(reporter.current_bytes(), 0);
    }

    #[test]
    fn gauge_guard_adds_then_subtracts_on_drop() {
        let reporter = AtomicReporter::new("executor");
        let gauge = reporter.gauge();
        {
            let _g = gauge.guard(1024);
            assert_eq!(reporter.current_bytes(), 1024);
            // Concurrent guards on the same reporter compose additively.
            let _g2 = gauge.guard(512);
            assert_eq!(reporter.current_bytes(), 1536);
        }
        assert_eq!(reporter.current_bytes(), 0);
    }

    #[test]
    fn reporter_name_is_exposed() {
        let reporter = AtomicReporter::new("reasoner");
        let dyn_reporter: Arc<dyn NonCacheReporter> = Arc::new(reporter);
        assert_eq!(dyn_reporter.name(), "reasoner");
    }
}
