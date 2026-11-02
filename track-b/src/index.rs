//! Postings of one segment and BOOL evaluation against them.
//!
//! Terms are split into `PARTS` partitions by id, so that the build of a
//! partition only touches its own counters and lists, which stay in cache.
//! Within a segment documents are numbered from 0 as `u16`. Every term has a
//! sorted list; a term in more than 1/`DENSE` of the segment's documents also
//! has a bitmap, which turns AND and NOT against it into bit tests.

use crate::query::{Step, AND, NONE, NOT, OR};
use std::arch::x86_64::*;

pub const PARTS: usize = 64;
const DENSE: usize = 64;

/// The words of a bitmap of `docs` documents, whole blocks of four (the
/// unit of `count_and_sum`); the bits past `docs` stay clear.
fn words(docs: usize) -> usize {
    docs.div_ceil(256) * 4
}

#[derive(Default)]
pub struct Part {
    /// Term `id` (with `id % PARTS` this partition) has the documents
    /// `docs[offsets[l]..offsets[l + 1]]`, where `l = id / PARTS`.
    offsets: Vec<u32>,
    docs: Vec<u16>,
    /// Where a dense term's bitmap starts in `bits`, or `NONE`.
    dense: Vec<u32>,
    bits: Vec<u64>,
}

impl Part {
    /// Counting sort of the `doc << 16 | l` pairs of `streams` (in document
    /// order) by `l`, for a segment of `docs` documents. Every `l` is below
    /// `terms` (the scan emits only term ids below `Table::ids`, and
    /// `terms * PARTS >= Table::ids`).
    pub fn build(&mut self, streams: &[&[u32]], terms: usize, docs: usize) {
        self.offsets = vec![0; terms + 1];
        let counts = &mut self.offsets[1..];
        // The streams were written by other threads: the count runs 256 bytes
        // ahead of itself with prefetches, so their lines arrive in time.
        for stream in streams {
            for (i, pairs) in stream.chunks(16).enumerate() {
                crate::prefetch(stream.as_ptr().wrapping_add(16 * i + 64));
                for &x in pairs {
                    // SAFETY: `x & 0xFFFF < terms`, the length of `counts` (see above).
                    unsafe { *counts.get_unchecked_mut((x & 0xFFFF) as usize) += 1 };
                }
            }
        }
        for l in 0..terms {
            self.offsets[l + 1] += self.offsets[l];
        }
        let total = self.offsets[terms] as usize;
        self.docs.clear();
        self.docs.reserve(total);
        let out = self.docs.spare_capacity_mut().as_mut_ptr();
        let cursor = &mut self.offsets[..terms];
        for stream in streams {
            for &x in *stream {
                // SAFETY: `l < terms` (see above), the length of `cursor`; the
                // cursor of `l` runs from the term's offset to the next one, so
                // each of the `total` slots is written exactly once.
                unsafe {
                    let at = cursor.get_unchecked_mut((x & 0xFFFF) as usize);
                    (*out.add(*at as usize)).write((x >> 16) as u16);
                    *at += 1;
                }
            }
        }
        // SAFETY: every slot below `total` was written above.
        unsafe { self.docs.set_len(total) };
        // Each cursor ended at the next term's offset: shift them back.
        self.offsets.rotate_right(1);
        self.offsets[0] = 0;
        self.dense = vec![NONE; terms];
        self.bits.clear();
        for l in 0..terms {
            let (from, to) = (self.offsets[l] as usize, self.offsets[l + 1] as usize);
            if (to - from) * DENSE > docs {
                let at = self.bits.len();
                self.dense[l] = at as u32;
                self.bits.resize(at + words(docs), 0);
                set_bits(&mut self.bits[at..], &self.docs[from..to]);
            }
        }
    }
}

