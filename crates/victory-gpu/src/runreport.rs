//! What the GPU side tells the run report (`victory_core::report`, which
//! this crate reaches through `victory_pipeline::report`: its one path to
//! victory-core, so the crate graph and the lock file stay as they are).
//!
//! Most of it is counted here already, for the log's lines: [`sample`] copies
//! those counters into the report just before one is written, so nothing is
//! added to a frame's path for them. What is fed as it happens is cheap: a
//! present, a draw or dispatch left out (with its reason), the host device's
//! facts once.

use std::cell::Cell;
use std::sync::atomic::Ordering::Relaxed;

use victory_pipeline::report::{self, GpuFacts, HostText, LeftOut, Stat};
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

    /// The sampler reads only counters and the system: it runs with or
    /// without a host device.
    #[test]
    fn the_sampler_runs_without_a_run() {
        sample();
    }
}
