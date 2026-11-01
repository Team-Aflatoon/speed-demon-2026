//! records.csv: a parser for the grammar in spec section 2.1, built on
//! 64-byte bitmasks rather than a byte-at-a-time state machine.
//!
//! For each 64-byte block, AVX2 compares give one bit per byte for `"`, `,`
//! and LF. In this grammar a byte is inside a quoted field exactly when an
//! odd number of quotes precede it (a `""` escape closes and reopens), so the
//! prefix XOR of the quote mask is the in-quotes mask, and the commas and LFs
//! outside it are the separators. The LFs among them end the records. A
//! record's four commas are then the first two separators after its start
//! and the last two before its end, read off the masks a word at a time.
//!
//! A chunk is handled a window of blocks at a time, in three tight loops:
//! `scan` makes the masks and lists the records' ends, `parse` reads each
//! record's fields and starts loading the region-map entry and name-table
//! line it needs, and `join` uses them, by which time most of those cache
//! misses have overlapped.

use crate::names::{Key, Pairs, Table};
use crate::run::Shared;
use std::arch::x86_64::*;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

const HEADER: &[u8] = b"id,name,note,region_id,amount";
/// The BOM's bytes EF BB BF as the low three bytes of a little-endian load.
const BOM: u64 = 0xBF_BB_EF;
/// Zero bytes after every chunk in its buffer: the parser reads whole 64-byte
/// blocks, and names' and numbers' words, up to 64 bytes past the end.
pub const PAD: usize = 128;
/// Bytes scanned per window; a multiple of 64.
const WINDOW: usize = 4096;
/// Records parsed before they are joined: a window's worth, at about 61
/// bytes a record (a window with more takes several batches).
const BATCH: usize = 128;
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
    (a[0], a[1], a[2]) = (a[0] + b[0], a[1] + b[1], a[2] + b[2]);
    (a[3], a[4]) = (a[3].min(b[3]), a[4].max(b[4]));
}

/// A record parsed and waiting for its join: its name's key and where its
/// raw bytes start, its region id, and its id, amount in cents and text
/// bytes (see `Agg`). 64 bytes.
#[derive(Clone, Copy, Default)]
struct Record {
    key: Key,
    name: u32,
    region: u32,
    nums: [i64; 3],
}

/// Where a chunk starts and ends in its buffer, and whether it ends the file.
pub type Span = (usize, usize, bool);

/// A record's end (its terminator's offset in the buffer) and the quote
/// bytes in the chunk before it.
type End = [u32; 2];

/// What the record loops read: the name table, the chunk's buffer, and its
/// separator masks with where it starts in the buffer (see `window`).
type Ctx<'a> = (&'a Table, &'a [u8], (&'a [u64], usize));

/// Where a record starts, and the quotes in the chunk before it.
type At = (usize, u32);

/// A worker's parser state, reused for every chunk: per 64-byte block of the
/// chunk its separators (see `window`), then the record ends found in the
/// current window, and a batch of its records.
pub struct Parser {
    seps: Vec<u64>,
    ends: Vec<End>,
    batch: Vec<Record>,
}

/// What a worker accumulates: aggregates by group, and its name pairs.
pub struct Local {
    pub aggs: [Agg; crate::GROUPS],
    pub pairs: Pairs,
}

impl Parser {
    pub fn new() -> Self {
        let (ends, batch) = (vec![[0; 2]; WINDOW + 64], vec![Record::default(); BATCH]);
        let seps = Vec::new();
        Parser { seps, ends, batch }
    }

