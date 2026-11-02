//! records.csv: a parser for the grammar in spec section 2.1, built on
//! 64-byte bitmasks rather than a byte-at-a-time state machine.
//!
//! For each 64-byte block, AVX2 compares give one bit per byte for `"`, `,`
//! and LF. In this grammar a byte is inside a quoted field exactly when an
//! odd number of quotes precede it (a `""` escape closes and reopens), so the
//! prefix XOR of the quote mask is the in-quotes mask, and the commas and LFs
//! outside it are the separators. The LFs among them end the records. The
//! separators' offsets are listed in order, so that a record's are simply
//! the next five: its four commas, then its terminator.
//!
//! A chunk is handled a window of blocks at a time, in three tight loops:
//! `scan` makes the masks and lists the separators, `parse` reads each
//! record's fields and starts loading the region-map entry and name-table
//! line it needs, and `join` uses them, by which time most of those cache
//! misses have overlapped. `join` is itself two loops: the second sets the
//! matrix bits whose lines the first started loading.

use crate::names::{Ids, Key, Table, ROW};
use std::arch::x86_64::*;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

/// Zero bytes after every chunk in its buffer (but see `Parser::chunk`): the
/// parser reads whole 64-byte blocks, and names' and numbers' words, up to
/// 64 bytes past the end.
pub const PAD: usize = 128;
/// Bytes scanned per window; a multiple of 64.
const WINDOW: usize = 4096;
/// Records parsed before they are joined: a window's worth, at about 61
/// bytes a record (a window with more takes several batches).
const BATCH: usize = 128;

/// Bits in a group's row of the matrix, so a pair's bit is `group * ROW_BITS
/// + id` (see `Table`).
const ROW_BITS: usize = ROW * 64;

/// A result, or the buffer offset near which the bytes break the grammar
/// (or a chunk did not start and end at record boundaries).
pub type Res<T> = Result<T, usize>;

/// Per group, what a worker adds: `[ids, cents, text, min, max, name_bytes,
/// recdist]`, the sums of ids, of amounts in cents and of text bytes, then
/// the smallest and largest amount, the sum of names' unescaped lengths, and
/// the records and distinct names together, `records << 32 | distinct`, so
/// that one add counts a record and one more a new pair. `text` sums both
/// text fields' unescaped lengths; note bytes are it minus name bytes.
///
/// Key: `[0]` id sum, `[1]` amount sum (cents), `[2]` text bytes, `[3]` min
/// amount, `[4]` max amount, `[5]` name bytes, `[6]` records << 32 | distinct.
pub type Agg = [i64; 7];
pub const EMPTY: Agg = [0, 0, 0, i64::MAX, i64::MIN, 0, 0];

/// Adds `b` into `a`.
pub fn merge(a: &mut Agg, b: &Agg) {
    (a[0], a[1], a[2]) = (a[0] + b[0], a[1] + b[1], a[2] + b[2]);
    (a[3], a[4]) = (a[3].min(b[3]), a[4].max(b[4]));
    (a[5], a[6]) = (a[5] + b[5], a[6] + b[6]);
}

/// A record parsed and waiting for its join: its name's key, its region id,
/// and its id, amount in cents, text bytes and name bytes (see `Agg`). The
/// join's first loop reuses `region` for the pair's bit in the matrix
/// (`group * ROW_BITS + id`), for the second. 48 bytes.
#[derive(Clone, Copy, Default)]
struct Record {
    key: Key,
    region: u32,
    nums: [i64; 4],
}

/// Where a chunk starts and ends in its buffer.
pub type Span = (usize, usize);

/// What the record loops read: the name table, the chunk's buffer, and per
/// 64-byte block of the buffer the quotes in the chunk before it and its
/// quote mask (see `quotes_before`).
///
/// Key: `.0` the name table, `.1` the chunk's buffer, `.2` per 64-byte block
/// `[quote bytes before the block, its quote mask]`.
type Ctx<'a> = (&'a Table, &'a [u8], &'a [[u64; 2]]);

