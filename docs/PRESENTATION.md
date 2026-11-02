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
| A (v13) | run-c0023c08eb4f (also run-674156b107d1, run-8dff5d3d18e6) | 19.6 ms (19.6 to 19.8) | 17.2 MB | 447 | 49.61 (49.32 to 49.61) | 84.78 | 32.05 s | about 1,630x |
| B (v12) | run-f20b7353a552 (also run-cbc79a44d139, run-a277e49d042a, run-fe126b463988) | 89.8 ms (88.8 to 89.8; 87.3 to 99.0 over 16 runs) | 38.0 MB | 1,540 | 35.83 (35.83 to 36.04) | 77.71 | 56.35 s | about 630x |
| C (v15) | run-2f3373c54a09 (also run-aca157ea0902, run-2c4fa89cafad) | 24.5 ms (24.3 to 24.5) | 12.1 MB | 600 | 37.16 (37.16 to 37.48) | 78.50 | 14.44 s | about 590x |
| D | run-60e7bb6f9004 (also run-79d73088c3c3, run-053708dfffa2) | 141.8 ms (141.8 to 148.6; 141.0 to 165.4 over 28 runs on Nov 2) | 70.5 MB | 618 | 24.75 to 25.22 (23.35 to 25.26 on Nov 2) | 69.68 to 70.08 | 72.69 s | about 513x |

Nov 2 took us from 20th (79.25) to 19th (80.50) on the live board, from three changes: a memory cap on A, a huge-page table on B, and hash-identified names with a shared bit matrix on C (the next slides explain each).

**Earlier builds:**

| Track | Build | T | M | K |
|---|---|---|---|---|
| A | v12 (S 466), 11 runs, e.g. run-8d63f862b12c | 19.7 to 26.9 ms | 16.8 to 17.8 MB | 41.86 to 49.29 |
| A | v9 (S 521), 27 runs at S 521; best run-ccb75cc37209 | 19.3 to 21.4 ms | 16.3 to 22.1 MB (5 runs at 21.1 to 22.1) | 44.41 to 49.05 |
| A | S 532, run-8fc3ec9b48d1 and two more | 20.4 to 21.1 ms | 17.9 to 18.3 MB | 46.0 to 46.9 |
| A | S 514, two runs | 20.0 and 20.1 ms | 18.0 MB | 47.72, 47.67 |
| B | v13 candidate (S 1,491), boxed twice, not shipped: run-12996e3a24f3, run-3d4480a45f74 | 88.5 and 95.7 ms (v12 in the same pairs: 88.8 and 89.8) | 38.3 and 38.1 MB | 36.28, 34.89 |
| B | v11 (S 1,531), 16 runs, e.g. run-39e71acbc73a | 90.3 to 96.7 ms | 37.4 to 38.4 MB | 34.56 to 35.76 |
| B | v5.1, run-438bfabbceeb | 113.6 ms | 36.3 MB | 31.43 |
| B | v5, run-407d7d7d6411 | 125.9 ms | 36.1 MB | 30.21 |
| B | v4.2 | 186.0 to 192.3 ms | 36.1 to 36.6 MB | 25.1 to 25.5 |
| C | v14 (S 685), 5 runs, e.g. run-9aa50ff4b16b | 25.2 to 26.3 ms | 11.6 to 12.0 MB | 35.46 to 36.11 |
| C | v13 (S 755), 17 runs, e.g. run-91c3366ba4bb | 26.3 to 28.3 ms | 16.0 to 16.9 MB | 31.47 to 32.66 |
| C | v5.2, run-52c515f71c76 | 32.4 ms | 16.7 MB | 28.60 |
| C | v5.1, run-658779ed2eee | 33.8 ms | 16.3 MB | 28.25 |
| C | v5, run-41dfe519e529 | 37.4 ms | 16.9 MB | 26.81 |
| C | v4.2 | 48.5 to 50.5 ms | 12.3 to 12.6 MB | 25.15 to 25.38 |
| D | v4, S 551, median of five (140.4 to 147.9); best run | 142.1 ms; 140.4 ms | 70.4 MB | best 25.80 |

