//! records.csv: a parser for the grammar in spec section 2.1, built on
//! 64-byte bitmasks rather than a byte-at-a-time state machine.
//!
//! For each 64-byte block, AVX2 compares give one bit per byte for `"`, `,`,
//! LF and CR. In this grammar a byte is inside a quoted field exactly when an
//! odd number of quotes precede it (a `""` escape closes and reopens), so the
//! prefix XOR of the quote mask is the in-quotes mask, and the commas and LFs
//! outside it are the separators. Their positions are flattened into a
//! buffer, then each record's fields are cut at its five separators and
//! parsed independently of each other. Nothing chases one position from
//! field to field, so the CPU can work on many records at once.

use crate::pipeline::Shared;
use crate::Agg;
use std::arch::x86_64 as simd;

const HEADER: &[u8] = b"id,name,note,region_id,amount";
/// The BOM's bytes EF BB BF as the low three bytes of a little-endian load.
const BOM: u64 = 0xBF_BB_EF;
pub const BOM_FIRST: u8 = 0xEF;
/// SWAR constants: '0' in every byte, 0x80 - 10 in every byte, and bit 7 of
/// every byte (see `classify`).
const ZEROS: u64 = 0x3030_3030_3030_3030;
const TENS: u64 = 0x7676_7676_7676_7676;
const HIGH: u64 = 0x8080_8080_8080_8080;
/// Records are parsed once `FLUSH` separators have piled up. `ROOM` adds a
/// block's worth, flattened eight at a time without bounds checks.
const FLUSH: usize = 160;
const ROOM: usize = FLUSH + 72;
/// The most records one flush completes.
pub const BATCH: usize = ROOM / 5;

/// Why a chunk could not be counted, worst last.
#[derive(Clone, Copy)]
pub enum Fail {
    /// More distinct names than the name table was sized for.
    Full = 1,
    /// The bytes do not follow the grammar, or the chunk did not start and
    /// end at record boundaries.
    Malformed = 2,
}

/// One parsed record: what the aggregates need, its group once joined, and
/// its name's raw bytes `b[start..start + len]` (see `text`).
#[derive(Clone, Copy, Default)]
pub struct Record {
    pub group: u8,
    pub start: usize,
    pub len: usize,
    region: u64,
    id: i64,
    cents: i64,
    name_len: i64,
    note_len: i64,
}

/// A chunk's separators not yet consumed: offsets from `base`, the quote
/// bytes before each, how many there are, and where their record starts;
/// and the batch the records they complete are parsed into.
struct Separators {
    at: [u32; ROOM],
    quotes: [u32; ROOM],
    n: usize,
    base: usize,
    record: usize,
    batch: [Record; BATCH],
}

