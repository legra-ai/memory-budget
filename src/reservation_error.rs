//! Typed refusal before a caller acquires allocation ownership.

use std::fmt;

/// An allocation reservation would exceed the aggregate lease allowance.
///
/// Refusal does not change accounting. Callers must refuse, spill, or retry
/// without allocating the payload they could not reserve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReservationError {
    requested_bytes: u64,
    reserved_bytes: u64,
    capacity_bytes: u64,
}

impl ReservationError {
    pub(crate) const fn new(requested: u64, reserved: u64, capacity: u64) -> Self {
        Self {
            requested_bytes: requested,
            reserved_bytes: reserved,
            capacity_bytes: capacity,
        }
    }

    /// Bytes the caller attempted to reserve.
    #[must_use]
    pub const fn requested_bytes(self) -> u64 {
        self.requested_bytes
    }

    /// Outstanding bytes observed when admission was refused.
    #[must_use]
    pub const fn reserved_bytes(self) -> u64 {
        self.reserved_bytes
    }

    /// Maximum aggregate reservation capacity.
    #[must_use]
    pub const fn capacity_bytes(self) -> u64 {
        self.capacity_bytes
    }
}

impl fmt::Display for ReservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "memory reservation refused: requested {} bytes with {} of {} bytes reserved",
            self.requested_bytes, self.reserved_bytes, self.capacity_bytes,
        )
    }
}

impl std::error::Error for ReservationError {}
