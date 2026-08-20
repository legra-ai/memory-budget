//! Threshold-driven jemalloc heap-profile dumps (feature `heap-profiling`).
//!
//! The budget tick already reads jemalloc's `stats.resident`. When the
//! non-cache bucket balloons (live heap with no registered reporter), the
//! totals tell you *how much* but not *where*. This profiler closes that gap: when
//! resident RSS crosses successive thresholds it asks jemalloc to write
//! a heap profile (`prof.dump`), so the growth curve is captured as a
//! series of dumps whose backtraces name the exact allocation sites.
//!
//! ## Activation
//!
//! Compiled in only with the `heap-profiling` feature (which pulls
//! `jemalloc`). The application must also initialize jemalloc profiling
//! before starting the budget. Dumps default to `<cwd>/logs`; override the
//! directory and thresholds with `MEMORY_BUDGET_HEAP_PROFILE_DIR` /
//! `MEMORY_BUDGET_HEAP_PROFILE_{FLOOR,STEP}_GB`.
//! (To toggle profiling on a non-feature build instead, the **prefixed**
//! `_RJEM_MALLOC_CONF=prof:true,...` env works on macOS; the unprefixed
//! `MALLOC_CONF` is Linux-only.)
//!
//! ## Symbolizing a dump
//!
//! The dump records raw runtime instruction addresses. Linux jeprof
//! reads `/proc/self/maps`; macOS has none, so this profiler logs the
//! dyld image slide at startup. Symbolize with:
//!
//! ```text
//! atos -o <application binary> -l 0x100000000+<slide> <addr> <addr> ...
//! ```

use std::ffi::{CString, c_char};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Default resident floor below which no profile is dumped (bytes).
const DEFAULT_FLOOR_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Default resident step between successive dumps (bytes).
const DEFAULT_STEP_BYTES: u64 = 1024 * 1024 * 1024;

/// Directory for dumps; absent → profiler disabled.
const DIR_ENV: &str = "MEMORY_BUDGET_HEAP_PROFILE_DIR";
/// Override the resident floor, in whole GiB.
const FLOOR_GB_ENV: &str = "MEMORY_BUDGET_HEAP_PROFILE_FLOOR_GB";
/// Override the resident step, in whole GiB.
const STEP_GB_ENV: &str = "MEMORY_BUDGET_HEAP_PROFILE_STEP_GB";

/// Dumps a jemalloc heap profile each time resident RSS climbs past the
/// next threshold (`floor`, then `floor + step`, `floor + 2·step`, …).
pub(crate) struct HeapProfiler {
    /// Directory the `*.heap` dumps are written into.
    dir: PathBuf,
    /// Resident-byte step between successive dumps.
    step_bytes: u64,
    /// Next resident-byte threshold that triggers a dump.
    next_threshold: AtomicU64,
    /// Monotonic dump sequence for stable filenames / ordering.
    seq: AtomicU64,
}

impl HeapProfiler {
    /// Build from the environment.
    ///
    /// The dump directory is [`DIR_ENV`] when set, else `<cwd>/logs` — the
    /// application's working directory determines where dumps land. Because
    /// this only compiles
    /// under the `heap-profiling` feature (a deliberately-built diagnostic
    /// binary), the profiler always arms; there is no "off" path to gate.
    ///
    /// Logs the dyld image slide (macOS) and warns if jemalloc was not
    /// started with `prof:true` — in that case dumps will fail, so the
    /// operator learns at startup rather than at the first threshold.
    pub(crate) fn from_env() -> Option<Self> {
        let dir = std::env::var_os(DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("logs")
            });
        let floor = gib_env(FLOOR_GB_ENV).unwrap_or(DEFAULT_FLOOR_BYTES);
        let step = gib_env(STEP_GB_ENV).unwrap_or(DEFAULT_STEP_BYTES).max(1);

        let prof_on = prof_is_active();
        Self::log_symbolization_anchor();
        tracing::info!(
            target: "memory_budget::heap_profile",
            dir = %dir.display(),
            floor_bytes = floor,
            step_bytes = step,
            prof_active = prof_on,
            "heap profiler armed{}",
            if prof_on {
                ""
            } else {
                " — but jemalloc opt.prof is false; dumps will fail (set \
                 _RJEM_MALLOC_CONF=prof:true,prof_active:true and build with \
                 --features heap-profiling)"
            }
        );

