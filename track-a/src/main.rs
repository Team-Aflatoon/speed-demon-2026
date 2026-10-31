//! Track A (Ingest): for every (UTC hour, endpoint) group of an HTTP request log, the
//! request count, nearest-rank p50/p95/p99 latency and the 5xx count and rate.
//!
//! Workers parse the memory-mapped log, in file order, into one 4-byte record per line
//! appended to that hour's log. An hour is reported as soon as the parsed prefix of the
//! file has moved two hours past it, while parsing goes on: its log is counting-sorted
//! by endpoint, so every group is one slice, three selections give the percentiles, the
//! log is freed, and the main thread writes the text once all earlier hours are out.
//! Nothing bounds how late a line may come, so exact counts show at the end whether
//! every hour was complete when reported; if one was not, the run is redone without
//! reporting early. README.md explains the design; PERF.md has the measurements.

use std::arch::x86_64::*;
use std::io::{self, Seek, Write};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::{cell::UnsafeCell, fs::File, ops::Range, os::fd::AsRawFd, ptr, slice, thread};

/// Hours in the log: the spec fixes the span at 2026-10-30T18:00Z to 2026-11-02T18:00Z.
const HOURS: usize = 72;
const SPAN_START: i64 = days_from_civil(2026, 10, 30) * 24 + 18;
/// A record has 11 bits for the endpoint id. Ids start at 1: a zero record is a slot
/// that was reserved but not (or not yet) written. The spec promises 1,842 endpoints.
const IDS: usize = 1 << 11;
/// Latencies below this are kept in the record, larger ones in the hour's side list.
const MARK: u32 = 1 << 20;
/// Open-addressing slots of the endpoint table: a power of two, under a quarter full.
const SLOTS: usize = 1 << 13;
/// Records a worker reserves at a time in an hour's log (1 KB, 16 cache lines).
const BLOCK: usize = 256;
/// Bytes of input a worker claims at a time.
const PIECE: usize = 1 << 20;
/// Bytes whose line ends are indexed in one go (a multiple of 64).
const SCAN: usize = 4096;
/// Readable bytes the parser may touch past a line's start (the longest line is 177).
const SLACK: usize = 256;
/// Parsing and reporting threads; the main thread writes. The box has 12 CPUs.
const WORKERS: usize = 11;

// From <sys/mman.h>, <sys/prctl.h> and <malloc.h>.
const PROT_READ: i32 = 1;
const PROT_READ_WRITE: i32 = 1 | 2;
const MAP_PRIVATE: i32 = 0x02;
const MAP_PRIVATE_ANONYMOUS_NORESERVE: i32 = 0x02 | 0x20 | 0x4000;
const MADV_DONTNEED: i32 = 4;
const MADV_POPULATE_READ: i32 = 22;
const PR_SET_THP_DISABLE: i32 = 41;
const M_MMAP_THRESHOLD: i32 = -3;
extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

fn main() {
    // Transparent huge pages (`always` on stock Debian) would back the sparse logs with
    // mostly empty 2 MB pages, all charged to the memory peak. A hint only.
    // SAFETY: a plain prctl that touches no memory of ours.
    unsafe { prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0) };
    // A fixed mmap threshold of 128 KB: glibc would otherwise raise it after the first
    // large free, and freed texts and scratch would stay in its heaps.
    // SAFETY: tunes the allocator before any thread exists; harmless if refused.
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
    // SAFETY: a private read-only mapping of the whole file, never unmapped, of a file
    // nobody writes while we run. It is in the page cache already, so it costs page-table
    // entries only and its pages are not charged to us.
    let data = unsafe { slice::from_raw_parts(map(len, PROT_READ, MAP_PRIVATE, input.as_raw_fd())?, len) };
    // Lines that start in the last SLACK bytes are parsed from a zero-padded copy.
    let body = line_start(data, (len + 1).saturating_sub(SLACK));
    let tail = [&data[body..], &[0; SLACK]].concat();
    let tail = &tail[..len - body];
    if !Job::new(data, tail, true)?.run(&mut out)? {
        // A line came after its hour was reported: start over, reporting at the end.
        (out.set_len(0)?, out.rewind()?);
        Job::new(data, tail, false)?.run(&mut out)?;
    }
    Ok(())
}

