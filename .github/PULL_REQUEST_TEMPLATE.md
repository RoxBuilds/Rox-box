## What changes, and why

<!-- One subject per pull request. Platform terms only: no title, publisher or title-file details. -->

## How it was tested

<!-- Which tests cover it (they build their own input, no title files). -->

## Which switch selects the old behaviour

<!-- A VICTORY_* switch, or "none: not a behaviour change" (documentation, refactor, test). -->

## Checklist

Run `scripts\build.ps1 -Check -Test` before you push (CI runs the same on Windows). It is, in order:

- [ ] `cargo test --locked --workspace`
- [ ] `cargo xtask policy`
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings`
- [ ] `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
- [ ] `powershell -ExecutionPolicy Bypass -File scripts\play.ps1 -SelfTest`
- [ ] If code behind the `diagnostics` feature changed: `cargo test -p victory-gpu --features diagnostics`
- [ ] If anything that reaches the release package changed (`scripts\`, `xtask`, `victory.example.toml`, `QUICKSTART.txt`, dependencies): `scripts\package-release.ps1` was run once
- [ ] A change in runtime behaviour is behind a `VICTORY_*` switch, with the previous behaviour still selectable
- [ ] Formatting applies only to the code I wrote or changed (no whole-file reformatting)

## Clean-room statement

- [ ] This contribution is my own work, written without proprietary or leaked code, SDK material, keys or game files, and without decompiled or disassembled title or system code.
- [ ] It names no title or publisher, and holds no hashes, offsets, byte patterns, shader names, logs, screenshots or video made from running one.
- [ ] I license it under GPL-3.0-or-later.