/// Where a record starts, and the quotes in the chunk before it.
///
/// Key: `.0` the record's start offset in the buffer, `.1` the quote bytes
/// before it in the chunk.
type At = (usize, u64);

/// What a worker owns: the tables it joins against (the name table and the
/// region map), and what it accumulates: aggregates by group, and the dense
/// ids it has reserved for new names.
pub struct Local<'a> {
    pub tables: (&'a Table, &'a [AtomicU8]),
    pub aggs: [Agg; crate::GROUPS],
    pub ids: Ids,
}

/// A worker's parser state, reused for every chunk: per 64-byte block of the
/// chunk its quotes (see `Ctx`), the separators found in the current window,
/// and a batch of its records.
#[derive(Default)]
pub struct Parser {
    /// While scanning a chunk: the in-quotes state (all ones or 0) after the
    /// last block, and the quotes so far.
    state: [u64; 2],
    quotes: Vec<[u64; 2]>,
    /// Offsets in the buffer, those of a record not yet ended first.
    seps: Vec<u32>,
    batch: Vec<Record>,
}

impl Parser {
    /// Parses the records in `b[start..end]` into `out`: one chunk of the
    /// file (after the header, for the first), read into a buffer and
    /// followed there by `PAD` zero bytes, but for an LF just past a last
    /// record that has no terminator (`run::read` adds it, and this scans
    /// that one byte more). It must start at a record start, and the bytes
    /// before it in its first 64-byte block of the buffer must be zeros,
    /// which `scan` then takes along (`run::read` zeroes them). Returns the
    /// number of quote bytes in it, which `run` uses to check the chunk cuts.
    ///
    /// Every record must hold exactly four commas outside quotes, with valid
    /// numbers in their fields: the separators are taken five at a time, and
    /// as `parse` checks that the fifth is always an LF, and this that there
    /// are as many LFs, none of the first four is one. What is not diagnosed
    /// is a quote placed where 2.1 forbids it (inside an unquoted field, or
    /// not followed by a separator after closing).
    pub fn chunk(&mut self, b: &[u8], (start, end): Span, out: &mut Local) -> Res<u64> {
        // A CR before an added LF would end the record as if before a real
        // one: refused.
        if b[end] == b'\n' && b[end - 1] == b'\r' {
            return Err(end - 1);
        }
        self.quotes.resize(end / 64 + 2, [0; 2]);
        self.seps.resize(WINDOW + 128, 0);
        self.batch.resize(BATCH, Record::default());
        let (mut at, mut carry) = ((start, 0), 0);
        self.state = [0; 2];
        for window in (start & !63..end).step_by(WINDOW) {
            let last = (window + WINDOW).min(end);
            let span = [window, last + usize::from(last == end)];
            let (n, records) = self.scan(b, span, carry);
            // Five separators a record, and at most four commas of the next.
            carry = n.checked_sub(5 * records).filter(|&c| c < 5).ok_or(at.0)?;
            let (table, map) = out.tables;
            for seps in self.seps[..5 * records].chunks(5 * BATCH) {
                let c = (table, b, &self.quotes[..]);
                let batch = &mut self.batch[..seps.len() / 5];
                at = parse(c, map, seps, batch, at)?;
                join(c, map, batch, out);
            }
            self.seps.copy_within(5 * records..n, 0);
        }
        // `at.0` is one past `end` after a record ended by an added LF.
        let whole = carry == 0 && at.0.min(end) == end;
        whole.then_some(self.state[1]).ok_or(at.0)
    }

