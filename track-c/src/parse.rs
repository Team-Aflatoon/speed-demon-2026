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

use crate::names::{Budget, Key, Pairs};
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
/// '0' in every byte of a word.
const ZEROS: u64 = 0x3030_3030_3030_3030;

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
                // SAFETY: separators are before the chunk's end (or at it).
                if unsafe { byte(b, lf) } != b'\n' && lf != end {
                    return Err(Fail::Malformed(record));
                }
                let (k, bit) = ((lf - window) / 64, (lf - window) % 64);
                let below = (self.quote_mask[k] & !(u64::MAX << bit)).count_ones();
                let quotes_at = self.quotes_before[k] + below;
                let f = &mut self.records[r];
                let ok = fields(f, sh, b, record, s, quotes_at - quotes_at_record);
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
                let name = &b[f.name as usize..];
                let id = sh.table.find(&f.key, name, &mut out.budget)?;
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
    if b.len() < HEADER.len() + 8 {
        return None;
    }
    // SAFETY: just checked.
    let p = 3 * usize::from(unsafe { bom_at(b, 0) });
    let q = p + HEADER.len();
    let crlf = usize::from(b[q] == b'\r');
    (b[p..q] == *HEADER && b[q + crlf] == b'\n').then_some(q + crlf + 1)
}

/// Parses into `f` the record starting at `at` whose separators (four
/// commas, then the terminator or the end of file) are at `s`, with `q`
/// quote bytes in it. The text fields
/// need no parsing: a field's raw bytes are between its separators (and its
/// quotes, if it is quoted), and its unescaped length is its width, less its
/// delimiters, less one per `""`.
#[inline(always)]
fn fields(f: &mut Record, sh: &Shared, b: &[u8], at: usize, s: &[u32; 5], q: u32) -> Option<()> {
    let [c0, c1, c2, c3, lf] = s.map(|c| c as usize);
    // SAFETY (both): the record starts at `at` and ends at `lf`, at most the
    // chunk's end; `c0 + 1` and `c1 + 1` are at most `c2`, and `lf - 1` is
    // at least `at`. These are the only bytes read below.
    let byte = |p| unsafe { byte(b, p) };
    let bom = usize::from(unsafe { bom_at(b, at) });
    let cr = usize::from((byte(lf - 1) == b'\r') & (byte(lf) == b'\n'));
    // A BOM directly after a terminator is not data (spec 2.1).
    let (id, region, cents) = numbers(b, at + 3 * bom, [c0, c2, c3], lf - cr)?;
    let (name_quoted, note_quoted) = (i64::from(byte(c0 + 1) == b'"'), i64::from(byte(c1 + 1) == b'"'));
    // Name and note together; only the quotes, delimiting or escaping, are
    // not value bytes, and an escape is two of them for one value byte.
    let text = c2 as i64 - c0 as i64 - 2 - name_quoted - note_quoted - i64::from(q / 2);
    let name = c0 + 1 + name_quoted as usize;
    let len = (c1 - name).checked_sub(name_quoted as usize)?;
    // Every id past the map's end shares its trailing 0 (`_UNMATCHED`).
    let region = region.min(sh.regions.map.len() as u64 - 1) as u32;
    (f.key, f.name, f.region) = (sh.table.key(b, name, len), name as u32, region);
    (f.id, f.cents, f.text) = (id as i64, cents, text);
    Some(())
}

