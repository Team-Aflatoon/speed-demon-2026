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

use crate::parse::load;
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
/// within this many of its home is full (see `Table::probe`).
const PROBES: usize = 1024;
/// Short slots: room for 131,072 short names and 32,768 long ones. The
/// spec's pool has 100,000 names, of which about 87% are short (up to 15
/// bytes) and the rest at most 27 bytes long, so the table is at most about
/// two-thirds full; anything that fills it stops the program.
const SLOTS: usize = 1 << 17;
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
}

/// Open addressing, in two arrays of slots in one mapping. Short names take
/// 16-byte slots, four to a cache line: a short name's home is a whole line,
/// which a lookup checks without a branch per slot, and finds it there for
/// 89% of records (seed 1); the rest overflowed to other lines. Longer names
/// take 32-byte slots of three name words and `meta`, probed linearly. A
/// slot's last word is its `meta`: 0 if empty, `FILLING`, or the name's tag
/// and, for a short name, its bytes 8 to 14 above the tag's byte. A name's
/// id is its slot's index, long slots counting on after the short ones, and
/// huge names after those.
pub struct Table {
    words: &'static [AtomicU64],
    /// For `key`: the masks of a hash to its home word, short and long, and
    /// where long slots start (twice the short slots, a power of two; a
    /// quarter as many long ones follow).
    homes: [usize; 3],
    /// Names over `INLINE` bytes, by id, and their unescaped lengths.
    huge: Mutex<(HashMap<Box<[u8]>, usize>, Vec<u64>)>,
}

impl Table {
    /// An empty table: `SLOTS` short slots, a quarter as many long ones, and
    /// room for an eighth as many huge names.
    pub fn new() -> Self {
        let short = SLOTS;
        // SAFETY: zero is a valid AtomicU64. The mapping is page-aligned, so
        // no slot straddles two cache lines.
        let words = unsafe { crate::mapped(3 * short, -1) };
        let (homes, huge) = ([2 * short - 8, short / 4 - 1, 2 * short], Mutex::default());
        Table { words, homes, huge }
    }

    /// Ids of each kind: short slots, long slots, huge names.
    fn ids(&self) -> [usize; 3] {
        let short = self.homes[2] / 2;
        [short, short / 4, short / 8]
    }

    /// Pair buckets: all ids over `BUCKET`.
    pub fn buckets(&self) -> usize {
        self.ids().iter().sum::<usize>() / BUCKET
    }

    /// Faults in share `k` of `n` of the table before phase 1 starts, so that
    /// the workers' first lookups do not each take a page fault.
    pub fn touch(&self, k: usize, n: usize) {
        let mut shares = self.words.chunks(self.words.len().div_ceil(n));
        crate::populate(shares.nth(k).unwrap_or(&[]));
    }

    /// The key of the `len` raw name bytes at `b[start..]`. Reads at most 24
    /// bytes past `start` beyond the name (the buffer's padding covers that).
    /// Its lookup starts at its home line for a short name, at its home slot
    /// for a long one.
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
        // Without a branch: the kind is as unpredictable as the names.
        let [short, long, long_at] = self.homes;
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
    pub fn find(&self, k: &Key, b: &[u8]) -> usize {
        let (len, at) = (k.tag as usize - 1, k.at as usize);
        if len > SHORT {
            // A long name's home slot, the same way, before any probing.
            let want = k.slot::<4>();
            // SAFETY: `key` masks `at` to a slot within the long slots.
            let slot = unsafe { self.words.get_unchecked(at..at + 4) };
            let same = |j: usize| slot[j].load(Acquire) == want[j];
            let short = self.homes[2] / 2;
            return match len {
                _ if same(3) && same(0) && same(1) && same(2) => short + (at - 2 * short) / 4,
                ..=INLINE => self.probe::<4>(k, false),
                _ => self.huge(&b[..len]),
            };
        }
        // The four slots of the home line at once. A slot only matches once
        // published, and the Acquire load of its `meta` makes the word
        // written before it (in `probe`) visible.
        let [w, meta] = k.slot::<2>();
        // SAFETY: `key` masks `at` to a line start within the short slots.
        let l = unsafe { self.words.get_unchecked(at..at + 8) };
        let metas = [0, 1, 2, 3].map(|j| l[2 * j + 1].load(Acquire));
        let same = |j: usize| (metas[j] == meta) & (l[2 * j].load(Relaxed) == w);
        let hit = (0..4).rev().fold(4, |hit, j| if same(j) { j } else { hit });
        // A line of four published names, none of them this one, is full for
        // good: the probe can start at the next line.
        let full = metas.iter().all(|&m| m.wrapping_add(1) > 1);
        match hit {
            4 => self.probe::<2>(k, full),
            _ => at / 2 + hit,
        }
    }

