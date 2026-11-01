//! Speed Demon 2026, Track C (Join): parses the hostile /data/records.csv,
//! joins it to /data/regions.csv and writes /out/aggregates.txt.
//! NOTES.md explains the design; the spec is specs/c-join.md.
//!
//! Usage: `track-c [DATA_DIR [OUT_DIR]]`, by default `/data` and `/out`.
//! Errors go to stderr, with exit status 1. So do notes on the two
//! fallbacks (a bigger name table, one chunk), which still exit 0.

// The parser's SIMD needs AVX2, which .cargo/config.toml turns on for the
// whole crate (target-cpu=x86-64-v3); refuse any other build.
#[cfg(not(target_feature = "avx2"))]
compile_error!("track-c needs AVX2: build with -C target-cpu=x86-64-v3 (see .cargo/config.toml)");

mod names;
mod parse;
mod regions;
mod run;
mod sys;

use parse::Fail;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering::AcqRel};
use std::{env, fs, process, thread};

/// Name-table slots for the first attempt: room for 114,688 distinct names
/// (the spec's name pool has 100,000). A larger input retries with 8 times
/// more, up to `MAX_SLOTS` (58.7 million names; 2 GiB of table).
const SLOTS: usize = 1 << 17;
const MAX_SLOTS: usize = 1 << 26;
/// Bytes per chunk of records.csv, read into a worker's buffer and parsed
/// while it is still in that core's L2 cache.
const PIECE: usize = 64 << 10;
/// Groups: `_UNMATCHED` (0) and up to 255 countries.
pub const GROUPS: usize = 256;

/// A value alone on its own pair of cache lines (Intel's adjacent-line
/// prefetcher moves lines in pairs), for counters that one core writes
/// while others read their neighbours.
#[repr(align(128))]
pub struct Padded<T>(pub T);

/// Asks the CPU to start loading the cache line of element `i` of `s`, so
/// that a later read finds it there. Used to overlap the cache misses of a
/// window's records. An `i` past the end is harmless, so it is not checked.
#[inline(always)]
pub fn prefetch<T>(s: &[T], i: usize) {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
    // SAFETY: a prefetch is only a hint: it never faults, whatever the
    // address, and has no effect on memory.
    unsafe { _mm_prefetch::<_MM_HINT_T0>(s.as_ptr().wrapping_add(i).cast()) }
}

fn main() {
    sys::no_huge_pages();
    let args: Vec<String> = env::args().collect();
    let data = args.get(1).map_or("/data", |s| s);
    let out = args.get(2).map_or("/out", |s| s);
    let path = format!("{data}/records.csv");
    let file = or_die(fs::File::open(&path), &path);
    let len = or_die(file.metadata(), &path).len();
    if len >= u64::from(u32::MAX) - 64 {
        die("records.csv is 4 GiB or more; offsets and name tags are 32-bit"); // spec: 202 MB
    }
    let regions = regions::Regions::open(&format!("{data}/regions.csv"));
    let (mut piece, mut slots) = (PIECE, SLOTS);
    let totals = loop {
        match run::run(piece, slots, &file, &regions) {
            Ok(totals) => break totals,
            Err(Fail::Full) if slots < MAX_SLOTS => {
                slots *= 8;
                eprintln!("track-c: note: name table full; retrying with {slots} slots");
            }
            Err(Fail::Full) => die(&format!("more than {} distinct names", MAX_SLOTS / 8 * 7)),
            // A single chunk starts at the start of the file: always right.
            Err(Fail::Malformed(_)) if piece < len as usize => {
                piece = len.max(1) as usize;
                eprintln!("track-c: note: chunk cuts unproven; reparsing as one chunk");
            }
            Err(Fail::Malformed(at)) => die(&format!("records.csv breaks spec 2.1 near byte {at}")),
        }
    };
    let mut text = String::new();
    // `_` sorts after every capital letter, so _UNMATCHED comes last.
    for (key, group) in regions.countries().chain([("_UNMATCHED".into(), 0)]) {
        let ([ids, cents, txt, min, max], [records, distinct, names]) = totals[usize::from(group)];
        let (sum, min, max) = (money(cents), money(min), money(max));
        let notes = txt - names as i64;
        let line = format!("{key} {records} {ids} {sum} {min} {max} {distinct} {names}");
        if records > 0 {
            writeln!(text, "{line} {notes}").unwrap();
        }
    }
    let path = format!("{out}/aggregates.txt");
    if let Err(e) = fs::write(&path, text) {
        let _ = fs::remove_file(&path); // no partial output
        die(&format!("{path}: {e}"));
    }
}

/// Integer cents as `-?D+.DD`, never `-0.00`.
fn money(cents: i64) -> String {
    let (sign, abs) = (if cents < 0 { "-" } else { "" }, cents.unsigned_abs());
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// The value of `r`, or the error about `what` it holds, through `die`.
pub fn or_die<T>(r: std::io::Result<T>, what: &str) -> T {
    r.unwrap_or_else(|e| die(&format!("{what}: {e}")))
}

/// The error path for anything the input or the system can cause. Cargo.toml
/// sets `panic = "abort"`, so the few `unwrap`s left (on locks, which abort
/// instead of being poisoned, on writes to a `String`, and on lengths just
/// checked) cannot fire on any input; if one ever did, the process would
/// still stop with a message and a non-zero status. Workers may fail at the
/// same time; the first caller reports and exits, any other one waits for
/// that exit, so stderr gets one message.
pub fn die(msg: &str) -> ! {
    static DYING: AtomicBool = AtomicBool::new(false);
    if DYING.swap(true, AcqRel) {
        loop {
            thread::park();
        }
    }
    eprintln!("track-c: {msg}");
    process::exit(1)
}
