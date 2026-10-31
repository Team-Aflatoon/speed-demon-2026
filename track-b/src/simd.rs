//! Byte classification many bytes at a time, as bitmasks.
//!
//! The scoring CPU has AVX2 and the build enables it for the whole crate
//! (`.cargo/config.toml`), which the `cfg`s below check at compile time. Each
//! function also has a plain fallback for other targets.

#[cfg(target_feature = "avx2")]
use std::arch::x86_64::*;

/// For 64 bytes: bit i of the first mask is set when `bytes[i]` is a token
/// byte (ASCII letter or digit), bit i of the second when it is an LF.
#[inline]
pub fn classify(bytes: &[u8; 64]) -> (u64, u64) {
    #[cfg(target_feature = "avx2")]
    // SAFETY: this branch is only compiled when the target has AVX2.
    unsafe {
        let (a0, n0) = classify32(bytes.as_ptr());
        let (a1, n1) = classify32(bytes.as_ptr().add(32));
        (
            u64::from(a0) | u64::from(a1) << 32,
            u64::from(n0) | u64::from(n1) << 32,
        )
    }
    #[cfg(not(target_feature = "avx2"))]
    bytes.iter().enumerate().fold((0, 0), |(a, n), (i, &b)| {
        (
            a | u64::from(crate::dict::ALNUM[b as usize]) << i,
            n | u64::from(b == b'\n') << i,
        )
    })
}

/// Bit i is set when `a[i] | 0x20 == x` and `b[i] | 0x20 == y`. With `b` taken
/// `len - 1` bytes after `a`, these are the possible starts of a word of length
/// `len` whose first and last bytes are `x` and `y`, in any case.
#[inline]
pub fn pair_mask(a: &[u8; 32], b: &[u8; 32], x: u8, y: u8) -> u32 {
    #[cfg(target_feature = "avx2")]
    // SAFETY: this branch is only compiled when the target has AVX2.
    unsafe {
        let ax = fold_eq(a.as_ptr(), x);
        let by = fold_eq(b.as_ptr(), y);
        _mm256_movemask_epi8(_mm256_and_si256(ax, by)) as u32
    }
    #[cfg(not(target_feature = "avx2"))]
    (0..32).fold(0, |m, i| {
        m | u32::from(a[i] | 0x20 == x && b[i] | 0x20 == y) << i
    })
}

/// (token bytes, LFs) masks of the 32 bytes at `p`, which must be readable.
#[cfg(target_feature = "avx2")]
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
#[cfg(target_feature = "avx2")]
#[target_feature(enable = "avx2")]
fn in_range(v: __m256i, lo: u8, hi: u8) -> __m256i {
    let d = _mm256_sub_epi8(v, _mm256_set1_epi8(lo as i8));
    _mm256_cmpeq_epi8(_mm256_min_epu8(d, _mm256_set1_epi8((hi - lo) as i8)), d)
}

/// Lanes of the 32 bytes at `p` (readable) that equal `c` after `| 0x20`.
#[cfg(target_feature = "avx2")]
#[target_feature(enable = "avx2")]
unsafe fn fold_eq(p: *const u8, c: u8) -> __m256i {
    let v = _mm256_loadu_si256(p.cast());
    _mm256_cmpeq_epi8(
        _mm256_or_si256(v, _mm256_set1_epi8(0x20)),
        _mm256_set1_epi8(c as i8),
    )
}
