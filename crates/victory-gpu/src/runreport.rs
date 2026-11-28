//! What the GPU side tells the run report (`victory_core::report`, which
//! this crate reaches through `victory_pipeline::report`: its one path to
//! victory-core, so the crate graph and the lock file stay as they are).
//!
//! Most of it is counted here already, for the log's lines: [`sample`] copies
//! those counters into the report just before one is written, so nothing is
//! added to a frame's path for them. What is fed as it happens is cheap: a
//! present, a draw or dispatch left out (with its reason), the host device's
//! facts once. The meters of the once-a-second frame line are taken with
//! `swap(0)` by the line itself, so the line gives them here as it takes them
//! ([`frame_line`] and the functions after it), once a second; the budget
//! governor gives its looks.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicU32, AtomicU64};

use victory_pipeline::report::{self, Figure, GpuFacts, HostText, LeftOut, Stat};
use windows::Win32::Graphics::Direct3D::{
    D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_12_0,
    D3D_FEATURE_LEVEL_12_1, D3D_FEATURE_LEVEL_12_2,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D_SHADER_MODEL, D3D_SHADER_MODEL_6_0, D3D_SHADER_MODEL_6_9, D3D12_FEATURE_D3D12_OPTIONS7,
    D3D12_FEATURE_DATA_D3D12_OPTIONS7, D3D12_FEATURE_DATA_FEATURE_LEVELS,
    D3D12_FEATURE_DATA_SHADER_MODEL, D3D12_FEATURE_FEATURE_LEVELS, D3D12_FEATURE_SHADER_MODEL,
    D3D12_MESH_SHADER_TIER_NOT_SUPPORTED, ID3D12Device,
};
use windows::Win32::Graphics::Dxgi::{DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter3, IDXGIDevice};
use windows::core::Interface;

thread_local! {
    /// Why the draw or dispatch this thread is recording is being left out,
    /// where the code that refused it knows better than its pipeline's state
    /// does (memory refused, no descriptor slots, indirect arguments not
    /// bounded). Taken by the next [`left_out`] on the thread.
    /// shortcut: a refusal that does not end in a draw left out (none is
    /// known) would lend its reason to the thread's next one.
    static HINT: Cell<Option<LeftOut>> = const { Cell::new(None) };
}

/// The draw being recorded is refused for `why`.
pub(crate) fn hint(why: LeftOut) {
    HINT.set(Some(why));
}

/// A draw, mesh dispatch or dispatch with the title pipeline `pipeline` is
/// left out: for the reason given by [`hint`], else for what its pipeline's
/// state says.
pub(crate) fn left_out(pipeline: usize) {
    let why = HINT
        .take()
        .unwrap_or_else(|| crate::pipeline::left_out_reason(pipeline as crate::object::This));
    report::left_out(why);
}

/// A present, with the draws and dispatches run on the host so far.
pub(crate) fn presented() {
    report::present(crate::draw::drawn(), crate::draw::dispatched());
}

/// Video memory now, from the budget governor's look (`[budget, used]` of
/// the card's own memory and of the PC's shared with it).
pub(crate) fn video_memory(card: [u64; 2], shared: [u64; 2]) {
    report::video_memory(card, shared);
}

/// Free physical memory and commit headroom now (bytes; `u64::MAX` not
/// known), as the budget governor asked the system.
pub(crate) fn system_memory(free_ram: u64, commit_headroom: u64) {
    report::memory_seen(free_ram, commit_headroom);
}

/// A look of the budget governor (about once a second): its level, and
/// whether the card's own memory is past its budget.
pub(crate) fn governor_look(s: &crate::host::budget::Sample, level: crate::host::budget::Level) {
    use crate::host::budget::Level;
    let level = match level {
        Level::Green => 1,
        Level::Yellow => 2,
        Level::Red => 3,
    };
    report::memory_look(level, s.card);
    // `[gpu_time]`: what the card and the PC had while the frames it times
    // ran, from this same look.
    report::gpu_time_look(s.card, s.shared, s.ram);
}

