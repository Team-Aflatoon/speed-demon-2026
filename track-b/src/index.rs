//! A throwaway inverted index over one batch of up to 4,096 consecutive
//! documents, holding only query terms.
//!
//! Each worker reuses one `Index`: build it from a batch, answer every query
//! against it, move on. Batches are small, so local document numbers fit in a
//! `u16` and a set of documents fits in a 512-byte bitmap, and the memory the
//! index needs stays a few hundred kilobytes whatever the corpus size.
//!
//! A term is stored one of two ways. Rare terms keep a sorted list of local
//! document numbers. A term that has once been seen in more than `DENSE_MIN`
//! documents of a batch is "dense" for this worker from then on: the scan sets
//! its bit in a bitmap directly, without the duplicate check or the list.

use crate::dict::{prefetch, Dict, Probe};
use crate::lex::{Event, Lexer};
use std::io;

pub const WORDS: usize = 64;
pub const BATCH_DOCS: usize = WORDS * 64;
pub type Bits = [u64; WORDS];

/// A bitmap costs 512 bytes and a word-wise pass per operation; a list costs
/// 2 bytes and one random access per document. Past 64 documents per full
/// batch (one per bitmap word) the bitmap wins on both counts.
const DENSE_MIN: usize = 64;
/// A worker's first batch is small: until it has seen a batch, every term is
/// still a list, and 12 full all-list batches at once would set the memory peak.
const WARMUP_DOCS: usize = 512;
/// A term's state word: `DENSE | bitmap slot`, or else the global id of the last
/// document that recorded it (ids have at most 9 digits, so bit 31 is free).
const DENSE: u32 = 1 << 31;
const NEVER: u32 = DENSE - 1;

