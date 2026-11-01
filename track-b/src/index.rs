//! Postings of one segment and BOOL evaluation against them.
//!
//! Terms are split into `PARTS` partitions by id, so that the build of a
//! partition only touches its own counters and lists, which stay in cache.
//! Within a segment documents are numbered from 0 as `u16`. Every term has a
//! sorted list; a term in more than 1/`DENSE` of the segment's documents also
//! has a bitmap, which turns AND and NOT against it into bit tests.

use crate::query::{AND, NONE, NOT, OR, TERM_BITS};

pub const PARTS: usize = 64;
const DENSE: usize = 16;

#[derive(Default)]
pub struct Part {
    /// Term `id` (with `id % PARTS` this partition) has the documents
    /// `docs[offsets[l]..offsets[l + 1]]`, where `l = id / PARTS`.
    offsets: Vec<u32>,
    docs: Vec<u16>,
    /// Where a dense term's bitmap starts in `bits`, or `NONE`.
    dense: Vec<u32>,
    bits: Vec<u64>,
    docs_in_segment: usize,
}

impl Part {
    /// Counting sort of the `doc << 16 | l` pairs of `streams` (in document
    /// order) by `l`, for a segment of `docs` documents.
    pub fn build(&mut self, streams: &[&[u32]], terms: usize, docs: usize, cursor: &mut Vec<u32>) {
        self.offsets.clear();
        self.offsets.resize(terms + 1, 0);
        for &x in streams.iter().copied().flatten() {
            self.offsets[(x & 0xFFFF) as usize + 1] += 1;
        }
        for l in 0..terms {
            self.offsets[l + 1] += self.offsets[l];
        }
        cursor.clear();
        cursor.extend_from_slice(&self.offsets[..terms]);
        self.docs.resize(self.offsets[terms] as usize, 0);
        for &x in streams.iter().copied().flatten() {
            let at = &mut cursor[(x & 0xFFFF) as usize];
            self.docs[*at as usize] = (x >> 16) as u16;
            *at += 1;
        }
        let words = docs.div_ceil(64);
        self.docs_in_segment = docs;
        self.dense.clear();
        self.dense.resize(terms, NONE);
        self.bits.clear();
        for l in 0..terms {
            let (from, to) = (self.offsets[l] as usize, self.offsets[l + 1] as usize);
            if (to - from) * DENSE > docs {
                self.dense[l] = self.bits.len() as u32;
                let bits = self.bits.len();
                self.bits.resize(bits + words, 0);
                for &d in &self.docs[from..to] {
                    self.bits[bits + usize::from(d) / 64] |= 1 << (d % 64);
                }
            }
        }
    }
}

/// The documents of one term in one segment.
#[derive(Clone, Copy)]
enum Set<'a> {
    Empty,
    List(&'a [u16]),
    Dense(&'a [u16], &'a [u64]),
}

