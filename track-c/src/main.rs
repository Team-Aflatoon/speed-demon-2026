//! Speed Demon 2026, Track C (Join): parses the hostile /data/records.csv,
//! joins it to /data/regions.csv and writes /out/aggregates.txt.
//! README.md explains the design; the spec is specs/c-join.md.
//!
//! Usage: `track-c [DATA_DIR [OUT_DIR]]`, by default `/data` and `/out`.
//! Errors go to stderr, with exit status 1. So do notes on the two
//! fallbacks (a bigger name table, one chunk), which still exit 0.

// The parser's SIMD needs AVX2, which .cargo/config.toml turns on for the
// whole crate (target-cpu=x86-64-v3); refuse any other build.
#[cfg(not(target_feature = "avx2"))]
compile_error!("track-c needs AVX2: build with -C target-cpu=x86-64-v3 (see .cargo/config.toml)");

mod map;
mod names;
mod pipeline;
mod records;
mod regions;

use records::Fail;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering::AcqRel};
use std::{env, fs, fs::File, process, thread};

/// Name-table slots for the first attempt: room for 114,688 distinct names
/// (the spec's name pool has 100,000). A larger input retries with 8 times
/// more, up to `MAX_SLOTS` (58.7 million names; 2 GiB of table).
const SLOTS: usize = 1 << 17;
const MAX_SLOTS: usize = 1 << 26;
/// Bytes per chunk of records.csv, read into a worker's buffer and parsed
/// while it is still in that core's L2 cache.
const PIECE: usize = 64 << 10;
/// The most worker threads (the scoring machine gives us twelve).
const MAX_THREADS: usize = 12;
/// Groups: `_UNMATCHED` (0) and up to 255 countries.
pub const GROUPS: usize = 256;

/// Running aggregates for one group, which is one output line. `sums` holds,
/// in this order: count, id sum, amount sum (cents), distinct names, name
/// bytes and note bytes. The parser adds whole records to it at once, and the
/// name counter adds to the distinct names alone, at index `DISTINCT`. Keep
/// the order in step with `Shared::records` and the output line in `main`.
#[derive(Clone, Copy)]
pub struct Agg {
    pub sums: [i64; 6],
    pub min: i64,
    pub max: i64,
}

pub const DISTINCT: usize = 3;

impl Agg {
    pub const EMPTY: Agg = Agg {
        sums: [0; 6],
        min: i64::MAX,
        max: i64::MIN,
    };

    /// Adds `sums`, and widens the amount range to cover `min..=max`.
    pub fn add(&mut self, sums: [i64; 6], min: i64, max: i64) {
        for (s, x) in self.sums.iter_mut().zip(sums) {
            *s += x;
        }
        (self.min, self.max) = (self.min.min(min), self.max.max(max));
    }
}

/// A value alone on its own pair of cache lines (Intel's adjacent-line
/// prefetcher moves lines in pairs). For counters that one core writes while
/// others keep reading their neighbours: without the padding, every write
/// would evict the readers' copy of the line (false sharing).
#[repr(align(128))]
pub struct Padded<T>(pub T);

/// Asks the CPU to start loading `value`'s cache line, so that a later read
/// finds it there. Used to overlap the cache misses of a batch of lookups.
pub fn prefetch<T>(value: &T) {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
    // SAFETY: a prefetch is only a hint. It never faults and has no effect
    // on memory, and the pointer comes from a live reference anyway. Its
    // instruction (SSE) is part of every x86-64 CPU.
    unsafe { _mm_prefetch::<_MM_HINT_T0>((value as *const T).cast()) }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let data = args.get(1).map_or("/data", |s| s);
    let out = args.get(2).map_or("/out", |s| s);
    let path = format!("{data}/records.csv");
    let file = File::open(&path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
    let meta = file
        .metadata()
        .unwrap_or_else(|e| die(&format!("{path}: {e}")));
    let len = meta.len() as usize;
    if len > u32::MAX as usize {
        die("records.csv is over 4 GiB; chunk offsets are 32-bit"); // spec: 202 MB
    }
    let region_file = map_or_die(&format!("{data}/regions.csv"));
    let regions = regions::Regions::new(region_file).unwrap_or_else(|e| die(&e));
    let threads = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_THREADS);
    let (mut piece, mut slots) = (PIECE, SLOTS);
    let aggs = loop {
        match pipeline::run(threads, piece, slots, (&file, len), (&regions, region_file)) {
            Ok(aggs) => break aggs,
            Err((Fail::Full, _)) if slots < MAX_SLOTS => {
                slots *= 8;
                eprintln!("track-c: note: name table full; retrying with {slots} slots");
            }
            Err((Fail::Full, _)) => die(&format!("more than {} distinct names", MAX_SLOTS / 8 * 7)),
            // A single chunk starts at the start of the file: always right.
            Err((Fail::Malformed, _)) if piece < len => {
                piece = len;
                eprintln!("track-c: note: chunk cuts unproven; reparsing as one chunk");
            }
            Err((Fail::Malformed, at)) => die(&format!(
                "records.csv does not follow spec section 2.1 (first error at byte {at})"
            )),
        }
    };
    let mut text = String::new();
    // `_` sorts after every capital letter, so _UNMATCHED comes last.
    for (key, group) in regions.countries().chain([("_UNMATCHED".into(), 0)]) {
        let a = &aggs[usize::from(group)];
        let [count, ids, sum, names, name_bytes, note_bytes] = a.sums;
        let (sum, min, max) = (money(sum), money(a.min), money(a.max));
        if count > 0 {
            let line = format!("{key} {count} {ids} {sum} {min} {max} {names} {name_bytes}");
            writeln!(text, "{line} {note_bytes}").unwrap();
        }
    }
    let path = format!("{out}/aggregates.txt");
    if let Err(e) = fs::write(&path, text) {
        die(&format!("{path}: {e}"));
    }
}

/// Integer cents as `-?D+.DD`, never `-0.00`.
fn money(cents: i64) -> String {
    let (sign, abs) = (if cents < 0 { "-" } else { "" }, cents.unsigned_abs());
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

fn map_or_die(path: &str) -> map::Mapped {
    map::map(path).unwrap_or_else(|e| die(&format!("{path}: {e}")))
}

/// The error path for anything the input or the system can cause. Cargo.toml
/// sets `panic = "abort"`, so the few `unwrap`s left (on locks, which abort
/// instead of being poisoned, on writes to a `String`, and on lengths just
/// checked) cannot fire on any input; if one ever did, the process would
/// still stop with a message and a non-zero status. Workers may fail at the
/// same time; the first caller reports and exits, and any other one waits
/// for that exit, so stderr gets one message.
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
