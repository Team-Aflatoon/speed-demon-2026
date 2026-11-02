# Track B: performance log

## Summary

**Final: v13** (commit be2c27b; step 25; S 1,491, D 1, no crates). It is v12 (step 24) plus a behaviour-preserving size cut (S 1,540 to 1,491, cachegrind instructions -1.2%, branch mispredicts -5.4%, output identical on seeds 1 and 7 and the 23 fuzz cases) and comment fixes from the judge review; v12 is v11 with the term table on a 2 MiB huge page. The reference takes 56.35 s and 2,200 MB.

Board runs of the final code: **run-39dd151ec1b7, 86.4 ms, 38.0 MB, K 36.72, 78.24 points** (the final commit f9f4afd, the fastest B run so far) and **run-34a8a8288297, 87.7 ms, 38.2 MB, K 36.43, 78.08 points** (f929929, which changes only Track C). Before it, v13 candidate runs (the same code as the final one, comments and two local names aside) in four interleaved pairs, each followed by a v12 production run:

| Pair | v13 | v12 |
|---|---|---|
| 1 | run-12996e3a24f3: 88.5 ms, 38.3 MB, K 36.28 | run-cbc79a44d139: 88.8 ms, 38.4 MB, K 36.04 |
| 2 | run-3d4480a45f74: 95.7 ms, 38.1 MB, K 34.89 | run-f20b7353a552: 89.8 ms, 38.0 MB, K 35.83 |
| 3 | run-26294481f7ef: 89.7 ms, 38.0 MB, K 36.03 | run-0c7664bbc162: 108.2 ms, 38.1 MB, K 32.64 |
| 4 | run-4136d4104f42: 101.9 ms, 38.2 MB, K 33.81 | run-9e73373e8fd0: 88.2 ms, 38.1 MB, K 36.17 |

The box does not separate v13 from v12. v13 is faster in 2 of the 4 pairs; means 93.95 against 93.75 ms, medians 92.7 against 89.3 ms; v12's own runs span 87.3 to 108.2 ms. We adopted v13 on its instruction count and its size (49 lines of S, worth about 0.1 points by the K formula), not on a measured gain in T. The step before it, v12 (step 24, commit ee4e67a, adopted as 8cb993c), is v11 with the term table on a 2 MiB huge page: 88.9 ms, 37.7 MB, K 36.03, 77.83 points (run-fe126b463988); against v11 production in the same three sessions it was faster in 3 of 3 pairs, 89.2 against 92.5 ms on average (-3.6%).

What changed on 2 November, and why. (1) The huge-page table (v12): every token probes the term table, which on seed 1 is 4 MiB (65,536 buckets at 38% slot load, 1.2% of tokens on the slow path), 1,024 pages of 4 KiB, and those probes missed the TLB; the table is now mapped on a 2 MiB boundary, advised for huge pages and populated by the main thread before it switches transparent huge pages off for the rest of the process. Our earlier huge-page test was a no-op because that same switch, a rule from v2 that keeps the memory peak down, had already turned huge pages off for the whole process. (2) The size cut (v13): S bought 49 lines with ideas from two public entries (step 25), and the box shows no change in T. (3) Dropped, see "Tried on Nov 2 and dropped" below: a fast start-up variant, profile-guided optimisation, and a table twice the size. The best-known profile of the remaining time (lookup 35%, finish 15%, lex 14%) shows no single idea that reaches the leaders' 64-68 ms; the rows below hold every run ID.

**Before it, v11 (step 23, commit 9e6a7d5).** v8 with behaviour-preserving cleanup: S 1,618 to 1,531, cachegrind instructions -0.13% and branch mispredicts +0.8% against v8. Board: 92.3 ms, 38.4 MB, K 35.39, 77.44 points (run-39e71acbc73a) and 91.4 ms, 38.1 MB, K 35.56, 77.55 points (run-98ab40bbff0b); S 1,531, D 1; the reference takes 56.35 s and 2,200 MB. v11 is the end of a day of work on v5.1 (113.6 ms, K 31.43): v6 cut the instructions by 25% (95.2 ms, run-46aaabc13772), v8 added finer bitmaps, 14 MB segments and a two-stage eval prefetch (91.2 and 90.7 ms, run-9678ee16f487 and run-1bab1a0031d4), and v11 took 87 lines off v8. Steps 18 to 23 give each with its board run and local evidence, including v7 (not adopted: a tie with v6), v8w (no gain) and the global-evaluation design we abandoned on memory. The M term is capped at 44 MB (2,200 MB / 50), so M under about 42 MB costs nothing; v8 spent about 3 MB of that headroom (34.8 to 37.7 MB) and took 4% off T.

**Before it, v5.1 (step 17, commit 1f63325).** Board run run-438bfabbceeb: 113.6 ms, 36.3 MB, K 31.43, 74.87 points; S 1,689. It is v5 with 12 MB segments and 64 KB reads (21 segments instead of 31, and each BOOL query pays a fixed cost per segment), a shorter serial setup (queries.txt mapped, the term table sized from a word count and built before the threads start), the anchor flag carried in the stored term id, contiguous phrase records with prefetched anchor heads, evaluation that skips query-segment pairs that provably match nothing, and pwrite output per format task; instructions -10.7% locally.

**Before it, v5 (step 16, commit f3edcde).** Board run run-407d7d7d6411: 125.9 ms, 36.1 MB, K 30.21,
74.01 points (50 log10 K); the reference takes 56.35 s and 2,200 MB. It won because the v4.2
scan spent about 120-190 instructions on every token; a branch-free term table (three keys in a
cache line) and a leaner token loop cut the instructions by 35% and the mispredicts by 53% (see
"v5" below). Output is byte-identical to v4.2's.

Previous final, v4.2 as submitted (step 15): board, median of its two runs, 189.2 ms, 36.1 MB,
K 25.33 (run-3ae92ad95108, run-f30168793c18).

v4.2's final repository state (commit eba990a; since 701dd4f only error paths, comments and docs changed, output identical, S 1299): run-9316371871a2, 186.5 ms, 36.6 MB, K 25.51. Three runs of this code: 186.0, 186.5 and 192.3 ms, median 186.5 ms.

