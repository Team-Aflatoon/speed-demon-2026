# Track A: Ingest

Team Aflatoon: Dhruv Sharma (@spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

One Rust binary, `src/main.rs`, no crates. It runs in three parallel phases over a
memory-mapped input.

**1. Ingest (one pass over the bytes).** The file is `mmap`ed read-only. The harness has
already pulled it into the page cache, so the mapping costs only page-table entries and
none of its pages are charged to our memory peak. The file is cut into one range of whole
lines per worker (up to 12, one per CPU of the cpuset) and every worker scans its range:

- *Line splitting:* 64 bytes at a time, two AVX2 compares and `movemask` give a bitmask of
  newlines; each set bit is a line end.
- *Hour:* lines arrive in roughly time order, so the hour is cached on the 13 bytes
  `YYYY-MM-DDTHH`; only when they change is the hour recomputed (days-from-civil, so any
  calendar date works) and turned into a row index relative to the first line's hour.
  Nothing assumes the spec's span start: any 72-hour window around the first line fits.
- *Latency and status:* parsed right to left from the newline with no loop. One 16-byte
  compare finds the space before the latency (at most 10 digits), the digits after it are
  masked in and summed as a 16-digit number by three multiply-add steps
  (`maddubs`/`madd`), and the status is the 3 bytes before that space (`5xx` = error).
- *Endpoint:* its leading `/` is one of the 8 bytes at offset 32 (27-byte timestamp, a
  space, a 3 to 7 byte method, a space); a SWAR zero-byte test finds it. Its end is 4 bytes
  before the latency's space. The name is hashed from its first and last 8 bytes and its
  length, and looked up in a per-worker open-addressing table. Names are copied into one
  compact per-worker arena so the compare touches a few hot cache lines (comparing against
  the first occurrence in the input scattered the compares over ~1,842 pages and cost a TLB
  miss on most lines). The compare is two 32-byte vector compares with the bits past the
  name's length masked off: no length-dependent branch and no `memcmp` call. The hash is
  only a shortcut: equality is always decided on the bytes, so the result is exact.
- *Storing latencies:* each worker has a dense table `row * 4096 + endpoint id ->
  [count, 5xx count, newest block]`. It is allocated zeroed and only the rows a worker
  actually meets are ever touched, so it costs a few hundred KB, not its 7 MB of address
  space. Every latency is appended to its group's chain of 16-latency (one cache line)
  blocks, taken from a per-worker bump arena. Exact nearest-rank percentiles need every
  value, so all 3.1M latencies are kept, but nothing else per line is.

**2. Merge and summarise.** The workers' endpoint tables are merged by inserting every
name into one more table (cheap: 12 x 1,842 lookups) and sorting the ~1,842 distinct
names once; byte order is exactly `&[u8]`'s `Ord`. Each hour is then cut into four tasks
by endpoint rank (hours differ fivefold in traffic, so this balances the threads), and
for every (hour, endpoint) the latencies are gathered from the chains of the workers that
saw that hour into a scratch vector, where `select_nth_unstable` finds p99, then p95 in
the prefix left of it, then p50 in the prefix left of that: linear time, no sort. Only a
24-byte record per output line is kept.

**3. Output.** The chains are freed first, so their memory and the output's page cache
never count towards the peak at the same time. The tasks are formatted in parallel (a
small hand-written integer formatter) and written in order with one writer: concurrent
`pwrite`s to one file just queue on its inode lock.

**Memory.** The peak is the latency chains (about 19 MB for 3.1M latencies, including the
partly filled last block of each worker's groups), the touched rows of the group tables
and the per-line records of phase 2.

**Errors.** Every I/O failure, a timestamp outside a 72-hour window, or more than 4,096
distinct endpoints is reported on stderr with a non-zero exit. `unsafe` is used only for
the `mmap` call, the AVX2 loads (each documented with why it stays in bounds) and nowhere
else.

## Dependencies

None. `mmap` is declared by hand with `extern "C"`; SIMD comes from `std::arch`.

## With another 24 hours

- Cut the per-line cost of the endpoint lookup further (it is the largest single cost in
  a profile): keep the hash tag and the first bytes of each name next to the slot so a hit
  needs one cache line, and interleave two independent line streams per thread so their
  load chains overlap.
- Shrink the latency store: three bytes per latency with an escape for the rare values of
  2^24 and above, and smaller first blocks for the many small groups, to bring the peak
  down towards the 12 MB the raw latencies need.
- Overlap the output with the summarise phase through an aligned `O_DIRECT` writer, which
  keeps the report out of the page cache, once the chains are small enough that the two
  can coexist under the peak.
