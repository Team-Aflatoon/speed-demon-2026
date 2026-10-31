# Track D: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

T and M below are local measurements on an i7-14650HX, cpuset 0-11, under the harness's
sandbox flags, with cgroup `memory.peak` in MiB. Unless a row says otherwise they come from
`bench/bench.sh` or an equivalent fresh-container run. The scoring box is roughly 1.5x
slower. For part of the work the machine was shared with other benchmark jobs, so close
calls were decided by interleaved A/B runs of saved binaries, or by repeating a phase
several times in one process and taking the minimum. Such rows are noted. Run IDs are
added once a step has a board run.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 72.69 | 893 | 0 |
| 1 | Rust v1: mmap, parallel parse with an atomic per-node counter, CSR, serial BFS renumbering, one Dijkstra per query (binary heap) on 12 threads, block-allocated distances | | 0.42 (local) | n/a | |
| 2 | Bucketed CSR build with per-thread cursors and a counting sort, so no locked instruction per edge (parse 24 to 11 ms per pass); BFS from one root per thread in parallel; radix heap | | 0.22 (local) | 87 (local) | |
| 3 | SWAR parsing: eight bytes per load, separator found by bit 4, Lemire's multiply combine (11 to 5.5 ms per pass) | | 0.21 (local) | 87 (local) | |
| 4 | Bidirectional Dijkstra on an extra in-edge CSR, sides taking turns (query phase 127 to 47 ms; settled nodes 24.9M to 7.6M) | | 0.145 (local) | 129 (local) | |
| 5 | Out- and in-edges built in the same two passes over the text (one CSR, 2n rows) | | 0.125 (local) | 129 (local) | |
| 6 | 4-byte edges: head and 11-bit weight in one u32, heavier weights in an escape table; glibc mmap threshold pinned so freed phases leave the peak | | 0.138 to 0.127 (local, A/B) | 121 to 82 (local) | |
| 7 | Forward graph by moving rows into BFS order, in-edges by an in-memory transpose (no second parse); BFS order freed before the move; distance blocks reserved up front; oversized heap buckets returned | | 0.126 to 0.114 (local, A/B) | 77 (local) | |
| 8 | BFS threads keep rooting new regions in their own id range (balanced, no serial sweep); edge row prefetched when a node is first queued | | 0.108 (local, bench median of 5; seed 7: 0.110) | 77 (local) | |
| 8 (board) | v1 as submitted (commit 984cb84) | run-0b83abcdda4d | 0.156 | 93.5 | 68.5 |
| 9 | Opt out of transparent huge pages (`prctl(PR_SET_THP_DISABLE)`). Box-only: the box measured 93.5 MB against 77 locally, where THP is opt-in; the call shows `THP_enabled: 0` in /proc/self/status and changes nothing locally | | 0.108 (local) | 77 (local) | |
| 10 | One persistent team of 12 threads woken by a barrier, instead of spawning threads for each of the 11 parallel phases (11 empty spawn rounds measured 5 ms) | | 0.110 to 0.107 (local, A/B medians) | 77 (local) | |
| 11 | The BFS walk copies each row as it dequeues it, so the forward graph is laid out in new-id order with one sequential renaming copy; `permute` and its random-access passes are gone (BFS + layout 24 to 21-24 ms, in-process minima) | | 0.105 (local) | 77 (local) | |
| 12 | The layout reuses the first graph's arrays (27 MB fewer page faults); S trims (shorter if/else and answer conversion, fewer attributes): S 553 to 525 | | 0.122 vs v1 0.135 (local, interleaved medians under heavy machine load) | 78 (local) | |
| 13 | Leaner BFS phase: `rank` is the claim flag (0 until claimed, then a provisional thread-and-index id fixed up once counts are known), the queue holds only the frontier, degrees take a byte each. Drops the order list, claim flags and end offsets (BFS + layout phase 75.6 to 58 MiB) | | same CPU (local) | 76 (local) | |
| 14 | 32-bit tentative distances; a query whose distances pass 2^32 - 2 is answered again with 64 bits, so results stay exact (query phase 76 to 71 MiB) | | same CPU: 1000 vs 1020 ms total (local) | 71 (local) | |
| 15 | The radix heap keys in the search's width too: a 32-bit search only queues keys that fit, so entries take 8 bytes, not 16 (query phase 71 to 68 MiB) | | CPU 1075 to 1035 ms total (local, A/B medians) | 68 (local) | |
| 16 | S trims: one blanket `Width` trait for the bounds, `distance` takes the pair of searches, in-edges back to `build_csr`, unused helpers removed: S 600 to 538 | | same (local) | 68 (local) | |

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

