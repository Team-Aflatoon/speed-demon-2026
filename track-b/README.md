# Track B: Index

Team Aflatoon: Dhruv Sharma (@spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

The queries are part of the input, so we never build a general index. The program reads
`queries.txt` first and puts every distinct query term (about 76,000) into one
open-addressing hash table with 16-byte slots (the first 12 bytes of the term inline, its id
and length packed into one word). Any corpus token that is not in that table is dropped the moment
it is seen, and that covers most of the 600,000-word vocabulary.

`docs.txt` is memory-mapped. The harness has already loaded it into the page cache, so the
mapping adds nothing to our memory peak. The file is cut into 3 MB blocks of whole lines, and
up to 12 worker threads claim blocks from an atomic counter. A worker handles a block in
batches of up to 4,096 documents. For each batch it builds a small inverted index of the query
terms, answers all 50,000 queries against it, and folds each non-empty answer into that query's
running total. Count and sum just add up, and the ten smallest ids merge like two sorted lists.
So the index never spans more than one batch, and its memory stays at a few hundred kilobytes
per thread whatever the corpus size.

**Tokenizing.** Each 64-byte chunk becomes two bitmasks (token bytes and LFs) from a few AVX2
compares. Token starts are `alnum & !(alnum << 1)`, and the lexer visits starts and line ends
with trailing-zero counts, so it branches once per token instead of once per byte. A token is
lowercased by a single `| 0x20` on its first 16 bytes, loaded as two `u64`s. That works because
every token byte is a letter or a digit, and digits already have that bit set. The two words
are the hash key.

**Parsing the queries** is serial, so it is two passes: parse every line, then insert the
words into a term table pre-sized from the word count, each insert prefetching the slot of the
word 16 places ahead.

**Hiding memory latency.** Most lookups of rare words miss the cache. A document is therefore
handled in three passes: hash every token and prefetch its table slot, resolve the tokens to
term ids and prefetch each term's state word, then record the terms. About 150 lookups per
document overlap instead of queueing one after another.

**Postings.** In a batch, a rare term keeps a sorted list of local document numbers (`u16`),
built by a counting sort of the batch's (document, term) hits. A term that once appears in more
than 64 of every 4,096 documents of a batch becomes "dense" for that worker. From then on the
scan sets its bit in a 512-byte bitmap directly, with no duplicate check and no list.

A worker's first batch is a 512-document warm-up, so that twelve first batches, in which every
term is still a list, don't set the memory peak. During the scan, dense bits go into a
one-word-per-term stripe for the current 64 documents, which stays in L1. It is flushed into the
bitmaps every 64 documents: one write per dense term instead of one cache line per hit.

**BOOL queries** run strictly left to right as a small state machine. A one-term query, or
`x AND <absent>`, only borrows the term's set; a combined result lives in a scratch bitmap,
where OR and NOT with a list set or clear its bits directly and AND goes through a mask.
A 10 KB per-batch presence bitset answers "is this term in the batch at all?" from L1, which is
the usual case for rare terms. The sum of the ids in a bitmap word is computed without visiting
the bits: bit k of a position is set exactly under the mask `0xAAAA..`, `0xCCCC..`, `0xF0F0..`
and so on, so the sum is `Σ popcount(w & mask_k) << k`.

**PHRASE queries** first AND their terms to get the candidate documents. In each candidate the
program then looks for whole-token occurrences of the phrase's longest term (usually its rare
anchor), using an AVX2 test of each position's first and last byte, 32 positions at a time. From
each occurrence it reads tokens outwards, only as far as the phrase reaches. No positions are
stored.

All I/O errors and malformed input end the program with a message on stderr and exit code 1.
`unsafe` appears only in the hand-declared `mmap`/`munmap`/`prctl`, the prefetch hint and the AVX2
loads, each with a `SAFETY` note.

## Dependencies

None. The Rust standard library only. `mmap`, `munmap` and `prctl` are declared by hand with
`extern "C"`. The build needs AVX2 (`.cargo/config.toml` sets `x86-64-v3`) and refuses to
compile without it.

## With another 24 hours

- The term table lookups were the largest single cost, at about 28 ms per thread, when we
  measured them with the earlier 24-byte slots. The 16-byte slots helped only within noise, so
  that cost is probably still there. A small separate table for the few hundred hottest terms is
  the next thing to try.
- Two ideas measured worse for us and are logged in PERF.md: global term classes from a sample,
  and a positional index for phrase anchors. Each would need a cheaper build step to pay off.
- Fewer lines: the evaluator's match arms grow a lot under rustfmt.
