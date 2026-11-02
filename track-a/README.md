# Track A: Ingest

Team Aflatoon: Dhruv Sharma (@Spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Result

Final code: **v13** (commit dae0fd7, this tree, **S 447**: Rust 441 + Dockerfile 6, D 1, no crates). It is v12 (commit
7d7779d, S 466, the code on the live board today) with a behaviour-preserving size cut, and its output is byte-identical
(seeds 1 and 7 re-checked on this tree, PERF.md step 76). The reference takes 32.05 s and 228 MB. M is in MB of 10^6
bytes, as the board reports it.

Scoring box, v12, five runs on Nov 2 (PERF.md step 75): 19.7 to 19.9 ms, 16.8 to 17.3 MB, K 49.12 to 49.29
(84.56 to 84.64 points). Best: **19.7 ms, 17.1 MB, K 49.29, 84.64 points** (run-8d63f862b12c). The others:
run-28bed15e9c09 19.8 ms, 17.1 MB, K 49.15; run-a6e0c93c5bdc 19.7 ms, 17.3 MB, K 49.12; run-e8ada4910704 19.9 ms,
16.8 MB, K 49.14; run-3a42a8e95305 19.9 ms, 16.9 MB, K 49.16. v13's first box run: run-8dff5d3d18e6, 19.8 ms,
17.4 MB, K 49.32, 84.65 points; more are pending (PERF.md step 76). The size cut from 466 to 447 is worth about 0.14 points by the K
formula, if the box confirms it runs as fast as v12.

Why v12 and not v9 (commit 6ded953, S 521: 19.3 to 21.1 ms, 16.9 to 17.3 MB when steady, run-ccb75cc37209 at best): the
final score takes the **maximum** `memory.peak` of 5 runs, and v9 peaked at 21 to 22 MB instead of 17 MB in a fifth to a
half of its board runs (run-b4ba6a189863 21.1 MB, run-8e6cfdf092c8 21.6 MB, run-7d7450969105 21.8 MB,
run-d024bbf208cc 22.1 MB). The cause, as far as we can tell, is a stalled frontier: a descheduled thread holding the oldest unparsed piece
stops hours from being reported, while the other threads race ahead and pile up logs and texts. v12 hands out a
piece only within 64 pieces (`LEAD`) of that oldest piece. v12's S 466 is v11's trim of v9 (S 521 to 464, plus 2 lines
for the cap); v11 had been dropped on Nov 1 for one slow run in two, which in hindsight was the same stall, not the
trim (PERF.md, lessons).

v9 is v8 (20.4 ms, 18.2 MB, S 532, K 46.93, run-996cb0022b51) with quiescent reports (an hour is reported only
after every piece that could write it is parsed, so no count check is needed), two-slot records for wide
latencies instead of side lists, branch-free percentiles for small groups, and memory cuts. v13 keeps all of that.

The idea, in three lines:

1. Read the log in 128 KB pieces, in file order, with `pread`; every line becomes one
   4-byte record in its hour's log.
2. Report each hour as soon as parsing is two hours past it and every piece that could write it is parsed,
   so the reports run alongside the parse instead of after it. No piece is handed out more than 64 pieces
   past the oldest one still being parsed, which bounds what a stalled thread can make the others pile up.
3. If a line ever arrives for an hour that was already closed, redo the run with all reports
   at the end.

Where the box's time goes (all board runs; the first two are probes of earlier versions
that skip work on purpose, so their output is wrong):

| Run | What it does | Box T | Run ID |
|---|---|---|---|
| Read floor | map the input and count its newlines on 12 threads, nothing else | 14.6 ms | run-8ea2db5074de |
| Ingest only | v6, parse everything, report nothing | 21.3 ms | run-d570a81674c0 |
| Report after ingest | v3 to v5 whole: the report phase after the ingest barrier added about 11 ms | 31.8-34.6 ms | run-f4376824385c |
| v9 | reports while parsing, reads with `pread`, hours closed before they are reported | 19.3 ms | run-ccb75cc37209 |
| This version's algorithm (v12) | the same plus the lookahead cap and the trimmed code | 19.7 ms | run-8d63f862b12c |

## How to run

`ingest [data_dir] [out_dir]` reads `data_dir/requests.log` and writes
`out_dir/summary.txt`; both default to the harness's `/data` and `/out`. With Docker:
`docker build -t track-a . && docker run --rm -v "$PWD/data:/data:ro" -v "$PWD/out:/out" track-a`.

