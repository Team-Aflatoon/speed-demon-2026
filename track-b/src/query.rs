//! Parsing `queries.txt` (spec 2.3) into term ids and operators.

use crate::dict::Dict;
use std::io;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Op {
    And,
    Or,
    Not,
}

#[derive(Clone, Copy)]
pub struct Step {
    pub op: Op,
    pub term: u32,
}

/// One query line: `steps[start..end]` of its `Queries`. A BOOL query is a
/// list of steps applied left to right to an initially empty set, so its first
/// step is `OR t1`. A phrase is a list of `AND` steps, which gives the
/// candidate documents to verify.
#[derive(Clone, Copy)]
pub struct Query {
    pub phrase: bool,
    pub start: u32,
    pub end: u32,
}

/// All queries in file order, with their steps in one shared array.
pub struct Queries {
    pub list: Vec<Query>,
    pub steps: Vec<Step>,
}

impl Queries {
    pub fn steps(&self, query: Query) -> &[Step] {
        &self.steps[query.start as usize..query.end as usize]
    }
}

/// Parses every line, then numbers the terms.
///
/// The term table is filled in a second pass: about 150,000 inserts into a
/// fresh table would each wait on a cache miss, so each one first prefetches
/// the slot of the word 16 places ahead.
pub fn parse(text: &[u8]) -> io::Result<(Queries, Dict)> {
    let mut queries = Queries {
        list: Vec::new(),
        steps: Vec::new(),
    };
    let mut words = Vec::new();
    for line in text.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        let start = queries.steps.len() as u32;
        let phrase =
            parse_line(line, &mut queries.steps, &mut words).ok_or_else(|| bad_query(line))?;
        let end = queries.steps.len() as u32;
        queries.list.push(Query { phrase, start, end });
    }
    let mut dict = Dict::with_capacity(words.len() / 2);
    for (i, step) in queries.steps.iter_mut().enumerate() {
        if let Some(ahead) = words.get(i + 16) {
            dict.prefetch_term(ahead);
        }
        step.term = dict.insert(words[i]);
    }
    Ok((queries, dict))
}

/// Appends the steps of one line, and their words to `words`; returns whether
/// it is a phrase. Term ids are filled in later.
fn parse_line<'a>(
    line: &'a [u8],
    steps: &mut Vec<Step>,
    words: &mut Vec<&'a [u8]>,
) -> Option<bool> {
    let mut push = |op, word: &'a [u8]| {
        let valid = (1..=32).contains(&word.len())
            && word
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        steps.push(Step { op, term: 0 });
        words.push(word);
        valid.then_some(())
    };
    if let Some(rest) = line.strip_prefix(b"BOOL ") {
        let mut words = rest.split(|&b| b == b' ');
        let mut op = Op::Or;
        loop {
            push(op, words.next()?)?;
            op = match words.next() {
                None => return Some(false),
                Some(b"AND") => Op::And,
                Some(b"OR") => Op::Or,
                Some(b"NOT") => Op::Not,
                Some(_) => return None,
            };
        }
    }
    let inner = line.strip_prefix(b"PHRASE \"")?.strip_suffix(b"\"")?;
    for (i, word) in inner.split(|&b| b == b' ').enumerate() {
        push(if i == 0 { Op::Or } else { Op::And }, word)?;
    }
    Some(true)
}

fn bad_query(line: &[u8]) -> io::Error {
    let msg = format!("malformed query: {}", String::from_utf8_lossy(line));
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
