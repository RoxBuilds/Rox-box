# Tuning notes for low-memory GPUs and hosts

These notes cover compatibility and memory-budgeting switches for 8 GB /
32-lane GPUs (including those with a ~1,000,000 shader-visible view heap limit)
and hosts with <= 24 GiB RAM, while preserving 64-lane behavior behind
`VICTORY_*` switches.

## CPU & exception emulation fixes

- **Unaligned `monitorx`/`mwaitx` patching.** Wait sites crossing 16-byte
  cache-line boundaries are patched safely byte-by-byte (`VICTORY_PATCH_WAITS=aligned`
  restricts to 16-byte-aligned sites).
- **Profiler thread-handle lifetime.** Thread handles in `VICTORY_PROFILE` remain
  open until profiling finishes so Windows does not recycle closed handles onto
  unrelated DirectStorage files mid-run.

## 32-lane GPU & heap limits

- **Descriptor-table exhaustion prevention.** GPUs capped at 1,000,000 SRV/CBV/UAV
  views allocate 2^18 dynamic slots; slots recycle immediately on fence landing
  (`VICTORY_TABLES_AT_LANDING=0` reverts) and wait up to `VICTORY_DYNAMIC_WAIT_MS`
  (default 50 ms).
- **Wave64 -> Wave32 shader translation guards.** Compute shaders using wave-wide
  operations and barriers inside divergent branches are handled via checked
  wave-pair meetings (`VICTORY_WAVE_WIDE_LAST`, `VICTORY_WAVE_PAIR`), lane-serial
  execution (`VICTORY_WAVE_SERIAL`), strict EXEC tracer guards
  (`VICTORY_TRACE_GUARDS`), and barrier validation (`VICTORY_WAVE_BARRIER_CHECK`).
  Any shader still requiring 64-lane lockstep on a 32-lane adapter is refused
  at translation (`VICTORY_WAVE64_REFUSE`) instead of crashing the driver.
- **Typed view kinds via descriptor tables.** Runtime stride/kind metadata table
  bound at `t252` (`SLOT_TABLE_STRIDES`, toggled by `VICTORY_TABLE_VIEW_KINDS` and
  `VICTORY_DYNAMIC_STRIDES`) supplies integer vs float types for table-indexed
  loads.

## Console-profile & Direct3D 12.X host fixes

- **Multi-plane presentation.** Swap chains with two present planes composite
  together in `crates/victory-gpu/src/host/planes.rs` (`VICTORY_PRESENT_PLANES=0`
  uses first plane only).
- **Working-set memory reporting.** `XMemGetWorkingSetStatistics` reports
  tracked guest pages rather than total host process commit (`VICTORY_TITLE_USED=process`
  reverts).
- **Committed texture aliasing barrier guard.** Skips aliasing barriers on
  committed textures where D3D12 rejects them (`VICTORY_ALIAS_ALWAYS=1` reverts).
- **Frequent CPU-rewritten R8/movie textures.** Uploads rewritten planes per
  draw (`VICTORY_MOVIE_PLANES`, `VICTORY_TEXTURE_STREAK=0` reverts).
- **Explicit UAV clears.** Supports `ClearUnorderedAccessViewUint`/`Float` calls
  used by upscalers (`VICTORY_CLEAR_UAV=0` reverts).
- **Optional external bake detection & exact uploads.** `VICTORY_BAKE_EXACT`
  refreshes sampled textures before bake draws; `VICTORY_BAKE_DETECT=shape`
  detects square multi-target atlases structurally, or uses hashes configured via
  `VICTORY_SHADER_PROFILE`.

## Low-RAM (<= 24 GiB) & 8 GB VRAM memory management

On hosts with <= 24 GiB RAM and 8 GB VRAM, host-visible mapped buffers count
against the shared-memory budget:

- **Finer memory windows:** `VICTORY_WINDOW_KB` defaults to 1024 KiB on `<= 24 GiB`
  hosts (4096 elsewhere).
- **Idle eviction & budget governor:** `VICTORY_RAM_KEEP`, `VICTORY_POOL_TRIM`,
  `VICTORY_TEXTURE_BLOCK_MB=16`, `VICTORY_STAGING_BLOCK_MB=16`,
  `VICTORY_LET_GO_SLICE_MS=1`, `VICTORY_SPARSE_WINDOWS`, `VICTORY_BUDGET`,
  `VICTORY_SCRATCH_DECAY_MS`, and `VICTORY_RESIDENCY_PRIORITY`.
