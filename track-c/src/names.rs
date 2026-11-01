//! Distinct names, in two phases, so that the hot path never writes a cache
//! line another core reads.
//!
//! Phase 1, while parsing: every name is interned into one shared lock-free
//! hash table (`Table`), and its slot's index is the name's id. After the
//! first few percent of the file nearly every name is already there, so the
//! table is read-mostly and its lines stay shared by all cores. Each record
//! then appends its (id, group) pair, as 16 bits, to a bucket of its own
//! worker's `Pairs`: bucket `id >> 8`, entry `group << 8 | id & 255`. Those
//! are private writes, mostly to a few hot lines.
//!
//! Phase 2, once every worker is done: the workers claim buckets, and each
//! bucket's pairs from all workers are counted against an 8 KiB bitset of
//! 256 ids x 256 groups that stays in L1. A pair is new exactly when its bit
//! flips, which gives `names_distinct`; every pair also adds one record and
//! its name's unescaped length to its group.

use crate::parse::{load, Fail, Res};
use crate::Padded;
use std::arch::x86_64::{_bzhi_u64, _mm_crc32_u64};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};

/// The longest name kept in a short slot (87% of records' names).
const SHORT: usize = 15;
/// The longest name kept whole in a long slot; the bytes after these 24 of
/// a longer one live in the arena.
const INLINE: usize = 24;
/// `meta` of a slot whose claimer is still writing it (no name's: see
/// `Table`).
const FILLING: u64 = u64::MAX;
/// A `meta` no name has (its tag byte is no short name's, and main refuses a
/// records.csv of 4 GiB or more): `fill` publishes it in a slot it has no
/// room for, so nobody waits on it forever.
const RETIRED: u64 = u32::MAX as u64 - 1;
/// Slots a worker reserves at once (see `Table::used`).
const RESERVE: usize = 64;
/// Words per stretch of the tables faulted in by one start-up task.
const STRETCH: usize = 1 << 16;
/// Ids per pair bucket.
pub const BUCKET: usize = 256;

/// A name as its table key: its first 24 raw bytes as three zero-padded
/// words (all of it, for nearly every name), its tag (length + 1) and its
/// hash. The raw bytes keep `""` escapes doubled; escaping is one-to-one, so equal raw bytes mean
/// equal values, and both empty forms have length 0.
#[derive(Clone, Copy, Default)]
pub struct Key {
    w: [u64; 3],
    tag: u32,
    hash: u32,
}

/// The key of the `len` raw name bytes at `b[start..]`. Reads at most 24
/// bytes past `start` beyond the name (the buffer's padding covers that).
#[inline(always)]
pub fn key(b: &[u8], start: usize, len: usize) -> Key {
    let w = [0, 1, 2].map(|k| word(b, start, len, k));
    // SAFETY (both intrinsics): BMI2 and SSE4.2 are part of x86-64-v3, which
    // the whole crate is compiled for (.cargo/config.toml; main.rs refuses
    // any other build).
    let crc = |h, w| unsafe { _mm_crc32_u64(h, w) };
    let mut h = crc(crc(crc(len as u64, w[0]), w[1]), w[2]);
    if len > INLINE {
        h = (3..len.div_ceil(8)).fold(h, |h, k| crc(h, word(b, start, len, k)));
    }
    let (tag, hash) = (len as u32 + 1, h as u32);
    Key { w, tag, hash }
}

/// Word `k` of the name, zero-padded past its end.
fn word(b: &[u8], start: usize, len: usize, k: usize) -> u64 {
    let bits = (8 * len.saturating_sub(8 * k)).min(64) as u32;
    // SAFETY: see `key`.
    unsafe { _bzhi_u64(load(b, start + 8 * k), bits) }
}

/// What a slot holds for key `k`, in `W`-word slots: the name words, then
/// `meta` (see `Table`), but for a long name's arena offset.
fn want<const W: usize>(k: &Key) -> [u64; W] {
    let tag = u64::from(k.tag);
    let meta = if W == 2 { tag | k.w[1] << 8 } else { tag };
    std::array::from_fn(|j| if j == W - 1 { meta } else { k.w[j] })
}

/// A worker's unused slot reservations, short and long (see `Table::used`).
pub type Budget = [usize; 2];

