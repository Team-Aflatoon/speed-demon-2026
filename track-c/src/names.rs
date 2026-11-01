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
//! its name's unescaped length to its group, in one add.

use crate::parse::{load, Fail, Res};
use std::arch::x86_64::{_bzhi_u64, _mm_crc32_u64};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::*};
use std::sync::Mutex;

/// The longest name kept in a short slot (87% of records' names).
const SHORT: usize = 15;
/// The longest name kept in a long slot; longer ones (1 in 5,000 records)
/// are interned under a lock (see `Table::huge`).
const INLINE: usize = 24;
/// `meta` of a slot whose claimer is still writing it (no name's: see
/// `Table`).
const FILLING: u64 = u64::MAX;
/// The most slots a lookup visits: a table so full that a name is not
/// within this many of its home counts as full (see `Table::probe`).
const PROBES: usize = 1024;
/// Words per stretch of the tables faulted in by one start-up task.
const STRETCH: usize = 1 << 16;
/// Ids per pair bucket.
const BUCKET: usize = 256;

/// A name as its table key: its first 24 raw bytes as three zero-padded
/// words (all of it, but for the rare huge name), its tag (length + 1) and
/// the word of the table its lookup starts at (see `Table::key`). The raw
/// bytes keep `""` escapes doubled; escaping is one-to-one, so equal raw
/// bytes mean equal values, and both empty forms have length 0.
#[derive(Clone, Copy, Default)]
pub struct Key {
    w: [u64; 3],
    tag: u32,
    at: u32,
}

impl Key {
    /// What a slot holds for this key, in `W`-word slots: the name words,
    /// then `meta` (see `Table`).
    fn slot<const W: usize>(&self) -> [u64; W] {
        let meta = u64::from(self.tag) | if W == 2 { self.w[1] << 8 } else { 0 };
        std::array::from_fn(|j| if j == W - 1 { meta } else { self.w[j] })
    }

    fn len(&self) -> usize {
        self.tag as usize - 1
    }
}

/// Open addressing, in two arrays of slots. Short names take 16-byte slots,
/// four to a cache line: a short name's home is a whole line, which a lookup
/// checks without a branch per slot, and finds it there for 89% of records
/// (seed 1); the rest overflowed to other lines. Longer names take 32-byte
/// slots of three name words and `meta`, probed linearly. A slot's last word
/// is its `meta`: 0 if empty, `FILLING`, or the name's tag and, for a short
/// name, its bytes 8 to 14 above the tag's byte. A name's id is its slot's
/// index, long slots counting on after the short ones, and huge names after
/// those.
pub struct Table {
    /// Both arrays, short slots first, in one mapping.
    words: &'static [AtomicU64],
    short: &'static [AtomicU64],
    long: &'static [AtomicU64],
    /// For `key`: the masks of a hash to its home word, short and long, and
    /// where long slots start.
    homes: [usize; 3],
    /// Names over `INLINE` bytes, by id, and their unescaped lengths.
    huge: Mutex<(HashMap<Box<[u8]>, usize>, Vec<u64>)>,
}

impl Table {
    /// An empty table with `slots` short slots (a power of two of at least
    /// `BUCKET`), a quarter as many long ones, and room for an eighth as many
    /// huge names.
    pub fn new(slots: usize) -> std::io::Result<Self> {
        // SAFETY: zero is a valid AtomicU64. The mappings are page-aligned,
        // so no slot straddles two cache lines.
        let words: &[AtomicU64] = unsafe { crate::sys::zeroed(3 * slots)? };
        let (short, long) = words.split_at(2 * slots);
        let homes = [2 * slots - 8, slots / 4 - 1, 2 * slots];
        Ok(Table { words, short, long, homes, huge: Mutex::default() })
    }

    /// Ids of each kind: short slots, long slots, huge names.
    fn ids(&self) -> [usize; 3] {
        let short = self.short.len() / 2;
        [short, self.long.len() / 4, short / 8]
    }

