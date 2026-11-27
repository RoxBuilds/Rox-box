# The open demo title

`tests/fixtures/demo-title` is a real 64-bit x64 executable built from our own
source that behaves as a small console title does. `victory run
demo-title.exe --execute` is therefore a whole run, the path every player's
run takes, that anyone may perform and share: the loader prepares and maps
it, its imports are bound, it starts on a title thread, makes its device,
renders, exits, and the run writes its report. Nothing in it comes from any
title: its code is ours, and its shader programs are assembled by
`victory_shader::native::synth` from the public instruction encodings.

It imports from `d3d12_xs.dll`, which no PC has, so started on its own it
stops at once with Windows' "DLL not found": only Rox-box runs it.

## Build and run it by hand

From the repository root, in PowerShell:

```powershell
# The demo title (its own lock file and target folder; it is no workspace member).
cargo build --locked --manifest-path tests\fixtures\demo-title\Cargo.toml --target-dir target\demo-title
scripts\build.ps1 -Release

# A game folder, a cache and a data folder of its own, outside the source tree
# (the runtime refuses to write below the repository's .victory-root marker).
$demo = Join-Path $env:TEMP "rox-box-demo"
New-Item -ItemType Directory -Force "$demo\game", "$demo\cache", "$demo\data" | Out-Null
Copy-Item target\demo-title\debug\demo-title.exe "$demo\game\"
$env:VICTORY_GAME_DIR = "$demo\game"
$env:VICTORY_CACHE_DIR = "$demo\cache"
$env:VICTORY_DATA_DIR = "$demo\data"
$env:VICTORY_D3D12_WARP = "1"   # the software adapter; leave it out to use the graphics card

target\release\victory.exe run "$demo\game\demo-title.exe" --execute -- --frames 300
target\release\victory.exe report
```

No `victory.toml` is needed: the three path variables are all a run reads
(run it from a folder that holds no `victory.toml`, or one would be read too).
The executable alone is a complete game folder: it ships no DLLs, and the
manifest a run reads beside the executable for the title's id and storage
quota (`MicrosoftGame.config`, `apps/victory/src/run.rs`) is optional; without
it the id is 0 and there is no quota.

Arguments after `--` are the title's (`--frames <n>`, default 300: five
seconds at the 60 frames a second it asks for). A small window (64x64) shows
the frames. The run ends with the demo's line

```text
demo title: 300 frames rendered and presented; every value it read back from its own memory matched
```

then Rox-box's line naming the run report, and exit code 0. A mismatch ends it
with `demo title failed: <what, where>` and exit code 1; a wrong argument with
exit code 2.

The report (`victory report`) should say `ended = "title_exited"`,
`exit_code = 0`, `presents` equal to the frames asked for, per frame one
submission, one dispatch and two draws (`[gpu_work]`), nothing in
`[gpu_work.left_out]`, three shaders translated and two pipelines made on the
first run (taken from the cache folder's shader cache and pipeline library on
the next), and nothing in `[unimplemented]`. It is safe to paste into an
issue like any run report: there is no title in it at all.

## What a minimal title has to do

What the demo does is what the runtime and the graphics layer expect of a
title before and while it renders (`tests/fixtures/demo-title/src/main.rs`):

- import `D3D12XboxCreateDevice`, `D3D12SerializeRootSignature` and
  `D3D12ConfigureVirtualMemoryX` from `d3d12_xs.dll` (served by
  `victory-hle/src/modules/d3d12x.rs`); everything else it imports is its C
  runtime's and Rust's standard library's (`kernel32`, `ntdll`,
  `vcruntime140`, the `api-ms-win-crt-*` and `api-ms-win-core-synch` sets),
  passed to the host by the default `HostPolicy` but for the few whose
  console meaning differs, which are ours (`kernel.rs`): the C runtime's
  argument list and its `exit` and `terminate`, module lookups
  (`GetModuleHandleW`, `GetProcAddress`, `LoadLibraryA`) and the
  unhandled-exception filter. `victory run` prints how many are ours and
  how many the host's (`bound` line); none may be missing;
- call `D3D12ConfigureVirtualMemoryX` before any graphics allocation, then
  create the device as `ID3D12Device11` (the runtime installs the title's
  memory allocator, `XMemAlloc`, as it creates the device);
