//! Speed Demon 2026, Track D (Traverse): exact point-to-point shortest paths on a
//! road-like graph (1.7M nodes, 5M weighted directed edges, 2,000 queries).
//!
//! 1. `graph.txt` is mapped and parsed by every thread at once into a CSR of
//!    out-edges (`build`).
//! 2. The nodes are renumbered region by region in BFS order (`renumber`). The file's
//!    ids are a random permutation; afterwards neighbouring streets have nearby ids,
//!    so a search touches a few cache lines instead of one per node.
//! 3. The in-edges are the transpose of the renumbered graph (`build` again).
//! 4. Every query is answered by a bidirectional Dijkstra (`search`). The queries are
//!    sorted by source first, so the threads, which run them at the same time, search
//!    the same part of the map and share the last-level cache.

use std::collections::VecDeque;
use std::error::Error;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicI64, AtomicU16, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::{Barrier, Mutex, OnceLock};
use std::thread::available_parallelism;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn mallopt(param: i32, value: i32) -> i32;
    fn prctl(option: i32, ...) -> i32;
}

// Values from the Linux and glibc headers for the calls above.
const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const M_MMAP_THRESHOLD: i32 = -3;
const PR_SET_THP_DISABLE: i32 = 41;

/// log2 of the nodes per bucket in `build`; a bucket's histogram (16 KiB) stays in L1.
const BUCKET_BITS: u32 = 12;
/// The most nodes one BFS region claims in `renumber` before it stops growing.
const REGION: u32 = 1 << 14;
/// Provisional ids in `renumber` are `thread << THREAD_SHIFT | index`.
const THREAD_SHIFT: u32 = 27;
/// Nodes per block of tentative distances in a search (`Side`).
const BLOCK: usize = 64;

/// What a team member runs in a phase: `job(k)` on member k.
type Job = &'static (dyn Fn(usize) + Sync);

/// One team of threads for every parallel phase, started once (spawning a fresh set
/// for each of the 9 phases cost 7.5 to 16 ms in all, locally). The caller is member
/// 0; the others wait at `start` for the next job.
struct Team {
    size: usize,
    job: Mutex<Option<Job>>,
    start: Barrier,
    done: Barrier,
}

static TEAM: OnceLock<Team> = OnceLock::new();

fn start_team(size: usize) {
    let (start, done) = (Barrier::new(size), Barrier::new(size));
    TEAM.get_or_init(|| Team { size, job: Mutex::new(None), start, done });
    for k in 1..size {
        std::thread::spawn(move || {
            let team = TEAM.get().unwrap();
            loop {
                team.start.wait();
                let job = team.job.lock().unwrap().expect("a job is set before start");
                job(k);
                team.done.wait();
            }
        });
    }
}

/// Runs `f(k)` on every team member k and returns the results in order. The
/// `unwrap`s on locks cannot fire: with panic = "abort" a failing thread ends the
/// process, so no lock is left poisoned.
fn par<R: Send>(f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    let team = TEAM.get().expect("the team is started first");
    let results: Vec<_> = (0..team.size).map(|_| Mutex::new(None)).collect();
    let job = |k: usize| *results[k].lock().unwrap() = Some(f(k));
    // SAFETY: only the lifetime is extended. Members call the job strictly between the
    // two barriers, and this function returns only after the second, so no call
    // outlives `job`.
    let job: Job = unsafe { std::mem::transmute::<&(dyn Fn(usize) + Sync), Job>(&job) };
    *team.job.lock().unwrap() = Some(job);
    team.start.wait();
    job(0);
    team.done.wait();
    results.into_iter().map(|r| r.into_inner().unwrap().unwrap()).collect()
}

/// An integer type and its atomic twin, so that threads can fill disjoint slots of one
/// array through a shared reference. Relaxed loads and stores are plain moves on x86.
trait Cell {
    type Atomic;
}
impl Cell for u16 {
    type Atomic = AtomicU16;
}
impl Cell for u32 {
    type Atomic = AtomicU32;
}

fn cells<T: Cell>(v: &mut [T]) -> &[T::Atomic] {
    // SAFETY: AtomicU16 and AtomicU32 have the size, alignment and bit validity of u16
    // and u32, and the exclusive borrow rules out plain access while the view lives.
    unsafe { &*(v as *mut [T] as *const [T::Atomic]) }
}

