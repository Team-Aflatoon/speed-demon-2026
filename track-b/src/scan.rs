//! The corpus scan. A chunk of `docs.txt` is read with `pread` into a reused
//! buffer, tokenized 64 bytes at a time with AVX2, and every token is looked up
//! in the query term table. For each document the scan emits its distinct BOOL
//! terms (as postings, split by partition for the build) and checks the
//! phrases anchored at its tokens, so phrase answers need no index at all.

use crate::fail;
use crate::index::PARTS;
use crate::query::{Lookup, Parsed, Probe, Table, BOOL_TERM, ID_MASK, NONE, RANGE_SHIFT, SHORT};
use std::arch::x86_64::*;
use std::fs::File;
use std::os::unix::fs::FileExt;

#[cfg(not(target_feature = "avx2"))]
compile_error!("track-b needs AVX2: build with -C target-cpu=x86-64-v3 (see .cargo/config.toml)");

/// Bytes per `pread`. Reading into a small reused buffer measured 15% faster
/// than `mmap` on the scoring box (Track A), whose page-fault path is slow.
const PIECE: usize = 128 << 10;
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
    /// `streams[p]`: `doc << 16 | id / PARTS` for every distinct BOOL term
    /// with `id % PARTS == p` of every document, in document order, where
    /// `doc` is numbered within the segment.
    pub streams: Vec<Vec<u32>>,
    /// `(phrase, document id)` of every phrase match, in document order.
    pub matches: Vec<(u32, u32)>,
    /// First document id of the chunk and its number of documents.
    pub first: u32,
    pub lines: u32,
}

/// Per-thread scratch, reused for every chunk.
pub struct Scanner<'a> {
    table: &'a Table,
    queries: &'a [u8],
    parsed_ranges: &'a [&'a Parsed],
    buf: Vec<u8>,
    /// Per token of the region being scanned: its start and end (one past)
    /// at `bounds[2k]` and `bounds[2k + 1]`.
    bounds: Vec<u32>,
    /// Per line of the region: one past its last token.
    lfs: Vec<u32>,
    doc: Doc,
}

/// A run of tokens of one document: their probes, their
/// bounds (start, end) in the buffer, and the region index of the first.
struct Run<'r> {
    probes: &'r [Probe],
    bounds: &'r [u32],
    first: usize,
}

/// The document being scanned: number `local` in its segment, whose first
/// token is token `first` of the region. The arrays are indexed by token
/// within the document and grow to its length (see `Doc::record`).
#[derive(Default)]
struct Doc {
    local: u32,
    first: usize,
    /// The term id of each token so far (0 for no query term).
    ids: Vec<u32>,
    /// The distinct BOOL terms so far: `fresh[..n_fresh]`.
    fresh: Vec<u32>,
    /// Invariant: at most the document's token count, and 0 between documents.
    n_fresh: usize,
    /// Tokens that anchor phrases: `hits[..n_hits]`, likewise.
    hits: Vec<u32>,
    n_hits: usize,
    /// Bitset over term ids: BOOL terms already in `fresh`.
    seen: Vec<u64>,
}

