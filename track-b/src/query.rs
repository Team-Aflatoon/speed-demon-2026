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

pub fn parse(text: &[u8], dict: &mut Dict) -> io::Result<Queries> {
    let mut queries = Queries {
        list: Vec::new(),
        steps: Vec::new(),
    };
    for line in text.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        let start = queries.steps.len() as u32;
        let phrase = parse_line(line, dict, &mut queries.steps).ok_or_else(|| bad_query(line))?;
        queries.list.push(Query {
            phrase,
            start,
            end: queries.steps.len() as u32,
        });
    }
    Ok(queries)
}

/// Appends the steps of one line; returns whether it is a phrase.
fn parse_line(line: &[u8], dict: &mut Dict, steps: &mut Vec<Step>) -> Option<bool> {
    if let Some(rest) = line.strip_prefix(b"BOOL ") {
        let mut words = rest.split(|&b| b == b' ');
        let mut op = Op::Or;
        loop {
            steps.push(Step {
                op,
                term: term(words.next()?, dict)?,
            });
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
        let op = if i == 0 { Op::Or } else { Op::And };
        steps.push(Step {
            op,
            term: term(word, dict)?,
        });
    }
    Some(true)
}

fn term(word: &[u8], dict: &mut Dict) -> Option<u32> {
    let valid = (1..=32).contains(&word.len())
        && word
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    valid.then(|| dict.insert(word))
}

fn bad_query(line: &[u8]) -> io::Error {
    let msg = format!("malformed query: {}", String::from_utf8_lossy(line));
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
