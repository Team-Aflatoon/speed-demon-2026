//! The corpus scan. A chunk of `docs.txt` is read with `pread` into a reused
//! buffer, tokenized 64 bytes at a time with AVX2, and every token is looked up
//! in the query term table. For each document the scan emits its distinct BOOL
//! terms (as postings, split by partition for the build) and checks the
//! phrases anchored at its tokens, so phrase answers need no index at all.

use crate::fail;
use crate::index::PARTS;
use crate::query::{Key, Parsed, Table, ANCHOR, BOOL_TERM, RANGE_SHIFT};
use std::arch::x86_64::*;
use std::fs::File;
use std::os::unix::fs::FileExt;

#[cfg(not(target_feature = "avx2"))]
compile_error!("track-b needs AVX2: build with -C target-cpu=x86-64-v3 (see .cargo/config.toml)");

/// Bytes per `pread`. Reading into a small reused buffer measured 15% faster
/// than `mmap` on the scoring box (Track A), whose page-fault path is slow.
const PIECE: usize = 128 << 10;
/// Slack after the data, so 64-byte vector loads and 12-byte key loads stay
/// inside the buffer; whatever they read past the data is masked off.
const PAD: usize = 64;
/// Tokens hashed, with their slots prefetched, before the first is resolved,
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
    phrases: &'a [&'a Parsed],
    buf: Vec<u8>,
    /// Per token of the region being scanned: start, end (one past) and term id.
    starts: Vec<u32>,
    ends: Vec<u32>,
    ids: Vec<u32>,
    /// LF positions of the region.
    lfs: Vec<u32>,
    doc: Doc,
}

/// The document being scanned: number `local` in its segment, and the run of
/// the region's tokens from `first`.
#[derive(Default)]
struct Doc {
    local: u32,
    first: usize,
    /// The distinct BOOL terms so far: `fresh[..n_fresh]` (sized like `ids`).
    fresh: Vec<u32>,
    n_fresh: usize,
    /// Tokens (region indexes) that anchor phrases.
    hits: Vec<u32>,
    /// Bitset over term ids: BOOL terms already in `fresh`.
    seen: Vec<u64>,
}

impl<'a> Scanner<'a> {
    pub fn new(table: &'a Table, queries: &'a [u8], phrases: &'a [&'a Parsed]) -> Scanner<'a> {
        let seen = vec![0; table.ids() / 64 + 1]; // one more id: `Table::miss`
        let doc = Doc {
            seen,
            ..Doc::default()
        };
        let (buf, starts, ends, ids, lfs) = Default::default();
        Scanner {
            table,
            queries,
            phrases,
            buf,
            starts,
            ends,
            ids,
            lfs,
            doc,
        }
    }

    /// Scans the lines that start in `docs.txt[from..to]`, numbering
    /// documents from `first` (the id of the segment's first line).
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
        loop {
            if self.buf.len() < len + PIECE + PAD {
                self.buf.resize(len + PIECE + PAD, 0);
            }
            let n = read_full(file, &mut self.buf[len..len + PIECE], off + len as u64);
            len += n;
            let mut start = 0;
            if skip {
                match self.buf[..len].iter().position(|&b| b == b'\n') {
                    Some(i) => (start, skip) = (i + 1, false),
                    None if n == 0 => return,
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
                true => self.buf[last..len].iter().position(|&b| b == b'\n'),
                false => None,
            };
            let (end, done) = if off + start as u64 >= to {
                (start, true)
            } else if let Some(i) = last_lf {
                (last + i + 1, true)
            } else {
                let lf = self.buf[start..len].iter().rposition(|&b| b == b'\n');
                (lf.map_or(start, |i| start + i + 1), n == 0)
            };
            if end > start {
                if !known {
                    out.first = parse_id(&self.buf[start..end]);
                    let local = out.first.checked_sub(first);
                    self.doc.local =
                        local.unwrap_or_else(|| fail("document ids are not ascending"));
                    known = true;
                }
                self.region(start, end, out);
            }
            if done {
                return;
            }
            self.buf.copy_within(end..len, 0);
            (off, len) = (off + end as u64, len - end);
        }
    }

    /// Finds the tokens and LFs of `buf[start..end]` (whole lines); returns
    /// how many of each.
    fn lex(&mut self, start: usize, end: usize) -> (usize, usize) {
        let (mut tokens, mut token_ends, mut lines, mut tabs) = (0, 0, 0, 0);
        // `in_id` is all ones while inside a line's "id TAB" prefix: every line
        // starts there, an LF enters it and the TAB leaves it, so a prefix XOR
        // of the LF and TAB bits marks it and the ids never become tokens.
        let (mut in_id, mut carry) = (!0u64, 0u64);
        for at in (start..end).step_by(64) {
            // `flatten` writes up to 64 positions past the count.
            if self.starts.len() < tokens + 64 {
                let n = 2 * self.starts.len() + 4096;
                let doc = &mut self.doc;
                for v in [
                    &mut self.starts,
                    &mut self.ends,
                    &mut self.ids,
                    &mut doc.fresh,
                ] {
                    v.resize(n, 0);
                }
            }
            let (alnum, lf, tab) = classify(&self.buf[at..at + 64]);
            let live = u64::MAX >> (64 - (end - at).min(64));
            let (lf, tab) = (lf & live, tab & live);
            let id = prefix_xor(lf | tab) ^ in_id;
            in_id = ((id as i64) >> 63) as u64;
            let token = alnum & live & !id;
            let before = token << 1 | carry;
            carry = token >> 63;
            flatten(token & !before, at as u32, &mut self.starts, &mut tokens);
            flatten(!token & before, at as u32, &mut self.ends, &mut token_ends);
            if lf != 0 {
                if self.lfs.len() < lines + 64 {
                    self.lfs.resize(2 * self.lfs.len() + 256, 0);
                }
                flatten(lf, at as u32, &mut self.lfs, &mut lines);
            }
            tabs += tab.count_ones() as usize;
        }
        if tabs != lines {
            fail("every line of docs.txt must hold exactly one TAB");
        }
        (tokens, lines)
    }

    /// Tokenizes `buf[start..end]` (whole lines) and records each document.
    fn region(&mut self, start: usize, end: usize, out: &mut ChunkOut) {
        let (tokens, lines) = self.lex(start, end);
        let (table, queries, phrases) = (self.table, self.queries, self.phrases);
        let (buf, doc, lfs) = (&self.buf[..], &mut self.doc, &self.lfs[..lines]);
        let (starts, ends, ids) = (
            &self.starts[..tokens],
            &self.ends[..tokens],
            &mut self.ids[..tokens],
        );
        doc.first = 0; // token indexes restart with each region
        let miss = table.miss();
        let mut keys = [(Key { lo: 0, w2: 0 }, 0); BATCH];
        let mut line = 0;
        let mut next_lf = lfs.first().copied().unwrap_or(u32::MAX);
        for (batch, (b_starts, b_ends)) in starts.chunks(BATCH).zip(ends.chunks(BATCH)).enumerate()
        {
            for ((key, &s), &e) in keys.iter_mut().zip(b_starts).zip(b_ends) {
                let k = Key::new(buf, s as usize, (e - s) as usize);
                let slot = table.slot(k);
                table.prefetch(slot);
                *key = (k, slot);
            }
            for (j, ((&(key, slot), &s), &e)) in keys.iter().zip(b_starts).zip(b_ends).enumerate() {
                let k = batch * BATCH + j;
                while s > next_lf {
                    doc.finish(&ids[..k], out, table, phrases);
                    line += 1;
                    next_lf = lfs.get(line).copied().unwrap_or(u32::MAX);
                }
                let w2 = table
                    .find(key, slot, &buf[s as usize..e as usize], queries)
                    .unwrap_or(miss);
                ids[k] = Table::id(w2);
                doc.token(k, w2);
            }
        }
        for _ in line..lines {
            doc.finish(ids, out, table, phrases);
        }
    }
}