    /// Parses the records in `b[start..end]` into `out`, `span` being
    /// `(start, end, eof)`: one chunk of the file (after the header, for the
    /// first), read into a buffer and followed there by `PAD` zero bytes. It
    /// must start at a record start; `eof` says whether it ends the file.
    /// Returns the number of quote bytes in it, which `run` uses to check the
    /// chunk cuts.
    ///
    /// Every record must hold exactly four commas outside quotes, with valid
    /// numbers in their fields: `parse` checks that each record has at least
    /// four, and this that the chunk has five separators per record in all.
    /// What is not diagnosed is a quote placed where 2.1 forbids it (inside
    /// an unquoted field, or not followed by a separator after closing).
    pub fn chunk(&mut self, sh: &Shared, b: &[u8], span: Span, out: &mut Local) -> Res<u64> {
        let (start, end, eof) = span;
        // One block more than the chunk's at each end (see `window`).
        let blocks = (end - start) / 64 + 3;
        self.seps.resize(self.seps.len().max(blocks), 0);
        // In-quotes state (all ones or 0) after the last block, quotes and
        // separators so far; records so far, and where the next one starts.
        let (mut state, mut records, mut at) = ([0, 0, 0], 0, (start, 0));
        for window in (start..end).step_by(WINDOW) {
            let last = (window + WINDOW).min(end);
            // SAFETY: main checked that the CPU has PCLMULQDQ.
            let mut n = unsafe { self.scan(b, [start, window, last], &mut state) };
            // The final record may end at end of file without a terminator.
            let tail = if n > 0 { self.ends[n - 1][0] as usize + 1 } else { at.0 };
            if eof && last == end && tail < end && state[0] == 0 {
                (self.ends[n], n, state[2]) = ([end as u32, state[1] as u32], n + 1, state[2] + 1);
            }
            let c = (&sh.table, b, (&self.seps[..], start));
            for ends in self.ends[..n].chunks(BATCH) {
                let batch = &mut self.batch[..ends.len()];
                at = parse(c, sh.regions.map, ends, batch, at)?;
                join(sh, b, batch, out)?;
            }
            records += n as u64;
        }
        // `at.0` is one past `end` after a final record with no terminator.
        let whole = at.0.min(end) == end.max(start) && state[2] == 5 * records;
        whole.then_some(state[1]).ok_or(Fail::Malformed(at.0))
    }

    /// Makes the separator masks of the blocks of `b[window..last]` and lists
    /// the ends of the records they end in `self.ends`; returns how many.
    ///
    /// # Safety
    /// The CPU must have PCLMULQDQ (see `prefix_xor`).
    #[target_feature(enable = "pclmulqdq")]
    unsafe fn scan(&mut self, b: &[u8], [start, window, last]: [usize; 3], state: &mut [u64; 3]) -> usize {
        let (first, mut n) = ((window - start) / 64 + 1, 0);
        let blocks = (last - window).div_ceil(64);
        for (k, sep) in self.seps[first..first + blocks].iter_mut().enumerate() {
            let o = window + 64 * k;
            let [quote, comma, lf] = masks(b, o);
            let inside = prefix_xor(quote) ^ state[0];
            state[0] = ((inside as i64) >> 63) as u64; // bit 63, broadcast
            *sep = (comma | lf) & !inside;
            n = push_ends(&mut self.ends, n, lf & !inside, [o as u64, quote, state[1]]);
            state[1] += u64::from(quote.count_ones());
            state[2] += u64::from(sep.count_ones());
        }
        n
    }
}

/// The 64 separator bits of the buffer's bytes `p - 64` to `p - 1`, for a `p`
/// from the start of the chunk at `start` to 64 bytes past its end. `seps`
/// holds the chunk's block k at index k + 1: index 0, an empty block, lets a
/// record's last 64 bytes reach back before the chunk, and one block after
/// it lets its first 64 reach past it. Bits past the window being parsed are
/// stale; see `fields`.
fn window((seps, start): (&[u64], usize), p: usize) -> u64 {
    let i = p - start;
    debug_assert!(i / 64 + 1 < seps.len());
    // SAFETY: as above, so within the chunk's blocks and the one after.
    let [lo, hi] = [0, 1].map(|j| u128::from(*unsafe { seps.get_unchecked(i / 64 + j) }));
    ((hi << 64 | lo) >> (i % 64)) as u64
}

/// Parses the records ending at `ends` into `out`, the first starting at
/// `at.0` with `at.1` quotes before it in the chunk, and starts loading what
/// their joins will need. Returns where the next record starts, and the
/// quotes before it.
#[inline(never)]
fn parse(c: Ctx, map: &[AtomicU8], ends: &[End], out: &mut [Record], at: At) -> Res<At> {
    let (mut s, mut quotes_before) = at;
    for (&[e, quotes], f) in ends.iter().zip(out) {
        let (e, q) = (e as usize, quotes.wrapping_sub(quotes_before));
        if !fields(f, c, (s, e), q) {
            slow(f, c, (s, e), q).ok_or(Fail::Malformed(s))?;
        }
        crate::prefetch(map, f.region as usize);
        c.0.prefetch(&f.key);
        (s, quotes_before) = (e + 1, quotes);
    }
    Ok((s, quotes_before))
}

