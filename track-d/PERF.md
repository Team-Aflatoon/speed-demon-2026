# Track D: performance log

## Summary

- **Final, as submitted:** step 19 (commit 13b0490), the v4 algorithm hardened against
  bad input. Board runs: run-79d73088c3c3 143.1 ms, 71.3 MB, K 25.16, and run-053708dfffa2
  148.6 ms, 70.5 MB, K 24.75; median 145.9 ms, K 24.96. S rose 551 → 600 because the
  program now fails loudly, with a clear message, on a malformed input it has not seen,
  instead of panicking or answering wrongly.
- **Best algorithmic build, before hardening:** v4 (step 18, commit 173a349), best run
  run-1d538165ea54 at 140.4 ms, 70.4 MB, K 25.80; median of its five runs 142.1 ms
  (run-bc431b72ba59, K 25.64). The reference is 72.69 s and 893 MB.
- **v1** (bucketed CSR, BFS renumbering, bidirectional Dijkstra, 4-byte edges): 156 ms,
  93.5 MB (run-0b83abcdda4d).
- **THP opt-out**, a box-only memory fix: 93.5 to 79.8 MB (run-bf037ba42528).
- **32-bit searches and a leaner BFS phase:** 70.8 MB (run-184dcb5b4f5a).
- **Box probe split**, from deliberate wrong-output *probes* (red on the board): d0
  run-94d4d2fef344, d3 run-e96c9c86ab33, d4 run-148c3b623aad, d2 run-9efbc7bcc92f, d1
  run-8db29a146c98. On the box: one parse ~10 ms, second parse and scatter ~14 ms, sort
  ~18 ms, rest of the build ~30 ms, queries ~75 ms.
- **Head-to-heads that kept v4**, with full correct *candidates*: one-parse
  run-c82840f14b70, buckets run-60ca41139fe7 and run-b927b67c49d7, pread
  run-8c76e26ef37d, fewer page faults (four runs), clean-room rework run-2a608e748f34.
  Those that gained 2-3% in T on the box were still rejected, because their extra lines
  (S) and memory (M) cost more in K than the time earned.
- **Box noise** reaches 17% on one build (156, 181.9 and 166.5 ms for v1), so decisions
  used reruns.
- **Lesson:** a local CPU win is a hypothesis until a board run confirms it.

