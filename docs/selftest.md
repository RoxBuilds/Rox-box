# The self-test

```powershell
victory selftest            # on the graphics card
victory selftest --warp     # on Windows' software adapter (WARP)
victory selftest --bench    # also time a few of the layer's hot paths
victory selftest --frames 8 # fewer frames in stage 2 (default 120)
```

`victory selftest` runs the layer's real frame path end to end with a
stand-in "title" made entirely of our own code. It needs no config file, no
game folder and no title files, and it writes one file of ours: its
[report](#its-report). No cache, no log file, no crash report (the graphics
driver may keep its own shader cache, as it does for any program). A small
window (64x64) opens for a moment.

The stand-in calls only what a title's calls arrive at. Its device comes from
the `d3d12_xs.dll!D3D12XboxCreateDevice` export, looked up in the runtime's
registry as a title's import is bound (`apps/victory/src/selftest.rs`), so the
runtime installs its memory allocator first, exactly as for a title. Every
object after that is reached through the function tables the device hands out,
at the offsets a title's code calls them (the device is created as
`ID3D12Device11`, so the queues and lists have the newer layouts current titles
see). The structures it passes are laid out as on PC, which is what a title
passes. Only the checks look behind the tables: a picture is read back from
the host texture the layer made for the title's render target, and the swap
chain is asked how many presents it took.

The driver is `crates/victory-gpu/src/selftest.rs`; the title side it drives
(the PC-layout structures, the table offsets, the calls and the scene) is
`crates/victory-standin`, which the open demo title links too
([demo-title.md](demo-title.md)); the shader containers come from
`crates/victory-shader/src/native/synth.rs`.

## Reading the output

Each stage prints one line in the style of `victory check`:

```text
[  OK   ] Device            <adapter>, driver <version>, <MB>; queue, list, fence and a 64x64 window
[  OK   ] Clear + present   120 frames cleared and presented; 3 of them read back and matched
[  OK   ] Unified memory    ...
[ SKIP  ] Translated shader the adapter has shader model 6.5; translated shaders need 6.6
[ SKIP  ] Translated draw   the adapter has shader model 6.5; translated shaders need 6.6
```

`FAIL` lines say what was expected and what was found, in our words. The run
ends with a summary; after a failure it names the first failing stage and what
to do: update the graphics driver, then run `victory selftest --warp`. If the
software adapter passes where the card failed, the driver (or the card) is at
fault; if both fail, the layer is, and the `FAIL` line is what to report (it
holds nothing from any title).

Exit codes: `0` every stage passed (skips allowed); `1` a stage failed; `2` a
wrong command line; `3` there is no Direct3D 12 device at all; `4` a step got
no answer from the GPU for 60 s (a hang); `5` the host GPU failed (device
removed), which ends the run at once as it would end a title's.

## Its report