/// The card's time by kind of a frame-line second (`host::gpu_split`), as
/// nanoseconds by kind in the order of `report::GPU_KINDS`, the volumes in
/// the order of `host::gpu_split::Volume`, and the frames partly measured,
/// over `frames` frames.
pub(crate) fn gpu_split(
    frames: u64,
    ns: [u64; crate::host::gpu_split::KINDS],
    volumes: [u64; crate::host::gpu_split::VOLUMES_N],
    partly: u64,
    pipelines: [u64; 4],
) {
    use report::GpuFigure as G;
    let mut values = [(G::Frames, frames); 2 + crate::host::gpu_split::KINDS + crate::host::gpu_split::VOLUMES_N + 5];
    values[1] = (G::PartlyFrames, partly);
    for (i, (&kind, n)) in report::GPU_KINDS.iter().zip(ns).enumerate() {
        values[2 + i] = (kind, n);
    }
    let volume = [
        G::Fills,
        G::FillBytes,
        G::HashPasses,
        G::HashedBytes,
        G::ComparePasses,
        G::ComparedBytes,
        G::CopiedBackBytes,
        G::UploadRanges,
        G::UploadedBytes,
        G::TextureBytes,
    ];
    for (i, (&figure, n)) in volume.iter().zip(volumes).enumerate() {
        values[2 + crate::host::gpu_split::KINDS + i] = (figure, n);
    }
    // The compute pipelines of the second, named nowhere: a second that had
    // any, how many ran, the card's time of the costliest 1, 5 and 20.
    let [ran, top1, top5, top20] = pipelines;
    let at = 2 + crate::host::gpu_split::KINDS + crate::host::gpu_split::VOLUMES_N;
    values[at..].copy_from_slice(&[
        (G::PipelineSeconds, u64::from(ran > 0)),
        (G::ComputePipelinesSum, ran),
        (G::TopPipelineNs, top1),
        (G::Top5PipelinesNs, top5),
        (G::Top20PipelinesNs, top20),
    ]);
    report::gpu_time(crate::host::gpu_split::state(), &values);
}

// The frame line's figures. It takes its meters with `swap(0)` and says them
// per frame, so they are given here as it has them (ms and MB of 10^6 bytes
// per frame over `frames` frames) and made totals again; once a second.

/// A total from `per_frame` over `frames` (`as` makes a negative or NaN 0).
fn total(per_frame: f64, frames: f64) -> u64 {
    (per_frame * frames).round() as u64
}

fn us(ms: f64, frames: f64) -> u64 {
    total(ms * 1e3, frames)
}

/// The frame line's own figures (`host`, `Meter::frame`): the meter's
/// `frames` and `elapsed`; per frame, ms preparing submissions, of it copies
/// (ms, MB) and textures (ms, MB), ms waiting for the GPU, and copying back
/// (ms, MB). With the queue of textures picked by index, as it stands.
pub(crate) fn frame_line(frames: f64, elapsed: std::time::Duration, figures: [f64; 8]) {
    // The line has taken the meters: none of its frames are left over.
    PENDING_FRAMES.store(0, Relaxed);
    let [cpu, copy_ms, copy_mb, texture_ms, texture_mb, gpu, back_ms, back_mb] = figures;
    report::figures(&[
        (Figure::Frames, frames as u64),
        (Figure::MeterUs, elapsed.as_micros() as u64),
        (Figure::PreparingUs, us(cpu, frames)),
        (Figure::CopiesUpUs, us(copy_ms, frames)),
        (Figure::CopiesUpBytes, total(copy_mb * 1e6, frames)),
        (Figure::TextureRefreshUs, us(texture_ms, frames)),
        (Figure::TextureRefreshBytes, total(texture_mb * 1e6, frames)),
        (Figure::CardWaitUs, us(gpu, frames)),
        (Figure::CopyBackUs, us(back_ms, frames)),
        (Figure::CopyBackBytes, total(back_mb * 1e6, frames)),
    ]);
    report::max(
        Stat::PickedWaitingMost,
        crate::draw::HEAP_TEXTURES_QUEUE[2].load(Relaxed),
    );
}