Rows marked "(local)" were measured on an i7-14650HX, cpuset 0-11, under the harness's
sandbox flags, with `bench/bench.sh` or an equivalent fresh-container run unless the row
says otherwise. Memory units: the board reports M in MB, meaning 10^6 bytes of cgroup
`memory.peak`; local figures are MiB (2^20 bytes). So the final 68 MiB locally is 71.3 MB,
and the board measured 70.4-71.3 MB. Rows with a run ID are board runs. A *probe* is a deliberately wrong-output build (it fails the hash and shows red on
the board) that measures part of the pipeline; a *candidate* is a full, correct build run
head to head against v4. The scoring box is roughly 1.45x slower than the local machine, and its own runs
vary: two reruns of the same v1 took 181.9 and 166.5 ms against 156.0. For part of the
work the local machine was shared with other benchmark jobs, so close calls were decided
by interleaved A/B runs of saved binaries, by repeating a phase several times in one
process and taking the minimum, or by total CPU time from the cgroup's `cpu.stat`. Such
rows say so. Points are the board's 50 x log10(K), clamped to 0-100, where
K = (72.69 s / T)^0.5 x min(893 MB / M, 50)^0.2 x min(43 / S, 1)^0.15 x (1 / D)^0.15. For
runs without a board figure they are computed from T, M and S with `harness/scoring.py`
and marked "(computed)".

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (board reference figures) | | 72.69 | 893 | 0 |
| 1 | Rust v1: mmap, parallel parse with an atomic per-node counter, CSR, serial BFS renumbering, one Dijkstra per query (binary heap) on 12 threads, block-allocated distances | | 0.42 (local) | n/a | |
| 2 | Bucketed CSR build with per-thread cursors and a counting sort, so no locked instruction per edge (parse 24 to 11 ms per pass); BFS from one root per thread in parallel; radix heap | | 0.22 (local) | 87 MiB (local) | |
| 3 | SWAR parsing: eight bytes per load, separator found by bit 4, Lemire's multiply combine (11 to 5.5 ms per pass) | | 0.21 (local) | 87 MiB (local) | |
| 4 | Bidirectional Dijkstra on an extra in-edge CSR, sides taking turns (query phase 127 to 47 ms; settled nodes 24.9M to 7.6M) | | 0.145 (local) | 129 MiB (local) | |
| 5 | Out- and in-edges built in the same two passes over the text (one CSR, 2n rows) | | 0.125 (local) | 129 MiB (local) | |
| 6 | 4-byte edges: head and 11-bit weight in one u32, heavier weights in an escape table; glibc mmap threshold pinned so freed phases leave the peak | | 0.138 to 0.127 (local, A/B) | 121 to 82 MiB (local) | |
| 7 | Forward graph by moving rows into BFS order, in-edges by an in-memory transpose (no second parse); BFS order freed before the move; distance blocks reserved up front; oversized heap buckets returned | | 0.126 to 0.114 (local, A/B) | 77 MiB (local) | |
| 8 | BFS threads keep rooting new regions in their own id range (balanced, no serial sweep); edge row prefetched when a node is first queued | | 0.108 (local, bench median of 5; seed 7: 0.110) | 77 MiB (local) | |
| 8 (board) | v1 as submitted (commit 984cb84), S 505 | run-0b83abcdda4d | 0.156 | 93.5 | 68.5 |
| 8 (board) | the same v1, rerun: box noise | run-145c9293e57e | 0.1819 | 93.5 | 66.8 (computed) |
| 8 (board) | the same v1, rerun: box noise | run-bd48a8574757 | 0.1665 | 93.5 | 67.8 (computed) |
| 9 | Opt out of transparent huge pages (`prctl(PR_SET_THP_DISABLE)`). A box-only effect: the box measured 93.5 MB against 77 MiB locally, where THP is opt-in. The call shows `THP_enabled: 0` in /proc/self/status and changes nothing locally; the v2 board run confirms it (93.5 to 79.8 MB) | | 0.108 (local) | 77 MiB (local) | |
| 10 | One persistent team of 12 threads woken by a barrier, instead of spawning threads for each of the 11 parallel phases (10 since step 11; 11 empty spawn rounds measured 5 ms) | | 0.110 to 0.107 (local, A/B medians) | 77 MiB (local) | |
| 11 | The BFS walk copies each row as it dequeues it, so the forward graph is laid out in new-id order with one sequential renaming copy; `permute` and its random-access passes are gone (BFS + layout 24 to 21-24 ms, in-process minima) | | 0.105 (local) | 77 MiB (local) | |
| 12 | The layout reuses the first graph's arrays (27 MB fewer page faults); S trims (shorter if/else and answer conversion, fewer attributes): S 553 to 525 | | 0.122 vs v1 0.135 (local, interleaved medians under heavy machine load) | 78 MiB (local) | |
| 12 (board) | v2 as submitted (commit 587a2e9), S 525 | run-bf037ba42528 | 0.1501 | 79.8 | 69.5 (computed) |
| 13 | Leaner BFS phase: `rank` is the claim flag (0 until claimed, then a provisional thread-and-index id fixed up once counts are known), the queue holds only the frontier, degrees take a byte each. Drops the order list, claim flags and end offsets (BFS + layout phase 75.6 to 58 MiB) | | same CPU (local) | 76 MiB (local) | |
| 14 | 32-bit tentative distances; a query whose distances pass 2^32 - 2 is answered again with 64 bits, so results stay exact (query phase 76 to 71 MiB) | | same CPU: 1000 vs 1020 ms total (local) | 71 MiB (local) | |
| 15 | The radix heap keys in the search's width too: a 32-bit search only queues keys that fit, so entries take 8 bytes, not 16 (query phase 71 to 68 MiB) | | CPU 1075 to 1035 ms total (local, A/B medians) | 68 MiB (local) | |
| 16 | S trims: one blanket `Width` trait for the bounds, `distance` takes the pair of searches, in-edges back to `build_csr`, unused helpers removed: S 600 to 538 | | same (local) | 68 MiB (local) | |
| 16 (board) | v3 as submitted (commit b661c23), S 538 | run-184dcb5b4f5a | 0.1459 | 70.8 | 70.2 |
| 17 | Review fixes, no change in speed: rustfmt, named libc constants, SAFETY notes. One real bug fixed: `split_lines` read `text[at - 1]` at `at == 0` when the edge text is shorter than the thread count (a one-edge graph on 12 threads aborted). New one-edge cases added to the tests. S 538 to 544 (named constants) | | same (local) | 68 MiB (local) | |
| 17 (board) | v3 with the review fixes (commit 2cfc0f4), S 544 | run-83d74bc76638 | 0.1421 | 71.1 | 70.5 (computed) |
| 18 | A third prefetch stage in the BFS walk: the claim flags (`rank` entries at random old ids) of the heads of the row 4 places ahead. The profile put about 70% of the walk on those loads | | total CPU -2.4% / -4% (local, two interleaved A/B runs of 6-8, medians); wall about 100 vs 104 ms | 68 MiB (local) | |
| 18 (board) | v4 code, restored after the probes (M not recorded) | run-92dc80fd3ba2 | 0.1479 | | |
| 18 (board) | v4 (commit 173a349), S 551, K 25.24: rerun | run-8e593d1f06b2 | 0.1464 | 70.7 | 70.10 |
| 18 (board) | v4 (commit 173a349), S 551, K 25.64: rerun, the median | run-bc431b72ba59 | 0.1421 | 70.4 | 70.45 |
| 18 (board) | v4 (commit 173a349), S 551, K 25.67: rerun | run-93e2691801f2 | 0.1412 | 71.2 | 70.47 |
| 18 (board) | v4 (commit 173a349), S 551, K 25.80: its best run, the best algorithmic build before hardening | run-1d538165ea54 | 0.1404 | 70.4 | 70.58 |
| 19 | Operator hardening, no change on valid input: errors are reported as `track-d: ...` with the path and exit status 1 (empty file, missing final newline, bad header, n above 2^26 or m above 2^32 - 1, node id not below n, edge count unlike the header, weight above 2^32 - 1, bad query line with its number, unreadable or unwritable files); the id check rides the counting pass. Optional `[data] [out]` arguments, a static binary, an offline build. S 551 to 600 | | same CPU as v4 (local) | 68 MiB (local) | |
| 19 (board) | step 19 as submitted (commit 13b0490), S 600, K 24.75 | run-053708dfffa2 | 0.1486 | 70.5 | 69.68 |
| **final** (board) | **step 19 as submitted** (commit 13b0490), S 600, K 25.16; the median of its two runs is 145.9 ms, K 24.96 | run-79d73088c3c3 | **0.1431** | **71.3** | **70.04** |
| 20 | Polish, no change on valid input: `fail` lets only its first caller report (team threads can fail at once); a line with fewer than three numbers fails with a byte offset instead of an index panic, checked on the parser's byte-at-a-time path only; team threads start through `Builder::spawn`; a `Walk` struct names the walk's results; doc fixes. S 618 (+18 over step 19) | | same (local) | 68 MiB (local) | |

