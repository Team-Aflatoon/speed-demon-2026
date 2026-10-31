//! Speed Demon 2026, Track D (Traverse): exact point-to-point shortest paths.
//!
//! 1. Map graph.txt and parse it in parallel into a CSR of out-edges (`build_csr`).
//! 2. Renumber the nodes in BFS order (`bfs_layout`): node ids in the file are random;
//!    renumbered, road-network neighbours sit at nearby ids, so a search touches a few
//!    cache lines instead of one per node. The walk lays out the forward graph in the
//!    new order as it goes; transposing that gives the in-edges (`build_csr` again).
//! 3. Answer the queries on all threads, each with a bidirectional Dijkstra.

use std::collections::VecDeque;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicI64, AtomicU16, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::{Barrier, Mutex, OnceLock};
use std::thread::available_parallelism;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mallopt(param: i32, value: i32) -> i32;
    fn prctl(option: i32, ...) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

// The values from the Linux and glibc headers for the calls above.
const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const M_MMAP_THRESHOLD: i32 = -3;
const PR_SET_THP_DISABLE: i32 = 41;
const PR_THP_DISABLE_EXCEPT_ADVISED: u64 = 1 << 1;
const MADV_HUGEPAGE: i32 = 14;

/// Maps a whole file read-only. The harness prewarms /data into the page cache, so this
/// costs neither a copy nor memory charged to us.
fn map_file(path: &str) -> std::io::Result<&'static [u8]> {
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    // SAFETY: a private read-only mapping of a file on a read-only mount; it stays valid
    // after `file` is closed and is never unmapped, hence 'static.
    let fd = file.as_raw_fd();
    let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0) };
    if ptr as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: mmap succeeded, so `ptr` points to `len` readable bytes, and the mapping is
    // never written to or unmapped while the program runs.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// Parses the unsigned decimal at `text[*i..]` and steps past the separator after it.
///
/// Eight bytes are read at once: digits (0x30..=0x39) have bit 4 set and both separators
/// (space 0x20, LF 0x0A) do not, so the lowest clear bit 4 marks the number's end. The
/// digits are shifted to the top of the word, the vacated bytes acting as leading zeros,
/// and combined in three multiplies (Lemire's eight-digit method). Numbers of eight or
/// more digits, and the last bytes of a piece, take the byte-at-a-time path.
#[inline(always)]
fn number(text: &[u8], i: &mut usize) -> u64 {
    if let Some(word) = text.get(*i..*i + 8) {
        let word = u64::from_le_bytes(word.try_into().unwrap());
        let separators = !word & 0x1010_1010_1010_1010;
        if separators != 0 {
            let len = separators.trailing_zeros() / 8;
            *i += len as usize + 1;
            // Subtracting borrows only past the separator, which the shift discards.
            let digits = word.wrapping_sub(0x3030_3030_3030_3030) << (64 - 8 * len);
            let pairs = digits.wrapping_mul(10).wrapping_add(digits >> 8);
            let low = (pairs & 0x0000_00FF_0000_00FF).wrapping_mul(100 + (1_000_000 << 32));
            let high = ((pairs >> 16) & 0x0000_00FF_0000_00FF).wrapping_mul(1 + (10_000 << 32));
            return low.wrapping_add(high) >> 32;
        }
    }
    let mut x = 0;
    loop {
        let c = text[*i];
        *i += 1;
        if c < b'0' {
            return x;
        }
        x = x * 10 + (c - b'0') as u64;
    }
}

/// Splits the edge lines into `parts` pieces that start and end on line boundaries.
fn split_lines(text: &[u8], parts: usize) -> Vec<&[u8]> {
    let mut cuts = vec![0];
    for k in 1..parts {
        // At least 1 so that `text[at - 1]` exists, at most the end (for tiny inputs).
        let lo = cuts[k - 1].max(1);
        let mut at = (text.len() * k / parts).max(lo).min(text.len());
        while at < text.len() && text[at - 1] != b'\n' {
            at += 1;
        }
        cuts.push(at);
    }
    cuts.push(text.len());
    cuts.windows(2).map(|w| &text[w[0]..w[1]]).collect()
}

