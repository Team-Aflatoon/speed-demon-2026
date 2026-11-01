//! The parallel engine. The workers first build the region map and fault in
//! the name table, then claim chunks of records.csv until none are left
//! (phase 1), then claim name buckets until none are left (phase 2, see
//! names.rs). A worker waits on the others only twice: for the start-up
//! work, and at the end of phase 1.
//!
//! Choosing the chunk cuts is where the quote-parity problem lives; see `run`.

use crate::names::{Counts, Pairs, Table};
use crate::parse::{header, merge, Agg, Fail, Local, Parser, Res, Span, EMPTY, PAD};
use crate::regions::Regions;
use crate::{die, GROUPS};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, Scope};

/// The most worker threads (the scoring machine gives us twelve).
const MAX_THREADS: usize = 12;
/// Pieces regions.csv is parsed in at start-up.
const REGION_PARTS: usize = 32;
/// Stack size for workers, which need about 40 KiB; a small stack can never
/// be given a huge page.
const STACK: usize = 256 << 10;
/// Bytes read past a chunk's nominal end to find its real end, at first
/// (records average 61 bytes); a longer last record doubles it until found.
const SLACK: usize = 1 << 10;

/// One output line's numbers: the parser's (see `Agg`), then phase 2's.
pub type Total = (Agg, Counts);

/// Everything the workers share. Chunk c covers nominally the `piece` bytes
/// from `c * piece` (see `read`); `next` is the next one to claim, and
/// `quotes[c]` the quote bytes found in chunk c. `fail` is the worst failure
/// so far.
pub struct Shared<'a> {
    file: &'a File,
    len: usize,
    piece: usize,
    pub regions: &'a Regions,
    pub table: Table,
    next: AtomicUsize,
    quotes: Vec<AtomicU64>,
    fail: Mutex<Option<Fail>>,
}

/// Counts the whole file with one worker per CPU (at most 12), in chunks of
/// nominally `piece` bytes, with a name table of `slots` short slots: totals
/// by group.
///
/// records.csv is cut into chunks just after an LF that is followed by a
/// digit or a BOM byte. By spec 2.2 no quoted value holds a digit or a
/// non-ASCII byte, so such an LF ends a record. That guess is checked, not
/// trusted. Each chunk is parsed as if it started outside quotes, and counts
/// its quote bytes. A position is inside quotes exactly when an odd number of
/// quotes precede it, so once every chunk has an even number of quotes before
/// it, each one did start outside quotes, right after a terminator, and was
/// parsed exactly as one sequential pass would have parsed it. If not, this
/// returns `Fail::Malformed`, and the caller retries with a single chunk.
pub fn run(piece: usize, slots: usize, file: &File, regions: &Regions) -> Res<Vec<Total>> {
    let threads = thread::available_parallelism().map_or(1, usize::from).min(MAX_THREADS);
    let len = file.metadata().map_or(0, |m| m.len()) as usize; // main has checked it
    let table = Table::new(slots).unwrap_or_else(|e| die(&format!("memory: {e}")));
    let quotes = (0..len.div_ceil(piece).max(1)).map(|_| AtomicU64::new(0)).collect();
    let (next, fail) = (AtomicUsize::new(0), Mutex::new(None));
    let sh = Shared { file, len, piece, regions, table, next, quotes, fail };
    // The start-up tasks: parse each piece of regions.csv, then fault in each
    // stretch of the name table. Whichever workers are up first claim them,
    // and none waits for the last thread to start, only for the region map
    // (faulting in only saves the first lookups their page faults).
    let stretches = sh.table.stretches();
    let (next_task, pending) = (AtomicUsize::new(0), AtomicUsize::new(REGION_PARTS));
    let (parsing, next_bucket) = (AtomicUsize::new(threads), AtomicUsize::new(0));
    let all_pairs: Vec<OnceLock<Pairs>> = (0..threads).map(|_| OnceLock::new()).collect();
    let total = Mutex::new(vec![(EMPTY, [0; 2]); GROUPS]);
    let buckets = sh.table.buckets();
    let work = |t: usize| {
        let pairs = Pairs::new(buckets, len).unwrap_or_else(|e| die(&format!("memory: {e}")));
        loop {
            match next_task.fetch_add(1, Relaxed) {
                k if k < REGION_PARTS => regions.parse(k, REGION_PARTS, &pending),
                k if k < REGION_PARTS + stretches => sh.table.touch(k - REGION_PARTS),
                _ => break,
            }
        }
        // Most of this worker's pairs, at 2 bytes for a record of about 61:
        // faulted in now, while the start-up tasks finish.
        pairs.touch(len / 32 / threads);
        wait(&pending);
        let aggs = [EMPTY; GROUPS];
        let mut local = Local { aggs, pairs };
        sh.work(&mut local);
        _ = all_pairs[t].set(local.pairs);
        parsing.fetch_sub(1, Release);
        wait(&parsing);
        // Phase 2, unless phase 1 failed somewhere; then nothing is counted.
        let mut counts = [[0; GROUPS]; 2];
        if !sh.failed() {
            let all: Vec<&Pairs> = all_pairs.iter().filter_map(OnceLock::get).collect();
            let mut bits = Box::new([0; 1024]);
            while let Some(b) = Some(next_bucket.fetch_add(1, Relaxed)).filter(|&b| b < buckets) {
                sh.table.count(&all, b, &mut bits, &mut counts);
            }
        }
        let mut total = total.lock().unwrap();
        for (g, (t, a)) in total.iter_mut().zip(&local.aggs).enumerate() {
            merge(&mut t.0, a);
            (t.1[0], t.1[1]) = (t.1[0] + counts[0][g], t.1[1] + counts[1][g]);
        }
    };
    thread::scope(|s| grow(s, 0, threads, &work));
    let mut before = 0;
    for (c, q) in sh.quotes.iter().enumerate() {
        if before % 2 == 1 {
            sh.fail(Fail::Malformed(c * piece));
        }
        before += q.load(Relaxed);
    }
    let fail = sh.fail.into_inner().unwrap();
    fail.map_or(Ok(total.into_inner().unwrap()), Err)
}