- **Pageable CPU backing copies:** `VICTORY_CPU_SIDES=plain` stores CPU-side
  snapshot copies in pageable memory instead of shared D3D12 upload heaps when
  RAM `<= 24 GiB`.
- **Lazy host command lists & graceful OOM recovery:** `VICTORY_LAZY_LISTS`,
  `VICTORY_RESET_AT_LANDING`, `VICTORY_LIST_RESERVE`, `VICTORY_OOM_RETRY_MS=250`,
  and `VICTORY_OOM_FATAL=1` prevent fatal list/reset OOM errors during transient
  pressure spikes.
- **Whole-buffer SHA digesting:** `VICTORY_WHOLE_DIGEST` skips redundant uploads
  for unwatched ranges whose 64 KiB chunks have not changed.

## Textures picked by index on a card smaller than the title's textures

When the copies the title's streamed textures want come to more than the card
(an 8 GB card at 1080p), a texture shown with fewer levels is a small defect
and one not shown at all a large one. The defaults below are on for cards
whose budget is 8 GiB or less and off on larger ones; each `=0` restores the
behaviour before it.

- **Fewer levels as the card fills** (`VICTORY_TEXTURE_DETAIL`; `0` off, `card`
  the card's band only, anything else also the PC's commit). In the band of
  10 % of the budget below the texture queue's line, new host copies leave out
  their largest levels step by step (at most 2048, 1024, 512, 256, then 128
  texels a side); past the line, as before, only the smallest levels
  (`VICTORY_TEXTURE_TAIL`). While the commit guard is critical new copies have
  at most 1024 texels a side. Slots so shown are held and looked at again when
  the band allows more levels, and then given a copy with them. Views count
  from the copy's first level as other reduced copies do
  (`VICTORY_BASE_VIEWS`, `descriptor/host.rs` `from_base`).
- **Headroom under the budget** (`VICTORY_CARD_HEADROOM_PCT`, 4 on small cards;
  0 to 20). The queue also stops while the system's own count of our card use
  (with what is kept for targets and buffer copies) is past the budget less
  this share: the queue's measure counts the free bytes inside our blocks of
  textures as free, and those were up to about 1 GB.
- **Idle copies cut down** (`VICTORY_TEXTURE_CUT`; unset: as
  `VICTORY_TEXTURE_DETAIL`). When the sweep is to free card memory and the
  copies nothing holds are not enough, whole copies that only slots hold and
  nothing asked for during the sweep's idle time are replaced by copies of
  their levels of at most the band's side (256 texels at the least), up to 64
  a look; the slots show the old copy until the new one is uploaded.
- **Small textures while the card is full** (`VICTORY_SMALL_WHEN_FULL_KB`, 64 on
  small cards). A texture of which no smallest-levels copy can be made (one
  level, 64 texels a side or less, texture objects) gets a whole copy past the
  line if it is this small, instead of waiting for room.
- **Buffers of copies of views' own bytes used again**
  (`VICTORY_VIEW_BYTES_REUSE`, on everywhere). Copies of buffer views off their
  copy's elements come and go with ring positions (tens to hundreds a frame);
  each was a buffer of its own, made resident and waited for until mapped.
  Their buffers are whole 64 KiB (what a committed buffer took anyway), and a
  buffer let go of is handed to another copy only once every submission begun
  or queued then has finished and the bury lag after it (`VICTORY_BURY_LAG`),
  as `retire` would have dropped it; at most 16 MiB of them, 2 s after that.
  Few came back that way: more were let go of during that wait than the
  16 MiB keeps, the oldest (nearest their turn) first.