impl Doc {
    /// Records token `k` with slot word `w2`. Branch-free apart from anchors:
    /// the id is always written to `fresh`, and kept only if it is a BOOL
    /// term not seen in this document. Bits set for other terms (and for
    /// `Table::miss`) are never cleared, which is harmless: they are never BOOL terms.
    #[inline]
    fn token(&mut self, k: usize, w2: u64) {
        let id = Table::id(w2);
        let (word, bit) = (id as usize / 64, 1u64 << (id % 64));
        let fresh = (self.seen[word] & bit == 0) & (w2 & BOOL_TERM != 0);
        self.seen[word] |= bit;
        self.fresh[self.n_fresh] = id;
        self.n_fresh += usize::from(fresh);
        if w2 & ANCHOR != 0 {
            self.hits.push(k as u32);
        }
    }

    /// Ends the document, whose tokens end where `ids` ends: emits its
    /// BOOL terms and checks the phrases anchored at its tokens against the
    /// token sequence.
    fn finish(&mut self, ids: &[u32], out: &mut ChunkOut, table: &Table, phrases: &[&Parsed]) {
        if self.local >= MAX_SEGMENT_DOCS {
            fail("a segment of docs.txt holds more than 65,536 documents");
        }
        for &term in &self.fresh[..self.n_fresh] {
            out.streams[term as usize % PARTS].push((self.local << 16) | (term / PARTS as u32));
            self.seen[term as usize / 64] &= !(1 << (term % 64));
        }
        self.n_fresh = 0;
        let doc = &ids[self.first..];
        let id = out.first + out.lines;
        let found = out.matches.len();
        for &at in &self.hits {
            let at = at as usize - self.first;
            let mut phrase = table.head(doc[at]);
            while phrase != crate::query::NONE {
                let p = phrases[(phrase >> RANGE_SHIFT) as usize];
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
        self.hits.clear();
        self.first = ids.len();
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

/// Appends `base + i` for every set bit i of `bits` at `out[*n..]`. Eight
/// positions are written unconditionally (64 bytes hold about nine tokens),
/// so only longer runs take a loop; `out` has room for 64 past `*n`.
#[inline]
fn flatten(mut bits: u64, base: u32, out: &mut [u32], n: &mut usize) {
    let count = bits.count_ones() as usize;
    let out = &mut out[*n..*n + 64];
    for slot in &mut out[..8] {
        *slot = base + bits.trailing_zeros();
        bits &= bits.wrapping_sub(1);
    }
    let mut i = 8;
    while bits != 0 {
        out[i] = base + bits.trailing_zeros();
        bits &= bits - 1;
        i += 1;
    }
    *n += count;
}

/// The decimal id at the start of a line, up to its TAB.
pub fn parse_id(line: &[u8]) -> u32 {
    let digits = line.iter().position(|&b| b == b'\t').unwrap_or(0);
    let id = std::str::from_utf8(&line[..digits])
        .ok()
        .and_then(|s| s.parse().ok());
    id.unwrap_or_else(|| fail("a line of docs.txt does not start with a document id"))
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
