//! Distinct names, counted as the records are parsed, with no second pass.
//!
//! A name is not stored: it is identified by a 64-bit hash of its raw bytes
//! and length (`Key`), and a record finds it in one shared lock-free table
//! (`Table`), which gives each new name the next dense id. A group's distinct
//! names are the set bits of its row of one shared bit matrix, a bit per
//! (group, id) pair; a record sets its pair's bit with an atomic OR, and the
//! OR says whether the bit was clear, so the record counts the pair then and
//! nothing is counted afterwards.
//!
//! Hash identity: two names are one if their hashes agree on 62 bits (15 for
//! the bucket, 47 for the stored tag). For the spec's pool of 100,000 names
//! the chance that any two do is n^2 / 2 / 2^62, about 1.1e-9 a dataset. The
//! hash is fixed and unseeded, so a dataset gives the same output every run.
//!
//! The table and the matrix share one 6 MiB mapping on three huge pages, so
//! that the random reads and bit writes of 3 million records touch few pages
//! and few TLB entries; each page is faulted in by one start-up task (see
//! `Table::touch`).
//!
//! Concurrency. A slot is one aligned `AtomicU64` that changes exactly once,
//! from 0 to its final value, by a compare-and-swap, and nothing else writes
//! it: a reader sees it empty or complete, never half filled. A slot is
//! claimed in the order of its line and its bucket's lines (`Table::insert`),
//! and a full line stays full, so two workers inserting one name walk the same
//! slots in the same order and meet at the first slot either claims: the
//! loser of that compare-and-swap reads the winner's value and finds its own
//! tag there. A dense id is spent only by a successful compare-and-swap, and
//! ids are handed out in disjoint blocks, so no id names two names; an id
//! left unused only leaves an empty column in the matrix. Matrix bits are set
//! with `fetch_or`, an atomic read-modify-write: in a word's modification
//! order exactly one OR finds a given bit clear, so each (group, id) pair is
//! counted once. The one place that relies on the hardware, not on Rust's
//! memory model, is the fast path of `Table::find`, which reads a line's
//! eight slots with two AVX2 loads: an aligned 8-byte lane is read whole on
//! x86, so each lane is 0 or final. A slot published a moment after its
//! lane was read only sends the lookup to `insert`, which reads atomically,
//! and a hit's id is read again through the atomic slot before it is used.

use crate::parse::load;
use crate::{mapped_huge, GROUPS};
use std::arch::x86_64::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};

/// Buckets: one cache line of 8 one-word slots each (2 MiB all together).
const BUCKETS: usize = 1 << 15;
/// Dense ids: bit columns of the matrix, so the most distinct names it holds
/// (a slot stores an id plus one in 17 bits).
const DENSE: usize = 1 << 17;
/// Words in a group's row of the matrix.
pub const ROW: usize = DENSE / 64;
/// Words in the table and the matrix, the table first.
const WORDS: usize = 8 * BUCKETS + GROUPS * ROW;
/// Words in a 2 MiB huge page.
const HUGE: usize = 1 << 18;
/// Huge pages in the mapping (see `Table::touch`).
pub const PAGES: usize = WORDS / HUGE;
/// Dense ids a worker reserves at once.
const BLOCK: usize = 64;
// The mapping must not end in the middle of a huge page.
const _: () = assert!(WORDS % HUGE == 0);

/// The dense ids a worker has reserved and not yet used, as `[next, end)`.
pub type Ids = [usize; 2];

/// A name's identity: the hash of its raw bytes and length. The raw bytes
/// keep `""` escapes doubled; escaping is one-to-one, so equal raw bytes mean
/// equal values, and both empty forms have length 0.
#[derive(Clone, Copy, Default)]
pub struct Key(u64);

impl Key {
    /// The word of the table where the name's bucket starts: 15 bits of the
    /// hash, times the 8 slots of a bucket.
    fn bucket(self) -> usize {
        8 * (self.0 as usize & (BUCKETS - 1))
    }

    /// The 47 bits above those, which a slot stores. Never 0, so that no tag
    /// matches an empty slot.
    fn tag(self) -> u64 {
        (self.0 >> 17).max(1)
    }
}

/// The name table, the matrix and the next dense id. A slot is 0 when empty,
/// else `tag << 17 | (id + 1)`.
pub struct Table {
    /// Table, then matrix: one mapping, each of its `PAGES` huge pages
    /// faulted in by `touch`.
    all: &'static [AtomicU64],
    slots: &'static [AtomicU64],
    /// `GROUPS` rows of `ROW` words; group g's bit for id i is bit `i % 64` of
    /// word `g * ROW + i / 64`.
    pub bits: &'static [AtomicU64],
    next: Next,
}

/// The first dense id nobody has reserved, alone on its cache line: it is
/// written by every worker that runs out of ids, and must not share a line
/// with anything read.
#[repr(align(64))]
struct Next(AtomicUsize);

/// 64-bit mix (the finaliser of MurmurHash3).
fn fmix(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^ h >> 33
}

/// One word into the running hash. A multiplication by an odd number and a
/// rotation are both one-to-one, so no word is ever lost from the hash.
#[inline(always)]
fn step(h: u64, w: u64) -> u64 {
    (h ^ w).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(31)
}

