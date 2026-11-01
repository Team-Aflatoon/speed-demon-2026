# Track A: performance log

## Summary

**19% fewer instructions moved the board 0%; moving 11 ms of work off the critical path
moved it 25%.**

Final: step 56, commit 1cd61cf: pread pieces + per-hour reports while parsing, verified by
exact counts. Board: 20.0 ms, 18.0 MB, K 47.72 (run-2491c9d07251) and 20.1 ms, 17.8 MB,
K 47.67 (run-237f722dfeb2), median 20.05 ms, 17.9 MB; S 514; reference 32.05 s, 228 MB.
Decisive steps (all board runs):

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
6. Simplification and review fixes, same output: S 530 → 514 at the same T (step 56).

Lesson: on the box, phase structure and kernel work (faults, barriers) set T; local
instruction counts did not (v3 to v5: 1,358M → 1,099M instructions, 31.8 → 34.6 ms).

Red runs on the board: besides the two probes of item 3 (wrong output by design), two
read-floor probes aborted at start, run-f70b36471354 and run-f71c8ec82249: they took
their mode from argv and the probe runner passes none (see v8). Every other board run
of this track passed.

## Measures

- **T**: wall time from exec to exit. A board row's T is what the board reported for that
  run ID; every other T is measured on this laptop and marked "(local)".
- **M**: the run's cgroup `memory.peak`. Board rows are the board's; local values are
  marked "(local)". Up to step 31 a local M is `bench/bench.sh`'s, which includes about
  5 MB of shell in the container; from v7 (step 32) on, a local M is `memory.peak` minus
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
  run-b471d3e70c35), step 56 20.0 / 20.1 ms (run-2491c9d07251, run-237f722dfeb2). One
  outlier is set aside: v2 at 44.2 ms (run-60abe77e7171) against 33.0 and 32.6 ms.
- **Local T and M cells**: a value appears only where that step was measured on its own,
  as the A/B pair that decided it (this step against the one before, same session); "—"
  where the decision rested on other evidence, which the row then gives (instruction
  counts, faults, single-core times).

## Labels

