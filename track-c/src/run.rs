//! The parallel engine. The workers first build the region map and fault in
//! the name table, then claim chunks of records.csv until none are left
//! (phase 1), then claim name buckets until none are left (phase 2, see
//! names.rs). A worker waits on the others only twice: for the region map,
//! and at the end of phase 1.
//!
//! Choosing the chunk cuts is where the quote-parity problem lives; see `run`.

use crate::names::{Bits, Counts, Pairs, Table};
use crate::parse::{merge, Agg, Local, Parser, Res, Span, EMPTY, PAD};
use crate::{die, or_die, GROUPS};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering::*};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, Scope};

/// The most worker threads (the scoring machine gives us twelve).
const MAX_THREADS: usize = 12;
/// Start-up tasks: pieces of regions.csv to parse, then of the name table
/// to fault in.
const PARTS: usize = 32;
/// Stack size for workers, which need about 40 KiB; a small stack can never
/// be given a huge page.
const STACK: usize = 256 << 10;
/// Bytes read past a chunk's nominal end to find its real end, at first
/// (records average 61 bytes); a longer last record doubles it until found.
const SLACK: usize = 1 << 10;

/// One output line's numbers: the parser's (see `Agg`), then phase 2's (see
/// `Table::count`).
pub type Total = (Agg, Counts);
pub type Totals = Vec<Total>;

/// Counts the whole file, `len` bytes, with one worker per CPU (at most
/// 12), in chunks of nominally `piece` bytes: totals by group.
///
/// records.csv is cut into chunks just after an LF that is followed by a
/// digit or a BOM byte. By spec 2.2 no quoted value holds a digit or a
/// non-ASCII byte, so such an LF ends a record. That guess is checked, not
/// trusted. Each chunk is parsed as if it started outside quotes, and counts
/// its quote bytes. A position is inside quotes exactly when an odd number of
/// quotes precede it, so once every chunk has an even number of quotes before
/// it, each one did start outside quotes, right after a terminator, and was
/// parsed exactly as one sequential pass would have parsed it. If not, this
/// returns the offset of a chunk cut it could not prove, and the caller
/// retries with a single chunk.
pub fn run(piece: usize, file: &File, len: usize, regions: &Regions) -> Res<Totals> {
    let threads = thread::available_parallelism().map_or(1, usize::from);
    let threads = threads.min(MAX_THREADS);
    let (table, chunks) = (Table::new(), len.div_ceil(piece).max(1));
    let buckets = table.buckets();
    let quotes: Vec<AtomicU64> = (0..chunks).map(|_| AtomicU64::new(0)).collect();
    // Shared counters: the next chunk, start-up task and name bucket to
    // claim, start-up tasks to wait for, workers still in phase 1.
    let [chunk, task, bucket] = [0, 0, 0].map(AtomicUsize::new);
    let (pending, parsing) = (AtomicUsize::new(PARTS), AtomicUsize::new(threads));
    // The lowest offset where the file broke the grammar, if it did. No
    // chunk is claimed after that.
    let fail = Mutex::new(None::<usize>);
    let failed = |at: usize| {
        chunk.store(usize::MAX / 2, Relaxed);
        let mut fail = fail.lock().unwrap();
        *fail = Some(fail.map_or(at, |old| old.min(at)));
    };
    let all_pairs: Vec<OnceLock<Pairs>> = (0..threads).map(|_| OnceLock::new()).collect();
    let total = Mutex::new(vec![(EMPTY, [0; 2]); GROUPS]);
    let work = |t: usize| {
        // The start-up tasks go to whichever workers are up first, so that
        // none waits for the last thread to start, only for the region map.
        loop {
            match task.fetch_add(1, Relaxed) {
                k if k < PARTS => regions.parse(k, &pending),
                k if k < 2 * PARTS => table.touch(k - PARTS, PARTS),
                _ => break,
            }
        }
        // Most of this worker's pairs, at 2 bytes for a record of about 61,
        // are faulted in now.
        let pairs = Pairs::new(buckets, len, len / 64 / threads);
        let aggs = [EMPTY; GROUPS];
        let (mut parser, mut buffer) = (Parser::new(&table, regions.map), Vec::new());
        let mut local = Local { aggs, pairs };
        wait(&pending);
        while let Some(c) = Some(chunk.fetch_add(1, Relaxed)).filter(|&c| c < chunks) {
            let span = read(file, len - c * piece, c * piece, piece, &mut buffer);
            match parser.chunk(&buffer, span, &mut local) {
                Ok(n) => quotes[c].store(n, Relaxed),
                Err(at) => failed(c * piece + at),
            }
        }
        _ = all_pairs[t].set(local.pairs);
        parsing.fetch_sub(1, Release);
        wait(&parsing);
        // Phase 2, unless phase 1 failed somewhere; then nothing is counted.
        let mut counts = [[0; GROUPS]; 2];
        if fail.lock().unwrap().is_none() {
            let all: Vec<&Pairs> = all_pairs.iter().filter_map(OnceLock::get).collect();
            let mut bits: Box<Bits> = Box::new([0; 1024]);
            while let Some(b) = Some(bucket.fetch_add(1, Relaxed)).filter(|&b| b < buckets) {
                table.count(&all, b, &mut bits, &mut counts);
            }
        }
        let mut total = total.lock().unwrap();
        for (g, (t, a)) in total.iter_mut().zip(&local.aggs).enumerate() {
            merge(&mut t.0, a);
            (t.1[0], t.1[1]) = (t.1[0] + counts[0][g], t.1[1] + counts[1][g]);
        }
    };
    thread::scope(|s| grow(s, 0, threads, &work));
    let mut before = 0;
    for (c, q) in quotes.iter().enumerate() {
        if before % 2 == 1 {
            failed(c * piece);
        }
        before += q.load(Relaxed);
    }
    let fail = fail.into_inner().unwrap();
    fail.map_or(Ok(total.into_inner().unwrap()), Err)
}

