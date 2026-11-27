# Rox-box

[![Discord](https://img.shields.io/badge/Discord-join%20the%20community-5865F2?logo=discord&logoColor=white)](https://discord.gg/aSmCFjCDTR)

**Rox-box** is an experimental, general-purpose compatibility layer, written in
Rust, that runs x86-64 PE32+ executables built against the Direct3D 12.X API on
Windows with ordinary Direct3D 12.

It contains no game. You point it at files you own, and it supplies the
platform those files expect.

**Community:** players and developers can get help, report problems and coordinate work on our
[Discord server](https://discord.gg/aSmCFjCDTR). Never share game files, keys or other
copyrighted material there.

- [Overview](#overview)
- [For players](#for-players)
- [For developers](#for-developers)
- [Legal and non-commercial notice](#legal-and-non-commercial-notice)
- [License](#license)

## Overview

A title built for the Xbox Series X|S family of consoles is an ordinary 64-bit
x86 Windows-format (PE32+) image. Its machine code needs no translation; what
differs is everything underneath it. Rox-box provides that:

- a **loader** that maps the executable and the DLLs shipped beside it into its
  own process and binds every import itself;
- a **runtime** that implements the console-only system APIs (memory, storage,
  input, audio, the game runtime) and passes the shared Windows APIs through to
  the host;
- a **graphics layer** that receives every D3D12.X command and replays it on a
  host Direct3D 12 device, keeping the title's unified memory in step with the
  graphics card's;
- a **shader translator** that turns the console GPU's native shader code into
  DXIL (Shader Model 6.6), with on-disk caches for translated shaders and
  compiled pipelines.

The command-line program, its config file and its environment switches keep
the name of the upstream project it descends from: `victory.exe`,
`victory.toml`, `VICTORY_*`.

### Maturity

This is research software, version 0.1. Be realistic about it:

- It runs **some** titles, with **major performance and visual limitations**.
  Frame rates are far below native, effects can be missing or wrong, and draws
  whose shaders do not translate yet are left out.
- Many console APIs are implemented only as far as the titles tried so far
  needed. An unimplemented call stops the run and names itself.
- Online services are not implemented; titles see an offline console.
- It needs a recent Direct3D 12 graphics card and driver, and a lot of memory.
- A driver reset or a crash is always possible. Save your work in other
  programs first.

There is no compatibility list and no promise that any particular title works.
[ROADMAP.md](ROADMAP.md) says where the project is going and what to work on
next.

## For players

### System requirements

| | |
|---|---|
| Windows | **Windows 11** (build 22000 or newer), 64-bit. Windows 10 is not supported: its built-in Direct3D 12 runtime stops at Shader Model 6.5, whatever the graphics card or driver |
| Processor | x64 with AVX2 (Intel from 2013, AMD from 2015) |
| Graphics | Direct3D 12 feature level 12_0 with **Shader Model 6.6** and a current driver. Without mesh-shader support, whatever a title draws with mesh shaders is left out |
| Memory | At least 12 GB of free *commit* memory (RAM plus page file) when a title starts; more RAM and video memory help a great deal |
| Software | Nothing to install: no runtime, no redistributable, no compiler |

`play.cmd -Check` (below) tests every one of these on your PC.

**Why not Windows 10?** Translated shaders are Shader Model 6.6, and Windows 10's own Direct3D 12
runtime cannot go past 6.5. Microsoft's redistributable Direct3D 12 runtime (the Agility SDK) would
lift that limit, but it is proprietary: its license bars distributing it in any way that brings it
under a copyleft license such as Rox-box's GPL-3.0, and it adds terms (an end-user agreement, an
indemnity) that a GPL-3.0 release cannot pass on. So release packages contain only Rox-box's own
files, and use the Direct3D 12 runtime that ships with Windows 11.

### Quick start

1. **Download** `Rox-box-<version>-windows-x64.zip` and
   `Rox-box-<version>-windows-x64.zip.sha256` from this repository's GitHub
   **Releases** page.
2. **Verify** the download. In PowerShell, in the download folder:

   ```powershell
   (Get-FileHash .\Rox-box-<version>-windows-x64.zip -Algorithm SHA256).Hash.ToLower()
   Get-Content .\Rox-box-<version>-windows-x64.zip.sha256
   ```

   The two hashes must be the same.
3. **Unzip** the whole folder anywhere (not inside a copy of the Rox-box source
   code) and keep its files together: `victory.exe`, `play.cmd`, `play.ps1`,
   `victory.example.toml`, `QUICKSTART.txt` and the license files.
4. **Double-click `play.cmd`.** The first time, a window asks for the folder
   that holds your own, legally obtained title files (the title's executable
   and everything shipped beside it); you can also drag that folder onto
   `play.cmd`. Rox-box remembers it in `victory.toml` next to `victory.exe`,
   checks your PC (`victory check`: every prerequisite is listed as `OK`,
   `WARN` or `MISSING`, and each line that is not `OK` says how to fix it) and
   asks `Start now? [Y/n]`.
5. **Play**. From then on, double-click `play.cmd`. If the game folder holds
   more than one `.exe`, drag the one to run onto `play.cmd`, or name it:

   ```
   play.cmd -Exe "D:\MyTitles\SomeTitle\title.exe"
   ```

`play.cmd -Setup` asks for the folder again: the old `victory.toml` is kept as
`victory.toml.bak-<date>` and only its `game_dir` line changes. A folder
dragged onto `play.cmd` is used for that run only. To set things up by hand
instead, copy `victory.example.toml` to `victory.toml` and set `game_dir`, in
single quotes:

```toml
[paths]
game_dir = 'D:\MyTitles\SomeTitle'
```

`play.cmd -Check` runs the check alone; it starts nothing and writes nothing.
The commands in this section are written for the Command Prompt (type `cmd` in
Explorer's address bar to open one in the folder). In PowerShell, put `.\` in
front of them: `.\play.cmd -Check`, `.\victory.exe shaders`.

The same steps, shorter, are in [QUICKSTART.txt](QUICKSTART.txt) inside the zip.

### Check your PC without a title

```
victory.exe selftest
```

runs Rox-box's whole frame path with a stand-in title made of its own code: it
creates the device, clears and presents frames, moves memory between the
processor's side and the graphics card's, translates and runs a compute
shader, and draws with a translated vertex and pixel shader. It needs no game
files and no setup, takes a few seconds and opens a small window
for a moment. Each stage is `OK`, `FAIL` or `SKIP`. If a stage fails, update
the graphics driver; if it still fails, run `victory.exe selftest --warp`, the
same test on Windows' software adapter. Passing there means the driver or the
card is at fault; failing there means Rox-box is, and the `FAIL` line is what
to report. `victory.exe selftest --bench` adds a few speed figures that can be
compared between PCs. [docs/selftest.md](docs/selftest.md) explains every
line.

### The first run: shader translation and warm-up

Every shader a title uses has to be translated to DXIL, and every pipeline has
to be compiled by your graphics driver. Both are kept on disk, and without
them the first minutes of a run stutter badly. `play.cmd` does both ahead of
the run, so there is nothing to type:

- **Shaders, automatically.** Before a run, `play.cmd` checks a stamp in the
  cache folder (`shaders\prepared-stamp.txt`) that names the `victory.exe` and
  the game folder the shaders were last translated for. If the stamp is
  missing, or names another `victory.exe` (after a Rox-box update) or another
  game folder, it says so in a line or two and runs `victory.exe shaders`,
  with its progress on screen. That takes a few minutes, once. No question is
  asked. The stamp is written only when the command succeeds; if it fails,
  `play.cmd` says so and starts the run anyway (a run translates shaders when
  it first needs them, with stutter), and tries again next time.
- **Pipelines, with one question.** After a Rox-box or graphics-driver update,
  or when the pipelines were never warmed, and when earlier runs have recorded
  pipelines, `play.cmd` asks once:
  `Warm up the pipelines of your earlier runs now? It takes 10 to 40 minutes and
  makes the run far smoother. [Y/n]`. Enter (or `y`) runs
  `victory.exe warm --used --settle`, which compiles the pipelines earlier runs
  drew with, in separate processes, and then waits until the driver's
  background compilation has gone quiet. When it ends, whatever its result,
  the run starts. `n` skips it and prints the `WARNING:` line described below.
  Without a person at the window (a scheduled task, redirected input) nothing is
  asked and nothing is warmed: only the `WARNING:` line is printed.
- **Nothing recorded yet.** `victory warm` only knows the pipelines that earlier
  runs wrote down, so the very first run is still the slow one: `play.cmd`
  switches the recording on for every run (`VICTORY_PIPELINE_BOOK=1`; each
  pipeline is hashed and written to the cache folder once, the first time the
  title creates it). Until something is recorded `play.cmd` only prints a line
  of advice. `play.cmd -More "VICTORY_PIPELINE_BOOK=0"` switches the recording
  off.

Two switches change this:

```
play.cmd -Prepare
```

does both steps without asking (the warm-up too, when it is due and pipelines are
recorded) and starts nothing afterwards: to prepare ahead of time, or from a
script. Its exit code is 0 when everything that ran succeeded.
`play.cmd -Prepare -SettleMax 20` stops the settling wait of the warm-up after
20 minutes instead of 40 (`-SettleMax` works the same on a normal run).

```
play.cmd -NoPrepare
```

skips all of it, and `play.cmd` behaves as it did before this existed. Nothing
is prepared for `-Check`, `-ShowSettings`, or when the check before a run fails.

The two commands still exist for doing it by hand, in a terminal in the Rox-box
folder: `victory.exe shaders` translates every shader it finds under `game_dir`
into the shader cache (a few minutes), and `victory.exe warm --used --settle`
compiles the pipelines earlier runs drew with (up to 40 minutes;
`--settle-max <minutes>` changes that).

When the pipelines were not warmed with the `victory.exe` the run is about to
start (and you did not warm them, or said no), `play.cmd` prints a `WARNING:`
line before the run with the exact command to run after you close the title.
That is advice, not an error.

### Settings

`play.cmd` passes everything after its name to `play.ps1`:

| Switch | Effect |
|---|---|
| `-Check` | Runs `victory check` and starts nothing |
| `-Setup` | Asks for the game folder again and updates `game_dir` in `victory.toml` (a copy of the old file is kept) |
| `<folder>` or `<file.exe>` first | What dragging onto `play.cmd` passes: a folder is the game folder for this run, an `.exe` is `-Exe` |
| `-Exe <file>` | The title executable, when `game_dir` holds several |
| `-Fast` | Low-memory preset: `-Console SeriesS`, `-Display 1920x1080`, and timed waits instead of spinning. It asks for no picture smaller than 1920x1080: a title can ship its upscaler for one output size and stop when given a smaller one |
| `-Safe` | Turns the newer, more experimental optimisations off |
| `-Console SeriesX` / `SeriesS` | Which console model the title is told it runs on (default `SeriesX`); titles choose resolution and memory budgets by it |
| `-Display <W>x<H>` | The display size reported to the title, e.g. `1920x1080` (default 3840x2160). The simplest way to lower the rendering load |
| `-Output <W>x<H>` | Sets `VICTORY_OUTPUT`. Reserved: the current runtime does not read it yet, so it changes nothing on its own |
| `-RayTracing` | Enables translation of a title's own ray-tracing compute passes (off by default; see [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md)) |
| `-Log` | Writes everything the run prints to a log file |
| `-Fps <N>`, `-SettingsXml`, `-SettingsValues` | Optional helpers that edit a title's own XML settings file before launch; they read `local\play.toml`, described at the top of `play.ps1` |
| `-More "NAME=value;NAME2=value2"` | Sets additional `VICTORY_*` environment switches for this run |
| `-ShowSettings` | Prints the environment switches these options give and starts nothing |
| `-Prepare` | Translates the shaders and warms up the pipelines (see [The first run](#the-first-run-shader-translation-and-warm-up)) without asking, and starts nothing; exit code 0 when everything that ran succeeded |
| `-NoPrepare` | Skips that preparation before a run |
| `-SettleMax <minutes>` | The longest the warm-up waits for the driver's background work to go quiet (default 40) |

Two things to know about switches:

- `play.ps1` **clears every `VICTORY_*` environment variable** except the path
  ones (`VICTORY_CONFIG`, `VICTORY_GAME_DIR`, `VICTORY_CACHE_DIR`,
  `VICTORY_DATA_DIR`, the three optional-file variables and the two
  `VICTORY_SKIP_*` checks and `VICTORY_REPORT`) and then sets its own. A switch you set in the
  terminal beforehand is ignored: pass it with `-More`.
- `-More` and `-SettingsValues` take `NAME=value` pairs separated by `;`, in
  double quotes: `play.cmd -More "VICTORY_AMBIENT=0.8;VICTORY_WINDOW_MAX=1920x1080"`.
  That works from `play.cmd`, a command prompt and PowerShell. Called from
  PowerShell, `play.ps1` also takes a hashtable (`.\play.ps1 -More @{ NAME = 'value' }`);
  `play.cmd` cannot pass one.

Window and input:

- Alt+Enter toggles a borderless 16:9 window.
- `VICTORY_WINDOW_MAX=1920x1080` (via `-More`) limits the window's size; it
  scales the picture and does not change the rendering resolution.
- XInput controllers are used as they are. With none connected, the keyboard
  stands in for one while the title's window has the focus:

  | Keys | Pad |
  |------|-----|
  | W A S D / I J K L | left / right stick |
  | arrows | D-pad |
  | Enter, Backspace, X, Y | A, B, X, Y |
  | Q / E, Z / C | shoulders, triggers |
  | F / H | stick clicks |
  | Esc, Tab | Menu, View |

### Where things go

| What | Where |
|---|---|
| Run logs (with `-Log`) | `%LOCALAPPDATA%\victory\logs` |
| Crash log | `%LOCALAPPDATA%\victory\data\<title id>\crashes.log` |
| Run reports (safe to share) | `%LOCALAPPDATA%\victory\data\reports` |
| Saves and title settings | `%LOCALAPPDATA%\victory\data` (keep this) |
| Caches: prepared images, translated shaders, pipelines | `%LOCALAPPDATA%\victory\cache` (safe to delete) |
| Backups of an edited settings XML | `%LOCALAPPDATA%\victory\backups` |

`cache_dir` and `data_dir` in `victory.toml` move the cache and data folders;
`victory.exe config` shows the folders in use. Both are refused inside a copy
of the Rox-box source tree; a `game_dir` inside one works, with a warning from
`victory check` and `victory config`.

`victory.toml` is looked for in this order: the file named by the
`VICTORY_CONFIG` environment variable, `victory.toml` in the current directory,
`victory.toml` next to `victory.exe`. Every `[paths]` entry can also be given as
an environment variable (named in [victory.example.toml](victory.example.toml)),
which wins over the file.

### Troubleshooting

Always start with `play.cmd -Check` and fix every `MISSING` line. The messages
below are the ones `victory check` and a run print.

| Message | What to do |
|---|---|
| `Config file  no victory.toml found` | Double-click `play.cmd` and pick your folder (or run `play.cmd -Setup`); by hand, copy `victory.example.toml` to `victory.toml` next to `victory.exe` and set `game_dir` |
| `Config file  the config file ... is not valid` | Compare it with `victory.example.toml`. Windows paths go in 'single quotes'; unknown entries are refused |
| `Game folder  not set` / `... does not exist` | Set or correct `game_dir` under `[paths]` in `victory.toml` |
| `Title executable  no .exe file in the game folder` | Point `game_dir` at the folder that holds the title's executable |
| `Title executable  N .exe files in the game folder` | Name the one to run: `play.cmd -Exe <file>` |
| `... is not a Windows executable image (no PE header)` | The file is encrypted, compressed or incomplete. A run needs the title's decrypted 64-bit executable, which you must obtain from your own copy |
| `... is a 32-bit image` / `built for another processor` | Only 64-bit x64 (PE32+) title executables can be run |
| `Patch file` / `Shader profile` / `Format tables` `MISSING` | An optional file named in `victory.toml` does not exist or is malformed: correct the entry or remove it |
| `Game folder  ... is inside a copy of the Rox-box source tree` (a warning) | The run starts; move the title's folder outside the source tree and correct `game_dir` |
| `Cache folder` / `Data folder` `MISSING` (`refusing to use ...`) | The folder is inside a copy of the source tree: set `cache_dir` / `data_dir` to a folder outside it |
| `Processor  this processor has no AVX2 instructions` | The processor is too old for titles |
| `Windows  version ... is older than Windows 11` | Windows 10 cannot run titles (its Direct3D 12 runtime stops at Shader Model 6.5): upgrade to Windows 11 |
| `Memory  ... GB of commit memory free of ... GB (under 12 GB)` | Close other programs (browsers first), or enlarge the page file: Advanced system settings > Performance > Virtual memory. Then try `play.cmd -Fast` |
| `Graphics card  no Direct3D 12 device (feature level 12_0)` | Install or update the graphics driver. Over Remote Desktop or in a virtual machine there may be no such device |
| `Graphics card  ...: a software adapter, not a graphics card` | The graphics driver is not installed |
| `Shader Model 6.6  not supported` | On Windows 11, update the graphics driver and Windows; if it stays, the graphics card is too old. On Windows 10 it is always reported: Windows 11 is required |
| `Mesh shaders  not supported` (a warning) | The run starts, but whatever the title draws with mesh shaders is left out |
| `error: this PC or its configuration cannot run a title yet` | `victory run --execute` found something `MISSING`; `play.cmd -Check` shows it |
| `the title's own DLLs are missing` | The game folder is incomplete: every file of the title belongs next to its executable |
| `hint: Windows ran out of memory to give` | As for `Memory` above |
| `WARNING: ... warmed with another build` / `No warm-up stamp` | Advice only: see [The first run](#the-first-run-shader-translation-and-warm-up) |
| The title stops or the screen freezes mid-run | Read the end of `crashes.log` (see above) and of the newest file in `%LOCALAPPDATA%\victory\logs` after a run with `-Log` |

### Reporting a problem: the run report

Every run writes a **run report**: a short text file with how the run went
(frame times, how many draws were left out and why, memory, how it ended) and
what PC it ran on. It holds numbers, Rox-box's own words and the names of your
processor, graphics card and driver, and nothing else: no title name, no file
or folder names, no addresses, nothing read from the title. It is the one file
that is safe to post.

```
victory.exe report
```

prints the newest one (`victory.exe report | clip` copies it). Open an issue
with the form on this repository's **Issues** page, describe what happened in
your own words, and paste the report and the output of `victory.exe selftest`.
`VICTORY_REPORT=0` (set in Windows, or `play.cmd -More "VICTORY_REPORT=0"`)
turns the reports off. [docs/run-report.md](docs/run-report.md) lists every
field.

**Do not attach or paste title files, logs, screenshots, video or crash
dumps**: they contain material from the title. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## For developers

### Prerequisites

- Windows 10/11 x64 to build; running titles needs Windows 11.
- [rustup](https://rustup.rs). The toolchain version is pinned in
  [rust-toolchain.toml](rust-toolchain.toml) and is installed automatically.
- The MSVC C++ build tools: Visual Studio or "Build Tools for Visual Studio"
  with the "Desktop development with C++" workload.
- A Windows 10 or 11 SDK (a component of that workload).

`scripts\build.ps1 -PrereqOnly` checks all of these and says how to install
what is missing.

### Build

```powershell
git clone <this repository's URL>
cd Rox-box
scripts\build.ps1            # debug build          -> target\debug\victory.exe
```

| Command | Does |
|---|---|
| `scripts\build.ps1` | Debug build |
| `scripts\build.ps1 -Release` | Optimised build, `target\release\victory.exe` |
| `scripts\build.ps1 -Diag` | Optimised build with the `diagnostics` feature, `target\diag\victory.exe` |
| `scripts\build.ps1 -Dist` | Hermetic distribution build (`cargo xtask dist`), `target\release-dist\victory.exe` |
| `scripts\build.ps1 -Test` | The workspace's tests; they need no title files |
| `scripts\build.ps1 -Check` | What CI checks besides the tests: the asset policy, clippy with warnings as errors (default features, then `--all-features`) and the launcher's self-test |
| `scripts\build.ps1 -PrereqOnly` | Only checks the prerequisites |

Several switches may be combined (`-Check -Test`), `-Jobs <n>` limits parallel
jobs, and every Cargo command runs with `--locked`. If PowerShell refuses to
run scripts, start them as
`powershell -ExecutionPolicy Bypass -File scripts\build.ps1`.

The Cargo commands underneath:

```powershell
cargo build --locked -p victory                                        # debug
cargo build --locked --release -p victory                              # release
cargo build --locked --profile diag -p victory --features diagnostics  # diagnostics
cargo xtask dist                                                       # hermetic distribution build
cargo test --locked --workspace
cargo xtask policy
cargo clippy --locked --workspace --all-targets -- -D warnings
```

### Run from the source tree

```powershell
copy victory.example.toml victory.toml   # at the repository root (git-ignored); set game_dir
scripts\play.cmd -Check
scripts\play.cmd
```

`scripts\play.ps1` finds `victory.exe` under `target\` (the first of `diag`,
`release`, `release-dist`, `debug` that is built, or the one `-Build` names) and
behaves as it does in a release folder. Without the launcher:

```powershell
target\release\victory.exe check
target\release\victory.exe inspect <title.exe> --imports
target\release\victory.exe run <title.exe> --execute --console series-x
```

`victory --help` lists every command: `inspect`, `census`, `lib-exports`,
`shader-info`, `translate`, `shaders`, `warm`, `run`, `config`, `check`,
`selftest`, `report` (and `replay` in a diagnostics build).

`victory selftest --warp` runs the whole frame path with a stand-in title of
our own code on the software adapter, so a change to the graphics layer or the
translator can be tried with no title files; `cargo test --workspace` runs it
too ([docs/selftest.md](docs/selftest.md)). The open demo title, an executable
of our own, takes the same scene through a whole `victory run --execute`
(loader, runtime, run report), so anyone can perform a real run
([docs/demo-title.md](docs/demo-title.md)).

### Package a release

```powershell
scripts\package-release.ps1            # builds `cargo xtask dist` first
scripts\package-release.ps1 -NoBuild   # packages the binary already built
```

writes `dist\Rox-box-<version>-windows-x64.zip` and its `.sha256` (the version
is `version` under `[workspace.package]` in [Cargo.toml](Cargo.toml)). Before
zipping, it checks that the binary names no build-machine path
(`cargo xtask verify-dist`), that every DLL it imports is part of Windows, and
that `THIRD_PARTY_LICENSES.md` is current
(`scripts\third-party-licenses.ps1` regenerates it). Pushing a tag `v<version>`
makes the [Build workflow](.github/workflows/build.yml) publish the same
package as a GitHub Release.

### Directory map

| Path | Holds |
|---|---|
| `apps/victory` | The `victory` command-line program |
| `crates/victory-core` | Configuration, logging, the runtime half of the asset policy, the run report |
| `crates/victory-pe` | Read-only PE32+ and import-library parsing |
| `crates/victory-loader` | Preparing, mapping, binding and starting a title image; crash reports |
| `crates/victory-hle` | The console's system APIs, implemented on Windows |
| `crates/victory-gpu` | D3D12.X objects and command lists on host Direct3D 12 |
| `crates/victory-shader` | Console shader code to DXIL |
| `crates/victory-formats` | Texture layouts, BCPack, audio codecs |
| `crates/victory-pipeline` | The content-addressed cache |
| `crates/victory-game` | Readers for container formats titles ship data in |
| `xtask` | Repository tasks: `policy`, `install-hooks`, `dist`, `verify-dist` |
| `scripts` | `build.ps1`, `play.cmd` / `play.ps1`, `package-release.ps1`, `third-party-licenses.ps1` |
| `tests/fixtures` | A stand-in "title" of our own code, for loader tests |
| `docs` | Deeper notes on single subjects |
| `local` | Git-ignored: your own patch files, shader profiles, launcher settings |

### Further reading

- [ROADMAP.md](ROADMAP.md): the goal, the milestones and the open tasks.
- [ARCHITECTURE.md](ARCHITECTURE.md): how the pieces fit, and where to start
  reading.
- [CONTRIBUTING.md](CONTRIBUTING.md): conventions, the checks every change must
  pass, and the clean-room rules.
- [docs/asset-policy.md](docs/asset-policy.md): what may never be committed.
- [docs/architecture.md](docs/architecture.md): import resolution and shader
  translation in more depth.
- [docs/debugging.md](docs/debugging.md): the diagnostics build and its switches.
- [docs/replay.md](docs/replay.md): recording a frame and running it again.
- [docs/low-memory-gpus.md](docs/low-memory-gpus.md): switches for 8 GB / 32-lane
  graphics cards and hosts with little RAM.
- [docs/findings.md](docs/findings.md): where a frame's time goes.
- [docs/selftest.md](docs/selftest.md): the title-free self-test and its
  benchmark.
- [docs/run-report.md](docs/run-report.md): the shareable run report, field by
  field, and how to add a counter.
- [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md): ray-tracing passes and the
  ambient fallback.
- [docs/README-fxceless.md](docs/README-fxceless.md): notes on the upstream
  lineage.

## Legal and non-commercial notice

Rox-box is an independent, **strictly non-commercial** technical project for
software interoperability, research and preservation. It is not sold, carries
no advertising, and is provided **"AS IS"**, without warranty of any kind.

- **It ships no copyrighted material.** This repository and its releases
  contain no game files or assets, no title executables or parts of them, no
  encryption keys, no proprietary SDK code, headers or documentation, and no
  means of circumventing copy protection.
- **You supply your own files.** Everything Rox-box runs comes from files you
  provide, and you are solely responsible for holding the legal right to use
  them. Rox-box does not help you obtain or decrypt anything.
- **No affiliation.** Rox-box is not affiliated with, endorsed by, sponsored by
  or supported by any hardware vendor, platform holder or software publisher.
- **Trademarks.** Xbox and Xbox Series X|S are trademarks of the Microsoft
  group of companies; Direct3D and Windows are trademarks of Microsoft
  Corporation. These and all other trademarks belong to their respective
  owners and are used here only to describe compatibility.

## License

Rox-box is free software under the GNU General Public License, version 3 or
(at your option) any later version (`GPL-3.0-or-later`). See [LICENSE](LICENSE).

### Modification notice (GPL-3.0 §5(a))

This software (**Rox-box**) is a modified version maintained by **ROX**
(updated `2026-11-27` UTC).

Rox-box builds upon the upstream **VIctory** open-source compatibility layer
lineage (`Marvisak/VIctory`, `fxceless/victory-fork`, `DexNom/VIctory`,
`Belobug/VIctory-Belobug-AMD`, `FQY55kai/ZJOU-VICTORY`, `myokex/victory-amd`,
`DoubleGhosty/cool_fox`, and contributors including Geckomacabre,
cxy0072019-sketch, Spexivus, joroyt, tightfall, and romainhedouin). Key
modifications in this release include:

- A clean-room AMD64 console instruction decoder in
  [console_insn_decode.rs](crates/victory-loader/src/console_insn_decode.rs),
  replacing code of GPL-2.0-only lineage.
- Externalised runtime loading for optional binary patches
  (`VICTORY_PATCHES_FILE`), shader profiles (`VICTORY_SHADER_PROFILE`), and
  BCPack format lookup tables (`VICTORY_BCPACK_TABLES`), so that nothing
  specific to a title is part of the source.
- Host and GPU compatibility tuning for 64-lane desktop graphics cards and for
  32-lane / 8 GB graphics cards, behind explicit runtime switches.
- A hermetic distribution build (`[profile.release-dist]`, `cargo xtask dist`)
  that strips debug metadata and build-machine paths, a one-command build
  script, a preflight check (`victory check`) and a packaged release.
- A title-free self-test and benchmark (`victory selftest`), with a builder
  for synthetic shader containers written from the public instruction
  encodings; a numbers-only run report written by every run
  (`victory report`); and first-run setup in the launcher.

Individual upstream changes are credited where they were adapted, in code
comments. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for the upstream
and third-party notices, and [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md)
for the license of every crate built into `victory.exe`.
