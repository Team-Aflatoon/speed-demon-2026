//! The records.csv parser, one slab (a byte range) at a time, with AVX2
//! bitmasks. Every number, length and check follows spec section 2.1.

use std::arch::x86_64::*;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};

/// Blocks of 64 bytes scanned before their records are parsed: enough
/// records per batch that the loop over them rarely mispredicts its end.
const BATCH: usize = 8;
/// Separator positions held at once: a batch's worth plus a leftover
/// partial record (under 5) plus the 8 that `push` may write past the end
/// is under 530.
/// (A power of two, so that masking the index proves it in bounds.)
const SEPS: usize = 1024;
const ZEROS: u64 = 0x3030_3030_3030_3030; // eight ASCII '0'
const BOM: u32 = 0xBF_BBEF; // EF BB BF, little-endian
/// Ids and whole amounts must be below this, so that no sum can overflow.
const LIMIT: u64 = 1 << 40;

/// Running totals for one group (one output line).
#[derive(Clone, Copy)]
pub struct Agg {
    pub count: i64,
    pub ids: i64,
    pub sum: i64,
    pub min: i64,
    pub max: i64,
    pub name_bytes: i64,
    pub note_bytes: i64,
}

impl Agg {
    pub const EMPTY: Agg = Agg {
        count: 0,
        ids: 0,
        sum: 0,
        min: i64::MAX,
        max: i64::MIN,
        name_bytes: 0,
        note_bytes: 0,
    };

    pub fn merge(&mut self, o: &Agg) {
        (self.count, self.ids, self.sum) =
            (self.count + o.count, self.ids + o.ids, self.sum + o.sum);
        (self.min, self.max) = (self.min.min(o.min), self.max.max(o.max));
        (self.name_bytes, self.note_bytes) = (
            self.name_bytes + o.name_bytes,
            self.note_bytes + o.note_bytes,
        );
    }
}

/// Where a slab's results go: the worker's totals, and one message per
/// record for the worker that owns the record's name (see `names.rs`).
pub struct Sink<'a> {
    pub aggs: &'a mut [Agg; 256],
    /// `owners` runs of `cap` messages, one run per owner.
    pub msgs: &'a [AtomicU64],
    pub cap: usize,
    pub counts: [usize; crate::MAX_THREADS],
    pub owners: usize,
}

/// Parses the records that start in `start..end`, taking `start` to be a
/// record start, and returns where the first record at or after `end`
/// starts. `d[..len]` is the file, which ends with LF. Returns `None` if
/// the bytes break the grammar (or a name is over 64 KiB).
pub fn parse(
    d: &[u8],
    len: usize,
    start: usize,
    end: usize,
    map: &[AtomicU8],
    sink: &mut Sink,
) -> Option<usize> {
    // Every separator is below `len` (the rest of `d` is zeros), every
    // block starts below `len + 64 * BATCH`, and no load reaches more than
    // 64 bytes past either: this makes all of `load`'s loads safe.
    assert!(d.len() >= len + 64 * BATCH + 128);
    let mut seps = [0u32; SEPS];
    let (mut n, mut rec, mut records, mut bad) = (0, start, 0u64, 0u64);
    let (mut at, mut skip) = (start & !63, !0u64 << (start & 63));
    // State carried from one block to the next, as bit 63 of the previous
    // block moved to bit 0: inside quotes (all ones), and whether the last
    // byte was a comma or quote, a closing quote, or a CR outside quotes.
    let (mut inside, mut was_cq, mut was_close, mut was_cr) = (0u64, 0, 0, 0);
    let mut lfs = 0u64;
    while rec < end {
        if at >= len {
            return None; // the last record before `end` never ends
        }
        for _ in 0..BATCH {
            let [q, c, l, r] = masks(d, at).map(|m| m & skip);
            skip = !0;
            // A byte is inside quotes iff an odd number of quotes precede
            // it: a "" escape closes and at once reopens. So the prefix XOR
            // of the quote mask is the inside mask (quotes themselves
            // count as inside when they open, outside when they close).
            let inq = prefix_xor(q) ^ inside;
            inside = ((inq as i64) >> 63) as u64;
            let (open, close, cr) = (q & inq, q & !inq, r & !inq);
            // The grammar, checked on whole blocks: a quote opens only
            // after a comma (a field start) or a closing quote (an escape);
            // a closing quote is followed by a quote, comma or terminator;
            // a CR outside quotes starts a CRLF.
            bad |= open & !((c | q) << 1 | was_cq);
            bad |= (close << 1 | was_close) & !(q | c | l | r);
            bad |= (cr << 1 | was_cr) & !l;
            (was_cq, was_close, was_cr) = ((c | q) >> 63, close >> 63, cr >> 63);
            let lf = l & !inq;
            lfs += u64::from(lf.count_ones());
            n = push(&mut seps, n, (c & !inq) | lf, at as u32);
            at += 64;
        }
        let mut k = 0;
        while k + 5 <= n && rec < end {
            let s = [0, 1, 2, 3, 4].map(|i| seps[k + i] as usize);
            bad |= record(d, rec, s, map, sink);
            (rec, records, k) = (s[4] + 1, records + 1, k + 5);
        }
        seps.copy_within(k..n, 0);
        n -= k;
    }
    // Each record's fifth separator is an LF (checked in `record`). If the
    // records hold no other LF, all their other separators are commas: every
    // record has exactly five fields.
    let left = seps[..n]
        .iter()
        .filter(|&&p| d[p as usize] == b'\n')
        .count();
    (bad == 0 && lfs - left as u64 == records).then_some(rec)
}