    /// Makes the masks of the blocks of `b[from..last]`, notes each
    /// block's quotes, and lists its separators in `self.seps` after the
    /// `n` there; returns how many there are then, and how many records
    /// they end.
    fn scan(&mut self, b: &[u8], [from, last]: [usize; 2], mut n: usize) -> (usize, usize) {
        let (first, mut records, state) = (from / 64, 0, &mut self.state);
        let blocks = (last - from).div_ceil(64);
        for (k, quotes) in self.quotes[first..first + blocks].iter_mut().enumerate() {
            let o = from + 64 * k;
            let [quote, comma, lf] = masks(b, o);
            // SAFETY: PCLMULQDQ is part of the build (see main.rs).
            let inside = unsafe { prefix_xor(quote) } ^ state[0];
            state[0] = ((inside as i64) >> 63) as u64; // bit 63, broadcast
            *quotes = [state[1], quote];
            state[1] += u64::from(quote.count_ones());
            n = flatten(&mut self.seps, n, (comma | lf) & !inside, o as u32);
            records += (lf & !inside).count_ones() as usize;
        }
        (n, records)
    }
}

/// The quote bytes in the chunk before offset `p` of the buffer, for a `p`
/// in a block `scan` has been through.
fn quotes_before(quotes: &[[u64; 2]], p: usize) -> u64 {
    debug_assert!(p / 64 < quotes.len());
    // SAFETY: `p / 64 < quotes.len()`. Every `p` here is the offset of a
    // separator that `scan` listed, so `scan` went through its block, and it
    // is at most the chunk's `end` (the added LF included), while
    // `Parser::chunk` sized `quotes` to `end / 64 + 2` blocks. BMI2 is part
    // of x86-64-v3, which the whole crate is compiled for (see `masks`).
    let [before, mask] = *unsafe { quotes.get_unchecked(p / 64) };
    before + u64::from(unsafe { _bzhi_u64(mask, (p % 64) as u32) }.count_ones())
}

/// Parses the records whose separators are `seps`, five each, into `out`,
/// the first starting at `at.0` with `at.1` quotes before it in the chunk,
/// and starts loading what their joins will need. Returns where the next
/// record starts, and the quotes before it.
#[inline(never)]
fn parse(c: Ctx, map: &[AtomicU8], seps: &[u32], out: &mut [Record], at: At) -> Res<At> {
    // `c` is the `Ctx`: `c.1` the buffer, `c.2` the per-block quotes.
    let (mut s, mut quotes_before_s) = at;
    for (seps, f) in seps.chunks_exact(5).zip(out) {
        let [c0, c1, c2, c3, e] = [0, 1, 2, 3, 4].map(|j| seps[j] as usize);
        // SAFETY: a separator is within the chunk, or the LF just past it.
        if unsafe { *c.1.get_unchecked(e) } != b'\n' {
            return Err(s);
        }
        let quotes = quotes_before(c.2, e);
        // The quotes in the record, and in its name's field.
        let q = [quotes, quotes_before(c.2, c1)].map(|q| (q - quotes_before_s) as u32);
        // A `Rec`: see its key.
        let rec = (s, [c0, c1, c2, c3, e], q);
        fields(f, c, rec).or_else(|| slow(f, c, rec)).ok_or(s)?;
        crate::prefetch(map, f.region as usize);
        c.0.prefetch(f.key);
        (s, quotes_before_s) = (e + 1, quotes);
    }
    Ok((s, quotes_before_s))
}