/// Maps `len` bytes with protection `prot` and flags `flags`: of file `fd`, or anonymous
/// memory if `fd` is -1.
fn map(len: usize, prot: i32, flags: i32, fd: i32) -> io::Result<*mut u8> {
    // SAFETY: asks for a fresh mapping at an address of the kernel's choosing, so no
    // existing memory is affected; failure is reported as MAP_FAILED (-1).
    let p = unsafe { mmap(ptr::null_mut(), len, prot, flags, fd, 0) };
    match p as isize {
        -1 => Err(io::Error::last_os_error()),
        _ => Ok(p),
    }
}

/// Reserves room for `len` atomics `T`, zeroed; pages cost nothing until first touched.
fn reserve<T>(len: usize) -> io::Result<&'static [T]> {
    let p = map(len * size_of::<T>(), PROT_READ_WRITE, MAP_PRIVATE_ANONYMOUS_NORESERVE, -1)?;
    // SAFETY: fresh memory, never unmapped; zero bytes are a valid atomic integer.
    Ok(unsafe { slice::from_raw_parts(p.cast(), len) })
}

/// Where the first line that starts at or after byte `at` starts.
fn line_start(data: &[u8], at: usize) -> usize {
    let newline = data[at.max(1) - 1..].iter().position(|&b| b == b'\n');
    match at {
        0 => 0,
        _ => newline.map_or(data.len(), |p| at + p),
    }
}

/// A log record as (endpoint id and 5xx bits, latency).
fn record(r: &AtomicU32) -> (u32, u32) {
    let r = r.load(Relaxed);
    (r, r % MARK)
}

/// A side entry as (endpoint id and 5xx bits, latency).
fn side_entry(e: &AtomicU64) -> (u32, u32) {
    let e = e.load(Relaxed);
    ((e >> 32) as u32, e as u32)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A lock is poisoned only by a thread that panics while holding it, and with
    // panic = "abort" a panic ends the process first: this `expect` cannot fire.
    m.lock().expect("no worker panics")
}

/// The counters of one hour. Each hour on its own cache line, so the counters one
/// worker bumps do not evict another hour that other workers are using.
#[derive(Default)]
#[repr(align(64))]
struct Hour {
    /// Log slots reserved (BLOCK at a time, so a worker appends to its own block), and
    /// side entries taken.
    used: AtomicUsize,
    big: AtomicUsize,
    /// The hour as two words of input text, `YYYY-MM-D` and `-MM-DDTHH`.
    stamp: [AtomicU64; 2],
    /// Lines its report counted.
    reported: AtomicUsize,
}

/// Who does what next, under one lock.
#[derive(Default)]
struct Plan {
    next_piece: usize,
    /// Pieces being parsed: the parsed prefix of the file ends at the first of them.
    parsing: Vec<usize>,
    next_hour: usize,
    /// Workers waiting for an hour to come due, so a finished piece wakes them only if any.
    waiting: usize,
    failure: Option<io::Error>,
}

enum Task {
    Piece(usize),
    Hour(usize),
    Stop,
}

/// Per hour, a worker's open block: next slot and end.
type Open = [(usize, usize); HOURS];
/// Some lines to parse: (bytes, start of the first line, end of the last).
type Piece = (*const u8, usize, usize);

/// Everything the workers share.
#[derive(Default)]
struct Job<'a> {
    data: &'a [u8],
    /// The padded copy of the lines that start in the last SLACK bytes: the last piece.
    tail: &'a [u8],
    pieces: usize,
    speculate: bool,
    /// The logs and side lists, `room` entries per hour: records with the endpoint id
    /// in bits 21 and up, the 5xx flag in bit 20 and a latency below MARK; side entries
    /// with the same in the high word and the latency in the low one. They are atomics
    /// (plain moves on x86) because an early report may read a block still being filled:
    /// it sees zeros there, and the counts catch that.
    room: usize,
    logs: &'a [AtomicU32],
    sides: &'a [AtomicU64],
    hours: Vec<Hour>,
    /// Lines parsed, summed over the workers once they are done, and workers done.
    lines: AtomicUsize,
    stopped: AtomicUsize,
    plan: Mutex<Plan>,
    changed: Condvar,
    /// Endpoint ids in output (byte) order, sorted again when new ones have come in.
    order: Mutex<Arc<Vec<u16>>>,
    /// Text buffers the writer is done with (at most four), and report scratch space (at
    /// most two), kept for reuse.
    texts: Mutex<Vec<Vec<u8>>>,
    scratch: Mutex<Vec<Vec<u32>>>,
}

