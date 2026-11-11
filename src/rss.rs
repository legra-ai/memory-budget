//! Process-memory reader abstraction.
//!
//! Production code uses [`SystemRss`], which reads the platform's native
//! measure of memory that only swap or compression can reclaim. Tests
//! substitute a hand-rolled source so the budget logic can be driven
//! through specific values without spawning a sub-process.

use std::sync::Mutex;

use sysinfo::System;

use crate::footprint::SwappableFootprint;

/// Abstraction over "how much memory does the current process own that
/// the operating system cannot reclaim without swapping", in bytes.
///
/// Implementations may cache their reading; the budget calls
/// [`RssSource::read_rss_bytes`] once per tick and is happy with a
/// best-effort value.
pub trait RssSource: Send + Sync {
    /// Current process memory in bytes.
    fn read_rss_bytes(&self) -> u64;

    /// Installed system RAM in bytes. Used at startup when auto-deriving
    /// the budget target. Implementations must return a positive value.
    fn total_ram_bytes(&self) -> u64 {
        panic!("RssSource::total_ram_bytes must be implemented");
    }
}

/// Live reader of the current process's swappable memory footprint.
///
/// The reading counts anonymous and private copy-on-write pages,
/// shared-memory pages (tmpfs, memfd, POSIX shm) and, where the platform
/// reports them, pages already swapped out or compressed. Clean
/// file-backed pages — memory-mapped immutable files, executable text,
/// page cache faulted in by reads — are excluded: the kernel drops and
/// refaults them from their file, so they can never cause swap. Plain
/// resident-set size counts them, which lets a process that only maps a
/// large file look like it is exhausting memory.
///
/// | Platform | Measurement |
/// | --- | --- |
/// | Linux, Android | `RssAnon + RssShmem + VmSwap` from `/proc/self/status` |
/// | macOS, iOS | `ri_phys_footprint` from `proc_pid_rusage` |
/// | Windows | private commit charge (`PrivateUsage`) |
/// | other | resident-set size |
///
/// Every read is a handful of system calls on a buffer reused across
/// reads, cheap enough for once-per-second sampling.
pub struct SystemRss {
    inner: Mutex<SystemRssInner>,
}

struct SystemRssInner {
    system: System,
    footprint: SwappableFootprint,
}

impl SystemRss {
    /// Construct a reader bound to the current process.
    ///
    /// # Panics
    ///
    /// Panics if `sysinfo` cannot initialise on this platform or the
    /// platform cannot identify the current process — both "cannot happen"
    /// conditions.
    #[must_use]
    pub fn new() -> Self {
        let mut system = System::new().expect("sysinfo cannot initialise on this platform");
        system.refresh_memory();
        Self {
            inner: Mutex::new(SystemRssInner {
                system,
                footprint: SwappableFootprint::new(),
            }),
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
        let SystemRssInner { system, footprint } = &mut *guard;
        footprint.read_bytes(system)
    }

    fn total_ram_bytes(&self) -> u64 {
        let mut guard = self.inner.lock().expect("system-rss lock");
        guard.system.refresh_memory();
        guard.system.total_memory()
    }
}