/// Joins the parsed records: their groups' aggregates, and their names'
/// (group, id) pairs, in two loops. The first finds each record's group and
/// name id, adds the record to its group, and starts loading, for writing,
/// the word of the matrix its pair's bit is in. By the second loop that line
/// has arrived, and the record sets the bit: an atomic OR (`lock bts`) that
/// says whether the bit was clear, which makes the pair a new distinct name
/// of its group. Every region id past the map's end shares its last entry, a
/// 0 (`_UNMATCHED`).
#[inline(never)]
fn join((table, _, _): Ctx, map: &[AtomicU8], records: &mut [Record], out: &mut Local) {
    for f in records.iter_mut() {
        // SAFETY: `entry` is at most `map.len() - 1`, and the map has at least
        // one entry (regions sized it).
        let entry = (f.region as usize).min(map.len() - 1);
        let group = unsafe { map.get_unchecked(entry) }.load(Relaxed);
        let [id, cents, text, name] = f.nums;
        let agg = &mut out.aggs[usize::from(group)];
        // One record added to its group's `Agg` (`1 << 32` is a record in the
        // high half of `[6]`). The extremes rarely change, so each is a
        // well-predicted branch, cheaper than a select and a store.
        (agg[0], agg[1], agg[2]) = (agg[0] + id, agg[1] + cents, agg[2] + text);
        (agg[5], agg[6]) = (agg[5] + name, agg[6] + (1 << 32));
        if cents < agg[3] {
            agg[3] = cents;
        }
        if cents > agg[4] {
            agg[4] = cents;
        }
        let id = table.find(f.key, &mut out.ids);
        let pair = usize::from(group) * ROW_BITS + id;
        crate::prefetch_write(table.bits, pair / 64);
        f.region = pair as u32;
    }
    for f in records.iter() {
        let pair = f.region as usize;
        let bit = 1u64 << (pair % 64);
        // SAFETY: `pair / 64` is below `GROUPS * ROW`, the length of `bits`:
        // the group is below `GROUPS` (a `u8`) and `id` below `ROW_BITS`.
        let word = unsafe { table.bits.get_unchecked(pair / 64) };
        // As a branch, not an add of the flag: only then does LLVM turn the
        // OR and the test into `lock bts`, not a compare-and-swap loop.
        if word.fetch_or(bit, Relaxed) & bit == 0 {
            // `[6]` is `records << 32 | distinct`: this adds to `distinct`.
            out.aggs[pair / ROW_BITS][6] += 1;
        }
    }
}

/// A record: where it starts, its four commas and terminator, and the quote
/// bytes in it and in its name's field.
///
/// Key: `.0` the start offset, `.1` the offsets of the commas after the id,
/// name, note and region id, then of the terminator (`[c0, c1, c2, c3, e]`),
/// `.2` `[quote bytes in the record, quote bytes in its id and name fields]`.
type Rec = (usize, [usize; 5], [u32; 2]);

/// Parses into `f` record `r`, if it takes the common form: an id and region
/// id of at most 8 digits, an amount of at most 5 before the dot. Anything
/// else is left to `slow`.
#[inline(always)]
fn fields(f: &mut Record, c: Ctx, (s, [c0, c1, c2, c3, e], q): Rec) -> Option<()> {
    let b = c.1;
    // SAFETY (all reads): the record's bytes, and these are within it.
    let byte = |p| unsafe { *b.get_unchecked(p) };
    let end = e - usize::from(byte(e - 1) == b'\r');
    let negative = byte(c3 + 1) == b'-';
    let a = c3 + 1 + usize::from(negative);
    // (`end - 3` is past `s`, as the four commas come before `e`.)
    (byte(end - 3) == b'.').then_some(())?;
    // Each number's field, the amount's with its dot, from its first byte
    // (the word's lowest): `digits` shifts it to the word's top.
    // SAFETY: each field ends before `end`, and the loads reach at most 8
    // bytes past a field's start (see `load`).
    let starts = [s, c2 + 1, a].map(|p| unsafe { load(b, p) });
    // A non-digit, a BOM before the id among them, takes the slow path.
    let [id, region, cents] = digits(starts, [c0 - s, c3 - c2 - 1, end - a])?;
    let cents = (1 - 2 * i64::from(negative)) * cents as i64;
    finish(f, c, [c0, c1, c2], [id as i64, region as i64, cents], q);
    Some(())
}