impl<'a> Job<'a> {
    fn new(data: &'a [u8], tail: &'a [u8], speculate: bool) -> io::Result<Job<'a>> {
        // Room for every line of the file in any one hour (a line has at least 40
        // bytes), plus each worker's last, partly used block.
        let room = (data.len() / 40 + 1 + WORKERS * BLOCK).next_multiple_of(1024);
        Ok(Job {
            data,
            tail,
            pieces: (data.len() - tail.len()).div_ceil(PIECE) + 1,
            speculate,
            room,
            logs: reserve(HOURS * room)?,
            sides: reserve(HOURS * room)?,
            hours: (0..HOURS).map(|_| Hour::default()).collect(),
            ..Default::default()
        })
    }

    /// Parses and reports everything, writing the hours to `out` in order. Returns
    /// whether every hour was complete when it was reported.
    fn run(&self, out: &mut File) -> io::Result<bool> {
        let (sender, receiver) = mpsc::channel();
        thread::scope(|s| {
            for _ in 0..WORKERS {
                let sender = sender.clone();
                s.spawn(move || self.work(sender));
            }
            drop(sender);
            // Hours finish out of order; each is written once all earlier ones are out.
            let (mut pending, mut written) = (vec![None; HOURS], 0);
            for (hour, text) in receiver {
                pending[hour] = Some(text);
                while let Some(mut text) = pending.get_mut(written).and_then(Option::take) {
                    out.write_all(&text)?;
                    text.clear();
                    // A few buffers are kept for reuse, which saves faulting in fresh pages;
                    // more, or any near the end, would only add to the memory peak.
                    let mut texts = lock(&self.texts);
                    if texts.len() < 4 && written + WORKERS < HOURS {
                        texts.push(text);
                    }
                    written += 1;
                }
            }
            Ok::<_, io::Error>(())
        })?;
        lock(&self.plan).failure.take().map_or(Ok(()), Err)?;
        // Each line is one record in its hour, so the reports saw every line exactly when
        // they saw as many as were parsed.
        let reported: usize = self.hours.iter().map(|h| h.reported.load(Relaxed)).sum();
        Ok(reported == self.lines.load(Relaxed))
    }

    /// One worker: parses pieces and reports hours, whichever is due, until all is done.
    fn work(&self, sender: mpsc::Sender<(usize, Vec<u8>)>) {
        let (mut open, mut parsed, mut lines) = ([(0, 0); HOURS], None, 0);
        loop {
            match self.next_task(parsed.take()) {
                Task::Piece(i) => {
                    // Fill the piece's page-table entries in one call rather than by a fault
                    // per 64 KB. A hint: if it is refused, the faults do the same.
                    // SAFETY: madvise on our own read-only mapping.
                    let (p, from, to) = self.piece(i);
                    let page = p.wrapping_add(from).map_addr(|a| a & !4095);
                    let bytes = p.wrapping_add(to) as usize - page as usize;
                    unsafe { madvise(page as _, bytes, MADV_POPULATE_READ) };
                    // SAFETY: see `piece`.
                    match unsafe { self.parse(&mut open, (p, from, to)) } {
                        Ok(n) => lines += n,
                        Err(e) => _ = lock(&self.plan).failure.get_or_insert(e),
                    }
                    parsed = Some(i);
                }
                Task::Hour(h) => {
                    let text = lock(&self.texts).pop().unwrap_or_default();
                    let mut sorted = lock(&self.scratch).pop().unwrap_or_default();
                    _ = sender.send((h, self.report(h, &mut sorted, text)));
                    // As for texts: a few are kept, more would only add to the peak.
                    let mut scratch = lock(&self.scratch);
                    if scratch.len() < 2 {
                        scratch.push(sorted);
                    }
                }
                Task::Stop => break,
            }
        }
        self.lines.fetch_add(lines, Relaxed);
        // The last worker out frees what only reports used, before the writer has written
        // the last hours, where the memory peaks: the logs (with their page tables) and the
        // pooled scratch.
        if self.stopped.fetch_add(1, AcqRel) + 1 == WORKERS {
            lock(&self.scratch).clear();
            // SAFETY: every report is done and nothing touches the logs again.
            unsafe { munmap(self.logs.as_ptr() as _, size_of_val(self.logs)) };
            unsafe { munmap(self.sides.as_ptr() as _, size_of_val(self.sides)) };
        }
    }

    /// Marks piece `parsed` done and picks the next task: an hour that is due, else a
    /// piece, else (once all pieces are taken) it waits for more hours to come due.
    fn next_task(&self, parsed: Option<usize>) -> Task {
        let mut plan = lock(&self.plan);
        if let Some(i) = parsed {
            plan.parsing.retain(|&j| j != i);
            if plan.waiting > 0 {
                self.changed.notify_all();
            }
        }
        loop {
            // An hour is due once every piece is parsed, or, speculating, once the parsed
            // prefix reaches a line two hours later: lines arrive at most minutes late
            // (shard 0 uploads each hour 5 to 10 minutes after it ends), though nothing
            // promises it.
            let frontier = plan.parsing.iter().fold(plan.next_piece, |a, &i| a.min(i));
            let due = match frontier == self.pieces {
                true => HOURS,
                false if self.speculate => self.first_row(frontier).saturating_sub(1),
                false => 0,
            };
            if plan.failure.is_some() || plan.next_hour == HOURS {
                return Task::Stop;
            } else if plan.next_hour < due {
                plan.next_hour += 1;
                return Task::Hour(plan.next_hour - 1);
            } else if plan.next_piece < self.pieces {
                let i = plan.next_piece;
                plan.parsing.push(i);
                plan.next_piece += 1;
                return Task::Piece(i);
            }
            plan.waiting += 1;
            plan = self.changed.wait(plan).expect("no worker panics");
            plan.waiting -= 1;
        }
    }

    /// Piece `i` as (bytes, first line start, end). Piece i < pieces - 1 holds the lines
    /// that start in [i * PIECE, (i + 1) * PIECE) of the file; the last is the tail. The
    /// SLACK bytes after any line start in a piece are readable.
    fn piece(&self, i: usize) -> Piece {
        let body = self.data.len() - self.tail.len();
        let at = |i: usize| line_start(self.data, (i * PIECE).min(body));
        match i + 1 < self.pieces {
            true => (self.data.as_ptr(), at(i), at(i + 1)),
            false => (self.tail.as_ptr(), 0, self.tail.len()),
        }
    }

    /// The hour row of piece `i`'s first line (0 if it is outside the span).
    fn first_row(&self, i: usize) -> usize {
        let (p, from, _) = self.piece(i);
        // SAFETY: a line start, so SLACK bytes are readable (zeros if the tail is empty).
        hour_row(unsafe { slice::from_raw_parts(p.add(from), 13) }).unwrap_or(0)
    }

    /// The output lines of one hour, appended to `text`; `sorted` is scratch space.
    fn report(&self, hour: usize, sorted: &mut Vec<u32>, mut text: Vec<u8>) -> Vec<u8> {
        let (h, at) = (&self.hours[hour], hour * self.room);
        let log = &self.logs[at..at + h.used.load(Relaxed)];
        let side = &self.sides[at..at + h.big.load(Relaxed)];
        // Count pass: requests (low half) and 5xx responses (high half) per endpoint. A
        // slot not written (yet) counts as id 0, which is never printed.
        let records = || log.iter().map(record).chain(side.iter().map(side_entry));
        let mut counts = [0u64; IDS];
        records().for_each(|(r, _)| counts[(r >> 21) as usize] += 1 | (r as u64 >> 20 & 1) << 32);
        let seen = counts[1..].iter().map(|&c| c as u32 as usize).sum();
        h.reported.store(seen, Relaxed);
        // Each endpoint gets a slice of `sorted`, in output order, and id 0 the last one.
        let order = self.order(counts.iter().rposition(|&c| c > 0).unwrap_or(0));
        let (mut next, mut total) = ([0u32; IDS], 0);
        for &id in order.iter().chain(&[0]) {
            (next[id as usize], total) = (total, total + counts[id as usize] as u32);
        }
        sorted.resize(sorted.len().max(total as usize), 0);
        // Scatter pass: every latency into its endpoint's slice. A record written after
        // the count pass (the hour was not complete) may find no room; the counts catch it.
        records().for_each(|(r, latency)| {
            let next = &mut next[(r >> 21) as usize];
            if let Some(slot) = sorted.get_mut(*next as usize) {
                *slot = latency;
            }
            *next += 1;
        });
        // The latencies are copied: give the pages back, so the logs shrink as the
        // output grows. A late line written there anyway is caught by the counts.
        // SAFETY: madvise(DONTNEED) on our own mapping; if it is refused, pages stay.
        unsafe { madvise(log.as_ptr() as _, size_of_val(log), MADV_DONTNEED) };
        unsafe { madvise(side.as_ptr() as _, size_of_val(side), MADV_DONTNEED) };
        let mut stamp = [0; 32];
        stamp[..8].copy_from_slice(&h.stamp[0].load(Relaxed).to_le_bytes());
        stamp[5..13].copy_from_slice(&h.stamp[1].load(Relaxed).to_le_bytes());
        stamp[13..21].copy_from_slice(b":00:00Z ");
        let mut start = 0;
        for &id in order.iter() {
            let n = counts[id as usize] as u32 as usize;
            let group = &mut sorted[start..start + n];
            put_line(&mut text, &stamp, id, group, counts[id as usize]);
            start += n;
        }
        text
    }

    /// Endpoint ids in byte order, covering at least ids 1 to `max_id`.
    fn order(&self, max_id: usize) -> Arc<Vec<u16>> {
        let mut order = lock(&self.order);
        if order.len() < max_id {
            // The id was published before its record was written, but maybe not yet seen
            // here (the record was read Relaxed); Acquire on the count makes the name so.
            while ENDPOINTS.len() < max_id {
                std::hint::spin_loop();
            }
            let mut ids: Vec<u16> = (1..=ENDPOINTS.len() as u16).collect();
            ids.sort_unstable_by_key(|&id| ENDPOINTS.get(id).bytes());
            *order = Arc::new(ids);
        }
        order.clone()
    }

    /// Parses the lines in `[from, to)` of the bytes at `p` into the hour logs, and
    /// returns how many there were.
    ///
    /// # Safety
    /// `from` must start a line, `to` must end one, and the SLACK bytes after every line
    /// start in between must be readable.
    unsafe fn parse(&self, open: &mut Open, (p, from, to): Piece) -> io::Result<usize> {
        let word = |at: usize| p.add(at).cast::<u64>().read_unaligned();
        // The current hour, as a cache: shards write in time order, so it rarely changes.
        // `key` holds its `YYYY-MM-DDTHH`, compared as the first 13 of 16 loaded bytes.
        let (mut key, mut row, (mut n, mut end), mut lines) = (_mm_setzero_si128(), 0, open[0], 0);
        let (mut hour, mut log) = (&self.hours[0], self.logs.as_ptr());
        let mut ends = [0usize; SCAN / 32];
        let (mut line, mut block) = (from, from);
        while line < to {
            // Index the line ends of the next SCAN bytes. A 64-byte block holds at most two
            // (a line has at least 40 bytes), so both candidates (the lowest set bit, and
            // the lowest once that is cleared) are stored and the count advances by the
            // popcount, without a branch per block. SCAN bytes hold at most
            // 103 lines, so the cap never bites; it only lets the compiler drop the checks.
            let (stop, mut k) = ((block + SCAN).min(to), 0);
            while block < stop {
                let bits = newlines(p.add(block));
                ends[k] = block + bits.trailing_zeros() as usize;
                ends[k + 1] = block + (bits & bits.wrapping_sub(1)).trailing_zeros() as usize;
                k = (k + bits.count_ones() as usize).min(SCAN / 32 - 2);
                block += 64;
            }
            // The last block may run past `to`, into the next piece.
            let k = ends[..k].partition_point(|&e| e < to);
            lines += k;
            for &e in &ends[..k] {
                let start = line;
                line = e + 1;
                let stamp = _mm_loadu_si128(p.add(start).cast());
                if _mm_movemask_epi8(_mm_cmpeq_epi8(stamp, key)) & 0x1FFF != 0x1FFF {
                    open[row] = (n, end);
                    key = stamp;
                    row = hour_row(slice::from_raw_parts(p.add(start), 13))?;
                    (hour, (n, end)) = (&self.hours[row], open[row]);
                    log = self.logs[row * self.room..].as_ptr();
                    // Stored once, so the shared line is not rewritten at every hour change.
                    if hour.stamp[1].load(Relaxed) == 0 {
                        hour.stamp[0].store(word(start), Relaxed);
                        hour.stamp[1].store(word(start + 5), Relaxed);
                    }
                }
                let (latency, space) = parse_latency(p.add(e - 16));
                let status = e - 16 + space - 3;
                let error = (*p.add(status) == b'5') as u32;
                // The endpoint starts after the space that ends the method (which follows
                // the 27-byte timestamp and its space), and ends before the status.
                let name = start + 29 + spaces(p.add(start + 28)).trailing_zeros() as usize;
                let record = ENDPOINTS.id(p.add(name), status - 1 - name)? << 21 | error << 20;
                // SAFETY (both stores): `used` and `big` never pass the hour's room (see
                // `Job::new`), and hand each slot to one worker.
                if latency < MARK {
                    if n == end {
                        n = hour.used.fetch_add(BLOCK, Relaxed);
                        end = n + BLOCK;
                    }
                    (*log.add(n)).store(record | latency, Relaxed);
                    n += 1;
                } else {
                    let entry = (record as u64) << 32 | latency as u64;
                    let side = row * self.room + hour.big.fetch_add(1, Relaxed);
                    (*self.sides.as_ptr().add(side)).store(entry, Relaxed);
                }
            }
        }
        open[row] = (n, end);
        Ok(lines)
    }
}

