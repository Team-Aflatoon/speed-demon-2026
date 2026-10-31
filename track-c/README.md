# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

Team Aflatoon: Dhruv Sharma (@spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

## Approach

A Rust binary with no crates, built as a static executable for x86-64-v3 (AVX2). Twelve worker threads parse `records.csv` in parallel, and the result is exact for any input that follows the grammar in spec section 2.1, not only for what the generator writes.

**Input.** Both files are memory-mapped (`map.rs`). The harness has already read them into the page cache, so nothing is copied, and the mapping ends in a page of zeros so that the parsers can always load whole 8-, 32- or 64-byte blocks.

**Parsing with bitmasks** (`records.rs`). For each 64-byte block, AVX2 compares give one bit per byte for `"`, `,`, LF and CR. In this grammar a byte is inside a quoted field exactly when an odd number of quote bytes come before it: a `""` escape simply closes and reopens. So the prefix XOR of the quote mask is the in-quotes mask, and the commas and LFs outside it are the separators. The same masks check the grammar: a quote that opens a field must follow `,` or `"`, one that closes a field must be followed by `,` or `"`, and a CR outside quotes must start a CRLF. Separator positions are flattened into a small buffer eight at a time, without a branch per separator. Each record is then its five separators, and its fields are parsed independently of each other: the integers with an 8-digit SWAR conversion, the amount into exact integer cents, the text fields by length alone (a quoted field's unescaped length is its width minus its delimiters minus half its inner quotes). Nothing walks from field to field, so the CPU overlaps many records. A BOM is skipped where 2.1 allows it, at the start of a record.

**Splitting the file safely** (`pipeline.rs`). The file is cut into 16 chunks per worker, just after an LF that is followed by a digit or a BOM byte. By spec 2.2 no quoted value contains a digit or a non-ASCII byte, so such an LF ends a record. That is a guess, and it is checked, not trusted. Every chunk is parsed as if it started outside quotes and counts its quote bytes. Since "inside quotes" means "an odd number of quotes so far", once every chunk has an even number of quotes in the chunks before it, every chunk did start outside quotes right after a terminator, and was parsed exactly as one sequential pass would have parsed it. If the check fails, or any chunk breaks the grammar, the whole file is parsed again by one worker as one chunk, and a grammar error there is reported with a non-zero exit. The tests below exercise this path with inputs that break the guess.

**Joining** (`regions.rs`). `regions.csv` becomes a byte array indexed by region id, holding a dense group number (0 is `_UNMATCHED`). The first four workers fill it in parallel while the others start. Records are parsed in batches of about 32, and the region-map entries of a whole batch are prefetched before any of them is read, so their cache misses overlap.

**Distinct names** (`names.rs`). Every name is interned into a dense id by one lock-free hash table shared by all workers. A slot is 32 bytes: up to 24 bytes of the name's raw (still escaped) bytes and a metadata word, so a lookup usually reads one cache line and compares the bytes right there. Raw bytes work as the key because escaping is one-to-one, and the empty unquoted name and `""` both have raw length 0. A worker claims an empty slot with a compare-and-swap, fills it, and publishes it with a release store. After the first few percent of the file nearly every name is already present, so the table is read-mostly and its lines stay shared by all cores. Each name id has 256 bits, one per group, in a shared bitset; a record sets its bit with an atomic OR, and the pair is new exactly when the bit was clear, so `names_distinct` is a count of flipped bits with nothing to merge. Hash, slot lookup and bit update each run over the whole batch, each pass prefetching what the next one reads. The table and bitset sit on transparent huge pages, faulted in up front from one thread. The ids are exact: the hash only chooses where to look, and the bytes are always compared.

**Aggregates.** Each worker keeps its own count, id sum, amount sum, min, max and byte counts per group; they are summed at the end, sorted by country code (index order already is code order), and printed with integer cents formatting. `_UNMATCHED` comes last.

**Limits, all far above the spec.** The name table starts with room for 114,688 names (the spec's pool has 100,000) and is rebuilt 8 times larger if a file has more. Up to 255 countries, region ids up to 100,000,000 and files up to 4 GiB; beyond those the program says so on stderr and exits non-zero.

**Testing.** Besides seeds 1 and 7 at full scale, the output was compared byte for byte with the reference on the hand-written cases of `tests/test_c_join.py` (the worked example and the four 2.1-only variants: no BOM, U+FEFF inside quotes, no final terminator, final LF or CRLF), on tiny and sample data for seeds 2 to 9, and on synthetic files using 2.1 features the generator never writes (unquoted non-empty text, CR, LF, digits and BOMs inside quotes, long names, more than 114,688 distinct names). Those last ones make the chunk check fail or the table overflow, and still match.

## Dependencies

None. The two libc functions used, `mmap` and `madvise`, are declared by hand with `extern "C"`; everything else is the Rust standard library.

## With another 24 hours

- Give each worker a private share of the names instead of the shared atomic bitset: route each (name, group) pair to its owner through single-producer rings. An earlier version did this and used about 10% less CPU, but it was 250 lines longer and stalls whenever one worker is descheduled, so it was set aside.
- Cut the per-record work of the parser: fewer passes over the batch arrays, a cheaper end-of-record check, and the amount parsed with one SWAR step.
- Shrink memory: a rank bitmap instead of the 1 MB region array, and 24-byte table slots.
- Profile on the scoring machine itself; the local machine was shared with other heavy jobs, so many small changes could not be measured reliably.
