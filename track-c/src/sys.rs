//! The few kernel calls the standard library does not offer, declared by
//! hand (no crates): memory straight from `mmap`, and `prctl` to refuse
//! transparent huge pages. Nothing here is ever unmapped; it all lives until
//! the process exits.

use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_long, c_void};
use std::{fs::File, io, ptr::null_mut, slice};

extern "C" {
    fn mmap(at: *mut c_void, n: usize, prot: c_int, how: c_int, fd: c_int, o: c_long) -> *mut u8;
    fn prctl(option: c_int, ...) -> c_int;
    fn madvise(at: *mut c_void, n: usize, advice: c_int) -> c_int;
}

const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_PRIVATE: c_int = 0x02;
const MAP_ANONYMOUS: c_int = 0x20;
/// Reserve address space without promising memory for it: the per-thread
/// pair arenas are sized for the worst case and mostly never touched.
const MAP_NORESERVE: c_int = 0x4000;
const MAP_FAILED: *mut u8 = !0 as *mut u8;
const PR_SET_THP_DISABLE: c_int = 41;
const MADV_POPULATE_WRITE: c_int = 23;

/// Turns transparent huge pages off for this process. A huge-page fault can
/// stall while the kernel compacts memory, and one would charge 2 MiB for a
/// table that is barely touched.
pub fn no_huge_pages() {
    // SAFETY: a plain prctl with integer arguments; it touches no memory of
    // ours. If the kernel refuses, we just keep its default.
    let (one, zero): (c_long, c_long) = (1, 0);
    unsafe { prctl(PR_SET_THP_DISABLE, one, zero, zero, zero) };
}

/// `n` zeroed values of `T`, in fresh pages that are only backed by memory
/// once touched.
///
/// # Safety
/// All-zero bytes must be a valid `T` (true for integers and atomics, the
/// only callers' types).
pub unsafe fn zeroed<T>(n: usize) -> io::Result<&'static mut [T]> {
    let bytes = (n * size_of::<T>()).max(1);
    let flags = MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE;
    // SAFETY: asks for new memory and touches none that exists.
    let base = unsafe { mmap(null_mut(), bytes, PROT_READ | PROT_WRITE, flags, -1, 0) };
    if base == MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `bytes` zeroed, writable, page-aligned bytes that are never
    // unmapped or aliased, and zero is a valid `T` (the caller's promise).
    Ok(unsafe { slice::from_raw_parts_mut(base.cast(), n) })
}

/// Maps a whole (non-empty) file read-only. Its pages are already in the page
/// cache, so this costs page-table entries, not copies.
pub fn map(file: &File, len: usize) -> io::Result<&'static [u8]> {
    let fd = file.as_raw_fd();
    // SAFETY: a new private read-only mapping; it aliases nothing of ours.
    let base = unsafe { mmap(null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0) };
    if base == MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `len` readable bytes that are never unmapped, of a file under
    // the read-only /data, so they stay valid and unchanged.
    Ok(unsafe { slice::from_raw_parts(base, len) })
}

/// Faults in the pages of `mem` (memory from `zeroed`) without changing
/// them: one call for the whole range, which the kernel walks far faster
/// than it takes one page fault after another. Only a hint: on a kernel
/// without the call (before 5.14) the pages fault in when first used.
pub fn populate<T>(mem: &[T]) {
    // SAFETY: the range is our own mapping, and populating leaves its bytes
    // as they are.
    let (at, n) = (mem.as_ptr() as *mut c_void, size_of_val(mem));
    unsafe { madvise(at, n, MADV_POPULATE_WRITE) };
}
