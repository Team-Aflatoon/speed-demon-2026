# Track C: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

All T and M after step 0 are **local** measurements (i7-14650HX, Docker Desktop, `--cpuset-cpus=0-11`, the bench script that mirrors the harness), not board runs; Run IDs are filled in from the board. The local machine was shared with other heavy benchmark jobs during this work, so single runs varied by up to 2x; where a change was small we compared it by interleaving runs of the two builds (A/B, medians) and say so. Points are left for the board.

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
| 15 | Huge pages faulted in from one thread up front: racing first touches were each charged 2 MB. Peak spread 15 to 20 MB down to 16.4 to 17.0 MB. Checks for the limits the code relies on (4 GiB file, region ids). Seed 1 bench mean of middle 3: 37.9 ms; seed 7 PASS. | | 0.038 (local) | 17 (local) | |

Lessons:

- Parsing field by field is latency-bound: each field's position depends on the previous one. Bitmasks first, then fields, roughly halved parse cost per byte. Moving the remaining branches out of the separator loop gained less (about 10%).
- With twelve threads the expensive thing is not computing, it is cache lines that move between cores: atomics on lines other cores read, counters next to read-mostly fields, and first touches of shared pages.
- Lockstep rounds and bounded queues both turn one slow or descheduled worker into a stall for everyone. Unsynchronised sharing with atomics is slower per operation but never waits.
- Under a noisy machine only interleaved A/B runs and CPU time told real changes from noise.
