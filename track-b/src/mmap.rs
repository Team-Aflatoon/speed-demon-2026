//! A read-only memory map of a whole file, declared by hand against libc so the
//! program needs no crates.
//!
//! The harness loads `/data` into the page cache before our cgroup exists, so a
//! mapping charges nothing for the file itself; reading it into a heap buffer
//! would charge all 254 MB to our memory peak.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::path::Path;

extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
        -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
}

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MAP_FAILED: *mut c_void = !0 as *mut c_void;

pub struct Mmap {
    ptr: *mut c_void,
    len: usize,
}

impl Mmap {
    pub fn open(path: &Path) -> io::Result<Mmap> {
        let file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len()).map_err(io::Error::other)?;
        if len == 0 {
            // mmap rejects empty mappings; an empty file is simply an empty slice.
            return Ok(Mmap {
                ptr: std::ptr::null_mut(),
                len,
            });
        }
        // SAFETY: a fresh private read-only mapping of an open file; the kernel
        // picks the address and we check for failure before using it.
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mmap { ptr, len })
    }
}

impl Deref for Mmap {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `ptr` maps `len` readable bytes until `drop`. /data is a
        // read-only mount, so nothing modifies the file underneath us.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        if self.len != 0 {
            // SAFETY: unmaps exactly the region `open` mapped; no slice outlives `self`.
            unsafe { munmap(self.ptr, self.len) };
        }
    }
}
