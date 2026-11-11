//! Linux status parsing counts swappable pages and never file-backed ones.

use crate::footprint::ProcStatus;

const STATUS: &str = "Name:\tlegra-custodian\n\
VmRSS:\t 9437184 kB\n\
RssAnon:\t  262144 kB\n\
RssFile:\t 9170944 kB\n\
RssShmem:\t    4096 kB\n\
VmSwap:\t    1024 kB\n\
Threads:\t64\n";

#[test]
fn counts_anonymous_shared_and_swapped_pages_but_not_file_pages() {
    assert_eq!(
        ProcStatus::new(STATUS).swappable_bytes(),
        (262_144 + 4096 + 1024) * 1024
    );
}

#[test]
#[should_panic(expected = "process status lacks RssAnon:")]
fn kernels_without_split_resident_counters_fail_fast() {
    ProcStatus::new("VmRSS:\t 1024 kB\nVmSwap:\t 0 kB\n").swappable_bytes();
}

#[test]
#[should_panic(expected = "malformed process status field RssShmem:")]
fn malformed_fields_fail_fast() {
    ProcStatus::new("RssAnon:\t 1 kB\nRssShmem:\t 1 MB\nVmSwap:\t 0 kB\n").swappable_bytes();
}