impl<'a> Scanner<'a> {
    pub fn new(
        table: &'a Table,
        queries: &'a [u8],
        parsed_ranges: &'a [&'a Parsed],
    ) -> Scanner<'a> {
        let seen = vec![0; table.ids().div_ceil(64)]; // a bit per id (see `Doc::record`)
        let doc = Doc {
            seen,
            ..Doc::default()
        };
        let (buf, bounds, lfs) = Default::default();
        Scanner {
            table,
            queries,
            parsed_ranges,
            buf,
            bounds,
            lfs,
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
        out.streams.resize_with(PARTS, Vec::new);
        out.streams.iter_mut().for_each(Vec::clear);
        out.matches.clear();
        out.lines = 0;
        // buf[..len] holds the file from `off`. A chunk that does not start the
        // file reads from `from - 1` and drops everything up to the first LF:
        // that is the end of the previous chunk's last line.
        let (mut off, mut len, mut skip) = (from.saturating_sub(1), 0, from > 0);
        let mut known = false; // whether `doc.local` is set
        let mut buf = std::mem::take(&mut self.buf); // put back on return
        loop {
            if buf.len() < len + PIECE + PAD {
                buf.resize(len + PIECE + PAD, 0);
            }
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
            let last_lf = match last < len {
                true => buf[last..len].iter().position(|&b| b == b'\n'),
                false => None,
            };
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
                    let at = off + start as u64;
                    out.first = parse_id(&buf[start..end], at);
                    let local = out.first.checked_sub(first);
                    let id = out.first;
                    let ascending =
                        || fail(&format!("document id {id} at byte {at} is out of order"));
                    self.doc.local = local.unwrap_or_else(ascending);
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
    }

    /// Finds the tokens and LFs of `buf[start..end]` (whole lines, the first
    /// of them document `first`); returns how many of each.
    fn lex(&mut self, buf: &[u8], start: usize, end: usize, first: u32) -> (usize, usize) {
        let (mut bounds, mut lines, mut tabs) = (0, 0, 0);
        // `in_id` is all ones while inside a line's "id TAB" prefix: every line
        // starts there, an LF enters it and the TAB leaves it, so a prefix XOR
        // of the LF and TAB bits marks it and the ids never become tokens.
        let (mut in_id, mut carry) = (!0u64, 0u64);
        for at in (start..end).step_by(64) {
            // `flatten` writes up to 64 positions past the count.
            if self.bounds.len() < bounds + 64 {
                self.bounds.resize(2 * self.bounds.len() + 8192, 0);
            }
            let (alnum, lf, tab) = classify(&buf[at..at + 64]);
            let live = u64::MAX >> (64 - (end - at).min(64));
            let (lf, tab) = (lf & live, tab & live);
            // Most blocks hold no LF or TAB, and stay all in or all out.
            let id = if lf | tab == 0 {
                in_id
            } else {
                prefix_xor(lf | tab) ^ in_id
            };
            in_id = ((id as i64) >> 63) as u64;
            let token = alnum & live & !id;
            let before = token << 1 | carry;
            carry = token >> 63;
            let starts = token & !before;
            if lf != 0 {
                if self.lfs.len() < lines + 64 {
                    self.lfs.resize(2 * self.lfs.len() + 256, 0);
                }
                // A line's tokens end where the next line's begin: count the
                // tokens that start before each LF (a token still open at
                // this block has its start in `bounds` and not its end).
                let (tokens, mut bits) = (bounds.div_ceil(2), lf);
                while bits != 0 {
                    let below = bits.isolate_lowest_one() - 1;
                    self.lfs[lines] = (tokens + (starts & below).count_ones() as usize) as u32;
                    lines += 1;
                    bits &= bits - 1;
                }
            }
            // Starts and ends alternate, so one list holds both.
            flatten(
                starts | (!token & before),
                at as u32,
                &mut self.bounds,
                &mut bounds,
            );
            tabs += tab.count_ones() as usize;
        }
        if tabs != lines {
            let at = format!("the {lines} lines from document {first}");
            fail(&format!(
                "every line of docs.txt must hold exactly one TAB ({at})"
            ));
        }
        // Every token ends inside the region, which ends with an LF.
        (bounds / 2, lines)
    }

    /// Tokenizes `buf[start..end]` (whole lines) and records each document.
    ///
    /// Tokens go in batches of `BATCH`: first every token's home bucket is
    /// computed and prefetched, then the tokens are looked up and recorded in
    /// runs that stop at the end of a batch or of a document.
    fn region(&mut self, buf: &[u8], start: usize, end: usize, out: &mut ChunkOut) {
        let (tokens, lines) = self.lex(buf, start, end, out.first + out.lines);
        let (table, queries, ranges) = (self.table, self.queries, self.parsed_ranges);
        let (doc, line_ends) = (&mut self.doc, &self.lfs[..lines]);
        let bounds = &self.bounds[..2 * tokens];
        doc.first = 0; // token indexes restart with each region
        let mut probes = [Probe::default(); BATCH];
        let mut line = 0;
        let look = table.lookup();
        assert!(end + SHORT <= buf.len()); // `scan` leaves `PAD` bytes after the data
        for a in (0..tokens).step_by(BATCH) {
            let z = (a + BATCH).min(tokens);
            // Every token's probe (its key and home bucket), whose bucket
            // starts loading now.
            for (probe, &[s, e]) in probes
                .iter_mut()
                .zip(bounds[2 * a..2 * z].as_chunks::<2>().0)
            {
                let (s, e) = (s as usize, e as usize);
                debug_assert!(s < end);
                // SAFETY: a token starts before `end` (`lex` only finds tokens
                // in `start..end`), and `buf` has 16 bytes after `end`
                // (checked above).
                *probe = unsafe { look.probe(buf.as_ptr().add(s), e - s) };
            }
            let mut k = a;
            loop {
                while line < lines && line_ends[line] as usize == k {
                    doc.finish(k, out, table, ranges);
                    line += 1;
                }
                if k == z {
                    break;
                }
                // `line < lines`: the last line ends at `tokens`, after `k`.
                let stop = z.min(line_ends[line] as usize);
                let run = Run {
                    probes: &probes[k - a..stop - a],
                    bounds: &bounds[2 * k..2 * stop],
                    first: k,
                };
                doc.record(run, look, buf, queries);
                k = stop;
            }
        }
        while line < lines {
            doc.finish(tokens, out, table, ranges);
            line += 1;
        }
    }
}

