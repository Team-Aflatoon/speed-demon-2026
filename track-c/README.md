# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

Team Aflatoon: Dhruv Sharma (@spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

## Result

The current version, v5.2, measured on the board as run-52c515f71c76: T 32.4 ms, M 16.7 MB, K 28.60 (72.82 points). S 882, D 1. Before it, v5.1 (commit 3aa2ae2, run-658779ed2eee: T 33.8 ms, M 16.3 MB, K 28.25, 72.55 points, S 858), D 1 (no crates). It is v5 (commit 6d23622, a clean-room rework of v4.2: 37.4 ms, 16.9 MB, K 26.81, run-41dfe519e529) plus one-register conversion of the three numbers, unchecked in-bounds parser reads and smaller pair blocks. The reference is 14.44 s and 367 MB. The previous final, v4.2 (5f8b6df and fbf7e89), ran 48.5 to 50.5 ms in 12.3 to 12.6 MB at K 25.15 to 25.38, so v5 is 11 to 13 ms faster and uses 4.6 MB more memory; K rose by 5.6% over v4.2's 25.38. M (bytes / 10^6) is mostly the program's own tables and buffers (see **Distinct names**); the rest is code, stacks and the container runtime's processes, charged before `main` runs. PERF.md has every step with its board run, including v4's race (a wrong output in about 1 run of 100, found by a stress test and fixed in v4.1) and why v5 replaced the design.

| File | What it does |
|---|---|
| `src/main.rs` | Arguments, the retry loop for the two fallbacks, output formatting, `die` |
| `src/run.rs` | Threads, start-up tasks, chunk claiming, pread and the chunk cuts, the quote-parity proof, the two phases |
| `src/parse.rs` | The records.csv parser: AVX2 masks, grammar checks, field parsing, windows of records |
| `src/names.rs` | The shared name table (phase 1) and the per-worker pair arenas with the counting (phase 2) |
| `src/regions.rs` | regions.csv into a region-to-group map, parsed by several workers |
| `src/sys.rs` | `mmap`, `madvise` and `prctl` by hand: file mappings, zeroed tables, prefaulting |

Usage: `track-c [DATA_DIR [OUT_DIR]]`, by default `/data` and `/out` (the harness passes none). Errors go to stderr with exit status 1; each fallback (a bigger name table, or one chunk) prints a note on stderr and still exits 0.

## Approach

A Rust binary with no crates, built as a static executable for x86-64-v3 (AVX2, BMI2, SSE4.2); the flags are in `.cargo/config.toml`, so a plain `cargo build` gives the same binary, and the crate refuses to compile without AVX2. Up to twelve worker threads (started as a binary tree, the main thread being worker 0) parse `records.csv` in parallel, and the result is exact for any input that follows the grammar in spec section 2.1, not only for what the generator writes. The work has two phases: phase 1 parses and joins, and phase 2, after one barrier, counts distinct names.

**Input** (`run.rs`, `sys.rs`).

- `records.csv` is read with `pread` in pieces of 64 KiB, each worker into one reusable buffer of its own, and each piece is parsed while it is still in the core's cache; the small `regions.csv` is memory-mapped. Not mapping `records.csv` was faster on the scoring box (PERF.md, rows 32 and 33).
- A worker's chunk is nominally the 64 KiB from `c * 65536`; it reads that plus 1 KiB of slack, and its real start and end are the first record boundaries (see below) at or after the nominal start and end. Both cuts are computed from the file's bytes alone, so two neighbouring chunks always agree on the cut between them.
- If a record runs past the slack, the read doubles until the end is found, so records of any length work, and a record longer than a whole piece simply leaves the chunks it covers empty.
- After a chunk's bytes the buffer holds 128 zero bytes, so the parser can always load whole 64-byte blocks.
- `prctl(PR_SET_THP_DISABLE)` runs first: a huge-page fault can stall while the kernel compacts memory.

**Parsing with bitmasks** (`parse.rs`). For each 64-byte block, AVX2 compares give one bit per byte for `"`, `,` and LF. In this grammar a byte is inside a quoted field exactly when an odd number of quote bytes come before it: a `""` escape simply closes and reopens. So the prefix XOR of the quote mask is the in-quotes mask, and the commas and LFs outside it are the separators. Separator positions are flattened into a small buffer eight at a time, without a branch per separator. Records are handled in windows of 2 KiB. Each record is its five separators, and its fields are parsed independently of each other: integers whose length the separators give, checked and converted eight digits at once (SWAR); an amount of up to five integer digits in one SWAR step with its dot shifted out; the text fields by length alone. A record's text bytes (name plus note) are its width minus its delimiters minus half its quote count, with the quote count taken once, at its LF. Name bytes come from phase 2, and note bytes are the difference. Pass 1 of a window parses the fields and prefetches the region-map byte and the name-table line each record needs; pass 2 does the join, the aggregates, the name lookup and the pair append, by which time most of those cache misses have overlapped. A BOM is skipped where 2.1 allows it, at the start of a record.

**Grammar checks.** Every record must be four commas and an LF outside quotes, with valid digits between them (a CR must come right before its LF); the LFs outside quotes must number exactly the records, which proves that the first four separators of every record were commas. A quote placed where 2.1 forbids it, inside an unquoted field, is not diagnosed. A failure is reported with the byte offset of the record, and a non-zero exit (see the fallback below).

**Splitting the file safely** (`run.rs`). Chunks are cut just after an LF that is followed by a digit or a BOM byte. By spec 2.2 no quoted value contains a digit or a non-ASCII byte, so such an LF ends a record. That is a guess, and it is checked, not trusted. Every chunk is parsed as if it started outside quotes and counts its quote bytes. Since "inside quotes" means "an odd number of quotes so far", once every chunk has an even number of quotes in the chunks before it, every chunk did start outside quotes right after a terminator, and was parsed exactly as one sequential pass would have parsed it. If the check fails, or any chunk breaks the grammar, `main` runs the whole file again with a single chunk, which starts at the start of the file and so is always right, and says so on stderr (`chunk cuts unproven; reparsing as one chunk`). A grammar error there is reported as `records.csv breaks spec 2.1 near byte N`, with exit status 1.

**Start-up** (`run.rs`). Before records start there are small tasks: faulting in six 512 KiB stretches of the name table, and parsing 32 pieces of `regions.csv`. Whichever workers are up first claim them, so workers wait only for the tasks to finish, not for the last thread to start; a late thread just joins phase 1. Each worker then faults in the expected share of its pair arena with one `MADV_POPULATE_WRITE`.

**Joining** (`regions.rs`). `regions.csv` becomes a map from region id to a dense group number (0 is `_UNMATCHED`), one byte per id, sized by the id on the file's last line (the largest, by spec 2.3). A region id past the map is clamped to a final 0 entry, so a lookup never branches. Any line that is not `id,CCC` stops the program with its byte offset.

**Distinct names** (`names.rs`). The shared hash table and the per-worker arenas keep the hot path from ever writing a cache line another core reads.

- Phase 1, while parsing: every name is interned in one shared lock-free table, and its slot index is its id. After the first few percent of the file nearly every name is already there, so the table is read-mostly. The record then appends its (id, group) pair, 16 bits, to a bucket in its own worker's arena: bucket `id >> 8`, entry `group << 8 | id & 255`. A bucket is a chain of 256-byte blocks (four cache lines: a 32-bit link and 126 entries), taken from the worker's own lazily backed arena, so these are private stores to a few hot lines.
- Phase 2, after one barrier: workers claim buckets. A bucket's pairs from all arenas are counted against an 8 KiB bitset (256 ids x 256 groups) that stays in L1. A new bit is a new distinct pair, and every entry also adds its record and its name's unescaped length to its group.
- The table has two arrays. Short names (at most 15 raw bytes) take 16-byte slots, four to a 64-byte line: the first name word and a `meta` word holding the tag (length + 1) and bytes 8 to 14. A short name's home is a whole line, which a lookup compares branch-free; overflow goes to lines an odd `step` apart, not to the next line, so there are no runs of full lines. Longer names take 32-byte slots of three name words and `meta`, probed linearly; their bytes past 24 live in an arena. Raw (still escaped) bytes are the key, which works because escaping is one-to-one, and the empty unquoted name and `""` both have length 0. The hash only chooses where to look; the bytes are always compared.
- A worker claims an empty slot by a compare-and-swap of `meta` from 0 to a FILLING marker, writes the name words, and publishes with one release store of the final `meta`. `meta` changes only through those two operations, and every reader loads it with acquire before reading the words, so a lookup that sees a published slot sees the whole name. A lookup that finds FILLING waits for the store. Workers reserve slots 64 at a time, so the table's counters are not touched on every insert.
- Memory: the name table is 3 MB (2 MiB of short slots and 1 MiB of long ones) and the pair arenas about 6.6 MB at 2 bytes per record, against v4.2's 4.2 MB table and 3.7 MB bitset. That is the price of v5's speed (PERF.md): 4.6 MB more on the board, K 5.6% higher.

**Waiting.** Workers wait twice, and both waits yield the CPU instead of spinning, so a crowded machine can run the workers still busy: at the start, for the start-up tasks, and once at the end of phase 1. Everything else is claimed: chunks in phase 1 and buckets in phase 2.

**Aggregates.** Each worker keeps its own id sum, amount sum, text bytes, min and max per group, and phase 2 yields records, distinct names and name bytes per group; they are summed at the end, sorted by country code, and printed with integer cents formatting. The note bytes are the text bytes minus the name bytes. `_UNMATCHED` comes last, and groups with no records are not printed.

**Limits, all far above the spec.** The name table starts with 2^17 short slots and 2^15 long ones, each array at most 7/8 full: room for 114,688 short names (the spec's pool has 100,000) and 28,672 long ones. If either fills, the run is repeated with 8 times as many slots, up to 2^26 short slots (58.7 million names, then `more than 58720256 distinct names` and exit 1); each attempt says so on stderr. Up to 255 countries (`regions.csv: over 255 countries` beyond), region ids up to 100,000,000 (`last id is over` beyond), and files under 4 GiB (the parser's offsets and name tags are 32-bit); beyond those the program says so on stderr and exits non-zero. Numbers of more than 8 digits saturate, so a huge region id is simply unknown, as the reference treats it.

**Testing.** The official tests live in the organisers' repository (`tests/test_c_join.py` there, not in this one). Every check below compares the output byte for byte with the reference, unless it says otherwise. The synthetic inputs were written by small scripts that build records.csv and regions.csv from the spec's grammar; the scripts and files are not kept in this repository.

| Case class | Inputs | Path exercised |
|---|---|---|
| Full scale | seeds 1, 7, 101 and 202 (each 3 times at 12 threads and once on one CPU) | the normal parallel path |
| Fuzz | 50 cases at 12, 2 and 1 CPUs: hand-made 2.1-only inputs (no BOM, U+FEFF inside quotes, no final terminator, final LF or CRLF), tiny and sample seeds, records longer than the read slack and than a whole chunk | EOF handling, BOM skipping, the read that doubles, empty chunks |
| Out of range, outside spec 2.2 | 7 of the fuzz cases: huge amounts, leading zeros, huge region ids (the generator never writes them; v4.2 failed these 7) | the long-number path, the unknown-region clamp |
| Forced single chunk | a digit after an LF inside quotes | the chunk check fails, so the one-chunk reparse |
| More than 114,688 distinct names | files of unique names | the name table's retry with 8 times the slots |
| Malformed records | 7 cases (a field missing or extra, an LF in place of a comma, a short or quoted amount, among them) | rejected with exit 1 and a byte offset, as the reference rejects them |
| Races | the Docker image run 300 times on seed 1 and 100 times on seed 7 | thread interleavings: 0 wrong outputs |

## Dependencies

None. The three libc functions used, `mmap`, `madvise` and `prctl`, are declared by hand with `extern "C"`; `pread` comes from the standard library (`FileExt::read_exact_at`), as does everything else.

## With another 24 hours

- Instructions are about 634 per record and spread thin: pass 1 about 260, the block scan about 68, flattening and pass 2 about 160, slow lookups about 50, phase 2 about 20. Bounds checks are about 20 per record. PCLMULQDQ for the prefix XOR would save about 8 per block, but needs a target feature beyond x86-64-v3 (and showed no gain in an earlier try, PERF.md row 22).
- Memory: 256-byte pair blocks leave about 1 MB of partly filled blocks; 128-byte blocks would halve that at the cost of longer chains in phase 2. The region map could be a rank bitmap (about 0.4 MB instead of 1 MB).
- S could drop further: rustfmt explodes every struct literal over 18 characters.
- Keep measuring on the scoring box: its probes (PERF.md) showed that local timings, taken on a faster laptop that was shared with other jobs, mispredicted which changes would help, in both directions.