- pace its frames with the device's `SetFrameIntervalX`,
  `ScheduleFrameEventX` and `WaitFrameEventX` (`victory-gpu/src/frame.rs`),
  and present each frame with the token its wait handed out. The wait
  counts frames, and the time a draw may wait for its pipeline to be made
  is a budget per such frame (`pipeline/make.rs`): a title that never waits
  for a frame spends one budget for its whole run;
- take its arguments from its C runtime: Rust's `std::env::args` reads the
  process's command line, which is Rox-box's;
- end by returning from `main`, so that its C runtime calls `exit` and the
  run report says how it ended (a direct `ExitProcess` would leave only the
  last periodic report).

## What it proves beyond the self-test

`victory selftest` drives the same scene in-process, with its imports looked
up in the runtime's registry ([selftest.md](selftest.md)). The demo title
adds everything a real run does around it:

- the loader: preparing an executable to be mapped as a DLL, mapping it away
  from its preferred base (relocations), redirecting its static TLS (Rust's
  thread-locals in an executable, `victory-loader/src/tls.rs`);
- import binding through `HleResolver`, with the default host policy;
- `spawn_main`: the title's thread, its C runtime's start-up and arguments,
  its `exit` (through ours), the loader's end of the process and its exit
  code (the first runs on a graphics card found the host's DLLs crashing in
  their process-detach after the loader had ended the other threads; the
  process now ends once the title images have detached,
  `victory-loader/src/threads.rs`, `VICTORY_EXIT_DETACH_HOST=1` for the old
  way);
- `preflight::before_execute`, on the adapter the run uses;
- frame pacing, the per-frame pipeline wait budget, the shader cache and the
  pipeline library on disk;
- the run report from a real run, which `apps/victory/tests/demo_title.rs`
  reads as its oracle.

## What it does not prove

- Pictures: a title cannot read its swap chain back, so the demo checks only
  what comes back into its own memory (the buffer copy, the dispatch's
  stores, the copy into the shader's buffer). The self-test reads the
  pictures of the same scene back from the host.
- Title DLLs, delay-loaded imports and TLS callbacks (the loader's own tests
  cover those with `tests/fixtures/hle-probe`), a C++ runtime, a statically
  linked C runtime.
- Any other area of the platform: files and storage, the game runtime,
  input, audio, threads of the title's own.
- Anything about a particular title, or about memory pressure: the scene is
  the self-test's, small, and its limits are the self-test's
  ([selftest.md](selftest.md#what-it-does-not-prove)).
- Speed: the frames are paced at 60 a second, so the report's frame times say
  how well the layer keeps up, not how fast it could go.

## Tests

`apps/victory/tests/demo_title.rs` builds the demo title into the test target
folder, copies it into a temporary game folder and runs `victory run
<demo-title.exe> --execute -- --frames 40` on the software adapter in a process
of its own, with temporary folders, no config file and no other `VICTORY_*`
variable. It asserts exit code 0, the demo's last line and the run report
(the values above). Like the self-test's, it is skipped where there is no
Direct3D 12 device at all: `victory run --execute` then exits with code 3.

## Adding to its scene

1. Write the piece once, in `crates/victory-standin/src/scene.rs`: what the
   title makes at loading, what it records into a list, and a check of what
   lands in title memory, reaching the layer only through `Title`'s
   methods (the tables) with PC-layout structures (`pc.rs`). Programs come
   from `victory_shader::native::synth`, written from the public
   encodings.
2. Record it in each frame of `tests/fixtures/demo-title/src/main.rs` and
   check it after the frame's wait, with one line of our own words on a
   mismatch.
3. Say what the report must now count in `apps/victory/tests/demo_title.rs`
   (draws, dispatches, shaders, pipelines per frame).
4. Use it in the self-test too (`crates/victory-gpu/src/selftest.rs`), with
   the checks that look behind the tables, so the two stay one scene.

The demo title has its own `Cargo.lock`, which may hold only packages of the
workspace's lock file, at the same versions. If the crates it depends on gain
a dependency, copy the workspace's `Cargo.lock` over it and build once with
`--offline` and without `--locked`: Cargo keeps the versions and drops what
the demo does not use.