/// Open addressing, in two arrays of slots. Short names take 16-byte slots,
/// four to a cache line: a short name's home is a whole line, which a lookup
/// checks without a branch per slot, and finds it there for 89% of records
/// (seed 1); the rest overflowed to other lines. Longer names take 32-byte
/// slots of three name words and `meta`, probed linearly. A slot's last word
/// is its `meta`: 0 if empty, `FILLING`, or the name's tag and, for a short
/// name, its bytes 8 to 14 above the tag's byte; for a long one, where its
/// arena words start, above bit 32. A name's id is its slot's index, long
/// slots counting on after the last short one.
pub struct Table {
    short: &'static [AtomicU64],
    long: &'static [AtomicU64],
    /// The tails of names longer than `INLINE`: their unescaped length, then
    /// their words from the fourth on.
    arena: &'static [AtomicU64],
    next_word: Padded<AtomicUsize>,
    /// Slots handed out so far per array, and the caps that keep each at
    /// most 7/8 full. Workers take them `RESERVE` at a time, so that these
    /// lines do not move between cores on every insert.
    used: [Padded<AtomicUsize>; 2],
    limit: [usize; 2],
}

impl Table {
    /// An empty table with `slots` short slots (a power of two of at least
    /// `BUCKET`) and a quarter as many long ones, for a file of `file_len`
    /// bytes.
    pub fn new(slots: usize, file_len: usize) -> std::io::Result<Self> {
        // SAFETY: zero is a valid AtomicU64. The mappings are page-aligned,
        // so no slot straddles two cache lines. A long name's tail words are
        // fewer than its record's bytes / 8 plus its length word, so the
        // arena cannot overflow.
        let short = unsafe { crate::sys::zeroed(2 * slots)? };
        let long = unsafe { crate::sys::zeroed(slots)? };
        let arena = unsafe { crate::sys::zeroed(file_len / 4 + 2)? };
        let next_word = Padded(AtomicUsize::new(0));
        let used = [0, 0].map(|_| Padded(AtomicUsize::new(0)));
        let limit = [slots / 8 * 7, slots / 4 / 8 * 7];
        Ok(Table { short, long, arena, next_word, used, limit })
    }

    /// Pair buckets: ids, short and long, over `BUCKET`.
    pub fn buckets(&self) -> usize {
        (self.short.len() / 2 + self.long.len() / 4) / BUCKET
    }

    /// The 512 KiB stretches the tables are faulted in by, one per start-up
    /// task (see `touch`).
    pub fn stretches(&self) -> usize {
        (self.short.len() + self.long.len()).div_ceil(STRETCH)
    }

    /// Faults in stretch `k` of the tables before phase 1 starts, so that
    /// the workers' first lookups do not each take a page fault.
    pub fn touch(&self, k: usize) {
        let mut stretches = self.short.chunks(STRETCH).chain(self.long.chunks(STRETCH));
        stretches.nth(k).map(crate::sys::populate);
    }