    /// Pair buckets: all ids over `BUCKET`.
    pub fn buckets(&self) -> usize {
        self.ids().iter().sum::<usize>() / BUCKET
    }

    /// The 512 KiB stretches the tables are faulted in by, one per start-up
    /// task (see `touch`).
    pub fn stretches(&self) -> usize {
        self.words.len().div_ceil(STRETCH)
    }

    /// Faults in stretch `k` of the tables before phase 1 starts, so that
    /// the workers' first lookups do not each take a page fault.
    pub fn touch(&self, k: usize) {
        self.words.chunks(STRETCH).nth(k).map(crate::sys::populate);
    }

    /// The key of the `len` raw name bytes at `b[start..]`. Reads at most 24
    /// bytes past `start` beyond the name (the buffer's padding covers that).
    /// Its lookup starts at the start of its home line for a short name, at
    /// its home slot for a long one.
    #[inline(always)]
    pub fn key(&self, b: &[u8], start: usize, len: usize) -> Key {
        // `bzhi` keeps the low `bits` bits, and the whole word from 64 on;
        // it reads only the index's low byte, so the words of a name over 31
        // bytes are wrong, but only huge names (see `huge`) are that long.
        let bits = |k: usize| (8 * len as u32).saturating_sub(64 * k as u32);
        // SAFETY (both intrinsics, and the loads): BMI2 and SSE4.2 are part
        // of x86-64-v3, which the whole crate is compiled for (main.rs
        // refuses any other build); the loads end at most 24 bytes past the
        // name (see `load`).
        let w = [0, 1, 2].map(|k| unsafe { _bzhi_u64(load(b, start + 8 * k), bits(k)) });
        let crc = |h, w| unsafe { _mm_crc32_u64(h, w) };
        let h = crc(crc(crc(len as u64, w[0]), w[1]), w[2]) as usize;
        let [short, long, long_at] = self.homes;
        // Without a branch: the kind is as unpredictable as the names.
        let long_name = 0usize.wrapping_sub(usize::from(len > SHORT));
        let at = 2 * h & short & !long_name | (long_at + 4 * (h & long)) & long_name;
        let (tag, at) = (len as u32 + 1, at as u32);
        Key { w, tag, at }
    }

    /// Starts loading the cache line a lookup of `k` reads first.
    pub fn prefetch(&self, k: &Key) {
        crate::prefetch(self.words, k.at as usize);
    }

    /// The id of the name with key `k`, inserting it if new; its raw bytes
    /// are at the start of `b`.
    #[inline(always)]
    pub fn find(&self, k: &Key, b: &[u8]) -> Res<usize> {
        if k.len() > SHORT {
            // A long name's home slot, the same way, before any probing.
            let (want, at) = (k.slot::<4>(), k.at as usize);
            // SAFETY: `key` masks `at` to a slot within the long slots.
            let slot = unsafe { self.words.get_unchecked(at..at + 4) };
            if [3, 0, 1, 2].iter().all(|&j| slot[j].load(Acquire) == want[j]) {
                return Ok(self.ids()[0] + (at - self.short.len()) / 4);
            }
            return self.long_name(k, b);
        }
        // The four slots of the home line at once. A slot only matches once
        // published, and the Acquire load of its `meta` makes the word
        // written before it (in `probe`) visible.
        let ([w, meta], first) = (k.slot::<2>(), k.at as usize / 2);
        // SAFETY: `key` masks `at` to a line start within the short slots.
        let l = unsafe { self.words.get_unchecked(2 * first..2 * first + 8) };
        let metas = [0, 1, 2, 3].map(|j| l[2 * j + 1].load(Acquire));
        let same = |j: usize| (metas[j] == meta) & (l[2 * j].load(Relaxed) == w);
        let hit = (0..4).rev().fold(4, |hit, j| if same(j) { j } else { hit });
        // A line of four published names, none of them this one, is full for
        // good: the probe can start at the next line.
        let full = metas.iter().all(|&m| m.wrapping_add(1) > 1);
        match hit {
            4 => self.probe::<2>(k, full),
            _ => Ok(first + hit),
        }
    }