// The frames since the frame line's last second. The line takes its meters
// once a whole second has passed, so a run's last part of a second (a short
// run: all of it) never reached the report. The meter tells of every frame it
// counts ([`frame_counted`]); when the report that names the run's end is
// written, [`last_meters`] takes the meters as the line would and gives them
// to the report, without a line in the log.

/// Frames the meter counted since its last line, and the microseconds (from
/// [`BASE`]) when its second began and when it counted the last of them.
static PENDING_FRAMES: AtomicU32 = AtomicU32::new(0);
static PENDING_SINCE_US: AtomicU64 = AtomicU64::new(0);
static PENDING_LAST_US: AtomicU64 = AtomicU64::new(0);
static BASE: OnceLock<std::time::Instant> = OnceLock::new();

/// The meters the line takes that live in `host` (submissions: preparing,
/// waiting for the card; copying back: microseconds, bytes; the card's
/// clock), given by the meter as it counts frames.
struct HostMeters {
    submission_us: &'static [AtomicU64; 2],
    copy_back: &'static [AtomicU64; 2],
    card_clock_us: &'static [AtomicU64; 3],
}

static HOST_METERS: OnceLock<HostMeters> = OnceLock::new();

/// The frame meter counted a frame: `frames` since its last line, which
/// began at `since`. A few relaxed stores a present.
pub(crate) fn frame_counted(
    frames: u32,
    since: std::time::Instant,
    submission_us: &'static [AtomicU64; 2],
    copy_back: &'static [AtomicU64; 2],
    card_clock_us: &'static [AtomicU64; 3],
) {
    let base = *BASE.get_or_init(|| since);
    let _ = HOST_METERS.get_or_init(|| HostMeters { submission_us, copy_back, card_clock_us });
    let us = |at: std::time::Instant| at.saturating_duration_since(base).as_micros() as u64;
    PENDING_SINCE_US.store(us(since), Relaxed);
    PENDING_LAST_US.store(us(std::time::Instant::now()), Relaxed);
    PENDING_FRAMES.store(frames, Relaxed);
}

/// The meters the frame line takes, taken one last time (the frames since
/// its last second): given to the report as the line gives them. Nothing if
/// no frame is left over. shortcut: the settles' waits (`host::late_line`)
/// are not taken here; their last part of a second stays out of
/// `[frame_time.waits]`.
fn last_meters() {
    let frames = PENDING_FRAMES.swap(0, Relaxed);
    let Some(host) = HOST_METERS.get().filter(|_| frames > 0) else {
        return;
    };
    let take = |a: &AtomicU64| a.swap(0, Relaxed);
    let elapsed = PENDING_LAST_US.load(Relaxed).saturating_sub(PENDING_SINCE_US.load(Relaxed));
    let prep = &crate::host::PREPARING;
    report::figures(&[
        (Figure::Frames, u64::from(frames)),
        (Figure::MeterUs, elapsed),
        (Figure::PreparingUs, take(&host.submission_us[0])),
        (Figure::CopiesUpUs, take(&prep[0])),
        (Figure::CopiesUpBytes, take(&prep[1])),
        (Figure::TextureRefreshUs, take(&prep[2])),
        (Figure::TextureRefreshBytes, take(&prep[3])),
        (Figure::CardWaitUs, take(&host.submission_us[1])),
        (Figure::CopyBackUs, take(&host.copy_back[0])),
        (Figure::CopyBackBytes, take(&host.copy_back[1])),
        (Figure::CardIdleUs, take(&host.card_clock_us[0])),
        (Figure::CardIdleBehindUs, take(&host.card_clock_us[1])),
        (Figure::StoodUs, take(&host.card_clock_us[2])),
        (Figure::HeapBufferPasses, take(&crate::draw::HEAP_BUFFER_PASSES)),
        (Figure::HeapBufferUs, take(&crate::draw::HEAP_BUFFER_MICROS)),
        (Figure::RefreshAllUs, take(&crate::texture::REFRESH_WAITS[3])),
    ]);
    crate::host::gpu_split::last(u64::from(frames));
    crate::pipeline::leave_out_end();
}

