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

## One run on a small card and a PC short of memory

The log of one run at about 2 frames a second, in the world, said this of
its frames (shares of a frame; not confirmed on any other PC):

- **Waiting for flights.** About a quarter of every frame the submitting
  thread stood for the card in settles for "title memory changed under
  shader writes": under one a frame, each until nearly all the work in the
  air had landed. Such settles came in seconds where the title's bytes under
  the flights' writes had no changed run at all, so they were not runs over
  what a submission sends up but ranges the merge could not look at: memory
  that is neither write-watched nor guarded, of which nothing says which
  pages the title wrote. `VICTORY_UNDER_UNWATCHED_MB=<n>` (off by default)
  compares such ranges whole, page by page, up to n MiB a submission, and
  puts in only what differs, as the pages the journal names are put in. The
  log line "title bytes put in under flights' shader writes" now counts why
  each range was left to a wait, in every build.
- **"heap buffers ... ms".** With the pass over the buffer views beside
  (`VICTORY_HEAP_PASS_ASIDE`), the figure is the texture side of the pass
  (textures picked by index made and staged), which takes a share of the
  time since the pass before (`VICTORY_TEXTURE_STAGE_SHARE`, a tenth; three
  times that with a backlog). It was a tenth of the frame in most seconds: it
  swings with the frame time, and shrinks with it. The frame line now gives
  the texture side, the time its slots ran past the pass's own time, and the
  buffer views' pass apart.
- **Compares for shader writes.** Of the bytes compared on the CPU about a
  quarter were on pages with a new byte; of the pieces compared on the card,
  under one in fifty was fetched. A narrower written set (the root parameters
  and table ranges a program can store through) would leave out only views no
  store goes through: a root view's range is the rest of its buffer whatever
  its shaders store. The line "shader writes declared by draws and
  dispatches" now splits root views from views in tables, to tell whether it
  is worth building; the compares run beside the submitting thread, and the
  card's are a small part of its time.

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
