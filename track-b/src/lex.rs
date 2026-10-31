//! Splitting a run of corpus lines into tokens and line ends.
//!
//! A byte-at-a-time tokenizer mispredicts a branch at nearly every token
//! boundary. Instead each 64-byte chunk becomes two bitmasks (token bytes and
//! LFs), the events are the set bits of `token starts | LFs`, and they are
//! visited with trailing-zero counts: the branches are per event, not per byte.

use crate::dict::ALNUM;
use crate::simd::classify;

pub enum Event {
    /// A token, `data[start..end]`.
    Token(usize, usize),
    /// The LF at this position.
    LineEnd(usize),
}

pub struct Lexer<'a> {
    data: &'a [u8],
    /// Start of the current chunk, and where the lines end.
    base: usize,
    end: usize,
    alnum: u64,
    lfs: u64,
    /// Events of the current chunk not yet returned.
    events: u64,
}

impl<'a> Lexer<'a> {
    /// Lexes `data[pos..end]`; both must be line starts.
    pub fn new(data: &'a [u8], pos: usize, end: usize) -> Lexer<'a> {
        let mut lexer = Lexer {
            data,
            base: pos,
            end,
            alnum: 0,
            lfs: 0,
            events: 0,
        };
        if pos < end {
            lexer.load(false);
        }
        lexer
    }

    /// Classifies the chunk at `base`. `carry` says whether the previous
    /// chunk ended inside a token, whose rest must not count as a new start.
    fn load(&mut self, carry: bool) {
        let chunk: [u8; 64] = match self.data.get(self.base..self.base + 64) {
            Some(bytes) => bytes.try_into().unwrap(),
            None => {
                let mut padded = [0; 64]; // zero is neither a token byte nor an LF
                padded[..self.data.len() - self.base].copy_from_slice(&self.data[self.base..]);
                padded
            }
        };
        let (alnum, lfs) = classify(&chunk);
        let starts = alnum & !(alnum << 1 | u64::from(carry));
        let live = match self.end - self.base {
            n if n >= 64 => !0,
            n => (1 << n) - 1,
        };
        (self.alnum, self.lfs, self.events) = (alnum, lfs & live, (starts | lfs) & live);
    }
}

impl Iterator for Lexer<'_> {
    type Item = Event;

    #[inline]
    fn next(&mut self) -> Option<Event> {
        while self.events == 0 {
            self.base += 64;
            if self.base >= self.end {
                return None;
            }
            self.load(self.alnum >> 63 != 0);
        }
        let i = self.events.trailing_zeros() as usize;
        self.events &= self.events - 1;
        let start = self.base + i;
        if self.lfs >> i & 1 != 0 {
            return Some(Event::LineEnd(start));
        }
        let run = (!(self.alnum >> i)).trailing_zeros() as usize;
        let mut end = start + run;
        if i + run == 64 {
            // The token runs into the next chunk. Lines end with an LF, so
            // this stops inside `data`.
            while ALNUM[self.data[end] as usize] {
                end += 1;
            }
        }
        Some(Event::Token(start, end))
    }
}
