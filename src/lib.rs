#![doc = include_str!("../README.md")]

mod budget;
mod config;
#[cfg(feature = "heap-profiling")]
mod heap_profile;
mod jemalloc;
mod lease;
pub mod policy;
mod reporter;
mod reservation_error;
mod resizable;
mod rss;

pub use budget::{BudgetSnapshot, CacheSnapshot, MemoryBudget, ReporterSnapshot};
pub use config::BudgetConfig;
#[cfg(feature = "jemalloc")]
pub use jemalloc::SystemJemallocStats;
pub use jemalloc::{JemallocStats, JemallocStatsSource, NoJemallocStats};
pub use lease::LeaseGuard;
pub use reporter::{AtomicReporter, GaugeGuard, MemoryGauge, NonCacheReporter};
pub use reservation_error::ReservationError;
pub use resizable::{Resizable, ResizableStats};
pub use rss::{RssSource, SystemRss};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod reservation_tests;
