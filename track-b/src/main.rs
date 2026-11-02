//! Speed Demon 2026, Track B: answers the BOOL and PHRASE queries of
//! `/data/queries.txt` over `/data/docs.txt` and writes `/out/results.txt`.
//!
//! The queries are read first, so only their terms are ever indexed. The
//! corpus is cut into segments of about 14 MB, each into chunks of 512 KB, and
//! twelve threads run one queue of tasks, in this order:
//!
//! - scan (per chunk): tokenize, look every token up in the term table, emit
//!   each document's BOOL terms into 64 partition streams, and check the
//!   phrases anchored at its tokens (`scan.rs`);
//! - build (per partition): counting-sort the streams of a segment into
//!   sorted lists and bitmaps (`index.rs`); one extra build task folds the
//!   segment's phrase matches into the phrase answers;
//! - eval (per half of a query range): answer every BOOL query against the
//!   segment and add the result to its running answer;
//! - format (per half of a query range, after its last eval): write its
//!   answer lines.
//!
//! The queue is `scan 0, scan 1, build 0, eval 0, scan 2, build 1, eval 1, ...`,
//! and a task waits only for what it reads: chunk streams are double buffered
//! by segment parity, so the next segment is scanned while one is built and
//! answered, and no thread idles at a phase boundary.

mod index;
mod query;
mod scan;

use index::{Answer, Evaluator, Part, PARTS};
use query::{Parsed, Setup, Table, PHRASE, QUERY_PAD, RANGE_SHIFT};
use scan::{parse_id, read_full, ChunkOut, Scanner, MAX_SEGMENT_DOCS};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{OnceLock, RwLock};

const MAX_THREADS: usize = 12;
const SEGMENT_BYTES: u64 = 14 << 20;
const CHUNK_BYTES: u64 = 512 << 10;

/// Ranges are named by `range << RANGE_SHIFT` in a u32 (see `query.rs`).
const _: () = assert!(MAX_THREADS <= 1 << (32 - RANGE_SHIFT));
/// A term's use count has seven bits (see `query.rs`).
const _: () = assert!(query::USES_CAP + MAX_THREADS as u32 <= 127);

/// `track-b [data dir] [out dir]`, by default `/data` and `/out`.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, default: &'static str| args.get(i).map_or(default, String::as_str);
    run(arg(1, "/data"), arg(2, "/out"));
}

/// Prints `msg` and exits with status 1. Every input and I/O error ends here.
/// `exit` must not run on two threads at once, so only the first caller
/// reports; any other thread that fails meanwhile waits for the exit.
pub fn fail(msg: &str) -> ! {
    static FAILED: AtomicBool = AtomicBool::new(false);
    while FAILED.swap(true, AcqRel) {
        std::thread::park();
    }
    eprintln!("track-b: {msg}");
    std::process::exit(1)
}

/// `value`, or the end of the run with `what` and the error.
fn or_fail<T, E: std::fmt::Display>(value: Result<T, E>, what: &str) -> T {
    value.unwrap_or_else(|e| fail(&format!("{what}: {e}")))
}

extern "C" {
    fn prctl(option: i32, ...) -> i32;
    pub fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    pub fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
}

/// Starts loading the cache line at `p`, so misses can overlap.
#[inline(always)]
pub fn prefetch<T>(p: *const T) {
    // SAFETY: a prefetch is only a hint; it never faults or changes memory.
    unsafe { std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(p.cast()) }
}

/// A run of whole lines of `docs.txt`, starting at document `first`.
struct Segment {
    start: u64,
    end: u64,
    first: u32,
}

impl Segment {
    /// The number of its `CHUNK_BYTES` chunks.
    fn chunks(&self) -> usize {
        (self.end - self.start).div_ceil(CHUNK_BYTES) as usize
    }
}

#[derive(Clone, Copy)]
enum Task {
    Scan(usize, usize),
    Build(usize, usize),
    Eval(usize, usize),
    Format(usize),
}

