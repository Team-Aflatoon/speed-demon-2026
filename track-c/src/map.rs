//! Memory straight from the kernel, through hand-declared libc calls (no
//! crates): read-only file mappings with a zero-filled tail, and zeroed
//! tables. Neither is ever unmapped; both live until the process exits.

use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_long, c_void};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::{fs::File, io, ptr::null_mut, slice};

extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: c_long)
        -> *mut c_void;
    fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
}

const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_PRIVATE: c_int = 0x02;
const MAP_FIXED: c_int = 0x10;
const MAP_ANONYMOUS: c_int = 0x20;
const MAP_FAILED: *mut c_void = !0 as *mut c_void;
const MADV_HUGEPAGE: c_int = 14;
const HUGE_PAGE: usize = 2 << 20;

/// Zero bytes guaranteed readable after the end of every mapped file.
pub const PAD: usize = 4096;

/// A mapped file: `b` is its `len` bytes followed by at least `PAD` zero
/// bytes. The padding lets the parsers load whole words and vectors near
/// the end without special cases; a zero byte is never a digit, quote or
/// separator, so every scan stops on it.
#[derive(Clone, Copy)]
pub struct Mapped {
    pub b: &'static [u8],
    pub len: usize,
}

/// Maps the file at `path`. Its pages are already in the page cache (the
/// harness pre-reads /data), so this costs page-table entries, not copies.
pub fn map(path: &str) -> io::Result<Mapped> {
    let file = File::open(path)?;
    let len = usize::try_from(file.metadata()?.len()).map_err(io::Error::other)?;
    let span = len.next_multiple_of(PAD) + PAD;
    let base = anonymous(span, 1, PROT_READ)?;
    if len > 0 {
        let fd = file.as_raw_fd();
        // SAFETY: MAP_FIXED replaces the start of our own fresh reservation,
        // which nothing else refers to, with the file.
        let at = unsafe { mmap(base.cast(), len, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0) };
        if at == MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: `span` readable bytes that are never unmapped, of a file under
    // the read-only /data, so they stay valid and unchanged: 'static holds.
    Ok(Mapped { b: unsafe { slice::from_raw_parts(base, span) }, len })
}

/// `n` zeroed atomics. Pages never touched are never backed by memory.
/// With `huge`, the memory is 2 MiB-aligned and offered to the kernel as
/// transparent huge pages: for the shared name table, one page fault per
/// 2 MiB instead of 512 (workers would otherwise queue on the page-table
/// lock), and a handful of TLB entries for millions of random lookups.
pub fn zeroed(n: usize, huge: bool) -> io::Result<&'static [AtomicU64]> {
    let (bytes, align) = (n * 8, if huge { HUGE_PAGE } else { 1 });
    let base = anonymous(bytes, align, PROT_READ | PROT_WRITE)?;
    if huge {
        // SAFETY: only a hint about our own mapping; if the kernel refuses
        // it, the pages simply stay small.
        unsafe { madvise(base.cast(), bytes, MADV_HUGEPAGE) };
    }
    // SAFETY: `bytes` zeroed, writable, aligned bytes that are never
    // unmapped, and all-zero bits are a valid AtomicU64.
    let words: &[AtomicU64] = unsafe { slice::from_raw_parts(base.cast(), n) };
    if huge {
        // Fault each huge page in now, from this one thread. Workers racing
        // to touch a page first can each be given (and charged) 2 MiB, all
        // but one freed right away, which shows up in peak memory.
        words.iter().step_by(HUGE_PAGE / 8).for_each(|w| w.store(0, Relaxed));
    }
    Ok(words)
}

/// A fresh private anonymous mapping of `bytes` plus `align` of slack,
/// returned at its first `align`-aligned address.
fn anonymous(bytes: usize, align: usize, prot: c_int) -> io::Result<*mut u8> {
    let flags = MAP_PRIVATE | MAP_ANONYMOUS;
    // SAFETY: asks for new memory and touches none that exists.
    let raw = unsafe { mmap(null_mut(), bytes + align, prot, flags, -1, 0) };
    if raw == MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let raw = raw.cast::<u8>();
    Ok(raw.wrapping_add(raw.align_offset(align)))
}