/// The documents of the current batch that contain a term.
#[derive(Clone, Copy)]
pub enum Set<'a> {
    Empty,
    List(&'a [u16]),
    Bits(&'a Bits),
}

pub struct Index {
    /// Global id of local document 0.
    pub first_doc: u32,
    /// Byte range of each document's text in the corpus.
    pub docs: Vec<(usize, usize)>,
    /// One bit per term: does it occur in this batch (or is it dense)? Most
    /// query terms are rare and absent from most batches; this 10 KB bitset
    /// answers that from L1 without touching the larger per-term arrays.
    present: Vec<u64>,
    /// Per term: see `DENSE`. One word, so recording a hit costs one access.
    state: Vec<u32>,
    dense_terms: Vec<u32>,
    bitmaps: Vec<Bits>,
    /// The scan's copy of the dense bitmaps, transposed: word `w` of slot `s`
    /// is `scan_bits[w * dense + s]`. A document's dense terms then all write
    /// to one small stretch of memory that stays in L1, instead of to one cache
    /// line per term; `invert` transposes it back once per batch.
    scan_bits: Vec<u64>,
    terms: Vec<u32>,
    /// Sparse (document, term) pairs in document order: `hits[hit_end[d-1]..hit_end[d]]`
    /// are the distinct rare terms of local document `d`.
    hits: Vec<u32>,
    hit_end: Vec<u32>,
    /// The inverted lists: term `t` is in `postings[bounds[t]..bounds[t + 1]]`.
    bounds: Vec<u32>,
    postings: Vec<u16>,
    probes: Vec<Probe>,
}

impl Index {
    pub fn new(terms: usize) -> Index {
        Index {
            first_doc: 0,
            docs: Vec::with_capacity(BATCH_DOCS),
            present: vec![0; terms.div_ceil(64)],
            state: vec![NEVER; terms],
            dense_terms: Vec::new(),
            bitmaps: Vec::new(),
            scan_bits: Vec::new(),
            terms: Vec::new(),
            hits: Vec::new(),
            hit_end: Vec::new(),
            bounds: vec![0; terms + 2],
            postings: Vec::new(),
            probes: Vec::new(),
        }
    }

    /// Indexes the lines of `data[pos..end]` (both line starts) until `end` or
    /// a full batch, and returns where it stopped.
    pub fn build(&mut self, data: &[u8], pos: usize, end: usize, dict: &Dict) -> io::Result<usize> {
        let limit = if self.hit_end.capacity() == 0 {
            WARMUP_DOCS
        } else {
            BATCH_DOCS
        };
        self.docs.clear();
        self.hits.clear();
        self.hit_end.clear();
        self.scan_bits.clear();
        self.scan_bits.resize(WORDS * self.bitmaps.len(), 0);
        let mut lexer = Lexer::new(data, pos, end);
        let mut line = pos;
        while line < end && self.docs.len() < limit {
            // A line is its id (the first token, right before a TAB), the text, an LF.
            let (id, text) = match lexer.next() {
                Some(Event::Token(s, e)) if s == line && data[e] == b'\t' => {
                    (parse_id(&data[s..e])?, e + 1)
                }
                _ => return Err(bad_corpus("a line does not start with an id and a TAB")),
            };
            let local = self.docs.len();
            if local == 0 {
                self.first_doc = id;
            } else if id != self.first_doc + local as u32 {
                return Err(bad_corpus("document ids are not dense and ascending"));
            }
            let lf = self.scan(&mut lexer, data, local, id, dict)?;
            self.docs.push((text, lf));
            self.hit_end.push(self.hits.len() as u32);
            line = lf + 1;
        }
        self.invert();
        Ok(line)
    }

    /// Records the query terms of one document's text; returns the LF ending it.
    ///
    /// Each pass prefetches what the next one reads: the first hashes every
    /// token and prefetches its table slot, the second resolves the tokens to
    /// terms and prefetches their state words, the third records them. A
    /// document's lookups (about 150) then wait on memory together instead of
    /// one after another.
    fn scan(
        &mut self,
        lexer: &mut Lexer,
        data: &[u8],
        local: usize,
        id: u32,
        dict: &Dict,
    ) -> io::Result<usize> {
        let mut probes = std::mem::take(&mut self.probes);
        probes.clear();
        let lf = loop {
            match lexer.next() {
                Some(Event::Token(start, end)) => probes.extend(dict.probe(data, start, end)),
                Some(Event::LineEnd(lf)) => break lf,
                None => return Err(bad_corpus("a line has no LF")),
            }
        };
        self.terms.clear();
        for probe in &probes {
            if let Some(term) = dict.resolve(probe, data) {
                prefetch(&self.state[term as usize]);
                self.terms.push(term);
            }
        }
        for i in 0..self.terms.len() {
            self.record(self.terms[i] as usize, local, id);
        }
        self.probes = probes;
        Ok(lf)
    }

    #[inline]
    fn record(&mut self, term: usize, local: usize, id: u32) {
        let state = self.state[term];
        if state & DENSE != 0 {
            let dense = self.bitmaps.len();
            self.scan_bits[local / 64 * dense + (state ^ DENSE) as usize] |= 1 << (local % 64);
        } else if state != id {
            self.state[term] = id;
            self.hits.push(term as u32);
        }
    }

    /// Counting sort of the sparse hits into per-term lists, then promotion of
    /// the terms whose list came out long to bitmaps.
    fn invert(&mut self) {
        let dense = self.bitmaps.len();
        for (slot, bits) in self.bitmaps.iter_mut().enumerate() {
            bits.iter_mut()
                .enumerate()
                .for_each(|(w, word)| *word = self.scan_bits[w * dense + slot]);
        }
        self.bounds.fill(0);
        self.present.fill(0);
        let mut heavy = Vec::new();
        let dense_min = (DENSE_MIN * self.docs.len() / BATCH_DOCS) as u32;
        // Count into `bounds[t + 2]` so that after the prefix sum `bounds[t + 1]`
        // is where term t starts; the scatter uses it as t's cursor, which leaves
        // it at t's end, so `bounds[t]..bounds[t + 1]` is the list of t.
        for &t in &self.hits {
            let count = &mut self.bounds[t as usize + 2];
            *count += 1;
            if *count == 1 {
                self.present[t as usize / 64] |= 1 << (t % 64);
            } else if *count == dense_min + 1 {
                heavy.push(t);
            }
        }
        for i in 2..self.bounds.len() {
            self.bounds[i] += self.bounds[i - 1];
        }
        self.postings.resize(self.hits.len(), 0);
        let mut start = 0;
        for (local, &end) in self.hit_end.iter().enumerate() {
            for &t in &self.hits[start..end as usize] {
                let cursor = &mut self.bounds[t as usize + 1];
                self.postings[*cursor as usize] = local as u16;
                *cursor += 1;
            }
            start = end as usize;
        }
        for t in heavy {
            let (lo, hi) = (
                self.bounds[t as usize] as usize,
                self.bounds[t as usize + 1] as usize,
            );
            let mut bits = [0; WORDS];
            self.postings[lo..hi]
                .iter()
                .for_each(|&d| bits[d as usize / 64] |= 1 << (d % 64));
            self.state[t as usize] = DENSE | self.bitmaps.len() as u32;
            self.bitmaps.push(bits);
            self.dense_terms.push(t);
        }
        for &t in &self.dense_terms {
            self.present[t as usize / 64] |= 1 << (t % 64);
        }
    }

    #[inline]
    pub fn set(&self, term: u32) -> Set<'_> {
        let t = term as usize;
        if self.present[t / 64] & (1 << (t % 64)) == 0 {
            return Set::Empty;
        }
        match self.state[t] {
            state if state & DENSE != 0 => Set::Bits(&self.bitmaps[(state ^ DENSE) as usize]),
            _ => Set::List(&self.postings[self.bounds[t] as usize..self.bounds[t + 1] as usize]),
        }
    }
}

/// A document id: 1 to 9 decimal digits.
fn parse_id(digits: &[u8]) -> io::Result<u32> {
    if digits.len() > 9 || !digits.iter().all(u8::is_ascii_digit) {
        return Err(bad_corpus("a document id is not a number"));
    }
    Ok(digits
        .iter()
        .fold(0, |id, &d| id * 10 + u32::from(d - b'0')))
}

pub fn bad_corpus(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("docs.txt: {msg}"))
}
