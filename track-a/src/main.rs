//! Track A (Ingest): for every (UTC hour, endpoint) group of an HTTP request log, the
//! request count, nearest-rank p50/p95/p99 latency and the 5xx count and rate.
//!
//! Workers read the log in pieces, in file order, and parse each into one 4-byte record
//! per line appended to that hour's log. An hour is reported as soon as the parsed
//! prefix of the file has moved two hours past it, while parsing goes on: its log is
//! counting-sorted by endpoint, so every group is one slice, three selections give the
//! percentiles, the log is freed, and the main thread writes the text once all earlier
//! hours are out. Nothing bounds how late a line may come, so exact counts show at the
//! end whether every hour was complete when reported; if one was not, the run is redone
//! without reporting early. README.md explains the design; PERF.md has the measurements.

use std::arch::x86_64::*;
use std::io::{self, Seek, Write};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::{cell::UnsafeCell, fs::File, ops::Range, os::unix::fs::FileExt, ptr, slice, thread};

/// Hours in the log: the spec fixes the span at 2026-10-30T18:00Z to 2026-11-02T18:00Z.
/// The span's start is also written into `hour_row` and `stamp`: change all three together.
const HOURS: usize = 72;
/// A record has 11 bits for the endpoint id. Ids start at 1: a zero record is a slot
/// that was reserved but not (or not yet) written. The spec promises 1,842 endpoints.
const IDS: usize = 1 << 11;
/// Latencies below this are kept in the record, larger ones in the hour's side list.
const INLINE_LATENCY_LIMIT: u32 = 1 << ERROR_SHIFT;
/// A record is `id << ID_SHIFT | 5xx << ERROR_SHIFT | latency`: the latency (below
/// INLINE_LATENCY_LIMIT) fills bits 0 to 19, the 5xx flag bit 20 and the id (below IDS)
/// bits 21 to 31.
const ERROR_SHIFT: u32 = 20;
const ID_SHIFT: u32 = 21;
const _: () = assert!(ID_SHIFT == ERROR_SHIFT + 1 && (IDS as u64) << ID_SHIFT == 1 << 32);
/// Open-addressing slots of the endpoint table: a power of two, under a quarter full.
const SLOTS: usize = 1 << 13;
/// Records a worker reserves at a time in an hour's log: 4 KB, one page per worker.
const BLOCK: usize = 1024;
/// Bytes of input a worker claims and reads at a time: a buffer that, with its
/// hyperthread sibling's, stays in a P-core's 1.25 MB L2.
const PIECE: usize = 128 << 10;
/// The longest line the grammar allows: timestamp, 7-byte method, 125-byte endpoint,
/// status and 10-digit latency, with their separators and the newline.
const MAX_LINE: usize = 27 + 1 + 7 + 1 + 125 + 1 + 3 + 1 + 10 + 1;
/// Bytes whose line ends are indexed in one go (a multiple of 64).
const SCAN: usize = 4096;
/// Readable bytes the parser may touch past a line's start (more than MAX_LINE).
const SLACK: usize = 256;
/// Parsing and reporting threads; the main thread writes. The box has 12 CPUs.
const WORKERS: usize = 11;

// From <sys/mman.h>, <sys/prctl.h> and <malloc.h>.
const PROT_READ_WRITE: i32 = 1 | 2;
const MAP_PRIVATE_ANONYMOUS_NORESERVE: i32 = 0x02 | 0x20 | 0x4000;
const MADV_DONTNEED: i32 = 4;
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
    // `ingest [data_dir] [out_dir]`, defaulting to the harness's /data and /out.
    let mut args = std::env::args().skip(1);
    let input = format!("{}/requests.log", args.next().as_deref().unwrap_or("/data"));
    let out = format!("{}/summary.txt", args.next().as_deref().unwrap_or("/out"));
    if let Err(e) = run(&input, &out) {
        eprintln!("ingest: {e}");
        // No summary is left behind on failure: a half-written one must not pass for a
        // whole one.
        _ = std::fs::remove_file(&out);
        std::process::exit(1);
    }
}

fn run(input: &str, out: &str) -> io::Result<()> {
    let input = File::open(input).map_err(|e| context(e, input))?;
    let mut out = File::create(out).map_err(|e| context(e, out))?;
    if !Job::new(&input, true)?.run(&mut out)? {
        eprintln!("ingest: a line came after its hour was reported; redoing the run");
        out.set_len(0)?;
        out.rewind()?;
        Job::new(&input, false)?.run(&mut out)?;
    }
    Ok(())
}

