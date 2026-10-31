//! The query vocabulary: every distinct term that appears in `queries.txt`,
//! numbered densely from 0, in an open-addressing hash table that the corpus
//! scan probes once per token.
//!
//! Only query terms are ever indexed, so a corpus token that misses this table
//! is dropped on the spot. That is most of the 600,000-word vocabulary.

/// `ALNUM[b]` is true for the bytes tokens are made of: `0-9`, `A-Z`, `a-z` (spec 2.2).
pub static ALNUM: [bool; 256] = {
    let mut table = [false; 256];
    let mut b = 0;
    while b < 256 {
        table[b] = (b as u8).is_ascii_alphanumeric();
        b += 1;
    }
    table
};

/// Query terms are at most 32 bytes, so a longer corpus token can never match one.
const MAX_TERM: usize = 32;
const VACANT: u32 = u32::MAX;

/// The first 16 bytes of a token, lowercased and zero padded, plus its length.
///
/// Every token byte is a letter or a digit, and for those `b | 0x20` is exactly
/// the ASCII lowercase (digits already have bit 5 set), so a whole word is
/// lowercased with one OR. Tokens never contain a zero byte, so the padding
/// cannot collide with real text.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    lo: u64,
    hi: u64,
    len: u32,
}

impl Key {
    /// `data[start..start + len]` must be a token. Reads up to 16 bytes from
    /// `start`, past the token's end if needed, and masks the excess away.
    #[inline]
    fn new(data: &[u8], start: usize, len: usize) -> Key {
        const LOWER: u64 = 0x2020_2020_2020_2020;
        let keep = |n: usize| if n >= 8 { !0 } else { (1u64 << (8 * n)) - 1 };
        Key {
            lo: (word(data, start) | LOWER) & keep(len),
            hi: (word(data, start + 8) | LOWER) & keep(len.saturating_sub(8)),
            len: len as u32,
        }
    }

    /// Folded multiply (as in foldhash): every input bit reaches every output bit.
    #[inline]
    fn hash(self) -> usize {
        let m = u128::from(self.lo ^ 0x243f_6a88_85a3_08d3)
            * u128::from(self.hi ^ 0x1319_8a2e_0370_7344);
        ((m as u64) ^ ((m >> 64) as u64) ^ u64::from(self.len)) as usize
    }
}

/// Eight bytes from `at`, little endian; zero filled past the end of `data`.
#[inline]
fn word(data: &[u8], at: usize) -> u64 {
    match data.get(at..at + 8) {
        Some(bytes) => u64::from_le_bytes(bytes.try_into().unwrap()),
        None => {
            let mut buf = [0u8; 8];
            let tail = data.get(at..).unwrap_or(&[]);
            buf[..tail.len()].copy_from_slice(tail);
            u64::from_le_bytes(buf)
        }
    }
}

#[derive(Clone, Copy)]
struct Slot {
    lo: u64,
    hi: u64,
    len: u32,
    id: u32,
}

const EMPTY_SLOT: Slot = Slot {
    lo: 0,
    hi: 0,
    len: 0,
    id: VACANT,
};

pub struct Dict {
    slots: Vec<Slot>,
    /// Term bytes, back to back; term `id` is `text[spans[id].0..spans[id].1]`.
    text: Vec<u8>,
    spans: Vec<(u32, u32)>,
}

impl Dict {
    pub fn new() -> Dict {
        Dict {
            slots: vec![EMPTY_SLOT; 1 << 12],
            text: Vec::new(),
            spans: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn text(&self, id: u32) -> &[u8] {
        let (start, end) = self.spans[id as usize];
        &self.text[start as usize..end as usize]
    }

    /// The id of `term` (lowercase, 1 to 32 bytes), adding it if it is new.
    pub fn insert(&mut self, term: &[u8]) -> u32 {
        if let Some(id) = self.get(term, 0, term.len()) {
            return id;
        }
        let id = self.spans.len() as u32;
        let start = self.text.len() as u32;
        self.text.extend_from_slice(term);
        self.spans.push((start, self.text.len() as u32));
        // Keep the load factor at or under 3/4 so misses stay short.
        if 4 * self.spans.len() > 3 * self.slots.len() {
            self.slots = vec![EMPTY_SLOT; 2 * self.slots.len()];
            (0..=id).for_each(|old| self.place(old));
        } else {
            self.place(id);
        }
        id
    }

    fn place(&mut self, id: u32) {
        let term = self.text(id);
        let key = Key::new(term, 0, term.len());
        let mask = self.slots.len() - 1;
        let mut i = key.hash() & mask;
        while self.slots[i].id != VACANT {
            i = (i + 1) & mask;
        }
        self.slots[i] = Slot {
            lo: key.lo,
            hi: key.hi,
            len: key.len,
            id,
        };
    }

    /// Starts looking up the token `data[start..end]` (any case): hashes it and
    /// prefetches its slot, so that a run of probes can wait on memory in parallel
    /// before `resolve` reads them. `None` if the token is too long to be a term.
    #[inline]
    pub fn probe(&self, data: &[u8], start: usize, end: usize) -> Option<Probe> {
        let len = end - start;
        if len > MAX_TERM {
            return None;
        }
        let key = Key::new(data, start, len);
        let slot = key.hash() & (self.slots.len() - 1);
        prefetch(&self.slots[slot]);
        Some(Probe { key, slot, start })
    }

    /// The id of a probed token, if it is a query term.
    #[inline]
    pub fn resolve(&self, probe: &Probe, data: &[u8]) -> Option<u32> {
        let (key, mask) = (probe.key, self.slots.len() - 1);
        let mut i = probe.slot;
        loop {
            let slot = self.slots[i];
            if slot.id == VACANT {
                return None;
            }
            if slot.lo == key.lo && slot.hi == key.hi && slot.len == key.len {
                // Keys hold 16 bytes; a longer token must also match the rest.
                let len = key.len as usize;
                let rest = probe.start + 16..probe.start + len;
                if len <= 16 || token_eq(&data[rest], &self.text(slot.id)[16..]) {
                    return Some(slot.id);
                }
            }
            i = (i + 1) & mask;
        }
    }

    pub fn get(&self, data: &[u8], start: usize, end: usize) -> Option<u32> {
        self.probe(data, start, end)
            .and_then(|probe| self.resolve(&probe, data))
    }
}

/// A token whose dictionary slot is being fetched.
pub struct Probe {
    key: Key,
    slot: usize,
    start: usize,
}

/// Hints the CPU to start loading `item`'s cache line.
#[inline]
pub fn prefetch<T>(item: &T) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a prefetch is only a hint; it never faults or changes memory.
    unsafe {
        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        _mm_prefetch::<_MM_HINT_T0>((item as *const T).cast());
    }
}

/// Whether a corpus token (any case) spells the lowercase `term`.
#[inline]
pub fn token_eq(token: &[u8], term: &[u8]) -> bool {
    token.len() == term.len() && token.iter().zip(term).all(|(&a, &b)| a | 0x20 == b)
}
