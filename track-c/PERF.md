# Track C: performance log

## Summary

Final: **v5.2** (row 40): v5.1 with bounds-check-free first pass and prefetches, unchecked region/name/pair reads (each with a SAFETY note), a cheaper number-length check and the BOM test on the slow path; instructions -5.6% against v5.1. Board 32.4 ms, 16.7 MB, K 28.60, 72.82 points (run-52c515f71c76); S 882. Before it, **v5.1** (commit 3aa2ae2, row 39): v5 plus one-register number conversion, unchecked in-bounds parser reads and 128-byte pair blocks. Board 33.8 ms, 16.3 MB, K 28.25, 72.55 points (run-658779ed2eee); S 858; instructions -16.5% against v5 locally. Before it, v5 (commit 6d23622, row 38), a clean-room rework: Board 37.4 ms, 16.9 MB, K 26.81, 71.42 points (run-41dfe519e529); S 827, D 1. The reference is 14.44 s and 367 MB. Why it won: on the box the distinct-name path cost about 21 ms of v4.2's 48.5, mostly an atomic OR per record on a shared bitset line that another core held. v5 interns names in a read-mostly table and appends private (id, group) pairs, which are counted after one barrier in an L1-resident bitset. It costs 4.6 MB more memory for the pair arenas, which is worth it: K is 5.6% higher than v4.2's 25.38. See "v5: two-phase distinct names" below.

Previous final: **v4.2 as submitted** (commit 5f8b6df, row 36). Board 48.9 ms, 12.6 MB, K 25.15, 70.03 points (run-31b13fc9019b) and 50.5 ms, 12.3 MB, K 24.85, 69.77 points (run-8721232bf1f2): median 49.7 ms. S 767. v4.2 is v4.1 (row 35: the race fix, 49.1 ms, S 734) plus error paths only. S rose by 33 lines so that input the generator never writes fails loudly: the byte offset of the first grammar error, the byte offset of a bad regions.csv line, a note on stderr for each fallback, and a cap on the table retries.

v4.2's repository state (commit eba990a; code fbf7e89 = 5f8b6df plus the removal of a partial output on a failed write, S 768): run-d79219fb688a, 48.5 ms, 12.3 MB, K 25.38. Runs of this error-path code: 48.9, 50.5 and 48.5 ms, median 48.9 ms.

**A race in v4, found by a stress test and fixed in one line.** v4 as first submitted (row 33, run-1e5d2353fd9f) printed a wrong output in about 1 run of 100: one group's distinct-name count off by one. A 300-run stress test of the submitted image found it; final scoring needs all five runs correct, so it was about a 5% chance of a zero. `Names::fill` wrote all four words of the name's key into its table slot, and the fourth word, the hash, landed on the slot's `meta` for a moment before the Release store of its real value; a lookup on another core could take that value for a published slot of another length, probe on, and insert the same name a second time. The fix writes only the three name words (row 35). The board never showed it: each board run is a single run.

Decisive steps, all board runs:

1. v1: 12 threads, chunk cuts guessed and then proven by quote parity, a shared lock-free name table: 61.9 ms, 16.2 MB (run-c898fb001979; a rerun gave 62.0 ms, run-247c2a2868f1).
2. v2: an AVX2 bitmask parser (prefix-XOR quotes), fixed 24-byte name keys, batched prefetching, parallel start-up: 52.8 ms, 12.1 MB (run-be5f1e4a22bf); v3, code hygiene, 52.4 ms (run-d3c2badf14df).
3. Failed: owner-partitioned names, 20 to 25% faster locally but 52.2 ms on the box at S 889 (run-0caa577fd90b); reverted.
4. Probes (wrong output on purpose, so red on the board): run-1f26df4f9d6d, run-eec975c57826, run-30db742f2dea, run-16449b27e8f7, run-c09fe1629bb8. The scan costs the same on both machines; per-record work costs 2 to 3 times more on the 2.5 GHz, turbo-off box.
5. pread instead of mmap: 49.3 ms (run-2890a0259c6b); 64 KiB pieces and a 128-byte pad bring M back: v4, 49.2 to 49.3 ms, 12.4 to 12.5 MB.
6. v4.1, the race fix: 49.1 and 49.4 ms, 12.1 and 12.3 MB. A simplification pass shipped with the fix first ran 58.9 ms (row 34) and was reverted.
7. v4.2, error paths: 48.9 and 50.5 ms, 12.6 and 12.3 MB, the same speed.
8. v5, a clean-room rework with two-phase distinct names: 37.4 ms, 16.9 MB, K 26.81 (run-41dfe519e529).

For v4.x, of the board's 12.1 to 12.6 MB, about 9.7 MB is the program's own tables and buffers: a 4.2 MB name table, a 3.7 MB pair bitset, a 1.0 MB region map and 0.8 MB of read buffers. The rest is code, stacks, page tables and the container runtime's own processes, which are charged to the same cgroup before `main` runs.

