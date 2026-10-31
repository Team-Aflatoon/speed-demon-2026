//! Speed Demon 2026, Track C (Join): parses the hostile /data/records.csv,
//! joins it to /data/regions.csv and writes /out/aggregates.txt.
//! NOTES.md explains the design; the spec is specs/c-join.md.

mod names;
mod records;
mod regions;
mod sys;

use records::{Agg, Sink};
use regions::Regions;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Mutex;
use std::{env, fmt::Write as _, fs, process, thread};

/// The most worker threads (the scoring machine has twelve).
pub const MAX_THREADS: usize = 12;
/// Bytes of records.csv per slab, the unit of parallel parsing.
const SLAB: usize = 256 << 10;
/// The most messages one slab can send to one owner: a record has five
/// separators, so records start at least five bytes apart. Rounded to
/// whole pages: a run of messages (typically 3 KB) then touches one page,
/// not two, and only touched pages are memory.
const CAP: usize = (SLAB / 5 + 1).next_multiple_of(512);
/// Rounds whose messages can be in flight at once.
const SLOTS: usize = 4;
const HEADER: &[u8] = b"id,name,note,region_id,amount";

/// One pass over records.csv. The file is cut into slabs of `SLAB` bytes,
/// which workers claim in order; slabs `r * threads ..` make round `r`.
/// Parsing a slab sends a message per record to the worker that owns its
/// name; once all of a round is parsed, each owner counts its messages
/// from that round. `SLOTS` rounds of messages can be in flight.
struct Job<'a> {
    d: &'a [u8],
    len: usize,
    first: usize,
    regions: &'a Regions,
    threads: usize,
    slabs: usize,
    /// Each slab starts where the previous one ended, so one worker must do
    /// them all in order; otherwise a slab's start is guessed (`start`).
    chained: bool,
    /// Per slot, slab of the round and owner: `CAP` messages and a count.
    msgs: &'a [AtomicU64],
    counts: Vec<AtomicU32>,
    /// Region pieces and slabs claimed so far, and region pieces filled.
    claims: [AtomicUsize; 2],
    filled: AtomicUsize,
    /// Per round: slabs parsed, and owners done with its messages.
    parsed: Vec<AtomicU32>,
    applied: Vec<AtomicU32>,
    /// One per worker, normally used by that worker alone.
    owners: Vec<Mutex<Owner>>,
}

/// A worker's results: totals, for each slab it parsed (slab, start, end
/// or None if malformed), and for each region piece it filled (piece,
/// first and last id, or an error).
struct Done {
    aggs: Box<[Agg; 256]>,
    slabs: Vec<(usize, usize, Option<usize>)>,
    pieces: Vec<(usize, regions::Filled)>,
}

fn main() {
    sys::no_huge_pages();
    let args: Vec<String> = env::args().collect();
    let (data, out) = (
        args.get(1).map_or("/data", |s| s),
        args.get(2).map_or("/out", |s| s),
    );
    let (d, mut len) = map(&format!("{data}/records.csv"));
    if len >= (u32::MAX >> 1) as usize {
        die("records.csv is over 2 GiB; offsets are 32-bit"); // spec: 202 MB
    }
    // The final record may end at the end of the file (2.1). Giving it an
    // LF there means that every record ends with one.
    if len > 0 && d[len - 1] != b'\n' {
        (d[len], len) = (b'\n', len + 1);
    }
    let d: &'static [u8] = d;
    let bom = if d.starts_with(b"\xEF\xBB\xBF") { 3 } else { 0 };
    let first = match d[bom..].strip_prefix(HEADER) {
        Some([b'\n', ..]) => bom + HEADER.len() + 1,
        Some([b'\r', b'\n', ..]) => bom + HEADER.len() + 2,
        _ => die("records.csv: bad header"),
    };
    let (text, text_len) = map(&format!("{data}/regions.csv"));
    let threads = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_THREADS);
    let regions = Regions::new(text, text_len, 4 * threads).unwrap_or_else(|e| die(&e));
    let parallel = Job::new(d, len, first, &regions, threads, false);
    let (mut job, mut done) = (&parallel, run(&parallel));
    let mut pieces: Vec<_> = done.iter_mut().flat_map(|w| w.pieces.drain(..)).collect();
    pieces.sort_unstable_by_key(|p| p.0);
    let ascending = pieces.into_iter().try_fold(0, |prev, (_, ids)| match ids? {
        (0, _) => Ok(prev),
        (a, b) if a > prev => Ok(b),
        _ => Err("regions.csv: region ids do not ascend".to_string()),
    });
    ascending.unwrap_or_else(|e| die(&e));
    let chained;
    if !parallel.valid(&done) {
        // A guessed slab start was wrong, or the file is malformed: parse it
        // again in one chain from the start, which is right by construction.
        chained = Job::new(d, len, first, &regions, 1, true);
        chained.claims[0].store(regions.pieces(), Relaxed); // the map is filled
        chained.filled.store(regions.pieces(), Relaxed);
        (job, done) = (&chained, run(&chained));
        if !job.valid(&done) {
            die("records.csv does not follow spec section 2.1 (or a name is over 64 KiB)");
        }
    }
    let (mut total, mut distinct) = ([Agg::EMPTY; 256], [0u64; 256]);
    for (w, o) in done.iter().zip(&job.owners) {
        let o = o.lock().unwrap_or_else(|_| die("a worker failed"));
        for g in 0..256 {
            total[g].merge(&w.aggs[g]);
            distinct[g] += u64::from(o.names.distinct[g]);
        }
    }
    let mut text = String::new();
    // `_` sorts after every capital letter, so _UNMATCHED comes last.
    for (key, g) in regions.countries().chain([("_UNMATCHED".into(), 0)]) {
        let (a, names) = (&total[usize::from(g)], distinct[usize::from(g)]);
        if a.count > 0 {
            let (sum, min, max) = (money(a.sum), money(a.min), money(a.max));
            let line = format!("{key} {} {} {sum} {min} {max} {names}", a.count, a.ids);
            writeln!(text, "{line} {} {}", a.name_bytes, a.note_bytes).unwrap();
        }
    }
    let path = format!("{out}/aggregates.txt");
    fs::write(&path, text).unwrap_or_else(|e| die(&format!("{path}: {e}")));
}

