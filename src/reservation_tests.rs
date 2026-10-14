//! Aggregate reservations must refuse before callers allocate any payload.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use crate::{BudgetConfig, MemoryBudget};

#[test]
fn reservations_cannot_exceed_the_process_target() {
    let budget = MemoryBudget::new(BudgetConfig::new(64));
    let first = budget.lease(48);
    let refused = catch_unwind(AssertUnwindSafe(|| budget.lease(17)));
    assert!(
        refused.is_err(),
        "aggregate overcommit must fail before allocation"
    );
    assert_eq!(budget.effective_target_bytes(), 16);
    drop(first);
    assert_eq!(budget.effective_target_bytes(), 64);
}

#[test]
fn an_oversized_reservation_cannot_wrap_the_accounting_counter() {
    let budget = MemoryBudget::new(BudgetConfig::new(64));
    let first = budget.lease(1);
    let refused = catch_unwind(AssertUnwindSafe(|| budget.lease(u64::MAX)));
    assert!(
        refused.is_err(),
        "oversized reservation must fail before addition"
    );
    assert_eq!(budget.effective_target_bytes(), 63);
    drop(first);
    assert_eq!(budget.effective_target_bytes(), 64);
}

#[test]
fn reservation_owns_its_accounting_until_release() {
    let budget = Arc::new(MemoryBudget::new(BudgetConfig::new(64)));
    let owner = Arc::downgrade(&budget);
    let reservation = budget.lease(32);
    drop(budget);
    // The public handle can disappear, but the reservation must retain the
    // internal accounting rather than silently skipping release through Weak.
    assert!(owner.upgrade().is_none());
    assert!(reservation.accounting_is_live());
}

#[test]
fn a_zero_capacity_budget_is_an_invalid_configuration() {
    assert!(catch_unwind(|| BudgetConfig::new(0)).is_err());
}