/// A fixed team of threads, started once: spawning a fresh set for each of the ten
/// parallel phases cost about 5 ms. The caller is member 0; the others sleep on `start`.
struct Team {
    size: usize,
    job: Mutex<Option<&'static (dyn Fn(usize) + Sync)>>,
    start: Barrier,
    done: Barrier,
}

static TEAM: OnceLock<&'static Team> = OnceLock::new();

impl Team {
    fn start(size: usize) {
        let [start, done] = [Barrier::new(size), Barrier::new(size)];
        let team: &'static Team = Box::leak(Box::new(Team {
            size,
            job: Mutex::new(None),
            start,
            done,
        }));
        for k in 1..size {
            std::thread::spawn(move || loop {
                team.start.wait();
                let job = team.job.lock().unwrap().expect("a job is set before start");
                job(k);
                team.done.wait();
            });
        }
        TEAM.set(team).ok();
    }

    /// Calls `job(k)` for every member k, each on its own thread, and returns when all
    /// have finished.
    fn run(&self, job: &(dyn Fn(usize) + Sync)) {
        // SAFETY: only the lifetime is erased. Members call the job strictly between the
        // `start` and `done` barriers, and this function returns only after `done`, so
        // no call can outlive the borrow.
        let job: &'static (dyn Fn(usize) + Sync) = unsafe { std::mem::transmute(job) };
        *self.job.lock().unwrap() = Some(job);
        self.start.wait();
        job(0);
        self.done.wait();
    }
}

/// Runs `f(k, states[k])` for every state, each on its own team member, and collects the
/// results in order. There may be at most one state per member. The `unwrap`s on locks
/// cannot fire: with `panic = "abort"` a panicking thread ends the process, so no lock is
/// ever left poisoned.
fn parallel<S: Send, R: Send>(states: Vec<S>, f: impl Fn(usize, S) -> R + Sync) -> Vec<R> {
    let team = TEAM.get().expect("Team::start runs first");
    assert!(states.len() <= team.size);
    let slots: Vec<Mutex<(Option<S>, Option<R>)>> = states
        .into_iter()
        .map(|s| Mutex::new((Some(s), None)))
        .collect();
    team.run(&|k| {
        if let Some(slot) = slots.get(k) {
            let state = slot.lock().unwrap().0.take().unwrap();
            let result = f(k, state);
            slot.lock().unwrap().1 = Some(result);
        }
    });
    slots
        .into_iter()
        .map(|slot| slot.into_inner().unwrap().1.unwrap())
        .collect()
}

/// Anything that can list a graph's edges `(tail, head, weight)` in `parts` disjoint
/// parts, one per thread.
trait EdgeSource: Sync {
    fn each(&self, part: usize, parts: usize, f: impl FnMut(usize, usize, u64));
}

/// The edge lines of graph.txt, split at line boundaries into one piece per part.
struct Text<'a>(Vec<&'a [u8]>);

impl EdgeSource for Text<'_> {
    fn each(&self, part: usize, _: usize, mut f: impl FnMut(usize, usize, u64)) {
        let (piece, mut i) = (self.0[part], 0);
        while i < piece.len() {
            let u = number(piece, &mut i) as usize;
            let v = number(piece, &mut i) as usize;
            f(u, v, number(piece, &mut i));
        }
    }
}

/// Views a slice of integers as atomics, so threads can fill disjoint slots of it.
fn as_atomic<T: Atomic>(v: &mut [T]) -> &[T::Cell] {
    // SAFETY: each atomic type has the same size, alignment and bit validity as its
    // integer, and the exclusive borrow rules out non-atomic access while the view lives.
    unsafe { &*(v as *mut [T] as *const [T::Cell]) }
}

trait Atomic {
    type Cell;
}
impl Atomic for u16 {
    type Cell = AtomicU16;
}
impl Atomic for u32 {
    type Cell = AtomicU32;
}

