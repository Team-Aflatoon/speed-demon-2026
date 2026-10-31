# Track A: Ingest

Team Aflatoon: Dhruv Sharma (@Spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

One Rust binary, `src/main.rs`, no crates. Eleven worker threads parse the memory-mapped
log and report its hours while they parse; the main thread writes the output. PERF.md has
the history and the measurements behind each choice (v7 is this design).

**Parsing into one log per hour.** The file is mapped read-only (the harness has it in the
page cache, so the mapping costs page-table entries and is not charged to our memory) and
cut into 1 MB pieces of whole lines, which workers take in file order. Every line becomes
one 4-byte record, `endpoint id << 21 | 5xx << 20 | latency`, appended to its hour's log.
Each hour's log is one region of a single `MAP_NORESERVE` reservation sized for the worst
case (every line of the file in that hour), so untouched pages cost nothing. Workers
reserve 256-record blocks of a log with one atomic `fetch_add` and fill their own block
with plain stores. Latencies of 2^20 and up, about 1.5% of them, go to the hour's side list
as 8-byte entries. Slots are atomics, which on x86 are ordinary moves; that matters only
for the early reports below.

**Reporting while parsing.** The parsed part of the file ends at the first piece still
being parsed. Once that piece's first line is two hours past hour h, hour h is reported:
the generator's shard 0 uploads each hour 5 to 10 minutes after it ends and the streaming
shards are at most 2.5 minutes late, so this leaves about 50 minutes of margin. The
scheduler (one lock and a condition variable) gives due hours priority over pieces, so
reports are spread over the run instead of waiting behind a barrier, and the writer starts
within about 2 ms. When parsing ends, only the last two hours are left to report.

**Exact whatever the input.** The spec promises no bound on how late a line can be, so the
speculation is checked, not trusted. A report counts the records it saw; a slot reserved
but not yet written reads as zero, endpoint id 0, which is never printed. Each line is
exactly one record in its hour, so the reports saw every line exactly when their counts
add up to the number of lines parsed. If they do not, the output is truncated and the
whole job runs again with every report at the end. On the generator's data that never
happens; a shuffled file, a reversed file or a late line make it happen, and still give
the right bytes, in about twice the time.

**One report.** A count pass over the hour's records gives requests and 5xx per endpoint
(one 64-bit add per record); a prefix sum in endpoint byte order gives every endpoint a
slice of a scratch array; a scatter pass drops each latency into its slice. Groups of up to
32 are sorted; larger ones get three `select_nth_unstable` calls, each searching only above
the previous rank. Then the hour's log pages are given back with `madvise(MADV_DONTNEED)`,
so the logs shrink while the output grows. Byte order of the names is a sorted list of
endpoint ids, rebuilt only when a report meets an id it does not cover. Numbers are
formatted eight digits at a time by SWAR arithmetic, with the leading zeros shifted out,
so there is no loop whose length depends on the value.

**The per-line path.** Line ends are indexed 4 KB at a time: each 64-byte block (two AVX2
compares and `movemask`) holds at most two line ends, so both candidates are stored and the
count advances by the popcount, with no branch per block. The hour is cached on two
overlapping words of the timestamp and recomputed (days from the civil date) only when
they change. The latency is parsed right to left from the newline with no loop: one SSE
compare finds the space before it, its digits are masked in and summed by three
multiply-add steps, and the status digit before that space gives the 5xx flag. One more
16-byte compare finds the space after the method, where the endpoint starts.

**Endpoints.** One table, shared by all workers and kept in a `static` so the hot loop
addresses it directly: open addressing over 8,192 slots on a hash of the name's first and
last 8 bytes and its length, each name stored once, zero-padded, in a 128-byte record. A
lookup compares 64 bytes at a time with AVX2 and masks off the bytes past the length with
BMI2 `bzhi`; the hash is only a shortcut and equality is always decided on the bytes.
Lookups take no lock. An insert takes one, looks again, writes the name and then publishes
its id with a Release store.

**Memory.** At the peak the process holds the output's tmpfs pages (10.1 MB), the records
of the few hours in flight, report scratch and texts. `prctl(PR_SET_THP_DISABLE)` keeps
transparent huge pages (`always` on Debian) off the sparse reservation; a fixed
`M_MMAP_THRESHOLD` stops glibc from raising its threshold after the first large free, so
freed text buffers really go back to the kernel; the writer keeps at most four text buffers
and the report scratch is pooled, so there are only as many of them as reports in flight.

**Errors.** An I/O failure, a timestamp outside the 72-hour span, or more than 2,047
distinct endpoints goes to stderr with exit code 1. `unsafe` is confined to the system
calls, the reserved logs, the SIMD intrinsics (AVX2, BMI2 and SSE4.1 come from
`target-cpu=x86-64-v3` in `.cargo/config.toml`) and the output writer; each block says why
it is sound.

## Dependencies

None. `mmap`, `madvise`, `prctl` and `mallopt` are declared by hand with `extern "C"`;
SIMD comes from `std::arch`.

## With another 24 hours

- Selection is the largest single cost left: running without it (wrong output, as a bound)
  takes 10% less time and a sixth fewer instructions. A one-pass selection that brackets
  the three ranks from a small sorted sample would get much of that back; our radix-sort
  attempt removed the mispredicts but cost more than it saved.
- On the box the bare read of the mapped input is 14.6 ms of our 24.5 ms. Reading with
  `pread` into per-thread buffers would avoid the input's page faults and the teardown of
  its mapping at exit; whether that is cheaper on the box is the next thing to measure.
- The ingest loop is about 150 instructions a line, and the compiler spills several of its
  loop variables; a leaner line path is worth another 10-15%.
