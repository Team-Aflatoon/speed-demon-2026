//! Box probe (deliberately fails the hash): how long does it take just to get the input's
//! bytes through 12 threads? `probe pread <KB>`: each thread claims KB-sized chunks from a
//! shared counter, preads them into its own buffer and counts newlines with AVX2.
//! `probe mmap <KB>`: the same over a private read-only mapping instead (the old floor).
//! Writes the newline count to /out/summary.txt.
use std::arch::x86_64::*;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}
unsafe fn count(p: *const u8, len: usize) -> u64 {
    let (nl, mut n, mut i) = (_mm256_set1_epi8(10), 0u64, 0);
    while i + 32 <= len {
        let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(i).cast()), nl));
        n += (m as u32).count_ones() as u64;
        i += 32;
    }
    n + (i..len).filter(|&j| *p.add(j) == b'\n').count() as u64
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let chunk = args[2].parse::<usize>().unwrap() << 10;
    let f = File::open("/data/requests.log").unwrap();
    let len = f.metadata().unwrap().len() as usize;
    let map = (args[1] == "mmap").then(|| unsafe { mmap(std::ptr::null_mut(), len, 1, 2, f.as_raw_fd(), 0) } as usize);
    let (next, total) = (AtomicUsize::new(0), AtomicU64::new(0));
    std::thread::scope(|s| {
        for _ in 0..12 {
            s.spawn(|| {
                let (mut buf, mut n) = (vec![0u8; chunk], 0);
                loop {
                    let at = next.fetch_add(1, Relaxed) * chunk;
                    if at >= len { break }
                    let got = (len - at).min(chunk);
                    n += unsafe { match map {
                        Some(base) => count((base as *const u8).add(at), got),
                        None => { f.read_exact_at(&mut buf[..got], at as u64).unwrap(); count(buf.as_ptr(), got) }
                    } };
                }
                total.fetch_add(n, Relaxed);
            });
        }
    });
    std::fs::write("/out/summary.txt", format!("{}\n", total.load(Relaxed))).unwrap();
}
