//! The parallel engine: the first workers split regions.csv, then all of
//! them claim chunks of records.csv until none are left. All shared state
//! (the region map, the name table and its pair bitset) is lock-free, and
//! no worker ever waits on another's progress except for regions.csv at the
//! start. Each worker keeps its own aggregates, summed at the end.
//!
//! Choosing the chunk cuts is where the quote-parity problem lives; see `run`.

use crate::map::Mapped;
use crate::names::Names;
use crate::records::{Fail, BOM_FIRST};
use crate::regions::Regions;
use crate::{die, Agg, GROUPS};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering::*};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;
use std::thread::{self, Scope};

/// The most workers that fault in the name tables: one per 2 MiB stretch of
/// them (see `map::touch`).
const TOUCHERS: usize = 4;
/// Stack size for workers, which need about 40 KiB.
const STACK: usize = 256 << 10;
/// Bytes read past a chunk's nominal end to find its real end, at first
/// (records average 61 bytes); a longer last record doubles it until the
/// end is found.
const SLACK: usize = 1 << 10;
/// Zero bytes after a chunk in its buffer. The parser reads at most 64
/// bytes past a chunk's end (one 64-byte block from inside it, or the byte
/// after such a block).
pub const PAD: usize = 128;

/// Everything the workers share: records.csv, the tables records are joined
/// and counted into, and the chunks. Chunk c covers nominally the `piece`
/// bytes from `c * piece` (see `read`); `next` is the next one to claim, and
/// `quotes[c]` the quote bytes found in chunk c. `fail` is the worst `Fail`
/// of any chunk, 0 for none.
pub struct Shared<'a> {
    file: &'a File,
    len: usize,
    piece: usize,
    pub regions: &'a Regions,
    pub names: Names,
    next: AtomicUsize,
    quotes: Vec<AtomicU64>,
    fail: AtomicU8,
}

/// Parses everything with `threads` workers, `chunks` chunks and a name
/// table of `slots` slots, and returns the aggregates by group.
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
pub fn run(
    threads: usize,
    piece: usize,
    slots: usize,
    records: (&File, usize),
    regions: (&Regions, Mapped),
) -> Result<Vec<Agg>, Fail> {
    let ((file, len), piece) = (records, piece.max(1));
    let names = Names::new(slots, len).unwrap_or_else(|e| die(&format!("memory: {e}")));
    let chunks = len.div_ceil(piece).max(1);
    let quotes = (0..chunks).map(|_| AtomicU64::new(0)).collect();
    let (next, fail) = (AtomicUsize::new(0), AtomicU8::new(0));
    let regions_map = regions.0;
    let shared = Shared { file, len, piece, regions: regions_map, names, next, quotes, fail };
    // Before records start, the first workers fault in the name tables (see
    // `Names::touch`) while the others split regions.csv; one worker alone
    // does both.
    let touchers = (threads / 3).clamp(1, TOUCHERS);
    let parsers = (threads - touchers).max(1);
    let waiting = AtomicUsize::new(touchers + parsers);
    let total = Mutex::new(vec![Agg::EMPTY; GROUPS]);
    let work = |t: usize| {
        if t < touchers {
            shared.names.touch(t, touchers);
            waiting.fetch_sub(1, Release);
        }
        if let Some(part) = t.checked_sub(touchers).or((threads == 1).then_some(0)) {
            if let Err(e) = regions.0.parse(regions.1, part, parsers) {
                die(e);
            }
            waiting.fetch_sub(1, Release);
        }
        // Yield rather than spin, so that a crowded machine can run the
        // workers still parsing; on a quiet one this returns at once.
        while waiting.load(Acquire) > 0 {
            thread::yield_now();
        }
        let mut aggs = [Agg::EMPTY; GROUPS];
        shared.work(&mut aggs);
        for (t, p) in total.lock().unwrap().iter_mut().zip(aggs) {
            t.add(p.sums, p.min, p.max);
        }
    };
    thread::scope(|s| grow(s, 0, threads, &work));
    let mut before = 0;
    for q in &shared.quotes {
        if before % 2 == 1 {
            shared.fail.store(Fail::Malformed as u8, Relaxed);
        }
        before += q.load(Relaxed);
    }
    match shared.fail.into_inner() {
        0 => Ok(total.into_inner().unwrap()),
        1 => Err(Fail::Full),
        _ => Err(Fail::Malformed),
    }
}

