//! Byte classification many bytes at a time, as bitmasks, with AVX2.
//!
//! The scoring CPU has AVX2 and `.cargo/config.toml` enables it for the whole
//! crate. A build without it is refused at compile time rather than given a
//! slow fallback that the shipped configuration would never run.

#[cfg(not(target_feature = "avx2"))]
compile_error!("track-b needs AVX2: build with -C target-cpu=x86-64-v3 (see .cargo/config.toml)");

use std::arch::x86_64::*;

/// For 64 bytes: bit i of the first mask is set when `bytes[i]` is a token
/// byte (ASCII letter or digit), bit i of the second when it is an LF.
#[inline]
pub fn classify(bytes: &[u8; 64]) -> (u64, u64) {
    // SAFETY: AVX2 is present (checked above at compile time), and the two
    // 32-byte loads at offsets 0 and 32 stay inside the 64-byte array.
    let ((a0, n0), (a1, n1)) = unsafe {
        (
            classify32(bytes.as_ptr()),
            classify32(bytes.as_ptr().add(32)),
        )
    };
    (
        u64::from(a0) | u64::from(a1) << 32,
        u64::from(n0) | u64::from(n1) << 32,
    )
}

/// Bit i is set when `a[i] | 0x20 == x` and `b[i] | 0x20 == y`. With `b` taken
/// `len - 1` bytes after `a`, these are the possible starts of a word of length
/// `len` whose first and last bytes are `x` and `y`, in any case.
#[inline]
pub fn pair_mask(a: &[u8; 32], b: &[u8; 32], x: u8, y: u8) -> u32 {
    // SAFETY: AVX2 is present, and each load reads exactly one 32-byte array.
    unsafe {
        _mm256_movemask_epi8(_mm256_and_si256(
            fold_eq(a.as_ptr(), x),
            fold_eq(b.as_ptr(), y),
        )) as u32
    }
}

/// (token bytes, LFs) masks of the 32 bytes at `p`, which must be readable.
#[target_feature(enable = "avx2")]
unsafe fn classify32(p: *const u8) -> (u32, u32) {
    let v = _mm256_loadu_si256(p.cast());
    let letter = in_range(_mm256_or_si256(v, _mm256_set1_epi8(0x20)), b'a', b'z');
    let digit = in_range(v, b'0', b'9');
    let lf = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8));
    let alnum = _mm256_movemask_epi8(_mm256_or_si256(letter, digit)) as u32;
    (alnum, _mm256_movemask_epi8(lf) as u32)
}

/// Bytes in `lo..=hi`: subtract `lo`, then an unsigned `<=` done with `min`.
#[target_feature(enable = "avx2")]
fn in_range(v: __m256i, lo: u8, hi: u8) -> __m256i {
    let d = _mm256_sub_epi8(v, _mm256_set1_epi8(lo as i8));
    _mm256_cmpeq_epi8(_mm256_min_epu8(d, _mm256_set1_epi8((hi - lo) as i8)), d)
}

/// Lanes of the 32 bytes at `p` (readable) that equal `c` after `| 0x20`.
#[target_feature(enable = "avx2")]
unsafe fn fold_eq(p: *const u8, c: u8) -> __m256i {
    let v = _mm256_loadu_si256(p.cast());
    _mm256_cmpeq_epi8(
        _mm256_or_si256(v, _mm256_set1_epi8(0x20)),
        _mm256_set1_epi8(c as i8),
    )
}
