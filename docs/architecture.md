# Architecture notes: import resolution and shader translation

The overview of the codebase (crates, the path of a run, caches, switches) is
[ARCHITECTURE.md](../ARCHITECTURE.md) at the repository root. This file goes
deeper into two subjects that the overview only names.

Compatibility: the layer targets x86-64 PE32+ titles built for the Xbox
Series X|S family and its Direct3D 12.X API. Xbox is a trademark of the
Microsoft group of companies; this project is independent and is not
affiliated with or endorsed by Microsoft or any publisher.

## Import resolution

For every `dll!function` the title imports, in order:

1. our implementation in `victory-hle::modules` (always wins);
2. the host's own function, if `HostPolicy` allows that DLL;
3. an export of a DLL the title ships beside its executable, mapped like
   the executable (only DLLs neither of the above serves);
4. a generated stub that names the import when called (`--unimplemented
   trap` breaks into the debugger and aborts; `zero` logs once and returns 0).

`LoadLibrary*` of a console-only DLL we implement (e.g. `xgameruntime.dll`)
returns a pseudo handle; `GetProcAddress` on it finds our functions.
`LoadLibrary*` of a title DLL returns its mapped image, mapping it first if
nothing imports it. By name, as by import, a title gets only host DLLs
`HostPolicy` passes through, and `GetProcAddress` on one returns our
override where import binding would.
`QueryApiImpl` hands out our runtime objects by provider and interface id
(`victory-hle::modules::xgameruntime` lists the areas); online areas answer
as an offline console. Game saves are files under the data folder
(`data_dir`).

Title storage is a drive on the console, and titles build paths on that.
`victory-hle::modules::files` binds every path-taking title import
(kernel32 and the C/C++ runtime) and rewrites paths on a virtual drive to
the host folder behind it; persistent local storage is one such drive over
a folder under the data folder. Other paths reach the host unchanged.

An exe's code reaches its static TLS as slot 0; mapped as a DLL it gets
another slot, so the loader redirects those loads (`victory-loader::tls`).
`victory run --execute` installs a crash reporter that names faults as
`module+rva` and unwinds the faulting thread's stack
(`victory-loader::crash`).

`victory census` shows this classification without running anything (it
does not map title DLLs; `victory run` without `--execute` does).

## Shaders

Shaders are translated when the title first draws with a pipeline, from the
bytes it passed when it created the pipeline, wherever it loaded them from
(executable, archive, memory). Translating them ahead of time from files
(`victory shaders`) is only a warm-up of the same cache.

`victory_shader::ir::build` turns a native program into SSA, one
instruction per machine instruction. `victory_shader::structure` then
removes EXEC: lane masks become per-lane booleans, wave-wide mask tests
become per-lane branches, and vector instructions keep a guard only where
a lane can reach them with its EXEC bit clear. `victory_shader::resource`
traces each descriptor and scalar memory address to the root parameter it
comes from (table, heap, root descriptor or constant). A descriptor a
waterfall loop reads from the first lane (`v_readfirstlane_b32`) is traced
to the per-lane load or choice that gives each lane its own (a mask's
`v_cndmask_b32`, or an if/else whose arms each set it: traced on the paths
the lanes take, from structurisation), and accessed with a non-uniform heap
index in one pass. `victory translate` runs every shader it finds (containers,
packed shader archives) through these and lists what they cannot handle yet.

What the host binds for a translated shader (user data at `b0`, descriptors
through Shader Model 6.6 direct heap indexing at the title's own descriptor
positions, immediate samplers as static samplers) is specified in the header
of `crates/victory-shader/src/emit/mod.rs`.

The console's waves are 64 lanes wide. On a host card whose waves are at most
32 lanes, programs that depend on the lane layout need other forms; those are
described in [low-memory-gpus.md](low-memory-gpus.md).
