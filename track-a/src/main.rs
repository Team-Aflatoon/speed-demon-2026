//! Track A (Ingest): for every (UTC hour, endpoint) group of an HTTP request log, the
//! request count, nearest-rank p50/p95/p99 latency and the 5xx count and rate.
//!
//! Twelve threads read the log in pieces, in file order, and parse each into one 4-byte record
//! per line appended to that hour's log. Speculating that lines come at most a little
//! late, an hour is closed once the parsed prefix of the file is two hours past it, and
//! reported as soon as every piece handed out before that is parsed, while parsing goes
//! on. So a report never races a writer: the hour's log is final. It is counting-sorted
//! by endpoint, so every group is one slice, three ranks give the percentiles, the log is
//! freed, and the text is written once all earlier hours are out, by whichever thread
//! completes that order. A line of a closed hour proves the speculation wrong: the run is
//! then redone with every hour reported at the end. README.md explains the design;
//! PERF.md has the measurements.

use std::arch::x86_64::*;
use std::io::{Seek, Write};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicUsize, Ordering::*};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::{cell::UnsafeCell, fmt::Display, fs::File, os::unix::fs::FileExt, ptr, slice, thread};

/// Hours in the log: the spec fixes the span at 2026-10-30T18:00Z to 2026-11-02T18:00Z.
/// The span's start is also written into `hour_row` and `stamp`: change all three together.
const HOURS: usize = 72;
/// A record is `id << ID_SHIFT | 5xx << ERROR_SHIFT | latency % LOW`: the endpoint id
/// (from 1; a zero is a slot reserved but not written) in bits 21 to 31, so below IDS, the
/// 5xx flag in bit 20 and the latency's low 20 bits. A wide latency, of LOW or more (about
/// 1.5% of them), puts its high bits in a slot before the record: one with id 0, like an
/// empty slot.
const ID_SHIFT: u32 = 21;
const ERROR_SHIFT: u32 = 20;
const LOW: u32 = 1 << ERROR_SHIFT;
const IDS: usize = 1 << (32 - ID_SHIFT);
/// Open-addressing slots of the endpoint table: a power of two, under a quarter full.
const SLOTS: usize = 1 << 13;
/// Records a worker reserves at a time in an hour's log: 4 KB, one page per worker.
const BLOCK: usize = 1024;
/// Speculating, the room for an hour's records: the generator's busiest hour has about
/// 74,000 lines (its traffic follows the time of day). A fuller hour fails the
/// speculation like a late line does, and the rerun gives every hour room for the whole
/// file. With hours this close together, the logs need few page-table pages.
const CROWDED: usize = 1 << 17;
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
/// Threads, the main one included, each parsing, reporting and writing: the box has 12
/// CPUs.
const THREADS: usize = 12;

// From <sys/mman.h>, <sys/prctl.h> and <malloc.h>; their constants are spelled out
// where they are used.
extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

fn main() {
    // SAFETY: process-wide hints that touch no memory of ours; refusal is harmless.
    // PR_SET_THP_DISABLE (41): transparent huge pages (`always` on stock Debian) would back
    // the sparse logs with mostly empty 2 MB pages, all charged to the memory peak.
    // M_MMAP_THRESHOLD (-3) fixed at 128 KB: glibc would otherwise raise it after the
    // first large free, and freed texts and scratch would stay in its heaps.
    unsafe {
        prctl(41, 1, 0, 0, 0);
        mallopt(-3, 128 << 10);
    }
    // `ingest [data_dir] [out_dir]`, defaulting to the harness's /data and /out.
    let mut args = std::env::args().skip(1);
    let input = format!("{}/requests.log", args.next().as_deref().unwrap_or("/data"));
    let out = format!("{}/summary.txt", args.next().as_deref().unwrap_or("/out"));
    let input = File::open(&input).unwrap_or_else(|e| die(format!("{input}: {e}")));
    let mut out = File::create(&out).unwrap_or_else(|e| die(format!("{out}: {e}")));
    if !Job::new(&input, true).run(&out) {
        eprintln!("ingest: a line came after its hour closed, or a crowded hour; redoing");
        out.set_len(0).and_then(|_| out.rewind()).unwrap_or_else(|e| die(e));
        Job::new(&input, false).run(&out);
    }
}

