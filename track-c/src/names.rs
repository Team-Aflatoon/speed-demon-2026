//! Distinct names. `Names` interns every name into a dense id with one
//! lock-free hash table shared by all workers, and records which (name,
//! group) pairs occur in a shared bitset of 256 bits per name, one per
//! group. A pair is new exactly when its bit flips from 0 to 1, so
//! `names_distinct` is a count of flips, with nothing to merge at the end.
//!
//! After the first few percent of the file nearly every name is already in
//! the table, so it is read-mostly and its cache lines stay shared by all
//! cores. Ids are exact: a lookup always compares the name's bytes.

use crate::records::{load, Name, BATCH};
use crate::{map, prefetch, Agg, Padded};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};

/// Words per table slot: up to three words of name, then `meta`.
const SLOT: usize = 4;
/// The longest name kept in its slot; longer ones live in the arena.
const INLINE: usize = 8 * (SLOT - 1);
/// `meta` of a slot whose claimer is still filling it in. Otherwise `meta`
/// is 0 for an empty slot, or `(id + 1) << 32 | length`.
const FILLING: u64 = u64::MAX;

/// The table was sized for fewer distinct names than the input holds.
pub struct Full;

pub struct Names {
    /// Open addressing with linear probing over 32-byte slots, so a lookup
    /// usually reads one cache line, with the name's words right there to
    /// compare. Slot i is `table[SLOT * i..][..SLOT]`.
    table: &'static [AtomicU64],
    mask: usize,
    /// Names longer than `INLINE`: their slot holds where their words start
    /// here, and their length.
    arena: &'static [AtomicU64],
    /// The groups seen with name id i are bits `4 * i * 64..` onwards.
    pairs: &'static [AtomicU64],
    /// Ids and arena words handed out so far, and the cap on ids that keeps
    /// the table at most 7/8 full.
    next_id: Padded<AtomicUsize>,
    next_word: Padded<AtomicUsize>,
    limit: usize,
}

impl Names {
    /// An empty set with `slots` table slots (a power of two) for a file of
    /// `file_len` bytes.
    pub fn new(slots: usize, file_len: usize) -> std::io::Result<Self> {
        let limit = slots / 8 * 7;
        Ok(Names {
            table: map::zeroed(SLOT * slots, true)?,
            mask: slots - 1,
            // A long name's padded words are fewer than its record's bytes,
            // so the arena cannot overflow.
            arena: map::zeroed(file_len / 8 + 1, false)?,
            // Huge pages for the same reasons as the table: workers touch
            // it all at once, at random.
            pairs: map::zeroed(4 * limit, true)?,
            next_id: Padded(AtomicUsize::new(0)),
            next_word: Padded(AtomicUsize::new(0)),
            limit,
        })
    }

    /// Interns a batch of names and adds every new (name, group) pair to its
    /// group's count. Each pass over the batch prefetches what the next one
    /// reads, so the cache misses of a whole batch overlap instead of
    /// following one another: hashes and table slots, ids and bitset words,
    /// then the bits themselves.
    pub fn count(&self, b: &[u8], batch: &[Name], aggs: &mut [Agg]) -> Result<(), Full> {
        let (mut hashes, mut at) = ([0; BATCH], [0; BATCH]);
        for (h, n) in hashes.iter_mut().zip(batch) {
            *h = hash(n.len, words(b, n));
            prefetch(&self.table[SLOT * (*h as usize & self.mask)]);
        }
        for ((at, &h), n) in at.iter_mut().zip(&hashes).zip(batch) {
            *at = 4 * self.id(h, b, n)? + usize::from(n.group / 64);
            crate::prefetch_write(&self.pairs[*at]);
        }
        for (&at, n) in at.iter().zip(batch) {
            let bit = 1 << (n.group % 64);
            let new = self.pairs[at].fetch_or(bit, Relaxed) & bit == 0;
            aggs[usize::from(n.group)].names += u64::from(new);
        }
        Ok(())
    }

    /// The id of name `n`, whose hash is `h`.
    fn id(&self, h: u64, b: &[u8], n: &Name) -> Result<usize, Full> {
        let mut i = h as usize & self.mask;
        loop {
            let slot = &self.table[SLOT * i..][..SLOT];
            let mut meta = slot[SLOT - 1].load(Acquire);
            if meta == 0 {
                // Claim the slot first, so that only one worker writes it.
                match slot[SLOT - 1].compare_exchange(0, FILLING, Acquire, Acquire) {
                    Ok(_) => return self.fill(slot, b, n),
                    Err(now) => meta = now,
                }
            }
            while meta == FILLING {
                std::hint::spin_loop(); // a few stores away from done
                meta = slot[SLOT - 1].load(Acquire);
            }
            if meta as u32 as usize == n.len.min(u32::MAX as usize) && self.holds(slot, b, n) {
                return Ok((meta >> 32) as usize - 1);
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Writes name `n` into a slot this worker has claimed, then publishes
    /// it: the Release store makes the words visible before the `meta`.
    fn fill(&self, slot: &[AtomicU64], b: &[u8], n: &Name) -> Result<usize, Full> {
        let id = self.next_id.fetch_add(1, Relaxed);
        if id >= self.limit {
            // Publish a slot no lookup matches, so nobody waits on it forever.
            slot[1].store(u64::MAX, Relaxed);
            slot[SLOT - 1].store(u64::from(u32::MAX), Release);
            return Err(Full);
        }
        if n.len <= INLINE {
            words(b, n).zip(slot).for_each(|(w, s)| s.store(w, Relaxed));
        } else {
            let at = self.next_word.fetch_add(n.len.div_ceil(8), Relaxed);
            words(b, n).zip(&self.arena[at..]).for_each(|(w, a)| a.store(w, Relaxed));
            slot[0].store(at as u64, Relaxed);
            slot[1].store(n.len as u64, Relaxed);
        }
        let len = n.len.min(u32::MAX as usize) as u64;
        slot[SLOT - 1].store((id as u64 + 1) << 32 | len, Release);
        Ok(id)
    }

    /// Whether a published slot whose length field matches holds name `n`.
    fn holds(&self, slot: &[AtomicU64], b: &[u8], n: &Name) -> bool {
        let stored = if n.len <= INLINE {
            slot
        } else if slot[1].load(Relaxed) == n.len as u64 {
            &self.arena[slot[0].load(Relaxed) as usize..]
        } else {
            return false;
        };
        words(b, n).zip(stored).all(|(w, s)| s.load(Relaxed) == w)
    }
}

/// The raw bytes of name `n` as little-endian 8-byte words, the last one
/// zero-padded.
fn words<'b>(b: &'b [u8], n: &Name) -> impl Iterator<Item = u64> + 'b {
    let (start, len) = (n.start, n.len);
    (0..len.div_ceil(8)).map(move |k| match len - 8 * k {
        rest @ 0..=7 => load(b, start + 8 * k) & ((1 << (8 * rest)) - 1),
        _ => load(b, start + 8 * k),
    })
}

/// A multiply-xorshift hash of a name's length and words.
fn hash(len: usize, words: impl Iterator<Item = u64>) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = (len as u64 + 1).wrapping_mul(K);
    for w in words {
        h = (h ^ w).wrapping_mul(K);
        h ^= h >> 29;
    }
    h.wrapping_mul(K) ^ (h >> 32)
}
