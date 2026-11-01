# Track A: Ingest

Team Aflatoon: Dhruv Sharma (@Spiritsfuse), Anushika Chauhan (@Anushika06), Pratyush Mishra (@PratyushMishra-2nd).

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Result

On the scoring box (commit 32b49b8, PERF.md step 57): **20.4 ms, 18.2 MB, S 532, K 46.93**
(run-996cb0022b51); a second run, run-361c33ed2406, gave 20.9 ms, 17.9 MB, K 46.45 (median
20.65 ms, 18.05 MB). The reference takes 32.05 s and 228 MB. The output is byte-identical
to the reference's. M is in MB of 10^6 bytes, as the board reports it.

Before the hardening of step 57 the same design ran 20.0 and 20.1 ms at S 514 (commit
1cd61cf, step 56). S rose to 532 so that unexpected input fails loudly: every error names
its path, value or size, and no half-written summary is left behind. The hot path did
not change (the instruction count is the same), and the two T medians differ by less
than the box's run-to-run noise.

The idea, in three lines:

1. Read the log in 128 KB pieces, in file order, with `pread`; every line becomes one
   4-byte record in its hour's log.
2. Report each hour as soon as parsing is two hours past it, so the reports run alongside
   the parse instead of after it.
3. Check with exact counts that every hour was complete when reported; if one was not,
   redo the run with all reports at the end.

Where the box's time goes (all board runs; the first two are probes of earlier versions
that skip work on purpose, so their output is wrong):

| Run | What it does | Box T | Run ID |
|---|---|---|---|
| Read floor | map the input and count its newlines on 12 threads, nothing else | 14.6 ms | run-8ea2db5074de |
| Ingest only | v6, parse everything, report nothing | 21.3 ms | run-d570a81674c0 |
| Report after ingest | v3 to v5 whole: the report phase after the ingest barrier added about 11 ms | 31.8-34.6 ms | run-f4376824385c |
| This version | reports while parsing, reads with `pread` | 20.4 ms | run-996cb0022b51 |

## How to run

`ingest [data_dir] [out_dir]` reads `data_dir/requests.log` and writes
`out_dir/summary.txt`; both default to the harness's `/data` and `/out`. With Docker:
`docker build -t track-a . && docker run --rm -v "$PWD/data:/data:ro" -v "$PWD/out:/out" track-a`.

## Approach