/// The card's clock per frame (`host::card_clock_line`): ms with nothing of
/// ours queued, of that while the submitting thread worked, and ms that
/// thread stood waiting for the card.
pub(crate) fn card_clock(frames: f64, [idle, behind, stood]: [f64; 3]) {
    report::figures(&[
        (Figure::CardIdleUs, us(idle, frames)),
        (Figure::CardIdleBehindUs, us(behind, frames)),
        (Figure::StoodUs, us(stood, frames)),
    ]);
}

/// The waiting for the card in settles per frame, ms, by what settled, in the
/// order of the log line (`host::late_line`): a synchronous submission or
/// present, the card had run them, a wait for a memory value, title memory
/// changed under shader writes, title memory read, a step of ours, lists in
/// the air, bottom-of-pipe writes, a blocking fence wait.
pub(crate) fn settle_waits(frames: f64, ms: [f64; 9]) {
    let causes = [
        Figure::SettledSync,
        Figure::SettledCardDone,
        Figure::SettledValue,
        Figure::SettledStale,
        Figure::SettledRead,
        Figure::SettledStep,
        Figure::SettledMost,
        Figure::SettledWrites,
        Figure::SettledFence,
    ];
    let values: [(Figure, u64); 9] = std::array::from_fn(|i| (causes[i], us(ms[i], frames)));
    report::figures(&values);
}

/// Work moved off the threads a frame waits for, ms per frame, in the order
/// of the log line (`host::moved_line`): objects let go on a thread of their
/// own, the log's table of pipelines put together on one, snapshots taken
/// beside the submitting thread's work, and its wait for them after.
pub(crate) fn moved(frames: f64, ms: [f64; 4]) {
    report::figures(&[
        (Figure::MovedBuryUs, us(ms[0], frames)),
        (Figure::MovedTableUs, us(ms[1], frames)),
        (Figure::MovedSnapshotsUs, us(ms[2], frames)),
        (Figure::SnapshotsWaitedUs, us(ms[3], frames)),
    ]);
}

/// Microseconds the submitting thread spent making memory for submissions'
/// sends since the frame line's last second (`host::send`; not per frame).
/// shortcut: the run's last part of a second ([`last_meters`]) is left out.
pub(crate) fn send_memory_made(us: u64) {
    if us > 0 {
        report::figures(&[(Figure::SendMemoryMadeUs, us)]);
    }
}

/// The write watch's kernel askings since the frame line's last second
/// (`watch::kernel_line`; not per frame): askings, KiB asked about, pages
/// found written, nanoseconds. shortcut: the run's last part of a second
/// ([`last_meters`]) is left out.
pub(crate) fn write_watch(all: [u64; 4]) {
    report::figures(&[
        (Figure::WriteWatchAskings, all[0]),
        (Figure::WriteWatchKib, all[1]),
        (Figure::WriteWatchPages, all[2]),
        (Figure::WriteWatchUs, all[3] / 1000),
    ]);
}

/// Passes over the heap's buffer views per frame, and their ms.
pub(crate) fn heap_buffers(frames: f64, passes: f64, ms: f64) {
    report::figures(&[
        (Figure::HeapBufferPasses, total(passes, frames)),
        (Figure::HeapBufferUs, us(ms, frames)),
    ]);
}

/// Work a unified-memory switch spared since the frame line's last second:
/// `total` of `figure` (one of the `Spared` figures), not per frame.
pub(crate) fn spared(figure: Figure, total: u64) {
    if total > 0 {
        report::figures(&[(figure, total)]);
    }
}

/// Texture refreshes on all threads per frame, ms.
pub(crate) fn texture_refreshes(frames: f64, ms: f64) {
    report::figures(&[(Figure::RefreshAllUs, us(ms, frames))]);
}

/// The host GPU failed for good (`crate::host::fail`): the report says so,
/// with the device's removal reason if it was removed, before the run ends.
pub(crate) fn host_failed(removed: Option<i32>) {
    let _ = report::finish(match removed {
        Some(hresult) => report::End::DeviceRemoved(hresult as u32),
        None => report::End::HostGpuFailed,
    });
}