/// Per segment: how many of its scan, build and eval tasks are done, and
/// (once it is scanned) its number of documents.
const SCANNED: usize = 0;
const BUILT: usize = 1;
const EVALUATED: usize = 2;
const DOCS: usize = 3;

/// Everything the tasks share. Each lock is taken by one writer task or by
/// readers, never both at once: the done counters order the tasks (a task
/// only starts once every task it reads from has counted itself done), so
/// the locks are never contended for long. The `unwrap`s on locks and on
/// `OnceLock::get` cannot fire: with `panic = "abort"` a panic ends the
/// process, so no lock is ever poisoned, and every `OnceLock` is set before
/// the barrier that precedes its reads.
struct Shared {
    file: File,
    segments: Vec<Segment>,
    tasks: Vec<Task>,
    next: AtomicUsize,
    /// Per segment, the counters `SCANNED`, `BUILT`, `EVALUATED`, `DOCS`.
    counts: Vec<[AtomicU32; 4]>,
    /// Chunk outputs of the even and the odd segments.
    chunks: [Vec<RwLock<ChunkOut>>; 2],
    parts: Vec<RwLock<Part>>,
    /// The BOOL answers of range `r` in two halves, `2r` and `2r + 1`, each
    /// evaluated by its own task; `halves[h]` counts the segments that half
    /// `h` has evaluated.
    bool_answers: Vec<RwLock<Vec<Answer>>>,
    halves: Vec<AtomicU32>,
    phrase_answers: Vec<RwLock<Vec<Answer>>>,
    /// `results.txt`, which the format tasks write in order, two per range:
    /// task `h` takes `out_at` once `formatted` reaches `h`, and moves it on.
    out: File,
    out_at: AtomicU64,
    formatted: AtomicU32,
}

fn run(data: &str, out_dir: &str) {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(MAX_THREADS));
    let open = |name: &str| or_fail(File::open(format!("{data}/{name}")), name);
    let setup = Setup::new(map_queries(&open("queries.txt")), threads);
    // The table first, alone: its huge pages must be populated before any other
    // thread exists. Then transparent huge pages go off for the rest of the
    // process (PR_SET_THP_DISABLE), so partly used 2 MB heap regions are not
    // backed (and counted in our memory peak) as whole huge pages. It is only
    // a hint, so a failure is ignored.
    setup.make_table();
    // SAFETY: option 41 is PR_SET_THP_DISABLE; it takes integer arguments and
    // touches no memory of ours.
    unsafe { prctl(41, 1u64, 0u64, 0u64, 0u64) };
    let shared: OnceLock<Shared> = OnceLock::new();
    let parsed: Vec<OnceLock<Parsed>> = many(threads);
    std::thread::scope(|s| {
        let worker = |i: usize| {
            let (mine, table) = setup.run(i);
            let shared = shared.wait();
            let answers = |n: usize| vec![Answer::default(); n];
            let n = mine.ends.len();
            *shared.bool_answers[2 * i].write().unwrap() = answers(n / 2);
            *shared.bool_answers[2 * i + 1].write().unwrap() = answers(n - n / 2);
            *shared.phrase_answers[i].write().unwrap() = answers(mine.term_ends.len());
            let _ = parsed[i].set(mine);
            setup.barrier.wait();
            let parsed: Vec<&Parsed> = parsed.iter().map(|p| p.get().unwrap()).collect();
            work(shared, table, setup.text, &parsed);
        };
        (0..threads).for_each(|i| drop(s.spawn(move || worker(i))));
        let file = open("docs.txt");
        let out = File::create(format!("{out_dir}/results.txt"));
        let out = or_fail(out, "results.txt");
        let segments = plan(&file);
        let max_chunks = segments.iter().map(Segment::chunks).max().unwrap_or(0);
        let _ = shared.set(Shared {
            tasks: schedule(&segments, threads),
            next: AtomicUsize::new(0),
            counts: many(segments.len()),
            chunks: [0, 1].map(|_| many(max_chunks)),
            parts: many(PARTS),
            bool_answers: many(2 * threads),
            halves: many(2 * threads),
            phrase_answers: many(threads),
            out_at: AtomicU64::new(0),
            formatted: AtomicU32::new(0),
            file,
            out,
            segments,
        });
    });
}

