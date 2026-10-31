//! Track A (Ingest): for every (UTC hour, endpoint) group of an HTTP request log, the
//! request count, nearest-rank p50/p95/p99 latency and the 5xx count and rate.
//!
//! One parallel pass over the memory-mapped log files every latency into per-worker,
//! per-group block chains; a second parallel pass walks the groups hour by hour and
//! selects the three ranks; a third formats the hours and writes them in place.
//! README.md describes the design and why.

use std::arch::x86_64::*;
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Write};
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::ptr::null_mut;
use std::sync::Mutex;
use std::thread;

/// Distinct endpoints one worker can hold. The spec promises 1,842 per seed; table rows
/// are this wide, but only the cells of endpoints actually seen are ever touched.
const MAX_ENDPOINTS: usize = 4096;
/// Slots of the open-addressing endpoint table: a power of two, at most half full.
const SLOTS: usize = 2 * MAX_ENDPOINTS;
/// Hour rows of a group table: 71 either side of the first line's hour, so any 72-hour
/// span that contains the first line fits without knowing where the span starts.
const ROWS: usize = 143;
/// Latencies per arena block: one cache line, and a power of two so the slot of the
/// next latency is a mask of the group's count.
const BLOCK: usize = 16;
const NONE: usize = usize::MAX;

extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
        -> *mut c_void;
}
const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;

fn main() {
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
    let workers = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(12);
    let base = hour_number(&data[..13]) - ROWS as i64 / 2;
    let shards = parallel(workers, split(data, workers), |range| {
        Shard::ingest(data, range, base)
    })
    .into_iter()
    .collect::<io::Result<Vec<_>>>()?;
    let (union, order, ids) = merge_endpoints(&shards)?;
    let names: Vec<&[u8]> = order.iter().map(|&g| union.name(g)).collect();
    let hours: Vec<Option<&[u8]>> = (0..ROWS)
        .map(|r| shards.iter().find_map(|s| s.hours[r]))
        .collect();
    // Hours differ up to fivefold in traffic, so for balance each is cut into four
    // tasks by endpoint rank; task order is still output order.
    let n = names.len();
    let tasks: Vec<(usize, Range<usize>)> = (0..ROWS * 4)
        .map(|t| (t / 4, n * (t % 4) / 4..n * (t % 4 + 1) / 4))
        .collect();
    let groups = parallel(workers, tasks.iter().cloned(), |(row, ranks)| {
        summarise(row, ranks, &shards, &ids)
    });
    // The block chains are spent: give their memory back before the output's page
    // cache starts to grow, so the two never count towards the peak together.
    drop(shards);

    // Format the hours in parallel, then write them in order. Writes to one file
    // serialise on its inode lock anyway, so one writer is as fast as many.
    let texts = parallel(workers, tasks.iter().zip(&groups), |(&(row, _), groups)| {
        let mut text = Vec::new();
        if let Some(hour) = hours[row] {
            groups
                .iter()
                .for_each(|g| g.format(hour, &names, &mut text));
        }
        text
    });
    texts.iter().try_for_each(|text| out.write_all(text))
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

/// Runs `task` on every item on up to `workers` threads, which take the next item
/// from a shared queue, and returns the results in item order.
fn parallel<I: Send, T: Send>(
    workers: usize,
    items: impl IntoIterator<Item = I>,
    task: impl Fn(I) -> T + Sync,
) -> Vec<T> {
    let items: Vec<I> = items.into_iter().collect();
    let threads = workers.min(items.len());
    let queue = Mutex::new(items.into_iter().enumerate());
    let mut done: Vec<(usize, T)> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let next = queue.lock().expect("no worker panics").next();
                        let Some((i, item)) = next else { return mine };
                        mine.push((i, task(item)));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("no worker panics"))
            .collect()
    });
    done.sort_unstable_by_key(|&(i, _)| i);
    done.into_iter().map(|(_, result)| result).collect()
}

