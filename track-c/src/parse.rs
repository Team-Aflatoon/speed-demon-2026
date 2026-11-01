//! records.csv: a parser for the grammar in spec section 2.1, built on
//! 64-byte bitmasks rather than a byte-at-a-time state machine.
//!
//! For each 64-byte block, AVX2 compares give one bit per byte for `"`, `,`
//! and LF. In this grammar a byte is inside a quoted field exactly when an
//! odd number of quotes precede it (a `""` escape closes and reopens), so the
//! prefix XOR of the quote mask is the in-quotes mask, and the commas and LFs
//! outside it are the separators. Their positions are flattened into a
//! buffer, and every record is then cut at its five separators, its fields
//! parsed independently of each other.
//!
//! A window of blocks is scanned first, then its records are parsed in two
//! passes: the first parses fields and starts loading the region-map entry
//! and name-table slot each record needs, the second uses them, by which
//! time most of those cache misses have overlapped.

use crate::names::{key, Budget, Key, Pairs};
use crate::run::Shared;
use std::arch::x86_64 as simd;
use std::sync::atomic::Ordering::Relaxed;

const HEADER: &[u8] = b"id,name,note,region_id,amount";
/// The BOM's bytes EF BB BF as the low three bytes of a little-endian load.
const BOM: u64 = 0xBF_BB_EF;
pub const BOM_FIRST: u8 = 0xEF;
/// Zero bytes after every chunk in its buffer: the parser reads whole 64-byte
/// blocks, and names' and numbers' words, up to 64 bytes past the end.
pub const PAD: usize = 128;
/// Bytes scanned per window; a multiple of 64.
const WINDOW: usize = 2048;
const BLOCKS: usize = WINDOW / 64;
/// Separators held at once: up to 4 left from the window before, a window's
/// worth, and room for `flatten` to write a whole 64 past the count.
const SEPS: usize = WINDOW + 8;
/// The most records one window completes (each takes five separators).
const RECORDS: usize = WINDOW / 5 + 2;
/// SWAR constants: '0' in every byte, 0x80 - 10 in every byte, and bit 7 of
/// every byte (see `classify`).
const ZEROS: u64 = 0x3030_3030_3030_3030;
const TENS: u64 = 0x7676_7676_7676_7676;
const HIGH: u64 = 0x8080_8080_8080_8080;

/// Why a run could not count the file, worst last.
#[derive(Clone, Copy, Debug)]
pub enum Fail {
    /// More distinct names than the name table was sized for.
    Full,
    /// The bytes break the grammar at this buffer offset, or a chunk did not
    /// start and end at record boundaries.
    Malformed(usize),
}

pub type Res<T> = Result<T, Fail>;

/// Per group, what a worker adds while parsing: `[ids, cents, text, min,
/// max]`, the sums of ids, of amounts in cents and of text bytes, then the
/// smallest and largest amount. `text` sums both text fields' unescaped
/// lengths; note bytes are it minus name bytes. Records, distinct names and
/// name bytes come from phase 2 (names.rs).
pub type Agg = [i64; 5];
pub const EMPTY: Agg = [0, 0, 0, i64::MAX, i64::MIN];

/// Adds `b` into `a`.
pub fn merge(a: &mut Agg, b: &Agg) {
    (0..3).for_each(|k| a[k] += b[k]);
    (a[3], a[4]) = (a[3].min(b[3]), a[4].max(b[4]));
}

/// A record parsed and waiting for its join: its name and where its raw
/// bytes start, its region id (clamped to the region map), id, amount in
/// cents, and text bytes (see `Agg`). 64 bytes.
#[derive(Clone, Copy, Default)]
struct Record {
    key: Key,
    name: u32,
    region: u32,
    id: i64,
    cents: i64,
    text: i64,
}

/// Where a chunk starts and ends in its buffer, and whether it ends the file.
pub type Span = (usize, usize, bool);

/// A worker's parser state, reused for every chunk: the separators found
/// but not yet consumed (`seps[..n]`, offsets into the chunk's buffer), per
/// block of the window its quote mask and the quotes before it, and the
/// records of the window.
pub struct Parser {
    seps: [u32; SEPS],
    quote_mask: [u64; BLOCKS + 1],
    quotes_before: [u32; BLOCKS + 1],
    records: [Record; RECORDS],
}

