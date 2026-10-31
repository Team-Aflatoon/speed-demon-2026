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

## Lessons

- In a parallel scatter, a locked read-modify-write per element costs more than the parse
  itself, because each one is a full fence. Per-thread cursors into private runs remove it.
- glibc's dynamic mmap threshold quietly turns "freed" into "still resident". Pinning it
  took 40 MB off the peak with one call.
- For the memory axis, what counts is the peak of the live set per phase. Ordering frees
  (and translating the queries before the transpose) mattered as much as compact layouts.
