# Track A: Ingest

Team Aflatoon: Dhruv Sharma (@spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

One Rust binary, `src/main.rs`, no crates. Two parallel phases over a memory-mapped input.

**1. Ingest (one pass over the bytes).** The file is `mmap`ed read-only. The harness has
already pulled it into the page cache, so the mapping costs only page-table entries and
none of its pages are charged to our memory peak. The file is cut into pieces of whole
lines (~1 MB each). Each of up to 12 workers (one per CPU of the cpuset) owns a
contiguous run of pieces and works through it front to back; a worker that runs out
steals pieces from the back of another's run. So a worker that started late or shares a
core just does less, all finish within a piece of each other, and each still sees only a
few hours of the file. Each worker finds its pieces' line-aligned edges itself (a piece
holds the lines that start in its nominal range). Within a piece, every field of a line
is read from one fixed 320-byte window of it at offsets that are `u8`s, so the compiler
can prove each read in bounds and drops the checks (they were about 100 instructions a
line); only the file's last few lines need a zero-padded copy of the window.

- *Line ends:* 64 bytes at a time, two AVX2 compares and `movemask` give a bitmask of
  newlines. A line has at least 40 bytes, so a block holds at most two: both candidates
  (lowest and highest set bit) are stored unconditionally and the count advances by the
  popcount. A 4 KB stretch is indexed this way first, so the per-line loop that follows
  has no hard-to-predict exit at every block boundary.
- *Hour:* lines arrive in roughly time order, so the hour is cached on the 13 bytes
  `YYYY-MM-DDTHH`; only when they change is it recomputed (days-from-civil, so any
  calendar date works) and turned into a row relative to the first line's hour. Nothing
  assumes the spec's span start: any 72-hour window around the first line fits.
- *Latency and status:* parsed right to left from the newline with no loop. One 16-byte
  compare finds the space before the latency (at most 10 digits), the digits after it are
  masked in and summed as a 16-digit number by three multiply-add steps
  (`maddubs`/`madd`), and the status is the 3 bytes before that space (`5xx` = error).
- *Endpoint:* its leading `/` is one of the 8 bytes at offset 32 (27-byte timestamp, a
  space, a 3 to 7 byte method, a space); a SWAR zero-byte test finds it. Its end is 4 bytes
  before the latency's space. It is hashed from its first and last 8 bytes and its length
  and looked up in a per-worker open-addressing table whose names live in one compact
  arena of records (hash, id, length, name): a hit reads its slot and then one record.
  (Comparing against each name's first occurrence in the input instead scattered the
  compares over ~1,842 pages and cost a TLB miss on most lines.) The compare is two
  32-byte vector compares with the bits past the name's length masked off. The hash is only
  a shortcut: equality is always decided on the bytes.
- *Storing the line:* one 4-byte record, `endpoint id << 20 | 5xx << 19 | latency`, is
  appended to this worker's log for that hour. 97% of latencies fit in 19 bits; a larger
  one is stored as an all-ones marker plus an (id, latency) pair on the side, so the result
  stays exact. The log grows in 256 KB chunks, each its own mapping, faulted in as written.
  Appending is a sequential write: no per-group counters or chains to update per line.

**2. Report and write, overlapped.** The workers' endpoint tables are merged through one
more hash table (12 x 1,842 lookups) and the ~1,842 distinct names are sorted once; byte
order is exactly `&[u8]`'s `Ord`. Then the workers take the hours in order. For an hour, the
logs of the workers that saw it are counting-sorted by endpoint rank: one pass counts lines
and errors per endpoint, a prefix sum gives each endpoint a slice of one array, a second
pass drops every latency into its slice. Every group is then contiguous, and
`select_nth_unstable` finds p50, then p95 in the half above it, then p99 in the twentieth
above that: about 1.55n steps, no sort (groups of 32 or fewer, most of them, are simply
insertion-sorted). The counting pass also rewrites each record's worker-local id as the
endpoint's rank, so the second pass needs no lookup. Digits are written ten places at a
time from a two-digit table, with no loop whose length depends on the value; text
buffers are reserved once and reused through a pool. As soon as an hour is reported its log chunks are unmapped,
so the logs shrink while the output grows. The main thread writes each hour's text as soon
as all earlier hours are out, so writing overlaps the work.

**Memory.** The peak is near the end of ingest: the 3.1M records (12.4 MB), the endpoint
tables and the first hours in flight. `prctl(PR_SET_THP_DISABLE)` keeps transparent huge
pages, `always` on stock Debian, from backing malloc's per-thread arenas with mostly empty
2 MB pages.

**Errors.** Every I/O failure, a timestamp outside a 72-hour window, or more than 3,072
distinct endpoints is reported on stderr with a non-zero exit. `unsafe` is used only for
the `mmap` and `prctl` calls and the AVX2 loads (each with why it stays in bounds).

## Dependencies

None. `mmap` and `prctl` are declared by hand with `extern "C"`; SIMD comes from
`std::arch`.

## With another 24 hours

- The ingest loop is now the bulk of the time (about 11 ms locally against a ~7 ms floor
  for merely streaming the input); the endpoint lookup is ~20% of all samples. Next: a
  cheaper first-level check for the lookup (most lines hit a handful of hot endpoints)
  and fewer bounds checks in that loop.
- The report phase still idles about 30% (rows finish out of order and the writer must
  go in order); finishing the formatting of a big hour in two halves would shorten the
  tail.
- Fewer lines (S): the report pipeline and the endpoint table can be written more
  compactly.
- Memory: two-byte latencies with a wider escape would bring the logs to ~8 MB, at the
  cost of more side pairs; worth measuring against the counting sort's cost.
