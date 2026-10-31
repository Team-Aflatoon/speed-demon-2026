//! regions.csv: a direct-indexed map from region id to group, filled by all
//! workers in parallel. Group 0 is `_UNMATCHED`; countries get 1..=255 in
//! order of first appearance, and are listed in code order at the end.

use crate::map::Mapped;
use crate::records::uint;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::sync::Mutex;

const HEADER: &[u8] = b"region_id,country\n";
const CODES: usize = 26 * 26 * 26;
/// The largest region id the map is sized for (a byte per id).
const MAX_ID: usize = 100_000_000;

pub struct Regions {
    /// Group by region id; 0 for an id the table does not contain.
    by_id: Vec<AtomicU8>,
    /// Group by country code index (`AAA` = 0); 0 until the code is seen.
    by_code: Vec<AtomicU8>,
    /// Groups handed out so far; held while handing out a new one.
    groups: Mutex<u8>,
}

impl Regions {
    /// An empty map, sized by the id on the file's last line: the largest,
    /// as the spec lists ids in ascending order (`parse` checks that).
    pub fn new(f: Mapped) -> Result<Self, &'static str> {
        let (b, len) = (f.b, f.len);
        if !b.starts_with(HEADER) || b[len - 1] != b'\n' {
            return Err("bad header, or no LF at the end");
        }
        let last = b[..len - 1].iter().rposition(|&c| c == b'\n');
        let max = uint(b, last.map_or(0, |i| i + 1)).map_or(0, |(id, _)| id as usize);
        if max > MAX_ID {
            return Err("region ids over 100,000,000 (the spec's stay below 1,000,000)");
        }
        let zeros = |n| (0..n).map(|_| AtomicU8::new(0)).collect();
        Ok(Regions { by_id: zeros(max + 1), by_code: zeros(CODES), groups: Mutex::new(0) })
    }

    pub fn group(&self, region: u64) -> u8 {
        self.by_id.get(region as usize).map_or(0, |g| g.load(Relaxed))
    }

    /// Starts loading the map entry for `region` into the cache.
    pub fn prefetch(&self, region: u64) {
        if let Some(g) = self.by_id.get(region as usize) {
            crate::prefetch(g);
        }
    }

    /// Adds share `t` of `n` of the lines `id,CCC\n`. A share runs from the
    /// first line start at or after its nominal start to the next share's.
    pub fn parse(&self, f: Mapped, t: usize, n: usize) -> Result<(), &'static str> {
        let b = f.b;
        let start = |k: usize| match k * f.len / n {
            0 => HEADER.len(),
            at => b[at - 1..f.len].iter().position(|&c| c == b'\n').map_or(f.len, |i| at + i),
        };
        let (mut p, end) = (start(t), start(t + 1));
        while p < end {
            let (id, q) = uint(b, p).ok_or("bad region id")?;
            let code = &b[q + 1..q + 4];
            if b[q] != b',' || b[q + 4] != b'\n' || !code.iter().all(u8::is_ascii_uppercase) {
                return Err("bad line");
            }
            let code = code.iter().fold(0, |c, &l| c * 26 + usize::from(l - b'A'));
            let slot = self.by_id.get(id as usize).ok_or("ids are not ascending")?;
            slot.store(self.group_of(code)?, Relaxed);
            p = q + 5;
        }
        Ok(())
    }

    fn group_of(&self, code: usize) -> Result<u8, &'static str> {
        let g = self.by_code[code].load(Relaxed);
        if g != 0 {
            return Ok(g);
        }
        let mut groups = self.groups.lock().unwrap();
        // Another worker may have handed this code a group meanwhile.
        if self.by_code[code].load(Relaxed) == 0 {
            *groups = groups.checked_add(1).ok_or("more than 255 countries")?;
            self.by_code[code].store(*groups, Relaxed);
        }
        Ok(self.by_code[code].load(Relaxed))
    }

    /// `(code, group)` for every country seen, in code order.
    pub fn countries(&self) -> impl Iterator<Item = (String, u8)> + '_ {
        let letters = |c: usize| [c / 676, c / 26 % 26, c % 26].map(|l| (b'A' + l as u8) as char);
        let groups = self.by_code.iter().map(|g| g.load(Relaxed)).enumerate();
        groups.filter(|&(_, g)| g != 0).map(move |(c, g)| (letters(c).iter().collect(), g))
    }
}