Over the five board runs of the v4 code: median 142.1 ms, middle-3 mean 143.2 ms, mean
143.6 ms, M 70.4-71.2 MB. Over the two runs of the submitted step 19: 143.1 and 148.6 ms,
median 145.9 ms.

Where the CPU goes (local, step 18, wall ms / total CPU ms / parallel efficiency, i.e.
CPU / (wall x 12), measured from the cgroup's `cpu.stat` at each phase boundary; a
typical run):

| Phase | Wall | CPU | Efficiency |
|---|---|---|---|
| Parse into the first CSR: count | 6 | 52 | 74% |
| Parse into the first CSR: scatter | 8-14 | 80 | 55-85% |
| Counting sort, then the BFS walk | 18 | 180 | 85% |
| Rank fix-up and layout | 4 | 22 | about 50% |
| In-edge transpose (count, scatter, sort) | 9 | 63 | about 55% |
| Queries | 42 | 465 | 92% |

The queries are about half of all CPU and already run on all 12 threads. The build
phases lose a quarter to a half of their threads to page faults on fresh buffers (kernel
spinlocks and TLB-flush IPIs show up in the profile) and to the barriers between steps.

Peak memory by phase (local, cgroup `memory.current` sampled every 0.1 ms, maximum per
phase, MiB). The board's M is the largest of these, and every v3 step aimed at whichever
phase was on top.

| Phase | v2 (step 12) | v3 (step 16) | What is live |
|---|---|---|---|
| Parse into the first CSR | 40 | 40 | edges 20 + tail side array 10 + offsets 7 |
| BFS walk and layout | **75.6** | 58 | first CSR 27 + copied rows 20 + rank 7 (v2: also the order list, end offsets, claim flags) |
| Read queries.txt | 39 | 38.5 | forward CSR 27 + rank 7 |
| In-edge transpose | 68 | **66.5** | forward 27 + in-edges being built 37 |
| Queries | **77** | **68** | both CSRs 54 + per-search state (v2 18, v3 10) |
| Write output | 60 | 60 | both CSRs + answers |

## Box ablation probes

