# Third-Party Notices

This project (**Rox-box**) is distributed under the GNU General Public License
v3.0 or later (`GPL-3.0-or-later`; see [LICENSE](LICENSE)) and preserves the
following upstream and third-party copyright notices and license terms.

## Upstream Compatibility Layer Lineage

Rox-box is a modified fork based on the open-source **VIctory** codebase
(`GPL-3.0-or-later`) and incorporates architecture, compatibility fixes, and
shader lowering concepts from upstream contributors:

- **Marvisak/VIctory** (`GPL-3.0-or-later`) — original loader, HLE services,
  D3D12.X translation layer, and DXIL shader recompiler
- **fxceless/victory-fork** (`GPL-3.0-or-later`) — desktop optimizations,
  read-back, and descriptor/memory synchronization work
- **DexNom/VIctory** — ray-dispatch sizing, cube gradient, and sparse range fixes
- **Belobug/VIctory-Belobug-AMD** — background pipeline worker patterns,
  ambient lighting concept, and constant/pipeline tracking
- **FQY55kai/ZJOU-VICTORY** — render-target and shader translation improvements
- **myokex/victory-amd** — multi-view page guard tracking (incorporating work
  from Spexivus, joroyt, and tightfall)
- **DoubleGhosty/cool_fox** (`GPL-3.0`) — XMA audio framing, descriptor-table
  view typing, tracer EXEC guards, movie-plane uploads, and OOM backoff concepts
- **PalmVIDev** — copy-back of shader writes that keeps concurrent title
  writes (the run-wise merge credited in `crates/victory-gpu/src/shadow.rs`,
  upstream VIctory #2009)
- **trustno1-coder/VIctory-NVIDIA** (`GPL-3.0-or-later`) — shader writes into
  write-combined memory left on the card, and the descriptor-heap record
  sync switches (several by way of Belobug/VIctory-Belobug-AMD), credited per
  commit where they were ported

Individual upstream changes are credited where they were adapted, in code
comments of the form "after upstream ..." with the upstream commit or pull
request.

## Adapted Open-Source Components

### 1. AMD Mesa `addrlib` (MIT)

- **File:** [crates/victory-formats/src/texture.rs](crates/victory-formats/src/texture.rs)
- **Upstream:** Mesa (`src/amd/addrlib`: `gfx10addrlib.cpp`, `addrlib2.cpp`,
  `gfx10SwizzlePattern.h`)
- **Copyright:** `Copyright (C) 2007-2026 Advanced Micro Devices, Inc. All rights reserved.`
  (the header of those files as of Mesa commit `ed09d38`, the revision the
  layout tests were checked against)
- **License:** `SPDX-License-Identifier: MIT`; the full permission notice is
  reproduced in the file's header.

### 2. Valve Mesa ACO GFX10 Opcode Tables (MIT)

- **File:** [crates/victory-shader/src/gfx10/opcodes.rs](crates/victory-shader/src/gfx10/opcodes.rs)
- **Upstream:** Mesa (`src/amd/compiler/aco_opcodes.py`, `mesa-22.3.0`)
- **Copyright:** `Copyright (c) 2018 Valve Corporation`
- **License:** MIT; the full permission notice is reproduced in the file's
  header.

### 3. FFmpeg WMA Pro Decoder & Tables (LGPL-2.1-or-later -> GPL-3.0-or-later)

- **Files:**
  - [crates/victory-formats/src/xma.rs](crates/victory-formats/src/xma.rs)
  - [crates/victory-formats/src/xma/tables.rs](crates/victory-formats/src/xma/tables.rs)
- **Upstream:** FFmpeg (`libavcodec/wmaprodec.c`, `libavcodec/wmaprodata.h`)
- **Copyright:**
  - `wmaprodec.c`: `Copyright (c) 2007 Baptiste Coudurier, Benjamin Larsson, Ulion`;
    `Copyright (c) 2008 - 2011 Sascha Sommer, Benjamin Larsson`
  - `wmaprodata.h`: `Copyright (c) 2007 Baptiste Coudurier, Benjamin Larsson, Ulion`;
    `Copyright (c) 2008 - 2009 Sascha Sommer`
- **License:** `LGPL-2.1-or-later`, redistributed here under `GPL-3.0-or-later`
  as LGPL-2.1 Section 3 allows. Both files carry FFmpeg's notice and a
  statement of the changes (a Rust rewrite restricted to XMA2) in their
  headers.

## Clean-Room Re-implementations

- [crates/victory-loader/src/console_insn_decode.rs](crates/victory-loader/src/console_insn_decode.rs)
  is a clean-room x86-64 / AMD64 ISA decoder implemented directly from public
  AMD64 Architecture Programmer's Manual specifications (`CLZERO`, `RDPRU`,
  `MONITORX`, `MWAITX`, `CLWB`, `RDPID`, `EXTRQ`, `INSERTQ`, `UD2`), with no
  GPL-2.0-only third-party code.

## Cargo Crate Dependencies

Runtime and build dependencies declared in [Cargo.toml](Cargo.toml) (such as
`zlib-rs` under Zlib and `opus-pure` under BSD-3-Clause) remain governed by
their respective licenses as declared in each crate's manifest.
