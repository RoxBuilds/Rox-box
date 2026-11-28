# Rox-box Launcher

`roxbox-launcher.exe`: one window over `victory.exe` and `play.cmd`. It starts
what you would type and shows what comes back. It knows no title: a title is a
folder you name. Nothing is installed; it runs from the unzipped Rox-box folder,
next to `victory.exe`, `play.cmd` and `play.ps1` (or from the source tree).

## The window

- **The rail** on the left: the Rox-Box emblem and wordmark, **Library / Play**,
  **Diagnostics**, **Settings**, and the engine's version at the bottom (with
  a badge when a newer release is out).
- **First run**: one place to drop your title folder, and **Browse**. That is
  all the setup there is: the launcher finds the engine beside it by itself
  (`ROXBOX_HOME` only names another folder for a development build).
- **Library / Play**: the active title as a hero card with one big button. It
  is **PLAY**; while shaders are prepared or the title runs it is **STOP**, and
  the line beside it says which, with a clock. Under it the quick toggles
  (Fast, Log, Safe, Prepare first, console model), then the **system
  readiness** strip (GPU, VRAM, RAM, page file, preflight), then the library.
- **Diagnostics**: every line of `victory check`, and the maintenance tools.
- **Settings**, in groups: General & behavior, Launch defaults, Updates,
  Auto-backups, Accessibility, About & folders.