Local speedups did not always survive the scoring box (turbo off, about 2.5 GHz against
the laptop's ~4.5 GHz boost, DDR4, 1.25 MB of L2 per core shared by two hyperthreads). So
the build was cut into prefixes and each prefix was run on the board. Each probe is v4 with
everything after one point replaced by a placeholder answer (one `0` per query), so the
board measures T and M even though the hash fails.

Rows are ordered by how much of the pipeline each probe runs (the names follow the order
in which they were made), so each row adds one step to the one above.

| Probe | Runs up to | Run ID | Box T (ms) | Box M (MB) | Local T (ms) |
|---|---|---|---|---|---|
| d0 | start-up only: map the file, split it, start the threads | run-94d4d2fef344 | 1.5 | 7.2 | 3.9 |
| d3 | + one full parse (the counting pass) | run-e96c9c86ab33 | 11.1 | 7.7 | 10.3 |
| d4 | + the second parse and the scatter | run-148c3b623aad | 24.7 | 33.0 | 19.1 |
| d2 | + the counting sort and offsets: the whole first CSR | run-9efbc7bcc92f | 43.0 | 40.1 | 23-27 |
| d1 | + BFS walk, layout and in-edges: the whole build, no queries | run-8db29a146c98 | 72.7 | 69.3 | 57-70 |
| v4 | everything (a full correct run) | run-92dc80fd3ba2 | 147.9 | | 108-127 |

By difference on the box: one parse about 9.6 ms, the second parse plus scatter 13.6 ms,
counting sort plus offsets 18.3 ms, BFS + layout + in-edges about 30 ms, and the queries
about 75 ms. The first CSR costs 1.7x its local time, about the clock ratio, but the sort
step costs 4-5x, which the local profile does not explain (its working set per bucket,
about 165 KB, fits in L2). Smaller buckets did not help (see below), so that cost is not
the per-bucket working set.

## Experiments that did not make it

Figures below are local unless a run ID is given.

- **No renumbering at all.** With the file's random ids, the same searches took 4.4x longer
  (step 1's one-way, binary-heap searches: query phase 920 ms against 210 ms of wall time). Locality, not the algorithm,
  is the first-order effect here.
- **Bidirectional search balanced on queue keys.** This is the textbook choice, but a
  source with no in-edges reaches the graph only through two ~2^32 weights. The backward
  side then settled the whole main body before the forward side moved: 0.33 s and 185 MiB.
  Taking turns fixed it.
- **Second CSR built from the first one in memory (at step 5).** It saved 8 ms of parsing
  but kept both graphs alive at once: +24 MiB, so the K change was about zero. Step 7 does
  the same thing with a far smaller footprint (moving rows needs no sort and no side array).
- **Plain-store claims in the BFS**, deduplicating the rare double claim afterwards. It
  made no measurable difference to the load-then-swap claim, which is exact and simpler.
- **Transparent huge pages** via `GLIBC_TUNABLES=glibc.malloc.hugetlb=1`: no gain in the
  query or build phases, and +17 MiB of peak.
- **Binary heap instead of the radix heap**: 7 MiB less memory but about 8% slower overall.
- **Occupancy bitmask in the radix heap** (`trailing_zeros` instead of scanning 65 bucket
  headers): no measurable change.
- **Smaller distance blocks** (32 or 64 nodes): 2-3 MiB less, no change in T, not kept.
- **Shared distance pairs** (`[forward, backward]` per node, so the meeting test reads the
  same line): the query phase went from 44 to 48 ms, because each side's own distances
  got half as dense.
- **Compressed offsets** (a u32 per 16 rows plus a u16 per row): 6 MiB less, but the
  layout went 7 ms slower and the queries 2 ms. Rejected.
- **In-edges read off the near-symmetric out-edges** (the lightest reverse weight as a u16
  next to each out-edge, plus a sorted list for the ~3% one-way edges), replacing the
  transposed CSR: 8 MiB less, but building it took 16 ms against 10, and the filtered,
  chained iteration made backward searches 11 ms slower. Rejected.
- **Pruning pushes by the other side's smallest key** (correct: from a node the other side
  has not settled, at least that much is left): no measurable change.
- **Expanding the side with the smaller queue** instead of taking turns: no change.
- **32-bit distances, judged on speed** (v2). A deliberately unsafe build showed no
  speedup, so they were set aside then. In v3, with memory the target, they came back
  with an exact 64-bit retry (step 14): no T change, 5 MiB less.
- **`malloc_trim(0)` before the queries, smaller distance blocks and heap buffers**: no
  change in M. The query phase's extra memory is the real per-search data (about 0.5 MiB
  for each of the 24 searches at their largest).
