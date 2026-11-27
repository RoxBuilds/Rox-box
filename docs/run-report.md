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
  as TOML.

## The format

Valid TOML that reads as plain text. It opens with a comment block saying
what it holds and what it never does. Sections and keys, in order:

| Key | Meaning |
|---|---|
| `format` | The report format's version (1) |
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
| `ended` | `title_exited` (with `exit_code`), `title_aborted`, `title_ended_by_rox_box`, `host_device_removed` (with `removed_reason_hresult` and `removed_reason`), `host_gpu_failed`, `crash` (with `crash_in`: `title_code`, `rox_box_code`, `host_dll`, `outside_any_module`; and `exception`: its kind), `unimplemented_import_trapped`, `rox_box_error`, or `killed_or_unknown` |
| `rox_box_panics` | Panics of ours during the run |
| **`[run.switches]`** | `VICTORY_*` variables set, by the rule above |
| **`[frames]`** | |
| `presents`, `first_present_s` | Presents, and when the first came |
| `average_fps`, `frame_ms_mean` | Over the presents' intervals |
| `frame_ms_p50`, `_p90`, `_p99`, `_max` | Present-to-present time percentiles, from buckets of 0.25 ms (to 100 ms) and 10 ms (to 2 s) |
| **`[frames.histogram]`** | Frames by present-to-present time, buckets at 120, 90, 60, 45, 30, 20, 10, 4 and 1 frames a second |
| **`[timeline]`** | Per window since the start (10 s; merged into longer windows to show at most 180): `presents`, `fps`, `worst_frame_ms`. Loading and warm-up come first, then the steady state |
| **`[gpu_work]`** | Per frame (between two presents; the first present's loading is left out), average and most: `submissions`, `draws` (with mesh dispatches) and `dispatches` run on the host, and those `left_out`; `submissions_total` |
| **`[gpu_work.left_out]`** | Draws and dispatches the title recorded and the host did not run, the whole run, by reason: `pipeline_not_ready` (not made within the frame's wait budget), `shader_not_translated`, `pipeline_refused` (by the host), `out_of_memory` (constants, scratch, vertices refused), `no_descriptor_slots`, `indirect_not_bounded`, `ray_tracing_builder_off`, `no_pipeline_set`, `other` |
| **`[pipelines]`** | `created_by_title`; host pipelines `host_made_now` (compiled during the run) and `host_from_library`; `host_failed` |
| **`[shaders]`** | Translations `translated_now`, `from_cache`, `failed` |
| **`[shaders.failures]`** | Failed translations by the translator's own kind (`not_a_container`, `malformed`, `native_code`, `unsupported`) and wording |
| **`[memory]`** | In MiB: `process_commit_peak`, `working_set_peak`; `vram_peak` and `shared_peak` with the budget when each was reached (`QueryVideoMemoryInfo`, as the budget governor samples it); `out_of_memory_retried_ok` / `_failed`; `governor_level_changes`, `governor_worst_level` |
| **`[unimplemented]`** | `methods_called` (distinct), `imports_named`, `imports_other` and `imports_other_calls`; then one `[[unimplemented.method]]` per D3D12.X method not written yet that was called (`interface`, `offset` in its function table, `calls`) and one `[[unimplemented.import]]` per named import (`name`, `calls`) |

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

format = 1

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

[unimplemented]
methods_called = 1
methods_lost = 0
imports_named = 1
# imports of DLLs that are not the platform's: only counted
imports_other = 2
imports_other_calls = 5

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

## Adding to it

1. **Pick the kind.** A counter or a gauge is a new variant of
   `report::Stat` (fed with `bump`, `set` or `max`); a new reason a draw is
   left out is a variant of `report::LeftOut` with its `key`; a new way a run
   ends is a variant of `report::End`.
2. **Feed it where it is known, cheaply.** One relaxed atomic operation; no
   allocation, no lock, no string built on a path a draw or frame takes. If
   the number is counted already for a log line, do not count it twice:
   copy it in from the GPU side's sampler (`victory-gpu/src/runreport.rs`,
   `sample`), which runs just before each report is written. victory-gpu
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
