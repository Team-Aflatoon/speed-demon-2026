//! Speed Demon 2026, Track C (Join): parses the hostile /data/records.csv,
//! joins it to /data/regions.csv and writes /out/aggregates.txt.
//! README.md explains the design; the spec is specs/c-join.md.

mod map;
mod names;
mod pipeline;
mod records;
mod regions;

use records::Fail;
use regions::Regions;
use std::fmt::Write as _;
use std::{env, fs, process, thread};

/// Name-table slots for the first attempt: room for 114,688 distinct names
/// (the spec's name pool has 100,000). A larger input retries with more.
const SLOTS: usize = 1 << 17;

/// Running aggregates for one group, which is one output line.
#[derive(Clone, Copy)]
pub struct Agg {
    pub count: u64,
    pub ids: u64,
    pub sum: i64,
    pub min: i64,
    pub max: i64,
    pub names: u64,
    pub name_bytes: u64,
    pub note_bytes: u64,
}

impl Agg {
    pub const EMPTY: Agg = Agg {
        count: 0,
        ids: 0,
        sum: 0,
        min: i64::MAX,
        max: i64::MIN,
        names: 0,
        name_bytes: 0,
        note_bytes: 0,
    };

    pub fn merge(&mut self, o: &Agg) {
        self.count += o.count;
        self.ids += o.ids;
        self.sum += o.sum;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
        self.names += o.names;
        self.name_bytes += o.name_bytes;
        self.note_bytes += o.note_bytes;
    }
}

/// A value alone on its own pair of cache lines (Intel's adjacent-line
/// prefetcher moves lines in pairs). For counters that one core writes while
/// others keep reading their neighbours: without the padding, every write
/// would evict the readers' copy of the line (false sharing).
#[repr(align(128))]
pub struct Padded<T>(pub T);

impl<T> std::ops::Deref for Padded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Asks the CPU to start loading `value`'s cache line, so that a later read
/// finds it there. Used to overlap the cache misses of a batch of lookups.
pub fn prefetch<T>(value: &T) {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
    // SAFETY: a prefetch is only a hint. It never faults and has no effect
    // on memory, and the pointer comes from a live reference anyway.
    unsafe { _mm_prefetch::<_MM_HINT_T0>((value as *const T).cast()) }
}

pub fn prefetch_write<T>(value: &T) {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_ET0};
    // SAFETY: as for `prefetch`.
    unsafe { _mm_prefetch::<_MM_HINT_ET0>((value as *const T).cast()) }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let data = args.get(1).map_or("/data", |s| s);
    let out = args.get(2).map_or("/out", |s| s);
    let records = map_or_die(&format!("{data}/records.csv"));
    if records.len > u32::MAX as usize {
        die("records.csv is over 4 GiB; chunk offsets are 32-bit"); // spec: 202 MB
    }
    let region_file = map_or_die(&format!("{data}/regions.csv"));
    let regions = Regions::new(region_file).unwrap_or_else(|e| die(&format!("regions.csv: {e}")));
    let cpus = thread::available_parallelism().map_or(1, |n| n.get());
    let (mut threads, mut slots) = (cpus.min(12), SLOTS);
    let aggs = loop {
        match pipeline::run(threads, slots, records, (&regions, region_file)) {
            Ok(aggs) => break aggs,
            Err(Fail::Full) => slots *= 8,
            // One worker means one chunk, cut at the start of the file.
            Err(Fail::Malformed) if threads > 1 => threads = 1,
            Err(Fail::Malformed) => die("records.csv does not follow spec section 2.1"),
        }
    };
    let path = format!("{out}/aggregates.txt");
    if let Err(e) = fs::write(&path, report(&regions, &aggs)) {
        die(&format!("{path}: {e}"));
    }
}

fn report(regions: &Regions, aggs: &[Agg]) -> String {
    let mut out = String::new();
    // `_` sorts after every capital letter, so _UNMATCHED comes last.
    for (key, group) in regions.countries().chain([("_UNMATCHED".into(), 0)]) {
        let a = &aggs[usize::from(group)];
        if a.count > 0 {
            let (sum, min, max) = (money(a.sum), money(a.min), money(a.max));
            let counts = [a.count, a.ids, a.names, a.name_bytes, a.note_bytes];
            let [count, ids, names, name_bytes, note_bytes] = counts;
            let line = format!("{key} {count} {ids} {sum} {min} {max} {names} {name_bytes}");
            writeln!(out, "{line} {note_bytes}").unwrap();
        }
    }
    out
}

/// Integer cents as `-?D+.DD`, never `-0.00`.
fn money(cents: i64) -> String {
    let (sign, abs) = (if cents < 0 { "-" } else { "" }, cents.unsigned_abs());
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

fn map_or_die(path: &str) -> map::Mapped {
    map::map(path).unwrap_or_else(|e| die(&format!("{path}: {e}")))
}

pub fn die(msg: &str) -> ! {
    eprintln!("track-c: {msg}");
    process::exit(1)
}