    /// The array of `W`-word slots.
    fn slots<const W: usize>(&self) -> &'static [AtomicU64] {
        [self.short, self.long][W / 4]
    }

    /// The slot a lookup of `k` in the `W`-word slots starts at: the start
    /// of its home line for a short name, its home slot for a long one.
    fn home<const W: usize>(&self, k: &Key) -> usize {
        k.hash as usize & (self.slots::<W>().len() / W - 1) & if W == 2 { !3 } else { !0 }
    }

    /// Starts loading the cache line a lookup of `k` reads first.
    pub fn prefetch(&self, k: &Key) {
        match k.tag as usize - 1 <= SHORT {
            true => crate::prefetch(&self.short[2 * self.home::<2>(k)]),
            false => crate::prefetch(&self.long[4 * self.home::<4>(k)]),
        }
    }

    /// The id of the name with key `k`, inserting it if new; its raw bytes
    /// are at the start of `b`, which the buffer's padding follows.
    #[inline(always)]
    pub fn find(&self, k: &Key, b: &[u8], budget: &mut Budget) -> Res<usize> {
        if k.tag as usize - 1 > SHORT {
            return Ok(self.short.len() / 2 + self.probe::<4>(k, b, budget)?);
        }
        // The four slots of the home line at once. A slot only matches once
        // published, and the Acquire load of its `meta` makes the word
        // written before it (in `fill`) visible.
        let ([w, meta], first) = (want::<2>(k), self.home::<2>(k));
        let l = &self.short[2 * first..2 * first + 8];
        let same = |j: usize| (l[2 * j + 1].load(Acquire) == meta) & (l[2 * j].load(Relaxed) == w);
        let hit = (0..4).rev().fold(4, |hit, j| if same(j) { j } else { hit });
        match hit {
            4 => self.probe::<2>(k, b, budget),
            _ => Ok(first + hit),
        }
    }

    /// Probes the `W`-word slots for the name with key `k`, inserting it in
    /// the first empty slot if it is not there; returns the slot's index.
    /// Long slots are probed linearly from the home slot. Short ones by
    /// lines: the home line, then lines `step` (odd, so every line comes up
    /// in turn) apart, not the next one: linear probing by lines would pile
    /// names up in runs of full lines. A long slot's `meta` is compared in its
    /// low 32 bits, the tag.
    fn probe<const W: usize>(&self, k: &Key, b: &[u8], budget: &mut Budget) -> Res<usize> {
        let (words, want, len) = (self.slots::<W>(), want::<W>(k), k.tag as usize - 1);
        let (mask, step) = (words.len() / W - 1, (k.hash >> 16 | 1) as usize);
        let mut i = self.home::<W>(k);
        loop {
            let slot = &words[W * i..W * i + W];
            let mut meta = slot[W - 1].load(Acquire);
            if meta == 0 {
                // Claim the slot first, so that only one worker writes it.
                match slot[W - 1].compare_exchange(0, FILLING, Acquire, Acquire) {
                    Ok(_) => return self.fill::<W>(i, k, b, budget).map(|_| i),
                    Err(now) => meta = now,
                }
            }
            while meta == FILLING {
                std::hint::spin_loop(); // a few stores away from published
                meta = slot[W - 1].load(Acquire);
            }
            // The Acquire load of a published `meta` makes the words written
            // before its Release store (in `fill`) visible here.
            let same_meta = meta & if W == 2 { !0 } else { 0xFFFF_FFFF } == want[W - 1];
            let same = same_meta && (0..W - 1).all(|j| slot[j].load(Relaxed) == want[j]);
            if same && (len <= INLINE || self.tail_is(meta, k, b)) {
                return Ok(i);
            }
            let next_line = W == 2 && i % 4 == 3;
            i = (if next_line { i - 3 + 4 * step } else { i + 1 }) & mask;
        }
    }

    /// Writes the name into slot `i`, which this worker has claimed, then
    /// publishes it. `meta` changes only through the claiming CAS and the
    /// Release store below, and every other word is written before that
    /// store, so a lookup that sees the published `meta` sees the whole name.
    #[cold]
    fn fill<const W: usize>(&self, i: usize, k: &Key, b: &[u8], budget: &mut Budget) -> Res<()> {
        let (kind, want, len) = (W / 4, want::<W>(k), k.tag as usize - 1);
        let slot = &self.slots::<W>()[W * i..W * i + W];
        if budget[kind] == 0 {
            if self.used[kind].0.fetch_add(RESERVE, Relaxed) + RESERVE > self.limit[kind] {
                slot[W - 1].store(RETIRED, Release);
                return Err(Fail::Full);
            }
            budget[kind] = RESERVE;
        }
        budget[kind] -= 1;
        (0..W - 1).for_each(|j| slot[j].store(want[j], Relaxed));
        let mut meta = want[W - 1];
        if len > INLINE {
            let unescaped = len - b[..len].iter().filter(|&&c| c == b'"').count() / 2;
            let tail = (3..len.div_ceil(8)).map(|j| word(b, 0, len, j));
            let at = self.next_word.0.fetch_add(len.div_ceil(8) - 2, Relaxed);
            let words = std::iter::once(unescaped as u64).chain(tail);
            let dst = &self.arena[at..];
            words.zip(dst).for_each(|(w, a)| a.store(w, Relaxed));
            meta |= (at as u64) << 32;
        }
        slot[W - 1].store(meta, Release);
        Ok(())
    }

    /// Whether the arena tail of a published long name matches key `k`'s.
    fn tail_is(&self, meta: u64, k: &Key, b: &[u8]) -> bool {
        let (len, arena) = (k.tag as usize - 1, &self.arena[(meta >> 32) as usize + 1..]);
        let tail = (3..len.div_ceil(8)).map(|j| word(b, 0, len, j));
        tail.zip(arena).all(|(w, a)| a.load(Relaxed) == w)
    }

    /// The unescaped length of the name with id `id` (0 for an empty slot):
    /// its length less half its quote bytes, since inside the delimiting
    /// quotes every quote byte is half of a `""`. Only called once every
    /// worker has finished phase 1.
    fn unescaped(&self, id: usize) -> u64 {
        let (name, len) = match id.checked_sub(self.short.len() / 2) {
            None => {
                let [w, meta] = [0, 1].map(|j| self.short[2 * id + j].load(Relaxed));
                ([w, meta >> 8, 0], meta & 0xFF)
            }
            Some(j) => {
                let [w0, w1, w2, meta] = [0, 1, 2, 3].map(|k| self.long[4 * j + k].load(Relaxed));
                if meta as u32 as usize > INLINE + 1 {
                    return self.arena[(meta >> 32) as usize].load(Relaxed);
                }
                ([w0, w1, w2], meta & 0xFFFF_FFFF)
            }
        };
        len.saturating_sub(1) - u64::from(name.map(quote_bytes).iter().sum::<u32>() / 2)
    }

    /// Phase 2 for one bucket: counts its pairs from every worker into `out`.
    /// `bits` must be all zero, and is left so.
    pub fn count(&self, all: &[&Pairs], bucket: usize, bits: &mut [u64; 1024], out: &mut [Counts]) {
        let lens: [u64; BUCKET] = std::array::from_fn(|j| self.unescaped(bucket * BUCKET + j));
        for pairs in all {
            pairs.each(bucket, |e| {
                let (e, c) = (usize::from(e), &mut out[usize::from(e >> 8)]);
                let (word, bit) = (&mut bits[e / 64], 1 << (e % 64));
                (c[0], c[1]) = (c[0] + 1, c[1] + u64::from(*word & bit == 0));
                (c[2], *word) = (c[2] + lens[e % BUCKET], *word | bit);
            });
        }
        bits.fill(0);
    }
}

