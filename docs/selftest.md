# The self-test

```powershell
victory selftest            # on the graphics card
victory selftest --warp     # on Windows' software adapter (WARP)
victory selftest --bench    # also time a few of the layer's hot paths
victory selftest --frames 8 # fewer frames in stage 2 (default 120)
```

`victory selftest` runs the layer's real frame path end to end with a
stand-in "title" made entirely of our own code. It needs no config file, no
game folder and no title files, and it writes nothing of ours: no cache, no
log file, no crash report (the graphics driver may keep its own shader cache,
as it does for any program). A small window (64x64) opens for a moment.

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

The driver is `crates/victory-gpu/src/selftest.rs`; the shader container comes
from `crates/victory-shader/src/native/synth.rs`.

## Reading the output

Each stage prints one line in the style of `victory check`:

```text
[  OK   ] Device            <adapter>, driver <version>, <MB>; queue, list, fence and a 64x64 window
[  OK   ] Clear + present   120 frames cleared and presented; 3 of them read back and matched
[  OK   ] Unified memory    ...
[ SKIP  ] Translated shader the adapter has shader model 6.5; translated shaders need 6.6
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
   (other constants the second time). Last, a list copies a title buffer into
   that shader's buffer, which now has a host copy, so the copy is made on the
   card (`host/place.rs`) and must land too. Skipped where the adapter has no
   shader model 6.6.

## What it does not prove

- Anything about a particular title: its shaders use far more of the
  instruction set than the one program here, and its frames far more state.
- Graphics pipelines: no vertex or pixel shader is translated or drawn with
  yet (see "Adding a stage").
- Mesh, indirect, ray-tracing, tiled resources, depth, MSAA, formats other
  than RGBA8, more than one queue, waits between queues, the pipeline library
  and the shader cache on disk (the self-test uses neither, so it writes
  nothing).
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

A rate that cannot be measured on the PC (no AVX2, no shader model 6.6) is
left out.

## Tests

`apps/victory/tests/selftest.rs` runs the built binary with
`selftest --warp --frames 8 --bench` on every `cargo test --workspace` and in
CI, in a process of its own (the host device is one per process). Like the
crate's GPU tests it is skipped where there is no Direct3D 12 device at all
(exit code 3); a stage that is skipped (no shader model 6.6) does not fail it.
`victory_shader::native::synth`'s own tests check that its containers read back
as written and that the program translates.

## Adding a stage

1. Write the stage as a method of `Title` in
   `crates/victory-gpu/src/selftest.rs` returning `Result<String, Failed>`:
   the `String` is the `OK` line's detail, the `Failed` reason the `FAIL`
   line's. Reach the layer only through the tables (`slot::<F>(object,
   offset)` at the offsets the table modules list), pass PC-layout
   structures, and wait with `sync`. Call `Watchdog::pet` in long loops.
2. Add its name to `STAGES` and call it from `run`; return
   `Outcome::Skip` with the reason where the PC cannot run it.
3. Programs come from `victory_shader::native::synth`: write them from the
   public instruction encodings (`synth::gfx10` has the few used so far), never
   from a title's code, and add a test there that the container translates.
4. Make it cheap: the integration test runs every stage on the software
   adapter.

What a graphics stage still needs: `synth` to attach the `ISG1`/`OSG1`
signature parts a vertex and a pixel program link by (the emitter writes them
for its own output in `emit/container.rs`, crate-private), encodings for
`exp pos0`, `exp param0` and the pixel program's export patch, and a builder
for the pipeline stream the device's `CreatePipelineState` (0x2b0) reads, with
the render target's format; then targets, viewport and scissor through the
list's table and a draw, read back as in stage 2.
