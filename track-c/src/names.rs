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
//! It is three AES rounds over the name's first 32 bytes (`Table::key`), and
//! the same again for each further 32.
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
// The table and the matrix must each be whole huge pages.
const _: () = assert!(8 * BUCKETS % HUGE == 0 && GROUPS * ROW % HUGE == 0);

/// The dense ids a worker has reserved and not yet used.
pub type Ids = std::ops::Range<usize>;

/// A name's identity: the hash of its raw bytes and length. The raw bytes
/// keep `""` escapes doubled; escaping is one-to-one, so equal raw bytes mean
/// equal values, and both empty forms have length 0.
pub type Key = u64;

/// Where a key's bucket starts in the table (15 bits of the hash, times the 8
/// slots of a bucket) and the tag a slot stores (the 47 bits above those).
/// The tag is never 0, so that none matches an empty slot.
fn locate(k: Key) -> (usize, u64) {
    (8 * (k as usize & (BUCKETS - 1)), (k >> 17).max(1))
}

/// The name table, the matrix and the next dense id. A slot is 0 when empty,
/// else `tag << 17 | (id + 1)`.
pub struct Table {
    /// The table (one mapping with the matrix, whose `PAGES` huge pages are
    /// faulted in by `touch`).
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

impl Table {
    /// An empty table and a zeroed matrix, in a new mapping on huge pages.
    pub fn new() -> Self {
        // SAFETY: zero is a valid AtomicU64.
        let all: &'static [AtomicU64] = unsafe { mapped_huge(WORDS) };
        let (slots, bits) = all.split_at(8 * BUCKETS);
        let next = Next(AtomicUsize::new(0));
        Table { slots, bits, next }
    }

    /// Faults in huge page `k` of the mapping without changing it
    /// (MADV_POPULATE_WRITE), in one call, which the kernel walks far faster
    /// than it takes one page fault after another; only a hint, as on a
    /// kernel without the call (before 5.14) the pages fault in when first
    /// used. One task does each page, and the workers wait for all of them
    /// before their first record: huge pages that
    /// several workers touch at once are each charged 2 MiB again (PERF.md,
    /// row 15), and where the kernel cannot give a huge page the same call
    /// faults the 4 KiB pages of its range, which is never more memory.
    pub fn touch(&self, k: usize) {
        let mut pages = self.slots.chunks(HUGE).chain(self.bits.chunks(HUGE));
        let page = pages.nth(k).unwrap();
        // SAFETY: the range is our own mapping, and populating leaves its
        // bytes as they are.
        unsafe { crate::madvise(page.as_ptr().cast(), size_of_val(page), 23) };
    }

    /// The key of the `len` raw name bytes at `b[start..]`: a hash of the
    /// first 32 bytes, then of each further 32 together with the hash so far.
    /// Reads at most 32 bytes from `start` (the buffer's padding covers that).
    pub fn key(b: &[u8], start: usize, len: usize) -> Key {
        let head = block(b, start, len.min(32), len as u64);
        if len > 32 {
            return rest(b, start, len, head);
        }
        head
    }

    /// Starts loading the cache line a lookup of `k` reads first.
    pub fn prefetch(&self, k: Key) {
        crate::prefetch(self.slots, locate(k).0);
    }

    /// The dense id of the name with key `k`, giving it the next id of the
    /// worker's block `ids` if it is new. Almost every record finds its name
    /// in its home bucket, which this compares branch-free: both halves of
    /// the line, the slots' tags (their value above the low 17 bits) against
    /// the key's, as eight lanes.
    #[inline(always)]
    pub fn find(&self, k: Key, ids: &mut Ids) -> usize {
        let (at, tag) = locate(k);
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
    /// looked: walks the slots from its home, line by line, to the name's
    /// slot or the first empty one, which it claims with a compare-and-swap.
    /// If another worker claimed that slot first, the swap hands back the
    /// value there, which is compared as any slot's is.
    #[cold]
    fn insert(&self, k: Key, ids: &mut Ids) -> usize {
        let (at, tag) = locate(k);
        // From the bucket's first slot to the table's end, then round from
        // its start: every slot once, and a full table ends the loop.
        for slot in self.slots[at..].iter().chain(self.slots) {
            let mut v = slot.load(Relaxed);
            if v == 0 {
                if ids.start == ids.end {
                    let first = self.next.0.fetch_add(BLOCK, Relaxed);
                    if first + BLOCK > DENSE - 1 {
                        crate::die("more than about 130,000 distinct names");
                    }
                    *ids = first..first + BLOCK;
                }
                let new = tag << 17 | (ids.start as u64 + 1);
                match slot.compare_exchange(0, new, Relaxed, Relaxed) {
                    Ok(_) => return ids.next().unwrap(),
                    Err(now) => v = now,
                }
            }
            if v >> 17 == tag {
                return (v & (DENSE as u64 - 1)) as usize - 1;
            }
        }
        crate::die("name table full")
    }
}

/// The hash of the `n` bytes (at most 32) at `b[at..]`, mixed with `seed`.
fn block(b: &[u8], at: usize, n: usize, seed: u64) -> u64 {
    // 32 bytes of ones, then 32 of zeros: loading 32 bytes from `32 - n`
    // keeps the first `n` bytes of a block and clears the rest.
    static KEEP: [[u8; 32]; 2] = [[0xFF; 32], [0; 32]];
    // SAFETY: the loads read 32 bytes from inside the buffer's padding (see
    // `Table::key`) and from `KEEP`, at most 32 from its start. AES-NI and AVX2
    // are part of the build (see main.rs).
    unsafe {
        let name = _mm256_loadu_si256(b.as_ptr().add(at).cast());
        let keep = _mm256_loadu_si256(KEEP.as_ptr().cast::<u8>().add(32 - n).cast());
        let bytes = _mm256_and_si256(name, keep);
        let key = _mm_set_epi64x(0x1D8E_4E27_C47D_124F, (seed ^ 0x5899_65CC_7537_4CC3) as i64);
        let low = _mm_xor_si128(_mm256_castsi256_si128(bytes), key);
        let mixed = _mm_aesenc_si128(low, _mm256_extracti128_si256::<1>(bytes));
        let mixed = _mm_aesenc_si128(_mm_aesenc_si128(mixed, key), key);
        _mm_cvtsi128_si64(mixed) as u64
    }
}

/// The key of a name of over 32 bytes, given the hash `head` of its first 32.
#[cold]
fn rest(b: &[u8], start: usize, len: usize, head: u64) -> u64 {
    let more = |h, at: usize| block(b, start + at, (len - at).min(32), h);
    (32..len).step_by(32).fold(head, more)
}