/// Starts loading what `Evaluator::run` will read for the terms of the BOOL
/// query `steps`, whose cache misses would otherwise stall it one after
/// another: first (not `lists`) each term's entries, its list's bounds and
/// bitmap index; a few queries later (`lists`) the first line of its list
/// and of its bitmap, which those entries locate.
pub fn prefetch(parts: &[&Part], steps: &[Step], lists: bool) {
    for &(_, term) in steps {
        let (part, l) = (parts[term as usize % PARTS], term as usize / PARTS);
        if lists {
            crate::prefetch(part.docs.as_ptr().wrapping_add(part.offsets[l] as usize));
            if part.dense[l] != NONE {
                crate::prefetch(part.bits.as_ptr().wrapping_add(part.dense[l] as usize));
            }
        } else {
            crate::prefetch(part.offsets.as_ptr().wrapping_add(l));
            crate::prefetch(part.dense.as_ptr().wrapping_add(l));
        }
    }
}

/// The documents of one term in one segment.
#[derive(Clone, Copy)]
enum Set<'a> {
    List(&'a [u16]),
    Dense(&'a [u16], &'a [u64]),
}

impl<'a> Set<'a> {
    /// The set of `term` (below `Table::ids`, so that `l` is below the
    /// `terms` the parts were built for) in the `PARTS` parts of a segment.
    fn of(parts: &'a [&'a Part], term: usize, docs: usize) -> Set<'a> {
        // SAFETY: `Part::build` makes `offsets` (ascending, ending at
        // `docs.len()`) one longer than `dense` (of `terms` entries, `parts`
        // being the `PARTS` parts of a segment), and gives every term whose
        // list holds more than 1/`DENSE` of the documents a bitmap of `words`
        // words at `dense[l]`; `l` is below `dense.len()` (see above).
        unsafe {
            let (part, l) = (*parts.get_unchecked(term % PARTS), term / PARTS);
            let offsets = part.offsets.as_ptr().add(l);
            let (from, to) = (*offsets, *offsets.add(1));
            let list = part.docs.get_unchecked(from as usize..to as usize);
            match list.len() {
                n if n * DENSE <= docs => Set::List(list),
                _ => {
                    let at = *part.dense.get_unchecked(l) as usize;
                    Set::Dense(list, part.bits.get_unchecked(at..at + words(docs)))
                }
            }
        }
    }

    fn list(self) -> &'a [u16] {
        let (Set::List(list) | Set::Dense(list, _)) = self;
        list
    }
}

/// The running result of a BOOL query. The first term's set is borrowed
/// until a second one has to be combined with it; after that the result is
/// the evaluator's `list` or its `bits`.
#[derive(Clone, Copy)]
enum Acc<'a> {
    Empty,
    Term(Set<'a>),
    List,
    Bits,
}

#[derive(Default)]
pub struct Evaluator {
    list: Vec<u16>,
    out: Vec<u16>,
    bits: Vec<u64>,
    /// A list spread into bitmap form while another is tested against it;
    /// all zero between uses.
    mask: Vec<u64>,
}

impl Evaluator {
    /// Adds the documents of segment `parts` (numbered from `base`, `docs` of
    /// them) that match the BOOL query `steps` to the answer `to`.
    pub fn run(&mut self, parts: &[&Part], steps: &[Step], docs: u32, base: u32, to: &mut Answer) {
        let docs = docs as usize;
        let words = words(docs);
        self.bits.resize(words, 0);
        self.mask.resize(words, 0);
        let present = |term: u32| {
            let (part, l) = (parts[term as usize % PARTS], term as usize / PARTS);
            // SAFETY: `offsets` has an entry past every `l` (see `Set::of`).
            unsafe { *part.offsets.get_unchecked(l + 1) > *part.offsets.get_unchecked(l) }
        };
        // The query matches nothing here if a term ANDed after the last OR
        // is absent (the first step counts as an OR), or if every term that
        // can add documents (the ORed ones) is absent.
        let last_or = steps.iter().rposition(|&(op, _)| op == OR).unwrap_or(0);
        let ands = &steps[last_or..];
        let and_absent = ands.iter().any(|&(op, term)| op == AND && !present(term));
        if and_absent || !steps.iter().any(|&(op, term)| op == OR && present(term)) {
            return;
        }
        let mut acc = Acc::Empty;
        for &(op, term) in steps {
            if let (Acc::Empty, AND | NOT) = (acc, op) {
                continue; // nothing to intersect with or subtract from
            }
            acc = self.step(acc, op, Set::of(parts, term as usize, docs), docs);
        }
        match acc {
            Acc::Empty => {}
            Acc::Term(set) => to.add_list(set.list(), base),
            Acc::List => to.add_list(&self.list, base),
            Acc::Bits => to.add_bits(&self.bits, base),
        }
    }