impl Doc {
    /// Looks up the tokens of `run` (all in this document), writes their ids
    /// to `ids` and records them. Branch-free apart from long tokens and
    /// overflowed buckets: every id is written to `fresh`, and kept only if it
    /// is a BOOL term not seen in this document; every token is written to
    /// `hits`, and kept if it anchors a phrase. Bits set in `seen` for other
    /// terms (and for id 0) are never cleared, which is harmless: they are
    /// never BOOL terms.
    #[inline(always)]
    fn record(&mut self, run: Run, look: Lookup, buf: &[u8], text: &[u8]) {
        let (mut n_fresh, mut n_hits) = (self.n_fresh, self.n_hits);
        // The run's tokens are tokens `at..to` of the document; `n_fresh`
        // and `n_hits` are at most `at`.
        let (at, to) = (
            run.first - self.first,
            run.first - self.first + run.probes.len(),
        );
        if self.ids.len() < to {
            let n = to.next_power_of_two().max(1024);
            for v in [&mut self.ids, &mut self.fresh, &mut self.hits] {
                v.resize(n, 0);
            }
        }
        // Unchecked indexing below, because this runs for every token: `seen`
        // has a bit for every id below `Table::ids` (see `Scanner::new`), and
        // every value's id is below it; `fresh` and `hits` have room for the
        // run (checked here).
        assert!(n_fresh.max(n_hits) + run.probes.len() <= self.fresh.len().min(self.hits.len()));
        let (seen, fresh, hits) = (
            self.seen.as_mut_ptr(),
            self.fresh.as_mut_ptr(),
            self.hits.as_mut_ptr(),
        );
        let ids = &mut self.ids[at..to];
        for (k, (&probe, id_out)) in (at..to).zip(run.probes.iter().zip(ids)) {
            let v = if !probe.long() {
                look.find_short(probe)
            } else {
                let i = 2 * (k - at);
                let (s, e) = (run.bounds[i] as usize, run.bounds[i + 1] as usize);
                look.find_long(&buf[s..e], text)
            };
            let id = v & ID_MASK;
            *id_out = id;
            let (word, bit) = (id as usize / 64, 1u64 << (id % 64));
            // SAFETY: see above.
            unsafe {
                let seen = seen.add(word);
                let new = (*seen & bit == 0) & (v & BOOL_TERM != 0);
                *seen |= bit;
                *fresh.add(n_fresh) = id;
                n_fresh += usize::from(new);
                *hits.add(n_hits) = k as u32;
                n_hits += (v >> 31) as usize;
            }
        }
        (self.n_fresh, self.n_hits) = (n_fresh, n_hits);
    }

