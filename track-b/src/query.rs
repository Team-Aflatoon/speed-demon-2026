//! The query side: `queries.txt` parsed in parallel, every distinct term put
//! into one lock-free hash table that the corpus scan probes per token, and
//! each phrase given an anchor term at which the scan will check it.

use crate::fail;
use std::ops::Range;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};
use std::sync::{Barrier, OnceLock};

pub const NONE: u32 = u32::MAX;
/// Slot flags: the term is in a BOOL query (the scan records its documents),
/// and the term anchors at least one phrase (the scan checks phrases there).
pub const BOOL_TERM: u64 = 1 << 62;
pub const ANCHOR: u64 = 1 << 63;
/// Slot flag: from segment 2 on, the scan sets this term's bits directly
/// instead of writing postings for it (see `Table::make_hot`).
pub const HOT: u64 = 1 << 61;
/// At most this many hot terms (the width of the scan's rows of hot bits).
const MAX_HOT: usize = 512;
/// Query operators, kept in the top two bits of a BOOL step.
pub const AND: u32 = 0;
pub const OR: u32 = 1;
pub const NOT: u32 = 2;
/// A BOOL step is `op << TERM_BITS | term`: the term id in bits 0..30, the
/// operator in bits 30 and 31.
pub const TERM_BITS: u32 = 30;
/// `order` entries with this bit are phrases.
pub const PHRASE: u32 = 1 << 31;
/// A slot holds the first 12 bytes of its term inline; longer terms are
/// confirmed against their text in `queries.txt`.
const INLINE: usize = 12;
/// `w2` bits 0..38 are the key (12 bytes inline + 6-bit length), 38..61 the
/// term id, 61 to 63 the flags.
const KEY_BITS: u64 = (1 << 38) - 1;
const ID_SHIFT: u32 = 38;
const ID_MASK: u32 = (1 << 23) - 1;
/// Term offsets in `queries.txt` are kept as u32.
pub const MAX_QUERY_BYTES: usize = 1 << 30;
/// Zero bytes after `queries.txt`, so a key can read 12 bytes from any term.
pub const QUERY_PAD: usize = 16;
/// Query frequencies only need to tell rare from common, so they saturate
/// here; 12 racing increments past the check cannot wrap a u8.
const FREQ_CAP: u8 = 200;

/// A token's identity: its first 8 bytes, then bytes 8..12 and its length.
/// Bytes are lowercased with `| 0x20`, exact for letters and digits (digits
/// already have bit 5 set), and zero padded, so `lo` and `w2` are unique for
/// tokens up to 12 bytes.
#[derive(Clone, Copy)]
pub struct Key {
    pub lo: u64,
    pub w2: u64,
}

impl Key {
    /// The token `bytes[at..at + len]`; 12 bytes from `at` must be readable
    /// (anything after the token is masked off).
    #[inline]
    pub fn new(bytes: &[u8], at: usize, len: usize) -> Key {
        let lo = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) | 0x2020_2020_2020_2020;
        let hi = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()) | 0x2020_2020;
        let hi_mask = (1u64 << (8 * (len.clamp(8, 12) - 8))) - 1;
        Key {
            lo: lo & (u64::MAX >> (64 - 8 * len.min(8))),
            // 63 is longer than any term, so a longer token never matches.
            w2: (u64::from(hi) & hi_mask) | (len.min(63) as u64) << 32,
        }
    }

    /// Folded multiply (as in foldhash): every input bit reaches the low bits.
    #[inline]
    pub fn hash(self) -> usize {
        let m = u128::from(self.lo ^ 0x243f_6a88_85a3_08d3)
            * u128::from(self.w2 ^ 0x1319_8a2e_0370_7344);
        (m as u64 ^ (m >> 64) as u64) as usize
    }
}

