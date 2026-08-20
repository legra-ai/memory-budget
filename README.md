# memory-budget

[![Crates.io](https://img.shields.io/crates/v/memory-budget.svg)](https://crates.io/crates/memory-budget)
[![Documentation](https://docs.rs/memory-budget/badge.svg)](https://docs.rs/memory-budget)
[![CI](https://github.com/legra-ai/memory-budget/actions/workflows/ci.yml/badge.svg)](https://github.com/legra-ai/memory-budget/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/legra-ai/memory-budget#license)

Adaptive process-memory budgeting for applications with bounded caches.

`memory-budget` coordinates caches and other memory reporters under a target
resident-set size. It samples process RSS, applies a pluggable policy, and
adjusts registered cache ceilings. A Tokio task can drive the periodic ticks;
the policy itself is synchronous and deterministic, which keeps it easy to
test.

## Core model

- `Resizable` is the small contract implemented by an adjustable cache.
- `NonCacheReporter` accounts for memory that cannot be resized by the budget.
- `MemoryBudget` owns weak registrations, leases, RSS sampling, and ticks.
- `Policy` decides new cache ceilings from the latest bounded snapshot.
- `RssSource` and `JemallocStatsSource` isolate platform and allocator data.

The budget does not own cache entries, queue work, or payloads. It only
coordinates the explicit byte measurements supplied by its participants.

## Example

```rust
use std::sync::Arc;

use memory_budget::{
    BudgetConfig,
    MemoryBudget,
    Resizable,
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let budget = Arc::new(MemoryBudget::new(BudgetConfig::new(512 * 1024 * 1024)));
    let cache: Arc<dyn Resizable> = Arc::new(MyCache::default());
    budget.register(&cache);
    let _tick = budget.spawn();
}

# #[derive(Default)]
# struct MyCache;
# impl Resizable for MyCache {
#     fn name(&self) -> &str { "example" }
#     fn current_bytes(&self) -> u64 { 0 }
#     fn max_bytes(&self) -> u64 { 0 }
#     fn set_max_bytes(&self, _new: u64) {}
#     fn stats(&self) -> memory_budget::ResizableStats { memory_budget::ResizableStats::default() }
# }
```

## Environment configuration

`BudgetConfig::from_env` reads these variables:

| Variable | Meaning |
| --- | --- |
| `MEMORY_BUDGET_TARGET_MIB` | Soft RSS target; otherwise derived from system RAM |
| `MEMORY_BUDGET_TICK_SECS` | Tick interval, default `5` seconds |
| `MEMORY_BUDGET_HARD_CEILING_MIB` | Optional hard process ceiling |

Use `BudgetConfig::new` when configuration must be supplied explicitly.

## Optional jemalloc support

Enable the `jemalloc` feature to expose `SystemJemallocStats` backed by
`tikv-jemalloc-ctl`. The `heap-profiling` feature adds threshold-driven
`prof.dump` diagnostics for applications built with jemalloc profiling.

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT License

at your option.