/// The number of `"` bytes in `w`: `y` has bit 7 clear exactly in the bytes
/// where `x` is zero, with no carry between bytes.
fn quote_bytes(w: u64) -> u32 {
    const LOW7: u64 = 0x7F7F_7F7F_7F7F_7F7F;
    let x = w ^ 0x2222_2222_2222_2222;
    let y = ((x & LOW7) + LOW7) | x | LOW7;
    (!y).count_ones()
}

/// u16s per pair block: a 32-bit link to the bucket's previous block, then
/// 126 entries. 256 bytes, so a block is four whole cache lines.
const BLOCK: usize = 128;

/// One worker's pairs. Each bucket is a chain of blocks in the worker's own
/// arena; `cur[bucket]` is where its next entry goes, or 0 before its first.
/// An entry at a block boundary means the block is full. `n` blocks are in
/// use.
pub struct Pairs {
    cur: Vec<u32>,
    arena: &'static mut [u16],
    n: usize,
}

impl Pairs {
    /// Room for every record of a file of `file_len` bytes (a record takes
    /// at least 11) in `buckets` buckets, though only what is used is ever
    /// backed by memory.
    pub fn new(buckets: usize, file_len: usize) -> std::io::Result<Self> {
        let blocks = file_len / 11 / (BLOCK - 2) + buckets + 1;
        // SAFETY: zero is a valid u16.
        let (cur, arena) = (vec![0; buckets], unsafe { crate::sys::zeroed(blocks * BLOCK)? });
        Ok(Pairs { cur, arena, n: 0 })
    }

    /// Faults in the first `bytes` of the arena, before phase 1 starts.
    pub fn touch(&self, bytes: usize) {
        crate::sys::populate(&self.arena[..(bytes / 2).min(self.arena.len())]);
    }

    #[inline(always)]
    pub fn push(&mut self, id: usize, group: u8) {
        let bucket = &mut self.cur[id / BUCKET];
        let mut p = *bucket as usize;
        if p % BLOCK == 0 {
            // A new block for a bucket whose cursor is 0 or the end of its
            // full block; its link is that block's index + 1, or 0.
            let (at, link) = (self.n * BLOCK, p / BLOCK);
            (self.arena[at], self.arena[at + 1]) = (link as u16, (link >> 16) as u16);
            (self.n, p) = (self.n + 1, at + 2);
        }
        self.arena[p] = u16::from(group) << 8 | (id % BUCKET) as u16;
        *bucket = p as u32 + 1;
    }

    /// Every entry of `bucket`, newest block first.
    fn each(&self, bucket: usize, mut f: impl FnMut(u16)) {
        let mut p = self.cur[bucket] as usize;
        while p != 0 {
            let at = (p - 1) / BLOCK * BLOCK;
            self.arena[at + 2..p].iter().for_each(|&e| f(e));
            p = (usize::from(self.arena[at]) | usize::from(self.arena[at + 1]) << 16) * BLOCK;
        }
    }
}

/// What phase 2 adds to one group: records, distinct names, name bytes.
pub type Counts = [u64; 3];
