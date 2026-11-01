//! Speed Demon 2026, Track B: answers the BOOL and PHRASE queries of
//! `/data/queries.txt` over `/data/docs.txt` and writes `/out/results.txt`.
//!
//! The queries are read first, so only their terms are ever indexed. The
//! corpus is cut into segments of about 8 MB, each into chunks of 512 KB, and
//! twelve threads run one queue of tasks, in this order:
//!
//! - scan (per chunk): tokenize, look every token up in the term table, emit
//!   each document's BOOL terms into 64 partition streams, and check the
//!   phrases anchored at its tokens (`scan.rs`);
//! - build (per partition): counting-sort the streams of a segment into
//!   sorted lists and bitmaps (`index.rs`); one extra build task folds the
//!   segment's phrase matches into the phrase answers;
//! - eval (per query range): answer every BOOL query against the segment and
//!   add the result to its running answer;
//! - format (per query range, after the last segment): write the range's
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
use query::{Parsed, Setup, Table, MAX_QUERY_BYTES, PHRASE, QUERY_PAD, RANGE_SHIFT};
use scan::{parse_id, read_full, ChunkOut, Scanner, MAX_SEGMENT_DOCS};
use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::*};
use std::sync::{Mutex, OnceLock, RwLock, RwLockReadGuard};

const MAX_THREADS: usize = 12;
const SEGMENT_BYTES: u64 = 8 << 20;
const CHUNK_BYTES: u64 = 512 << 10;

/// Ranges are named by `range << RANGE_SHIFT` in a u32 (see `query.rs`).
const _: () = assert!(MAX_THREADS <= 1 << (32 - RANGE_SHIFT));

/// `track-b [data dir] [out dir]`, by default `/data` and `/out`.
fn main() {
    disable_huge_pages();
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, default: &'static str| args.get(i).map_or(default, String::as_str);
    run(arg(1, "/data"), arg(2, "/out"));
}

/// Prints `msg` and exits with status 1. Every input and I/O error ends here.
/// `exit` must not run on two threads at once, so only the first caller
/// reports; any other thread that fails meanwhile waits for the exit.
pub fn fail(msg: &str) -> ! {
    static FAILED: AtomicBool = AtomicBool::new(false);
    if FAILED.swap(true, AcqRel) {
        loop {
            std::thread::park();
        }
    }
    eprintln!("track-b: {msg}");
    std::process::exit(1)
}

extern "C" {
    fn prctl(option: i32, ...) -> i32;
}

/// Turns transparent huge pages off for this process, so partly used 2 MB
/// heap regions are not backed (and counted in our memory peak) as whole huge
/// pages. It is only a hint, so a failure is ignored.
fn disable_huge_pages() {
    const PR_SET_THP_DISABLE: i32 = 41;
    // SAFETY: PR_SET_THP_DISABLE takes integer arguments and touches no memory of ours.
    unsafe { prctl(PR_SET_THP_DISABLE, 1u64, 0u64, 0u64, 0u64) };
}

/// A run of whole lines of `docs.txt`, starting at document `first`.
struct Segment {
    start: u64,
    end: u64,
    first: u32,
    chunks: usize,
}

#[derive(Clone, Copy)]
enum Task {
    Scan(usize, usize),
    Build(usize, usize),
    Eval(usize, usize),
    Format(usize),
}

/// Everything the tasks share. Each lock is taken by one writer task or by
/// readers, never both at once: the done counters order the tasks (a task
/// only starts once every task it reads from has counted itself done), so
/// the locks are never contended for long. The `unwrap`s on locks and on
/// `OnceLock::get` cannot fire: with `panic = "abort"` a panic ends the
/// process, so no lock is ever poisoned, and every `OnceLock` is set before
/// the barrier that precedes its reads.
struct Shared<'a> {
    file: &'a File,
    segments: Vec<Segment>,
    tasks: Vec<Task>,
    next: AtomicUsize,
    /// Chunk outputs of the even and the odd segments.
    chunks: [Vec<RwLock<ChunkOut>>; 2],
    /// Documents in each segment, known once it is scanned.
    docs: Vec<AtomicU32>,
    parts: Vec<RwLock<Part>>,
    bool_answers: Vec<Mutex<Vec<Answer>>>,
    phrase_answers: Vec<Mutex<Vec<Answer>>>,
    lines: Vec<Mutex<Vec<u8>>>,
    scanned: Vec<AtomicU32>,
    built: Vec<AtomicU32>,
    evaluated: Vec<AtomicU32>,
}