/// What a worker accumulates: aggregates by group, its name pairs, and its
/// name-table reservations.
pub struct Local {
    pub aggs: [Agg; crate::GROUPS],
    pub pairs: Pairs,
    pub budget: Budget,
}

impl Parser {
    pub fn new() -> Box<Self> {
        // SAFETY: every field is integers, for which all-zero bytes are valid.
        unsafe { Box::new_zeroed().assume_init() }
    }

    /// Parses the records in `b[start..end]` into `out`, `span` being
    /// `(start, end, eof)`: one chunk of the file (after the header, for the
    /// first), read into a buffer and followed there by `PAD` zero bytes. It
    /// must start at a record start; `eof` says whether it ends the file.
    /// Returns the number of quote bytes in it, which `run` uses to check the
    /// chunk cuts.
    ///
    /// The checks here and in `fields` reject every record that is not four
    /// commas and an LF outside quotes with valid numbers between them. What
    /// they do not diagnose is a quote placed where 2.1 forbids it (inside
    /// an unquoted field, or not followed by a separator after closing).
    pub fn chunk(&mut self, sh: &Shared, b: &[u8], span: Span, out: &mut Local) -> Res<u64> {
        let (start, end, eof) = span;
        let (mut n, mut record, mut quotes_at_record) = (0, start, 0);
        let last_region = sh.regions.map.len() as u64 - 1;
        // In-quotes state (all ones or zero) at the end of the last block,
        // quotes so far, and the LFs outside quotes, which must number
        // exactly the records.
        let (mut inside, mut quotes, mut lfs, mut records) = (0u64, 0u32, 0, 0);
        for window in (start..end).step_by(WINDOW) {
            let blocks = (end - window).div_ceil(64).min(BLOCKS);
            for k in 0..blocks {
                let o = window + 64 * k;
                let [quote, comma, lf] = masks(b, o);
                let ins = prefix_xor(quote) ^ inside;
                inside = ((ins as i64) >> 63) as u64; // bit 63, broadcast
                lfs += (lf & !ins).count_ones() as usize;
                (self.quote_mask[k], self.quotes_before[k]) = (quote, quotes);
                quotes += quote.count_ones();
                n = flatten(&mut self.seps, n, (comma | lf) & !ins, o as u32);
            }
            (self.quote_mask[blocks], self.quotes_before[blocks]) = (0, quotes);
            // The final record may end at end of file without a terminator.
            if eof && window + WINDOW >= end && n % 5 == 4 && inside == 0 {
                (self.seps[n], n, lfs) = (end as u32, n + 1, lfs + 1);
            }
            let done = n / 5;
            for r in 0..done {
                let s: &[u32; 5] = self.seps[5 * r..5 * r + 5].try_into().unwrap();
                let lf = s[4] as usize;
                if b[lf] != b'\n' && lf != end {
                    return Err(Fail::Malformed(record));
                }
                let (k, bit) = ((lf - window) / 64, (lf - window) % 64);
                let below = (self.quote_mask[k] & !(u64::MAX << bit)).count_ones();
                let quotes_at = self.quotes_before[k] + below;
                let f = &mut self.records[r];
                let ok = fields(f, b, record, s, quotes_at - quotes_at_record, last_region);
                ok.ok_or(Fail::Malformed(record))?;
                crate::prefetch(&sh.regions.map[f.region as usize]);
                sh.table.prefetch(&f.key);
                (record, quotes_at_record) = (lf + 1, quotes_at);
            }
            for f in &self.records[..done] {
                let group = sh.regions.map[f.region as usize].load(Relaxed);
                let a = &mut out.aggs[usize::from(group)];
                (a[0], a[1], a[2]) = (a[0] + f.id, a[1] + f.cents, a[2] + f.text);
                (a[3], a[4]) = (a[3].min(f.cents), a[4].max(f.cents));
                let id = sh.table.find(&f.key, &b[f.name as usize..], &mut out.budget)?;
                out.pairs.push(id, group);
            }
            self.seps.copy_within(5 * done..n, 0);
            (n, records) = (n - 5 * done, records + done);
        }
        // `record` is one past `end` after a final record with no terminator.
        let whole = n == 0 && record.min(end) == end.max(start) && lfs == records;
        let quotes = whole.then_some(u64::from(quotes));
        quotes.ok_or(Fail::Malformed(record))
    }
}