/// Joins the parsed records: their groups' aggregates, and their names'
/// (id, group) pairs. Every region id past the map's end shares its last
/// entry, a 0 (`_UNMATCHED`).
#[inline(never)]
fn join(sh: &Shared, b: &[u8], records: &[Record], out: &mut Local) -> Res<()> {
    let map = sh.regions.map;
    for f in records {
        // SAFETY (both): the map has at least one entry (main sized it), and
        // a name starts within its record.
        let group = unsafe { map.get_unchecked((f.region as usize).min(map.len() - 1)) }.load(Relaxed);
        let [id, cents, text] = f.nums;
        merge(&mut out.aggs[usize::from(group)], &[id, cents, text, cents, cents]);
        let id = sh.table.find(&f.key, unsafe { b.get_unchecked(f.name as usize..) })?;
        out.pairs.push(id, group);
    }
    Ok(())
}

/// Parses into `f` the record `b[s..e]`, `e` being its terminator (or the
/// end of file), with `q` quote bytes in it, if it takes the common form:
/// its four commas within 64 bytes of its ends (in 64-bit windows of the
/// separator masks), an id and region id of at most 8 digits, an amount of
/// at most 5 before the dot. Anything else is left to `slow`.
///
/// The windows may hold stale bits past the window being parsed, but only
/// at bytes past `e`; a record with fewer than four separators of its own
/// might take one of those for its second comma, which then is not before
/// its third, and `slow` decides.
#[inline(always)]
fn fields(f: &mut Record, c: Ctx, (s, e): (usize, usize), q: u32) -> bool {
    let (b, m) = (c.1, c.2);
    let (head, tail) = (window(m, s + 64), window(m, e));
    let (head2, top) = (head & head.wrapping_sub(1), tail.leading_zeros().min(63));
    let tail2 = tail & !(1 << (63 - top));
    let [c0, c1] = [head, head2].map(|w| s + w.trailing_zeros() as usize);
    // Wrapping: with too few separators the positions may be junk, and the
    // checks below reject them.
    let [c2, c3] = [tail2.leading_zeros(), top].map(|z| e.wrapping_sub(1 + z as usize));
    if (head2 == 0) | (tail2 == 0) | (c1 >= c2) {
        return false;
    }
    // SAFETY (all reads): the record starts at `s` and ends at `e`, at most
    // the chunk's end, and these bytes are within it.
    let byte = |p| unsafe { byte(b, p) };
    let end = e - usize::from((byte(e - 1) == b'\r') & (byte(e) == b'\n'));
    let negative = byte(c3 + 1) == b'-';
    let whole = end.wrapping_sub(c3 + 4 + usize::from(negative)); // digits before the dot
    let (n_id, n_region) = (c0 - s, c3 - c2 - 1);
    // All three lengths in range at once: `n - 1` wraps below 0.
    let common = (n_id.wrapping_sub(1) | n_region.wrapping_sub(1)) < 8 && whole.wrapping_sub(1) < 5;
    if !common || byte(end - 3) != b'.' {
        return false;
    }
    // Each number right-aligned in a word: its n digits at the top (the
    // bytes after the field fall off), and '0's below them. The amount's
    // last 8 bytes have the dot shifted out first.
    // SAFETY (`bzhi`, the low `shift` bits of '0's): BMI2 is part of
    // x86-64-v3, which the whole crate is compiled for. The loads: `end - 8`
    // is at least `s`, as a record is at least 10 bytes before its
    // terminator; `s` and `c2 + 1` start fields that end before `end`.
    let fit = |p: usize, n: usize| unsafe { load(b, p) << (64 - 8 * n) | _bzhi_u64(ZEROS, 64 - 8 * n as u32) };
    let w = unsafe { load(b, end - 8) };
    let undotted = (w & 0xFF_FFFF_FFFF) << 8 | w & 0xFFFF << 48;
    let before = !(u64::MAX << (8 * (6 - whole)));
    let lanes = [fit(s, n_id), fit(c2 + 1, n_region), undotted & !before | ZEROS & before, ZEROS];
    // A non-digit, a BOM before the id among them, takes the slow path.
    let Some([id, region, cents, _]) = digits(lanes) else {
        return false;
    };
    let cents = (1 - 2 * i64::from(negative)) * cents as i64;
    finish(f, c, [c0, c1, c2], [id as i64, region as i64, cents], q);
    true
}