/// `queries.txt` mapped read-only, and `QUERY_PAD` zero bytes after it: the
/// file is mapped over a zeroed anonymous mapping a page longer, so the
/// bytes past its end read as zero. Mapping its cached pages costs far less
/// than copying it into a fresh buffer, whose every page faults. Never
/// unmapped.
fn map_queries(file: &File) -> &'static [u8] {
    use std::os::fd::AsRawFd;
    // MAP_PRIVATE (0x02), MAP_FIXED | MAP_POPULATE (0x10 | 0x8000), MAP_ANONYMOUS (0x20).
    let (private, fixed_populate, anonymous) = (0x02, 0x10 | 0x8000, 0x20);
    let len = or_fail(file.metadata(), "queries.txt").len() as usize;
    assert!(len < query::MAX_QUERY_BYTES, "queries.txt over 1 GB");
    let span = len.div_ceil(4096) * 4096 + 4096;
    let failed = |p: *mut u8| p as isize == -1;
    // SAFETY: new mappings at an address of the kernel's choosing (the file
    // over the start of the anonymous one) touch no memory of ours.
    unsafe {
        let base = mmap(std::ptr::null_mut(), span, 1, private | anonymous, -1, 0);
        let over = |base| mmap(base, len, 1, private | fixed_populate, file.as_raw_fd(), 0);
        if failed(base) || (len > 0 && failed(over(base))) {
            fail("cannot map queries.txt");
        }
        // SAFETY: `span >= len + QUERY_PAD` readable bytes, never unmapped;
        // the inputs are read-only, so the file does not change under it.
        std::slice::from_raw_parts(base, len + QUERY_PAD)
    }
}

/// `n` default values (locks, counters).
fn many<T: Default>(n: usize) -> Vec<T> {
    (0..n).map(|_| T::default()).collect()
}

/// Cuts `docs.txt` into segments at line starts, of about `SEGMENT_BYTES`
/// and at most `MAX_SEGMENT_DOCS` documents.
fn plan(file: &File) -> Vec<Segment> {
    let len = or_fail(file.metadata(), "docs.txt").len();
    let mut last = [0u8; 1];
    if len > 0 && (read_full(file, &mut last, len - 1) != 1 || last[0] != b'\n') {
        fail("docs.txt must end with an LF");
    }
    let docs = if len > 0 { last_id(file, len) + 1 } else { 0 };
    // The first line start at or after `at` and its id (the end of the file
    // counts as the line after the last, whose id is `docs`).
    let line = |at: u64| match next_line(file, len, at) {
        end if end == len => (len, docs),
        start => (start, id_at(file, start)),
    };
    let (mut segments, mut start) = (Vec::new(), 0);
    let mut first = if len > 0 { id_at(file, 0) } else { 0 };
    while start < len {
        // Many short documents: halve the size until the segment holds at
        // most `MAX_SEGMENT_DOCS` (one line always does).
        let mut size = SEGMENT_BYTES;
        let (mut end, mut next) = line(start + size);
        while next - first > MAX_SEGMENT_DOCS {
            size = (size / 2).max(1);
            (end, next) = line(start + size);
        }
        segments.push(Segment { start, end, first });
        (start, first) = (end, next);
    }
    segments
}

/// The id of the line starting at `at`.
fn id_at(file: &File, at: u64) -> u32 {
    let mut id = [0u8; 16];
    let n = read_full(file, &mut id, at);
    parse_id(&id[..n], at)
}

/// The id of the last line of the file (`len > 0` bytes, ending with an LF).
fn last_id(file: &File, len: u64) -> u32 {
    let mut end = len - 1; // the final LF
    loop {
        let (from, mut buf) = (end.saturating_sub(4096), [0u8; 4096]);
        let n = read_full(file, &mut buf[..(end - from) as usize], from);
        match buf[..n].iter().rposition(|&b| b == b'\n') {
            Some(i) => return id_at(file, from + i as u64 + 1),
            None if from == 0 => return id_at(file, 0),
            None => end = from,
        }
    }
}

