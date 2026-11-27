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

## For players

### System requirements

| | |
|---|---|
| Windows | Windows 11, or Windows 10 version 2004 (build 19041) or newer, 64-bit |
| Processor | x64 with AVX2 (Intel from 2013, AMD from 2015) |
| Graphics | Direct3D 12 feature level 12_0 with **Shader Model 6.6** and a current driver. Without mesh-shader support, whatever a title draws with mesh shaders is left out |
| Memory | At least 12 GB of free *commit* memory (RAM plus page file) when a title starts; more RAM and video memory help a great deal |
| Software | Nothing to install: no runtime, no redistributable, no compiler |

`play.cmd -Check` (below) tests every one of these on your PC.

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
4. **Copy** `victory.example.toml` to `victory.toml`, next to `victory.exe`.
5. **Edit** `victory.toml` and set `game_dir` to the folder that holds your own,
   legally obtained title files (the title's executable and everything shipped
   beside it), in single quotes:

   ```toml
   [paths]
   game_dir = 'D:\MyTitles\SomeTitle'
   ```

6. **Check**. Open a Command Prompt in the folder (type `cmd` in Explorer's
   address bar) and run:

   ```
   play.cmd -Check
   ```

   The commands in this section are written for the Command Prompt. In
   PowerShell, put `.\` in front of them: `.\play.cmd -Check`,
   `.\victory.exe shaders`.

   This runs `victory check`: every prerequisite is listed as `OK`, `WARN` or
   `MISSING`, and each line that is not `OK` says how to fix it. It starts
   nothing and writes nothing.
7. **Play**. Double-click `play.cmd`, or run it from the terminal. If the game
   folder holds more than one `.exe`, name the one to run:

   ```
   play.cmd -Exe "D:\MyTitles\SomeTitle\title.exe"
   ```

The same steps, shorter, are in [QUICKSTART.txt](QUICKSTART.txt) inside the zip.

### The first run: shader translation and warm-up

Every shader a title uses has to be translated to DXIL, and every pipeline has
to be compiled by your graphics driver. Both happen on first use and both are
kept on disk, so the **first run of a title is slow and stutters**; later runs
start from the caches. Two optional commands do the work ahead of time. Run
them in a terminal in the Rox-box folder:

```
victory.exe shaders
```

translates every shader it finds under `game_dir` into the shader cache (a few
minutes; needed again only after a Rox-box update that changes the translator).

```
victory.exe warm --used --settle
```

compiles, in separate processes, the pipelines that earlier runs drew with, and
then waits until the driver's background compilation has gone quiet (up to 40
minutes; `--settle-max <minutes>` changes that). It is worth doing again after
a Rox-box or graphics-driver update. `victory warm` only knows the pipelines
that earlier runs wrote down, so play once first: `play.cmd` switches the
recording on for every run (`VICTORY_PIPELINE_BOOK=1`; each pipeline is hashed
and written to the cache folder once, the first time the title creates it).
`play.cmd -More "VICTORY_PIPELINE_BOOK=0"` switches it off.

`play.cmd` prints a `WARNING:` line before a run when the pipelines were not
warmed with the `victory.exe` it is about to start, with the exact command to
run after you close the title. That is advice, not an error.

### Settings

`play.cmd` passes everything after its name to `play.ps1`:

| Switch | Effect |
|---|---|
| `-Check` | Runs `victory check` and starts nothing |
| `-Exe <file>` | The title executable, when `game_dir` holds several |
| `-Fast` | Low-memory preset: `-Console SeriesS`, `-Display 1280x720`, `-Output 1280x720`, and timed waits instead of spinning |
| `-Safe` | Turns the newer, more experimental optimisations off |
| `-Console SeriesX` / `SeriesS` | Which console model the title is told it runs on (default `SeriesX`); titles choose resolution and memory budgets by it |
| `-Display <W>x<H>` | The display size reported to the title, e.g. `1920x1080` (default 3840x2160). The simplest way to lower the rendering load |
| `-Output <W>x<H>` | Sets `VICTORY_OUTPUT`. Reserved: the current runtime does not read it yet, so it changes nothing on its own |
| `-RayTracing` | Enables translation of a title's own ray-tracing compute passes (off by default; see [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md)) |
| `-Log` | Writes everything the run prints to a log file |
| `-Fps <N>`, `-SettingsXml`, `-SettingsValues` | Optional helpers that edit a title's own XML settings file before launch; they read `local\play.toml`, described at the top of `play.ps1` |
| `-More "NAME=value;NAME2=value2"` | Sets additional `VICTORY_*` environment switches for this run |
| `-ShowSettings` | Prints the environment switches these options give and starts nothing |

Two things to know about switches:

- `play.ps1` **clears every `VICTORY_*` environment variable** except the path
  ones (`VICTORY_CONFIG`, `VICTORY_GAME_DIR`, `VICTORY_CACHE_DIR`,
  `VICTORY_DATA_DIR`, the three optional-file variables and the two
  `VICTORY_SKIP_*` checks) and then sets its own. A switch you set in the
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
| `Config file  no victory.toml found` | Copy `victory.example.toml` to `victory.toml` next to `victory.exe` and set `game_dir` |
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
| `Windows  version ... is too old` / `older than Windows 10 version 2004` | Install the pending Windows updates |
| `Memory  ... GB of commit memory free of ... GB (under 12 GB)` | Close other programs (browsers first), or enlarge the page file: Advanced system settings > Performance > Virtual memory. Then try `play.cmd -Fast` |
| `Graphics card  no Direct3D 12 device (feature level 12_0)` | Install or update the graphics driver. Over Remote Desktop or in a virtual machine there may be no such device |
| `Graphics card  ...: a software adapter, not a graphics card` | The graphics driver is not installed |
| `Shader Model 6.6  not supported` | Update the graphics driver and Windows; if it stays, the graphics card is too old |
| `Mesh shaders  not supported` (a warning) | The run starts, but whatever the title draws with mesh shaders is left out |
| `error: this PC or its configuration cannot run a title yet` | `victory run --execute` found something `MISSING`; `play.cmd -Check` shows it |
| `the title's own DLLs are missing` | The game folder is incomplete: every file of the title belongs next to its executable |
| `hint: Windows ran out of memory to give` | As for `Memory` above |
| `WARNING: ... warmed with another build` / `No warm-up stamp` | Advice only: see [The first run](#the-first-run-shader-translation-and-warm-up) |
| The title stops or the screen freezes mid-run | Read the end of `crashes.log` (see above) and of the newest file in `%LOCALAPPDATA%\victory\logs` after a run with `-Log` |

If you report a problem, describe it in your own words. **Do not attach or
paste title files, logs, screenshots or crash dumps**: they contain material
from the title. See [CONTRIBUTING.md](CONTRIBUTING.md).

## For developers

### Prerequisites

- Windows 10/11 x64.
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
`shader-info`, `translate`, `shaders`, `warm`, `run`, `config`, `check` (and
`replay` in a diagnostics build).

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
| `crates/victory-core` | Configuration, logging, the runtime half of the asset policy |
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

Individual upstream changes are credited where they were adapted, in code
comments. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for the upstream
and third-party notices, and [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md)
for the license of every crate built into `victory.exe`.