impl<'a> Set<'a> {
    fn of(parts: &'a [&'a Part], term: usize, words: usize) -> Set<'a> {
        let (part, l) = (parts[term % PARTS], term / PARTS);
        let list = &part.docs[part.offsets[l] as usize..part.offsets[l + 1] as usize];
        match list.len() {
            0 => Set::Empty,
            n if n * DENSE <= part.docs_in_segment => Set::List(list),
            _ => {
                let at = part.dense[l] as usize;
                Set::Dense(list, &part.bits[at..at + words])
            }
        }
    }

    fn list(self) -> &'a [u16] {
        match self {
            Set::Empty => &[],
            Set::List(list) | Set::Dense(list, _) => list,
        }
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
    /// them) that match the BOOL query `steps` to `answer`.
    pub fn run(
        &mut self,
        parts: &[&Part],
        steps: &[u32],
        docs: usize,
        base: u32,
        answer: &mut Answer,
    ) {
        let words = docs.div_ceil(64);
        self.bits.resize(words, 0);
        self.mask.resize(words, 0);
        let mut acc = Acc::Empty;
        for &step in steps {
            let op = step >> TERM_BITS;
            if let (Acc::Empty, AND | NOT) = (acc, op) {
                continue; // nothing to intersect with or subtract from
            }
            let set = Set::of(parts, (step & ((1 << TERM_BITS) - 1)) as usize, words);
            acc = self.step(acc, op, set, docs);
        }
        match acc {
            Acc::Empty => {}
            Acc::Term(set) => answer.add_list(set.list(), base),
            Acc::List => answer.add_list(&self.list, base),
            Acc::Bits => answer.add_bits(&self.bits, base),
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
        if let Set::Empty = set {
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
        let list = if let Acc::List = acc {
            &mine[..]
        } else {
            r.list()
        };
        let next = match (op, set, r) {
            // Two dense sets: one pass over the words.
            (AND | NOT, Set::Dense(_, b), Set::Dense(_, a)) => {
                let not = if op == NOT { !0 } else { 0 };
                for (o, (&a, &b)) in self.bits.iter_mut().zip(a.iter().zip(b)) {
                    *o = a & (b ^ not);
                }
                Acc::Bits
            }
            (AND | NOT, Set::Dense(_, b), _) => filter(list, b, op == NOT, &mut self.out),
            // A dense set and a list: test the list against the bitmap.
            (AND, Set::List(b), Set::Dense(_, a)) => filter(b, a, false, &mut self.out),
            (NOT, Set::List(_), Set::Dense(_, a)) => {
                self.bits.copy_from_slice(a);
                self.list = mine;
                return self.bits_op(NOT, set);
            }
            (AND | NOT, Set::List(b), _) => {
                combine(list, b, op == NOT, &mut self.mask, &mut self.out)
            }
            _ if (list.len() + set.list().len()) * DENSE <= docs => {
                union(list, set.list(), &mut self.out)
            }
            // A large OR: spread both sides into the bitmap.
            _ => {
                match r {
                    Set::Dense(_, a) => self.bits.copy_from_slice(a),
                    _ => {
                        self.bits.fill(0);
                        set_bits(&mut self.bits, list);
                    }
                }
                self.list = mine;
                return self.bits_op(OR, set);
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

fn set_bits(bits: &mut [u64], list: &[u16]) {
    for &d in list {
        bits[usize::from(d) / 64] |= 1 << (d % 64);
    }
}

/// The documents of `list` whose bit is set (clear, if `not`). Written
/// branch-free: every document is stored, and kept by advancing the count.
fn filter<'a>(list: &[u16], bits: &[u64], not: bool, out: &mut Vec<u16>) -> Acc<'a> {
    out.resize(list.len(), 0);
    let mut n = 0;
    for &d in list {
        out[n] = d;
        n += usize::from((bits[usize::from(d) / 64] >> (d % 64) & 1 != 0) != not);
    }
    out.truncate(n);
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

/// `a` without (`not`) or with only the documents of `b`: `b` is spread into
/// `mask` and `a` tested against it, which is about three times faster than
/// a merge (whose steps depend on each other) once neither list is tiny.
fn sift<'a>(a: &[u16], b: &[u16], not: bool, mask: &mut [u64], out: &mut Vec<u16>) -> Acc<'a> {
    set_bits(mask, b);
    filter(a, mask, not, out);
    b.iter().for_each(|&d| mask[usize::from(d) / 64] = 0);
    Acc::List
}

/// `a` with only (or, if `not`, without) the documents of `b`. When one list
/// is `GALLOP` times longer the shorter one gallops through it; otherwise the
/// one that decides is spread into `mask` and the other tested against it.
fn combine<'a>(a: &[u16], b: &[u16], not: bool, mask: &mut [u64], out: &mut Vec<u16>) -> Acc<'a> {
    if !not && a.len() > b.len() {
        return combine(b, a, not, mask, out); // AND is symmetric: `a` the shorter
    }
    if a.len() * GALLOP >= b.len() {
        // NOT spreads what it removes; AND spreads the shorter list.
        let (tested, spread) = if not { (a, b) } else { (b, a) };
        return sift(tested, spread, not, mask, out);
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

fn union<'a>(a: &[u16], b: &[u16], out: &mut Vec<u16>) -> Acc<'a> {
    out.resize(a.len() + b.len(), 0);
    let (mut i, mut j, mut n) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        out[n] = x.min(y);
        n += 1;
        i += usize::from(x <= y);
        j += usize::from(y <= x);
    }
    out.truncate(n);
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    Acc::List
}

/// count, sum of ids and the ten smallest ids of a set of documents (spec 4).
#[derive(Clone, Copy, Default)]
pub struct Answer {
    count: u32,
    kept: u32,
    sum: u64,
    ten: [u32; 10],
}

impl Answer {
    /// Adds one document; ids must arrive in ascending order.
    pub fn push(&mut self, id: u32) {
        self.count += 1;
        self.sum += u64::from(id);
        if self.kept < 10 {
            self.ten[self.kept as usize] = id;
            self.kept += 1;
        }
    }

    /// Adds a sorted list of documents numbered from `base`, all above the
    /// documents added so far.
    fn add_list(&mut self, list: &[u16], base: u32) {
        let n = list.len() as u32;
        self.count += n;
        self.sum +=
            u64::from(n) * u64::from(base) + list.iter().map(|&d| u64::from(d)).sum::<u64>();
        for &d in list.iter().take(10 - self.kept as usize) {
            self.ten[self.kept as usize] = base + u32::from(d);
            self.kept += 1;
        }
    }

    /// Adds every document of a bitmap whose bit 0 is document `base`.
    fn add_bits(&mut self, bits: &[u64], base: u32) {
        for (i, &word) in bits.iter().enumerate().filter(|(_, &w)| w != 0) {
            let first = base + 64 * i as u32;
            let ones = word.count_ones();
            self.count += ones;
            self.sum += u64::from(first) * u64::from(ones) + bit_index_sum(word);
            let mut rest = word;
            while self.kept < 10 && rest != 0 {
                self.ten[self.kept as usize] = first + rest.trailing_zeros();
                self.kept += 1;
                rest &= rest - 1;
            }
        }
    }

    /// Appends the output line (spec 4).
    pub fn write(&self, out: &mut Vec<u8>) {
        put(out, u64::from(self.count));
        out.push(b' ');
        put(out, self.sum);
        for &id in &self.ten[..self.kept as usize] {
            out.push(b' ');
            put(out, u64::from(id));
        }
        out.push(b'\n');
    }
}

/// The sum of the positions of the set bits of `w`, without visiting them:
/// bit k of a position is set for exactly the bits under `MASKS[k]`.
fn bit_index_sum(w: u64) -> u64 {
    const MASKS: [u64; 6] = [
        0xAAAA_AAAA_AAAA_AAAA,
        0xCCCC_CCCC_CCCC_CCCC,
        0xF0F0_F0F0_F0F0_F0F0,
        0xFF00_FF00_FF00_FF00,
        0xFFFF_0000_FFFF_0000,
        0xFFFF_FFFF_0000_0000,
    ];
    let ones = |k: usize| u64::from((w & MASKS[k]).count_ones()) << k;
    (0..6).map(ones).sum()
}

/// Decimal digits of `v`, without the formatting machinery.
fn put(out: &mut Vec<u8>, mut v: u64) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[at..]);
}