/// Adjacency in compressed sparse rows: the edges of row r are `edges[off[r]..off[r + 1]]`.
/// An edge is one u32, `head << weight_bits | weight`, so 4 bytes per edge. Road weights
/// are small (11 bits hold all but ~0.1% of them at full scale); a larger weight is
/// stored as the all-ones `escape` and looked up in `escapes`.
struct Csr {
    off: Vec<u32>,
    edges: Vec<u32>,
    weight_bits: u32,
    /// (tail row, head, weight) of every escaped edge, sorted; per (row, head) only the
    /// lightest is kept, which is the only one a shortest path can use.
    escapes: Vec<(u32, u32, u32)>,
}

impl Csr {
    fn rows(&self) -> usize {
        self.off.len() - 1
    }

    /// The packed edges of row `r`.
    fn row(&self, r: usize) -> &[u32] {
        &self.edges[self.off[r] as usize..self.off[r + 1] as usize]
    }

    /// Out-edges of row `r` as (head, weight).
    fn out(&self, r: usize) -> impl Iterator<Item = (usize, u64)> + '_ {
        let esc = (1 << self.weight_bits) - 1;
        self.row(r).iter().map(move |&e| {
            let (v, w) = ((e >> self.weight_bits) as usize, e & esc);
            (v, if w < esc { w as u64 } else { self.heavy(r, v) })
        })
    }

    /// The weight of the lightest escaped edge from row `r` to `v`.
    #[cold]
    fn heavy(&self, r: usize, v: usize) -> u64 {
        let key = (r as u32, v as u32, 0);
        self.escapes[self.escapes.partition_point(|&e| e < key)].2 as u64
    }
}

/// A CSR lists its edges by contiguous ranges of rows.
impl EdgeSource for Csr {
    fn each(&self, part: usize, parts: usize, mut f: impl FnMut(usize, usize, u64)) {
        for r in part * self.rows() / parts..(part + 1) * self.rows() / parts {
            self.out(r).for_each(|(v, w)| f(r, v, w));
        }
    }
}

/// Asks for 2 MiB pages on a fresh build buffer that is about to be filled in full: one
/// page fault per 2 MiB instead of 512. Only such buffers get them (see the prctl in
/// `main`), so sparsely touched memory, like the searches' blocks, is not rounded up.
fn huge<T>(mut v: Vec<T>) -> Vec<T> {
    let (at, len) = (v.as_mut_ptr() as usize, std::mem::size_of_val(&v[..]));
    let (start, end) = ((at + 4095) & !4095, (at + len) & !4095);
    if end > start {
        // SAFETY: only advice, on whole pages inside the vector's own allocation.
        unsafe { madvise(start as *mut u8, end - start, MADV_HUGEPAGE) };
    }
    v
}

/// Big buffers handed on by a finished phase. The next phase overwrites pages that are
/// already mapped instead of faulting in fresh zeroed ones: a run takes about 32,000 page
/// faults, and on the scoring box each costs several times what it does locally.
#[derive(Default)]
struct Spare {
    edges: Vec<u32>,
    low: Vec<u16>,
    off: Vec<u32>,
}

/// `v` with length `len`, keeping its old contents (the caller overwrites every element it
/// later reads), or a fresh zeroed buffer if `v` is too small.
fn reuse<T: Copy + Default>(mut v: Vec<T>, len: usize) -> Vec<T> {
    if v.capacity() < len {
        return huge(vec![T::default(); len]);
    }
    v.resize(len, T::default()); // shrinks, or writes only the few missing elements
    v
}

