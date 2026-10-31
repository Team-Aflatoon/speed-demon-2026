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
| 14 | v2 as submitted (commit 51067d3): steps 9-13, no text-buffer pool (no measurable effect, so removed) | | median 25.5 vs 34.9 ms for v1, interleaved in the sandbox, under load; CPU 141 vs 175 ms | median 24.9 vs 32.5 for v1 | |
| 15 | Timeline instrumentation (per-thread phase events, temporary build): ingest threads started 0.2-2.5 ms apart and finished 4-4.5 ms apart (static equal ranges), the endpoint merge took 0.7 ms on one thread, reporting ~8 ms with ~30% idle, the writer finishing 0.8-1.5 ms after the last report. Fix for the first two: the file is cut into ~1 MB pieces; each worker owns a contiguous run and steals from the back of others' runs when done (a shared counter over all pieces scattered every worker over every hour: 12 x 72 log chunks, ingest 18 ms instead of 12, so it was replaced by owned runs) | | ingest ends now within 0.3-0.6 ms of each other; whole run min 21.0 vs 21.1 ms, median 22.9 vs 23.4 ms vs v2 (interleaved) | unchanged | |
| 16 | Piece edges found by each worker instead of by the main thread before spawning (that touched 240 pages serially: first worker started at 0.8 ms, now 0.2-0.4 ms) | | timeline only; whole-run difference within noise | unchanged | |
| 17 | Percentiles selected low to high (p50 on all n, p95 in the n/2 above it, p99 in the n/20 above that: 1.55n steps instead of 2.94n) | | inside noise in whole runs (selection is ~10% of CPU) | unchanged | |
| 18 | v3 as submitted (commit 7b8250b): steps 15-17 | | sandbox, under load: min 25.5 / median 35.6 ms vs v2 25.5 / 34.4 in the same interleaved set (no measurable whole-run change locally; the gain is structural, on load balance and start-up, and should show on a quiet box) | 23-27 | |
| 19 | Hard counters. This WSL2 VM exposes no hardware perf events (`cycles`/`instructions` "not supported"), so instruction and branch-mispredict counts come from `valgrind --tool=cachegrind --branch-sim=yes` (deterministic: same count every run, whatever the load). v3: 1,358M instructions (438 a line), 3.71M mispredicts. By function: ingest loop 69%, report 27%. By source line, ~100 instructions a line were bounds and range checks (`core/src/slice/index.rs`, `checked_sub`). A TSC-ticks-per-phase build showed the report's formatting at 102-130M ticks, mostly growing fresh text buffers | | | | |
| 20 | Text buffers pooled and reserved once per hour (no regrowth, no freeing on another thread): formatting 102-130M to 33-35M TSC ticks | | total CPU min 136 vs 139 ms (noise level) | unchanged | |
| 21 | Report: each record's local id rewritten as its global rank during the counting pass, so the scatter pass needs no lookup; percentiles of groups of 32 or fewer by one insertion sort (without it: +16M instructions, +0.25M mispredicts) | | CPU min 135 vs 137 ms | unchanged | |
| 22 | Hot helpers `#[inline(always)]` (the endpoint lookup was a call that spilled the loop's registers): 1,311M to 1,244M instructions | | | | |
| 23 | Every field of a line read from one fixed 320-byte window at u8 offsets, so the compiler proves each read in bounds and drops the checks: 1,244M to 1,164M; record header read as one 16-byte slice, log chunks as fixed-size arrays: to 1,136M | | single core min 89.5 vs 94.5 ms | unchanged | |
| 24 | 8,192 hash slots instead of 4,096 (probe collisions were 0.42M mispredicts): 3.37M to 3.12M mispredicts; decimal digits written ten places at a time from a 2-digit table, no value-dependent loop (its exit was the most mispredicted branch, 0.59M): to 2.85M mispredicts | | single core min 83.4 vs 88.1 ms (slots) | unchanged | |
| 25 | v4 as it stands: steps 20-24. Against v3: 1,141M vs 1,358M instructions (368 vs 438 a line, -16%), 2.84M vs 3.71M mispredicts (-23%) | | total CPU (perf stat task-clock, 15 interleaved runs) min 128.7 vs 140.3 ms, median 141.2 vs 153.1; single core min 84.7 vs 96.0 ms, median 87.3 vs 98.3; bench seed 1 20.1-22.9 ms | 25-29 | |

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
- **PGO.** A trial build trained on seed 7 cut total CPU by 6-10% and single-core time
  by about 4%. Built properly inside the image (the allowlisted llvm-tools image,
  training on the published generator's output, seed 2, sample and then full scale), the
  binary showed no gain we could separate from noise: single-core minima 97.6 (full-scale
  training) and 92.6 (sample) vs 98.2 ms without, in one interleaved set, and equal total
  CPU (149 vs 151 ms median) in another.
  Reverted: four more Dockerfile lines and a heavier build for nothing measurable.
- **A simpler endpoint table** (each name in its own padded box, no record arena): 25
  fewer lines, but 4-5% slower on a single core (min 102.3 vs 97.8 ms). Kept the arena.
- **Formatting a line's six numbers into one stack buffer** instead of per-number pushes
  into the text: no measurable CPU change (skipping all formatting saves 10-25 ms of the
  ~150 ms CPU total, skipping selection 10-15 ms). Reverted.
- **Writer tail.** One timeline showed the writer finishing 8 ms after the last report:
  single 140 KB buffered writes took up to 1 ms each while other jobs on the machine were
  writing (dirty-page throttling, as far as we can tell). Other runs showed 0.8-1.5 ms. Not
  changed; it is the main risk left in the report phase.
- **Software prefetch one 4 KB stretch ahead of the newline scan** (half the ingest
  loop's samples sat on the scan's loads): no difference in CPU time or wall time we
  could separate from noise (median 205 vs 209 ms CPU). Reverted.
- **A two-pass design (count, then place every latency at its exact final slot).** Ruled
  out by measurement: one bare AVX2 newline scan over the mapped 239 MB already takes
  about 7 ms with 12 threads here (memory bandwidth), so a second pass costs more than
  any layout could save.

## What the counters say now

cachegrind on v4: 368 instructions and 0.92 mispredicted branches a line, 70% of them
in the ingest loop (hashing and looking up the endpoint, parsing, appending the record)
and 27% in the report (counting sort, selection, formatting). The single-core profile
puts about half of the ingest loop's samples on the newline scan's loads, i.e. waiting
for the input to stream in from memory; a bare scan of the 239 MB takes about 7 ms on
12 threads here, so the ingest phase (about 10 ms) is within 1.5x of that floor.