/// What the run's host device is made on, once, when it is made.
pub(crate) fn host_device(
    device: &ID3D12Device,
    adapter: Option<&IDXGIAdapter3>,
    waves: (u32, u32),
) {
    if !report::active() {
        return;
    }
    // SAFETY: plain queries; each writes the structure it is given, of the
    // size it is given (the feature-level query reads the list it points to,
    // which lives until the call returns).
    let facts = unsafe {
        let desc = adapter.and_then(|a| a.GetDesc1().ok());
        let driver = adapter
            .and_then(|a| a.CheckInterfaceSupport(&IDXGIDevice::IID).ok())
            .map(|v| v as u64);
        let asked: [D3D_FEATURE_LEVEL; 5] = [
            D3D_FEATURE_LEVEL_12_2,
            D3D_FEATURE_LEVEL_12_1,
            D3D_FEATURE_LEVEL_12_0,
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
        ];
        let mut levels = D3D12_FEATURE_DATA_FEATURE_LEVELS {
            NumFeatureLevels: asked.len() as u32,
            pFeatureLevelsRequested: asked.as_ptr(),
            MaxSupportedFeatureLevel: D3D_FEATURE_LEVEL(0),
        };
        let level = if device
            .CheckFeatureSupport(
                D3D12_FEATURE_FEATURE_LEVELS,
                (&raw mut levels).cast(),
                size_of_val(&levels) as u32,
            )
            .is_ok()
        {
            let v = levels.MaxSupportedFeatureLevel.0 as u32;
            (((v >> 12) & 0xf) as u8, ((v >> 8) & 0xf) as u8)
        } else {
            (0, 0)
        };
        // The query fails for a model the runtime does not know: ask down.
        let mut model = (0, 0);
        for ask in (D3D_SHADER_MODEL_6_0.0..=D3D_SHADER_MODEL_6_9.0).rev() {
            let mut m = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL(ask),
            };
            if device
                .CheckFeatureSupport(
                    D3D12_FEATURE_SHADER_MODEL,
                    (&raw mut m).cast(),
                    size_of_val(&m) as u32,
                )
                .is_ok()
            {
                let got = m.HighestShaderModel.0 as u32;
                model = ((got >> 4) as u8, (got & 0xf) as u8);
                break;
            }
        }
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
        let mesh = device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS7,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .is_ok()
            && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED;
        let name = desc.as_ref().map(|d| {
            let end = d
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(d.Description.len());
            String::from_utf16_lossy(&d.Description[..end])
        });
        GpuFacts {
            adapter: HostText::new(name.as_deref().unwrap_or("")),
            vendor_id: desc.as_ref().map_or(0, |d| d.VendorId),
            driver,
            dedicated_mib: desc
                .as_ref()
                .map_or(0, |d| d.DedicatedVideoMemory as u64 >> 20),
            software: desc
                .as_ref()
                .is_some_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0),
            feature_level: level,
            shader_model: model,
            wave_min: waves.0,
            wave_max: waves.1,
            mesh_shaders: mesh,
        }
    };
    report::host_gpu(facts);
}

/// The translator failed for `e`: counted under its own kind and wording.
pub(crate) fn shader_failed(e: &victory_shader::ShaderError) {
    use victory_shader::ShaderError;
    let (kind, what) = match e {
        ShaderError::NotDxbc => ("not_a_container", ""),
        ShaderError::Malformed(what) => ("malformed", *what),
        ShaderError::Isa { reason, .. } => ("native_code", *reason),
        ShaderError::Unsupported { what, .. } => ("unsupported", *what),
    };
    report::shader_failed(kind, what);
}

