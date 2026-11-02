# Track A: performance log

## Summary

**19% fewer instructions did not help on the board (v5 even ran about 9% slower than v3);
moving 11 ms of work off the critical path made it 25% faster.**

Final: **v9** (steps 58 to 65, commit 6ded953; the code in this tree): v8's design with quiescent reports (an hour is
closed, and reported once every piece that could write it is parsed, so no count check is needed), two-slot records for
wide latencies instead of side lists, branch-free percentiles for groups of up to 64, and memory cuts. Board: 19.3 ms,
17.1 MB, K 48.94, 84.48 points (run-ccb75cc37209); earlier runs of the same code 19.5 ms, 16.9 MB, K 48.84
(run-89cc8aa5d119) and 19.4 ms, 17.2 MB, K 48.71 (run-438cdec0eae2); S 521, D 1; reference 32.05 s, 228 MB. Of 12 board
runs of this code, T ran from 19.3 to 21.1 ms and none was over 24 ms (step 65). Two later builds, v10 (step 69) and v11
(step 71), were as fast on their best runs and cut S to 481 and 464, but 6 of their 13 board runs spiked to 22 to 31 ms,
so v9 stayed (step 72). Also rejected on the box: a9b, 64 KB pieces (step 66), and a9c, a mapping per piece (step 67).

Before it, v8: step 57, commit 32b49b8: pread pieces + per-hour reports while parsing, verified by
exact counts. Board: 20.4 ms, 18.2 MB, K 46.93 (run-996cb0022b51) and 20.9 ms, 17.9 MB,
K 46.45 (run-361c33ed2406), median 20.65 ms, 18.05 MB; S 532; reference 32.05 s, 228 MB.
Before that hardening, step 56 (commit 1cd61cf) ran 20.0 and 20.1 ms at S 514; S rose
so that unexpected input fails loudly (errors with their path, value or size, no
half-written output), on an unchanged hot path. Decisive steps (all board runs):

v8's final repository state (commit eba990a; since 32b49b8 only comments and docs changed, S 532): run-8fc3ec9b48d1, 21.1 ms, 18.3 MB, K 46.02. Three runs of this code: 20.4, 20.9 and 21.1 ms, median 20.9 ms.

1. v1 Rust rewrite (mmap, 12 workers): 46.1 ms, 47.5 MB (run-a288ae43d4e8).
2. v2 per-hour 4-byte logs + counting sort, THP off: 33.0 ms, 25.8 MB (run-078b64667d1c);
   v3-v5 cut instructions ~19% locally and did not move the box (31.8-34.6 ms).
3. Probes (wrong output on purpose, so red on the board): read floor 14.6 ms
   (run-8ea2db5074de), ingest only 21.3 ms (run-d570a81674c0), i.e. ~11 ms was the
   report after the ingest barrier.
4. v7 redesign: report each hour while parsing; exact counts verify it, with a full rerun
   if a line came late: 24.5 ms, 19.5 MB (run-d96b1571240a).
5. pread vs mmap, two correct builds back to back: 24.5 → 20.7 ms (run-5450dc2d90d1 vs
   run-cbad326faf08).
6. Simplification, same output: S 530 → 514 at the same T (step 56); then hardening
   against unexpected input, S 514 → 532 at the same instruction count (step 57).

Second lesson (v9 to v11): on the box, a build can match another on its best runs and still
spike. v10 had the best single run of the track (19.5 ms, K 49.62) but 4 of the 13 runs of v10, v10b and v11
were over 24 ms; v9 had none in 12. We shipped the steadier build.

Lesson: on the box, phase structure and kernel work (faults, barriers) set T; local
instruction counts did not (v3 to v5: 1,358M → 1,099M instructions, 31.8 → 34.6 ms).

Red runs on the board: besides the two probes of item 3 (wrong output by design), three
read-floor probes aborted at start, run-f70b36471354, run-f71c8ec82249 and a third on commit
2e78b12 (no run ID in the public history; that commit changed only Track D, so the probe from
the commit before was still in the tree): they took their mode from argv and the probe runner
passes none (see v8). Every other board run
of this track passed.

## Measures

- **T**: wall time from exec to exit. A board row's T is what the board reported for that
  run ID; every other T is measured on this laptop and marked "(local)".
- **M**: the run's cgroup `memory.peak`. Board figures are the board's, in MB of 10^6
  bytes (bytes / 10^6, as the board reports it). Local figures, in the tables and in the
  text, are in MiB (2^20 bytes: our scripts print KiB), even where older text says "MB";
  in the tables they are marked "(local)". Up to step 31 a local M is `bench/bench.sh`'s,
  which includes about 5 MiB of shell in the container; from v7 (step 32) on, a local M is `memory.peak` minus
  the container's own baseline, i.e. the program's share only.
- **S**: code lines counted by `static_axes.py` (scc after default rustfmt, Dockerfile
  included); **D**: 1 + dependencies, always 1 here.
- **K** = (T_ref/T)^0.5 × min(M_ref/M, 50)^0.2 × min(S_ref/S, 1)^0.15 × (D_ref/D)^0.15, and
  **Points** = 50 × log10(K), both as the board computes them. Tables give whichever of the
  two the board showed for that run.
- **Box against laptop**: one ratio is used throughout, about **1.9x**: v7 took 24.5 ms on
  the box (run-d96b1571240a) against a 13.1 ms local median. The box runs 12 threads on 6
  P-cores with turbo off.
- **Box noise**: repeated runs of one build differed by at most **3.5%**: v3 31.8 / 32.8 ms
  (run-f4376824385c, run-3d15bb7568d0), a20 20.2 / 20.9 ms (run-97875b283660,
  run-b471d3e70c35), step 56 20.0 / 20.1 ms (run-2491c9d07251, run-237f722dfeb2), step 57
  20.4 / 20.9 ms (run-996cb0022b51, run-361c33ed2406). One
  outlier is set aside: v2 at 44.2 ms (run-60abe77e7171) against 33.0 and 32.6 ms.