/// Appends one group's output line to `text`: `stamp` (21 bytes, padded to 32), the
/// endpoint `id`'s name, and the numbers from its latencies `v` and `counts` (requests
/// in the low half, 5xx in the high half). Nothing for an empty group.
fn put_line(text: &mut Vec<u8>, stamp: &[u8; 32], id: u16, v: &mut [u32], counts: u64) {
    let (n, errors, name) = (v.len() as u32, (counts >> 32) as u32, ENDPOINTS.get(id));
    if n == 0 {
        return;
    }
    let [p50, p95, p99] = percentiles(v);
    let ppm = (errors as u64 * 1_000_000 / n as u64) as u32;
    text.reserve(256);
    // SAFETY: `reserve` gave 256 writable bytes past the end; the line uses at most
    // 21 + 127 (name copy) or 21 + 125 + 6 * 11 + 8 (last digit word) of them.
    unsafe {
        let start = text.as_mut_ptr().add(text.len());
        ptr::copy_nonoverlapping(stamp.as_ptr(), start, 32);
        ptr::copy_nonoverlapping(name.bytes.as_ptr(), start.add(21), 127);
        let mut p = start.add(21 + name.len as usize);
        for v in [n, p50, p95, p99, errors, ppm] {
            *p = b' ';
            p = put_number(p.add(1), v);
        }
        *p = b'\n';
        text.set_len(text.len() + p.offset_from(start) as usize + 1);
    }
}

