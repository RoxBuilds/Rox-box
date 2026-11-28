# Debugging and the diagnostics build

A release build has few debugging aids: it logs `info` and above, and the
tools that record or dump a run are compiled out. Build them in with

```powershell
scripts\build.ps1 -Diag        # target\diag\victory.exe
```

(`cargo build --locked --profile diag -p victory --features diagnostics`).
`scripts\play.ps1` prefers that build when it exists; `-Build release` picks
another.

Everything a run writes (logs, captures, dumps) contains material from the
title. Keep it outside the repository and do not post it
([asset-policy.md](asset-policy.md)).

## Logs

- `scripts\play.ps1 -Log` writes the run's output to
  `%LOCALAPPDATA%\victory\logs`, with the switches it set at the top.
- Every build appends crashes and host GPU failures to `crashes.log` in the
  title's data folder (`%LOCALAPPDATA%\victory\data\<title id>\`), with the
  fault as `module+rva` and the faulting thread's stack.
- `VICTORY_LOG=victory_loader=trace,info` filters logging by target
  (`RUST_LOG` is the fallback). Debug and trace output exist only in a
  diagnostics build.

`play.ps1` clears the `VICTORY_*` variables it does not set itself: pass
switches with `-More "NAME=value;NAME2=value2"` (from `play.cmd` or
PowerShell; a hashtable also works from PowerShell), or run
`victory.exe run <title.exe> --execute` directly.

## Switches

The on/off switches (`...=1`) are off when unset or set to `0`. Most of these
do something only in a diagnostics build.

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a 32-bit value of the title image when it changes |
| `VICTORY_POKE=<rva>:<byte>,...` | writes a byte of the title image twice a second |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves ranges of the title image to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_CAPTURE=<dir>` with `VICTORY_CAPTURE_EVERY_S=<seconds>` | in any build, not only diagnostics: a half-size picture of the presented frame every that many seconds as `shot-<seconds>s.png` (seconds counted from the first presented frame), at most 120 per run, in a subfolder of `<dir>` named by the run's start in UTC digits (earlier runs' files are left alone). `play.ps1 -Shots <seconds>` sets both with `<data_dir>\shots`. Unset, it costs nothing; set, the frame it takes waits for the card once. A folder inside the source tree is refused; any failure is logged once and ends the pictures, never the run. The pictures are not in the run report |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight (`play.ps1` sets `light`) |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver), in every build; `run --execute` then checks that adapter before it starts |
| `VICTORY_PANEL=1` | the run's numbers drawn on the picture; F3 shows and hides it |
| `VICTORY_PIPELINE_WAIT_MS=24` | the per-frame wait for background pipeline creation (default 500 ms). A draw is left out until its pipeline is ready |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | leaves out draws and dispatches whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane (own shader cache entries) |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first XMA voices' context and input |
| `VICTORY_SKIP_PREFLIGHT=1` | `run --execute` starts whatever the host lacks |
| `VICTORY_SKIP_CPU_CHECK=1` | skips the AVX2 check (under an instruction emulator, say) |

Each switch is described in a comment where it is read: search the source for
its name, or browse [switches.md](switches.md), generated from those comments. A whole frame can be recorded and run again without the title: see
[replay.md](replay.md).

A heavy workload or a shader that translates wrongly can make Windows reset
the graphics driver (a TDR) or lose the device; the run then ends with a line
in `crashes.log`. `VICTORY_D3D12_WARP=1` renders on the software adapter
instead, which cannot reset the driver.

## The DXIL validator in shader tests

The shader tests and `victory translate --validate` use `dxil.dll` to validate
Shader Model 6.6 containers. An older Windows SDK's validator can load
successfully and then reject them with `Unknown shader model 'ps_6_6'`. Use a
current Windows SDK, or a current x64 validator from
[Microsoft's DirectX Shader Compiler releases](https://github.com/microsoft/DirectXShaderCompiler/releases),
and name it for the session:

```powershell
$env:VICTORY_DXIL = 'C:\tools\dxc\bin\x64\dxil.dll'
cargo test -p victory-shader --lib --locked -- --test-threads=1
```

Without the variable, the newest installed Windows SDK's
`bin\<version>\x64\dxil.dll` is used, then the DLL search path. This is a
test and offline-validation dependency only: a run does not need it, and it
is never bundled.