/// Builds the CSR of n nodes holding the edge `map(u, v)` = (tail, head), of weight w,
/// for every edge (u, v, w) of `source`. No atomic read-modify-write is needed: pass 1
/// counts edges per (thread, bucket of 4096 tails), pass 2 writes each edge into its
/// thread's private run of its bucket (with the tail's low bits alongside, in `low`),
/// and finally every bucket is counting-sorted by tail on its own thread.
fn build_csr(
    source: &impl EdgeSource,
    parts: usize,
    n: usize,
    map: impl Fn(usize, usize) -> (usize, usize) + Sync,
    spare: Spare,
) -> (Csr, Vec<u16>) {
    // Bits left for the weight once a head id below n is stored.
    let weight_bits = 32 - (usize::BITS - (n.max(2) - 1).leading_zeros());
    assert!(weight_bits >= 6, "too many nodes for 32-bit edges");
    let escape = (1u64 << weight_bits) - 1;
    const BUCKET_BITS: u32 = 12; // a bucket's histogram stays in L1
    let buckets = (n >> BUCKET_BITS) + 1;
    let counts = parallel(vec![(); parts], |k, ()| {
        let mut count = vec![0u32; buckets];
        source.each(k, parts, |u, v, _| count[map(u, v).0 >> BUCKET_BITS] += 1);
        count
    });
    // Each thread's cursor per bucket: bucket start plus what earlier threads put there.
    let mut cursors = vec![vec![0u32; buckets]; parts];
    let mut bucket_start = vec![0u32; buckets + 1];
    let mut at = 0;
    for b in 0..buckets {
        bucket_start[b] = at;
        for k in 0..parts {
            cursors[k][b] = at;
            at += counts[k][b];
        }
    }
    bucket_start[buckets] = at;
    // calloc'd: the writers fault the pages in. Relaxed atomic stores are plain stores.
    let (mut edges, mut low) = (
        reuse(spare.edges, at as usize),
        reuse(spare.low, at as usize),
    );
    let (edge_cells, low_cells) = (as_atomic(&mut edges), as_atomic(&mut low));
    let escaped = parallel(cursors, |k, mut cursor| {
        let mut escaped = Vec::new();
        source.each(k, parts, |u, v, w| {
            let (x, y) = map(u, v);
            let slot = &mut cursor[x >> BUCKET_BITS];
            edge_cells[*slot as usize]
                .store((y << weight_bits) as u32 | w.min(escape) as u32, Relaxed);
            low_cells[*slot as usize].store((x & ((1 << BUCKET_BITS) - 1)) as u16, Relaxed);
            *slot += 1;
            if w >= escape {
                escaped.push((x as u32, y as u32, w as u32));
            }
        });
        escaped
    });
    let mut escapes = escaped.concat();
    escapes.sort_unstable();
    escapes.dedup_by_key(|e| (e.0, e.1));

    // Counting-sort every bucket by tail; its histogram is also that bucket's offsets.
    // Buckets are disjoint, so the arrays are split between threads.
    let mut off = reuse(spare.off, n + 1);
    off[0] = 0; // every other entry is written below
    let mut work = Vec::with_capacity(buckets);
    let (mut rest_e, mut rest_l, mut rest_o) = (&mut edges[..], &low[..], &mut off[1..]);
    for b in 0..buckets {
        let len = (bucket_start[b + 1] - bucket_start[b]) as usize;
        let ((e, tail_e), (l, tail_l)) = (rest_e.split_at_mut(len), rest_l.split_at(len));
        let (o, tail_o) = rest_o.split_at_mut(rest_o.len().min(1 << BUCKET_BITS));
        work.push((bucket_start[b], e, l, o));
        (rest_e, rest_l, rest_o) = (tail_e, tail_l, tail_o);
    }
    let per = buckets.div_ceil(parts);
    let mut chunks = Vec::new();
    while !work.is_empty() {
        chunks.push(work.drain(..per.min(work.len())).collect::<Vec<_>>());
    }
    parallel(chunks, |_, chunk| {
        let (mut count, mut sorted) = (vec![0u32; 1 << BUCKET_BITS], Vec::new());
        for (start, e, l, o) in chunk {
            count.fill(0);
            l.iter().for_each(|&x| count[x as usize] += 1);
            let mut sum = 0;
            for (x, c) in count.iter_mut().enumerate() {
                (*c, sum) = (sum, sum + *c); // count becomes each tail's first slot
                if let Some(end) = o.get_mut(x) {
                    *end = start + sum; // off[r + 1] = end of row r
                }
            }
            sorted.resize(e.len(), 0);
            for (&x, &edge) in l.iter().zip(e.iter()) {
                sorted[count[x as usize] as usize] = edge;
                count[x as usize] += 1;
            }
            e.copy_from_slice(&sorted);
        }
    });
    (
        Csr {
            off,
            edges,
            weight_bits,
            escapes,
        },
        low,
    )
}