/// Nearest-rank p50, p95 and p99 of a non-empty group, reordering it.
fn percentiles(v: &mut [u32]) -> [u32; 3] {
    let rank = |p: usize| (p * v.len() + 99) / 100 - 1;
    let [r50, r95, r99] = [rank(50), rank(95), rank(99)];
    if v.len() <= 32 {
        v.sort_unstable();
        return [v[r50], v[r95], v[r99]];
    }
    // Low to high, each selection searching only above the previous one: about 1.55n
    // steps. Beyond 32 values the three ranks are distinct.
    let (_, &mut p50, above) = v.select_nth_unstable(r50);
    let (_, &mut p95, above) = above.select_nth_unstable(r95 - r50 - 1);
    let (_, &mut p99, _) = above.select_nth_unstable(r99 - r95 - 1);
    [p50, p95, p99]
}

/// Writes `v` in decimal at `p` and returns the end, with no loop: the digits are made
/// eight at a time and the leading zeros shifted out.
///
/// # Safety
/// `p` must have 18 writable bytes.
unsafe fn put_number(p: *mut u8, v: u32) -> *mut u8 {
    const ZEROS: u64 = 0x3030_3030_3030_3030;
    if v >= 100_000_000 {
        let p = put_number(p, v / 100_000_000);
        let low = eight_digits(v % 100_000_000) | ZEROS;
        p.cast::<u64>().write_unaligned(low);
        return p.add(8);
    }
    // Leading zero digits are the low zero bytes of the (memory-order) digit word.
    let digits = eight_digits(v);
    let len = 8 - (digits.trailing_zeros() / 8).min(7) as usize;
    let digits = (digits | ZEROS) >> (64 - 8 * len);
    p.cast::<u64>().write_unaligned(digits);
    p.add(len)
}

