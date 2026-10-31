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

use crate::names::{Full, Names};
use crate::regions::Regions;
use crate::Agg;
use std::arch::x86_64 as simd;

const HEADER: &[u8] = b"id,name,note,region_id,amount";
const BOM: u64 = 0xBF_BB_EF; // EF BB BF, read little-endian
/// Records are parsed once `FLUSH` separators have piled up. `ROOM` adds a
/// block's worth, flattened eight at a time without bounds checks.
const FLUSH: usize = 160;
const ROOM: usize = FLUSH + 72;
/// The most records one flush completes.
pub const BATCH: usize = ROOM / 5;

/// Why a chunk could not be counted.
pub enum Fail {
    /// The bytes do not follow the grammar, or the chunk did not start and
    /// end at record boundaries.
    Malformed,
    /// More distinct names than the name table was sized for.
    Full,
}

impl From<Full> for Fail {
    fn from(_: Full) -> Self {
        Fail::Full
    }
}

/// A name for the distinct-name counter: its group, and its raw bytes
/// `b[start..start + len]` (see `text`).
#[derive(Clone, Copy, Default)]
pub struct Name {
    pub group: u8,
    pub start: usize,
    pub len: usize,
}

/// One record, parsed but not yet joined.
#[derive(Clone, Copy, Default)]
struct Fields {
    id: u64,
    region: u64,
    cents: i64,
    name: Name,
    name_len: usize,
    note_len: usize,
}

/// The mapped records.csv (file plus zero padding, see map.rs), and the
/// tables records are joined and counted into.
pub struct Input<'a> {
    pub b: &'a [u8],
    pub len: usize,
    pub regions: &'a Regions,
    pub names: &'a Names,
}

/// A chunk's separators not yet consumed: offsets from `base`, the quote
/// bytes before each, how many there are, and where their record starts.
struct Separators {
    at: [u32; ROOM],
    quotes: [u32; ROOM],
    n: usize,
    base: usize,
    record: usize,
}

