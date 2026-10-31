//! Track A (Ingest): for every (UTC hour, endpoint) group of an HTTP request log, the
//! request count, nearest-rank p50/p95/p99 latency and the 5xx count and rate.
//!
//! One parallel pass over the memory-mapped log appends a 4-byte record per line to a
//! per-worker, per-hour log; then workers report the hours in order (a counting sort
//! by endpoint, then three selections per group) while the main thread writes each
//! hour out. README.md describes the design and why.

use std::arch::x86_64::*;
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Write};
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::thread;

/// Distinct endpoints one worker can hold (a record has 12 bits for the id); the spec
/// promises 1,842 per seed.
const MAX_ENDPOINTS: usize = 3072;
/// A record has 12 bits for an endpoint id or rank, so at most 4,096 names in all.
const NAMES: usize = 1 << 12;
const _: () = assert!(MAX_ENDPOINTS <= NAMES && MAX_ENDPOINTS < SLOTS);
/// Slots of the open-addressing endpoint table: a power of two, at most 3/8 full.
const SLOTS: usize = 8192;
/// Hour rows: 71 either side of the first line's hour, so any 72-hour span that
/// contains the first line fits without knowing where the span starts.
const ROWS: usize = 143;
/// A record's latency field is 19 bits; this value (all ones) marks a latency that
/// did not fit and is kept in `Hour::large` instead.
const MARK: u32 = (1 << 19) - 1;
/// Records per log chunk: 256 KB, so the allocator maps each chunk on its own and
/// unmaps it when the hour is freed.
const CHUNK: usize = 1 << 16;
/// Bytes of the line window: room for the longest line (177 bytes) and for reads of up
/// to 64 bytes at any u8 offset.
const WINDOW: usize = 320;
/// Bytes of input a worker claims at a time.
const PIECE: usize = 1 << 20;
/// Bytes of input whose line ends are found in one go (a multiple of 64).
const SCAN: usize = 4096;

extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
        -> *mut c_void;
    fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}
const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const PR_SET_THP_DISABLE: i32 = 41;
const M_MMAP_THRESHOLD: i32 = -3;

fn main() {
    // Transparent huge pages, `always` on stock Debian, would back malloc's per-thread
    // arenas with 2 MB pages and charge mostly empty ones to our memory peak. Opting
    // out is a hint: if an old kernel refuses it, the run is just as correct.
    // SAFETY: a plain prctl that touches no memory of ours.
    unsafe { prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0) };
    // Pin malloc's mmap threshold at 128 KB (setting it also stops glibc from raising
    // it as mapped blocks are freed), so every 256 KB log chunk is its own mapping and
    // freeing an hour returns its memory to the system at once.
    // SAFETY: mallopt only changes the allocator's tuning; nothing is allocated yet.
    unsafe { mallopt(M_MMAP_THRESHOLD, 128 << 10) };
    if let Err(e) = run() {
        eprintln!("ingest: {e}");
        std::process::exit(1);
    }
}

