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
# The demo title and its two DLLs (each with its own lock file and target
# folder; none is a workspace member).
foreach ($f in "demo-title", "demo-static-dll", "demo-late-dll") {
    cargo build --locked --manifest-path tests\fixtures\$f\Cargo.toml --target-dir target\$f
}
scripts\build.ps1 -Release

# A game folder, a cache and a data folder of its own, outside the source tree
# (the runtime refuses to write below the repository's .victory-root marker).
$demo = Join-Path $env:TEMP "rox-box-demo"
New-Item -ItemType Directory -Force "$demo\game", "$demo\cache", "$demo\data" | Out-Null
Copy-Item target\demo-title\debug\demo-title.exe "$demo\game\"
Copy-Item target\demo-static-dll\debug\demo_static.dll "$demo\game\"
Copy-Item target\demo-late-dll\debug\demo_late.dll "$demo\game\"
Copy-Item target\demo-late-dll\debug\demo_late.dll "$demo\game\demo_twin.dll"
$env:VICTORY_GAME_DIR = "$demo\game"
$env:VICTORY_CACHE_DIR = "$demo\cache"
$env:VICTORY_DATA_DIR = "$demo\data"
$env:VICTORY_D3D12_WARP = "1"   # the software adapter; leave it out to use the graphics card