/// Hints the CPU to pull the cache line holding `data[0]` into L1.
fn prefetch<T>(data: &[T]) {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
    // SAFETY: the instruction is SSE, which every x86-64 CPU has (the crate is built for
    // x86-64-v3, see .cargo/config.toml), and a prefetch never faults, whatever the address.
    unsafe { _mm_prefetch::<_MM_HINT_T0>(data.as_ptr() as *const i8) };
}

/// Renumbers the nodes in BFS order and returns `rank` (old id -> new id) with the
/// graph laid out in that order. Each thread walks its own share of the old ids and
/// grows a BFS region from every one not yet claimed, so the first regions spread until
/// they meet and later roots mop up what is left; no thread idles while another still
/// has work. Inside a region the BFS rings put road-network neighbours at nearby ids.
///
/// A thread copies each row as it dequeues it, so its rows come out already in new-id
/// order and placing them is one sequential, renaming copy. To keep this phase's memory
/// down, `rank` itself is the claim flag: 0 while unclaimed, then `(k << 27 | i) + 1` for
/// thread k's i-th node until the threads' counts are known. Degrees are kept in a byte
/// each (255 means "see `wide`"), and the BFS queue holds only the frontier.
fn bfs_layout(g: Csr, threads: usize) -> (Vec<u32>, Csr, Vec<u32>) {
    let n = g.rows();
    // One spare slot, so that `rank` can later become the n + 1 in-edge offsets.
    let mut rank = huge(vec![0u32; n + 1]);
    rank.truncate(n);
    let ids = as_atomic(&mut rank);
    // The rows a thread copies go to its own stretch of one buffer, sized from its share
    // of the edges with a margin (anything past it spills), which later holds the in-edges.
    let cap = g.edges.len() / threads * 9 / 8 + 4096;
    let mut copies = vec![0u32; cap * threads];
    // The plain load first: most neighbours are already claimed, and skipping the
    // locked exchange for them keeps the walk free of fences.
    let claim = |v: usize, id: u32| {
        ids[v].load(Relaxed) == 0 && ids[v].compare_exchange(0, id, Relaxed, Relaxed).is_ok()
    };
    let walks = parallel(copies.chunks_mut(cap).collect(), |k, rows: &mut [u32]| {
        let (mut queue, mut degrees, mut wide, mut used, mut spill) =
            (VecDeque::new(), Vec::new(), Vec::new(), 0, Vec::new());
        // i < 2^27 always: `build_csr` asserts n <= 2^26 (at least 6 weight bits).
        let mut next = (k << 27) as u32 + 1;
        for root in k * n / threads..(k + 1) * n / threads {
            if claim(root, next) {
                queue.push_back(root as u32);
                next += 1;
            }
            while let Some(u) = queue.pop_front() {
                // The queue says which rows come next, so fetch ahead in three stages: an
                // offset 16 nodes out, the row it points to 8 out, and the claim flags of
                // that row's heads 4 out (these are the random reads that dominate).
                if let Some(&ahead) = queue.get(8) {
                    prefetch(&g.edges[g.off[ahead as usize] as usize..]);
                }
                if let Some(&ahead) = queue.get(16) {
                    prefetch(&g.off[ahead as usize..]);
                }
                if let Some(&ahead) = queue.get(4) {
                    for &e in g.row(ahead as usize) {
                        prefetch(&ids[(e >> g.weight_bits) as usize..]);
                    }
                }
                let row = g.row(u as usize);
                if spill.is_empty() && used + row.len() <= rows.len() {
                    rows[used..used + row.len()].copy_from_slice(row);
                    used += row.len();
                } else {
                    spill.extend_from_slice(row);
                }
                degrees.push(row.len().min(255) as u8);
                if row.len() >= 255 {
                    wide.push(row.len() as u32);
                }
                for &e in row {
                    if claim((e >> g.weight_bits) as usize, next) {
                        queue.push_back(e >> g.weight_bits);
                        next += 1;
                    }
                }
            }
        }
        (degrees, wide, used, spill)
    });
    // Thread k's nodes take the next run of new ids and its rows the next run of edges.
    let starts: Vec<_> = walks
        .iter()
        .scan((0, 0), |at, w| {
            Some(std::mem::replace(
                at,
                (at.0 + w.0.len(), at.1 + w.2 + w.3.len()),
            ))
        })
        .collect();
    let ranges = (0..threads)
        .map(|k| k * n / threads..(k + 1) * n / threads)
        .collect();
    parallel(ranges, |_, nodes| {
        for v in nodes {
            let id = ids[v].load(Relaxed) - 1;
            let first = starts[id as usize >> 27].0 as u32;
            ids[v].store(first + (id & ((1 << 27) - 1)), Relaxed);
        }
    });
    // Every row has been copied out, so the old arrays (same sizes) take the new graph:
    // overwriting them costs no fresh page faults.
    let (mut off, mut edges, weight_bits) = (g.off, g.edges, g.weight_bits);
    let (off_cells, edge_cells) = (as_atomic(&mut off[1..]), as_atomic(&mut edges));
    let mask = (1 << weight_bits) - 1;
    parallel(
        walks.into_iter().zip(starts).collect(),
        |k, ((degrees, wide, used, spill), (first, at))| {
            let (mut w, mut end) = (wide.into_iter(), at as u32);
            for (i, &d) in degrees.iter().enumerate() {
                end += if d < 255 { d as u32 } else { w.next().unwrap() };
                off_cells[first + i].store(end, Relaxed);
            }
            let rows = copies[k * cap..k * cap + used].iter().chain(&spill);
            for (i, &e) in rows.enumerate() {
                edge_cells[at + i].store(
                    ids[(e >> weight_bits) as usize].load(Relaxed) << weight_bits | e & mask,
                    Relaxed,
                );
            }
        },
    );
    let rename = |&(t, h, w): &(u32, u32, u32)| (rank[t as usize], rank[h as usize], w);
    let mut escapes: Vec<_> = g.escapes.iter().map(rename).collect();
    escapes.sort_unstable();
    (
        rank,
        Csr {
            off,
            edges,
            weight_bits,
            escapes,
        },
        copies,
    )
}

