//! PROBE p4: names identified by a 64-bit hash of their raw bytes and length
//! (bucket index 15 bits + stored tag 47 bits = 62 bits), dense ids in the
//! slot, and a country-major bit matrix of (group, dense id) set with
//! `lock bts`. Research prototype only.

use crate::parse::load;
use std::arch::x86_64::_bzhi_u64;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};

/// Buckets of 8 one-word slots, one cache line each.
const BUCKETS: usize = 1 << 15;
/// Most dense ids: bit columns of each group's row.
const DENSE: usize = 1 << 17;
/// Words per group row.
pub const ROW: usize = DENSE / 64;
/// u64 words per 2 MiB huge page.
const HUGE: usize = 1 << 18;
/// Dense ids a worker reserves at once.
const BLOCK: usize = 64;

#[derive(Clone, Copy, Default)]
pub struct Key {
    h: u64,
}

pub struct Table {
    slots: &'static [AtomicU64],
    pub bits: &'static [AtomicU64],
    next: AtomicUsize,
}

/// 64-bit mix (the finaliser of MurmurHash3).
fn fmix(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^ h >> 33
}

/// One word into the running hash.
#[inline(always)]
fn step(h: u64, w: u64) -> u64 {
    (h ^ w).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(31)
}

impl Table {
    pub fn new() -> Self {
        let n = (8 * BUCKETS + 256 * ROW).next_multiple_of(HUGE);
        // SAFETY: zero is a valid AtomicU64.
        let raw: &'static [AtomicU64] = unsafe { crate::mapped(n + HUGE, -1) };
        let skip = (raw.as_ptr() as usize).next_multiple_of(8 * HUGE) - raw.as_ptr() as usize;
        let all = &raw[skip / 8..skip / 8 + n];
        crate::hugepage(all);
        let (slots, bits) = all.split_at(8 * BUCKETS);
        Table { slots, bits: &bits[..256 * ROW], next: AtomicUsize::new(0) }
    }

    /// Faults in huge page `k` of the mapping (one task per page).
    pub fn touch(&self, k: usize, _n: usize) {
        // SAFETY: slots and bits are one mapping, bits right after slots.
        let all = unsafe { std::slice::from_raw_parts(self.slots.as_ptr(), 8 * BUCKETS + 256 * ROW) };
        crate::populate(all.chunks(HUGE).nth(k).unwrap_or(&[]));
    }

    /// The key of the `len` raw name bytes at `b[start..]` (reads up to 24
    /// bytes past `start`; longer names are hashed whole, out of line).
    #[inline(always)]
    pub fn key(&self, b: &[u8], start: usize, len: usize) -> Key {
        if len > 24 {
            return Key { h: long_hash(&b[start..start + len]) };
        }
        let bits = |k: usize| (8 * len as u32).saturating_sub(64 * k as u32);
        // SAFETY: BMI2 is in x86-64-v3; loads within the padding (see `load`).
        let w = [0, 1, 2].map(|k| unsafe { _bzhi_u64(load(b, start + 8 * k), bits(k)) });
        let h = step(step(step(len as u64, w[0]), w[1]), w[2]);
        Key { h: fmix(h) }
    }

    pub fn prefetch(&self, k: &Key) {
        crate::prefetch(self.slots, 8 * (k.h as usize & (BUCKETS - 1)));
    }

    /// The dense id of the name with key `k`, giving it the next id of the
    /// worker's block `blk` if it is new. A slot is 0 (empty) or `tag << 17 |
    /// (id + 1)`, written once by a CAS from 0, so a reader sees all or none.
    #[inline(always)]
    pub fn find(&self, k: &Key, _b: &[u8], blk: &mut [usize; 2]) -> usize {
        let tag = (k.h >> 17).max(1); // never 0, so no empty slot matches
        let mut at = 8 * (k.h as usize & (BUCKETS - 1));
        // PROBE p5: the home line in two AVX2 loads, tags compared at once.
        // Each aligned 8-byte lane is read whole: a slot goes from 0 to its
        // final value in one CAS.
        // SAFETY: `at` is a bucket start, 64-byte aligned (huge mapping).
        let m = unsafe {
            use std::arch::x86_64::*;
            let p = self.slots.as_ptr().add(at).cast::<__m256i>();
            let t = _mm256_set1_epi64x(tag as i64);
            let [a, b] = [p, p.add(1)].map(|q| _mm256_srli_epi64::<17>(_mm256_load_si256(q)));
            let ma = _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(a, t))) as u32;
            let mb = _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(b, t))) as u32;
            ma | mb << 4
        };
        if m != 0 {
            let v = self.slots[at + m.trailing_zeros() as usize].load(Relaxed);
            return (v & 0x1_FFFF) as usize - 1;
        }
        for _ in 0..BUCKETS {
            // SAFETY: `at` is a bucket start.
            let line = unsafe { self.slots.get_unchecked(at..at + 8) };
            for s in line {
                let mut v = s.load(Relaxed);
                if v == 0 {
                    if blk[0] == blk[1] {
                        let s0 = self.next.fetch_add(BLOCK, Relaxed);
                        if s0 + BLOCK >= DENSE {
                            crate::die("more distinct names than the bit rows hold");
                        }
                        *blk = [s0, s0 + BLOCK];
                    }
                    match s.compare_exchange(0, tag << 17 | (blk[0] as u64 + 1), Relaxed, Relaxed) {
                        Ok(_) => {
                            blk[0] += 1;
                            return blk[0] - 1;
                        }
                        Err(now) => v = now,
                    }
                }
                if v >> 17 == tag {
                    return (v & 0x1_FFFF) as usize - 1;
                }
            }
            at = (at + 8) & (8 * BUCKETS - 1);
        }
        crate::die("name table full")
    }
}

/// The hash of a name over 24 bytes: every word, then the length.
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
