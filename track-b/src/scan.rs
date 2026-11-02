//! The corpus scan. A chunk of `docs.txt` is read with `pread` into a reused
//! buffer, tokenized 64 bytes at a time with AVX2, and every token is looked up
//! in the query term table. For each document the scan emits its distinct BOOL
//! terms (as postings, split by partition for the build) and checks the
//! phrases anchored at its tokens, so phrase answers need no index at all.

use crate::fail;
use crate::index::PARTS;
use crate::query::{Parsed, Probe, Table, ID_MASK, NEVER, NONE, RANGE_SHIFT, SHORT};
use std::arch::x86_64::*;
use std::fs::File;
use std::os::unix::fs::FileExt;

#[cfg(not(target_feature = "avx2"))]
compile_error!("track-b needs AVX2: build with -C target-cpu=x86-64-v3 (see .cargo/config.toml)");

/// Bytes per `pread`. Reading into a small reused buffer measured 15% faster
/// than `mmap` on the scoring box (Track A), whose page-fault path is slow.
const PIECE: usize = 64 << 10;
/// Slack after the data, so 64-byte vector loads and 16-byte key loads stay
/// inside the buffer; whatever they read past the data is masked off.
const PAD: usize = 64;
/// Tokens hashed, with their buckets prefetched, before the first is resolved,
/// so that the cache misses of rare words overlap.
const BATCH: usize = 64;
/// Documents are numbered from 0 in each segment and stored as `u16`.
pub const MAX_SEGMENT_DOCS: u32 = 1 << 16;

/// What the scan of one chunk leaves for the build.
#[derive(Default)]
pub struct ChunkOut {
    /// `doc << 16 | id / PARTS` for every distinct BOOL term of every
    /// document, grouped by partition `p = id % PARTS` and in document order
    /// within one: partition `p` is `postings[starts[p]..starts[p + 1]]`.
    /// `doc` is numbered within the segment.
    postings: Vec<u32>,
    starts: Vec<u32>,
    /// `(phrase, document id)` of every phrase match, in document order.
    pub matches: Vec<(u32, u32)>,
    /// First document id of the chunk and its number of documents.
    pub first: u32,
    pub lines: u32,
}

impl ChunkOut {
    /// The postings of partition `p`.
    pub fn stream(&self, p: usize) -> &[u32] {
        &self.postings[self.starts[p] as usize..self.starts[p + 1] as usize]
    }

    /// Groups the postings by partition. Document `i` of the chunk (numbered
    /// `local + i` in the segment) has the distinct BOOL terms
    /// `fresh[ends[i - 1]..ends[i]]`.
    fn partition(&mut self, fresh: &[u32], ends: &[u32], local: u32) {
        let mut cursor = [0u32; PARTS];
        for &id in fresh {
            cursor[id as usize % PARTS] += 1;
        }
        self.starts = vec![0];
        let mut at = 0;
        for c in &mut cursor {
            (*c, at) = (at, at + *c);
            self.starts.push(at);
        }
        self.postings.clear();
        self.postings.reserve(fresh.len());
        let out = self.postings.spare_capacity_mut().as_mut_ptr();
        let mut begin = 0;
        for (i, &end) in ends.iter().enumerate() {
            let doc = (local + i as u32) << 16;
            for &id in &fresh[begin..end as usize] {
                let c = &mut cursor[id as usize % PARTS];
                // SAFETY: the cursor of partition `p` runs from its start to
                // the next one, so each of the `fresh.len()` slots is written
                // exactly once.
                unsafe { (*out.add(*c as usize)).write(doc | (id / PARTS as u32)) };
                *c += 1;
            }
            begin = end as usize;
        }
        // SAFETY: every slot below `fresh.len()` was written above.
        unsafe { self.postings.set_len(fresh.len()) };
    }
}

/// Per-thread scratch, reused for every chunk.
pub struct Scanner<'a> {
    table: &'a Table,
    queries: &'a [u8],
    parsed_ranges: &'a [&'a Parsed],
    /// The read buffer; once the scan is over, a format task's output.
    pub buf: Vec<u8>,
    /// Per token of the region being scanned: its start and end (one past)
    /// at `bounds[2k]` and `bounds[2k + 1]`.
    bounds: Vec<u32>,
    /// Per line of the region: one past its last token.
    lfs: Vec<u32>,
    /// Per token of the region: its value in the term table (0 for no query
    /// term).
    vals: Vec<u32>,
    doc: Doc,
}

