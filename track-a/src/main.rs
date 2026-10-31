//! PROBE a1: the read floor. mmap /data/requests.log, count its newlines with AVX2 on
//! up to 12 threads (each a contiguous range), create an empty /out/summary.txt, exit 0.
use std::arch::x86_64::*;
use std::fs::File;
use std::os::fd::AsRawFd;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

fn count(bytes: &[u8]) -> u64 {
    let (mut n, mut k) = (0u64, 0);
    // SAFETY: each 32-byte load stays inside `bytes` (k + 64 <= len).
    unsafe {
        let nl = _mm256_set1_epi8(b'\n' as i8);
        while k + 64 <= bytes.len() {
            let x = _mm256_loadu_si256(bytes.as_ptr().add(k).cast());
            let y = _mm256_loadu_si256(bytes.as_ptr().add(k + 32).cast());
            n += u64::from((_mm256_movemask_epi8(_mm256_cmpeq_epi8(x, nl)) as u32).count_ones());
            n += u64::from((_mm256_movemask_epi8(_mm256_cmpeq_epi8(y, nl)) as u32).count_ones());
            k += 64;
        }
    }
    n + bytes[k..].iter().filter(|&&b| b == b'\n').count() as u64
}

fn main() -> std::io::Result<()> {
    let input = File::open("/data/requests.log")?;
    File::create("/out/summary.txt")?;
    let len = input.metadata()?.len() as usize;
    if len == 0 {
        return Ok(());
    }
    // SAFETY: a fresh private read-only mapping of the whole file, never unmapped.
    let data: &[u8] = unsafe {
        let p = mmap(std::ptr::null_mut(), len, 1, 2, input.as_raw_fd(), 0);
        if p as isize == -1 {
            return Err(std::io::Error::last_os_error());
        }
        std::slice::from_raw_parts(p, len)
    };
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).min(12);
    let lines: u64 = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|w| s.spawn(move || count(&data[len * w / workers..len * (w + 1) / workers])))
            .collect();
        handles.into_iter().map(|h| h.join().expect("no worker panics")).sum()
    });
    std::hint::black_box(lines);
    Ok(())
}