## Approach

One Rust binary, `src/main.rs`, no crates. Eleven worker threads read and parse the log
and report its hours while they parse; the main thread writes the output, and parses pieces
between writes. PERF.md has the history and the measurements behind each choice; this design
is v13: v9's algorithm (steps 58 to 65 on top of v8's steps 42 to 57), trimmed in v11, with v12's lookahead
cap and v13's further size cut (steps 71 to 76; PERF.md's Labels section maps the step, version and build names).

**Reading in pieces.** The file is cut into 128 KB pieces, which workers take in file
order. A worker reads its piece with `pread` into its own buffer: from the byte before
the piece (a line starts after a newline, so that byte decides where the piece's first
line starts) to the end of the last line that starts in it, followed by 256 zero bytes so
the parser may read a little past any line without a bounds check. The buffer, with its
hyperthread sibling's, stays in the core's L2. We read rather than map the file: on the
scoring box this build ran 3.8 ms faster than the same program mapping the input (PERF.md,
v8). The two differ in piece size too (1 MB mapped, 128 KB read), so that gain is the
mapping's page faults together with whatever the smaller, L2-sized pieces bring, not
faults alone.

**Parsing into one log per hour.** Every line becomes one 4-byte record,
`endpoint id << 21 | 5xx << 20 | latency`, appended to its hour's log. Each hour's log is
one region of a single `MAP_NORESERVE` reservation, so untouched pages cost nothing; each
has room for 2^17 records (the generator's busiest hour has about 74,000 lines), and a
fuller hour is handled like a late line (see below). Workers reserve 4 KB blocks of a log,
one page each, with one atomic `fetch_add` and fill their own block with plain stores.
A latency of 2^20 or more, about 1.5% of them, takes two slots: its high bits go in a slot
before the record, a slot with endpoint id 0, which is never printed. A wide record that
would start in a block's last slot leaves that slot empty. The slots are atomics, which on
x86 are ordinary moves, because many threads write the shared logs.

**Reporting while parsing.** The parsed part of the file ends at the first piece still
being parsed. Once that piece's first line is two hours past hour h, hour h is closed: no
piece handed out from then on may write it. It is reported as soon as every piece handed
out before it closed has been parsed, so a report never races a writer and the hour's log
is final. The generator's shard 0 uploads each hour 5 to 10 minutes after it ends and the
streaming shards are at most 2.5 minutes late (the spec's Order paragraph). An hour closes at a
line at least 60 minutes after it ends, and its own lines arrive at most 10 minutes after it
ends, so this leaves about 50 minutes of margin.
The scheduler (one lock and a condition variable, signalled only when a worker waits) gives
due hours priority over pieces, so reports are spread over the run instead of waiting
behind a barrier. The main thread writes each hour once all earlier hours are out, and
parses pieces whenever no text is waiting, so all 12 hardware threads parse. When parsing
ends, every remaining hour is closed and reported; the last few are the day's busiest,
and that tail is about a millisecond locally (PERF.md, step 64).

**Bounded lookahead.** A piece is handed out only if it is fewer than `LEAD = 64` pieces past the frontier, the oldest
piece still being parsed. The reason is memory, not speed: if the thread that holds the frontier piece is descheduled,
no further hour can be reported (an hour is reported only once every earlier-handed piece is parsed), while the other
threads keep parsing, so logs and texts for hours that cannot be reported yet pile up, and the board's final M is the
maximum over 5 runs. In a normal run about 12 pieces are in flight and the cap never binds. It cannot deadlock: when it
binds, the frontier piece belongs to a thread that will run again, and its `parsed()` wakes the waiters; with no
piece being parsed the frontier is the next piece to hand out, which is below the cap (when none is left, every hour is
due). A worker that finds neither an hour due nor a piece within the cap waits on the condition variable.

**Exact whatever the input.** The spec promises no bound on how late a line can be, so the
speculation is checked, not trusted. A piece that finds a line for an hour already closed,
or an hour with no room left, marks the run late and stops; the workers stop, and the whole
job runs again with no speculation: every hour closes only once every piece is parsed, and
each hour's log has room for every line of the file. The first run's logs are not unmapped (only a run that was not
late does that), so the rerun starts with their pages still resident; that rare path has a higher memory peak.
On the generator's data that never
happens; a shuffled file, a reversed file or a late line make it happen, and still give the
right bytes, more slowly.

Why this is safe: speculation can make a run slower, never wrong. A report runs only after
its hour is closed and every piece that could have written it is parsed, so it sees all of
the hour's records and no later write can change them; a line that would arrive later is
exactly the case that is detected. The fallback is tested on purpose, not left to chance: a
shuffled file, a reversed file, a 23 MB file whose last line belongs to the first hour, a
small mixed-tail file, and crowded-hour and wide-latency cases each force the rerun and
give the reference's bytes; the unmodified 23 MB sample does not rerun.

**One report.** A count pass over the hour's records tallies them by their top 12 bits,
endpoint id and 5xx flag together, one increment per record; a prefix sum in endpoint
byte order gives every endpoint a slice of a scratch array; a scatter pass drops each
latency into its slice (a wide latency's high bits come from the slot before its record).
Then the hour's log pages are given back with `madvise(MADV_DONTNEED)`, so the logs shrink
while the output grows. A group of up to 64 gets its three percentiles by rank counting:
the value of rank r is the largest x with at most r values below it, found with vector
compares and no branch on the data (sorting small groups mispredicted about every other
compare). Larger groups get three `select_nth_unstable` calls, each searching only above
the previous rank. The byte order of the names is a sorted list of endpoint ids, rebuilt
when new ids have come in since the last report. Numbers are formatted eight digits at a
time by SWAR arithmetic, with the leading zeros shifted out, so there is no loop whose
length depends on the value.

**The per-line path.** Line ends are indexed 4 KB at a time: each 64-byte block (two AVX2
compares and `movemask`) holds at most two line ends, so both candidates (the lowest set
bit, and the lowest once that is cleared) are stored and the count advances by the
popcount, with no branch per block. The hour is cached as the first 13 bytes of the
timestamp and checked with one 16-byte compare; its row is worked out from the date only
when it changes (the span crosses one month end, October's 31st, so no calendar is
needed). The output's hour stamp is made from the row: the date comes from a table of the span's four days
(`10-30` to `11-02`), the hour from the row modulo 24. (The idea of a date table for the label, instead of
calendar arithmetic, is from the public AVENGERS entry, github.com/Mukul07777/speed-demon-2026; the code is
ours.) The latency is parsed right to left from the newline with no loop: one SSE compare finds the space before it, its digits
are masked in and summed by three multiply-add steps, and the status digit before that
space gives the 5xx flag. One more 16-byte compare finds the space after the method,
where the endpoint starts.

**Endpoints.** One table, shared by all workers and kept in a `static` so the hot loop
addresses it directly: open addressing over 8,192 slots on a hash of the name's first and
last 8 bytes and its length. Each name is stored once, followed by its space and
zero-padded, in a 128-byte record. A lookup compares 64 bytes at a time with AVX2 and masks
off the bytes past the space with BMI2 `bzhi`; since names contain no spaces, comparing
the space too checks the length. The hash is only a shortcut: equality is always decided
on the bytes. Lookups take no lock. An insert takes one, looks again, writes the name and
then publishes its id with a Release store.

**Memory.** At the peak the process holds the output's tmpfs pages (10.1 MB), the records
of the few hours in flight, report scratch and texts. `prctl(PR_SET_THP_DISABLE)` keeps
transparent huge pages (`always` on Debian) off the sparse reservation; a fixed
`M_MMAP_THRESHOLD` stops glibc from raising its threshold after the first large free, so
freed buffers really go back to the kernel. Each hour's text reserves room for all its
lines at once, so it never grows by copying, and report scratch is allocated once at full
size (growing it by doubling left about 130 KB of freed smaller copies in each worker's
malloc arena). The hours' logs are 512 KB apart, so neighbouring hours share page-table
pages. The writer keeps at most four text buffers and none for the last hours, at most two
report scratch buffers are kept, each worker frees its read buffer after one of the last
pieces, and the last worker to finish unmaps the logs, page tables included, before the
final hours are written. Recycling log blocks through a pool instead of `madvise` cut page
faults from 6.2k to 2.4k, but the memory peak then followed the stalls of the run (PERF.md,
step 58), so the logs stay per hour. The `LEAD` cap is what keeps that peak steady when a thread stalls: before it,
v9 peaked at 21 to 22 MB instead of about 17 MB in some runs; with it, the board's five v12 runs read 16.8 to 17.3 MB.

**Errors.** An I/O failure (with the file's path where it has one), a failed `mmap`, or a
timestamp outside the 72-hour span (with the hour it names) goes to stderr with exit code 1;
a partly written `summary.txt` is left as it is. More than 2,047 distinct endpoints, or an
endpoint over 126 bytes, is not diagnosed: the endpoint table's indexing fails and the
process aborts (`panic = "abort"`), and no valid input has either (the spec promises 1,842
and 125). A rerun after a late line is noted on stderr; it still exits 0.

**Assumptions.** Well-formed input only: lines are assumed to follow the spec's grammar,
and only the checks above are made. `WORKERS = 11` assumes the 12-CPU scoring box (the
main thread is the twelfth). Speculating, the logs are one `MAP_NORESERVE` reservation of
72 hours of 2^17 records (38 MB of address space); the rerun's reservation holds every line
of the file in every hour, about 3.4 GB for the 239 MB input (238,836,234 bytes on seed 1; room is len/20 slots of 4 bytes per hour). Neither costs memory until
touched, but they need the kernel to allow overcommit (Linux's default heuristic mode does;
with `vm.overcommit_memory = 2` the `mmap` fails and says so).

**Unsafe code.** Five uses, each with its `SAFETY` note in the source:

1. **System calls** (`mmap`, `madvise`, `munmap`, `prctl`, `mallopt`). Sound because each
   acts only on mappings this program made or on process-wide hints; a failed `mmap` is
   reported, and a refused hint changes nothing.
2. **The reserved logs**, handed out as `'static` slices and written through raw pointers.
   Sound because the memory is fresh and zeroed (zero is a valid atomic), each block is
   handed to one worker by `fetch_add`, a block is used only after the counter is checked
   against the hour's room, and the last worker unmaps the logs only once every hour is
   reported, so no report can read them afterwards. The record stores are `Relaxed`: a report starts only after the
   plan mutex has seen the `parsed()` of every piece that could write the hour, and the mutex orders those
   stores before the report's loads.
3. **Unchecked buffer access**: the parser reads past a line's start, and the formatter
   writes into reserved capacity. Sound because every line start in the read buffer is
   followed by at least 256 readable bytes, more than any read from it (the call sites of
   `parse_latency` and `Endpoints::id` each show the arithmetic), and each output line reserves 256 bytes and
   uses at most 220.
4. **SIMD intrinsics** (AVX2, BMI2, SSE4.1). Sound because `target-cpu=x86-64-v3` in
   `.cargo/config.toml` enables them for the whole crate, and the box's CPU has them.
5. **The endpoint table**: names in `UnsafeCell`s behind an `unsafe impl Sync`. Sound
   because each name is written once, under the insert lock, before its id is published
   with a Release store, and readers reach a name only through an id loaded with Acquire.

## Dependencies

None. `mmap`, `munmap`, `madvise`, `prctl` and `mallopt` are declared by hand with
`extern "C"`; SIMD comes from `std::arch`.

## With another 24 hours

- Wait for the box: v13's runs decide whether the 19 lines of S it saves are free (PERF.md, step 76). If it ran as
  fast as v12 on the board it is final; if not, v12 (commit 7d7779d) is the fallback with the same behaviour.
- The cap leaves one memory mode: about a quarter of capped runs locally showed a peak 3 MiB above the rest,
  from the in-order writer, which the cap does not bound. A smaller `LEAD` is the obvious thing to try; untested,
  and it would have to be judged on the board's max of 5, not on medians.
- Percentiles are the largest single cost left: before v9's rank counting for small groups,
  selection and the small sorts were 14% of the instructions and 62% of all branch
  mispredicts (cachegrind). A histogram of 264 log-spaced buckets to bracket the ranks of
  big groups bought nothing in a microbenchmark (9.71 against 9.75 ms over all 126,908 groups
  of seed 1, one core), so what is left to try is a one-pass selection from a small sorted
  sample.
- Page faults are what the scoring box charges most for. A recycled pool of log blocks cut
  them from 6.2k to 2.4k and CPU by 4%, but wall time was equal and the memory peak grew
  by 2 MiB or more under stalls (PERF.md, step 58). `LEAD` now bounds how far ahead blocks
  can be cut, so a pool sized to that bound is the version that might pay; untested.
- The tail after the last piece is one busy hour's report (count 35 to 60 us, scatter 100 to
  190 us, format 300 to 430 us locally). Splitting it across idle threads cost more total
  work than it saved (PERF.md, step 68).
- The ingest loop spills several of its loop variables; a leaner line path is worth another
  try.