    /// The id of a name of more than `INLINE` bytes, given as `name`.
    #[cold]
    fn huge(&self, name: &[u8]) -> usize {
        let [short, long, huge] = self.ids();
        let mut lock = self.huge.lock().unwrap();
        let (ids, lens) = &mut *lock;
        if let Some(&id) = ids.get(name) {
            return id;
        }
        if lens.len() == huge {
            crate::die("more than 16,384 distinct names over 24 bytes");
        }
        lens.push(unescaped(name));
        ids.insert(name.into(), short + long + lens.len() - 1);
        short + long + lens.len() - 1
    }

    /// Probes the `W`-word slots for the name with key `k`, inserting it in
    /// the first empty slot if it is not there; returns its id. Long slots
    /// are probed linearly from the home slot; short ones by lines, the home
    /// line (unless `skip`: it is full), then lines `step` (odd, so every line comes up in turn) apart,
    /// not the next one: linear probing by lines would pile names up in runs
    /// of full lines.
    ///
    /// A worker inserts by claiming an empty slot's `meta` (0 to `FILLING`)
    /// with a CAS, so only it writes the slot; nothing else ever writes
    /// `meta`. A name not found within `PROBES` slots means the table is
    /// full: every lookup of a name follows the same sequence, so a name in
    /// the table is always within the `PROBES` its insertion visited.
    #[cold]
    fn probe<const W: usize>(&self, k: &Key, skip: bool) -> usize {
        let (base, want) = (W / 4 * self.homes[2], k.slot::<W>());
        let (mask, words) = (self.ids()[W / 4] - 1, &self.words[base..]);
        let mix = (k.w[0] ^ k.w[1] ^ k.w[2]).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let step = (mix >> 40 | 1) as usize;
        let mut i = ((k.at as usize - base) / W + if skip { 4 * step } else { 0 }) & mask;
        for _ in 0..PROBES {
            let slot = &words[W * i..W * i + W];
            let mut meta = slot[W - 1].load(Acquire);
            if meta == 0 {
                // Claim the slot first, so that only one worker writes it.
                if let Err(now) = slot[W - 1].compare_exchange(0, FILLING, Acquire, Acquire) {
                    meta = now;
                } else {
                    // Every word before the Release store of `meta`, so a
                    // lookup that sees `meta` published sees the name.
                    (0..W - 1).for_each(|j| slot[j].store(want[j], Relaxed));
                    slot[W - 1].store(want[W - 1], Release);
                    return base / 2 + i;
                }
            }
            while meta == FILLING {
                std::hint::spin_loop(); // a few stores away from published
                meta = slot[W - 1].load(Acquire);
            }
            // The Acquire load of a published `meta` makes the words written
            // before its Release store (above) visible here.
            if meta == want[W - 1] && (0..W - 1).all(|j| slot[j].load(Relaxed) == want[j]) {
                return base / 2 + i;
            }
            let next_line = W == 2 && i % 4 == 3;
            i = (if next_line { i - 3 + 4 * step } else { i + 1 }) & mask;
        }
        crate::die("more distinct names than the name table holds")
    }

