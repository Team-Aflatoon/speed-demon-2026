# Track B: performance log

## Summary

**Final: v4.2 (step 13).** Board: 189.6 ms, 36.6 MB, K 25.25 (run-d4d593bc34cf); the
reference takes 56.35 s and 2.2 GB.

Decisive steps (all board runs):

1. **v1:** index only the query terms; a throwaway index per 4,096 documents, all queries
   answered per batch: 309 ms, 59.7 MB (run-7450e3d4343b).
2. **v2-v3:** memory, not time (THP off, bitmap stripes): 59.7 → 39.1 MB at ~303 ms
   (run-43f854129993, run-d53081aa4120). Local CPU gains of 5-10% never showed on the box.
3. **v4 redesign:** phrases checked during the scan at a rare anchor chosen by query frequency
   (no positions, no re-reads); postings built once per 8 MB segment; one task queue: 184.3 ms,
   35.8 MB (run-64a666bd15fb).
4. **v4.1/v4.2:** every capacity provable for any valid input; table size changed nothing
   (run-0778bc8cbf5f vs run-1ca61642aede).

Lesson: ten tuning steps moved the board 3%; removing repeated work (84 query passes,
candidate re-reads) moved it 39%. Every Track B board run is a correct build; no probes were
run.

## Steps

Rows with a Run ID are board measurements; row 0 is the board's reference figures. Every other
T and M is **local**: `bench/bench.sh b 5` (the harness's sandbox flags, cpuset 0-11, warm page
cache, cgroup `memory.peak`) on an i7-14650HX laptop, with T the mean of the middle three of five
runs. The laptop was shared with other jobs, so its absolute timings drift by ±15% or more from
hour to hour. Every decision under about 10% was therefore made from interleaved A/B runs of the
two builds (median of 7 to 9 runs each, plus the cgroup's CPU time), and those numbers are given
per step. Compare a step's A/B with its parent, not its local T with another row's.

Points are 50 × log10(K), which reproduces the board's figures (K 25.45 is 70.29 points, K 25.25
is 70.11, up to the rounding of K). Rows 7 and 10 ("≈ … (computed)") and row 12 (from its board K
of 25.26) were worked out from K; the other points are read off the board.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (board reference figures) | | 56.35 | 2,200 | 0 |
| 1 | Rust, no crates. Query terms only, in one hash table. Corpus memory-mapped and cut into 3 MB blocks for 12 threads. A 4,096-document index per batch (sparse `u16` lists, plus bitmaps for terms seen in more than 64 documents), every query answered per batch, answers merged under a per-query mutex. Phrases verified by re-tokenizing each candidate document. | | 0.325 (local) | 58 (local) | |
| 2 | Phrases verified around occurrences of the longest term instead of re-tokenizing the whole document (phrase work was about 100 ms of 300 per thread). Per-batch presence bitset, so absent terms resolve from L1. Query steps in one flat array. Token lookups hashed and prefetched one document at a time, then resolved. | | 0.235 (local) | 55 (local) | |
| 3 | Bitmask lexer: 64 bytes per step with AVX2, events visited by trailing-zero count instead of a byte-at-a-time loop that mispredicted at every token boundary. AVX2 first/last-byte search for phrase anchors. One state word per term (dense slot or last document) instead of two arrays. | | 0.200 (local) | 55 (local) | |
| 4 | Dense bitmaps written transposed during the scan (one L1-resident stretch per 64 documents) and transposed back per batch. Term state words prefetched between resolving and recording. Small BOOL results kept as sorted lists. A/B against step 3: 176 vs 185 ms. **This is v1.** | run-7450e3d4343b (rerun run-67ffb0644a3e: 0.3061 s) | 0.3091 | 59.7 | 64.3 |
| 5 | A worker's first batch is a 512-document warm-up. Until a worker has seen a batch, every term is still a list, and twelve full all-list batches at once set the memory peak; the promotion threshold scales with the batch size. Output buffer cut from 1 MB to 64 KB. A/B: 183 vs 198 ms, CPU 1,855 vs 1,925 ms, M 50.9 vs 56.1 MiB. Transparent huge pages were then switched off with `prctl`. On the board, v2 and v3 show box M ≈ local M, so THP was not inflating Track B's memory much (unlike Tracks A and D). | | 0.183 (local) | 51 (local) | |
| 6 | Query parse in two passes: parse every line first, then insert the 155,000 words into a table pre-sized from the word count, prefetching the slot of the word 16 places ahead. The serial parse had been waiting on a cache miss per insert and per rehash. A/B: 179.5 vs 183.9 ms, CPU 1,800 vs 1,845 ms. | | 0.180 (local) | 52 (local) | |
| 7 | BOOL accumulator simplified back to borrowed term or scratch bitmap: OR/NOT apply a list's bits directly, AND spreads it into a mask. It is faster than the step-4 sorted-list accumulator and 50 lines shorter. A/B: 184 vs 192 ms, CPU 1,832 vs 1,888 ms. **This is v2.** | run-43f854129993 | 0.3157 | 52.5 | ≈ 64.7 (computed) |
| 8 | Term table slots shrunk from 24 to 16 bytes (first 12 bytes of the term inline, id and length packed into one word; longer terms are confirmed against their text), so four slots share a cache line and the table is 2 MB instead of 3. A/B against v2: 207.8 vs 215.0 ms, CPU 1,986 vs 2,008 ms, M 0.7 MiB lower: a small win, within noise. (The local T of 0.20 looks worse than step 7's only because it was measured on a busier machine.) | | 0.20 (local) | 51 (local) | |
| 9 | Dense bits written into a one-word-per-term stripe for the current 64 documents and flushed into the bitmaps every 64 documents, instead of into a full transposed copy of every bitmap. It stays as L1-friendly as step 4, drops a ~770 KB buffer per thread and the per-batch transpose. A/B: M 38.7 vs 50.9 MiB, 194 vs 202 ms median, CPU 1,956 vs 1,897 ms (within noise). M is now under the 44 MB cap of the memory axis. | | 0.19 (local) | 40 (local) | |
| 10 | Line count, no behaviour change: `query.rs` folded into `main.rs`, `Answer::merge` reduced to a sort of at most 20 ids. S down 7 lines, to 993 as the board counts it. **This is v3.** | run-d53081aa4120 (reruns run-4f979d5ac144: 0.3021 s, 39.7 MB; run-a5f9c82b0e45: 0.3070 s) | 0.3035 | 39.1 | ≈ 65.9 (computed) |
| 11 | Clean-room redesign, built separately and adopted whole (see "v4" below): phrases checked during the scan against each document's token ids, so they need no index; postings built once per 8 MB segment by partition instead of a throwaway index per 4,096 documents; BOOL queries answered once per segment instead of once per batch; corpus read with `pread`; parallel query parse into a lock-free table. Head-to-head on the board: T 184.3 vs ~302 ms, K 25.45 vs 20.87. S rose from 993 to 1,364. **This is v4.** | run-64a666bd15fb | 0.1843 | 35.8 | 70.29 |
| 12 | Provable safety for any valid input (see "v4.1" below): the term table is sized from a distinct-term estimate but falls back to a table with room for every word when an insert runs out of probes, so it can no longer fill up; segments are bisected so that none holds more than 65,536 documents. Also: inserts prefetch 8 words ahead, the table comes from `calloc`ed memory (the estimate and `calloc` were dropped again in step 13), and S fell from 1,364 to 1,347. Board: K 25.26, within noise of v4. A/B on the board with a table half the size (run-0778bc8cbf5f: 0.1932 s, 35.3 MB, K 25.09): also within noise. **This is v4.1.** | run-1ca61642aede | 0.1906 | 37.0 | 70.12 |
| 13 | Clarity: v4.1's distinct-term estimate, probe limit and fallback table are dropped. The board showed table size is not a lever, and on seed 1 the estimate produced the same 262,144-slot table as sizing for every query word to be distinct, which needs no fallback because it can never fill up. With the estimate went v4.1's `calloc`: the slots are again filled by one thread before the inserts, with no board difference (189.6 vs 190.6 ms). Query start-up moved into `query::Setup`. S 1,347 → 1,317, K 25.25. After this run the binary was linked statically (`+crt-static`, as in Tracks A and C; same output on seeds 1 and 7 and all 23 fuzz cases), comments, names and error messages were revised, and a simplification pass took S from 1,316 to 1,269 (the insert prefetch of step 12, neutral on the board, went with it; output byte-identical on seeds 1, 7, 101 and 202, the 23 fuzz cases and three stress builds; local bench unchanged). **This is v4.2, the final version.** | run-d4d593bc34cf | 0.1896 | 36.6 | 70.11 |

**What the board says.** Board T stayed at 302-316 ms from v1 to v3: the local gains of
steps 5-10 (a few percent each in A/B) are within the board's run-to-run spread. The board win
was memory: 59.7 MB → 39.1 MB, which puts the memory axis at its cap.

v2 against v1, interleaved on a quiet machine (9 runs each): 181.3 vs 183.2 ms median, CPU 1,815
vs 1,808 ms, M 51.9 vs 56.4 MiB. v3 against v2, interleaved (9 runs each, busy machine): 223 vs
239 ms median, CPU 2,215 vs 2,361 ms, M 38.6 vs 51.4 MiB.

We measured memory before cutting it. The cgroup's `memory.stat` after the run shows about 48 MB
anonymous, about 1.3 MB kernel, 0.8 MB page tables and **0 bytes of file pages**, even after
`results.txt` is written. So O_DIRECT output would not lower the peak, and we did not do it. The
per-worker `Index` buffers, printed from an instrumented copy, came to about 3 MB per thread. The
largest was the transposed scan buffer, which step 9 removed. Once M was under 44 MB the
remaining ideas (a leaner per-query answer instead of 50,000 mutexes, smaller per-thread term
arrays) could no longer raise the score, so we left them out.

## Where the time goes (measured on v2, local, per worker thread)

These figures predate steps 8-10. Step 8 changes the lookup cost a little and step 9 the
recording cost, both within noise.

About 95 ms building batch indexes and 50 ms answering queries (roughly half BOOL, half phrase
checks), after the serial query parse (16 ms in v1, less after step 6; not separately re-timed).
In the build: about 34 ms lexing and hashing, 28 ms resolving lookups in the term table (then
3 MB with 24-byte slots), and 27 ms recording hits. We measured these by switching off each
stage in a copy of the program.

## Experiments that did not help

- **8,192-document batches** (half the passes over the 50,000 queries). Query time did not move
  (52 vs 50 ms per thread) and M rose from 55 to 75 MB. Query cost follows the documents, not the
  number of passes. This is also our main evidence against one global index evaluated once per
  query: it would save per-pass overhead, and that overhead is small. It would also need about
  25 MB of compressed postings for the 22 million (term, document) pairs. We did not build it.
- **Global term classes from a parallel sample** (v2 attempt). Twelve threads read 64 KB each,
  estimate document frequencies, and the terms are renumbered so dense terms are ids `0..D`. That
  removes per-thread promotion and the state lookup for dense hits. It was 15% slower: CPU 2,156
  vs 1,838 ms, wall 218 vs 187 ms. The sampling round cost 6-9 ms of wall time and the build got
  slower too. A lower dense threshold (1/32 instead of 1/64) was slower again. Reverted.
- **A positional index for phrase anchors** (v2 attempt). Byte offsets of every occurrence of
  rare phrase terms, with phrases checked from the rarest term's offsets instead of searching
  candidate documents.
  - First version: M jumped to 105 MB, because in a worker's first batch every phrase term (even
    "the") is still rare.
  - Restricted to terms in at most 16 documents of the previous batch: M was still +11 MB (about
    80,000 offsets per batch) and CPU +7%, with the build cost cancelling the phrase saving.
  - Along the way, finding each occurrence's document by a linear walk from document 0 made it
    much slower until we replaced it with a binary search.
  - Reverted.
