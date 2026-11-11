//! Platform-native measurement of the memory only swap can reclaim.
//!
//! See [`SystemRss`](crate::SystemRss) for what each platform counts and why
//! clean file-backed pages are excluded.

use sysinfo::System;

/// Current-process swappable-memory probe for one platform.
pub(crate) struct SwappableFootprint {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    status: Vec<u8>,
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pid: libc::c_int,
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    pid: sysinfo::Pid,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl SwappableFootprint {
    /// `/proc/self/status` is a few KiB; the buffer is reused across reads.
    const STATUS_CAPACITY: usize = 4096;

    pub(crate) fn new() -> Self {
        Self {
            status: Vec::with_capacity(Self::STATUS_CAPACITY),
        }
    }

    pub(crate) fn read_bytes(&mut self, _system: &mut System) -> u64 {
        use std::io::Read;

        self.status.clear();
        std::fs::File::open("/proc/self/status")
            .and_then(|mut file| file.read_to_end(&mut self.status))
            .expect("cannot read /proc/self/status");
        let status = std::str::from_utf8(&self.status).expect("/proc/self/status is not UTF-8");
        ProcStatus::new(status).swappable_bytes()
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl SwappableFootprint {
    pub(crate) fn new() -> Self {
        // SAFETY: getpid has no preconditions and cannot fail.
        Self {
            pid: unsafe { libc::getpid() },
        }
    }

    pub(crate) fn read_bytes(&mut self, _system: &mut System) -> u64 {
        let mut info = std::mem::MaybeUninit::<libc::rusage_info_v0>::zeroed();
        // SAFETY: the kernel writes at most one `rusage_info_v0` (the
        // requested flavor) into a buffer of exactly that type and size.
        let status = unsafe {
            libc::proc_pid_rusage(self.pid, libc::RUSAGE_INFO_V0, info.as_mut_ptr().cast())
        };
        assert_eq!(
            status,
            0,
            "proc_pid_rusage failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: a successful call initialised the whole record; the
        // buffer was also zero-initialised before the call.
        unsafe { info.assume_init() }.ri_phys_footprint
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
impl SwappableFootprint {
    pub(crate) fn new() -> Self {
        Self {
            pid: sysinfo::get_current_pid().expect("sysinfo cannot resolve current pid"),
        }
    }

    pub(crate) fn read_bytes(&mut self, system: &mut System) -> u64 {
        let refresh = sysinfo::ProcessRefreshKind::nothing().with_memory();
        let refreshed = system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[self.pid]),
            true,
            refresh,
        );
        assert_eq!(refreshed, 1, "sysinfo did not refresh the current process");
        let process = system
            .process(self.pid)
            .expect("sysinfo lost the current process");
        // Windows reports private commit (`PrivateUsage`) as virtual memory:
        // the pagefile-backed bytes. Elsewhere only the resident set exists.
        if cfg!(windows) {
            process.virtual_memory()
        } else {
            process.memory()
        }
    }
}

/// The swappable-memory fields of one Linux `/proc/<pid>/status` record.
#[cfg(any(target_os = "linux", target_os = "android", test))]
pub(crate) struct ProcStatus<'a> {
    record: &'a str,
}

#[cfg(any(target_os = "linux", target_os = "android", test))]
impl<'a> ProcStatus<'a> {
    /// Anonymous private pages, including copy-on-write file pages.
    const ANONYMOUS: &'static str = "RssAnon:";
    /// tmpfs, memfd and shared anonymous pages: swappable, not file-backed.
    const SHARED_MEMORY: &'static str = "RssShmem:";
    /// Anonymous pages already pushed to swap still belong to the process.
    const SWAPPED: &'static str = "VmSwap:";

    pub(crate) fn new(record: &'a str) -> Self {
        Self { record }
    }

    /// `RssAnon + RssShmem + VmSwap`, excluding `RssFile`.
    ///
    /// # Panics
    ///
    /// Panics if a field is missing or malformed: the kernel predates
    /// Linux 4.5 or the record is not a process status.
    pub(crate) fn swappable_bytes(&self) -> u64 {
        [Self::ANONYMOUS, Self::SHARED_MEMORY, Self::SWAPPED]
            .into_iter()
            .map(|field| self.kibibytes(field))
            .try_fold(0_u64, u64::checked_add)
            .and_then(|kib| kib.checked_mul(1024))
            .expect("process memory overflows u64")
    }

    fn kibibytes(&self, field: &str) -> u64 {
        let value = self
            .record
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .unwrap_or_else(|| panic!("process status lacks {field}"));
        let mut parts = value.split_whitespace();
        let (Some(amount), Some("kB"), None) = (parts.next(), parts.next(), parts.next()) else {
            panic!("malformed process status field {field}{value}");
        };
        amount
            .parse()
            .unwrap_or_else(|error| panic!("malformed process status field {field}: {error}"))
    }
}
