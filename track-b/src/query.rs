//! The query side: `queries.txt` parsed in parallel, every distinct term put
//! into one lock-free hash table that the corpus scan probes per token, and
//! each phrase given an anchor term at which the scan will check it.

use crate::fail;
use std::arch::x86_64::*;
use std::ops::Range;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering::*};
use std::sync::{Barrier, OnceLock};

pub const NONE: u32 = u32::MAX;
/// The stamp of a term id the scan never records (see `Table::stamps`).
pub const NEVER: u8 = u8::MAX;
/// A term's value in the table:
///
/// ```text
///   bits  0..22  term id (0 is never a term: the value of a token that is no query term)
///   bits 22..29  how many query words are the term, saturating at USES_CAP
///   bit  31      ANCHOR: anchors a phrase (the scan checks phrases there)
/// ```
pub const ANCHOR: u32 = 1 << 31;
pub const ID_MASK: u32 = (1 << 22) - 1;
const USES_SHIFT: u32 = 22;
/// Query frequencies only need to tell rare from common. Threads that race
/// past the check add at most one each, so a count stays below
/// `USES_CAP + MAX_THREADS <= 127` and inside its seven bits.
pub const USES_CAP: u32 = 63;
/// In a bucket's fourth value only: a term whose search passed this bucket
/// lives in a later one.
const OVERFLOW: u32 = 1 << 29;
/// Query operators, kept in the top two bits of a BOOL step.
pub const AND: u32 = 0;
pub const OR: u32 = 1;
pub const NOT: u32 = 2;
const OPS: [&[u8]; 3] = [b"AND", b"OR", b"NOT"]; // in the order of their values
/// A BOOL step is `op << TERM_BITS | term`: the term id in bits 0..30, the
/// operator in bits 30 and 31.
pub const TERM_BITS: u32 = 30;
/// `order` entries with this bit are phrases.
pub const PHRASE: u32 = 1 << 31;
/// Tokens of up to this many bytes are their own key (see `Key`).
pub const SHORT: usize = 16;
/// Term offsets in `queries.txt` are kept as u32.
pub const MAX_QUERY_BYTES: usize = 1 << 30;
/// Zero bytes after `queries.txt`, so a key can read 16 bytes from any term.
pub const QUERY_PAD: usize = 16;

/// A token's identity in 16 bytes. Bytes are lowercased with `| 0x20`
/// (exact for letters and digits, whose bit 5 is already set) and zero
/// padded. A token of up to `SHORT` bytes has no zero byte, so its key is
/// unique to it, and its byte 15 is 0 or a letter or digit. A longer token
/// keeps its first 15 bytes and puts `0x80 | length` (at most 127) in byte
/// 15, which no short token has; two long tokens with the same key are told
/// apart by comparing the rest of their bytes. The two halves are bytes 0..8
/// and 8..16 read as little-endian words.
#[derive(Clone, Copy)]
struct Key(u64, u64);

/// `SLOT[m]`: the slot whose key matches, from the bits `m` of matching key
/// words (bits 2j and 2j + 1 for slot j), or 3 (the bucket's miss value).
static SLOT: [u8; 64] = {
    let (mut t, mut i) = ([3; 64], 0);
    while i < 192 {
        // Slots 2, 1, 0 in turn, so the lowest one that matches is set last.
        let (m, j) = (i % 64, 2 - i / 64);
        if m >> (2 * j) & 3 == 3 {
            t[m] = j as u8;
        }
        i += 1;
    }
    t
};
/// A token's key and home bucket, as `Lookup::probe` makes them. Only
/// `probe` makes one, so `Lookup::find` can trust the bucket.
#[derive(Clone, Copy)]
pub struct Probe {
    key: __m128i,
    home: *const Bucket,
}

/// The home of every long token: it matches no key, and every value sends
/// `Lookup::find` to the slow path, which compares the token's bytes.
static LONG: Bucket = Bucket {
    keys: [const { AtomicU64::new(0) }; 6],
    vals: [const { AtomicU32::new(OVERFLOW) }; 4],
};

impl Default for Probe {
    fn default() -> Probe {
        // SAFETY: SSE2 is part of x86-64-v3, which the crate requires.
        let key = unsafe { _mm_setzero_si128() };
        Probe { key, home: &LONG }
    }
}