/// Maps a file read-only. The harness has /data in the page cache already, so this
/// neither copies the file nor counts it as our memory.
fn map(path: &str) -> std::io::Result<&'static [u8]> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    let (fd, anywhere) = (file.as_raw_fd(), std::ptr::null_mut());
    // SAFETY: plain mmap call; the result is checked below.
    let ptr = unsafe { mmap(anywhere, len, PROT_READ, MAP_PRIVATE, fd, 0) };
    if ptr as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the mapping holds `len` readable bytes. It outlives `file` and is never
    // written or unmapped, so the slice is valid for the rest of the program.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// Parses the decimal number at `text[*i..]` and steps past the separator after it.
///
/// Eight bytes are read at once. Digits (0x30..=0x39) have bit 4 set and the two
/// separators (space, LF) do not, so the lowest clear bit 4 ends the number. The
/// digits are shifted to the top of the word (the vacated bytes act as leading zeros)
/// and combined with three multiplies (Lemire's eight-digit method). Longer numbers,
/// and the last bytes of a piece, take the byte loop.
#[inline(always)]
fn number(text: &[u8], i: &mut usize) -> u64 {
    if let Some(bytes) = text.get(*i..*i + 8) {
        let word = u64::from_le_bytes(bytes.try_into().unwrap());
        let separators = !word & 0x1010_1010_1010_1010;
        if separators != 0 {
            let len = separators.trailing_zeros() / 8; // at least one digit
            *i += len as usize + 1;
            // A borrow only happens past the separator, and the shift drops those bytes.
            let digits = word.wrapping_sub(0x3030_3030_3030_3030) << (64 - 8 * len);
            let pairs = digits.wrapping_mul(10).wrapping_add(digits >> 8);
            let low = (pairs & 0x0000_00FF_0000_00FF).wrapping_mul(100 + (1_000_000 << 32));
            let high = ((pairs >> 16) & 0x0000_00FF_0000_00FF).wrapping_mul(1 + (10_000 << 32));
            return low.wrapping_add(high) >> 32;
        }
    }
    let mut x = 0;
    while let Some(&c) = text.get(*i).filter(|&&c| c >= b'0') {
        x = x * 10 + (c - b'0') as u64;
        *i += 1;
    }
    *i += 1;
    x
}

/// Cuts the edge lines into `parts` pieces, each ending at a line end.
fn split(text: &[u8], parts: usize) -> Vec<&[u8]> {
    let mut cuts = vec![0];
    for k in 1..parts {
        let mut at = (text.len() * k / parts).max(cuts[k - 1]);
        while at > 0 && at < text.len() && text[at - 1] != b'\n' {
            at += 1;
        }
        cuts.push(at);
    }
    cuts.push(text.len());
    cuts.windows(2).map(|w| &text[w[0]..w[1]]).collect()
}

/// Adjacency rows: row r is `edges[off[r]..off[r + 1]]`. An edge is one u32,
/// `head << bits | weight`. Road weights are small; one that does not fit below the
/// all-ones value of `bits` bits is stored as that value, and its real weight is in
/// `escapes`: (row, head, weight), sorted, so the lightest of a parallel group comes
/// first. That keeps the graph at 4 bytes per edge.
struct Csr {
    off: Vec<u32>,
    edges: Vec<u32>,
    bits: u32,
    escapes: Vec<(u32, u32, u32)>,
}

impl Csr {
    fn row(&self, r: usize) -> &[u32] {
        &self.edges[self.off[r] as usize..self.off[r + 1] as usize]
    }

    /// The weight of edge `e` of row `r`.
    #[inline(always)]
    fn weight(&self, r: usize, e: u32) -> u64 {
        match e & ((1 << self.bits) - 1) {
            w if w < (1 << self.bits) - 1 => w as u64,
            _ => self.escaped(r, e >> self.bits),
        }
    }

    #[cold]
    fn escaped(&self, r: usize, head: u32) -> u64 {
        let key = (r as u32, head, 0);
        self.escapes[self.escapes.partition_point(|x| *x < key)].2 as u64
    }
}

/// Edges handed out in `parts` disjoint shares as `f(tail, head, weight)`.
trait Source: Sync {
    fn each(&self, k: usize, parts: usize, f: impl FnMut(usize, usize, u64));
}

/// The edge lines of graph.txt, one piece per share.
struct Text<'a>(Vec<&'a [u8]>);

