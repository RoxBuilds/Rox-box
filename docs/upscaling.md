# The host upscaler

Some titles upscale their picture with AMD FidelityFX: they render at a lower
resolution and call the public FidelityFX API of an upscaler DLL they ship,
built for the console. Run as translated compute, that console upscaler is
very costly on a PC: on a mid-range laptop card one title spent about 190 ms
of each frame in it. A title's simpler built-in upscaler is cheap, but it does not
gather detail over several frames, so hair, foliage and soft shadows stay
speckled and aliased.

The host upscaler takes the title's FidelityFX upscale and runs it on an
upscaler made for the PC instead: AMD FSR 3.1, from a DLL **you supply**. The
title's own upscaler stays there to fall back on, so a run without the DLL
behaves exactly as before.

**Nothing proprietary ships with Rox-box.** The DLL is AMD's, downloaded by
you from AMD's own release. It is never in the Rox-box repository or a Rox-box
release, and Rox-box never downloads it.

## What to do

1. Download **FidelityFX SDK v1.1.4** (or another v1.1.x) from AMD's GPUOpen
   release page on GitHub: `GPUOpen-LibrariesAndSDKs/FidelityFX-SDK`, release
   `v1.1.4`, the package `FidelityFX-SDK-v1.1.4.zip` (the release that has the
   pre-built, signed DLLs). Read and accept AMD's licence in it.
2. Take **`amd_fidelityfx_dx12.dll`** out of the package (search the unpacked
   folder for that name; use the release build, not a debug one). It is the
   only file needed. The DLLs of SDK 2.x, split into several, are not this
   one.
3. Put it in one of these places. They are looked in, in this order, and the
   first that has it is used:
   1. the path in `victory.toml`, `[paths] upscaler_dll = '...'` (the DLL, or
      the folder it is in), or in the environment variable
      `VICTORY_UPSCALER_DLL`, which wins over the file;
   2. a folder named `vendor` next to `victory.exe`
      (`<Rox-box folder>\vendor\amd_fidelityfx_dx12.dll`);
   3. a folder named `vendor` in Rox-box's data folder (`data_dir`; by default
      `%LOCALAPPDATA%\victory\data\vendor\amd_fidelityfx_dx12.dll`).
4. Run the title as usual, with its FidelityFX upscaling chosen in its
   settings, if it has the choice.

That is all: the host upscaler is on whenever the DLL is found.
`VICTORY_HOST_UPSCALER=0` turns it off (then every FidelityFX call goes to the
title's own DLL), to compare.

## How to tell it ran

The log says, once each:

- `host upscaler: found the upscaler DLL` (with its path), or
  `host upscaler: no amd_fidelityfx_dx12.dll found` with the places looked at;
- `host upscaler: the title's FidelityFX entry point is bound to ours` for
  each of the three entry points the title uses;
- `host upscaler: ready` with the FSR version, and
  `host upscaler: a context` with the sizes the title asked for;
- `host upscaler: the first upscale ran on the host` with its numbers.

Whenever the title's own upscaler runs instead, a line says why:
`host upscaler: the title's own upscaler runs: <reason>` (once per reason).

The run report's `[upscaler]` table ([run-report.md](run-report.md)) has the
numbers: whether the host upscaler ran, how many upscales ran on it and how
many on the title's own, failures, and the card's time per upscale.

## What it does

- When a title module imports `ffxCreateContext`, `ffxDestroyContext` or
  `ffxDispatch` from any DLL, or looks one up with `GetProcAddress`, it is
  bound to a shim of ours (`victory_hle::modules::fidelityfx`). Only the
  API's public names are matched; nothing about any title is known.
  `ffxConfigure` and `ffxQuery` stay bound to the title's DLL.
- Every context is still created by the title's own DLL. A context created
  for upscaling is noted, with its largest render and output sizes.
- At the first upscale dispatch of such a context, a host FSR context of the
  same sizes and flags is made on Rox-box's host D3D12 device. From then on
  each upscale dispatch becomes a step in the title's command list: where the
  title's dispatch would have run, its colour, depth, motion vectors,
  exposure, reactive and transparency masks are brought up to date and made
  readable, the output writable, and the FSR dispatch is recorded on the host
  command list. Jitter offset, motion-vector scale, sizes, near and far
  planes, field of view, frame time, pre-exposure, sharpening and the reset
  flag pass through as the title gives them: both sides speak the same public
  API description, so nothing is converted.
- Anything else (another effect, a query, a configuration) goes to the title's
  DLL unchanged.

### When the title's own upscaler runs

- No DLL found, a DLL that does not load, lacks an export, or offers no FSR
  3.1 or later: nothing is bound, or every call goes to the title's DLL.
- A host context that cannot be made: that context's upscales are the
  title's.
- A dispatch the host cannot take (a missing input, a size beyond the
  context's, a texture with no host copy): that dispatch is the title's.
- A host dispatch that fails: the title's for that context from then on. With
  `VICTORY_THIN=1` the host dispatch is recorded later than the title's call,
  so the frame of the failure is left without an upscale; the next ones are
  the title's.

## For developers

The code is `crates/victory-gpu/src/upscale.rs` (with `upscale/ffx.rs`, the
API's structures written from the layouts of the SDK's public, MIT-licensed
headers; `upscale/find.rs`, where the DLL is looked for; `upscale/fsr.rs`, the
FSR backend) and `crates/victory-hle/src/modules/fidelityfx.rs`. A second
backend (an upscaler reached through another vendor's interposer DLL, also
supplied by the user) would be another variant of `upscale::Backend`; none is
built.