/// The eight decimal digits of `v` < 10^8, zero-padded, as bytes 0 to 9 in memory order.
fn eight_digits(v: u32) -> u64 {
    // SWAR: split into 4-digit halves in two 32-bit lanes, each into 2-digit quarters in
    // 16-bit lanes, each into digits in bytes. Division by 100 and by 10 are multiplies
    // and shifts that are exact for these ranges, and no lane overflows into the next.
    let x = ((v % 10_000) as u64) << 32 | (v / 10_000) as u64;
    let hundreds = (x * 10_486 >> 20) & 0x7F_0000_007F;
    let x = (x - hundreds * 100) << 16 | hundreds;
    let tens = (x * 103 >> 10) & 0x000F_000F_000F_000F;
    (x - tens * 10) << 8 | tens
}

/// The newlines of the 64 bytes at `p`, as a bitmask.
///
/// # Safety
/// The 64 bytes must be readable. AVX2 is enabled crate-wide (target-cpu=x86-64-v3).
#[inline(always)]
unsafe fn newlines(p: *const u8) -> u64 {
    let eq = |at| _mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(at).cast()), _mm256_set1_epi8(10));
    (_mm256_movemask_epi8(eq(32)) as u32 as u64) << 32 | _mm256_movemask_epi8(eq(0)) as u32 as u64
}