/// Waits, yielding rather than spinning, so that a crowded machine can run
/// the workers still busy, until `left` reaches 0.
fn wait(left: &AtomicUsize) {
    while left.load(Acquire) > 0 {
        thread::yield_now();
    }
}

/// Starts workers `2t + 1` and `2t + 2` (and, through them, the rest of the
/// `n`), then runs worker `t` on this thread. Starting threads in a tree
/// takes about log2(n) thread starts before the last worker runs, not n.
fn grow<'s, F: Fn(usize) + Sync>(s: &'s Scope<'s, '_>, t: usize, n: usize, work: &'s F) {
    for child in [2 * t + 1, 2 * t + 2].into_iter().filter(|&c| c < n) {
        let builder = thread::Builder::new().stack_size(STACK);
        let spawned = builder.spawn_scoped(s, move || grow(s, child, n, work));
        spawned.unwrap_or_else(|e| die(&format!("starting a thread: {e}")));
    }
    work(t);
}

/// Reads the chunk of nominally `piece` bytes at `from` of records.csv, of
/// which `rest` bytes are left from there, into `buffer` with pread, rather than mapping the file: on the scoring
/// box the page faults, page-table set-up and tear-down of a 200 MB mapping
/// cost more than copying the bytes. Returns where in `buffer` the chunk
/// starts and ends, with `PAD` zero bytes after it.
///
/// The chunk runs from the first record boundary at or after `from` to the
/// first at or after `from + piece` (the first chunk from the end of the
/// header line, the last one to the end of the file). A boundary is just
/// past an LF that is followed by a digit or a BOM byte: a guess, checked as
/// `run` says. Both ends are found from the file's bytes alone, so
/// neighbouring chunks always agree on the cut between them.
fn read(file: &File, rest: usize, from: usize, piece: usize, buffer: &mut Vec<u8>) -> Span {
    let (nominal, mut want) = (piece.min(rest), (piece + SLACK).min(rest));
    loop {
        buffer.resize(want + PAD, 0);
        let read = file.read_exact_at(&mut buffer[..want], from as u64);
        let b = &buffer[..or_die(read.map(|_| want), "records.csv")];
        // The header line: an optional BOM, then an optional CR before its LF.
        let bom = 3 * usize::from(b.first() != Some(&0xEF));
        let header = &"\u{feff}id,name,note,region_id,amount".as_bytes()[bom..];
        let crlf = usize::from(b.get(header.len()) == Some(&b'\r'));
        let lf = b.starts_with(header) && b.get(header.len() + crlf) == Some(&b'\n');
        let start = match from {
            0 if lf => Some(header.len() + crlf + 1),
            0 => die("records.csv: no header line"),
            _ => cut(b, 0),
        };
        // The last chunk ends at the end of the file, where no cut is.
        if let Some(end) = cut(b, nominal).or((want == rest).then_some(want)) {
            let start = start.unwrap_or(end).min(end);
            buffer[end..end + PAD].fill(0);
            // Nor are the bytes before the chunk in its first 64-byte block.
            buffer[start & !63..start].fill(0);
            // An LF after a last record with no terminator (see `Parser::chunk`).
            if end == rest && end > start && buffer[end - 1] != b'\n' {
                buffer[end] = b'\n';
            }
            return (start, end);
        }
        want = (2 * want).min(rest);
    }
}

