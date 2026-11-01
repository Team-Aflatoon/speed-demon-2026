## Team Aflatoon · Speed Demon 2026

Four tracks, four Rust programs, zero dependencies.

- Dhruv Sharma ([@Spiritsfuse](https://github.com/Spiritsfuse))
- Anushika Chauhan ([@Anushika06](https://github.com/Anushika06))
- Pratyush Mishra ([@PratyushMishra-2nd](https://github.com/PratyushMishra-2nd))

Repo: [README](../README.md) · logs: `track-a/PERF.md` to `track-d/PERF.md`

---

## Results

Live board, seed 1, single runs on the scoring machine; final scoring uses a private holdout seed and five runs.

**The shipped code:**

| Track | Run | T | M | S | K | Points | Reference | Speedup |
|---|---|---|---|---|---|---|---|---|
| A | run-8fc3ec9b48d1 (also run-996cb0022b51, run-361c33ed2406) | 21.1 ms (20.4 to 21.1) | 18.3 MB | 532 | 46.02 (46.0 to 46.9) | 83.15 | 32.05 s | about 1,520x |
| B (v5.1) | run-438bfabbceeb | 113.6 ms | 36.3 MB | 1,689 | 31.43 | 74.87 | 56.35 s | about 496x |
| C (v5.2) | run-52c515f71c76 | 32.4 ms | 16.7 MB | 882 | 28.60 | 72.82 | 14.44 s | about 446x |
| D | run-60e7bb6f9004 (also run-79d73088c3c3, run-053708dfffa2) | 141.8 ms (141.8 to 148.6) | 70.5 MB | 618 | 24.75 to 25.22 | 69.68 to 70.08 | 72.69 s | about 513x |

Second-round redesigns on B and C, each judged head to head on the box: B 186 → 113.6 ms, C 48.5 → 32.4 ms.

**Earlier builds:**

| Track | Build | T | M | K |
|---|---|---|---|---|
| A | S 514, two runs | 20.0 and 20.1 ms | 18.0 MB | 47.72, 47.67 |
| B | v5, run-407d7d7d6411 | 125.9 ms | 36.1 MB | 30.21 |
| B | v4.2 | 186.0 to 192.3 ms | 36.1 to 36.6 MB | 25.1 to 25.5 |
| C | v5.1, run-658779ed2eee | 33.8 ms | 16.3 MB | 28.25 |
| C | v5, run-41dfe519e529 | 37.4 ms | 16.9 MB | 26.81 |
| C | v4.2 | 48.5 to 50.5 ms | 12.3 to 12.6 MB | 25.15 to 25.38 |
| D | v4, S 551, median of five (140.4 to 147.9); best run | 142.1 ms; 140.4 ms | 70.4 MB | best 25.80 |

D = 1 on every track. Every run ID is in its track's PERF.md. Red runs in our history are all deliberate probes (README, "Red runs").

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
- Short code: S counts, so every line must earn its place (the one exception: error handling that makes unseen input fail loudly)
- Exact on any valid input: speculation is always checked, never trusted
- Every speed decision is settled by a board run; figures marked "local" are laptop numbers that only chose candidates

---

## The key insight: the laptop is not the box

The scoring box runs at 2.5 GHz with turbo off and has smaller caches; memory-bound work hurts most.

| Observation | Number |
|---|---|
| Box vs our laptop | 1.45x (D), 1.6x (C), 1.9x (A, v7: 24.5 ms against 13.1 ms) |
| Local wins of 20 to 25% | no faster on the box |
| Track C scan | 1.1x; per-record work 2 to 3 times more costly on the box |
| Track D counting sort | 4 to 5 times its laptop cost |
| Track B, ten tuning steps | moved the board 3% |
| Track B, removing repeated work | moved the board 39% |
| Board noise, one build | up to 3.5% (A); 140 to 148 ms (D); up to 17% in early, loaded runs |

Consequence: a local CPU win is a hypothesis until a board run confirms it, and a difference of a few percent between two builds needs reruns.

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

## The S/K trade-off: size and memory are priced

The formula penalises code size and memory, so a faster build is not automatically a better one. We rejected builds whose extra S and M cost more than the T they gained.

- Track D's one-parse build: 140.3 ms against 141 to 148, but S 551 to 662, so K fell
- Track D's page-fault builds: 2 to 3% faster, +2.5 MB and S 589 to 609, so K fell
- Track C's owner-partitioned names: 20 to 25% faster locally, no faster on the box, S 889
- Track C's half-load name table: faster, but about 7 MB more

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
| Before hardening (S 514) | 20.0 ms, 18.0 MB, K 47.72 | run-2491c9d07251 |
| **Shipped (S 532)** | **20.4 to 21.1 ms, 18.2 to 18.3 MB, K 46.0 to 46.9** | run-996cb0022b51, run-361c33ed2406, run-8fc3ec9b48d1 |

---

## Track B: Index

50,000 boolean and phrase queries over a 250 MB corpus. Reference 56.35 s, 2.2 GB.

**Design**
- The queries are input: index only their terms, in one lock-free hash table
- A phrase needs no index: it is checked during the scan at its rarest-query term
- Postings built per 12 MB segment; one task queue (scan, build, answer) with no idle phase boundary

**Decisive steps (board)**

| Step | T | M | Run |
|---|---|---|---|
| v1 query terms only | 309 ms | 59.7 MB | run-7450e3d4343b |
| v2-v3 THP off, bitmap stripes | about 303 ms | 39.1 MB | run-43f854129993 |
| v4 redesign | 184.3 ms | 35.8 MB | run-64a666bd15fb |
| v4.2 (S 1299) | 186.0 ms | 36.1 MB | run-3ae92ad95108 |
| v5 branch-free term table, leaner token loop (S 1,562) | 125.9 ms | 36.1 MB | run-407d7d7d6411 |
| **v5.1, shipped (S 1,689)** | **113.6 ms** | **36.3 MB** | run-438bfabbceeb |

K 31.43 shipped, 74.87 points (v4.2: K 25.5). v4.1/v4.2 made every capacity provable for any valid input; v5 and v5.1 are the second-round redesign, with output byte-identical to v4.2's.

---

## Track C: Join

Hostile CSV (quotes, embedded newlines, CRLF, BOMs) joined to a lookup table. Reference 14.44 s, 367 MB.

**Design**
- AVX2 bitmasks per 64-byte block; the prefix XOR of the quote mask marks what is inside quotes
- Chunk cuts are guessed (an LF followed by a digit), then proven by quote parity; if not, one worker reparses the whole file
- v5 onward: names go into a read-mostly lock-free table, each record appends a private (name, group) pair, and exact distinct counts come from an L1-resident bitset after one barrier

**Decisive steps (board)**

| Step | T | M | Run |
|---|---|---|---|
| v1 12 threads, verified cuts | 61.9 ms | 16.2 MB | run-c898fb001979 |
| v2 bitmask parser, 24-byte keys | 52.8 ms | 12.1 MB | run-be5f1e4a22bf |
| pread instead of mmap | 49.3 ms | 14.1 MB | run-2890a0259c6b |
| v4.1, the race fix (S 734) | 49.1 ms | 12.1 MB | run-0fc0507654be |
| v4.2, plus error paths (S 767) | 48.9 ms | 12.6 MB | run-31b13fc9019b |
| v5 two-phase distinct names (S 827) | 37.4 ms | 16.9 MB | run-41dfe519e529 |
| v5.1 (S 858) | 33.8 ms | 16.3 MB | run-658779ed2eee |
| **v5.2, shipped (S 882)** | **32.4 ms** | **16.7 MB** | run-52c515f71c76 |

K 28.60 shipped, 72.82 points (v4.2: K 25.15). A faster-looking owner-partitioned design failed on the box (run-0caa577fd90b) and was reverted.

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
| v4 before hardening (S 551): median of five; best | 142.1 ms; 140.4 ms | 70.4 MB | run-1d538165ea54 (best) |
| **Shipped (S 618)** | **141.8 to 148.6 ms** | **70.5 MB** | run-60e7bb6f9004, run-79d73088c3c3, run-053708dfffa2 |

K 24.75 to 25.22 shipped (best v4 run: 25.80). Probes and head-to-heads (one parse, pread, smaller buckets, clean-room rework) all kept the v4 algorithm.

---

## Correctness discipline

Speed that is wrong scores zero, and the final data is a seed we have never seen.

- No data from seed 1 in the code: everything is computed from the input
- Full-size unseen seeds, tiny and sample seeds, adversarial spec-valid inputs
- Every fallback forced on purpose and checked for the right bytes
- Holdout-safety audit of every fixed limit; one Track B limit made provably safe
- Concurrency audit of A, B and D: every shared protocol argued correct, then 5,200 real runs + 3,000 with injected delays, 0 wrong
- The new B and C designs passed a fresh concurrency review with 2,400 stress runs plus 500 race-injection runs, 0 wrong

| Track | Fallback we forced |
|---|---|
| A | rerun when an hour was reported before a late line |
| B | segment splitting |
| C | single-chunk reparse, name-table regrowth |
| D | 64-bit distance retry |

**The bug our own stress test caught.** Track C's lock-free name table had a 1-in-100 publication race: a slot's hash word briefly overwrote its `meta` word, so another core could insert the same name twice and a distinct count came out one too high. A 300-run stress test found it; the fix was one line. It matters because final scoring needs all five runs correct, so a 1% race was about a 5% chance of a zero. One board run per build could never have shown it.

---

## Lessons

- Measure on the target machine; a local win is only a hypothesis
- Remove repeated work before tuning loops (Track B: 39% against 3%), and redesign again once the box says where the time goes (B 186 to 113.6 ms, C 48.5 to 32.4 ms)
- Phase structure and kernel work (faults, barriers, copies) set T, not instruction counts
- Memory behaves differently on the box too: transparent huge pages cost Track D 93.5 MB instead of 79.8 MB
- Test deliberately incomplete builds: subtraction beats guessing
- Every speculation needs a check and a fallback
- A single good run does not show a rare failure; hundreds of runs do
- Price every line and megabyte: S and M are in the formula

---

## What next

- A: a one-pass selection from a small sorted sample; a pre-faulted pool of log pages; a leaner line path (about 140 instructions a line)
- B: hide the 6 ms query parse behind the first scan tasks; a direct table for one- and two-byte tokens
- C: a leaner per-record path (about 634 instructions a record before v5.2), measured on the box
- D: find why the counting sort costs 4 to 5 times more on the box; lower the two memory peaks together

Thank you.