D = 1 on every track. Every run ID is in its track's PERF.md. Red runs in our history are deliberate probes, except one Track C v3 run that hit the name-table race later fixed in v4.1; the public history has 19 not-ok runs (A 5, B 0, C 9, D 5) among 559, and none from Nov 2 (README, "Red runs").

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

Live board: seed 1, one run. Final scoring: private holdout seed, five runs; **T is the mean of the middle three, M is the maximum of the five** (`harness/run_submission.py`), so one memory spike counts in full.

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
| C | "name table" 21 ms (the probe removed all name work at once; we read it as the atomic OR, which cost about 2 ms when we rebuilt it on Nov 2); field parsing and aggregates 12.3 ms; separator flattening 5.2 ms |
| D | first parse about 10 ms; second parse and scatter about 14 ms; counting sort about 18 ms; rest of the build about 30 ms; queries about 75 ms |

Then we stopped probing and compared correct builds head to head. Each probe is documented in its track's PERF.md.

---

## The S/K trade-off: size and memory are priced

The formula penalises code size and memory, so a faster build is not automatically a better one. We rejected builds whose extra S and M cost more than the T they gained.

- Track D's one-parse build: 140.3 ms against 141 to 148, but S 551 to 662, so K fell
- Track D's page-fault builds: 2 to 3% faster, +2.5 MB and S 589 to 609, so K fell
- Track C's owner-partitioned names: 20 to 25% faster locally, no faster on the box, S 889
- Track C's half-load name table: faster, but about 7 MB more
- Track C's v15: 85 fewer lines than v14 (S 685 to 600) with 3.9% fewer instructions, so it won on both sides

---

## Nov 2: what we got wrong, and the fix

On the last day we audited every idea we had rejected. The same few mistakes turned up, and three of them were worth fixing before the freeze (20th at 79.25 at 07:31 UTC, 19th at 80.50 at 10:58 UTC, from best runs).

| What we got wrong | The fix | Board result |
|---|---|---|
| **A: compared builds on median T and typical M.** The final M is the maximum of five runs. v9 read 21 to 22 MB instead of 17 in 5 of 27 runs; v11 (smaller code) was dropped on one slow run in two, which most likely had the same cause | A lookahead cap (a piece is handed out only within 64 of the oldest one still being parsed), v11's trim back, then a size cut: v13 | M 16.8 to 17.8 MB in all 11 runs of v12; S 521 to 447; K 49.61 |
| **C: blamed the atomic OR.** A probe removed all name work at once, and v5's rewrite bundled several changes and was credited to its pair arenas | Hash-identified names, one shared country-by-name bit matrix on huge pages (the technique of two public entries, our own code); AES-NI hash and size cut | M 16.2 to 11.8 MB, S 755 to 600, T 26.7 to 24.4 ms, K 32.5 to 37.4 |
| **B: a rule applied too broadly.** "THP off for the whole process" made our huge-page test a no-op | Populate the term table on a huge page first, then switch THP off | 3 of 3 paired sessions faster, 89.2 against 92.5 ms |
| **A size cut we did not ship.** B's v13 (S 1,540 to 1,491, same output) | Boxed in two pairs against production: 88.5 against 88.8 ms, then 95.7 against 89.8. The rule was not slower in either pair | B ships v12 |

Also on the list: single box runs inside the noise used to reject builds, rejections made on local numbers only, an exact-name rule and a 44 MB memory cap treated as walls, unverified codegen, and never reading public code. Tried and rejected today: profile-guided optimisation (B, C: no gain locally, S +7), a B fast start-up (the mechanism did not move), glibc `arena_max`.

**The price:** C's names now merge only if 62 hash bits agree, about 1.1e-9 per dataset. The credit: AVENGERS and Cache Me showed the shared-matrix technique.

