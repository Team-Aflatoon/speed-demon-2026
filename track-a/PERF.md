# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

T and M below are local measurements (`bench/bench.sh`, the harness's sandbox flags, cpuset
0-11 of an i7-14650HX, cgroup `memory.peak`), median of the runs shown, unless a row says
otherwise. The scoring box is roughly 1.5x slower. The machine was shared with other
benchmark jobs during part of the work, so close calls were decided by interleaved A/B runs
of saved binaries and by single-core (`taskset -c 0`) runs, and noted as such. Run IDs are
added once a step has a board run.

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
| 8 | Fewer lines for S (rank + five numbers as one tuple struct, splatted SIMD weights): 456 to 421 Rust lines after rustfmt; no change in T | | 0.023 (local) | 33 (local) | |

## Experiments that did not make it

- **Parallel `pwrite` of the formatted hours.** A profile showed 6% of all samples in
  `osq_lock` under `ovl_write_iter`: the writes queue on the file's inode lock, so the
  threads spin instead of writing in parallel. Replaced by one writer (step 7).
- **Formatting straight into a shared `mmap` of the output file.** Needed the exact length
  of every line up front (another pass over 127k records) and its write faults cost more
  than the copy they saved: the write phase went from about 4 to 5 ms. Reverted.
- **Fused summarise-and-write pipeline with an `O_DIRECT` writer.** `O_DIRECT` did keep the
  report out of the page cache (`memory.stat` showed `file` at 0 after the write), but the
  peak is the latency chains, not the output, and hours finished out of order piled up
  behind the synchronous writer: M went up to 37-42 MB and the phase took 11-13 ms on the
  local virtual disk. Reverted; it may return once the chains are smaller.
- **Upper-bound probes.** Comparing only the 64-bit hash (wrong in general, never shipped)
  saved 1-1.5 ms, so the byte compare is not the problem; skipping the whole lookup saved
  about 22% of single-core time, which is why the lookup is the first item for next steps.

## What the profile says now

Single core, `perf` with a software clock: the endpoint lookup is about 30% of samples, the
rest of the ingest loop about 25% (spread over many instructions: input streaming, the
group-table read, block stores), the summarise phase about 18%, page faults and page
zeroing for the chains about 5%. With 12 workers on 6 cores ingest takes about 12 ms
locally, summarise 3.5 ms, freeing the chains 1 ms and the write 3 ms.
