# Rox-box Launcher

`roxbox-launcher.exe`: one window over `victory.exe` and `play.cmd`. It starts
what you would type and shows what comes back. It knows no title: a title is a
folder you name. Nothing is installed; it runs from the unzipped Rox-box folder,
next to `victory.exe`, `play.cmd` and `play.ps1` (or from the source tree).

## What it does

| Part | What happens when you use it |
|---|---|
| **Titles** | Add a folder (button, or drop a folder or a title's `.exe` on the window). Several are kept; the chosen one is the title of every command. **Write to victory.toml** sets `game_dir` to it: every other line stays and a copy `victory.toml.bak-<time>` is kept, as `play.cmd -Setup` does. A first folder is written by itself when there is no `victory.toml` yet. |
| **Launch** | `play.ps1 <folder> [-Fast] [-Safe] [-Log] [-Build ..] [-Console SeriesS] [-More "..."]`, started the way `play.cmd` starts it. **Prepare first** runs the same with `-Prepare` before it (shaders and the pipeline warm-up) and starts the title only if that succeeds. |
| **Stop** | `taskkill /PID <pid> /T /F` on what was started: the script, `victory.exe` and the title. |
| **Status** | Idle, Preparing, Running, or what ended and its exit code. |
| **System** | GPU and video memory from `victory check`; RAM, Windows, page file and commit limit from Windows. The launcher's own notes and warnings (little video memory, 16 GB of RAM, no or a small page file) are said before a run. Below them, every line of `victory check`. It reads; it changes nothing. |
| **Prepare shaders** | `victory shaders` |
| **Warm cache** | `victory warm --used --settle` |
| **Run self-test** | `victory selftest`, with PASS or FAIL from its exit code |
| **Copy run report** | `victory report`, its text to the clipboard. The report holds numbers and Rox-box's own words only (`docs/run-report.md`), so it can go into a public issue as it is. |
| **Reset pipeline cache** | After a confirmation, removes the host pipeline library and the warm-up's marks from `<cache_dir>\pipelines` (the folder `victory config` names). What earlier runs recorded stays, so **Warm cache** makes everything again. For a cache gone stale after a driver or version change. |
| **Console** | Hidden until opened: every line the commands print, and **Copy log**. A log can hold paths of your PC: read it before posting it. |
| **Version** | The version `victory.exe` states, and on **Check latest** the newest release's name from GitHub's public API (through the `curl.exe` Windows ships). Nothing is downloaded or installed; **Releases page** opens the page in your browser. |

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
file of anyone else's SDK. Its icon is drawn by its own code. It has no
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
- Launching a title from it has not been tried; every other part has.
