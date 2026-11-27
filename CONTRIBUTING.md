# Contributing to Rox-box

Thank you for wanting to help. Rox-box is a non-commercial, GPL-3.0-or-later
project, and it stays publishable only as long as nothing proprietary ever
enters it. Please read the [clean-room rules](#clean-room-rules) before you
open an issue or a pull request: they are strict, and they apply to issues,
comments and commit messages as much as to code.

[ARCHITECTURE.md](ARCHITECTURE.md) explains how the code is organised and
where to start reading.

## Setting up

```powershell
git clone <this repository's URL>
cd Rox-box
scripts\build.ps1 -PrereqOnly     # checks rustup, the pinned toolchain, MSVC tools, the Windows SDK
scripts\build.ps1                 # debug build
cargo xtask install-hooks         # runs the asset-policy check on every commit
```

To run a title of your own, copy `victory.example.toml` to `victory.toml` at
the repository root (it is git-ignored) and set `game_dir` to a folder
**outside** the repository. The tests need no title files.

## Clean-room rules

Rox-box is written from publicly documented behaviour and from observing our
own code run. A contribution must be something you wrote yourself and may
license under GPL-3.0-or-later.

**Never contribute, in any form** (source, comment, test, fixture, doc, issue,
pull-request text, commit message, attachment):

- copyrighted code from anyone who has not licensed it compatibly, including
  decompiled or disassembled title or system code;
- proprietary or leaked SDK material: headers, libraries, documentation,
  samples, or text copied or paraphrased from them. If you have had access to
  such material under an agreement, do not contribute to the areas it covers;
- keys, certificates, or anything that circumvents copy protection;
- game files or anything made from them: executables, assets, archives, memory
  or crash dumps, shader binaries, converted or extracted data;
- anything that identifies or describes a specific title's contents: the
  title's or publisher's name, hashes of its executable or of its shaders,
  function or data offsets, byte patterns, shader names, patch bytes;
- logs, screenshots or video from running a title. They contain the title's
  text, images and addresses.

**What belongs in the repository** is code that understands a *platform*: an
API's behaviour, a file format, an instruction encoding, written by us and
tested with synthetic data our own code generates.

**What a single title needs** stays on your PC, in files the layer reads at
run time and git never sees:

| File | Purpose | Configured by |
|---|---|---|
| a patch file | byte-checked patches for your title's executable | `patches_file` / `VICTORY_PATCHES_FILE` |
| a shader profile | names, bindings and hashes of shaders that need special handling | `shader_profile` / `VICTORY_SHADER_PROFILE` |
| BCPack tables | the format's lookup tables | `bcpack_tables` / `VICTORY_BCPACK_TABLES` |
| `local\play.toml` | the launcher's per-title settings | read by `scripts\play.ps1` |

The `local/` folder at the repository root is git-ignored and is the place for
them, and for your notes, logs and measurements. Caches and saves must be
outside the repository altogether; the runtime refuses to write them inside
it.

### Reporting a problem without breaking the rules

Describe what happens in your own words and in platform terms: which API,
which D3D12.X call, which instruction or shader construct, what you expected,
what the layer did. Quote **our** messages (a line of `victory check`, an
`error:` line, the name of an unimplemented import) after removing paths and
anything from the title. Do not name the title. Do not attach files.

### How the policy is enforced

1. `.gitignore` keeps common data types, logs and `local/` out of
   `git status`.
2. `cargo xtask policy` is an **allowlist** over every tracked and untracked
   (not ignored) file. A file passes only if its type is allowed (source,
   TOML, Markdown, text, scripts, workflow files), it is small (640 KiB; Rust
   source 768 KiB), it is text, it starts with no known binary signature, and
   it holds no long hex or base64 run. No file may name a user-profile folder
   (`C:\Users\<name>`, `/home/<name>`).
3. **The local deny-list.** If `local/policy-denylist.txt` exists, no file may
   contain any of its entries: one literal per line; words match as whole
   words, whatever their case; hex identifiers match anywhere. Put the names
   and identifiers of the titles you work with in it, so that one cannot slip
   into a commit by accident. The list is git-ignored and is never published;
   do not paste it or its matches anywhere.
4. The pre-commit hook (`cargo xtask install-hooks`) runs
   `cargo xtask policy --staged`. CI runs the full check.
5. At run time, everything that writes derived data calls
   `victory_core::ensure_outside_repo`.

If something slips through, a follow-up commit is not enough: tell the
maintainer at once, so that history can be rewritten.
[docs/asset-policy.md](docs/asset-policy.md) has the details.

## Coding conventions

- **Formatting.** `rustfmt.toml` sets the style (100 columns), but the tree is
  not uniformly formatted and CI does not run `cargo fmt --check`. Format the
  code you write or change; **do not reformat whole files** or mix formatting
  with a change, which makes history and upstream comparison unreadable.
- **Comments explain why.** A module opens with what console behaviour it
  reproduces and why the code is shaped as it is; a comment inside says what
  the code cannot say itself (a measurement, a failure that led here, a
  limitation). Do not narrate what a line does.
- **Mark approximations** with `shortcut:` and say what the full behaviour
  would be.
- **Unsafe code.** `unsafe_op_in_unsafe_fn` is denied; give every `unsafe`
  block a `// SAFETY:` comment.
- **Switches.** A change in runtime behaviour goes behind a `VICTORY_*`
  environment variable, with the previous behaviour still selectable, so that
  two runs can be compared and a regression can be switched off. Read it
  where it is used (`switch::on`, `toggled!`, see
  [ARCHITECTURE.md](ARCHITECTURE.md#configuration-and-switches)), read it
  once, and describe it in a comment there. If `scripts\play.ps1` should set
  it by default, add it to `Get-RunSettings`.
- **Game-agnostic code.** No branch on a title's identity. If one title needs
  different behaviour, find the platform rule that explains it, or make it a
  switch or an entry in a user-supplied file.
- **Unimplemented is loud.** A missing API or method should log what was
  called and fail, not pretend to succeed.
- **Credit.** When you adapt a change from an upstream fork, say so in a
  comment (`after upstream <project> <commit or pull request>`) and keep
  [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) accurate. After changing
  it or a dependency, run `scripts\third-party-licenses.ps1` to regenerate
  `THIRD_PARTY_LICENSES.md`.
- **Dependencies.** Add one only when needed, with a GPL-compatible license,
  pinned in `[workspace.dependencies]`, and commit `Cargo.lock`. Every build
  uses `--locked`.
- **Tests.** New logic comes with tests that need no title files: build the
  input in the test. Tests that can use real files take their paths from
  environment variables and skip when those are unset; they assert on
  properties, never on snapshots of title data.

## What every pull request must pass

Run these before you push; CI ([.github/workflows/ci.yml](.github/workflows/ci.yml))
runs the same on Windows.

```powershell
scripts\build.ps1 -Check -Test
```

`scripts\build.ps1 -Check -Test` is, in order:

```powershell
cargo test --locked --workspace
cargo xtask policy
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
powershell -ExecutionPolicy Bypass -File scripts\play.ps1 -SelfTest
```

The second clippy run covers the `diagnostics` feature, which compiles a good
deal of code the default build leaves out. If you changed that code, also run
`cargo test -p victory-gpu --features diagnostics`.

If you changed anything that reaches the release package (`scripts\`,
`xtask`, `victory.example.toml`, `QUICKSTART.txt`, dependencies), run
`scripts\package-release.ps1` once: it builds the hermetic binary, checks it,
and assembles the zip under `dist\` (git-ignored).

## Pull request workflow

1. For anything larger than a fix, open an issue first and say what you intend.
2. Branch from `main`. Keep a pull request to one subject; separate
   refactoring from behaviour changes.
3. Write commit messages that say what changed and why. If you measured
   something, give the method and the relative result, not a title's name or
   your PC's details.
4. Say in the description how you tested it and which switch selects the old
   behaviour.
5. Make sure the checks above pass. A pull request that fails the asset policy
   is closed, not fixed in place, if it put proprietary material into the
   repository's history.
6. By opening a pull request you state that the contribution is your own work,
   follows the clean-room rules above, and is licensed under
   GPL-3.0-or-later.

Rox-box is and stays non-commercial: it accepts no payment for features,
support or builds.