/// Open addressing with linear probing; a slot is `[lo, w2]`. Inserts claim a
/// slot with a CAS on `lo`, so all threads insert at once; after the parse the
/// table is only read. It has room for every query word to be a distinct term
/// at a load of at most 3/4, so it can never fill up. Term ids are
/// block-dense: a thread takes `ID_BLOCK` ids at a time from a shared counter
/// and hands them out in order, so ids cost no shared counter per term, and
/// each thread leaves at most one block partly unused.
pub struct Table {
    slots: Vec<[AtomicU64; 2]>,
    pub mask: usize,
    next_block: AtomicU32,
    /// Per term id: where the term starts in `queries.txt` (for long terms),
    /// its slot, and its column among the hot terms.
    text_at: Vec<AtomicU32>,
    slot_of: Vec<AtomicU32>,
    hot_of: Vec<AtomicU32>,
    hot_count: AtomicU32,
    /// Per term id: a saturating count of its query words, for choosing anchors.
    freq: Vec<AtomicU8>,
    /// Per term id: the first phrase anchored at it, linked on through
    /// `Parsed::next`.
    heads: Vec<AtomicU32>,
}

const ID_BLOCK: u32 = 1024;

impl Table {
    /// Room for the terms of `words` query words, and ids for all of them from
    /// `threads` threads.
    pub fn new(words: usize, threads: usize) -> Table {
        let n = (words * 4 / 3).next_power_of_two().max(1 << 10);
        let ids = words + threads * ID_BLOCK as usize;
        // A partition-local term number (id / 64) must fit in 16 bits.
        if ids > 1 << 22 {
            fail("too many query words");
        }
        Table {
            slots: (0..n).map(|_| Default::default()).collect(),
            mask: n - 1,
            next_block: AtomicU32::new(0),
            text_at: (0..ids).map(|_| AtomicU32::new(0)).collect(),
            slot_of: (0..ids).map(|_| AtomicU32::new(0)).collect(),
            hot_of: (0..ids).map(|_| AtomicU32::new(0)).collect(),
            hot_count: AtomicU32::new(0),
            freq: (0..ids).map(|_| AtomicU8::new(0)).collect(),
            heads: (0..ids).map(|_| AtomicU32::new(NONE)).collect(),
        }
    }

    /// Makes BOOL term `id` hot, returning its column, unless there are
    /// `MAX_HOT` already. Called by the build of segment 0, which every scan
    /// of segment 2 and later waits for.
    pub fn make_hot(&self, id: u32) -> Option<usize> {
        let column = self.hot_count.fetch_add(1, Relaxed) as usize;
        (column < MAX_HOT).then(|| {
            self.hot_of[id as usize].store(column as u32, Relaxed);
            let slot = self.slot_of[id as usize].load(Relaxed) as usize;
            self.slots[slot][1].fetch_or(HOT, Relaxed);
            column
        })
    }

    /// The number of hot terms, once segment 0 is built.
    pub fn hot_width(&self) -> usize {
        (self.hot_count.load(Relaxed) as usize).min(MAX_HOT)
    }

    /// The column of hot term `id`.
    #[inline]
    pub fn hot_column(&self, id: u32) -> usize {
        self.hot_of[id as usize].load(Relaxed) as usize
    }

    /// The first phrase anchored at term `id`, or `NONE`.
    #[inline]
    pub fn head(&self, id: u32) -> u32 {
        self.heads[id as usize].load(Relaxed)
    }

    /// One past the last id of the last block taken: a bound on the term ids
    /// (larger than the number of terms by the unused ends of blocks).
    pub fn ids(&self) -> usize {
        self.next_block.load(Relaxed) as usize
    }

    /// Starts loading slot `i`, so a run of lookups can wait on memory together.
    #[inline]
    pub fn prefetch(&self, i: usize) {
        // SAFETY: a prefetch is only a hint; it never faults or changes memory.
        unsafe {
            std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(
                self.slots[i].as_ptr().cast(),
            )
        }
    }

    /// The slot where the search for `key` starts.
    #[inline]
    pub fn slot(&self, key: Key) -> usize {
        key.hash() & self.mask
    }

    /// The term id in a slot's `w2`.
    #[inline]
    pub fn id(w2: u64) -> u32 {
        (w2 >> ID_SHIFT) as u32 & ID_MASK
    }

