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
mod query;
mod simd;

use dict::Dict;
use eval::{Answer, Evaluator};
use index::{bad_corpus, Index};
use mmap::Mmap;
use query::Queries;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;

/// About 3,500 average documents, so most blocks are a single batch.
const BLOCK_BYTES: usize = 3 << 20;
const MAX_THREADS: usize = 12;

fn main() {
    if let Err(err) = run(Path::new("/data"), Path::new("/out")) {
        eprintln!("track-b: {err}");
        std::process::exit(1);
    }
}

fn run(data_dir: &Path, out_dir: &Path) -> io::Result<()> {
    let mut dict = Dict::new();
    let queries = query::parse(&Mmap::open(&data_dir.join("queries.txt"))?, &mut dict)?;
    let docs = Mmap::open(&data_dir.join("docs.txt"))?;
    if !docs.is_empty() && !docs.ends_with(b"\n") {
        return Err(bad_corpus("the last line has no LF"));
    }
    let answers = search(&docs, &dict, &queries)?;
    let mut out =
        io::BufWriter::with_capacity(1 << 20, std::fs::File::create(out_dir.join("results.txt"))?);
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
