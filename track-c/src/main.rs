//! Speed Demon 2026, Track C (Join): parses the hostile /data/records.csv,
//! joins it to /data/regions.csv and writes /out/aggregates.txt.
//! README.md and PERF.md explain the design; the spec is specs/c-join.md.
//!
//! Usage: `track-c [DATA_DIR [OUT_DIR]]`, by default `/data` and `/out`.
//!
//! Errors come in two kinds, and everything is on stderr. Whatever the input
//! files or the system can cause and nothing can continue from (a file that
//! cannot be read or mapped, a thread that will not start, a regions.csv line
//! that is not `id,CCC`, a limit passed, a records.csv that breaks the grammar
//! for good) goes to `die`: one line, exit status 1, no output file. The record
//! parser and `run::run` instead return `Res<T>`, the buffer offset near which
//! a chunk broke the grammar or a guessed chunk cut was not proven. Such an
//! offset is not yet an error: `main` parses the file once more as one chunk
//! (a note on stderr, exit status 0 if that works), and only a second failure
//! is reported, through `die`.

// The SIMD needs AVX2, and the parser and name hash PCLMULQDQ and AES-NI;
// .cargo/config.toml turns them on for the whole crate (x86-64-v3 and the other
// two, all of them on every CPU since Haswell); refuse any other build.
#[cfg(not(target_feature = "avx2"))]
compile_error!("track-c needs AVX2: build with -C target-cpu=x86-64-v3 (see .cargo/config.toml)");
#[cfg(not(all(target_feature = "aes", target_feature = "pclmulqdq")))]
compile_error!("track-c needs AES and PCLMULQDQ: see .cargo/config.toml");

mod names;
mod parse;
mod run;

use std::arch::{asm, x86_64::*};
use std::{env, fs, process, sync::Mutex};

/// Bytes per chunk of records.csv, read into a worker's buffer and parsed
/// while it is still in that core's L2 cache.
const PIECE: usize = 64 << 10;
/// Group numbers the aggregates have room for: any `u8`.
pub const GROUPS: usize = 256;
/// Rows of the name matrix: `_UNMATCHED` and the 240 countries of spec 2.3.
/// A group number from `regions.csv` is below this, or `Regions` stops.
pub const MATRIX_ROWS: usize = 241;