fn run(data: &str, out_dir: &str) {
    let (query_path, docs_path) = (format!("{data}/queries.txt"), format!("{data}/docs.txt"));
    let mut queries =
        std::fs::read(&query_path).unwrap_or_else(|e| fail(&format!("{query_path}: {e}")));
    if queries.len() >= MAX_QUERY_BYTES {
        fail(&format!("{query_path} is larger than 1 GB"));
    }
    queries.extend_from_slice(&[0; QUERY_PAD]);
    let queries = &queries[..];
    let file = File::open(&docs_path).unwrap_or_else(|e| fail(&format!("{docs_path}: {e}")));
    let segments = plan(&file, &docs_path);
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_THREADS);
    let (n, max_chunks) = (segments.len(), segments.iter().map(|s| s.chunks).max());
    let shared = Shared {
        file: &file,
        tasks: schedule(&segments, threads),
        next: AtomicUsize::new(0),
        chunks: [0, 1].map(|_| many(max_chunks.unwrap_or(0))),
        docs: many(n),
        parts: many(PARTS),
        bool_answers: many(threads),
        phrase_answers: many(threads),
        lines: many(threads),
        scanned: many(n),
        built: many(n),
        evaluated: many(n),
        segments,
    };
    let setup = Setup::new(queries, threads);
    let parsed: Vec<OnceLock<Parsed>> = many(threads);
    std::thread::scope(|s| {
        for i in 0..threads {
            let (shared, setup, parsed) = (&shared, &setup, &parsed);
            s.spawn(move || {
                let (mine, table) = setup.run(i);
                *shared.bool_answers[i].lock().unwrap() = vec![Answer::default(); mine.ends.len()];
                *shared.phrase_answers[i].lock().unwrap() =
                    vec![Answer::default(); mine.term_ends.len()];
                let _ = parsed[i].set(mine);
                setup.wait();
                let parsed: Vec<&Parsed> = parsed.iter().map(|p| p.get().unwrap()).collect();
                work(shared, table, queries, &parsed);
            });
        }
    });
    let out = format!("{out_dir}/results.txt");
    let mut file = File::create(&out).unwrap_or_else(|e| fail(&format!("{out}: {e}")));
    for lines in &shared.lines {
        if let Err(e) = file.write_all(&lines.lock().unwrap()) {
            let _ = std::fs::remove_file(&out); // no partial results.txt
            fail(&format!("{out}: {e}"));
        }
    }
}

/// `n` default values (locks, counters).
fn many<T: Default>(n: usize) -> Vec<T> {
    (0..n).map(|_| T::default()).collect()
}

