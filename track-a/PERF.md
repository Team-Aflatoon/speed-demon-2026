# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

Rows with a run ID are live-board measurements (T is the board's mean of the middle
three of five runs; Points as the board computes them). All other T and M values are
local, in the same units: `bench/bench.sh` or the same sandbox flags (cpuset 0-11 of an
i7-14650HX, cgroup `memory.peak`), quiet-machine medians unless the row says otherwise.
The machine was shared with two other benchmark jobs for most of the work, so local wall
times drift by 30% or more between minutes; every keep-or-drop decision was made on
interleaved A/B runs of saved binaries, on total CPU time (`perf stat` task-clock), on
single-core runs, or (from step 19) on deterministic instruction and mispredict counts,
and each row gives the evidence. The box is about 2x slower than this laptop per core
(turbo off), and its own run-to-run spread is about Â±30% (v2's three runs below).

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (the board's reference numbers) | | 32.05 | 228 | 0 |
| 1 | Rust v1: mmap, 12 workers, per-worker endpoint hash table, per-group block chains, scalar newline and latency loops, merge by sorting all workers' names, per-hour formatting, one write | | 0.039 | 37.5 | |
| 2 | AVX2 newline bitmask; loop-free SIMD latency parse; skip workers that never saw an hour when summarising | | 0.030 | 37 | |
| 3 | Merge endpoint tables through one more hash table and sort the 1,842 survivors, instead of sorting 22,104 names (that phase: 5-6 ms to 0.9 ms) | | 0.029 | 37 | |
| 4 | Copy names into a compact per-worker arena (compares against first occurrences in the input cost a TLB miss on most lines); check the stored hash before comparing bytes; fixed-size compares instead of `memcmp` | | 0.023 | 37 | |
| 5 | Summarise into 24-byte records first, free the chains, then format in parallel: the output's page cache and the chains no longer peak together | | 0.023 | 35 | |
| 6 | Branchless name compare: two 32-byte AVX2 compares with the bits past the length masked (interleaved A/B min 24.1 vs 25.7 ms) | | 0.023 | 35 | |
| 7 | Power-of-two blocks with a separate link array (a mask instead of `% 15` per line); each hour split into four tasks by endpoint rank for balance; one sequential writer (single core 99 to 96.6 ms) | | 0.023 | 33 | |
| 8 | **v1 as submitted** (commit 0ed7fba). Fewer lines for S (rank + five numbers as one tuple struct, splatted SIMD weights): 456 to 421 Rust lines; S 427. Reruns: run-1fc879b67a36, run-b114fce0b3dd (44.0 ms, 49.0 MB) | run-a288ae43d4e8 | 0.0461 | 47.5 | 71.3 |
| 9 | Line ends found per 4 KB in bulk: both candidate newlines of each 64-byte block are stored unconditionally and the count advances by popcount, so the per-line loop has no unpredictable exit at every block boundary (single core min 94.8 vs 104.5 ms; 12 threads min 30.6 vs 32.2 ms) | | 0.021 | 33 | |
| 10 | Endpoint table as one arena of records (hash, id, length, name) so a hit reads its slot and then one record; 4,096 slots (neutral within noise; kept for the smaller footprint) | | 0.021 | 33 | |
| 11 | Report and write overlapped: workers report the hours in order and the main thread writes each one as soon as all earlier ones are out (buffered writes; a bounded channel caps the texts in flight; interleaved min 28.7 vs 33.9 ms under load) | | 0.020 | 36 | |
| 12 | Per-worker, per-hour logs of 4-byte records (`id << 20 \| 5xx << 19 \| latency`, latencies of 2^19 and up kept aside) in 256 KB chunks; each hour reported by counting-sorting its logs by endpoint into one array, so every group is one contiguous slice; each hour's chunks are freed (unmapped) as soon as it is reported, so the logs shrink while the output's page cache grows (CPU 128 vs 159 ms; min 22.8 vs 26.1 ms) | | 0.020 | 26 | |
| 13 | `prctl(PR_SET_THP_DISABLE)` at start. v1's box M was 47.5 MB against about 32 MB locally, consistent with transparent huge pages set to `always` there (Debian's default) backing malloc's per-thread arenas with 2 MB pages. Locally THP is `madvise`, so it shows nothing here; the board run of v2 is the proof: M fell from 47.5 to 25.8 MB, in line with the ~25 MB we measure locally | | 0.020 | 26 | |
| 14 | **v2 as submitted** (commit 51067d3): steps 9-13. The text-buffer pool tried with step 11 was dropped here because whole-run timing could not see it (it came back in step 20, once per-phase TSC counts showed what buffer growth cost). Reruns: run-a4147923f8f1 (32.6 ms), run-60abe77e7171 (44.2 ms): the box's own noise | run-078b64667d1c | 0.0330 | 25.8 | 77.3 |
| 15 | Timeline instrumentation (per-thread phase events, temporary build): ingest threads started 0.2-2.5 ms apart and finished 4-4.5 ms apart (static equal ranges), the endpoint merge took 0.7 ms on one thread, reporting ~8 ms with ~30% idle, the writer finishing 0.8-1.5 ms after the last report. Fix for the first two: the file is cut into ~1 MB pieces; each worker owns a contiguous run and steals from the back of others' runs when done (a shared counter over all pieces scattered every worker over every hour, 12 x 72 log chunks, and ingest took 18 ms instead of 12). Ingest now ends within 0.3-0.6 ms across threads | | 0.021 | 25 | |
| 16 | Piece edges found by each worker instead of by the main thread before spawning (that touched 240 pages serially: the first worker started at 0.8 ms, now 0.2-0.4 ms) | | 0.021 | 25 | |
| 17 | Percentiles selected low to high (p50 on all n, p95 in the n/2 above it, p99 in the n/20 above that: 1.55n steps instead of 2.94n) | | 0.021 | 25 | |
| 18 | **v3 as submitted** (commit 7b8250b): steps 15-17; S 491. Rerun: run-3d15bb7568d0 (32.8 ms). Locally no whole-run change could be measured, and on the box T barely moved either (33.0 to 31.8 ms): load balance was not the bottleneck | run-f4376824385c | 0.0318 | 24.2 | 77.8 |
| 19 | Hard counters. This WSL2 VM exposes no hardware perf events (`cycles`/`instructions` "not supported"), so instruction and branch-mispredict counts come from `valgrind --tool=cachegrind --branch-sim=yes` (deterministic: same count every run, whatever the load). v3: 1,358M instructions (438 a line), 3.71M mispredicts; ingest loop 69%, report 27%. By source line, ~100 instructions a line were bounds and range checks. A TSC-ticks-per-phase build showed the report's formatting at 102-130M ticks, mostly growing fresh text buffers | | 0.021 | 25 | |
| 20 | Text buffers pooled again and reserved once per hour (no regrowth, no freeing on another thread): formatting 102-130M to 33-35M TSC ticks | | 0.021 | 25 | |
| 21 | Report: each record's local id rewritten as its global rank during the counting pass, so the scatter pass needs no lookup; percentiles of groups of 32 or fewer by one `sort_unstable` (without it: +16M instructions, +0.25M mispredicts) | | 0.021 | 25 | |
| 22 | Hot helpers `#[inline(always)]` (the endpoint lookup was a call that spilled the loop's registers): 1,311M to 1,244M instructions | | 0.021 | 25 | |
| 23 | Every field of a line read from one fixed 320-byte window at u8 offsets, so the compiler proves each read in bounds and drops the checks: 1,244M to 1,164M; record header read as one 16-byte slice, log chunks as fixed-size arrays: to 1,136M (single core min 89.5 vs 94.5 ms) | | 0.021 | 25 | |
| 24 | 8,192 hash slots instead of 4,096 (probe collisions were 0.42M mispredicts): 3.37M to 3.12M mispredicts; decimal digits written ten places at a time from a 2-digit table, no value-dependent loop (its exit was the most mispredicted branch, 0.59M): to 2.85M | | 0.021 | 25 | |
| 25 | **v4 as submitted** (commit 338f0cb): steps 19-24; S 537. Against v3: 1,141M vs 1,358M instructions (-16%), 2.84M vs 3.71M mispredicts (-23%); local CPU min 128.7 vs 140.3 ms, single core min 84.7 vs 96.0 ms. On the box T did not move (31.9 vs 31.8 ms): v3's and v4's local CPU gains did not show up there, which led to the CPU-per-line diagnosis that the box (12 threads on 6 P-cores, turbo off) is bound by total work per line, and that the leader does about half of ours | run-f192c44a86c0 | 0.0319 | 25.9 | 77.1 |
| 26 | Report tables indexed by the records' 12-bit id/rank field (4,096-entry arrays, ranks stored pre-shifted), so the compiler drops their bounds checks; explicit loops over the log chunks instead of a flattened iterator; slot table as a fixed-size array: 1,141M to 1,098M instructions (-4%), 2.88M mispredicts | | 0.021 | 26 | |
| 27 | Review fixes: `Mutex` instead of a write-only `RwLock`; `mallopt(M_MMAP_THRESHOLD, 128 KB)` pins the allocator so every 256 KB log chunk really is its own mapping (glibc would otherwise raise the threshold as mapped blocks are freed); a compile-time assert ties the endpoint limits to the record's bit layout; SAFETY notes name the crate-wide target features | | 0.021 | 26 | |
| 28 | **v5 as submitted** (commit c3486cf): steps 26-27. 1,099M instructions, 2.91M mispredicts; local CPU min 137.3 vs 139.3 ms, single core min 90.7 vs 91.2 ms against v4 in the same interleaved set (a small gain); S 560 | | 0.022 | 26 | |
| 29 | Kernel work, measured with `strace -c -f` and `perf stat -e page-faults` (software events do work here): v5 made 277 mmap and 259 munmap calls (one per 256 KB log chunk, each taking the address-space lock for writing and stalling the other threads' page faults; each munmap also flushes the TLBs of all our CPUs), 485 futex calls (0.20 s summed across threads under strace) and 9.9k page faults. Freeing the hours does lower the peak here: without it M was 37-40 MB instead of 25-29, because the output's page cache is charged to the run | | 0.022 | 26 | |
| 30 | One log arena: a single `MAP_NORESERVE` anonymous mapping sized for the worst case, carved by a bump pointer into chunks of 16 KB doubling to 256 KB (most thread-hours hold ~3,600 records, 14 KB); an hour's chunks are given back with `madvise(MADV_DONTNEED)` (address-space lock taken for reading only) instead of munmap; the malloc threshold pin is no longer needed. mmap 277 to 119, munmap 259 to 101, futex time 0.20 to 0.05 s, faults unchanged (9.6k). The chunk being filled is held as its own field, so a push costs one compare: 1,077M instructions (v5: 1,099M) | | 0.020 | 27 | |
| 31 | **v6** as it stands: steps 29-30. Sandbox, 6 interleaved runs each: median 20.8 vs 22.5 ms for v5, min 19.9 vs 21.1 ms; total CPU unchanged within noise (median 145.9 vs 143.9 ms; the gain is time not spent waiting on the address-space lock); M median 28.8 vs 27.5 MB (noise level); S 590 | | 0.021 | 28 | |

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
- **A single forward pass over each line (v5 attempt).** No newline index: from a line's
  start the endpoint start is found by the SWAR test, its end by a 64-byte compare for
  the space, the newline 5 bytes later by another compare, and the next line starts
  after it. 12% fewer instructions (1,141M to 1,002M), yet 12% slower on a single core
  (min 104.6 vs 88.0 ms): each line's start now depends on parsing the previous one, a
  serial chain of loads and compares the CPU cannot overlap, while the newline index
  makes every line start known in advance. Two cursors over the two halves of each
  piece, taking turns line by line, won most of it back (93.7 ms) but added 2M
  mispredicts and still lost to the index (90.7 ms). Reverted; the report-side changes
  of that branch were kept (step 26).
- **Masks and the name compare read straight from the window** instead of from copied
  64-byte arrays: 1,002M to 1,015M instructions, so the copies were already free.
- **`MAP_POPULATE` on the input** (pre-map all of it in the `mmap` call): page faults fell
  from 9.9k to 7.0k, but the single-threaded populate added about 4 ms of wall time
  before any worker started (sandbox median 27.6 vs 21.7 ms). Reverted.
- **A two-pass design (count, then place every latency at its exact final slot).** Ruled
  out by measurement: one bare AVX2 newline scan over the mapped 239 MB already takes
  about 7 ms with 12 threads here (memory bandwidth), so a second pass costs more than
  any layout could save.

## What the counters say now

cachegrind on v5: 1,099M instructions (355 a line) and 2.9M mispredicted branches, about
two thirds in the ingest loop and one third in the report. The single-core profile puts
about half of the ingest loop's samples on the newline scan's loads, i.e. waiting for
the input to stream in; a bare scan of the 239 MB takes about 7 ms on 12 threads here.
On the box, v3 to v5 have all landed at about 32 ms whatever their local CPU gains,
within its ±30% run-to-run noise: the next real step has to cut the work per line by
something like half, not by percents.