target\release\victory.exe run "$demo\game\demo-title.exe" --execute -- --frames 300
target\release\victory.exe report
```

No `victory.toml` is needed: the three path variables are all a run reads
(run it from a folder that holds no `victory.toml`, or one would be read too).
The executable, its two DLLs and `demo_twin.dll` (the late DLL's file once
more, under another name) are a complete game folder. `demo_static.dll`
must be there: the executable imports it, and a run refuses to start a title
whose own DLLs are missing (`victory run` prints `missing dlls`). A DLL is
the title's, and is mapped from the game folder, when its name is one that
neither the host policy passes to the host (`victory-hle/src/host.rs`) nor
Rox-box serves (`run.rs`, `title_dll`). The manifest a run reads beside the
executable for the title's id and storage quota (`MicrosoftGame.config`,
`apps/victory/src/run.rs`) is optional; without it the id is 0 and there is
no quota.

Arguments after `--` are the title's (`--frames <n>`, default 300: five
seconds at the 60 frames a second it asks for). A small window (64x64) shows
the frames. The run ends with the demo's lines

```text
demo title: its DLLs demo_static.dll (imported) and demo_late.dll (LoadLibraryW, GetProcAddress) were each attached once, ...
demo title: demo_twin.dll, a copy of demo_late.dll under another name, is a module of its own, freed and unloaded
demo title: its picture, copied into a buffer it maps (CopyTextureRegion), matched what it drew in every frame
demo title: 7 threads of its own (CreateThread, _beginthreadex, std::thread; ended by returning, ExitThread, _endthreadex, FreeLibraryAndExitThread) ...
demo title: demo_late.dll: its last FreeLibrary ran its process detach and unloaded it (...); loaded again, it was attached afresh, and unloaded again
demo title: 300 frames rendered and presented; every value it read back from its own memory matched
demo title: at its exit, each of its DLLs heard of the process's end once, and no thread's start or end came twice
```

then Rox-box's line naming the run report, and exit code 0. A mismatch ends it
with `demo title failed: <what, where>` and exit code 1; a wrong argument with
exit code 2. The last line is said by the executable's own TLS callback as the
process ends, after its DLLs' detach; a mismatch there ends the process at
once with code 1.

After its frames it can end as a title may, each with a code of 0 to 255
that the report's `exit_code` and `victory run`'s own exit code then say:
`--return <code>` returns it from `main` (its C runtime then calls `exit`),
`--exit <code>` calls the C runtime's `exit`, and `--exit-process <code>`
calls `ExitProcess`, which skips the C runtime's exit functions. With
`--alive` it so ends with everything alive (see below).

Before anything else it reads its arguments two ways, from its C runtime
(`__p___argc`, `__p___argv`) and from the process's command line
(`GetCommandLineW`, which Rust's `std::env::args` parses), and checks that
`GetCommandLineA` says the same line as `GetCommandLineW` and that the
program's name is its own file name with no folder: it stops with
`demo title failed: ...` (exit code 1) if not. Under Rox-box all of them are
`demo-title.exe` followed by the arguments after `--`
(`victory-hle/src/modules/kernel.rs`, `CommandLine`;
`VICTORY_TITLE_COMMAND_LINE=0` gives the host's line again, which the demo
then refuses).

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
  console meaning differs, which are ours (`kernel.rs`): the command line
  (`GetCommandLineW` and `A`) and the C runtime's argument list, its
  `exit` and `terminate`, `ExitProcess`, module lookups
  (`GetModuleHandleW` / `A`, `GetModuleFileNameW`, `GetProcAddress`,
  `LoadLibraryA` / `W`, `FreeLibrary`, `FreeLibraryAndExitThread`) and the
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
- end by returning from `main`, calling `exit` or calling `ExitProcess`:
  each is bound to ours, which records the end with its code before the
  title images detach and the process ends with that code
  (`VICTORY_TITLE_EXITS=0`: the host's `ExitProcess` and `_exit` again,
  which leave only the last periodic report).

## What it checks beside its frames

Every run does all of this (it costs a few milliseconds); each check fails
the run with one line of our words and exit code 1.

### Threads (`src/threads.rs`, `src/tls.rs`)

Seven threads of its own run beside the frames: three made with
`CreateThread` (one returns, one calls `ExitThread`, one gives back a
reference of its own on the late DLL with `FreeLibraryAndExitThread`), two
with the C runtime's
`_beginthreadex` (one returns, one calls `_endthreadex`), one with Rust's
`std::thread`, and a waiter made with `CreateThread` before the late DLL is
loaded. Each one, in the executable and in each DLL:

- finds its static TLS holding the image's TLS template's values at its
  start, and later only its own (they are all alive at once, so a slot shared
  between threads shows);
- was heard of by each image's TLS callback or `DllMain` as it started, once
  (the waiter: not by the late DLL, loaded after it started, as on Windows);
- uses a thread-local with a destructor.

They say their thread ids and wait for an event the main thread sets at the
second frame (the ids of threads alive at once must differ), fill their parts
of a buffer in title memory, which the main thread copies on the GPU at the
third frame and checks, and end. After joining them the main thread checks
their exit codes and, per thread, that each image heard of its end once and
that each TLS destructor ran once. The notes are kept per thread under a
token from the thread's first notice, not its thread id: Windows hands a
finished thread's id to the next thread, which may be one of the runtime's.

The executable's own TLS callback (`.CRT$XLC`) proves what the loader passes
on to an executable mapped as a DLL with its callbacks detached
(`victory-loader/src/threads.rs`); its thread-locals, which Rust reaches
through the TLS slot an executable assumes, prove the redirection of
`victory-loader/src/tls.rs` on several threads at once.

### Its DLLs (`src/dlls.rs`, `tests/fixtures/demo-static-dll`, `demo-late-dll`)

`demo_static.dll` is imported by the executable (so the loader maps it beside
it, binds its imports and runs its `DllMain` on the title's thread before the
entry point); `demo_late.dll` is imported by nothing, and the demo loads it by
name (`LoadLibraryW`), looks its exports up (`GetProcAddress`) and, at the
end, frees it. Both are one source (`demo-static-dll/src/dll.rs`), import
from `kernel32` themselves (their imports are bound like the title's: the
thread id from the host, their own file name from Rox-box), have static TLS
and a destructor, and note what `DllMain` hears. The demo checks:

- each was attached once, before its first use, on the thread that loaded
  it; `GetProcAddress` finds its exports and not one it lacks;
- thread notices for the threads made after it was loaded, once each, and
  its own TLS on each (above);
- `GetModuleHandleW` / `A` find each by its file name (any case, with or
  without `.dll`), the executable by its own and by `NULL`, all the same
  module as the instance handle `DllMain` got and as the code's
  (`GetModuleHandleExW`); `GetModuleFileNameW` names each DLL in the folder
  of the executable, the same whether the title or the DLL itself asks;
- references are counted as Windows counts them: a second `LoadLibraryW`
  of the late DLL gives the same module, not attached again; a thread that
  gives that reference back with `FreeLibraryAndExitThread` leaves it loaded;
  the last `FreeLibrary` runs its process detach and unloads it
  (`GetModuleHandleW` then finds nothing), and a `LoadLibraryW` after that
  maps and attaches it afresh (its statics start again), until its own last
  `FreeLibrary` (`victory-hle/src/modules/kernel.rs`, `title_dll_free`;
  `apps/victory/src/run.rs`, `LateDlls::free`). With
  `VICTORY_TITLE_DLL_FREE=0`, the old way, `FreeLibrary` of a title DLL does
  nothing and the demo says its detach comes at the process's end, but
  `FreeLibraryAndExitThread` is the host's, which unmaps the DLL under the
  loader: the next thread notice runs code that is gone, and the run
  crashes;
- `demo_twin.dll`, the late DLL's file under another name, loads as a module
  of its own (its own base and `DllMain`), as on Windows. Before, the
  prepared copies in the cache were named by their bytes alone, so both
  names mapped one module, whose C runtime was then initialised a second
  time: the process failed fast in the second `LoadLibraryW`, with no crash
  log and no end in the report (`victory-loader/src/windows.rs`, `map_dll`
  now names the copy by its bytes and its name);
- at the process's end (`tls.rs`, the executable's TLS callback, which the
  loader runs after the DLLs' detach): each DLL heard of the process's end
  exactly once, and no thread notice or destructor came twice anywhere.

What a title sees of its paths: `GetModuleFileNameW` gives the original file
in the game folder, as a full path on the host (a drive letter and every
folder above the game folder), for the executable and for its DLLs, never the
prepared copies in the cache folder. The command line gives only the
executable's name (`kernel.rs`, `CommandLine`); the module names do not.

### Its picture (`victory-standin/src/scene.rs`, `ReadBack`)

Each frame, after the draws, the render target is copied with
`CopyTextureRegion` into a placed footprint of a buffer on a read-back heap,
which the demo maps; after the frame's fence it compares every pixel with
what it drew (the two draws, the clear and the texture, within one step per
channel). The buffer is overwritten with a marker before each frame, so a
copy that did not land shows. The layer turns such a copy into a copy into a
buffer of its own and copies the texels into title memory once the list has
run (`victory-gpu/src/list.rs`, `read_back`; `VICTORY_READ_BACK=0` leaves it
undone, which the demo then reports).

### The end with everything alive (`--alive`)

With `--alive` and one of `--return`, `--exit` or `--exit-process`, two more
threads (one `CreateThread`, one `std::thread`) are left waiting on an event
nobody sets, the late DLL stays loaded, and a last frame is submitted and
presented without waiting for it; then the title ends. The run must end with
the title's code, the report must say `title_exited` with it, and no
`crashes.log` may appear. This is the case that crashed in the host DLLs'
process detach on a graphics card before the loader ended the process itself
(`victory-loader/src/threads.rs`, `detach_all`; `VICTORY_EXIT_DETACH_HOST=1`
gives the old way).

## What it proves beyond the self-test

`victory selftest` drives the same scene in-process, with its imports looked
up in the runtime's registry ([selftest.md](selftest.md)). The demo title
adds everything a real run does around it:

- the loader: preparing an executable to be mapped as a DLL, mapping it away
  from its preferred base (relocations), redirecting its static TLS (Rust's
  thread-locals in an executable, `victory-loader/src/tls.rs`);
- import binding through `HleResolver`, with the default host policy;
- `spawn_main`: the title's thread, its C runtime's start-up and arguments,
  its command line read both ways, its `exit` and `ExitProcess` (through
  ours), the loader's end of the process and its exit
  code (the first runs on a graphics card found the host's DLLs crashing in
  their process-detach after the loader had ended the other threads; the
  process now ends once the title images have detached,
  `victory-loader/src/threads.rs`, `VICTORY_EXIT_DETACH_HOST=1` for the old
  way), also with threads waiting, a DLL loaded and a frame in flight;
- the title's threads: thread start and end notices passed on by the loader
  to the executable's TLS callbacks and the DLLs' `DllMain`s, static TLS from
  the template on every thread, TLS destructors, the C runtime's and Win32's
  ways to make and end a thread;
- the title's DLLs: one mapped because it is imported, one by `LoadLibraryW`
  (`run.rs`, `LateDlls`), their own imports bound, their initialisers run on
  the right thread, their exports found, module lookups by name and handle,
  their process detach at the end;
- its picture, read back into its own memory as a title can;
- `preflight::before_execute`, on the adapter the run uses;
- frame pacing, the per-frame pipeline wait budget, the shader cache and the
  pipeline library on disk;
- the run report from a real run, which `apps/victory/tests/demo_title.rs`
  reads as its oracle.

## What it does not prove

- The swap chain: the picture it checks is its render target, read back
  before the present; what the window shows is not read (the self-test reads
  the host's copy of the target, which is what is presented).
- Delay-loaded imports (the loader's own tests cover those with
  `tests/fixtures/hle-probe`), a DLL that loads another from its `DllMain`,
  a `DllMain` that fails, `DisableThreadLibraryCalls`, a late DLL that
  imports another title DLL (whose references `LateDlls` also counts); a
  thread notice that comes while a DLL is being unloaded (the loader's
  shortcut: such a notice may reach the DLL after its process detach); a
  C++ runtime (its
  exceptions, `std::thread`), a statically linked C runtime.
- Fibers, fiber-local storage, thread pools, `TlsAlloc` slots, and threads
  of more than a few: the threads are seven (nine with `--alive`).
- Any other area of the platform: files and storage, the game runtime,
  input, audio.
- Anything about a particular title, or about memory pressure: the scene is
  the self-test's, small, and its limits are the self-test's
  ([selftest.md](selftest.md#what-it-does-not-prove)).
- Speed: the frames are paced at 60 a second, so the report's frame times say
  how well the layer keeps up, not how fast it could go.

## Tests

`apps/victory/tests/demo_title.rs` builds the demo title and its two DLLs
into the test target folder (once per test process), copies them into a
temporary game folder (the late DLL a second time, as `demo_twin.dll`) and runs `victory run <demo-title.exe> --execute --
--frames 40` on the software adapter in a process of its own, with temporary
folders, no config file and no other `VICTORY_*` variable. It asserts exit
code 0, the demo's lines above (each check's, and the one said at the
process's end), both DLLs mapped as the title's with every import bound, no
`crashes.log`, and the run report (the values above, and `[frame_time]`
`frames` equal to the presents, the first one allowed for). Three more runs
of 10 frames end through `--exit-process 5`, `--return 7` and `--exit 9`, and
three more the same with `--alive`: each must end `victory run` with that
code, say the same lines, leave no crash log and a report that says
`title_exited` with the code. Like the self-test's, they are skipped where
there is no Direct3D 12 device at all: `victory run --execute` then exits
with code 3.

The end with everything alive on a graphics card is not in the tests (CI has
none); run it by hand as above, without `VICTORY_D3D12_WARP`, with
`-- --frames 10 --alive --exit 9`.

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

Its DLLs depend on no crate: their lock files name only themselves. The
demo title has its own `Cargo.lock`, which may hold only packages of the
workspace's lock file, at the same versions. If the crates it depends on gain
a dependency, copy the workspace's `Cargo.lock` over it and build once with
`--offline` and without `--locked`: Cargo keeps the versions and drops what
the demo does not use.