/// The first line start at or after `at` (or the end of the file).
fn next_line(file: &File, len: u64, at: u64) -> u64 {
    let (mut buf, mut pos) = ([0u8; 4096], at - 1);
    while pos < len {
        let n = read_full(file, &mut buf, pos);
        if let Some(i) = buf[..n].iter().position(|&b| b == b'\n') {
            return pos + i as u64 + 1;
        }
        pos += n as u64;
    }
    len
}

/// The task queue: see the module comment.
fn schedule(segments: &[Segment], ranges: usize) -> Vec<Task> {
    let mut tasks = Vec::new();
    for s in 0..=segments.len() {
        if let Some(segment) = segments.get(s) {
            tasks.extend((0..segment.chunks()).map(|c| Task::Scan(s, c)));
        }
        if let Some(s) = s.checked_sub(1) {
            tasks.extend((0..=PARTS).map(|p| Task::Build(s, p)));
            tasks.extend((0..2 * ranges).map(|h| Task::Eval(s, h)));
        }
    }
    tasks.extend((0..2 * ranges).map(Task::Format));
    tasks
}

/// Each BOOL query prefetches the lists of the query this far ahead, and the
/// term entries of the one twice as far.
const AHEAD: usize = 4;

/// Waits until `count` reaches `n`; the Acquire pairs with the Release of
/// the tasks that counted themselves done, so their writes are visible.
/// Most waits are short, so it first spins with `PAUSE`, which leaves the
/// core to its other hardware thread, and only then yields to the kernel.
fn wait(count: &AtomicU32, n: usize) {
    for spins in 0.. {
        if count.load(Acquire) as usize >= n {
            return;
        }
        match spins < 2000 {
            true => std::hint::spin_loop(),
            false => std::thread::yield_now(),
        }
    }
}

