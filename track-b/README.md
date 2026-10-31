# Track B: Index

Team Aflatoon: Dhruv Sharma (@spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

The queries are part of the input, so the program never builds a general index. It reads
`queries.txt` first and puts every distinct query term (about 76,000) into one
open-addressing hash table. Any corpus token that is not in that table is dropped the moment
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

**Hiding memory latency.** Most lookups of rare words miss the cache. A document is therefore
handled in three passes: hash every token and prefetch its table slot, resolve the tokens to
term ids and prefetch each term's state word, then record the terms. About 150 lookups per
document overlap instead of queueing one after another.

**Postings.** In a batch, a rare term keeps a sorted list of local document numbers (`u16`),
built by a counting sort of the batch's (document, term) hits. A term that once appears in more
than 64 documents of a batch becomes "dense" for that worker. From then on the scan sets its bit
in a 512-byte bitmap directly, with no duplicate check and no list. During the scan the dense
bitmaps are stored transposed (all dense terms' words for the same 64 documents sit side by
side), so a document's writes stay in L1. They are transposed back once per batch.

**BOOL queries** run strictly left to right as a small state machine. A one-term query, or
`x AND <absent>`, only borrows the term's set. A combined result lives in scratch space as a
sorted list while it is small (ANDing with a list can only shrink it) and as a bitmap otherwise.
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
`unsafe` appears only in the hand-declared `mmap`/`munmap`, the prefetch hint and the AVX2
loads, each with a `SAFETY` note.

## Dependencies

None. The Rust standard library only. `mmap` is declared by hand with `extern "C"`.

## With another 24 hours

- The serial query parse (about 17 ms of a 200 ms run locally) could overlap with the corpus
  scan, or the term table could be built in parallel.
- The per-thread term arrays (about 600 KB per thread) are the next memory cost. Picking dense
  terms globally from a sample would let the dense flag live in the shared hash slot and save a
  random access per token.
- Batch-level work for phrases could use stored anchor offsets instead of searching candidate
  documents.
- Fewer lines: the evaluator's match arms grow a lot under rustfmt.
