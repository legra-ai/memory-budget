//! Contract every cache that wants to participate in the
//! [`MemoryBudget`](crate::MemoryBudget) reapportionment must satisfy.

/// Minimal interface a memory-budget-aware cache must expose.
///
/// All methods are `&self` because caches use interior mutability (a
/// `Mutex<LruCache<...>>` or a `moka::sync::Cache`). The budget holds
/// `Weak<dyn Resizable>` handles so a dropped cache is silently
/// removed from the next tick.
pub trait Resizable: Send + Sync {
    /// Human-readable label for log lines and metrics.
    fn name(&self) -> &str;

    /// Current bytes held by the cache.
    fn current_bytes(&self) -> u64;

    /// Configured soft cap. Honoured eagerly (eviction-on-insert) and
    /// at each tick by [`Resizable::set_max_bytes`].
    fn max_bytes(&self) -> u64;

    /// Update the soft cap. The cache may either evict synchronously
    /// (small caches) or schedule eviction over the next few ops
    /// (large caches). Either is acceptable as long as
    /// [`Resizable::current_bytes`] converges to `new` within one
    /// budget tick.
    fn set_max_bytes(&self, new: u64);

    /// Stats sample used by the reapportionment policy.
    fn stats(&self) -> ResizableStats;
}

/// Point-in-time stats sample from a registered cache.
///
/// All counters are monotonic-since-construction. The policy compares
/// the delta between consecutive snapshots, so the absolute values
/// only need to be consistent within a single sample.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResizableStats {
    /// Successful lookups served from the cache.
    pub hits: u64,
    /// Lookups that missed and required falling back to the inner store.
    pub misses: u64,
    /// Total evictions over the cache's lifetime.
    pub evictions: u64,
}

impl ResizableStats {
    /// Fraction of `hits + misses` served from the cache. `None`
    /// when no lookups have occurred (avoids division by zero and a
    /// misleading 0.0 reading on idle caches).
    #[must_use]
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits.checked_add(self.misses)?;
        if total == 0 {
            return None;
        }
        Some(self.hits as f64 / total as f64)
    }
}