    /// Ends the document before token `end` of the region: emits its BOOL
    /// terms and checks the phrases anchored at its tokens against the
    /// token sequence.
    fn finish(&mut self, end: usize, out: &mut ChunkOut, table: &Table, ranges: &[&Parsed]) {
        if self.local >= MAX_SEGMENT_DOCS {
            let id = out.first + out.lines;
            fail(&format!(
                "too many documents in one segment, at document id {id}"
            ));
        }
        let streams = &mut out.streams[..PARTS];
        assert!(self.n_fresh <= self.fresh.len());
        for &term in &self.fresh[..self.n_fresh] {
            streams[term as usize % PARTS].push((self.local << 16) | (term / PARTS as u32));
            // SAFETY: `seen` has a bit for every id (see `record`).
            unsafe { *self.seen.get_unchecked_mut(term as usize / 64) &= !(1 << (term % 64)) };
        }
        self.n_fresh = 0;
        let doc = &self.ids[..end - self.first];
        let id = out.first + out.lines;
        let found = out.matches.len();
        for &at in &self.hits[..self.n_hits] {
            let at = at as usize;
            let mut phrase = table.head(doc[at]);
            while phrase != NONE {
                let p = ranges[(phrase >> RANGE_SHIFT) as usize];
                let i = (phrase & ((1 << RANGE_SHIFT) - 1)) as usize;
                let (terms, anchor) = (p.phrase(i), usize::from(p.anchor[i]));
                let here = at >= anchor
                    && doc.len() - (at - anchor) >= terms.len()
                    && terms.iter().zip(&doc[at - anchor..]).all(|(a, b)| a == b);
                if here && !out.matches[found..].iter().any(|m| m.0 == phrase) {
                    out.matches.push((phrase, id));
                }
                phrase = p.next[i];
            }
        }
        self.n_hits = 0;
        self.first = end;
        self.local += 1;
        out.lines += 1;
    }
}

/// For 64 bytes, the masks of token bytes (ASCII letters and digits), LFs and TABs.
#[inline]
fn classify(bytes: &[u8]) -> (u64, u64, u64) {
    let bytes: &[u8; 64] = bytes[..64].try_into().unwrap();
    // SAFETY: AVX2 is enabled for the whole crate (see the compile_error above).
    let ((a0, l0, t0), (a1, l1, t1)) =
        unsafe { (classify32(&bytes[..32]), classify32(&bytes[32..])) };
    (a0 | a1 << 32, l0 | l1 << 32, t0 | t1 << 32)
}

#[target_feature(enable = "avx2")]
fn classify32(bytes: &[u8]) -> (u64, u64, u64) {
    assert!(bytes.len() >= 32);
    // SAFETY: the 32 bytes are readable (checked above); loadu has no alignment rule.
    let v = unsafe { _mm256_loadu_si256(bytes.as_ptr().cast()) };
    let letter = in_range(_mm256_or_si256(v, _mm256_set1_epi8(0x20)), b'a', 25);
    let alnum = _mm256_or_si256(letter, in_range(v, b'0', 9));
    let lf = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8));
    let tab = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\t' as i8));
    let mask = |m| _mm256_movemask_epi8(m) as u32 as u64;
    (mask(alnum), mask(lf), mask(tab))
}

/// Bytes in `lo..=lo + n`: subtract `lo`, then an unsigned `<= n` done with `min`.
#[target_feature(enable = "avx2")]
fn in_range(v: __m256i, lo: u8, n: u8) -> __m256i {
    let d = _mm256_sub_epi8(v, _mm256_set1_epi8(lo as i8));
    _mm256_cmpeq_epi8(_mm256_min_epu8(d, _mm256_set1_epi8(n as i8)), d)
}

/// Bit i of the result is the XOR of bits 0..=i of `x`.
#[inline]
fn prefix_xor(mut x: u64) -> u64 {
    for shift in [1, 2, 4, 8, 16, 32] {
        x ^= x << shift;
    }
    x
}

/// Appends `base + i` for every set bit i of `bits` at `out[*n..]`. 24
/// positions are written unconditionally (64 bytes hold about nine tokens,
/// so 18 bounds), so few blocks take the loop; `out` has room for 64 past `*n`.
#[inline]
fn flatten(mut bits: u64, base: u32, out: &mut [u32], n: &mut usize) {
    let count = bits.count_ones() as usize;
    let out = &mut out[*n..*n + 64];
    for slot in &mut out[..24] {
        *slot = base + bits.trailing_zeros();
        bits &= bits.wrapping_sub(1);
    }
    let mut i = 24;
    while bits != 0 {
        out[i] = base + bits.trailing_zeros();
        bits &= bits - 1;
        i += 1;
    }
    *n += count;
}

/// The decimal id at the start of a line, up to its TAB.
/// `at` is the line's offset in docs.txt, for the error message.
pub fn parse_id(line: &[u8], at: u64) -> u32 {
    let digits = line.iter().position(|&b| b == b'\t').unwrap_or(0);
    let id = std::str::from_utf8(&line[..digits]).map(str::parse);
    match id {
        Ok(Ok(id)) => id,
        _ => fail(&format!(
            "the line at byte {at} of docs.txt does not start with a document id"
        )),
    }
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