impl Source for Text<'_> {
    fn each(&self, k: usize, _: usize, mut f: impl FnMut(usize, usize, u64)) {
        let (piece, mut i) = (self.0[k], 0);
        while i < piece.len() {
            let u = number(piece, &mut i) as usize;
            let v = number(piece, &mut i) as usize;
            f(u, v, number(piece, &mut i));
        }
    }
}

/// Every edge of a CSR, turned around.
struct Reversed<'a>(&'a Csr);

impl Source for Reversed<'_> {
    fn each(&self, k: usize, parts: usize, mut f: impl FnMut(usize, usize, u64)) {
        let (g, rows) = (self.0, self.0.off.len() - 1);
        for r in k * rows / parts..(k + 1) * rows / parts {
            for &e in g.row(r) {
                f((e >> g.bits) as usize, r, g.weight(r, e));
            }
        }
    }
}

/// Builds the CSR of `n` nodes holding every edge of `src`, on `t` threads. No atomic
/// read-modify-write is needed (on x86 each one is a full fence): pass 1 counts the
/// edges per (thread, bucket of tails), which gives every thread a private run in every
/// bucket; pass 2 writes each edge into its run, with the tail's low bits alongside in
/// `low`; then each bucket is counting-sorted by tail, and its histogram is its offsets.
///
/// `off` may be a buffer of the right size left by an earlier phase, whose pages are
/// faulted in already (zeroing fresh pages is a tenth of all CPU time), or empty.
fn build(src: &impl Source, t: usize, n: usize, mut off: Vec<u32>) -> Csr {
    let bits = 31 - (n.max(2) - 1).ilog2(); // what is left of 32 once a head is stored
    assert!(bits >= 6, "too many nodes for 4-byte edges");
    let escape = (1u64 << bits) - 1;
    let buckets = (n >> BUCKET_BITS) + 1;
    let counts = par(|k| {
        let mut count = vec![0u32; buckets];
        src.each(k, t, |u, v, _| {
            assert!(u.max(v) < n, "edge endpoint out of range");
            count[u >> BUCKET_BITS] += 1;
        });
        count
    });
    // Thread k's first slot in bucket b: the bucket's start plus earlier threads' runs.
    let (mut cursors, mut start, mut m) = (vec![vec![0u32; buckets]; t], vec![0; buckets + 1], 0);
    for b in 0..buckets {
        start[b] = m;
        for k in 0..t {
            cursors[k][b] = m;
            m += counts[k][b];
        }
    }
    start[buckets] = m;
    // calloc'd: the pages are faulted in by the threads that write them.
    if off.len() != n + 1 {
        off = vec![0; n + 1];
    }
    off[0] = 0; // the sort below writes every other offset
    let (mut edges, mut low) = (vec![0u32; m as usize], vec![0u16; m as usize]);
    let (e, l, o) = (cells(&mut edges), cells(&mut low), cells(&mut off));
    let escapes = par(|k| {
        let mut cursor = cursors[k].clone();
        let mut heavy = Vec::new();
        src.each(k, t, |u, v, w| {
            let slot = &mut cursor[u >> BUCKET_BITS];
            e[*slot as usize].store((v as u32) << bits | w.min(escape) as u32, Relaxed);
            l[*slot as usize].store((u & ((1 << BUCKET_BITS) - 1)) as u16, Relaxed);
            *slot += 1;
            if w >= escape {
                heavy.push((u as u32, v as u32, w as u32));
            }
        });
        heavy
    });
    par(|k| {
        let (mut count, mut sorted) = (vec![0u32; 1 << BUCKET_BITS], Vec::new());
        for b in k * buckets / t..(k + 1) * buckets / t {
            let (lo, hi) = (start[b] as usize, start[b + 1] as usize);
            count.fill(0);
            (lo..hi).for_each(|i| count[l[i].load(Relaxed) as usize] += 1);
            let mut at = lo as u32;
            for (x, c) in count.iter_mut().enumerate() {
                (*c, at) = (at, at + *c); // c becomes the first slot of tail x
                if let Some(end) = o.get((b << BUCKET_BITS) + x + 1) {
                    end.store(at, Relaxed);
                }
            }
            sorted.resize(hi - lo, 0);
            for i in lo..hi {
                let x = &mut count[l[i].load(Relaxed) as usize];
                sorted[*x as usize - lo] = e[i].load(Relaxed);
                *x += 1;
            }
            (lo..hi).for_each(|i| e[i].store(sorted[i - lo], Relaxed));
        }
    });
    let mut escapes = escapes.concat();
    escapes.sort_unstable();
    Csr { off, edges, bits, escapes }
}

