# Track B: Index

Team Aflatoon: Dhruv Sharma (@Spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Result

Final: v13 (S 1,491, D 1, no crates). Board: run-12996e3a24f3 gave 88.5 ms, 38.3 MB, K 36.28 and run-3d4480a45f74 gave 95.7 ms, 38.1 MB, K 34.89; the v12 production runs in the same two sessions took 88.8 and 89.8 ms, so v13 tied once and was 6.6% slower once (v12's own runs span 87.3 to 99.0 ms). The box shows no speed gain from the size cut; the gain is in S. The step before it, v12, ran on the
board at 88.9 ms, 37.7 MB, K 36.03, 77.83 points (run-fe126b463988); a second run on a busy box,
run-98bc3fd89798, gave 99.0 ms, K 34.12. In three paired sessions v12 beat v11 by 3.6% (mean
89.2 against 92.5 ms, faster in 3 of 3 pairs). That is about 630 times faster than the
reference (56.35 s) in about a fifty-eighth of its memory (2,200 MB).

What the last two steps are:

- **v13** is v12 with a behaviour-preserving size cut: S 1,540 to 1,491 (1,531 to 1,482 without the
  huge-page table), cachegrind instructions -1.2% (4,066.5 M to 4,018.9 M), branch mispredicts
  16.08 M to 15.21 M, output identical. `Answer` has one shared add path, evaluator steps are
  `(operator, term)` pairs, the prefetch pair is one function, and the counting sort uses the list
  bounds as its cursors. Some of these ideas come from the public AVENGERS and Cache Me entries
  (see PERF.md), written here in our own code.
- **v12** is v11 with the term table on 2 MiB huge pages: mapped 2 MiB aligned, advised for huge
  pages and populated by the main thread before transparent huge pages are switched off for the
  rest of the process. Every token probes this table, and on 4 KiB pages the probes missed the
  TLB. Our earlier huge-page test (PERF.md step 18) did nothing because our own `PR_SET_THP_DISABLE`
  rule had switched huge pages off for the whole process.
- **v11** is v8 (91.2 and 90.7 ms, K 35.30 and 35.40, S 1,618; run-9678ee16f487 and
  run-1bab1a0031d4) with 87 fewer lines and the same speed (board 92.3 and 91.4 ms, run-39e71acbc73a
  and run-98ab40bbff0b). v8 is v6 with finer bitmaps (terms in more than 1/64 of a segment's
  documents), 14 MB segments and a two-stage eval prefetch. v6 is v5.1 (113.6 ms, K 31.43,
  run-438bfabbceeb) with cheaper inner loops and 25% fewer instructions (95.2 ms, K 34.17,
  run-46aaabc13772). The design underneath is v4's, with v5's much cheaper per-token scan
  (v4.2 took 186.0-192.3 ms at 36.1-36.6 MB, K 25.1-25.5); the output is byte-identical to
  v4.2's. The steps that got here, including the builds we rejected, are in [PERF.md](PERF.md).

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
only its terms. The main thread maps the file, counts its words and makes the term table
alone, before any worker exists (see Start-up); then twelve threads parse line-aligned ranges of
`queries.txt` and insert every word into the table at the same time. The table is open
addressing over buckets of three slots, each bucket one 64-byte cache line (three 16-byte keys
and four 32-bit values). A thread claims an empty slot with a compare-and-swap on the first word
of its key, so no locks are needed. A term's value holds its id (22 bits), how many query words
are the term (7 bits, saturating at 63) and one flag: the term anchors a phrase (whether it is
in a BOOL query is a byte in a per-id array). Ids are block-dense: each thread takes 1,024 at a
time from a shared counter and hands them out in order, so an id costs no shared counter per
term and each thread leaves at most one block partly unused.

**Table size.** There is a slot for every query word: the bucket count is the word count over
three, rounded up to a power of two (at least 1,024), so the table can never fill up, whatever
the input. On seed 1 there are 152,904 query words (75,747 distinct terms), so 50,968 buckets
rounded up to 65,536, 64 bytes each: 4 MiB, exactly two 2 MiB huge pages, with 38% of the slots
used. 2,341 buckets (3.6%) are full and marked `OVERFLOW`, and 430,000 of the scan's 36.3 M tokens
(1.2%, measured with a temporary counter) land in one and take the slow path. Any input of 98,305
to 196,608 words gets the same size.

**Huge pages.** The buckets come from one anonymous `mmap`, rounded to a 2 MiB boundary, then
`madvise(MADV_HUGEPAGE)` and `madvise(MADV_POPULATE_WRITE)`: the kernel maps and zeroes all of it in
one call, far cheaper than twelve threads faulting it in a page at a time, and, where it grants huge
pages, the table needs two TLB entries instead of 1,024. Right after the table is made, `main` calls
`prctl(PR_SET_THP_DISABLE)`, which switches transparent huge pages off for the rest of the
process, so partly used 2 MB heap regions are not backed (and counted in the memory peak) as whole
huge pages. Both calls are hints: if the kernel grants no huge pages, or lacks
`MADV_POPULATE_WRITE` (before Linux 5.14), the table is the same with 4 KiB pages. The per-id arrays
are plain anonymous mappings, faulted in only where ids are.

**Term keys and the probe.** A token of up to 16 bytes is its own key: its bytes `| 0x20`
(which lowercases letters and leaves digits alone), zero padded, so no two tokens share a key.
A longer token keeps its first 15 bytes and puts `0x80 | length` in byte 15, which no short
key has; two long tokens with the same key are told apart by comparing the rest of their
bytes against `queries.txt` (the slow path, `find_slow`). The hash is one CRC-32C instruction on the key's first 12 bytes. Looking up a short token is branch-free: two AVX2 compares of the key
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
becomes three bitmasks (token bytes, LFs and TABs), classified from the high and low nibble of
each byte with two `vpshufb` table lookups. Every line starts
with "id TAB", so a prefix XOR of the LF and TAB bits marks exactly those bytes and the ids
never become tokens; the XOR is skipped for the 94% of blocks with no LF or TAB. Token starts
and ends go into one alternating list of bounds (`flatten` writes the positions of the set bits
eight to a byte, for all 64 positions of a block, with no branches). At each LF the lexer counts the tokens before it, so a
document's end is a token index and the scan needs no per-token boundary test.

**The token loop** (`Scanner::region`) works on a region of whole lines, 64 tokens at a time.
Pass A makes each token's key in an XMM register (load 16 bytes, or 0x20, and with a length mask)
and its home bucket, and prefetches that bucket, so the cache misses of rare words overlap. Pass
B looks the tokens up (`Lookup::find`; the table's buckets and mask are copied into a `Lookup`
value, so the loop keeps them in registers). The table values of the whole region's tokens go
into one array, and only then is each document recorded (`Doc::finish`), in a separate loop with
its counters in locals and unchecked stores behind one reservation per document. Recording is
branch-free: every id is written to `fresh` and kept only if the id's stamp is older than the
document's. A stamp is a `u8` per id that holds the number of the last document that saw the id,
and ids that are no BOOL term keep `NEVER`, so they never look new; the stamps of BOOL terms are
cleared only when the 254 numbers run out. At the end of the document `anchors()` reads the
table values' `ANCHOR` bit eight at a time to find the tokens that anchor a phrase, so there is
no separate list of hits. A chunk's distinct BOOL terms are kept in one list and grouped by
partition in a single counting pass at the end of the chunk, rather than pushed into 64 streams
a document at a time.

**Postings.** The corpus is cut into segments of about 14 MB, and documents are numbered from
0 within a segment as `u16`. The scan of each 512 KB chunk leaves its documents' distinct BOOL
terms as `doc << 16 | id / 64` in 64 streams, chosen by `id % 64`. One build task per stream
counting-sorts it into a sorted list per term and adds a bitmap for terms found in more than 1/64
of the segment's documents. Everything a build task touches is about 155 KB on seed 1: its partition's 75 KB of
stream, 37 KB of lists, about 21 KB of two per-term arrays (list bounds, bitmap index) and 21 KB of
bitmaps. The counting sort moves the list bounds themselves as its cursors and shifts them back
afterwards, so it needs no cursor array.

**BOOL queries** are answered once per segment, strictly left to right. A query is skipped for
a segment where it provably matches nothing: a term ANDed after the last OR has no documents there,
or none of its ORed terms does. The first term's set
is borrowed until a second set has to be combined with it. AND and NOT against a dense term
test its bitmap. Between two lists, one is spread into a small bitmap and the other tested
against it, which is about three times faster than a merge (whose steps depend on each
other); when one list is 16 times longer, the short one gallops through it instead, and a
union of such lists gallops too. A large OR goes to a bitmap. Filtering a list against a bitmap takes
eight documents a step, with an AVX2 gather and a shuffle that packs the survivors. Filtering
and setting bits index unchecked after one check of a list's last element, and the evaluator
prefetches in two stages: the term entries of the query eight places ahead, and the first
lines of its lists and bitmaps four places ahead. Answers add up across segments in id order: count and sum add,
and the ten smallest ids are the first ten that arrive. The count and the sum of the ids in a bitmap
come from nibble tables, four words at a time, rather than a visit to every bit.

**One task queue.** Scanning, building and answering are tasks taken in the order
`scan 0, scan 1, build 0, eval 0, scan 2, build 1, eval 1, ...`, with eval split into two tasks per query range, and two formatting tasks per range at the end. A task waits only for the tasks whose output it reads, first spinning
on `PAUSE` (which leaves the core to the other hardware thread) and then yielding. The streams
are double buffered by segment parity, so threads go on scanning the next segment while the
last one is built and answered, and no thread idles at a phase boundary.

**Start-up.** `main` maps `queries.txt` read-only (`MAP_POPULATE`, over a zeroed page that
supplies the padding), so there is no copy into a buffer, counts its words, makes the table
with its huge pages and only then calls `prctl`. After that it starts the twelve workers. They
parse their ranges, insert their words and, after a barrier, choose the phrase anchors, while the main
thread plans the segments (line starts found with a few small reads); a second barrier makes every
worker's queries visible to the others before the first scan. (`Setup` still holds the table in a
`OnceLock` that `run` waits on; since `make_table` runs before the workers exist the wait returns
at once, a remnant of an earlier order in which workers parsed while the table was made.) The
per-term use count lives in the term's value word, so there is no shared per-id frequency array for
the threads to fight over. A worker's scratch is not freed at exit, because each unmap would flush
every core's TLB and the process is about to end.

An input the program cannot handle ends the run with a message on stderr and a non-zero
status (see Errors). `unsafe` appears only in the hand-declared `prctl`, `madvise` and `mmap`, the prefetch
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

Most errors go through one function: one line on stderr, prefixed `track-b: `, and exit code 1.

- a missing or unreadable input file, with its path (`docs.txt` must also end with an LF), a
  failed read of `docs.txt`, a `results.txt` that cannot be created or written, a `queries.txt`
  that cannot be mapped;
- a malformed query line (printed quoted), including a phrase of more than 255 terms or a term
  that is not 1 to 32 lowercase letters and digits;
- a docs.txt line without an id, `no document id at byte N of docs.txt`, or ids out of order,
  `document ids out of order`.

A few checks are assertions. The build uses `panic = "abort"`, so they print the panic message and
abort instead of exiting with code 1: `queries.txt` of 1 GB or more, a term table that cannot be
mapped, too many query words (`too many query words: N`), and a docs.txt line without a TAB
(`a line from document N on lacks a TAB`). A `results.txt` created before an error is left as it
was; the program does not remove it.

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
cases. Seed 1 was run 300 times in one container with no differing output. v13 (the final build) passes the 23 cases and seeds 1 and 7 byte for byte, the 23 cases also in the three stress builds and the 8-value-hash build, and seed 1 in two of the stress builds. From v7 the gate also runs `valgrind --tool=none --fair-sched=yes` (5 runs on 30 MB and the 46 fuzz runs): it reproduces a lock-order deadlock between the two format tasks of a range that native runs almost never hit (PERF.md step 19).

## Dependencies

None. The Rust standard library only. `prctl`, `madvise` and `mmap` are declared by hand with
`extern "C"`. The build needs AVX2 (`.cargo/config.toml` sets `x86-64-v3`, which also gives the
BMI2 and CRC-32C instructions the probe uses) and refuses to compile without it, and links the
binary statically so that a run never pays for the dynamic loader.

## With another 24 hours

- The start-up before the first scan is still about 4.4 to 4.7 ms locally (instrumented): two
  barriers, a table filled by all threads, and the inserts' cache misses. It is bound by twelve
  threads contending on the same table lines and page faults while they parse and insert, not
  by spawning them: a variant that started the workers first (`experiment/b-startup`,
  dropped) started the first worker earlier but not the first scan, cost 36 lines of S, and conflicted with building
  the huge-page table on the main thread first. Letting the first scan tasks read and lex the
  corpus (which needs no table) while the table is filled would hide most of it, and is the
  one start-up idea we did not try.
- We never measured Track B's phases on the box, only locally; where the 21 to 25 ms between our
  89 ms and the leaders' 64-68 ms goes there is a guess from instruction counts (lookup 35%,
  finish 15%, lex 14%, PERF.md), and no single idea we found closes it.
- CPU time is about 60% scan, 25% eval and 10% build (v6, local), and eval's cost is mostly
  the glue of each query-segment pair (about 250 instructions a pair in `run` itself, PERF.md
  step 20); no single piece is large any more, so the next gains are in several small ones,
  each measured on the box. Answering every BOOL query once over a global index would remove
  the per-pair glue but needs 51-53 MiB, against a 44 MB cap (PERF.md step 22).
- Writing postings costs about 4 ns per (term, document) pair (measured on v4.2). Setting the
  commonest terms' bits directly in the scan lost on the board (197 ms, run-ff23bf5cc8f4: two
  more writes per token cost what the skipped postings saved), so the attempt that worked made the
  per-pair write cheaper: v6 groups a chunk's postings by partition in one counting pass
  instead of pushing each pair into its stream (PERF.md step 18).
