//! Distinct names per group, for the names that one worker owns.
//!
//! Every record's name goes, as a message, to the one worker that owns it
//! (chosen by a hash of the name), so each table here is private to its
//! worker: no locks, no atomics, and only about 1/12 of the names.

use std::arch::x86_64::*;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Bit words per name: one bit per group, 256 groups.
const BITS: usize = 4;
/// Zero words kept after the last entry, so that a 32-byte load at any
/// key stays in bounds.
const PAD: usize = 5;
/// Entries start below this word offset (24 bits in a slot).
const MAX_WORDS: usize = 1 << 24;
/// 32 bytes of ones then 32 of zeros: a 32-byte window into it starting at
/// `32 - n` keeps the first `n` bytes of a vector.
static WINDOW: [u8; 64] = {
    let mut w = [0; 64];
    let mut i = 0;
    while i < 32 {
        w[i] = 0xFF;
        i += 1;
    }
    w
};

pub struct Names {
    /// Open addressing, linear probing: 0 is empty, else 8 bits of the
    /// name's hash (to skip most other names without reading them) and
    /// 1 + its entry's word offset in `words`.
    slots: Vec<u32>,
    /// The entries: `BITS` words of group bits, then the key's length (2
    /// bytes) and bytes, zero-padded to whole words.
    words: Vec<u64>,
    names: usize,
    /// Distinct names counted so far, per group.
    pub distinct: [u32; 256],
}

/// A message's name: where its raw bytes are in the file, how many there
/// are, its first 32 of them (zero-padded) and its hash.
#[derive(Clone, Copy)]
struct Key {
    at: usize,
    len: usize,
    head: __m256i,
    hash: u64,
}

impl Names {
    /// Room for the names of one owner of twelve (spec: 100,000 in all)
    /// before anything has to grow; untouched capacity costs no memory.
    pub fn new() -> Names {
        let words = Vec::with_capacity(1 << 17);
        Names {
            slots: vec![0; 1 << 14],
            words,
            names: 0,
            distinct: [0; 256],
        }
        .padded()
    }

    fn padded(mut self) -> Names {
        self.words.extend([0; PAD]);
        self
    }

    /// Counts the messages' (name, group) pairs (see `records::record`).
    /// Batches of `BATCH` hide the cache misses: the names of the batch
    /// after next, then the slots of this batch, then their entries are
    /// prefetched before any is used.
    pub fn add_all(&mut self, d: &[u8], msgs: &[AtomicU64]) {
        const BATCH: usize = 16;
        for (b, batch) in msgs.chunks(BATCH).enumerate() {
            for m in msgs.iter().skip((b + 2) * BATCH).take(BATCH) {
                crate::prefetch(d, m.load(Relaxed) as u32 as usize);
            }
            let mut keys = [(
                Key {
                    at: 0,
                    len: 0,
                    head: zero(),
                    hash: 0,
                },
                0,
            ); BATCH];
            for (k, m) in keys.iter_mut().zip(batch) {
                let m = m.load(Relaxed);
                *k = (
                    key(d, m as u32 as usize, (m >> 32) as u16 as usize),
                    (m >> 48) as u8,
                );
                crate::prefetch(&self.slots, k.0.hash as usize & (self.slots.len() - 1));
            }
            for (k, _) in &keys[..batch.len()] {
                let s = self.slots[k.hash as usize & (self.slots.len() - 1)];
                crate::prefetch(&self.words, (s & 0xFF_FFFF) as usize);
            }
            for &(k, group) in &keys[..batch.len()] {
                let e = self.find(d, k);
                let (w, bit) = (
                    &mut self.words[e + usize::from(group / 64)],
                    1u64 << (group % 64),
                );
                self.distinct[usize::from(group)] += u32::from(*w & bit == 0);
                *w |= bit;
            }
        }
    }

    /// The entry of `k`, made if new.
    fn find(&mut self, d: &[u8], k: Key) -> usize {
        let (mask, tag) = (self.slots.len() - 1, (k.hash >> 56) as u32);
        let mut i = k.hash as usize & mask;
        loop {
            let s = self.slots[i];
            if s == 0 {
                return self.insert(i, d, k);
            }
            let e = (s & 0xFF_FFFF) as usize - 1;
            if s >> 24 == tag && self.same(e, d, k) {
                return e;
            }
            i = (i + 1) & mask;
        }
    }