/// `e` with `what` (a path, say) in front of its message, keeping its kind.
fn context(e: io::Error, what: impl std::fmt::Display) -> io::Error {
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// Reserves room for `len` atomics `T`, zeroed; pages cost nothing until first touched.
///
/// The slice is `'static` only in name. Its one holder is a `Job` (`logs`, `sides`),
/// and the last of that job's workers unmaps it at the end of `work`, before `run`
/// returns; after that only the hours' counters are read, never the slice. A `Job`
/// that never runs leaves it mapped until the process exits.
fn reserve<T>(len: usize) -> io::Result<&'static [T]> {
    let (bytes, flags) = (len * size_of::<T>(), MAP_PRIVATE_ANONYMOUS_NORESERVE);
    // SAFETY: asks for a fresh mapping at an address of the kernel's choosing, so no
    // existing memory is affected; failure is reported as MAP_FAILED (-1).
    let p = unsafe { mmap(ptr::null_mut(), bytes, PROT_READ_WRITE, flags, -1, 0) };
    if p as isize == -1 {
        let e = io::Error::last_os_error();
        return Err(context(e, format!("mmap of {bytes} bytes")));
    }
    // SAFETY: fresh memory, zeroed, and zero bytes are a valid atomic integer. It stays
    // mapped until the last worker unmaps it (see `work`), after which nothing uses it.
    Ok(unsafe { slice::from_raw_parts(p.cast(), len) })
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
    /// Log slots reserved, BLOCK at a time so a worker appends to its own block (a slot
    /// reserved is not yet written: it reads as zero until then), and side entries taken.
    used: AtomicUsize,
    big: AtomicUsize,
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
type Lines = (*const u8, usize, usize);

/// Everything the workers share.
#[derive(Default)]
struct Job<'a> {
    /// The log and its length (an `Option` only so that `Job` can derive `Default`).
    input: Option<&'a File>,
    len: usize,
    /// Per piece, the hour row of its first line plus one, once a worker has read it.
    rows: Vec<AtomicUsize>,
    speculate: bool,
    /// The logs and side lists, `room` entries per hour: records with the endpoint id
    /// from bit ID_SHIFT up, the 5xx flag in bit ERROR_SHIFT and a latency below INLINE_LATENCY_LIMIT; side entries
    /// with the same in the high word and the latency in the low one. They are atomics
    /// (plain moves on x86) because an early report may read a block still being filled:
    /// it sees zeros there, and the counts catch that.
    room: usize,
    logs: &'a [AtomicU32],
    sides: &'a [AtomicU64],
    hours: Vec<Hour>,
    /// Lines parsed, summed over the workers once they are done; lines the reports
    /// counted; and workers done.
    lines: AtomicUsize,
    reported: AtomicUsize,
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
    fn new(input: &'a File, speculate: bool) -> io::Result<Job<'a>> {
        let (len, input) = (input.metadata()?.len() as usize, Some(input));
        let pieces = len.div_ceil(PIECE);
        // Room for every line of the file in any one hour (a line has at least 40
        // bytes), plus each worker's last, partly used block. A whole number of blocks
        // keeps every hour's log (and side list) page-aligned, as madvise(DONTNEED) in
        // `report` requires: an unaligned start fails with EINVAL and keeps the pages.
        let room = (len / 40 + 1 + WORKERS * BLOCK).next_multiple_of(BLOCK);
        Ok(Job {
            input,
            len,
            rows: (0..pieces).map(|_| AtomicUsize::new(0)).collect(),
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
        Ok(self.reported.load(Relaxed) == self.lines.load(Relaxed))
    }

    /// One worker: parses pieces and reports hours, whichever is due, until all is done.
    fn work(&self, sender: mpsc::Sender<(usize, Vec<u8>)>) {
        let (mut open, mut parsed, mut lines, mut buf) = ([(0, 0); HOURS], None, 0, vec![]);
        loop {
            match self.next_task(parsed.take()) {
                Task::Piece(i) => {
                    // SAFETY: `read` leaves SLACK zero bytes after the piece's lines.
                    let piece = self.read(i, &mut buf);
                    match piece.and_then(|piece| unsafe { self.parse(&mut open, piece) }) {
                        Ok(n) => lines += n,
                        Err(e) => _ = lock(&self.plan).failure.get_or_insert(e),
                    }
                    parsed = Some(i);
                    // Among the last pieces, this was most likely this worker's last one:
                    // free its buffer now, not at the memory peak at the end.
                    if i + WORKERS >= self.rows.len() {
                        buf = Vec::new();
                    }
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
            // SAFETY: every worker is done, so no report reads the logs any more, and nothing
            // else ever does: `run` only reads the hours' counters, and a rerun has its own.
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
            let due = match self.rows.get(frontier) {
                None => HOURS,
                Some(row) if self.speculate => row.load(Relaxed).saturating_sub(2),
                Some(_) => 0,
            };
            if plan.failure.is_some() || plan.next_hour == HOURS {
                return Task::Stop;
            } else if plan.next_hour < due {
                plan.next_hour += 1;
                return Task::Hour(plan.next_hour - 1);
            } else if plan.next_piece < self.rows.len() {
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

    /// Reads piece `i`, the lines that start in [i * PIECE, (i + 1) * PIECE), into `buf`:
    /// from the byte before it to the end of its last line, then SLACK zero bytes. Also
    /// notes the hour of its first line for the plan (one outside the span stays unknown).
    fn read(&self, i: usize, buf: &mut Vec<u8>) -> io::Result<Lines> {
        let at = (i * PIECE).saturating_sub(1);
        let end = self.len.min((i + 1) * PIECE + MAX_LINE);
        let input = self.input.expect("set by `new`");
        buf.resize(end - at + SLACK, 0);
        input.read_exact_at(&mut buf[..end - at], at as u64)?;
        buf[end - at..].fill(0);
        // A line starts after a newline, so the byte before the piece decides where its
        // first line starts, and the first newline from its last byte on where it stops.
        let after_newline = |from: usize| {
            let newline = buf[from..end - at].iter().position(|&b| b == b'\n');
            newline.map_or(end - at, |p| from + p + 1)
        };
        let from = if i == 0 { 0 } else { after_newline(0) };
        let to = if (i + 1) * PIECE < self.len {
            after_newline((i + 1) * PIECE - 1 - at)
        } else {
            end - at
        };
        let row = hour_row(&buf[from..from + 13]).map_or(0, |row| row + 1);
        self.rows[i].store(row, Relaxed);
        Ok((buf.as_ptr(), from, to))
    }

    /// The output lines of one hour, appended to `text`; `sorted` is scratch space. In
    /// five stages:
    /// 1. count pass: requests and 5xx per endpoint, and the lines seen, for the check;
    /// 2. a slice of `sorted` per endpoint, in output order (a prefix sum);
    /// 3. scatter pass: every latency into its endpoint's slice;
    /// 4. the hour's log pages given back;
    /// 5. per endpoint, three selections for the percentiles and its output line.
    fn report(&self, hour: usize, sorted: &mut Vec<u32>, mut text: Vec<u8>) -> Vec<u8> {
        let (h, at) = (&self.hours[hour], hour * self.room);
        let log = &self.logs[at..at + h.used.load(Relaxed)];
        let side = &self.sides[at..at + h.big.load(Relaxed)];
        // Count pass, by endpoint id and 5xx flag together (the record's top 12 bits), so
        // `tally[2 * id]` holds the endpoint's non-5xx responses and `tally[2 * id + 1]` its
        // 5xx ones; then per endpoint, requests in the low half and 5xx responses in the
        // high half. A slot not written (yet) counts as id 0, which is never printed. Both kinds are read as
        // (endpoint id and 5xx bits, latency).
        let records = || {
            let small = log
                .iter()
                .map(|r| r.load(Relaxed))
                .map(|r| (r, r % INLINE_LATENCY_LIMIT));
            small.chain(
                side.iter()
                    .map(|e| e.load(Relaxed))
                    .map(|e| ((e >> 32) as u32, e as u32)),
            )
        };
        let mut tally = [0u32; 2 * IDS];
        records().for_each(|(r, _)| tally[(r >> ERROR_SHIFT) as usize] += 1);
        let counts: [u64; IDS] = std::array::from_fn(|id| {
            let (ok, errors) = (tally[2 * id] as u64, tally[2 * id + 1] as u64);
            (ok + errors) | (errors << 32)
        });
        let seen = counts[1..].iter().map(|&c| c as u32 as usize).sum();
        self.reported.fetch_add(seen, Relaxed);
        // Each endpoint gets a slice of `sorted`, in output order, and id 0 the last one.
        let order = self.order(counts.iter().rposition(|&c| c > 0).unwrap_or(0));
        let (mut next, mut total) = ([0u32; IDS], 0);
        for &id in order.iter().chain(&[0]) {
            (next[id as usize], total) = (total, total + counts[id as usize] as u32);
        }
        sorted.resize(sorted.len().max(total as usize), 0);
        // Scatter pass: every latency into its endpoint's slice. A record written after
        // the count pass (the hour was not complete) was not counted, so its endpoint's
        // slice is one short: the latency lands in the next endpoint's slice, overwriting
        // one of its values, or in space that is never printed. Either way this hour's
        // text is wrong, and that is harmless only because the record's line is missing
        // from `reported`, so the count check in `run` throws the text away and redoes
        // the run.
        records().for_each(|(r, latency)| {
            let next = &mut next[(r >> ID_SHIFT) as usize];
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
        let stamp = stamp(hour);
        // Room for every line at once (untouched pages cost nothing), so the text never
        // grows by copying.
        text.reserve(256 * order.len());
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
            while ENDPOINTS.published.load(Acquire) < max_id {
                std::hint::spin_loop();
            }
            let mut ids: Vec<u16> = (1..=ENDPOINTS.published.load(Acquire) as u16).collect();
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
    unsafe fn parse(&self, open: &mut Open, (p, from, to): Lines) -> io::Result<usize> {
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
                }
                let (latency, space) = parse_latency(p.add(e - 16));
                let status = e - 16 + space - 3;
                let error = (*p.add(status) == b'5') as u32;
                // The endpoint starts after the space that ends the method (which follows
                // the 27-byte timestamp and its space), and ends before the status.
                let name = start + 29 + spaces(p.add(start + 28)).trailing_zeros() as usize;
                let id = ENDPOINTS.id(p.add(name), status - 1 - name)?;
                let record = (id << ID_SHIFT) | (error << ERROR_SHIFT);
                // SAFETY (both stores): `used` and `big` never pass the hour's room (see
                // `Job::new`), and hand each slot to one worker.
                // The count check rests on this: a slot goes from zero to its final value in
                // one atomic store, never through a partial value, so a report sees each line
                // either whole or not at all (as id 0) and never counts one twice.
                if latency < INLINE_LATENCY_LIMIT {
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
    // 21 + 128 (name copy) or 21 + 125 + 6 * 11 + 8 (last digit word) of them.
    unsafe {
        let start = text.as_mut_ptr().add(text.len());
        ptr::copy_nonoverlapping(stamp.as_ptr(), start, 32);
        // All 128 bytes of the record (a whole number of vector moves, no `memcpy` call);
        // what follows the name is overwritten below.
        ptr::copy_nonoverlapping((name as *const Name).cast::<u8>(), start.add(21), 128);
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
    // Nearest rank: the 1-indexed rank ceil(p * n / 100), the spec's (p * n + 99) div 100.
    let rank = |p: usize| (p * v.len()).div_ceil(100) - 1;
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
    let x = (((v % 10_000) as u64) << 32) | (v / 10_000) as u64;
    let hundreds = ((x * 10_486) >> 20) & 0x7F_0000_007F;
    let x = ((x - hundreds * 100) << 16) | hundreds;
    let tens = ((x * 103) >> 10) & 0x000F_000F_000F_000F;
    ((x - tens * 10) << 8) | tens
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

/// The row (0 to 71) of a `YYYY-MM-DDTHH` hour, or an error outside the spec's span. The
/// span runs from 2026-10-30T18 over the end of October (31 days) into November.
fn hour_row(t: &[u8]) -> io::Result<usize> {
    let num = |r: Range<usize>| t[r].iter().fold(0, |a, &d| a * 10 + d as i64 - 48);
    let day = num(8..10) + if num(5..7) == 11 { 31 } else { 0 };
    let row = usize::try_from((day - 30) * 24 + num(11..13) - 18);
    match (num(0..4), num(5..7), row) {
        (2026, 10 | 11, Ok(row)) if row < HOURS => Ok(row),
        _ => Err(context(
            io::Error::other("hour outside the span"),
            t.escape_ascii(),
        )),
    }
}

/// The output's `YYYY-MM-DDTHH:00:00Z ` for hour row `row`, padded to 32 bytes.
fn stamp(row: usize) -> [u8; 32] {
    let (day, hour) = (30 + (row + 18) / 24, (row + 18) % 24);
    let (month, day) = if day > 31 { (11, day - 31) } else { (10, day) };
    let mut stamp = [0; 32];
    stamp[..21].copy_from_slice(format!("2026-{month:02}-{day:02}T{hour:02}:00:00Z ").as_bytes());
    stamp
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
            let a = _mm256_loadu_si256(p.add(at).cast());
            let b = _mm256_loadu_si256((self as *const Name).cast::<u8>().add(at).cast());
            let eq = _mm256_cmpeq_epi8(a, b);
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
        // The top bits of the product index the slots.
        let mut i = (hash >> (64 - SLOTS.trailing_zeros())) as usize;
        loop {
            match self.slots[i % SLOTS].load(Acquire) {
                0 => return self.insert(p, len, i),
                id if self.get(id).is(p, len) => return Ok(id as u32),
                _ => i += 1,
            }
        }
    }

    /// Adds the `len`-byte endpoint at `p`, which slot `i` of its probe found empty,
    /// unless another worker added it meanwhile; returns its id either way.
    ///
    /// # Safety
    /// As for `id`: 128 bytes at `p` must be readable.
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
        if *count == IDS - 1 {
            return Err(io::Error::other(format!("over {} endpoints", IDS - 1)));
        }
        if len > 126 {
            return Err(io::Error::other(format!(
                "a {len}-byte endpoint (over 126)"
            )));
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