- **Copies of views' own bytes cut from blocks** (`VICTORY_VIEW_BYTES_POOL`, on
  everywhere). A copy of up to 64 KiB is a range of a block of 1 MiB of the
  same memory instead of a buffer of its own: no buffer made, nothing made
  resident or waited for, a few hundred bytes taken instead of 64 KiB. A range
  begins on a multiple of 768 bytes (16, 256 and every element a typed view
  has, 12 bytes among them), so views take it as they took a buffer's start;
  the last 64 KiB of a block are never handed out. A range goes back when its
  copy goes (slots hold such copies themselves, as they held a buffer), and is
  handed out again after the same wait as a buffer above; a block empty for
  2 s goes, past the first empty one. Longer copies are buffers of their own.
  The look for copies to let go of walks every copy kept; it runs once in 64
  new copies (`VICTORY_VIEW_BYTES_SWEEP`; `1`: at every one, as before).
- **Relief at red keeps the scratch the lists had lately**
  (`VICTORY_SCRATCH_RELIEF=idle`, the default; `floor`: as before). While the
  PC's memory is at red, the governor's look once a second let go of spare
  list scratch above its floor however lately a list had it, and the next
  frame made it again. Now it lets go only of chunks no list has had for 3 s;
  a relief after a refused allocation is as before. The line "the lists' spare
  scratch let go of by reliefs" says the MB let go of and the chunks made
  again within 2 s of a relief.
- **A list's texture copies read from its scratch**
  (`VICTORY_LIST_UPLOAD_SCRATCH`, on everywhere): the upload memory a copy
  into a texture reads (and the zeros of new levels) lies in the list's
  scratch chunks where it fits one, not in an upload buffer made for each.
- The `new memory waited for` lines split upload buffers (the lists' scratch
  chunks, one per list copy, scratch asks longer than a chunk, blocks of
  texture staging, other) and shared memory (windows of title memory, copies
  of title buffers, other).
- The lines `new memory waited for` and `new card memory waited for until it
  is mapped` now say what the memory was (blocks of textures, textures of their
  own, depth twins, upload buffers, copies of views' own bytes, other buffers
  of shared memory, card buffers),
  and the texture queue's count of slots "waiting for their memory" no longer
  includes slots uploaded and waiting for a look by fingerprint (the line
  `textures picked by index on a filling card` gives those).

## Submission & CPU overhead controls

- **Background shader optimization control:** On `<= 24 GiB` RAM or `<= 8 GB`
  VRAM systems, D3D12 background shader threads are disabled when Developer Mode
  is active (`VICTORY_D3D12_BACKGROUND`) or pinned to efficiency cores at idle
  priority (`VICTORY_BACKGROUND_SLOW`).
- **Thread synchronization & lock contention:** Per-thread window caches
  (`VICTORY_LOOSE_FOUND`), relaxed lock scopes (`VICTORY_WINDOW_MAKE_LOCKED`,
  `VICTORY_TEXTURE_AT_SHARED`, `VICTORY_UNFOLD_LOCKED`), background frame telemetry
  (`VICTORY_FRAME_LINE_ASIDE`), dedicated sweep threads (`VICTORY_SWEEP_ASIDE`),
  and APU scratch reuse (`VICTORY_APU_SCRATCH`).
- **Console wait timer emulation:** `VICTORY_MWAITX_WAIT=1` turns spinning
  `monitorx`/`mwaitx` loops into timed waits (`VICTORY_MWAITX_WAIT_US=250`).
- **Card-compare coalescing & TDR guards:** `VICTORY_CARD_JOIN=<k>`,
  `VICTORY_WAVE_GUARD`, and `VICTORY_WAVE_PAIR_ROUNDS=4096` reduce GPU hang risk
  and redundant card-side memory comparisons.