/// The spaces among the 16 bytes at `p`, as a bitmask.
///
/// # Safety
/// The 16 bytes must be readable. SSE2 is part of x86-64.
#[inline(always)]
unsafe fn spaces(p: *const u8) -> u32 {
    let eq = _mm_cmpeq_epi8(_mm_loadu_si128(p.cast()), _mm_set1_epi8(b' ' as i8));
    _mm_movemask_epi8(eq) as u32
}

/// The latency that ends the 16 bytes at `p` (the bytes before a newline), and the index
/// of the space in front of it. With no loop: the latency's at most 10 digits are masked
/// in and summed as a 16-digit number by three multiply-add steps.
///
/// # Safety
/// The 16 bytes must be readable. SSE4.1 is enabled crate-wide (target-cpu=x86-64-v3).
#[inline(always)]
unsafe fn parse_latency(p: *const u8) -> (u32, usize) {
    let space = 31 - spaces(p).leading_zeros();
    let index = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let after = _mm_cmpgt_epi8(index, _mm_set1_epi8(space as i8));
    let digits = _mm_sub_epi8(_mm_loadu_si128(p.cast()), _mm_set1_epi8(b'0' as i8));
    let digits = _mm_and_si128(digits, after);
    let pairs = _mm_maddubs_epi16(digits, _mm_set1_epi16(0x010A)); // 10a + b
    let quads = _mm_madd_epi16(pairs, _mm_set1_epi32(0x0001_0064)); // 100ab + cd
    let eights = _mm_madd_epi16(_mm_packus_epi32(quads, quads), _mm_set1_epi32(0x0001_2710));
    let high = (_mm_cvtsi128_si32(eights) as u32).wrapping_mul(100_000_000);
    let latency = high.wrapping_add(_mm_extract_epi32::<1>(eights) as u32);
    (latency, space as usize)
}