/// Per-chunk scratch: the number `local` of the next document in its
/// segment, the distinct BOOL terms of the chunk's documents (`fresh`, the
/// documents ending at `ends`), and the stamps that tell which terms a
/// document has already shown.
#[derive(Default)]
struct Doc {
    local: u32,
    fresh: Vec<u32>,
    ends: Vec<u32>,
    /// Per term id, the stamp of the last document it was seen in: a term
    /// was seen in this document if its stamp is `stamp`. Stamps run from 1
    /// to `NEVER - 1`, and are cleared when they wrap. Ids that are no BOOL
    /// term keep `NEVER`, so they never look new.
    stamps: Vec<u8>,
    stamp: u8,
    /// The document's anchor tokens and their first phrase links.
    links: Vec<(u32, u32)>,
}

impl<'a> Scanner<'a> {
    pub fn new(table: &'a Table, queries: &'a [u8], parsed_ranges: &'a [&'a Parsed]) -> Self {
        let mut doc = Doc::default();
        (doc.stamps, doc.stamp) = (table.stamps(), 1);
        let (buf, bounds, lfs, vals) = Default::default();
        Scanner {
            table,
            queries,
            parsed_ranges,
            buf,
            bounds,
            lfs,
            vals,
            doc,
        }
    }

    /// Scans the lines that start in `docs.txt[from..to]`, numbering
    /// documents from `first` (the id of the segment's first line).
    ///
    /// Each turn reads up to `PIECE` more bytes behind the carried ones, then:
    /// while `skip`, drops bytes up to the first LF (the end of the previous
    /// chunk's last line); tokenizes the whole lines it has; and carries the
    /// partial last line to the front for the next read. It is `done` after
    /// the line holding byte `to - 1`, or at the end of the file.
    pub fn scan(&mut self, file: &File, (from, to): (u64, u64), first: u32, out: &mut ChunkOut) {
        self.doc.fresh.clear();
        self.doc.ends.clear();
        out.matches.clear();
        out.lines = 0;
        // buf[..len] holds the file from `off`. A chunk that does not start the
        // file reads from `from - 1` and drops everything up to the first LF:
        // that is the end of the previous chunk's last line.
        let (mut off, mut len, mut skip) = (from.saturating_sub(1), 0, from > 0);
        let mut known = false; // whether `doc.local` is set
        let mut buf = std::mem::take(&mut self.buf); // put back on return
        loop {
            buf.resize(buf.len().max(len + PIECE + PAD), 0);
            let n = read_full(file, &mut buf[len..len + PIECE], off + len as u64);
            len += n;
            let mut start = 0;
            if skip {
                match buf[..len].iter().position(|&b| b == b'\n') {
                    Some(i) => (start, skip) = (i + 1, false),
                    None if n == 0 => break,
                    None => {
                        (off, len) = (off + len as u64, 0);
                        continue;
                    }
                }
            }
            // The chunk's last line is the one holding byte `to - 1`, so it
            // ends at the first LF from there.
            let last = (to - 1).saturating_sub(off) as usize;
            let last_lf = buf[last.min(len)..len].iter().position(|&b| b == b'\n');
            let (end, done) = if off + start as u64 >= to {
                (start, true)
            } else if let Some(i) = last_lf {
                (last + i + 1, true)
            } else {
                let lf = buf[start..len].iter().rposition(|&b| b == b'\n');
                (lf.map_or(start, |i| start + i + 1), n == 0)
            };
            if end > start {
                if !known {
                    out.first = parse_id(&buf[start..end], off + start as u64);
                    let local = out.first.checked_sub(first);
                    self.doc.local = local.unwrap_or_else(|| fail("document ids out of order"));
                    known = true;
                }
                self.region(&buf, start, end, out);
            }
            if done {
                break;
            }
            buf.copy_within(end..len, 0);
            (off, len) = (off + end as u64, len - end);
        }
        self.buf = buf;
        let doc = &self.doc;
        out.partition(&doc.fresh, &doc.ends, doc.local - out.lines);
    }

    /// Finds the tokens and LFs of `buf[start..end]` (whole lines, the first
    /// of them document `first`); returns how many of each.
    #[inline(never)]
    fn lex(&mut self, buf: &[u8], start: usize, end: usize, first: u32) -> (usize, usize) {
        // A block adds at most 64 bounds and 64 line ends, and `flatten`
        // writes up to 64 bounds past the count. The arrays are filled
        // through pointers and their lengths set at the end, so their unused
        // room is never touched.
        let most = (end - start).div_ceil(64) * 64 + 64;
        self.bounds.clear();
        self.bounds.reserve(most);
        self.lfs.clear();
        self.lfs.reserve(most);
        assert!(end + PAD <= buf.len()); // every block is readable
        let (bounds, lfs) = (self.bounds.as_mut_ptr(), self.lfs.as_mut_ptr());
        let (mut n, mut lines, mut tabs) = (0usize, 0, 0);
        // `in_id` is all ones while inside a line's "id TAB" prefix: every line
        // starts there, an LF enters it and the TAB leaves it, so a prefix XOR
        // of the LF and TAB bits marks it and the ids never become tokens.
        let (mut in_id, mut carry) = (!0u64, 0u64);
        for at in (start..end).step_by(64) {
            // SAFETY: `at + 64 <= end + PAD` bytes are readable (checked above).
            let (alnum, lf, tab) = unsafe { classify(buf.as_ptr().add(at)) };
            let live = u64::MAX >> (64 - (end - at).min(64));
            let (lf, tab) = (lf & live, tab & live);
            // Most blocks hold no LF or TAB, and stay all in or all out.
            let flips = lf | tab;
            let id = in_id ^ if flips != 0 { prefix_xor(flips) } else { 0 };
            in_id = ((id as i64) >> 63) as u64;
            let token = alnum & live & !id;
            let before = token << 1 | carry;
            carry = token >> 63;
            let starts = token & !before;
            // A line's tokens end where the next line's begin: count the
            // tokens that start before each LF (a token still open at this
            // block has its start in `bounds` and not its end).
            let (tokens, mut bits) = (n.div_ceil(2), lf);
            while bits != 0 {
                let below = bits.isolate_lowest_one() - 1;
                let end = tokens + (starts & below).count_ones() as usize;
                // SAFETY: at most 64 line ends per block (see `most`).
                unsafe { *lfs.add(lines) = end as u32 };
                lines += 1;
                bits &= bits - 1;
            }
            // Starts and ends alternate, so one list holds both.
            // SAFETY: at most 64 bounds per block, and 64 more (see `most`).
            n += unsafe { flatten(starts | (!token & before), at as u32, bounds.add(n)) };
            tabs += tab.count_ones() as usize;
        }
        assert!(tabs == lines, "a line from document {first} on lacks a TAB");
        // SAFETY: the first `n` bounds and `lines` line ends were written above.
        unsafe { self.bounds.set_len(n) };
        unsafe { self.lfs.set_len(lines) };
        // Every token ends inside the region, which ends with an LF.
        (n / 2, lines)
    }

    /// Tokenizes `buf[start..end]` (whole lines), looks every token up and
    /// records each document.
    ///
    /// Tokens are looked up `BATCH` at a time: first every token's home
    /// bucket is computed and prefetched, then the tokens are looked up, so
    /// that the cache misses of rare words overlap.
    #[inline(never)]
    fn region(&mut self, buf: &[u8], start: usize, end: usize, out: &mut ChunkOut) {
        let (tokens, lines) = self.lex(buf, start, end, out.first + out.lines);
        let (look, text) = (self.table.lookup(), self.queries);
        if self.vals.len() < tokens {
            self.vals.resize(tokens.next_power_of_two(), 0);
        }
        let bounds = self.bounds[..2 * tokens].as_chunks::<2>().0;
        assert!(end + SHORT <= buf.len()); // `scan` leaves `PAD` bytes after the data
        let mut probes = [Probe::default(); BATCH];
        for (bounds, vals) in bounds.chunks(BATCH).zip(self.vals.chunks_mut(BATCH)) {
            for (probe, &[s, e]) in probes.iter_mut().zip(bounds) {
                // SAFETY: a token starts before `end` (`lex` only finds tokens
                // in `start..end`), and `buf` has 16 bytes after `end`
                // (checked above).
                *probe = unsafe { look.probe(buf.as_ptr().add(s as usize), (e - s) as usize) };
            }
            for ((val, &probe), &[s, e]) in vals.iter_mut().zip(&probes).zip(bounds) {
                *val = look.find(probe, || &buf[s as usize..e as usize], text);
            }
        }
        let mut first = 0;
        for &last in &self.lfs[..lines] {
            let doc = &self.vals[first..last as usize];
            self.doc.finish(doc, out, self.table, self.parsed_ranges);
            first = last as usize;
        }
    }
}