- **Non-temporal prefetch of the corpus** ahead of the lexer, to keep it from evicting the term
  table out of L3: 243 vs 247 ms (A/B), within noise. Reverted.
- **Bitmap-only accumulator that spreads every list into a mask** (simplest code): 4% more CPU
  than setting OR/NOT bits directly (2,020 vs 1,938 ms). Step 7 keeps the direct path.
- **A separate small table for the hottest terms** in front of the main term table was not
  attempted: the 16-byte slots alone gave only a within-noise gain, which suggests most hot-term
  lookups already hit the cache.
- **Detecting dense terms from an empty list** in `Index::set`: query time went from 52 to 72 ms
  per thread, because dense terms' `bounds` entries are cold. Reverted.
- **Prefetching per-term data for the query 8 places ahead**: no gain (217 vs 208 ms). Reverted.

## Lessons

- Profile first. Re-tokenizing phrase candidates looked cheap on paper (214,000 candidates) and
  turned out to be a third of the run of step 1, our first working version.
- On a shared laptop only interleaved A/B runs, with CPU time next to wall time, are trustworthy
  for anything under about 10%. Single runs swung by up to 2x.
- A few percent locally does not show on the board, whose spread is about as large. Memory, being
  deterministic, moved exactly as measured.
- Ideas that remove work in one place often add it in another: the global classes and the anchor
  offsets were both clean on paper and net losses in measurement.