/// `ingest [data_dir] [out_dir]`, defaulting to the harness's /data and /out.
fn run() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    let input = File::open(format!("{data_dir}/requests.log"))?;
    let mut out = File::create(format!("{out_dir}/summary.txt"))?;
    let len = input.metadata()?.len() as usize;
    if len == 0 {
        return Ok(());
    }
    // SAFETY: a private read-only mapping of the whole file, never unmapped; nothing
    // writes the file while we run. The harness has it in the page cache already, so
    // the mapping costs page-table entries only and its pages are not charged to us.
    let data: &[u8] = unsafe { std::slice::from_raw_parts(map(&input, len)?, len) };
    // The scoring cpuset is 12 CPUs (6 P-cores with hyperthreads); more workers than
    // that would only time-slice.
    let workers = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(12);
    let base = hour_number(&data[..13]) - ROWS as i64 / 2;
    // The file is cut into ~1 MB pieces and each worker owns a contiguous run of them,
    // which it works through front to back; one that runs out steals pieces from the
    // back of another's run. So a worker that started late or ran slow just does
    // less, all finish within a piece of each other, and each still sees few hours.
    let pieces = len.div_ceil(PIECE);
    let runs: Vec<_> = (0..workers)
        .map(|w| Mutex::new(pieces * w / workers..pieces * (w + 1) / workers))
        .collect();
    let shards = thread::scope(|s| {
        let runs = &runs;
        let handles: Vec<_> = (0..workers)
            .map(|w| s.spawn(move || Shard::ingest(data, runs, w, base)))
            .collect();
        let shards = handles
            .into_iter()
            // With panic = "abort" a worker cannot unwind, so join and lock never see
            // a panicked thread or a poisoned lock; the expects document that.
            .map(|h| h.join().expect("no worker panics"));
        shards.collect::<io::Result<Vec<_>>>()
    })?;
    let (names, rank_of) = merge_endpoints(&shards)?;
    // From here on each worker's hour sits behind a lock: the task that reports it
    // needs it mutably, then frees it, while other tasks work on other hours.
    let hours: Vec<Vec<Mutex<Hour>>> = shards
        .into_iter()
        .map(|s| s.hours.into_iter().map(Mutex::new).collect())
        .collect();

    // Workers report the hours in order while this thread writes each one as soon as
    // all earlier ones are out, so the write overlaps the work. The bounded channel
    // stops workers running far ahead of the writer, which bounds the texts held.
    // Written texts go back to a pool: reusing their buffers saves growing fresh ones
    // (page faults, copies) and freeing them on another thread (allocator locks).
    let (next, pool) = (AtomicUsize::new(0), Mutex::new(Vec::new()));
    let (sender, receiver) = mpsc::sync_channel(2 * workers);
    thread::scope(|s| {
        for _ in 0..workers {
            let (sender, next, pool) = (sender.clone(), &next, &pool);
            let (hours, rank_of, names) = (&hours, &rank_of, &names);
            s.spawn(move || {
                let mut latencies = Vec::new();
                loop {
                    let row = next.fetch_add(1, Ordering::Relaxed);
                    if row >= ROWS {
                        return;
                    }
                    let mut text = pool
                        .lock()
                        .expect("no worker panics")
                        .pop()
                        .unwrap_or_default();
                    report(row, hours, rank_of, names, &mut latencies, &mut text);
                    // The text holds all that is left of the hour: free its logs now
                    // (each chunk is its own mapping, so the memory goes back at once).
                    for hour in hours {
                        drop(std::mem::take(
                            &mut hour[row].lock().expect("no worker panics").chunks,
                        ));
                    }
                    if sender.send((row, text)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(sender);
        let mut pending = vec![None; ROWS];
        let mut written = 0;
        for (t, text) in receiver {
            pending[t] = Some(text);
            while let Some(mut text) = pending.get_mut(written).and_then(Option::take) {
                out.write_all(&text)?;
                text.clear();
                pool.lock().expect("no worker panics").push(text);
                written += 1;
            }
        }
        Ok(())
    })
}

/// Maps the first `len` bytes of `file`, private and read-only.
fn map(file: &File, len: usize) -> io::Result<*const u8> {
    // SAFETY: asks for a fresh mapping at an address of the kernel's choosing, so no
    // existing memory is affected; failure is reported as MAP_FAILED (-1).
    let fd = file.as_raw_fd();
    let p = unsafe { mmap(null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0) };
    if p as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(p.cast())
}

/// Where the first line starting at or after byte `at` starts. Piece i holds the lines
/// that start in its nominal range [i * PIECE, (i + 1) * PIECE), so every line is in
/// exactly one piece, and each worker finds its pieces' edges itself.
fn line_start(data: &[u8], at: usize) -> usize {
    match at {
        0 => 0,
        _ if at >= data.len() => data.len(),
        _ => data[at - 1..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(data.len(), |p| at + p),
    }
}

/// Every endpoint any worker saw, in output order, and `ranks[w][id]`: the output
/// rank of worker w's endpoint `id`. Deduplicating through one more hash table and
/// sorting the ~1,842 survivors is far cheaper than sorting every worker's names.
fn merge_endpoints(shards: &[Shard]) -> io::Result<(Vec<Vec<u8>>, Vec<Box<Ranks>>)> {
    let mut union = Endpoints::new();
    let global = shards
        .iter()
        .map(|s| {
            let e = &s.endpoints;
            let id = |l| union.id(&e.arena, e.records[l] + 16, e.end(l), e.hash(l));
            (0..e.len()).map(id).collect()
        })
        .collect::<Option<Vec<Vec<usize>>>>()
        .ok_or_else(too_many_endpoints)?;
    // Output order is plain byte order, exactly `&[u8]`'s Ord (a proper prefix first).
    let mut order: Vec<usize> = (0..union.len()).collect();
    order.sort_unstable_by_key(|&g| union.name(g));
    let mut rank = vec![0; order.len()];
    for (r, &g) in order.iter().enumerate() {
        rank[g] = r as u32;
    }
    if order.len() > NAMES {
        return Err(too_many_endpoints());
    }
    // Ranks are stored already shifted into a record's id bits.
    let ranks = global
        .iter()
        .map(|g| {
            let mut ranks = Box::new([0; NAMES]);
            g.iter()
                .zip(ranks.iter_mut())
                .for_each(|(&g, r)| *r = rank[g] << 20);
            ranks
        })
        .collect();
    Ok((
        order.iter().map(|&g| union.name(g).to_vec()).collect(),
        ranks,
    ))
}

/// Rank (shifted into the id bits of a record) of each of a worker's endpoint ids.
type Ranks = [u32; NAMES];

fn too_many_endpoints() -> io::Error {
    io::Error::other(format!("more than {MAX_ENDPOINTS} distinct endpoints"))
}

/// What one worker learns from its pieces of the file.
struct Shard<'a> {
    endpoints: Endpoints,
    hours: Vec<Hour<'a>>,
}

impl<'a> Shard<'a> {
    fn ingest(
        data: &'a [u8],
        runs: &[Mutex<Range<usize>>],
        me: usize,
        base: i64,
    ) -> io::Result<Self> {
        let mut shard = Shard {
            endpoints: Endpoints::new(),
            hours: (0..ROWS).map(|_| Hour::default()).collect(),
        };
        let claim = |w: usize| {
            let mut run = runs[w].lock().expect("no worker panics");
            if w == me {
                run.next()
            } else {
                run.next_back()
            }
        };
        let order = (me..runs.len()).chain(0..me);
        while let Some(piece) = order.clone().find_map(claim) {
            let edges = (
                line_start(data, piece * PIECE),
                line_start(data, (piece + 1) * PIECE),
            );
            shard.ingest_piece(data, edges, base)?;
        }
        Ok(shard)
    }

    /// Files every line of `data[start..end]`.
    fn ingest_piece(
        &mut self,
        data: &'a [u8],
        (start, end): (usize, usize),
        base: i64,
    ) -> io::Result<()> {
        // Lines arrive in roughly time order, so the hour is cached on its 13-byte text
        // `YYYY-MM-DDTHH`, compared as the two overlapping 8-byte words at 0 and 5.
        let (mut seen, mut row) = (([0; 8], [0; 8]), 0);
        let (mut line, mut ends, mut padded) = (start, [0; 2 * SCAN / 64], [0; WINDOW]);
        for scan in (start..end).step_by(SCAN) {
            for &nl in newlines(data, scan, end.min(scan + SCAN), &mut ends) {
                // Every field is read from one fixed-size window of the line at an offset
                // that is a u8: the compiler can prove each read in bounds and drops the
                // checks (they were ~100 instructions a line). Only the last lines of
                // the file need a zero-padded copy.
                let w: &[u8; WINDOW] = match data.get(line..line + WINDOW) {
                    Some(w) => w.try_into().expect("a slice of WINDOW bytes"),
                    None => {
                        padded.fill(0);
                        padded[..data.len() - line].copy_from_slice(&data[line..]);
                        &padded
                    }
                };
                let nl = u8::try_from(nl - line).map_err(|_| io::Error::other("line too long"))?;
                let key = (word::<8>(w, 0), word::<8>(w, 5));
                if key != seen {
                    seen = key;
                    row = self.hour_row(data, line, base)?;
                }
                let (latency, sp) = parse_latency(w, nl);
                // The method is 3 to 7 bytes after the 27-byte timestamp and a space,
                // so the endpoint's leading '/' is one of the 8 bytes at offset 32. XOR
                // turns '/' into a zero byte; the SWAR zero-byte test flags the first.
                let x = u64::from_le_bytes(word(w, 32)) ^ 0x2f2f_2f2f_2f2f_2f2f;
                let zero = x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080;
                let (start, end) = (32 + zero.trailing_zeros() as u8 / 8, sp - 4);
                let hash = endpoint_hash(w, start, end);
                let id = self.endpoints.id(w, start.into(), end.into(), hash);
                let id = id.ok_or_else(too_many_endpoints)?;
                self.hours[row].push(id, latency, w[usize::from(sp - 3)] == b'5');
                line += usize::from(nl) + 1;
            }
        }
        Ok(())
    }

    /// The row of the line's hour, relative to `base`; starts the hour if new.
    fn hour_row(&mut self, data: &'a [u8], line: usize, base: i64) -> io::Result<usize> {
        let text = &data[line..line + 13];
        let row = usize::try_from(hour_number(text) - base)
            .ok()
            .filter(|&r| r < ROWS)
            .ok_or_else(|| io::Error::other("timestamps span more than 72 hours"))?;
        let hour = &mut self.hours[row];
        if hour.prefix.is_empty() {
            hour.prefix = text;
        }
        Ok(row)
    }
}

/// What one worker saw of one hour: an append-only log with one 4-byte record per
/// line, `endpoint id << 20 | 5xx << 19 | latency`. 97% of latencies are below 2^19
/// (0.5 s); a larger one is logged as `MARK` and kept in `large`. Appending is a
/// sequential write per line, and the report phase reads each log front to back.
#[derive(Default)]
struct Hour<'a> {
    /// The `YYYY-MM-DDTHH` of one of its lines, to print the hour; empty if none.
    prefix: &'a [u8],
    chunks: Vec<Box<[u32; CHUNK]>>,
    len: usize,
    /// (endpoint id, latency) of every latency of `MARK` or more.
    large: Vec<(u32, u32)>,
}

impl Hour<'_> {
    #[inline(always)]
    fn push(&mut self, id: usize, latency: u32, error: bool) {
        if latency >= MARK {
            self.large.push((id as u32, latency));
        }
        if self.len % CHUNK == 0 {
            // Allocated zeroed: fresh pages from the kernel, faulted in as used.
            let chunk = vec![0; CHUNK].into_boxed_slice().try_into();
            self.chunks.push(chunk.expect("a slice of CHUNK records"));
        }
        let record = (id as u32) << 20 | u32::from(error) << 19 | latency.min(MARK);
        self.chunks[self.len / CHUNK][self.len % CHUNK] = record;
        self.len += 1;
    }

    /// The log, as one slice per chunk.
    fn records(&self) -> impl Iterator<Item = &[u32]> {
        let used = |i: usize| (self.len - i * CHUNK).min(CHUNK);
        self.chunks
            .iter()
            .enumerate()
            .map(move |(i, c)| &c[..used(i)])
    }
}

/// Hash of endpoint `w[start..end]` from its first and last 8 bytes and its length.
/// Both loads stay inside the line (28 bytes precede an endpoint, at least 7 follow
/// it); bytes outside the name are masked off, as they differ between its lines.
#[inline(always)]
fn endpoint_hash(w: &[u8; WINDOW], start: u8, end: u8) -> u64 {
    let len = end - start;
    let head = u64::from_le_bytes(word(w, start)) & (u64::MAX >> (8 * 8u8.saturating_sub(len)));
    let tail = if len >= 8 {
        u64::from_le_bytes(word(w, end - 8))
    } else {
        0
    };
    (head ^ tail.rotate_left(32) ^ len as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// Open-addressing hash table from endpoint bytes to dense ids 0, 1, 2, ...
struct Endpoints {
    /// One record per name, back to back: its hash (8 bytes), its id and length (4 + 4),
    /// the name, zero padding to a multiple of 8; then 64 zero bytes, so `same` can
    /// always load 64 bytes from a name's start. A hit reads its slot, then one record,
    /// usually one cache line. Keeping the names here, not pointing into the input,
    /// matters: compares against the first occurrence of each name in the input touched
    /// ~1,842 scattered pages and cost a TLB miss on most lines.
    arena: Vec<u8>,
    /// Id -> offset of its record in `arena`.
    records: Vec<usize>,
    /// Hash slot -> 1 + offset of a record, or 0 for an empty slot.
    slots: Box<[u32; SLOTS]>,
}

impl Endpoints {
    fn new() -> Self {
        Endpoints {
            arena: vec![0; 64],
            records: Vec::new(),
            slots: Box::new([0; SLOTS]),
        }
    }

    fn len(&self) -> usize {
        self.records.len()
    }

    fn hash(&self, id: usize) -> u64 {
        load(&self.arena, self.records[id])
    }

    fn name(&self, id: usize) -> &[u8] {
        &self.arena[self.records[id] + 16..self.end(id)]
    }

    /// Where the name of `id` ends in `arena`.
    fn end(&self, id: usize) -> usize {
        let at = self.records[id];
        at + 16 + (load(&self.arena, at + 8) >> 32) as usize
    }

    /// The id of the name `text[start..end]`, which hashes to `hash`, adding it if
    /// new; None when the table is full.
    #[inline(always)]
    fn id(&mut self, text: &[u8], start: usize, end: usize, hash: u64) -> Option<usize> {
        let len = end - start;
        let mut slot = (hash >> (64 - SLOTS.trailing_zeros())) as usize;
        while self.slots[slot] != 0 {
            let at = self.slots[slot] as usize - 1;
            let header = &self.arena[at..at + 16];
            let id_len = load(header, 8);
            if load(header, 0) == hash
                && (id_len >> 32) as usize == len
                && same(&self.arena, at + 16, text, start, len)
            {
                return Some(id_len as u32 as usize);
            }
            slot = (slot + 1) % SLOTS;
        }
        if self.len() == MAX_ENDPOINTS {
            return None;
        }
        let at = self.arena.len() - 64;
        self.arena.truncate(at);
        self.arena.extend_from_slice(&hash.to_le_bytes());
        self.arena
            .extend_from_slice(&((len as u64) << 32 | self.len() as u64).to_le_bytes());
        self.arena.extend_from_slice(&text[start..end]);
        self.arena
            .resize(self.arena.len().next_multiple_of(8) + 64, 0);
        self.slots[slot] = at as u32 + 1;
        self.records.push(at);
        Some(self.len() - 1)
    }
}

/// Whether `a[i..i + n]` equals `b[j..j + n]`. Names of up to 64 bytes, nearly all of
/// them, are compared as two 32-byte vector compares whose result bits past `n` are
/// masked off: no length-dependent branch to mispredict and no memcmp call.
#[inline(always)]
fn same(a: &[u8], i: usize, b: &[u8], j: usize, n: usize) -> bool {
    match (a.get(i..i + 64), b.get(j..j + 64)) {
        (Some(x), Some(y)) if n <= 64 => {
            // SAFETY: each load reads 32 bytes inside a 64-byte slice. AVX2 is enabled
            // crate-wide (target-cpu=x86-64-v3 in .cargo/config.toml).
            let equal = unsafe {
                let eq = |k: usize| {
                    let x = _mm256_loadu_si256(x.as_ptr().add(k).cast());
                    let y = _mm256_loadu_si256(y.as_ptr().add(k).cast());
                    _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, y)) as u32
                };
                u64::from(eq(0)) | u64::from(eq(32)) << 32
            };
            let wanted = u64::MAX >> (64 - n);
            equal & wanted == wanted
        }
        _ => a[i..i + n] == b[j..j + n],
    }
}

/// The report lines of hour `row`, `hour endpoint count p50 p95 p99 errors error_ppm`,
/// appended to `text`. The workers' logs of the hour are counting-sorted by endpoint
/// rank into `latencies`, so every group ends up as one contiguous slice: two
/// sequential passes over the logs, no pointer chasing.
fn report(
    row: usize,
    hours: &[Vec<Mutex<Hour>>],
    rank_of: &[Box<Ranks>],
    names: &[Vec<u8>],
    latencies: &mut Vec<u32>,
    text: &mut Vec<u8>,
) {
    // Each worker read a few hours' worth of the file, so most miss most hours.
    let mut locked: Vec<_> = hours
        .iter()
        .map(|h| h[row].lock().expect("no worker panics"))
        .collect();
    let mut seen: Vec<_> = locked
        .iter_mut()
        .zip(rank_of)
        .filter(|(h, _)| !h.prefix.is_empty())
        .collect();
    let Some(prefix) = seen.first().map(|(h, _)| h.prefix) else {
        return;
    };
    // Count lines and errors per endpoint, and rewrite each record's worker-local
    // id as the endpoint's global rank, so the second pass needs no lookup. Ids and
    // ranks are the records' top 12 bits, and these tables have 4,096 entries, so
    // the compiler sees that no index can be out of bounds and checks none.
    let mut counts = [[0u32; 2]; NAMES]; // per endpoint: lines, errors
    for (hour, rank_of) in &mut seen {
        let len = hour.len;
        for (i, chunk) in hour.chunks.iter_mut().enumerate() {
            for record in &mut chunk[..(len - i * CHUNK).min(CHUNK)] {
                let rank = rank_of[(*record >> 20) as usize];
                let count = &mut counts[(rank >> 20) as usize];
                count[0] += 1;
                count[1] += *record >> 19 & 1;
                *record = rank | *record & MARK;
            }
        }
    }
    let mut ends = [0usize; NAMES];
    let mut end = 0;
    for (e, c) in ends.iter_mut().zip(&counts) {
        end += c[0] as usize;
        *e = end;
    }
    latencies.clear();
    latencies.resize(end, 0);
    // Fill each group from its end backwards, leaving `ends` at the group starts.
    let mut put = |rank: u32, latency| {
        let end = &mut ends[(rank >> 20) as usize];
        *end -= 1;
        latencies[*end] = latency;
    };
    for (hour, rank_of) in &seen {
        for chunk in hour.records() {
            for &record in chunk.iter().filter(|&&r| r & MARK != MARK) {
                put(record, record & MARK);
            }
        }
        for &(id, latency) in &hour.large {
            put(rank_of[id as usize & (NAMES - 1)], latency);
        }
    }
    // At most 20 + 1 + 125 + 6 * 11 + 1 bytes a line: reserve once, never regrow.
    text.reserve(counts.iter().filter(|c| c[0] > 0).count() * 213);
    for (rank, &[count, errors]) in counts[..names.len()]
        .iter()
        .enumerate()
        .filter(|(_, c)| c[0] > 0)
    {
        let group = &mut latencies[ends[rank]..ends[rank] + count as usize];
        let [p50, p95, p99] = percentiles(group);
        let ppm = (u64::from(errors) * 1_000_000 / u64::from(count)) as u32;
        text.extend_from_slice(prefix);
        text.extend_from_slice(b":00:00Z ");
        text.extend_from_slice(&names[rank]);
        for n in [count, p50, p95, p99, errors, ppm] {
            text.push(b' ');
            push_decimal(text, n);
        }
        text.push(b'\n');
    }
}

/// Nearest-rank p50, p95 and p99 (rank ceil(p * n / 100)), by repeated selection: each
/// select leaves the larger values to its right, so the next, higher rank only needs
/// to search that suffix: n + n/2 + n/20 steps in all.
fn percentiles(values: &mut [u32]) -> [u32; 3] {
    let n = values.len();
    if n <= 32 {
        // Most groups are this small: one sort_unstable (an insertion sort at this size)
        // beats three selections.
        values.sort_unstable();
        return [50, 95, 99].map(|p| values[(p * n + 99) / 100 - 1]);
    }
    let (mut from, mut value) = (0, 0); // values[from..] hold the ranks not yet fixed
    [50, 95, 99].map(|p| {
        let rank = (p * n + 99) / 100 - 1;
        if rank >= from {
            value = *values[from..].select_nth_unstable(rank - from).1;
            from = rank + 1;
        }
        value
    })
}

/// The newlines of `data[from..to]`, written to `ends`. A line has at least 40 bytes,
/// so a 64-byte block holds at most two: both candidates are stored unconditionally
/// and the count advances by the popcount, so the per-line loop that follows has no
/// hard-to-predict exit at every block boundary.
fn newlines<'e>(data: &[u8], from: usize, to: usize, ends: &'e mut [usize]) -> &'e [usize] {
    let mut n = 0;
    for block in (from..to).step_by(64) {
        let mask = newline_mask(data, block, to);
        ends[n] = block + mask.trailing_zeros() as usize;
        ends[n + 1] = block + 63 - mask.leading_zeros() as usize;
        n += mask.count_ones() as usize;
    }
    &ends[..n]
}