/// Where the first record starts, after the BOM-prefixed header line.
pub fn header(b: &[u8]) -> Option<usize> {
    let p = 3 * usize::from(bom_at(b, 0));
    let q = p + HEADER.len();
    let crlf = usize::from(b[q] == b'\r');
    (b[p..q] == *HEADER && b[q + crlf] == b'\n').then_some(q + crlf + 1)
}

/// Parses into `f` the record starting at `at` whose separators (four
/// commas, then the terminator or the end of file) are at `s`, with `quotes`
/// quote bytes in it; `last` is the region map's last index. The text fields
/// need no parsing: a field's raw bytes are between its separators (and its
/// quotes, if it is quoted), and its unescaped length is its width, less its
/// delimiters, less one per `""`.
#[inline(always)]
fn fields(f: &mut Record, b: &[u8], at: usize, s: &[u32; 5], quotes: u32, last: u64) -> Option<()> {
    let [c0, c1, c2, c3, lf] = s.map(|c| c as usize);
    // A BOM directly after a terminator is not data (spec 2.1).
    let id = number(b, at + 3 * usize::from(bom_at(b, at)), c0)?;
    let region = number(b, c2 + 1, c3)?;
    let cr = usize::from((b[lf - 1] == b'\r') & (b[lf] == b'\n'));
    let cents = cents(b, c3 + 1, lf - cr)?;
    let name_quoted = i64::from(b[c0 + 1] == b'"');
    let note_quoted = i64::from(b[c1 + 1] == b'"');
    // Name and note together; only the quotes, delimiting or escaping, are
    // not value bytes, and an escape is two of them for one value byte.
    let text = c2 as i64 - c0 as i64 - 2 - name_quoted - note_quoted - i64::from(quotes / 2);
    let name = c0 + 1 + name_quoted as usize;
    let len = (c1 - name).checked_sub(name_quoted as usize)?;
    // Every id past the map's end shares its trailing 0 (`_UNMATCHED`).
    (f.key, f.name, f.region) = (key(b, name, len), name as u32, region.min(last) as u32);
    (f.id, f.cents, f.text) = (id as i64, cents, text);
    Some(())
}

/// Masks of the bytes equal to `"`, `,` and LF among the 64 at `o`; bit i
/// stands for byte `o + i`.
#[inline(always)]
fn masks(b: &[u8], o: usize) -> [u64; 3] {
    let block = b[o..o + 64].as_ptr().cast::<simd::__m256i>();
    // SAFETY: `block` points to 64 readable bytes. The intrinsics need AVX2,
    // which the whole crate is compiled for (`-C target-cpu=x86-64-v3` in
    // .cargo/config.toml; main.rs refuses to build without it).
    unsafe {
        let halves = [block, block.add(1)].map(|h| simd::_mm256_loadu_si256(h));
        (*b"\",\n").map(|c| {
            let c = simd::_mm256_set1_epi8(c as i8);
            let [lo, hi] = halves.map(|h| simd::_mm256_cmpeq_epi8(h, c));
            let [lo, hi] = [lo, hi].map(|m| simd::_mm256_movemask_epi8(m) as u32);
            u64::from(lo) | u64::from(hi) << 32
        })
    }
}

/// Bit i of the result is the XOR of bits 0..=i: the in-quotes mask.
#[inline(always)]
fn prefix_xor(mut x: u64) -> u64 {
    for shift in [1, 2, 4, 8, 16, 32] {
        x ^= x << shift;
    }
    x
}

/// Appends the offsets `base + i` of the set bits i of `mask` to `seps[n..]`
/// and returns the new count: eight per round with no branch per bit. Rounds
/// past the last bit write junk past the count, which later appends
/// overwrite. `n + 64` must be within `seps` (at most 4 + `WINDOW` - 64
/// separators come before a window's last block).
#[inline(always)]
fn flatten(seps: &mut [u32; SEPS], n: usize, mut mask: u64, base: u32) -> usize {
    let count = mask.count_ones() as usize;
    let out: &mut [u32; 64] = (&mut seps[n..n + 64]).try_into().unwrap();
    for round in 0..8 {
        for i in 0..8 {
            out[8 * round + i] = base + mask.trailing_zeros();
            mask &= mask.wrapping_sub(1);
        }
        if mask == 0 {
            break;
        }
    }
    n + count
}