impl Input<'_> {
    /// Parses the records in `b[start..end]`, which must start at a record
    /// start, or at the header when `header` is set, into `aggs`. Returns
    /// the number of quote bytes in the range.
    pub fn chunk(&self, start: usize, end: usize, header: bool, aggs: &mut [Agg]) -> Result<u64, Fail> {
        let b = self.b;
        let base = if header { self.header(start).ok_or(Fail::Malformed)? } else { start };
        let mut s = Separators { at: [0; ROOM], quotes: [0; ROOM], n: 0, base, record: base };
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
            // `fields`, this rejects every byte sequence the grammar does not
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
            let fields = self.fields(&s, 0, end).ok_or(Fail::Malformed)?;
            self.join(&[fields], aggs)?;
            (s.n, s.record) = (0, end);
        }
        if s.n == 0 && s.record == end {
            Ok(u64::from(quotes))
        } else {
            Err(Fail::Malformed)
        }
    }

    /// The header line; returns where the first record starts.
    fn header(&self, p: usize) -> Option<usize> {
        let p = p + 3 * usize::from(load(self.b, p) & 0xFF_FFFF == BOM);
        let q = p + HEADER.len();
        (self.b[p..q] == *HEADER).then_some(())?;
        match (self.b[q], self.b[q + 1]) {
            (b'\n', _) => Some(q + 1),
            (b'\r', b'\n') => Some(q + 2),
            _ => None,
        }
    }

    /// Parses and joins the records whose five separators (four commas,
    /// then an LF) have all been seen. Parsing the whole batch before
    /// joining any of it lets the region-map loads, which mostly miss the
    /// cache, overlap.
    fn records(&self, s: &mut Separators, aggs: &mut [Agg]) -> Result<(), Fail> {
        let mut batch = [Fields::default(); BATCH];
        let done = s.n / 5;
        for (r, fields) in batch[..done].iter_mut().enumerate() {
            let lf = s.base + s.at[5 * r + 4] as usize;
            let parsed = self.fields(s, 5 * r, lf).filter(|_| self.b[lf] == b'\n');
            *fields = parsed.ok_or(Fail::Malformed)?;
            self.regions.prefetch(fields.region);
            s.record = lf + 1;
        }
        s.at.copy_within(5 * done..s.n, 0);
        s.quotes.copy_within(5 * done..s.n, 0);
        s.n -= 5 * done;
        self.join(&batch[..done], aggs)
    }

    /// The record starting at `s.record`, whose commas are separators
    /// `k..k + 4` and whose terminator (or the end of file) is at `end`.
    fn fields(&self, s: &Separators, k: usize, end: usize) -> Option<Fields> {
        let b = self.b;
        let [c0, c1, c2, c3] = [0, 1, 2, 3].map(|j| s.base + s.at[k + j] as usize);
        if [c0, c1, c2, c3].iter().any(|&c| b[c] != b',') {
            return None;
        }
        // A BOM directly after a terminator is not data (spec 2.1).
        let start = s.record + 3 * usize::from(load(b, s.record) & 0xFF_FFFF == BOM);
        let (name, name_len) = text(c0, c1, s.quotes[k + 1] - s.quotes[k]);
        let (_, note_len) = text(c1, c2, s.quotes[k + 2] - s.quotes[k + 1]);
        let (id, region) = (number(b, start, c0)?, number(b, c2 + 1, c3)?);
        let cents = cents(b, c3 + 1, end - usize::from(b[end - 1] == b'\r'))?;
        Some(Fields { id, region, cents, name, name_len, note_len })
    }

    /// Joins parsed records to their groups, adds them into `aggs`, and
    /// counts their names.
    fn join(&self, batch: &[Fields], aggs: &mut [Agg]) -> Result<(), Fail> {
        let mut names = [Name::default(); BATCH];
        for (f, name) in batch.iter().zip(&mut names) {
            let group = self.regions.group(f.region);
            let a = &mut aggs[usize::from(group)];
            a.count += 1;
            a.ids += f.id;
            a.sum += f.cents;
            a.min = a.min.min(f.cents);
            a.max = a.max.max(f.cents);
            a.name_bytes += f.name_len as u64;
            a.note_bytes += f.note_len as u64;
            *name = Name { group, ..f.name };
        }
        Ok(self.names.count(self.b, &names[..batch.len()], aggs)?)
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
fn text(before: usize, after: usize, quotes: u32) -> (Name, usize) {
    let (width, q) = (after - before - 1, usize::from(quotes > 0));
    let (start, len) = (before + 1 + q, width - 2 * q);
    (Name { group: 0, start, len }, width - q - quotes as usize / 2)
}

/// Masks of the bytes equal to `"`, `,`, LF and CR among the 64 at `p`;
/// bit i stands for byte `p + i`.
fn masks(b: &[u8], p: usize) -> [u64; 4] {
    let block = b[p..p + 64].as_ptr().cast::<simd::__m256i>();
    // SAFETY: `block` points to 64 readable bytes, and the build enables
    // AVX2 (target-cpu=x86-64-v3, see the Dockerfile).
    unsafe {
        let halves = [simd::_mm256_loadu_si256(block), simd::_mm256_loadu_si256(block.add(1))];
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

/// The decimal integer that is exactly `b[start..end]`.
fn number(b: &[u8], start: usize, end: usize) -> Option<u64> {
    uint(b, start).and_then(|(value, stop)| (stop == end).then_some(value))
}

/// The amount `-?[0-9]+\.[0-9]{2}` that is exactly `b[start..end]`, in cents.
fn cents(b: &[u8], start: usize, end: usize) -> Option<i64> {
    let negative = b[start] == b'-';
    let (whole, dot) = uint(b, start + usize::from(negative))?;
    let (d1, d2) = (b[dot + 1].wrapping_sub(b'0'), b[dot + 2].wrapping_sub(b'0'));
    if b[dot] != b'.' || d1 > 9 || d2 > 9 || dot + 3 != end {
        return None;
    }
    let cents = i64::try_from(whole).ok()?.checked_mul(100)? + i64::from(d1 * 10 + d2);
    Some(if negative { -cents } else { cents })
}

/// Little-endian load of the 8 bytes at `p`.
pub fn load(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}

const POW10: [u64; 9] = [1, 10, 100, 1_000, 10_000, 100_000, 1_000_000, 10_000_000, 100_000_000];

/// A run of one or more ASCII digits at `start` as `(value, end)`, eight
/// digits per step with no per-digit branch (SWAR).
pub fn uint(b: &[u8], start: usize) -> Option<(u64, usize)> {
    let (mut value, mut p) = (0u64, start);
    loop {
        // Bytes minus '0': digits become 0..=9. Any other byte becomes >= 10
        // (or wraps to >= 0xD0), which the +0x76 sets bit 7 for. Borrows and
        // carries only move towards later bytes, so bytes up to and including
        // the first non-digit are classified correctly.
        let w = load(b, p).wrapping_sub(0x3030_3030_3030_3030);
        let other = (w.wrapping_add(0x7676_7676_7676_7676) | w) & 0x8080_8080_8080_8080;
        let n = other.trailing_zeros() as usize / 8;
        if n > 0 {
            // Shift the n digits to the top, so the vacated low bytes act as
            // leading zeros, and convert all eight lanes at once.
            let digits = eight_digits(w << (64 - 8 * n));
            value = value.checked_mul(POW10[n])?.checked_add(digits)?;
        }
        p += n;
        if n < 8 {
            return (p > start).then_some((value, p));
        }
    }
}

/// Eight digit values (0..=9, first digit in the lowest byte) to an integer.
fn eight_digits(v: u64) -> u64 {
    let v = v.wrapping_mul(10).wrapping_add(v >> 8); // pairs of digits
    let lo = (v & 0x0000_00FF_0000_00FF).wrapping_mul(100 + (1_000_000 << 32));
    let hi = ((v >> 16) & 0x0000_00FF_0000_00FF).wrapping_mul(1 + (10_000 << 32));
    (lo.wrapping_add(hi) >> 32) & 0xFFFF_FFFF
}