/// Just past the first LF at or after `from` that is followed by a digit or
/// a BOM byte, if `b` holds one. The byte after the LF must be in `b`, so an
/// LF at its very end is not taken: the caller reads further and asks again.
fn cut(b: &[u8], from: usize) -> Option<usize> {
    let at = |i: usize| b[i] == b'\n' && (b[i + 1].is_ascii_digit() || b[i + 1] == 0xEF);
    let lf = (from..b.len().saturating_sub(1)).find(|&i| at(i))?;
    Some(lf + 1)
}

/// regions.csv: a direct-indexed map from region id to group, filled by the
/// workers in parallel at start-up. Group 0 is `_UNMATCHED`; countries get
/// 1..=255 in order of first sight, and are listed in code order at the end.
/// Any line that is not `id,CCC` stops the program with its byte offset.
pub struct Regions {
    /// The lines after the header line.
    lines: &'static [u8],
    /// Group by region id, one byte each. The last entry is a 0 that every
    /// larger id is clamped to (see `parse`), so a lookup never branches.
    pub map: &'static [AtomicU8],
    /// Group by country code index (`AAA` = 0); 0 until the code is seen,
    /// and the groups handed out so far, held while handing out a new one.
    codes: (Vec<AtomicU8>, Mutex<u8>),
}

impl Regions {
    /// Maps the file and sizes the map by the id on its last line, the
    /// largest, since spec 2.3 lists ids unique and ascending (`parse`
    /// rejects any id above it, so a file out of order cannot overrun).
    pub fn open(path: &str) -> Self {
        let f = or_die(File::open(path), path);
        let len = or_die(f.metadata(), path).len() as usize;
        // SAFETY: any bytes are valid `u8`s.
        let file: &[u8] = unsafe { crate::mapped(len, std::os::fd::AsRawFd::as_raw_fd(&f)) };
        let lines = file.strip_prefix(b"region_id,country\n").unwrap_or(b"");
        if lines.last() != Some(&b'\n') {
            die(&format!("{path}: bad header, or no LF at its end"));
        }
        let last = lines[..lines.len() - 1].iter().rposition(|&c| c == b'\n');
        let last = last.map_or(0, |p| p + 1);
        let max = lines[last..].iter().take_while(|c| c.is_ascii_digit());
        let max = max.fold(0, |n, &c| (n * 10 + usize::from(c - b'0')).min(1 << 27));
        // SAFETY: zero is a valid AtomicU8.
        let map = unsafe { crate::mapped(max + 2, -1) };
        let codes = (0..26 * 26 * 26).map(|_| AtomicU8::new(0)).collect();
        let codes = (codes, Mutex::new(0));
        Regions { lines, map, codes }
    }

