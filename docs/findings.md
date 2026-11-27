# Where a frame goes: performance notes

These notes summarise where the host spends a frame and what each optimisation
pass was worth. Figures are relative changes measured with diagnostics builds
on a 64-lane desktop graphics card; they show the order of magnitude, not what
any other PC or title will see. The overview of the memory model is in
[ARCHITECTURE.md](../ARCHITECTURE.md#memory).

## Where a frame goes

The primary CPU bottleneck is preparing command submissions on the host
submission thread (command-list recording, host-copy refreshes, descriptor-record
copying, write-watch queries, and AVX2 byte comparisons), alongside GPU card
execution. On the console, CPU and GPU share unified memory; on discrete PC GPUs
they do not, so every large cost falls into one of four categories: **tracking**
CPU writes (write watch, memory guards), **tracking** GPU writes (compute
hashing + CPU comparison), **copying** changed ranges, and **synchronizing**
flight ordering.

Frame-time optimisations, in the order they were made (each measured against
the state before it):

| Optimization | Change in the time it addresses |
|---|---|
| Deferring descriptor copies of draws' tables until draw time | frame time roughly halved |
| Parallel compute hashing over card copies | GPU time about one sixth lower |
| Dedicated submission-preparation relay thread (`VICTORY_RELAY`) | frame time about a quarter lower |
| Command-list translation fanned across worker threads (`VICTORY_THIN`) | frame time about a tenth lower |
| Batched submissions, card steps, ahead texture staging | frame time a few percent lower |
| AVX2 multi-threaded comparison of shader-written buffers | that step two to three times faster |
| Filtering guarded writes before resetting protections | that step's CPU time about a tenth of what it was |

## Faults found and resolved

- **Missing compute texture read-back.** Textures written by compute shaders and
  read back by CPU physics were synchronized via read-back (`VICTORY_TEXTURE_BACK=0`
  disables; on by default after upstream `fxceless/victory-fork`).
- **Stale slot before texture upload.** Ordering uploads ahead of slot publication
  via `VICTORY_SHOW_AFTER=1` prevented transient page faults on newly created
  textures.
- **Unmapped-view write race.** Synchronizing view mapping against guard inspection
  and locks eliminated lost writes under concurrent streaming.
- **Indirect dispatch clamping.** Indirect dispatches requesting more than 65,535
  groups per dimension are bounded cleanly.
- **Quad helper-lane behavior in wave reads.** On RDNA architectures helper lanes
  are inactive during wave ops; translated pixel shaders account for wave
  semantics accordingly.
- **DXIL intrinsics & formats.** Fixed DXIL `Log` base-2 semantics, normalized
  buffer format codes, and indirect dispatch thread/group flag handling.

## Evaluated without adoption

- Unordered early pipelines for all graphics states (driver compiler crashes).
- Sixteen-thread page mapping (increased contention without extra throughput).
- Scalarized 4-dword buffer loads (`VICTORY_SHADER_OPT=127`), disabled barriers,
  or raw host shader injection (negligible or negative payoff on desktop drivers).
- The on-screen telemetry overlay (`VICTORY_PANEL`) adds rendering overhead, so
  it defaults to off (toggled via F3).

## Optimization directions

1. Pipelining two-stage submission preparation and keeping buffer copies resident
   on the GPU when populated via fills.
2. Keeping small buffer copies on the card (`VICTORY_CARD_BUFFERS_MIN_MB=64`).
3. Limiting slow pipeline compilation hitches via `victory warm`.
4. Tracking indirect budget exhaustion per list (`VICTORY_INDIRECT_REST=0`).
5. Bounding private allocator, window, and upload working sets.

## Upstream repository notes

Upstream compatibility lineage studied (credits in
[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md)) includes `Marvisak/VIctory`,
`fxceless/victory-fork`, `DexNom/VIctory`, `Belobug/VIctory-Belobug-AMD`, and
`FQY55kai/ZJOU-VICTORY`:

- `fxceless/victory-fork`: texture read-back, small buffers on card, range hashing.
- `DexNom/VIctory`: ray-dispatch thread counts, cube gradients, sparse range merging.
- `Belobug/VIctory-Belobug-AMD`: background pipeline workers, ambient estimate
  concept, pipeline/constant tracking (romainhedouin).
- `FQY55kai/ZJOU-VICTORY`: render-target rebind avoidance and shader fixes.
