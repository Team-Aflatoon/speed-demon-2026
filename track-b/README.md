# Track B: Index

Team Aflatoon: Dhruv Sharma (@Spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

The queries are part of the input, so the program reads `queries.txt` first and indexes
only its terms. Twelve threads parse line-aligned ranges of the file and insert every word
into one open-addressing hash table at the same time: a thread claims an empty slot with a
compare-and-swap on its first word, so no locks are needed. A slot is 16 bytes: the first
12 bytes of the term inline, its length, a term id and two flags (the term appears in a BOOL
query; the term anchors a phrase). Ids are block-dense: each thread takes 1,024 at a time from
a shared counter and hands them out in order, so an id costs no shared counter per term and
each thread leaves at most one block partly unused. The table has room for every query word to be a
distinct term at a load of at most 3/4, so it can never fill up, whatever the input.

**Phrases need no index.** Every phrase gets one anchor term: the one that occurs least often
in `queries.txt`, the longest on a tie. Common words appear in many queries and rare words in
one or two, so this picks rare corpus words without reading the corpus: on seed 1 the chosen
anchors occur 1.26 million times in `docs.txt`, against 0.92 million for the truly rarest
terms and 52 million for "the longest term". While scanning a document the program keeps
the term id of every token (a dummy id for tokens that are no query term). When the document
ends, each phrase anchored at one of its tokens is compared with the ids around that token.
Positions are never stored and no document is read twice.

**Reading and tokenizing.** `docs.txt` is read with `pread` into a 128 KB buffer per
thread, cut at the last line end, with the partial line carried to the next read. Each
64-byte block becomes three bitmasks (token bytes, LFs and TABs) from a few AVX2 compares.
Every line starts with "id TAB", so a prefix XOR of the LF and TAB bits marks exactly those
bytes and the ids never become tokens. Token starts and ends are extracted from the masks
into two arrays. Tokens are then keyed (`| 0x20` lowercases letters and leaves digits alone),
hashed and their table slots prefetched 64 at a time, and only then looked up, so the cache
misses of rare words overlap.

**Postings.** The corpus is cut into segments of about 8 MB (some 8,000 documents), and
documents are numbered from 0 within a segment as `u16`. For each document the scan writes
its distinct BOOL terms (found with an L1-sized bitset) as `doc << 16 | id / 64` into one of
64 streams, chosen by `id % 64`. One build task per stream counting-sorts it into a sorted
list per term and adds a bitmap for terms found in more than 1/16 of the segment's documents.
Everything a build task touches fits in about 100 KB.

**BOOL queries** are answered once per segment, strictly left to right. The first term's set
is borrowed until a second set has to be combined with it. AND and NOT against a dense term
test its bitmap. Between two lists, one is spread into a small bitmap and the other tested
against it, which is about three times faster than a merge (whose steps depend on each
other); when one list is 16 times longer, the short one gallops through it instead. A large OR
goes to a bitmap. Answers add up across segments in id order: count and sum add, and the ten
smallest ids are the first ten that arrive. The sum of the ids in a bitmap word comes from six
masked popcounts rather than a visit to every bit.

**One task queue.** Scanning, building and answering are tasks taken in the order
`scan 0, scan 1, build 0, answer 0, scan 2, build 1, answer 1, ...`. A task waits only for the
tasks whose output it reads. The streams are double buffered by segment parity, so threads go
on scanning the next segment while the last one is built and answered, and no thread idles at
a phase boundary.

All I/O errors and malformed input end the program with a message on stderr and exit code 1.
`unsafe` appears only in the hand-declared `prctl`, the prefetch hint and the AVX2 loads, each
with a `SAFETY` note.

## Dependencies

None. The Rust standard library only. `prctl` is declared by hand with `extern "C"`. The
build needs AVX2 (`.cargo/config.toml` sets `x86-64-v3`) and refuses to compile without it,
and links the binary statically so that a run never pays for the dynamic loader.

## With another 24 hours

- The query parse runs before the scan can start and takes about 6 ms of wall time: four
  barriers, a 4 MB table filled by one thread while the others wait, and the inserts' cache
  misses (prefetching ahead made no difference on the board). Letting the first scan tasks read
  and lex the corpus (which needs no table) while the table is filled would hide most of it.
- About 40% of the scan's CPU time is the table lookup itself, and halving the table changed
  nothing on the board, so the lookup's own work (hashing, compares, branches) is what to cut.
  Next would be a direct table for the one- and two-byte tokens (1,332 entries), which are among
  the most frequent and would then need no hash, measured on the box.
- Writing the BOOL postings costs about 4 ns per (term, document) pair. The commonest terms make
  up about a third of the pairs and end up as bitmaps anyway, so the scan could set their bits
  directly and skip their postings and their counting sort.