/// `fields` for any record that is not in its common form: its numbers one
/// byte at a time; `None` if it breaks the grammar. Out of line, so that the
/// hot loop keeps its registers.
#[cold]
fn slow(f: &mut Record, ctx: Ctx, (s, [c0, c1, c2, c3, e], q): Rec) -> Option<()> {
    let b = ctx.1;
    let crlf = e > c3 + 1 && b[e - 1] == b'\r';
    // A BOM directly after a terminator is not data (spec 2.1).
    let id = &b[s + 3 * usize::from(b[s..].starts_with(b"\xEF\xBB\xBF"))..c0];
    // The amount, `-?[0-9]+\.[0-9]{2}`, as cents.
    let amount = &b[c3 + 1..e - usize::from(crlf)];
    let (sign, digits) = amount.strip_prefix(b"-").map_or((1, amount), |d| (-1, d));
    let (whole, fraction) = digits.split_at(digits.len().checked_sub(3)?);
    let (whole, fraction) = (number(whole)?, number(fraction.strip_prefix(b".")?)?);
    let cents = i64::try_from(whole.checked_mul(100)?.checked_add(fraction)?).ok()?;
    // A region id too big for the map is simply unknown (see `join`).
    let region = number(&b[c2 + 1..c3])?.min(u32::MAX.into());
    let nums = [number(id)? as i64, region as i64, sign * cents];
    finish(f, ctx, [c0, c1, c2], nums, q);
    Some(())
}