/// Ends the run on an error: a failed system call, or input the spec rules out.
fn die(e: impl Display) -> ! {
    eprintln!("ingest: {e}");
    std::process::exit(1)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A lock is poisoned only by a thread that panics while holding it, and with
    // panic = "abort" a panic ends the process first: this `expect` cannot fire.
    m.lock().expect("no worker panics")
}


/// The slots reserved in an hour's log, BLOCK at a time, so each thread appends to its
/// own block. Each hour's count on its own cache line, so the one that threads bump does
/// not evict another hour that other threads are using.
#[derive(Default)]
#[repr(align(64))]
struct Used(AtomicUsize);

/// Who does what next, under one lock.
#[derive(Default)]
struct Plan {
    next_piece: usize,
    /// Pieces being parsed: the parsed prefix of the file ends at the first of them.
    parsing: Vec<usize>,
    /// Per closed hour (hours close in order), the number of pieces handed out before it
    /// closed: only those may write it, and it is reported once they are all parsed.
    closed: Vec<usize>,
    next_hour: usize,
    /// Workers waiting for an hour to come due, so a finished piece wakes them only if any.
    waiting: usize,
    /// A line came for a closed hour, or an hour ran out of room: the speculation failed.
    late: bool,
}

enum Task {
    /// Parse a piece; the hours below the second field are closed.
    Piece(usize, usize),
    Hour(usize),
    Stop,
}

/// Per hour, a thread's open block, as (next slot, end): both are indices into that
/// hour's log, and the block is full when they meet (or holds one slot only, and the next
/// record is a wide one).
type Open = [(usize, usize); HOURS];
/// Some lines to parse, as (bytes, start, end): the buffer, the offset in it of the first
/// line's first byte, and the offset just past the last line's newline.
type Lines = (*const u8, usize, usize);

/// Everything the workers share.
struct Job<'a> {
    input: &'a File,
    len: usize,
    speculate: bool,
    /// Per piece, the hour row of its first line plus one, once a worker has read it.
    rows: Vec<AtomicUsize>,
    /// The hours' logs, `room` slots each (see ID_SHIFT for the records).
    room: usize,
    logs: &'static [AtomicU32],
    used: Vec<Used>,
    plan: Mutex<Plan>,
    changed: Condvar,
    /// Text buffers already written, and report scratch space, for reuse.
    texts: Mutex<Vec<Vec<u8>>>,
    scratch: Mutex<Vec<Vec<u32>>>,
    /// Texts of reported hours waiting for earlier ones, and the next hour to write.
    output: Mutex<(Vec<Option<Vec<u8>>>, usize)>,
    /// Threads done.
    stopped: AtomicUsize,
}