impl Doc {
    /// Records the document whose tokens have the table values `doc`: emits
    /// its distinct BOOL terms and checks the phrases anchored at its tokens
    /// against the token sequence.
    #[inline(never)]
    fn finish(&mut self, doc: &[u32], out: &mut ChunkOut, table: &Table, ranges: &[&Parsed]) {
        assert!(self.local < MAX_SEGMENT_DOCS, "too many documents");
        // The distinct BOOL terms, appended to `fresh`. Branch-free: every
        // id is written, and kept only if its stamp is older than this
        // document's (a BOOL term not seen yet); every id's stamp is raised
        // to this document's (`NEVER` stays).
        self.fresh.reserve(doc.len());
        let (stamps, stamp) = (self.stamps.as_mut_ptr(), self.stamp);
        let (fresh, mut n) = (self.fresh.as_mut_ptr(), self.fresh.len());
        for &v in doc {
            let id = v & ID_MASK;
            // SAFETY: `stamps` has a slot for every id below `Table::ids` (see
            // `Scanner::new`), and every value's id is below it; `fresh` has
            // room for every token (reserved above).
            unsafe {
                let seen = stamps.add(id as usize);
                let old = *seen;
                *seen = old.max(stamp);
                *fresh.add(n) = id;
                n += usize::from(old < stamp);
            }
        }
        // SAFETY: the ids up to `n` were written above.
        unsafe { self.fresh.set_len(n) };
        self.ends.push(n as u32);
        // A new stamp for the next document; when they run out, the stamps of
        // BOOL terms are cleared, so no old stamp can match one reused.
        self.stamp += 1;
        if self.stamp == NEVER {
            let clear = |s: &mut u8| *s &= NEVER * u8::from(*s == NEVER);
            self.stamps.iter_mut().for_each(clear);
            self.stamp = 1;
        }
        let id = out.first + out.lines;
        let found = out.matches.len();
        // The phrase lists of the anchors are cold: the heads of all of them
        // are loaded at once, then the first record of each, before any is
        // read; then each phrase is compared with the tokens around its anchor.
        let record = |link: u32| {
            let records = &ranges[(link >> RANGE_SHIFT) as usize].records;
            &records[(link & ((1 << RANGE_SHIFT) - 1)) as usize..]
        };
        self.links.clear();
        for block in (0..doc.len()).step_by(64) {
            let mut mask = anchors(doc, block);
            while mask != 0 {
                let at = block + mask.trailing_zeros() as usize;
                mask &= mask - 1;
                table.prefetch_head(doc[at] & ID_MASK);
                self.links.push((at as u32, 0));
            }
        }
        for (at, link) in &mut self.links {
            *link = table.head(doc[*at as usize] & ID_MASK);
            if *link != NONE {
                crate::prefetch(record(*link).as_ptr());
            }
        }
        for &(at, mut link) in &self.links {
            let at = at as usize;
            while link != NONE {
                let record = record(link);
                let (anchor, len) = ((record[2] & 0xFF) as usize, (record[2] >> 8) as usize);
                let terms = &record[3..3 + len];
                let same = |(&a, &b): (&u32, &u32)| a == b & ID_MASK;
                let here = at >= anchor
                    && doc.len() - (at - anchor) >= len
                    && terms.iter().zip(&doc[at - anchor..]).all(same);
                let phrase = link >> RANGE_SHIFT << RANGE_SHIFT | record[1];
                if here && !out.matches[found..].iter().any(|m| m.0 == phrase) {
                    out.matches.push((phrase, id));
                }
                link = record[0];
            }
        }
        self.local += 1;
        out.lines += 1;
    }
}