impl Table {
    /// An empty table and a zeroed matrix, in a new mapping on huge pages.
    pub fn new() -> Self {
        // SAFETY: zero is a valid AtomicU64.
        let all: &'static [AtomicU64] = unsafe { mapped_huge(WORDS) };
        let (slots, bits) = all.split_at(8 * BUCKETS);
        Table { all, slots, bits, next: Next(AtomicUsize::new(0)) }
    }

    /// Faults in huge page `k` of the mapping (MADV_POPULATE_WRITE), then
    /// counts the task off `left`. One task does each page, and the workers
    /// wait for all of them before their first record: huge pages that
    /// several workers touch at once are each charged 2 MiB again (PERF.md,
    /// row 15), and where the kernel cannot give a huge page the same call
    /// faults the 4 KiB pages of its range, which is never more memory.
    pub fn touch(&self, k: usize, left: &AtomicUsize) {
        crate::populate(&self.all[k * HUGE..(k + 1) * HUGE]);
        left.fetch_sub(1, Release);
    }

    /// The key of the `len` raw name bytes at `b[start..]`. Reads at most 24
    /// bytes past `start` beyond the name (the buffer's padding covers that)
    /// unless the name is longer, when all of it is hashed out of line.
    #[inline(always)]
    pub fn key(&self, b: &[u8], start: usize, len: usize) -> Key {
        if len > 24 {
            return Key(long_hash(&b[start..start + len]));
        }
        // `bzhi` keeps the low `bits` bits, and the whole word from 64 on.
        let bits = |k: usize| (8 * len as u32).saturating_sub(64 * k as u32);
        // SAFETY (the intrinsic, and the loads): BMI2 is part of x86-64-v3,
        // which the whole crate is compiled for (main.rs refuses any other
        // build); the loads end at most 24 bytes past the name (see `load`).
        let w = [0, 1, 2].map(|k| unsafe { _bzhi_u64(load(b, start + 8 * k), bits(k)) });
        Key(fmix(step(step(step(len as u64, w[0]), w[1]), w[2])))
    }

    /// Starts loading the cache line a lookup of `k` reads first.
    pub fn prefetch(&self, k: &Key) {
        crate::prefetch(self.slots, k.bucket());
    }

    /// The dense id of the name with key `k`, giving it the next id of the
    /// worker's block `ids` if it is new. Almost every record finds its name
    /// in its home bucket, which this compares branch-free: both halves of
    /// the line, the slots' tags (their value above the low 17 bits) against
    /// the key's, as eight lanes.
    #[inline(always)]
    pub fn find(&self, k: &Key, ids: &mut Ids) -> usize {
        let (at, tag) = (k.bucket(), k.tag());
        // SAFETY: `at` is a bucket start, so the line is 64-byte aligned (the
        // mapping is 2 MiB aligned). Each aligned 8-byte lane is read whole
        // by an x86 load, and a slot is 0 or final, so a lane is too (see
        // the module's notes): the only place that relies on that.
        let hits = unsafe {
            let p = self.slots.as_ptr().add(at).cast::<__m256i>();
            let tags = _mm256_set1_epi64x(tag as i64);
            let [a, b] = [p, p.add(1)].map(|q| _mm256_srli_epi64::<17>(_mm256_load_si256(q)));
            let eq = |h| _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(h, tags)));
            eq(a) as u32 | (eq(b) as u32) << 4
        };
        if hits == 0 {
            return self.insert(k, ids);
        }
        // The slot read whole and atomically: a hit is already final.
        let slot = self.slots[at + hits.trailing_zeros() as usize].load(Relaxed);
        (slot & (DENSE as u64 - 1)) as usize - 1
    }

    /// `find`, for a name not in its home bucket's first line when `find`
    /// looked: walks the buckets from its home, line by line and slot by
    /// slot, to the name's slot or the first empty one, which it claims with
    /// a compare-and-swap. If another worker claimed that slot first, the
    /// swap hands back the value there, which is compared as any slot's is.
    #[cold]
    #[inline(never)]
    fn insert(&self, k: &Key, ids: &mut Ids) -> usize {
        let (mut at, tag) = (k.bucket(), k.tag());
        for _ in 0..BUCKETS {
            for slot in &self.slots[at..at + 8] {
                let mut v = slot.load(Relaxed);
                if v == 0 {
                    if ids[0] == ids[1] {
                        let first = self.next.0.fetch_add(BLOCK, Relaxed);
                        if first + BLOCK > DENSE - 1 {
                            crate::die("more than about 130,000 distinct names");
                        }
                        *ids = [first, first + BLOCK];
                    }
                    match slot.compare_exchange(0, tag << 17 | (ids[0] as u64 + 1), Relaxed, Relaxed) {
                        Ok(_) => {
                            ids[0] += 1;
                            return ids[0] - 1;
                        }
                        Err(now) => v = now,
                    }
                }
                if v >> 17 == tag {
                    return (v & (DENSE as u64 - 1)) as usize - 1;
                }
            }
            at = (at + 8) & (8 * BUCKETS - 1);
        }
        crate::die("name table full")
    }
}

/// The hash of a name of over 24 bytes: every 8-byte word of it (the last
/// padded with zeros), from a start that holds its length and differs from
/// the short names'.
#[cold]
#[inline(never)]
fn long_hash(name: &[u8]) -> u64 {
    let mut h = name.len() as u64 ^ 0x5555_5555_5555_5555;
    for c in name.chunks(8) {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        h = step(h, u64::from_le_bytes(w));
    }
    fmix(h)
}