/// Cuts the file into `parts` ranges of whole lines.
fn split(data: &[u8], parts: usize) -> Vec<(usize, usize)> {
    let mut bounds = vec![0];
    for i in 1..parts {
        let from = (data.len() * i / parts).max(bounds[i - 1]);
        let cut = data[from..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(data.len(), |p| from + p + 1);
        bounds.push(cut);
    }
    bounds.push(data.len());
    bounds.windows(2).map(|w| (w[0], w[1])).collect()
}

/// Every endpoint any worker saw, as one table, its ids in output order, and
/// `ids[w][r]`: worker w's id for the endpoint of rank r, or NONE. Deduplicating
/// through one more hash table and sorting the ~1,842 survivors is far cheaper than
/// sorting the concatenation of every worker's names.
fn merge_endpoints(shards: &[Shard]) -> io::Result<(Endpoints, Vec<usize>, Vec<Vec<usize>>)> {
    let mut union = Endpoints::new();
    let global = shards
        .iter()
        .map(|s| {
            let e = &s.endpoints;
            e.entries
                .iter()
                .map(|&(hash, start, end)| union.id(&e.bytes, start, end, hash))
                .collect()
        })
        .collect::<Option<Vec<Vec<usize>>>>()
        .ok_or_else(too_many_endpoints)?;
    // Output order is plain byte order, exactly `&[u8]`'s Ord (a proper prefix first).
    let mut order: Vec<usize> = (0..union.len()).collect();
    order.sort_unstable_by_key(|&g| union.name(g));
    let mut rank = vec![0; order.len()];
    for (r, &g) in order.iter().enumerate() {
        rank[g] = r;
    }
    let ids = global
        .iter()
        .map(|global| {
            let mut ids = vec![NONE; order.len()];
            for (local, &g) in global.iter().enumerate() {
                ids[rank[g]] = local;
            }
            ids
        })
        .collect();
    Ok((union, order, ids))
}

fn too_many_endpoints() -> io::Error {
    io::Error::other(format!("more than {MAX_ENDPOINTS} distinct endpoints"))
}

/// What one worker learns from its range of the file.
struct Shard<'a> {
    endpoints: Endpoints,
    /// Hour row -> the `YYYY-MM-DDTHH` prefix of one of its lines, to print the hour.
    hours: Vec<Option<&'a [u8]>>,
    /// `row * MAX_ENDPOINTS + id` -> [count, 5xx count, 1 + newest block, or 0].
    /// Allocated zeroed and touched sparsely, so untouched rows never cost memory.
    groups: Vec<[u32; 3]>,
    /// Bump arena of latency blocks, chained newest to oldest per group: `links[b]`
    /// is 1 + the block before block b in its group's chain, or 0.
    blocks: Vec<[u32; BLOCK]>,
    links: Vec<u32>,
}

impl<'a> Shard<'a> {
    fn ingest(data: &'a [u8], (start, end): (usize, usize), base: i64) -> io::Result<Self> {
        let mut shard = Shard {
            endpoints: Endpoints::new(),
            hours: vec![None; ROWS],
            groups: vec![[0; 3]; ROWS * MAX_ENDPOINTS],
            // A line is at least 40 bytes, so this never reallocates. Untouched capacity
            // is never faulted in, so the generous reservation is free.
            blocks: Vec::with_capacity((end - start) / 40 + 1),
            links: Vec::with_capacity((end - start) / 40 + 1),
        };
        // Lines arrive in roughly time order, so the hour is cached on its 13-byte text.
        let (mut seen, mut row) = ((0, 0), 0);
        let mut line = start;
        for block in (start..end).step_by(64) {
            let mut mask = newline_mask(data, block, end);
            while mask != 0 {
                let nl = block + mask.trailing_zeros() as usize;
                mask &= mask - 1;
                let key = (load(data, line), load(data, line + 5));
                if key != seen {
                    seen = key;
                    row = shard.hour_row(data, line, base)?;
                }
                shard.add(data, line, nl, row)?;
                line = nl + 1;
            }
        }
        Ok(shard)
    }

    /// The row of the line's hour, relative to `base`.
    fn hour_row(&mut self, data: &'a [u8], line: usize, base: i64) -> io::Result<usize> {
        let text = &data[line..line + 13];
        let row = usize::try_from(hour_number(text) - base)
            .ok()
            .filter(|&r| r < ROWS)
            .ok_or_else(|| io::Error::other("timestamps span more than 72 hours"))?;
        self.hours[row].get_or_insert(text);
        Ok(row)
    }