impl<'a> Job<'a> {
    fn new(
        d: &'a [u8],
        len: usize,
        first: usize,
        regions: &'a Regions,
        threads: usize,
        chained: bool,
    ) -> Self {
        let slabs = (len - first).div_ceil(SLAB);
        let rounds = slabs.div_ceil(threads);
        let buffers = SLOTS * threads * threads;
        // SAFETY: all-zero bytes are valid AtomicU64 values.
        let msgs = unsafe { sys::zeroed(buffers * CAP) }.unwrap_or_else(|e| die(&e.to_string()));
        let atomics = |n| (0..n).map(|_| AtomicU32::new(0)).collect();
        let (counts, parsed, applied) = (atomics(buffers), atomics(rounds), atomics(rounds));
        let (claims, filled) = (Default::default(), AtomicUsize::new(0));
        let owners = (0..threads)
            .map(|_| {
                Mutex::new(Owner {
                    names: names::Names::new(),
                    next: 0,
                })
            })
            .collect();
        Job {
            d,
            len,
            first,
            regions,
            threads,
            slabs,
            chained,
            msgs,
            counts,
            claims,
            filled,
            parsed,
            applied,
            owners,
        }
    }

    /// Whether every slab parsed and ended exactly where the next began.
    /// Slab 0 starts at the first record, so by induction each slab then
    /// started at a record start and was parsed as one sequential pass
    /// would have parsed it.
    fn valid(&self, done: &[Done]) -> bool {
        let mut slabs: Vec<_> = done.iter().flat_map(|w| w.slabs.iter().copied()).collect();
        slabs.sort_unstable();
        let starts = slabs.iter().skip(1).map(|s| s.1).chain([self.len]);
        slabs.len() == self.slabs && slabs.iter().zip(starts).all(|(s, next)| s.2 == Some(next))
    }

    /// Where slab `g` starts: after the header, or where slab `g - 1` ended
    /// if chained, or else the first LF after the cut that is followed by
    /// a digit or a BOM byte. By 2.2, no quoted value has an LF before a
    /// digit or a non-ASCII byte, so that LF ends a record. `valid` checks
    /// this guess rather than trusting it.
    fn start(&self, g: usize, prev_end: usize) -> usize {
        if g == 0 || self.chained {
            return if g == 0 { self.first } else { prev_end };
        }
        let d = self.d;
        let starts = |&i: &usize| d[i - 1] == b'\n' && (d[i].is_ascii_digit() || d[i] == 0xEF);
        (self.first + g * SLAB..self.len)
            .find(starts)
            .unwrap_or(self.len)
    }

    /// The number of slabs in round `r`.
    fn round_slabs(&self, r: usize) -> u32 {
        (self.slabs - r * self.threads).min(self.threads) as u32
    }
}

/// Runs `job` on its workers, the main thread being worker 0.
fn run(job: &Job) -> Vec<Done> {
    thread::scope(|s| tree(s, job, 0))
}