    /// Phase 2 for one bucket: counts its pairs from every worker into `out`:
    /// distinct names by group in `out[0]`, and in `out[1]` records above
    /// bit 32 and name bytes below it (fewer than 2^32 of each, as
    /// records.csv is under 4 GiB: see main). `bits` must be all zero, and is
    /// left so. Only called once every worker has finished phase 1.
    pub fn count(&self, all: &[&Pairs], bucket: usize, bits: &mut Bits, out: &mut [[u64; 256]; 2]) {
        let [short, long, _] = self.ids();
        // Locked once per bucket, and only for the huge names' buckets.
        let huge = (bucket * BUCKET >= short + long).then(|| self.huge.lock().unwrap());
        // Each id's unescaped name length: its length less half its quote
        // bytes, since inside the delimiting quotes every quote byte is half
        // of a `""`. 0 for an empty slot.
        let lens: [u64; BUCKET] = std::array::from_fn(|j| {
            let (id, word) = (bucket * BUCKET + j, |k: usize| self.words[k].load(Relaxed));
            let j = id.wrapping_sub(short); // a long slot's index, if `id` is one
            let [w0, w1, w2, meta] = match id {
                _ if id < short => [word(2 * id), word(2 * id + 1) >> 8, 0, word(2 * id + 1)],
                _ if j < long => std::array::from_fn(|k| word(2 * short + 4 * j + k)),
                _ => return 1 << 32 | huge.as_ref().and_then(|h| h.1.get(j - long)).unwrap_or(&0),
            };
            let quotes: u32 = [w0, w1, w2].map(quote_bytes).iter().sum();
            1 << 32 | (u64::from((meta as u8).saturating_sub(1)) - u64::from(quotes / 2))
        });
        drop(huge);
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

/// The unescaped length of a quoted field's raw bytes (see `Table::count`).
fn unescaped(raw: &[u8]) -> u64 {
    (raw.len() - raw.iter().filter(|&&c| c == b'"').count() / 2) as u64
}

/// Phase 2's bitset: a bit per (id in a bucket, group).
pub type Bits = [u64; BUCKET * 256 / 64];

/// u16s per pair block: a 32-bit link to the bucket's previous block, then
/// 62 entries. 128 bytes, so a block is two whole cache lines, and a bucket's
/// last, partly filled block wastes 64 bytes on average.
const BLOCK: usize = 64;

/// One worker's (id, group) pairs, by bucket of `BUCKET` ids: entry
/// `group << 8 | id % BUCKET`. Each bucket is a chain of blocks in the
/// worker's own arena, a mapping of its own (private writes, no allocator
/// locks); `cur[bucket]` is where its next entry goes, or 0 before its first.
/// An entry at a block boundary means the block is full. `n` blocks are in
/// use.
pub struct Pairs {
    cur: Vec<u32>,
    arena: &'static mut [u16],
    n: usize,
}

impl Pairs {
    /// Room for every record of a file of `len` bytes (a record takes at
    /// least 11) in `buckets` buckets, backed by memory only where used; the
    /// first `touch` entries are faulted in now.
    pub fn new(buckets: usize, len: usize, touch: usize) -> Self {
        // SAFETY: zero is a valid u16.
        let blocks = len / 11 / (BLOCK - 2) + buckets + 1;
        let arena: &mut [u16] = unsafe { crate::mapped(blocks * BLOCK, -1) };
        crate::populate(&arena[..touch.min(arena.len())]);
        Pairs { cur: vec![0; buckets], arena, n: 0 }
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
            // The next block (another core wrote it) loads while this one is
            // counted.
            let next = (link * BLOCK).wrapping_sub(BLOCK);
            (crate::prefetch(self.arena, next), crate::prefetch(self.arena, next + 32));
            self.arena[at + 2..p].iter().for_each(|&e| f(e));
            p = link * BLOCK;
        }
    }
}

/// What phase 2 adds to one group (see `Table::count`).
pub type Counts = [u64; 2];