/// Bit j of the result: token `at + j` of `doc` (one of the 64 from `at`)
/// has a term that anchors a phrase (flagged `ANCHOR`, the top bit), read
/// eight ids at a time.
fn anchors(doc: &[u32], at: usize) -> u64 {
    let block = &doc[at..doc.len().min(at + 64)];
    let mut mask = 0;
    for (j, ids) in block.chunks(8).enumerate() {
        let bits = match ids.len() {
            // SAFETY: 8 readable u32s; AVX2 is required by the crate.
            8 => unsafe { _mm256_movemask_ps(_mm256_loadu_ps(ids.as_ptr().cast())) as u64 },
            _ => (0..ids.len()).fold(0, |m, i| m | u64::from(ids[i] >> 31) << i),
        };
        mask |= bits << (8 * j);
    }
    mask
}

/// For the 64 bytes at `p`, the masks of token bytes (ASCII letters and
/// digits), LFs and TABs. A byte is a letter or digit if the classes of its
/// high and low nibble share a bit (`HIGH` and `LOW`): bit 0 for digits,
/// bit 1 for `A`-`O` and `a`-`o`, bit 2 for `P`-`Z` and `p`-`z`.
///
/// SAFETY: 64 bytes from `p` must be readable.
#[inline(always)]
unsafe fn classify(p: *const u8) -> (u64, u64, u64) {
    const HIGH: [u8; 16] = [0, 0, 0, 1, 2, 4, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0];
    const LOW: [u8; 16] = [5, 7, 7, 7, 7, 7, 7, 7, 7, 7, 6, 2, 2, 2, 2, 2];
    let mask = |m| _mm256_movemask_epi8(m) as u32 as u64;
    let half = |v: __m256i| {
        let table = |t: [u8; 16]| _mm256_broadcastsi128_si256(_mm_loadu_si128(t.as_ptr().cast()));
        let nibbles = _mm256_set1_epi8(0x0F);
        let hi = _mm256_and_si256(_mm256_srli_epi16(v, 4), nibbles);
        let high = _mm256_shuffle_epi8(table(HIGH), hi);
        let low = _mm256_shuffle_epi8(table(LOW), _mm256_and_si256(v, nibbles));
        let other = _mm256_cmpeq_epi8(_mm256_and_si256(high, low), _mm256_setzero_si256());
        let lf = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8));
        let tab = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\t' as i8));
        (!mask(other) & 0xFFFF_FFFF, mask(lf), mask(tab))
    };
    // SAFETY: the caller vouches for 64 bytes; loadu has no alignment rule;
    // AVX2 is required by the crate.
    unsafe {
        let (a0, l0, t0) = half(_mm256_loadu_si256(p.cast()));
        let (a1, l1, t1) = half(_mm256_loadu_si256(p.add(32).cast()));
        (a0 | a1 << 32, l0 | l1 << 32, t0 | t1 << 32)
    }
}