- **Local T and M cells**: a pair "x vs y" is the A/B that decided the step (this step
  against the one before, same session); a single value marked "(local median)" is the
  median of that build's own runs, with no A/B partner; "—" where the decision rested on other
  evidence, which the row then gives (instruction counts, faults, single-core times).

## Labels

- **Steps** 0 to 72 number every change in order; a step is one row.
- **v1 to v8** are the versions submitted in turn: v1 step 8, v2 step 14, v3 step 18, v4
  step 25, v5 step 28, v6 step 31 (never run whole on the board), v7 step 41; v8 is the
  pread line that followed, submitted as a13 (step 48), a19 (51), a20 (53) and finally
  step 56 (commit 1cd61cf, before hardening) and step 57 (commit 32b49b8, final).
- **v9** is steps 58 to 65 (commit 6ded953, built as a9). **a9b**, **a9c** (steps 66, 67), **a10** (v10, step 69), **a10b** (step 70) and **a11** (v11, step 71) are board builds of the v9 work; a10, a10b and a11 were not kept (step 72). v10 was shipped for a short while (commit 799d8d3) and reverted (commit 501b40f). The steps below 58 are numbered as before.
- **a12 to a21** are board builds of the v8 work: a12 v7's mmap reading with steps 42-46
  (step 47); a13 the same reading with pread (step 48); **a14** a12 plus one
  `MADV_POPULATE_READ` per piece (run-d5a15d3df29a, 26.0 ms; v8 experiments); a19, a20 as
  above; a21 the main thread parsing too (v8 experiments). a15 to a18 were local builds
  only and never ran on the board.

## Steps 0 to 31 (v1 to v6)

Rows with a run ID are live-board measurements. All other T and M values are local, in
the same units: `bench/bench.sh` or the same sandbox flags (cpuset 0-11 of an
i7-14650HX, cgroup `memory.peak`), quiet-machine medians unless the row says otherwise.
The machine was shared with two other benchmark jobs for most of the work, so local wall
times drift by 30% or more between minutes; every keep-or-drop decision was made on
interleaved A/B runs of saved binaries, on total CPU time (`perf stat` task-clock), on
single-core runs, or (from step 19) on deterministic instruction and mispredict counts,
and each row gives the evidence. The box ratio and its noise are in Measures.