    /// `acc op set`. In order: an empty `set` (AND empties `acc`, OR and NOT
    /// keep it); an empty `acc` (OR borrows `set`, AND and NOT stay empty); a
    /// bitmap `acc` (`bits_op`); otherwise `acc` is a list, either the borrowed
    /// first term or `self.list` (which `Acc::List` stands for, and which is
    /// taken out while `self.out` receives the result). A list is combined by
    /// bitmap tests against a dense set, by `combine` or a merge with a list,
    /// or, for a large OR, spread into the bitmap.
    fn step<'a>(&mut self, acc: Acc<'a>, op: u32, set: Set<'a>, docs: usize) -> Acc<'a> {
        if set.list().is_empty() {
            return if op == AND { Acc::Empty } else { acc };
        }
        let r = match acc {
            Acc::Empty if op == OR => return Acc::Term(set),
            Acc::Empty => return Acc::Empty,
            Acc::Bits => return self.bits_op(op, set),
            Acc::Term(r) => r,
            Acc::List => Set::List(&[]), // stands for `self.list`, taken below
        };
        let mine = std::mem::take(&mut self.list);
        let list = match acc {
            Acc::List => &mine[..],
            _ => r.list(),
        };
        let (out, mask) = (&mut self.out, &mut self.mask);
        let next = match (op, set, r) {
            // Two dense sets: one pass over the words.
            (AND | NOT, Set::Dense(_, b), Set::Dense(_, a)) => {
                let not = if op == NOT { !0 } else { 0 };
                for (o, (&a, &b)) in self.bits.iter_mut().zip(a.iter().zip(b)) {
                    *o = a & (b ^ not);
                }
                Acc::Bits
            }
            (AND | NOT, Set::Dense(_, b), _) => filter(list, b, op == NOT, out),
            // A dense set and a list: test the list against the bitmap.
            (AND, Set::List(b), Set::Dense(_, a)) => filter(b, a, false, out),
            (NOT, Set::List(_), Set::Dense(_, a)) => {
                self.bits.copy_from_slice(a);
                self.bits_op(NOT, set)
            }
            (AND | NOT, Set::List(b), _) => combine(list, b, op == NOT, mask, out),
            _ if (list.len() + set.list().len()) * DENSE <= docs => union(list, set.list(), out),
            // A large OR: spread both sides into the bitmap.
            _ => {
                match r {
                    Set::Dense(_, a) => self.bits.copy_from_slice(a),
                    _ => {
                        self.bits.fill(0);
                        set_bits(&mut self.bits, list);
                    }
                }
                self.bits_op(OR, set)
            }
        };
        self.list = mine;
        if let Acc::List = next {
            std::mem::swap(&mut self.list, &mut self.out);
            if self.list.is_empty() {
                return Acc::Empty;
            }
        }
        next
    }

    /// `bits = bits op set`; AND with a list gives a list.
    fn bits_op<'a>(&mut self, op: u32, set: Set<'a>) -> Acc<'a> {
        match (op, set) {
            (_, Set::Dense(_, b)) => self.bits.iter_mut().zip(b).for_each(|(a, &b)| {
                *a = match op {
                    AND => *a & b,
                    OR => *a | b,
                    _ => *a & !b,
                }
            }),
            (AND, _) => {
                filter(set.list(), &self.bits, false, &mut self.list);
                // A list, or `Empty` if nothing is left.
                return [Acc::List, Acc::Empty][usize::from(self.list.is_empty())];
            }
            (OR, _) => set_bits(&mut self.bits, set.list()),
            _ => {
                for &d in set.list() {
                    self.bits[usize::from(d) / 64] &= !(1 << (d % 64));
                }
            }
        }
        Acc::Bits
    }
}

/// Whether every document of the sorted `list` has a bit in `bits`: its
/// last is the largest. The loops below index `bits` unchecked after this.
fn fits(list: &[u16], bits: &[u64]) -> bool {
    list.last().is_none_or(|&d| bits.len() > d as usize / 64)
}