/// The rest of `fields`, given a record's first three commas and its
/// numbers: its text bytes, its name's bytes and its name's key. The text
/// fields need no parsing: a field's raw bytes are between its separators
/// (and its quotes, if it is quoted), and its unescaped length is its width,
/// less its delimiters, less one per `""`.
#[inline(always)]
fn finish(f: &mut Record, (_, b, _): Ctx, [c0, c1, c2]: [usize; 3], nums: [i64; 3], q: [u32; 2]) {
    let [id, region, cents] = nums;
    // SAFETY: `c0 + 1` and `c1 + 1` are at most `c2`, within the record.
    let quoted = |c: usize| usize::from(unsafe { *b.get_unchecked(c + 1) } == b'"');
    let (name_quoted, note_quoted) = (quoted(c0), quoted(c1));
    // Name and note together; only the quotes, delimiting or escaping, are
    // not value bytes, and an escape is two of them for one value byte.
    let text = c2 as i64 - c0 as i64 - 2 - (name_quoted + note_quoted) as i64 - i64::from(q[0] / 2);
    let name = c0 + 1 + name_quoted;
    let len = (c1 - name).saturating_sub(name_quoted);
    (f.key, f.region) = (Table::key(b, name, len), region as u32);
    // The name's unescaped length: its raw length, less half its quotes
    // (every escape is two), but for its delimiting pair, which is not one.
    let name_bytes = len as i64 + name_quoted as i64 - i64::from(q[1] / 2);
    f.nums = [id, cents, text, name_bytes];
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

/// Appends the offsets of the set bits of `mask`, the separators of the
/// block at `o`, to `seps[n..]`, and returns the new count. Most blocks hold
/// at most seven (a record has five in about 61 bytes), so seven are written
/// unconditionally (junk past the count, which later appends overwrite) and
/// only more take the loop. `n` is at most four more than the bytes scanned
/// before this block in the window, so the writes stay below `WINDOW + 128`.
fn flatten(seps: &mut [u32], n: usize, mut mask: u64, o: u32) -> usize {
    debug_assert!(n + 64 <= seps.len());
    let count = mask.count_ones() as usize;
    let mut sep = |i: usize, mask: u64| {
        // SAFETY: `i < n + max(7, count) <= WINDOW + 128 = seps.len()`. A
        // window holds at most 65 blocks (the added LF can open one more), a
        // byte is at most one separator, and `Parser::chunk` carries at most
        // four over from the last window, so before block k of a window
        // `n <= 4 + 64 k <= 4100`; then `n + 7 <= 4107` and
        // `n + count <= 4 + 64 (k + 1) <= 4164`, both under 4224.
        *unsafe { seps.get_unchecked_mut(i) } = o + mask.trailing_zeros();
        mask & mask.wrapping_sub(1)
    };
    for i in n..n + 7 {
        mask = sep(i, mask);
    }
    for i in n + 7..n + count {
        mask = sep(i, mask);
    }
    n + count
}

/// Little-endian load of the 8 bytes at `p`, unchecked: about one in
/// twenty of the hot loop's instructions was this bounds check.
///
/// # Safety
/// `p + 8 <= b.len()`. Every caller loads within a chunk's buffer at most
/// 24 bytes past the end of a field, so at most 32 past the chunk's end, and
/// every buffer has `PAD` (128) bytes after it.
unsafe fn load(b: &[u8], p: usize) -> u64 {
    debug_assert!(p + 8 <= b.len());
    // SAFETY: the caller's promise.
    unsafe { b.as_ptr().add(p).cast::<u64>().read_unaligned() }
}

/// The values of an id, a region id and an amount (in cents), given the
/// words at their fields' starts and their lengths in bytes, or `None` if
/// any byte is not a digit or a length is out of range: 1 to 8, and the
/// amount's 4 to 8 (with its dot third from last). Each field is shifted to
/// the top of its word, the bytes after it falling off and zeros (as digits)
/// coming in below it; then adjacent digits combine into pairs, the pairs
/// into fours, the fours into eight. The amount's dot is masked out and
/// weighs nothing, and its last two digits are cents, so its top four digits
/// weigh 1,000 x.
fn digits(words: [u64; 3], lens: [usize; 3]) -> Option<[u64; 3]> {
    const DOT: i64 = !(0xFF << 40);
    // SAFETY: plain AVX2 arithmetic on locals; the whole crate is compiled
    // for AVX2 (see `masks`).
    unsafe {
        let [w0, w1, w2] = words.map(|w| w as i64);
        let [n0, n1, n2] = lens.map(|n| 64 - 8 * n as i64);
        let zero = _mm256_set1_epi8(b'0' as i8);
        let v = _mm256_sub_epi8(_mm256_set_epi64x(0, w2, w1, w0), zero);
        // The lengths in range: shifts from 0 to 56 bits, the amount's to 32.
        let shifts = _mm256_set_epi64x(0, n2, n1, n0);
        let long = _mm256_cmpgt_epi64(shifts, _mm256_set_epi64x(0, 32, 56, 56));
        let bad = _mm256_or_si256(long, _mm256_cmpgt_epi64(_mm256_setzero_si256(), shifts));
        // (`& 63` changes no count left, but spares the shift its case for
        // counts of 64 or more, which give 0.)
        let shifts = _mm256_and_si256(shifts, _mm256_set1_epi64x(63));
        let keep = _mm256_set_epi64x(0, DOT, -1, -1);
        let d = _mm256_and_si256(_mm256_sllv_epi64(v, shifts), keep);
        let digit = _mm256_cmpeq_epi8(_mm256_min_epu8(d, _mm256_set1_epi8(9)), d);
        let ok = _mm256_andnot_si256(bad, digit);
        let t = 10 | 1 << 8;
        let weights = _mm256_set_epi16(0, 0, 0, 0, t, 1, t, t, t, t, t, t, t, t, t, t);
        let pairs = _mm256_maddubs_epi16(d, weights);
        let fours = _mm256_madd_epi16(pairs, _mm256_set1_epi32(100 | 1 << 16));
        let high = _mm256_mul_epu32(fours, _mm256_set_epi64x(0, 1_000, 10_000, 10_000));
        let value = _mm256_add_epi64(high, _mm256_srli_epi64::<32>(fours));
        let mut out = [0u64; 4];
        _mm256_storeu_si256(out.as_mut_ptr().cast(), value);
        (_mm256_movemask_epi8(ok) == -1).then_some([out[0], out[1], out[2]])
    }
}

/// The decimal integer that is exactly `digits` (not empty). 2.1 sets no
/// limit on its length, so this saturates at `u64::MAX`: such a region id is
/// simply unknown, while an id or amount that large cannot be summed in 64
/// bits anyway (2.2 keeps ids below 4 x 10^7 and amounts below 10^5).
#[cold]
fn number(digits: &[u8]) -> Option<u64> {
    let digits = (!digits.is_empty() && digits.iter().all(u8::is_ascii_digit)).then_some(digits)?;
    let step = |v: u64, &d: &u8| v.saturating_mul(10).saturating_add(u64::from(d - b'0'));
    Some(digits.iter().fold(0, step))
}
