# Track D: Traverse

Team Aflatoon: Dhruv Sharma (@spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

The whole run is one Rust binary, `src/main.rs`, with no dependencies. Following the spec's
hint that the queries are local and that parsing and memory layout dominate, most of the
work goes into getting the graph into a compact, cache-friendly shape quickly. The
2,000 searches themselves are short.

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
rings put neighbouring streets at nearby ids. This alone made the searches about 4x faster.
Each thread copies every row as it dequeues it, so its rows come out already in new-id
order. Laying out the forward graph is then one sequential copy that renames the heads,
written into the first graph's own arrays (same sizes), which saves 27 MB of fresh page
faults. The in-edge graph is a transpose of that, built in memory with the same bucketed
build.

**One team of threads.** The dozen parallel phases share a single team of 12 threads,
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
- Tentative distances live in per-thread blocks of 128 nodes that are allocated only where
  a search actually goes, so memory follows the size of the search rather than the graph.
- When a node is first queued, its edge row is prefetched, since it is needed as soon as
  the node is settled.

**Memory.** Peak memory is the graph plus whichever phase is live, so phases free what they
no longer need (the BFS numbering is dropped as soon as the queries are translated).
glibc's mmap threshold is pinned at 1 MiB, because left dynamic it grows after the first
large free, and later large buffers would then come from arenas that never return memory.
The process also opts out of transparent huge pages (`prctl(PR_SET_THP_DISABLE)`). With
THP on, a buffer touched in a few places is charged in 2 MiB pages: the first board run
measured 93.5 MB, against 77 MB locally where THP is opt-in only.

Correctness was checked against the reference on the spec's worked example and the hand-written
cases from the official tests, 60 random small graphs (zero and near-2^32 weights, parallel
edges, self-loops, unreachable pairs), tiny seeds 1-8, sample seeds 1-3 and full seeds 1, 3
and 7, with 1, 3 and 12 CPUs.

## Dependencies

None. The three libc functions used (`mmap`, `mallopt`, `prctl`) are declared by hand.

## With another 24 hours

Ideas we measured and dropped are in PERF.md: compressed offsets, deriving in-edges from
the near-symmetric out-edges, shared distance pairs, and pruning by the other side's
radius. What is left to try:

- Fuse the first count pass into the scatter: parse each piece once into per-thread
  buckets sized from the piece length, with a spill list for overflow. That saves one pass
  over the text (about 5 ms) if the memory can be kept under the query-phase peak.
- Lower both memory peaks together: the transpose (16-bit tail side array) and the
  query-phase distances (32-bit storage with a 64-bit fallback for the rare search that
  crosses 2^32). Each alone does not move the peak.
- Goal-directed search (ALT landmarks) would cut the searches, the largest phase, but one
  full Dijkstra per landmark costs more than it saves at 2,000 short queries. Worth
  revisiting only with a much cheaper way to get the landmark distances.
