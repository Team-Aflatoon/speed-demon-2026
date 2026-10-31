//! The query side: `queries.txt` parsed in parallel, every distinct term put
//! into one lock-free hash table that the corpus scan probes per token, and
//! each phrase given an anchor term at which the scan will check it.

use crate::fail;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering::*};

pub const NONE: u32 = u32::MAX;
/// Slot flags: the term is in a BOOL query (the scan records its documents),
/// and the term anchors at least one phrase (the scan checks phrases there).
pub const BOOL_TERM: u64 = 1 << 62;
pub const ANCHOR: u64 = 1 << 63;
/// Query operators, kept in the top two bits of a BOOL step.
pub const AND: u32 = 0;
pub const OR: u32 = 1;
pub const NOT: u32 = 2;
pub const SLOT_BITS: u32 = 30;
/// `order` entries with this bit are phrases.
pub const PHRASE: u32 = 1 << 31;
/// A slot holds the first 12 bytes of its term inline; longer terms are
/// confirmed against their text in `queries.txt`.
const INLINE: usize = 12;
/// `w2` bits 0..38 are the key (12 bytes inline + 6-bit length), 38..62 the
/// term id, 62 and 63 the flags.
const KEY_BITS: u64 = (1 << 38) - 1;
const ID_SHIFT: u32 = 38;
const ID_MASK: u32 = (1 << 24) - 1;
pub const MAX_QUERY_BYTES: usize = 1 << 30;
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

    fn len(self) -> usize {
        (self.w2 >> 32 & 63) as usize
    }
}

/// Open addressing with linear probing; a slot is `[lo, w2]`. Inserts claim a
/// slot with a CAS on `lo`, so all threads insert at once; after the parse the
/// table is only read. Term ids are dense: each thread hands them out from its
/// own block of `ID_BLOCK`, so ids cost no shared counter per term.
pub struct Table {
    slots: Vec<[AtomicU64; 2]>,
    pub mask: usize,
    next_block: AtomicU32,
    /// Per term id: where the term starts in `queries.txt` (for long terms).
    text_at: Vec<AtomicU32>,
    /// Per term id: a saturating count of its query words, for choosing anchors.
    freq: Vec<AtomicU8>,
    /// Per term id: the first phrase anchored at it, linked on through `Parsed::next`.
    pub heads: Vec<AtomicU32>,
}

const ID_BLOCK: u32 = 1024;

/// A thread's unused term ids: `next..end`.
pub struct Ids {
    next: u32,
    end: u32,
}

impl Table {
    /// Room for `words` query words, all distinct at worst, inserted by
    /// `threads` threads, at a load factor of at most 3/4.
    pub fn new(words: usize, threads: usize) -> Table {
        let n = (words * 4 / 3).next_power_of_two().max(1 << 10);
        let ids = words + threads * ID_BLOCK as usize;
        // A partition-local term number (id / 64) must fit in 16 bits.
        if ids > 1 << 22 {
            fail("too many query words");
        }
        Table {
            slots: (0..n)
                .map(|_| [AtomicU64::new(0), AtomicU64::new(0)])
                .collect(),
            mask: n - 1,
            next_block: AtomicU32::new(0),
            text_at: (0..ids).map(|_| AtomicU32::new(0)).collect(),
            freq: (0..ids).map(|_| AtomicU8::new(0)).collect(),
            heads: (0..ids).map(|_| AtomicU32::new(NONE)).collect(),
        }
    }