/// `fields` for any record that is not in its common form: its separators
/// found bit by bit, and its numbers one byte at a time; `None` if it breaks
/// the grammar. Out of line, so that the hot loop keeps its registers.
#[cold]
#[inline(never)]
fn slow(f: &mut Record, ctx: Ctx, (s, e): (usize, usize), q: u32) -> Option<()> {
    let b = ctx.1;
    let mut seps = (s..e).filter(|&p| window(ctx.2, p + 64) & 1 == 1);
    let c = [seps.next()?, seps.next()?, seps.next()?, seps.next()?];
    let crlf = e > c[3] + 1 && b[e - 1] == b'\r' && b.get(e) == Some(&b'\n');
    // A BOM directly after a terminator is not data (spec 2.1).
    let id = s + 3 * usize::from(b[s..].starts_with(&BOM.to_le_bytes()[..3]));
    let [id, region] = [(id, c[0]), (c[2] + 1, c[3])].map(|(from, to)| number(&b[from..to]));
    let amount = &b[c[3] + 1..e - usize::from(crlf)];
    let region = region?.min(u64::from(u32::MAX));
    let (nums, extra) = ([id? as i64, region as i64, cents(amount)?], seps.next());
    extra.is_none().then(|| finish(f, ctx, [c[0], c[1], c[2]], nums, q))
}

/// The rest of `fields`, given a record's first three commas and its
/// numbers: its text bytes and its name's key. The text fields need no
/// parsing: a field's raw bytes are between its separators (and its quotes,
/// if it is quoted), and its unescaped length is its width, less its
/// delimiters, less one per `""`.
#[inline(always)]
fn finish(f: &mut Record, (t, b, _): Ctx, [c0, c1, c2]: [usize; 3], nums: [i64; 3], q: u32) {
    let [id, region, cents] = nums;
    // SAFETY: `c0 + 1` and `c1 + 1` are at most `c2`, within the record.
    let quoted = |c: usize| usize::from(unsafe { byte(b, c + 1) } == b'"');
    let (name_quoted, note_quoted) = (quoted(c0), quoted(c1));
    // Name and note together; only the quotes, delimiting or escaping, are
    // not value bytes, and an escape is two of them for one value byte.
    let text = c2 as i64 - c0 as i64 - 2 - (name_quoted + note_quoted) as i64 - i64::from(q / 2);
    let name = c0 + 1 + name_quoted;
    let len = (c1 - name).saturating_sub(name_quoted);
    // `slow` clamps a huge region id; `join`, any past the map.
    (f.key, f.name, f.region) = (t.key(b, name, len), name as u32, region as u32);
    f.nums = [id, cents, text];
}

/// Where the first record starts, after the BOM-prefixed header line.
pub fn header(b: &[u8]) -> Option<usize> {
    let p = 3 * usize::from(b.starts_with(&BOM.to_le_bytes()[..3]));
    let q = p + HEADER.len();
    let crlf = usize::from(b.get(q) == Some(&b'\r'));
    (b.get(p..q) == Some(HEADER) && b.get(q + crlf) == Some(&b'\n')).then_some(q + crlf + 1)
}

/// Masks of the bytes equal to `"`, `,` and LF among the 64 at `o`; bit i
/// stands for byte `o + i`.
fn masks(b: &[u8], o: usize) -> [u64; 3] {
    debug_assert!(o + 64 <= b.len());
    let block = b.as_ptr().wrapping_add(o).cast::<__m256i>();
    // SAFETY: `block` points to 64 readable bytes: blocks start before the
    // chunk's end, which `PAD` bytes follow. The intrinsics need AVX2,
    // which the whole crate is compiled for (`-C target-cpu=x86-64-v3` in
    // .cargo/config.toml; main.rs refuses to build without it).
    unsafe {
        let halves = [block, block.add(1)].map(|h| _mm256_loadu_si256(h));
        (*b"\",\n").map(|c| {
            let c = _mm256_set1_epi8(c as i8);
            let [lo, hi] = halves.map(|h| _mm256_movemask_epi8(_mm256_cmpeq_epi8(h, c)) as u32);
            u64::from(lo) | u64::from(hi) << 32
        })
    }
}

