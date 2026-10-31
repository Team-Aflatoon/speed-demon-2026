//! The parallel engine: the first workers split regions.csv, then all of
//! them claim chunks of records.csv until none are left. All
//! shared state (the region map, the name table and its pair bitset) is
//! lock-free; each worker keeps its own aggregates, summed at the end.
//!
//! Choosing the chunk cuts is where the quote-parity problem lives; see `run`.

use crate::map::Mapped;
use crate::names::Names;
use crate::records::{Fail, Input};
use crate::regions::Regions;
use crate::{die, Agg};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::*};
use std::{hint::spin_loop, thread};

/// Chunks per worker. More, smaller chunks even out the workers' finish times.
const CHUNKS_PER_WORKER: usize = 16;
/// Workers that split regions.csv between them. Threads start one after
/// another, so the first few doing it all lets records start sooner than
/// waiting for the last thread to start.
const REGION_WORKERS: usize = 4;

/// Parses everything with `threads` workers and a name table of `slots`
/// slots, and returns the aggregates by group.
///
/// records.csv is cut into chunks just after an LF that is followed by a
/// digit or a BOM byte. By spec 2.2 no quoted value holds a digit or a
/// non-ASCII byte, so such an LF ends a record. That guess is checked, not
/// trusted. Each chunk is parsed as if it started outside quotes, and counts
/// its quote bytes. A position is inside quotes exactly when an odd number of
/// quotes precede it, so once every chunk has an even number of quotes before
/// it, each one did start outside quotes, right after a terminator, and was
/// parsed exactly as one sequential pass would have parsed it. If not, this
/// returns `Fail::Malformed`, and the caller retries with one worker, which
/// parses the file as a single chunk.
pub fn run(threads: usize, slots: usize, records: Mapped, regions: (&Regions, Mapped)) -> Result<Vec<Agg>, Fail> {
    let (b, len) = (records.b, records.len);
    let names = Names::new(slots, len).unwrap_or_else(|e| die(&format!("memory: {e}")));
    let chunks = if threads > 1 { threads * CHUNKS_PER_WORKER } else { 1 };
    let boundary = |i: usize| b[i] == b'\n' && (b[i + 1].is_ascii_digit() || b[i + 1] == 0xEF);
    let job = Job {
        input: Input { b, len, regions: regions.0, names: &names },
        cuts: cuts(len, chunks, boundary),
        next: AtomicUsize::new(0),
        quotes: (0..chunks).map(|_| AtomicU64::new(0)).collect(),
        malformed: AtomicBool::new(false),
        full: AtomicBool::new(false),
    };
    let parsers = threads.min(REGION_WORKERS);
    let waiting = AtomicUsize::new(parsers);
    let work = |t: usize| {
        if t < parsers {
            if let Err(e) = regions.0.parse(regions.1, t, parsers) {
                die(&format!("regions.csv: {e}"));
            }
            waiting.fetch_sub(1, Release);
        }
        // Spin rather than block: every worker has a CPU of its own, and a
        // futex wake-up would take longer than the wait.
        while waiting.load(Acquire) > 0 {
            spin_loop();
        }
        job.work()
    };
    let parts: Vec<[Agg; 256]> = thread::scope(|s| {
        let workers: Vec<_> = (0..threads).map(|t| s.spawn(move || work(t))).collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    let mut before = 0;
    for q in &job.quotes {
        job.malformed.fetch_or(before % 2 == 1, Relaxed);
        before += q.load(Relaxed);
    }
    if job.malformed.into_inner() {
        return Err(Fail::Malformed);
    }
    if job.full.into_inner() {
        return Err(Fail::Full);
    }
    let mut total = vec![Agg::EMPTY; 256];
    for part in &parts {
        total.iter_mut().zip(part).for_each(|(sum, a)| sum.merge(a));
    }
    Ok(total)
}

struct Job<'a> {
    input: Input<'a>,
    /// Chunk c of records.csv is `cuts[c]..cuts[c + 1]`; `next` is the next
    /// chunk to claim, and `quotes[c]` the quote bytes found in chunk c.
    cuts: Vec<usize>,
    next: AtomicUsize,
    quotes: Vec<AtomicU64>,
    malformed: AtomicBool,
    full: AtomicBool,
}

impl Job<'_> {
    /// One worker: claims and parses chunks until none are left.
    fn work(&self) -> [Agg; 256] {
        let mut aggs = [Agg::EMPTY; 256];
        loop {
            let c = self.next.fetch_add(1, Relaxed);
            let Some(&end) = self.cuts.get(c + 1) else {
                return aggs;
            };
            match self.input.chunk(self.cuts[c], end, c == 0, &mut aggs) {
                Ok(quotes) => self.quotes[c].store(quotes, Relaxed),
                Err(Fail::Malformed) => self.malformed.store(true, Relaxed),
                Err(Fail::Full) => self.full.store(true, Relaxed),
            }
        }
    }
}

/// `n + 1` ascending cut positions: 0, then each nominal share start moved
/// to just past the next byte where `boundary` holds, then `len`.
fn cuts(len: usize, n: usize, boundary: impl Fn(usize) -> bool) -> Vec<usize> {
    let mut cuts = vec![0];
    for k in 1..n {
        let nominal = (len * k / n).max(cuts[k - 1]);
        cuts.push((nominal..len).find(|&i| boundary(i)).map_or(len, |i| i + 1));
    }
    cuts.push(len);
    cuts
}