    /// Files one line, `ts SP method SP endpoint SP status SP latency`, ending at `nl`.
    fn add(&mut self, data: &[u8], line: usize, nl: usize, row: usize) -> io::Result<()> {
        let (latency, sp) = parse_latency(data, nl);
        let error = data[sp - 3] == b'5';
        // The method is 3 to 7 bytes after the 27-byte timestamp and a space, so the
        // endpoint's leading '/' is one of the 8 bytes at line + 32. XOR turns '/' into
        // a zero byte, and the classic SWAR zero-byte test flags the first one.
        let x = load(data, line + 32) ^ 0x2f2f_2f2f_2f2f_2f2f;
        let zero = x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080;
        let start = line + 32 + zero.trailing_zeros() as usize / 8;
        let end = sp - 4;
        let id = self
            .endpoints
            .id(data, start, end, endpoint_hash(data, start, end))
            .ok_or_else(too_many_endpoints)?;

        let group = &mut self.groups[row * MAX_ENDPOINTS + id];
        let used = group[0] as usize % BLOCK;
        if used == 0 {
            self.blocks.push([0; BLOCK]);
            self.links.push(group[2]);
            group[2] = self.blocks.len() as u32;
        }
        self.blocks[group[2] as usize - 1][used] = latency;
        group[0] += 1;
        group[1] += u32::from(error);
        Ok(())
    }

    /// Appends the latencies of group cell `cell` to `out`; returns its 5xx count.
    fn collect(&self, cell: usize, out: &mut Vec<u32>) -> u32 {
        let [count, errors, mut block] = self.groups[cell];
        // The newest block holds 1..=BLOCK latencies, every older one is full.
        let mut fill = (count as usize + BLOCK - 1) % BLOCK + 1;
        while block != 0 {
            out.extend_from_slice(&self.blocks[block as usize - 1][..fill]);
            block = self.links[block as usize - 1];
            fill = BLOCK;
        }
        errors
    }
}