/// A monotone priority queue for Dijkstra: a key lives in the bucket of the highest bit
/// where it differs from the last key popped, so pushes are O(1) and each key moves
/// down at most 64 times. Valid because Dijkstra never pushes below the last pop.
/// Keys are kept in the search's width `T`: a 32-bit search queues only keys that fit
/// (it checks every distance before storing it), so its entries take 8 bytes, not 16.
struct RadixHeap<T> {
    last: u64,
    buckets: Vec<Vec<(T, u32)>>,
}

impl<T: Width> RadixHeap<T> {
    fn new() -> Self {
        RadixHeap {
            last: 0,
            buckets: (0..65).map(|_| Vec::new()).collect(),
        }
    }

    fn push(&mut self, key: u64, v: u32) {
        let entry = (
            T::try_from(key)
                .ok()
                .expect("the search checks every key fits"),
            v,
        );
        self.buckets[(64 - (key ^ self.last).leading_zeros()) as usize].push(entry);
    }

    /// The smallest key, or u64::MAX when empty. Refills bucket 0 from the first
    /// non-empty bucket, whose minimum becomes the new reference key.
    fn min(&mut self) -> u64 {
        if self.buckets[0].is_empty() {
            let Some(b) = self.buckets.iter().position(|b| !b.is_empty()) else {
                return u64::MAX;
            };
            let moved = std::mem::take(&mut self.buckets[b]);
            self.last = moved.iter().map(|e| e.0.into()).min().unwrap();
            for &(key, v) in &moved {
                self.push(key.into(), v);
            }
            self.buckets[b] = moved;
            self.buckets[b].clear();
        }
        self.last
    }

    /// Removes an entry with the smallest key; call `min` first.
    fn pop(&mut self) -> (u64, u32) {
        let (key, v) = self.buckets[0].pop().unwrap();
        (key.into(), v)
    }