/// Parses the record at `rec` whose separators are `s`; returns nonzero if
/// it breaks the grammar.
#[inline(always)]
fn record(d: &[u8], rec: usize, s: [usize; 5], map: &[AtomicU8], sink: &mut Sink) -> u64 {
    // A BOM at the start of a record is skipped (2.1).
    let bom = usize::from(load::<u32>(d, rec) & 0xFF_FFFF == BOM);
    let (name_bytes, key, key_len) = text::<1>(d, s[0] + 1, s[1]);
    let (note_bytes, _, _) = text::<2>(d, s[1] + 1, s[2]);
    let tail = load::<u16>(d, s[4] - 1); // the terminator, CRLF or LF
    let cr = usize::from(tail as u8 == b'\r');
    let (id, region, cents, mut bad) = numbers(d, rec + 3 * bom, s, s[4] - cr);
    bad |= u64::from(tail >> 8 != u16::from(b'\n')) | (key_len >> 16) as u64;
    let group = map.get(region as usize).map_or(0, |g| g.load(Relaxed));
    let a = &mut sink.aggs[usize::from(group)];
    (a.count, a.ids, a.sum) = (a.count + 1, a.ids + id as i64, a.sum + cents);
    (a.min, a.max) = (a.min.min(cents), a.max.max(cents));
    (a.name_bytes, a.note_bytes) = (
        a.name_bytes + name_bytes as i64,
        a.note_bytes + note_bytes as i64,
    );
    // The owner of a name is picked by a hash of its first 8 bytes and its
    // length, a function of the value alone, so equal names meet there.
    // SAFETY: BMI2 (x86-64-v3, as the whole crate is compiled).
    let first = unsafe { _bzhi_u64(load(d, key), 8 * key_len.min(8) as u32) };
    let h = (first ^ key_len as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let owner = ((u128::from(h) * sink.owners as u128) >> 64) as usize;
    let i = sink.counts[owner];
    sink.counts[owner] = i + 1;
    let msg = key as u64 | (key_len as u64) << 32 | u64::from(group) << 48;
    sink.msgs[owner * sink.cap + i].store(msg, Relaxed);
    bad
}

/// The text field `d[a..e]`: its value's length, and where its raw bytes
/// (still escaped, without the delimiting quotes) start and how many there
/// are. Escaping is one-to-one, so raw bytes identify a value; the empty
/// unquoted field and `""` both have none. A quoted field's value is its
/// width minus its two delimiters minus one per `""`, i.e. half its other
/// quotes. Counts quotes `V` vectors at a time.
#[inline(always)]
fn text<const V: usize>(d: &[u8], a: usize, e: usize) -> (usize, usize, usize) {
    let w = e - a;
    let quoted = usize::from(load::<u8>(d, a) == b'"'); // if empty, the separator
    let mut quotes = 0;
    let mut i = 0;
    loop {
        for _ in 0..V {
            let m = quote_mask(d, a + i);
            // SAFETY: BMI2 (x86-64-v3, as the whole crate is compiled).
            quotes +=
                unsafe { _bzhi_u32(m, w.saturating_sub(i).min(32) as u32) }.count_ones() as usize;
            i += 32;
        }
        if i >= w {
            break;
        }
    }
    (w - quotes / 2 - quoted, a + quoted, w - 2 * quoted)
}

/// The id `d[at..s[0]]`, the region id between `s[2]` and `s[3]` and the
/// amount `-?D+.DD` between `s[3]` and `end` (as cents), and nonzero if
/// any is malformed. Ids and region ids of up to 8 digits and amounts of
/// up to 5 integer digits (all of spec 2.2) take one SWAR step each, and
/// are checked together without a branch. Dropping the amount's dot leaves
/// its cents as a string of digits.
#[inline(always)]
fn numbers(d: &[u8], at: usize, s: [usize; 5], end: usize) -> (u64, u64, i64, u64) {
    let neg = load::<u8>(d, s[3] + 1) == b'-';
    let (id_len, region_len) = (s[0].wrapping_sub(at), s[3] - s[2] - 1);
    let whole = end.wrapping_sub(s[3] + 4 + usize::from(neg));
    if (id_len.wrapping_sub(1) | region_len.wrapping_sub(1)) >= 8 || whole.wrapping_sub(1) >= 5 {
        return long_numbers(d, at, s, end);
    }
    let v: u64 = load(d, end - 8);
    let id = pad_digits(load(d, s[0] - 8), id_len);
    let region = pad_digits(load(d, s[3] - 8), region_len);
    let amount = pad_digits((v & 0xFF_FFFF_FFFF) << 8 | v & 0xFFFF << 48, whole + 2);
    let bad = not_digits(id)
        | not_digits(region)
        | not_digits(amount)
        | (v >> 40 & 0xFF ^ u64::from(b'.'));
    let cents = swar8(amount) as i64;
    (
        swar8(id),
        swar8(region),
        if neg { -cents } else { cents },
        bad,
    )
}

/// `numbers` for any length. A region id too large to exist is just
/// unmatched; ids and amounts that large are beyond the spec's magnitudes
/// (section 3) and refused.
#[cold]
fn long_numbers(d: &[u8], at: usize, s: [usize; 5], end: usize) -> (u64, u64, i64, u64) {
    let (id, id_ok) = long_digits(d.get(at..s[0]).unwrap_or_default());
    let (region, region_ok) = long_digits(&d[s[2] + 1..s[3]]);
    let (cents, amount_ok) = long_amount(&d[s[3] + 1..end.max(s[3] + 1)]);
    (
        id,
        region,
        cents,
        u64::from(!(id_ok && region_ok && amount_ok && id < LIMIT)),
    )
}

/// The general cases of `digits`: none, or more than 8, with the value
/// saturating (spec 2.2 values have at most 8 digits).
#[cold]
fn long_digits(s: &[u8]) -> (u64, bool) {
    let value = s.iter().fold(0u64, |v, &b| {
        v.saturating_mul(10)
            .saturating_add(u64::from(b.wrapping_sub(b'0')))
    });
    (value, !s.is_empty() && s.iter().all(u8::is_ascii_digit))
}

#[cold]
fn long_amount(s: &[u8]) -> (i64, bool) {
    let (neg, body) = s.strip_prefix(b"-").map_or((false, s), |b| (true, b));
    let Some((whole, frac)) = body.split_at_checked(body.len().wrapping_sub(3)) else {
        return (0, false);
    };
    let (w, w_ok) = long_digits(whole);
    let (f, f_ok) = long_digits(&frac[1..]);
    let cents = (w * 100 + f) as i64;
    (
        if neg { -cents } else { cents },
        w_ok && f_ok && frac[0] == b'.' && w < LIMIT,
    )
}

/// `x` (8 bytes, little-endian) with all but its last `n` bytes set to '0'.
#[inline(always)]
fn pad_digits(x: u64, n: usize) -> u64 {
    let keep = !0u64 << (8 * (8 - n));
    x & keep | ZEROS & !keep
}

/// Nonzero unless all 8 bytes of `x` are ASCII digits.
#[inline(always)]
fn not_digits(x: u64) -> u64 {
    const HIGH: u64 = 0xF0F0_F0F0_F0F0_F0F0;
    (x & HIGH ^ ZEROS) | (x.wrapping_add(0x0606_0606_0606_0606) & HIGH ^ ZEROS)
}

/// The value of 8 ASCII digits, the first in the lowest byte: digit pairs,
/// then quads, then all eight, combined by multiplies (Lemire's method).
#[inline(always)]
fn swar8(x: u64) -> u64 {
    let v = x.wrapping_sub(ZEROS);
    let v = v.wrapping_mul(10).wrapping_add(v >> 8);
    let lo = (v & 0xFF_0000_00FF).wrapping_mul(100 + (1_000_000 << 32));
    let hi = (v >> 16 & 0xFF_0000_00FF).wrapping_mul(1 + (10_000 << 32));
    lo.wrapping_add(hi) >> 32
}

/// Appends the positions of the set bits of `m` (block at `base`) to
/// `seps[n..]`, eight at a time without a branch per bit; returns the new
/// count. Up to 8 entries past it get junk, which `SEPS` leaves room for.
#[inline(always)]
fn push(seps: &mut [u32; SEPS], n: usize, mut m: u64, base: u32) -> usize {
    let end = n + m.count_ones() as usize;
    let mut i = n;
    while i < end {
        for j in 0..8 {
            seps[(i + j) & (SEPS - 1)] = base + m.trailing_zeros();
            m &= m.wrapping_sub(1);
        }
        i += 8;
    }
    end
}

/// Bits for the bytes of the 64-byte block at `at` that are `"`, `,`, LF, CR.
#[inline(always)]
fn masks(d: &[u8], at: usize) -> [u64; 4] {
    debug_assert!(at + 64 <= d.len());
    // SAFETY: the 64 bytes are in bounds as for `load`; AVX2 comes from
    // compiling the whole crate for x86-64-v3.
    unsafe {
        let p = d.as_ptr().add(at);
        let (a, b) = (
            _mm256_loadu_si256(p.cast()),
            _mm256_loadu_si256(p.add(32).cast()),
        );
        (*b"\",\n\r").map(|ch| {
            let ch = _mm256_set1_epi8(ch as i8);
            let lo = _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, ch)) as u32;
            let hi = _mm256_movemask_epi8(_mm256_cmpeq_epi8(b, ch)) as u32;
            u64::from(lo) | u64::from(hi) << 32
        })
    }
}

