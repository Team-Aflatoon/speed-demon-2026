# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

T and M are local measurements unless a row has a run ID: `bench/bench.sh` or the same
sandbox flags (cpuset 0-11 of an i7-14650HX, cgroup `memory.peak`). The machine was
shared with two other benchmark jobs for most of the work, so absolute local numbers
drift by 30% or more between minutes. Every keep-or-drop decision was made on
interleaved A/B runs of saved binaries (median and minimum), on total CPU time
(`perf stat` task-clock, which load disturbs less) or on single-core runs, and the row
says which. The first board run showed the box at about 2x our quiet local wall time.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | Rust v1: mmap, 12 workers, per-worker endpoint hash table, per-group block chains, scalar newline and latency loops, merge by sorting all workers' names, per-hour formatting, one write | | 0.039 (local) | 37.5 (local) | |
| 2 | AVX2 newline bitmask; loop-free SIMD latency parse; skip workers that never saw an hour when summarising | | 0.030 (local) | 37 (local) | |
| 3 | Merge endpoint tables through one more hash table and sort the 1,842 survivors, instead of sorting 22,104 names (that phase: 5-6 ms to 0.9 ms) | | 0.029 (local) | 37 (local) | |
| 4 | Copy names into a compact per-worker arena (compares against first occurrences in the input cost a TLB miss on most lines); check the stored hash before comparing bytes; fixed-size compares instead of `memcmp` | | 0.023 (local) | 37 (local) | |
| 5 | Summarise into 24-byte records first, free the chains, then format in parallel: the output's page cache and the chains no longer peak together | | 0.023 (local) | 35 (local) | |
| 6 | Branchless name compare: two 32-byte AVX2 compares with the bits past the length masked | | min 24.1 vs 25.7 ms, interleaved A/B | 35 (local) | |
| 7 | Power-of-two blocks with a separate link array (a mask instead of `% 15` per line); each hour split into four tasks by endpoint rank for balance; one sequential writer | | 0.023 quiet, 0.032-0.038 under load (local); single core 99 to 96.6 ms | 33 (local) | |
| 8 | v1 as submitted. Fewer lines for S (rank + five numbers as one tuple struct, splatted SIMD weights): 456 to 421 Rust lines after rustfmt; no change in T | run-a288ae43d4e8 | 0.0461 | 47.5 | 71.3 |
| 9 | Line ends found per 4 KB in bulk: both candidate newlines of each 64-byte block are stored unconditionally and the count advances by popcount, so the per-line loop has no unpredictable exit at every block boundary | | single core min 94.8 vs 104.5 ms; 12 threads min 30.6 vs 32.2 ms, interleaved | 33 (local) | |
| 10 | Endpoint table as one arena of records (hash, id, length, name) so a hit reads its slot and then one record; 4,096 slots | | neutral within noise (min 98.3 vs 99.2 ms single core); kept for the smaller footprint | 33 (local) | |
| 11 | Report and write overlapped: workers report the hours in order and the main thread writes each one as soon as all earlier ones are out (buffered writes; a bounded channel caps the texts in flight) | | min 28.7 vs 33.9 ms, median 38.6 vs 46.3 ms, interleaved, under load | 36 (local) | |
| 12 | Per-worker, per-hour logs of 4-byte records (`id << 20 \| 5xx << 19 \| latency`, latencies of 2^19 and up kept aside) in 256 KB chunks; each hour reported by counting-sorting its logs by endpoint into one array, so every group is one contiguous slice; each hour's chunks are freed (unmapped) as soon as it is reported, so the logs shrink while the output's page cache grows | | CPU 128 vs 159 ms; min 22.8 vs 26.1 ms, interleaved | 25-28 (local) | |
| 13 | `prctl(PR_SET_THP_DISABLE)` at start: the box's M was 47.5 MB against about 32 MB locally, consistent with transparent huge pages set to `always` there (Debian's default) backing malloc's per-thread arenas with 2 MB pages. Locally THP is `madvise`, so this cannot be measured here; the call returns 0 (checked with `perf trace`) | | no local change (expected) | no local change (expected) | |
| 14 | v2 as it stands: steps 9-13, no text-buffer pool (no measurable effect, so removed) | | median 25.5 vs 34.9 ms for v1, interleaved in the sandbox, under load; CPU 141 vs 175 ms | median 24.9 vs 32.5 for v1 | |

## Experiments that did not make it

- **Parallel `pwrite` of the formatted hours (v1 work).** 6% of all samples in `osq_lock`
  under `ovl_write_iter`: the writes queue on the file's inode lock. Replaced by one writer.
- **Formatting straight into a shared `mmap` of the output file (v1 work).** Needed every
  line's length up front and its write faults cost more than the copy saved (write phase
  about 4 to 5 ms). Reverted.
- **`O_DIRECT` output, twice.** It does keep the report out of the page cache
  (`memory.stat` `file` stayed at 0), and with the pipelined writer it cut M from 31 to
  27 MB. But each 512 KB direct write is synchronous: on the local virtual disk the 10 MB
  took 8-13 ms of writer time and the whole pipeline waited on it. With the box's disk
  unknown, a few MB of M did not justify the risk to T. Reverted to buffered writes.
- **Three-byte latencies in 32-byte chain blocks** (instead of 4-byte ones in 64-byte
  blocks): the chains fell from about 19 to 14 MB, but the peak had moved to the output
  phase, so M moved only 0.5 MB until step 12 changed the layout anyway.
- **Freeing an hour's chains on the writer thread right after writing it.** The chains
  were then freed late (only once written), and the frees sat on the writer. Step 12
  frees each hour in the worker that reports it, which also lowered the peak by about
  2 MB (median 28.4 vs 30.9 MB).
- **Prefetching the next endpoint's newest chain block** while walking the current one
  (before step 12): no change in CPU time (median 179.9 vs 179.3 ms). The misses were in
  the rest of the chains; step 12 removed the chains instead.
- **Counting-sort with each hour split into 2 or 4 tasks.** Every task has to scan the
  whole hour's logs: CPU went to 199 ms (2 tasks) and 320 ms (4 tasks) against 137 ms for
  one task per hour, which balances well enough over 72 hours.
- **One writer turn per worker (no channel):** each worker waits for its hour's turn to
  write. It held fewer texts (M about 3 MB lower) but workers sat idle: min 26.5 vs 22.8 ms.
- **Not freeing hours during the report at all:** same T (min 22.0 vs 21.7 ms), 4 MB more M.
- **A two-pass design (count, then place every latency at its exact final slot).** Ruled
  out by measurement: one bare AVX2 newline scan over the mapped 239 MB already takes
  about 7 ms with 12 threads here (memory bandwidth), so a second pass costs more than
  any layout could save.

## What the profile says now

12 threads, `perf` with a software clock: ingest loop about 33% of samples and the
endpoint lookup about 20% (together the ~11-12 ms ingest phase, against a ~7 ms floor
for merely streaming the input), the report phase about 20%, kernel page faults and
zeroing about 6%. The report phase is limited by its CPU work (about 0.45 ms of core
time per hour), not by the writer, which is idle half the time.
