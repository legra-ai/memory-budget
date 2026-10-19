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

## Allocation reservations

Call `try_lease(bytes)` **before** allocating a payload. Outstanding leases
share one finite allowance equal to `target_bytes`; an oversized or competing
request returns `ReservationError` without changing the counter or allocating
a payload. Transfer the guard with ownership and drop it only after the owned
allocation is released. `lease(bytes)` uses the same accounting and panics on
refusal. It no longer permits overcommit. Zero-capacity budgets are invalid.

```rust
use memory_budget::{BudgetConfig, MemoryBudget};

let budget = MemoryBudget::new(BudgetConfig::new(1024));
let first = budget.try_lease(768).expect("first allocation fits");
assert!(budget.try_lease(257).is_err());
drop(first);
assert!(budget.try_lease(1024).is_ok());
```

This enforces the sum of reservations, **not a whole-process allocation cap**.
Unregistered allocations remain outside admission, and cache ceilings shrink
on coordination ticks rather than synchronously at reservation time. The
application must govern baseline, caches, copies, decoder expansion and native
allocations before claiming a process memory bound. The optional abort ceiling
remains a reactive safety mechanism.

`observed_usage_bytes()` samples memory without a coordination tick. It and
the tick use the maximum of OS process usage, jemalloc residency and live heap
allocation. These overlapping measurements must not be added, and a smaller
allocator reading must not hide other process memory.

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

These features are supported on Linux and macOS. Windows uses the default
system-memory backend. CI tests default behavior on all three platforms and
allocator features on their supported platforms, including a real allocator
integration test. Keep the allocator and control crate requirements aligned
when updating the native jemalloc family.

The minimum supported Rust version is 1.97.1. Main CI is the sole publisher;
it validates the checkout and extracted archive before publication, then tests
the actual public crate. Shared pipeline references are immutable and updated
together by Dependabot.

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT License

at your option.