---

## Track A: Ingest

3.1M log lines to per-hour, per-endpoint counts, p50/p95/p99 and 5xx rate. Reference 32.05 s, 228 MB.

**Design**
- 11 workers, 128 KB pieces read with `pread`, one 4-byte record per line in its hour's log
- Hours are reported while parsing, once every piece that could write them is parsed; a late line makes the job rerun
- A piece is handed out only within 64 pieces of the oldest one still being parsed (Nov 2), so a stalled thread cannot make the others pile up memory
- AVX2 line indexing, branch-free latency parse, count-and-select percentiles

**Decisive steps (board)**

| Step | T | M | Run |
|---|---|---|---|
| v1 Rust, mmap | 46.1 ms | | run-a288ae43d4e8 |
| v2 per-hour 4-byte logs | 33.0 ms | | run-078b64667d1c |
| v7 report while parsing | 24.5 ms | | run-d96b1571240a |
| pread instead of mmap | 20.7 ms | | run-cbad326faf08 |
| v9 quiescent reports (S 521) | 19.3 ms | 17.1 MB | run-ccb75cc37209 |
| v9 memory spike (5 of 27 runs) | 21.1 ms | 21.1 MB | run-b4ba6a189863 |
| v12 = v11 trim + lookahead cap (S 466) | 19.7 ms | 17.1 MB | run-8d63f862b12c |
| **v13, shipped (S 447)** | **19.6 ms** | **17.2 MB** | run-c0023c08eb4f |