## v4: clean-room redesign

v1 to v3 plateaued at about 302 ms on the board while the local time kept falling. The design
itself had a ceiling: every 4,096-document batch built a throwaway index and then answered all
50,000 queries against it, about 84 passes over the query set, and phrase candidates were
re-read from the text. v4 was written from scratch in a separate folder and adopted whole once
it beat v3 on the board.

**Board head-to-head:** run-64a666bd15fb, T 184.3 ms, M 35.8 MB, S 1,364, K 25.45, against
v3's ~302 ms, 39.1 MB, S 993, K 20.87. The time gain outweighs the longer code by far: S costs
about 5% of K, T gains about 28%.

What changed, and why:

1. **Phrases are checked during the scan, with no index.** Each phrase gets one anchor: the
   term with the fewest occurrences in `queries.txt` (longest on a tie). Common words appear in
   many queries and rare ones in one or two, so this finds rare words without a pass over the
   corpus. On seed 1 the anchors occur 1.26 M times in `docs.txt`, against 0.92 M for the true
   rarest terms (measured offline) and 52 M for v3's "longest term". The scan keeps every
   token's term id; at the end of a document each phrase anchored there is compared with the
   ids around the anchor. v3's route, candidates from postings and then the text, would be
   214,000 candidate documents re-read, a `pread` each now that the corpus is not mapped.
