//! Process-RSS reader abstraction.
//!
//! Production code uses [`SystemRss`] which delegates to `sysinfo`.
//! Tests substitute a hand-rolled source so the budget logic can be
//! driven through specific RSS values without spawning a sub-process.

use std::sync::Mutex;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Abstraction over "what is the current process RSS, in bytes".
///
/// Implementations may cache their reading; the budget calls
/// [`RssSource::read_rss_bytes`] once per tick and is happy with a
/// best-effort value.
pub trait RssSource: Send + Sync {
    /// Current process RSS in bytes.
    fn read_rss_bytes(&self) -> u64;

    /// Installed system RAM in bytes. Used at startup when auto-deriving
    /// the budget target. Implementations must return a positive value.
    fn total_ram_bytes(&self) -> u64 {
        panic!("RssSource::total_ram_bytes must be implemented");
    }
}

/// Live RSS reader backed by [`sysinfo`].
pub struct SystemRss {
    inner: Mutex<SystemRssInner>,
}

struct SystemRssInner {
    system: System,
    pid: Pid,
}

impl SystemRss {
    /// Construct a reader bound to the current process.
    ///
    /// # Panics
    ///
    /// Panics if the current process id is not representable as a
    /// [`Pid`] on this platform — a "cannot happen" condition.
    #[must_use]
    pub fn new() -> Self {
        let pid = sysinfo::get_current_pid().expect("sysinfo cannot resolve current pid");
        let mut system = System::new();
        system.refresh_memory();
        Self {
            inner: Mutex::new(SystemRssInner { system, pid }),
        }
    }
}

impl Default for SystemRss {
    fn default() -> Self {
        Self::new()
    }
}

impl RssSource for SystemRss {
    fn read_rss_bytes(&self) -> u64 {
        let mut guard = self.inner.lock().expect("system-rss lock");
        let pid = guard.pid;
        let refresh = ProcessRefreshKind::nothing().with_memory();
        let refreshed = guard.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            refresh,
        );
        assert_eq!(refreshed, 1, "sysinfo did not refresh the current process");
        guard
            .system
            .process(pid)
            .map(sysinfo::Process::memory)
            .expect("sysinfo lost the current process")
    }

    fn total_ram_bytes(&self) -> u64 {
        let mut guard = self.inner.lock().expect("system-rss lock");
        guard.system.refresh_memory();
        guard.system.total_memory()
    }
}
