# Track B: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

Rows with a Run ID are board measurements; row 0 is the board's reference figures. Every other
T and M is **local**: `bench/bench.sh b 5` (the harness's sandbox flags, cpuset 0-11, warm page
cache, cgroup `memory.peak`) on an i7-14650HX laptop, with T the mean of the middle three of five
runs. The laptop was shared with other jobs, so its absolute timings drift by ±15% or more from
hour to hour. Every decision under about 10% was therefore made from interleaved A/B runs of the
two builds (median of 7 to 9 runs each, plus the cgroup's CPU time), and those numbers are given
per step. Compare a step's A/B with its parent, not its local T with another row's.

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
| 10 | Line count, no behaviour change: `query.rs` folded into `main.rs`, `Answer::merge` reduced to a sort of at most 20 ids. S 1,013 to 1,006. **This is v3.** | run-d53081aa4120 (reruns run-4f979d5ac144: 0.3021 s, 39.7 MB; run-a5f9c82b0e45: 0.3070 s) | 0.3035 | 39.1 | ≈ 65.9 (computed) |

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