/// Little-endian load of the 8 bytes at `p`.
#[inline(always)]
pub fn load(b: &[u8], p: usize) -> u64 {
    assert!(p + 8 <= b.len());
    // SAFETY: the 8 bytes at `p` are in `b`, as just checked. One compare
    // instead of the slice and conversion checks of the safe form.
    unsafe { b.as_ptr().add(p).cast::<u64>().read_unaligned() }
}

fn bom_at(b: &[u8], p: usize) -> bool {
    load(b, p) & 0xFF_FFFF == BOM
}

/// The decimal integer that is exactly `b[start..end]`. Its length is
/// known, so up to eight digits are checked and converted in one step.
#[inline(always)]
fn number(b: &[u8], start: usize, end: usize) -> Option<u64> {
    let n = end.checked_sub(start).filter(|&n| n > 0)?;
    if n > 8 {
        return long_number(&b[start..end]);
    }
    // Shift the n digits to the top: the bytes after the field fall off, and
    // the vacated low bytes read as leading zeros.
    let (values, other) = classify(load(b, start));
    let shift = 64 - 8 * n;
    (other << shift == 0).then(|| eight_digits(values << shift))
}

/// A number of more than eight digits. 2.1 sets no limit, so this
/// saturates at `u64::MAX`: such a region id is simply unknown, while an id
/// or amount that large cannot be summed in 64 bits anyway (2.2 keeps ids
/// below 4 x 10^7 and amounts below 10^5).
#[cold]
fn long_number(digits: &[u8]) -> Option<u64> {
    digits.iter().try_fold(0u64, |v, &d| {
        let d = d.checked_sub(b'0').filter(|&d| d < 10)?;
        Some(v.saturating_mul(10).saturating_add(u64::from(d)))
    })
}

/// The amount `-?[0-9]+\.[0-9]{2}` that is exactly `b[start..end]`, in
/// cents. Up to five integer digits (every amount 2.2 allows) take one SWAR
/// step: the dot is shifted out of the word that ends at `end`, and the
/// bytes before the field are replaced by '0's.
#[inline(always)]
fn cents(b: &[u8], start: usize, end: usize) -> Option<i64> {
    let negative = b[start] == b'-';
    let digits = end.checked_sub(start + usize::from(negative) + 3)?;
    let cents = if (1..=5).contains(&digits) {
        let w = load(b, end - 8); // a record is at least 10 bytes before its LF
        let undotted = (w & 0xFF_FFFF_FFFF) << 8 | w & 0xFFFF << 48;
        let before = (1u64 << (8 * (6 - digits))) - 1;
        let (values, other) = classify(undotted & !before | ZEROS & before);
        (other == 0 && w >> 40 & 0xFF == u64::from(b'.')).then(|| eight_digits(values))?
    } else {
        let whole = number(b, end - 3 - digits, end - 3)?;
        let fraction = number(b, end - 2, end)?;
        (b[end - 3] == b'.').then_some(whole.checked_mul(100)?.checked_add(fraction)?)?
    };
    let cents = i64::try_from(cents).ok()?;
    Some(if negative { -cents } else { cents })
}

/// The bytes of `w` minus '0', and bit 7 set in every byte that is not a
/// digit. Digits become 0 to 9; any other byte becomes 10 or more (or wraps
/// to 0xD0 or more), which adding 0x76 lifts to bit 7. Borrows and carries
/// only move towards later bytes, so bytes up to and including the first
/// non-digit are classified correctly, which is all the callers rely on.
#[inline(always)]
fn classify(w: u64) -> (u64, u64) {
    let values = w.wrapping_sub(ZEROS);
    (values, (values.wrapping_add(TENS) | values) & HIGH)
}

/// Eight digit values (0..=9, the first in the lowest byte) to an integer:
/// adjacent digits combine into pairs, the pairs into fours, the fours into
/// eight, two lanes per multiply.
#[inline(always)]
fn eight_digits(v: u64) -> u64 {
    let v = v.wrapping_mul(10).wrapping_add(v >> 8) & 0x00FF_00FF_00FF_00FF;
    let v = v.wrapping_mul(1 + (100 << 16)) >> 16 & 0x0000_FFFF_0000_FFFF;
    v.wrapping_mul(1 + (10_000 << 32)) >> 32
}
