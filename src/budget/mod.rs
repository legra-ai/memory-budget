//! Process-wide memory budget coordinator.

mod memory;
mod snapshot;
mod state;

pub use memory::MemoryBudget;
pub use snapshot::{BudgetSnapshot, CacheSnapshot, ReporterSnapshot};
pub(crate) use state::BudgetState;