fn set_bits(bits: &mut [u64], list: &[u16]) {
    assert!(fits(list, bits));
    for &d in list {
        // SAFETY: `d / 64 < bits.len()` for every `d` (checked above).
        unsafe { *bits.get_unchecked_mut(usize::from(d) / 64) |= 1 << (d % 64) };
    }
}

/// `PACK[m]`: the shuffle that moves the u16 lanes set in `m` (of eight) to
/// the front, in order.
static PACK: [[u8; 16]; 256] = {
    let (mut t, mut i) = ([[0x80; 16]; 256], 0usize);
    while i < 2048 {
        // Lane `lane` of `m`, if set, goes after the set lanes below it.
        let (m, lane) = (i / 8, i % 8);
        if m >> lane & 1 == 1 {
            let k = (m & ((1 << lane) - 1)).count_ones() as usize;
            (t[m][2 * k], t[m][2 * k + 1]) = (2 * lane as u8, 2 * lane as u8 + 1);
        }
        i += 1;
    }
    t
};

/// The documents of the sorted `list` whose bit is set (clear, if `not`),
/// eight at a time: their 32-bit words of `bits` are gathered, each one's
/// bit is shifted to the top of its lane, and the documents kept are packed
/// to the front with a shuffle from `PACK`; branch-free.
fn filter<'a>(list: &[u16], bits: &[u64], not: bool, out: &mut Vec<u16>) -> Acc<'a> {
    assert!(fits(list, bits));
    out.clear();
    out.reserve(list.len() + 8); // each step stores eight lanes
    let (to, mut n) = (out.as_mut_ptr(), 0);
    let (blocks, rest) = list.as_chunks::<8>();
    let flip = if not { 0xFF } else { 0 };
    // SAFETY: every document has a bit in `bits` (checked above), so every
    // gathered word is inside it; the stores stay within `list.len() + 8`
    // u16s, as `n` counts the documents kept; AVX2 is required by the crate.
    unsafe {
        let words: *const i32 = bits.as_ptr().cast();
        for block in blocks {
            let docs = _mm_loadu_si128(block.as_ptr().cast());
            let wide = _mm256_cvtepu16_epi32(docs);
            let word = _mm256_i32gather_epi32::<4>(words, _mm256_srli_epi32::<5>(wide));
            let low = _mm256_and_si256(wide, _mm256_set1_epi32(31));
            let up = _mm256_sub_epi32(_mm256_set1_epi32(31), low);
            let keep = _mm256_movemask_ps(_mm256_castsi256_ps(_mm256_sllv_epi32(word, up))) ^ flip;
            let pack = _mm_loadu_si128(PACK[keep as usize].as_ptr().cast());
            _mm_storeu_si128(to.add(n).cast(), _mm_shuffle_epi8(docs, pack));
            n += keep.count_ones() as usize;
        }
        for &d in rest {
            *to.add(n) = d;
            let bit = *bits.get_unchecked(usize::from(d) / 64) >> (d % 64) & 1 != 0;
            n += usize::from(bit != not);
        }
        out.set_len(n);
    }
    Acc::List
}

/// When one list is this many times longer, the short one gallops through it.
const GALLOP: usize = 16;

/// The first index `i >= from` with `list[i] >= x`, by doubling then bisecting.
fn seek(list: &[u16], from: usize, x: u16) -> usize {
    let (mut lo, mut step) = (from, 1);
    while lo + step < list.len() && list[lo + step] < x {
        lo += step;
        step *= 2;
    }
    let hi = (lo + step + 1).min(list.len());
    lo + list[lo..hi].partition_point(|&y| y < x)
}

/// `a` with only (or, if `not`, without) the documents of `b`. When one list
/// is `GALLOP` times longer the shorter one gallops through it; otherwise the
/// one that decides is spread into `mask` and the other tested against it,
/// which is about three times faster than a merge (whose steps depend on each
/// other) once neither list is tiny.
fn combine<'a>(a: &[u16], b: &[u16], not: bool, mask: &mut [u64], out: &mut Vec<u16>) -> Acc<'a> {
    if !not && a.len() > b.len() {
        return combine(b, a, not, mask, out); // AND is symmetric: `a` the shorter
    }
    if a.len() * GALLOP >= b.len() {
        // NOT spreads what it removes; AND spreads the shorter list.
        let (tested, spread) = if not { (a, b) } else { (b, a) };
        set_bits(mask, spread);
        filter(tested, mask, not, out);
        spread.iter().for_each(|&d| mask[usize::from(d) / 64] = 0);
        return Acc::List;
    }
    out.clear();
    let mut at = 0;
    for &x in a {
        at = seek(b, at, x);
        if (at < b.len() && b[at] == x) != not {
            out.push(x);
        }
    }
    Acc::List
}