/// Starts workers `2t + 1` and `2t + 2` (and, through them, the rest of the
/// `n`), then runs worker `t` on this thread. Starting threads in a tree
/// takes about log2(n) thread starts before the last worker runs, not n.
fn grow<'s, F: Fn(usize) + Sync>(s: &'s Scope<'s, '_>, t: usize, n: usize, work: &'s F) {
    for child in [2 * t + 1, 2 * t + 2].into_iter().filter(|&c| c < n) {
        // Small stacks: a 2 MiB one could be given a huge page on a kernel
        // that uses them everywhere.
        let builder = thread::Builder::new().stack_size(STACK);
        let spawned = builder.spawn_scoped(s, move || grow(s, child, n, work));
        spawned.unwrap_or_else(|e| die(&format!("starting a thread: {e}")));
    }
    work(t);
}

impl Shared<'_> {
    /// One worker: claims, reads and parses chunks until none are left.
    fn work(&self, aggs: &mut [Agg; GROUPS]) {
        let mut buffer = Vec::new();
        loop {
            let c = self.next.fetch_add(1, Relaxed);
            if c >= self.quotes.len() {
                return;
            }
            let (start, end) = self.read(c, &mut buffer);
            let eof = c * self.piece + end == self.len;
            match self.chunk(&buffer[start..], end - start, c == 0, eof, aggs) {
                Ok(quotes) => self.quotes[c].store(quotes, Relaxed),
                Err(f) => _ = self.fail.fetch_max(f as u8, Relaxed),
            }
        }
    }

    /// Reads chunk c into `buffer` with pread, rather than mapping the file:
    /// on the scoring box the page faults, page-table set-up and tear-down of
    /// a 200 MB mapping cost more than copying the bytes. Returns where in
    /// `buffer` the chunk starts and ends, with `PAD` zero bytes after it.
    ///
    /// The chunk runs from the first record boundary at or after `c * piece`
    /// to the first at or after `(c + 1) * piece` (chunk 0 from the start of
    /// the file, the last one to its end). A boundary is just past an LF that
    /// is followed by a digit or a BOM byte: a guess, checked as `run` says.
    /// Both ends are found from the file's bytes alone, so neighbouring
    /// chunks always agree on the cut between them.
    fn read(&self, c: usize, buffer: &mut Vec<u8>) -> (usize, usize) {
        let from = c * self.piece;
        let nominal_end = (from + self.piece).min(self.len) - from;
        let mut want = (nominal_end + SLACK).min(self.len - from);
        loop {
            buffer.resize(want + PAD, 0);
            if let Err(e) = self.file.read_exact_at(&mut buffer[..want], from as u64) {
                die(&format!("records.csv: {e}"));
            }
            let whole = from + want == self.len;
            let start = if c == 0 { Some(0) } else { cut(&buffer[..want], 0) };
            let end = match nominal_end == self.len - from {
                true => Some(want),
                false => cut(&buffer[..want], nominal_end),
            };
            if let Some(end) = end.or(whole.then_some(want)) {
                let start = start.unwrap_or(end).min(end);
                buffer[end..end + PAD].fill(0);
                return (start, end);
            }
            want = (2 * want).min(self.len - from);
        }
    }
}

/// Just past the first LF at or after `from` that is followed by a digit or
/// a BOM byte, if `b` holds one. The byte after the LF must be in `b`, so an
/// LF at its very end is not taken: the caller reads further and asks again.
fn cut(b: &[u8], from: usize) -> Option<usize> {
    let boundary = |i: &usize| b[*i] == b'\n' && (b[i + 1].is_ascii_digit() || b[i + 1] == BOM_FIRST);
    (from..b.len().saturating_sub(1)).find(boundary).map(|i| i + 1)
}