/// Bits for the quotes among the 32 bytes at `at`.
#[inline(always)]
fn quote_mask(d: &[u8], at: usize) -> u32 {
    debug_assert!(at + 32 <= d.len());
    // SAFETY: in bounds as for `load`; AVX2 as in `masks`.
    unsafe {
        let v = _mm256_loadu_si256(d.as_ptr().add(at).cast());
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'"' as i8))) as u32
    }
}

/// Bit i of the result is the XOR of bits 0..=i of `x`: a carry-less
/// multiply by all ones.
#[inline(always)]
fn prefix_xor(x: u64) -> u64 {
    // SAFETY: PCLMULQDQ is enabled for the whole crate (.cargo/config.toml).
    unsafe {
        let p = _mm_clmulepi64_si128(_mm_set_epi64x(0, x as i64), _mm_set1_epi8(-1), 0);
        _mm_cvtsi128_si64(p) as u64
    }
}

/// An unaligned little-endian load of a `T` at `d[at..]`.
#[inline(always)]
pub fn load<T: Copy>(d: &[u8], at: usize) -> T {
    debug_assert!(at + size_of::<T>() <= d.len());
    // SAFETY: the parser only loads at positions below `len + 64 * BATCH +
    // 64`, which `parse` asserts are inside `d`; `T` is a plain integer.
    unsafe { d.as_ptr().add(at).cast::<T>().read_unaligned() }
}