    /// A `w2` for tokens that are no query term: an id past all real ones,
    /// and no flags.
    pub fn miss(&self) -> u64 {
        (self.ids() as u64) << ID_SHIFT
    }

    /// The slot word `w2` (id and flags) of the token `token` (any case) with
    /// key `key`, probing from slot `i`. Tokens longer than 12 bytes are
    /// compared in full against `text` (queries.txt).
    #[inline]
    pub fn find(&self, key: Key, mut i: usize, token: &[u8], text: &[u8]) -> Option<u64> {
        let slots = &self.slots[..];
        loop {
            let slot = &slots[i & (slots.len() - 1)];
            let (lo, w2) = (slot[0].load(Relaxed), slot[1].load(Relaxed));
            if lo == key.lo
                && w2 & KEY_BITS == key.w2
                && (token.len() <= INLINE || self.tail_eq(token, Table::id(w2), text))
            {
                return Some(w2);
            }
            if lo == 0 {
                return None;
            }
            i += 1;
        }
    }

    /// The id and slot of the term `text[at..at + len]`, claiming both if it is new.
    /// `ids` are the thread's unused ids.
    fn insert(&self, text: &[u8], at: usize, len: usize, ids: &mut Range<u32>) -> (u32, usize) {
        let key = Key::new(text, at, len);
        let mut i = key.hash() & self.mask;
        for _ in 0..self.slots.len() {
            let slot = &self.slots[i];
            let mut lo = slot[0].load(Acquire);
            if lo == 0 {
                match slot[0].compare_exchange(0, key.lo, AcqRel, Acquire) {
                    Ok(_) => {
                        if ids.start == ids.end {
                            let block = self.next_block.fetch_add(ID_BLOCK, Relaxed);
                            *ids = block..block + ID_BLOCK;
                        }
                        let id = ids.next().unwrap();
                        self.text_at[id as usize].store(at as u32, Relaxed);
                        self.slot_of[id as usize].store(i as u32, Relaxed);
                        slot[1].store(key.w2 | u64::from(id) << ID_SHIFT, Release);
                        return (id, i);
                    }
                    Err(now) => lo = now,
                }
            }
            if lo == key.lo {
                // Another thread may have claimed `lo` and not yet written `w2`.
                let w2 = loop {
                    match slot[1].load(Acquire) {
                        0 => std::hint::spin_loop(),
                        w2 => break w2,
                    }
                };
                let id = Table::id(w2);
                if w2 & KEY_BITS == key.w2
                    && (len <= INLINE || self.tail_eq(&text[at..at + len], id, text))
                {
                    return (id, i);
                }
            }
            i = (i + 1) & self.mask;
        }
        unreachable!("a table with room for every word has a free slot")
    }

    fn flag(&self, i: usize, flag: u64) {
        if self.slots[i][1].load(Relaxed) & flag == 0 {
            self.slots[i][1].fetch_or(flag, Relaxed);
        }
    }

    fn term_len(&self, i: usize) -> usize {
        (self.slots[i][1].load(Relaxed) >> 32 & 63) as usize
    }

    /// Whether the token's bytes after the inline 12 match term `id`.
    fn tail_eq(&self, token: &[u8], id: u32, text: &[u8]) -> bool {
        let at = self.text_at[id as usize].load(Relaxed) as usize + INLINE;
        let term = &text[at..at + token.len() - INLINE];
        let tail = token[INLINE..].iter();
        tail.zip(term).all(|(&a, &b)| a | 0x20 == b)
    }
}