/// Hash of endpoint `data[start..end]` from its first and last 8 bytes and its length.
/// Both loads stay inside the line (28 bytes precede an endpoint, at least 7 follow
/// it); bytes outside the name are masked off, as they differ between its lines.
fn endpoint_hash(data: &[u8], start: usize, end: usize) -> u64 {
    let len = end - start;
    let head = load(data, start) & (u64::MAX >> (8 * 8usize.saturating_sub(len)));
    let tail = if len >= 8 { load(data, end - 8) } else { 0 };
    (head ^ tail.rotate_left(32) ^ len as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// Open-addressing hash table from endpoint bytes to dense ids 0, 1, 2, ...
struct Endpoints {
    /// Every name back to back, then 64 zero bytes so `same` can always load 64 bytes
    /// from a name's start. Lookups compare against these few hot cache lines, not
    /// against wherever in the input a name first appeared: that scattered the
    /// compares over ~1,842 pages and cost a TLB miss on most lines.
    bytes: Vec<u8>,
    /// Id -> (hash, start, end of its name in `bytes`).
    entries: Vec<(u64, usize, usize)>,
    /// Hash slot -> id + 1, or 0 for an empty slot.
    slots: Vec<u16>,
}

impl Endpoints {
    fn new() -> Self {
        Endpoints {
            bytes: vec![0; 64],
            entries: Vec::new(),
            slots: vec![0; SLOTS],
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn name(&self, id: usize) -> &[u8] {
        let (_, start, end) = self.entries[id];
        &self.bytes[start..end]
    }

    /// The id of the name `text[start..end]`, which hashes to `hash`, adding it if
    /// new; None when the table is full.
    fn id(&mut self, text: &[u8], start: usize, end: usize, hash: u64) -> Option<usize> {
        let mut slot = (hash >> (64 - SLOTS.trailing_zeros())) as usize;
        while self.slots[slot] != 0 {
            let id = self.slots[slot] as usize - 1;
            let (h, at, to) = self.entries[id];
            if h == hash
                && to - at == end - start
                && same(&self.bytes, at, text, start, end - start)
            {
                return Some(id);
            }
            slot = (slot + 1) % SLOTS;
        }
        if self.len() == MAX_ENDPOINTS {
            return None;
        }
        let at = self.bytes.len() - 64;
        self.bytes.truncate(at);
        self.bytes.extend_from_slice(&text[start..end]);
        self.bytes.resize(self.bytes.len() + 64, 0);
        self.entries.push((hash, at, at + end - start));
        self.slots[slot] = self.len() as u16;
        Some(self.len() - 1)
    }
}

/// Whether `a[i..i + n]` equals `b[j..j + n]`. Names of up to 64 bytes, nearly all of
/// them, are compared as two 32-byte vector compares whose result bits past `n` are
/// masked off: no length-dependent branch to mispredict and no memcmp call.
fn same(a: &[u8], i: usize, b: &[u8], j: usize, n: usize) -> bool {
    match (a.get(i..i + 64), b.get(j..j + 64)) {
        (Some(x), Some(y)) if n <= 64 => {
            // SAFETY: each load reads 32 bytes inside a 64-byte slice.
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

/// One output line before formatting: the endpoint's rank, then count, p50, p95,
/// p99 and errors (24 bytes, as all 127k of them are held at the memory peak).
struct Group(u32, [u32; 5]);

impl Group {
    /// `hour endpoint count p50 p95 p99 errors error_ppm`, from the hour's 13-byte prefix.
    fn format(&self, hour: &[u8], names: &[&[u8]], out: &mut Vec<u8>) {
        out.extend_from_slice(hour);
        out.extend_from_slice(b":00:00Z ");
        out.extend_from_slice(names[self.0 as usize]);
        let [count, .., errors] = self.1.map(u64::from);
        for n in self
            .1
            .map(u64::from)
            .into_iter()
            .chain([errors * 1_000_000 / count])
        {
            out.push(b' ');
            push_decimal(out, n);
        }
        out.push(b'\n');
    }
}

/// Every group of hour `row` whose endpoint rank is in `ranks`, in rank order.
fn summarise(row: usize, ranks: Range<usize>, shards: &[Shard], ids: &[Vec<usize>]) -> Vec<Group> {
    // Each worker read a few hours' worth of the file, so most miss most hours.
    let present: Vec<_> = shards
        .iter()
        .zip(ids)
        .filter(|(s, _)| s.hours[row].is_some())
        .collect();
    let (mut groups, mut latencies) = (Vec::new(), Vec::new());
    for rank in ranks {
        latencies.clear();
        let mut errors = 0;
        for (shard, ids) in &present {
            if ids[rank] != NONE {
                errors += shard.collect(row * MAX_ENDPOINTS + ids[rank], &mut latencies);
            }
        }
        if !latencies.is_empty() {
            let count = latencies.len() as u32;
            let [p99, p95, p50] = percentiles(&mut latencies);
            groups.push(Group(rank as u32, [count, p50, p95, p99, errors]));
        }
    }
    groups
}

/// Nearest-rank p99, p95 and p50 (rank ceil(p * n / 100)), by repeated selection: each
/// select leaves the smaller values to its left, so the next, lower rank only needs to
/// search that prefix.
fn percentiles(values: &mut [u32]) -> [u32; 3] {
    let n = values.len();
    let mut last = (n, 0);
    [99, 95, 50].map(|p| {
        let rank = (p * n + 99) / 100 - 1;
        if rank < last.0 {
            last = (rank, *values[..last.0].select_nth_unstable(rank).1);
        }
        last.1
    })
}

/// Bit i is set when `data[at + i]` is a newline before `end`: two AVX2 compares.
fn newline_mask(data: &[u8], at: usize, end: usize) -> u64 {
    let mut padded = [0; 64];
    let chunk = data.get(at..at + 64).unwrap_or_else(|| {
        padded[..data.len() - at].copy_from_slice(&data[at..]);
        &padded
    });
    // SAFETY: both 32-byte loads read inside `chunk`, which is 64 bytes long.
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
fn parse_latency(data: &[u8], nl: usize) -> (u32, usize) {
    let tail = &data[nl - 16..nl];
    // SAFETY: the load reads the 16 bytes of `tail`.
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
        ((high * 100_000_000 + low) as u32, nl - 16 + space as usize)
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

fn load(data: &[u8], at: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(bytes)
}

fn push_decimal(out: &mut Vec<u8>, mut value: u64) {
    let mut digits = [0; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[i..]);
}