    /// Whether entry `e` holds the key `k`: the same length and bytes.
    fn same(&self, e: usize, d: &[u8], k: Key) -> bool {
        let at = (e + BITS) * 8;
        let bytes = self.bytes();
        let len = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        // SAFETY: AVX2 (x86-64-v3, as the whole crate is compiled).
        let diff = unsafe { _mm256_xor_si256(load32(bytes, at + 2), k.head) };
        let equal = unsafe { _mm256_testz_si256(diff, window(k.len)) } == 1;
        let rest = || bytes[at + 34..at + 2 + k.len] == d[k.at + 32..k.at + k.len];
        usize::from(len) == k.len && equal && (k.len <= 32 || rest())
    }

    #[cold]
    fn insert(&mut self, slot: usize, d: &[u8], k: Key) -> usize {
        self.words.truncate(self.words.len() - PAD);
        let e = self.words.len();
        if e + 1 >= MAX_WORDS {
            crate::die("over 2 million distinct names for one worker");
        }
        self.words.extend([0; BITS]);
        let key = (k.len as u16)
            .to_le_bytes()
            .into_iter()
            .chain(d[k.at..k.at + k.len].iter().copied());
        let mut key = key.peekable();
        while key.peek().is_some() {
            self.words.push(
                key.by_ref()
                    .take(8)
                    .enumerate()
                    .fold(0, |w, (i, b)| w | u64::from(b) << (8 * i)),
            );
        }
        self.words.extend([0; PAD]);
        self.slots[slot] = ((k.hash >> 56) as u32) << 24 | (e as u32 + 1);
        self.names += 1;
        if self.names * 8 > self.slots.len() * 5 {
            self.grow();
        }
        e
    }

    /// Doubles the slots (at 5/8 full), rehashing each entry's key.
    fn grow(&mut self) {
        let bigger = vec![0; 2 * self.slots.len()];
        let old = std::mem::replace(&mut self.slots, bigger);
        let mask = self.slots.len() - 1;
        for s in old.into_iter().filter(|&s| s != 0) {
            let (bytes, at) = (self.bytes(), ((s & 0xFF_FFFF) as usize - 1 + BITS) * 8);
            let len = usize::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]));
            let mut i = key(bytes, at + 2, len).hash as usize & mask;
            while self.slots[i] != 0 {
                i = (i + 1) & mask;
            }
            self.slots[i] = s;
        }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the same memory as `words`, viewed as bytes, which have
        // no alignment or validity requirements.
        unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast(), self.words.len() * 8) }
    }
}

/// The key `d[at..at + len]`: its first 32 bytes, zero-padded, and a hash
/// of them and its length (longer keys that share those differ only in
/// the compare). Two 64x64-bit multiplies, folding the high half of each
/// product onto its low half, mix every byte into every bit.
fn key(d: &[u8], at: usize, len: usize) -> Key {
    // SAFETY: AVX2 as above; any 32 bytes are a valid [u64; 4].
    let head = unsafe { _mm256_and_si256(load32(d, at), window(len)) };
    let [a, b, c, e] = unsafe { std::mem::transmute::<__m256i, [u64; 4]>(head) };
    let mum = |x: u64, y: u64| {
        let p = u128::from(x) * u128::from(y);
        p as u64 ^ (p >> 64) as u64
    };
    let hash = mum(a ^ 0x9E37_79B9_7F4A_7C15, b ^ 0xC2B2_AE3D_27D4_EB4F)
        ^ mum(c ^ len as u64, e ^ 0x1656_67B1_9E37_79F9);
    Key {
        at,
        len,
        head,
        hash: mum(hash, 0xD6E8_FEB8_6659_FD93),
    }
}

/// A vector that keeps the first `n` bytes (all of them if `n >= 32`).
fn window(n: usize) -> __m256i {
    load32(&WINDOW, 32 - n.min(32))
}

fn zero() -> __m256i {
    // SAFETY: AVX2 as above.
    unsafe { _mm256_setzero_si256() }
}

fn load32(d: &[u8], at: usize) -> __m256i {
    assert!(at + 32 <= d.len());
    // SAFETY: in bounds (just asserted); AVX2 as above.
    unsafe { _mm256_loadu_si256(d.as_ptr().add(at).cast()) }
}
