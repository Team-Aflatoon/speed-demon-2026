//! regions.csv: a map from region id to group, filled in parallel pieces.

use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::sync::Mutex;

/// Three capital letters: 26^3 codes.
const CODES: usize = 26 * 26 * 26;
/// Region ids beyond this are refused (the map has a byte per id; spec
/// 2.3 ids are below 1,000,000).
const MAX_ID: u64 = 1 << 28;

/// A filled piece's first and last region id (0, 0 if it has none).
pub type Filled = Result<(u64, u64), String>;

pub struct Regions {
    /// Group by region id; 0 is `_UNMATCHED` (no such region).
    pub map: &'static [AtomicU8],
    /// Group by country code, numbered in order of first sight (0: unused).
    codes: Vec<AtomicU8>,
    groups: Mutex<u8>,
    text: &'static [u8],
    /// Line starts that cut the lines into one piece per worker.
    cuts: Vec<usize>,
}

impl Regions {
    pub fn new(text: &'static [u8], len: usize, pieces: usize) -> Result<Regions, String> {
        let body = text[..len]
            .strip_prefix(b"region_id,country\n")
            .ok_or("regions.csv: bad header")?;
        let first = len - body.len();
        if !body.is_empty() && body[body.len() - 1] != b'\n' {
            return Err("regions.csv: no LF at the end".into());
        }
        // Ids ascend (2.3), so the last line has the largest.
        let last = body[..body.len().saturating_sub(1)]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        let max = if body.is_empty() {
            0
        } else {
            parse_id(&body[last..], &mut 0)?
        };
        // SAFETY: all-zero bytes are valid AtomicU8 values.
        let map = unsafe { crate::sys::zeroed::<AtomicU8>(max as usize + 1) }
            .map_err(|e| e.to_string())?;
        let mut cuts: Vec<usize> = (0..pieces)
            .map(|p| first + body.len() * p / pieces)
            .collect();
        for c in &mut cuts[1..] {
            *c += text[*c - 1..len]
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(0);
        }
        cuts.push(len);
        let codes = (0..CODES).map(|_| AtomicU8::new(0)).collect();
        Ok(Regions {
            map,
            codes,
            groups: Mutex::new(0),
            text,
            cuts,
        })
    }

    /// Fills the map for the lines of piece `p`; returns its first and last
    /// region ids (0, 0 if it has none) so that the caller can check that
    /// they ascend across pieces too.
    pub fn fill(&self, p: usize) -> Filled {
        let (mut at, end) = (self.cuts[p], self.cuts[p + 1]);
        let (mut first, mut prev) = (0, 0);
        while at < end {
            let id = parse_id(self.text, &mut at)?;
            let c = &self.text[at..at + 4];
            if id <= prev
                || id as usize >= self.map.len()
                || c[3] != b'\n'
                || !c[..3].iter().all(u8::is_ascii_uppercase)
            {
                return Err(format!(
                    "regions.csv: bad line at byte {at}, or ids not ascending"
                ));
            }
            let code = c[..3]
                .iter()
                .fold(0, |v, &b| v * 26 + usize::from(b - b'A'));
            self.map[id as usize].store(self.group(code)?, Relaxed);
            (first, prev, at) = (if first == 0 { id } else { first }, id, at + 4);
        }
        Ok((first, prev))
    }

    fn group(&self, code: usize) -> Result<u8, String> {
        let g = self.codes[code].load(Relaxed);
        if g != 0 {
            return Ok(g);
        }
        let mut n = self.groups.lock().unwrap();
        if self.codes[code].load(Relaxed) == 0 {
            *n = n.checked_add(1).ok_or("regions.csv: over 255 countries")?;
            self.codes[code].store(*n, Relaxed);
        }
        Ok(self.codes[code].load(Relaxed))
    }

    pub fn pieces(&self) -> usize {
        self.cuts.len() - 1
    }

    /// (country, group) in country order, which is output order.
    pub fn countries(&self) -> impl Iterator<Item = (String, u8)> + '_ {
        self.codes.iter().enumerate().filter_map(|(code, g)| {
            let g = g.load(Relaxed);
            let letters =
                [code / 676, code / 26 % 26, code % 26].map(|x| char::from(b'A' + x as u8));
            (g != 0).then(|| (letters.iter().collect(), g))
        })
    }
}

/// The id at `t[*at..]` up to its comma, which `*at` is moved past.
fn parse_id(t: &[u8], at: &mut usize) -> Result<u64, String> {
    let digits = t[*at..].iter().take_while(|b| b.is_ascii_digit()).count();
    let id = t[*at..*at + digits].iter().fold(0u64, |v, &b| {
        v.saturating_mul(10).saturating_add(u64::from(b - b'0'))
    });
    if digits == 0 || t.get(*at + digits) != Some(&b',') || id > MAX_ID {
        return Err(format!("regions.csv: bad region id at byte {at}"));
    }
    *at += digits + 1;
    Ok(id)
}
