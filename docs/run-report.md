# The run report

Logs, screenshots and crash dumps of a run may not be posted anywhere: they
hold the title's text, names, hashes and addresses
([CONTRIBUTING.md](../CONTRIBUTING.md#clean-room-rules)). The run report is
the one thing about a run that may be shared. Every `victory run --execute`
writes a small text file that says where the run's time and failures went,
made so that it is safe to attach to a public issue **as it is**: it holds
numbers and Rox-box's own words, and nothing else.

## Where and when

`<data_dir>\reports\run-<UTC yyyymmdd-hhmmss>.txt` (a second run in the same
second gets `-2`, `-3`...). The folder is made with `Config::data_subdir`, so
it is refused inside a source tree like every other folder we write to. The
newest 20 reports are kept; older ones are removed when a run starts.

The report is written

- when the run ends: the title's entry point returns or it calls `exit`,
  `_exit`, `quick_exit`, `ExitProcess`, `abort`, `terminate` or
  `TerminateProcess` on itself; Rox-box ends it (the window was closed, or
  the title relaunched itself); an unimplemented import traps; the host GPU
  fails for good (device removed);
- on a crash (an unhandled exception): the exception filter only stores how
  the run ended and wakes the report's thread, which writes the file while the
  filter writes `crashes.log`; the filter waits for it at most 2 s;
- every 30 s while the run goes on, from a helper thread at the lowest thread
  priority. Each write goes to `<name>.part` and is renamed over the last one,
  so a process that is killed still leaves a complete report at most 30 s old
  (it says `ended = "killed_or_unknown"`).

At the end of a run one line names the file and says it is safe to share.

`VICTORY_REPORT=0` turns all of it off: no thread, no file, no host queries.
(The counters it would read are a handful of atomic adds either way.)

### The self-test's report

`victory selftest` ([selftest.md](selftest.md)) writes a report of its own
when it ends, through the same renderer and guard:
`<data_dir>\reports\selftest-<UTC yyyymmdd-hhmmss>.txt` (with no config file
the default data folder, as for a run; the newest 20 are kept, apart from
the run reports' 20). Its path is said on stderr, so that the self-test's
stdout can be pasted as it is; `VICTORY_REPORT=0` writes none. It holds
`format`, `kind = "selftest"`, `[rox_box]`, `[host]` and `[host.gpu]` as a
run report does, then:

| Key | Meaning |
|---|---|
| **`[selftest]`** | |
| `ended` | `completed`, `no_device` (no Direct3D 12 device: exit code 3), `hung` (a step had no answer from the GPU for 60 s) or `host_gpu_failed` |
| `software_adapter`, `frames`, `bench` | What it was asked: `--warp`, `--frames`, `--bench` |
| `passed`, `stages_ok`, `stages_failed`, `stages_skipped` | Whether no stage failed and it completed; the stages by outcome |
| **`[selftest.switches]`** | `VICTORY_*` variables set, by the rule below |
| **`[selftest.rates]`**, **`[selftest.rate_units]`** | With `--bench`: each rate's value, and its unit in words (`MB per s`) |
| **`[[selftest.stage]]`** | One per stage, in order: `name`, `outcome` (`ok`, `fail`, `skip`) and, for a skip, our fixed `reason` (`needs the device stage`, `needs clear + present`, `the adapter has no shader model 6.6`); for a hang or a GPU failure, the stage it stopped in, `fail`, with that as its reason |

What a stage prints (the adapter's name and driver, a failure's expected
and found values) is built at run time and is not written: the `FAIL` line
on the terminal is what says it, and it holds nothing from any title.

## `victory report`

| Command | Prints |
|---|---|
| `victory report` | The newest report, a run's or a self-test's, on stdout; then its path, on stderr (so that `victory report \| clip` copies the report alone: the path names your folders) |
| `victory report --list` | Every report kept, newest first, with how the run ended, how long it ran and its average frame rate; for a self-test, how it ended and its stages by outcome |
| `victory report --path` | The newest report's path only |

## Comparing two reports

`victory report --diff` compares the two newest reports of the newest one's
kind (older = A, newer = B: two runs, or two self-tests); `victory report
--diff <a> <b>` compares two named ones, each a path to a
report file or a number from `victory report --list` (1 = newest). It needs
no game folder, and with two paths no config file. It is for an A/B run: the
same thing run twice with one switch changed. It prints, in plain text meant
for an issue:

1. the formats (said first when they differ), then for A and B the Rox-box
   version, console, display mode, duration and how the run ended; whether
   `[host]` and `[host.gpu]` are the same, and the differing keys if not;
2. `[run.switches]` only in A, only in B, and with different values;
3. every numeric key whose change is over 5 % and over 0.005 (`--threshold
   <percent>` changes the 5; `--all` prints every key), as
   `key  A  B  delta  percent`, grouped by section: the steady state first
   (`[frame_time.steady]`, `[frame_time.waits.steady]`, the `*_steady` keys,
   when both reports have them), then the whole run. Arrays (`[timeline]`) are
   shown as `[len]`, `[mean]`, `[min]` and `[max]`; the count tables
   (`[gpu_work.left_out]`, `[shaders.failures]`, `[frames.histogram]`, the
   unimplemented methods and imports) key by key. A key only one report has
   prints `-` for the other; a key an older format lacks is such a key;
4. one `verdict:` line of figures only: steady-state frame time, average
   frame rate, draws left out per frame.

Two self-tests' reports compare the same way, with the stages whose
outcome differs after the switches, `[selftest]` and `[selftest.rates]` as
the numbers, and the rates both have in the verdict. A self-test's report
and a run's say which is which, their builds and their PCs, and nothing
more: their figures are of different things.

Only keys and values that are in the two reports are printed, so the output
holds what a report holds. The two paths go to stderr, like the one `victory
report` prints. Input that is not a report, or cannot be read, is an error
naming the argument (exit code 1), as is a `--diff` with fewer than two
reports kept.

## What it may hold: an allowlist

A report holds only

1. **numbers**;
2. **words from Rox-box's source code**: enum names, our wording of reasons,
   our names of interfaces and methods, the shader translator's own wording of
   what it cannot translate;
3. **facts about the PC** that a developer needs and that are the player's
   own: the Windows version and build, the processor's brand string and
   logical processors, memory sizes, the graphics adapter's name, vendor id,
   driver version, video memory, feature level, shader model, wave lanes,
   mesh-shader support, the host's view heap size.

It never holds anything that comes from a title or identifies one or the
player: no title id or name, no executable or DLL file names from the game
folder, no paths at all, no user or computer name, no hashes of executables or
shaders, no shader or pipeline names, no memory addresses or offsets into the
title, no text read from title memory, no command-line arguments.

Two things come close and have their own rule:

- **Unimplemented imports.** An import's name comes from the title's import
  table. It is written as `dll!function` only when the DLL is the platform's
  (one our registry serves or the default `HostPolicy` passes through) and
  both parts are plain identifiers; any other (a DLL the title ships) is only
  counted, under `imports_other`.
- **Switches.** The names of the `VICTORY_*` variables that are set are
  listed. A value is written only if it matches `^[A-Za-z0-9.,x:+-]{0,24}$`,
  looks like no file name (a dot before a letter) or hex number (`0x`, or five
  or more hex digits mixing letters and digits), and the variable is not one
  whose value can be a path or a list of hashes or offsets (`VICTORY_CONFIG`,
  `*_DIR`, `*_FILE`, `*_PATH`, `*_LOG`, `*_DUMP`, `VICTORY_SHADER_PROFILE`,
  `VICTORY_BCPACK_TABLES`, `VICTORY_TEST_TITLES`, `VICTORY_PROFILE`,
  `VICTORY_CAPTURE`, `VICTORY_RECORD`, `VICTORY_XMA_DUMP`,
  `VICTORY_AUDIO_CAPTURE`, `VICTORY_SKIP_PIPELINES`, `VICTORY_WATCH`,
  `VICTORY_POKE`, `VICTORY_CRASH_SAVE`, `VICTORY_DXIL`, `VICTORY_LOG`, and
  any name with `CAPTURE`, `RECORD`, `PROFILE`, `HASH`, `RVA`, `SAVE`,
  `FOLDER`, `_OUT`, `DXC`, `REFERENCE`, `DUMP`, `SITES`, `MATCH`, `TARGETS`,
  `TRACE`, `WATCH` or `POKE` in it). Otherwise the value is `"<set>"`.

### How it is enforced

By the types of `victory_core::report`, not by care at each call:

- No public function takes a runtime `String`. Text goes in as
  [`Ours`](../crates/victory-core/src/report.rs) (a `&'static str`: a literal
  of our source), as `HostText` (made only by `HostText::new`, which replaces
  unusual characters and withholds anything longer than 48 characters,
  path-like, or with a run of eight or more hex digits), or through the import
  and switch sanitizers above.
- Rendering is private, and every string it writes passes a last guard: one
  with a backslash, a slash, `:\`, a run of eight or more hex digits, a quote
  or more than 112 characters becomes `"<withheld>"`. In the host section a
  slash between two letters or digits is allowed (adapter names such as
  `XT/XTX`); nothing path-like is.
- Unit tests feed a path (with a placeholder user name), a 16-hex-digit hash,
  an address and a long free-form sentence through every public entry point
  and check that the rendered report holds none of them, and that it parses
  as TOML. The entry points that take numbers alone (`figures`,
  `memory_seen`, `memory_look`, the counters) are fed an address, and the
  sections they fill (`[frame_time]`, `[memory.pressure]`, `[memory.views]`,
  `[memory.map]`, `[textures]`) must
  hold numbers and booleans only.

## The format

Valid TOML that reads as plain text. It opens with a comment block saying
what it holds and what it never does. Sections and keys, in order:

| Key | Meaning |
|---|---|
| `format` | The report format's version (8; 7 counted in `card_relief_freed_mib` the blocks of textures made ahead that relief let go and that were made again at once, and had no `card_relief_in_blocks_mib`, `card_compact_blocks_released` or `card_compact_returned_mib`; 6 left the card's memory in use and the host lists' driver objects out of `[memory.map]`'s `accounted_total_mib`, so its `unaccounted_mib` held them, and had no `card_in_use_mib`, `host_lists_driver_estimate_mib`, `card_texture_blocks_free_mib`, `host_lists_idle`, `card_relief_looks` or `card_relief_freed_mib`; 5 had no figures of compute pipelines in `[gpu_time]` and no `asked_to_leave_out`; 4 had no `[gpu_time]`; 3 had no `[memory.map]`; 2 had no `kind`, and its `[frame_time]` left out the frames of the last part of a second, and a short run's `[frame_time.steady]` was all zeros; 1 had no `[frame_time]`, `[memory.pressure]`, `[textures]`, `first_draw_s`, `left_out_avg_steady` or `pipelines_gone_without`) |
| `kind` | `run` (`selftest` for [the self-test's report](#the-self-tests-report)) |
| **`[rox_box]`** | |
| `version`, `diagnostics` | Rox-box's version; whether it is a build with the `diagnostics` feature |
| **`[host]`** | |
| `windows`, `windows_build` | `RtlGetVersion`'s major.minor and build |
| `cpu`, `logical_processors` | The processor's brand string (CPUID); the logical processors this process may use |
| `ram_gib`, `free_commit_gib_at_start` | Installed memory, rounded; commit memory free when the run began |
| **`[host.gpu]`** | The adapter the run's host device was made on (absent until the title made its device) |
| `adapter`, `vendor_id`, `driver` | `DXGI_ADAPTER_DESC1`'s description and vendor id; the driver's version |
| `dedicated_vram_gib`, `software_adapter` | Dedicated video memory, rounded; whether it is a software rasteriser |
| `feature_level`, `shader_model` | The highest of each the device supports |
| `wave_lanes_min`, `wave_lanes_max`, `mesh_shaders` | `D3D12_OPTIONS1` and `OPTIONS7` |
| `view_heap_views` | Views a host shader-visible heap holds (`descriptor::host::probe_view_limit`) |
| **`[run]`** | |
| `console`, `display_mode` | The console model and the display mode the title is told of |
| `duration_s` | From the start of the title to this report |
| `first_draw_s` | When the first present came that had draws run on the host before it |
| `ended` | `title_exited` (with `exit_code`), `title_aborted`, `title_ended_by_rox_box`, `host_device_removed` (with `removed_reason_hresult` and `removed_reason`), `host_gpu_failed`, `crash` (with `crash_in`: `title_code`, `rox_box_code`, `host_dll`, `outside_any_module`; and `exception`: its kind), `unimplemented_import_trapped`, `rox_box_error`, or `killed_or_unknown` |
| `rox_box_panics` | Panics of ours during the run |
| **`[run.switches]`** | `VICTORY_*` variables set, by the rule above |
| **`[frames]`** | |
| `presents`, `first_present_s` | Presents, and when the first came |
| `average_fps`, `frame_ms_mean` | Over the presents' intervals |
| `frame_ms_p50`, `_p90`, `_p99`, `_max` | Present-to-present time percentiles, from buckets of 0.25 ms (to 100 ms) and 10 ms (to 2 s) |
| **`[frames.histogram]`** | Frames by present-to-present time, buckets at 120, 90, 60, 45, 30, 20, 10, 4 and 1 frames a second |
| **`[timeline]`** | Per window since the start (10 s; merged into longer windows to show at most 180): `presents`, `fps` (the last window's over the time the run spent in it, at least 1 s), `worst_frame_ms`. Loading and warm-up come first, then the steady state |
| **`[gpu_work]`** | Per frame (between two presents; the first present's loading is left out), average and most: `submissions`, `draws` (with mesh dispatches) and `dispatches` run on the host, and those `left_out`; `left_out_avg_steady`, the same over the steady state (below); `submissions_total` |
| **`[gpu_work.left_out]`** | Draws and dispatches the title recorded and the host did not run, the whole run, by reason: `pipeline_not_ready` (not made within the frame's wait budget), `shader_not_translated`, `pipeline_refused` (by the host), `out_of_memory` (constants, scratch, vertices refused), `no_descriptor_slots`, `indirect_not_bounded`, `ray_tracing_builder_off`, `no_pipeline_set`, `asked_to_leave_out` (a shader of its pipeline is named in `VICTORY_LEAVE_OUT`), `other` |
| **`[frame_time]`** | What a frame cost, per frame over the whole run, from the meters of the log's once-a-second frame line (["Reading the frame figures"](#reading-the-frame-figures)) |
| **`[frame_time.steady]`** | The same over the steady state, which begins at `from_s`; a comment and no figures when the run was too short for one ([below](#reading-the-frame-figures)) |
| **`[frame_time.waits]`**, **`.steady`** | Per frame, ms the submitting thread waited for the card in settles of submissions sent ahead, by what settled them |
| **`[frame_time.spared]`**, **`.steady`** | Per frame, work the unified-memory synchronisation switches spared (all 0 with them off; [below](#reading-the-spared-work)) |
| **`[frame_time.moved]`**, **`.steady`** | Per frame, ms of work moved off the threads a frame waits for (all 0 with the switches off; [below](#reading-the-moved-work)) |
| **`[gpu_time]`**, **`[gpu_time.steady]`** | Per frame, the card's own time on the host queue by kind of work (the title's draws and dispatches, each kind of bookkeeping of ours), what our work moved, and the memory the budget governor saw meanwhile; the steady state as `[frame_time.steady]`'s ([below](#reading-the-gpu-time)) |
| **`[pipelines]`** | `created_by_title`; host pipelines `host_made_now` (compiled during the run) and `host_from_library`; `host_failed` |
| **`[shaders]`** | Translations `translated_now`, `from_cache`, `failed` |
| **`[shaders.failures]`** | Failed translations by the translator's own kind (`not_a_container`, `malformed`, `native_code`, `unsupported`) and wording |
| **`[memory]`** | In MiB: `process_commit_peak`, `working_set_peak`; `vram_peak` and `shared_peak` with the budget when each was reached (`QueryVideoMemoryInfo`, as the budget governor samples it); `out_of_memory_retried_ok` / `_failed`; `governor_level_changes`, `governor_worst_level` |
| **`[memory.pressure]`** | How short memory ran: the lowest free memory and commit headroom, time at each governor level, the card past its budget, the commit guard, the waits for new memory ([below](#reading-memory-pressure-and-textures)) |
| **`[memory.views]`** | The runtime's views of title pages and the descriptor table's entries of views without a resource, since the start ([below](#reading-memoryviews)) |
| **`[memory.map]`**, **`[memory.map.peak]`** | Where the process's memory is, by our own categories, in MiB, as of the report (sampled each time it is written) and the most each held ([below](#reading-the-memory-map)) |
| **`[memory.view_copies]`** | Copies of buffer views' own bytes (views that begin off their copy's elements) since the start: `cut_from_blocks` (`VICTORY_VIEW_BYTES_POOL`), `own_buffers_made` (each a new buffer of the PC's memory the card shares, made resident and waited for); and the blocks they are cut from now, `blocks` and `blocks_mib`. With the pool on, `own_buffers_made` is the few copies longer than 64 KiB; with it off, every copy not made on a buffer let go of before. The blocks' bytes no copy holds are counted in `[memory.map]`'s `title_buffer_copies_mib` |
| **`[textures]`** | Textures shaders pick from the view heap by index: uploads, slots waiting for their memory, a full card; texture refreshes per frame ([below](#reading-memory-pressure-and-textures)) |
| **`[unimplemented]`** | `methods_called` (distinct), `methods_lost` (calls that found the table full), `pipelines_gone_without` (calls of the device's pipeline creation methods that are not implemented, `DeserializeGraphicsPipelineStateX` and `DeserializeComputePipelineStateX` at 0x1c8 and 0x218: each a pipeline the title did not get that way), `imports_named`, `imports_other` and `imports_other_calls`; then one `[[unimplemented.method]]` per D3D12.X method not written yet that was called, those two included (`interface`, `offset` in its function table, `calls`) and one `[[unimplemented.import]]` per named import (`name`, `calls`) |

### Reading the frame figures

`[frame_time]` holds what the log of every build (not only `diagnostics`)
says once a second of the frames since its last line, added up over the run
and divided by the frames the frame meter counted (presents the swap chain
took). The line takes its meters and sets them to zero, so the report is fed
where the line takes them (`victory-gpu/src/runreport.rs`, `frame_line` and
the functions after it) and the log says what it said before. The line comes
once a whole second has passed; what the meters hold of the run's last part
of a second is taken once more when the report that names the run's end is
written (`last_meters`), with no line in the log, so `frames` is every
present the swap chain took (a run's first present only starts the meter).

**The steady state** is the frames from the first 10 s window at or after a
third of the time from the first present to the report (`from_s`): loading
and first-time work (shaders, pipelines, textures made for the first time)
fall in the first third. It is by time, not by what was drawn, so a run that
ends in a long loading screen has some of it in its steady state. Compare the
two: a figure much higher over the whole run is warm-up; one as high at steady
state is what every frame pays.

A run with no whole 10 s window after that point (under 30 s from its first
present) is a short run, and its steady state is the last two thirds of its
frames by time at the frame meter's resolution: the meter's readings (one a
second, and the last one when the report is written) begun at or after
`from_s`, the first whole second from the first present at or after a third
of the time from it; a comment line says so. A run too short for any such
reading (the demo title's 40 frames; any run of a second or so, which has one
reading of all of it) has no steady state:
`[frame_time.steady]` holds a comment saying why and no figures, and
`left_out_avg_steady`, `refresh_ms_steady` and `[frame_time.waits.steady]`
are left out (rather than written as zeros).

| Key | From (the frame line's words) | A high value means |
|---|---|---|
| `frames`, `frame_ms` | The meter's frames and the time its windows lasted | `frame_ms` is the frame meter's own ms per frame (presents the swap chain took), close to `frame_ms_mean` |
| `preparing_ms` | "N preparing": the submitting thread's work on a submission before it goes to the card | The CPU side of submissions is what limits the frame rate; see the two below for where it goes |
| `copies_up_ms`, `copies_up_mb` | "copies N ms N MB": title buffers copied up to the card | The title rewrites much memory each frame that the card reads; MB per frame shows the volume, ms what it costs |
| `texture_refresh_ms`, `texture_refresh_mb` | "textures N ms N MB": textures read, hashed and detiled on the submitting thread | Textures change or are looked at again each frame; compare with `[textures].refresh_ms`, which counts all threads |
| `waiting_for_card_ms` | "N on the GPU": the submitting thread's waits for the card in submissions | The card (or a wait we impose) holds the submitting thread; read with `card_busy_pct` and `[frame_time.waits]` |
| `copying_back_ms`, `copying_back_mb` | "N copying back N MB": what shaders wrote, compared and copied back into title memory | Shaders write much memory the title reads on the CPU |
| `card_busy_pct` | "the card about N% busy" (the card's clock): 100 less the share of time nothing of ours was queued | Near 100: the card is the limit. Low with a slow frame: the card waits for us or the title |
| `card_idle_ms` | "nothing of ours queued N ms" | Time the card had nothing to do |
| `card_idle_behind_us_ms` | "N ms of that while the submitting thread was working" | The card stood idle behind our own CPU work (preparing, copying back): our overhead, not the title's |
| `submitting_thread_stood_ms` | "the submitting thread stood waiting for the card N ms" | The submitting thread blocked on the card; with a low `card_busy_pct`, the waits themselves are the cost |
| `heap_buffer_passes`, `heap_buffer_ms` | "heap buffers N passes N ms": passes over the view heap for shaders that pick views by index; with the buffer views' pass on a thread beside (the default) the ms are its texture side, which takes a share of the frame (`VICTORY_TEXTURE_STAGE_SHARE`); the line splits them | Many or slow passes: big heaps rescanned often, or many textures made and staged; a tenth of the frame is the share at work |

`[frame_time.waits]` (and its `.steady`) splits the waiting in settles of
submissions sent ahead (`VICTORY_LATE_WAIT` or the relay; all 0 when
submissions are not sent ahead), from the line
"of the waiting for the card in settles, per frame, by what settled", ms per
frame. The cause with the most ms is what makes the submitting thread wait:
`sync_submission_or_present` (a submission or present that must finish
first), `card_had_run_them` (the card was done; little is lost), `wait_for_memory_value`
(the title waits for a value the card writes), `title_memory_changed_under_shader_writes`
(the title wrote memory shaders in the air write too), `title_memory_read`
(the title reads memory the card may still write: buffer copies, upload
fills, textures), `step_of_ours`, `lists_in_the_air` (more lists sent ahead
than `VICTORY_LATE_WAIT_LISTS` lets fly), `bottom_of_pipe_writes`
(`VICTORY_LATE_WAIT_WRITES`), `blocking_fence_wait` (the title blocks on a
fence).

### Reading the GPU time

`[gpu_time]` says what the card spent its own time on, in every build
(`VICTORY_GPU_SPLIT`, on by default; `host/gpu_split.rs`). A list writes a
GPU timestamp (`EndQuery` on a TIMESTAMP query heap) where a run of commands
of one kind begins: the thousands of draws of a frame cost one timestamp each
time the kind of work changes, not one each. A small list of ours at the end
of each submission writes one more and resolves them all into a read-back
buffer, which is read once that submission has landed (where the host already
learns it): nothing waits for the card to read them, and no command moves.

A run's time is the difference between its timestamp and the next one: when
the card reached the one point and when it reached the other. It holds the
run's work, the waits of its barriers, and any time the card waited inside
those commands for memory to be paged in or made resident (the card's memory
paged out to the PC's, or the PC's own memory paged to disk). Time the card
was idle between submissions is in no run, so `measured_ms` is at most the
card's busy time ([`[frame_time]`](#reading-the-frame-figures)'s `frame_ms`
times `card_busy_pct`); much less than it means work outside any run (see
below). Each second the frame line takes what landed in it, so a submission is
counted in the second it landed, divided by the frames the frame meter counted
then.

| Key | What |
|---|---|
| `split` | `on`; `off` (`VICTORY_GPU_SPLIT=0`: nothing recorded, no query heap); `refused_by_driver` (the queue gave no timestamp frequency or the heap could not be made: logged once, then off); `not_started` (no list has marked a run yet) |
| `frames`, `partly_measured_pct` | The frame meter's frames; the share of them in which some run could not be given one kind: the frame's budget of boundaries ran out (8,192 by default, 4,096 with `VICTORY_GPU_SPLIT=kinds`; `VICTORY_GPU_SPLIT=<n>` sets another, 16 to 16,384) or the heap had no slot left. Such a run is `unattributed_ms` |
| `draws_ms` | The title's draws, mesh dispatches and indirect draws (with what the list does unmarked behind them: barriers, state) |
| `dispatches_wave64_ms` | The title's dispatches of compute programs written for 64-lane waves (the console program says so). On a card whose `wave_lanes_max` is 32 these are the programs the translator gives one of its forms for 64-lane waves (meetings at group barriers, halves meeting by themselves, lane-serial, or the approximate form its guard falls back to); the host pipeline does not keep which form, so this is all of them |
| `dispatches_other_ms` | The title's other dispatches |
| `title_other_ms` | The title's copies, clears, resolves and texture read-backs |
| `fills_ms` | Ours: the steps done on the card instead of waiting for flights (`VICTORY_CARD_STEPS`: fills of constant buffers, buffer copies) |
| `hashes_ms` | Ours: the hash passes over copies in the card's memory after ranges were sent up |
| `compares_ms` | Ours: the compare passes after a submission's lists |
| `copy_backs_ms` | Ours: the fetches of pieces shaders wrote in copies in the card's memory (on the drawing queue; with `VICTORY_RELAY_COPY=1` they go over the copy queue and are not measured), and textures copied back at a list's close |
| `uploads_ms` | Ours: what changed of copies in the card's memory sent up to them |
| `texture_uploads_ms` | Ours: staging buffers copied into textures |
| `other_passes_ms` | Ours: twin copies and depth planes, the state preludes, the clamps of indirect arguments, the ambient stand-in, the planes of a present put together, the present's copy |
| `unattributed_ms` | Runs holding work of more than one kind (see `partly_measured_pct`) |
| `title_ms`, `ours_ms`, `measured_ms` | The title's four kinds, our seven, and every kind with the unattributed |
| `fills`, `fills_mb` | Fills done on the card per frame, and their bytes |
| `hash_passes`, `hashed_mb` | Hash passes per frame, and the bytes they read |
| `compare_passes`, `compared_mb` | Compare passes per frame, and the bytes they read |
| `copied_back_mb` | Bytes of the pieces fetched from copies in the card's memory (textures copied back are not in it) |
| `upload_ranges`, `uploaded_mb` | Ranges sent up to copies in the card's memory per frame, and their bytes |
| `texture_uploads_mb` | Bytes of staging buffers copied into textures per frame |
| `governor_looks` and the `*_mib_*` keys | What the budget governor saw at its looks (about one a second, from the asks it makes anyway; none with `VICTORY_BUDGET=0`) while those frames ran: the card's own memory in use (average, peak) and its budget (average), the same of the shared memory (the PC's memory the card uses), free physical memory (average, lowest) |
| `looks_card_over_budget`, `looks_free_ram_under_512_mib` | Looks that found the card's memory in use past its budget, and free physical memory under 512 MiB |
| `pipeline_seconds`, `compute_pipelines_a_second_avg` | With the title's dispatches timed by pipeline (`VICTORY_GPU_SPLIT`'s default `pipelines`): the frame line's seconds that had any, and how many compute pipelines ran in such a second on average. No pipeline is named: the log's table names them |
| `costliest_pipeline_ms`, `costliest_5_pipelines_ms`, `costliest_20_pipelines_ms` | Per frame, the card's time of the costliest 1, 5 and 20 compute pipelines of each second, summed over the seconds: how much of `dispatches_wave64_ms` plus `dispatches_other_ms` a few programs take |

**Telling the suspects apart.** Read the steady state, and two runs with
`victory report --diff` where one thing changed:

- **Our bookkeeping** is `ours_ms` against `measured_ms`, and which of its
  kinds holds it. The volumes say why: `hashed_mb / hashes_ms` and
  `compared_mb / compares_ms` are MB per ms (GB/s) of those passes. Near the
  card's bandwidth, the volume is the cost (fewer or smaller ranges would
  help); far under it with many `hash_passes`, each pass's fixed cost is
  (fewer, joined passes would help). A small `ours_ms` clears our bookkeeping.
- **The title's compute in the 64-lane forms** is `dispatches_wave64_ms`
  against `dispatches_other_ms` and `draws_ms`, and it stays as large in a run
  with little memory pressure; a run with the forms changed
  (`VICTORY_WAVE_GUARD`, `VICTORY_WAVE_SERIAL`) moves it alone.
- **The card's memory paged** shows as every kind growing together while the
  volumes stay the same (the rates in MB per ms fall), with
  `looks_card_over_budget`, `card_used_mib_avg` at `card_budget_mib_avg`, much
  `shared_used_mib_avg`, a low `free_ram_mib_lowest` and
  `looks_free_ram_under_512_mib` above 0. Compare the whole run with the steady
  state, or a run with more free memory: time that falls with the pressure,
  work the same, was waiting for memory.

What it costs: at each change of kind in a list, a load, a lock and an
`EndQuery`; a submission, one list of ours more (one `EndQuery` and a
`ResolveQueryData` per run of consecutive slots); a second, reading what
landed. On the card each timestamp may let the work before it drain before
it is written (some microseconds each, up to the budget a frame). A 1 MiB
query heap and a 512 KiB read-back buffer are made at the first mark.

### Reading the memory map

`[memory.map]` says where the process's memory went by our own categories,
each in MiB, as of the moment the report was written (it is rewritten every
30 s and at the end; the last one is the run's end). `[memory.map.peak]` has
the most each category held at any of those moments, so a peak between two
writes is missed. A category nothing counted is left out (not known); one
counted and empty is `0`. Nothing here is a name, an address or a path.

| Key | From | Exact? |
|---|---|---|
| `process_private_commit_mib` | The process's private commit now (`PagefileUsage`) | Exact |
| `accounted_total_mib` | The sum of the private categories and the charged ones below that are known (format 7; a 6 added the private ones alone) | Sum of the figures; each shown rounded to a MiB, so the shown parts may differ from it by a MiB or two |
| `unaccounted_mib` | `process_private_commit_mib` less `accounted_total_mib` (0 if the categories add up to more): what the map does not explain, such as the driver's other objects, the title's own heaps (its `HeapAlloc`s and stacks, apart from the pages it committed) and allocations we do not count | The difference |
| `card_in_use_mib` | Charged: the card's own memory in use by the process, as the system counts it (`QueryVideoMemoryInfo`, local group, the budget governor's last look; left out while the card gives no budget). Every allocation in the card's memory is charged 1:1 to the process's private commit (measured on one PC and driver: a 512 MiB buffer there added 516 MiB of private usage, a 256 MiB texture 256 MiB), so it is in the total; the `card_*` keys beside are a breakdown of it, never added. Past the budget the system moves the overflow into the PC's memory, which is where free memory goes | Exact as the system counts it |
| `host_lists_driver_estimate_mib` | Charged: `host_lists_alive` times 0.8 MiB, what one host list (its command allocators and lists) measured to cost of the private commit whatever it recorded | Estimate |
| `title_committed_mib` | Host pages the title committed through its `VirtualAlloc`s, each counted once, less what it freed (the figure the title is told it uses). Left out with `VICTORY_TITLE_USED=counter` or `=process` | Exact |
| `title_default_blocks_mib` | Blocks the title took from the default heap | Exact |
| `title_buffer_copies_mib` | Host copies of title buffers: bytes of copies alive less the windows' lengths, and the bytes of the blocks copies of views' own bytes are cut from that no copy holds (`[memory.view_copies]`) | Estimate (a copy only a list still holds, whose window is gone from the table, counts here) |
| `windows_of_title_memory_mib` | Host copies of windows of title memory outside buffers (not sparse: those hold tiles on the card) | Exact lengths; the table is read without waiting, the last figure stands if it is being written |
| `snapshot_buffers_mib` | Snapshot buffers kept for reuse (capacity) | Exact, as above |
| `list_scratch_mib` | Scratch chunks of host lists alive (held, spare and reserve) | Exact |
| `list_upload_buffers_mib` | The other upload buffers of host lists | Exact |
| `texture_staging_mib` | Blocks of staging buffers (with the texture pool, `VICTORY_TEXTURE_POOL`, on by default), else the staging buffers alive | Exact for the blocks; a staging buffer made outside the pool is not in it |
| `send_spare_mib` | Memory kept spare for sends to the card-side copies | Exact (in the card's memory with `VICTORY_SEND_BAR=1`) |
| `view_table_mib` | The tables of distinct views (entries, index nodes, shapes) | Estimate |
| `shader_containers_mib`, `shader_translations_mib` | Shader containers kept by the pipeline table, and the host code of their translations | Exact bytes of the containers and code; not the objects around them |
| `pipeline_library_mib` | The pipeline library's file, kept whole in memory | Exact |
| `title_page_pool_mib` | Pages the title was given from its page pool (sections: not in private commit, so not in the total) | Exact |
| `card_textures_filled_mib`, `card_textures_rendered_mib` | The card's memory for host copies of textures the title fills, and for the rest of the host textures (targets, depth, textures shaders write) | Exact as the host sizes them |
| `card_texture_tiles_mib`, `card_sparse_tiles_mib` | Tiles given to textures, and to sparse windows of title memory | Exact |
| `card_buffer_copies_mib` | Card sides of host copies of title buffers | Exact |
| `descriptor_heaps_mib` | Shader-visible descriptor heaps the host holds (driver memory; not in the total) | Exact heap sizes |
| `card_texture_blocks_free_mib` | What the blocks of textures (`VICTORY_TEXTURE_POOL`) hold that no texture uses: the card's memory they keep (in `card_in_use_mib`), which new textures take first. Much of it with the card past its budget is memory the blocks do not give back until a whole block is empty | Exact; left out while the pool's lock is held when the report is written |
| `host_lists_alive` | A count: host lists alive (their driver objects: `host_lists_driver_estimate_mib`) | Exact count |
| `host_lists_idle` | A count: of those, the ones kept idle for the next recordings and in the reserve kept back for want of memory (`VICTORY_IDLE_BY_FRAME`: as many as a frame asked for lately, on a PC of 24 GiB or less) | Exact count; left out while the pool's lock is held |

The card's own use is also in `[memory]` (`vram_peak`, from the system's own
counters); the `card_*` keys say what of it is ours, by kind, and what is left
of `card_in_use_mib` after them is the card's memory we do not name (blocks of
textures free, objects let go that wait for the submissions that may still
use them, the driver's own). Not counted, so in `unaccounted_mib`: the
driver's other objects, the title's non-committed heaps, the pipeline and
shader objects the host made (only the bytes above are counted) and the
tables of descriptors on the CPU side. With `VICTORY_SEND_BAR=1`
`send_spare_mib` lies in the card's memory and is counted twice.

### Reading the spared work

`[frame_time.spared]` (and its `.steady`) counts work that a switch of the
unified-memory synchronisation leaves out or does another way, per frame,
from the frame line's own lines for each switch. Every key is 0 while its
switch is off.

| Key | Switch | What it is, and which figure it should move |
|---|---|---|
| `shader_writes_left_on_card_mb` | `VICTORY_WC_NO_BACK` | MB a frame shaders were declared to write into big copies of write-combined title memory and that stayed on the card: no compare, fetch or merge. `copying_back_ms`/`_mb` and `title_memory_changed_under_shader_writes` should fall |
| `heap_pages_read_in_place` | `VICTORY_SYNC_IN_PLACE` | Changed heap pages whose descriptor records were read where they are and compared as read, not copied through the kernel first. The heaps' records line's "reading and comparing" and the submitting thread's wait for them (in `preparing_ms`) should fall |
| `slot_writes_sparing_spans` | `VICTORY_SPANS_QUICK` | Writes of view slots that left the placed views' spans and their lock alone. Less waiting on that lock, in the heaps' records' round and the texture passes |
| `heap_rounds_begun_first` | `VICTORY_HEAP_SYNC_FIRST` | Submissions whose round of the heaps' records began as they took the submission lock, beside the planning before it |
| `heap_rounds_asked_at_submit` | `VICTORY_HEAP_SYNC_AT_SUBMIT` | Preparings that found the round they need asked for at the title's submit call, while the submission waited in the line (with `VICTORY_ASYNC_SUBMIT`) |

### Reading the moved work

`[frame_time.moved]` (and its `.steady`) says, in ms per frame, what work
ran on a thread a frame does not wait for instead of on one it does, from
the frame line's own lines. Every key is 0 while its switch is off.

| Key | Switch | What it is, and which figure it should move |
|---|---|---|
| `objects_let_go_aside_ms` | `VICTORY_BURY_ASIDE` | Objects let go past the bury lag, dropped on a thread of their own instead of by the landing that let them go. The relay's landings end sooner: `lists_in_the_air` in `[frame_time.waits]` and `copying_back_ms` should fall |
| `pipeline_table_aside_ms` | `VICTORY_PIPE_TABLE_ASIDE` | The log's table of compute pipelines (and the look at their programs' loops) put together on a thread of its own, not by the submitting thread right after a present |
| `snapshots_beside_ms` | `VICTORY_SNAPSHOTS_BESIDE` | Time a submission's snapshots were being taken by the kept threads while the submitting thread planned the copies in the card's memory |
| `snapshots_waited_ms` | `VICTORY_SNAPSHOTS_BESIDE` | What the submitting thread then still waited for them; `snapshots_beside_ms` less this is what came off `preparing_ms` |

### Reading memory pressure and textures

| Key | From | A high value means |
|---|---|---|
| `lowest_free_ram_mib`, `lowest_commit_headroom_mib` | The budget governor's asks of the system (at most every 32 ms; absent with `VICTORY_BUDGET=0`) | Low is the warning here: under about 1,500 the PC pages and the governor and commit guard hold work back |
| `governor_green_s`, `_yellow_s`, `_red_s` | The governor's looks (about once a second): the time from each look to the next is the level it found | Time at red is time memory ran out: uploads held back, idle memory let go, textures shown late |
| `vram_over_budget`, `vram_over_budget_looks` | Looks that found the card's own memory in use past its budget | The driver moves memory out of the card: stutter, and textures and buffers read from the PC's memory |
| `card_relief_looks`, `card_relief_freed_mib` | Looks of the governor that relieved the card (`VICTORY_CARD_RELIEF`): from 95 % of its budget in use until it is back under 90 %, empty blocks of textures go at once (not the ones made ahead), idle cells of sparse windows at the next submission, idle host copies of textures in the sweeps (the largest first, by the system's own count of the card), and new copies are made of at most 256 texels a side. `_freed_mib` (format 8) is only memory given back to the system: blocks of textures that went (emptied ones and those the compaction emptied) and host copies of their own memory let go or cut down (those go once the submissions that may use them are done). A 7 also counted blocks made ahead that were made again at once | Many looks with `vram_over_budget_looks` close to them: the relief does not bring the card back under its line, and something it does not let go of grows (see the `card_*` keys of `[memory.map]`) |
| `card_relief_in_blocks_mib` | While the card is relieved: host copies of textures let go, cut down or made again elsewhere that lay inside blocks of textures. They give the card nothing by themselves: a block goes only once all of it is empty | Large beside a small `card_relief_freed_mib`: the copies let go were scattered over blocks that stayed (with `VICTORY_CARD_COMPACT=0`, or blocks the compaction could not empty) |
| `card_compact_blocks_released`, `card_compact_returned_mib` | The card's compaction (`VICTORY_CARD_COMPACT`, on): while the card is relieved, the blocks of textures least full (under half of a block) are emptied (their idle copies go, the copies slots hold are made again in another block and shown as before until uploaded), and a block emptied goes once the submissions that may read it are done. The blocks that went, and what the system's count of the card was less just after they went than just before, summed (other allocations made in between count too: an estimate) | `_returned_mib` close to the blocks' bytes (16 MiB each on a card of 8 GiB): the compaction gives the card back what it empties. Few blocks with `card_texture_blocks_free_mib` large: what is left in those blocks is held (by slots whose copies cannot be moved, or used again) |
| `commit_critical_times` | The commit guard ("commit is critical, holding back uploads"): times commit headroom fell under 1,536 MiB | Commit ran short again and again |
| `commit_uploads_held_back` | Stagings for uploads the guard held back ("a staging buffer was held back") | Textures were uploaded later than asked for |
| `commit_allocations_refused` | Allocations refused with `E_OUTOFMEMORY` the guard saw | Memory ran out outright |
| `resident_waits`, `resident_wait_ms`, `resident_wait_longest_ms` | "new memory waited for": `MakeResident` of new memory, times and ms in all | Much new memory made during frames; a long longest wait is a stutter |
| `mapped_waits`, `mapped_wait_ms`, `mapped_wait_longest_ms` | "new card memory waited for until it is mapped" | The same for new card memory |
| `picked_uploaded`, `picked_uploaded_mib` | The frame line's "textures picked by index: N uploaded so far (N MB)" | Volume of texture uploads for shaders that pick textures by index |
| `waiting_for_memory_most`, `_last` | "N waiting for their memory" (the queue line): slots whose texture's memory the title has not mapped, the most at a once-a-second look and the last | Streaming is behind: those textures show nothing or a fallback |
| `no_room_passes` | Passes that made no texture as "the card has no room" | The card was full: new textures waited |
| `uploaded_again_changed_unseen` | "changed after its upload with nothing to tell of it: uploaded again" | The title rewrote textures in a way no watch saw; each was found by looking again |
| `uploaded_again_while_full` | Uploads of textures already on the card made while it had no room | Bytes spent while the card was full |
| `smallest_levels_only`, `_mib` | "host copies of only the smallest levels" made while the card had no room (`VICTORY_TEXTURE_TAIL`) | Textures shown blurry for want of card memory |
| `detail_cut_for_card`, `detail_cut_for_pc` | Host copies made with fewer levels for the card's band, and for the PC's commit (`VICTORY_TEXTURE_DETAIL=pc` only) | Textures shown blurry, and for which shortage |
| `made_whole_in_pool_room` | Slots whose texture was made whole in the room the blocks of textures had free while the card counted short (`VICTORY_CARD_SHORT`) | Detail kept where the card's memory was there |
| `not_shown_unwritten` | Slots that showed the neutral texture, or fewer levels, instead of a host copy's memory no upload went into (`VICTORY_SHOW_WRITTEN`) | Uploads held back or late; nothing unwritten was shown |
| `refresh_ms`, `refresh_ms_steady` | "texture refreshes per frame, all threads: N ms" | Texture work on every thread, the submitting one included |

### Reading `[memory.views]`

Counted since the start by the runtime (`xmem`, which the launcher registers
as a sampler of its own beside the GPU side's) and by the descriptor table
(`descriptor`). Plain counts, a handful of relaxed atomic adds where views
are mapped and unmapped.

| Key | From | A high value means |
|---|---|---|
| `title_views_mapped`, `title_views_unmapped` | Views of the host's mapped and unmapped at title addresses: the title's mapping calls, the GPU's page-mapping copies, views kept in the books made real | Each is a system call (an unmap flushes every core's address cache) and an entry of the process's address space while it is there; tens of thousands a minute is the title streaming |
| `title_view_pages_avg` | The 64 KiB pages those views showed, per view | Near 1: the page-mapping copies map a view a page; `VICTORY_MAP_RUNS` (on by default with 24 GiB or less) maps a run of consecutive pages in one view, which raises it |
| `book_views_noted`, `book_views_let_go_untouched`, `book_views_made_real`, `book_views_now` | Views kept in the runtime's books alone, with no view of the host's until touched (`VICTORY_LAZY_VIEWS`, `VICTORY_SOFT_VIEWS`; all 0 with both off) | `let_go_untouched` is what was spared (no map, no unmap); `made_real` close to `noted` means the views are touched anyway and nothing is gained |
| `ownerless_entries_kept`, `ownerless_entries_dropped` | Descriptor table entries of views no resource releases (constant buffer views and views of placed memory that do not fit a record): kept now, and freed for the cap (`VICTORY_OWNERLESS_VIEWS`; none dropped with it off) | Kept growing for the whole run is memory that is never given back; with the cap, `dropped` counts entries made stale |

### A sample

Rendered from synthetic numbers by the unit test
`report::tests::a_report_is_valid_toml_with_every_section`
(`cargo test --locked -p victory-core a_report_is_valid -- --nocapture`):

```toml
# Rox-box run report
#
# Written by Rox-box at the end of a run (and every 30 seconds while it ran).
# It is meant to be attached to a public issue, and holds only:
#   - numbers: counts, times, sizes, frame times;
#   - words from Rox-box's own source code: reasons, our enum names, our names
#     of interfaces and methods, names of unimplemented imports of the
#     platform's own DLLs;
#   - facts about this PC: the Windows build, the processor model, memory
#     sizes, the graphics adapter's name, vendor and driver version;
#   - the names of the VICTORY_* switches set, with a value only where it
#     is a short plain number or word.
# It never holds: the title's name or id, file or folder names, paths, your
# user or computer name, hashes of executables or shaders, shader or
# pipeline names, memory addresses or offsets into the title, text from the
# title, or command-line arguments. docs/run-report.md explains every field.

format = 8
kind = "run"

[rox_box]
version = "0.2.0"
diagnostics = false

[host]
windows = "10.0"
windows_build = 26100
cpu = "Some Vendor(R) Processor 12-Core"
logical_processors = 24
ram_gib = 32
free_commit_gib_at_start = 40.23

[host.gpu]
adapter = "Some Adapter XT/XTX"
vendor_id = 0x1234
driver = "32.0.15.6094"
dedicated_vram_gib = 8
software_adapter = false
feature_level = "12_2"
shader_model = "6.8"
wave_lanes_min = 32
wave_lanes_max = 32
mesh_shaders = true
view_heap_views = 1000000

[run]
console = "series-x"
display_mode = "3840x2160"
duration_s = 62.40
first_draw_s = 20.90
ended = "title_exited"
exit_code = 0
rox_box_panics = 0

[run.switches]
# VICTORY_* variables set; a value only where it is a plain number or word
"VICTORY_PIPELINE_WAIT_MS" = "500"
"VICTORY_LOG" = "<set>"

[frames]
presents = 1001
first_present_s = 21.30
average_fps = 49.02
# present-to-present times, in ms
frame_ms_mean = 20.40
frame_ms_p50 = 16.75
frame_ms_p90 = 16.75
frame_ms_p99 = 33.25
frame_ms_max = 120.40

[frames.histogram]
# frames by present-to-present time, in ms
"under 8.3" = 0
"8.3 to 11.1" = 0
"11.1 to 16.7" = 900
"16.7 to 22.2" = 0
"22.2 to 33.3" = 90
"33.3 to 50" = 0
"50 to 100" = 0
"100 to 250" = 10
"250 to 1000" = 0
"1000 and over" = 0

[timeline]
# per window since the start of the run (warm-up first, then steady state)
window_s = 10
presents = [0, 0, 180, 600, 221, 0, 0]
fps = [0.0, 0.0, 18.0, 60.0, 22.1, 0.0, 0.0]
worst_frame_ms = [0, 0, 120, 34, 17, 0, 0]

[gpu_work]
# per frame (between two presents), the first present's loading left out
submissions_avg = 9.00
submissions_max = 14
draws_avg = 2100.00
draws_max = 3950
dispatches_avg = 640.00
dispatches_max = 1210
left_out_avg = 1.88
left_out_max = 260
# the same at steady state (see [frame_time])
left_out_avg_steady = 0.08
submissions_total = 9000

[gpu_work.left_out]
# draws and dispatches not run on the host, whole run, by reason
pipeline_not_ready = 1830
shader_not_translated = 44
pipeline_refused = 0
out_of_memory = 0
no_descriptor_slots = 0
indirect_not_bounded = 6
ray_tracing_builder_off = 0
no_pipeline_set = 0
asked_to_leave_out = 0
other = 0

[frame_time]
# per frame, from the meters of the log's once-a-second frame line; ms, MB (10^6 bytes)
# whole run here; [frame_time.steady]: the steady state, the frames from the first
# 10 s window at or after a third of the time from the first present to this report
# (a run with no whole window after that: the readings of the frame meter after it)
frames = 1000
frame_ms = 20.40
preparing_ms = 6.20
copies_up_ms = 2.10
copies_up_mb = 4.10
texture_refresh_ms = 3.00
texture_refresh_mb = 12.00
waiting_for_card_ms = 7.50
copying_back_ms = 1.20
copying_back_mb = 0.90
card_busy_pct = 70.10
card_idle_ms = 6.10
card_idle_behind_us_ms = 4.00
submitting_thread_stood_ms = 7.00
heap_buffer_passes = 2.00
heap_buffer_ms = 0.40

[frame_time.steady]
from_s = 40.00
frames = 370
frame_ms = 16.76
preparing_ms = 4.59
copies_up_ms = 1.41
copies_up_mb = 2.43
texture_refresh_ms = 1.08
texture_refresh_mb = 4.05
waiting_for_card_ms = 7.84
copying_back_ms = 0.81
copying_back_mb = 0.57
card_busy_pct = 85.48
card_idle_ms = 2.43
card_idle_behind_us_ms = 1.62
submitting_thread_stood_ms = 7.30
heap_buffer_passes = 2.00
heap_buffer_ms = 0.30

[frame_time.waits]
# ms per frame the waiting for the card in settles took, by what settled, whole run
sync_submission_or_present = 2.00
card_had_run_them = 0.50
wait_for_memory_value = 0.00
title_memory_changed_under_shader_writes = 1.50
title_memory_read = 1.20
step_of_ours = 0.30
lists_in_the_air = 0.00
bottom_of_pipe_writes = 0.80
blocking_fence_wait = 0.20

[frame_time.waits.steady]
sync_submission_or_present = 1.89
card_had_run_them = 0.54
wait_for_memory_value = 0.00
title_memory_changed_under_shader_writes = 2.43
title_memory_read = 1.35
step_of_ours = 0.00
lists_in_the_air = 0.00
bottom_of_pipe_writes = 0.81
blocking_fence_wait = 0.27

[frame_time.spared]
# per frame, work the unified-memory switches spared (0 with them off), whole run
shader_writes_left_on_card_mb = 50.00
heap_pages_read_in_place = 9.00
slot_writes_sparing_spans = 0.00
heap_rounds_begun_first = 0.00
heap_rounds_asked_at_submit = 0.00

[frame_time.spared.steady]
shader_writes_left_on_card_mb = 50.00
heap_pages_read_in_place = 9.00
slot_writes_sparing_spans = 0.00
heap_rounds_begun_first = 0.00
heap_rounds_asked_at_submit = 0.00

[frame_time.moved]
# ms per frame of work moved off the threads a frame waits for (0 with the switches off), whole run
objects_let_go_aside_ms = 4.00
pipeline_table_aside_ms = 0.00
snapshots_beside_ms = 3.00
snapshots_waited_ms = 0.50

[frame_time.moved.steady]
objects_let_go_aside_ms = 3.00
pipeline_table_aside_ms = 0.00
snapshots_beside_ms = 0.00
snapshots_waited_ms = 0.00

[gpu_time]
# the card's own time on the host queue per frame by kind of work, ms (GPU timestamps around
# runs of commands of one kind; VICTORY_GPU_SPLIT), what our work moved, MB (10^6 bytes), and
# the memory the budget governor saw meanwhile, MiB; whole run here, [gpu_time.steady] as
# [frame_time.steady]
split = "on"
frames = 1000
partly_measured_pct = 2.00
# the title's work
draws_ms = 6.00
dispatches_wave64_ms = 3.50
dispatches_other_ms = 1.20
title_other_ms = 0.40
# ours
fills_ms = 0.80
hashes_ms = 2.10
compares_ms = 1.30
copy_backs_ms = 0.30
uploads_ms = 0.60
texture_uploads_ms = 0.20
other_passes_ms = 0.50
# runs no one kind could be given (past the frame's budget of boundaries)
unattributed_ms = 0.10
# sums: the title's, ours, and all measured (the unattributed with them)
title_ms = 11.10
ours_ms = 5.80
measured_ms = 17.00
# what our work moved, per frame
fills = 2800.00
fills_mb = 0.90
hash_passes = 80.00
hashed_mb = 10.50
compare_passes = 7.00
compared_mb = 488.00
copied_back_mb = 3.20
upload_ranges = 580.00
uploaded_mb = 52.00
texture_uploads_mb = 1.50
# memory while those frames ran: the budget governor's looks (about one a second), MiB
governor_looks = 60
card_used_mib_avg = 7100.00
card_used_mib_peak = 7300.00
card_budget_mib_avg = 7600.00
shared_used_mib_avg = 4800.00
shared_used_mib_peak = 5100.00
shared_budget_mib_avg = 7900.00
free_ram_mib_avg = 900.00
free_ram_mib_lowest = 612.00
looks_card_over_budget = 3
looks_free_ram_under_512_mib = 0
# the title's compute pipelines (VICTORY_GPU_SPLIT pipelines), named nowhere: how many ran in a
# second of the frame line (seconds with any), and per frame the card's time of the costliest
# 1, 5 and 20 of each second, ms
pipeline_seconds = 60
compute_pipelines_a_second_avg = 120.00
costliest_pipeline_ms = 2.00
costliest_5_pipelines_ms = 5.00
costliest_20_pipelines_ms = 8.00

[gpu_time.steady]
from_s = 40.00
frames = 370
partly_measured_pct = 0.00
# the title's work
draws_ms = 5.50
dispatches_wave64_ms = 3.60
dispatches_other_ms = 1.10
title_other_ms = 0.30
# ours
fills_ms = 0.80
hashes_ms = 2.00
compares_ms = 1.30
copy_backs_ms = 0.30
uploads_ms = 0.50
texture_uploads_ms = 0.10
other_passes_ms = 0.40
# runs no one kind could be given (past the frame's budget of boundaries)
unattributed_ms = 0.00
# sums: the title's, ours, and all measured (the unattributed with them)
title_ms = 10.50
ours_ms = 5.40
measured_ms = 15.90
# what our work moved, per frame
fills = 2800.00
fills_mb = 0.90
hash_passes = 80.00
hashed_mb = 10.50
compare_passes = 7.00
compared_mb = 488.00
copied_back_mb = 3.20
upload_ranges = 580.00
uploaded_mb = 52.00
texture_uploads_mb = 1.50
# memory while those frames ran: the budget governor's looks (about one a second), MiB
governor_looks = 20
card_used_mib_avg = 7100.00
card_used_mib_peak = 7300.00
card_budget_mib_avg = 7600.00
shared_used_mib_avg = 4800.00
shared_used_mib_peak = 5100.00
shared_budget_mib_avg = 7900.00
free_ram_mib_avg = 900.00
free_ram_mib_lowest = 612.00
looks_card_over_budget = 3
looks_free_ram_under_512_mib = 0
# the title's compute pipelines (VICTORY_GPU_SPLIT pipelines), named nowhere: how many ran in a
# second of the frame line (seconds with any), and per frame the card's time of the costliest
# 1, 5 and 20 of each second, ms
pipeline_seconds = 20
compute_pipelines_a_second_avg = 120.00
costliest_pipeline_ms = 2.00
costliest_5_pipelines_ms = 5.00
costliest_20_pipelines_ms = 8.00

[pipelines]
created_by_title = 4212
host_made_now = 310
host_from_library = 1402
host_failed = 2

[shaders]
translated_now = 120
from_cache = 2960
failed = 3

[shaders.failures]
# failed translations by the translator's own reason
"unsupported: an instruction" = 3

[memory]
# MiB
process_commit_peak = 21000
working_set_peak = 9800
vram_peak = 7300
vram_budget_at_peak = 7600
shared_peak = 5100
shared_budget_at_peak = 7900
out_of_memory_retried_ok = 4
out_of_memory_retried_failed = 0
governor_level_changes = 2
governor_worst_level = "yellow"

[memory.pressure]
# MiB; the lowest the budget governor saw (it asks at most every 32 ms)
lowest_free_ram_mib = 612
lowest_commit_headroom_mib = 2100
# seconds at each level of the budget governor (it looks about once a second)
governor_green_s = 41.00
governor_yellow_s = 18.50
governor_red_s = 2.90
vram_over_budget = true
vram_over_budget_looks = 3
# looks that found the card past the line of its relief (95 % of its budget, until it is back under 90 %),
# and what they let go of at once (empty blocks of textures; the rest goes in the sweeps after them)
card_relief_looks = 0
card_relief_freed_mib = 0
# copies let go or made again elsewhere inside blocks of textures while relieved (which give back nothing by
# themselves); blocks the compaction emptied that went, and the system's count of the card less after than before
card_relief_in_blocks_mib = 0
card_compact_blocks_released = 0
card_compact_returned_mib = 0
# the commit guard: times commit went critical, stagings it held back, refusals it saw
commit_critical_times = 1
commit_uploads_held_back = 14
commit_allocations_refused = 0
# waits for new memory to be made resident, and for new card memory to be mapped
resident_waits = 52000
resident_wait_ms = 3400.00
resident_wait_longest_ms = 41.00
mapped_waits = 2400
mapped_wait_ms = 900.00
mapped_wait_longest_ms = 12.50

[memory.views]
# views of the host's at title addresses since the start, and the pages one showed on average
title_views_mapped = 40000
title_views_unmapped = 39000
title_view_pages_avg = 2.50
# views kept in the books alone until touched (VICTORY_LAZY_VIEWS, VICTORY_SOFT_VIEWS)
book_views_noted = 0
book_views_let_go_untouched = 0
book_views_made_real = 0
book_views_now = 0
# descriptor table entries of views no resource releases: kept now, freed for the cap (VICTORY_OWNERLESS_VIEWS)
ownerless_entries_kept = 1048576
ownerless_entries_dropped = 52000

[memory.map]
# MiB, sampled when the report is written; a category left out is one nothing counted (not known)
# private: what the process holds in its own private memory; charged: what the driver charges to it
# (the card's memory in use, the host lists' objects); accounted_total is the sum of both,
# unaccounted the process's private commit less it (the title's own heaps, other driver objects, what is not counted)
process_private_commit_mib = 9000
accounted_total_mib = 6200
unaccounted_mib = 2800
# private memory, in the total
title_committed_mib = 4000
windows_of_title_memory_mib = 1500
list_scratch_mib = 700
send_spare_mib = 0
# charged to the private commit, in the total (the host lists' figure is an estimate: lists alive times a measured cost)
# beside it (card memory, mapped sections, descriptor heaps): not in the total
card_textures_filled_mib = 3000
# counts
host_lists_alive = 640

[memory.map.peak]
# the most each category held when a report was written (a report is written every 30 s and at the end)
accounted_total_mib = 6400
unaccounted_mib = 3000
title_committed_mib = 4100
windows_of_title_memory_mib = 1600
list_scratch_mib = 800
send_spare_mib = 100
card_textures_filled_mib = 3100
host_lists_alive = 700

[textures]
# textures picked by index (from the view heap by shaders), since the start
picked_uploaded = 18400
picked_uploaded_mib = 5200
waiting_for_memory_most = 950
waiting_for_memory_last = 12
no_room_passes = 210
uploaded_again_changed_unseen = 7
uploaded_again_while_full = 0
smallest_levels_only = 330
smallest_levels_only_mib = 21
detail_cut_for_card = 0
detail_cut_for_pc = 0
made_whole_in_pool_room = 0
not_shown_unwritten = 0
# texture refreshes on all threads, ms per frame (whole run, steady state)
refresh_ms = 4.50
refresh_ms_steady = 1.62

[unimplemented]
methods_called = 2
methods_lost = 0
# calls of a pipeline creation method not implemented: pipelines the title went without
pipelines_gone_without = 36
imports_named = 1
# imports of DLLs that are not the platform's: only counted
imports_other = 2
imports_other_calls = 5

[[unimplemented.method]]
interface = "device"
offset = 0x218
calls = 36

[[unimplemented.method]]
interface = "command list"
offset = 0x1a8
calls = 3

[[unimplemented.import]]
name = "kernel32.dll!SomeFunction"
calls = 1
```

## What it leaves out, and why

- **A title that ends its process in a way not bound to ours** (a direct
  system call, an import of `ntdll`'s, or any of the C runtime's or
  `kernel32`'s exits with `VICTORY_TITLE_EXITS=0`) ends it without any code
  of ours running; the report then is the last periodic one
  (`killed_or_unknown`, at most 30 s old). Writing from the loader's
  process-detach notice is not safe: the other threads are gone by then and
  may have held the heap.
- **The settles' waits of the last part of a second**
  (`[frame_time.waits]`) are not taken at the end as the other meters are:
  they are kept where the log's settle line reads them. The same holds for
  `[frame_time.spared]`.
- **Per-frame submissions and draws** are counted between two presents of any
  queue; with several presenting queues the frames interleave.
- **Shaders that fail before the translator** (a container that does not
  parse) leave their draws out as `shader_not_translated`, but are not in
  `[shaders]`: only the translator's own outcomes are counted there.
- **The reason of a draw left out** is the one its refusal said on that
  thread (memory, slots, indirect bounds), else what its pipeline's state says
  (not made yet, a shader that does not translate, refused); a draw refused
  for any other reason is `other`.
- **`[frame_time]`** has what the frame line had, and its last part of a
  second: the line's meters, made totals again from its per-frame figures.
  The frames of a meter's second are counted in the 10 s window it ends in
  (and, for a short run's steady state, the second from the first present it
  ends in). The card's busy
  share is 100 % when the card's clock could not be made (no idle time is
  then counted).
- **`first_draw_s`** is known at the present after the first draw, not at the
  draw: the draw path counts nothing for the report.
- **`waiting_for_memory_most`** is the most of a queue's count seen once a
  second, not every pass's.
- **`[memory.pressure]` with `VICTORY_BUDGET=0`** has no lowest free memory
  or commit headroom and no time at the levels: the governor does not look.
  `[gpu_time]` then has no memory figures either.
- **`[gpu_time]`** counts a submission in the second it landed, not the frame
  it was made in. Not measured: the relay's fetches over the copy queue
  (`VICTORY_RELAY_COPY=1`), the compares sent ahead of a plan's sends
  (`VICTORY_CARD_JOIN`), and lists other threads of ours put on the queue on
  their own (textures loaded ahead, tile mappings). A run that spans two of a
  submission's calls to the queue holds whatever another thread put there
  between them (a fetch of the relay's on the drawing queue): counted twice.
  Commands of ours inside the title's lists that are not marked (barriers,
  batched clamps) are in the run before them.

## Adding to it

1. **Pick the kind.** A counter or a gauge is a new variant of
   `report::Stat` (fed with `bump`, `set` or `max`); a new reason a draw is
   left out is a variant of `report::LeftOut` with its `key`; a new way a run
   ends is a variant of `report::End`.
2. **Feed it where it is known, cheaply.** One relaxed atomic operation; no
   allocation, no lock, no string built on a path a draw or frame takes. If
   the number is counted already for a log line, do not count it twice:
   copy it in from the GPU side's sampler (`victory-gpu/src/runreport.rs`,
   `sample`), which runs just before each report is written. A meter that a
   log line takes with `swap(0)` cannot be read twice: give it to the report
   where the line takes it, as a `report::Figure` (`report::figures`, summed
   per 10 s window for the steady state). victory-gpu
   reaches the module as `victory_pipeline::report`, victory-hle as
   `victory_loader::report` (their paths to victory-core; no crate gains a
   dependency).
3. **Render it** in `render` (`victory-core/src/report.rs`), under a key
   that is a literal, with `Doc::int`, `float`, `word` and the like; text only
   as `Ours` or `HostText`.
4. **Keep the allowlist.** Never add a function that takes a runtime string
   from a title or the user's files. If a new kind of text is needed, give it
   its own sanitizing constructor, add it to the test that smuggles a path,
   a hash, an address and a long sentence through every entry point, and say
   here what it may hold.
5. **Describe it** in the table above, and bump `report::FORMAT` if a key
   changes its meaning or goes away.