/// Waits, yielding rather than spinning, so that a crowded machine can run
/// the workers still busy, until `left` reaches 0.
fn wait(left: &AtomicUsize) {
    while left.load(Acquire) > 0 {
        thread::yield_now();
    }
}

/// Starts workers `2t + 1` and `2t + 2` (and, through them, the rest of the
/// `n`), then runs worker `t` on this thread. Starting threads in a tree
/// takes about log2(n) thread starts before the last worker runs, not n.
fn grow<'s, F: Fn(usize) + Sync>(s: &'s Scope<'s, '_>, t: usize, n: usize, work: &'s F) {
    for child in [2 * t + 1, 2 * t + 2].into_iter().filter(|&c| c < n) {
        let builder = thread::Builder::new().stack_size(STACK);
        let spawned = builder.spawn_scoped(s, move || grow(s, child, n, work));
        spawned.unwrap_or_else(|e| die(&format!("starting a thread: {e}")));
    }
    work(t);
}

impl Shared<'_> {
    /// Phase 1 for one worker: claims, reads and parses chunks until none
    /// are left (or some worker has failed: see `fail`).
    fn work(&self, local: &mut Local) {
        let (mut parser, mut buffer) = (Parser::new(), Vec::new());
        let claim = || Some(self.next.fetch_add(1, Relaxed)).filter(|&c| c < self.quotes.len());
        while let Some(c) = claim() {
            let span = self.read(c, &mut buffer);
            match parser.chunk(self, &buffer, span, local) {
                Ok(quotes) => self.quotes[c].store(quotes, Relaxed),
                Err(Fail::Malformed(at)) => self.fail(Fail::Malformed(c * self.piece + at)),
                Err(f) => self.fail(f),
            }
        }
    }

    /// Notes a failure, keeping the worst: a malformed file (with its lowest
    /// offset) over a full name table. No chunk is claimed after one.
    #[cold]
    fn fail(&self, f: Fail) {
        self.next.store(usize::MAX / 2, Relaxed);
        let mut fail = self.fail.lock().unwrap();
        *fail = match (*fail, f) {
            (Some(Fail::Malformed(a)), Fail::Malformed(b)) => Some(Fail::Malformed(a.min(b))),
            (Some(Fail::Malformed(a)), Fail::Full) => Some(Fail::Malformed(a)),
            _ => Some(f),
        };
    }

    /// Whether some worker has failed.
    fn failed(&self) -> bool {
        self.fail.lock().unwrap().is_some()
    }

    /// Reads chunk c into `buffer` with pread, rather than mapping the file:
    /// on the scoring box the page faults, page-table set-up and tear-down of
    /// a 200 MB mapping cost more than copying the bytes. Returns where in
    /// `buffer` the chunk starts and ends, with `PAD` zero bytes after it,
    /// and whether it ends the file.
    ///
    /// The chunk runs from the first record boundary at or after `c * piece`
    /// to the first at or after `(c + 1) * piece` (chunk 0 from the end of
    /// the header, the last one to the end of the file). A boundary is just
    /// past an LF that is followed by a digit or a BOM byte: a guess, checked
    /// as `run` says. Both ends are found from the file's bytes alone, so
    /// neighbouring chunks always agree on the cut between them.
    fn read(&self, c: usize, buffer: &mut Vec<u8>) -> Span {
        let from = c * self.piece;
        let rest = self.len - from;
        let nominal = self.piece.min(rest);
        let mut want = (nominal + SLACK).min(rest);
        loop {
            buffer.resize(want + PAD, 0);
            if let Err(e) = self.file.read_exact_at(&mut buffer[..want], from as u64) {
                die(&format!("records.csv: {e}"));
            }
            let start = match c {
                0 => Some(header(buffer).unwrap_or_else(|| die("records.csv: no header line"))),
                _ => cut(&buffer[..want], 0),
            };
            // The last chunk ends at the end of the file, where no cut is.
            let end = cut(&buffer[..want], nominal).or((want == rest).then_some(want));
            if let Some(end) = end {
                buffer[end..end + PAD].fill(0);
                return (start.unwrap_or(end).min(end), end, end == rest);
            }
            want = (2 * want).min(rest);
        }
    }
}

/// Just past the first LF at or after `from` that is followed by a digit or
/// a BOM byte, if `b` holds one. The byte after the LF must be in `b`, so an
/// LF at its very end is not taken: the caller reads further and asks again.
fn cut(b: &[u8], from: usize) -> Option<usize> {
    let at = |i: usize| b[i] == b'\n' && (b[i + 1].is_ascii_digit() || b[i + 1] == 0xEF);
    (from..b.len().saturating_sub(1)).find(|&i| at(i)).map(|i| i + 1)
}