2. **`pread` into 128 KB per-thread buffers instead of `mmap`.** Track A measured this 15%
   faster on the box (its page-fault path is slow); locally the two were the same.
3. **Postings built once per 8 MB segment, by partition.** Each document's distinct BOOL terms
   go into 64 streams by `id % 64`; one build task per stream counting-sorts it into `u16`
   lists, plus bitmaps for terms in more than 1/16 of the segment. Each build task touches about
   100 KB.
4. **BOOL queries answered once per segment** (32 segments instead of ~84 batches), against
   real lists: AND/NOT against a dense term test its bitmap; two lists are combined by
   spreading one into an L1 mask and testing the other; gallop when one is 16x longer.
5. **One task queue instead of phase barriers**, with chunk streams double buffered by
   segment parity, so the next segment is scanned while one is built and answered. Waiting
   is about 1% of CPU time.
6. **Parallel query parse into a lock-free table** (CAS on a slot's first word) with
   block-dense ids: each thread takes 1,024 at a time from a shared counter.

### Steps (local)

Local figures are from `bench/bench.sh b` or a dev runner with the same cpuset; v3 measured
165-175 ms on the bench in the same session.

| Step | Change | Local T | M |
|---|---|---|---|
| a | First correct build, 16 MB segments | 121-127 ms | 54.6 MB |
| b | Scanner arrays grown on demand instead of zero-filled to a worst-case size (9.4 MB across 12 threads); dense term ids instead of table-slot ids (the seen bitset shrinks from 32 to 9.5 KB per thread, per-partition metadata from 2 MB to 0.6 MB) | 118-135 ms | 44.3-44.8 MB |
| c | 8 MB segments: chunk streams and postings halve. Eval cost hardly depends on the segment size (15.7% of CPU at 8 MB, 14.0% at 32 MB) | 125-132 ms | 35.0-36.1 MB |
| d | Eval: no lookups once an AND or NOT has emptied the result; a dense first term filters the list instead of merging with it | eval CPU 257 to 246 ms | |
| e | Eval: AND/NOT of two lists through an L1 mask (`sift`) instead of a branch-free merge, whose steps depend on each other | 103-112 ms; eval CPU 246 to ~190 ms | |
| f | Scan: local slices instead of fields (no reloads of vector headers after every store, fewer bounds checks), token ids per region instead of a per-document vector, a dummy id for misses so recording a token is branch-free | 101-106 ms; scan CPU ~880 to ~820 ms | 35.4-36.1 MB |

CPU split of step f (sum over 12 threads, about 1.2 s): scan ~820 ms, eval ~190, build ~75,
waiting ~10; the parallel query parse takes 5.5-6.4 ms of wall time before the scan starts.
Scan ablation (before step f): read + classify + flatten 165 ms, + key/hash/prefetch 380, +
table lookups 766, + per-document recording 870, + writing the postings 960.

### Experiments that did not help (v4)

- 16, 32 or 128 tokens per prefetch round instead of 64: the same.
- No slot prefetch at all: the same locally (the laptop's out-of-order window hides L3 hits);
  kept for the box, whose L2 is smaller.
- A 2 MB table (load 0.58) instead of 4 MB (load 0.29): the same locally.
- `_MM_HINT_T1` instead of `_MM_HINT_T0`: worse.
- Union-to-bitmap thresholds of 1/32, 1/64 and 1/128 of the documents instead of 1/16: the
  same.
- **Hot terms straight to bitmaps, on the board.** Terms with a bitmap in segment 0 (179 on seed 1) were
  flagged hot: the scan set their bits per 64-document row instead of emitting postings, and the build
  ORed those rows in, cutting postings from ~21.1M to ~13.6M pairs. Correct on every check, but the
  box measured 197.2 ms (run-ff23bf5cc8f4, 38.3 MB, K 24.66) against v4.2's 184-193 ms, and S rose by
  83. The two extra per-token list writes cost about what the skipped postings and sort saved. Dropped.
  The simplified v4.2 itself ran 188.7 ms, 36.0 MB, K 25.45 on the board (run-6a7f77612f9d).
- **Term table size, on the board.** Two v4.1 builds that differ only in the table: room for 1.5
  times the estimated distinct terms (2 MB, load 0.58; run-0778bc8cbf5f, 193.2 ms, 35.3 MB) and
  3 times (4 MB, load 0.29; run-1ca61642aede, 190.6 ms, 37.0 MB). Both are within the board's
  noise of v4 (184.3 ms). The halved table did not help even on the box's 1.25 MB L2, so the
  lookups are not limited by where the table lives; v4.1 keeps the 4 MB table.
- A lookup that settles its first slot with one branch (match or empty) and probes further only
  on a collision: no change in pinned single-thread runs (1,125-1,198 vs 1,080-1,262 ms).
  Reverted.

### v4.1: provable safety

A fixed capacity that only real data cannot exceed is still a failure the holdout could trip,
and a failed run scores zero. Two of v4's could, in principle, end the run on a valid input:

1. **The term table.** The table must not fill up. Sizing it for every query word to be distinct
   always works (that is v4), but a table sized from an estimate of the distinct terms (linear
   counting over a 2^17-bit sketch per thread, accurate to well under 1% at this size) is only
   *almost* always big enough. v4.1 does both: the first table comes from the estimate, every
   insert into it gives up after 64 probes, and if any insert gives up all threads redo the
   inserts into a table with room for every word at a load of at most 3/4. That table always has
   free slots, so its inserts may sweep it whole and always succeed. On the generated data the
   fallback never runs; tests force it. *The estimate and fallback were removed in step 13: the
   final table is always sized for every word, which can never fill up and needs neither.*
2. **Documents per segment.** Documents are numbered from 0 in each segment as `u16`, and v4's
   segments were plain 8 MB byte ranges: one would overflow if its documents averaged under
   128 bytes (the generator's average about 1 KB). v4.1 reads the last line's id to get the
   document count, and bisects any segment holding more than 65,536 documents.

Every other fixed capacity was checked against the input format: a phrase has at most 8 terms
(an anchor position is a byte), 50,000 queries hold at most 400,000 words (ids have 22 bits),
term offsets in `queries.txt` are 32-bit, and tokens longer than 63 bytes are clamped to a
length that no term has. Everything else grows as needed.

### Correctness checks

Besides seeds 1 and 7, 23 cases against the reference: the spec's worked example, every
hand-written case of `tests/test_b_index.py`, generated tiny seeds 1-6 and sample seeds 2-3,
and random cases with long tokens (12, 13, 17, 32 and 33+ bytes), terms sharing their first 12
bytes, a line longer than a read buffer, op words as terms and NOT emptying a set. The same 23
also pass with a stress build whose read buffer, chunk and segment are 1 KB, 4 KB and 64 KB,
which exercises every boundary path. Fuzzing found one bug before submission: two threads
failing on malformed queries could both call `exit` and hang; only the first now reports.
For v4.1, two more builds force the safety paths: one pins the distinct-term estimate to 1 (a
1,024-slot first table, so the fallback always runs) and is byte-identical on seeds 1 and 7 and
passes all 23 cases; one caps segments at 100 documents (so every segment is bisected) and passes
all 23 cases. The fallback was removed in step 13 (the final table is always sized for every
word); the segment cap stays, and v4.2 passes the 100-document build too.

### Lessons (v4)

- A design's ceiling is set by its structure, not its tuning. Ten steps of v1 to v3 moved the
  board 3%; changing what is repeated (84 query passes, candidate re-reads) moved it 39%.
- Statistics before code: one offline pass over the corpus (term frequencies, phrase candidate
  counts, anchor costs under several heuristics) chose the phrase design before any of it was
  written.
- Memory was found by accounting, not guessing: printing every buffer's capacity showed zero-
  filled worst-case scratch arrays and slot-indexed per-term arrays as the two big items.