    fn clear(&mut self) {
        self.buckets.iter_mut().for_each(Vec::clear);
        self.last = 0;
    }
}

/// Per-thread tentative distances, stored as `d + 1` so that 0 means "not reached", in
/// a `T` of 32 or 64 bits. Nodes are grouped into blocks of `BLOCK`; a block is allocated
/// the first time a search touches it, so memory follows the size of the search, not
/// of the graph.
struct Distances<T> {
    dir: Vec<u32>,
    owners: Vec<u32>,
    blocks: Vec<[T; BLOCK]>,
}

const BLOCK: usize = 128;

/// What a search can store a distance in: u32 or u64.
trait Width: Copy + Default + Into<u64> + TryFrom<u64> {}
impl<T: Copy + Default + Into<u64> + TryFrom<u64>> Width for T {}

impl<T: Width> Distances<T> {
    fn new(n: usize) -> Self {
        // Reserved for the whole graph, which only costs address space: a block's
        // memory is faulted in when first used, and the Vec never has to grow.
        let blocks = Vec::with_capacity(n / BLOCK + 1);
        Distances {
            dir: vec![0; n / BLOCK + 1],
            owners: Vec::new(),
            blocks,
        }
    }

    fn slot(&mut self, v: usize) -> &mut T {
        let entry = &mut self.dir[v / BLOCK];
        if *entry == 0 {
            self.blocks.push([T::default(); BLOCK]);
            self.owners.push((v / BLOCK) as u32);
            *entry = self.blocks.len() as u32;
        }
        &mut self.blocks[*entry as usize - 1][v % BLOCK]
    }

    /// The tentative distance of `v`, if this search has reached it.
    fn get(&self, v: usize) -> Option<u64> {
        match self.dir[v / BLOCK] {
            0 => None,
            b => Into::<u64>::into(self.blocks[b as usize - 1][v % BLOCK]).checked_sub(1),
        }
    }

    fn clear(&mut self) {
        self.owners.drain(..).for_each(|b| self.dir[b as usize] = 0);
        self.blocks.clear();
    }
}

/// One direction of a bidirectional search: tentative distances plus its queue.
struct Side<T> {
    dist: Distances<T>,
    heap: RadixHeap<T>,
}

impl<T: Width> Side<T> {
    fn new(n: usize) -> Self {
        Side {
            dist: Distances::new(n),
            heap: RadixHeap::new(),
        }
    }

    fn start(&mut self, v: usize) {
        self.heap.clear();
        self.dist.clear();
        *self.dist.slot(v) = T::try_from(1).ok().expect("1 fits any width");
        self.heap.push(0, v as u32);
    }

    /// Settles the closest queued node of `g` (a stale entry is just dropped) and returns
    /// the best s-t length found through a node the `other` search has reached, or None
    /// if a distance outgrew `T`.
    #[inline(always)]
    fn step(&mut self, g: &Csr, other: &Distances<T>, mut best: u64) -> Option<u64> {
        let (d, u) = self.heap.pop();
        if d + 1 > (*self.dist.slot(u as usize)).into() {
            return Some(best); // stale: u was settled at a smaller distance
        }
        for (v, w) in g.out(u as usize) {
            let nd = d + w;
            let slot = self.dist.slot(v);
            let known: u64 = (*slot).into();
            if known == 0 || nd < known - 1 {
                *slot = T::try_from(nd + 1).ok()?;
                self.heap.push(nd, v as u32);
                prefetch(&g.edges[g.off[v] as usize..]); // v's row is needed once v settles
                if let Some(rest) = other.get(v) {
                    best = best.min(nd + rest);
                }
            }
        }
        Some(best)
    }
}

