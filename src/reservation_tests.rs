//! Aggregate reservations must refuse before callers allocate any payload.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

use crate::{BudgetConfig, MemoryBudget, RssSource};

struct LifetimeRss(Arc<AtomicBool>);

impl RssSource for LifetimeRss {
    fn read_rss_bytes(&self) -> u64 {
        0
    }
}

impl Drop for LifetimeRss {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

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
    let dropped = Arc::new(AtomicBool::new(false));
    let budget = MemoryBudget::with_rss(
        BudgetConfig::new(64),
        Box::new(LifetimeRss(Arc::clone(&dropped))),
    );
    let reservation = budget.lease(32);
    drop(budget);
    assert!(!dropped.load(Ordering::Relaxed));
    drop(reservation);
    assert!(dropped.load(Ordering::Relaxed));
}

#[test]
fn a_zero_capacity_budget_is_an_invalid_configuration() {
    assert!(catch_unwind(|| BudgetConfig::new(0)).is_err());
}

#[test]
fn concurrent_callers_share_one_finite_allowance() {
    let budget = Arc::new(MemoryBudget::new(BudgetConfig::new(64)));
    let admitted = Arc::new(Barrier::new(9));
    let released = Arc::new(Barrier::new(9));
    std::thread::scope(|scope| {
        // bounded: eight fixed competing callers; no payload allocations.
        let mut workers = Vec::with_capacity(8);
        for _ in 0..8 {
            let budget = Arc::clone(&budget);
            let admitted = Arc::clone(&admitted);
            let released = Arc::clone(&released);
            workers.push(scope.spawn(move || {
                let reservation = budget.try_lease(32);
                admitted.wait();
                released.wait();
                reservation.is_ok()
            }));
        }
        admitted.wait();
        assert_eq!(budget.effective_target_bytes(), 0);
        released.wait();
        let successes = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().expect("reservation worker")))
            .sum::<usize>();
        assert_eq!(successes, 2);
    });
    assert_eq!(budget.effective_target_bytes(), 64);
}

#[test]
fn typed_refusal_preserves_capacity_and_drop_allows_retry() {
    let budget = MemoryBudget::new(BudgetConfig::new(64));
    let reservation = budget.try_lease(48).expect("first fits");
    let error = budget.try_lease(17).err().expect("aggregate refusal");
    assert_eq!(error.requested_bytes(), 17);
    assert_eq!(error.reserved_bytes(), 48);
    assert_eq!(error.capacity_bytes(), 64);
    assert_eq!(budget.effective_target_bytes(), 16);
    drop(reservation);
    let full = budget.try_lease(64).expect("release restores admission");
    assert_eq!(budget.effective_target_bytes(), 0);
    drop(full);
    assert_eq!(budget.effective_target_bytes(), 64);
}

#[test]
fn unwinding_work_releases_its_reservation() {
    let budget = MemoryBudget::new(BudgetConfig::new(64));
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _reservation = budget.lease(64);
        panic!("simulated allocating task failure");
    }));
    assert!(result.is_err());
    assert_eq!(budget.effective_target_bytes(), 64);
}