/// Bit i of the result is the XOR of bits 0..=i of `x`.
fn prefix_xor(mut x: u64) -> u64 {
    for shift in [1, 2, 4, 8, 16, 32] {
        x ^= x << shift;
    }
    x
}

/// `POSITIONS[b]`: the positions of the set bits of byte `b`, ascending.
static POSITIONS: [[u8; 8]; 256] = {
    let (mut t, mut i) = ([[0; 8]; 256], 0);
    while i < 2048 {
        // Bit `bit` of `b`, set, is the one after the set bits below it.
        let (b, bit) = (i / 8, i % 8);
        if b >> bit & 1 == 1 {
            t[b][(b & ((1 << bit) - 1)).count_ones() as usize] = bit as u8;
        }
        i += 1;
    }
    t
};

/// Writes `base + i` for every set bit i of `bits` to `out`, and returns
/// how many: a byte of `bits` at a time, its bit positions come from
/// `POSITIONS`, widened to eight u32s and stored whole. No branches.
///
/// SAFETY: 64 u32s from `out` must be writable.
#[inline(always)]
unsafe fn flatten(bits: u64, base: u32, out: *mut u32) -> usize {
    // SAFETY: AVX2 is required by the crate; each row of `POSITIONS` is 8
    // readable bytes; the bytes before byte i had at most 8i bits, so the
    // store for byte i ends within 8i + 8 <= 64 u32s.
    unsafe {
        let (mut at, eight) = (_mm256_set1_epi32(base as i32), _mm256_set1_epi32(8));
        let mut k = 0;
        for i in 0..8 {
            let byte = (bits >> (8 * i)) as u8;
            let row = POSITIONS[usize::from(byte)].as_ptr();
            let pos = _mm256_cvtepu8_epi32(_mm_loadl_epi64(row.cast()));
            _mm256_storeu_si256(out.add(k).cast(), _mm256_add_epi32(pos, at));
            at = _mm256_add_epi32(at, eight);
            k += byte.count_ones() as usize;
        }
        k
    }
}

/// The decimal id at the start of a line, up to its TAB.
/// `at` is the line's offset in docs.txt, for the error message.
pub fn parse_id(line: &[u8], at: u64) -> u32 {
    let d = &line[..line.iter().position(|&b| b == b'\t').unwrap_or(0)];
    let id = std::str::from_utf8(d).ok().and_then(|d| d.parse().ok());
    id.unwrap_or_else(|| fail(&format!("no document id at byte {at} of docs.txt")))
}

/// Fills `buf` from `offset`, short only at the end of the file.
pub fn read_full(file: &File, buf: &mut [u8], offset: u64) -> usize {
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => fail(&format!("reading docs.txt: {e}")),
        }
    }
    done
}