impl<'a> Job<'a> {
    fn new(input: &'a File, speculate: bool) -> Job<'a> {
        let len = input.metadata().unwrap_or_else(|e| die(e)).len() as usize;
        let pieces = len.div_ceil(PIECE);
        // Room for every line of the file in any one hour (a line takes one slot per 40
        // bytes at most, or two if wide, with at least 46), plus each thread's last,
        // partly used block; or, speculating, less. A whole number of blocks keeps every
        // hour's log page-aligned, as madvise requires.
        let room = (len / 20 + 1 + THREADS * BLOCK).next_multiple_of(BLOCK);
        let room = if speculate { room.min(CROWDED) } else { room };
        // The logs: zeroed pages that cost nothing until first touched. `'static` only in
        // name: the last thread of the job unmaps them (see `work`), after which nothing
        // reads them.
        // SAFETY: a fresh mapping at an address of the kernel's choosing, so no existing
        // memory is affected; failure is reported as MAP_FAILED (-1). Zero bytes are a
        // valid atomic integer.
        // PROT_READ | PROT_WRITE (3); MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE (0x4022).
        let logs = unsafe { mmap(ptr::null_mut(), HOURS * room * 4, 3, 0x4022, -1, 0) };
        if logs as isize == -1 {
            die(std::io::Error::last_os_error());
        }
        Job {
            input,
            len,
            speculate,
            rows: (0..pieces).map(|_| AtomicUsize::new(0)).collect(),
            room,
            logs: unsafe { slice::from_raw_parts(logs.cast(), HOURS * room) },
            used: (0..HOURS).map(|_| Used::default()).collect(),
            plan: Mutex::default(),
            changed: Condvar::new(),
            texts: Mutex::default(),
            scratch: Mutex::default(),
            output: Mutex::new((vec![None; HOURS], 0)),
            stopped: AtomicUsize::new(0),
        }
    }

    /// Parses and reports everything, writing the hours to `out` in order. Returns
    /// whether the speculation held, i.e. whether `out` is right.
    fn run(&self, out: &File) -> bool {
        thread::scope(|s| {
            for _ in 1..THREADS {
                s.spawn(|| self.work(out));
            }
            self.work(out);
        });
        !lock(&self.plan).late
    }

    /// One thread: parses pieces and reports hours, whichever is due, until all is done.
    fn work(&self, out: &File) {
        let (mut open, mut buf, mut parsed) = ([(0, 0); HOURS], Vec::new(), None);
        loop {
            match self.next_task(parsed.take()) {
                Task::Piece(i, closed) => {
                    // SAFETY: `read` leaves SLACK zero bytes after the piece's lines.
                    if !unsafe { self.parse(&mut open, closed, self.read(i, &mut buf)) } {
                        lock(&self.plan).late = true;
                    }
                    parsed = Some(i);
                    // Among the last pieces, this was most likely this thread's last one:
                    // free its buffer now, not at the memory peak at the end.
                    if i + THREADS >= self.rows.len() {
                        buf = Vec::new();
                    }
                }
                Task::Hour(h) => self.write(out, h, self.report(h)),
                Task::Stop => break,
            }
        }
        // The last thread out frees what only reports used, before the last hours are
        // written, where the memory peaks: the scratch and the logs (with their page
        // tables). Not after a late line: another thread may still be parsing.
        if self.stopped.fetch_add(1, AcqRel) + 1 == THREADS && !lock(&self.plan).late {
            lock(&self.scratch).clear();
            // SAFETY: every hour is reported, so every piece is parsed and nothing reads
            // or writes the logs any more: `run` only reads the plan, a rerun has its own.
            unsafe { munmap(self.logs.as_ptr() as _, size_of_val(self.logs)) };
        }
    }

    /// Writes hour `hour`'s text once all earlier hours are out, and with it any later
    /// ones that were waiting for it (hours are reported out of order).
    fn write(&self, out: &File, hour: usize, text: Vec<u8>) {
        let mut output = lock(&self.output);
        let (pending, next) = &mut *output;
        pending[hour] = Some(text);
        while let Some(mut text) = pending.get_mut(*next).and_then(Option::take) {
            (&*out).write_all(&text).unwrap_or_else(|e| die(e));
            // A few are kept for reuse, which saves faulting in fresh pages; more, or any
            // near the end, would only add to the memory peak.
            let mut texts = lock(&self.texts);
            if texts.len() < 4 && *next + THREADS < HOURS {
                text.clear();
                texts.push(text);
            }
            *next += 1;
        }
    }

    /// Marks piece `parsed` done and picks the next task: an hour whose pieces are all
    /// parsed, else a piece, else (once all pieces are taken) it waits for an hour.
    fn next_task(&self, parsed: Option<usize>) -> Task {
        let mut plan = lock(&self.plan);
        if let Some(i) = parsed {
            plan.parsing.retain(|&j| j != i);
            if plan.waiting > 0 {
                self.changed.notify_all();
            }
        }
        loop {
            // Speculating, an hour closes once the parsed prefix reaches a line two hours
            // later: lines arrive at most minutes late (shard 0 uploads each hour 5 to 10
            // minutes after it ends), though nothing promises it. Every hour closes once
            // every piece is parsed.
            let frontier = plan.parsing.iter().fold(plan.next_piece, |a, &i| a.min(i));
            let close = match self.rows.get(frontier) {
                None => HOURS,
                Some(row) if self.speculate => row.load(Relaxed).saturating_sub(2),
                Some(_) => 0,
            };
            let (pieces, closed) = (plan.next_piece, plan.closed.len());
            plan.closed.resize(close.max(closed), pieces);
            let h = plan.next_hour;
            if plan.late || h == HOURS {
                return Task::Stop;
            } else if plan.closed.get(h).is_some_and(|&pieces| frontier >= pieces) {
                plan.next_hour += 1;
                return Task::Hour(h);
            } else if plan.next_piece < self.rows.len() {
                let i = plan.next_piece;
                plan.parsing.push(i);
                plan.next_piece += 1;
                return Task::Piece(i, plan.closed.len());
            }
            plan.waiting += 1;
            plan = self.changed.wait(plan).expect("no worker panics");
            plan.waiting -= 1;
        }
    }

    /// Reads piece `i`, the lines that start in [i * PIECE, (i + 1) * PIECE), into `buf`:
    /// from the byte before it to the end of its last line, then SLACK zero bytes. Returns
    /// the bytes and the offsets of its first line's start and its last line's end. Also
    /// notes the hour of its first line for the plan.
    fn read(&self, i: usize, buf: &mut Vec<u8>) -> Lines {
        let at = (i * PIECE).saturating_sub(1);
        let end = self.len.min((i + 1) * PIECE + MAX_LINE) - at;
        buf.resize(end + SLACK, 0);
        let read = self.input.read_exact_at(&mut buf[..end], at as u64);
        read.unwrap_or_else(|e| die(e));
        buf[end..].fill(0);
        // A line starts after a newline, so the byte before the piece decides where its
        // first line starts, and the first newline from its last byte on where it stops.
        let after_newline = |from: usize| {
            let newline = buf[from..end].iter().position(|&b| b == b'\n');
            newline.map_or(end, |p| from + p + 1)
        };
        let from = if i == 0 { 0 } else { after_newline(0) };
        let to = after_newline(((i + 1) * PIECE - 1 - at).min(end));
        // An unknown hour (0; a piece may hold no line start) only delays the reports.
        let row = hour_row(&buf[from..from + 13]).map_or(0, |row| row + 1);
        self.rows[i].store(row, Relaxed);
        (buf.as_ptr(), from, to)
    }

    /// Parses the lines in `[from, to)` of the bytes at `p` into the hour logs. Returns
    /// false, and stops, at a line of an hour below `closed`, or one an hour has no more
    /// room for.
    ///
    /// # Safety
    /// `from` must start a line, `to` must end one, and the SLACK bytes after every line
    /// start in between must be readable.
    unsafe fn parse(&self, open: &mut Open, closed: usize, (p, from, to): Lines) -> bool {
        // The current hour, as a cache: shards write in time order, so it rarely changes.
        // `key` holds its `YYYY-MM-DDTHH`, compared as the first 13 of 16 loaded bytes.
        let (mut key, mut row, (mut n, mut end)) = (_mm_setzero_si128(), 0, open[0]);
        let (mut used, mut log) = (&self.used[0].0, self.logs.as_ptr());
        let mut ends = [0usize; SCAN / 32];
        let (mut line, mut block) = (from, from);
        while line < to {
            // Index the line ends of the next SCAN bytes. A 64-byte block holds at most two
            // (a line has at least 40 bytes), so both candidates (the lowest set bit, and
            // the lowest once that is cleared) are stored and the count advances by the
            // popcount, without a branch per block. SCAN bytes hold at most 103 lines, so
            // the cap never bites; it only lets the compiler drop the bounds checks.
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
            for &e in &ends[..k] {
                let start = line;
                line = e + 1;
                let stamp = _mm_loadu_si128(p.add(start).cast());
                if _mm_movemask_epi8(_mm_cmpeq_epi8(stamp, key)) & 0x1FFF != 0x1FFF {
                    open[row] = (n, end);
                    let t = slice::from_raw_parts(p.add(start), 13);
                    let what = || format!("{}: hour outside the span", t.escape_ascii());
                    row = hour_row(t).unwrap_or_else(|| die(what()));
                    if row < closed {
                        return false;
                    }
                    (key, used, (n, end)) = (stamp, &self.used[row].0, open[row]);
                    log = self.logs[row * self.room..].as_ptr();
                }
                let (latency, space) = parse_latency(p.add(e - 16));
                let status = e - 16 + space - 3;
                let error = (*p.add(status) == b'5') as u32;
                // The endpoint starts after the space that ends the method (which follows
                // the 27-byte timestamp and its space), and ends before the status.
                let name = start + 29 + spaces(p.add(start + 28)).trailing_zeros() as usize;
                let id = ENDPOINTS.id(p.add(name), status - 1 - name);
                let record = id << ID_SHIFT | error << ERROR_SHIFT;
                // One slot, or two for a wide latency, in one block: a wide one that
                // would start in the block's last slot leaves it empty.
                let wide = latency >= LOW;
                n += (wide && n + 1 == end) as usize;
                if n == end {
                    n = used.fetch_add(BLOCK, Relaxed);
                    end = n + BLOCK;
                    if end > self.room {
                        return false;
                    }
                }
                // SAFETY (both stores): `used` never passes the hour's room (the check
                // above stops first), and hands each block to one worker.
                if wide {
                    (*log.add(n)).store(latency / LOW, Relaxed);
                    n += 1;
                }
                (*log.add(n)).store(record | latency % LOW, Relaxed);
                n += 1;
            }
        }
        open[row] = (n, end);
        true
    }

    /// The output lines of one hour, appended to `text`; `sorted` is scratch space. The
    /// hour is closed and every piece that could write it parsed, so its log is final.
    /// 1. count pass: requests and 5xx per endpoint;
    /// 2. a slice of `sorted` per endpoint, in output order (a prefix sum);
    /// 3. scatter pass: every latency into its endpoint's slice;
    /// 4. the hour's log pages given back;
    /// 5. per endpoint, three selections for the percentiles and its output line.
    fn report(&self, hour: usize) -> Vec<u8> {
        // Scratch has room for any hour from the start: grown step by step, its freed
        // smaller copies would stay in the malloc heap.
        let sorted = lock(&self.scratch).pop();
        let mut sorted = sorted.unwrap_or_else(|| vec![0; 2 * self.room]);
        let mut text = lock(&self.texts).pop().unwrap_or_default();
        let at = hour * self.room;
        let log = &self.logs[at..at + self.used[hour].0.load(Relaxed)];
        let slots = || log.iter().map(|r| r.load(Relaxed));
        // Count pass by endpoint id and 5xx flag together (the record's top 12 bits), so
        // `tally[2 * id]` holds the endpoint's other responses and `tally[2 * id + 1]` its
        // 5xx ones.
        let mut tally = [0u32; 2 * IDS];
        slots().for_each(|r| tally[(r >> ERROR_SHIFT) as usize] += 1);
        let count = |id: u16| tally[2 * id as usize] + tally[2 * id as usize + 1];
        // Each endpoint gets a slice of `sorted`, in output order, and id 0 the last one.
        let order = ENDPOINTS.order();
        let (mut next, mut total) = ([0u32; IDS], 0);
        for &id in order.iter().chain(&[0]) {
            (next[id as usize], total) = (total, total + count(id));
        }
        // Scatter pass. A wide latency's high bits are in the slot before its record; that
        // slot reads as id 0, so it falls in id 0's slice, never printed.
        slots().fold(0, |before, r| {
            let high = if before >> ID_SHIFT == 0 { before } else { 0 };
            let next = &mut next[(r >> ID_SHIFT) as usize];
            sorted[*next as usize] = high * LOW + r % LOW;
            *next += 1;
            r
        });
        // SAFETY: madvise(DONTNEED) on our own mapping, which reads as zeros afterwards;
        // nothing reads this hour's log again. If it is refused, the pages stay.
        // MADV_DONTNEED (4).
        unsafe { madvise(log.as_ptr() as _, size_of_val(log), 4) };
        let stamp = stamp(hour);
        // Room for every line at once (untouched pages cost nothing), so the text never
        // grows by copying.
        text.reserve(256 * order.len());
        let mut start = 0;
        for &id in &order {
            let n = count(id) as usize;
            let errors = tally[2 * id as usize + 1];
            put_line(&mut text, &stamp, id, &mut sorted[start..start + n], errors);
            start += n;
        }
        // As for texts: a few are kept, more would only add to the peak.
        let mut scratch = lock(&self.scratch);
        if scratch.len() < 2 {
            scratch.push(sorted);
        }
        text
    }
}

/// Appends one group's output line to `text`: `stamp` (21 bytes, padded to 32), the
/// endpoint `id`'s name, and the numbers from its latencies `v` and its 5xx count.
/// Nothing for an empty group.
fn put_line(text: &mut Vec<u8>, stamp: &[u8; 32], id: u16, v: &mut [u32], errors: u32) {
    let (n, name) = (v.len() as u32, ENDPOINTS.get(id));
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

/// Nearest-rank p50, p95 and p99 of a non-empty group, which it may reorder.
fn percentiles(v: &mut [u32]) -> [u32; 3] {
    // Nearest rank: the 1-indexed rank ceil(p * n / 100), the spec's (p * n + 99) div 100.
    let ranks = [50, 95, 99].map(|p| (p * v.len()).div_ceil(100) - 1);
    if v.len() <= 64 {
        // The value of rank r is the largest x with at most r values below it. Counting
        // them for every x takes n^2 compares, but vector ones, with no branch on the
        // data (sorting small groups mispredicted about every other compare).
        let mut p = [0; 3];
        for &x in v.iter() {
            let below = v.iter().map(|&y| (y < x) as u32).sum::<u32>() as usize;
            for (p, &r) in p.iter_mut().zip(&ranks) {
                *p = (*p).max(if below <= r { x } else { 0 });
            }
        }
        return p;
    }
    // Low to high, each selection searching only above the previous one: about 1.55n
    // steps. Beyond 64 values the three ranks are distinct.
    let (_, &mut p50, above) = v.select_nth_unstable(ranks[0]);
    let (_, &mut p95, above) = above.select_nth_unstable(ranks[1] - ranks[0] - 1);
    let (_, &mut p99, _) = above.select_nth_unstable(ranks[2] - ranks[1] - 1);
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

/// The row (0 to 71) of a `YYYY-MM-DDTHH` hour, if in the spec's span. The span runs from
/// 2026-10-30T18 over the end of October (31 days) into November; its start is hardcoded
/// here and in `stamp` (see `HOURS`).
fn hour_row(t: &[u8]) -> Option<usize> {
    let two = |at: usize| (t[at] as i64 - 48) * 10 + t[at + 1] as i64 - 48;
    let (month, day) = (two(5), two(8));
    let row = (day + if month == 11 { 31 } else { 0 } - 30) * 24 + two(11) - 18;
    let ok = two(0) * 100 + two(2) == 2026 && (month == 10 || month == 11);
    usize::try_from(row).ok().filter(|&row| ok && row < HOURS)
}

/// The output's `YYYY-MM-DDTHH:00:00Z ` for hour row `row`, padded to 32 bytes. The
/// span start is hardcoded here and in `hour_row` (see `HOURS`).
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
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, b)) as u32 as u64
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
    /// Ids handed out so far; its lock serialises inserts.
    count: Mutex<usize>,
    /// Ids in output (byte) order of their names, sorted again when new ones came in.
    order: Mutex<Vec<u16>>,
}