One Rust binary, `src/main.rs`, no crates. Eleven worker threads read and parse the log
and report its hours while they parse; the main thread writes the output. PERF.md has the
history and the measurements behind each choice; this design is v8, steps 42 to 57
(PERF.md's Labels section maps the step, version and build names).

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
one region of a single `MAP_NORESERVE` reservation sized for the worst case (every line
of the file in that hour), so untouched pages cost nothing. Workers reserve 4 KB blocks of
a log, one page each, with one atomic `fetch_add` and fill their own block with plain
stores. Latencies of 2^20 and up, about 1.5% of them, go to the hour's side list as 8-byte
entries. Slots are atomics, which on x86 are ordinary moves; that matters only for the
early reports below.

**Reporting while parsing.** The parsed part of the file ends at the first piece still
being parsed. Once that piece's first line is two hours past hour h, hour h is reported:
the generator's shard 0 uploads each hour 5 to 10 minutes after it ends and the streaming
shards are at most 2.5 minutes late, so this leaves about 50 minutes of margin. The
scheduler (one lock and a condition variable, signalled only when a worker is waiting)
gives due hours priority over pieces, so reports are spread over the run instead of
waiting behind a barrier, and the writer starts within about 2 ms. When parsing ends, only
the last two hours are left to report.

**Exact whatever the input.** The spec promises no bound on how late a line can be, so the
speculation is checked, not trusted. A report counts the records it saw; a slot reserved
but not yet written reads as zero, endpoint id 0, which is never printed. Each line is
exactly one record in its hour, so the reports saw every line exactly when their counts
add up to the number of lines parsed. If they do not, the output is truncated and the
whole job runs again with every report at the end. On the generator's data that never
happens; a shuffled file, a reversed file or a late line make it happen, and still give
the right bytes, in about twice the time.

Why this is safe: speculation can make a run slower, never wrong. The count check holds
because a record slot goes from zero to its final value in one atomic store, so a report
sees each line whole or not at all and never counts one twice. The fallback is tested on
purpose, not left to chance: a shuffled file, a reversed file, a 23 MB file whose last
line belongs to the first hour, and a small mixed-tail file each force the rerun (one
truncate and two full reads each, counted with `perf`), and each gives the reference's
bytes; the unmodified 23 MB sample does not rerun.

**One report.** A count pass over the hour's records tallies them by their top 12 bits,
endpoint id and 5xx flag together, one increment per record; a prefix sum in endpoint
byte order gives every endpoint a slice of a scratch array; a scatter pass drops each
latency into its slice. Groups of up to 32 are sorted; larger ones get three
`select_nth_unstable` calls, each searching only above the previous rank. Then the hour's
log pages are given back with `madvise(MADV_DONTNEED)`, so the logs shrink while the output
grows. Byte order of the names is a sorted list of endpoint ids, rebuilt only when a report
meets an id it does not cover. Numbers are formatted eight digits at a time by SWAR
arithmetic, with the leading zeros shifted out, so there is no loop whose length depends
on the value.

**The per-line path.** Line ends are indexed 4 KB at a time: each 64-byte block (two AVX2
compares and `movemask`) holds at most two line ends, so both candidates (the lowest set
bit, and the lowest once that is cleared) are stored and the count advances by the
popcount, with no branch per block. The hour is cached as the first 13 bytes of the
timestamp and checked with one 16-byte compare; its row is worked out from the date only
when it changes (the span crosses one month end, October's 31st, so no calendar is
needed), and the output's hour stamp is made from the row. The latency is parsed right to
left from the newline with no loop: one SSE compare finds the space before it, its digits
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
lines at once, so it never grows by copying. The writer keeps at most four text buffers
and none for the last hours, at most two report scratch buffers are kept, each worker
frees its read buffer after one of the last pieces, and the last worker to finish unmaps
the logs, page tables included, before the final hours are written.

**Errors.** An I/O failure (with the file's path, or the size of a failed `mmap`), a
timestamp outside the 72-hour span (with the hour it names), more than 2,047 distinct
endpoints, or an endpoint over 126 bytes (with its length) goes to stderr with exit code 1,
and the partly written `summary.txt` is removed. A rerun after a late line is noted on
stderr; it still exits 0.

**Assumptions.** Well-formed input only: lines are assumed to follow the spec's grammar,
and only the checks above are made. `WORKERS = 11` assumes the 12-CPU scoring box (the
main thread is the twelfth). The logs are one `MAP_NORESERVE` reservation sized for every
line landing in every hour, about 5.2 GB of address space for the 239 MB input; it costs
no memory until touched, but needs the kernel to allow overcommit (Linux's default
heuristic mode does; with `vm.overcommit_memory = 2` the `mmap` fails and says so).

**Unsafe code.** Five uses, each with its `SAFETY` note in the source:

1. **System calls** (`mmap`, `madvise`, `munmap`, `prctl`, `mallopt`). Sound because each
   acts only on mappings this program made or on process-wide hints; a failed `mmap` is
   reported, and a refused hint changes nothing.
2. **The reserved logs**, handed out as `'static` slices and written through raw pointers.
   Sound because the memory is fresh and zeroed (zero is a valid atomic), each slot is
   handed to one worker by `fetch_add`, the counters never pass the room sized for every
   line, and the last worker unmaps it only once no report can read it.
3. **Unchecked buffer access**: the parser reads past a line's start, and the formatter
   writes into reserved capacity. Sound because every line start in the read buffer is
   followed by at least 256 readable bytes, more than any read from it, and each output
   line reserves 256 bytes and uses at most 220.
4. **SIMD intrinsics** (AVX2, BMI2, SSE4.1). Sound because `target-cpu=x86-64-v3` in
   `.cargo/config.toml` enables them for the whole crate, and the box's CPU has them.
5. **The endpoint table**: names in `UnsafeCell`s behind an `unsafe impl Sync`. Sound
   because each name is written once, under the insert lock, before its id is published
   with a Release store, and readers reach a name only through an id loaded with Acquire.

## Dependencies

None. `mmap`, `munmap`, `madvise`, `prctl` and `mallopt` are declared by hand with
`extern "C"`; SIMD comes from `std::arch`.

## With another 24 hours

- Selection is the largest single cost left: running without it (wrong output, as a bound)
  takes 10% less time and 15% fewer instructions. A one-pass selection that brackets
  the three ranks from a small sorted sample would get much of that back; our radix-sort
  attempt removed the mispredicts but cost more than it saved.
- Page faults are what the scoring box charges most for. The logs' 3k faults could go
  with a reused, pre-faulted pool of log pages, if a late line can be kept from landing
  in a page that another hour has taken over.
- The ingest loop is about 140 instructions a line, and the compiler spills several of its
  loop variables; a leaner line path is worth another 10%.
