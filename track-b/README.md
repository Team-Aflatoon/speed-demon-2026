# Track B: Index

Team Aflatoon: Dhruv Sharma (@Spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Result

Board, v5.1 (commit 1f63325, PERF.md step 17): **113.6 ms, 36.3 MB, K 31.43, 74.87 points**
(run-438bfabbceeb), S 1,689. v5.1 is v5 (125.9 ms, K 30.21, run-407d7d7d6411) with 12 MB segments,
a shorter setup and leaner token and phrase paths; see PERF.md step 17. The figures below that
refer to v5 are kept as measured. That is about 448 times faster than the reference (56.35 s) in about a
sixtieth of its memory (2,200 MB). The previous build, v4.2, took 186.0-192.3 ms at 36.1-36.6 MB
(K 25.1-25.5); v5 has the same design with a much cheaper per-token scan and writes
byte-identical output. The steps that got here are in [PERF.md](PERF.md).

| File | What it holds |
|---|---|
| `src/main.rs` | Segment planning, the task queue and the worker loop, output |
| `src/query.rs` | Query parsing, the lock-free bucketed term table and its probe, phrase anchors, start-up |
| `src/scan.rs` | Reading and lexing `docs.txt`, the per-token loop, postings streams, phrase checks |
| `src/index.rs` | Postings per segment, BOOL evaluation, answers |

`track-b [data dir] [out dir]` reads `queries.txt` and `docs.txt` from the data directory
(default `/data`) and writes `results.txt` to the output directory (default `/out`).

## Approach

The queries are part of the input, so the program reads `queries.txt` first and indexes
only its terms. Twelve threads parse line-aligned ranges of `queries.txt` and insert every word
into one open-addressing hash table at the same time. The table is made of buckets of three
slots, each bucket one 64-byte cache line (three 16-byte keys and four 32-bit values). A thread
claims an empty slot with a compare-and-swap on the first word of its key, so no locks are
needed. A term's value holds its id (22 bits), how many query words are the term (7 bits,
saturating at 63) and two flags: the term appears in a BOOL query; the term anchors a phrase.
Ids are block-dense: each thread takes 1,024 at a time from a shared counter and hands them out
in order, so an id costs no shared counter per term and each thread leaves at most one block
partly unused. The table has a slot for every query word (the bucket count is the word count
over three, rounded up to a power of two, and at least 1,024), so it can never fill up,
whatever the input; on seed 1 it is 4 MiB. The buckets come from `mmap(MAP_POPULATE)`, which
the kernel maps and zeroes in one go; the per-id arrays are plain anonymous mappings, faulted in
only where ids are.

**Term keys and the probe.** A token of up to 16 bytes is its own key: its bytes `| 0x20`
(which lowercases letters and leaves digits alone), zero padded, so no two tokens share a key.
A longer token keeps its first 15 bytes and puts `0x80 | length` in byte 15, which no short
key has; two long tokens with the same key are told apart by comparing the rest of their
bytes against `queries.txt` (the slow path, `find_long`). The hash is CRC-32C of the key
(two instructions). Looking up a short token is branch-free: two AVX2 compares of the key
against the bucket's three keys, a 64-entry table (`SLOT`) from the match bits to the slot, and
one load; a miss reads the bucket's fourth value, 0 for "no term". That value carries
`OVERFLOW` when some term had to move on to a later bucket, and only a miss in such a bucket
takes the slow path.

**Phrases need no index.** Every phrase gets one anchor term: the one that occurs least often
in `queries.txt`, the longest on a tie. Common words appear in many queries and rare words in
one or two, so this picks rare corpus words without reading the corpus. What the scan pays is
the number of times the anchors occur in `docs.txt` (seed 1):

| Anchor | Occurrences in `docs.txt` |
|---|---|
| The truly rarest term (needs a pass over the corpus) | 0.92 million |
| Fewest occurrences in `queries.txt` (used) | 1.26 million |
| The longest term | 52 million |