/// Copies the counters the GPU side keeps for its own lines into the report,
/// with the process's memory peaks: registered as the report's sampler by
/// the run, called before each report is written.
pub fn sample() {
    if report::ending() {
        last_meters();
    }
    // `[upscaler]`: the host upscaler's figures.
    crate::upscale::report();
    let (made, loaded) = crate::pipeline::host_counts();
    report::set(Stat::PipelinesMadeNow, made);
    report::set(Stat::PipelinesFromLibrary, loaded);
    let (hits, _) = crate::shader_cache::counts();
    report::set(Stat::ShadersFromCache, hits);
    let [ok, failed] = &crate::host::OOM_RETRIES;
    report::set(Stat::OomRetriedOk, ok.load(Relaxed));
    report::set(Stat::OomRetriedFailed, failed.load(Relaxed));
    let limit = victory_shader::emit::view_heap_limit();
    if limit > 0 {
        report::set(Stat::ViewHeapViews, u64::from(limit) + 1);
    }
    let [looks, freed, in_blocks, blocks, returned] = crate::host::budget::relief_counts();
    report::set(Stat::CardReliefLooks, looks);
    report::set(Stat::CardReliefFreed, freed);
    report::set(Stat::CardReliefInBlocks, in_blocks);
    report::set(Stat::CardCompactBlocks, blocks);
    report::set(Stat::CardCompactReturned, returned);
    let [critical, held, refused] = crate::host::commit::counts();
    report::set(Stat::CommitCritical, critical);
    report::set(Stat::CommitHeldBack, held);
    report::set(Stat::CommitRefusals, refused);
    for (waits, [times, us, longest]) in [
        (&crate::host::RESIDENT, [Stat::ResidentWaits, Stat::ResidentUs, Stat::ResidentLongestUs]),
        (&crate::host::MAPPED, [Stat::MappedWaits, Stat::MappedUs, Stat::MappedLongestUs]),
    ] {
        report::set(times, waits[0].load(Relaxed));
        report::set(us, waits[1].load(Relaxed));
        report::set(longest, waits[2].load(Relaxed));
    }
    use crate::draw::{
        HEAP_TEXTURE_BYTES, HEAP_TEXTURES, HEAP_TEXTURES_FULL, HEAP_TEXTURES_QUEUE,
        HEAP_TEXTURES_RECHECKED, HEAP_TEXTURES_REUPLOADED_FULL,
    };
    report::set(Stat::PickedUploaded, HEAP_TEXTURES.load(Relaxed));
    report::set(Stat::PickedBytes, HEAP_TEXTURE_BYTES.load(Relaxed));
    let waiting = HEAP_TEXTURES_QUEUE[2].load(Relaxed);
    report::set(Stat::PickedWaitingLast, waiting);
    report::max(Stat::PickedWaitingMost, waiting);
    report::set(Stat::PickedNoRoom, HEAP_TEXTURES_FULL.load(Relaxed));
    report::set(Stat::PickedChangedUnseen, HEAP_TEXTURES_RECHECKED[1].load(Relaxed));
    report::set(
        Stat::PickedReuploadedFull,
        HEAP_TEXTURES_REUPLOADED_FULL.load(Relaxed),
    );
    report::set(Stat::TailCopies, crate::resource::TAILS[0].load(Relaxed));
    report::set(Stat::TailBytes, crate::resource::TAILS[1].load(Relaxed));
    let (cut, unwritten) = (&crate::resource::DETAIL_WHY, &crate::resource::UNWRITTEN);
    report::set(Stat::DetailCutCard, cut[1].load(Relaxed) + cut[2].load(Relaxed));
    report::set(Stat::DetailCutPc, cut[3].load(Relaxed));
    report::set(Stat::PoolRoomWhole, crate::resource::POOL_WHOLE.load(Relaxed));
    report::set(Stat::UnwrittenKept, [0, 1, 3, 4].iter().map(|&i| unwritten[i].load(Relaxed)).sum());
    // `[memory.views]`: the descriptor table's entries of views without a
    // resource (`VICTORY_OWNERLESS_VIEWS`).
    let [kept, dropped] = crate::descriptor::ownerless_counts();
    report::set(Stat::OwnerlessViewsKept, kept);
    report::set(Stat::OwnerlessViewsDropped, dropped);
    // `[gpu_work.descriptor_slots]`: the dynamic slots of the view heaps.
    report::descriptor_slots(crate::descriptor::host::ring::report());
    // One more look at video memory (at most every 32 ms, and it feeds the
    // peaks), in case nothing else asked lately.
    let _ = crate::host::budget::sample();
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0 {
        report::max(Stat::CommitPeak, c.PeakPagefileUsage as u64);
        report::max(Stat::WorkingSetPeak, c.PeakWorkingSetSize as u64);
    }
    memory_map();
}