| Step | Board T | M | Run |
|---|---|---|---|
| v1: index only the query terms; a throwaway index per 4,096 documents, every query answered per batch | 309 ms | 59.7 MB | run-7450e3d4343b |
| v2-v3: memory, not time (THP off, bitmap stripes); local CPU gains of 5-10% never showed on the box | ~303 ms | 39.1 MB | run-43f854129993, run-d53081aa4120 |
| v4 redesign: phrases checked during the scan at a rare anchor chosen by query frequency (no positions, no re-reads); postings once per 8 MB segment; one task queue | 184.3 ms | 35.8 MB | run-64a666bd15fb |
| v4.1-v4.2: every capacity provable for any valid input; table size changed nothing (4 MB and 2 MB tables) | 190.6 ms; 193.2 ms | 37.0 MB; 35.3 MB | run-1ca61642aede; run-0778bc8cbf5f |
| v4.2 as submitted: error paths for unseen input, static linking | 189.2 ms (median of 186.0 and 192.3) | 36.1 MB | run-3ae92ad95108, run-f30168793c18 |
| **v5: a cheaper per-token scan** (branch-free cache-line term table, leaner token loop, token-index document ends, cheaper setup); same design, same output | **125.9 ms** | **36.1 MB** | run-407d7d7d6411 |
| **v5.1** (12 MB segments and 64 KB reads, shorter serial setup, anchor flag in the term id, contiguous phrase records; instructions -10.7%) | 113.6 ms | 36.3 MB | run-438bfabbceeb |
| **v6: cheaper inner loops** (one lookup pass per region, chunk-wide grouping of postings, gather-based list filter; instructions -25%) | 95.2 ms; 96.4 ms | 34.8 MB; 34.8 MB | run-46aaabc13772; run-e96e8647d819 |
| v7: early spawn, tapered last segments, eval and format in half ranges (S 1,742 to 1,597); not adopted, a tie with v6 | 98.1 ms | 34.3 MB | run-84244f988ab3 |
| **v8: 14 MB segments, bitmaps for terms in more than 1/64 of a segment, two-stage eval prefetch** | 91.2 ms; 90.7 ms | 37.7 MB; 38.1 MB | run-9678ee16f487; run-1bab1a0031d4 |
| v8w: v8 whose waits only spin on `PAUSE`; no gain | 90.6 ms | 38.3 MB | run-38d533ad9929 |
| **v11: v8 with trimmed code (S 1,618 to 1,531), same output and speed** | **92.3 ms; 91.4 ms** | **38.4 MB; 38.1 MB** | run-39e71acbc73a; run-98ab40bbff0b |
| v12 candidate (huge-page table), three box sessions paired with v11 production | 89.1 ms; 87.3 ms; 91.2 ms (v11 in the same sessions: 91.3; 93.8; 92.4 ms) | 37.9 MB; 38.2 MB; 37.7 MB (v11: 37.5; 37.7; 37.7 MB) | run-a459624accea; run-ee38559b265b; run-59ac1f49db7f (v11: run-01512e5d22b4; run-4448b86a7d90; run-f7e65e70d909) |
| **v12 as adopted (S 1,540)** | **88.9 ms**; 99.0 ms (busy box) | **37.7 MB**; 38.5 MB | run-fe126b463988; run-98bc3fd89798 |
| **v13: v12 with the size cut (S 1,540 to 1,491), same output**, four pairs against v12 production | **88.5; 95.7; 89.7; 101.9 ms (v12 after each: 88.8; 89.8; 108.2; 88.2 ms)** | **38.3; 38.1; 38.0; 38.2 MB (v12: 38.4; 38.0; 38.1; 38.1 MB)** | run-12996e3a24f3; run-3d4480a45f74; run-26294481f7ef; run-4136d4104f42 (v12: run-cbc79a44d139; run-f20b7353a552; run-0c7664bbc162; run-9e73373e8fd0) |
| **v13 as shipped, final commits f9f4afd; f929929** | **86.4 ms; 87.7 ms** | **38.0 MB; 38.2 MB** | run-39dd151ec1b7; run-34a8a8288297 |

Lesson: ten tuning steps moved the board 3%; removing repeated work (84 query passes,
candidate re-reads) moved it 39%. Then, with the design fixed, rewriting the per-token path so
the compiler could keep it in registers cut the board time by about a third again (v5). Every Track B board run is
a correct build; no probes were run. The day after v5.1 repeated the lesson with smaller numbers: two rounds of instruction cuts (v6, v8) took the board from 113.6 to 91 ms, while the S cut of v7, 145 lines, bought nothing on the box because it brought extra segments and waits with it.

## Lessons of the last day