/// The 16 bytes at `p`, or-ed with 0x20 (which lowercases letters and keeps
/// digits), and with every byte from `len` on zeroed (none for `len` from 16
/// to 127).
///
/// SAFETY: 16 bytes from `p` must be readable.
#[inline(always)]
unsafe fn lowered(p: *const u8, len: usize) -> __m128i {
    // SAFETY: the caller vouches for the 16 bytes; unaligned loads have no
    // alignment rule; SSE2 is part of x86-64-v3, which the crate requires.
    unsafe {
        let iota = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        let mask = _mm_cmpgt_epi8(_mm_set1_epi8(len as i8), iota);
        let v = _mm_or_si128(_mm_loadu_si128(p.cast()), _mm_set1_epi8(0x20));
        _mm_and_si128(v, mask)
    }
}

impl Key {
    /// The token `bytes[at..at + len]`; 16 bytes from `at` must be readable
    /// (anything after the token is masked off).
    pub fn new(bytes: &[u8], at: usize, len: usize) -> Key {
        // SAFETY: the 16 bytes are checked by the slice.
        let key = Key::of(unsafe { lowered(bytes[at..at + 16].as_ptr(), len.min(SHORT)) });
        let tag = (0x80 | len.min(127) as u64) << 56;
        match len {
            ..=SHORT => key,
            _ => Key(key.0, key.1 & (u64::MAX >> 8) | tag),
        }
    }

    /// The key held in a vector.
    #[inline(always)]
    fn of(v: __m128i) -> Key {
        // SAFETY: SSE4.1 is part of x86-64-v3, which the crate requires.
        unsafe { Key(_mm_cvtsi128_si64(v) as _, _mm_extract_epi64::<1>(v) as _) }
    }

    /// CRC-32C of the first 12 bytes of the key (the CRC of `lo` from the
    /// low half of `hi`): one instruction, and every one of those bits
    /// reaches the low bits that pick the bucket. Keys that differ only in
    /// bytes 12 to 15 share a bucket, which is rare and only costs a probe.
    #[inline(always)]
    pub fn hash(self) -> usize {
        // SAFETY: SSE4.2 is part of x86-64-v3, which the crate requires.
        unsafe { _mm_crc32_u64(self.1, self.0) as usize }
    }
}

/// Three keys and their values in one cache line. `vals[3]` is the value of
/// a token that is not in the bucket: 0 (no term), or `OVERFLOW` when it may
/// be in a later bucket. An empty slot has key 0, which no token has.
#[repr(C, align(64))]
struct Bucket {
    keys: [AtomicU64; 6],
    vals: [AtomicU32; 4],
}

/// The read side of the table, for the per-token code of the scan: the
/// buckets and mask are copied into this value, so that loop keeps them in
/// registers (through `&Table` it reloads them after every store).
#[derive(Clone, Copy)]
pub struct Lookup<'t> {
    buckets: &'static [Bucket],
    mask: usize,
    table: &'t Table,
}

impl Lookup<'_> {
    /// The key and home bucket of the token of `len` bytes at `p`, and starts
    /// loading the bucket.
    ///
    /// SAFETY: 16 bytes from `p` must be readable.
    #[inline(always)]
    pub unsafe fn probe(self, p: *const u8, len: usize) -> Probe {
        // SAFETY: the caller vouches for the 16 bytes; a prefetch is only a
        // hint and never faults. (A long token's key is not used.)
        unsafe {
            let key = lowered(p, len);
            // `& mask` makes this a bucket of the table (whose bucket count
            // is a power of two), which lives until the process exits.
            let home: *const Bucket = self.buckets.get_unchecked(Key::of(key).hash() & self.mask);
            crate::prefetch(home);
            let home = if len <= SHORT { home } else { &LONG };
            Probe { key, home }
        }
    }

    /// The value of the token `token()` of `probe`: branch-free, unless the
    /// token is long or its bucket has overflowed and it is not in it.
    #[inline(always)]
    pub fn find<'b>(self, probe: Probe, token: impl FnOnce() -> &'b [u8], text: &[u8]) -> u32 {
        // SAFETY: `probe` made the home a bucket of the table, which lives until
        // the process exits, or `LONG`. The parse is over (a barrier orders
        // every insert before the scan), so the bucket is no longer written
        // while it is read here; its 64 bytes are aligned and readable; AVX2
        // is required by the crate.
        let v = unsafe {
            let bucket = &*probe.home;
            let p: *const __m256i = bucket.keys.as_ptr().cast();
            let q = _mm256_broadcastsi128_si256(probe.key);
            let eq = |v| _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(v, q))) as u32;
            // Bits 2j and 2j + 1: slot j's lo and hi match (the second load's
            // top half holds the values, masked off).
            let m = eq(_mm256_load_si256(p)) | (eq(_mm256_load_si256(p.add(1))) & 3) << 4;
            *bucket.vals[usize::from(SLOT[m as usize])].as_ptr()
        };
        if v & OVERFLOW != 0 {
            return self.table.find_slow(Key::of(probe.key), token(), text);
        }
        v
    }
}