    /// Adds share `t` of `PARTS` of the lines `id,CCC\n`, then counts it off
    /// `left`. A share runs from the first line start at or after its nominal
    /// start to the next share's.
    fn parse(&self, t: usize, left: &AtomicUsize) {
        let b = self.lines;
        let start = |k: usize| match k * b.len() / PARTS {
            0 => 0,
            p => p + b[p - 1..].iter().position(|&c| c == b'\n').unwrap(), // the last byte is an LF
        };
        let (mut p, end) = (start(t), start(t + 1));
        while p < end {
            let (line, mut id) = (p, 0);
            let swar = b.get(p..p + 8).and_then(|w| id_of(w.try_into().unwrap()));
            if let Some((value, digits)) = swar {
                (id, p) = (value, p + digits);
            }
            while b[p].is_ascii_digit() && id < self.map.len() {
                (id, p) = (id * 10 + usize::from(b[p] - b'0'), p + 1);
            }
            let code = b.get(p + 1..p + 4).unwrap_or(b"");
            let ok = p > line && id < self.map.len() - 1 && b[p] == b',';
            if !ok || b.get(p + 4) != Some(&b'\n') || !code.iter().all(u8::is_ascii_uppercase) {
                die(&format!("regions.csv byte {line}: not `id,CCC` in order"));
            }
            let code = code.iter().fold(0, |c, &l| c * 26 + usize::from(l - b'A'));
            self.map[id].store(self.group_of(code), Relaxed);
            p += 5;
        }
        left.fetch_sub(1, Release);
    }

    /// The group of country code `code`, handing it the next one if new.
    fn group_of(&self, code: usize) -> u8 {
        let (codes, groups) = &self.codes;
        if codes[code].load(Relaxed) == 0 {
            let mut groups = groups.lock().unwrap();
            // Another worker may have handed this code a group meanwhile.
            if codes[code].load(Relaxed) == 0 {
                let next = groups.checked_add(1);
                *groups = next.unwrap_or_else(|| die("regions.csv: over 255 countries"));
                codes[code].store(*groups, Relaxed);
            }
        }
        codes[code].load(Relaxed)
    }

    /// `(code, group)` for every country seen, in code order.
    pub fn countries(&self) -> impl Iterator<Item = (String, u8)> + '_ {
        let letters = |c: usize| [c / 676, c / 26 % 26, c % 26].map(|l| (b'A' + l as u8) as char);
        let seen = self.codes.0.iter().map(|g| g.load(Relaxed)).enumerate();
        let seen = seen.filter(|&(_, g)| g != 0);
        seen.map(move |(c, g)| (letters(c).iter().collect(), g))
    }
}

/// The id a line starts with and its length, read off the line's first
/// eight bytes `w`, if it is 1 to 7 digits followed by a comma: the digits
/// right-aligned with '0's below them, then checked and converted eight at
/// once. Anything else is left to the caller's loop, one byte at a time.
fn id_of(w: [u8; 8]) -> Option<(usize, usize)> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    let (w, commas) = (u64::from_le_bytes(w), u64::from_le_bytes(w) ^ 0x2C * ONES);
    let n = ((commas.wrapping_sub(ONES) & !commas & 0x80 * ONES).trailing_zeros() / 8) as usize;
    let low = |n: usize| (w << (64 - 8 * n) | 0x30 * ONES >> (8 * n)) ^ 0x30 * ONES;
    let digits = (1..8).contains(&n).then(|| low(n))?;
    // Every byte at most 9, then pairs, fours and the eight combined.
    ((digits | digits.wrapping_add(0x76 * ONES)) & 0x80 * ONES == 0).then_some(())?;
    let v = digits.wrapping_mul(10 << 8 | 1) >> 8 & 0x00FF_00FF_00FF_00FF;
    let v = v.wrapping_mul(100 << 16 | 1) >> 16 & 0x0000_FFFF_0000_FFFF;
    Some(((v.wrapping_mul(10_000 << 32 | 1) >> 32) as usize, n))
}
