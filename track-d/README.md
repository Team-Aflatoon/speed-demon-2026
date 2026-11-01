# Track D: Traverse

Team Aflatoon: Dhruv Sharma (@spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Result

The submitted version (commit 173a349) measured on the board as run-1d538165ea54:
T 140.4 ms, M 70.4 MB, S 551, D 1, K 25.80 (70.58 points). The median of its five board
runs is 142.1 ms. The reference is 72.69 s and 893 MB. PERF.md has every step with its
board run.

Where the time goes on the scoring box, from prefix probes (PERF.md):

| Phase | Box time (ms) |
|---|---|
| First parse of graph.txt (counting) | ~10 |
| Second parse and scatter | ~14 |
| Counting sort and offsets | ~18 |
| BFS renumbering, layout and in-edges | ~30 |
| 2,000 queries | ~75 |

Notable: transparent huge pages cost 14 MB on the box only, and opting out of them fixed
it (run-bf037ba42528).

## How to run

    docker build -t traverse .
    docker run --rm -v /path/to/data:/data:ro -v /path/to/out:/out traverse

or, with Rust installed, `cargo build --release` and then
`target/release/traverse [data dir] [out dir]` (by default /data and /out). The data
directory holds `graph.txt` and `queries.txt`, and the answers go to `distances.txt`.

## Approach

The whole run is one Rust binary, `src/main.rs`, with no dependencies. Most of the work
goes into getting the graph into a compact, cache-friendly shape quickly, because that is
what our measurements said matters: renumbering the nodes for locality alone made the
searches 4.4x faster (measured in our first version, with one-way binary-heap searches). The 2,000 searches themselves are short.

**Parsing.** `graph.txt` is memory-mapped (the harness has it in the page cache already, so
this costs no copy and no charged memory) and cut into one piece per thread at line
boundaries. Numbers are parsed eight bytes at a time. Digits have bit 4 set and the two
separators do not, so a mask finds the end of a number. The digits are then combined with
three multiplications (Lemire's eight-digit trick) rather than a loop per byte.

**Building the CSR without atomics.** A naive parallel CSR build bumps a shared counter per
edge, and on x86 every such locked instruction is a full fence, which serialises the cache
misses. Instead, nodes are grouped into buckets of 4,096. Pass 1 counts edges per
(thread, bucket), which gives every thread a private run inside every bucket. Pass 2 writes
each edge into its run with plain stores. Finally each bucket is counting-sorted by tail on
its own thread, and its histogram doubles as that bucket's row offsets.

**Compact edges.** An edge is a single `u32`: the head in the high bits and the weight in
the rest (21 + 11 bits at full scale). All but about 0.1% of road weights fit in 11 bits.
A heavier edge stores an all-ones escape value and its real weight sits in a small sorted
side table keyed by (tail, head), which keeps only the lightest of any parallel group,
since no shortest path uses the others. The graph is 4 bytes per edge plus 4 per node, in
each direction.

**Renumbering for locality.** Node ids in the file are a random permutation, so a search
over the raw graph misses the cache on almost every node. After the first build, the nodes
are renumbered in BFS order. Every thread grows BFS regions from roots in its own share of
the ids, claiming nodes with an atomic flag, until the regions meet. Inside a region, BFS
rings put neighbouring streets at nearby ids (the 4.4x above).
Each thread copies every row as it dequeues it, so its rows come out already in new-id
order. During the walk `rank` itself is the claim flag (a provisional thread-and-index id,
fixed up afterwards), and the queue holds only the frontier, so the walk adds little
memory beyond the copied rows. Laying out the forward graph is then one sequential copy
that renames the heads, written into the first graph's own arrays (same sizes), which
saves 27 MB of fresh page faults. The in-edge graph is a transpose of that, built in
memory with the same bucketed build.

**One team of threads.** The ten parallel phases share a single team of 12 threads,
started once and woken per phase by a barrier, with the main thread as member 0. Spawning
a fresh set per phase had cost about 5 ms.

**Queries.** Each of the 12 threads takes queries from a shared counter and answers them
with bidirectional Dijkstra: forward from `s` on the out-edges and backward from `t` on the
in-edges, stopping once the two queue minima sum to at least the best meeting length.
- The sides take turns instead of balancing on keys. Balancing on keys lets the backward
  side sweep the whole graph when the source sits behind a 2^32 weight.
- An exhausted side ends the search immediately, which answers the unreachable cases
  (no out-edges, no in-edges, islands) almost for free.
- The priority queue is a radix heap, which suits Dijkstra's monotone integer keys.
- Distances and heap keys are 32-bit. A query whose distances pass 2^32 - 2 (only
  possible through near-2^32 weights) is detected and answered again with 64-bit
  searches, so every answer is exact. That halves the searches' memory.
- Tentative distances live in per-thread blocks of 128 nodes that are allocated only where
  a search actually goes, so memory follows the size of the search rather than the graph.
- When a node is first queued, its edge row is prefetched, since it is needed as soon as
  the node is settled.

**Memory.** Peak memory is the graph plus whichever phase is live, so phases free what they
no longer need (the BFS numbering is dropped as soon as the queries are translated).
glibc's mmap threshold is pinned at 1 MiB, because left dynamic it grows after the first
large free, and later large buffers would then come from arenas that never return memory.
The process also opts out of transparent huge pages (`prctl(PR_SET_THP_DISABLE)`). With
THP on, a buffer touched in a few places is charged in 2 MiB pages. The first board run
measured 93.5 MB, against 77 MiB on our machine, where THP is opt-in only. The next board
run, with the opt-out, measured 79.8 MB.

Correctness was checked against the reference on the spec's worked example and the hand-written
cases from the official tests, 60 random small graphs (zero and near-2^32 weights, parallel
edges, self-loops, unreachable pairs), one-edge graphs, tiny seeds 1-8, sample seeds 1-3
and full seeds 1, 3, 7, 101 and 202, with 1, 3 and 12 CPUs, plus 100 repeated runs of
seed 1 with identical output.

## Limits

- n ≤ 2^26 nodes: a head id and at least 6 bits of weight share each 32-bit edge.
- m < 2^32 edges, since row offsets are u32.
- Weights are u32 (0 to 2^32 - 1), as the spec says; answers stay below 2^63.
- At most 12 threads (`MAX_THREADS`), the number the harness grants.
- Each of the 48 searches (two directions, two widths, 12 threads) reserves address space
  for a distance block per 128 nodes, about 0.5 GB in all at full scale. Only the blocks a
  search touches use memory, so this relies on Linux overcommitting virtual memory.

## Errors

The input is trusted to follow the spec's format, and every violation that is cheap to
detect is reported on stderr as `track-d: ...`, with the file's path where it applies,
and exit status 1. That covers an empty or unreadable file, a missing final newline, a
bad header, n above 2^26 or m above 2^32 - 1, a node id not below n, an edge count unlike the header's, a
weight above 2^32 - 1, a query line that is not `s t` with both below n (with its line
number), and an output file that cannot be written. Checking every byte of every line
would cost the parse its speed, so a line that is malformed in other ways can still
produce a wrong answer or abort the run.

## Dependencies

None. The three libc functions used (`mmap`, `mallopt`, `prctl`) are declared by hand.
The image builds offline (`cargo build --release --offline --locked`) into a static
binary.

## With another 24 hours

Ideas we measured and dropped are in PERF.md, including four that were checked on the
scoring box itself: one parse for the first CSR, smaller sort buckets, `pread` instead of
`mmap`, and a clean-room rework. What is left to try:

- Find out why the first CSR's counting sort costs 4-5x its local time on the box (about
  18 ms of the 43 ms phase, from the probe split in PERF.md). Neither smaller buckets nor
  sorting straight into the final array changed it.
- Lower the two memory peaks together. The in-edge transpose and the queries now both
  sit at about 67 MiB. A direct atomic transpose already takes the first down to 57 MiB,
  and compressing the row offsets would take about 6 MiB off both. Each pays only if the
  other moves too.
- Goal-directed search (ALT landmarks) would cut the searches, the largest phase, but one
  full Dijkstra per landmark costs more than it saves at 2,000 short queries. Worth
  revisiting only with a much cheaper way to get the landmark distances.