    /// `find` for a name of more than `SHORT` bytes.
    #[cold]
    fn long_name(&self, k: &Key, b: &[u8]) -> Res<usize> {
        let [short, long, huge] = self.ids();
        if k.len() <= INLINE {
            return Ok(short + self.probe::<4>(k, false)?);
        }
        let mut lock = self.huge.lock().unwrap();
        let (ids, lens) = &mut *lock;
        if let Some(&id) = ids.get(&b[..k.len()]) {
            return Ok(id);
        }
        if lens.len() == huge {
            return Err(Fail::Full);
        }
        lens.push(unescaped(&b[..k.len()]));
        ids.insert(b[..k.len()].into(), short + long + lens.len() - 1);
        Ok(short + long + lens.len() - 1)
    }

    /// Probes the `W`-word slots for the name with key `k`, inserting it in
    /// the first empty slot if it is not there; returns the slot's index.
    /// Long slots are probed linearly from the home slot. Short ones by
    /// lines: the home line (or the next one if `skip`), then lines `step`
    /// (odd, so every line comes up in turn) apart, not the next one: linear
    /// probing by lines would pile names up in runs of full lines.
    ///
    /// A worker inserts by claiming an empty slot's `meta` (0 to `FILLING`)
    /// with a CAS, so only it writes the slot; nothing else ever writes
    /// `meta`. A name not found within `PROBES` slots makes the table count
    /// as full, and the run is retried with a larger one: every lookup of a
    /// name follows the same sequence, so a name in the table is always
    /// within the `PROBES` its insertion visited.
    fn probe<const W: usize>(&self, k: &Key, skip: bool) -> Res<usize> {
        let (words, want) = ([self.short, self.long][W / 4], k.slot::<W>());
        let mix = (k.w[0] ^ k.w[1] ^ k.w[2]).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let (mask, step) = (words.len() / W - 1, (mix >> 40 | 1) as usize);
        let home = (k.at as usize - (W / 4) * self.short.len()) / W;
        let mut i = (home + if skip { 4 * step } else { 0 }) & mask;
        for _ in 0..PROBES {
            let slot = &words[W * i..W * i + W];
            let mut meta = slot[W - 1].load(Acquire);
            if meta == 0 {
                // Claim the slot first, so that only one worker writes it.
                match slot[W - 1].compare_exchange(0, FILLING, Acquire, Acquire) {
                    Ok(_) => {
                        // Every word before the Release store of `meta`, so a
                        // lookup that sees `meta` published sees the name.
                        (0..W - 1).for_each(|j| slot[j].store(want[j], Relaxed));
                        slot[W - 1].store(want[W - 1], Release);
                        return Ok(i);
                    }
                    Err(now) => meta = now,
                }
            }
            while meta == FILLING {
                std::hint::spin_loop(); // a few stores away from published
                meta = slot[W - 1].load(Acquire);
            }
            // The Acquire load of a published `meta` makes the words written
            // before its Release store (above) visible here.
            if meta == want[W - 1] && (0..W - 1).all(|j| slot[j].load(Relaxed) == want[j]) {
                return Ok(i);
            }
            let next_line = W == 2 && i % 4 == 3;
            i = (if next_line { i - 3 + 4 * step } else { i + 1 }) & mask;
        }
        Err(Fail::Full)
    }

    /// The unescaped lengths of the names with ids `from..from + BUCKET` (0
    /// for an empty slot). Only called once every worker has finished phase
    /// 1.
    fn lengths(&self, from: usize) -> [u64; BUCKET] {
        let [short, long, _] = self.ids();
        let huge = self.huge.lock().unwrap();
        std::array::from_fn(|j| {
            let id = from + j;
            let [w0, w1, w2, meta] = if id < short {
                let [w, meta] = [0, 1].map(|k| self.short[2 * id + k].load(Relaxed));
                [w, meta >> 8, 0, meta]
            } else if id < short + long {
                std::array::from_fn(|k| self.long[4 * (id - short) + k].load(Relaxed))
            } else {
                return huge.1.get(id - short - long).copied().unwrap_or(0);
            };
            let quotes: u32 = [w0, w1, w2].map(quote_bytes).iter().sum();
            u64::from((meta as u8).saturating_sub(1)) - u64::from(quotes / 2)
        })
    }

