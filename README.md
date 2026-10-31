# Team Aflatoon · Speed Demon 2026

Our entry for [Speed Demon 2026](https://speeddemon.dev), the 72-hour performance championship by Hackathon Raptors: take a deliberately naive Python reference, keep its output byte for byte, and make it as fast and as lean as we can on the organisers' scoring machine.

We entered all four tracks. Each one is a standalone Rust program with **no dependencies** beyond the standard library, built offline from its own folder.

| Track | Task | Folder | Approach in one line |
|---|---|---|---|
| A · Ingest | 3.1M log lines → per-hour, per-endpoint counts, p50/p95/p99 and 5xx rate | [`track-a/`](track-a/) | See [track-a/README.md](track-a/README.md) |
| B · Index | 50,000 boolean and phrase queries over a 250 MB corpus | [`track-b/`](track-b/) | See [track-b/README.md](track-b/README.md) |
| C · Join | Hostile CSV (quotes, embedded newlines, CRLF, BOMs) joined to a lookup table | [`track-c/`](track-c/) | See [track-c/README.md](track-c/README.md) |
| D · Traverse | 2,000 shortest-path queries on a 5M-edge weighted digraph | [`track-d/`](track-d/) | See [track-d/README.md](track-d/README.md) |

Every track folder holds what the rules ask for: `Dockerfile`, `.speed-demon.toml`, `README.md` (approach, dependencies, next 24 hours), `PERF.md` (the optimisation log, with live-board run IDs) and the source.

## How it is scored

K = (T_ref/T)^0.50 × min(M_ref/M, 50)^0.20 × min(S_ref/S, 1)^0.15 × (D_ref/D)^0.15. Wall time dominates and is uncapped. Memory pays up to 50× less than the reference. Source lines and dependencies can only cost. That shaped every decision here: zero crates (D = 1), no input copies (the dataset is memory-mapped, and it is already in the page cache), and short code.

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