impl Shared<'_> {
    /// Parses the records in `b[start..end]` into `aggs`; `start` must be a
    /// record start, or 0 for the chunk holding the header. Returns the
    /// number of quote bytes in the range.
    pub fn chunk(&self, start: usize, end: usize, aggs: &mut [Agg]) -> Result<u64, Fail> {
        let b = self.b;
        let base = if start == 0 { self.header()? } else { start };
        let (at, quotes, batch) = ([0; ROOM], [0; ROOM], [Record::default(); BATCH]);
        let mut s = Separators {
            at,
            quotes,
            n: 0,
            base,
            record: base,
            batch,
        };
        // Carried between blocks: in-quotes state (all ones or zero), quotes
        // so far, and whether the previous block's last byte was `,` or `"`.
        let (mut inside, mut quotes, mut sep_before) = (0u64, 0u32, 0u64);
        for o in (base..end).step_by(64) {
            let live = u64::MAX >> (64 - (end - o).min(64)); // bytes before `end`
            let [quote, comma, lf, cr] = masks(b, o).map(|m| m & live);
            let ins = prefix_xor(quote) ^ inside;
            // Grammar checks, as masks. A quote that opens a field (or
            // reopens it after the first half of a `""`) must follow `,` or
            // `"`; one that closes a field must be followed by `,` or `"`; a
            // CR outside quotes must start a CRLF. With the checks in
            // `parse`, this rejects every byte sequence the grammar does not
            // produce.
            let sep = comma | quote;
            let next = b[o + 64];
            let sep_after = sep >> 1 | u64::from(next == b',' || next == b'"') << 63;
            let lf_after = lf >> 1 | u64::from(next == b'\n') << 63;
            let opens = quote & ins & !(sep << 1 | sep_before);
            let closes = quote & !ins & !sep_after;
            if opens | closes | cr & !ins & !lf_after != 0 {
                return Err(Fail::Malformed);
            }
            s.push((comma | lf) & !ins, (o - base) as u32, quote, quotes);
            quotes += quote.count_ones();
            inside = ((ins as i64) >> 63) as u64; // bit 63, broadcast
            sep_before = sep >> 63;
            if s.n >= FLUSH {
                self.records(&mut s, aggs)?;
            }
        }
        self.records(&mut s, aggs)?;
        // The final record may end at end of file without a terminator.
        if s.n == 4 && end == self.len && inside == 0 {
            self.parse(&mut s, 0, end, 0).ok_or(Fail::Malformed)?;
            self.join(&mut s.batch[..1], aggs)?;
            (s.n, s.record) = (0, end);
        }
        let whole = s.n == 0 && s.record == end;
        whole.then_some(u64::from(quotes)).ok_or(Fail::Malformed)
    }

    /// The header line; returns where the first record starts.
    fn header(&self) -> Result<usize, Fail> {
        let p = 3 * usize::from(bom_at(self.b, 0));
        let q = p + HEADER.len();
        let crlf = usize::from(self.b[q] == b'\r');
        let ok = self.b[p..q] == *HEADER && self.b[q + crlf] == b'\n';
        ok.then_some(q + crlf + 1).ok_or(Fail::Malformed)
    }

    /// Parses and joins the records whose five separators (four commas,
    /// then an LF) have all been seen. Parsing the whole batch before
    /// joining any of it lets the region-map loads, which mostly miss the
    /// cache, overlap.
    fn records(&self, s: &mut Separators, aggs: &mut [Agg]) -> Result<(), Fail> {
        let done = s.n / 5;
        for r in 0..done {
            let lf = s.base + s.at[5 * r + 4] as usize;
            let parsed = self.b[lf] == b'\n' && self.parse(s, 5 * r, lf, r).is_some();
            if !parsed {
                return Err(Fail::Malformed);
            }
            self.regions.prefetch(s.batch[r].region);
            s.record = lf + 1;
        }
        s.at.copy_within(5 * done..s.n, 0);
        s.quotes.copy_within(5 * done..s.n, 0);
        s.n -= 5 * done;
        self.join(&mut s.batch[..done], aggs)
    }

    /// Parses into `s.batch[r]` the record starting at `s.record`, whose
    /// commas are separators `k..k + 4` and whose terminator (or the end of
    /// file) is at `end`.
    fn parse(&self, s: &mut Separators, k: usize, end: usize, r: usize) -> Option<()> {
        let b = self.b;
        let [c0, c1, c2, c3] = [0, 1, 2, 3].map(|j| s.base + s.at[k + j] as usize);
        if [c0, c1, c2, c3].iter().any(|&c| b[c] != b',') {
            return None;
        }
        // A BOM directly after a terminator is not data (spec 2.1).
        let id_at = s.record + 3 * usize::from(bom_at(b, s.record));
        let (start, len, name_len) = text(c0, c1, s.quotes[k + 1] - s.quotes[k]);
        let (_, _, note_len) = text(c1, c2, s.quotes[k + 2] - s.quotes[k + 1]);
        let (id, region) = (number(b, id_at, c0)? as i64, number(b, c2 + 1, c3)?);
        let cents = cents(b, c3 + 1, end - usize::from(b[end - 1] == b'\r'))?;
        let out = &mut s.batch[r]; // its group is set by `join`
        (out.start, out.len, out.region) = (start, len, region);
        (out.id, out.cents, out.name_len, out.note_len) = (id, cents, name_len, note_len);
        Some(())
    }

    /// Joins parsed records to their groups, adds them into `aggs`, and
    /// counts their names.
    fn join(&self, batch: &mut [Record], aggs: &mut [Agg]) -> Result<(), Fail> {
        for r in batch.iter_mut() {
            r.group = self.regions.group(r.region);
            let sums = [1, r.id, r.cents, 0, r.name_len, r.note_len];
            aggs[usize::from(r.group)].add(sums, r.cents, r.cents);
        }
        self.names.count(self.b, batch, aggs)
    }
}

impl Separators {
    /// Appends the separators in `mask` (bit i is byte `offset + i`), whose
    /// block has quote mask `quote` and `quotes` quote bytes before it.
    /// Eight per round, with no branch per separator; rounds past the last
    /// one write junk past `n`, which later pushes overwrite.
    fn push(&mut self, mut mask: u64, offset: u32, quote: u64, quotes: u32) {
        let (count, mut i) = (mask.count_ones() as usize, self.n);
        loop {
            for _ in 0..8 {
                let bit = mask.trailing_zeros();
                let before = quote & 1u64.wrapping_shl(bit).wrapping_sub(1);
                (self.at[i], self.quotes[i]) = (offset + bit, quotes + before.count_ones());
                mask &= mask.wrapping_sub(1);
                i += 1;
            }
            if mask == 0 {
                break;
            }
        }
        self.n += count;
    }
}

/// The text field between separators `before` and `after` holding `quotes`
/// quote bytes, as its raw bytes and its unescaped length. The raw bytes
/// exclude the delimiting quotes but keep `""` escapes doubled: escaping is
/// one-to-one, so equal raw bytes mean equal values. A quoted field (`q`
/// is 1) loses its two delimiters, and each `""` pair counts once.
fn text(before: usize, after: usize, quotes: u32) -> (usize, usize, i64) {
    let (width, q) = (after - before - 1, usize::from(quotes > 0));
    let unescaped = width - q - quotes as usize / 2;
    (before + 1 + q, width - 2 * q, unescaped as i64)
}

