# Rox-box Launcher

`roxbox-launcher.exe`: one window over `victory.exe` and `play.cmd`. It starts
what you would type and shows what comes back. It knows no title: a title is a
folder you name. Nothing is installed; it runs from the unzipped Rox-box folder,
next to `victory.exe`, `play.cmd` and `play.ps1` (or from the source tree).

## The window

- **The rail** on the left: the Rox-Box emblem and wordmark, **Library / Play**,
  **Diagnostics**, **Settings**, and the engine's version at the bottom (with
  a badge when a newer release was found).
- **Library / Play**: the active title as a hero card with one big button. It
  is **PLAY**; while shaders are prepared or the title runs it is **STOP**, and
  the line beside it says which, with a clock. Under it the quick toggles
  (Fast, Log, Safe, Prepare first, console model), then the **system
  readiness** strip (GPU, VRAM, RAM, page file, preflight: a green, amber or
  red dot each, the reason on hover), then the library of title folders.
- **Diagnostics**: every line of `victory check`, and the maintenance tools.
- **Settings**: what the window does when a title starts, the engine build
  (source tree), extra switches, `victory.toml`, the version.
- **Console**: out of sight until "Console" at the bottom is pressed; every
  line the commands print, and Copy log.

## What each part runs

| Part | What happens when you use it |
|---|---|
| **Add folder** | A folder dialog, or drop a folder or a title's `.exe` on the window. Several are kept; the active one is the title of every command. A first folder is written to `victory.toml` by itself when that file does not exist. |
| **PLAY** | `play.ps1 <folder> [-Fast] [-Safe] [-Log] [-Build ..] [-Console SeriesS] [-More "..."]`, started the way `play.cmd` starts it. **Prepare first** runs the same with `-Prepare` before it and starts the title only if that succeeds. |
| **STOP** | Ends the whole process tree at once: what is started is put into a Windows job object, and Stop ends the job, so `victory.exe` and the title go with the script, also when a parent in between has ended already. (`taskkill /T /F` where the job could not be made.) |
| **Readiness** | GPU and video memory from `victory check`; RAM, Windows, page file from Windows. It reads; it changes nothing. |
| **Prepare shaders** | `victory shaders` |
| **Warm cache** | `victory warm --used --settle` |
| **Self-test** | `victory selftest`, with PASS or FAIL from its exit code |
| **Run report** | `victory report`, its text to the clipboard. The report holds numbers and Rox-box's own words only (`docs/run-report.md`), so it can go into a public issue as it is. |
| **Reset pipeline cache** | After a confirmation, removes the host pipeline library and the warm-up's marks from `<cache_dir>\pipelines` (the folder `victory config` names). What earlier runs recorded stays, so **Warm cache** makes everything again. |
| **Write active title** | Sets `game_dir` in `victory.toml`: every other line stays and a copy `victory.toml.bak-<time>` is kept, as `play.cmd -Setup` does. |
| **Check for a newer release** | The newest release's name from GitHub's public API (through the `curl.exe` Windows ships). Nothing is downloaded or installed; **Releases page** opens the page in your browser. |

## While a title runs

The launcher draws a frame only when something happened: a key, the pointer,
a line of output. While a title runs its only timer is the run's clock, once
a second, and none at all while it is minimized. **Settings, When a title
starts** goes further: *Minimize it* (it comes back when the title ends) or
*Close it* (the launcher exits; the title goes on, and its output is then
only in its log file, so Log is switched on for that run).

The chosen title is given to `victory.exe` as `VICTORY_GAME_DIR`, which wins
over `victory.toml`, and to `play.ps1` as its first argument ("the game folder
for this run"). So choosing another title needs no change of `victory.toml`.

## What it writes

- `victory.toml`: only the `game_dir` line, only on **Write to victory.toml**
  (or for the first folder when the file does not exist).
- `local\launcher.toml`: your title folders and the toggles. `local\` is
  git-ignored in the source tree; it holds paths of your PC and is never
  committed.

Nothing else. No registry entries, no files outside the Rox-box folder.

## What it is not

It holds no title's name, picture, icon, identifier or hash, and no code or
file of anyone else's SDK. It ships no image and no font of its own: the
emblem, the wordmark, the icon and every sign are drawn by its code. It has no
updater. It does not make a title run better: it runs the same commands.

## Building

It is a crate of its own, outside the repository's workspace on purpose (it
shares nothing with the engine crates, so work on one never touches the other):

```powershell
cd launcher
cargo build --release
```

`target\release\roxbox-launcher.exe` is the whole program (about 3 MB). Copy it
next to `victory.exe` in a release folder; in the source tree it finds the
repository above it. `ROXBOX_HOME` names another Rox-box folder for a
development build. `roxbox-launcher.exe --probe` prints what it found and
opens no window. `cargo test` runs its tests; none needs a title or a GPU.

It is written in Rust with egui, not with a web view: the repository's asset
policy (`cargo xtask policy`) admits source and text files only, and a web
front end would need page, style and image files that it refuses.

## Not done yet

- It is not in the release package (`scripts\package-release.ps1`) or in CI:
  both are outside `launcher\` and were left alone.
- Launching a title from it has not been tried, nor Stop on a title (Stop is
  tested on a tree of processes of our own); every other part has.
