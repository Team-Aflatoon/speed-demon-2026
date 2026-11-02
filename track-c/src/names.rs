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
//! Hash identity: two names are one if their hashes agree on 61 bits (14 for
//! the bucket, 47 for the stored tag). For the spec's pool of 100,000 names
//! the chance that any two do is n^2 / 2 / 2^61, about 2.2e-9 a dataset. The
//! hash is fixed and unseeded, so a dataset gives the same output every run.
//! It is three AES rounds over the name's first 32 bytes (`Table::key`), and
//! the same again for each further 32.
//!
//! The table (1 MiB) and the matrix (241 rows of 1,600 words, 2.9 MiB) share
//! one mapping on two huge pages, so that the random reads and bit writes of 3
//! million records touch few pages and few TLB entries; each page is faulted
//! in by one start-up task (see `Table::touch`). The sizes follow the spec:
//! its pool is 100,000 names and it lists 240 countries.
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
//! counted once.
//!
//! The plain AVX2 load. The one place that relies on the hardware, not on
//! Rust's memory model, is `Table::lanes` (the fast path of `Table::find`, and
//! the skip over full lines in `insert`): it reads a line's eight `AtomicU64`
//! slots with two 32-byte vector loads, which Rust counts as plain reads
//! racing with the compare-and-swaps that fill the slots. It is sound on x86
//! for three reasons.
//! - Each slot is an aligned 8-byte lane, and x86 never reads such a lane
//!   torn (the 32-byte load as a whole is not atomic, so the lanes may come
//!   from different moments, but no lane is ever half old and half new).
//! - A slot only ever goes from 0 to its final value, so a lane is 0 or
//!   final. A stale 0 gives no tag match (and shows a full line as having a
//!   free slot), and the lookup goes to `insert`'s walk, which reads and
//!   claims slots atomically and so finds the name if it was published
//!   meanwhile. A non-zero lane is final, never a stale
//!   half-way value, so a match is a real match.
//! - A hit's id is read again through the atomic slot (`Relaxed`, as the
//!   slot publishes no other data: names are not stored), so the plain load
//!   only picks which slot, never the id used.
//!
//! The fully safe alternative is eight `AtomicU64::load`s per lookup. They
//! are plain `mov`s on x86 but LLVM does not vectorise them, so each record
//! would take eight loads, shifts, compares and mask steps in place of two
//! loads and a few vector instructions (a branchy probe loop instead was the
//! largest part of the earlier design's name cost: PERF.md, row 50). We did
//! not measure that build, and did not pay for a safety the hardware already
//! gives.

use crate::{mapped_huge, MATRIX_ROWS};
use std::arch::x86_64::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};

/// Buckets: one cache line of 8 one-word slots each (1 MiB all together).
const BUCKETS: usize = 1 << 14;
/// Dense ids: bit columns of the matrix, so the most distinct names it holds
/// (a slot stores an id plus one in `ID_BITS` bits, which are enough for it).
const DENSE: usize = 102_400;
/// Bits of a slot that hold an id plus one; the tag is above them.
const ID_BITS: i32 = 17;
const _: () = assert!(DENSE < 1 << ID_BITS);
/// The id bits of a slot.
const ID_MASK: u64 = (1 << ID_BITS) - 1;
/// Words in a group's row of the matrix.
pub const ROW: usize = DENSE / 64;
/// Words in the table and the matrix, the table first.
const WORDS: usize = 8 * BUCKETS + MATRIX_ROWS * ROW;
/// Words in a 2 MiB huge page.
const HUGE: usize = 1 << 18;
/// Huge pages in the mapping (see `Table::touch`).
pub const PAGES: usize = 2;
/// Dense ids a worker reserves at once.
const BLOCK: usize = 64;
// The table and the matrix must fit the mapping's huge pages.
const _: () = assert!(WORDS <= PAGES * HUGE);

/// The dense ids a worker has reserved and not yet used.
pub type Ids = std::ops::Range<usize>;

/// A name's identity: the hash of its raw bytes and length. The raw bytes
/// keep `""` escapes doubled; escaping is one-to-one, so equal raw bytes mean
/// equal values, and both empty forms have length 0.
pub type Key = u64;

/// Where a key's bucket starts in the table (14 bits of the hash, times the 8
/// slots of a bucket) and the tag a slot stores (the 47 bits from bit 17 up;
/// bits 14 to 16 are not used). The tag is never 0, so that none matches an
/// empty slot.
fn locate(k: Key) -> (usize, u64) {
    (8 * (k as usize & (BUCKETS - 1)), (k >> ID_BITS).max(1))
}

