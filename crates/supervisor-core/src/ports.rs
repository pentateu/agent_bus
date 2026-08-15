//! The port allocator (C3).
//!
//! Pure integer math over a range with a reserved set (the supervisor's own
//! API + workspace ports, 4198 and 4199 by default, which are never handed to
//! a project). `alloc` always returns the lowest free port in range that is
//! neither reserved nor already handed out.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// The default supervisor API port; never allocated to a project.
pub const DEFAULT_API_PORT: u16 = 4198;
/// The default supervisor-workspace port; never allocated to a project.
pub const DEFAULT_SUPERVISOR_PORT: u16 = 4199;
/// The reserved set shipped by default.
pub const DEFAULT_RESERVED_PORTS: [u16; 2] = [DEFAULT_API_PORT, DEFAULT_SUPERVISOR_PORT];
/// The default allocator range (`[supervisor] port_range`).
pub const DEFAULT_PORT_RANGE: RangeInclusive<u16> = 4100..=4299;

/// A failed allocator operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortError {
    /// `reserve` was asked for a port that is already allocated or reserved.
    AlreadyUsed(u16),
    /// `alloc` was asked to work with an invalid range or a reserved port is
    /// outside the range.
    InvalidRange(u16),
    /// The range is fully exhausted.
    Exhausted,
}

/// Allocates project ports from a range, never touching the reserved set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortAllocator {
    range: RangeInclusive<u16>,
    reserved: BTreeSet<u16>,
    used: BTreeSet<u16>,
}

impl Default for PortAllocator {
    fn default() -> Self {
        Self::default_allocator()
    }
}

impl PortAllocator {
    /// Build an allocator over `range`, excluding every port in `reserved`.
    #[must_use]
    pub fn new(range: RangeInclusive<u16>, reserved: impl IntoIterator<Item = u16>) -> Self {
        Self { range, reserved: reserved.into_iter().collect(), used: BTreeSet::new() }
    }

    /// The default allocator: the default range minus the reserved supervisor
    /// ports (4198, 4199).
    #[must_use]
    pub fn default_allocator() -> Self {
        Self::new(DEFAULT_PORT_RANGE, DEFAULT_RESERVED_PORTS)
    }

    /// Mark `port` as taken by a *recorded* workspace (adopt-or-kill). Refuses
    /// ports that are already allocated or reserved.
    ///
    /// # Errors
    /// [`PortError::AlreadyUsed`] if the port is already used or reserved.
    pub fn reserve(&mut self, port: u16) -> Result<(), PortError> {
        if self.reserved.contains(&port) || self.used.contains(&port) {
            return Err(PortError::AlreadyUsed(port));
        }
        if !self.range.contains(&port) {
            return Err(PortError::InvalidRange(port));
        }
        self.used.insert(port);
        Ok(())
    }

    /// The lowest free port in range that is neither reserved nor used.
    /// Returns `None` when the range is exhausted.
    pub fn alloc(&mut self) -> Option<u16> {
        for port in self.range.clone() {
            if !self.reserved.contains(&port) && !self.used.contains(&port) {
                self.used.insert(port);
                return Some(port);
            }
        }
        None
    }

    /// Return `port` to the pool. No-op if it was not allocated.
    pub fn free(&mut self, port: u16) {
        self.used.remove(&port);
    }

    /// Is `port` handed out to a project right now?
    #[must_use]
    pub fn is_allocated(&self, port: u16) -> bool {
        self.used.contains(&port)
    }

    /// Is `port` in the reserved set (never handed to a project)?
    #[must_use]
    pub fn is_reserved(&self, port: u16) -> bool {
        self.reserved.contains(&port)
    }

    /// The ports currently handed out, in ascending order.
    #[must_use = "allocated ports are only visible through the returned iterator"]
    pub fn allocated(&self) -> impl Iterator<Item = u16> + '_ {
        self.used.iter().copied()
    }
}

/// Interpret an allocator failure as a [`crate::CoreError`] for config/error
/// surfacing.
#[must_use]
pub fn port_error_message(err: &PortError) -> String {
    match err {
        PortError::AlreadyUsed(p) => format!("port {p} is already allocated or reserved"),
        PortError::InvalidRange(p) => format!("port {p} is outside the configured range"),
        PortError::Exhausted => "no free ports in the configured range".to_owned(),
    }
}

impl From<PortError> for CoreError {
    fn from(e: PortError) -> Self {
        let (port, reason) = match e {
            PortError::AlreadyUsed(p) => (p, "already allocated or reserved"),
            PortError::InvalidRange(p) => (p, "outside the configured range"),
            PortError::Exhausted => (0, "range exhausted"),
        };
        Self::InvalidPort { port, reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocator() -> PortAllocator {
        PortAllocator::new(1000..=1003, [1001])
    }

    #[test]
    fn alloc_returns_lowest_free_in_range() {
        let mut a = allocator();
        assert_eq!(a.alloc(), Some(1000));
        assert_eq!(a.alloc(), Some(1002));
        assert_eq!(a.alloc(), Some(1003));
        assert_eq!(a.alloc(), None, "range exhausted");
    }

    #[test]
    fn reserved_ports_are_never_handed_out() {
        let mut a = allocator();
        assert_eq!(a.alloc(), Some(1000));
        assert_eq!(a.alloc(), Some(1002));
        assert!(!a.is_reserved(1000));
        assert!(a.is_reserved(1001));
    }

    #[test]
    fn default_allocator_never_yields_reserved_ports() {
        let mut a = PortAllocator::default_allocator();
        for _ in 0..198 {
            let p = a.alloc().expect("default range has 198 allocable ports");
            assert!(!DEFAULT_RESERVED_PORTS.contains(&p), "reserved port {p} leaked");
        }
        assert_eq!(a.alloc(), None, "all 198 non-reserved ports handed out");
    }

    #[test]
    fn reserve_refuses_used_and_reserved() {
        let mut a = allocator();
        a.reserve(1002).unwrap();
        assert_eq!(a.reserve(1002), Err(PortError::AlreadyUsed(1002)));
        assert_eq!(a.reserve(1001), Err(PortError::AlreadyUsed(1001)));
        assert!(a.is_allocated(1002));
    }

    #[test]
    fn reserve_refuses_out_of_range() {
        let mut a = allocator();
        assert_eq!(a.reserve(999), Err(PortError::InvalidRange(999)));
        assert_eq!(a.reserve(1004), Err(PortError::InvalidRange(1004)));
    }

    #[test]
    fn free_returns_a_port_to_the_pool() {
        let mut a = allocator();
        let p = a.alloc().unwrap();
        assert!(a.is_allocated(p));
        a.free(p);
        assert!(!a.is_allocated(p));
        assert_eq!(a.alloc(), Some(p), "freed port is reused lowest-first");
    }

    #[test]
    fn free_of_unallocated_port_is_a_noop() {
        let mut a = allocator();
        a.free(1002);
        assert_eq!(a.alloc(), Some(1000));
        assert_eq!(a.alloc(), Some(1002));
    }

    #[test]
    fn reserved_range_that_is_fully_reserved_is_exhausted() {
        let mut a = PortAllocator::new(1000..=1001, [1000, 1001]);
        assert_eq!(a.alloc(), None);
    }
}
