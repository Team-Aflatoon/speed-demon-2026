//! regions.csv: a direct-indexed map from region id to group, filled by the
//! workers in parallel at start-up. Group 0 is `_UNMATCHED`; countries get
//! 1..=255 in order of first sight, and are listed in code order at the end.
//! Any line that is not `id,CCC` stops the program with its byte offset.

use crate::{die, or_die};
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::sync::Mutex;

const HEADER: &[u8] = b"region_id,country\n";
const CODES: usize = 26 * 26 * 26;
/// The largest region id the map is sized for (a byte per id).
const MAX_ID: usize = 100_000_000;

pub struct Regions {
    file: &'static [u8],
    /// Group by region id, one byte each. The last entry is a 0 that every
    /// larger id is clamped to (see `parse::fields`), so a lookup never
    /// branches.
    pub map: &'static [AtomicU8],
    /// Group by country code index (`AAA` = 0); 0 until the code is seen.
    codes: Vec<AtomicU8>,
    /// Groups handed out so far; held while handing out a new one.
    groups: Mutex<u8>,
}

impl Regions {
    /// Maps the file and sizes the map by the id on its last line, the
    /// largest, since spec 2.3 lists ids unique and ascending (`parse`
    /// rejects any id above it, so a file out of order cannot overrun).
    pub fn open(path: &str) -> Self {
        let f = or_die(std::fs::File::open(path), path);
        let len = or_die(f.metadata(), path).len() as usize;
        let file = if len > HEADER.len() { or_die(crate::sys::map(&f, len), path) } else { b"" };
        if !file.starts_with(HEADER) || file[len - 1] != b'\n' {
            die(&format!("{path}: bad header, or no LF at the end"));
        }
        let last = file[..len - 1].iter().rposition(|&c| c == b'\n').unwrap() + 1;
        let max = file[last..].iter().take_while(|c| c.is_ascii_digit());
        let max = max.fold(0, |n, &c| (n * 10 + usize::from(c - b'0')).min(MAX_ID + 1));
        if max > MAX_ID {
            die(&format!("{path}: last id is over {MAX_ID}"));
        }
        // SAFETY: zero is a valid AtomicU8.
        let map = or_die(unsafe { crate::sys::zeroed(max + 2) }, "memory");
        let codes = (0..CODES).map(|_| AtomicU8::new(0)).collect();
        Regions { file, map, codes, groups: Mutex::new(0) }
    }

    /// Adds share `t` of `n` of the lines `id,CCC\n`. A share runs from the
    /// first line start at or after its nominal start to the next share's.
    pub fn parse(&self, t: usize, n: usize) {
        let b = self.file;
        let start = |k: usize| {
            let p = HEADER.len() + k * (b.len() - HEADER.len()) / n;
            p + b[p - 1..].iter().position(|&c| c == b'\n').unwrap() // the last byte is an LF
        };
        let (mut p, end) = (start(t), start(t + 1));
        while p < end {
            let (line, mut id) = (p, 0);
            while b[p].is_ascii_digit() && id <= MAX_ID {
                (id, p) = (id * 10 + usize::from(b[p] - b'0'), p + 1);
            }
            let code = b.get(p + 1..p + 4).unwrap_or(b"");
            let ok = p > line && id < self.map.len() - 1 && b[p] == b',';
            if !ok || b.get(p + 4) != Some(&b'\n') || !code.iter().all(u8::is_ascii_uppercase) {
                die(&format!("regions.csv, line at byte {line}: not `id,CCC`, or id out of order"));
            }
            let code = code.iter().fold(0, |c, &l| c * 26 + usize::from(l - b'A'));
            self.map[id].store(self.group_of(code), Relaxed);
            p += 5;
        }
    }

    fn group_of(&self, code: usize) -> u8 {
        let g = self.codes[code].load(Relaxed);
        if g != 0 {
            return g;
        }
        let mut groups = self.groups.lock().unwrap();
        // Another worker may have handed this code a group meanwhile.
        if self.codes[code].load(Relaxed) == 0 {
            *groups = groups.checked_add(1).unwrap_or_else(|| die("regions.csv: over 255 countries"));
            self.codes[code].store(*groups, Relaxed);
        }
        self.codes[code].load(Relaxed)
    }

    /// `(code, group)` for every country seen, in code order.
    pub fn countries(&self) -> impl Iterator<Item = (String, u8)> + '_ {
        let letters = |c: usize| [c / 676, c / 26 % 26, c % 26].map(|l| (b'A' + l as u8) as char);
        let seen = self.codes.iter().map(|g| g.load(Relaxed)).enumerate();
        seen.filter(|&(_, g)| g != 0).map(move |(c, g)| (letters(c).iter().collect(), g))
    }
}