        Some(Self {
            dir,
            step_bytes: step,
            next_threshold: AtomicU64::new(floor),
            seq: AtomicU64::new(0),
        })
    }

    /// Dump a profile if `resident_bytes` has crossed the next
    /// threshold, then advance the threshold past the current resident
    /// so each further `step_bytes` of growth captures one more dump.
    ///
    /// Called from the single budget ticker, so the load/store pair on
    /// `next_threshold` needs no CAS.
    pub(crate) fn maybe_dump(&self, resident_bytes: u64) {
        if resident_bytes < self.next_threshold.load(Ordering::Relaxed) {
            return;
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.dump(seq, resident_bytes);
        // Arm the next threshold one full step above the current
        // resident, so a transient plateau does not re-dump every tick.
        self.next_threshold
            .store(resident_bytes + self.step_bytes, Ordering::Relaxed);
    }

    /// Write one dump to `<dir>/heap-<seq>-<gib>gib.heap`.
    fn dump(&self, seq: u64, resident_bytes: u64) {
        let gib = resident_bytes / (1024 * 1024 * 1024);
        let path = self.dir.join(format!("heap-{seq:04}-{gib}gib.heap"));
        let Ok(c_path) = CString::new(path.as_os_str().as_encoded_bytes()) else {
            tracing::warn!(
                target: "memory_budget::heap_profile",
                path = %path.display(),
                "heap dump path contained a NUL byte; skipping"
            );
            return;
        };
        // `prof.dump` reads `newp` as `const char **` — a pointer to the
        // filename pointer — so the value handed to `raw::write` is the
        // `*const c_char` itself (it takes its address internally).
        let name_ptr: *const c_char = c_path.as_ptr();
        // SAFETY: `prof.dump` is a valid mallctl name when jemalloc was
        // built with profiling; `c_path` outlives the call.
        let result = unsafe { tikv_jemalloc_ctl::raw::write(b"prof.dump\0", name_ptr) };
        match result {
            Ok(()) => tracing::warn!(
                target: "memory_budget::heap_profile",
                seq,
                resident_bytes,
                path = %path.display(),
                "heap profile dumped"
            ),
            Err(e) => tracing::warn!(
                target: "memory_budget::heap_profile",
                seq,
                resident_bytes,
                error = %e,
                "heap profile dump failed (is jemalloc built with profiling and started \
                 with prof:true?)"
            ),
        }
    }

    /// Log the symbolization anchor for post-hoc `atos` resolution.
    ///
    /// On macOS the dumps carry no mapping section (no `/proc/self/maps`),
    /// so the dyld image slide is needed to translate runtime addresses
    /// back to `__TEXT`-relative ones. On other targets jeprof reads the
    /// process maps directly and no anchor is required.
    fn log_symbolization_anchor() {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: image index 0 is the main executable; the libdyld
            // symbol is always present on macOS.
            let slide = unsafe { dyld_image_vmaddr_slide_0() };
            tracing::info!(
                target: "memory_budget::heap_profile",
                dyld_slide = format!("0x{slide:x}"),
                symbolize = "atos -o <application> -l 0x100000000+<dyld_slide> <addr>...",
                "heap profile symbolization anchor (macOS: no /proc maps)"
            );
        }
    }
}

/// Parse a whole-GiB env override into bytes.
fn gib_env(name: &str) -> Option<u64> {
    let gib = std::env::var(name).ok()?.trim().parse::<u64>().ok()?;
    Some(gib.saturating_mul(1024 * 1024 * 1024))
}

/// Whether jemalloc was started with profiling enabled (`opt.prof`).
fn prof_is_active() -> bool {
    // SAFETY: `opt.prof` is a valid read-only mallctl name; reads a bool.
    unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"opt.prof\0") }.unwrap_or(false)
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// dyld's ASLR slide for the main executable image (index 0).
    #[link_name = "_dyld_get_image_vmaddr_slide"]
    fn dyld_image_vmaddr_slide_0_raw(image_index: u32) -> isize;
}

#[cfg(target_os = "macos")]
unsafe fn dyld_image_vmaddr_slide_0() -> isize {
    unsafe { dyld_image_vmaddr_slide_0_raw(0) }
}