/// Masks of the bytes equal to `"`, `,` and LF among the 64 at `o`; bit i
/// stands for byte `o + i`.
#[inline(always)]
fn masks(b: &[u8], o: usize) -> [u64; 3] {
    debug_assert!(o + 64 <= b.len());
    let block = b.as_ptr().wrapping_add(o).cast::<simd::__m256i>();
    // SAFETY: `block` points to 64 readable bytes: blocks start before the
    // chunk's end, which `PAD` bytes follow. The intrinsics need AVX2,
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

/// Little-endian load of the 8 bytes at `p`, unchecked: about one in
/// twenty of the hot loop's instructions was this bounds check.
///
/// # Safety
/// `p + 8 <= b.len()`. Every caller loads within a chunk's buffer at most
/// 24 bytes past the end of a field or name (`Table::key`), so at most 32
/// past the chunk's end, and every buffer has `PAD` (128) bytes after it.
#[inline(always)]
pub unsafe fn load(b: &[u8], p: usize) -> u64 {
    debug_assert!(p + 8 <= b.len());
    // SAFETY: the caller's promise.
    unsafe { b.as_ptr().add(p).cast::<u64>().read_unaligned() }
}

/// The byte at `p`, unchecked like `load`.
///
/// # Safety
/// `p < b.len()`: every caller's `p` is within a record of the chunk, or
/// its terminator, so at most the chunk's end, which `PAD` bytes follow.
#[inline(always)]
unsafe fn byte(b: &[u8], p: usize) -> u8 {
    debug_assert!(p < b.len());
    // SAFETY: the caller's promise.
    unsafe { *b.get_unchecked(p) }
}

/// Whether a BOM starts at `p`.
///
/// # Safety
/// As for `load`: `p` is a record start (or 0), at most the chunk's end.
unsafe fn bom_at(b: &[u8], p: usize) -> bool {
    // SAFETY: the caller's promise.
    unsafe { load(b, p) & 0xFF_FFFF == BOM }
}

/// A record's id, region id and amount in cents: the fields `b[id..c0]`,
/// `b[c2 + 1..c3]` and `b[c3 + 1..end]`. In the common case (ids of up to 8
/// digits, amounts of up to 5 before the dot: everything 2.2 writes) the
/// three are checked and converted together, as the lanes of one AVX2
/// register; anything else takes the slow `number` and `cents`.
#[inline(always)]
fn numbers(b: &[u8], id: usize, [c0, c2, c3]: [usize; 3], end: usize) -> Option<(u64, u64, i64)> {
    // SAFETY: `c3 + 1` is at most `end`, within the chunk; `end - 3` is
    // read only when the amount has at least four bytes.
    let negative = unsafe { byte(b, c3 + 1) } == b'-';
    let whole = end.wrapping_sub(c3 + 4 + usize::from(negative)); // digits before the dot
    let (n_id, n_region) = (c0.wrapping_sub(id), c3.wrapping_sub(c2 + 1));
    let common = (1..=8).contains(&n_id) & (1..=8).contains(&n_region) & (1..=5).contains(&whole);
    if !common || unsafe { byte(b, end - 3) } != b'.' {
        return slow(b, id, [c0, c2, c3], end);
    }
    // Each number right-aligned in a word: its n digits at the top (the
    // bytes after the field fall off), and '0's below them.
    // SAFETY (`bzhi`, the low `shift` bits of '0's): BMI2 is part of
    // x86-64-v3, which the whole crate is compiled for.
    let fit = |w: u64, n: usize| {
        let shift = 64 - 8 * n as u32;
        w << shift | unsafe { simd::_bzhi_u64(ZEROS, shift) }
    };
    // The amount's last 8 bytes with the dot shifted out: `whole` digits,
    // then the two after the dot, at the top.
    // SAFETY (the three loads): `end - 8` is at least 0, as a record is at
    // least 10 bytes before its LF; `id` and `c2 + 1` start fields that end
    // before `end`, within the chunk.
    let w = unsafe { load(b, end - 8) };
    let before = !(u64::MAX << (8 * (6 - whole)));
    let undotted = (w & 0xFF_FFFF_FFFF) << 8 | w & 0xFFFF << 48;
    let amount = undotted & !before | ZEROS & before;
    let [id, region] = [id, c2 + 1].map(|p| unsafe { load(b, p) });
    let lanes = [fit(id, n_id), fit(region, n_region), amount, ZEROS];
    let ([id, region, cents, _], sign) = (digits(lanes)?, 1 - 2 * i64::from(negative));
    Some((id, region, sign * cents as i64))
}

/// Four words of eight ASCII digits each (the first in the lowest byte) to
/// their values, or `None` if any byte is not a digit. Adjacent digits
/// combine into pairs, the pairs into fours, the fours into eight.
#[inline(always)]
fn digits(lanes: [u64; 4]) -> Option<[u64; 4]> {
    // SAFETY: plain AVX2 arithmetic on a local array; the whole crate is
    // compiled for AVX2 (see `masks`).
    unsafe {
        let v = simd::_mm256_loadu_si256(lanes.as_ptr().cast());
        let d = simd::_mm256_sub_epi8(v, simd::_mm256_set1_epi8(b'0' as i8));
        let ok = simd::_mm256_cmpeq_epi8(simd::_mm256_min_epu8(d, simd::_mm256_set1_epi8(9)), d);
        let pairs = simd::_mm256_maddubs_epi16(d, simd::_mm256_set1_epi16(10 | 1 << 8));
        let fours = simd::_mm256_madd_epi16(pairs, simd::_mm256_set1_epi32(100 | 1 << 16));
        let high = simd::_mm256_mul_epu32(fours, simd::_mm256_set1_epi64x(10_000));
        let value = simd::_mm256_add_epi64(high, simd::_mm256_srli_epi64::<32>(fours));
        let mut out = [0u64; 4];
        simd::_mm256_storeu_si256(out.as_mut_ptr().cast(), value);
        (simd::_mm256_movemask_epi8(ok) == -1).then_some(out)
    }
}

/// The decimal integer that is exactly `digits` (not empty). 2.1 sets no
/// limit on its length, so this saturates at `u64::MAX`: such a region id is
/// simply unknown, while an id or amount that large cannot be summed in 64
/// bits anyway (2.2 keeps ids below 4 x 10^7 and amounts below 10^5).
/// `numbers` for any other fields, one at a time; out of line, so that the
/// hot loop keeps its registers.
#[cold]
#[inline(never)]
fn slow(b: &[u8], id: usize, [c0, c2, c3]: [usize; 3], end: usize) -> Option<(u64, u64, i64)> {
    let region = number(b.get(c2 + 1..c3)?)?;
    Some((number(b.get(id..c0)?)?, region, cents(b, c3 + 1, end)?))
}

#[cold]
fn number(digits: &[u8]) -> Option<u64> {
    let digits = (!digits.is_empty()).then_some(digits)?;
    digits.iter().try_fold(0u64, |v, &d| {
        let d = d.checked_sub(b'0').filter(|&d| d < 10)?;
        Some(v.saturating_mul(10).saturating_add(u64::from(d)))
    })
}

/// The amount `-?[0-9]+\.[0-9]{2}` that is exactly `b[start..end]`, in
/// cents (see `numbers` for the common case).
#[cold]
fn cents(b: &[u8], start: usize, end: usize) -> Option<i64> {
    let negative = b[start] == b'-';
    let digits = end.checked_sub(start + usize::from(negative) + 3)?;
    let whole = number(&b[end - 3 - digits..end - 3])?;
    let fraction = number(&b[end - 2..end])?;
    let cents = (b[end - 3] == b'.').then_some(whole.checked_mul(100)?.checked_add(fraction)?)?;
    let cents = i64::try_from(cents).ok()?;
    Some(if negative { -cents } else { cents })
}
