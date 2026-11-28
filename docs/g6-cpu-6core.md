# G6 on a 6-core CPU: where the CPU time goes

Measured 2026-11-27 on a Ryzen 5 5600X (6 cores, 12 threads), branch `g6-vt3` (`da7fdfc`),
the diagnostics build, `scripts/g6/run-g6.ps1`'s switch set with `-Console series-s` and DLSS quality,
shaders warm (`VICTORY_PIPELINE_BOOK=1` + `victory warm`), the same save and scripted keys every run,
5 minutes a run. Numbers are per frame, averaged over the steady part of the world
(the prison-window shot, ~4,400 draws).

**Result: about 16 fps (15.7–16.8 across baseline runs), 62–64 ms a frame. The card is idle about half of it.
The limit is the whole CPU, not one thread.**

## The submitting thread

61.6 ms of work in a 63.6 ms frame:

| Step | ms |
|---|---|
| lists' steps | 24.7 |
| · refreshes of copies of title memory (8,749; 88% find nothing new) | 16.6 |
| · copies and fills | 5.8 |
| copies in the card's memory (226 hash + 411 compare passes) | 9.8 |
| waiting for the heaps' records at the end | 6.8 |
| asking the write watch | 4.6 |
| title bytes under flights' writes | 3.7 |
| waiting for flights (two small fills of upload memory not done on the card) | 3.4 |
| sending the lists | 1.8 |

The "nothing new" checks are cheap (~1.4 µs, ~7 ms summed over all threads); the refreshes' wall time is the
real updates (~850, ~27 µs each) and the fan-outs to the `shadow::wide` threads, which run at ~40% efficiency
here: `wide_threads()` gives 3 helpers on 12 logical processors, and they sleep between jobs.

## Thread counts do not help on their own

With `VICTORY_THIN_THREADS=4; VICTORY_UNDER_THREADS=3; VICTORY_LAND_THREADS=2` (the set uses 8/6/4) the
submitting thread dropped from 61.9 to 52.0 ms (refreshes 18.8 → 12.9) and the title's threads stopped waiting for
it at presents (22.4 → 0.1 ms), but list translation became the limit and the frame rate stayed at 15.9.
The process uses ~8.6 cores' worth of CPU a frame (~490 ms user + 60 ms kernel per 63.6 ms).

`VICTORY_INDIRECT_WINDOW=1; VICTORY_INDIRECT_BATCH=2` gave no gain either (14.9 fps, indirect calls still 32 µs each).

## List translation (`VICTORY_THIN`): ~270 ms of CPU a frame

By kind of call (new line `list calls translated per frame by kind`):

| Call | Per frame | ms (all threads) | Each |
|---|---|---|---|
| EXECUTE_INDIRECT | 4,541 | 136 | 30 µs |
| DISPATCH | 2,750 | 69 | 25 µs |
| DISPATCH_MESH | 344 | 13 | 39 µs |
| COPY_TEXTURE | 5 | 11 | 2.3 ms |
| RESET / CLOSE | 203 / 203 | 9 / 8 | 46 / 37 µs |
| ~30,000 state calls together | | ~7 | 0.1–0.3 µs |

Most of a draw or dispatch is `DrawState::bind` (~200 ms a frame, ~27 µs a bind), and most of that is
**descriptor tables (~160 ms)**:

| `tables` path | Tables a frame | ms |
|---|---|---|
| made in dynamic slots | 9,480–12,600 | 133–170 |
| direct entries | 12,000–15,000 | ~41 |
| known, views looked at again | 3,000–5,000 | 12–16 |
| known, taken as they were | ~1,000 | 0.5 |
| hashing | 13,000–19,000 | 2–3 |

Of the tables made: slots copied from a view the list made before 84 ms (38,583 slots, 2.2 µs each), the other slots
26 ms (making views only 3 ms), and `readable` across all callers 56 ms (67,577 calls, 0.8 µs).

**Why so many tables are made:** `fresh_tables` belongs to one list's draw state and is cleared with its dynamic
slots in `tables_done`; the title records ~203 lists a frame, so the same tables are made again list after list.
Sharing made tables between lists (slots given back when their last user has run; `readable` still per list)
would save an estimated 60–70 ms of CPU a frame.

## The new diagnostics lines

All behind `feature = "diagnostics"`, no change in behaviour:

- `refresh flushes per frame, inside:` — fan-outs and their wall time; nothing-new checks, updates and texture
  refreshes on the threads beside; checks in front (`host.rs`, `Batch::refresh_spans`).
- `list calls translated per frame by kind` — calls and time by op (`list/thin.rs`, `translate`).
- `binds per frame` — the whole bind, textures picked by index, descriptor tables, title memory by address (`draw.rs`).
- `descriptor tables per frame` — the paths above and hashing (`draw.rs`, `tables`).
- `descriptor slots per frame` — slots of tables made, `make_at`, `readable` everywhere (`draw.rs`).

The timers cost little but not nothing: instrumented runs read 14.3–16.5 fps against 15.7–16.8 without them,
inside the run-to-run spread; compare a change against a run with the same timers.