    /// One past the largest term id handed out.
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
    fn insert(&self, text: &[u8], at: usize, len: usize, ids: &mut Ids) -> (u32, usize) {
        let key = Key::new(text, at, len);
        let mut i = key.hash() & self.mask;
        for _ in 0..self.slots.len() {
            let slot = &self.slots[i];
            let mut lo = slot[0].load(Acquire);
            if lo == 0 {
                match slot[0].compare_exchange(0, key.lo, AcqRel, Acquire) {
                    Ok(_) => {
                        if ids.next == ids.end {
                            ids.next = self.next_block.fetch_add(ID_BLOCK, Relaxed);
                            ids.end = ids.next + ID_BLOCK;
                        }
                        let id = ids.next;
                        ids.next += 1;
                        self.text_at[id as usize].store(at as u32, Relaxed);
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
                let id = (w2 >> ID_SHIFT) as u32 & ID_MASK;
                if w2 & KEY_BITS == key.w2
                    && (len <= INLINE || self.tail_eq(&text[at..at + len], id, text))
                {
                    return (id, i);
                }
            }
            i = (i + 1) & self.mask;
        }
        fail("the term table is full") // cannot happen: it is sized for every word to be distinct
    }

    fn flag(&self, i: usize, flag: u64) {
        if self.slots[i][1].load(Relaxed) & flag == 0 {
            self.slots[i][1].fetch_or(flag, Relaxed);
        }
    }

    fn term_len(&self, i: usize) -> usize {
        Key {
            lo: 0,
            w2: self.slots[i][1].load(Relaxed),
        }
        .len()
    }

    /// Whether the token's bytes after the inline 12 match term `id`.
    fn tail_eq(&self, token: &[u8], id: u32, text: &[u8]) -> bool {
        let at = self.text_at[id as usize].load(Relaxed) as usize + INLINE;
        let term = &text[at..at + token.len() - INLINE];
        token[INLINE..]
            .iter()
            .zip(term)
            .all(|(&a, &b)| a | 0x20 == b)
    }
}

/// The queries of one line-aligned range of `queries.txt`, in file order.
#[derive(Default)]
pub struct Parsed {
    /// Per line: the index of its BOOL query, or `PHRASE |` its phrase index.
    pub order: Vec<u32>,
    /// BOOL query `i` is `steps[ends[i - 1]..ends[i]]`, each `op << 30 | slot`;
    /// the first step is `OR t1` applied to an empty set.
    pub steps: Vec<u32>,
    pub ends: Vec<u32>,
    /// Phrase `i` is the slots `terms[term_ends[i - 1]..term_ends[i]]`.
    pub terms: Vec<u32>,
    pub term_ends: Vec<u32>,
    /// Position of each phrase's anchor among its terms.
    pub anchor: Vec<u8>,
    /// The next phrase with the same anchor (`NONE` ends the list).
    pub next: Vec<u32>,
    /// `(offset, length)` in `queries.txt` of every term, in step order.
    words: Vec<(u32, u8)>,
    /// Table slots of the phrase terms, until anchors are chosen.
    slots: Vec<u32>,
}

/// A phrase is named across ranges by `range << 28 | index`.
pub const RANGE_SHIFT: u32 = 28;

impl Parsed {
    pub fn bool_steps(&self, i: usize) -> &[u32] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.steps[start..self.ends[i] as usize]
    }

    pub fn phrase(&self, i: usize) -> &[u32] {
        let start = if i == 0 {
            0
        } else {
            self.term_ends[i - 1] as usize
        };
        &self.terms[start..self.term_ends[i] as usize]
    }

    /// Parses the lines of `text[from..to]` (a whole number of lines). Term
    /// slots are filled in by `insert`.
    pub fn parse(text: &[u8], from: usize, to: usize) -> Parsed {
        let mut p = Parsed::default();
        let mut at = from;
        while at < to {
            let end = at
                + text[at..to]
                    .iter()
                    .position(|&b| b == b'\n')
                    .unwrap_or(to - at);
            if end > at && p.line(text, at, end).is_none() {
                fail(&format!(
                    "malformed query: {}",
                    String::from_utf8_lossy(&text[at..end])
                ));
            }
            at = end + 1;
        }
        p
    }

    fn line(&mut self, text: &[u8], at: usize, end: usize) -> Option<()> {
        let line = &text[at..end];
        let word = |p: &mut Parsed, start: usize, len: usize| {
            let ok = (1..=32).contains(&len)
                && text[start..start + len]
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
            p.words.push((start as u32, len as u8));
            ok.then_some(())
        };
        let mut words = Words {
            line,
            pos: 0,
            base: at,
        };
        if line.starts_with(b"BOOL ") {
            words.pos = 5;
            self.order.push(self.ends.len() as u32);
            let mut op = OR;
            loop {
                let (start, len) = words.next()?;
                word(self, start, len)?;
                self.steps.push(op << SLOT_BITS);
                op = match words.next() {
                    None => break,
                    Some((s, 3)) if &text[s..s + 3] == b"AND" => AND,
                    Some((s, 2)) if &text[s..s + 2] == b"OR" => OR,
                    Some((s, 3)) if &text[s..s + 3] == b"NOT" => NOT,
                    Some(_) => return None,
                };
            }
            self.ends.push(self.steps.len() as u32);
        } else {
            let inner = line.strip_prefix(b"PHRASE \"")?.strip_suffix(b"\"")?;
            self.order.push(PHRASE | self.term_ends.len() as u32);
            words = Words {
                line: inner,
                pos: 0,
                base: at + 8,
            };
            for (start, len) in words {
                word(self, start, len)?;
                self.terms.push(0);
            }
            let n = self.terms.len() - self.term_ends.last().map_or(0, |&e| e as usize);
            if n == 0 || n > usize::from(u8::MAX) {
                return None;
            }
            self.term_ends.push(self.terms.len() as u32);
        }
        Some(())
    }

    pub fn word_count(&self) -> usize {
        self.words.len()
    }

    /// Puts every term into the table, counts how often each occurs and flags
    /// BOOL terms. Words were recorded in line order, so they are consumed
    /// line by line into the BOOL steps or the phrase terms.
    pub fn insert(&mut self, table: &Table, text: &[u8]) {
        let (mut w, mut s, mut t) = (0, 0, 0);
        let mut ids = Ids { next: 0, end: 0 };
        for &entry in &self.order {
            let (count, phrase) = match entry & PHRASE {
                0 => (self.ends[entry as usize] as usize - s, false),
                _ => (
                    self.term_ends[(entry & !PHRASE) as usize] as usize - t,
                    true,
                ),
            };
            for _ in 0..count {
                let (at, len) = self.words[w];
                w += 1;
                let (id, slot) = table.insert(text, at as usize, len as usize, &mut ids);
                if table.freq[id as usize].load(Relaxed) < FREQ_CAP {
                    table.freq[id as usize].fetch_add(1, Relaxed);
                }
                if phrase {
                    self.terms[t] = id;
                    self.slots.push(slot as u32);
                    t += 1;
                } else {
                    table.flag(slot, BOOL_TERM);
                    self.steps[s] |= id;
                    s += 1;
                }
            }
        }
        self.words = Vec::new();
    }

    /// Anchors each phrase at its term with the fewest occurrences in the
    /// queries, the longest on a tie. Common words appear in many queries and
    /// rare ones in one or two, so on seed 1 this picks terms whose corpus
    /// occurrences add up to 1.26 million, against 0.92 million for the true
    /// rarest terms and 52 million for "the longest term".
    pub fn choose_anchors(&mut self, table: &Table, range: usize) {
        for i in 0..self.term_ends.len() {
            let start = if i == 0 {
                0
            } else {
                self.term_ends[i - 1] as usize
            };
            let (terms, slots) = (
                self.phrase(i),
                &self.slots[start..self.term_ends[i] as usize],
            );
            let rank = |j: usize| {
                (
                    table.freq[terms[j] as usize].load(Relaxed),
                    usize::MAX - table.term_len(slots[j] as usize),
                )
            };
            let a = (0..terms.len()).min_by_key(|&j| rank(j)).unwrap();
            let (term, slot) = (terms[a], slots[a]);
            table.flag(slot as usize, ANCHOR);
            self.anchor.push(a as u8);
            let phrase = (range as u32) << RANGE_SHIFT | i as u32;
            self.next
                .push(table.heads[term as usize].swap(phrase, AcqRel));
        }
        self.slots = Vec::new();
    }
}

/// Space-separated words of a line, as `(offset in the file, length)`.
struct Words<'a> {
    line: &'a [u8],
    pos: usize,
    base: usize,
}

impl Iterator for Words<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        if self.pos > self.line.len() {
            return None;
        }
        let rest = &self.line[self.pos..];
        let len = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
        let start = self.base + self.pos;
        self.pos += len + 1;
        Some((start, len))
    }
}
