# Upstream fork notes

> Historical design and feature notes adapted from the upstream
> `fxceless/victory-fork`, kept for reference. For the current project status,
> legal notices and build instructions see the root [README](../README.md);
> for how the code is organised, [ARCHITECTURE.md](../ARCHITECTURE.md).

Rox-box descends from [VIctory](https://github.com/Marvisak/VIctory) and from
forks of it. This file records what one of those forks added, because the
switches it introduced are still in the code.

**This repository contains no title files, code or assets.** You need your own
copy of any title you run.

## Upstream notes and contributors

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors (GPL-3.0-or-later): loader, runtime services, Direct3D 12 back end,
  and shader recompiler.
- Page-guard tracking across aliased views incorporates upstream contributions
  by **myokex** ([victory-amd](https://github.com/myokex/victory-amd)), Spexivus,
  joroyt, and tightfall, alongside concurrent kernel-read handling.

The full list of upstream projects and third-party code is
[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).

## Technical additions from this lineage

Descriptors and views:

- Descriptor records copied into heaps via plain CPU writes are mirrored before
  submissions.
- Unaligned buffer views receive isolated byte copies when hardware requires
  element alignment.
- Indirect compute dispatches are clamped at the hardware limit of 65,535
  groups per dimension.

Memory sync between title memory and the copies on the card:

- A write through one view of a physical page is seen by the copies taken
  through its other views (`VICTORY_VIEW_GUARD=0` turns it off). A title may
  show one page at several addresses; guarding a copy at only one of them
  leaves it stale after a write through another.
- Kernel reads into title memory are read beside and copied in, so a block
  guarded again in between no longer fails the read.
- Writes of the storage layer into physical pages are counted where the title
  has those pages mapped, and can open the guards there:
  `VICTORY_PAGE_WRITES=guards` (`1`, `slots`, `all` do more).
- Failed page-destination reads are retried (`VICTORY_DSTORAGE_RETRIES=0`: as
  before).

Card memory:

- Levels on demand (`VICTORY_LEVELS_ON_DEMAND`): a texture the title fills takes
  card memory only for the mip levels the title holds.
- Refused texture slots are held back for a few seconds so that a pass reaches
  the slots behind them (`VICTORY_FULL_CARD_PASS=0`: as before); room can be
  kept for the card copies of title buffers (`VICTORY_CARD_ROOM_FOR_BUFFERS`).

Rendering and translation:

- Buffer views are created as the kind the shader reads them as.
- Per-draw descriptor tables are recycled so that draws do not run out of slots.
- Light packed by compute shaders into integer textures is read back through
  aliased views, for every mip level and layer.
- Gathers at a named mip level (with and without an offset) read that level.
- Implicit-level samples outside pixel shaders are lowered to valid DXIL.
- Rectangle lists, depth and stencil twins for depth textures written by
  compute, and texture read-backs.
- Ray tracing, in progress and off by default: see
  [fork-ray-tracing.md](fork-ray-tracing.md).

Speed and memory:

- Waits for memory values, write-watch queries and page queries no longer walk
  page tables per call.
- Large title buffers get a copy in card memory, with shader writes found by
  hashing on the card.
- Submissions can be sent ahead of the wait for the card (`VICTORY_LATE_WAIT`),
  with a relay thread that lands results as the card finishes (`VICTORY_RELAY`).
- Independent host-copy refreshes run in parallel on systems with at least four
  logical CPUs (`VICTORY_WIDE_REFRESH=0` disables this; `=1` forces it).
- Title memory can be opened as a graphics heap so that buffers are used in
  place instead of copied (`VICTORY_INPLACE`, first stage only).
- The pipeline library is mapped instead of read into memory, and its file
  handle stays open while it is mapped.
- Translated shaders get clean-up passes over the finished module (dead code,
  constant folding, reuse of values, branch facts); `VICTORY_SHADER_OPT=0`
  turns them off.

Tools and diagnostics:

- `victory shaders [paths] --jobs N` translates every shader found into the
  shader cache ahead of a run (`victory translate [paths] --warm <file>` does it
  for the shader hashes listed in a file, `--warm all` for every one). The
  shader cache is keyed on the translator's source, so a new build that does
  not touch the translator starts warm.
- Diagnostics-only counters: `VICTORY_WALLS_TRACE`, `VICTORY_CARD_ACCOUNTING`,
  and a census of the copies the texture sweep never lets go.

Each switch is described in a comment where it is read. Some approximations of
console behaviour are marked `shortcut:` in the code. The diagnostics switches
are listed in [debugging.md](debugging.md).