/// Bit i of the result is the XOR of bits 0..=i: the in-quotes mask. That
/// is a carry-less multiplication by all ones, one instruction.
#[target_feature(enable = "pclmulqdq")]
fn prefix_xor(x: u64) -> u64 {
    let product = _mm_clmulepi64_si128(_mm_set_epi64x(0, x as i64), _mm_set1_epi8(-1), 0);
    _mm_cvtsi128_si64(product) as u64
}

/// Appends the record ends in the block at `o`, the set bits of `mask`, to
/// `ends[n..]` with the quotes before each (`before`, plus those of `quotes`
/// below it), and returns the new count. Most blocks end at most two
/// records, so two are written unconditionally (junk past the count, which
/// later appends overwrite) and only more take the loop. `n` is at most the
/// LFs of the window's blocks before this one, so the writes stay below
/// `WINDOW + 64`.
fn push_ends(ends: &mut [End], n: usize, mut mask: u64, [o, quotes, before]: [u64; 3]) -> usize {
    debug_assert!(n + 64 <= ends.len());
    let count = mask.count_ones() as usize;
    let mut end = |i: usize, mask: u64| {
        let below = (quotes & (mask & mask.wrapping_neg()).wrapping_sub(1)).count_ones();
        let at = (o + u64::from(mask.trailing_zeros())) as u32;
        // SAFETY: as above.
        *unsafe { ends.get_unchecked_mut(i) } = [at, before as u32 + below];
        mask & mask.wrapping_sub(1)
    };
    mask = end(n, mask);
    mask = end(n + 1, mask);
    let mut i = n + 2;
    while mask != 0 {
        (mask, i) = (end(i, mask), i + 1);
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
unsafe fn byte(b: &[u8], p: usize) -> u8 {
    debug_assert!(p < b.len());
    // SAFETY: the caller's promise.
    unsafe { *b.get_unchecked(p) }
}

/// Four words of eight ASCII digits each (the first in the lowest byte) to
/// their values, or `None` if any byte is not a digit. Adjacent digits
/// combine into pairs, the pairs into fours, the fours into eight.
fn digits(lanes: [u64; 4]) -> Option<[u64; 4]> {
    // SAFETY: plain AVX2 arithmetic on a local array; the whole crate is
    // compiled for AVX2 (see `masks`).
    unsafe {
        let v = _mm256_loadu_si256(lanes.as_ptr().cast());
        let d = _mm256_sub_epi8(v, _mm256_set1_epi8(b'0' as i8));
        let ok = _mm256_cmpeq_epi8(_mm256_min_epu8(d, _mm256_set1_epi8(9)), d);
        let pairs = _mm256_maddubs_epi16(d, _mm256_set1_epi16(10 | 1 << 8));
        let fours = _mm256_madd_epi16(pairs, _mm256_set1_epi32(100 | 1 << 16));
        let high = _mm256_mul_epu32(fours, _mm256_set1_epi64x(10_000));
        let value = _mm256_add_epi64(high, _mm256_srli_epi64::<32>(fours));
        let mut out = [0u64; 4];
        _mm256_storeu_si256(out.as_mut_ptr().cast(), value);
        (_mm256_movemask_epi8(ok) == -1).then_some(out)
    }
}

/// The decimal integer that is exactly `digits` (not empty). 2.1 sets no
/// limit on its length, so this saturates at `u64::MAX`: such a region id is
/// simply unknown, while an id or amount that large cannot be summed in 64
/// bits anyway (2.2 keeps ids below 4 x 10^7 and amounts below 10^5).
#[cold]
fn number(digits: &[u8]) -> Option<u64> {
    let digits = (!digits.is_empty()).then_some(digits)?;
    digits.iter().try_fold(0u64, |v, &d| {
        let d = d.checked_sub(b'0').filter(|&d| d < 10)?;
        Some(v.saturating_mul(10).saturating_add(u64::from(d)))
    })
}

/// The amount `-?[0-9]+\.[0-9]{2}` that is exactly `field`, in cents (see
/// `fields` for the common case).
#[cold]
fn cents(field: &[u8]) -> Option<i64> {
    let (sign, digits) = field.strip_prefix(b"-").map_or((1, field), |d| (-1, d));
    let (whole, fraction) = digits.split_at(digits.len().checked_sub(3)?);
    if fraction[0] != b'.' {
        return None;
    }
    let (whole, fraction) = (number(whole)?, number(&fraction[1..])?);
    let cents = whole.checked_mul(100)?.checked_add(fraction)?;
    Some(sign * i64::try_from(cents).ok()?)
}