fn main() {
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
    // A single chunk starts at the start of the file: always right. The first
    // pass's offset is discarded (`|_|`) because it cannot say whether the
    // file is bad or only a guessed cut was unproven; the second pass's
    // offset, if it fails too, is the file's first real break, and is reported.
    let totals = run::run(PIECE, &file, len, &regions).or_else(|_| {
        eprintln!("track-c: note: chunk cuts unproven; reparsing as one chunk");
        run::run(len.max(1), &file, len, &regions)
    });
    let totals = totals.unwrap_or_else(|at| die(&format!("records.csv breaks spec 2.1 at {at}")));
    let mut text = String::new();
    // `_` sorts after every capital letter, so _UNMATCHED comes last.
    for (key, group) in regions.countries().chain([("_UNMATCHED".into(), 0)]) {
        let [ids, cents, txt, min, max, names, recdist] = totals[usize::from(group)];
        let (records, distinct) = (recdist >> 32, recdist & 0xFFFF_FFFF);
        let (sum, min, max, notes) = (money(cents), money(min), money(max), txt - names);
        let line = format!("{key} {records} {ids} {sum} {min} {max} {distinct} {names} {notes}\n");
        if records > 0 {
            text += &line;
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
pub fn prefetch<T>(s: &[T], i: usize) {
    // SAFETY: a prefetch is only a hint: it never faults, whatever the
    // address, and has no effect on memory.
    unsafe { _mm_prefetch::<_MM_HINT_T0>(s.as_ptr().wrapping_add(i).cast()) }
}

/// Asks the CPU to start loading the cache line of element `i` of `s` for
/// writing (PREFETCHW), so that a later atomic OR there finds the line in
/// this core's cache, already owned. The `_MM_HINT_ET0` intrinsic compiles
/// to a plain read prefetch under our flags, so this is the instruction
/// itself. Harmless like `prefetch`.
pub fn prefetch_write<T>(s: &[T], i: usize) {
    let p = s.as_ptr().wrapping_add(i);
    // SAFETY: a prefetch never faults and changes no memory.
    unsafe { asm!("prefetchw [{0}]", in(reg) p, options(nostack, preserves_flags, readonly)) }
}

// The few kernel calls the standard library does not offer, declared by
// hand (no crates). Nothing mapped here is ever unmapped; it all lives until
// the process exits.
extern "C" {
    fn mmap(at: *mut u8, n: usize, prot: i32, how: i32, fd: i32, offset: i64) -> *mut u8;
    fn madvise(at: *const u8, n: usize, advice: i32) -> i32;
}

/// `n` values of `T` in a new private mapping, page-aligned and never
/// unmapped: the first bytes of the file `fd`, read-only (its pages are
/// already in the page cache, so this costs page-table entries, not copies),
/// or with `fd` -1 zeroed pages that are only backed by memory once touched.
/// Never made huge pages (MADV_NOHUGEPAGE): where the kernel gives huge pages
/// to every mapping that can hold one, a sparse map would be charged 2 MiB
/// for each of them. Only `mapped_huge` asks for them.
///
/// # Safety
/// The bytes must make valid `T`s: zero bytes for a fresh mapping (true for
/// integers and atomics, the only callers' types), any for `u8`; and a
/// file's mapping must never be written. The one caller with a file,
/// `Regions::open`, only reads it, as a `&[u8]` (the `&mut` is not used), and
/// the map is `PROT_READ`, so a stray write would fault, not reach the file.
pub unsafe fn mapped<T>(n: usize, fd: i32) -> &'static mut [T] {
    // The Linux values: MAP_PRIVATE 0x2, MAP_ANONYMOUS 0x20 and MAP_NORESERVE
    // 0x4000 (address space without a promise of memory: most of it is never
    // touched), and PROT_READ 1, PROT_WRITE 2. So 0x4022 is MAP_PRIVATE |
    // MAP_ANONYMOUS | MAP_NORESERVE, read-write (3); a file is MAP_PRIVATE
    // (0x2) and read-only (1).
    let [how, prot] = if fd < 0 { [0x4022, 3] } else { [0x02, 1] };
    let bytes = (n * size_of::<T>()).max(1);
    // SAFETY: the kernel picks the address (null), so the new mapping aliases
    // nothing of ours; a length of 0 would fail, hence the `max(1)`; failure
    // (-1) is checked below.
    let base = unsafe { mmap(std::ptr::null_mut(), bytes, prot, how, fd, 0) };
    if base as isize == -1 {
        die(&format!("mmap: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: our new mapping; the advice changes no bytes. 15 is
    // MADV_NOHUGEPAGE (14, MADV_HUGEPAGE, is used by `mapped_huge`).
    unsafe { madvise(base, bytes, 15) };
    // SAFETY: bytes for `n` values, never unmapped or aliased, valid as `T`s
    // (the caller's promise).
    unsafe { std::slice::from_raw_parts_mut(base.cast(), n) }
}

/// `n` zeroed values of `T` in a new private mapping that starts on a 2 MiB
/// boundary, for which the kernel is asked to use huge pages (MADV_HUGEPAGE),
/// whatever its default: the name table and matrix, which are read and
/// written at random all over. The huge-page range is `n` values rounded up to
/// whole pages; the mapping has up to 2 MiB more address space than that,
/// which stays untouched.
///
/// # Safety
/// Zero bytes must make a valid `T`, as for `mapped`.
pub unsafe fn mapped_huge<T>(n: usize) -> &'static mut [T] {
    const HUGE: usize = 2 << 20;
    let bytes = n * size_of::<T>();
    // Whole huge pages: advice on a range ending inside one would leave that
    // page to 4 KiB pages.
    let pages = bytes.next_multiple_of(HUGE);
    // SAFETY: any bytes are valid `u8`s; the caller's promise covers the `T`s.
    let all: &'static mut [u8] = unsafe { mapped(pages + HUGE, -1) };
    let skip = all.as_ptr().align_offset(HUGE);
    // SAFETY: `skip` is below `HUGE`, so `pages` more are within the mapping.
    let base = unsafe { all.as_mut_ptr().add(skip) };
    // SAFETY: that range of our new mapping; the advice changes no bytes. 14
    // is MADV_HUGEPAGE.
    unsafe { madvise(base, pages, 14) };
    // SAFETY: bytes for `n` values, aligned (a multiple of `HUGE` from the
    // page-aligned start), never unmapped or aliased.
    unsafe { std::slice::from_raw_parts_mut(base.cast(), n) }
}