/// One worker: takes tasks from the queue in order until it is empty. Each
/// task's locks are released before it counts itself done, with one exception:
/// an eval task bumps its `halves` counter while still holding its answers'
/// write lock (a format task that sees the count then waits briefly for the
/// read lock).
///
/// What each task waits for (`s` a segment):
///
/// | task     | waits until                   | because it                          |
/// |----------|-------------------------------|-------------------------------------|
/// | scan s   | build s - 2 is done           | reuses the chunk buffers of s - 2   |
/// | build s  | scan s and eval s - 1 are done | reads s's chunks, rewrites `parts`  |
/// | eval s   | build s is done               | reads `parts`                       |
/// | format r | r's last evals are done       | reads the finished answers          |
///
/// It cannot deadlock: tasks are claimed in queue order and a task waits only
/// on tasks earlier in the queue, so the oldest unfinished task waits on
/// nothing. It is always running, and so the queue always drains.
fn work(sh: &Shared, table: &Table, queries: &[u8], parsed: &[&Parsed]) {
    let mut scanner = Scanner::new(table, queries, parsed);
    let mut eval = Evaluator::default();
    let (terms, ranges) = (table.ids().div_ceil(PARTS), parsed.len());
    let count = |s: usize, which: usize| &sh.counts[s][which];
    while let Some(&task) = sh.tasks.get(sh.next.fetch_add(1, Relaxed)) {
        let done = match task {
            Task::Scan(s, c) => {
                if s >= 2 {
                    wait(count(s - 2, BUILT), PARTS + 1); // the build of s - 2 read this buffer
                }
                let seg = &sh.segments[s];
                let at = |c: usize| (seg.start + c as u64 * CHUNK_BYTES).min(seg.end);
                let out = &mut *sh.chunks[s % 2][c].write().unwrap();
                scanner.scan(&sh.file, (at(c), at(c + 1)), seg.first, out);
                count(s, SCANNED)
            }
            Task::Build(s, p) => {
                wait(count(s, SCANNED), sh.segments[s].chunks());
                if s >= 1 {
                    wait(count(s - 1, EVALUATED), 2 * ranges); // the eval of s - 1 read `parts`
                }
                build(sh, s, p, terms);
                count(s, BUILT)
            }
            Task::Eval(s, h) => {
                wait(count(s, BUILT), PARTS + 1);
                let guards: Vec<_> = sh.parts.iter().map(|p| p.read().unwrap()).collect();
                let parts: Vec<&Part> = guards.iter().map(|p| &**p).collect();
                let (docs, base) = (count(s, DOCS).load(Relaxed), sh.segments[s].first);
                let (r, mut answers) = (h / 2, sh.bool_answers[h].write().unwrap());
                let first = parsed[r].ends.len() / 2 * (h % 2); // the half's first query
                let queries = first + answers.len();
                for (q, answer) in (first..).zip(answers.iter_mut()) {
                    if q + 2 * AHEAD < queries {
                        index::prefetch(&parts, parsed[r].bool_steps(q + 2 * AHEAD), false);
                    }
                    if q + AHEAD < queries {
                        index::prefetch(&parts, parsed[r].bool_steps(q + AHEAD), true);
                    }
                    eval.run(&parts, parsed[r].bool_steps(q), docs, base, answer);
                }
                sh.halves[h].fetch_add(1, Release);
                count(s, EVALUATED)
            }
            Task::Format(h) => {
                let r = h / 2;
                // Once both halves of the range have evaluated every segment.
                wait(&sh.halves[2 * r], sh.segments.len());
                wait(&sh.halves[2 * r + 1], sh.segments.len());
                // The read buffer is no longer needed, and its pages are mapped.
                let out = &mut scanner.buf;
                out.clear();
                let low = sh.bool_answers[2 * r].read().unwrap();
                let high = sh.bool_answers[2 * r + 1].read().unwrap();
                let phrases = sh.phrase_answers[r].read().unwrap();
                let order = &parsed[r].order;
                let (lines, half) = (order.len(), order.len() / 2);
                for &entry in &order[[0, half][h % 2]..[half, lines][h % 2]] {
                    let (i, split) = ((entry & !PHRASE) as usize, low.len());
                    match entry & PHRASE {
                        0 if i < split => low[i].write(out),
                        0 => high[i - split].write(out),
                        _ => phrases[i].write(out),
                    }
                }
                // No lock is held while waiting for the tasks before this one.
                drop((low, high, phrases));
                wait(&sh.formatted, h); // the halves before this one took their bytes
                let at = sh.out_at.load(Acquire);
                sh.out_at.store(at + out.len() as u64, Release);
                sh.formatted.fetch_add(1, Release);
                or_fail(sh.out.write_all_at(out, at), "results.txt");
                continue;
            }
        };
        done.fetch_add(1, Release);
    }
    // Freeing the scratch would unmap memory while the other threads run,
    // which costs every core a TLB flush; the process is about to exit and
    // frees it all at once.
    std::mem::forget((scanner, eval));
}

/// Build task `p` of segment `s`: partition `p`'s postings, or (`p == PARTS`)
/// the segment's phrase matches folded into the phrase answers, in document
/// order. Each records the segment's document count, the chunks' lines.
fn build(sh: &Shared, s: usize, p: usize, terms: usize) {
    let chunks = &sh.chunks[s % 2][..sh.segments[s].chunks()];
    let chunks: Vec<_> = chunks.iter().map(|c| c.read().unwrap()).collect();
    let docs: u32 = chunks.iter().map(|c| c.lines).sum();
    sh.counts[s][DOCS].store(docs, Relaxed);
    if p < PARTS {
        let streams: Vec<&[u32]> = chunks.iter().map(|c| c.stream(p)).collect();
        let mut part = sh.parts[p].write().unwrap();
        return part.build(&streams, terms, docs as usize);
    }
    for &(phrase, doc) in chunks.iter().flat_map(|c| &c.matches) {
        let (range, i) = (phrase >> RANGE_SHIFT, phrase & ((1 << RANGE_SHIFT) - 1));
        sh.phrase_answers[range as usize].write().unwrap()[i as usize].push(doc);
    }
}