Lessons: local A/B on memory-bound changes did not transfer to the box, only measurements on the box did; and one board run per build cannot show a 1% failure, a few hundred local runs can.

## Log

K is the board's efficiency coefficient, K = (T_ref/T)^0.50 × min(M_ref/M, 50)^0.20 × min(S_ref/S, 1)^0.15 × (D_ref/D)^0.15, and Points = 50 × log10(K), clamped to 0..100; Points are what the track total uses. Points are given for board runs of correct builds.

Rows with a Run ID are board measurements. All other T and M are **local** (i7-14650HX, Docker Desktop, `--cpuset-cpus=0-11`, the bench script that mirrors the harness); the board box was about 1.6x slower than this machine when quiet. Units: board M is bytes / 10^6 (MB). Local M is the bench's cgroup peak in KiB divided by 1,000, so a local "MB" is 1.024 MB; local figures are only compared with each other. Power-of-two sizes are in KiB and MiB. The local machine was shared with other heavy benchmark jobs during this work, so single runs varied by up to 2x. Small changes were therefore compared by interleaving runs of the two builds (A/B) and taking medians of wall time and of total CPU time (`perf stat` task-clock), which is far less sensitive to the other jobs. Hardware counters are not available in this VM. "CPU 1t" means one worker on one core.

