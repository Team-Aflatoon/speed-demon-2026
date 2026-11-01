# Team Aflatoon · Speed Demon 2026

Our entry for [Speed Demon 2026](https://speeddemon.dev), the 72-hour performance championship by Hackathon Raptors: take a deliberately naive Python reference, keep its output byte for byte, and make it as fast and as lean as we can on the organisers' scoring machine.

We entered all four tracks. Each one is a standalone Rust program with **no dependencies** beyond the standard library, built offline from its own folder. Our presentation is in [`docs/PRESENTATION.md`](docs/PRESENTATION.md).

## Results on the live board

Seed 1, single runs on the organisers' scoring machine; final scoring uses a private holdout seed and five runs. Every row below is a board run that passed the output check on seed 1. Run IDs are listed in each track's `PERF.md`.

**The shipped code** (the commit each track folder holds now; board runs of the final repository state, commit eba990a, plus the earlier run of the same code where there is one):

| Track | Code commit | Final board run | T | M | S | D | K | Points | Earlier run of this code | Reference | Speedup |
|---|---|---|---|---|---|---|---|---|---|---|---|
| A · Ingest | d0fa7bb | run-8fc3ec9b48d1 | 21.1 ms | 18.3 MB | 532 | 1 | 46.02 | 83.15 | run-996cb0022b51 20.4 ms, run-361c33ed2406 20.9 ms | 32.05 s | about 1,550x |
| B · Index | 129b96e | run-9316371871a2 | 186.5 ms | 36.6 MB | 1299 | 1 | 25.51 | 70.33 | run-3ae92ad95108 186.0 ms, run-f30168793c18 192.3 ms | 56.35 s | about 302x |
| C · Join | fbf7e89 | run-d79219fb688a | 48.5 ms | 12.3 MB | 768 | 1 | 25.38 | 70.22 | run-31b13fc9019b 48.9 ms, run-8721232bf1f2 50.5 ms (5f8b6df, S 767) | 14.44 s | about 298x |
| D · Traverse | a74853d | run-60e7bb6f9004 | 141.8 ms | 70.5 MB | 618 | 1 | 25.22 | 70.08 | run-79d73088c3c3 143.1 ms, run-053708dfffa2 148.6 ms (13b0490, S 600) | 72.69 s | about 513x |

Points are 50 × log10(K), worked out from the board's K.

**The same algorithms before the last hardening and error-message changes**, for the record. These were measured on earlier commits with fewer lines:

| Track | Build | Board runs | T | M | K |
|---|---|---|---|---|---|
| A | 1cd61cf, S 514 | run-2491c9d07251, run-237f722dfeb2 | 20.0 and 20.1 ms (median 20.05) | 18.0 MB | 47.72, 47.67 |
| B | same output code, S 1269 | run-6a7f77612f9d, run-51caec2f4dcc, run-e44ae1bf8ad5 | 188.7, 187.6 and 188.6 ms (median 188.6) | 36.2 MB | 25.46 (median) |
| C | v4.1, S 734 | run-0fc0507654be | 49.1 ms | 12.1 MB | 25.47 |
| D | v4, S 551 | median of five runs; best run-1d538165ea54 | median 142.1 ms (140.4 to 147.9); best 140.4 ms | 70.4 MB | best 25.80 |

The shipped builds have a slightly lower K on A, C and D (B's is within noise): the added code (input checks, error messages naming the offending path or byte, notes on the fallbacks) costs lines, and the T differences between the two tables are within the board noise described below. We ship them because an input we have never seen must fail loudly and say why. Track C's shipped build also carries the race fix described under Correctness.

### Red runs in our board history

A red run is a deliberate wrong-output probe (or, on Track A, two probes that aborted at start because they read command-line arguments), never a failure of a submitted build.

| Track | Red runs | What they were |
|---|---|---|
| A | run-8ea2db5074de, run-d570a81674c0; aborted at start: run-f70b36471354, run-f71c8ec82249 | read-floor and ingest-only probes; the two aborted ones were read-floor probes that took their mode from argv |
| B | none | every Track B board run is a correct build |
| C | run-1f26df4f9d6d, run-eec975c57826, run-30db742f2dea, run-16449b27e8f7, run-c09fe1629bb8 | probes c2, c4, c3, c1, c5: scan only, plus flattening, plus field parsing, everything but names, and the same on 6 threads |
| D | run-94d4d2fef344, run-e96c9c86ab33, run-148c3b623aad, run-9efbc7bcc92f, run-8db29a146c98 | prefix probes d0, d3, d4, d2, d1: start-up, one parse, second parse and scatter, first CSR, whole build with no queries |

### Reading the numbers: the box, the laptop and board noise

- **The box is slower than our laptop, by a different factor for each kind of work.** About 1.45x on Track D, about 1.6x on Track C, and 1.9x on Track A's v7 (24.5 ms on the box against a 13.1 ms local median). Scan-bound work transfers (Track C's block scan: 1.1x); per-record work costs 2 to 3 times more on the box (2.5 GHz, turbo off, smaller caches). Sources: [A](track-a/PERF.md), [C](track-c/PERF.md), [D](track-d/PERF.md).
- **Board noise.** Repeated runs of one build differed by at most 3.5% on Track A (excluding one outlier set aside in its PERF), and by 5% on Track D's final code (140.4 to 147.9 ms over five runs). Early, loaded runs spread more: up to 17% on Track D's v1 (156.0, 166.5 and 181.9 ms). Differences of a few percent between two builds are therefore not evidence either way; we decided on reruns and on head-to-head pairs.
- **Which numbers are which.** Board figures carry a run ID. Anything labelled "local" in the READMEs and PERF files was measured on our laptop and only chose candidates.

## What each track does

| Track | Task | Folder | Approach in one line |
|---|---|---|---|
| A · Ingest | 3.1M log lines → per-hour, per-endpoint counts, p50/p95/p99 and 5xx rate | [`track-a/`](track-a/) | Eleven workers read 128 KB pieces with `pread`, turn each line into a 4-byte record in its hour's log, and report each hour while parsing; exact counts prove the early reports were safe, otherwise the job reruns |
| B · Index | 50,000 boolean and phrase queries over a 250 MB corpus | [`track-b/`](track-b/) | Index only the terms that appear in `queries.txt`; phrases are checked during the scan at a rare anchor term (no positions stored), postings are built per 8 MB segment, and one task queue keeps all 12 threads busy |
| C · Join | Hostile CSV (quotes, embedded newlines, CRLF, BOMs) joined to a lookup table | [`track-c/`](track-c/) | AVX2 bitmask CSV parser (prefix-XOR quote mask) over 64 KiB `pread` pieces, chunk cuts guessed and then verified by quote parity, and one lock-free name table with a shared bitset for the distinct counts |
| D · Traverse | 2,000 shortest-path queries on a 5M-edge weighted digraph | [`track-d/`](track-d/) | Bucketed parallel CSR build without atomics and 4-byte edges, BFS renumbering for cache locality, then bidirectional Dijkstra with a radix heap on 12 threads |

Every track folder holds what the rules ask for: `Dockerfile`, `.speed-demon.toml`, `README.md` (approach, dependencies, next 24 hours), `PERF.md` (the optimisation log, with live-board run IDs) and the source.

## Correctness

Speed that is wrong scores zero, and the final data is a seed we have never seen.

- **Unseen seeds.** Full-size seeds beyond the public one, tiny and sample seeds, and the spec's own worked examples.
- **Adversarial, spec-valid inputs.** Hand-written edge cases and hostile files: shuffled, reversed and late-line logs, huge records, records longer than a read buffer, malformed lines the reference also rejects.
- **Every fallback forced.** A's rerun, B's segment splitting, C's single-chunk reparse and table regrowth, D's 64-bit retry were each triggered on purpose and produced the right bytes.
- **A holdout-safety audit.** We listed every fixed limit in all four programs and checked it against what the spec guarantees for any seed; Track B had one limit that was only safe in practice (at most 65,536 documents per segment), and it now holds for any valid input.
- **A concurrency audit.** We wrote down the publication protocol of every shared structure in Tracks A, B and D (A's endpoint table, hour logs and speculative count check; B's lock-free term table and task queue; D's thread team, BFS claim ids and per-thread searches) and argued each one correct. Then we stress-tested them: 5,200 runs of the real images on seeds 1, 7, 101 and 202, plus 3,000 runs of copies with random sleeps and yields injected inside the suspected race windows (2,200 on A, 450 on B, 350 on D). 0 wrong outputs, crashes or hangs. Track C's protocol is covered by the race fix below and 450 stress runs of the fixed build.
- **Fail loudly.** We accepted a few dozen extra lines per track (S rose by 18 to 49) so that an input the generator never writes fails with a clear message and exit 1 instead of a panic or a wrong answer; this is the one place we spent K on purpose.
- **The one real bug our own stress testing caught.** Track C's v4 had a 1-in-100 publication race in its lock-free name table: a slot's hash word briefly overwrote its `meta` word, so another core could take the slot for a published one of a different length and insert the same name twice, which changed one distinct count by one. A 300-run stress test of the submitted image found it, and the fix was one line (write only the three name words). It matters because final scoring needs all five runs correct: at 1 in 100 that is about a 5% chance of a zero. The board never showed it, since each board run is a single run. Details in [`track-c/PERF.md`](track-c/PERF.md).
- **Judge-style reviews.** We reviewed each track against the official criteria (code quality, technique, optimisation log), fixed what came up, and re-verified before every push.

## How we worked

- **Rust with zero dependencies.** Standard library plus a few hand-declared libc calls, so D = 1 and the code stays short.
- **Every speed decision settled on the scoring box.** Our laptop and the box disagree (see above): several changes that were 20 to 25% faster locally were no faster there. Local timings only chose candidates; board runs decided.
- **Box ablation probes.** The board records time and memory even when the output is wrong, so we pushed deliberately incomplete builds (read the file only, parse without the name table, build the graph without the queries). They fail the hash on purpose and are the red runs above. Subtracting their times gave the cost of each phase on the real machine.
- **Correct head-to-head candidates after that.** Once a question was answered we stopped probing and compared complete, correct builds back to back (for example `pread` against `mmap` in Track A: 24.5 ms down to 20.7 ms).
- **We rejected faster builds when they cost more than they earned.** The formula penalises code size and memory, so a build that gained 2 to 3% in T but added 100 lines or a few MB lost K. Track D's one-parse build (140.3 ms, S 551 to 662) and its page-fault-reduction builds (about 2 to 3% faster, +2.5 MB and S 589 to 609) were dropped for that reason, as were Track C's owner-partitioned names (20 to 25% faster locally, no faster on the box, S 889) and its half-load name table (faster, but about 7 MB more).

## How it is scored

K = (T_ref/T)^0.50 × min(M_ref/M, 50)^0.20 × min(S_ref/S, 1)^0.15 × (D_ref/D)^0.15. Wall time dominates and is uncapped. Memory pays up to 50× less than the reference. Source lines and dependencies can only cost. That shaped every decision here: zero crates (D = 1), no needless copies of the input (Tracks A, B and C read it in cache-sized pieces with `pread`; Track D memory-maps it), and short code.

## How to verify a change

`check.py` is the organisers' checker from the starter template. `bench/bench.sh` is our stricter local stand-in for the scoring harness: the same sandbox flags, cores 0-11, a warmed page cache, and the run's cgroup `memory.peak`, with a sha256 check of the output. `bench.sh` holds the seed-1 hashes; pass `EXPECT_SHA` for any other seed.

```sh
# 1. Generate the data with the public generators (seed 1 is the board's dataset)
git clone https://github.com/hackathon-raptors/speed-demon-2026 ../sd-official
python3 ../sd-official/generators/a_ingest.py --seed 1 --scale full --out data/a   # likewise b, c, d

# 2. Load it into the docker volume `sd-data`: subdirs a..d for seed 1, s7-a..s7-d for seed 7
docker run --rm -v "$PWD/data:/src:ro" -v sd-data:/data \
  debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad \
  sh -c 'mkdir -p /data/a && cp /src/a/* /data/a/'      # repeat with b, c, d; for seed 7 copy a seed-7 tree to /data/s7-a ...

# 3. Correctness against the organisers' checker
python3 check.py a --data data/a

# 4. Timing, memory.peak and output sha, five runs, seed 1
bash bench/bench.sh a 5

# 5. A second seed: take the sha of the reference output for that seed
EXPECT_SHA=<sha> bash bench/bench.sh a 3 s7-a

# 6. S and D exactly as the harness counts them (needs ../sd-official and its tools image)
python3 bench/static_axes.py track-a

# 7. The live board: our runs and each track's top five, then the overall standing
python3 bench/board.py
python3 bench/overall.py
```

Replace `a` with `b`, `c` or `d` for the other tracks.

## Team

- Dhruv Sharma ([@Spiritsfuse](https://github.com/Spiritsfuse))
- Anushika Chauhan ([@Anushika06](https://github.com/Anushika06))
- Pratyush Mishra ([@PratyushMishra-2nd](https://github.com/PratyushMishra-2nd))

## Licence

MIT, see [LICENSE](LICENSE). The organisers' reference code that the starter shipped is copyright Hackathon Raptors CIC under the same licence.
