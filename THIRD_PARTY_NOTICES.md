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
  write-combined memory left on the card, HID gamepad input (by Blitzball996
  there), and the descriptor-heap record
  sync switches (several by way of Belobug/VIctory-Belobug-AMD); picture
  fixes: the BC7 rotation field of compressed textures, the words placed
  views keep (from JMAN730/project-flamingo), raw buffer views kept inside
  their buffer (from fxceless/victory-fork by way of CuteBooty/victory-8gb),
  the order texture uploads and slots are shown in and the texture queue's
  marks, the console's early depth test, a stand-in for reflections without
  rays and the ambient stand-in's colour filter, and console threads given
  from a group counter (from Vice-Tay); credited per commit where they were
  ported; and the design of the host upscaler (its `docs/dlss.md`): binding a
  title's imports of the FidelityFX API to shims that keep the title's own
  DLL for everything else, taking only the upscale dispatch, and recording
  it as a step in the title's command list (in the list's written-down
  stream with `VICTORY_THIN=1`); written anew here for an FSR backend
  (`crates/victory-gpu/src/upscale.rs`,
  `crates/victory-hle/src/modules/fidelityfx.rs`)

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

### 4. AMD FidelityFX SDK API layouts (MIT)

- **File:** [crates/victory-gpu/src/upscale/ffx.rs](crates/victory-gpu/src/upscale/ffx.rs)
- **Upstream:** FidelityFX SDK v1.1.4 (`ffx-api/include/ffx_api/ffx_api.h`,
  `ffx_api_types.h`, `ffx_upscale.h`, `dx12/ffx_api_dx12.h`), by AMD
  (GPUOpen-LibrariesAndSDKs/FidelityFX-SDK)
- **Copyright:** `Copyright (C) 2024 Advanced Micro Devices, Inc.`
- **License:** MIT (the SDK's `LICENSE.txt`). The structures, constants and
  the DXGI-to-surface-format table are written in Rust from the headers'
  public layouts and values, which an implementation of the API must agree
  on; no header text is copied. The permission notice, in the standard MIT
  wording (the SDK's own `LICENSE.txt` is the text that governs; its wording
  differs from this only in spacing and punctuation):

  > Permission is hereby granted, free of charge, to any person obtaining a
  > copy of this software and associated documentation files (the
  > "Software"), to deal in the Software without restriction, including
  > without limitation the rights to use, copy, modify, merge, publish,
  > distribute, sublicense, and/or sell copies of the Software, and to permit
  > persons to whom the Software is furnished to do so, subject to the
  > following conditions:
  >
  > The above copyright notice and this permission notice shall be included
  > in all copies or substantial portions of the Software.
  >
  > THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
  > OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
  > MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
  > NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
  > DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
  > OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
  > USE OR OTHER DEALINGS IN THE SOFTWARE.

- **Not included:** the FidelityFX DLLs themselves. The host upscaler loads
  `amd_fidelityfx_dx12.dll` at run time from where the user put it
  ([docs/upscaling.md](docs/upscaling.md)); it is never in this repository or
  a release, and its own licence is the user's to accept.

## Clean-Room Re-implementations

- [crates/victory-loader/src/console_insn_decode.rs](crates/victory-loader/src/console_insn_decode.rs)
  is a clean-room x86-64 / AMD64 ISA decoder implemented directly from public
  AMD64 Architecture Programmer's Manual specifications (`CLZERO`, `RDPRU`,
  `MONITORX`, `MWAITX`, `CLWB`, `RDPID`, `EXTRQ`, `INSERTQ`, `UD2`), with no
  GPL-2.0-only third-party code.

## Installer

The Windows setup program (`Rox-box-<version>-windows-x64-setup.exe`) is built
with [Inno Setup](https://jrsoftware.org/isinfo.php) by Jordan Russell and
Martijn Laan. Its license permits building and distributing installers with it,
including for GPL-licensed programs; the setup stub inside the installer is
Inno Setup's own code, under that license. The installer script is
[dist/installer/rox-box.iss](dist/installer/rox-box.iss) and installs only
Rox-box's own files.

## Cargo Crate Dependencies

Runtime and build dependencies declared in [Cargo.toml](Cargo.toml) (such as
`zlib-rs` under Zlib and `opus-pure` under BSD-3-Clause) remain governed by
their respective licenses as declared in each crate's manifest.
