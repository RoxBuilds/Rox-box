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
  `abort`, `terminate` or `TerminateProcess` on itself; Rox-box ends it (the
  window was closed, or the title relaunched itself); an unimplemented import
  traps; the host GPU fails for good (device removed);
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

## `victory report`

| Command | Prints |
|---|---|
| `victory report` | The newest report, on stdout; then its path, on stderr (so that `victory report \| clip` copies the report alone: the path names your folders) |
| `victory report --list` | Every report kept, newest first, with how the run ended, how long it ran and its average frame rate |
| `victory report --path` | The newest report's path only |

## Comparing two reports

`victory report --diff` compares the two newest reports (older = A, newer =
B); `victory report --diff <a> <b>` compares two named ones, each a path to a
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
  sections they fill (`[frame_time]`, `[memory.pressure]`, `[textures]`) must
  hold numbers and booleans only.

## The format

Valid TOML that reads as plain text. It opens with a comment block saying
what it holds and what it never does. Sections and keys, in order:

| Key | Meaning |
|---|---|
| `format` | The report format's version (2; 1 had no `[frame_time]`, `[memory.pressure]`, `[textures]`, `first_draw_s`, `left_out_avg_steady` or `pipelines_gone_without`) |
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
| **`[gpu_work.left_out]`** | Draws and dispatches the title recorded and the host did not run, the whole run, by reason: `pipeline_not_ready` (not made within the frame's wait budget), `shader_not_translated`, `pipeline_refused` (by the host), `out_of_memory` (constants, scratch, vertices refused), `no_descriptor_slots`, `indirect_not_bounded`, `ray_tracing_builder_off`, `no_pipeline_set`, `other` |
| **`[frame_time]`** | What a frame cost, per frame over the whole run, from the meters of the log's once-a-second frame line (["Reading the frame figures"](#reading-the-frame-figures)) |
| **`[frame_time.steady]`** | The same over the steady state, which begins at `from_s` |
| **`[frame_time.waits]`**, **`.steady`** | Per frame, ms the submitting thread waited for the card in settles of submissions sent ahead, by what settled them |
| **`[pipelines]`** | `created_by_title`; host pipelines `host_made_now` (compiled during the run) and `host_from_library`; `host_failed` |
| **`[shaders]`** | Translations `translated_now`, `from_cache`, `failed` |
| **`[shaders.failures]`** | Failed translations by the translator's own kind (`not_a_container`, `malformed`, `native_code`, `unsupported`) and wording |
| **`[memory]`** | In MiB: `process_commit_peak`, `working_set_peak`; `vram_peak` and `shared_peak` with the budget when each was reached (`QueryVideoMemoryInfo`, as the budget governor samples it); `out_of_memory_retried_ok` / `_failed`; `governor_level_changes`, `governor_worst_level` |
| **`[memory.pressure]`** | How short memory ran: the lowest free memory and commit headroom, time at each governor level, the card past its budget, the commit guard, the waits for new memory ([below](#reading-memory-pressure-and-textures)) |
| **`[textures]`** | Textures shaders pick from the view heap by index: uploads, slots waiting for their memory, a full card; texture refreshes per frame ([below](#reading-memory-pressure-and-textures)) |
| **`[unimplemented]`** | `methods_called` (distinct), `methods_lost` (calls that found the table full), `pipelines_gone_without` (calls of the device's pipeline creation methods that are not implemented, `DeserializeGraphicsPipelineStateX` and `DeserializeComputePipelineStateX` at 0x1c8 and 0x218: each a pipeline the title did not get that way), `imports_named`, `imports_other` and `imports_other_calls`; then one `[[unimplemented.method]]` per D3D12.X method not written yet that was called, those two included (`interface`, `offset` in its function table, `calls`) and one `[[unimplemented.import]]` per named import (`name`, `calls`) |

### Reading the frame figures

`[frame_time]` holds what the log of every build (not only `diagnostics`)
says once a second of the frames since its last line, added up over the run
and divided by the frames the frame meter counted (presents the swap chain
took). The line takes its meters and sets them to zero, so the report is fed
where the line takes them (`victory-gpu/src/runreport.rs`, `frame_line` and
the functions after it) and the log says what it said before.

**The steady state** is the frames from the first 10 s window at or after a
third of the time from the first present to the report (`from_s`): loading
and first-time work (shaders, pipelines, textures made for the first time)
fall in the first third. It is by time, not by what was drawn, so a run that
ends in a long loading screen has some of it in its steady state. Compare the
two: a figure much higher over the whole run is warm-up; one as high at steady
state is what every frame pays.

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
| `heap_buffer_passes`, `heap_buffer_ms` | "heap buffers N passes N ms": passes over the view heap's buffer views for shaders that pick them by index | Many or slow passes: big heaps rescanned often |

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

### Reading memory pressure and textures

| Key | From | A high value means |
|---|---|---|
| `lowest_free_ram_mib`, `lowest_commit_headroom_mib` | The budget governor's asks of the system (at most every 32 ms; absent with `VICTORY_BUDGET=0`) | Low is the warning here: under about 1,500 the PC pages and the governor and commit guard hold work back |
| `governor_green_s`, `_yellow_s`, `_red_s` | The governor's looks (about once a second): the time from each look to the next is the level it found | Time at red is time memory ran out: uploads held back, idle memory let go, textures shown late |
| `vram_over_budget`, `vram_over_budget_looks` | Looks that found the card's own memory in use past its budget | The driver moves memory out of the card: stutter, and textures and buffers read from the PC's memory |
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
| `refresh_ms`, `refresh_ms_steady` | "texture refreshes per frame, all threads: N ms" | Texture work on every thread, the submitting one included |

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

format = 2

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
other = 0

[frame_time]
# per frame, from the meters of the log's once-a-second frame line; ms, MB (10^6 bytes)
# whole run here; [frame_time.steady]: the steady state, the frames from the first
# 10 s window at or after a third of the time from the first present to this report
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

- **A title that calls the host's `ExitProcess` itself** ends the process
  without any code of ours running; the report then is the last periodic one
  (`killed_or_unknown`, at most 30 s old). Writing from the loader's
  process-detach notice is not safe: the other threads are gone by then and
  may have held the heap.
- **Per-frame submissions and draws** are counted between two presents of any
  queue; with several presenting queues the frames interleave.
- **Shaders that fail before the translator** (a container that does not
  parse) leave their draws out as `shader_not_translated`, but are not in
  `[shaders]`: only the translator's own outcomes are counted there.
- **The reason of a draw left out** is the one its refusal said on that
  thread (memory, slots, indirect bounds), else what its pipeline's state says
  (not made yet, a shader that does not translate, refused); a draw refused
  for any other reason is `other`.
- **`[frame_time]`** has what the frame line had, and no more: the line's
  meters, made totals again from its per-frame figures. The frames of a
  meter's second are counted in the 10 s window it ends in. The card's busy
  share is 100 % when the card's clock could not be made (no idle time is
  then counted).
- **`first_draw_s`** is known at the present after the first draw, not at the
  draw: the draw path counts nothing for the report.
- **`waiting_for_memory_most`** is the most of a queue's count seen once a
  second, not every pass's.
- **`[memory.pressure]` with `VICTORY_BUDGET=0`** has no lowest free memory
  or commit headroom and no time at the levels: the governor does not look.

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