/// The queries of one line-aligned range of `queries.txt`, in file order.
#[derive(Default)]
pub struct Parsed {
    /// Per line: the index of its BOOL query, or `PHRASE |` its phrase index.
    pub order: Vec<u32>,
    /// BOOL query `i` is `span(steps, ends, i)`, each step `op << TERM_BITS | term`;
    /// the first step is `OR t1` applied to an empty set.
    pub steps: Vec<u32>,
    pub ends: Vec<u32>,
    /// Phrase `i` is the terms `span(terms, term_ends, i)`.
    pub terms: Vec<u32>,
    pub term_ends: Vec<u32>,
    /// Position of each phrase's anchor among its terms.
    pub anchor: Vec<u8>,
    /// The next phrase with the same anchor (`NONE` ends the list).
    pub next: Vec<u32>,
    /// `(offset in queries.txt, length)` of each step's and each phrase
    /// term's word, until `insert` turns them into term ids.
    bool_words: Vec<(u32, u8)>,
    phrase_words: Vec<(u32, u8)>,
    /// Table slots of the phrase terms, until anchors are chosen.
    slots: Vec<u32>,
}

/// A phrase is named across ranges by `range << 28 | index`, which allows 16
/// ranges (one per thread, so at most 16 threads; there are 12) of up to 2^28
/// phrases.
pub const RANGE_SHIFT: u32 = 28;

/// Record `i` of a list of variable-length records that end at `ends`.
fn span<'a>(items: &'a [u32], ends: &[u32], i: usize) -> &'a [u32] {
    let start = if i == 0 { 0 } else { ends[i - 1] as usize };
    &items[start..ends[i] as usize]
}

impl Parsed {
    pub fn bool_steps(&self, i: usize) -> &[u32] {
        span(&self.steps, &self.ends, i)
    }

    pub fn phrase(&self, i: usize) -> &[u32] {
        span(&self.terms, &self.term_ends, i)
    }

    /// Parses the lines of `text[from..to]` (a whole number of lines). Term
    /// ids are filled in by `insert`. Every line must be a query (spec 2.3);
    /// only the empty piece after the final LF is not a line.
    pub fn parse(text: &[u8], from: usize, to: usize) -> Parsed {
        let mut p = Parsed::default();
        let lines = &text[from..to];
        let lines = lines.strip_suffix(b"\n").unwrap_or(lines);
        // An empty range has no lines (splitting it would give one empty one).
        for line in lines.split(|&b| b == b'\n').take_while(|_| from < to) {
            if p.line(line, text).is_none() {
                let line = String::from_utf8_lossy(line);
                fail(&format!("malformed query: {line:?}"));
            }
        }
        p
    }

    fn line(&mut self, line: &[u8], text: &[u8]) -> Option<()> {
        // Words are recorded by their offset in `text`, which they are slices of.
        let word = |w: &[u8]| {
            let ok = (1..=32).contains(&w.len())
                && w.iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
            let at = w.as_ptr() as usize - text.as_ptr() as usize;
            ok.then_some((at as u32, w.len() as u8))
        };
        if let Some(rest) = line.strip_prefix(b"BOOL ") {
            self.order.push(self.ends.len() as u32);
            let (mut words, mut op) = (rest.split(|&b| b == b' '), OR);
            loop {
                self.bool_words.push(word(words.next()?)?);
                self.steps.push(op << TERM_BITS);
                op = match words.next() {
                    None => break,
                    Some(b"AND") => AND,
                    Some(b"OR") => OR,
                    Some(b"NOT") => NOT,
                    Some(_) => return None,
                };
            }
            self.ends.push(self.steps.len() as u32);
        } else {
            let inner = line.strip_prefix(b"PHRASE \"")?.strip_suffix(b"\"")?;
            self.order.push(PHRASE | self.term_ends.len() as u32);
            for w in inner.split(|&b| b == b' ') {
                self.phrase_words.push(word(w)?);
                self.terms.push(0);
            }
            let start = self.term_ends.last().map_or(0, |&e| e as usize);
            if self.terms.len() - start > usize::from(u8::MAX) {
                return None; // anchor positions are bytes
            }
            self.term_ends.push(self.terms.len() as u32);
        }
        Some(())
    }

    pub fn word_count(&self) -> usize {
        self.bool_words.len() + self.phrase_words.len()
    }

