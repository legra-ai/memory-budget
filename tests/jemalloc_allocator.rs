//! Exercise the native allocator/control pair used by real consumers.

#![cfg(all(unix, feature = "jemalloc"))]

use memory_budget::{JemallocStatsSource, SystemJemallocStats};

#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[test]
fn statistics_observe_live_allocations_from_the_linked_allocator() {
    let source = SystemJemallocStats::new().expect("linked jemalloc must expose statistics");
    // bounded: one fixed 1 MiB allocation proves the native allocator contract.
    let mut live = vec![42_u8; 1024 * 1024];
    std::hint::black_box(&mut live);
    let stats = source.read();
    assert!(stats.allocated_bytes >= live.len() as u64);
}