/// Hints the CPU to pull the cache line at `p` into L1.
fn prefetch<T>(p: *const T) {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
    // SAFETY: SSE is part of x86-64, and a prefetch never faults, whatever the address.
    unsafe { _mm_prefetch::<_MM_HINT_T0>(p.cast()) }
}

/// The new id of every old one: old id v becomes `start[k] + i` where
/// `rank[v] = (k << THREAD_SHIFT | i) + 1`, i.e. the i-th node thread k numbered.
struct Ids {
    rank: Vec<u32>,
    start: Vec<u32>,
}

impl Ids {
    fn get(&self, v: usize) -> u32 {
        let p = self.rank[v] - 1;
        self.start[(p >> THREAD_SHIFT) as usize] + (p & ((1 << THREAD_SHIFT) - 1))
    }
}

/// Renumbers the nodes of `g` and returns the graph in the new order with the new ids.
///
/// Thread k walks the old ids of its share in turn and grows a BFS region from every
/// one not claimed yet, until the region holds `REGION` nodes or meets other regions.
/// Inside a region the BFS rings put road neighbours at nearby ids, and a region small
/// enough to sit in one part of the map also lets the sorted queries share the cache.
/// `rank` is the claim flag: 0 until a thread claims the node with a provisional id.
///
/// A thread copies every row it dequeues, so its rows come out in new-id order and
/// placing them is one sequential copy that renames the heads. That copy goes into the
/// arrays of `g` (same sizes), which saves faulting in fresh pages.
fn renumber(g: Csr, t: usize) -> (Csr, Ids) {
    let n = g.off.len() - 1;
    let mut rank = vec![0u32; n];
    let flags = cells(&mut rank);
    // The plain load first: most heads are claimed already, and it spares them the
    // locked instruction, which is a full fence.
    let claim = |v: usize, id: u32| {
        flags[v].load(Relaxed) == 0 && flags[v].compare_exchange(0, id, Relaxed, Relaxed).is_ok()
    };
    let walks = par(|k| {
        let (mut queue, mut degrees, mut rows) = (VecDeque::new(), vec![], vec![]);
        let mut next = ((k as u32) << THREAD_SHIFT) + 1; // n < 2^26 (see `build`), so no overflow
        for root in k * n / t..(k + 1) * n / t {
            if !claim(root, next) {
                continue;
            }
            let end = next + REGION;
            queue.push_back(root);
            next += 1;
            while let Some(u) = queue.pop_front() {
                // The queue says which rows come next: fetch an offset 16 places ahead,
                // the row it points to 8 ahead and the claim flags of its heads 4 ahead
                // (those random reads are most of the walk's time).
                if let Some(&a) = queue.get(16) {
                    prefetch(&g.off[a]);
                }
                if let Some(&a) = queue.get(8) {
                    prefetch(g.edges.as_ptr().wrapping_add(g.off[a] as usize));
                }
                if let Some(&a) = queue.get(4) {
                    g.row(a).iter().for_each(|&e| prefetch(&flags[(e >> g.bits) as usize]));
                }
                let row = g.row(u);
                rows.extend_from_slice(row);
                degrees.push(row.len() as u32);
                for &e in row {
                    let v = (e >> g.bits) as usize;
                    if next < end && claim(v, next) {
                        queue.push_back(v);
                        next += 1;
                    }
                }
            }
        }
        (degrees, rows)
    });
    // Thread k's nodes take the next run of new ids and its rows the next run of edges.
    let (mut start, mut at) = (vec![0u32], vec![0usize]);
    for w in &walks {
        start.push(start[start.len() - 1] + w.0.len() as u32);
        at.push(at[at.len() - 1] + w.1.len());
    }
    let ids = Ids { rank, start };
    let (mut off, mut edges, bits, mask) = (g.off, g.edges, g.bits, (1 << g.bits) - 1);
    let (o, e) = (cells(&mut off), cells(&mut edges));
    par(|k| {
        let (degrees, rows) = &walks[k];
        let (mut end, first) = (at[k] as u32, ids.start[k] as usize);
        for (i, &d) in degrees.iter().enumerate() {
            end += d;
            o[first + i + 1].store(end, Relaxed);
        }
        for (i, &x) in rows.iter().enumerate() {
            e[at[k] + i].store(ids.get((x >> bits) as usize) << bits | x & mask, Relaxed);
        }
    });
    let mut escapes: Vec<_> = (g.escapes.iter())
        .map(|&(u, v, w)| (ids.get(u as usize), ids.get(v as usize), w))
        .collect();
    escapes.sort_unstable();
    (Csr { off, edges, bits, escapes }, ids)
}