/// Bidirectional Dijkstra: forward from `s` over the out-edges `g[0]`, backward from `t`
/// over the in-edges `g[1]`. The sides take turns, so both settle about as many nodes
/// (balancing on keys instead lets one side sweep the whole graph when the other starts
/// behind a 2^32 weight). Once the two smallest keys sum to at least the best meeting
/// length found, no shorter path can remain. An exhausted side (key u64::MAX) also
/// stops it: it has settled every node on its side of any s-t path, so `best` is
/// exact, or u64::MAX if there is no path. This covers every unreachable case cheaply
/// as long as one side is small: a source without out-edges, a target without in-edges.
/// Returns None if a distance does not fit the searches' `T`.
fn distance<T: Width>(g: [&Csr; 2], [f, b]: &mut [Side<T>; 2], s: usize, t: usize) -> Option<i64> {
    if s == t {
        return Some(0);
    }
    f.start(s);
    b.start(t);
    let mut best = u64::MAX;
    for round in 0usize.. {
        let (kf, kb) = (f.heap.min(), b.heap.min());
        if kf.saturating_add(kb) >= best {
            break;
        }
        best = if round % 2 == 0 {
            f.step(g[0], &b.dist, best)?
        } else {
            b.step(g[1], &f.dist, best)?
        };
    }
    Some(best.try_into().unwrap_or(-1)) // u64::MAX (no path) is the one value i64 cannot hold
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Fix glibc's mmap threshold at 1 MiB. Left dynamic, it grows after the first big free,
    // and later big buffers then come from arenas that never give memory back, so every
    // finished build phase would stay in the peak. Also opt out of transparent huge pages:
    // with THP always on, a buffer touched in a few places is charged in 2 MiB pages
    // instead of 4 KiB ones.
    // SAFETY: plain libc calls with constant arguments, made before any other thread exists.
    unsafe {
        mallopt(M_MMAP_THRESHOLD, 1 << 20);
        // Huge pages only where `huge` asks for them; kernels before 6.18 refuse the flag,
        // and then they are off everywhere.
        if prctl(
            PR_SET_THP_DISABLE,
            1u64,
            PR_THP_DISABLE_EXCEPT_ADVISED,
            0u64,
            0u64,
        ) != 0
        {
            prctl(PR_SET_THP_DISABLE, 1u64, 0u64, 0u64, 0u64);
        }
    }
    let threads = available_parallelism().map_or(1, usize::from).min(12);
    Team::start(threads);
    let text = map_file("/data/graph.txt")?;
    let mut i = 0;
    let n = number(text, &mut i) as usize;
    number(text, &mut i); // m: the edge count is implied by the lines themselves
    let text = Text(split_lines(&text[i..], threads));

    let (first, low) = build_csr(&text, threads, n, |u, v| (u, v), Spare::default());
    let (rank, fwd, copies) = bfs_layout(first, threads);
    let node = |id: &str| id.parse().map(|id: usize| rank[id] as usize);
    let queries = std::fs::read_to_string("/data/queries.txt")?
        .lines()
        .map(|line| {
            let (s, t) = line.split_once(' ').ok_or("bad line in queries.txt")?;
            Ok((node(s)?, node(t)?))
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    // The walk's copy buffer, the first build's side array and `rank` become the in-edges.
    let spare = Spare {
        edges: copies,
        low,
        off: rank,
    };
    let (bwd, _) = build_csr(&fwd, threads, n, |u, v| (v, u), spare);

    let answers: Vec<AtomicI64> = (0..queries.len()).map(|_| AtomicI64::new(0)).collect();
    let next = AtomicUsize::new(0);
    parallel(vec![(); threads], |_, ()| {
        // 32-bit distances halve the searches' memory. The rare query whose distances
        // pass 2^32 - 2 (it needs a near-2^32 edge) is answered again with 64 bits.
        let mut narrow = [Side::<u32>::new(n), Side::new(n)];
        let mut wide = [Side::<u64>::new(n), Side::new(n)];
        loop {
            let q = next.fetch_add(1, Relaxed);
            let Some(&(s, t)) = queries.get(q) else { break };
            let g = [&fwd, &bwd];
            let answer = distance(g, &mut narrow, s, t).or_else(|| distance(g, &mut wide, s, t));
            answers[q].store(answer.expect("64-bit distances always fit"), Relaxed);
        }
    });
    let out: String = answers
        .iter()
        .map(|a| format!("{}\n", a.load(Relaxed)))
        .collect();
    std::fs::write("/out/distances.txt", out)?;
    Ok(())
}
