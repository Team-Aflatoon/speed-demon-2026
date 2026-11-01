# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

Team Aflatoon: Dhruv Sharma (@spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

## Result

The submitted version, v4.2 (commit 5f8b6df), measured on the board as run-31b13fc9019b: T 48.9 ms, M 12.6 MB, K 25.15 (70.03 points), and run-8721232bf1f2: 50.5 ms, 12.3 MB, K 24.85 (69.77 points); median 49.7 ms. S 767, D 1 (no crates); this folder adds one line since (a failed write removes the partial output; S 768). The reference is 14.44 s and 367 MB. Before its error paths were added (v4.1, run-0fc0507654be) the same hot code ran 49.1 ms at S 734; the 33 extra lines make input the generator never writes fail loudly, with a byte offset, and the fallbacks say so on stderr. PERF.md has every step with its board run. M (bytes / 10^6) is about 9.7 MB of tables and buffers; the rest is code, stacks and the container runtime's processes, charged before `main` runs.

v4 as first submitted (run-1e5d2353fd9f) had a race that gave a wrong output in about 1 run of 100; a 300-run stress test found it, and v4.1 fixed it in one line (see **Distinct names**, and PERF.md's summary).

| File | What it does |
|---|---|
| `src/main.rs` | Arguments, the retry loop for the two fallbacks, output formatting, `die` |
| `src/pipeline.rs` | Threads, chunk claiming, pread and the chunk cuts, the quote-parity proof |
| `src/records.rs` | The records.csv parser: AVX2 masks, grammar checks, field parsing, batches |
| `src/names.rs` | The shared name table and the (name, group) bitset |
| `src/regions.rs` | regions.csv into a region-to-group map, parsed by several workers |
| `src/map.rs` | mmap and madvise by hand: file mappings and zeroed tables |

Usage: `track-c [DATA_DIR [OUT_DIR]]`, by default `/data` and `/out` (the harness passes none). Errors go to stderr with exit status 1; each fallback (a bigger name table, or one chunk) prints a note on stderr and still exits 0.

## Approach

A Rust binary with no crates, built as a static executable for x86-64-v3 (AVX2); the flags are in `.cargo/config.toml`, so a plain `cargo build` gives the same binary, and the crate refuses to compile without AVX2. Twelve worker threads (started as a binary tree, the main thread being worker 0) parse `records.csv` in parallel, and the result is exact for any input that follows the grammar in spec section 2.1, not only for what the generator writes.

**Input** (`pipeline.rs`, `map.rs`).

- `records.csv` is read with `pread` in pieces of 64 KiB, each worker into one reusable buffer of its own, and each piece is parsed while it is still in the core's cache; the small `regions.csv` is memory-mapped.
- Why not map `records.csv` too, as earlier versions did: locally the two cost the same, but head-to-head board runs of correct builds (PERF.md, rows 32 and 33) showed pread 6% faster on the scoring box, where, with turbo off at 2.5 GHz, faulting in, setting up and tearing down the page tables of a 200 MB mapping cost more than copying the bytes.
- A worker's chunk is nominally the 64 KiB from `c * 65536`; it reads that plus 1 KiB of slack, and its real start and end are the first record boundaries (see below) at or after the nominal start and end. Both cuts are computed from the file's bytes alone, so two neighbouring chunks always agree on the cut between them.
- If a record runs past the slack, the read doubles until the end is found, so records of any length work, and a record longer than a whole piece simply leaves the chunks it covers empty.
- After a chunk's bytes the buffer holds 128 zero bytes, so the parsers can always load whole 8-, 32- or 64-byte blocks; `regions.csv` gets the same from a page of zeros after its mapping.

**Parsing with bitmasks** (`records.rs`). For each 64-byte block, AVX2 compares give one bit per byte for `"`, `,`, LF and CR. In this grammar a byte is inside a quoted field exactly when an odd number of quote bytes come before it: a `""` escape simply closes and reopens. So the prefix XOR of the quote mask is the in-quotes mask, and the commas and LFs outside it are the separators. The same masks check the grammar: a quote that opens a field must follow `,` or `"`, one that closes a field must be followed by `,` or `"`, and a CR outside quotes must start a CRLF. Separator positions are flattened into a small buffer eight at a time, without a branch per separator. Each record is then its five separators, and its fields are parsed independently of each other: the integers, whose length the separators already give, checked and converted eight digits at once (SWAR) with no loop, the amount into exact integer cents, the text fields by length alone (a quoted field's unescaped length is its width minus its delimiters minus half its inner quotes). Nothing walks from field to field, so the CPU overlaps many records. A BOM is skipped where 2.1 allows it, at the start of a record.

**Splitting the file safely** (`pipeline.rs`). Chunks are cut just after an LF that is followed by a digit or a BOM byte. By spec 2.2 no quoted value contains a digit or a non-ASCII byte, so such an LF ends a record. That is a guess, and it is checked, not trusted. Every chunk is parsed as if it started outside quotes and counts its quote bytes. Since "inside quotes" means "an odd number of quotes so far", once every chunk has an even number of quotes in the chunks before it, every chunk did start outside quotes right after a terminator, and was parsed exactly as one sequential pass would have parsed it. If the check fails, or any chunk breaks the grammar, the whole file is parsed again by one worker as one chunk, and a grammar error there is reported, with its byte offset, and a non-zero exit. The tests below exercise this path with inputs that break the guess.

**Joining** (`regions.rs`). `regions.csv` becomes a map from region id to a dense group number (0 is `_UNMATCHED`), one byte per id packed eight to a word. Before records start, eight workers fill it in parallel while the other four fault in the name tables. Records are parsed in place into batches of about 32, and the region-map entries of a whole batch are prefetched before any of them is read, so their cache misses overlap.

**Distinct names** (`names.rs`). Every name is interned into a dense id by one lock-free hash table shared by all workers. A slot is 32 bytes: up to 24 bytes of the name's raw (still escaped) bytes and a metadata word, so a lookup usually reads one cache line and compares the bytes right there. Raw bytes work as the key because escaping is one-to-one, and the empty unquoted name and `""` both have raw length 0. A worker claims an empty slot by a compare-and-swap of its metadata word from 0 to a FILLING marker, writes the name words, and publishes it with one release store of the final metadata; a lookup that finds FILLING waits for that store. The metadata word changes only through those two operations, and the name words are written before the release store, so a lookup that sees a published slot sees the whole name. (v4 broke this: it wrote the key's hash word over the metadata before the release store, and a lookup that read it in that moment inserted the name a second time.) After the first few percent of the file nearly every name is already present, so the table is read-mostly and its lines stay shared by all cores. Each name id has 256 bits, one per group, in a shared bitset; a record sets its bit with an atomic OR, and the pair is new exactly when the bit was clear, so `names_distinct` is a count of flipped bits with nothing to merge. Each name's first 24 bytes are loaded once as three masked words, which serve to hash it, to compare it and to store it. Hashing, slot lookup and bit update each run over the whole batch, each pass prefetching what the next one reads. Both tables are faulted in before records start, each 2 MiB stretch by one worker, so that workers do not queue on the kernel's page-table locks; huge pages are refused, as a huge-page fault can stall while the kernel compacts memory. The ids are exact: the hash only chooses where to look, and the bytes are always compared.

**Waiting.** Workers only ever wait at the start, for regions.csv and the tables (about 2 ms), and that wait yields the CPU instead of spinning, so a crowded machine can run the workers still parsing. Everything else is claimed: a worker that finds no chunk left adds its aggregates to the total and stops.

**Aggregates.** Each worker keeps its own count, id sum, amount sum, min, max and byte counts per group; they are summed at the end, sorted by country code (index order already is code order), and printed with integer cents formatting. `_UNMATCHED` comes last.

**Limits, all far above the spec.** The name table starts with room for 114,688 names (the spec's pool has 100,000) and is rebuilt 8 times larger if a file has more, up to 58.7 million names. Each attempt faults its tables in up front and none is unmapped, so reaching that last size (a 2 GiB name table and a 1.9 GB pair bitset) peaks at about 4.6 GB, within the harness's 8 GB; the step before it, 7.3 million names, needs about 0.6 GB in all. Up to 255 countries, region ids up to 100,000,000 and files up to 4 GiB; beyond those the program says so on stderr and exits non-zero.

**Testing.** The official tests live in the organisers' repository (`tests/test_c_join.py` there, not in this one). Every check below compares the output byte for byte with the reference, unless it says otherwise. The synthetic inputs were written by small Python scripts that build records.csv and regions.csv from the spec's grammar (quoting, escapes, CR, BOMs and record lengths chosen per case); the scripts and files are not kept in this repository.

| Case class | Inputs | Path exercised |
|---|---|---|
| Full scale | seeds 1 and 7, and 101 and 202 generated afresh | the normal parallel path |
| The official hand-written cases | the worked example and the four 2.1-only variants: no BOM, U+FEFF inside quotes, no final terminator, final LF or CRLF | normal path, EOF handling, BOM skipping |
| Small and sample data | tiny data for seeds 2 to 6, sample data for seeds 2 to 9 | files of one chunk or a few |
| 2.1 features the generator never writes | unquoted non-empty text, CR, LF, digits and BOMs inside quotes, long names | the chunk check fails, so the one-chunk reparse |
| More than 114,688 distinct names | a synthetic file of unique names | the name table's retry with 8 times the slots |
| Records longer than the read slack and than a whole 64 KiB piece | four files | the read that doubles, empty chunks; where Python's csv module refuses a field over 128 KiB, compared with the earlier memory-mapped build |
| Malformed records | a field missing or extra, an LF in place of a comma, a short or quoted amount | rejected with an error and the byte offset, as the reference rejects them |
| Races | seed 1 run 100 to 300 times in a row in one container, by a stress script kept outside this folder | thread interleavings; found v4's race (PERF.md) |

## Dependencies

None. The two libc functions used, `mmap` and `madvise`, are declared by hand with `extern "C"`; `pread` comes from the standard library (`FileExt::read_exact_at`), as does everything else.

## With another 24 hours

- The atomic OR on the shared pair bitset costs about 7% of CPU at 12 threads compared with a plain store (measured by swapping in a racy one). Routing each pair to a worker that owns its name could recover part of that, but needs a queue that never blocks: an earlier ring-based version let one descheduled worker stall the others.
- Make the per-record path leaner. After the names (21 ms of the box's time), field parsing and aggregates cost 12.3 ms there and separator flattening 5.2 ms (locally 15 of 180 ms of single-thread CPU, 8%; PERF.md, probe table). Counting quotes per text field instead of at every separator was tried and was 7% slower; any next attempt has to be measured on the box, where per-record work costs 2 to 3 times more than on a laptop.
- Shrink memory: a rank bitmap instead of the 1 MB region map, and 24-byte table slots. The 3.7 MB pair bitset is close to the floor for exact counting, and the part of M that is not the program's own (2 to 3 MB on the board) is out of its reach.
- Keep measuring on the scoring box: its probes (PERF.md) showed that local timings, taken on a faster laptop that was shared with other jobs, mispredicted which changes would help, in both directions.