- **Our earlier huge-page test was a no-op.** In v6 we tried `MADV_HUGEPAGE` on the table and saw nothing. Since v2 the process switches transparent huge pages off with `prctl(PR_SET_THP_DISABLE)` so that partly used 2 MB heap regions are not backed, and counted in the memory peak, as whole huge pages. The rule was right for memory, but it also blocked the one table that benefits from huge pages. The fix is an order: build and populate the table on the main thread first, then switch the rest off. The box then showed -3.6%.
- **We have never measured B's phase split on the box.** The research report proposed two red probe runs; they have not been run. Every statement about where B's time goes (setup 4.4-5.9 ms, tail 2.3-6.7 ms, scan the rest) is local and instrumented, and the box's 26 ms gap to the leaders is inferred from instruction counts, which tracked the box in v8 (-3.9% instructions, -4.2% T).
- **The fast start-up experiment was dropped** (`experiment/b-startup`, commit 4999234). An instrumented build showed the first scan still starting at 4.4-4.7 ms: setup is bound by the contended parse and insert (twelve threads on the same table lines and page faults), not by spawning threads. It cost S +36 and conflicted with building the huge-page table on the main thread before any worker exists.
- **Research found no single idea that reaches the leaders' 64-68 ms** (`sd-alt/research/B-REPORT.md`). Cachegrind, seed 1, v11: `Scanner::region` (probe and look up) 35% of instructions, `Doc::finish` 15%, `Scanner::lex` 14%, `Evaluator::run` 11%, `Part::build` 9%, `ChunkOut::partition` 8%. The per-token probe is about 36 instructions with no fat. 704 BOOL queries can match more than 25,000 documents each and their 1,274 terms cover 42% of all tokens, so every byte has to be lexed and every token looked up; the items left are worth 3 to 7% each.
- A size cut is free only if the instruction count stays level or falls: v8 to v11 (-0.13%) and v12 to v13 (-1.2%) did. The four box pairs of v13 against v12 are a tie (means 93.95 and 93.75 ms, 2 of 4 pairs each way, inside v12's own 87.3 to 108.2 ms spread), so v13 was adopted on instructions and S, not on T; v7's 145-line cut bought nothing, because it brought extra segments and waits with it (step 19).

## Tried on Nov 2 and dropped

| Idea | Result | Run IDs |
|---|---|---|
| Term table twice the size (8 MiB, four huge pages; branch b14-table2x) | slower and 4 to 5 MB bigger: 110.7 ms, 43.0 MB, K 32.44 and 96.5 ms, 42.2 MB, K 34.73, against production v12 at 92.2 ms, 38.3 MB, K 35.37 and 89.2 ms, 37.9 MB, K 35.95. The table was already 4 MiB at 38% load with 1.2% of tokens on the slow path; populating 8 MiB serially on the main thread costs more than the fewer slow-path probes save | run-6cbf57b2302b, run-e802a25b48d9 (v12: run-480d788ad0fb, run-14243918fd68) |
| Fast start-up (workers claim set-up ranges, spin-then-park waits, table built while spawning; commit 4999234) | the mechanism it targeted did not move: an instrumented build still starts the first scan at 4.4 to 4.7 ms; laptop wall a tie (58.1 against 59.7 ms median, n = 40, local); S +36; its set-up order conflicts with the huge-page table. Not boxed | none |
| Profile-guided optimisation | local wall -1.2% (71.6 to 70.8 ms, inside the noise), 1-thread 0.0%; S +7 (Dockerfile). Not boxed | none |
| `target-cpu=raptorlake` | not tried on B; A and C showed no gain | none |
| glibc `arena_max=1` | a few KB, not worth a line | none |

## Steps

Rows with a Run ID are board measurements; row 0 is the board's reference figures. Every other
T and M is **local**: `bench/bench.sh b 5` (the harness's sandbox flags, cpuset 0-11, warm page
cache, cgroup `memory.peak`) on an i7-14650HX laptop, with T the mean of the middle three of five
runs. The laptop was shared with other jobs, so its absolute timings drift by ±15% or more from
hour to hour. Every decision under about 10% was therefore made from interleaved A/B runs of the
two builds (median of 7 to 9 runs each, plus the cgroup's CPU time), and those numbers are given
per step. Compare a step's A/B with its parent, not its local T with another row's. Board M is in
MB of 10^6 bytes (the board reports the cgroup's `memory.peak` bytes / 10^6); local memory
figures come from the bench's KiB / 1024 and are in MiB.

Points are 50 × log10(K), which reproduces the board's figures (K 25.45 is 70.29 points, K 25.25
is 70.11, up to the rounding of K). Rows 7 and 10 ("≈ … (computed)"), row 12 (from its board K of 25.26),
row 14 (the median K of its three runs, 25.46) and row 15 (the median of 25.54 and 25.12) were
worked out from K; the other points are read off the board. Row 16 is one board run: its 74.01
points are 50 log10 of its K of 30.21.

| Step | Change | Run ID | T (s) | M (MB; local MiB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (board reference figures) | | 56.35 | 2,200 | 0 |
| 1 | Rust, no crates. Query terms only, in one hash table. Corpus memory-mapped and cut into 3 MB blocks for 12 threads. A 4,096-document index per batch (sparse `u16` lists, plus bitmaps for terms seen in more than 64 documents), every query answered per batch, answers merged under a per-query mutex. Phrases verified by re-tokenizing each candidate document. | | 0.325 (local) | 58 (local) | |
| 2 | Phrases verified around occurrences of the longest term instead of re-tokenizing the whole document (phrase work was about 100 ms of 300 per thread). Per-batch presence bitset, so absent terms resolve from L1. Query steps in one flat array. Token lookups hashed and prefetched one document at a time, then resolved. | | 0.235 (local) | 55 (local) | |
| 3 | Bitmask lexer: 64 bytes per step with AVX2, events visited by trailing-zero count instead of a byte-at-a-time loop that mispredicted at every token boundary. AVX2 first/last-byte search for phrase anchors. One state word per term (dense slot or last document) instead of two arrays. | | 0.200 (local) | 55 (local) | |
| 4 | Dense bitmaps written transposed during the scan (one L1-resident stretch per 64 documents) and transposed back per batch. Term state words prefetched between resolving and recording. Small BOOL results kept as sorted lists. A/B against step 3: 176 vs 185 ms. **This is v1.** | run-7450e3d4343b (rerun run-67ffb0644a3e: 0.3061 s) | 0.3091 | 59.7 | 64.3 |
| 5 | A worker's first batch is a 512-document warm-up. Until a worker has seen a batch, every term is still a list, and twelve full all-list batches at once set the memory peak; the promotion threshold scales with the batch size. Output buffer cut from 1 MB to 64 KB. A/B: 183 vs 198 ms, CPU 1,855 vs 1,925 ms, M 50.9 vs 56.1 MiB. Transparent huge pages were then switched off with `prctl`. On the board, v2 and v3 show box M ≈ local M, so THP was not inflating Track B's memory much (unlike Tracks A and D). | | 0.183 (local) | 51 (local) | |
| 6 | Query parse in two passes: parse every line first, then insert the 155,000 words into a table pre-sized from the word count, prefetching the slot of the word 16 places ahead. The serial parse had been waiting on a cache miss per insert and per rehash. A/B: 179.5 vs 183.9 ms, CPU 1,800 vs 1,845 ms. | | 0.180 (local) | 52 (local) | |
| 7 | BOOL accumulator simplified back to borrowed term or scratch bitmap: OR/NOT apply a list's bits directly, AND spreads it into a mask. It is faster than the step-4 sorted-list accumulator and 50 lines shorter. A/B: 184 vs 192 ms, CPU 1,832 vs 1,888 ms. **This is v2.** | run-43f854129993 | 0.3157 | 52.5 | ≈ 64.7 (computed) |
| 8 | Term table slots shrunk from 24 to 16 bytes (first 12 bytes of the term inline, id and length packed into one word; longer terms are confirmed against their text), so four slots share a cache line and the table is 2 MB instead of 3. A/B against v2: 207.8 vs 215.0 ms, CPU 1,986 vs 2,008 ms, M 0.7 MiB lower: a small win, within noise. (The local T of 0.20 looks worse than step 7's only because it was measured on a busier machine.) | | 0.20 (local) | 51 (local) | |
| 9 | Dense bits written into a one-word-per-term stripe for the current 64 documents and flushed into the bitmaps every 64 documents, instead of into a full transposed copy of every bitmap. It stays as L1-friendly as step 4, drops a ~770 KB buffer per thread and the per-batch transpose. A/B: M 38.7 vs 50.9 MiB, 194 vs 202 ms median, CPU 1,956 vs 1,897 ms (within noise). M is now under the 44 MB cap of the memory axis. | | 0.19 (local) | 40 (local) | |
| 10 | Line count, no behaviour change: `query.rs` folded into `main.rs`, `Answer::merge` reduced to a sort of at most 20 ids. S down 7 lines, to 993 as the board counts it. **This is v3.** | run-d53081aa4120 (reruns run-4f979d5ac144: 0.3021 s, 39.7 MB; run-a5f9c82b0e45: 0.3070 s) | 0.3035 | 39.1 | ≈ 65.9 (computed) |
| 11 | Clean-room redesign, adopted whole (see "v4" below): phrases checked during the scan against each document's token ids, so they need no index; postings built once per 8 MB segment; BOOL queries answered once per segment instead of once per batch; corpus read with `pread`; parallel query parse into a lock-free table. Board: 184.3 vs ~302 ms, K 25.45 vs 20.87; S 993 → 1,364. **This is v4.** | run-64a666bd15fb | 0.1843 | 35.8 | 70.29 |
| 12 | Provable safety for any valid input (see "v4.1" below): segments bisected so none holds more than 65,536 documents; the term table sized from an estimate with a fallback (reverted in step 13); insert prefetch; S 1,347. Within noise of v4; a half-size table (run-0778bc8cbf5f: 0.1932 s, 35.3 MB) too. **This is v4.1.** | run-1ca61642aede | 0.1906 | 37.0 | 70.12 |
| 13 | Clarity: v4.1's estimate, probe limit, fallback table and `calloc` dropped (table size is not a lever, and the table sized for every word can never fill up); start-up moved into `query::Setup`. S 1,317. | run-d4d593bc34cf | 0.1896 | 36.6 | 70.11 |
| 14 | v4.2 before the error-path round: static linking (`+crt-static`), revised comments, and a simplification pass (S 1,316 → 1,269, the insert prefetch removed); output byte-identical on seeds 1, 7, 101 and 202. Three board runs (0.1887, 0.1876, 0.1886 s; median shown). | run-6a7f77612f9d, run-51caec2f4dcc, run-e44ae1bf8ad5 | 0.1886 | 36.2 | 70.29 |
| 15 | Errors for unseen input: every input error names its byte offset, document id or line span; query lines in messages are quoted and cut to 80 bytes; a failed write removes the partial results.txt; optional `[data] [out]` arguments; `--locked`. S rose from 1,269 to 1,299 only for this, so that a holdout the program cannot handle fails loudly and says where; output is unchanged. Two board runs (0.1860, 0.1923 s; median shown). **This is v4.2 as submitted (701dd4f).** | run-3ae92ad95108, run-f30168793c18 | 0.1892 | 36.1 | 70.18 |
| 16 | v5 (see "v5: a cheaper per-token scan" below): term table of three-key buckets probed branch-free; per-token loop in two passes (key and prefetch, then look up and record); document ends found by token index in the lexer; document-local scratch; galloping union and prefetched term entries in eval; cheaper setup. Local: instructions 9,554 M to 6,172 M (-35%), mispredicts -53%, CPU -34%. S 1,299 to 1,562. **This is v5 (f3edcde).** | run-407d7d7d6411 | 0.1259 | 36.1 | 74.01 |
| 17 | **v5.1.** Round 2 on v5: 12 MB segments and 64 KB reads; queries.txt mapped and the term table sized from a word count before the threads start; anchor flag in the stored term id; a per-document stamp instead of the seen bitset; one contiguous record per phrase with anchor heads prefetched; evaluation skips query-segment pairs that provably match nothing; pwrite output per format task. Instructions 6,186M -> 5,525M (-10.7%); S 1,562 -> 1,689. Dropped: a three-stage pipeline (+2.5 MiB, same wall), a token pre-filter (79% of tokens are query terms). | run-438bfabbceeb | 0.1136 | 36.3 | 74.87 |
| 18 | **v6 (b6, commit 798470e).** Same design as v5.1, cheaper inner loops, output byte-identical: (1) every token of a region is looked up in one pass (probe 64 ahead, then find) and each document is recorded in a separate loop, which removes the run splitting at batch and document ends; (2) the dedupe stamps are pre-set to `NEVER` for every id that is not a BOOL term, so "new in this document" is one compare; (3) each document's distinct BOOL terms are appended to a chunk-wide list and grouped by partition in one counting pass at the end of the chunk, with no 64 pushes per document; (4) the build is a counting sort with unchecked indexing and no zero-fill; (5) the lexer classifies bytes with a `vpshufb` nibble table and fills pre-sized arrays through pointers; (6) the key mask is a compare and the hash is one CRC32 of the first 12 bytes, long tokens pointing at a static bucket that sends them to the slow path; (7) `filter` (list against bitmap) handles 8 documents per step with an AVX2 gather and a `PACK` shuffle, and bitmap results are counted and summed with nibble tables, four words per step; (8) vectors pre-sized in the parse. Instructions (cachegrind, 1 thread, seed 1) 5.67 G -> 4.24 G (-25%): region 2.32 G -> 1.43 G, lex 0.70 -> 0.56, build 0.59 -> 0.29, eval 1.04 -> 0.72. Interleaved A/B against v5.1 (local, busy laptop): seed 1, 15 runs, wall median 63.8 -> 57.7 ms (-9.5%), min 61.0 -> 53.2, CPU 709 -> 591 ms (-17%); seed 7, 7 runs, wall median 68.4 -> 62.5 ms, CPU 726 -> 619 ms. S 1,689 -> 1,742; M 34.2-35.8 MiB locally. Gates: the 23 fuzz cases on the normal build and the three stress builds and the 8-value-hash build, seeds 1, 7, 101 and 202, 300 runs of seed 1 as uid 65534 with 0 bad. Tried and dropped: a two-stage eval prefetch (no measurable change), spawning workers before mapping the table, THP for the table with `MADV_HUGEPAGE` (nothing granted). The box tracked the CPU cut (-15% T) better than the local wall cut (-9.5%). A second board run: run-e96e8647d819, 96.4 ms, 34.8 MB, K 33.96, 76.55 points | run-46aaabc13772 | 0.0952 | 34.8 | 76.68 |
| 19 | **v7 (b7), not adopted.** v6 plus: workers spawned first (the first starts at 0.15 ms instead of 1.3 ms, but setup ends no earlier, about 4.6-4.8 ms: parse and insert from 12 threads contend on hot table lines and page faults), parse and insert in one pass, the last segments tapered (each at most half of what is left, at least 1.5 MB: tail after the last scan 2.79 -> 2.16 ms), eval in two half-range tasks with each range formatted as soon as its own evals finish, line-buffered two-digit formatting, and a compaction pass (S 1,742 -> 1,597). Instructions 4.265 G (v6 4.243 G; +38 M from the extra segments' fixed eval cost); setup end 5.05 -> 4.26 ms (instrumented); plain A/B wall median 55.9 against 57.6 ms and CPU 574 against 586 ms, within noise. A deadlock found before any candidate: with two format tasks per range, each locked the range's answers and then waited for the write offset while holding them, so if the upper half locked first it waited for the lower half, which waited for the lock. Natively the lower half nearly always wins the race; `valgrind --tool=none --fair-sched=yes` reproduces it every time. Fixed by putting the answers behind `RwLock`s and dropping the read locks before waiting; the valgrind run (5 runs on 30 MB and the 46 fuzz runs) is now a gate. Dropped on the way: answering BOOL queries in the scan at rare "gate" terms (eval CPU 145 -> 65 ms, but each event cost about 100 cycles in the scan and the scan rose by more: CPU 564 against 576 ms for gates used by at most one query word, 616 for two, 593 for three, about 800 for five); owner-partitioned inserts (page faults in a setup-only run 2.3k -> 5.6k, setup +0.5 ms); workers faulting their own table slices (faults 2.3k -> 3.0k, no gain). Kept, against the instruction count: the phrase-list prefetch staging, whose removal saved 25 M instructions and cost about 30 ms of scan CPU (two dependent cold loads per anchor). On the box: 98.1 ms, a tie with v6 (95.2 and 96.4 ms, K 34.17 and 33.96): the S gain was eaten by T, the smaller final segments adding a fixed eval cost per segment and more waits between phases | run-84244f988ab3 | 0.0981 | 34.3 | 76.63 |
| 20 | **v8 (b8, commit f6bf3b8).** v7 without the taper, plus an analysis of eval's fixed cost: 744 k query-segment pairs, 27% skipped early at about 100 TSC cycles each (8 ms in all), 73% evaluated at about 600 cycles each (about 130 ms), about 250 instructions per pair in `run` itself (skip test, `Set::of` with bounds checks, dispatch), the rest in list kernels on mostly tiny per-segment lists; prefetching list heads helped little (-5%), so it is mostly instructions. What moved it: 16 MB segments instead of 12 (-58 M instructions; the same speed, but a bench M of 39.6-41.9 MiB, too close to the cap, so 14 MB, which gives 36.4-38.4 MiB); `Set::of` and the presence test unchecked (-19 M); bitmaps for terms in more than 1/64 of a segment's documents instead of 1/16 (-107 M instructions, eval time per pair -17%: 130.6 -> 107.9 ms; 1/128 is worse); a two-stage eval prefetch (term entries 8 queries ahead, list and bitmap heads 4 ahead; eval time -4.5%). Instructions 4.079 G (v6 4.243 G, v7 4.265 G); instrumented, eval CPU 163 -> 123 ms and END 58.4 -> 54.6 ms against v7. Plain A/B against v6: seed 1 (15 runs) wall min 58.3 -> 53.8 ms, median 75.4 -> 63.0 (noisy), CPU median 702 -> 609 ms; seed 7 (11 runs) wall min 54.1 -> 54.3, median 59.5 -> 57.3, CPU 606 -> 579 ms. S 1,618. Gates as v6, plus the valgrind run. On the box 91.2 ms against v6's 95.2 and 96.4 ms: -4.2% T for -3.9% instructions. Second board run: run-1bab1a0031d4, 90.7 ms, 38.1 MB, K 35.40, 77.45 points. Dropped: populating the four quarters of the table in parallel (setup end no earlier: fault contention again) | run-9678ee16f487 | 0.0912 | 37.7 | 77.39 |
| 21 | **v8w, no gain.** v8 with the waits only spinning on `PAUSE` (no `yield_now`), on the reasoning that every hardware thread runs one worker, so there is nothing to yield to. Kernel time was 14-18% of samples locally, about 5% of it the yield loop and the futex barriers; PAUSE-only waiting cut it to about 10.5% but raised user CPU and was not faster locally, and the shared laptop could not judge it (eval waits swung from 13 to 830 ms of CPU a run with outside load). On the box 90.6 ms, K 35.43, against v8's 90.4 to 91.2 ms (K 35.26-35.45): no gain, so v8's spin-then-yield stays (it also degrades more gently if the cgroup grants fewer CPUs than `available_parallelism` reports) | run-38d533ad9929 | 0.0906 | 38.3 | 77.47 |
| 22 | **Global evaluation, abandoned on memory (checkpoint, no build).** Idea: index every BOOL posting of the whole corpus and answer each query once, instead of once per segment. Sized from real data (a v8 variant dumped every BOOL posting and query of seeds 1, 7 and 202): 21.03-21.10 M postings, 53.4 k BOOL terms, 31 k BOOL queries, and 418 M postings touched by the queries' terms. A global index with the terms in more than 1/k of the documents as bitmaps and the rest delta-bitpacked in blocks of 16 takes 20.0 MiB for k=8, 21.3 for k=16 and 25.2 for k=32; the base without per-segment eval state is 26 to 28 MiB; lists arrive in pieces per segment, which costs 4-5 MiB more. So 51-53 MiB against a 44 MB cap (floor with ideal coding about 44): the M term would cost -2.5% to -3.3% in K, so T would have to fall 5 to 6.5% to break even, and the T case was weaker than first estimated (global evaluation still decodes 85 M sparse postings and 19 M bitmap words and adds an encode step in the build; it saves only per-pair glue of about 100-150 M instructions). Stopped before coding. Larger segments (16 MB, -12% eval pairs, -58 M instructions) had already given no measurable speed change on their own. Also costed and not built (b9): fusing the token lookup with the per-document dedupe (about 1 instruction a token at best), bitmap-only dense terms from a dense set chosen on segment 0 (about -2%, with a per-posting lookup in the partition pass); built in b9 and not adopted: merging the two eval prefetch functions (-10 M instructions, S 1,617), with no clear win elsewhere in that round | | | | |
| 23 | **v11 (b11, commit 9e6a7d5; final on Nov 1).** v8's algorithm with the code reduced for S, behaviour-preserving: query.rs (the `SLOT` table as one loop, `Table::new` building the struct directly, `stamps` through atomic loads with no `unsafe`, BOOL operators from an `OPS` table, accessors removed), scan.rs (flip and classify lets, the position table as one loop, one-line `set_len` unsafes), index.rs (two debug asserts dropped, `Set::of` offsets by pointer, one-line helpers) and main.rs (`prctl` inlined, `Segment` without a `chunks` field, constants inlined at their single use); one planned edit (the `PACK` table as one loop) reverted because it did not compile. S 1,618 -> 1,531 (Rust 1,524: index.rs 465, main.rs 311, query.rs 394, scan.rs 354; Dockerfile 7). Cachegrind, 1 thread, seed 1: instructions 4,071,713,322 -> 4,066,497,403 (-0.13%), mispredicts 15,949,813 -> 16,075,104 (+0.79%). Gates: the 23 fuzz cases on the normal build and three stress builds; seeds 1, 7, 101 and 202 (normal and 100-document-segment builds); `valgrind --fair-sched=yes`, 5 runs on 30 MB and 46 of 46 fuzz runs; 300 runs of seed 1 as uid 65534, 0 bad; memory.peak 36-37 MiB (5 runs). Board: 92.3 ms against v8's 90.4-91.2 ms (K 35.39 against 35.26-35.45), so it was kept pending a second run, with the rule to go back to v8 if that was also over 92 ms; it ran 91.4 ms, K 35.56, run-98ab40bbff0b, and v11 stayed. A later run of the same code (commit 807cd14) gave 91.9 ms, K 35.45 | run-39e71acbc73a | 0.0923 | 38.4 | 77.44 |
| 24 | **v12 (b-thp, commit ee4e67a; adopted in board commit 8cb993c).** v11 with the term table on a huge page. The buckets are mapped 2 MiB aligned (the mapping is 2 MiB longer than the table and its start rounded up), advised with `MADV_HUGEPAGE` (14) and populated with `MADV_POPULATE_WRITE` (23, Linux 5.14) by the main thread, which now makes the table before any worker is spawned (v11 spawned the workers first and built the table while they parsed) and only then calls `prctl(PR_SET_THP_DISABLE)` (41) for the rest of the process. On seed 1 the table is 65,536 buckets, 4 MiB, two huge pages, 38% of its slots in use; every one of the scan's 36.3 M token lookups touches it, and with 4 KiB pages that is 1,024 pages the probes miss the TLB on. S 1,531 -> 1,540. Candidate box runs, each followed by a v11 production run in the same session (K in brackets): run-a459624accea 89.1 ms, 37.9 MB (K 35.98) against run-01512e5d22b4 91.3 ms, 37.5 MB (35.57); run-ee38559b265b 87.3 ms, 38.2 MB (36.35) against run-4448b86a7d90 93.8 ms, 37.7 MB (35.10); run-59ac1f49db7f 91.2 ms, 37.7 MB (35.56) against run-f7e65e70d909 92.4 ms, 37.7 MB (35.36). Mean 89.2 against 92.5 ms (-3.6%), faster in 3 of 3 pairs (-2.4%, -6.9%, -1.3%); memory unchanged within 0.4 MB. Adopted as production: run-fe126b463988 88.9 ms, 37.7 MB, K 36.03; a later run, run-98bc3fd89798, took 99.0 ms, 38.5 MB, K 34.12, on a busy box. | run-fe126b463988 (candidate runs above) | 0.0889 | 37.7 | 77.83 |
| 25 | **v13 (b-scut, box candidate commit 5e7741f; final as be2c27b, which adds comments and two local names).** v12 with a behaviour-preserving size cut, output identical: `Answer` has one shared `add`, used by `push`, `add_list` and `add_bits` (the filled slots of the ten smallest ids follow from the count), evaluator steps are `(operator, term)` pairs, the empty set and the presence bit array are gone, the two eval prefetch functions are one with a `lists` flag, the counting sort uses the list bounds themselves as cursors (no cursor array) and shifts them back, the digit pairs are one byte string and the Dockerfile drops `WORKDIR`. Three of these ideas we took from the public entries of AVENGERS (github.com/Mukul07777/speed-demon-2026: a shared tally `add` with the filled slots derived from the count; queries as `(op, term)` steps) and Cache Me (github.com/manusingh090/speed-demon-2026-entry: one prefetch function with a lists flag), and wrote in our own code. S 1,531 -> 1,482 without the huge-page change; 1,540 -> 1,491 with it (Rust 1,485: index.rs, main.rs, query.rs, scan.rs; Dockerfile 6). Cachegrind, 1 thread, seed 1: instructions 4,066.5 M -> 4,018.9 M (-1.2%), branch mispredicts 16.08 M -> 15.21 M (-5.4%). Local wall (seed 1, local, shared laptop) 58.2 -> 57.4 ms. Gates: seeds 1 and 7 byte-identical (sha a5a56828... for seed 7); the 23 fuzz cases on the normal build, three stress builds (1 KB pieces, 4 KB chunks, 64 KB segments, 3-token batches; 4 KB pieces; 100-document segments, 1 KB pieces, 1-token batches) and a build whose hash takes 8 values; seed 1 in two of the stress builds; bench memory.peak 38-40 MiB locally. The commit after it (3c9d879) changes comments and two local names only (`half` -> `split` in the format task, `end` -> `lf_end` in the lexer): S stays 1,491 and the outputs on seeds 1 and 7 are identical. Box: run-12996e3a24f3 88.5 ms, 38.3 MB (K 36.28) against the v12 production run after it, run-cbc79a44d139, 88.8 ms, 38.4 MB (36.04); run-3d4480a45f74 95.7 ms, 38.1 MB (34.89) against run-f20b7353a552 (v12) 89.8 ms, 38.0 MB (35.83). Two more pairs followed in the next batch: run-26294481f7ef 89.7 ms, 38.0 MB (K 36.03) against run-0c7664bbc162 (v12) 108.2 ms, 38.1 MB (32.64); run-4136d4104f42 101.9 ms, 38.2 MB (33.81) against run-9e73373e8fd0 (v12) 88.2 ms, 38.1 MB (36.17). Over the four pairs v13 is faster in 2, the means are 93.95 against 93.75 ms and the medians 92.7 against 89.3 ms: a tie inside the box's spread (v12 alone ran 87.3 to 108.2 ms). The case for v13 rests on S (-49 lines) and the instruction count, not on the board time. Final commits (comments and two local names changed since this candidate): f9f4afd run-39dd151ec1b7 86.4 ms, 38.0 MB, K 36.72, 78.24 points; f929929 run-34a8a8288297 87.7 ms, 38.2 MB, K 36.43, 78.08 points. | run-12996e3a24f3; run-3d4480a45f74; run-26294481f7ef; run-4136d4104f42; run-39dd151ec1b7 (final); run-34a8a8288297 (final) | 0.0885; 0.0957; 0.0897; 0.1019; 0.0864; 0.0877 | 38.3; 38.1; 38.0; 38.2; 38.0; 38.2 | 77.98; 77.13; 77.83; 76.45; 78.24; 78.08 |

**What the board says.** Board T stayed at 302-316 ms from v1 to v3: the local gains of
steps 5-10 (a few percent each in A/B) are within the board's run-to-run spread. The board win
was memory: 59.7 MB → 39.1 MB, which puts the memory axis at its cap.

v2 against v1, interleaved on a quiet machine (9 runs each): 181.3 vs 183.2 ms median, CPU 1,815
vs 1,808 ms, M 51.9 vs 56.4 MiB. v3 against v2, interleaved (9 runs each, busy machine): 223 vs
239 ms median, CPU 2,215 vs 2,361 ms, M 38.6 vs 51.4 MiB.

We measured memory before cutting it. The cgroup's `memory.stat` after the run shows about 48 MB
anonymous, about 1.3 MB kernel, 0.8 MB page tables and **0 bytes of file pages**, even after
`results.txt` is written. So O_DIRECT output would not lower the peak, and we did not do it. The
per-worker `Index` buffers, printed from an instrumented copy, came to about 3 MB per thread. The
largest was the transposed scan buffer, which step 9 removed. Once M was under 44 MB the
remaining ideas (a leaner per-query answer instead of 50,000 mutexes, smaller per-thread term
arrays) could no longer raise the score, so we left them out.

## Where the time went in v2

About 95 ms of index building and 50 ms of query answering per worker thread (local); see v4's
ablation below for the final design.

## Experiments that did not help

v1-v3: bigger batches, sampled term classes, a positional anchor index, corpus prefetch, a
bitmap-only accumulator, a hot-term table and two kinds of per-term prefetch. v4 is listed below.

- **8,192-document batches** (half the passes over the 50,000 queries). Query time did not move
  (52 vs 50 ms per thread) and M rose from 55 to 75 MB. Query cost follows the documents, not the
  number of passes. This is also our main evidence against one global index evaluated once per
  query: it would save per-pass overhead, and that overhead is small. It would also need about
  25 MB of compressed postings for the 22 million (term, document) pairs. We did not build it.
- **Global term classes from a parallel sample** (v2 attempt). Twelve threads read 64 KB each,
  estimate document frequencies, and the terms are renumbered so dense terms are ids `0..D`. That
  removes per-thread promotion and the state lookup for dense hits. It was 15% slower: CPU 2,156
  vs 1,838 ms, wall 218 vs 187 ms. The sampling round cost 6-9 ms of wall time and the build got
  slower too. A lower dense threshold (1/32 instead of 1/64) was slower again. Reverted.
- **A positional index for phrase anchors** (v2 attempt). Byte offsets of every occurrence of
  rare phrase terms, with phrases checked from the rarest term's offsets instead of searching
  candidate documents.
  - First version: M jumped to 105 MB, because in a worker's first batch every phrase term (even
    "the") is still rare.
  - Restricted to terms in at most 16 documents of the previous batch: M was still +11 MB (about
    80,000 offsets per batch) and CPU +7%, with the build cost cancelling the phrase saving.
  - Along the way, finding each occurrence's document by a linear walk from document 0 made it
    much slower until we replaced it with a binary search.
  - Reverted.
- **Non-temporal prefetch of the corpus** ahead of the lexer, to keep it from evicting the term
  table out of L3: 243 vs 247 ms (A/B), within noise. Reverted.
- **Bitmap-only accumulator that spreads every list into a mask** (simplest code): 4% more CPU
  than setting OR/NOT bits directly (2,020 vs 1,938 ms). Step 7 keeps the direct path.
- **A separate small table for the hottest terms** in front of the main term table was not
  attempted: the 16-byte slots alone gave only a within-noise gain, which suggests most hot-term
  lookups already hit the cache.
- **Detecting dense terms from an empty list** in `Index::set`: query time went from 52 to 72 ms
  per thread, because dense terms' `bounds` entries are cold. Reverted.
- **Prefetching per-term data for the query 8 places ahead**: no gain (217 vs 208 ms). Reverted.

## Lessons

- Profile first. Re-tokenizing phrase candidates looked cheap on paper (214,000 candidates) and
  turned out to be a third of the run of step 1, our first working version.
- On a shared laptop only interleaved A/B runs, with CPU time next to wall time, are trustworthy
  for anything under about 10%. Single runs swung by up to 2x.
- A few percent locally does not show on the board, whose spread is about as large. Memory, being
  deterministic, moved exactly as measured.
- Ideas that remove work in one place often add it in another: the global classes and the anchor
  offsets were both clean on paper and net losses in measurement.

## v4: clean-room redesign

v1 to v3 plateaued at about 302 ms on the board while the local time kept falling. The design
itself had a ceiling: every 4,096-document batch built a throwaway index and then answered all
50,000 queries against it, about 84 passes over the query set, and phrase candidates were
re-read from the text. v4 was written from scratch in a separate folder and adopted whole once
it beat v3 on the board.

**Board head-to-head:** run-64a666bd15fb, T 184.3 ms, M 35.8 MB, S 1,364, K 25.45, against
v3's ~302 ms, 39.1 MB, S 993, K 20.87. The time gain outweighs the longer code by far: S costs
about 5% of K, T gains about 28%.

What changed, and why:

1. **Phrases are checked during the scan, with no index.** Each phrase gets one anchor: the
   term with the fewest occurrences in `queries.txt` (longest on a tie). Common words appear in
   many queries and rare ones in one or two, so this finds rare words without a pass over the
   corpus. On seed 1 the anchors occur 1.26 M times in `docs.txt`, against 0.92 M for the true
   rarest terms (measured offline) and 52 M for v3's "longest term". The scan keeps every
   token's term id; at the end of a document each phrase anchored there is compared with the
   ids around the anchor. v3's route, candidates from postings and then the text, would be
   214,000 candidate documents re-read, a `pread` each now that the corpus is not mapped.
2. **`pread` into 128 KB per-thread buffers instead of `mmap`.** Track A measured this 15%
   faster on the box (its page-fault path is slow); locally the two were the same.
3. **Postings built once per 8 MB segment, by partition.** Each document's distinct BOOL terms
   go into 64 streams by `id % 64`; one build task per stream counting-sorts it into `u16`
   lists, plus bitmaps for terms in more than 1/16 of the segment. Each build task touches only its
   own partition's streams, counters and lists.
4. **BOOL queries answered once per segment** (32 segments instead of ~84 batches), against
   real lists: AND/NOT against a dense term test its bitmap; two lists are combined by
   spreading one into an L1 mask and testing the other; gallop when one is 16x longer.
5. **One task queue instead of phase barriers**, with chunk streams double buffered by
   segment parity, so the next segment is scanned while one is built and answered. Waiting
   is about 1% of CPU time.
6. **Parallel query parse into a lock-free table** (CAS on a slot's first word) with
   block-dense ids: each thread takes 1,024 at a time from a shared counter.

### Steps (local)

Local figures are from `bench/bench.sh b` or a dev runner with the same cpuset; v3 measured
165-175 ms on the bench in the same session.

| Step | Change | Local T | M |
|---|---|---|---|
| a | First correct build, 16 MB segments | 121-127 ms | 54.6 MiB |
| b | Scanner arrays grown on demand instead of zero-filled to a worst-case size (9.4 MB across 12 threads); dense term ids instead of table-slot ids (the seen bitset shrinks from 32 to 9.5 KB per thread, per-partition metadata from 2 MB to 0.6 MB) | 118-135 ms | 44.3-44.8 MiB |
| c | 8 MB segments: chunk streams and postings halve. Eval cost hardly depends on the segment size (15.7% of CPU at 8 MB, 14.0% at 32 MB) | 125-132 ms | 35.0-36.1 MiB |
| d | Eval: no lookups once an AND or NOT has emptied the result; a dense first term filters the list instead of merging with it | eval CPU 257 to 246 ms | |
| e | Eval: AND/NOT of two lists through an L1 mask (`sift`) instead of a branch-free merge, whose steps depend on each other | 103-112 ms; eval CPU 246 to ~190 ms | |
| f | Scan: local slices instead of fields (no reloads of vector headers after every store, fewer bounds checks), token ids per region instead of a per-document vector, a dummy id for misses so recording a token is branch-free | 101-106 ms; scan CPU ~880 to ~820 ms | 35.4-36.1 MiB |

CPU split of step f (sum over 12 threads, about 1.2 s): scan ~820 ms, eval ~190, build ~75,
waiting ~10; the parallel query parse takes 5.5-6.4 ms of wall time before the scan starts.
Scan ablation (before step f): read + classify + flatten 165 ms, + key/hash/prefetch 380, +
table lookups 766, + per-document recording 870, + writing the postings 960.

### Experiments that did not help (v4)

Prefetch batch size and hint, the table's size and first probe, the union threshold, page faults
(next3) and hot terms as bitmaps; only the last three ran on the board.

- 16, 32 or 128 tokens per prefetch round instead of 64: the same.
- No slot prefetch at all: the same locally (the laptop's out-of-order window hides L3 hits);
  kept for the box, whose L2 is smaller.
- A 2 MB table (load 0.58) instead of 4 MB (load 0.29): the same locally.
- `_MM_HINT_T1` instead of `_MM_HINT_T0`: worse.
- Union-to-bitmap thresholds of 1/32, 1/64 and 1/128 of the documents instead of 1/16: the
  same.
- **Hot terms straight to bitmaps, on the board.** Terms with a bitmap in segment 0 (179 on seed 1) were
  flagged hot: the scan set their bits per 64-document row instead of emitting postings, and the build
  ORed those rows in, cutting postings from ~21.1M to ~13.6M pairs. Correct on every check, but the
  box measured 197.2 ms (run-ff23bf5cc8f4, 38.3 MB, K 24.66) against v4.2's 184-193 ms, and S rose by
  83. The two extra per-token list writes cost about what the skipped postings and sort saved. Dropped.
- **Fewer page faults, on the board** (next3). Reads tokenized in 16 KB runs of whole lines, so the
  per-token arrays shrink from ~340 to ~40 KB per thread; `queries.txt` read without a growth copy;
  output buffers pre-sized. Minor faults 9.5k → 8.2k and board M 36.1 → 31.6 MB, but 190.8 ms
  (run-f833acb4171c, K 25.13), the same as v4.2, and M already scored its cap. Not adopted.
- **Term table size, on the board.** Two v4.1 builds that differ only in the table: room for 1.5
  times the estimated distinct terms (2 MB, load 0.58; run-0778bc8cbf5f, 193.2 ms, 35.3 MB) and
  3 times (4 MB, load 0.29; run-1ca61642aede, 190.6 ms, 37.0 MB). Both are within the board's
  noise of v4 (184.3 ms). The halved table did not help even on the box's 1.25 MB L2, so the
  lookups are not limited by where the table lives; v4.1 keeps the 4 MB table.
- A lookup that settles its first slot with one branch (match or empty) and probes further only
  on a collision: no change in pinned single-thread runs (1,125-1,198 vs 1,080-1,262 ms).
  Reverted.

### v4.1: provable safety

A capacity that generated data cannot exceed is still a failure a valid input could trip, and a
failed run scores zero. v4 had one real hazard: documents are numbered from 0 in each segment as
`u16`, and its segments were plain 8 MB byte ranges, so one would overflow if its documents
averaged under 128 bytes (the generator's average about 1 KB). v4.1 fixed it: it reads the last
line's id to get the document count and bisects any segment holding more than 65,536 documents.

v4.1 also sized the term table from an estimate of the distinct terms, with a fallback to a table
with room for every word. That was a detour: v4's table, sized for every word to be distinct,
can never fill up, the estimate gave the same table on seed 1, and the board showed no
difference, so v4.2 went back to it.

Every other fixed capacity was checked against the input format: a phrase has at most 8 terms
(an anchor position is a byte; longer phrases are rejected), 50,000 queries hold at most 400,000
words (ids stay under 2^22), term offsets in `queries.txt` are 32-bit, and tokens longer than 63
bytes are clamped to a length that no term has. Everything else grows as needed.

### Correctness checks

Besides seeds 1 and 7, 23 cases against the reference: the spec's worked example, every
hand-written case of `tests/test_b_index.py`, generated tiny seeds 1-6 and sample seeds 2-3,
and random cases with long tokens (12, 13, 17, 32 and 33+ bytes), terms sharing their first 12
bytes, a line longer than a read buffer, op words as terms and NOT emptying a set. The same 23
also pass with a stress build whose read buffer, chunk and segment are 1 KB, 4 KB and 64 KB,
which exercises every boundary path. Fuzzing found one bug before submission: two threads
failing on malformed queries could both call `exit` and hang; only the first now reports.
A build that caps segments at 100 documents (so every segment is bisected) passes all 23 cases
too. v4.2 was also checked on the audit seeds 101 and 202 and by a 100-run repeat of seed 1.

### Lessons (v4)

- A design's ceiling is set by its structure, not its tuning. Ten steps of v1 to v3 moved the
  board 3%; changing what is repeated (84 query passes, candidate re-reads) moved it 39%.
- Statistics before code: one offline pass over the corpus (term frequencies, phrase candidate
  counts, anchor costs under several heuristics) chose the phrase design before any of it was
  written.
- Memory was found by accounting, not guessing: printing every buffer's capacity showed zero-
  filled worst-case scratch arrays and slot-indexed per-term arrays as the two big items.

## v5: a cheaper per-token scan

v4.2 sat at 184-193 ms on the board, and its time had tracked local CPU (about local CPU x 1.9 /
12). The design was not the limit any more: the scan ran at about 120-190 instructions per
token. Cachegrind showed most of them in bounds checks, iterator plumbing and register spills,
and a branch mispredict on about half the tokens inside the table probe. v5 keeps the v4
design (queries first into a lock-free term table, `pread` and an AVX2 lexer, postings per 8 MB
segment in 64 partitions, BOOL answered per segment, phrases checked during the scan at a rare
anchor, one task queue) and rewrites the per-token path. The output is byte-identical to v4.2's.

**Board:** run-407d7d7d6411, T 125.9 ms, M 36.1 MB, K 30.21, 74.01 points, against v4.2's
186.0-192.3 ms, 36.1-36.6 MB, K 25.1-25.5.

**What S cost against what T gained.** S rose from 1,299 to 1,562 (+263 lines, about -2.7% K).
T fell from 186-192 ms to 125.9 ms, which took K up by 18% (against 25.5, the best v4.2 run).
The longer code paid for itself several times over.

### What changed, and why

In order of effect:

1. **Term table: buckets of three 16-byte keys in one cache line, probed branch-free**
   (`query.rs`: `Bucket`, `Lookup::find_short`). A token of up to 16 bytes is its own key
   (bytes or-ed with 0x20, zero padded), so no two tokens share one. A longer token keeps 15
   bytes and puts `0x80 | length` in byte 15, which no short key has; two long tokens with
   the same key are told apart by comparing the rest of their bytes (the slow path). A bucket
   holds three keys and four values in 64 bytes. The probe is two AVX2 compares, a 64-entry
   table (`SLOT`) from the match bits to the slot, and one load. The fourth value is the miss
   value; it carries `OVERFLOW` when some term had to move on to a later bucket, and only a
   miss in such a bucket takes the slow path. The hash is CRC-32C (two instructions). This
   step alone took local CPU from 1,096 to 939 ms.
2. **The per-token loop rewritten for the compiler** (`scan.rs`: `Scanner::region`,
   `Doc::record`). Pass A builds each key in an XMM register (load 16 bytes, or 0x20, and
   `MASKS[len]`) and makes a `Probe`: the key plus a pointer to its home bucket, null for a
   long token. It prefetches the bucket, 64 tokens at a time. Pass B looks the tokens up and
   records them in runs that end at a batch or at a document boundary, so there is no
   boundary test per token. Its counters live in locals, and its stores are unchecked behind
   one capacity assert per run. The table's buckets and mask are copied into a `Lookup` value,
   so the loop keeps them in registers instead of reloading them after every store.
3. **The lexer finds document ends by token index.** At each LF it counts the tokens before
   it, from the start mask. It flattens starts and ends into one alternating list (one loop
   instead of two, 24 positions written unconditionally), and it skips the prefix XOR on the
   94% of blocks with no LF or TAB.
4. **Document-local `ids`, `fresh` and `hits`**, grown to the longest document instead of
   region-sized: -4 MB, which brought memory back under v4.2's.
5. **Eval:** a galloping union when one list is 16 times longer than the other (most merged
   elements were in such unions). `filter` and `set_bits` index unchecked after one check of
   the sorted list's last element. Each query prefetches the term entries of the query four
   ahead (the `offsets` load was eval's hottest instruction).
6. **Setup** (serial before the scan, about 6.5 ms down to about 4.7 ms locally): the table
   and its per-id arrays come from `mmap(MAP_POPULATE)`, because the allocator zeroes
   64-byte-aligned memory with one thread; a term's query-use count lives in its value word,
   so there is no separate per-id frequency array whose lines all twelve threads incremented;
   segments are planned on the main thread while the workers parse; `queries.txt` is read into
   a padded buffer without the regrowth copy that appending the padding forced.
7. Smaller items: the build loops run over each stream directly (no `flatten()` chain);
   emission goes into a fixed-length stream slice; waits spin on `PAUSE` (which leaves the
   core to the other hardware thread) before yielding; output buffers are pre-sized; worker
   scratch is not freed at exit (each unmap would flush every core's TLB).

### Measurements (local)

Interleaved A/B in one container (cpuset 0-11, the box's 6 cores x 2 threads), `perf stat`
task-clock; v4.2 against v5:

| Seed | Build | Wall median (min) | CPU median (min) |
|---|---|---|---|
| 1 (15 runs each) | v4.2 | 108.4 ms (106.1) | 1,093 ms (1,062) |
| 1 | v5 | 79.3 ms (75.5) | 722 ms (698) |
| 7 (7 runs each) | v4.2 | 109.0 ms (107.2) | 1,100 ms (1,086) |
| 7 | v5 | 78.8 ms (75.2) | 742 ms (697) |

That is 33-34% less CPU and 27-29% less wall time. Local memory is 35.5-36.3 MiB (37.2-38.0
MB by the bench's `memory.peak`), under the 44 MB cap of the memory axis. The board then
measured 125.9 ms, at the low end of the 125-140 ms that v4.2's time-to-CPU relation had
predicted before the run.

Instruction counts (cachegrind; mispredicts from its branch simulator):

| Input | v4.2 | v5 |
|---|---|---|
| Seed 1, full | 9,554 M instructions, 39.6 M mispredicts | 6,172 M (-35%), 18.6 M (-53%) |
| First 32 MB of seed 1, all queries | 1,311 M, 5.6 M | 878 M, 3.3 M |

CPU split of v5 (perf, local): lookup and record about 25-29%, eval about 15%, finish
(emission and phrase checks) about 10%, lex about 9-13%, build about 6%, the kernel copy for
`pread` about 3-4%, waiting 1-6%.

### Measured and dropped (v5)

- **Driver-based BOOL eval.** When the result must lie inside a small set (the smallest term
  ANDed after the last OR), test only its documents against every term. Correct, but eval's
  share fell by only about 5% (about 1% of the run) for 60 more lines. Removed.
- **`mmap` of `docs.txt` with `MADV_POPULATE_READ` per chunk** (no copy, no page faults).
  Correct, but the same CPU locally (720 vs 725 ms) and slightly worse wall time: the
  page-table work costs what the copy saved. v4.2 kept `pread` because mmap measured 15%
  slower on the box in Track A, and this gave no reason to change that. Not adopted.
- Dense-bitmap threshold of 1/32 and 1/64 of the segment instead of 1/16: within noise.
- 256 KB chunks instead of 512 KB, and splitting a segment's scans around the previous
  segment's build and answers: they moved the waiting between phases but not the wall time.
- Prefetching buckets ahead in the query inserts, and a use-count cap of 31 instead of 63
  (same anchors on seed 1): insert time unchanged (about 1.4 ms, bound by cross-core misses).
- Starting threads as a tree (4 started by the main thread, each starting 2): no measurable
  change. Removed to keep S down.

### Correctness

- Seeds 1, 7, 101 and 202: byte-identical to the reference.
- The 23 fuzz cases: the spec's example, the hand-written cases of `tests/test_b_index.py`,
  long tokens of 12/13/17/32/33+ bytes, terms sharing prefixes, a line longer than a read
  piece, op words as terms, NOT emptying a set, tiny and sample seeds.
- The same 23 cases, plus seed 1, in three stress builds that take every boundary path:
  1 KB pieces, 4 KB chunks, 64 KB segments and 3-token batches; 4 KB pieces and at most 100
  documents per segment; 1 KB pieces and 1-token batches.
- A build whose hash takes only 8 values, so that every bucket overflows into long chains,
  passes the 23 cases.
- 300 runs of seed 1 in one container as uid 65534: 0 differing outputs.

### Limits (as v4.2, plus)

- Query terms are 1-32 bytes (the grammar); corpus tokens of any length are handled (17+
  bytes by the slow path).
- A term's use count has seven bits. `USES_CAP + MAX_THREADS <= 127` is asserted at compile
  time.
- The table has a slot for every query word, so it never fills; a probe for a missing token
  in a full chain is bounded by the bucket count.
- At most 2^22 term ids, 2^16 documents per segment, 12 threads (as before).

### Risks on the box

- The local A/B is on a shared laptop with cpuset 0-11; only the whole build was measured on
  the board (one run, 125.9 ms). The `mmap(MAP_POPULATE)` setup, the `PAUSE` spins and the
  bucket prefetch were each chosen locally and not isolated on the box, whose page-fault
  path and L2 differ.
- The probe needs AVX2, BMI2 and SSE4.2 (x86-64-v3, as before); the build refuses to compile
  without AVX2.
- The `mmap` of the corpus stays rejected on the strength of Track A's box measurement, not
  a Track B one.
