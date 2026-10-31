//! The parallel engine: the first workers split regions.csv, then all of
//! them claim chunks of records.csv until none are left. All shared state
//! (the region map, the name table and its pair bitset) is lock-free, and
//! no worker ever waits on another's progress except for regions.csv at the
//! start. Each worker keeps its own aggregates, summed at the end.
//!
//! Choosing the chunk cuts is where the quote-parity problem lives; see `run`.

use crate::map::Mapped;
use crate::names::Names;
use crate::records::Fail;
use crate::regions::Regions;
use crate::{die, Agg};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering::*};
use std::sync::Mutex;
use std::thread::{self, Scope};

/// Everything the workers share: records.csv, the tables records are joined
/// and counted into, and the chunks. Chunk c is `cuts[c]..cuts[c + 1]`;
/// `next` is the next one to claim, and `quotes[c]` the quote bytes found in
/// chunk c. `fail` is the worst `Fail` of any chunk, 0 for none.
pub struct Shared<'a> {
    pub b: &'a [u8],
    pub len: usize,
    pub regions: &'a Regions,
    pub names: Names,
    cuts: Vec<usize>,
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
pub fn run(threads: usize, chunks: usize, slots: usize, records: Mapped, regions: (&Regions, Mapped)) -> Result<Vec<Agg>, Fail> {
    let (b, len) = (records.b, records.len);
    let names = Names::new(slots, len).unwrap_or_else(|e| die(&format!("memory: {e}")));
    let boundary = |i: usize| b[i] == b'\n' && (b[i + 1].is_ascii_digit() || b[i + 1] == 0xEF);
    let (cuts, next) = (cuts(len, chunks, boundary), AtomicUsize::new(0));
    let quotes = (0..chunks).map(|_| AtomicU64::new(0)).collect();
    let fail = AtomicU8::new(0);
    let shared = Shared { b, len, regions: regions.0, names, cuts, next, quotes, fail };
    // Before records start, the first workers fault in the name tables (see
    // `Names::touch`) while the others split regions.csv; one worker alone
    // does both.
    let touchers = (threads / 3).clamp(1, 4);
    let parsers = (threads - touchers).max(1);
    let waiting = AtomicUsize::new(touchers + parsers);
    let total = Mutex::new(vec![Agg::EMPTY; 256]);
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
        let aggs = shared.work();
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
        // Small stacks: a worker needs about 40 KiB, and a 2 MiB stack could
        // be given a huge page on a kernel that uses them everywhere.
        let builder = thread::Builder::new().stack_size(256 << 10);
        let spawned = builder.spawn_scoped(s, move || grow(s, child, n, work));
        spawned.unwrap_or_else(|e| die(&format!("starting a thread: {e}")));
    }
    work(t);
}

impl Shared<'_> {
    /// One worker: claims and parses chunks until none are left.
    fn work(&self) -> [Agg; 256] {
        let mut aggs = [Agg::EMPTY; 256];
        loop {
            let c = self.next.fetch_add(1, Relaxed);
            let Some(&end) = self.cuts.get(c + 1) else {
                return aggs;
            };
            match self.chunk(self.cuts[c], end, &mut aggs) {
                Ok(quotes) => self.quotes[c].store(quotes, Relaxed),
                Err(f) => _ = self.fail.fetch_max(f as u8, Relaxed),
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