/// Cuts `docs.txt` into segments at line starts, of about `SEGMENT_BYTES`
/// and at most `MAX_SEGMENT_DOCS` documents.
fn plan(file: &File, path: &str) -> Vec<Segment> {
    let meta = file.metadata();
    let len = meta.map_or_else(|e| fail(&format!("{path}: {e}")), |m| m.len());
    let mut last = [0u8; 1];
    if len > 0 && (read_full(file, &mut last, len - 1) != 1 || last[0] != b'\n') {
        fail(&format!("{path} must end with an LF"));
    }
    if len == 0 {
        return Vec::new();
    }
    // The first line start at or after `at`, with its id (the end of the file
    // counts as the line after the last).
    let docs = last_id(file, len) + 1;
    let line = |at: u64| match next_line(file, len, at) {
        end if end == len => (len, docs),
        start => (start, id_at(file, start)),
    };
    let mut segments: Vec<Segment> = Vec::new();
    let (mut start, mut first) = (0, id_at(file, 0));
    while start < len {
        let (mut end, mut next) = line(start + SEGMENT_BYTES);
        if next - first > MAX_SEGMENT_DOCS {
            // Many short documents: bisect for the last line start that keeps
            // the segment within u16 document numbers (`start + 1` does).
            let (mut lo, mut hi) = (start + 1, end);
            while lo + 1 < hi {
                let mid = lo + (hi - lo) / 2;
                if line(mid).1 - first <= MAX_SEGMENT_DOCS {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            (end, next) = line(lo);
        }
        let chunks = (end - start).div_ceil(CHUNK_BYTES) as usize;
        segments.push(Segment {
            start,
            end,
            first,
            chunks,
        });
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

/// The id of the last line of the file (`len` bytes, ending with an LF).
fn last_id(file: &File, len: u64) -> u32 {
    let mut end = len - 1; // the final LF
    loop {
        let from = end.saturating_sub(4096);
        let mut buf = [0u8; 4096];
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
    let mut buf = [0u8; 4096];
    let mut pos = at - 1;
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
    let finish = |tasks: &mut Vec<Task>, s: usize| {
        tasks.extend((0..=PARTS).map(|p| Task::Build(s, p)));
        tasks.extend((0..ranges).map(|r| Task::Eval(s, r)));
    };
    for (s, segment) in segments.iter().enumerate() {
        tasks.extend((0..segment.chunks).map(|c| Task::Scan(s, c)));
        if s > 0 {
            finish(&mut tasks, s - 1);
        }
    }
    if !segments.is_empty() {
        finish(&mut tasks, segments.len() - 1);
    }
    tasks.extend((0..ranges).map(Task::Format));
    tasks
}

/// Waits until `count` reaches `n`; the Acquire pairs with the Release of
/// the tasks that counted themselves done, so their writes are visible.
fn wait(count: &AtomicU32, n: usize) {
    while (count.load(Acquire) as usize) < n {
        std::thread::yield_now();
    }
}

/// One worker: takes tasks from the queue in order until it is empty. Each
/// task's locks are released before it counts itself done.
///
/// What each task waits for (`s` a segment):
///
/// | task     | waits until                   | because it                          |
/// |----------|-------------------------------|-------------------------------------|
/// | scan s   | build s - 2 is done           | reuses the chunk buffers of s - 2   |
/// | build s  | scan s and eval s - 1 are done | reads s's chunks, rewrites `parts`  |
/// | eval s   | build s is done               | reads `parts`                       |
/// | format   | the last eval is done         | reads the finished answers          |
///
/// It cannot deadlock: tasks are claimed in queue order and a task waits only
/// on tasks earlier in the queue, so the oldest unfinished task waits on
/// nothing. It is always running, and so the queue always drains.
fn work(sh: &Shared, table: &Table, queries: &[u8], parsed: &[&Parsed]) {
    let mut scanner = Scanner::new(table, queries, parsed);
    let mut eval = Evaluator::default();
    let mut cursor = Vec::new();
    let terms = table.ids().div_ceil(PARTS);
    loop {
        let Some(&task) = sh.tasks.get(sh.next.fetch_add(1, Relaxed)) else {
            return;
        };
        let done = match task {
            Task::Scan(s, c) => {
                if s >= 2 {
                    wait(&sh.built[s - 2], PARTS + 1); // the build of s - 2 read this buffer
                }
                let seg = &sh.segments[s];
                let at = |c: usize| (seg.start + c as u64 * CHUNK_BYTES).min(seg.end);
                let out = &mut *sh.chunks[s % 2][c].write().unwrap();
                scanner.scan(sh.file, (at(c), at(c + 1)), seg.first, out);
                &sh.scanned[s]
            }
            Task::Build(s, p) => {
                wait(&sh.scanned[s], sh.segments[s].chunks);
                if s >= 1 {
                    wait(&sh.evaluated[s - 1], parsed.len()); // the eval of s - 1 read `parts`
                }
                build(sh, s, p, terms, &mut cursor);
                &sh.built[s]
            }
            Task::Eval(s, r) => {
                wait(&sh.built[s], PARTS + 1);
                let guards: Vec<_> = sh.parts.iter().map(|p| p.read().unwrap()).collect();
                let parts: Vec<&Part> = guards.iter().map(|p| &**p).collect();
                let (docs, base) = (sh.docs[s].load(Relaxed) as usize, sh.segments[s].first);
                for (q, answer) in sh.bool_answers[r].lock().unwrap().iter_mut().enumerate() {
                    eval.run(&parts, parsed[r].bool_steps(q), docs, base, answer);
                }
                &sh.evaluated[s]
            }
            Task::Format(r) => {
                if let Some(last) = sh.evaluated.last() {
                    wait(last, parsed.len());
                }
                let bools = sh.bool_answers[r].lock().unwrap();
                let phrases = sh.phrase_answers[r].lock().unwrap();
                let out = &mut sh.lines[r].lock().unwrap();
                for &entry in &parsed[r].order {
                    match entry & PHRASE {
                        0 => bools[entry as usize].write(out),
                        _ => phrases[(entry & !PHRASE) as usize].write(out),
                    }
                }
                continue;
            }
        };
        done.fetch_add(1, Release);
    }
}

/// Build task `p` of segment `s`: partition `p`'s postings, or (`p == PARTS`)
/// the segment's phrase matches folded into the phrase answers, in document order.
fn build(sh: &Shared, s: usize, p: usize, terms: usize, cursor: &mut Vec<u32>) {
    let chunks = &sh.chunks[s % 2][..sh.segments[s].chunks];
    let chunks: Vec<_> = chunks.iter().map(|c| c.read().unwrap()).collect();
    let docs = check_ids(&sh.segments, s, &chunks);
    sh.docs[s].store(docs, Relaxed);
    if p < PARTS {
        let streams: Vec<&[u32]> = chunks.iter().map(|c| &c.streams[p][..]).collect();
        let mut part = sh.parts[p].write().unwrap();
        return part.build(&streams, terms, docs as usize, cursor);
    }
    for &(phrase, doc) in chunks.iter().flat_map(|c| &c.matches) {
        let (range, i) = (phrase >> RANGE_SHIFT, phrase & ((1 << RANGE_SHIFT) - 1));
        sh.phrase_answers[range as usize].lock().unwrap()[i as usize].push(doc);
    }
}

/// Checks that the chunks of segment `s` hold consecutive document ids that
/// end where the next segment starts, and returns the segment's document count.
fn check_ids(segments: &[Segment], s: usize, chunks: &[RwLockReadGuard<ChunkOut>]) -> u32 {
    let first = segments[s].first;
    let mut lines = chunks.iter().filter(|c| c.lines > 0);
    let end = lines.try_fold(first, |next, c| (c.first == next).then_some(next + c.lines));
    match end {
        Some(end)
            if segments.get(s + 1).is_none_or(|n| n.first == end) && (s > 0 || first == 0) =>
        {
            end - first
        }
        _ => fail(&format!(
            "document ids must be 0, 1, 2, ... in order (in docs.txt bytes {}..{})",
            segments[s].start, segments[s].end
        )),
    }
}