/// `a` or `b`. When one list is `GALLOP` times longer, each document of the
/// short one is sought in it and the runs in between are copied whole;
/// otherwise a branch-free merge.
fn union<'a>(a: &[u16], b: &[u16], out: &mut Vec<u16>) -> Acc<'a> {
    out.clear();
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short.len() * GALLOP < long.len() {
        let mut at = 0;
        for &x in short {
            let i = seek(long, at, x);
            out.extend_from_slice(&long[at..i]);
            out.push(x);
            at = i + usize::from(long.get(i) == Some(&x));
        }
        out.extend_from_slice(&long[at..]);
        return Acc::List;
    }
    out.reserve(a.len() + b.len());
    let (to, mut n) = (out.as_mut_ptr(), 0);
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        // SAFETY: each step consumes at least one element of `a` or `b`, so
        // `n < a.len() + b.len()`, the room reserved above.
        unsafe { *to.add(n) = x.min(y) };
        n += 1;
        i += usize::from(x <= y);
        j += usize::from(y <= x);
    }
    // SAFETY: the first `n` elements were written above.
    unsafe { out.set_len(n) };
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    Acc::List
}

/// count, sum of ids and the ten smallest ids of a set of documents (spec 4).
/// The first `min(count, 10)` of `ten` are filled.
#[derive(Clone, Copy, Default)]
pub struct Answer {
    count: u32,
    sum: u64,
    ten: [u32; 10],
}

impl Answer {
    /// Adds `count` documents whose numbers add up to `sum`, numbered from
    /// `base`; `ids` are their numbers in ascending order (at least the first
    /// ten), and all above the documents added so far.
    fn add(&mut self, base: u32, count: u64, sum: u64, ids: impl Iterator<Item = u32>) {
        let kept = (self.count as usize).min(10);
        for (slot, id) in self.ten[kept..].iter_mut().zip(ids) {
            *slot = base + id;
        }
        self.count += count as u32;
        self.sum += count * u64::from(base) + sum;
    }

    /// Adds one document.
    pub fn push(&mut self, id: u32) {
        self.add(0, 1, id.into(), [id].into_iter());
    }

    /// Adds a sorted list of documents numbered from `base`.
    fn add_list(&mut self, list: &[u16], base: u32) {
        let sum = list.iter().map(|&d| u64::from(d)).sum();
        self.add(base, list.len() as u64, sum, list.iter().map(|&d| d.into()));
    }

    /// Adds every document of a bitmap whose bit 0 is document `base`.
    fn add_bits(&mut self, bits: &[u64], base: u32) {
        let (count, sum) = count_and_sum(bits);
        let ids = bits.iter().enumerate().flat_map(|(i, &word)| {
            let mut rest = word;
            std::iter::from_fn(move || {
                let bit = rest.trailing_zeros(); // 64 when nothing is left
                (bit < 64).then(|| {
                    rest &= rest - 1;
                    64 * i as u32 + bit
                })
            })
        });
        self.add(base, count, sum, ids);
    }

    /// Appends the output line (spec 4).
    pub fn write(&self, out: &mut Vec<u8>) {
        // At most 12 numbers of up to 20 digits, each with a space or the LF.
        let (mut line, mut at) = ([0u8; 12 * 21], 0);
        let ten = self.ten.iter().take(self.count as usize).map(|&i| i.into());
        for v in [u64::from(self.count), self.sum].into_iter().chain(ten) {
            at = put(&mut line, at, v);
            line[at] = b' ';
            at += 1;
        }
        line[at - 1] = b'\n';
        out.extend_from_slice(&line[..at]);
    }
}

