//! Memory straight from the kernel, through hand-declared libc calls (no
//! crates): read-only file mappings with a zero-filled tail, and zeroed
//! tables. Neither is ever unmapped; both live until the process exits.

use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_long, c_void};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::{fs::File, io, ptr::null_mut, slice};

extern "C" {
    fn mmap(at: *mut c_void, n: usize, prot: c_int, how: c_int, fd: c_int, o: c_long) -> *mut u8;
    fn madvise(at: *mut u8, n: usize, advice: c_int) -> c_int;
}

const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_PRIVATE: c_int = 0x02;
const MAP_FIXED: c_int = 0x10;
const MAP_ANONYMOUS: c_int = 0x20;
const MAP_FAILED: *mut u8 = !0 as *mut u8;
const MADV_NOHUGEPAGE: c_int = 15;
const PAGE: usize = 4096;
/// The memory one page-table page maps.
const STRETCH: usize = 2 << 20;

/// Zero bytes guaranteed readable after the end of every mapped file.
pub const PAD: usize = PAGE;

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
    let base = anonymous(span, PROT_READ)?;
    let (fd, flags) = (file.as_raw_fd(), MAP_PRIVATE | MAP_FIXED);
    // SAFETY: MAP_FIXED replaces the start of our own fresh reservation,
    // which nothing else refers to, with the file.
    let at = unsafe { mmap(base.cast(), len, PROT_READ, flags, fd, 0) };
    if len > 0 && at == MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `span` readable bytes that are never unmapped, of a file under
    // the read-only /data, so they stay valid and unchanged: 'static holds.
    let b = unsafe { slice::from_raw_parts(base, span) };
    Ok(Mapped { b, len })
}

/// `n` zeroed atomics, in 4 KiB pages: transparent huge pages are refused.
/// They made no measurable difference to speed, a huge-page fault can stall
/// for milliseconds while the kernel compacts memory, and a kernel set to
/// use them everywhere would charge 2 MiB for a table that is barely
/// touched. Only touched pages are ever backed by memory.
pub fn zeroed(n: usize) -> io::Result<&'static [AtomicU64]> {
    let bytes = n * 8;
    let base = anonymous(bytes, PROT_READ | PROT_WRITE)?;
    // SAFETY: only a hint about our own mapping.
    unsafe { madvise(base, bytes, MADV_NOHUGEPAGE) };
    // SAFETY: `bytes` zeroed, writable, aligned bytes that are never
    // unmapped, and all-zero bits are a valid AtomicU64.
    Ok(unsafe { slice::from_raw_parts(base.cast(), n) })
}

/// Faults in every page of 2 MiB stretches `part`, `part + parts`, ... of
/// `words`. A shared table touched at random by all workers would fault in
/// with every worker queueing on the lock of the page table covering each
/// 2 MiB; giving each stretch to one worker faults it all in parallel.
pub fn touch(words: &[AtomicU64], part: usize, parts: usize) {
    for stretch in words.chunks(STRETCH / 8).skip(part).step_by(parts) {
        stretch
            .iter()
            .step_by(PAGE / 8)
            .for_each(|w| w.store(0, Relaxed));
    }
}

/// A fresh private anonymous mapping of `bytes`.
fn anonymous(bytes: usize, prot: c_int) -> io::Result<*mut u8> {
    let flags = MAP_PRIVATE | MAP_ANONYMOUS;
    // SAFETY: asks for new memory and touches none that exists.
    let raw = unsafe { mmap(null_mut(), bytes, prot, flags, -1, 0) };
    if raw == MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(raw)
}
