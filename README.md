# Team Aflatoon · Speed Demon 2026

Our entry for [Speed Demon 2026](https://speeddemon.dev), the 72-hour performance championship by Hackathon Raptors: take a deliberately naive Python reference, keep its output byte for byte, and make it as fast and as lean as we can on the organisers' scoring machine.

We entered all four tracks. Each one is a standalone Rust program with **no dependencies** beyond the standard library, built offline from its own folder. Our presentation is in [`docs/PRESENTATION.md`](docs/PRESENTATION.md).

| Track | Task | Folder | Approach in one line |
|---|---|---|---|
| A · Ingest | 3.1M log lines → per-hour, per-endpoint counts, p50/p95/p99 and 5xx rate | [`track-a/`](track-a/) | Eleven workers read 128 KB pieces with `pread`, turn each line into a 4-byte record in its hour's log, and report each hour while parsing; exact counts prove the early reports were safe, otherwise the job reruns |
| B · Index | 50,000 boolean and phrase queries over a 250 MB corpus | [`track-b/`](track-b/) | Index only the terms that appear in `queries.txt`; phrases are checked during the scan at a rare anchor term (no positions stored), postings are built per 8 MB segment, and one task queue keeps all 12 threads busy |
| C · Join | Hostile CSV (quotes, embedded newlines, CRLF, BOMs) joined to a lookup table | [`track-c/`](track-c/) | AVX2 bitmask CSV parser (prefix-XOR quote mask) over 64 KiB `pread` pieces, chunk cuts guessed and then verified by quote parity, and one lock-free name table with a shared bitset for the distinct counts |
| D · Traverse | 2,000 shortest-path queries on a 5M-edge weighted digraph | [`track-d/`](track-d/) | Bucketed parallel CSR build without atomics and 4-byte edges, BFS renumbering for cache locality, then bidirectional Dijkstra with a radix heap on 12 threads |

Every track folder holds what the rules ask for: `Dockerfile`, `.speed-demon.toml`, `README.md` (approach, dependencies, next 24 hours), `PERF.md` (the optimisation log, with live-board run IDs) and the source.

## Results on the live board

Live board, seed 1, single run; final scoring uses a private holdout seed and five runs. The board runs on the organisers' scoring machine, so these are real scoring-box numbers. Each row is the submitted build, verified correct.

| Track | Best board run | T | M | S | D | K | Points |
|---|---|---|---|---|---|---|---|
| A · Ingest | run-97875b283660 | 20.2 ms | 17.9 MB | 530 | 1 | 47.33 | 83.76 |
| B · Index | run-d4d593bc34cf | 189.6 ms | 36.6 MB | 1317 | 1 | 25.25 | 70.11 |
| C · Join | run-1e5d2353fd9f | 49.3 ms | 12.4 MB | 729 | 1 | 25.32 | 70.17 |
| D · Traverse | run-1d538165ea54 | 140.4 ms | 70.4 MB | 551 | 1 | 25.80 | 70.58 |

Against the organisers' reference programs:

| Track | Reference time | Our time | Speedup |
|---|---|---|---|
| A | 32.05 s | 20.2 ms | about 1,590x |
| B | 56.35 s | 189.6 ms | about 297x |
| C | 14.44 s | 49.3 ms | about 293x |
| D | 72.69 s | 140.4 ms | about 518x |

Later runs of the current code, for the record: A run-722c4fd2fd96 (20.2 ms, 18.3 MB, K 47.03; the current source is S 537 after a review fix that changes no behaviour), B run-e86006cfe15d (192.4 ms), C run-a35a3da640f6 (49.1 ms, K 25.29), D run-bc431b72ba59 (142.1 ms). Board runs vary by a few percent from run to run. Each track's `PERF.md` lists every step with its run ID.

## How we worked

- **Rust with zero dependencies.** Standard library plus a few hand-declared libc calls, so D = 1 and the code stays short.
- **Every change measured on the scoring box itself.** Our laptop and the box disagree: several changes that were 20 to 25% faster locally were no faster there (turbo off at 2.5 GHz, smaller caches, and memory-bound work suffers most). Local timings only chose candidates.
- **Box ablation probes.** The board records time and memory even when the output is wrong, so we pushed deliberately incomplete builds (read the file only, parse without the name table, build the graph without the queries). They fail the hash on purpose and are the red runs in our board history. Subtracting their times gave the cost of each phase on the real machine, and each one is documented in its track's `PERF.md`.
- **Correct head-to-head candidates after that.** Once a question was answered we stopped probing and compared complete, correct builds back to back (for example `pread` against `mmap` in Track A: 24.5 ms down to 20.7 ms).
- **Correctness on many unseen seeds.** Full-size seeds beyond the public one, tiny and sample seeds, hand-written edge cases from the spec, and adversarial files (shuffled, reversed, late lines, huge records). Every fallback path (A's rerun, B's segment splitting, C's single-chunk reparse and table regrowth, D's 64-bit retry) was forced on purpose and produced the right bytes.
- **A holdout-safety audit.** We listed every fixed limit in all four programs and checked it against what the spec guarantees for any seed; the two Track B limits that were only safe in practice were made safe for any valid input.
- **Judge-style reviews.** We reviewed each track against the official criteria (code quality, technique, optimisation log), fixed what came up, and re-verified before every push.

## How it is scored

K = (T_ref/T)^0.50 × min(M_ref/M, 50)^0.20 × min(S_ref/S, 1)^0.15 × (D_ref/D)^0.15. Wall time dominates and is uncapped. Memory pays up to 50× less than the reference. Source lines and dependencies can only cost. That shaped every decision here: zero crates (D = 1), no needless copies of the input (Tracks A, B and C read it in cache-sized pieces with `pread`; Track D memory-maps it), and short code.

## Reproducing our numbers

`check.py` is the organisers' checker from the starter template. `bench/bench.sh` is our stricter local stand-in for the scoring harness: the same sandbox flags, cores 0-11, a warmed page cache, and the run's cgroup `memory.peak`, over five runs with a sha256 check.

```sh
# datasets come from the public generators (seed 1 is the board's dataset)
git clone https://github.com/hackathon-raptors/speed-demon-2026 ../sd-official
python3 ../sd-official/generators/a_ingest.py --seed 1 --scale full --out data/a
# load data/<track> into the docker volume `sd-data`, then:
bash bench/bench.sh a 5
python3 bench/static_axes.py track-a   # S and D exactly as the harness counts them
```

## Team

- Dhruv Sharma ([@Spiritsfuse](https://github.com/Spiritsfuse))
- Anushika Chauhan ([@Anushika06](https://github.com/Anushika06))
- Pratyush Mishra ([@PratyushMishra-2nd](https://github.com/PratyushMishra-2nd))

## Licence

MIT, see [LICENSE](LICENSE). The organisers' reference code that the starter shipped is copyright Hackathon Raptors CIC under the same licence.
