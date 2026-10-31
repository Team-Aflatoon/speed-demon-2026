//! Memory straight from the kernel through hand-declared libc calls (no
//! crates): file mappings with a zeroed tail, and lazily zeroed tables.
//! Nothing is ever unmapped; it all lives until the process exits.

use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_long, c_void};
use std::{fs::File, io, ptr::null_mut, slice};

extern "C" {
    fn mmap(at: *mut c_void, n: usize, prot: c_int, flags: c_int, fd: c_int, o: c_long) -> *mut u8;
    fn prctl(option: c_int, ...) -> c_int;
}

const PROT_RW: c_int = 1 | 2;
const MAP_PRIVATE: c_int = 0x02;
const MAP_FIXED: c_int = 0x10;
const MAP_ANONYMOUS: c_int = 0x20;
const MAP_NORESERVE: c_int = 0x4000;
const PR_SET_THP_DISABLE: c_int = 41;

/// Zero bytes readable (and writable) after the end of every mapped file,
/// so that parsers can load whole vectors near the end without a special
/// case. A zero byte is never a digit, quote or separator.
pub const PAD: usize = 4096;

/// Turns transparent huge pages off for this process. The scoring machine
/// runs with THP=always, which would back each barely touched table with
/// 2 MiB, all of it charged as memory.
pub fn no_huge_pages() {
    // SAFETY: PR_SET_THP_DISABLE takes four integer arguments and changes
    // only a flag of this process.
    unsafe { prctl(PR_SET_THP_DISABLE, 1u64, 0u64, 0u64, 0u64) };
}

/// Maps the file at `path` copy-on-write, followed by at least `PAD` zero
/// bytes; returns the mapping and the file's length. The harness has the
/// file in the page cache, so this costs page-table entries, not copies,
/// and only a page that is written to becomes private memory.
pub fn map_file(path: &str) -> io::Result<(&'static mut [u8], usize)> {
    let file = File::open(path)?;
    let len = usize::try_from(file.metadata()?.len()).map_err(io::Error::other)?;
    let span = len.next_multiple_of(PAD) + PAD;
    let base = anonymous(span)?;
    if len > 0 {
        // SAFETY: MAP_FIXED replaces the start of our own fresh mapping,
        // which nothing refers to yet, with the file.
        let at = unsafe {
            mmap(
                base.cast(),
                len,
                PROT_RW,
                MAP_PRIVATE | MAP_FIXED,
                file.as_raw_fd(),
                0,
            )
        };
        if at != base {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: `span` bytes we own exclusively that are never unmapped. The
    // mapping is private, so later changes to the file cannot show through.
    Ok((unsafe { slice::from_raw_parts_mut(base, span) }, len))
}

/// `n` values of `T` in fresh zeroed memory. Pages are only backed (and
/// charged) once touched, so a generous `n` costs address space only.
///
/// # Safety
/// All-zero bytes must be a valid `T` (true of the atomic integers used).
pub unsafe fn zeroed<T>(n: usize) -> io::Result<&'static [T]> {
    let base = anonymous(n * size_of::<T>())?;
    // SAFETY: page-aligned zeroed memory that is never unmapped, valid as
    // `T` by the caller's promise; atomics make sharing it sound.
    Ok(unsafe { slice::from_raw_parts(base.cast(), n) })
}

fn anonymous(bytes: usize) -> io::Result<*mut u8> {
    let flags = MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE;
    // SAFETY: asks for new memory and touches none that exists.
    let at = unsafe { mmap(null_mut(), bytes.max(1), PROT_RW, flags, -1, 0) };
    if at as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(at)
}