- **A 32-bit tail side array** (which drops the atomic-type trait): 9 fewer lines, but the
  transpose was about 2 ms slower. Reverted.
- **In-edges by a direct atomic transpose** (count, then place each edge at its row's
  next slot; cheap after the BFS renumbering because the counters a thread bumps are its
  own): correct and as fast, and its phase drops from 67 to 57 MiB. But the query phase
  stays on top, so M does not move. Removed to save 35 lines.
- **Releasing a big search's distance blocks after the query** (`munmap`, so the peak
  follows the searches running now rather than each thread's largest): -6 MiB, but +25%
  total CPU. Every unmap in a 12-thread process shoots down the TLBs of all its CPUs, and
  the pages fault back in on the next big search.
- **Reusing the first CSR's tail side array for the in-edge build** (it has the same
  size, so no fresh page faults): median CPU 1051 against 1069 ms, within noise, and M
  unchanged. Not worth the extra parameter.
- **Smaller buckets for both bucketed builds** (candidates d7f and d8f). With 1,024-node
  buckets (run-60ca41139fe7) the box measured 151.2 ms, 70.3 MB, K 24.86. With 2,048
  (run-b927b67c49d7) it measured 141.4 ms, 70.5 MB, K 25.70. Both are v4-level or worse
  against 4,096 (141-148 ms), and the same locally. Kept 4,096.
- **Reading graph.txt with `pread` instead of `mmap`** (candidate d10,
  run-8c76e26ef37d: T 143.7 ms, M 70.4 MB, K 25.38). Each thread read its piece in 128 KiB
  chunks into one buffer, carrying the partial last line forward. On track A the same
  change cut box T by 15%. Here it landed inside v4's 141-148 ms spread, and the same
  locally, at S +18. Kept the mapping.
- **A clean-room rework of v4** (box, full correct run, run-2a608e748f34: T 165.2 ms,
  M 73.2 MB, K 23.98). One persistent thread team for all phases, BFS regions capped at
  16K nodes, a per-direction inlined search loop, queries sorted by renumbered source so
  concurrent searches share cache, per-CSR escape tables, and 64-node distance blocks;
  S 551 → 495. Locally it used 8% less CPU (1,430 vs 1,559 ms) and its search phase was
  6% faster, yet the box measured it 12-16% slower than v4. Kept v4. Lesson, again: on this
  box a local CPU win is only a hypothesis until a board run confirms it.
- **Fewer page faults** (box, full correct runs, two each). Track A's largest box wins came
  from cutting page faults, and v4 takes ~32K minor faults per run, so two candidates
  reused buffers across phases: d11 hands each finished phase's arrays to the next (32.1K →
  22.5K
  faults; run-dbc41ae6803b 137.8 ms, run-827ac63d50c0 142.4 ms, both 73 MB), and candidate
  d12 adds huge pages on the big build arrays only (13.7K faults; run-457ba7431807 140.9
  ms, run-3662b39214de 136.3 ms, 72.5-73.1 MB). Mean T 140.1 and 138.6 ms against v4's
  143.6 ms (the mean of its five runs; median 142.1): a 2-3% gain, but +2.5 MB of M and
  S 551 → 589/609 cancel it (K 25.15-25.60 against v4's 25.24-25.80). Kept v4.
- **One parse for the first CSR** (candidate d6, run-c82840f14b70: T 140.3 ms, M 71.4 MB).
  Each thread scatters into its own per-bucket runs, sized from its share of the text
  with a spill list for overflow, and a counting sort per bucket writes the final CSR
  straight from the runs. Locally that phase took 30% less CPU (105-124 against 146-198
  ms), and the whole run 2-3% less. On the box the full run came in at 140.3 against
  v4's 141-148 ms, inside the box's run-to-run spread. It needs a second build function,
  because the in-edges must stay in place to keep their phase under the memory peak:
  S 551 to 662, so K would fall (25.09 against v4's median 25.64). Dropped.
- **Interleaving out- and in-edge rows behind one offset array** (our idea, -5 MiB):
  not tried in the end. Building it within the current peak needs an in-place serial
  expansion of the edge array, and compressed offsets had already shown that the extra
  row arithmetic costs more in T than the saved memory earns in M.

## Lessons

- In a parallel scatter, a locked read-modify-write per element costs more than the parse
  itself, because each one is a full fence. Per-thread cursors into private runs remove it.
- glibc's dynamic mmap threshold quietly turns "freed" into "still resident". Pinning it
  took 40 MiB off the peak with one call.
- For the memory axis, what counts is the peak of the live set per phase. Ordering frees
  (and translating the queries before the transpose) mattered as much as compact layouts.