// SAFETY: a name is written once, under `count`'s lock, before its id is published with
// a Release store to its slot; readers reach a name only through an id they loaded with
// Acquire, or one below a count read under the same lock, and nobody writes it again.
unsafe impl Sync for Endpoints {}

/// SAFETY: all zero bytes are a valid, empty `Name`.
const EMPTY: Name = unsafe { std::mem::zeroed() };
static ENDPOINTS: Endpoints = Endpoints {
    slots: [const { AtomicU16::new(0) }; SLOTS],
    names: [const { UnsafeCell::new(EMPTY) }; IDS],
    count: Mutex::new(0),
    order: Mutex::new(Vec::new()),
};

impl Endpoints {
    fn get(&self, id: u16) -> &Name {
        // SAFETY: only published ids are passed in (see the Sync note).
        unsafe { &*self.names[id as usize % IDS].get() }
    }

    /// Every id handed out so far, in output order. A report asks once its hour's pieces
    /// are parsed, so the ids in its log are among them.
    fn order(&self) -> Vec<u16> {
        let mut order = lock(&self.order);
        let count = *lock(&self.count);
        if order.len() < count {
            *order = (1..=count as u16).collect();
            order.sort_unstable_by_key(|&id| &self.get(id).bytes[..self.get(id).len as usize]);
        }
        order.clone()
    }

    /// The id of the `len`-byte endpoint at `p`, adding it if new.
    ///
    /// # Safety
    /// 128 bytes at `p` and the 8 bytes before `p + len` must be readable.
    #[inline(always)]
    unsafe fn id(&self, p: *const u8, len: usize) -> u32 {
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
                id if self.get(id).is(p, len) => return id as u32,
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
    unsafe fn insert(&self, p: *const u8, len: usize, mut i: usize) -> u32 {
        let mut count = lock(&self.count);
        // Another worker may have added it since our probe found the slot empty.
        while let id @ 1.. = self.slots[i % SLOTS].load(Relaxed) {
            if self.get(id).is(p, len) {
                return id as u32;
            }
            i += 1;
        }
        // Over 2,047 endpoints, or a name over 126 bytes (with its space, before the length
        // byte), fail the indexing below: the spec promises 1,842 and 125.
        *count += 1;
        let name = &mut *self.names[*count].get();
        name.bytes[..=len].copy_from_slice(slice::from_raw_parts(p, len + 1));
        name.len = len as u8;
        self.slots[i % SLOTS].store(*count as u16, Release);
        *count as u32
    }
}
