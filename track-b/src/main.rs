//! Speed Demon 2026, Track B: answers the BOOL and PHRASE queries of
//! `/data/queries.txt` over `/data/docs.txt` and writes `/out/results.txt`.
//!
//! The queries are known before the corpus is read, so there is no global
//! index. The corpus is cut into blocks of whole lines; worker threads claim
//! blocks, index each one for the query terms only, answer every query against
//! it and fold the per-block answers into one running answer per query. Count
//! and sum simply add up, and the ten smallest ids merge like sorted lists.

mod dict;
mod eval;
mod index;
mod lex;
mod mmap;
mod simd;

use dict::Dict;
use eval::{Answer, Evaluator};
use index::{bad_corpus, Index};
use mmap::Mmap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;

/// About 3,500 average documents, so most blocks are a single batch.
const BLOCK_BYTES: usize = 3 << 20;
const MAX_THREADS: usize = 12;

fn main() {
    mmap::disable_huge_pages();
    if let Err(err) = run(Path::new("/data"), Path::new("/out")) {
        eprintln!("track-b: {err}");
        std::process::exit(1);
    }
}

fn run(data_dir: &Path, out_dir: &Path) -> io::Result<()> {
    let (queries, dict) = parse(&Mmap::open(&data_dir.join("queries.txt"))?)?;
    let docs = Mmap::open(&data_dir.join("docs.txt"))?;
    if !docs.is_empty() && !docs.ends_with(b"\n") {
        return Err(bad_corpus("the last line has no LF"));
    }
    let answers = search(&docs, &dict, &queries)?;
    let mut out =
        io::BufWriter::with_capacity(1 << 16, std::fs::File::create(out_dir.join("results.txt"))?);
    answers
        .iter()
        .try_for_each(|answer| write_answer(&mut out, answer))?;
    out.flush()
}

fn search(docs: &[u8], dict: &Dict, queries: &Queries) -> io::Result<Vec<Answer>> {
    let answers: Vec<Mutex<Answer>> = queries.list.iter().map(|_| Mutex::default()).collect();
    let blocks = docs.len().div_ceil(BLOCK_BYTES);
    let next = AtomicUsize::new(0);
    let threads = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_THREADS);
    let worker = || -> io::Result<()> {
        let mut index = Index::new(dict.len());
        let mut eval = Evaluator::new();
        loop {
            let block = next.fetch_add(1, Ordering::Relaxed);
            if block >= blocks {
                return Ok(());
            }
            let mut pos = line_start(docs, block * BLOCK_BYTES);
            let end = line_start(docs, (block + 1) * BLOCK_BYTES);
            while pos < end {
                pos = index.build(docs, pos, end, dict)?;
                for (&query, total) in queries.list.iter().zip(&answers) {
                    let answer = eval.run(&index, queries.steps(query), query.phrase, docs, dict);
                    if answer.count > 0 {
                        total.lock().unwrap().merge(&answer);
                    }
                }
            }
        }
    };
    thread::scope(|s| {
        let handles: Vec<_> = (0..threads).map(|_| s.spawn(worker)).collect();
        handles.into_iter().try_for_each(|h| h.join().unwrap())
    })?;
    Ok(answers
        .into_iter()
        .map(|a| a.into_inner().unwrap())
        .collect())
}

/// The first line start at or after `pos`, so every line belongs to exactly
/// the block its first byte falls in.
fn line_start(docs: &[u8], pos: usize) -> usize {
    if pos == 0 || pos >= docs.len() {
        return pos.min(docs.len());
    }
    docs[pos - 1..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(docs.len(), |i| pos + i)
}

fn write_answer(out: &mut impl Write, answer: &Answer) -> io::Result<()> {
    write!(out, "{} {}", answer.count, answer.sum)?;
    answer
        .ten()
        .iter()
        .try_for_each(|id| write!(out, " {id}"))?;
    out.write_all(b"\n")
}

// ---- Queries (spec 2.3): parsed into term ids and operators.

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