/// Bit i is set when `data[at + i]` is a newline before `end`: two AVX2 compares.
fn newline_mask(data: &[u8], at: usize, end: usize) -> u64 {
    let mut padded = [0; 64];
    let chunk = data.get(at..at + 64).unwrap_or_else(|| {
        padded[..data.len() - at].copy_from_slice(&data[at..]);
        &padded
    });
    // SAFETY: both 32-byte loads read inside `chunk`, which is 64 bytes long. AVX2 is
    // enabled crate-wide (target-cpu=x86-64-v3).
    let mask = unsafe {
        let nl = _mm256_set1_epi8(b'\n' as i8);
        let lo = _mm256_loadu_si256(chunk.as_ptr().cast());
        let hi = _mm256_loadu_si256(chunk.as_ptr().add(32).cast());
        let lo = _mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, nl)) as u32;
        let hi = _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, nl)) as u32;
        u64::from(lo) | u64::from(hi) << 32
    };
    if end - at < 64 {
        mask & ((1 << (end - at)) - 1)
    } else {
        mask
    }
}

/// The latency that ends at newline `nl`, and the index of the space before it,
/// without a loop: the 16 bytes before `nl` always hold that space (a latency has at
/// most 10 digits), so one compare finds it, and the digits after it are summed as a
/// 16-digit number with leading zeros by multiply-add steps (pairs, quads, octets).
#[inline(always)]
fn parse_latency(w: &[u8; WINDOW], nl: u8) -> (u32, u8) {
    let tail: [u8; 16] = word(w, nl - 16);
    // SAFETY: the load reads the 16 bytes of `tail`. SSE4.1 and SSSE3 are enabled
    // crate-wide (target-cpu=x86-64-v3).
    unsafe {
        let v = _mm_loadu_si128(tail.as_ptr().cast());
        let spaces = _mm_movemask_epi8(_mm_cmpeq_epi8(v, _mm_set1_epi8(b' ' as i8))) as u32;
        let space = 31 - spaces.leading_zeros() as i32;
        let index = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        let digits = _mm_and_si128(
            _mm_sub_epi8(v, _mm_set1_epi8(b'0' as i8)),
            _mm_cmpgt_epi8(index, _mm_set1_epi8(space as i8)),
        );
        // Weights (10, 1), (100, 1) and (10000, 1), splatted as little-endian pairs.
        let pairs = _mm_maddubs_epi16(digits, _mm_set1_epi16(0x010a));
        let quads = _mm_madd_epi16(pairs, _mm_set1_epi32(0x0001_0064));
        let quads = _mm_packus_epi32(quads, quads);
        let octets = _mm_madd_epi16(quads, _mm_set1_epi32(0x0001_2710));
        let high = _mm_cvtsi128_si32(octets) as u32 as u64;
        let low = _mm_extract_epi32::<1>(octets) as u32 as u64;
        ((high * 100_000_000 + low) as u32, nl - 16 + space as u8)
    }
}