// The memory map (`[memory.map]`): this crate's share of it, from counters
// it keeps for its own lines, sampled with the rest just before a report is
// written. Each is a relaxed load; the few that are behind a lock are taken
// without waiting for it (the last figure stands if it is held).

/// What one host list (its command allocators and lists) costs of the
/// private commit in the driver, whatever it records: about 0.8 MiB, measured
/// on one PC and driver by making lists and reading the process's private
/// usage before and after. An estimate for `[memory.map]`, labelled so there.
const HOST_LIST_DRIVER_BYTES: u64 = 800 << 10;

/// Gives the report the bytes this crate holds, by category, and the
/// process's private commit.
fn memory_map() {
    use report::Part;
    let set = report::map_set;
    let [copies, windows, snapshots, card_copies, sparse] = crate::shadow::report_memory();
    set(Part::TitleBufferCopies, copies);
    set(Part::Windows, windows);
    set(Part::SnapshotBuffers, snapshots);
    set(Part::CardBufferCopies, card_copies);
    set(Part::CardSparseTiles, sparse);
    set(Part::ListScratch, crate::host::ALIVE[2].load(Relaxed));
    set(Part::ListUploads, crate::host::ALIVE[7].load(Relaxed));
    let lists = crate::host::ALIVE[0].load(Relaxed);
    set(Part::HostListsAlive, lists);
    if let Some(idle) = crate::list::report_idle() {
        set(Part::HostListsIdle, idle);
    }
    // What the driver charges to the private commit for us. Every allocation
    // in the card's own memory is charged to it 1:1 (measured), so the card's
    // use as the governor last saw it belongs in the total; and each host
    // list's allocators and lists cost about the same whatever they record,
    // so the lists alive times that cost is an estimate of theirs. Without
    // them, two thirds of a run's private commit stood as "unaccounted".
    let card = crate::host::budget::sample().card;
    if card[0] > 0 {
        set(Part::CardInUse, card[1]);
    }
    set(Part::HostListsDriver, lists.saturating_mul(HOST_LIST_DRIVER_BYTES));
    if crate::pool::on()
        && let Some(free) = crate::pool::report_texture_room()
    {
        set(Part::CardTextureBlocksFree, free);
    }
    // The pool's blocks hold the staging made from them (and what is free
    // in them); without the pool each staging buffer is its own.
    let staging = if crate::pool::on() {
        crate::pool::report_staging_bytes()
    } else {
        Some(crate::texture::report_staged())
    };
    if let Some(bytes) = staging {
        set(Part::TextureStaging, bytes);
    }
    if let Some(bytes) = crate::host::report_send_spare() {
        set(Part::SendSpare, bytes);
    }
    set(Part::ViewTable, crate::descriptor::report_views());
    let kept = crate::pipeline::kept();
    set(Part::ShaderContainers, kept[1]);
    set(Part::ShaderTranslations, kept[3]);
    set(Part::PipelineLibrary, kept[4]);
    let filled = crate::resource::KINDS[1].load(Relaxed);
    set(Part::CardTextures, filled);
    set(
        Part::CardTargets,
        crate::resource::HOST_TEXTURES[1].load(Relaxed).saturating_sub(filled),
    );
    set(Part::CardTextureTiles, crate::tiles::given_total());
    set(Part::DescriptorHeaps, crate::descriptor::host::HEAP_BYTES.load(Relaxed));
    // `[memory.view_copies]` (their blocks' bytes are in the copies of title
    // buffers above, `crate::shadow::report_memory`).
    let [cut, own, blocks, bytes] = crate::shadow::view_blocks_counts();
    report::view_copies_set(cut, own, blocks, bytes);
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0 {
        report::map_commit(c.PagefileUsage as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_is_taken_by_the_next_draw_left_out() {
        hint(LeftOut::OutOfMemory);
        assert_eq!(HINT.take(), Some(LeftOut::OutOfMemory));
        assert_eq!(HINT.take(), None);
    }

    #[test]
    fn shader_failures_keep_the_translators_words() {
        // (Counted into the report's table; only the kinds are checked here.)
        shader_failed(&victory_shader::ShaderError::NotDxbc);
        shader_failed(&victory_shader::ShaderError::Unsupported {
            offset: 4,
            what: "a construct",
        });
        shader_failed(&victory_shader::ShaderError::Isa {
            offset: 8,
            reason: "a reason",
        });
        shader_failed(&victory_shader::ShaderError::Malformed("a part"));
    }

    /// The memory map is given by the sampler without a host device too: the
    /// counters it reads are zero then, and known to be.
    #[test]
    fn the_sampler_gives_the_memory_map() {
        use report::Part;
        sample();
        for part in [
            Part::TitleBufferCopies,
            Part::Windows,
            Part::SnapshotBuffers,
            Part::ListScratch,
            Part::ListUploads,
            Part::ViewTable,
            Part::ShaderContainers,
            Part::CardTextures,
            Part::DescriptorHeaps,
        ] {
            assert!(report::map_get(part).is_some(), "{} not given", part.key());
        }
        // (The title's own parts come from victory-hle's sampler.)
        assert_eq!(report::map_get(Part::TitleCommitted), None);
    }

    /// The sampler reads only counters and the system: it runs with or
    /// without a host device.
    #[test]
    fn the_sampler_runs_without_a_run() {
        sample();
    }

    /// The frames since the line's last second are taken at the end, and
    /// none that a line has taken. (Another test's presents may count frames
    /// meanwhile, so only the order of the times is checked here; the demo
    /// title's test checks that `[frame_time]` counts every present.)
    #[test]
    fn the_last_part_of_a_second_is_taken_at_the_end() {
        static SUBMISSION: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
        static BACK: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
        static CLOCK: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
        let since = std::time::Instant::now();
        frame_counted(3, since, &SUBMISSION, &BACK, &CLOCK);
        assert!(PENDING_LAST_US.load(Relaxed) >= PENDING_SINCE_US.load(Relaxed));
        last_meters();
        frame_counted(5, since, &SUBMISSION, &BACK, &CLOCK);
        frame_line(5.0, std::time::Duration::from_secs(1), [0.0; 8]);
        last_meters();
    }

    /// The frame line's per-frame figures are made totals again, whatever
    /// they hold; the feeds run with or without a report being kept.
    #[test]
    fn per_frame_figures_are_made_totals_again() {
        assert_eq!(us(6.2, 30.0), 186_000);
        assert_eq!(total(4.1 * 1e6, 30.0), 123_000_000);
        assert_eq!(total(-1.0, 30.0), 0);
        assert_eq!(total(f64::NAN, 30.0), 0);
        assert_eq!(total(f64::INFINITY, 1.0), u64::MAX);
        frame_line(30.0, std::time::Duration::from_secs(1), [6.2, 1.0, 4.1, 2.0, 12.0, 7.5, 1.2, 0.9]);
        card_clock(30.0, [6.1, 4.0, 7.0]);
        settle_waits(30.0, [2.0, 0.5, 0.0, 1.5, 1.2, 0.3, 0.0, 0.8, 0.2]);
        heap_buffers(30.0, 2.0, 0.4);
        texture_refreshes(30.0, 4.5);
        system_memory(u64::MAX, u64::MAX);
        let s = crate::host::budget::Sample::UNKNOWN;
        governor_look(&s, crate::host::budget::Level::Green);
        gpu_split(30, [1_000; crate::host::gpu_split::KINDS], [7; crate::host::gpu_split::VOLUMES_N], 1, [40, 9_000, 20_000, 30_000]);
    }
}
