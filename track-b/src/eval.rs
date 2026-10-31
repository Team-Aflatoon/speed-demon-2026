//! Answering one query against one batch, and merging batch answers.

use crate::dict::{token_eq, Dict, ALNUM};
use crate::index::{Bits, Index, Set, WORDS};
use crate::query::{Op, Step};
use crate::simd::pair_mask;

/// count, sum of ids and the ten smallest ids of a set of documents (spec 4).
#[derive(Clone, Copy, Default)]
pub struct Answer {
    pub count: u32,
    pub len: u32,
    pub sum: u64,
    pub ten: [u32; 10],
}

impl Answer {
    pub fn ten(&self) -> &[u32] {
        &self.ten[..self.len as usize]
    }

    /// Adds a document; ids must arrive in ascending order.
    #[inline]
    fn push(&mut self, id: u32) {
        self.count += 1;
        self.sum += u64::from(id);
        if self.len < 10 {
            self.ten[self.len as usize] = id;
            self.len += 1;
        }
    }

    /// Adds every document of a bitmap whose bit 0 is document `base`.
    fn push_bits(&mut self, bits: &Bits, base: u32) {
        for (i, &word) in bits.iter().enumerate().filter(|(_, &w)| w != 0) {
            let first = base + 64 * i as u32;
            let ones = word.count_ones();
            self.count += ones;
            self.sum += u64::from(first) * u64::from(ones) + bit_index_sum(word);
            let mut rest = word;
            while self.len < 10 && rest != 0 {
                self.ten[self.len as usize] = first + rest.trailing_zeros();
                self.len += 1;
                rest &= rest - 1;
            }
        }
    }

    /// Adds the answer for another, disjoint set of documents.
    pub fn merge(&mut self, other: &Answer) {
        self.count += other.count;
        self.sum += other.sum;
        if self.len == 10 && other.ten[0] > self.ten[9] {
            return; // the usual case: later batches only hold larger ids
        }
        let (a, b) = (self.ten(), other.ten());
        let (mut i, mut j, mut out, mut n) = (0, 0, [0; 10], 0);
        while n < 10 && (i < a.len() || j < b.len()) {
            if j == b.len() || (i < a.len() && a[i] < b[j]) {
                out[n] = a[i];
                i += 1;
            } else {
                out[n] = b[j];
                j += 1;
            }
            n += 1;
        }
        (self.ten, self.len) = (out, n as u32);
    }
}

/// The sum of the positions of the set bits of `w`, without visiting them:
/// bit k of a position is set for exactly the bits under `MASKS[k]`, so each
/// mask contributes its popcount times 2^k.
fn bit_index_sum(w: u64) -> u64 {
    const MASKS: [u64; 6] = [
        0xAAAA_AAAA_AAAA_AAAA,
        0xCCCC_CCCC_CCCC_CCCC,
        0xF0F0_F0F0_F0F0_F0F0,
        0xFF00_FF00_FF00_FF00,
        0xFFFF_0000_FFFF_0000,
        0xFFFF_FFFF_0000_0000,
    ];
    MASKS
        .iter()
        .enumerate()
        .map(|(k, m)| u64::from((w & m).count_ones()) << k)
        .sum()
}

/// The running result of a query within a batch. Until two sets really have
/// to be combined only the first one is borrowed (`Term`), so a single-term
/// query or `t AND <absent>` costs nothing. A combined result lives in the
/// evaluator's scratch space: as a sorted list while it is small (ANDing with a
/// list can only shrink it, so that never touches a bitmap), else as a bitmap.
#[derive(Clone, Copy)]
enum Acc<'a> {
    Empty,
    Term(Set<'a>),
    List,
    Bits,
}

pub struct Evaluator {
    bits: Bits,
    list: Vec<u16>,
}

impl Evaluator {
    pub fn new() -> Evaluator {
        Evaluator {
            bits: [0; WORDS],
            list: Vec::new(),
        }
    }