/// The name table, the matrix and the next dense id. A slot is 0 when empty,
/// else `tag << ID_BITS | (id + 1)`.
pub struct Table {
    /// The table (one mapping with the matrix, whose `PAGES` huge pages are
    /// faulted in by `touch`).
    slots: &'static [AtomicU64],
    /// `MATRIX_ROWS` rows of `ROW` words; group g's bit for id i is bit `i % 64` of
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
    /// before their first record: huge pages that several workers touch at
    /// once are each charged 2 MiB again (PERF.md, row 15), and where the
    /// kernel cannot give a huge page the same call faults the 4 KiB pages
    /// of its range, which is never more memory.
    pub fn touch(&self, k: usize) {
        // (`run` hands out `k` below `PAGES`; the last page is only partly
        // used, and the huge page behind its range is faulted in whole.)
        let (page, words) = (self.slots.as_ptr().wrapping_add(k * HUGE), WORDS - k * HUGE);
        // SAFETY: the range is inside our own mapping (the table, then the
        // matrix), and populating leaves its bytes as they are. 23 is
        // MADV_POPULATE_WRITE.
        unsafe { crate::madvise(page.cast(), 8 * words.min(HUGE), 23) };
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

    /// The slots of the line at `at` whose tag is `tag`, and its empty slots,
    /// as bit masks over its eight slots (low bit = first slot), compared as
    /// eight lanes (their value above the low `ID_BITS` bits against the tag).
    #[inline(always)]
    fn lanes(&self, at: usize, tag: u64) -> (u32, u32) {
        // SAFETY: `at` is a line start, so the line is 64-byte aligned (the
        // mapping is 2 MiB aligned). Each aligned 8-byte lane is read whole
        // by an x86 load, and a slot is 0 or final, so a lane is too (see
        // the module's notes): the only place that relies on that.
        unsafe {
            let p = self.slots.as_ptr().add(at).cast::<__m256i>();
            let tags = _mm256_set1_epi64x(tag as i64);
            let zero = _mm256_setzero_si256();
            let [a, b] = [p, p.add(1)].map(|q| _mm256_load_si256(q));
            let mask = |h| _mm256_movemask_pd(_mm256_castsi256_pd(h)) as u32;
            let eq = |h| mask(_mm256_cmpeq_epi64(_mm256_srli_epi64::<ID_BITS>(h), tags));
            let empty = |h| mask(_mm256_cmpeq_epi64(h, zero));
            (eq(a) | eq(b) << 4, empty(a) | empty(b) << 4)
        }
    }

    /// The dense id of the name with key `k`, giving it the next id of the
    /// worker's block `ids` if it is new. Almost every record finds its name
    /// in its home bucket, which this compares branch-free (`lanes`).
    #[inline(always)]
    pub fn find(&self, k: Key, ids: &mut Ids) -> usize {
        let (at, tag) = locate(k);
        let hits = self.lanes(at, tag).0;
        if hits == 0 {
            return self.insert(k, ids);
        }
        // The slot read whole and atomically: a hit is already final.
        let lane = at + hits.trailing_zeros() as usize;
        // SAFETY: a lane of the bucket's line, which is within the table.
        let slot = unsafe { self.slots.get_unchecked(lane) }.load(Relaxed);
        (slot & ID_MASK) as usize - 1
    }

    /// `find`, for a name not in its home bucket's line when `find` looked:
    /// skips the lines from its home that are full and do not hold it (a full
    /// line stays full), then walks the slots from there, to the name's slot
    /// or the first empty one, which it claims with a compare-and-swap. If
    /// another worker claimed that slot first, the swap hands back the value
    /// there, which is compared as any slot's is.
    #[cold]
    fn insert(&self, k: Key, ids: &mut Ids) -> usize {
        let (mut at, tag) = locate(k);
        for _ in 0..BUCKETS {
            let (hits, empty) = self.lanes(at, tag);
            if hits != 0 {
                let slot = self.slots[at + hits.trailing_zeros() as usize].load(Relaxed);
                return (slot & ID_MASK) as usize - 1;
            }
            if empty != 0 {
                break;
            }
            at = (at + 8) % (8 * BUCKETS);
        }
        // From the line's first slot to the table's end, then round from the
        // table's start: every slot once, and a full table ends the loop.
        for slot in self.slots[at..].iter().chain(self.slots) {
            let mut v = slot.load(Relaxed);
            if v == 0 {
                if ids.start == ids.end {
                    let first = self.next.0.fetch_add(BLOCK, Relaxed);
                    if first + BLOCK > DENSE - 1 {
                        crate::die("more than about 102,000 distinct names");
                    }
                    *ids = first..first + BLOCK;
                }
                let new = tag << ID_BITS | (ids.start as u64 + 1);
                match slot.compare_exchange(0, new, Relaxed, Relaxed) {
                    // (`ids` was refilled above if it was empty.)
                    Ok(_) => return ids.next().unwrap(),
                    Err(now) => v = now,
                }
            }
            if v >> ID_BITS == tag {
                return (v & ID_MASK) as usize - 1;
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