- **Console**: out of sight until "Console" at the bottom (or Ctrl+L).
- **Notices** come and go at the top right ("Run report copied", "Backup
  created"); the last one stays in the footer.

Nothing has a Save button. A toggle, a title chosen, a size: each is kept the
moment it changes, in `local\launcher.toml`. In a release folder `victory.toml`
is kept naming the active title by itself, when a title is added, chosen or
played: only its `game_dir` line is written, every other line stays, and a copy
`victory.toml.bak-<time>` is kept when it changes, as `play.cmd -Setup` does.
In a source tree the file is the developer's own: choosing a title does not
touch it; PLAY writes it, and so does a button in Settings.

## What each part runs

| Part | What happens when you use it |
|---|---|
| **Add folder / drop** | A folder dialog, or a folder (or a title's `.exe`) dropped on the window. Several are kept; the active one is the title of every command. |
| **PLAY** | `play.ps1 <folder> [-Fast] [-Safe] [-Log] [-Build ..] [-Console SeriesS] [-More "..."]`, started the way `play.cmd` starts it. **Prepare first** runs the same with `-Prepare` before it and starts the title only if that succeeds. |
| **STOP** | Ends the whole process tree at once: what is started is put into a Windows job object, and Stop ends the job, so `victory.exe` and the title go with the script, also when a parent in between has ended already. (`taskkill /T /F` where the job could not be made.) |
| **Readiness** | GPU and video memory from `victory check`; RAM, Windows, page file from Windows. It reads; it changes nothing. |
| **Prepare shaders** | `victory shaders` |
| **Warm cache** | `victory warm --used --settle` |
| **Self-test** | `victory selftest`, with its verdict from its exit code |
| **Run report** | `victory report`, its text to the clipboard. The report holds numbers and Rox-box's own words only (`docs/run-report.md`), so it can go into a public issue as it is. |
| **Reset pipeline cache** | After a confirmation, removes the host pipeline library and the warm-up's marks from `<cache_dir>\pipelines` (the folder `victory config` names). What earlier runs recorded stays, so **Warm cache** makes everything again. |
| **Open ... folder** | Explorer, on the Rox-box folder, the logs folder, the backups folder, or `victory.toml`. |

## Updates

When it starts (unless switched off in Settings) and on **Check now**, the
launcher asks GitHub's public API for the newest release of
`RoxBuilds/Rox-box`, through the `curl.exe` Windows ships. A newer one is shown
as a banner with the start of its notes.

**Download & install**, in a release folder:

1. The release's package (`Rox-box-<version>-windows-x64.zip`) and its checksum
   file are downloaded into `local\update\<tag>\`. Files are only taken from
   this project's own release downloads.
2. The package's SHA-256 is compared with the checksum file. A release that
   publishes none is not compared, and the console says so. A package that
   does not match is not used.
3. It is unpacked there, by the `tar.exe` Windows ships.
4. Each of its files replaces the one in the Rox-box folder. The old copy is
   renamed aside first (a running program can be renamed, not overwritten), so
   if any step fails the new copies are removed and every old file is put
   back. Never replaced: `victory.toml` and anything under `local\`; caches and
   saves are elsewhere and are not part of a package.
5. The launcher starts again and removes the copies that were renamed aside.

If anything fails, or in a source tree, nothing is replaced and the release
page is opened instead.

## Save backups

Off unless switched on in Settings. After a session ends (and on **Back up
now**) the save folder is packed into `saves-<date>-<time>.zip` in the backup
folder, by `tar.exe`, and only the newest few are kept (5 unless changed; 0
keeps all). Only files with that name are ever removed. The save folder is the
one you name. When you name none, it is a `saves` folder inside the engine's
data folder (`data_dir` as `victory config` states it) if there is one, else
that data folder itself, and then only if it holds no more than 250 MB: the
data folder can hold logs, reports and pictures as well, so a large one is not
packed unasked, and the launcher says to name your save folder instead. The
launcher knows no title's own save folder.

## Accessibility

- **Size**: 85 % to 150 %, in Settings or with Ctrl +, Ctrl - and Ctrl 0; kept.
- **High contrast**: brighter text and edges (body text over 15:1, secondary
  text over 7:1, edges over 4.5:1 against the cards; a test holds the figures).
- **Reduced motion**: nothing fades or slides.
- **States are not colours alone**: each has a shape and a word (a disc and
  OK, a triangle and WARN, a square and ERR), and a toggle that is on has a
  tick.
- **Keyboard**: Tab and Shift+Tab reach every control, with a white ring on
  the one that has the focus; Enter or Space presses it; Ctrl+Enter is Play
  or Stop; Ctrl+L the console; Esc closes the console, a question, the notices.
- **Screen readers**: AccessKit is on, and the controls drawn by the launcher
  itself (the window's buttons, PLAY, the rail, the pills, the readiness
  strip, the library's rows) state their role, their name and whether they
  are chosen.

## While a title runs

The launcher draws a frame only when something happened: a key, the pointer,
a line of output. While a title runs its only timer is the run's clock, once
a second, and none at all while it is minimized. **Settings, When a title
starts** goes further: *Minimize it* (it comes back when the title ends) or
*Close it* (the launcher exits; the title goes on, its output is then only in
its log file, so Log is switched on for that run, and no backup is made).

The active title is given to `victory.exe` as `VICTORY_GAME_DIR`, which wins
over `victory.toml`, and to `play.ps1` as its first argument ("the game folder
for this run").

## What it writes

- `victory.toml`: only the `game_dir` line (see above).
- `local\launcher.toml`: your title folders and settings.
- `local\update\`: a downloaded release, while it is installed.
- `local\backups\` (or the folder you name): save backups, when switched on.

`local\` is git-ignored in the source tree; it holds paths of your PC and is
never committed. No registry entries; nothing outside the Rox-box folder
except a backup folder you name yourself.

## What it is not

It holds no title's name, picture, icon, identifier or hash, and no code or
file of anyone else's SDK. It ships no image and no font of its own: the
emblem, the wordmark, the icon and every sign are drawn by its code. It does
not make a title run better: it runs the same commands.

## Building

It is a crate of its own, outside the repository's workspace on purpose (it
shares nothing with the engine crates, so work on one never touches the other):

```powershell
cd launcher
cargo build --release
```

`target\release\roxbox-launcher.exe` is the whole program (about 3.5 MB). Copy it
next to `victory.exe` in a release folder; in the source tree it finds the
repository above it. `ROXBOX_HOME` names another Rox-box folder for a
development build. `roxbox-launcher.exe --probe` prints what it found and
opens no window. `cargo test` runs its tests; none needs a title or a GPU.

It is written in Rust with egui, not with a web view: the repository's asset
policy (`cargo xtask policy`) admits source and text files only, and a web
front end would need page, style and image files that it refuses.

## Not done yet, and not tried

- It is not in the release package (`scripts\package-release.ps1`) or in CI:
  both are outside `launcher\` and were left alone. Until the package holds
  `roxbox-launcher.exe`, an update replaces the engine and the scripts and
  starts the launcher that is there again.
- Tried only by tests on files and processes of our own, never for real: an
  update from a release (the checksum, the swap and the putting back are
  tested; the download and the restart are not), launching a title, Stop on
  a title, a backup after a session.
- Not tried at all: a screen reader reading the window; dragging and resizing
  the window by hand.

`ROXBOX_RELEASE_JSON` names a file with the API's answer for a release, read
instead of asking GitHub: for looking at the banner without the network.