    /// The answer of one query (BOOL or phrase) within the batch `index`.
    pub fn run(
        &mut self,
        index: &Index,
        steps: &[Step],
        phrase: bool,
        data: &[u8],
        dict: &Dict,
    ) -> Answer {
        let mut acc = Acc::Empty;
        for step in steps {
            acc = self.step(acc, step.op, index.set(step.term));
            if phrase && matches!(acc, Acc::Empty) {
                return Answer::default(); // a phrase term is missing from the batch
            }
        }
        let mut answer = Answer::default();
        let base = index.first_doc;
        match (phrase, self.view(acc)) {
            (false, Set::Bits(bits)) => answer.push_bits(bits, base),
            (false, set) => for_each_doc(set, |d| answer.push(base + u32::from(d))),
            // A phrase's AND gives the documents holding all its terms; check each.
            (true, set) => for_each_doc(set, |d| {
                let (start, end) = index.docs[d as usize];
                if phrase_in(&data[start..end], steps, dict) {
                    answer.push(base + u32::from(d));
                }
            }),
        }
        answer
    }

    fn view<'a>(&'a self, acc: Acc<'a>) -> Set<'a> {
        match acc {
            Acc::Empty => Set::Empty,
            Acc::Term(set) => set,
            Acc::List => Set::List(&self.list),
            Acc::Bits => Set::Bits(&self.bits),
        }
    }

    fn step<'a>(&mut self, acc: Acc<'a>, op: Op, set: Set<'a>) -> Acc<'a> {
        match (acc, op, set) {
            (_, Op::And, Set::Empty) => Acc::Empty,
            (acc, _, Set::Empty) => acc,
            (Acc::Empty, Op::Or, set) => Acc::Term(set),
            (Acc::Empty, _, _) => Acc::Empty,
            (Acc::Term(Set::List(first)), op, set) => {
                self.list.clear();
                self.list.extend_from_slice(first);
                self.on_list(op, set)
            }
            (Acc::Term(Set::Bits(first)), op, set) => {
                self.bits = *first;
                self.on_bits(op, set)
            }
            (Acc::Term(Set::Empty), _, _) => unreachable!("a borrowed term is never empty"),
            (Acc::List, op, set) => self.on_list(op, set),
            (Acc::Bits, op, set) => self.on_bits(op, set),
        }
    }

    /// `list = list op set`, for a non-empty `set`.
    fn on_list<'a>(&mut self, op: Op, set: Set<'a>) -> Acc<'a> {
        match (op, set) {
            (Op::Or, set) => {
                self.bits = [0; WORDS];
                self.list
                    .iter()
                    .for_each(|&d| self.bits[d as usize / 64] |= 1 << (d % 64));
                return self.on_bits(Op::Or, set);
            }
            (Op::And, Set::Bits(bits)) => self.list.retain(|&d| has(bits, d)),
            (Op::Not, Set::Bits(bits)) => self.list.retain(|&d| !has(bits, d)),
            (Op::And, Set::List(other)) => retain_sorted(&mut self.list, other, true),
            (Op::Not, Set::List(other)) => retain_sorted(&mut self.list, other, false),
            (_, Set::Empty) => {}
        }
        if self.list.is_empty() {
            Acc::Empty
        } else {
            Acc::List
        }
    }

    /// `bits = bits op set`, for a non-empty `set`.
    fn on_bits<'a>(&mut self, op: Op, set: Set<'a>) -> Acc<'a> {
        let bits = &mut self.bits;
        match (op, set) {
            (Op::And, Set::List(list)) => {
                self.list.clear();
                self.list.extend(list.iter().filter(|&&d| has(bits, d)));
                return if self.list.is_empty() {
                    Acc::Empty
                } else {
                    Acc::List
                };
            }
            (Op::Or, Set::List(list)) => list
                .iter()
                .for_each(|&d| bits[d as usize / 64] |= 1 << (d % 64)),
            (Op::Not, Set::List(list)) => list
                .iter()
                .for_each(|&d| bits[d as usize / 64] &= !(1 << (d % 64))),
            (Op::Or, Set::Bits(other)) => bits.iter_mut().zip(other).for_each(|(a, b)| *a |= b),
            (Op::Not, Set::Bits(other)) => bits.iter_mut().zip(other).for_each(|(a, b)| *a &= !b),
            (Op::And, Set::Bits(other)) => bits.iter_mut().zip(other).for_each(|(a, b)| *a &= b),
            (_, Set::Empty) => {}
        }
        // Once AND or NOT has emptied the result, later ANDs and NOTs are free.
        if op != Op::Or && bits.iter().all(|&w| w == 0) {
            Acc::Empty
        } else {
            Acc::Bits
        }
    }
}

#[inline]
fn has(bits: &Bits, d: u16) -> bool {
    bits[d as usize / 64] >> (d % 64) & 1 != 0
}