/// Open addressing over buckets of three slots, probed bucket by bucket.
/// Inserts claim a slot with a CAS on its first key word, so all threads
/// insert at once; after the parse the table is only read. It has a slot for
/// every query word, so it can never fill up. Term ids are block-dense: a
/// thread takes `ID_BLOCK` ids at a time from a shared counter and hands them
/// out in order, so ids cost no shared counter per term, and each thread
/// leaves at most one block partly unused.
pub struct Table {
    buckets: &'static [Bucket],
    mask: usize,
    next_block: AtomicU32,
    /// Per term id: where the term starts in `queries.txt` (for long terms).
    text_at: &'static [AtomicU32],
    /// Per term id: the first phrase anchored at it plus 1 (0 for none, so
    /// the array starts zeroed), linked on through `Parsed::next`.
    heads: &'static [AtomicU32],
    /// Per term id: 1 if the term is in a BOOL query.
    bools: &'static [AtomicU8],
}

/// `n` zeroed values in fresh anonymous pages, never unmapped (the table
/// lives until the process exits). With `populate`, the kernel maps and
/// zeroes them all at once, far cheaper than faulting them in one at a time
/// from twelve threads, or than the allocator zeroing them (as it does for
/// 64-byte aligned memory), and the caller's thread does it before any other
/// exists: on 2 MB-aligned memory asked for huge pages, so the table needs
/// one TLB entry per 2 MB. Whatever pages the kernel finds, all of it is
/// populated, so the memory is the same. Otherwise pages are faulted in where
/// they are touched.
///
/// SAFETY: `T` must be valid as zero bytes, and need no more than page alignment.
unsafe fn zeroed<T>(n: usize, populate: bool) -> &'static [T] {
    const HUGE: usize = 2 << 20;
    let len = (n * std::mem::size_of::<T>()).max(1);
    // MAP_PRIVATE | MAP_ANONYMOUS.
    let span = if populate { len + HUGE } else { len };
    // SAFETY: an anonymous mapping (PROT_READ | PROT_WRITE) at an address of
    // the kernel's choosing touches no memory of ours.
    let mut p = unsafe { crate::mmap(std::ptr::null_mut(), span, 0x3, 0x22, -1, 0) };
    assert!(p as isize != -1, "cannot map {len} bytes for the table");
    if populate {
        // SAFETY: advice on our own range, inside the mapping.
        unsafe {
            p = p.add(p.align_offset(HUGE));
            crate::madvise(p, len, 14); // MADV_HUGEPAGE
            crate::madvise(p, len, 23); // MADV_POPULATE_WRITE (Linux 5.14)
        }
    }
    // SAFETY: the mapping is `len` bytes, page aligned, zeroed, used for
    // nothing else and never unmapped; the caller vouches for `T`.
    unsafe { std::slice::from_raw_parts(p.cast(), n) }
}

const ID_BLOCK: u32 = 1024;
/// The binding limit on term ids (see `Table::new`).
const MAX_IDS: usize = 1 << 22;

impl Table {
    /// Room for the terms of `words` query words, and ids for all of them from
    /// `threads` threads.
    pub fn new(words: usize, threads: usize) -> Table {
        let n = words.div_ceil(3).next_power_of_two().max(1 << 10);
        // Id 0 is the no-term value, so blocks start at 1.
        let ids = 1 + words + threads * ID_BLOCK as usize;
        // The binding limit on ids: a posting holds the partition-local term
        // number (id / 64) in 16 bits, so ids stay under 2^22. The id field of
        // a value is the same 22 bits (`ID_MASK`); that of a BOOL step (30
        // bits) is looser. 50,000 queries hold at most 400,000 words.
        assert!(ids <= MAX_IDS, "too many query words: {words}");
        // SAFETY: zero bytes are valid atomic integers, which is all a
        // `Bucket` holds; it needs 64-byte alignment, less than a page.
        // The buckets are touched all over; the per-id arrays only where ids are.
        unsafe {
            Table {
                text_at: zeroed(ids, false),
                heads: zeroed(ids, false),
                bools: zeroed(ids, false),
                buckets: zeroed(n, true),
                mask: n - 1,
                next_block: AtomicU32::new(1),
            }
        }
    }