Labels: v1 to v5 are the builds submitted as the track's entry; c1 to c11 are probe and candidate builds, numbered as they were made. c1 to c6 are the probes in the table below, c8 is row 31's PREFETCHW build, c9 its group-bits-in-the-slot build, c10 is row 32's first pread build (128 KiB buffers), and c11 is row 33's version of it, which became v4. c-next and c-next2 are row 30's local experiments.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 14.44 | 367 | 0 |
| 1 | Rust, 12 threads, mmap. Scalar byte-level parser; chunks cut after an LF followed by a digit or BOM and accepted only if each ends exactly where the next starts. Names in a shared lock-free table, with an atomic group bitset inside each entry. Correct on seeds 1 and 7 and all fuzz cases from here on. | | 0.048 (local) | 15 (local) | |
| 2 | Ablation: names cost about 35 of the 48 ms, because every atomic OR hit a line that other cores also read to compare names. Tried owner-partitioned names (each worker owns the names of one hash range) exchanged in lockstep rounds between barriers. | | 0.050 (local) | 16 (local) | |
| 3 | Rounds lost about 25% to barrier waits, since one slow worker stalls all. Replaced by asynchronous mailboxes, and the byte parser by AVX2 bitmasks with a prefix-XOR quote mask. | | 0.032 to 0.040 (local) | 20 (local) | |
| 4 | Mailboxes grew without bound (20 MB peak). Replaced by fixed single-producer rings per (sender, owner) pair. | | 0.036 to 0.043 (local) | 13 to 16 (local) | |
| 5 | Separators flattened into a buffer eight at a time, no branch per separator. Single-thread parse 100 to 90 ms. | | 0.034 (local) | 13 (local) | |
| 6 | Failed: fixed 32-byte messages so ring reads do not depend on one another. No change (A/B 33.8 vs 33.3 ms). Kept for simplicity, later removed. | | 0.034 (local) | 13 (local) | |
| 7 | Shared interning to dense ids, 8-byte (id, group) messages. First slower (49 ms): the insert counters shared cache lines with data every core reads. Padding those atomics to their own lines: 39 ms. Transport now nearly free (ablation), interning about 15 ms. | | 0.039 (local) | 13 (local) | |
| 8 | Batches of about 32 records: parse all, prefetch their region-map entries and name slots, then join. Single-thread interning 165 to 95 ms; 12 threads within noise. | | 0.039 (local) | 13 (local) | |
| 9 | Name table slots of 32 bytes holding up to 24 name bytes inline, so a lookup reads one line instead of two. | | 0.039 (local) | 13 (local) | |
| 10 | Per-owner ids, removing a 64-bit division per record; worker state allocated before the threads start (each worker's own first allocations caused TLB shootdowns). Profile then showed 6.8% in the kernel's page-table lock. | | 0.035 to 0.040 (local) | 13 (local) | |
| 11 | Name table on transparent huge pages (2 faults instead of about 1000, and fewer TLB misses). A/B median 43.2 to 40.3 ms. | | 0.040 (local) | 14 (local) | |
| 12 | Size: S was 990 after rustfmt. Rewrote the name path as a shared atomic bitset (prefetched before the OR) instead of rings and per-owner state; one struct of per-worker results. S 990 to 735. About 10% more CPU than the ring version in A/B, but no worker can stall another, which the rings did whenever the machine was oversubscribed. | | 0.042 (local) | 16 (local) | |
| 13 | Pair bitset on huge pages too. A/B CPU time 476 to 447 ms. | | 0.042 (local) | 17 (local) | |
| 14 | Only the first four workers split regions.csv, so records start before the last thread is up. A/B median 46.5 to 44.7 ms. | | 0.041 (local) | 17 (local) | |
| 15 | Huge pages faulted in from one thread up front: racing first touches were each charged 2 MiB. Peak spread 15 to 20 MB down to 16.4 to 17.0 MB. Checks for the limits the code relies on (4 GiB file, region ids). Seed 1 bench mean of middle 3: 37.9 ms (local); seed 7 PASS. **v1.** Board: 61.9 ms, 16.2 MB, K 21.31, rank 41/80; a rerun of the same build gave 62.0 ms (run-247c2a2868f1). The box was about 1.6x slower than the local bench. | run-c898fb001979 | 0.0619 | 16.2 | 66.4 |
| 16 | Ablation of the name path: hashing cost as much as the table lookup (about 7 ms each), because the name was walked as a variable-length word iterator twice. Now a fixed key of the first 24 bytes (three masked words, all of nearly every name) is built once and used to hash, compare and insert. A/B CPU 1t 326 to 298 ms. | | | | |
| 17 | The waits that could spin no longer do: the start-up barrier yields; no other wait remains, as workers simply stop when no chunk is left. Threads start as a binary tree, with worker 0 on the main thread, so the last one runs after about four thread starts instead of twelve. Region workers fault in the name tables in parallel. Worker stacks are 256 KiB, so no kernel can back them with huge pages. | | | | |
| 18 | Back to 4 KiB pages for the tables (MADV_NOHUGEPAGE), still faulted in up front with each 2 MiB stretch given to one worker. Same CPU as huge pages in A/B (441 vs 472 ms, within noise), but a huge-page fault here sometimes stalled for 5 ms while the kernel compacted memory, and a kernel set to THP=always now cannot charge 2 MiB for a mostly untouched mapping (the long-name arena). Removing the prefault entirely: A/B median 39 to 46 ms, so it stays. | | | | |
| 19 | Size: Agg as six sums plus min and max, one record struct for parsing and joining, one shared struct, one failure flag, shorter expressions that rustfmt keeps on one line. S 753 to 663. | | | | |
| 20 | Records parsed in place into a batch kept in the chunk state, instead of returned as 80-byte Option values and copied. A/B CPU 1t 280 to 232 ms. | | | | |
| 21 | Integers parsed with their length known from the separators: up to eight digits checked and converted in one step, no loop and no overflow checks (the old loop remains for longer numbers). The amount's dot must be three bytes before its end. Forced inlining. A/B CPU 1t 225 to 191, then 189 to 178 ms. | | | | |
| 22 | Failed or no gain, all reverted: PREFETCHW before the atomic OR (A/B CPU 263 vs 273 ms), 48 instead of 16 chunks per worker (279 vs 276), PCLMULQDQ for the prefix XOR (181 vs 181 1t), MADV_POPULATE_WRITE for the prefault (264 vs 265). | | | | |
| 23 | 12 threads vs v1 in A/B: CPU 369 to 288 ms, wall median 46 to 39 ms; CPU 1t 220 to 171 ms. Seed 1 bench mean of middle 3: 31.8 ms (local). | | 0.0318 (local) | 16.7 (local) | |
| 24 | Start-up, measured with a per-worker timeline: records began at about 2.5 to 2.9 ms. Now the first four workers fault in the name tables while the other eight split regions.csv, and the 1 MB region map is lazily zeroed memory (eight groups per word) filled and faulted in by those workers instead of zero-filled by the main thread. Records now begin at about 1.9 ms. A/B wall median 38.7 to 37.6 ms, CPU 292 to 278 ms. | | | | |
| 25 | **v2.** Size pass on the new code (S 700 to 675). Seed 1 bench mean of middle 3: 30.3 ms (v1: 37.9); A/B against v1: CPU 405 to 313 ms, wall median 49.8 to 42.2 ms (busy machine). Seed 7 PASS, all 22 extra cases pass. Board: 52.8 ms (v1 61.9, -15%), 12.1 MB, S 675, K 24.85, rank 33. M is lower on the board than locally because the box's container wrapper charges less than the local bench's shell. | run-be5f1e4a22bf | 0.0528 | 12.1 | 69.8 |
| 26 | Ablation of the name path, one thread: no name counting 96 ms CPU, keys and hashes only 114 ms, everything 183 ms, so the slot lookups and atomic ORs are about 69 ms (38%) even with prefetching. Tried, none measurable, all reverted: the group bits inside the name's slot so a record touches one 64-byte line (A/B CPU 295 vs 293 ms, and 296 vs 295 with PREFETCHW); a software-pipelined slot prefetch 6 or 12 records ahead instead of a burst (1t min 193 and 177 vs 179 ms); prefetching the next line as well (184 vs 185); probes starting at the line's first slot (199 vs 202); unrolling the separator loop by 6 instead of 8 (178 vs 176). One change did help: half the table load (2^18 slots), 1t 179 to 170 and 12-thread 295 to 285 ms. It is not kept, because it doubles the name table and its bit set (about +7 MB), which costs more in K than it gains. | | | | – |
| 27 | Code hygiene: named constants for every magic number (SWAR masks, BOM, group count, thread and stack limits, workers that prefault), one shared SWAR `classify` for both integer parsers, SAFETY notes that state AVX2 comes from compiling the whole crate for x86-64-v3, a note that the remaining `unwrap`s cannot fire under `panic = "abort"`, and every source file run through rustfmt. S 675 to 690. Seed 1 bench mean of middle 3: 30.4 ms (local), seed 7 PASS, all 22 extra cases pass. **v3.** Board: 52.4 ms, within 1% of v2 (52.8), as the local runs predicted. | run-d3c2badf14df | 0.0524 | | 69.96 (best v3 run, K 25.07) |
| 28 | Failed: a separate clean-room redesign of the name path (each name owned by one worker and its (name, group) pairs sent to the owner as messages, so no shared atomics at all). It was 20 to 25% faster locally, but on the board it ran 52.2 ms against v3's 52.4 ms, with more memory (13.0 MB) and much more code (S 889), so K fell to 23.67. Reverted. Lesson: on this laptop (turbo clocks, a different cache hierarchy, shared with other jobs) removing cross-core traffic looked like a large win, and on the scoring box it was almost nothing; local A/B results on memory-bound changes do not transfer, so the next decisions come from probes measured on the box itself. | run-0caa577fd90b | 0.0522 | 13.0 | 68.71 |
| 29 | Probes c1 (v3 without names) and c2 (the block scan alone) on the box, to find where its time goes: c2 12.7 ms, c1 31.3 ms, v3 52.4 ms. Details below the table and in the probe table. | run-1f26df4f9d6d, run-16449b27e8f7 | 0.0127, 0.0313 | 8.4, 9.0 | none: hashes fail by design |
| 30 | Local-only, not sent to the board. c-next: the four per-record comma checks replaced by one per-chunk check (the chunk's LF separators must number exactly its records, which proves the first four separators of every record were commas), and the amount converted in one SWAR step with its dot shifted out. Correct (22 cases, both seeds, and new malformed-input tests that the reference also rejects, among them an LF in place of a comma that only the new count catches); locally no change (CPU 1t 180 vs 175 ms). c-next2 additionally counted quotes per text field instead of per separator: 7% slower (181 vs 168 ms), dropped. An instruction-level profile of the no-names probe shows no hot spot in field parsing: about 15% of it waits on the input blocks' loads from memory, and the rest is spread over the SWAR arithmetic and the per-record aggregate updates. | | | | – |
| 31 | Board, full correct builds. PREFETCHW on the group-bit word before the atomic OR (c8): 54.7 ms, 12.3 MB, K 24.24, slower than v3's 52.4 ms. Group bits inside the name's 64-byte table slot, one line per record (c9): 59.0 ms, 12.8 MB, K 23.26 (worse). c-next (row 30): 51.4 ms, 12.8 MB, K 24.76, about the same as v3. So neither guess about the names' cost on the box (waiting for line ownership, or touching two lines) was right. | run-23d1c4617d19, run-83bd28c8ff63, run-478f4a1646af | 0.0547, 0.0590, 0.0514 | 12.3, 12.8, 12.8 | 69.23, 68.33, 69.69 |
| 32 | c10: read records.csv with pread into a 128 KiB buffer per worker instead of mapping it (an idea from track A, where it cut the box's time by 15%). Each worker reads its chunk plus a little slack, finds the chunk's cuts itself, and parses from the buffer; the cuts are the same pure function of the file's bytes as before, so the quote-parity proof is unchanged. Correct on the 22 cases plus four new ones with records longer than the slack and longer than a whole chunk. Locally slower (CPU 331 vs 296 ms at 12 threads, from copying the bytes); on the box faster: 49.3 ms against v3's 52.4 ms. But M rose to 14.1 MB (v3 11.9 to 12.6) with twelve buffers, so K fell to 24.67 (v3's best 25.07). | run-2890a0259c6b | 0.0493 | 14.1 | 69.61 |
| 33 | **v4 as submitted** (c11): the pread version with 64 KiB pieces, a 128-byte zero pad and 1 KiB of read slack. Board 49.2 and 49.3 ms, 12.5 and 12.4 MB, K 25.29 and 25.32; a third run 51.4 ms (run-6fbf421cc19e). Racy, as found later (row 35). | run-8fe71efe0f84, run-1e5d2353fd9f | 0.0492, 0.0493 | 12.5, 12.4 | 70.15; 70.17 |
| 34 | Failed: a simplification pass (23 code lines fewer) shipped together with the race fix: 58.9 ms. Reverted. | run-1d81a262b26c | 0.0589 | | – |
| 35 | **v4.1, before the error paths:** v4 plus the one-line race fix. 49.4 ms, 12.3 MB, K 25.29, then 49.1 ms, 12.1 MB, S 734, K 25.47. | run-0f99c23b4b3e, run-0fc0507654be | 0.0494, 0.0491 | 12.3, 12.1 | 70.15, 70.30 |
| 36 | **v4.2 as submitted (5f8b6df):** v4.1 plus error paths only: stderr notes for both fallbacks, the byte offset of the first grammar error, regions.csv errors naming their line's byte offset, a cap on the table retries, one message when several workers fail at once. S 734 to 767. Median 49.7 ms. | run-31b13fc9019b, run-8721232bf1f2 | 0.0489, 0.0505 | 12.6, 12.3 | 70.03, 69.77 |
| 37 | After 5f8b6df, not yet on the board: a failed write of aggregates.txt removes the partial file before exiting. S 768. | | | | |
| 38 | **v5 (6d23622):** clean-room rework with two-phase distinct names: names interned in a read-mostly table, private (id, group) pairs counted after one barrier; two-array name table; separators-only text lengths; window parsing in two passes; start-up tasks. Correct on seeds 1, 7, 101 and 202, 50 fuzz cases, 0 wrong in 300 + 100 stress runs. S 827, D 1. See the section below. | run-41dfe519e529 | 0.0374 | 16.9 | 71.42 |
| 39 | **v5.1.** Round 2 on v5: id, region id and amount right-aligned into one AVX2 register and converted together (unusual numbers on a never-inlined slow path); parser reads skip bounds checks, each call site stating why it is in bounds (128 bytes of chunk padding, debug assertions on in tests); the name key carries its home slot; pair blocks of 128 bytes. Instructions 2.106 G -> 1.759 G (-16.5%), CPU -10% at 12 threads, M -0.3 MB locally. An owner-ring design that removed the pair arenas (-3.6 MB) was correct but 40-60% slower and was dropped. S 827 -> 858. | run-658779ed2eee | 0.0338 | 16.3 | 72.55 |
| 40 | **v5.2 (final).** Round 3 on v5.1: the first pass walks separators five at a time with the record slots and indexes per-block quote arrays mod 64 (compile-time assert), prefetch never bounds-checks, the region map, name home line and pair cursor are read unchecked (SAFETY notes), one OR-and-compare length check for the SIMD number path, the BOM test moved to the slow path, a full home line skips to the next line on a miss. Instructions 1.759 G -> 1.661 G (-5.6%); M unchanged; S 858 -> 882. A debug build with every unchecked read asserted passes the fuzz cases. | run-52c515f71c76 | 0.0324 | 16.7 | 72.82 |

Notes on the longer rows:

- **Row 29.** probe-c1 is v3 without any name counting (no interning, no pair bits, names_distinct printed as 0, everything else identical, which was checked column by column). probe-c2 is only the 64-byte block scan: masks, quote parity and the grammar masks, counting the records outside quotes, with no field parsing, no region map and no prefault. Local bench, mean of middle 3 over two rounds of 5: v3 38.2 and 32.2 ms, probe-c1 23.1 and 17.9 ms, probe-c2 10.1 and 11.7 ms; M about 16.4, 8.8 and 8.2 MB. So locally the names are about 45% of the time and 7.7 MB of the memory, field parsing and joining about 20%, and the scan (with process start) about 35%. Box against local round 2: the scan 12.7 vs 11.7 ms (1.1x), field parsing, regions and join 18.6 vs 6.2 ms (3x), names 21.1 vs 14.3 ms (1.5x). The scan transfers; the per-record work does not, so that is where to look, on the box.
- **Row 33.** Against c10: 64 KiB pieces instead of 128 (twelve buffers, 0.8 MB in all), a 128-byte zero pad instead of 4 KiB (the parser reads at most 64 bytes past a chunk) and 1 KiB of read slack, and each worker filling its caller's 16 KiB of aggregates instead of returning a copy. Not mapping records.csv also saves the page tables of a 200 MB mapping, which roughly offsets the buffers: locally M 16.66 against v3's 16.62 and c10's 18.16 (local MB), at the same speed. Correct on seeds 1 and 7, the 26 cases and the malformed-input tests, each run once or twice; that was not enough to see the race of row 35.
- **Row 34.** The pass merged the two integer parsers, had every worker fault in its share of the name tables (v4 gives that to at most four while the rest split regions.csv), and moved names over 24 bytes to a map under a mutex. Twelve workers faulting in table pages at once is the likely cause of the regression; the box punishes page faults (rows 32 and 33).
- **Row 35.** The race: `Names::fill` wrote all four key words into the slot, and the fourth (the hash) overwrote the slot's `meta` until the Release store of its real value. A lookup on another core that read `meta` in that window saw neither "empty" nor "being filled" but a published slot of the wrong length, probed on, claimed the next slot and gave the name a second id; the name's (name, group) pairs were then counted twice. Found by running the submitted image 300 times on seed 1 (1 wrong output; 2 of 200 for a later build). A debug build that scans the table for duplicate names after each run found them in 11 of 250 runs before the fix and in 0 of 250 after. The fixed build gave 0 wrong outputs in 300 runs of seed 1 and 100 of seed 7, and this folder's current build 0 in 100.

## Probes on the box

Probe builds strip work out of v3 to show where the box's time goes; the board times them even though their output is deliberately wrong. Each is v3 with a few lines changed, marked `// PROBE` (kept outside this folder). Local figures are the bench's mean of the middle 3 of 5 runs, and total CPU time with one worker on one core. The five red runs on the board are probes c2, c4, c3, c1 and c5; the share column is each step's added time on the box, as a share of v3's 52.4 ms (v4.1 takes 49.1 ms, after pread took about 3 ms off the input).

| Probe | What it runs | Run ID | Board result | Box T (ms) | Box M (MB) | Local T (ms) | Local CPU, 1 thread (ms) | Share of v3's 52.4 ms |
|---|---|---|---|---|---|---|---|---|
| c2 | Block scan only: masks, quote parity, grammar masks, record count | run-1f26df4f9d6d | hash fails by design (red) | 12.7 | 8.4 | 11.4 | 24 | 12.7 ms, 24% |
| c4 | c2 plus separator flattening and the terminator check | run-eec975c57826 | hash fails by design (red) | 17.9 | 11.7 | 12.0 | 39 | +5.2 ms, 10% |
| c3 | Everything but names and the region map (fields parsed, aggregates kept) | run-30db742f2dea | hash fails by design (red) | 30.2 | | 17.8 | 89 | +12.3 ms, 23% |
| c1 | Everything but names | run-16449b27e8f7 | hash fails by design (red) | 31.3 | 9.0 | 16.7 | 90 | +1.1 ms, 2% |
| v3 | Everything | run-d3c2badf14df | correct (v3) | 52.4 | | 29.6 | 180 | +21.1 ms (names), 40% |
| c5 | c1 with 6 threads (one per P-core) | run-c09fe1629bb8 | hash fails by design (red) | 33.5 | | 25.4 | | – |
| c6 | v3 with 6 threads (one per P-core) | run-4733eeb44285 | correct, K 21.60, 66.72 points | 71.0 | 11.5 | 44.9 | | – |

Box split: scan 12.7 ms, flattening +5.2, field parsing and aggregates +12.3, region map and its start-up +1.1, names +21.1. Hyperthread pairs help on the box (6 threads: v3 71.0 ms against 52.4, and 33.5 against 31.3 without names), so the pairs sharing a core's 1.25 MiB L2 are not the problem. The scan costs about the same on both machines, while the per-record work costs 2 to 3 times more on the box. The box runs at 2.5 GHz with turbo off, against roughly 4.5 GHz here, so local timings underweight computation. They also underweighted the cost of mapping the 200 MB input: page faults, page-table set-up and tear-down turned out dearer on the box than copying the bytes, which pread later removed (rows 32 and 33). Local single-thread CPU is the better guide to the box's computation, and it agrees on the order: names 50%, field parsing and aggregates 28%, scan 13%, flattening 8%.

Lessons:

- Parsing field by field is latency-bound: each field's position depends on the previous one. Bitmasks first, then fields, roughly halved parse cost per byte. Moving the remaining branches out of the separator loop gained less (about 10%).
- With twelve threads the expensive thing is not computing, it is cache lines that move between cores: atomics on lines other cores read, counters next to read-mostly fields, and first touches of shared pages.
- Lockstep rounds and bounded queues both turn one slow or descheduled worker into a stall for everyone. Unsynchronised sharing with atomics is slower per operation but never waits.
- Under a noisy machine only interleaved A/B runs and CPU time told real changes from noise.
- Most of what remains is memory latency, not instructions: three random lines per record (region map, name slot, group bits) and an atomic OR. Prefetching already hides what it can; only a sparser table measurably helped, at a memory cost.
- The box differs from the laptop in what is expensive, not just in speed: with turbo off, its page faults, page-table set-up and tear-down for a 200 MB mapping cost more than copying the same bytes with pread, a difference invisible locally. Probes that strip work out of a correct build, timed on the box itself, found that; local A/B could not.
- Of the board's 12.1 to 12.6 MB, about 9.7 MB is the program's own tables and buffers: a 4.2 MB name table, a 3.7 MB pair bitset, a 1.0 MB region map and 0.8 MB of read buffers. The rest is code, stacks, page tables and the container runtime's own processes, which are charged to the same cgroup before `main` runs. The pair bitset is near the floor for exact distinct counting, so M moved little.
- The atomic OR on the shared pair bitset looked like the main remaining parallel cost in the profile, but swapping in a racy plain load and store (for timing only) saved just 7% of CPU (A/B 267 vs 248 ms), and dropping the update entirely 249 ms. Owner-private bitsets fed by queues could win at most that, so the shared atomic version stays: it never makes one worker wait for another. (This held for v4.x; v5 removes the shared writes altogether, see below.)

## v5: two-phase distinct names

A clean-room rewrite (commit 6d23622, row 38) aimed at the costs the box's probes measured, not at the laptop's. Same language, thread count, usage and output as v4.2; only the design below changed.

### Why

The probes (above) split v4.2's 48.5 ms on the box into: scan 12.7 ms (with process start), separator flattening 5.2, field parsing and aggregates 12.3, region map 1.1, **distinct names 21.1**. v4 counted distinct (name, country) pairs in one shared bitset with an atomic OR per record. Each name's bits are OR'ed about 33 times in a run, by random cores, so nearly every OR hit a line that another core held modified: a cross-core transfer plus a locked instruction that cannot overlap with anything. At 2.5 GHz with turbo off, 3.32 M of those are about what the 21 ms is. The box had also been shown to punish page faults and random access into multi-MB tables.

### Design

**Distinct names in two phases** (`names.rs`), so that the hot path never writes a line another core reads.

- Phase 1, while parsing: every name is interned in one shared lock-free table, and the slot index is the name's id. The table is read-mostly after the first few percent of the file. The record then appends its (id, group) pair, 16 bits, to a bucket in its own worker's arena: bucket `id >> 8`, entry `group << 8 | id & 255`. These are private stores to a few hot lines. Arenas are chains of 256-byte blocks, lazily backed, and the expected share is faulted in at start-up with one `MADV_POPULATE_WRITE`.
- Phase 2, after one barrier: workers claim buckets. A bucket's pairs from all 12 arenas are counted against an 8 KiB bitset (256 ids x 256 groups) that stays in L1. A new bit is a new distinct pair, and every entry also adds its record and its name's unescaped length to the group. Phase 2 takes about 0.5 ms locally.
- Unlike row 28's owner-partitioned build, no pair is sent to another worker while parsing: the arenas are private and are read only after the barrier, so no worker waits for another during phase 1.

**Name table.** Two arrays.

- Short names (at most 15 raw bytes, 87% of records) use 16-byte slots, four to a 64-byte line: the first name word plus `meta`, where `meta` is the tag (length + 1) with bytes 8 to 14 above it. A short name's home is a whole line, and the fast path compares all four slots without a branch per slot. It hits for 88.6% of short lookups at full scale. Overflow goes to lines `step` (odd) apart, not to the next line, which avoids runs of full lines.
- Longer names use 32-byte slots of three words plus `meta`, probed linearly; bytes past 24 go to an arena.
- Counters showed that linear probing over 32-byte slots, as in v4, left 36% of lookups off their home slot and 28% in another line. That, in branches, was the main cost of the lookups, not memory.
- Insertion uses the protocol v4.1 proved (row 35): claim `meta` 0 to FILLING by CAS, write the name words, publish `meta` with one Release store; nothing else writes `meta`.
- Workers reserve slot capacity 64 at a time. Insert-heavy profiles had shown the shared "used" counter at 4.5% of multi-threaded samples.
- The table is 3 MB, down from 4.2.

**Parsing** (`parse.rs`) keeps the approach and the proof of the chunk cuts (AVX2 masks with a prefix-XOR in-quotes mask, separators flattened 8 at a time, cuts guessed after an LF followed by a digit or BOM and proven by quote parity, single-chunk fallback). What changed:

- A record's text bytes come from its separators and one quote count at its LF: name + note = width - quotes/2 - delimiters. Name bytes come from phase 2, using the unescaped length computed once per distinct name; note bytes are the difference. Quote counts are no longer kept per separator.
- Amounts with up to 5 integer digits convert in one SWAR step, with the dot shifted out.
- Records are handled in windows of 2 KiB. Pass 1 parses fields and prefetches the region-map byte and the name-table line; pass 2 joins, aggregates, interns and appends.
- Every record must be 4 commas + LF outside quotes with valid digits, and the LFs outside quotes must equal the number of records. A quote misplaced inside an unquoted field is not diagnosed.
- Numbers of more than 8 digits saturate, so a huge region id is simply unknown, as the reference treats it. v4.2 rejected that case.

**Start-up** (`run.rs`). The first threads up claim small tasks: six 512 KiB stretches of the name table to populate, and 32 pieces of regions.csv. Workers wait only for those tasks, not for the last thread to start; late threads just join phase 1. `prctl(PR_SET_THP_DISABLE)` runs first. records.csv is still read with pread in 64 KiB pieces, which the box measured faster than mmap (rows 32 and 33).

### Measurements

Local i7-14650HX, Docker, cpuset 0-11; the machine is noisy, so runs were interleaved.

| | v4.2 | v5 | change |
|---|---|---|---|
| task-clock, 12 threads, median of 15 interleaved | 358 ms | 245 ms | -32% |
| user / sys time, same runs | 249 / 40 ms | 169 / 30 ms | |
| page faults | 2,623 | 1,194 | |
| task-clock, 1 thread (taskset), median of 5 | 196 ms | 144 ms | -27% |
| instructions, 1 thread, full seed 1 (cachegrind) | 2.741 G | 2.106 G | -23% (634 per record) |
| bench.sh wall, 2 x 5 interleaved, median | about 40 ms | about 30 ms | machine busy; quiet earlier: 28 vs 21 |
| bench.sh M (local KiB / 1000, includes the shell) | 13.1 to 16.6 | 17.1 to 17.8 | about +3.5 to 4 MB |
| S | 768 | 827 | |
| D | 1 | 1 | |

Ablations, 1 thread, an earlier build: no name lookups at all -27 ms; name lookup and pair append both removed -41 ms (of 137); table prefetch off +6 ms; region prefetch off +9 ms.

Phase timeline, 12 threads: records begin at about 1.5 to 1.8 ms (locally, threads take up to 1 to 3 ms to get scheduled); phase 1 takes about 15 ms with an end-of-phase imbalance of about 50 us; phase 2 about 0.5 ms.

**On the board** (run-41dfe519e529): 37.4 ms, 16.9 MB, K 26.81, 71.42 points, S 827, D 1, against v4.2's 48.5 to 50.5 ms, 12.3 to 12.6 MB, K 25.15 to 25.38. The pre-board estimate in the design notes was 32 to 38 ms at about 16 MB, from removing about 23% of instructions and all the shared atomic ORs; the box landed at the slow end of it. The notes' K estimate was +11% over v4.2, and the board gave +5.6% over 25.38.

**The memory trade.** The pair arenas cost 2 bytes per record (about 6.6 MB), against v4.2's 3.7 MB shared bitset, and the name table shrank from 4.2 to 3 MB. On the board M rose by 4.6 MB (12.3 to 16.9). It is worth it: the time term of K gained far more than the memory term lost, K +5.6%.

### Correctness

- Byte-identical to the reference on seeds 1, 7, 101 and 202, on a case with a digit after an in-quote LF (which forces the single-chunk fallback), and on 50 fuzz cases. The fuzz cases include 7 out-of-range ones that v4.2 failed: huge amounts, leading zeros, huge region ids. They are outside spec section 2.2, so the generator never writes them.
- Malformed inputs (7 cases) are rejected with exit 1 and a byte offset.
- Stress: 0 wrong in 300 runs of seed 1 and in 100 runs of seed 7.

### What did not work, and what was left

The design notes record no failed v5 experiment of their own; the failed tries in this direction are the earlier ones in the log, and they shaped v5:

- Row 28: sending each (name, group) pair to a worker that owned its name, with queues: 52.2 ms on the box at S 889. v5 does not route anything during parsing.
- Row 26: PREFETCHW, a software-pipelined slot prefetch, the group bits inside the name's slot and probes starting at the line's first slot all made no measurable difference; only a sparser table helped, at a memory cost. v5's gain came from removing the shared writes and the off-line probes.
- Lessons (above): swapping a plain store for the atomic OR saved only 7% of CPU locally. The local result did not predict the box, where the names cost 21 ms, mostly the shared-line atomics.

Not done:

- Instructions are about 634 per record and spread thin: pass 1 about 260, block scan about 68, flatten + pass 2 about 160, slow lookups about 50, phase 2 about 20. Bounds checks are about 20 per record. PCLMULQDQ for the prefix XOR would save about 8 per block but needs a target feature beyond x86-64-v3 (row 22 saw no gain from it).
- Memory: 256-byte pair blocks leave about 1 MB of partly filled blocks; 128-byte blocks would halve that at the cost of longer chains in phase 2. The region map could be a rank bitmap (about 0.4 MB instead of 1 MB).
- S could drop further: rustfmt explodes every struct literal over 18 characters.