So the anchors are within 1.4x of the best possible without reading the corpus. While scanning
a document the program keeps the term id of every token (id 0 for tokens that are no query
term). When the document ends, each phrase anchored at one of its tokens is compared with the
ids around that token. Positions are never stored and no document is read twice.

**Reading and lexing.** `docs.txt` is read with `pread` into a buffer per thread, 64 KB at a time, cut
at the last line end, with the partial line carried to the next read. Each 64-byte block
becomes three bitmasks (token bytes, LFs and TABs) from a few AVX2 compares. Every line starts
with "id TAB", so a prefix XOR of the LF and TAB bits marks exactly those bytes and the ids
never become tokens; the XOR is skipped for the 94% of blocks with no LF or TAB. Token starts
and ends go into one alternating list of bounds (`flatten` writes the positions of the set bits
eight to a byte, for all 64 positions of a block, with no branches). At each LF the lexer counts the tokens before it, so a
document's end is a token index and the scan needs no per-token boundary test.

**The token loop** (`Scanner::region`) takes tokens 64 at a time in two passes. Pass A makes
each token's key in an XMM register (load 16 bytes, or 0x20, and with a length mask) and its
home bucket, and prefetches that bucket, so the cache misses of rare words overlap. Pass B
looks the tokens up and records them in runs that end at a batch or a document boundary
(`Doc::record`), with its counters in locals and unchecked stores behind one capacity assert
per run. Recording is branch-free: every id is written to `fresh` and kept only if it is a
BOOL term not yet seen in the document (a `u8` stamp per id, which holds the number of the last
document that saw it; the stamps are cleared only when the 255 numbers run out). Every token's
id goes into `ids` with the term's `ANCHOR` bit, and at the end of the document `anchors()`
reads that bit eight ids at a time to find the tokens that anchor a phrase, so there is no
separate list of hits. The table's buckets and mask are copied into a
`Lookup` value, so the loop keeps them in registers. A document's `ids` and `fresh`
arrays grow to the longest document, not to the size of a read.

**Postings.** The corpus is cut into segments of about 12 MB, and
documents are numbered from 0 within a segment as `u16`. At the end of each document the scan
writes its distinct BOOL terms as `doc << 16 | id / 64` into one of 64 streams, chosen by
`id % 64`. One build task per stream counting-sorts it into a sorted list per term and adds a
bitmap for terms found in more than 1/16 of the segment's documents. Everything a build task
touches fits in about 100 KB.

**BOOL queries** are answered once per segment, strictly left to right. The first term's set
is borrowed until a second set has to be combined with it. AND and NOT against a dense term
test its bitmap. Between two lists, one is spread into a small bitmap and the other tested
against it, which is about three times faster than a merge (whose steps depend on each
other); when one list is 16 times longer, the short one gallops through it instead, and a
union of such lists gallops too. A large OR goes to a bitmap. Filtering and setting bits index
unchecked after one check of a list's last element, and each query prefetches the term entries
of the query four places ahead. Answers add up across segments in id order: count and sum add,
and the ten smallest ids are the first ten that arrive. The sum of the ids in a bitmap word
comes from six masked popcounts rather than a visit to every bit.

**One task queue.** Scanning, building and answering are tasks taken in the order
`scan 0, scan 1, build 0, eval 0, scan 2, build 1, eval 1, ...`, with one formatting task per
query range at the end. A task waits only for the tasks whose output it reads, first spinning
on `PAUSE` (which leaves the core to the other hardware thread) and then yielding. The streams
are double buffered by segment parity, so threads go on scanning the next segment while the
last one is built and answered, and no thread idles at a phase boundary.

**Start-up.** While the workers parse the queries and fill the table, the main thread plans
the segments (line starts found with a few small reads). `queries.txt` is mapped read-only
(`MAP_POPULATE`, over a zeroed page that supplies the padding), so there is no copy into a
buffer; the per-term use count lives in the term's value word, so there is no shared per-id frequency array for the threads to fight
over. A worker's scratch is not freed at exit, because each unmap would flush every core's TLB
and the process is about to end.