    /// The link to the first phrase anchored at term `id`, or `NONE`.
    pub fn head(&self, id: u32) -> u32 {
        self.heads[id as usize].load(Relaxed).wrapping_sub(1)
    }

    /// Starts loading the head of term `id`'s phrase list.
    pub fn prefetch_head(&self, id: u32) {
        crate::prefetch(self.heads.as_ptr().wrapping_add(id as usize));
    }

    /// A stamp per term id for the scan's `Doc::finish`: 0 for BOOL terms,
    /// `NEVER` for every other id (0, phrase-only terms, unused ids).
    pub fn stamps(&self) -> Vec<u8> {
        let bools = self.bools[..self.ids()].iter();
        bools.map(|b| NEVER * (1 - b.load(Relaxed))).collect()
    }

    /// One past the last id of the last block taken: a bound on the term ids
    /// (larger than the number of terms by the unused ends of blocks).
    pub fn ids(&self) -> usize {
        self.next_block.load(Relaxed) as usize
    }

    /// The scan's view of the table.
    pub fn lookup(&self) -> Lookup<'_> {
        Lookup {
            buckets: self.buckets,
            mask: self.mask,
            table: self,
        }
    }

    /// The value of the token `token` (of key `key` if it is short), by
    /// probing: for long tokens and overflowed buckets. A long token is told
    /// apart from other long tokens with the same key by its bytes, compared
    /// with the term's in `queries.txt` (`text`).
    #[cold]
    #[inline(never)]
    fn find_slow(&self, mut key: Key, token: &[u8], text: &[u8]) -> u32 {
        let long = token.len() > SHORT;
        if long {
            key = Key::new(token, 0, token.len());
        }
        let mut b = key.hash() & self.mask; // the home bucket
        for _ in 0..self.buckets.len() {
            let bucket = &self.buckets[b];
            for j in 0..3 {
                let v = bucket.vals[j].load(Relaxed);
                let same = |i: usize, k: u64| bucket.keys[2 * j + i].load(Relaxed) == k;
                let tail = || self.tail_eq(token, v & ID_MASK, text);
                if same(0, key.0) && same(1, key.1) && (!long || tail()) {
                    return v;
                }
            }
            if bucket.vals[3].load(Relaxed) & OVERFLOW == 0 {
                break;
            }
            b = (b + 1) & self.mask;
        }
        0
    }

    /// The id and slot (`bucket * 4 + j`) of the term `text[at..at + len]`,
    /// claiming both if it is new, and counts one more use of it. `ids` are
    /// the thread's unused ids.
    fn insert(&self, text: &[u8], at: usize, len: usize, ids: &mut Range<u32>) -> (u32, usize) {
        let key = Key::new(text, at, len);
        let mut b = key.hash() & self.mask; // the home bucket
        for _ in 0..self.buckets.len() {
            let bucket = &self.buckets[b];
            for j in 0..3 {
                let mut lo = bucket.keys[2 * j].load(Acquire);
                if lo == 0 {
                    match bucket.keys[2 * j].compare_exchange(0, key.0, AcqRel, Acquire) {
                        Ok(_) => {
                            if ids.start == ids.end {
                                let block = self.next_block.fetch_add(ID_BLOCK, Relaxed);
                                *ids = block..block + ID_BLOCK;
                            }
                            let id = ids.next().unwrap();
                            self.text_at[id as usize].store(at as u32, Relaxed);
                            bucket.keys[2 * j + 1].store(key.1, Relaxed);
                            // Release: whoever sees the value sees `hi` and `text_at`.
                            bucket.vals[j].store(id | 1 << USES_SHIFT, Release);
                            return (id, 4 * b + j);
                        }
                        Err(now) => lo = now,
                    }
                }
                if lo == key.0 {
                    // Another thread may have claimed the slot and not yet
                    // written its value (ids are never 0).
                    let v = loop {
                        match bucket.vals[j].load(Acquire) {
                            0 => std::hint::spin_loop(),
                            v => break v,
                        }
                    };
                    let id = v & ID_MASK;
                    let tail = || self.tail_eq(&text[at..at + len], id, text);
                    if bucket.keys[2 * j + 1].load(Relaxed) == key.1 && (len <= SHORT || tail()) {
                        if v >> USES_SHIFT & 0x7F < USES_CAP {
                            bucket.vals[j].fetch_add(1 << USES_SHIFT, Relaxed);
                        }
                        return (id, 4 * b + j);
                    }
                }
            }
            // All three slots hold other terms: this term lives further on.
            self.flag(4 * b + 3, OVERFLOW);
            b = (b + 1) & self.mask;
        }
        unreachable!("a table with a slot for every word has a free slot")
    }

    /// Sets `flag` in the value of slot `s` (as returned by `insert`).
    fn flag(&self, s: usize, flag: u32) {
        let v = &self.buckets[s / 4].vals[s % 4];
        if v.load(Relaxed) & flag == 0 {
            v.fetch_or(flag, Relaxed);
        }
    }

    /// How many query words are the term in slot `s`, up to about `USES_CAP`.
    fn uses(&self, s: usize) -> u32 {
        self.buckets[s / 4].vals[s % 4].load(Relaxed) >> USES_SHIFT & 0x7F
    }

    /// Whether a long token's bytes from 15 on match term `id`, whose key,
    /// and so whose length, equals the token's.
    fn tail_eq(&self, token: &[u8], id: u32, text: &[u8]) -> bool {
        let at = self.text_at[id as usize].load(Relaxed) as usize;
        let term = &text[at + 15..at + token.len()];
        token[15..].iter().zip(term).all(|(&a, &b)| a | 0x20 == b)
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
    /// The phrases as the scan reads them, one record each: the link to the
    /// next phrase with the same anchor (`NONE` ends the list), the phrase's
    /// index, `anchor | len << 8` (the anchor's position among its `len`
    /// terms), then the terms. A link is `range << RANGE_SHIFT | offset`.
    pub records: Vec<u32>,
    /// `(offset in queries.txt, length)` of each step's and each phrase
    /// term's word, until `insert` turns them into term ids.
    words: [Vec<(u32, u8)>; 2],
    /// Table slots of the phrase terms, until anchors are chosen.
    slots: Vec<u32>,
}