| Step | Change | Run ID | T (s) | M (MB board, MiB local) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (the board's reference numbers) | | 32.05 | 228 | 0 |
| 1 | Rust v1: mmap, 12 workers, per-worker endpoint hash table, per-group block chains, scalar newline and latency loops, merge by sorting all workers' names, per-hour formatting, one write | | 0.039 (local median) | 37.5 (local median) | |
| 2 | AVX2 newline bitmask; loop-free SIMD latency parse; skip workers that never saw an hour when summarising | | 0.030 (local median) | 37 (local median) | |
| 3 | Merge endpoint tables through one more hash table and sort the 1,842 survivors, instead of sorting 22,104 names (that phase: 5-6 ms to 0.9 ms) | | 0.029 (local median) | 37 (local median) | |
| 4 | Copy names into a compact per-worker arena (compares against first occurrences in the input cost a TLB miss on most lines); check the stored hash before comparing bytes; fixed-size compares instead of `memcmp` | | 0.023 (local median) | 37 (local median) | |
| 5 | Summarise into 24-byte records first, free the chains, then format in parallel: the output's page cache and the chains no longer peak together | | — | 35 (local median) | |
| 6 | Branchless name compare: two 32-byte AVX2 compares with the bits past the length masked (interleaved A/B min 24.1 vs 25.7 ms) | | 0.0241 vs 0.0257 (local) | — | |
| 7 | Power-of-two blocks with a separate link array (a mask instead of `% 15` per line); each hour split into four tasks by endpoint rank for balance; one sequential writer (single core 99 to 96.6 ms) | | — | 33 (local median) | |
| 8 | **v1 as submitted** (commit 0ed7fba). Fewer lines for S (rank + five numbers as one tuple struct, splatted SIMD weights): 456 to 421 Rust lines; S 427. Reruns: run-1fc879b67a36, run-b114fce0b3dd (44.0 ms, 49.0 MB) | run-a288ae43d4e8 | 0.0461 | 47.5 | 71.3 |
| 9 | Line ends found per 4 KB in bulk: both candidate newlines of each 64-byte block are stored unconditionally and the count advances by popcount, so the per-line loop has no unpredictable exit at every block boundary (single core min 94.8 vs 104.5 ms; 12 threads min 30.6 vs 32.2 ms) | | 0.0306 vs 0.0322 (local) | — | |
| 10 | Endpoint table as one arena of records (hash, id, length, name) so a hit reads its slot and then one record; 4,096 slots (neutral within noise; kept for the smaller footprint) | | — | — | |
| 11 | Report and write overlapped: workers report the hours in order and the main thread writes each one as soon as all earlier ones are out (buffered writes; a bounded channel caps the texts in flight; interleaved min 28.7 vs 33.9 ms under load) | | 0.0287 vs 0.0339 (local) | 36 (local median) | |
| 12 | Per-worker, per-hour logs of 4-byte records (`id << 20 \| 5xx << 19 \| latency`, latencies of 2^19 and up kept aside) in 256 KB chunks; each hour reported by counting-sorting its logs by endpoint into one array, so every group is one contiguous slice; each hour's chunks are freed (unmapped) as soon as it is reported, so the logs shrink while the output's page cache grows (CPU 128 vs 159 ms; min 22.8 vs 26.1 ms) | | 0.0228 vs 0.0261 (local) | 26 (local median) | |
| 13 | `prctl(PR_SET_THP_DISABLE)` at start. v1's box M was 47.5 MB against about 32 MB locally, consistent with transparent huge pages set to `always` there (Debian's default) backing malloc's per-thread arenas with 2 MB pages. Locally THP is `madvise`, so it shows nothing here; the board run of v2 is the proof: M fell from 47.5 to 25.8 MB, in line with the ~25 MB we measure locally | | — | — | |
| 14 | **v2 as submitted** (commit 51067d3): steps 9-13. The text-buffer pool tried with step 11 was dropped here because whole-run timing could not see it (it came back in step 20, once per-phase TSC counts showed what buffer growth cost). Reruns: run-a4147923f8f1 (32.6 ms) and run-60abe77e7171 (44.2 ms, the one outlier set aside in Measures) | run-078b64667d1c | 0.0330 | 25.8 | 77.3 |
| 15 | Timeline instrumentation (per-thread phase events, temporary build): ingest threads started 0.2-2.5 ms apart and finished 4-4.5 ms apart (static equal ranges), the endpoint merge took 0.7 ms on one thread, reporting ~8 ms with ~30% idle, the writer finishing 0.8-1.5 ms after the last report. Fix for the first two: the file is cut into ~1 MB pieces; each worker owns a contiguous run and steals from the back of others' runs when done (a shared counter over all pieces scattered every worker over every hour, 12 x 72 log chunks, and ingest took 18 ms instead of 12). Ingest now ends within 0.3-0.6 ms across threads | | — | — | |
| 16 | Piece edges found by each worker instead of by the main thread before spawning (that touched 240 pages serially: the first worker started at 0.8 ms, now 0.2-0.4 ms) | | — | — | |
| 17 | Percentiles selected low to high (p50 on all n, p95 in the n/2 above it, p99 in the n/20 above that: 1.55n steps instead of 2.94n) | | — | — | |
| 18 | **v3 as submitted** (commit 7b8250b): steps 15-17; S 491. Rerun: run-3d15bb7568d0 (32.8 ms). Locally no whole-run change could be measured, and on the box T barely moved either (33.0 to 31.8 ms): load balance was not the bottleneck | run-f4376824385c | 0.0318 | 24.2 | 77.8 |
| 19 | Hard counters. This WSL2 VM exposes no hardware perf events (`cycles`/`instructions` "not supported"), so instruction and branch-mispredict counts come from `valgrind --tool=cachegrind --branch-sim=yes` (deterministic: same count every run, whatever the load). v3: 1,358M instructions (438 a line), 3.71M mispredicts; ingest loop 69%, report 27%. By source line, ~100 instructions a line were bounds and range checks. A TSC-ticks-per-phase build showed the report's formatting at 102-130M ticks, mostly growing fresh text buffers | | — | — | |
| 20 | Text buffers pooled again and reserved once per hour (no regrowth, no freeing on another thread): formatting 102-130M to 33-35M TSC ticks | | — | — | |
| 21 | Report: each record's local id rewritten as its global rank during the counting pass, so the scatter pass needs no lookup; percentiles of groups of 32 or fewer by one `sort_unstable` (without it: +16M instructions, +0.25M mispredicts) | | — | — | |
| 22 | Hot helpers `#[inline(always)]` (the endpoint lookup was a call that spilled the loop's registers): 1,311M to 1,244M instructions | | — | — | |
| 23 | Every field of a line read from one fixed 320-byte window at u8 offsets, so the compiler proves each read in bounds and drops the checks: 1,244M to 1,164M; record header read as one 16-byte slice, log chunks as fixed-size arrays: to 1,136M (single core min 89.5 vs 94.5 ms) | | — | — | |
| 24 | 8,192 hash slots instead of 4,096 (probe collisions were 0.42M mispredicts): 3.37M to 3.12M mispredicts; decimal digits written ten places at a time from a 2-digit table, no value-dependent loop (its exit was the most mispredicted branch, 0.59M): to 2.85M | | — | — | |
| 25 | **v4 as submitted** (commit 338f0cb): steps 19-24; S 537. Against v3: 1,141M vs 1,358M instructions (-16%), 2.84M vs 3.71M mispredicts (-23%); local CPU min 128.7 vs 140.3 ms, single core min 84.7 vs 96.0 ms. On the box T did not move (31.9 vs 31.8 ms): v3's and v4's local CPU gains did not show up there, which led to the CPU-per-line diagnosis that the box (12 threads on 6 P-cores, turbo off) is bound by total work per line, and that the leader does about half of ours | run-f192c44a86c0 | 0.0319 | 25.9 | 77.1 |
| 26 | Report tables indexed by the records' 12-bit id/rank field (4,096-entry arrays, ranks stored pre-shifted), so the compiler drops their bounds checks; explicit loops over the log chunks instead of a flattened iterator; slot table as a fixed-size array: 1,141M to 1,098M instructions (-4%), 2.88M mispredicts | | — | — | |
| 27 | Review fixes: `Mutex` instead of a write-only `RwLock`; `mallopt(M_MMAP_THRESHOLD, 128 KB)` pins the allocator so every 256 KB log chunk really is its own mapping (glibc would otherwise raise the threshold as mapped blocks are freed); a compile-time assert ties the endpoint limits to the record's bit layout; SAFETY notes name the crate-wide target features | | — | — | |
| 28 | **v5 as submitted** (commit c3486cf): steps 26-27. 1,099M instructions, 2.91M mispredicts; local CPU min 137.3 vs 139.3 ms, single core min 90.7 vs 91.2 ms against v4 in the same interleaved set (a small gain); S 560. Board: 34.6 ms, slower than v3's 31.8 and 32.8 ms despite 19% fewer instructions | run-8cf0c65ed342 | 0.0346 | | |
| 29 | Kernel work, measured with `strace -c -f` and `perf stat -e page-faults` (software events do work here): v5 made 277 mmap and 259 munmap calls (one per 256 KB log chunk, each taking the address-space lock for writing and stalling the other threads' page faults; each munmap also flushes the TLBs of all our CPUs), 485 futex calls (0.20 s summed across threads under strace) and 9.9k page faults. Freeing the hours does lower the peak here: without it M was 37-40 MB instead of 25-29, because the output's page cache is charged to the run | | — | — | |
| 30 | One log arena: a single `MAP_NORESERVE` anonymous mapping sized for the worst case, carved by a bump pointer into chunks of 16 KB doubling to 256 KB (most thread-hours hold ~3,600 records, 14 KB); an hour's chunks are given back with `madvise(MADV_DONTNEED)` (address-space lock taken for reading only) instead of munmap; the malloc threshold pin is no longer needed. mmap 277 to 119, munmap 259 to 101, futex time 0.20 to 0.05 s, faults unchanged (9.6k). The chunk being filled is held as its own field, so a push costs one compare: 1,077M instructions (v5: 1,099M) | | — | — | |
| 31 | **v6** (commit 8329ce4; never run whole on the board, see v7 below): steps 29-30. Sandbox, 6 interleaved runs each: median 20.8 vs 22.5 ms for v5, min 19.9 vs 21.1 ms; total CPU unchanged within noise (median 145.9 vs 143.9 ms; the gain is time not spent waiting on the address-space lock); M median 28.8 vs 27.5 MB (noise level); S 590 | | 0.0208 vs 0.0225 (local) | 28.8 vs 27.5 (local) | |

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
- **Is the 239 MB input (238,836,234 bytes on seed 1) mapping itself expensive (faults, and tearing it down at exit)?**
  Probed with a trivial program that reads every byte of the input on 12 threads and
  exits, timed inside and from outside the process (outside includes exec and exit,
  like the board's timer; a shell running nothing costs ~0.8 ms the same way):
  mmap took 7.4-12.5 ms inside and 8.7-13.9 ms outside, so exit teardown costs at most
  about 0.5-1 ms here; `pread` into a 1 MB buffer per thread took 10.9-21.5 ms inside
  (the extra copy of 239 MB) and added 11 MB of buffers to M. The mapping's 3.1k page
  faults (fault-around maps 16 pages each) are about 13% of that probe's CPU samples,
  but taking them out did not help: `madvise(MADV_POPULATE_READ)` per thread range cut
  faults to 71 and CPU from 68 to 36 ms, yet wall rose (8.0 to 10.7 ms in-process,
  threads waiting instead of overlapping the mapping with the scan); per 1 MB piece in
  the real program it cut faults from 9.6k to 6.9k with CPU unchanged and wall worse
  (sandbox median 21.9 vs 20.5 ms). mmap stays. A side finding for every track: in this
  sandbox the probe's `memory.peak` is 8.1-8.7 MB for a program holding almost nothing,
  and 5.6-6.9 MB for a bare shell, so about 6 MB of every M here is the container's
  own baseline.
- **A two-pass design (count, then place every latency at its exact final slot).** Ruled
  out by measurement: one bare AVX2 newline scan over the mapped 239 MB already takes
  about 7 ms with 12 threads here (memory bandwidth), so a second pass costs more than
  any layout could save.

## What the counters said at v5

cachegrind on v5: 1,099M instructions (355 a line) and 2.9M mispredicted branches, about
two thirds in the ingest loop and one third in the report. The single-core profile puts
about half of the ingest loop's samples on the newline scan's loads, i.e. waiting for
the input to stream in; a bare scan of the 239 MB takes about 7 ms on 12 threads here.
On the box, v3 to v5 have all landed at 31.8 to 34.6 ms whatever their local CPU gains,
never faster than v3: the next real step has to cut the work per line by something like
half, not by percents.

## v7: a clean-room redesign

Steps 1 to 31 had plateaued: v3 to v5 landed at 31.8 to 34.6 ms on the box whatever their
local gains (v6 was never run whole there). Two box probes (builds that deliberately fail
the hash, so the board records T and M but scores nothing) showed where that time went:

| Probe | What it runs | Run ID | T (s) |
|---|---|---|---|
| Read floor | mmap the input, 12 threads count newlines with AVX2, nothing else | run-8ea2db5074de | 0.0146 |
| v6 ingest only | v6 with its report phase cut out | run-d570a81674c0 | 0.0213 |
| v3 to v5 | the whole program (v6 kept their report phase) | run-f4376824385c, run-f192c44a86c0, run-8cf0c65ed342 | 0.0318-0.0346 |

So about 11 ms of a whole run's box time was the report phase that runs after the ingest
barrier, and about 7 ms the ingest's work above the bare read. v7 was written from scratch in a
separate folder to attack both: report while parsing instead of after it, and do a third
fewer instructions. Steps 32 to 41 are its development. Their local T values are medians
of interleaved runs of saved binaries with the output on a tmpfs, as on the box
(`/srv/speeddemon/work` is a tmpfs there), and M is `memory.peak` minus the container's
own baseline (about 5 MB of shell), so both read lower than `bench/bench.sh`, which
writes to an ext4 volume; instruction counts are from cachegrind.

| Step | Change | Run ID | T (s) | M (MB board, MiB local) | Points |
|---|---|---|---|---|---|
| 32 | Rewrite: per-(worker, hour) logs in one `MAP_NORESERVE` reservation, a barrier, then per-hour reports (739M instructions against v6's 1,077M; wall equal to v6) | | 0.0195 (local median) | 20 (local median) | |
| 33 | The main thread parses during ingest and writes afterwards, with 11 reporters, so the writer has a CPU of its own (its tail after the last report: 3 ms to 0.2 ms) | | 0.0145 (local median) | — | |
| 34 | One log per hour shared by all workers, which reserve 1 KB blocks of it with one `fetch_add`; pieces in file order; one endpoint table shared by all workers | | — | — | |
| 35 | Speculative reporting: an hour is reported as soon as the parsed prefix is two hours past it, while parsing goes on; an exact count check, and a rerun without speculation if it fails (median 13.0 vs 14.5 ms, CPU -8%) | | 0.0130 vs 0.0145 (local) | 19.5 (local median) | |
| 36 | Count and scatter as plain loops; the endpoint start found by one 16-byte compare (767M to 701M instructions) | | — | — | |
| 37 | Endpoint table as a `static`, structs built from `Default`, logs as slices (S 559 to 484; 688M instructions) | | — | — | |
| 38 | A cap on the line-end index count, so the compiler drops its bounds checks (691M to 682M) | | — | — | |
| 39 | Verification by the total line count instead of per-hour counts (one less variable in the hot loop) | | — | — | |
| 40 | `mallopt(M_MMAP_THRESHOLD)` fixed at 128 KB, text pool capped at four, report scratch pooled (M median 19.5 to 17.9 MB) | | — | 17.9 vs 19.5 (local) | |
| 41 | **v7 as submitted** (commit a46b7ae): steps 32-40. Local: 686M instructions, 2.14M mispredicts, wall min/median 12.1/13.1 ms against v6's 17.7/19.6 in one interleaved set, CPU median 111 vs 140 ms; S 501 | run-d96b1571240a | 0.0245 | 19.5 | 81.5 |

### How v7 works, and why it is exact

The design is described in README.md; in short, each line becomes a 4-byte record in its
hour's log, an hour is reported (counting sort by endpoint, three selections, format) as
soon as parsing is two hours past it, and the hour's log is freed at once, so only a few
hours of records ever exist. The input promises no bound on lateness, so the speculation
is checked rather than trusted: every line is exactly one record in its hour, so the
reports saw every line exactly when their counts add up to the number of lines parsed. If
they do not, the output is truncated and the job runs again with all reports at the end.
On the generator's data the margin is about 50 minutes (an hour closes at a line 60 minutes
after it ends; shard 0 uploads an hour 5 to 10 minutes after it ends; the other shards are at
most 2.5 minutes late); full seeds 1, 3, 7
and 11 and sample seeds 1 and 5 never rerun, while a shuffled file, a reversed file and a
sorted file with one early line moved to the end do, and all give the reference's bytes.

### v7 against v6, locally

| | v6 | v7 |
|---|---|---|
| Instructions (cachegrind) | 1,077M | 686M |
| Branch mispredicts | 2.77M | 2.14M |
| Wall, 12 interleaved runs, min / median | 17.7 / 19.6 ms | 12.1 / 13.1 ms |
| CPU (`perf stat` task-clock), median | 140 ms | 111 ms |
| `memory.peak` minus the container's own baseline, median | 24.1 MB | 17.9 MB |
| S | 590 | 501 |

On the box the gain was smaller than locally (31.8-34.6 ms for v3-v5 to 24.5 ms, against
19.6 to 13.1 ms locally for v6 to v7): at the box's 1.9x (Measures), work that overlaps
with the memory stream here is on the critical path there.

### v7 experiments that did not make it

- **8-byte records with no side list** (21 fewer lines): page faults 7.0k to 10.6k, CPU
  +5%, M unchanged.
- **12 workers plus the writer** (13 threads on 12 CPUs): median 15.0 vs 13.0 ms. **The
  main thread parsing between writes** (12 parsers, none idle): median 15.1 vs 15.0 ms,
  minimum 13.5 vs 12.9 ms.
- **`MADV_POPULATE_READ` per piece:** faults 7.2k to 7.0k, no change in wall time.
- **Dropping the input's page-table entries after the last piece**, beside the final
  reports: done-to-exit gap 1.0-1.3 ms either way.
- **Release/Acquire instead of Relaxed for the records:** the same instruction count. The
  records stay Relaxed; a report that sees an endpoint id newer than its name order waits
  on the Acquire load of the published-name count before it reads the names.
- **Radix-sorting each hour by latency** (two stable 10-bit passes before the stable
  scatter, so every group comes out sorted and percentiles are lookups): mispredicts 2.14M
  to 0.69M, but 686M to 700M instructions and median 15.8 vs 14.3 ms (the 1,024-way
  scatters miss L1). Leaving selection out entirely (wrong output, as a bound) gives 572M
  instructions and 10% less wall time, so selection is the largest single cost left.
- **Pieces of 512 KB or 2 MB** instead of 1 MB, **sorting groups of up to 64 or 128**
  instead of 32: within noise.

## v8: reading with pread, decided on the box

v7 landed at 24.5 ms on the box against 13.1 ms here (the 1.9x of Measures, set by this
very pair), and the read-floor probe (14.6 ms just to scan the mapped input) said where to
look: on the box the mapping's page faults are expensive, and here they are not. Local
timing could not decide between reading paths (mmap and pread were within noise of each
other), so two correct builds that differ only in how they read went to the board head to
head, in consecutive slots:

| Build | Input read by | Run ID | T (s) | M (MB) | K |
|---|---|---|---|---|---|
| a12 | private read-only mmap (v7's way), one fault per 64 KB | run-5450dc2d90d1 | 0.0245 | 17.6 | 43.11 |
| a13 | `pread` of each 128 KB piece into a per-worker buffer | run-cbad326faf08 | 0.0207 | 18.5 | 46.55 |

The two builds share everything but the reading and the piece size that goes with it
(1 MB mapped, 128 KB read), so the 3.8 ms (15%) is the two together: the input's page
faults on the box, plus whatever the smaller pieces that stay in L2 bring; the runs do
not separate them. The extra copy costs less than both, and there is no mapping to tear
down at exit. The
buffers cost 0.9 MB of M.

(Two earlier read-floor probes, run-f70b36471354 and run-f71c8ec82249, aborted at startup:
the probe programs took their mode from argv, and the probe runner builds a probe with
track-a's own Dockerfile, which passes none. The probes were rewritten to need no
arguments, but by then every board push had to pass, so the question was settled with
the two correct builds above.)

The other changes from v7 to v8 are measured locally, by deterministic cachegrind counts
and interleaved runs:

| Step | Change | Run ID | T (s) | M (MB board, MiB local) | Points |
|---|---|---|---|---|---|
| 42 | The hour cache compared as one 16-byte SSE load (13 bytes masked) instead of two words: 679M to 657M instructions, since it frees registers in the hot loop | | — | — | |
| 43 | Second line-end candidate as the lowest set bit once the lowest is cleared (`blsr` + `tzcnt` instead of `lzcnt` arithmetic): 663M to 654M | | — | — | |
| 44 | The plan's condition variable is signalled only when a worker waits: about 1,900 `futex` calls fewer per run | | — | — | |
| 45 | Names stored with the space that follows them, so one masked 64-byte compare also checks the length (no separate length compare) | | — | — | |
| 46 | Memory released before the peak: report scratch pooled at most two, no text pooled for the last hours, the last worker unmaps the logs (and their page tables) | | — | 16.3 (local median) | |
| 47 | a12 on the board: steps 42-46 on v7's mmap reading | run-5450dc2d90d1 | 0.0245 | 17.6 | 81.7 |
| 48 | a13 on the board: the same with `pread` in 128 KB pieces; each worker frees its buffer after one of the last pieces | run-cbad326faf08 | 0.0207 | 18.5 | 83.4 |
| 49 | Count pass tallied by the record's top 12 bits (endpoint id and 5xx flag together), one increment per record instead of a packed 64-bit add: 654M to 628M instructions | | — | — | |
| 50 | Log blocks of 4 KB (one page per worker) instead of 1 KB: page faults 9.1k to 6.3k, since four workers no longer race to fault in the same fresh page | | — | — | |
| 51 | a19 on the board (commit 78c891f): a13 with steps 49 and 50. It came in 2 ms (10%) above a13's despite fewer instructions and faults, more than the box's noise, and was never explained; a20 (step 53) ran 20.2 and 20.9 ms | run-7090294e66d1 | 0.0227 | 18.4 | 82.4 |
| 52 | Each hour's text reserves room for all its lines at once (untouched pages cost nothing), so it never grows by copying: M median 17.1 to 15.9 MB, 300 fewer page faults | | — | 15.9 vs 17.1 (local) | |
| 53 | **a20 as submitted** (commit c8e36fb): steps 42-52, a19 plus step 52; 628M instructions, 6.2k page faults; S 530. A second board run, run-b471d3e70c35, took 20.9 ms | run-97875b283660 | 0.0202 | 17.9 | 83.76 |
| 54 | Review fixes, no change in behaviour: rustfmt, clippy clean (precedence parentheses, `div_ceil` for the rank, two statements for the rerun's truncate and rewind), the record layout as named constants with a compile-time check, hour logs sized in whole blocks with the page alignment `madvise` needs spelled out, `# Safety` on `insert`, `if` instead of `match` on booleans; S 537 | | — | — | |
| 55 | Simplification, same output: the hour row comes from the date directly (the span crosses only October's end, so no civil-date arithmetic) and the output stamp is made from the row, not copied from a line; one count of reported lines for the job, not one per hour; the piece count read from `rows`; single-use helpers (record decoding, the endpoint count) folded into their callers. 635M instructions (+0.4%); S 514. Local medians taken alongside step 54's build: 15.1 against 15.9 ms, within this machine's noise | | 0.0151 vs 0.0159 (local) | — | |
| 56 | **As submitted, before hardening** (commit 1cd61cf): steps 54 and 55 on a20, S 514. Two board runs: run-2491c9d07251 20.0 ms, 18.0 MB, K 47.72; run-237f722dfeb2 20.1 ms, 17.8 MB, K 47.67; median 20.05 ms, 17.9 MB | run-2491c9d07251 | 0.0200 | 18.0 | 83.94 |
| 57 | **As submitted** (commit 32b49b8): step 56 hardened against unexpected input, same output: errors name their path, value or size (`mmap of N bytes`, the bad timestamp, the endpoint count or length, now two messages); no `summary.txt` is left behind on failure; a rerun is noted on stderr; `--locked` in the Dockerfile; `ERROR_SHIFT` in the record-layout check; `MARK` renamed `INLINE_LATENCY_LIMIT` and the parse span type `Lines`; comments on the zero-to-final store the count check rests on, on the scatter's overflow when an hour was incomplete, on `reserve`'s lifetime, and the report's stages numbered. 634M instructions (no change); S 532. Two board runs: run-996cb0022b51 20.4 ms, 18.2 MB, K 46.93; run-361c33ed2406 20.9 ms, 17.9 MB, K 46.45; median 20.65 ms, 18.05 MB, against step 56's 20.05 ms, within the box's noise | run-996cb0022b51 | 0.0204 | 18.2 | 83.57 |

The reading decision rests on all the runs, not one pair: every build that maps the input
took 24.5 ms or more (v7 24.5, a12 24.5, a14 26.0), every build that preads it 22.7 ms
or less (a13 20.7, a19 22.7, a20 20.2 and 20.9, a21 20.05, step 56 20.0 and 20.1). On the
box, instruction counts matter less than kernel work: a12 does 5% fewer instructions
than v7 and ran in the same 24.5 ms, while switching to pread (no input faults, smaller
pieces) saved 3.8 ms. So the last step went after faults too.

### v8 experiments that did not make it

- **`MADV_POPULATE_READ` per piece** on the mapping (a12 plus one call per piece that
  fills its page-table entries instead of faults): run-d5a15d3df29a, 26.0 ms, slower than
  plain mmap on the box (24.5 ms); pread is the way.
- **Splitting the reports of the hours left when parsing ends into four parts** for the
  idle workers: the tail after parsing stayed at about 1 ms (each part repeats the count
  and scatter, and needs its own scratch).
- **Per-stretch block reservation**, so no line checks for the end of its block: 5M fewer
  instructions (0.8%), not worth eight more lines.
- **Pointer pairs instead of indices for the open blocks:** no change in instructions.
- **The main thread also parsing pieces whenever no text is waiting to be written** (a21,
  so all 12 hardware threads parse): run-b576df172e9e, 20.05 ms against a20's 20.2 and
  20.9 ms, inside the box's run-to-run spread, for 13 more lines. Not adopted.

## v9 and the v10 and v11 attempts

Everything from here is from the last day. Local figures are as in Measures: this laptop was shared with other benchmark jobs, so decisions used cachegrind instruction counts, `perf` software counters (faults, task-clock), event timelines and interleaved A/B runs. Board rows give the run ID. Where the board history has several runs of a build and only some have run IDs in our notes, the rows say how many runs there were and what range they covered.

The v8 baseline measured the same way (local, cpuset 0-11): wall median 14.6 ms, CPU 120-128 ms, 6.3k page faults, 636M instructions, 2.19M mispredicts. By `perf` cpu-clock: parse 28%, the kernel's `pread` copy 25%, reports 23% (select_nth 6%, `put_line` 5%, percentiles and small sorts 4%, count and scatter 6%), page faults about 10%, write 3%. Reports were often slow early (2 to 5 ms instead of 0.3 ms) and the writer, which writes in hour order, waited on them, with up to 17 texts piled up at 471 KB each.

| Step | Change | Run ID | T (s) | M (MB board, MiB local) | Points |
|---|---|---|---|---|---|
| 58 | Quiescent reports with u64 records in 4 KB blocks from a recycled pool (v9a/b): an hour is closed, then reported once every piece handed out before the close is parsed, so no report races a writer and no count check is needed; the reporter zeroes its blocks and returns them. Correct on all 52 cases; page faults 6.2k to 2.4k, CPU -4%, wall equal. But the pool keeps its high-water mark resident: blocks cut per run varied from 545 to 2,850 with the load (frontier stalls) and M followed, 18.4 to 29.9 MiB against v8's 16 to 17. Back to v8's per-hour regions with `madvise` after each report (resident = live hours only); the quiescent reports stay. Also dropped: lock-free blocks with per-worker free lists (v9d: faults 4.5k, slower); read buffers allocated by `main` before spawning (the 11 threads then started in 1.7-2.2 ms instead of 0.65 ms, since mapping a stack waits on workers faulting adjacent buffer mappings; workers allocate their own); the endpoint order kept sorted at insert (first pieces about 0.3 ms slower, the insert lock held longer during the burst of 1,842 inserts; back to one lazy sort). futex calls rose from 165 to 470-520 with the pool's lock | | — | 18.4-29.9 vs 16-17 (local) | |
| 59 | v9g: v8's per-hour logs, quiescent reports, errors through `die()`, lazy endpoint order. Speed equal to v8; 649M instructions (v8 636M), 2.13M mispredicts; S 523 | | — | — | |
| 60 | Percentiles of groups of up to 64 by rank counting, with vector compares and no branch on the data, instead of a sort; larger groups keep `select_nth_unstable`. Cachegrind had select and the small sorts at 14% of the instructions and 62% of all mispredicts (select_nth 557k, small sorts about 790k). Microbenchmark over all 126,908 groups of seed 1 (one core, min of 15): select plus sort 11.57 ms, rank counting plus select 9.75 ms (-16%); single-core whole runs -2 to -4%. A 264-bucket log histogram to bracket the ranks of big groups bought nothing more (9.71 ms) for 20 lines: dropped. Instructions +51M | | 0.00975 vs 0.01157 (local, microbenchmark) | — | |
| 61 | Memory cuts (v9k): report scratch allocated once at full size (grown by doubling, each worker's malloc arena kept about 130 KB of freed smaller copies: smaps showed 11 arenas of 130-230 KB); room per hour speculatively 2^17 slots (the busiest hour has about 74,000 lines, the same daily curve on seeds 1, 7, 101 and 202: 14.5k to 73.6k lines an hour), so hours 512 KB apart share page-table pages (page tables were 1.27 MB); a fuller hour fails the speculation like a late line. Local M above base: 15.3 against 16.6 MiB (-1.3 MiB). Uncapped text and scratch pools (v9i) cost +2 MiB and were dropped | | — | 15.3 vs 16.6 (local) | |
| 62 | Wide latencies (2^20 and up, about 1.5%) as a two-slot record, the high bits in an id-0 slot before the record, instead of the hour's side list: no contended `fetch_add`, one reservation less, -0.5 MiB. +45M instructions (+6%) in cachegrind, single core +1.4%; kept for S and M. A wide record that would start in a block's last slot leaves it empty instead of checking `n + wide >= end` on every line: 724M to 709M instructions. The rerun needs room for 2 slots per wide line (len / 20), found in review and tested by a new all-wide crowded case. Dropped: the high bits in the slot after the record with a fix-up in the scatter (745M) | | — | -0.5 (local) | |
| 63 | The main thread also parses pieces whenever no text is waiting (locally neutral; on the box the same change, a21 in the v8 experiments, ran 20.05 ms against a20's 20.2 and 20.9 ms, run-b576df172e9e). Dropped after trying: starting threads as a tree (no gain locally, spawning is serialised elsewhere); splitting the last hours' reports into four parts by endpoint (the tail got longer, each part repeats the count and scatter, 40% of a report); per-piece `mmap(MAP_POPULATE)` plus `munmap` of 1 MB pieces (CPU -14%, no copy, but wall +35% locally: mmap and munmap serialise on the address-space lock, context switches 636); a whole-file map with `MADV_POPULATE_READ` and `MADV_DONTNEED` per piece (CPU = pread, wall worse); `_exit` right after the last write (no change, teardown is the kernel's) | | — | — | |
| 64 | **v9 as built (a9, candidate 1).** Quiescent reports, two-slot wide records, 2^17 slots per hour speculatively, one-shot scratch, rank-counting percentiles, lazy endpoint order, the main thread parsing between writes, `die()` errors. S 521 (v8 532). Instructions 709M (v8 636M), mispredicts 1.60M (v8 2.18M, -26%). M above base, 8 local runs: median 15.4 MiB against v8's 16.4. Wall and CPU within noise of v8 on the shared laptop (medians swung by 30% with the load); single-core total work -2 to -4%. Local timeline (median ms from exec): main 0.6, last worker 1.4, last piece 11.8-13.5, last report +0.9-1.0, last write +0.1, exit +1.1. Correctness: 52 earlier cases plus 5 new ones (wide_crowded, wide_stream, wide_late, early_line, busy_hour), seeds 1, 7, 101, 202 and the late, shuffled and one-group-big files all pass; the reruns happen exactly where expected; 0 bad in 300 runs of seed 1 and 100 of seed 7 through the Dockerfile build | | — | 15.4 vs 16.4 (local) | |
| 65 | **v9 on the board (a9, commit 6ded953).** First run (as the candidate, commit 81945ba) 19.5 ms, 16.9 MB, K 48.84, against v8's 20.0 to 21.1 ms; then 19.4 ms, 17.2 MB, K 48.71 (run-438cdec0eae2, the adopted commit). Later runs of the same code: run-5bf3f6bb5ef5 19.3 ms, 16.9 MB, K 49.05; run-d27a5073a8e6 19.8 ms, 17.3 MB, K 48.18; run-7d7450969105 20.7 ms, 21.8 MB, K 45.06; run-d024bbf208cc 21.1 ms, 22.1 MB, K 44.52; run-ccb75cc37209 19.3 ms, 17.1 MB, K 48.94. Of 12 board runs of this code (five more, at commits 501b40f, 87f535b, 62a1095, 2938130 and 9e6a7d5, ran 19.4 to 20.2 ms), T ran from 19.3 to 21.1 ms; the two slowest also read 21.8 and 22.1 MB, the same pattern as the spikes of v10 but milder | run-89cc8aa5d119 | 0.0195 | 16.9 | 84.44 |
| 66 | a9b, rejected: a9 with 64 KB pieces instead of 128 KB. Locally M -1.1 MiB more, CPU +1 to 2% (noise), S 521. On the box 20.0 ms, 16.3 MB, K 48.56 against a9's 19.5 and 19.4 ms: 0.5 to 0.6 ms slower for 0.6 to 0.9 MB less memory | run-b00309653ee2 | 0.0200 | 16.3 | 84.31 |
| 67 | a9c, rejected: a9 with a private mmap per 1 MB piece (demand faults, never unmapped; `pread` for the last piece), a probe of whether per-piece VMAs change the whole-file map's cost. Locally CPU -5%, wall within noise, no read buffers; S 551. On the box 24.1 ms, K 43.66, 4.6 ms worse than a9. mmap loses on the box again, so that line is closed | run-5ea620eb10de | 0.0241 | 16.7 | 82.01 |
| 68 | The tail after the last piece (local, on v9): when the last piece is parsed, hours 68 to 71 are still to report, and 70 and 71 (16:00 and 17:00 UTC, about 73k lines each) are the busiest; a final report takes about 0.7 ms (count 35-60 us, scatter 100-190 us, format 300-430 us). Dropped: formatting split into batches for idle threads (tail 945 to 831 us, +45 lines); parts that each scan the whole log but scatter and format one endpoint range (4 or 6 per hour: each part about 360 us against 680 whole, but more total work at the end, tail 909 to 1,073 us for 4 and 1,197 us for 6, 30 runs); a channel plus writer thread version of the split (the writer's wake-up latency ate the gain); keeping texts pooled while hours remain (final formats -70 to -90 us, M +0.3 to 0.8 MiB). Under load one descheduled thread holding a piece stalls the frontier so that all hours report at the end (5 to 9 ms tails in some local runs); the box is dedicated. Net: no tail change worth its lines | | — | — | |
| 69 | **v10 (a10).** No writer thread and no channel: all 12 threads, the main thread included, parse, report and write, and a thread that finishes an hour writes it and any later hours waiting, under the output lock; `mallopt(M_ARENA_MAX, 1)` (one malloc arena instead of one per thread, each of which kept about 130 KB: anon at the end of `main` 1,984 KB to 648 KB, the peak not visibly moved in 12-run sets, median about 15.2 MiB both ways); inlining and spelled-out constants. S 521 to 481. Instructions 714M (a9 703M, +1.5%). 16-run A/B: wall median 14.24 against 14.09 ms, CPU 124.3 against 126.9 ms, tail 907 against 981 us, all noise. Stress: 0 bad in 300 runs of seed 1, 100 of seed 7, 50 each of seeds 101 and 202. First board run 19.6 ms, 17.2 MB, K 49.15 (84.57 points), one of the best runs of the track (another run of the same code, commit 4a6b6b1, reached 19.5 ms, K 49.62, 84.78 points). Adopted (commit 799d8d3) and then measured 22.3 ms, 21.7 MB, K 43.96 (82.15 points). Ten board runs of this code in all: 19.5, 19.6, 19.7, 20.3, 20.3, 20.8, 22.3, 23.4, 26.2 and 31.2 ms (commits 4a6b6b1, 3c649aa, a10408f, 2c6533e, 30aa122, 5018217, 799d8d3, 6d7f94c, 5811f90, a8a250f). Reverted to v9 (commit 501b40f) | run-dcb842591f8b | 0.0196 | 17.2 | 84.57 |
| 70 | a10b, rejected: v10 without the `mallopt` line, to see whether the one arena caused the spikes. 25.1 ms, 21.5 MB, K 41.49 (80.89 points), S 480: it spiked too, so the arena setting was not the cause | run-6b2fa30482f4 | 0.0251 | 21.5 | 80.89 |
| 71 | **v11 (a11), not kept.** v9's algorithm with the code trimmed for S, behaviour-preserving: constants spelled out, `reserve` inlined, the speculation flag a `lag` field, `read` merged into `piece`, `put_line` merged into the report loop, imports merged. S 521 to 464 (Rust 458 + Dockerfile 6; the 440 target was missed because rustfmt turns most planned let-inlinings back into the same number of lines). Cachegrind against a9 in the same session: instructions 702.8-705.9M against 703.4-704.2M, mispredicts 1.597-1.603M against 1.595-1.608M; M above base 15,600 against 15,586 KiB; all 52 cases, the adversarial set and seeds 1, 7, 101 and 202 pass; 0 bad in 300 runs of seed 1 and 100 of seed 7. Board: 28.9 ms, 18.4 MB, K 40.12 (80.17 points), then on a second try 20.4 ms, 16.9 MB, K 48.62 (84.34 points) | run-578b1fd5f3ab, run-86f1982555d3 | 0.0289, 0.0204 | 18.4, 16.9 | 80.17, 84.34 |
| 72 | **Decision: v9 stays.** Board history of the three builds that differ from v9 only in the writer thread, the arena setting and the trimmed code (v10, v10b, v11): 13 runs, 4 over 24 ms (25.1, 26.2, 28.9 and 31.2 ms) and 6 over 22 ms; v9: 12 runs, the slowest 21.1 ms. We could not tie the spikes to one edit: v11 changes no algorithm, and v10b removes the arena setting without curing it. The gain from the smaller S was about 0.4 points of the machine score by the K formula (S 521 to 464), against the risk of a spike in the five final runs. | | — | — | |
