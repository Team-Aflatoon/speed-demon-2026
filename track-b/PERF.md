# Track B: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

T and M in steps 1 to 4 are **local** measurements: `bench/bench.sh b 5`, which copies the
harness's sandbox flags, cpuset 0-11, warm page cache and cgroup `memory.peak`, on an
i7-14650HX laptop. T is the mean of the middle three of five runs. The laptop's timings drift
by ±15% with its temperature, so changes under about 10% were decided by interleaved A/B runs of
the two builds instead (noted per step). Points are estimates: box T taken as 1.5 × local T, and
S = 1,033 throughout. Board run IDs get filled in as the runs are made.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 56.35 | 2,200 | 0 |
| 1 | Rust, no crates. Query terms only, in one hash table. Corpus memory-mapped and cut into 3 MB blocks for 12 threads. A 4,096-document index per batch (sparse `u16` lists, plus bitmaps for terms seen in more than 64 documents), every query answered per batch, answers merged under a per-query mutex. Phrases verified by re-tokenizing each candidate document. | | 0.325 (local) | 58 (local) | ≈ 59 (est.) |
| 2 | Phrases verified around occurrences of the longest term instead of re-tokenizing the whole document (phrase work was about 100 ms of 300 per thread). Per-batch presence bitset, so absent terms resolve from L1. Query steps in one flat array. Token lookups hashed and prefetched one document at a time, then resolved. | | 0.235 (local) | 55 (local) | ≈ 63 (est.) |
| 3 | Bitmask lexer: 64 bytes per step with AVX2, events visited by trailing-zero count instead of a byte-at-a-time loop that mispredicted at every token boundary. AVX2 first/last-byte search for phrase anchors. One state word per term (dense slot or last document) instead of two arrays. | | 0.200 (local) | 55 (local) | ≈ 65 (est.) |
| 4 | Dense bitmaps written transposed during the scan (one L1-resident stretch per 64 documents) and transposed back per batch. Term state words prefetched between resolving and recording. Small BOOL results kept as sorted lists instead of bitmaps. A/B against step 3: 176 vs 185 ms median of 7 interleaved runs. | | 0.201 (local) | 58 (local) | ≈ 65 (est.) |

## Where the time goes (step 4, local, per worker thread)

About 95 ms building batch indexes and 50 ms answering queries, after 17 ms of serial query
parsing. In the build, per thread: about 34 ms for lexing and hashing, 28 ms for resolving
lookups in the 3 MB term table, and 27 ms for recording hits. Those were measured by switching
off each stage in a copy of the program.

## Experiments that did not help

- **8,192-document batches** (half the passes over the 50,000 queries). Query time did not move
  (52 vs 50 ms per thread) and M rose from 55 to 75 MB. The query cost is per document, not per
  pass, so the batch size went back to 4,096.
- **Detecting dense terms from an empty list** in `Index::set`, to skip reading the state word.
  Query time went from 52 to 72 ms per thread, because dense terms are the most-queried ones and
  their `bounds` entries are cold. Reverted.
- **Prefetching per-term data for the query 8 places ahead.** 217 vs 208 ms median (A/B, within
  noise, if anything slower). Reverted.
- **The prefetching two-pass scan on its own**, before the bitmask lexer: no measurable gain
  (the scalar tokenizer's branch misses hid it). It started to pay once the lexer stopped
  mispredicting.

## Lessons

- Profile first. Re-tokenizing phrase candidates looked cheap on paper (214,000 candidates)
  and turned out to be a third of the run.
- On this laptop only interleaved A/B runs are trustworthy for anything under about 10%.