/// A phrase is named across ranges by `range << 28 | index`, and its record
/// by `range << 28 | offset`, which allows 16 ranges (one per thread, so at
/// most 16 threads; there are 12) of up to 2^28 phrases and record words
/// (term ids are under 2^22, and so are the words that hold them).
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

    /// Parses the lines of `text[from..to]` (a whole number of lines); term
    /// ids are filled in by `insert`. Every line must be a query (spec 2.3);
    /// only the empty piece after the final LF is not a line.
    pub fn parse(text: &[u8], from: usize, to: usize) -> Parsed {
        let mut p = Parsed::default();
        // Room for the words of a typical range (about 11 bytes each).
        p.steps.reserve((to - from) / 8);
        p.terms.reserve((to - from) / 8);
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

    /// Parses one line (a slice of `text`, where words are recorded by their
    /// offsets).
    fn line(&mut self, line: &[u8], text: &[u8]) -> Option<()> {
        let words = &mut self.words;
        let mut word = |w: &[u8], phrase: usize| {
            let term = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
            let ok = (1..=32).contains(&w.len()) && w.iter().all(term);
            let at = (w.as_ptr() as usize - text.as_ptr() as usize) as u32;
            ok.then(|| words[phrase].push((at, w.len() as u8)))
        };
        if let Some(rest) = line.strip_prefix(b"BOOL ") {
            self.order.push(self.ends.len() as u32);
            let (mut words, mut op) = (rest.split(|&b| b == b' '), OR);
            loop {
                word(words.next()?, 0)?;
                self.steps.push(op << TERM_BITS);
                match words.next() {
                    None => break,
                    Some(o) => op = OPS.iter().position(|&k| k == o)? as u32,
                }
            }
            self.ends.push(self.steps.len() as u32);
        } else {
            let inner = line.strip_prefix(b"PHRASE \"")?.strip_suffix(b"\"")?;
            self.order.push(PHRASE | self.term_ends.len() as u32);
            let start = self.terms.len();
            for w in inner.split(|&b| b == b' ') {
                word(w, 1)?;
                self.terms.push(0);
            }
            // Anchor positions are bytes (the spec allows 8 terms).
            (self.terms.len() - start <= usize::from(u8::MAX)).then_some(())?;
            self.term_ends.push(self.terms.len() as u32);
        }
        Some(())
    }

    /// Puts every word into the table, turning steps and phrase terms into
    /// term ids, counts how often each term occurs and marks BOOL terms.
    pub fn insert(&mut self, table: &Table, text: &[u8]) {
        let mut ids = 0..0;
        let mut insert =
            |&(at, len): &(u32, u8)| table.insert(text, at as usize, len as usize, &mut ids);
        for (step, word) in self.steps.iter_mut().zip(&self.words[0]) {
            let id = insert(word).0;
            *step |= id;
            if table.bools[id as usize].load(Relaxed) == 0 {
                table.bools[id as usize].store(1, Relaxed);
            }
        }
        for (term, word) in self.terms.iter_mut().zip(&self.words[1]) {
            let slot;
            (*term, slot) = insert(word);
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
            let terms = span(&self.terms, &self.term_ends, i);
            let slots = span(&self.slots, &self.term_ends, i);
            let words = &self.words[1][self.term_ends[i] as usize - terms.len()..];
            let rank = |j: usize| (table.uses(slots[j] as usize), u8::MAX - words[j].1);
            let a = (0..terms.len()).min_by_key(|&j| rank(j)).unwrap();
            let (term, slot) = (terms[a], slots[a]);
            table.flag(slot as usize, ANCHOR);
            let link = (range as u32) << RANGE_SHIFT | self.records.len() as u32;
            let previous = table.heads[term as usize].swap(link + 1, AcqRel);
            let previous = previous.wrapping_sub(1);
            let meta = a as u32 | (terms.len() as u32) << 8;
            self.records.extend_from_slice(&[previous, i as u32, meta]);
            self.records.extend_from_slice(terms);
        }
        (self.slots, self.words) = Default::default();
    }
}