All I/O errors and malformed input end the program with a message on stderr (with the byte
offset, document id or query line where it can tell) and exit code 1, and leave no partial
`results.txt`. `unsafe` appears only in the hand-declared `prctl` and `mmap`, the prefetch
hints, and the SIMD and unchecked accesses of the table probe and the token loop, each with a
`SAFETY` note.

## Limits

- At most 2^22 query words, less 1 and 1,024 per thread (50,000 queries hold at most 400,000).
- Phrases of at most 255 terms (the spec allows 8); query terms of 1 to 32 bytes (corpus tokens
  of any length are handled; those over 16 bytes take the slow path).
- `queries.txt` under 1 GB; `docs.txt` ending with an LF, document ids `0, 1, 2, ...` in order,
  each line with one TAB.
- At most 12 threads. Tasks that wait for others spin and then `yield_now`, which burns CPU if
  the cgroup grants fewer CPUs than `available_parallelism` reports.
- A term's use count has seven bits; `USES_CAP + MAX_THREADS <= 127` is asserted at compile time.
- Segments are cut to hold at most 65,536 documents, so any number of documents is fine.

## Errors

Each ends the run with exit code 1 and one line on stderr, prefixed `track-b: `:

- a missing or unreadable input file, with its path (`docs.txt` must also end with an LF, and
  `queries.txt` must be under 1 GB);
- a malformed query line (quoted, cut to 80 bytes), or a phrase of more than 255 terms;
- too many query words, with the count and the limit (`too many query words: N, at most M`);
- a docs.txt line without an id, `the line at byte N of docs.txt does not start with a
  document id`; a line without exactly one TAB; ids out of order (`document id N at byte B is
  out of order`, or `document ids must be 0, 1, 2, ... in order` with the byte range of the
  segment); more than 65,536 documents in one segment;
- a term table that cannot be mapped;
- a failed write of `results.txt` (the partial file is removed).

## Testing

Every build was checked against the reference's output on seeds 1 and 7 and the audit seeds 101
and 202, and on 23 small cases: the spec's worked example, every hand-written case of
`tests/test_b_index.py`, generated tiny and sample seeds, and random cases with long tokens
(12, 13, 17, 32 and 33+ bytes), terms sharing prefixes, a line longer than a read buffer, op
words as terms and NOT emptying a set. The 23 (and seed 1) also run in three stress builds that
shrink constants to take every boundary path: `PIECE` (scan.rs) to 1 KB, `CHUNK_BYTES` and
`SEGMENT_BYTES` (main.rs) to 4 KB and 64 KB, and `BATCH` (scan.rs) to 3 tokens; `PIECE` to 4 KB
with `MAX_SEGMENT_DOCS` (scan.rs) cut to 100; and `PIECE` of 1 KB with 1-token batches. A fourth
build whose hash takes only 8 values, so every bucket overflows into long chains, passes the 23
cases. Seed 1 was run 300 times in one container with no differing output.

## Dependencies

None. The Rust standard library only. `prctl` and `mmap` are declared by hand with
`extern "C"`. The build needs AVX2 (`.cargo/config.toml` sets `x86-64-v3`, which also gives the
BMI2 and CRC-32C instructions the probe uses) and refuses to compile without it, and links the
binary statically so that a run never pays for the dynamic loader.

## With another 24 hours

- The start-up before the scan can begin is about 4.7 ms of wall time locally: two barriers, a table filled by
  all threads, and the inserts' cache misses (prefetching ahead made no difference). Letting the first scan tasks read and lex the corpus (which needs no table)
  while the table is filled would hide most of it.
- Lookup and record are about 25-29% of the CPU time, eval about 15%, finish (emission and
  phrase checks) about 10% and the lexer 9-13%; no single piece is large any more, so the
  next gains are in several small ones, each measured on the box.
- Writing postings costs about 4 ns per (term, document) pair (measured on v4.2). Setting the
  commonest terms' bits directly in the scan lost on the board (197 ms, run-ff23bf5cc8f4: two
  more writes per token cost what the skipped postings saved), so the next attempt would make
  the per-pair write itself cheaper: one bulk copy per document into each stream instead of a
  push per pair.