K 49.61 shipped, 84.78 points (v9's best: K 49.05). The gain is mostly M and S: v9's 21 to 22 MB runs cost about 1 point each when one lands in the five final runs.

---

## Track B: Index

50,000 boolean and phrase queries over a 250 MB corpus. Reference 56.35 s, 2.2 GB.

**Design**
- The queries are input: index only their terms, in one lock-free hash table
- A phrase needs no index: it is checked during the scan at its rarest-query term
- Postings built per 14 MB segment; one task queue (scan, build, answer) with no idle phase boundary
- The 4 MB term table sits on a 2 MiB huge page, built before huge pages are switched off for the rest of the process (Nov 2)

**Decisive steps (board)**

| Step | T | M | Run |
|---|---|---|---|
| v1 query terms only | 309 ms | 59.7 MB | run-7450e3d4343b |
| v2-v3 THP off, bitmap stripes | about 303 ms | 39.1 MB | run-43f854129993 |
| v4 redesign | 184.3 ms | 35.8 MB | run-64a666bd15fb |
| v4.2 (S 1299) | 186.0 ms | 36.1 MB | run-3ae92ad95108 |
| v5 branch-free term table, leaner token loop (S 1,562) | 125.9 ms | 36.1 MB | run-407d7d7d6411 |
| v5.1 (S 1,689) | 113.6 ms | 36.3 MB | run-438bfabbceeb |
| v6 cheaper inner loops, 25% fewer instructions (S 1,742) | 95.2 ms | 34.8 MB | run-46aaabc13772 |
| v8 finer bitmaps, 14 MB segments (S 1,618) | 91.2 ms | 37.7 MB | run-9678ee16f487 |
| v11 trimmed code (S 1,531) | 92.3 ms | 38.4 MB | run-39e71acbc73a |
| **v12 huge-page term table, shipped (S 1,540)** | **88.9 ms** (latest run 89.8) | **37.7 MB** | run-fe126b463988, run-f20b7353a552 |

K 35.83 shipped (v12; its best run K 36.35), 77.71 points (v4.2: K 25.5). v4.1/v4.2 made every capacity provable for any valid input; v5 to v12 are the second-round redesign and its tuning, with output byte-identical to v4.2's. A size cut (v13, S 1,491) was boxed twice and not shipped: 88.5 ms against 88.8 in its first pair, 95.7 against 89.8 in its second.

---

## Track C: Join

Hostile CSV (quotes, embedded newlines, CRLF, BOMs) joined to a lookup table. Reference 14.44 s, 367 MB.

**Design**
- AVX2 bitmasks per 64-byte block; the prefix XOR of the quote mask marks what is inside quotes
- Chunk cuts are guessed (an LF followed by a digit), then proven by quote parity; if not, one worker reparses the whole file
- v5 to v13: names go into a read-mostly lock-free table, each record appends a private (name, group) pair, and exact distinct counts come from an L1-resident bitset after one barrier
- v14 and v15 (Nov 2): a name is a 62-bit hash (three AES-NI rounds, no name bytes stored); the distinct (country, name) pairs are one shared bit matrix set with `lock bts`; table and matrix on three huge pages; no pair logs, no second phase

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
| v5.2 (S 882) | 32.4 ms | 16.7 MB | run-52c515f71c76 |
| v13 layout compaction (S 755) | 26.8 ms | 16.0 MB | run-91c3366ba4bb |
| v14 hash names + shared matrix (S 685) | 25.3 ms | 12.0 MB | run-9aa50ff4b16b |
| **v15, shipped (S 600)** | **24.5 ms** | **12.1 MB** | run-2f3373c54a09 |

K 37.16 shipped, 78.50 points (best v15 run K 37.48, 78.69; v4.2: K 25.15). A faster-looking owner-partitioned design failed on the box (run-0caa577fd90b) and was reverted. The price of v14 and v15 is hash identity: two names merge only if 62 bits agree, about 1.1e-9 per dataset (Correctness).

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
- The B and C designs of Nov 1 passed a fresh concurrency review with 2,400 stress runs plus 500 race-injection runs, 0 wrong
- Nov 2 gates: seeds 1, 7, 101 and 202 byte-identical for every candidate; A's cap 0 wrong in 300 + 100 runs and 0 in 80 with two CPU hogs; B 0 in 300 + 100; C's v14 and v15 0 in 300 + 100 + 100 (+ 100 on three CPUs for v15)
- A root test suite, `tests/`: 33 end-to-end cases over all four tracks, run as the scoring harness runs a submission (`bash tests/run.sh`)
- C's hash identity: two names merge only if all 62 hash bits agree, about 1.1e-9 per dataset for 100,000 names; six more full seeds (7001 to 7006) identical to the reference, 7.79 M structured names with 0 collisions; the build needs AES-NI
- A known divergence: C rejects a quoted amount, which the reference accepts; the generator never quotes an amount (spec 2.2)

| Track | Fallback we forced |
|---|---|
| A | rerun when an hour was reported before a late line |
| B | segment splitting |
| C | single-chunk reparse (more than about 130,000 distinct names exits 1 by design) |
| D | 64-bit distance retry |

**The bug our own stress test caught.** Track C's v4 lock-free name table (gone in v14 and v15) had a 1-in-100 publication race: a slot's hash word briefly overwrote its `meta` word, so another core could insert the same name twice and a distinct count came out one too high. A 300-run stress test found it; the fix was one line. It matters because final scoring needs all five runs correct, so a 1% race was about a 5% chance of a zero. One board run per build could never have shown it.

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
- Count what the score counts: M is the maximum of five runs, so a rare memory spike matters as much as the median time
- A probe that removes several things at once cannot blame one of them; a rewrite that changes several things cannot credit one of them
- A rejection needs repeated, interleaved box runs, and a rule you set for one table should not switch off a feature for the whole program
- Read what the competition published, early; credit it, and write your own code

---

## What next

- A: a one-pass selection from a small sorted sample; a pre-faulted pool of log pages; a leaner line path (about 140 instructions a line)
- B: let the first scan tasks lex the corpus while the term table is filled; measure B's phases on the box, which we never did
- C: a 1 MiB table of full hashes with the region map on the same huge pages; a leaner per-record path (about 358 instructions a record in v15)
- D: find why the counting sort costs 4 to 5 times more on the box; lower the two memory peaks together

Thank you.