- **Steps** 0 to 57 number every change in order; a step is one row.
- **v1 to v8** are the versions submitted in turn: v1 step 8, v2 step 14, v3 step 18, v4
  step 25, v5 step 28, v6 step 31 (never run whole on the board), v7 step 41; v8 is the
  pread line that followed, submitted as a13 (step 48), a19 (51), a20 (53) and finally
  step 56 (commit 1cd61cf).
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

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (the board's reference numbers) | | 32.05 | 228 | 0 |
| 1 | Rust v1: mmap, 12 workers, per-worker endpoint hash table, per-group block chains, scalar newline and latency loops, merge by sorting all workers' names, per-hour formatting, one write | | 0.039 (local) | 37.5 (local) | |
| 2 | AVX2 newline bitmask; loop-free SIMD latency parse; skip workers that never saw an hour when summarising | | 0.030 (local) | 37 (local) | |
| 3 | Merge endpoint tables through one more hash table and sort the 1,842 survivors, instead of sorting 22,104 names (that phase: 5-6 ms to 0.9 ms) | | 0.029 (local) | 37 (local) | |
| 4 | Copy names into a compact per-worker arena (compares against first occurrences in the input cost a TLB miss on most lines); check the stored hash before comparing bytes; fixed-size compares instead of `memcmp` | | 0.023 (local) | 37 (local) | |
| 5 | Summarise into 24-byte records first, free the chains, then format in parallel: the output's page cache and the chains no longer peak together | | — | 35 (local) | |
| 6 | Branchless name compare: two 32-byte AVX2 compares with the bits past the length masked (interleaved A/B min 24.1 vs 25.7 ms) | | 0.0241 vs 0.0257 (local) | — | |
| 7 | Power-of-two blocks with a separate link array (a mask instead of `% 15` per line); each hour split into four tasks by endpoint rank for balance; one sequential writer (single core 99 to 96.6 ms) | | — | 33 (local) | |
| 8 | **v1 as submitted** (commit 0ed7fba). Fewer lines for S (rank + five numbers as one tuple struct, splatted SIMD weights): 456 to 421 Rust lines; S 427. Reruns: run-1fc879b67a36, run-b114fce0b3dd (44.0 ms, 49.0 MB) | run-a288ae43d4e8 | 0.0461 | 47.5 | 71.3 |
| 9 | Line ends found per 4 KB in bulk: both candidate newlines of each 64-byte block are stored unconditionally and the count advances by popcount, so the per-line loop has no unpredictable exit at every block boundary (single core min 94.8 vs 104.5 ms; 12 threads min 30.6 vs 32.2 ms) | | 0.0306 vs 0.0322 (local) | — | |
| 10 | Endpoint table as one arena of records (hash, id, length, name) so a hit reads its slot and then one record; 4,096 slots (neutral within noise; kept for the smaller footprint) | | — | — | |
| 11 | Report and write overlapped: workers report the hours in order and the main thread writes each one as soon as all earlier ones are out (buffered writes; a bounded channel caps the texts in flight; interleaved min 28.7 vs 33.9 ms under load) | | 0.0287 vs 0.0339 (local) | 36 (local) | |
| 12 | Per-worker, per-hour logs of 4-byte records (`id << 20 \| 5xx << 19 \| latency`, latencies of 2^19 and up kept aside) in 256 KB chunks; each hour reported by counting-sorting its logs by endpoint into one array, so every group is one contiguous slice; each hour's chunks are freed (unmapped) as soon as it is reported, so the logs shrink while the output's page cache grows (CPU 128 vs 159 ms; min 22.8 vs 26.1 ms) | | 0.0228 vs 0.0261 (local) | 26 (local) | |
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
- **Is the 239 MB input mapping itself expensive (faults, and tearing it down at exit)?**
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

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 32 | Rewrite: per-(worker, hour) logs in one `MAP_NORESERVE` reservation, a barrier, then per-hour reports (739M instructions against v6's 1,077M; wall equal to v6) | | 0.0195 (local) | 20 (local) | |
| 33 | The main thread parses during ingest and writes afterwards, with 11 reporters, so the writer has a CPU of its own (its tail after the last report: 3 ms to 0.2 ms) | | 0.0145 (local) | — | |
| 34 | One log per hour shared by all workers, which reserve 1 KB blocks of it with one `fetch_add`; pieces in file order; one endpoint table shared by all workers | | — | — | |
| 35 | Speculative reporting: an hour is reported as soon as the parsed prefix is two hours past it, while parsing goes on; an exact count check, and a rerun without speculation if it fails (median 13.0 vs 14.5 ms, CPU -8%) | | 0.0130 vs 0.0145 (local) | 19.5 (local) | |
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
On the generator's data the margin is about 50 minutes (shard 0 uploads an hour 5 to 10
minutes after it ends; the other shards are at most 2.5 minutes late); full seeds 1, 3, 7
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

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 42 | The hour cache compared as one 16-byte SSE load (13 bytes masked) instead of two words: 679M to 657M instructions, since it frees registers in the hot loop | | — | — | |
| 43 | Second line-end candidate as the lowest set bit once the lowest is cleared (`blsr` + `tzcnt` instead of `lzcnt` arithmetic): 663M to 654M | | — | — | |
| 44 | The plan's condition variable is signalled only when a worker waits: about 1,900 `futex` calls fewer per run | | — | — | |
| 45 | Names stored with the space that follows them, so one masked 64-byte compare also checks the length (no separate length compare) | | — | — | |
| 46 | Memory released before the peak: report scratch pooled at most two, no text pooled for the last hours, the last worker unmaps the logs (and their page tables) | | — | 16.3 (local) | |
| 47 | a12 on the board: steps 42-46 on v7's mmap reading | run-5450dc2d90d1 | 0.0245 | 17.6 | 81.7 |
| 48 | a13 on the board: the same with `pread` in 128 KB pieces; each worker frees its buffer after one of the last pieces | run-cbad326faf08 | 0.0207 | 18.5 | 83.4 |
| 49 | Count pass tallied by the record's top 12 bits (endpoint id and 5xx flag together), one increment per record instead of a packed 64-bit add: 654M to 628M instructions | | — | — | |
| 50 | Log blocks of 4 KB (one page per worker) instead of 1 KB: page faults 9.1k to 6.3k, since four workers no longer race to fault in the same fresh page | | — | — | |
| 51 | a19 on the board (commit 78c891f): a13 with steps 49 and 50. It came in 2 ms (10%) above a13's despite fewer instructions and faults, more than the box's noise, and was never explained; a20 (step 53) ran 20.2 and 20.9 ms | run-7090294e66d1 | 0.0227 | 18.4 | 82.4 |
| 52 | Each hour's text reserves room for all its lines at once (untouched pages cost nothing), so it never grows by copying: M median 17.1 to 15.9 MB, 300 fewer page faults | | — | 15.9 vs 17.1 (local) | |
| 53 | **a20 as submitted** (commit c8e36fb): steps 42-52, a19 plus step 52; 628M instructions, 6.2k page faults; S 530. A second board run, run-b471d3e70c35, took 20.9 ms | run-97875b283660 | 0.0202 | 17.9 | 83.76 |
| 54 | Review fixes, no change in behaviour: rustfmt, clippy clean (precedence parentheses, `div_ceil` for the rank, two statements for the rerun's truncate and rewind), the record layout as named constants with a compile-time check, hour logs sized in whole blocks with the page alignment `madvise` needs spelled out, `# Safety` on `insert`, `if` instead of `match` on booleans; S 537 | | — | — | |
| 55 | Simplification, same output: the hour row comes from the date directly (the span crosses only October's end, so no civil-date arithmetic) and the output stamp is made from the row, not copied from a line; one count of reported lines for the job, not one per hour; the piece count read from `rows`; single-use helpers (record decoding, the endpoint count) folded into their callers. 635M instructions (+0.4%); S 514. Local medians taken alongside step 54's build: 15.1 against 15.9 ms, within this machine's noise | | 0.0151 vs 0.0159 (local) | — | |
| 56 | **As submitted** (commit 1cd61cf): steps 54 and 55 on a20, S 514. Two board runs: run-2491c9d07251 20.0 ms, 18.0 MB, K 47.72; run-237f722dfeb2 20.1 ms, 17.8 MB, K 47.67; median 20.05 ms, 17.9 MB | run-2491c9d07251 | 0.0200 | 18.0 | 83.94 |
| 57 | Review fixes after step 56, same output (not yet on the board): errors name their path, value or size (`mmap of N bytes`, the bad timestamp, the endpoint count or length, now two messages); no `summary.txt` is left behind on failure; a rerun is noted on stderr; `--locked` in the Dockerfile; `ERROR_SHIFT` in the record-layout check; `MARK` renamed `INLINE_LATENCY_LIMIT` and the parse span type `Lines`; comments on the zero-to-final store the count check rests on, on the scatter's overflow when an hour was incomplete, on `reserve`'s lifetime, and the report's stages numbered. 634M instructions (no change); S 532 | | — | — | |

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