/// What a search keeps its distances in: u32 or u64.
trait Width: Copy + Default + Into<u64> + TryFrom<u64> {}
impl<T: Copy + Default + Into<u64> + TryFrom<u64>> Width for T {}

/// One direction of a bidirectional search: tentative distances, stored as d + 1 so
/// that 0 means "not reached", and a radix heap. The distances live in blocks of
/// `BLOCK` nodes allocated when the search first touches them, so a search's memory
/// follows its size, not the graph's. The heap holds a key in the bucket of the highest
/// bit where it differs from the last minimum: Dijkstra never queues a key below that,
/// so a push is O(1) and a key moves down at most 64 times.
struct Side<T> {
    dir: Vec<u32>,
    blocks: Vec<[T; BLOCK]>,
    touched: Vec<u32>,
    heap: Vec<Vec<(T, u32)>>,
    last: u64,
}

impl<T: Width> Side<T> {
    fn new(n: usize) -> Self {
        Side {
            dir: vec![0; n / BLOCK + 1],
            // Address space only: pages are faulted in as blocks are used.
            blocks: Vec::with_capacity(n / BLOCK + 1),
            touched: Vec::new(),
            heap: (0..65).map(|_| Vec::new()).collect(),
            last: 0,
        }
    }

    fn start(&mut self, v: usize) {
        self.touched.drain(..).for_each(|b| self.dir[b as usize] = 0);
        self.blocks.clear();
        self.heap.iter_mut().for_each(Vec::clear);
        self.last = 0;
        self.improve(v, 0);
    }

    fn get(&self, v: usize) -> Option<u64> {
        match self.dir[v / BLOCK] {
            0 => None,
            b => Into::<u64>::into(self.blocks[b as usize - 1][v % BLOCK]).checked_sub(1),
        }
    }

    /// Lowers the distance of `v` to `d` and queues it, if that is an improvement.
    /// None if `d` does not fit in `T`.
    #[inline(always)]
    fn improve(&mut self, v: usize, d: u64) -> Option<bool> {
        let block = &mut self.dir[v / BLOCK];
        if *block == 0 {
            self.blocks.push([T::default(); BLOCK]);
            self.touched.push((v / BLOCK) as u32);
            *block = self.blocks.len() as u32;
        }
        let slot = &mut self.blocks[*block as usize - 1][v % BLOCK];
        let known: u64 = (*slot).into();
        if known != 0 && known <= d + 1 {
            return Some(false);
        }
        *slot = T::try_from(d + 1).ok()?;
        self.push(T::try_from(d).ok()?, v as u32);
        Some(true)
    }

    /// Settles the closest queued node (a stale entry is just dropped), relaxing its
    /// edges in `g`, and returns the best s-t length through a node `other` has
    /// reached. None if a distance does not fit in `T`.
    #[inline(always)]
    fn step(&mut self, g: &Csr, other: &Self, mut best: u64) -> Option<u64> {
        let (d, u) = self.heap[0].pop().unwrap();
        let (d, u) = (d.into(), u as usize);
        if self.get(u) != Some(d) {
            return Some(best); // stale: u was settled at a smaller distance
        }
        for &e in g.row(u) {
            let v = (e >> g.bits) as usize;
            let w = g.weight(u, e);
            if self.improve(v, d + w)? {
                prefetch(g.edges.as_ptr().wrapping_add(g.off[v] as usize)); // read when v settles
                if let Some(rest) = other.get(v) {
                    best = best.min(d + w + rest);
                }
            }
        }
        Some(best)
    }

    fn push(&mut self, key: T, v: u32) {
        let bucket = 64 - (key.into() ^ self.last).leading_zeros();
        self.heap[bucket as usize].push((key, v));
    }

    /// The smallest queued key, or u64::MAX if none. Refills bucket 0 from the first
    /// non-empty bucket, whose minimum becomes the new reference key.
    fn min(&mut self) -> u64 {
        if self.heap[0].is_empty() {
            let Some(b) = self.heap.iter().position(|b| !b.is_empty()) else {
                return u64::MAX;
            };
            let mut moved = std::mem::take(&mut self.heap[b]);
            self.last = moved.iter().map(|x| x.0.into()).min().unwrap();
            moved.drain(..).for_each(|(key, v)| self.push(key, v));
            self.heap[b] = moved; // empty, but keeps its capacity
        }
        self.last
    }
}