/// Keeps the elements of the sorted `list` that are (or with `keep_common`
/// false, are not) in the sorted `other`, in one merge pass.
fn retain_sorted(list: &mut Vec<u16>, other: &[u16], keep_common: bool) {
    let mut j = 0;
    list.retain(|&d| {
        while j < other.len() && other[j] < d {
            j += 1;
        }
        (j < other.len() && other[j] == d) == keep_common
    });
}

fn for_each_doc(set: Set, mut f: impl FnMut(u16)) {
    match set {
        Set::Empty => {}
        Set::List(list) => list.iter().for_each(|&d| f(d)),
        Set::Bits(bits) => {
            for (i, &word) in bits.iter().enumerate() {
                let mut rest = word;
                while rest != 0 {
                    f((64 * i) as u16 + rest.trailing_zeros() as u16);
                    rest &= rest - 1;
                }
            }
        }
    }
}

/// Whether the document `text` holds the phrase at consecutive token positions.
///
/// Rather than tokenizing the whole document, this finds the whole-token
/// occurrences of the phrase's longest term (usually its rare anchor) and reads
/// tokens outwards from each one, only as far as the phrase reaches.
fn phrase_in(text: &[u8], steps: &[Step], dict: &Dict) -> bool {
    let term = |i: usize| dict.text(steps[i].term);
    let anchor = (0..steps.len()).max_by_key(|&i| term(i).len()).unwrap_or(0);
    occurrences(text, term(anchor)).any(|start| {
        let mut left = start;
        let before = (0..anchor).rev().all(|i| match token_before(text, left) {
            Some((s, e)) if token_eq(&text[s..e], term(i)) => {
                left = s;
                true
            }
            _ => false,
        });
        let mut right = start + term(anchor).len();
        before
            && (anchor + 1..steps.len()).all(|i| match token_after(text, right) {
                Some((s, e)) if token_eq(&text[s..e], term(i)) => {
                    right = e;
                    true
                }
                _ => false,
            })
    })
}

/// The last token that ends at or before `pos`.
fn token_before(text: &[u8], pos: usize) -> Option<(usize, usize)> {
    let end = text[..pos].iter().rposition(|&b| ALNUM[b as usize])? + 1;
    let start = text[..end]
        .iter()
        .rposition(|&b| !ALNUM[b as usize])
        .map_or(0, |i| i + 1);
    Some((start, end))
}

/// The first token that starts at or after `pos`.
fn token_after(text: &[u8], pos: usize) -> Option<(usize, usize)> {
    let start = pos + text[pos..].iter().position(|&b| ALNUM[b as usize])?;
    let end = text[start..]
        .iter()
        .position(|&b| !ALNUM[b as usize])
        .map_or(text.len(), |i| start + i);
    Some((start, end))
}

/// Start offsets of the whole-token occurrences of `word` in `text`, any case.
///
/// Candidates come 32 positions at a time from a vector test of each
/// position's first and last byte; only those are compared in full.
fn occurrences<'t>(text: &'t [u8], word: &'t [u8]) -> impl Iterator<Item = usize> + 't {
    let (first, last, len) = (word[0], word[word.len() - 1], word.len());
    let starts = (text.len() + 1).saturating_sub(len);
    (0..starts)
        .step_by(32)
        .flat_map(move |base| {
            let window = |at: usize| -> [u8; 32] {
                match text.get(at..at + 32) {
                    Some(bytes) => bytes.try_into().unwrap(),
                    None => {
                        let mut bytes = [0; 32]; // zero never matches a term byte
                        bytes[..text.len() - at].copy_from_slice(&text[at..]);
                        bytes
                    }
                }
            };
            let keep = if starts - base >= 32 {
                !0
            } else {
                (1u32 << (starts - base)) - 1
            };
            let mask = pair_mask(&window(base), &window(base + len - 1), first, last) & keep;
            std::iter::successors(Some(mask), |m| Some(m & (m - 1)))
                .take_while(|&m| m != 0)
                .map(move |m| base + m.trailing_zeros() as usize)
        })
        .filter(move |&i| {
            let whole = (i == 0 || !ALNUM[text[i - 1] as usize])
                && (i + len == text.len() || !ALNUM[text[i + len] as usize]);
            whole
                && text[i..i + len]
                    .iter()
                    .zip(word)
                    .all(|(&a, &b)| ALNUM[a as usize] && a | 0x20 == b)
        })
}
