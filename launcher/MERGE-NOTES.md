# What the rest of the repository needs for the launcher to ship

Everything of the launcher is in `launcher\`, which is a Cargo workspace of
its own. Nothing outside `launcher\` was changed for it. These are the changes
outside it that shipping it needs, each with its commands and paths, so they
can be made in one pass. Delete this file when they are done.

## 1. `scripts\package-release.ps1`: the launcher in the package

Build it with the same hermetic habits as the engine, and put the one file
beside `victory.exe`:

```powershell
cargo build --release --locked --manifest-path launcher\Cargo.toml
Copy-Item launcher\target\release\roxbox-launcher.exe <package folder>\
```

- The file to ship: `launcher\target\release\roxbox-launcher.exe` (about
  3.5 MB). It needs nothing beside it.
- Add `roxbox-launcher.exe` to the script's list of expected files and to its
  check of imported DLLs. It imports Windows' own DLLs only, but the list is
  longer than the engine's (it has a window): `kernel32`, `user32`, `gdi32`,
  `opengl32`, `shell32`, `ole32`, `oleaut32`, `advapi32`, `comdlg32`,
  `dwmapi`, `uxtheme`, `imm32`, `bcryptprimitives`, `ntdll` and the `api-ms-*`
  sets. Run the script's own import listing on the built file and take what it
  prints rather than this list.
- It is built with the default (dynamic) C runtime. If the package's rule is a
  static one, add `launcher\.cargo\config.toml` with
  `[target.x86_64-pc-windows-msvc] rustflags = ["-C", "target-feature=+crt-static"]`
  and build again.
- The package's name and layout must stay as they are for the launcher's
  updater: `Rox-box-<version>-windows-x64.zip` with one top folder (or
  `victory.exe` at the top), and `Rox-box-<version>-windows-x64.zip.sha256`
  holding `<64 hex digits>  <file name>`.
- `QUICKSTART.txt` and the package's own text: one line, "double-click
  `roxbox-launcher.exe`, or use `play.cmd` as before".

## 2. `.github\workflows\ci.yml`: a job for the launcher

`launcher\` is not a member of the root workspace, so the `Check` job does not
build or test it. A job on `windows-latest`:

```powershell
cd launcher
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

- No GPU and no title are needed. One test starts `cmd.exe` and `ping`, one
  runs `C:\Windows\System32\tar.exe`; both are on the hosted runners.
- `Swatinem/rust-cache` needs `workspaces: launcher` to cache its `target`.
- The root `cargo xtask policy` already reads `launcher\` (it walks every
  tracked file), so the asset policy needs no change. `launcher\Cargo.lock`
  passes it today.
- If the job is to be required, add its name to the "Protect main" ruleset
  beside `Check`.

## 3. `scripts\third-party-licenses.ps1`: the launcher's crates

`THIRD_PARTY_LICENSES.md` is generated from the root workspace's dependencies.
The launcher adds crates the engine does not have (eframe and egui with
epaint, emath, ecolor; winit; glutin and glow; accesskit; rfd; winreg; and
what they bring). Before the file ships with the launcher in the package:

```powershell
cargo metadata --format-version 1 --locked --manifest-path launcher\Cargo.toml
```

gives the same shape the script reads for the root workspace: run it for both
manifests and merge the two lists by crate name and version. All of the
launcher's crates are under MIT, Apache-2.0, or both, with a few under
Zlib, BSD-3-Clause, ISC, OFL-1.1 and Unicode terms (the fonts egui embeds are
Ubuntu Light and Hack, under the Ubuntu Font Licence and MIT, with emoji
fonts under OFL and Apache-2.0); check what the script's licence allowlist
says to each rather than this sentence. The script's `-Check` should cover
both manifests.

## 4. Words

- `README.md` and `QUICKSTART.txt`: the launcher as the way to start, with
  `play.cmd` as it is. `launcher\README.md` has every part described.
- `ROADMAP.md`: the launcher belongs under "nothing to install".

## What the launcher needs of the engine to stay as it is

It reads and starts these, and nothing else of the engine:

| It uses | How |
|---|---|
| `scripts\play.ps1` (release: `play.ps1`) | `powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File play.ps1 <folder> [-Fast] [-Safe] [-Log] [-Build <b>] [-Console SeriesS] [-More "<a=b;c=d>"] [-Prepare] [-Shots <s>]`. `-Shots` is given only when the script's text holds `]$Shots`. |
| `victory.exe --version` | a word of digits and dots |
| `victory.exe check` | lines `[ <STATUS> ] <name>  <detail>` (two spaces before the detail); the line named `Graphics card` with `<name>, <n> MB video memory, ...`; the last line as the summary |
| `victory.exe config` | lines `cache_dir : <path>` and `data_dir : <path>` |
| `victory.exe shaders`, `warm --used --settle`, `selftest`, `report` | as they are; `report`'s standard output is what is copied |
| `<cache_dir>\pipelines\` | removes `host-pipelines.bin`, `warm-stamp.txt`, `warm-done.txt`, `warm-bad.txt` on a reset; nothing else |
| `VICTORY_GAME_DIR` | set for every `victory.exe` it starts |
| `victory.toml` | reads `game_dir` under `[paths]`; writes that line only (a copy kept) |

A change to any row needs the matching change in `launcher\src\runner.rs`,
`system.rs` or `main.rs`; each has a test with the format in it.
