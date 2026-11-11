//! The process reading counts memory that can swap, not mapped file pages.
//!
//! One test per binary: a concurrent test's allocations would move the
//! whole-process reading this test compares.

#![cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]

use std::io::Write;

use memory_budget::{RssSource, SystemRss};

/// Large enough to dwarf allocator and runtime noise.
const REGION_BYTES: usize = 256 * 1024 * 1024;
/// Tolerated unrelated movement while one region is touched.
const NOISE_BYTES: u64 = 32 * 1024 * 1024;

struct Region {
    address: *mut libc::c_void,
    len: usize,
}

impl Region {
    fn map_file(file: &std::fs::File, len: usize) -> Self {
        use std::os::fd::AsRawFd;
        // SAFETY: a fresh read-only private mapping of an open file; the
        // region is unmapped on drop and never aliased.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(address, libc::MAP_FAILED, "file mapping failed");
        Self { address, len }
    }

    fn map_anonymous(len: usize) -> Self {
        // SAFETY: a fresh private anonymous mapping, unmapped on drop.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        assert_ne!(address, libc::MAP_FAILED, "anonymous mapping failed");
        Self { address, len }
    }

    fn page_bytes() -> usize {
        // SAFETY: sysconf has no preconditions.
        usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap()
    }

    fn read_every_page(&self) {
        for offset in (0..self.len).step_by(Self::page_bytes()) {
            // SAFETY: offset is inside the live readable mapping.
            std::hint::black_box(unsafe { self.address.cast::<u8>().add(offset).read_volatile() });
        }
    }

    fn write_every_page(&self) {
        for offset in (0..self.len).step_by(Self::page_bytes()) {
            // SAFETY: offset is inside the live writable mapping.
            unsafe { self.address.cast::<u8>().add(offset).write_volatile(1) };
        }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly the region this value mapped.
        assert_eq!(unsafe { libc::munmap(self.address, self.len) }, 0);
    }
}

fn immutable_file(len: usize) -> std::fs::File {
    let path = std::env::temp_dir().join(format!("memory-budget-footprint-{}", std::process::id()));
    let mut file = std::fs::File::create_new(&path).unwrap();
    // bounded: one fixed 1 MiB chunk written repeatedly.
    let chunk = vec![7_u8; 1024 * 1024];
    for _ in 0..len / chunk.len() {
        file.write_all(&chunk).unwrap();
    }
    file.sync_all().unwrap();
    drop(file);
    let file = std::fs::File::open(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    file
}

#[test]
fn mapped_file_pages_are_excluded_while_anonymous_pages_are_counted() {
    let reader = SystemRss::new();
    let file = immutable_file(REGION_BYTES);

    let before_file = reader.read_rss_bytes();
    let mapped = Region::map_file(&file, REGION_BYTES);
    mapped.read_every_page();
    let after_file = reader.read_rss_bytes();
    drop(mapped);

    let before_anonymous = reader.read_rss_bytes();
    let anonymous = Region::map_anonymous(REGION_BYTES);
    anonymous.write_every_page();
    let after_anonymous = reader.read_rss_bytes();
    drop(anonymous);

    assert!(
        after_file.saturating_sub(before_file) < NOISE_BYTES,
        "reading a {REGION_BYTES}-byte mapped file moved the reading from {before_file} to {after_file}"
    );
    assert!(
        after_anonymous.saturating_sub(before_anonymous) >= REGION_BYTES as u64 - NOISE_BYTES,
        "touching {REGION_BYTES} anonymous bytes moved the reading only from {before_anonymous} to {after_anonymous}"
    );
}