- **Title bytes of memory nothing watches, under flights' writes**
  (`VICTORY_UNDER_UNWATCHED_MB=<n>`, off by default; try 32 in an A/B run).
  Ranges of such memory that flights' shaders may write, and that the next
  submission refreshes, are compared whole up to n MiB a submission instead
  of waiting for those flights ([findings](findings.md#one-run-on-a-small-card-and-a-pc-short-of-memory)).
  If it does what it is for, the report's `[frame_time.waits.steady]`
  `title_memory_changed_under_shader_writes` and `[frame_time.steady]`
  `waiting_for_card_ms` fall by most of the former, the log's "left to a wait
  for the flights" counts of memory not watched fall to near 0, and "compared
  whole" shows the ranges and MB that took their place. Where that line
  counts many ranges left for "more changed runs than a submission sends
  up" instead, `VICTORY_UNDER_RUNS=<n>` raises that limit (4,096 with card
  steps, as before when unset).

## Display, timing & pre-warming

- `VICTORY_MODE_120=1` exposes a 120 Hz display mode alongside standard modes.
- Alt+Enter toggles borderless 16:9 presentation cleanly without exclusive
  fullscreen mode switches.
- `victory warm --used --settle` pre-compiles pipelines and records a stamp in
  `<cache>\pipelines\warm-stamp.txt` checked by `scripts/play.ps1`.
- Optional user-supplied binary patches (`VICTORY_PATCHES_FILE` / `[paths].patches_file`)
  and external shader profiles (`VICTORY_SHADER_PROFILE` / `[paths].shader_profile`)
  enable per-title overrides cleanly outside the codebase.

These five were written from one run's report and log and are tested on the
software adapter against a made-up budget; **no run of a title has confirmed
them yet**. What a run's report and log should show if they do what they are
for:

- the report's `[memory.pressure]` `resident_waits` and `mapped_waits` (and
  their ms) fall sharply, and the log's wait lines name few "copies of views'
  own bytes"; the view-bytes line's count of copies "made on a buffer let go
  of before" is most of the copies made;
- `[frame_time.steady]` `preparing_ms` falls with them;
- `[memory]` `vram_peak` stays under `vram_budget_at_peak`, and
  `[memory.pressure]` `vram_over_budget` is false;
- `[textures]` `waiting_for_memory_most` is far lower (part of that is the
  corrected count), `smallest_levels_only` grows more slowly, and the log's
  detail line counts copies "made with fewer levels" and "cut down while
  idle".

If a run shows otherwise, switch them off one at a time (`=0`) and compare
two reports with `victory report --diff`.

One run of a title on an 8 GiB card with 16 GiB of RAM has since been read.
It confirmed the card's side: `vram_peak` stayed under `vram_budget_at_peak`
and `vram_over_budget` was false. It did not confirm the rest:
`resident_waits` stayed in the tens of thousands, because the wall on that PC
was not the card but the PC's own memory (the budget governor at red for most
of the run, free RAM down to a few hundred MiB).

## When the PC's memory is the wall

A PC with 16 GiB of RAM can run out of memory before the card does. The
report's `[memory.pressure]` says so (`governor_red_s`, `lowest_free_ram_mib`),
`[memory.map]` says where the process's memory is by our own categories, and
`[memory.views]` counts the views of title pages. These switches are aimed at
it; all were ported from other projects of the same family (credited where
they are read, and in `THIRD_PARTY_NOTICES.md`) and **none has been confirmed
by a run of a title here yet**:

- `VICTORY_MAP_RUNS` (on by default with 24 GiB of RAM or less; `=0` off): a
  run of consecutive title pages is mapped as one view, not a view a page.
  `[memory.views]` `title_view_pages_avg` should be above 1 and
  `title_views_mapped` lower.
- `VICTORY_OWNERLESS_VIEWS=<n>` (off): at most `n` table entries of views no
  resource releases are kept (1048576 is the figure it was measured with
  elsewhere). `ownerless_entries_kept` stops growing.
- `VICTORY_LAZY_VIEWS=1`, `VICTORY_SOFT_VIEWS=copies` (off, experimental):
  views of title pages are kept in our books and mapped only when touched.
  `book_views_let_go_untouched` against `book_views_made_real` says whether
  it saves anything.
- `VICTORY_WC_NO_BACK=1` (off): what shaders write into large copies of
  write-combined title memory stays on the card. `[frame_time.spared]`
  `shader_writes_left_on_card_mb` counts it; `copying_back_ms` and the
  `title_memory_changed_under_shader_writes` wait should fall.
- `VICTORY_SYNC_IN_PLACE=1`, `VICTORY_SPANS_QUICK=1`, `VICTORY_SYNC_FINE=1`,
  `VICTORY_HEAP_SYNC_FIRST=1`, `VICTORY_HEAP_SYNC_AT_SUBMIT=1` (off): the
  descriptor heaps' records are read and compared with less work and earlier.
  `[frame_time.spared]` counts each; `preparing_ms` should fall.

Turn them on one group at a time with `scripts\play.cmd ... -More "NAME=1"`
and compare two reports with `victory report --diff`.
