//! Speed Demon 2026, Track C (Join): parses the hostile /data/records.csv,
//! joins it to /data/regions.csv and writes /out/aggregates.txt.
//! README.md and PERF.md explain the design; the spec is specs/c-join.md.
//!
//! Usage: `track-c [DATA_DIR [OUT_DIR]]`, by default `/data` and `/out`.
//! Errors go to stderr, with exit status 1. So does a note on the fallback
//! to one chunk, which still exits 0.

// The parser's SIMD needs AVX2, which .cargo/config.toml turns on for the
// whole crate (target-cpu=x86-64-v3); refuse any other build.
#[cfg(not(target_feature = "avx2"))]
compile_error!("track-c needs AVX2: build with -C target-cpu=x86-64-v3 (see .cargo/config.toml)");

mod names;
mod parse;
mod run;

use std::fmt::Write as _;
use std::{env, fs, process, sync::Mutex};

/// Bytes per chunk of records.csv, read into a worker's buffer and parsed
/// while it is still in that core's L2 cache.
const PIECE: usize = 64 << 10;
/// Groups: `_UNMATCHED` (0) and up to 255 countries.
pub const GROUPS: usize = 256;

fn main() {
    if !std::arch::is_x86_feature_detected!("pclmulqdq") {
        die("this CPU lacks PCLMULQDQ, which the parser uses (see parse::prefix_xor)");
    }
    // No transparent huge pages: a huge-page fault can stall while the
    // kernel compacts memory, and would charge 2 MiB for a barely used table.
    // SAFETY: a plain prctl (PR_SET_THP_DISABLE) with integer arguments; if
    // the kernel refuses, we just keep its default.
    unsafe { prctl(41, 1, 0, 0, 0) };
    let args: Vec<String> = env::args().collect();
    let data = args.get(1).map_or("/data", |s| s);
    let out = args.get(2).map_or("/out", |s| s);
    let path = format!("{data}/records.csv");
    let file = or_die(fs::File::open(&path), &path);
    let len = or_die(file.metadata(), &path).len() as usize;
    if len >= u32::MAX as usize - 64 {
        die("records.csv is 4 GiB or more; offsets are 32-bit"); // spec: 202 MB
    }
    let regions = run::Regions::open(&format!("{data}/regions.csv"));
    // A single chunk starts at the start of the file: always right.
    let totals = run::run(PIECE, &file, len, &regions).or_else(|_| {
        eprintln!("track-c: note: chunk cuts unproven; reparsing as one chunk");
        run::run(len.max(1), &file, len, &regions)
    });
    let totals = totals.unwrap_or_else(|at| die(&format!("records.csv breaks spec 2.1 at {at}")));
    let mut text = String::new();
    // `_` sorts after every capital letter, so _UNMATCHED comes last.
    for (key, group) in regions.countries().chain([("_UNMATCHED".into(), 0)]) {
        let ([ids, cents, txt, min, max], [distinct, both]) = totals[usize::from(group)];
        let (records, names) = (both >> 32, both & 0xFFFF_FFFF);
        let (sum, min, max, notes) = (money(cents), money(min), money(max), txt - names as i64);
        let line = format!("{key} {records} {ids} {sum} {min} {max} {distinct} {names}");
        if records > 0 {
            _ = writeln!(text, "{line} {notes}");
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
/// same time: the first caller keeps the lock until it exits, so stderr gets
/// one message.
pub fn die(msg: &str) -> ! {
    static DYING: Mutex<()> = Mutex::new(());
    let _exiting = DYING.lock();
    eprintln!("track-c: {msg}");
    process::exit(1)
}

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

// The few kernel calls the standard library does not offer, declared by
// hand (no crates). Nothing mapped here is ever unmapped; it all lives until
// the process exits.
extern "C" {
    fn mmap(at: *mut u8, n: usize, prot: i32, how: i32, fd: i32, offset: i64) -> *mut u8;
    fn madvise(at: *const u8, n: usize, advice: i32) -> i32;
    fn prctl(option: i32, ...) -> i32;
}

/// `n` values of `T` in a new private mapping, page-aligned and never
/// unmapped: the first bytes of the file `fd`, copy-on-write (its pages are
/// already in the page cache, so this costs page-table entries, not copies),
/// or with `fd` -1 zeroed pages that are only backed by memory once touched.
///
/// # Safety
/// The bytes must make valid `T`s: zero bytes for a fresh mapping (true for
/// integers and atomics, the only callers' types), any for `u8`.
pub unsafe fn mapped<T>(n: usize, fd: i32) -> &'static mut [T] {
    // MAP_PRIVATE, and for no file MAP_ANONYMOUS | MAP_NORESERVE (address
    // space without a promise of memory: most of it is never touched).
    let how = if fd < 0 { 0x02 | 0x20 | 0x4000 } else { 0x02 };
    // SAFETY: a new readable, writable mapping, which aliases nothing of ours.
    let bytes = (n * size_of::<T>()).max(1);
    let base = unsafe { mmap(std::ptr::null_mut(), bytes, 3, how, fd, 0) };
    if base as isize == -1 {
        die(&format!("mmap: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: bytes for `n` values, never unmapped or aliased, valid as `T`s
    // (the caller's promise).
    unsafe { std::slice::from_raw_parts_mut(base.cast(), n) }
}

/// Faults in the pages of `mem` (from `mapped`) without changing
/// them (MADV_POPULATE_WRITE): one call for the whole range, which the
/// kernel walks far faster than it takes one page fault after another. Only
/// a hint: on a kernel without the call (before 5.14) the pages fault in
/// when first used.
pub fn populate<T>(mem: &[T]) {
    // SAFETY: the range is our own mapping, and populating leaves its bytes
    // as they are.
    unsafe { madvise(mem.as_ptr().cast(), size_of_val(mem), 23) };
}