/// Absolute hour number of `YYYY-MM-DDTHH` (days since 1970 times 24, plus the hour),
/// using the days-from-civil algorithm from Howard Hinnant's date algorithms.
fn hour_number(text: &[u8]) -> i64 {
    let digit = |i: usize| i64::from(text[i] - b'0');
    let num = |from: usize, to: usize| (from..to).fold(0, |n, i| n * 10 + digit(i));
    let (month, day) = (num(5, 7), num(8, 10));
    let year = num(0, 4) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    (era * 146_097 + day_of_era - 719_468) * 24 + num(11, 13)
}

/// The N bytes of the line window at offset `at`. Since `at` is a u8 and the window
/// has room for 64 more bytes past offset 255, the compiler sees this never fails.
#[inline(always)]
fn word<const N: usize>(w: &[u8; WINDOW], at: u8) -> [u8; N] {
    let mut bytes = [0; N];
    bytes.copy_from_slice(&w[usize::from(at)..usize::from(at) + N]);
    bytes
}

fn load(data: &[u8], at: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// Appends `value` in decimal. All ten digit places are filled two at a time from a
/// table, with no loop whose length depends on the value (its exit was the report
/// phase's most mispredicted branch), and then only the significant ones are kept.
fn push_decimal(out: &mut Vec<u8>, value: u32) {
    let (mut digits, mut rest) = ([0; 10], value as usize);
    for pair in digits.rchunks_exact_mut(2) {
        pair.copy_from_slice(&DIGIT_PAIRS[2 * (rest % 100)..][..2]);
        rest /= 100;
    }
    out.extend_from_slice(&digits[9 - value.checked_ilog10().unwrap_or(0) as usize..]);
}

/// "00" to "99", back to back.
const DIGIT_PAIRS: [u8; 200] = {
    let mut table = [0; 200];
    let mut i = 0;
    while i < 200 {
        table[i] = b'0' + (i / 2 / [10, 1][i % 2] % 10) as u8;
        i += 1;
    }
    table
};
