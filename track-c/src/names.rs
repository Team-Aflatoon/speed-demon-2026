//! Distinct names: one lock-free hash table shared by all workers, with a
//! 64-byte slot per name holding the name's bytes and a 256-bit set of the
//! groups it has been seen in. A (name, group) pair is new exactly when its
//! bit flips from 0 to 1, so `names_distinct` is a count of flips, with
//! nothing to merge at the end. A record touches one cache line: the slot
//! it compares its name against is the one whose bit it sets. Lookups are
//! exact: they always compare the name's bytes, never trust the hash alone.

use crate::records::{load, Fail, Record, BATCH};
use crate::{map, prefetch, Agg, Padded, NAMES};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};

/// Words per table slot, one cache line: three words of name, `meta`, then
/// four words of group bits.
const SLOT: usize = 8;
const META: usize = 3;
const GROUPS: usize = 4;
/// The longest name kept in its slot; longer ones live in the arena.
const INLINE: usize = 8 * META;
/// `meta` of a slot whose claimer is still filling it in. Otherwise `meta`
/// is 0 for an empty slot, or `1 << 32 | length`, where a length of `LONG`
/// or more is `LONG` and the slot's first word holds the real one.
const FILLING: u64 = u64::MAX;
const LONG: usize = u32::MAX as usize;

pub struct Names {
    /// Open addressing with linear probing over line-sized slots; slot i is
    /// `table[SLOT * i..][..SLOT]`.
    table: &'static [AtomicU64],
    mask: usize,
    /// Names longer than `INLINE`: their slot holds their length and where
    /// their words start here.
    arena: &'static [AtomicU64],
    /// Slots and arena words handed out so far, and the cap on slots that
    /// keeps the table at most 7/8 full.
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
            table: map::zeroed(SLOT * slots)?,
            mask: slots - 1,
            // A long name's padded words are fewer than its record's bytes,
            // so the arena cannot overflow.
            arena: map::zeroed(file_len / 8 + 1)?,
            next_id: Padded(AtomicUsize::new(0)),
            next_word: Padded(AtomicUsize::new(0)),
            limit,
        })
    }

    /// Faults in this worker's share of the tables (see `map::touch`).
    pub fn touch(&self, part: usize, parts: usize) {
        map::touch(self.table, part, parts);
    }

    /// Interns a batch of names and adds every new (name, group) pair to its
    /// group's count. Each pass over the batch prefetches what the next one
    /// reads, so the cache misses of a whole batch overlap instead of
    /// following one another: keys and table slots, then slot lookups, then
    /// the bits themselves.
    pub fn count(&self, b: &[u8], batch: &[Record], aggs: &mut [Agg]) -> Result<(), Fail> {
        let (mut keys, mut at) = ([[0; 4]; BATCH], [0; BATCH]);
        for (k, n) in keys.iter_mut().zip(batch) {
            *k = key(b, n);
            prefetch(&self.table[SLOT * (k[3] as usize & self.mask)]);
        }
        for ((at, key), n) in at.iter_mut().zip(&keys).zip(batch) {
            *at = SLOT * self.slot(key, b, n)? + SLOT - GROUPS + usize::from(n.group / 64);
        }
        for (&at, n) in at.iter().zip(batch) {
            let bit = 1 << (n.group % 64);
            let new = self.table[at].fetch_or(bit, Relaxed) & bit == 0;
            aggs[usize::from(n.group)].sums[NAMES] += i64::from(new);
        }
        Ok(())
    }

    /// The index of name `n`'s slot; `key` is its key.
    fn slot(&self, key: &Key, b: &[u8], n: &Record) -> Result<usize, Fail> {
        let mut i = key[3] as usize & self.mask;
        loop {
            let slot = &self.table[SLOT * i..][..SLOT];
            let mut meta = slot[META].load(Acquire);
            if meta == 0 {
                // Claim the slot first, so that only one worker writes it.
                match slot[META].compare_exchange(0, FILLING, Acquire, Acquire) {
                    Ok(_) => return self.fill(slot, key, b, n).map(|()| i),
                    Err(now) => meta = now,
                }
            }
            while meta == FILLING {
                std::hint::spin_loop(); // a few stores away from done
                meta = slot[META].load(Acquire);
            }
            if meta as u32 as usize == n.len.min(LONG) && self.holds(slot, key, b, n) {
                return Ok(i);
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Writes name `n` into a slot this worker has claimed, then publishes
    /// it: the Release store makes the words visible before the `meta`.
    fn fill(&self, slot: &[AtomicU64], key: &Key, b: &[u8], n: &Record) -> Result<(), Fail> {
        if self.next_id.0.fetch_add(1, Relaxed) >= self.limit {
            // Publish a slot no lookup matches, so nobody waits on it forever.
            slot[0].store(u64::MAX, Relaxed);
            slot[META].store(1 << 32 | LONG as u64, Release);
            return Err(Fail::Full);
        }
        if n.len <= INLINE {
            key[..META].iter().zip(slot).for_each(|(&w, s)| s.store(w, Relaxed));
        } else {
            let at = self.next_word.0.fetch_add(n.len.div_ceil(8), Relaxed);
            words(b, n).zip(&self.arena[at..]).for_each(|(w, a)| a.store(w, Relaxed));
            slot[0].store(n.len as u64, Relaxed);
            slot[1].store(at as u64, Relaxed);
        }
        slot[META].store(1 << 32 | n.len.min(LONG) as u64, Release);
        Ok(())
    }

    /// Whether a published slot whose length field matches holds name `n`.
    fn holds(&self, slot: &[AtomicU64], key: &Key, b: &[u8], n: &Record) -> bool {
        if n.len <= INLINE {
            return (0..META).all(|k| slot[k].load(Relaxed) == key[k]);
        }
        let arena = &self.arena[slot[1].load(Relaxed) as usize..];
        let same = |(w, a): (u64, &AtomicU64)| a.load(Relaxed) == w;
        slot[0].load(Relaxed) == n.len as u64 && words(b, n).zip(arena).all(same)
    }
}

/// A name's first 24 bytes as three zero-padded words, which for nearly
/// every name is all of it, then its hash. Fixed-size, so hashing and
/// comparing need no loop.
type Key = [u64; 4];

fn key(b: &[u8], n: &Record) -> Key {
    let word = |k: usize| load(b, n.start + 8 * k) & keep(n.len.saturating_sub(8 * k));
    let [w0, w1, w2] = [0, 1, 2].map(word);
    let mut h = mix(w0 ^ mix(w1 ^ mix(w2 ^ n.len as u64)));
    if n.len > INLINE {
        h = words(b, n).skip(3).fold(h, |h, w| mix(h ^ w));
    }
    [w0, w1, w2, h ^ h >> 32]
}

/// A mask of the low `bytes` bytes of a word (all of it from 8 on).
fn keep(bytes: usize) -> u64 {
    u64::MAX.checked_shr(64 - 8 * bytes.min(8) as u32).unwrap_or(0)
}

/// One multiply-xorshift round.
fn mix(x: u64) -> u64 {
    let x = x.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^ x >> 29
}

/// The raw bytes of name `n` as little-endian 8-byte words, the last one
/// zero-padded.
fn words<'b>(b: &'b [u8], n: &Record) -> impl Iterator<Item = u64> + 'b {
    let (start, len) = (n.start, n.len);
    (0..len.div_ceil(8)).map(move |k| load(b, start + 8 * k) & keep(len - 8 * k))
}