## Experiments that did not make it

- **No renumbering at all.** With the file's random ids, the same searches took 4.4x longer
  (query phase 920 ms against 210 ms in step 1). Locality, not the algorithm, is the first
  order effect here.
- **Bidirectional search balanced on queue keys.** This is the textbook choice, but a
  source with no in-edges reaches the graph only through two ~2^32 weights. The backward
  side then settled the whole main body before the forward side moved: 0.33 s and 185 MB.
  Taking turns fixed it.
- **Second CSR built from the first one in memory (at step 5).** It saved 8 ms of parsing
  but kept both graphs alive at once: +24 MB, so the K change was about zero. Step 7 does
  the same thing with a far smaller footprint (moving rows needs no sort and no side array).
- **Plain-store claims in the BFS**, deduplicating the rare double claim afterwards. It
  made no measurable difference to the load-then-swap claim, which is exact and simpler.
- **Transparent huge pages** via `GLIBC_TUNABLES=glibc.malloc.hugetlb=1`: no gain in the
  query or build phases, and +17 MB of peak.
- **Binary heap instead of the radix heap**: 7 MB less memory but about 8% slower overall.
- **Occupancy bitmask in the radix heap** (`trailing_zeros` instead of scanning 65 bucket
  headers): no measurable change.
- **Smaller distance blocks** (32 or 64 nodes): 2-3 MB less, no change in T, not kept.

- **Shared distance pairs** (`[forward, backward]` per node, so the meeting test reads the
  same line): the query phase went from 44 to 48 ms, because each side's own distances
  got half as dense.
- **Compressed offsets** (a u32 per 16 rows plus a u16 per row): 6 MB less, but the
  layout went 7 ms slower and the queries 2 ms. Rejected.
- **In-edges read off the near-symmetric out-edges** (the lightest reverse weight as a u16
  next to each out-edge, plus a sorted list for the ~3% one-way edges), replacing the
  transposed CSR: 8 MB less, but building it took 16 ms against 10, and the filtered,
  chained iteration made backward searches 11 ms slower. Rejected.
- **Pruning pushes by the other side's smallest key** (correct: from a node the other side
  has not settled, at least that much is left): no measurable change.
- **Expanding the side with the smaller queue** instead of taking turns: no change.
- **32-bit distances**, measured with a deliberately unsafe build first to see the upside:
  no speedup, so not worth the 64-bit fallback.
- **`malloc_trim(0)` before the queries, smaller distance blocks and heap buffers**: no
  change in M. The query phase's extra memory is the real per-search data (about 0.5 MB
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
- **Interleaving out- and in-edge rows behind one offset array** (the lead's idea, -5 MiB):
  not tried in the end. Building it within the current peak needs an in-place serial
  expansion of the edge array, and compressed offsets had already shown that the extra
  row arithmetic costs more in T than the saved memory earns in M.

## Lessons

- In a parallel scatter, a locked read-modify-write per element costs more than the parse
  itself, because each one is a full fence. Per-thread cursors into private runs remove it.
- glibc's dynamic mmap threshold quietly turns "freed" into "still resident". Pinning it
  took 40 MB off the peak with one call.
- For the memory axis, what counts is the peak of the live set per phase. Ordering frees
  (and translating the queries before the transpose) mattered as much as compact layouts.