/// The number of set bits of `bits` (whole blocks of four words) and the
/// sum of their positions, four words at a time. Per byte, nibble tables give
/// the count of its bits and the sum of their positions within the byte; a
/// byte's offset in its word (8 times its index) weighs its count, and the
/// word's index weighs the word's count.
fn count_and_sum(bits: &[u64]) -> (u64, u64) {
    // Bits set in a nibble, and the sum of their positions.
    const COUNT: [i8; 16] = [0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4];
    const SUM: [i8; 16] = [0, 0, 1, 1, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6];
    // SAFETY: AVX2 is required by the crate; the loads read whole blocks.
    unsafe {
        let table = |t: [i8; 16]| _mm256_broadcastsi128_si256(_mm_loadu_si128(t.as_ptr().cast()));
        let (count_t, sum_t, nibbles) = (table(COUNT), table(SUM), _mm256_set1_epi8(0x0F));
        let offsets = _mm256_set1_epi64x(0x3830_2820_1810_0800); // 8 * byte index
        let [mut counts, mut sums, mut weighted] = [_mm256_setzero_si256(); 3];
        let mut index = _mm256_setr_epi64x(0, 1, 2, 3);
        for block in bits.as_chunks::<4>().0 {
            let v = _mm256_loadu_si256(block.as_ptr().cast());
            let lo = _mm256_and_si256(v, nibbles);
            let hi = _mm256_and_si256(_mm256_srli_epi16::<4>(v), nibbles);
            let c_hi = _mm256_shuffle_epi8(count_t, hi);
            let c = _mm256_add_epi8(_mm256_shuffle_epi8(count_t, lo), c_hi);
            // Within a byte: the low nibble's positions, the high one's plus 4 each.
            let s_lo = _mm256_shuffle_epi8(sum_t, lo);
            let s = _mm256_add_epi8(s_lo, _mm256_shuffle_epi8(sum_t, hi));
            let s = _mm256_add_epi8(s, _mm256_slli_epi16::<2>(c_hi));
            let word_counts = _mm256_sad_epu8(c, _mm256_setzero_si256());
            counts = _mm256_add_epi64(counts, word_counts);
            weighted = _mm256_add_epi64(weighted, _mm256_mul_epu32(word_counts, index));
            // Byte offsets times counts, in pairs, plus the sums within bytes
            // (at most 888 a pair), added up per 32 bits.
            let pairs = _mm256_maddubs_epi16(c, offsets);
            let pairs = _mm256_add_epi16(pairs, _mm256_maddubs_epi16(s, _mm256_set1_epi8(1)));
            sums = _mm256_add_epi32(sums, _mm256_madd_epi16(pairs, _mm256_set1_epi16(1)));
            index = _mm256_add_epi64(index, _mm256_set1_epi64x(4));
        }
        let lanes = |v: __m256i| std::mem::transmute::<_, [u64; 4]>(v).iter().sum::<u64>();
        // The 32-bit sums, two to a lane, added as 64-bit lanes would carry.
        let (high, low) = (_mm256_srli_epi64::<32>(sums), _mm256_slli_epi64::<32>(sums));
        let sums = _mm256_add_epi64(high, _mm256_srli_epi64::<32>(low));
        (lanes(counts), lanes(sums) + 64 * lanes(weighted))
    }
}

/// Writes the decimal digits of `v` to `line` from `at`, two at a time,
/// without the formatting machinery; returns where they end.
fn put(line: &mut [u8], at: usize, mut v: u64) -> usize {
    /// `PAIRS[2i..2i + 2]`: the two digits of i.
    const PAIRS: &[u8; 200] =
        b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";
    let (mut digits, mut from) = ([0u8; 20], 20);
    while v >= 10 {
        let pair = 2 * (v % 100) as usize;
        (from, v) = (from - 2, v / 100);
        digits[from..from + 2].copy_from_slice(&PAIRS[pair..pair + 2]);
    }
    // A top digit when the number of digits is odd (or v was 0). When it is
    // even, the last pair taken is the top two digits, so has no leading zero.
    if v > 0 || from == 20 {
        from -= 1;
        digits[from] = b'0' + v as u8;
    }
    line[at..at + 20 - from].copy_from_slice(&digits[from..]);
    at + 20 - from
}
