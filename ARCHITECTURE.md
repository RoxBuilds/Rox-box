# Architecture

This is the map of the codebase: what each part does, how a run gets from a
command line to a frame on screen, and where to start reading. It is meant to
take about ten minutes. Deeper notes on single subjects are linked at the end.

## The idea in one paragraph

A title built for the Xbox Series X|S family is an x86-64 PE32+ image, so its
machine code runs on a PC processor unchanged. Rox-box maps that image into its
own process, binds every function it imports to an implementation of ours (or
to Windows' own, where the two platforms share an API), and starts it. The
title then talks to what it believes is the console: its memory manager, its
storage, its audio hardware and its Direct3D 12.X device. The largest part of
the work is the last one: D3D12.X commands are replayed on a host Direct3D 12
device, the console GPU's native shader code is translated to DXIL, and the
title's unified memory is kept in step with the copies the graphics card needs.

```
 the user's files (outside the repository)         Rox-box (one process)
 ─────────────────────────────────────────         ────────────────────────────────────────
 title executable + its DLLs ─► victory-loader ──► title code, running natively
                                prepare, map, bind      │ imports
                                                        ▼
                                                   victory-hle ──► host Windows (shared APIs)
                                                        │ D3D12.X calls
                                                        ▼
                                                   victory-gpu ──► host Direct3D 12 ──► window
                                                        │ shader containers, at pipeline creation
                                                        ▼
                                                   victory-shader ◄──► cache_dir (shaders, pipelines)
```

The names `victory`, `victory.toml` and `VICTORY_*` come from the upstream
project this one descends from.

## Layout

| Crate | Role | Start in |
|---|---|---|
| `apps/victory` | The command-line front end: one module per command | `src/main.rs` |
| `crates/victory-core` | Config lookup, logging, the runtime half of the asset policy, and the shareable run report | `src/config.rs`, `src/policy.rs`, `src/report.rs` |
| `crates/victory-pe` | Read-only parsing of PE32+ images and import libraries | `src/image.rs` |
| `crates/victory-loader` | Prepares, maps, binds and starts the title image; thread and TLS plumbing; crash reports; emulation of a few console-CPU instructions | `src/lib.rs` |
| `crates/victory-hle` | The console's system APIs, one module per area, and the policy for passing shared APIs to the host | `src/lib.rs`, `src/modules/mod.rs` |
| `crates/victory-gpu` | D3D12.X objects and command lists on a host D3D12 device; memory synchronisation; pipelines and their caches | `src/lib.rs` |
| `crates/victory-shader` | Console shader containers to DXIL | `src/lib.rs`, `src/emit/mod.rs` |
| `crates/victory-formats` | Pure format code: console texture layouts (tiling and detiling), BCPack, XMA2 audio | `src/lib.rs` |
| `crates/victory-pipeline` | The content-addressed on-disk cache (`Cache`, `CacheKey`) | `src/cache.rs` |
| `crates/victory-game` | Readers for container formats that titles ship data in (packed shader archives). Nothing about any one title | `src/lib.rs` |
| `crates/victory-standin` | The title side of a stand-in title (PC-layout structures, table offsets, the self-test's scene), shared by the self-test and the demo title | `src/lib.rs`, `src/scene.rs` |
| `xtask` | Repository tasks: `policy`, `install-hooks`, `dist`, `verify-dist` | `src/main.rs` |
| `tests/fixtures/hle-probe` | A stand-in "title" of our own code, built on demand by the loader tests | |
| `tests/fixtures/demo-title` | The open demo title: an executable of our own that `victory run --execute` runs end to end, with threads of its own and two DLLs ([docs/demo-title.md](docs/demo-title.md)) | `src/main.rs` |
| `tests/fixtures/demo-static-dll`, `demo-late-dll` | The demo title's DLLs: the one it imports and the one it loads by name (one source) | `demo-static-dll/src/dll.rs` |

Everything is platform-level: no crate holds knowledge of a particular title.
What a single title needs (a byte patch, the names of shaders that want special
handling, a format table) comes from optional files the user supplies at run
time. See [Asset policy](#asset-policy).

## From `victory run --execute` to a frame

The whole sequence is `apps/victory/src/run.rs::run`. In order:

1. **Configuration.** `victory_core::Config::load` reads `victory.toml`.
   `preflight::title_exe` picks the executable (the one named, or the only
   `.exe` in `game_dir`) and `preflight::validate_title_exe` checks it is a
   64-bit x64 image.
2. **Preflight.** `preflight::before_execute` refuses to start when the host
   lacks something a run cannot do without (AVX2, a D3D12 device, Shader Model
   6.6) and warns about the rest. `victory check` prints the same items.
3. **Prepare.** `victory_loader::prepare` makes a lightly patched copy of the
   image: marked as a DLL, with no entry point, no import directory and its TLS
   callbacks detached. The Windows loader will map and relocate such a file,
   allocate its static TLS and register its unwind tables, but run none of its
   code.
4. **Map.** `victory_loader::load` writes the copy under `cache_dir\images` and
   loads it as a real module, so debuggers and stack walks see it. Optional
   user patches are applied here (`victory-loader/src/resolution.rs`).
   `load_title_dlls` maps the DLLs the title ships beside its executable the
   same way.
5. **Set the scene.** The title's id and storage quota are read from its
   manifest; the package folder and a temporary folder are mounted as virtual
   drives (`victory_hle::modules::files::mount_at`); the console model is set
   (`victory_gpu::console::set`); the shader and pipeline caches are opened
   (`victory_hle::modules::d3d12x::use_shader_cache`).
6. **Bind.** `LoadedImage::bind_imports` resolves every import through
   `victory_hle::HleResolver`, in this order: our implementation; the host's own
   function if `HostPolicy` passes that DLL through; an export of a title DLL;
   otherwise a generated stub that names the import when it is called
   (`victory-loader/src/thunks.rs`). Without `--execute` the run stops here,
   having printed what is bound and what is missing.
7. **Start.** A crash reporter is installed (`victory_loader::report_crashes`,
   writing `crashes.log` in the title's data folder), the run report is begun
   (`apps/victory/src/report.rs::start`), then
   `LoadedImage::spawn_main` runs the title DLLs' initialisers, the TLS
   callbacks and the original entry point on a thread with the stack size the
   image asks for. From here on the process runs title code.
8. **Device.** The title calls `D3D12XboxCreateDevice`, which is
   `victory_hle::modules::d3d12x::create_device` and then
   `victory_gpu::device::create`. The first use of the host
   (`victory_gpu::host::host`) creates one host D3D12 device, one direct queue
   and a window.
9. **Recording.** D3D12.X records commands with code inlined into the title,
   which writes packets into a command list while there is room and calls the
   list's method otherwise. Our lists report no room, so every command arrives
   as a call into a function table (`victory-gpu/src/list.rs`) and is recorded
   onto a host command list. Draw state is kept in `draw.rs`; each draw brings
   the host list up to date with the host pipeline, our root signature, the
   descriptors and the buffers the translated shaders read.
10. **Pipelines.** A title creates far more pipelines than it draws with, so a
    host pipeline is made on first use, by worker threads
    (`pipeline/make.rs`). The shader containers go through
    `shader_cache::host_shader`, which translates them
    (`victory_shader::emit::translate`) or reads the translation from disk;
    the compiled pipeline goes into the host's pipeline library
    (`pipeline/library.rs`). A draw whose pipeline is not ready within the
    frame's budget is dropped rather than stalling the title.
11. **Submission.** `ExecuteCommandLists` is `queue.rs::execute`. Before the
    host runs the lists, the title memory they read is copied up to the card
    (`shadow.rs`, `texture.rs`), and afterwards what shaders wrote is brought
    back into title memory (`land.rs`).
12. **Present.** `queue.rs::present` hands the title's back buffer (or several
    planes, composited by `host/planes.rs`) to `Host::present` in `host.rs`,
    which shows it through the swap chain of our window.

## Subsystems

### Loader (`victory-loader`)

`lib.rs` describes the five steps above. Worth knowing beyond them:

- `tls.rs`: an executable's code reaches its static TLS as slot 0; mapped as a
  DLL it gets another slot, so those loads are redirected.
- `threads.rs`: the prepared images have no entry point, so thread start and
  end notices are passed on to them by us.
- Title DLLs the title loads by name are mapped, bound and attached by
  `apps/victory/src/run.rs` (`LateDlls`), which counts their references as
  Windows does: the last `FreeLibrary` detaches and unmaps one
  (`VICTORY_TITLE_DLL_FREE`). Prepared copies are named by their bytes and
  their file name, so one DLL shipped under two names is two modules.
- `console_insn.rs`, `console_insn_decode.rs`: instructions the console's CPU
  has and some host CPUs lack are emulated from a vectored exception handler.
  The decoder is a clean-room implementation from the public AMD64 manuals.
- `console_wait.rs`: optionally turns spinning `monitorx`/`mwaitx` loops into
  timed waits (`VICTORY_MWAITX_WAIT`).
- `crash.rs`: names a fault as `module+rva`, unwinds the faulting thread and
  appends to `crashes.log`.
- `resolution.rs`: the optional, user-supplied patch file. Every patch checks
  the bytes it expects before writing, and a group applies all or nothing.
  With no file configured this module does nothing.

### Runtime (`victory-hle`)

`registry.rs` is the table of everything implemented; `modules/` has one file
per area. To add an API, write an `extern "system"` function named like the
import and list it in the module's `module()`. `victory census` shows, without
running anything, which imports of a set of executables are covered.

| Area | Module |
|---|---|
| Memory: console virtual and physical memory from host memory, with the alignment and address limits titles rely on | `xmem.rs` |
| Files: virtual drives over host folders; every path-taking import is rewritten | `files.rs`, `storage.rs` |
| Streaming: the console's DirectStorage, with zlib and (given the user's tables) BCPack decoding | `dstorage.rs` |
| Graphics entry points: device creation, root-signature serialisation | `d3d12x.rs`, `xg.rs` (texture layout library) |
| Audio: the audio coprocessor's queues, XMA decoding on our own thread, Opus decoding; mixed audio leaves through the host's WASAPI | `audio.rs`, `apu.rs`, `dsp.rs` |
| Input: XInput gamepads, with the keyboard as a stand-in pad | `gameinput.rs` |
| Game runtime: users, saves, packages, store, async tasks; online areas answer as an offline console | `xgameruntime.rs`, `xuser.rs`, `xgamesave.rs`, `xpackage.rs`, `xstore.rs`, `xasync.rs` |
| Life cycle and window | `lifecycle.rs`, `display.rs` |
| Overrides of shared Windows APIs whose console meaning differs | `kernel.rs` |
| Network: offline by design | `net.rs`, `xnetworking.rs` |

`host.rs` is `HostPolicy`: which Windows DLLs a title may reach directly.
`run --strict` disables pass-through, so that everything must be ours.

### Memory

Console memory is unified: the title hands the GPU plain CPU addresses, and
either side may write. A PC graphics card cannot read title memory, so
`victory-gpu` keeps host copies and pays for four things: **tracking** what the
CPU wrote, **tracking** what shaders wrote, **copying** the changed ranges, and
keeping all of that in **order** with submissions still in flight.

- `watch.rs`: which title memory was written since a copy took it (the
  kernel's write watching, or read-only guards on physical-page views).
- `shadow.rs`: host copies of title buffers, refreshed per submission.
- `texture.rs`, `tiles.rs`: textures the title writes itself, detiled from the
  console layout (`victory-formats/src/texture.rs`) into host textures.
- `land.rs`: results shaders wrote, brought back into title memory.
- `inplace.rs`, `memory.rs`: title memory opened as card memory, and title
  memory addressed directly by translated shaders.
- `host/budget.rs`, `host/residency.rs`, `pool.rs`: keeping within the card's
  and the PC's memory.

[docs/findings.md](docs/findings.md) describes where a frame's time goes, and
[docs/low-memory-gpus.md](docs/low-memory-gpus.md) the switches for small
cards.

### D3D12.X translation and the host GPU (`victory-gpu`)

`lib.rs` lists the modules. The objects a title sees are laid out as its
inlined code expects (`object.rs`): a function table that starts like the PC
interface, with console additions after it. A method that is not written yet
logs its table offset, so a run says what to write next.

- `device.rs`, `dxgi.rs`: the device and its adapter side (display modes).
- `resource.rs`, `descriptor.rs`, `descriptor/host.rs`, `root.rs`,
  `sampler.rs`: resources, descriptor heaps (host heaps keep the title's slot
  order, because translated shaders index them directly) and root signatures.
- `list.rs`, `draw.rs`, `indirect.rs`: command lists, draws, indirect
  execution.
- `queue.rs`, `fence.rs`, `frame.rs`: submission, fences, frame pacing and
  present.
- `host.rs` and `host/`: the one host device, queue and window; sending work
  to the card and waiting for it.
- `upscale.rs`, `upscale/`: the host upscaler, which runs a title's FidelityFX
  upscale on AMD FSR 3.1 from a DLL the user supplies, with the title's own
  upscaler to fall back on (its title side is
  `victory-hle/src/modules/fidelityfx.rs`; [docs/upscaling.md](docs/upscaling.md)).
- `ambient.rs`: an optional screen-space ambient estimate, used when a title's
  ray-tracing passes are not run
  ([docs/fork-ray-tracing.md](docs/fork-ray-tracing.md)).
- `replay/`, `capture.rs`, `timeline/` (diagnostics builds): recording a frame
  and running it again ([docs/replay.md](docs/replay.md)).
- `selftest.rs`: a stand-in title that drives the D3D12.X tables end to end
  with no title files ([docs/selftest.md](docs/selftest.md)); `runreport.rs`:
  what the graphics layer feeds the run report
  ([docs/run-report.md](docs/run-report.md)).

### Shader translation (`victory-shader`)

A console shader is a standard DXBC container whose `DXIL` part carries, next
to the LLVM bitcode, the native code for the console GPU. Because the bitcode
may use console-only intrinsics, the native code is what gets translated:

| Step | Module |
|---|---|
| Container and parts | `dxbc.rs`, `dxil.rs` |
| Native programs and their root parameters | `native.rs`, `spill.rs` |
| Instruction decoding | `gfx10/` |
| One-to-one SSA form | `ir.rs` (`ir::build`) |
| Structurisation: the per-wave execution mask becomes per-lane control flow | `structure.rs` |
| Input/output raising and resource tracking: each descriptor and memory address is traced to the root parameter it comes from | `io.rs`, `resource.rs` |
| Lowering and DXIL emission (Shader Model 6.6, direct heap indexing) | `emit/` (`emit::translate`, `emit/lower.rs`, `emit/bitcode/`) |

`victory translate` runs every shader it finds through these steps and tallies
what cannot be handled yet; that list is the translator's to-do list.

### Caches, the pipeline library and warm-up

All under `cache_dir` (default `%LOCALAPPDATA%\victory\cache`); all can be
deleted at any time.

| Folder | Holds | Code |
|---|---|---|
| `images` | Prepared copies of title images | `victory-loader/src/windows.rs` |
| `shaders` | Translated shaders, keyed by the container's content, a hash of the translator's sources, the value of every switch the translator reads, and what the host device reported | `victory-gpu/src/shader_cache.rs` |
| `pipelines` | The host's pipeline library, the list of pipelines earlier runs drew with (started early on the next run), and, with `VICTORY_PIPELINE_BOOK=1` (which `play.ps1` sets), every pipeline the title created | `pipeline/library.rs`, `pipeline/known.rs` |

Because the shader key includes the translator's source hash, a rebuild that
does not touch `victory-shader` starts warm, and one that does never serves a
stale translation.

- `victory shaders` (`apps/victory/src/translate.rs::precompile`) fills the
  shader cache from files, ahead of any run.
- `victory warm` (`apps/victory/src/warm.rs`) makes the host pipelines listed
  in the pipeline book in child processes, so a driver compiler that crashes
  or hangs costs one pipeline. `--used` restricts it to pipelines earlier runs
  drew with; `--settle` then holds them until the driver's background
  compilation is idle. It writes `pipelines\warm-stamp.txt`, which
  `scripts\play.ps1` compares with the executable it is about to run.

### Asset policy

Nothing from a title is ever committed, and nothing derived from one is ever
written into the source tree. Two halves enforce it:

- statically, `cargo xtask policy` (`xtask/src/policy.rs`) is an allowlist over
  every file git would track;
- at run time, everything that writes derived data calls
  `victory_core::ensure_outside_repo` (`victory-core/src/policy.rs`), which
  refuses any folder below a `.victory-root` marker.

[docs/asset-policy.md](docs/asset-policy.md) is the full rule.

## Configuration and switches

**Paths** live in `victory.toml` (`victory-core/src/config.rs`). The file is the
one `VICTORY_CONFIG` names, else `victory.toml` in the current directory, else
`victory.toml` next to the executable. Under `[paths]`:

| Key | Environment variable | Meaning |
|---|---|---|
| `game_dir` | `VICTORY_GAME_DIR` | The user's title folder (required) |
| `cache_dir` | `VICTORY_CACHE_DIR` | Derived data; refused inside the source tree |
| `data_dir` | `VICTORY_DATA_DIR` | Saves, title settings, crash logs; refused inside the source tree |
| `patches_file` | `VICTORY_PATCHES_FILE` | Optional byte-checked patches for the user's title |
| `shader_profile` | `VICTORY_SHADER_PROFILE` | Optional names and bindings of shaders that need special handling |
| `bcpack_tables` | `VICTORY_BCPACK_TABLES` | Optional BCPack tables, supplied by the user |
| `upscaler_dll` | `VICTORY_UPSCALER_DLL` | Optional PC upscaler DLL for the host upscaler, supplied by the user ([docs/upscaling.md](docs/upscaling.md)) |
| `test_titles` | `VICTORY_TEST_TITLES` (`;`-separated) | Other folders for `census` and `translate` |

An environment variable wins over the file. A relative path in the file is
relative to the file. Unknown keys are an error. `victory config` prints what
was resolved; [victory.example.toml](victory.example.toml) documents every
entry and the schema of the optional files.

**Behaviour switches** are `VICTORY_*` environment variables. There is no
central list on purpose: each is read where it is used and described in a
comment there, so `grep VICTORY_NAME` finds its meaning and its code together.
Three patterns cover nearly all of them (`victory-gpu/src/switch.rs`):

- `switch::on("VICTORY_X")`: off unless set to something other than `0`;
- `toggled!("VICTORY_X")`: on unless set to `0`, read once;
- a value (`VICTORY_X=<n>`), parsed where it is read.

A change of behaviour goes behind a switch that can select the old behaviour,
so two runs can be compared. Switches the shader translator reads are
collected at build time (`victory-shader/build.rs`) and are part of the shader
cache key.

[docs/switches.md](docs/switches.md) lists every switch, collected from those
comments (`cargo xtask switches`; it is generated, never edited by hand).

`scripts\play.ps1` sets a tested combination of switches for a run
(`Get-RunSettings`), clears any other `VICTORY_*` variable, and accepts more
through `-More`. Logging is filtered with `VICTORY_LOG`
(`victory-core/src/logging.rs`); debug and trace output exist only in a build
with the `diagnostics` feature ([docs/debugging.md](docs/debugging.md)).

## Where to start reading

- **To follow a run**: `apps/victory/src/run.rs`, then
  `crates/victory-loader/src/lib.rs`.
- **To add a missing API**: run `victory census --missing`, then read
  `crates/victory-hle/src/modules/mod.rs` and a small module such as
  `xsystem.rs`.
- **To fix a shader that does not translate**: `victory translate --list`, then
  `crates/victory-shader/src/lib.rs` and the header of `emit/mod.rs`.
- **To understand a graphics problem**: the headers of
  `crates/victory-gpu/src/list.rs`, `draw.rs` and `shadow.rs`; then record a
  frame ([docs/replay.md](docs/replay.md)).
- **To change what a player sees first**: `apps/victory/src/preflight.rs` and
  `scripts/play.ps1`.
- **To try a change with no title files**: `victory selftest --warp`
  ([docs/selftest.md](docs/selftest.md)); for the whole run path, the demo
  title ([docs/demo-title.md](docs/demo-title.md)).
- **To see what to work on**: [ROADMAP.md](ROADMAP.md).

Most files open with a module comment that explains the console behaviour
being reproduced and why the code is shaped as it is. Approximations are
marked `shortcut:` in the code.

## Deeper notes

- [docs/architecture.md](docs/architecture.md): import resolution and shader
  translation in more depth.
- [docs/asset-policy.md](docs/asset-policy.md)
- [docs/debugging.md](docs/debugging.md)
- [docs/switches.md](docs/switches.md): every `VICTORY_*` switch, generated from the source
- [docs/replay.md](docs/replay.md)
- [docs/findings.md](docs/findings.md)
- [docs/selftest.md](docs/selftest.md)
- [docs/run-report.md](docs/run-report.md)
- [docs/low-memory-gpus.md](docs/low-memory-gpus.md)
- [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md)
- [docs/upscaling.md](docs/upscaling.md)
- [docs/README-fxceless.md](docs/README-fxceless.md)