/// The query side's start-up, run by every thread at once: parse a range of
/// `queries.txt`, fill the shared term table, choose the phrase anchors.
pub struct Setup<'a> {
    /// `queries.txt` and `QUERY_PAD` zero bytes.
    pub text: &'a [u8],
    /// Thread `i` parses the lines in `text[ranges[i]..ranges[i + 1]]`.
    ranges: Vec<usize>,
    pub barrier: Barrier,
    table: OnceLock<Table>,
}

impl<'a> Setup<'a> {
    /// The ranges of `queries.txt` (and `QUERY_PAD` zero bytes) for
    /// `threads` threads; the table comes with `make_table`.
    pub fn new(text: &'a [u8], threads: usize) -> Setup<'a> {
        let len = text.len() - QUERY_PAD;
        let ranges = (0..=threads).map(|i| line_start(&text[..len], len * i / threads));
        Setup {
            text,
            ranges: ranges.collect(),
            barrier: Barrier::new(threads),
            table: OnceLock::new(),
        }
    }

    /// Makes the table, sized from the words of `queries.txt`: in a line
    /// that parses, every term follows a space (a phrase's first through its
    /// quote), and every other word after a space is an uppercase operator.
    /// (A line that does not parse ends the run before it inserts anything.)
    pub fn make_table(&self) {
        let text = &self.text[..self.text.len() - QUERY_PAD];
        let term = |(&a, &b): (&u8, &u8)| usize::from((a == b' ') & !b.is_ascii_uppercase());
        let words = text.iter().zip(&text[1..]).map(term).sum();
        let _ = self.table.set(Table::new(words, self.ranges.len() - 1));
    }

    /// Thread `i`'s share of the start-up: its queries, parsed while the
    /// table is made, and the finished table.
    pub fn run(&self, i: usize) -> (Parsed, &Table) {
        let mut mine = Parsed::parse(self.text, self.ranges[i], self.ranges[i + 1]);
        let table = self.table.wait();
        mine.insert(table, self.text);
        self.barrier.wait(); // anchors read every thread's counts
        mine.choose_anchors(table, i);
        (mine, table)
    }
}

/// The first line start of `text` at or after `at`.
fn line_start(text: &[u8], at: usize) -> usize {
    let lf = text[at.max(1) - 1..].iter().position(|&b| b == b'\n');
    match at {
        0 => 0,
        _ => at + lf.unwrap_or(text.len() - at),
    }
}