    /// Phase 2 for one bucket: counts its pairs from every worker into `out`
    /// (by group, both halves of its `Counts`).
    /// `bits` must be all zero, and is left so.
    pub fn count(&self, all: &[&Pairs], bucket: usize, bits: &mut Bits, out: &mut [[u64; 256]; 2]) {
        // A record, at bit 32, and its name's bytes (see `Counts`).
        let lens = self.lengths(bucket * BUCKET).map(|len| 1 << 32 | len);
        for pairs in all {
            pairs.each(bucket, |e| {
                let (e, group) = (usize::from(e), usize::from(e >> 8));
                let (word, bit) = (&mut bits[e / 64], 1 << (e % 64));
                // Two arrays, not one of pairs, so that the adds stay scalar.
                out[0][group] += u64::from(*word & bit == 0);
                (out[1][group], *word) = (out[1][group] + lens[e % BUCKET], *word | bit);
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
    (!(((x & LOW7) + LOW7) | x | LOW7)).count_ones()
}

/// The unescaped length of a quoted field's raw bytes: inside the
/// delimiting quotes every quote byte is half of a `""`.
fn unescaped(raw: &[u8]) -> u64 {
    (raw.len() - raw.iter().filter(|&&c| c == b'"').count() / 2) as u64
}

/// u16s per pair block: a 32-bit link to the bucket's previous block, then
/// 62 entries. 128 bytes, so a block is two whole cache lines, and a bucket's
/// last, partly filled block wastes 64 bytes on average.
const BLOCK: usize = 64;

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
        // SAFETY: every id has a bucket (`Table::buckets`).
        let bucket = unsafe { self.cur.get_unchecked_mut(id / BUCKET) };
        let mut p = *bucket as usize;
        if p % BLOCK == 0 {
            // A new block for a bucket whose cursor is 0 or the end of its
            // full block; its link is that block's index + 1, or 0.
            let (at, link) = (self.n * BLOCK, p / BLOCK);
            (self.arena[at], self.arena[at + 1]) = (link as u16, (link >> 16) as u16);
            (self.n, p) = (self.n + 1, at + 2);
        }
        // SAFETY: the arena has room for every record (see `new`), and `p`
        // is within the block of this bucket that has room.
        *unsafe { self.arena.get_unchecked_mut(p) } = u16::from(group) << 8 | (id % BUCKET) as u16;
        *bucket = p as u32 + 1;
    }

    /// Every entry of `bucket`, newest block first.
    fn each(&self, bucket: usize, mut f: impl FnMut(u16)) {
        let mut p = self.cur[bucket] as usize;
        while p != 0 {
            let at = (p - 1) / BLOCK * BLOCK;
            let link = usize::from(self.arena[at]) | usize::from(self.arena[at + 1]) << 16;
            let next = link * BLOCK;
            // The next block (another core wrote it) loads while this one is
            // counted.
            (0..BLOCK).step_by(32).for_each(|k| crate::prefetch(self.arena, next.wrapping_sub(BLOCK) + k));
            self.arena[at + 2..p].iter().for_each(|&e| f(e));
            p = next;
        }
    }
}

/// What phase 2 adds to one group: distinct names, then records above bit
/// 32 and name bytes below it (fewer than 2^32 of each, as records.csv is
/// under 4 GiB: see main).
pub type Counts = [u64; 2];

/// Phase 2's bitset: a bit per (id in a bucket, group).
pub type Bits = [u64; BUCKET * 256 / 64];