/// The row (0 to 71) of a `YYYY-MM-DDTHH` hour, or an error outside the spec's span.
fn hour_row(t: &[u8]) -> io::Result<usize> {
    let num = |r: Range<usize>| t[r].iter().fold(0, |a, &d| a * 10 + d as i64 - 48);
    let hours = days_from_civil(num(0..4), num(5..7), num(8..10)) * 24 + num(11..13);
    match usize::try_from(hours - SPAN_START) {
        Ok(row) if row < HOURS => Ok(row),
        _ => Err(io::Error::other("a timestamp outside the 72-hour span")),
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let (era, doy) = (y.div_euclid(400), (153 * ((m + 9) % 12) + 2) / 5 + d - 1);
    let yoe = y - era * 400;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// One endpoint name and the space after it, zero-padded so it can be compared and
/// copied in fixed-size blocks, with its length in the last byte (the spec's longest name
/// has 125 bytes).
#[repr(C, align(64))]
struct Name {
    bytes: [u8; 127],
    len: u8,
}

impl Name {
    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// Whether the `len` bytes at `p` are this name. Compares 64 bytes at a time with the
    /// bits past `len + 1` masked off; only names of 64 bytes and more need a second step.
    /// Byte `len` settles the length too: a stored name is followed by a space, like the
    /// name in a line (before the status), and names contain no spaces.
    ///
    /// # Safety
    /// 128 bytes at `p` must be readable. AVX2 and BMI2 are enabled crate-wide.
    #[inline(always)]
    unsafe fn is(&self, p: *const u8, len: usize) -> bool {
        // `b` is taken from the whole struct: the last block also covers `len`, masked off.
        let eq = |at: usize| {
            let (a, b) = (p.add(at).cast(), (self as *const Name).cast::<u8>().add(at).cast());
            let eq = _mm256_cmpeq_epi8(_mm256_loadu_si256(a), _mm256_loadu_si256(b));
            _mm256_movemask_epi8(eq) as u32 as u64
        };
        let all = |at| (eq(at) | eq(at + 32) << 32) | !_bzhi_u64(!0, (len + 1 - at) as u32) == !0;
        all(0) && (len < 64 || all(64))
    }
}

/// The endpoint table, shared by all workers: open addressing over slots that hold an
/// id (0 = empty), each name stored once by id. Lookups take no lock; an insert takes
/// one, rechecks, and stores the name before publishing its id.
struct Endpoints {
    slots: [AtomicU16; SLOTS],
    names: [UnsafeCell<Name>; IDS],
    /// Ids handed out so far; under the lock that serialises inserts.
    count: Mutex<usize>,
    published: AtomicUsize,
}

// SAFETY: a name is written once, under `count`'s lock, before its id is published with
// a Release store (to its slot and to `published`); readers reach a name only through an
// id they loaded with Acquire, and nobody writes it again.
unsafe impl Sync for Endpoints {}

/// SAFETY: all zero bytes are a valid, empty `Name`.
const EMPTY: Name = unsafe { std::mem::zeroed() };
static ENDPOINTS: Endpoints = Endpoints {
    slots: [const { AtomicU16::new(0) }; SLOTS],
    names: [const { UnsafeCell::new(EMPTY) }; IDS],
    count: Mutex::new(0),
    published: AtomicUsize::new(0),
};

impl Endpoints {
    fn len(&self) -> usize {
        self.published.load(Acquire)
    }

    fn get(&self, id: u16) -> &Name {
        // SAFETY: only published ids are passed in (see the Sync note).
        unsafe { &*self.names[id as usize % IDS].get() }
    }

    /// The id of the `len`-byte endpoint at `p`, adding it if new.
    ///
    /// # Safety
    /// 128 bytes at `p` and the 8 bytes before `p + len` must be readable.
    #[inline(always)]
    unsafe fn id(&self, p: *const u8, len: usize) -> io::Result<u32> {
        // Hash the first and last 8 bytes and the length. For a name under 8 bytes both
        // words are cut to the name, as their other bytes belong to the line around it.
        let cut = 64 - 8 * len.min(8) as u32;
        let head = p.cast::<u64>().read_unaligned() << cut;
        let tail = p.add(len).sub(8).cast::<u64>().read_unaligned() >> cut;
        let hash = (head ^ tail.rotate_left(31) ^ len as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut i = (hash >> 51) as usize;
        loop {
            match self.slots[i % SLOTS].load(Acquire) {
                0 => return self.insert(p, len, i),
                id if self.get(id).is(p, len) => return Ok(id as u32),
                _ => i += 1,
            }
        }
    }

    #[cold]
    unsafe fn insert(&self, p: *const u8, len: usize, mut i: usize) -> io::Result<u32> {
        let mut count = lock(&self.count);
        // Another worker may have added it since our probe found the slot empty.
        while let id @ 1.. = self.slots[i % SLOTS].load(Relaxed) {
            if self.get(id).is(p, len) {
                return Ok(id as u32);
            }
            i += 1;
        }
        // Room for the name and the space after it, before the length byte.
        if *count == IDS - 1 || len > 126 {
            return Err(io::Error::other("over 2,047 endpoints or 126 bytes"));
        }
        *count += 1;
        let name = &mut *self.names[*count].get();
        name.bytes[..=len].copy_from_slice(slice::from_raw_parts(p, len + 1));
        name.len = len as u8;
        self.slots[i % SLOTS].store(*count as u16, Release);
        self.published.store(*count, Release);
        Ok(*count as u32)
    }
}
