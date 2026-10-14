//! Published API regression: independent callers share finite capacity.

use memory_budget::{BudgetConfig, MemoryBudget};

#[test]
fn a_second_caller_cannot_reserve_the_first_callers_capacity() {
    let budget = MemoryBudget::new(BudgetConfig::new(64));
    let first = budget.try_lease(48).expect("first caller fits");
    let refused = budget
        .try_lease(17)
        .err()
        .expect("second caller is refused");
    assert_eq!(refused.requested_bytes(), 17);
    assert_eq!(refused.reserved_bytes(), 48);
    assert_eq!(refused.capacity_bytes(), 64);
    drop(first);
    assert!(budget.try_lease(64).is_ok());
}
