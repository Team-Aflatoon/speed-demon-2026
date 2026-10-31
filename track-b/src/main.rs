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
//!   add the result to its running answer.
//!
//! The queue is `scan 0, scan 1, build 0, eval 0, scan 2, build 1, eval 1, ...`,
//! and a task waits only for what it reads: chunk streams are double buffered
//! by segment parity, so the next segment is scanned while one is built and
//! answered, and no thread idles at a phase boundary.

mod index;
mod query;
mod scan;

use index::{Answer, Evaluator, Part, PARTS};
use query::{distinct, Parsed, Table, MAX_QUERY_BYTES, PHRASE, RANGE_SHIFT, SKETCH_BITS};
use scan::{parse_id, read_full, ChunkOut, Scanner, MAX_SEGMENT_DOCS};
use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::*};
use std::sync::{Barrier, Mutex, OnceLock, RwLock};

const MAX_THREADS: usize = 12;
const SEGMENT_BYTES: u64 = 8 << 20;
const CHUNK_BYTES: u64 = 512 << 10;

fn main() {
    disable_huge_pages();
    run("/data", "/out");
}

/// Prints `msg` and exits with status 1. Every input and I/O error ends here.
/// `exit` must not run on two threads at once, so only the first caller
/// reports; any other thread that fails meanwhile waits for the exit.
pub fn fail(msg: &str) -> ! {
    static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
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
/// the locks are never contended for long.
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
    let mut queries = std::fs::read(format!("{data}/queries.txt"))
        .unwrap_or_else(|e| fail(&format!("queries.txt: {e}")));
    if queries.len() >= MAX_QUERY_BYTES {
        fail("queries.txt is larger than 1 GB");
    }
    queries.extend_from_slice(&[0; 16]); // keys read 12 bytes from a term's start
    let queries = &queries[..];
    let text_len = queries.len() - 16;
    let file =
        File::open(format!("{data}/docs.txt")).unwrap_or_else(|e| fail(&format!("docs.txt: {e}")));
    let segments = plan(&file);
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_THREADS);
    let ranges: Vec<usize> = (0..=threads)
        .map(|i| line_start(&queries[..text_len], text_len * i / threads))
        .collect();
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
    let words = AtomicUsize::new(0);
    let sketches: Vec<Mutex<Vec<u64>>> = many(threads);
    // The table sized from the estimate, and the one sized for every word to
    // be distinct, used only if an insert into the first ran out of probes.
    let (table, fallback, overflow) = (OnceLock::new(), OnceLock::new(), AtomicBool::new(false));
    let parsed: Vec<OnceLock<Parsed>> = (0..threads).map(|_| OnceLock::new()).collect();
    let barrier = Barrier::new(threads);
    std::thread::scope(|s| {
        for i in 0..threads {
            let (shared, words, table, parsed, barrier, ranges) =
                (&shared, &words, &table, &parsed, &barrier, &ranges);
            let (sketches, fallback, overflow) = (&sketches, &fallback, &overflow);
            s.spawn(move || {
                // Parse in parallel, size the table, insert in parallel, then anchor.
                let mut mine = Parsed::parse(queries, ranges[i], ranges[i + 1]);
                words.fetch_add(mine.word_count(), Relaxed);
                *sketches[i].lock().unwrap() = std::mem::take(&mut mine.sketch);
                if barrier.wait().is_leader() {
                    let mut sketch = vec![0; SKETCH_BITS / 64];
                    for part in sketches {
                        let part = part.lock().unwrap();
                        sketch
                            .iter_mut()
                            .zip(part.iter())
                            .for_each(|(a, b)| *a |= b);
                    }
                    let estimate = Some(distinct(&sketch));
                    let _ = table.set(Table::new(words.load(Relaxed), estimate, threads));
                }
                barrier.wait();
                let mut table = table.get().unwrap();
                if !mine.insert(table, queries) {
                    overflow.store(true, Relaxed);
                }
                barrier.wait();
                if overflow.load(Relaxed) {
                    if barrier.wait().is_leader() {
                        let _ = fallback.set(Table::new(words.load(Relaxed), None, threads));
                    }
                    barrier.wait();
                    table = fallback.get().unwrap();
                    let inserted = mine.insert(table, queries);
                    assert!(inserted, "a table with free slots takes every word");
                    barrier.wait();
                }
                mine.choose_anchors(table, i);
                *shared.bool_answers[i].lock().unwrap() = vec![Answer::default(); mine.ends.len()];
                *shared.phrase_answers[i].lock().unwrap() =
                    vec![Answer::default(); mine.term_ends.len()];
                let _ = parsed[i].set(mine);
                barrier.wait();
                let parsed: Vec<&Parsed> = parsed.iter().map(|p| p.get().unwrap()).collect();
                work(shared, table, queries, &parsed);
            });
        }
    });
    let out = format!("{out_dir}/results.txt");
    let mut file = File::create(&out).unwrap_or_else(|e| fail(&format!("{out}: {e}")));
    for lines in &shared.lines {
        file.write_all(&lines.lock().unwrap())
            .unwrap_or_else(|e| fail(&format!("{out}: {e}")));
    }
}

/// `n` default values (locks, counters).
fn many<T: Default>(n: usize) -> Vec<T> {
    (0..n).map(|_| T::default()).collect()
}

/// Cuts `docs.txt` into segments at line starts, of about `SEGMENT_BYTES`
/// and at most `MAX_SEGMENT_DOCS` documents.
fn plan(file: &File) -> Vec<Segment> {
    let meta = file.metadata();
    let len = meta.map_or_else(|e| fail(&format!("docs.txt: {e}")), |m| m.len());
    let mut last = [0u8; 1];
    if len > 0 && (read_full(file, &mut last, len - 1) != 1 || last[0] != b'\n') {
        fail("docs.txt must end with an LF");
    }
    if len >= 1 << 32 {
        fail("docs.txt is larger than 4 GB");
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
    parse_id(&id[..n])
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

/// The first line start of `text` at or after `at`.
fn line_start(text: &[u8], at: usize) -> usize {
    match at {
        0 => 0,
        _ => text[at - 1..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(text.len(), |i| at + i),
    }
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
        let streams = chunks.iter().map(|c| c.streams[p].as_slice());
        let mut part = sh.parts[p].write().unwrap();
        return part.build(streams, terms, docs as usize, cursor);
    }
    let mut answers: Vec<_> = sh
        .phrase_answers
        .iter()
        .map(|a| a.lock().unwrap())
        .collect();
    for &(phrase, doc) in chunks.iter().flat_map(|c| &c.matches) {
        let (range, i) = (phrase >> RANGE_SHIFT, phrase & ((1 << RANGE_SHIFT) - 1));
        answers[range as usize][i as usize].push(doc);
    }
}

/// Checks that the chunks of segment `s` hold consecutive document ids that
/// end where the next segment starts, and returns the segment's document count.
fn check_ids(
    segments: &[Segment],
    s: usize,
    chunks: &[impl std::ops::Deref<Target = ChunkOut>],
) -> u32 {
    let mut next = segments[s].first;
    for c in chunks.iter().filter(|c| c.lines > 0) {
        if c.first != next {
            fail("document ids must be 0, 1, 2, ... in order");
        }
        next += c.lines;
    }
    if segments.get(s + 1).is_some_and(|n| n.first != next) || (s == 0 && segments[0].first != 0) {
        fail("document ids must be 0, 1, 2, ... in order");
    }
    next - segments[s].first
}
