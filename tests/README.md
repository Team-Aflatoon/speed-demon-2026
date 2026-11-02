# End-to-end tests

Small black-box tests for the four tracks. Each case is a folder of input files, laid out exactly as the program finds them in `/data`, plus the expected output.

## Run

    bash tests/run.sh          # all four tracks
    bash tests/run.sh c d      # selected tracks

Needs Docker and bash (Linux, or Git Bash on Windows). For each track the script builds the image (`docker build --network=none -t sd-test/track-<t> track-<t>`), then runs every case as the scoring harness runs a submission: no network, read-only root, `/data` mounted read-only, user 65534, CPUs pinned to 0-11 (fewer if the machine has fewer), 8 GB memory. The output file is compared byte for byte with `expected.txt`. A case with an `expect-exit-1` marker is malformed input and must make the program exit with status 1. One PASS/FAIL line per case, a summary, and a non-zero exit on any failure. `SKIP_BUILD=1` reuses images already built.

## Cases (`tests/cases/<track>/<case>/`)

| Track | Input files | Cases |
|---|---|---|
| a | `requests.log` | spec worked example, every spec edge case in one file, empty input, tiny seeds 1 and 2, late lines and a shuffled file (300-line slices of tiny) |
| b | `docs.txt`, `queries.txt` | spec worked example, tokenizer byte rule, empty documents, left-to-right boolean, phrase adjacency, ten smallest ids, digit tokens, tiny seeds 1 and 2 (first 70 documents, 110 queries, to keep them small) |
| c | `records.csv`, `regions.csv` | spec worked example, BOM inside quotes and no final terminator, tiny seeds 1 and 2, five malformed inputs (bad amount, extra field, LF for a comma, LF inside a record, missing field) |
| d | `graph.txt`, `queries.txt` | spec worked example, parallel edges, duplicate edges, zero-weight cycle, node without out-edges, chain of maximum weights, tiny seeds 1 and 2 |

Expected outputs come from the organisers' reference implementations (`reference/*.py` in the official repository); where the spec or the official tests state the output, the reference was checked against it. The malformed inputs are also rejected by the reference.

## Not covered here

The heavier checks are not in this suite: full-size seeds 1, 7, 101 and 202, fuzzing, stress runs and race injection. They were run with `bench/bench.sh` and scratch scripts, and the results are recorded in each track's `PERF.md`.
