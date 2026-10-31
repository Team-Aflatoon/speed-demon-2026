## Team Aflatoon · Speed Demon 2026

Four tracks, four Rust programs, zero dependencies.

- Dhruv Sharma ([@Spiritsfuse](https://github.com/Spiritsfuse))
- Anushika Chauhan ([@Anushika06](https://github.com/Anushika06))
- Pratyush Mishra ([@PratyushMishra-2nd](https://github.com/PratyushMishra-2nd))

Repo: [README](../README.md) · logs: `track-a/PERF.md` to `track-d/PERF.md`

---

## The challenge and how it is scored

Take a naive Python reference, keep its output byte for byte, make it fast and lean on the organisers' scoring machine.

**K = (T_ref/T)^0.50 × min(M_ref/M, 50)^0.20 × min(S_ref/S, 1)^0.15 × (D_ref/D)^0.15**

| Axis | Weight | Notes |
|---|---|---|
| T, wall time | 0.50 | uncapped, dominates |
| M, peak memory | 0.20 | pays up to 50x less than the reference |
| S, source lines | 0.15 | can only cost |
| D, dependencies | 0.15 | can only cost; ours is 1 (none) |

Points = 50 × log10(K). Track total = 65% machine points + 35% judges (code quality 15, technique 12, optimisation log 8).

Live board: seed 1, one run. Final scoring: private holdout seed, five runs.

---

## Our approach and principles

- Rust, standard library only; libc calls declared by hand
- Parallel over all 12 cores of the scoring box (cores 0-11)
- Do less work: remove repeated passes, not just tune loops
- Short code: S counts, so every line must earn its place
- Exact on any valid input: speculation is always checked, never trusted
- Every claim about speed is a board measurement

---

## The key insight: the laptop is not the box

The scoring box is slower and different: turbo off at 2.5 GHz, smaller caches, memory-bound work hurts most.

| Observation | Number |
|---|---|
| Box vs our laptop, quiet machine | about 1.45x to 1.6x slower |
| Local wins of 20 to 25% | no faster on the box |
| Track C per-record work | 2 to 3 times more costly on the box |
| Track D counting sort | 4 to 5 times its laptop cost |
| Track B, ten tuning steps | moved the board 3% |
| Track B, removing repeated work | moved the board 39% |

Consequence: a local CPU win is a hypothesis until a board run confirms it.

---

## The probe method

The board records time and memory even when the output is wrong. So we pushed deliberately incomplete builds (they fail the hash on purpose, the red runs in our history) and subtracted.

| Track | Phase cost on the box |
|---|---|
| A | read the file only 14.6 ms; ingest only 21.3 ms; report after the barrier about 11 ms |
| C | name table 21 ms; field parsing and aggregates 12.3 ms; separator flattening 5.2 ms |
| D | first parse about 10 ms; second parse and scatter about 14 ms; counting sort about 18 ms; rest of the build about 30 ms; queries about 75 ms |

Then we stopped probing and compared correct builds head to head. Each probe is documented in its track's PERF.md.

---

## Track A: Ingest

3.1M log lines to per-hour, per-endpoint counts, p50/p95/p99 and 5xx rate. Reference 32.05 s, 228 MB.

**Design**
- 11 workers, 128 KB pieces read with `pread`, one 4-byte record per line in its hour's log
- Hours are reported while parsing; exact counts prove it, a rerun covers a late line
- AVX2 line indexing, branch-free latency parse, count-and-select percentiles

**Decisive steps (board)**

| Step | T | Run |
|---|---|---|
| v1 Rust, mmap | 46.1 ms | run-a288ae43d4e8 |
| v2 per-hour 4-byte logs | 33.0 ms | run-078b64667d1c |
| v7 report while parsing | 24.5 ms | run-d96b1571240a |
| pread instead of mmap | 20.7 ms | run-cbad326faf08 |
| **Final** | **20.2 ms, 17.9 MB, K 47.33** | run-97875b283660 |

---

## Track B: Index

50,000 boolean and phrase queries over a 250 MB corpus. Reference 56.35 s, 2.2 GB.

**Design**
- The queries are input: index only their terms, in one lock-free hash table
- A phrase needs no index: it is checked during the scan at its rarest-query term
- Postings built per 8 MB segment; one task queue (scan, build, answer) with no idle phase boundary

**Decisive steps (board)**

| Step | T | M | Run |
|---|---|---|---|
| v1 query terms only | 309 ms | 59.7 MB | run-7450e3d4343b |
| v2-v3 THP off, bitmap stripes | about 303 ms | 39.1 MB | run-43f854129993 |
| v4 redesign | 184.3 ms | 35.8 MB | run-64a666bd15fb |
| **Final v4.2** | **189.6 ms** | **36.6 MB** | run-d4d593bc34cf |

K 25.25. v4.1/v4.2 made every capacity provable for any valid input.

---

## Track C: Join

Hostile CSV (quotes, embedded newlines, CRLF, BOMs) joined to a lookup table. Reference 14.44 s, 367 MB.

**Design**
- AVX2 bitmasks per 64-byte block; the prefix XOR of the quote mask marks what is inside quotes
- Chunk cuts are guessed (an LF followed by a digit), then proven by quote parity; if not, one worker reparses the whole file
- One lock-free name table; a shared bitset gives exact distinct counts

**Decisive steps (board)**

| Step | T | M | Run |
|---|---|---|---|
| v1 12 threads, verified cuts | 61.9 ms | 16.2 MB | run-c898fb001979 |
| v2 bitmask parser, 24-byte keys | 52.8 ms | 12.1 MB | run-be5f1e4a22bf |
| pread instead of mmap | 49.3 ms | 14.1 MB | run-2890a0259c6b |
| **Final v4** | **49.3 ms** | **12.4 MB** | run-1e5d2353fd9f |

K 25.32. A faster-looking owner-partitioned design failed on the box (run-0caa577fd90b) and was reverted.

---

## Track D: Traverse

2,000 shortest-path queries on a 5M-edge weighted digraph. Reference 72.69 s, 893 MB.

**Design**
- Parallel CSR build with per-thread, per-bucket runs: plain stores, no atomics
- 4-byte edges (head plus 11-bit weight, escape table for heavy ones)
- BFS renumbering for locality: searches about 4.4x faster
- Bidirectional Dijkstra, radix heap, 32-bit distances with an exact 64-bit retry

**Decisive steps (board)**

| Step | T | M | Run |
|---|---|---|---|
| v1 as submitted | 156 ms | 93.5 MB | run-0b83abcdda4d |
| THP opt-out | | 79.8 MB | run-bf037ba42528 |
| 32-bit searches, leaner BFS | | 70.8 MB | run-184dcb5b4f5a |
| **Final v4** | **140.4 ms** | **70.4 MB** | run-1d538165ea54 |

K 25.80. Probes and head-to-heads (one parse, pread, smaller buckets, clean-room rework) all kept v4.

---

## Correctness discipline

Speed that is wrong scores zero, and the final data is a seed we have never seen.

- No data from seed 1 in the code: everything is computed from the input
- Full-size unseen seeds, tiny and sample seeds, spec edge cases, adversarial files
- Every fallback forced on purpose and checked for the right bytes

| Track | Fallback we forced |
|---|---|
| A | rerun when an hour was reported before a late line |
| B | segment splitting |
| C | single-chunk reparse, name-table regrowth |
| D | 64-bit distance retry |

- Holdout-safety audit of every fixed limit; two Track B limits made provably safe
- Judge-style reviews against the official criteria before each push

---

## Results summary

Live board, seed 1, single run; final scoring uses a private holdout seed and five runs.

| Track | Run | T | M | S | K | Points | Reference | Speedup |
|---|---|---|---|---|---|---|---|---|
| A | run-97875b283660 | 20.2 ms | 17.9 MB | 530 | 47.33 | 83.76 | 32.05 s | about 1,590x |
| B | run-d4d593bc34cf | 189.6 ms | 36.6 MB | 1317 | 25.25 | 70.11 | 56.35 s | about 297x |
| C | run-1e5d2353fd9f | 49.3 ms | 12.4 MB | 729 | 25.32 | 70.17 | 14.44 s | about 293x |
| D | run-1d538165ea54 | 140.4 ms | 70.4 MB | 551 | 25.80 | 70.58 | 72.69 s | about 518x |

D = 1 on every track.

---

## Lessons

- Measure on the target machine; a local win is only a hypothesis
- Remove repeated work before tuning loops (Track B: 39% against 3%)
- Phase structure and kernel work (faults, barriers, copies) set T, not instruction counts
- Memory behaves differently on the box too: transparent huge pages cost Track D 93.5 MB instead of 79.8 MB
- Test deliberately incomplete builds: subtraction beats guessing
- Every speculation needs a check and a fallback

---

## What next

- A: a one-pass selection from a small sorted sample; a pre-faulted pool of log pages; a leaner line path (about 140 instructions a line)
- B: hide the 6 ms query parse behind the first scan tasks; a direct table for one- and two-byte tokens
- C: avoid the shared atomic OR (about 7% of CPU); a leaner per-record path, measured on the box
- D: find why the counting sort costs 4 to 5 times more on the box; lower the two memory peaks together

Thank you.