/// Bidirectional Dijkstra: forward from `s` on the out-edges `g[0]`, backward from `t`
/// on the in-edges `g[1]`. The sides take turns, so both settle about as many nodes
/// (balancing on keys instead lets one side sweep the map when the other starts behind
/// a 2^32 weight). Once the two smallest keys add up to at least the best meeting
/// length, nothing shorter is left. An exhausted side (key u64::MAX) also ends it: it
/// has settled its whole end of every s-t path, so `best` is exact, or u64::MAX if there
/// is no path. Returns None if a distance does not fit in `T`.
fn search<T: Width>(g: [&Csr; 2], sides: &mut [Side<T>; 2], s: usize, t: usize) -> Option<i64> {
    if s == t {
        return Some(0);
    }
    let [fwd, bwd] = sides;
    fwd.start(s);
    bwd.start(t);
    let mut best = u64::MAX;
    for turn in 0usize.. {
        if fwd.min().saturating_add(bwd.min()) >= best {
            break;
        }
        best = match turn % 2 {
            0 => fwd.step(g[0], bwd, best)?,
            _ => bwd.step(g[1], fwd, best)?,
        };
    }
    Some(best.try_into().unwrap_or(-1)) // u64::MAX, no path, is the one value i64 lacks
}

fn main() -> Result<(), Box<dyn Error>> {
    // SAFETY: plain libc calls with constant arguments, before any other thread exists.
    unsafe {
        // Pin glibc's mmap threshold: left dynamic it grows after the first big free, and
        // later big buffers come from arenas that never give memory back, so finished
        // phases would stay in the peak.
        mallopt(M_MMAP_THRESHOLD, 1 << 20);
        // With transparent huge pages always on, a sparsely touched buffer (a search's
        // distance blocks) is charged in 2 MiB pages.
        prctl(PR_SET_THP_DISABLE, 1u64, 0u64, 0u64, 0u64);
    }
    let threads = available_parallelism().map_or(1, |p| p.get().min(12));
    start_team(threads);
    let text = map("/data/graph.txt")?;
    let mut i = 0;
    let n = number(text, &mut i) as usize;
    number(text, &mut i); // m: the edge lines themselves say how many there are
    let first = build(&Text(split(&text[i..], threads)), threads, n, vec![]);
    let (fwd, ids) = renumber(first, threads);
    let node = |x: &str| -> Result<u32, Box<dyn Error>> {
        let x: usize = x.parse()?;
        (x < n).then(|| ids.get(x)).ok_or("node id out of range in queries.txt".into())
    };
    let queries = (std::fs::read_to_string("/data/queries.txt")?.lines())
        .map(|line| {
            let (s, t) = line.split_once(' ').ok_or("bad line in queries.txt")?;
            Ok((node(s)?, node(t)?))
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    let mut spare = ids.rank; // the in-edges' offsets reuse its pages
    spare.push(0);
    let bwd = build(&Reversed(&fwd), threads, n, spare);

    // Distinct queries in source order: neighbouring sources run at the same time.
    let mut jobs = queries.clone();
    jobs.sort_unstable();
    jobs.dedup();
    let answers: Vec<AtomicI64> = jobs.iter().map(|_| AtomicI64::new(0)).collect();
    let next = AtomicUsize::new(0);
    par(|_| {
        // 32-bit distances halve a search's memory. The rare query whose distances pass
        // 2^32 - 2 (only near-2^32 weights do that) is answered again with 64 bits.
        let mut narrow = [Side::<u32>::new(n), Side::new(n)];
        let mut wide = [Side::<u64>::new(n), Side::new(n)];
        loop {
            let j = next.fetch_add(1, Relaxed);
            let Some(&(s, t)) = jobs.get(j) else { break };
            let (g, s, t) = ([&fwd, &bwd], s as usize, t as usize);
            let answer = search(g, &mut narrow, s, t).or_else(|| search(g, &mut wide, s, t));
            answers[j].store(answer.expect("64-bit distances always fit"), Relaxed);
        }
    });
    let answer = |q| answers[jobs.binary_search(q).unwrap()].load(Relaxed).to_string();
    let out: String = queries.iter().map(|q| answer(q) + "\n").collect();
    std::fs::write("/out/distances.txt", out)?;
    Ok(())
}