/// Starts worker `me` after spawning workers `2me + 1` and `2me + 2`
/// (which do the same), so that the last of twelve starts after four
/// thread starts rather than eleven; returns the results of its subtree.
fn tree<'s>(s: &'s thread::Scope<'s, '_>, job: &'s Job, me: usize) -> Vec<Done> {
    let kids: Vec<_> = (2 * me + 1..(2 * me + 3).min(job.threads))
        .map(|k| thread::Builder::new().spawn_scoped(s, move || tree(s, job, k)))
        .map(|h| h.unwrap_or_else(|e| die(&e.to_string())))
        .collect();
    let mut done = vec![work(job, me)];
    for k in kids {
        done.extend(k.join().unwrap_or_else(|_| die("a worker failed")));
    }
    done
}

/// One worker: claims region pieces until none are left, then slabs, and
/// counts its names whenever a round is complete. It waits only when the
/// next slab's message buffer is still being read, and meanwhile counts
/// names for any owner that lags behind (a descheduled worker, say).
fn work(job: &Job, me: usize) -> Done {
    let t = job.threads;
    let (all, mut pieces) = (job.regions.pieces(), Vec::new());
    loop {
        let p = job.claims[0].fetch_add(1, Relaxed);
        if p >= all {
            break;
        }
        pieces.push((p, job.regions.fill(p)));
        job.filled.fetch_add(1, Release);
    }
    while job.filled.load(Acquire) < all {
        thread::yield_now();
    }
    let mut aggs = Box::new([Agg::EMPTY; 256]);
    let (mut slabs, mut prev_end) = (Vec::new(), job.first);
    loop {
        let g = job.claims[1].fetch_add(1, Relaxed);
        if g >= job.slabs {
            break;
        }
        let (r, j) = (g / t, g % t);
        if r >= SLOTS {
            job.help_until(&job.applied[r - SLOTS], me); // our buffer is being read
        }
        let base = (r % SLOTS * t + j) * t;
        let mut sink = Sink {
            aggs: &mut aggs,
            msgs: &job.msgs[base * CAP..],
            cap: CAP,
            counts: [0; MAX_THREADS],
            owners: t,
        };
        let start = job.start(g, prev_end);
        let end = (job.first + (g + 1) * SLAB).min(job.len);
        let parsed = records::parse(job.d, job.len, start, end, job.regions.map, &mut sink);
        prev_end = parsed.unwrap_or(end); // a failed chain is refused anyway
        slabs.push((g, start, parsed));
        for (o, &n) in sink.counts[..t].iter().enumerate() {
            job.counts[base + o].store(n as u32, Relaxed);
        }
        job.parsed[r].fetch_add(1, Release);
        job.count(me);
    }
    if let Some(last) = job.applied.last() {
        job.help_until(last, me);
    }
    Done {
        aggs,
        slabs,
        pieces,
    }
}

/// The names of one owner, and the next round they are to be counted for.
struct Owner {
    names: names::Names,
    next: usize,
}

impl Job<'_> {
    /// Counts owner `o`'s names in every round that is fully parsed and not
    /// yet counted, unless another worker is at it; returns whether it
    /// counted any.
    fn count(&self, o: usize) -> bool {
        let Ok(mut owner) = self.owners[o].try_lock() else {
            return false;
        };
        let t = self.threads;
        let start = owner.next;
        while owner.next < self.parsed.len()
            && self.parsed[owner.next].load(Acquire) == self.round_slabs(owner.next)
        {
            let r = owner.next;
            for j in 0..self.round_slabs(r) as usize {
                let base = (r % SLOTS * t + j) * t + o;
                let n = self.counts[base].load(Relaxed) as usize;
                owner
                    .names
                    .add_all(self.d, &self.msgs[base * CAP..base * CAP + n]);
            }
            self.applied[r].fetch_add(1, Release);
            owner.next += 1;
        }
        owner.next > start
    }

    /// Waits until every owner has counted the round of `applied`, counting
    /// for any owner meanwhile, starting with `me`.
    fn help_until(&self, applied: &AtomicU32, me: usize) {
        let t = self.threads;
        while applied.load(Acquire) < t as u32 {
            if !(0..t).fold(false, |any, o| self.count((me + o) % t) | any) {
                thread::yield_now();
            }
        }
    }
}

/// Asks the CPU to start loading the cache line of `d[at]`.
pub fn prefetch<T>(d: &[T], at: usize) {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
    // SAFETY: a prefetch is only a hint: it never faults, whatever the
    // address, and changes no memory.
    unsafe { _mm_prefetch::<_MM_HINT_T0>(d.as_ptr().wrapping_add(at).cast()) }
}

/// Integer cents as `-?D+.DD`, never `-0.00`.
fn money(cents: i64) -> String {
    let (sign, abs) = (if cents < 0 { "-" } else { "" }, cents.unsigned_abs());
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

fn map(path: &str) -> (&'static mut [u8], usize) {
    sys::map_file(path).unwrap_or_else(|e| die(&format!("{path}: {e}")))
}

/// The error path for anything the input or the system can cause.
pub fn die(msg: &str) -> ! {
    eprintln!("track-c: {msg}");
    process::exit(1)
}