/// Masks of the bytes equal to `"`, `,`, LF and CR among the 64 at `p`;
/// bit i stands for byte `p + i`.
fn masks(b: &[u8], p: usize) -> [u64; 4] {
    let block = b[p..p + 64].as_ptr().cast::<simd::__m256i>();
    // SAFETY: `block` points to 64 readable bytes. The intrinsics need AVX2,
    // which the whole crate is compiled for (`-C target-cpu=x86-64-v3` in
    // the Dockerfile), so every function here may use it.
    unsafe {
        let halves = [block, block.add(1)].map(|h| simd::_mm256_loadu_si256(h));
        [b'"', b',', b'\n', b'\r'].map(|c| {
            let c = simd::_mm256_set1_epi8(c as i8);
            let [lo, hi] = halves.map(|h| simd::_mm256_cmpeq_epi8(h, c));
            let [lo, hi] = [lo, hi].map(|m| simd::_mm256_movemask_epi8(m) as u32);
            u64::from(lo) | u64::from(hi) << 32
        })
    }
}

/// Bit i of the result is the XOR of bits 0..=i: the in-quotes mask.
fn prefix_xor(mut x: u64) -> u64 {
    for shift in [1, 2, 4, 8, 16, 32] {
        x ^= x << shift;
    }
    x
}

/// The decimal integer that is exactly `b[start..end]`. Its length is
/// known, so up to eight digits are checked and converted in one step.
#[inline(always)]
fn number(b: &[u8], start: usize, end: usize) -> Option<u64> {
    let n = end.checked_sub(start).filter(|&n| n > 0)?;
    if n > 8 {
        return uint(b, start).and_then(|(value, stop)| (stop == end).then_some(value));
    }
    // Shift the n digits to the top: the bytes after the field fall off,
    // and the vacated low bytes read as leading zeros.
    let (values, other) = classify(load(b, start));
    let shift = 64 - 8 * n;
    (other << shift == 0).then(|| eight_digits(values << shift))
}

/// The amount `-?[0-9]+\.[0-9]{2}` that is exactly `b[start..end]`, in cents.
#[inline(always)]
fn cents(b: &[u8], start: usize, end: usize) -> Option<i64> {
    let negative = b[start] == b'-';
    let dot = end.checked_sub(3)?;
    let whole = number(b, start + usize::from(negative), dot)?;
    let fraction = number(b, dot + 1, end)? as i64;
    let cents = i64::try_from(whole).ok()?.checked_mul(100)? + fraction;
    (b[dot] == b'.').then_some(if negative { -cents } else { cents })
}

/// Little-endian load of the 8 bytes at `p`.
pub fn load(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}

/// A run of one or more ASCII digits at `start` as `(value, end)`, eight
/// digits per step with no per-digit branch (SWAR).
pub fn uint(b: &[u8], start: usize) -> Option<(u64, usize)> {
    let (mut value, mut p) = (0u64, start);
    loop {
        let (w, other) = classify(load(b, p));
        let n = other.trailing_zeros() as usize / 8;
        if n > 0 {
            // Shift the n digits to the top, so the vacated low bytes act as
            // leading zeros, and convert all eight lanes at once.
            let digits = eight_digits(w << (64 - 8 * n));
            value = value
                .checked_mul(10u64.pow(n as u32))?
                .checked_add(digits)?;
        }
        p += n;
        if n < 8 {
            return (p > start).then_some((value, p));
        }
    }
}

/// The bytes of `w` minus '0', and bit 7 set in every byte that is not a
/// digit. Digits become 0..=9; any other byte becomes >= 10 (or wraps to
/// >= 0xD0), which adding 0x76 lifts to bit 7. Borrows and carries only move
/// towards later bytes, so bytes up to and including the first non-digit are
/// classified correctly, which is all the callers rely on.
fn classify(w: u64) -> (u64, u64) {
    let values = w.wrapping_sub(ZEROS);
    (values, (values.wrapping_add(TENS) | values) & HIGH)
}

/// Whether a BOM starts at `p`.
fn bom_at(b: &[u8], p: usize) -> bool {
    load(b, p) & 0xFF_FFFF == BOM
}

/// Eight digit values (0..=9, first digit in the lowest byte) to an integer:
/// adjacent digits combine into pairs, the pairs into fours (times 100),
/// and the fours into eight (times 10,000), two lanes per multiply.
fn eight_digits(v: u64) -> u64 {
    let v = v.wrapping_mul(10).wrapping_add(v >> 8); // pairs of digits
    let lo = (v & 0x0000_00FF_0000_00FF).wrapping_mul(100 + (1_000_000 << 32));
    let hi = ((v >> 16) & 0x0000_00FF_0000_00FF).wrapping_mul(1 + (10_000 << 32));
    (lo.wrapping_add(hi) >> 32) & 0xFFFF_FFFF
}