When it ends (however: passed, failed, no device, hung, GPU failed) the
self-test writes a report through the run report's renderer and privacy
guard, to `<data_dir>\reports\selftest-<UTC time>.txt`: with no config file
the default data folder (`%LOCALAPPDATA%\victory\data`), as for a run;
`VICTORY_DATA_DIR` or the config file's `data_dir` elsewhere. The line naming
it goes to stderr, never stdout, because the path names your folders.
`victory report` shows the newest report, which is then this one, and
`victory report --diff` compares the two newest self-tests. It holds the
PC's facts a run report holds (Windows, processor, memory, the graphics
adapter and driver), each stage's name and outcome (`ok`, `fail`, `skip`)
with our fixed reason for a skip, and the benchmark's rates as numbers: none
of the text the stages print, which is built at run time. Like a run report
it may be attached to an issue as it is
([run-report.md](run-report.md#the-self-tests-report) lists its keys).
`VICTORY_REPORT=0` writes none.

## What each stage proves

1. **Device.** The runtime's device creation works, the host Direct3D 12
   device comes up (feature level 12_0; with `--warp` on the software adapter,
   which is checked), and a command queue, allocator, list and fence are made
   through the device's table. One empty frame is presented, so the swap chain
   and the window exist.
2. **Clear + present.** For `--frames` frames, a list clears the render target
   to a colour that changes every frame, goes through `ExecuteCommandLists`
   (`queue.rs::execute`), is presented through the queue's `PresentX`
   (`queue.rs::present`, then `Host::present`), and the frame is waited for
   with the fence. The first, the middle and the last frame are read back and
   every pixel compared; the swap chain must have taken every present.
3. **Unified memory.**
   - A 1 MiB buffer the title fills through `Map` is copied by a list into
     another title buffer, which the title then reads through its own `Map`;
     the title changes three stretches of the source (a page, 100 bytes in
     another page, the last 17 bytes) and the next submission's copy must
     carry them. Neither buffer has a host copy, so the layer copies at
     submission (`HostList::defer_copy`).
   - A 64x64 RGBA8 texture is written by the title in the console's own tiled
     layout (`victory_formats::texture`, the 4 KiB standard swizzle the layer
     assumes for a texture of layout 0), copied into the render target
     (`texture.rs` detiles it on the way), presented and read back; then the
     title rewrites a 16x16 block, storing only the bytes that change (so only
     those pages are written, as when a title streams part of a texture), and
     the next copy must show the block. This exercises write watching
     (`watch.rs`) and the texture refresh.
4. **Translated shader.** A compute program (one group of 64 threads, wave32)
   is assembled from the public GFX10 encodings, wrapped in a container
   exactly as the parser reads one (`XDAT` index, `XBCH`-packed `FSSM` record,
   root layout and used-resource tables), and given to the device's
   `CreateComputePipelineState` with a root signature serialized through the
   title's `D3D12SerializeRootSignature` import. The list binds it, sets four
   root constants and a descriptor table holding a raw UAV of a title buffer,
   and dispatches. The host pipeline is made on first use by the pipeline
   workers (`pipeline/make.rs`), the container is translated by
   `shader_cache::host_shader` and `victory_shader::emit::translate`, and the
   stores must land in title memory (`land.rs`), checked word by word, twice
   (other constants the second time). The card's time of those dispatches must
   have been measured by the split of the card's time by kind
   (`host/gpu_split.rs`, the run report's `[gpu_time]`): more than none, in
   runs counted; where the driver refuses timestamp queries on the queue, or
   with `VICTORY_GPU_SPLIT=0`, the stage says so and checks nothing of it.
   Last, a list copies a title buffer into
   that shader's buffer, which now has a host copy, so the copy is made on the
   card (`host/place.rs`) and must land too. Skipped where the adapter has no
   shader model 6.6.
5. **Translated draw.** A vertex program and a pixel program (wave32) are
   assembled from the public GFX10 encodings and wrapped in containers with
   the `ISG1`/`OSG1` signature parts they link by and their stage data. The
   vertex program takes x and y from its `POSITION` attribute (input row 0,
   which the fetch shader loads into v4-v5), exports `pos0` and, as `param0`
   (`COLOR`, row 1), the four root constants of its root slot 0. The pixel
   program interpolates that colour's red, green and blue (`v_interp_p1_f32`
   and `v_interp_p2_f32` at the pixel centre, `attr0`) and takes alpha from
   its own root constant (slot 1). Its code holds the colour export for a
   16-bit float target (compressed, which the translator refuses); the stage
   data's export patch for target 0 holds the one for 32-bit channels, which
   the layer applies (`NativeProgram::patched_code`), as the console does for
   the format a pipeline renders to. The two containers, the title's root
   signature, an input layout, the blend, rasterizer, depth-stencil,
   topology and target-format state go to the device's `CreatePipelineState`
   (0x2b0) as a pipeline-state stream, the description a title of the
   current generation uses (`pipeline::create_stream`; the plain graphics
   description at 0x50 keeps its containers only and gets no host pipeline).
   The host pipeline is made on first use (`pipeline/host.rs`), both programs
   translated by `shader_cache::host_shader`. A list then clears the render
   target and sets it (`OMSetRenderTargets`), a viewport, a scissor, the
   topology, the root signature, the pipeline, a vertex buffer in title
   memory (an upload-heap buffer the title fills through `Map`, copied to the
   card by the draw, `shadow.rs`) and each stage's root constants, all
   through the list's table, and draws one triangle over the target
   (`draw.rs`). It is presented and every pixel read back and compared; then
   a second list draws twice, with other constants, the second draw cut to
   the left half by its scissor, and both halves must show their own draw.
   Skipped where the adapter has no shader model 6.6.

## What it does not prove

- The path of a real run: the loader, import binding, the title's thread,
  its C runtime and exit, frame pacing, the shader cache on disk and the run
  report. The open demo title runs the same scene that way
  ([demo-title.md](demo-title.md)).
- Anything about a particular title: its shaders use far more of the
  instruction set than the one program here, and its frames far more state.
- Most of a graphics pipeline: the draw's programs read no texture, sampler,
  constant buffer or descriptor table; there is no index buffer, depth,
  stencil, blending or second target; the vertex program gets its attribute
  from the input layout (a title's vertex programs often fetch their own with
  typed buffer loads, which no stage here uses); the console's own
  topologies (rectangles, quads) and wave64 graphics programs are not drawn;
  the plain graphics description (0x50) is not used.
- Mesh, indirect, ray-tracing, tiled resources, depth, MSAA, formats other
  than RGBA8, more than one queue, waits between queues, the pipeline library
  and the shader cache on disk (the self-test uses neither, so it writes
  nothing but its report).
- Memory pressure: everything is small. A card that passes may still run out
  of memory with a title.
- Performance with a title: the benchmark measures the layer's own paths on
  small synthetic data.

## The benchmark (`--bench`)

Each rate prints as `name value unit`. They are meant to compare two PCs, or
two builds on one PC; run each a few times, as a busy PC moves them by tens of
percent. Debug builds are many times slower; compare release builds.

| Name | Unit | What is timed |
|---|---|---|
| `present_interval` | ms | One frame of stage 2: a 64x64 clear, its submission, the present and the fence wait (median, frames that were read back left out). The floor of the layer's per-frame cost, plus the driver's present. |
| `buffer_copy` | MB/s | 64 MiB that the title has just rewritten, copied by a list into another title buffer and waited for (median of 5). |
| `write_watch_query` | us/GiB | One `watch::writes` over 256 MiB of write-watched memory with one byte written every 64 KiB, scaled to a GiB: the kernel's answer and our journal. A title's refresh asks this every submission. |
| `avx2_compare` | GB/s | `safecopy::differ` over two equal 64 MiB buffers, counting both: the AVX2 compare that finds what changed. Mostly the PC's memory bandwidth. |
| `list_recording` | calls/s | 20,000 one-pixel `ClearRenderTargetView` calls through the list's table onto the host list: the cost of a command arriving as a call. |
| `shader_translation` | ms | The synthetic compute container to DXIL (`shader_cache::host_shader`). |
| `pipeline_creation` | ms | From the pipeline's first use to its host pipeline being ready: translation again plus the driver's compile. |
| `graphics_pipeline_creation` | ms | The same for stage 5's vertex and pixel pipeline: both translations plus the driver's compile. |

A rate that cannot be measured on the PC (no AVX2, no shader model 6.6) is
left out.

## The benchmark in CI

The Build workflow (`.github/workflows/build.yml`) records the benchmark on
every push and pull request, with the packaged `victory.exe` on the hosted
runner's software adapter, through `scripts/bench-record.ps1`. It records
and warns; it does not gate, because hosted runners are shared machines on
which the same build moves by tens of percent from one run to the next, and
a hard limit on a number would fail builds at random. The job fails only
when a run itself fails: a stage that says FAIL, a non-zero exit code, or a
demo title run whose report does not say `title_exited` with code 0.

What is recorded:

- The eight rates above, from five `selftest --warp --bench` runs: per rate
  the median, the minimum and the maximum.
- One run of the open demo title ([demo-title.md](demo-title.md), 600
  frames, in temporary folders, on the software adapter), from its run
  report ([run-report.md](run-report.md)): `average_fps`, `frame_ms_p50`,
  `frame_ms_p99`, `preparing_ms` and `waiting_for_card_ms` of the whole run,
  and how it ended.
- The Rox-box version and the adapter's name.

Where to find it: the Build run's job summary shows the table (`bench.md`),
and the run's `bench` artifact holds `bench.md` and `bench.json` (flat, sorted
by figure name, the same keys every time).

Comparing with the base commit's run needs a download of another run's
artifact, which needs a token and API calls; that is not built. Compare two
`bench.json` files by hand, from two runs' artifacts or two local runs:

```powershell
$old = Join-Path $env:TEMP "bench-old"
$new = Join-Path $env:TEMP "bench-new"
scripts\bench-record.ps1 -Victory target\release\victory.exe -Out $old
scripts\bench-record.ps1 -Victory target\release\victory.exe -Out $new -Compare "$old\bench.json"
```

`-Compare` prints per figure the old median, the new median and the change
in percent, marks a change beyond `-Threshold` percent (default 30) with a
`!` and a `::warning::` line (an annotation on a GitHub Actions run), and
exits 0 all the same. `-DemoTitle <demo-title.exe>` and `-DemoFrames <n>`
add the demo title as the workflow does, `-Runs <n>` changes the five, and
`-SelfTest` tests the script's own parsing, median and comparison functions
on text written in it. A figure of one run is a point in a trend, not a
verdict: look at several runs before believing a change.

## Tests

`apps/victory/tests/selftest.rs` runs the built binary with
`selftest --warp --frames 8 --bench` on every `cargo test --workspace` and in
CI, in a process of its own (the host device is one per process), with a
temporary data folder and no config file, and checks that the report
appears there, parses as TOML and holds every stage and the rates. Like the
crate's GPU tests it is skipped where there is no Direct3D 12 device at all
(exit code 3); a stage that is skipped (no shader model 6.6) does not fail it.
`victory_shader::native::synth`'s own tests check that its containers and
signature parts read back as written, that each instruction encoder decodes
back (with the crate's own decoder) to what it says it writes, and that the
programs translate (the pixel program only with its export patch).

## Adding a stage

1. Write what the title does in `crates/victory-standin`: a call through
   the tables is a method of `Title` (`slot::<F>(object, offset)` at the
   offsets the table modules list), a structure goes in `pc.rs` with the PC
   layout (and a check of it against the host's bindings in
   `structures_have_the_pc_layouts`), and a piece of the scene in
   `scene.rs`, checking only what lands in title memory. Then write the
   stage as a function in `crates/victory-gpu/src/selftest.rs` returning
   `Result<String, Failed>`: the `String` is the `OK` line's detail, the
   `Failed` reason the `FAIL` line's; the checks that look behind the
   tables (pictures read back, a pipeline's first use waited for) live
   there. Wait with `sync`; `Watchdog::pet` is called at every submission
   and wait, call it in other long loops.
2. Add its name to `STAGES` and call it from `run`; return
   `Outcome::Skip` with the reason where the PC cannot run it.
3. Programs come from `victory_shader::native::synth`: write them from the
   public instruction encodings (`synth::gfx10` has the few used so far), never
   from a title's code, and add a test there that the container translates.
4. Make it cheap: the integration test runs every stage on the software
   adapter.

A graphics program's container needs what the translator reads of it
(`synth::draw_vertex`, `synth::draw_pixel`):

- `ISG1` and `OSG1` (`synth::signature`): per element the semantic name and
  index, the system value, the component type, the register and the mask.
  The translator keeps them as they are in the host's signatures, so the
  vertex program's output rows and the pixel program's input rows must be
  the same rows for the host's stages to link. An attribute the stage data
  names by row must have that row in `ISG1`; `exp pos0` needs `SV_Position`
  in `OSG1`; `exp mrt<n>` needs `SV_Target` at register n.
- The stage data (`StageIo::Vs`, `StageIo::Ps` in `native.rs`): for a vertex
  program its inputs (row, channels, the VGPR channel 0 arrives in) and its
  outputs (row, `param<n>`); for a pixel program its inputs (row, `attr<n>`),
  `SPI_PS_INPUT_ADDR` (which barycentrics and system values arrive in which
  VGPRs, `io.rs`) and, per render target, the export patch the layer applies.
- The root layout: which user SGPRs hold which root parameter.