    /// Puts every word into the table, turning steps and phrase terms into
    /// term ids, counts how often each term occurs and flags BOOL terms.
    pub fn insert(&mut self, table: &Table, text: &[u8]) {
        let mut ids = 0..0;
        let mut insert = |(at, len): (u32, u8)| {
            let (id, slot) = table.insert(text, at as usize, len as usize, &mut ids);
            if table.freq[id as usize].load(Relaxed) < FREQ_CAP {
                table.freq[id as usize].fetch_add(1, Relaxed);
            }
            (id, slot)
        };
        for (step, &word) in self.steps.iter_mut().zip(&self.bool_words) {
            let (id, slot) = insert(word);
            table.flag(slot, BOOL_TERM);
            *step |= id;
        }
        for (term, &word) in self.terms.iter_mut().zip(&self.phrase_words) {
            let (id, slot) = insert(word);
            *term = id;
            self.slots.push(slot as u32);
        }
    }

    /// Anchors each phrase at its term with the fewest occurrences in the
    /// queries, the longest on a tie. Common words appear in many queries and
    /// rare ones in one or two, so on seed 1 this picks terms whose corpus
    /// occurrences add up to 1.26 million, against 0.92 million for the true
    /// rarest terms and 52 million for "the longest term".
    pub fn choose_anchors(&mut self, table: &Table, range: usize) {
        for i in 0..self.term_ends.len() {
            let (terms, slots) = (self.phrase(i), span(&self.slots, &self.term_ends, i));
            let freq = |j: usize| table.freq[terms[j] as usize].load(Relaxed);
            let rank = |j: usize| (freq(j), usize::MAX - table.term_len(slots[j] as usize));
            let a = (0..terms.len()).min_by_key(|&j| rank(j)).unwrap();
            let (term, slot) = (terms[a], slots[a]);
            table.flag(slot as usize, ANCHOR);
            self.anchor.push(a as u8);
            let phrase = (range as u32) << RANGE_SHIFT | i as u32;
            let previous = table.heads[term as usize].swap(phrase, AcqRel);
            self.next.push(previous);
        }
        (self.slots, self.bool_words, self.phrase_words) = Default::default();
    }
}

/// The query side's start-up, run by every thread at once: parse a range of
/// `queries.txt`, fill the shared term table, choose the phrase anchors.
pub struct Setup<'a> {
    /// `queries.txt` and `QUERY_PAD` zero bytes.
    text: &'a [u8],
    /// Thread `i` parses the lines in `text[ranges[i]..ranges[i + 1]]`.
    ranges: Vec<usize>,
    barrier: Barrier,
    words: AtomicUsize,
    table: OnceLock<Table>,
}

impl<'a> Setup<'a> {
    pub fn new(text: &'a [u8], threads: usize) -> Setup<'a> {
        let len = text.len() - QUERY_PAD;
        let ranges = (0..=threads).map(|i| line_start(&text[..len], len * i / threads));
        Setup {
            text,
            ranges: ranges.collect(),
            barrier: Barrier::new(threads),
            words: AtomicUsize::new(0),
            table: OnceLock::new(),
        }
    }

    /// Waits for every thread.
    pub fn wait(&self) {
        self.barrier.wait();
    }

    /// Thread `i`'s share of the start-up: its queries, and the finished
    /// table. The table is sized once every thread has counted its words.
    pub fn run(&self, i: usize) -> (Parsed, &Table) {
        let mut mine = Parsed::parse(self.text, self.ranges[i], self.ranges[i + 1]);
        self.words.fetch_add(mine.word_count(), Relaxed);
        if self.barrier.wait().is_leader() {
            let (words, threads) = (self.words.load(Relaxed), self.ranges.len() - 1);
            let _ = self.table.set(Table::new(words, threads));
        }
        self.wait();
        let table = self.table.get().unwrap();
        mine.insert(table, self.text);
        self.wait(); // anchors read every thread's counts
        mine.choose_anchors(table, i);
        (mine, table)
    }
}

/// The first line start of `text` at or after `at`.
fn line_start(text: &[u8], at: usize) -> usize {
    let lf = text[at.max(1) - 1..].iter().position(|&b| b == b'\n');
    if at == 0 {
        0
    } else {
        lf.map_or(text.len(), |i| at + i)
    }
}
