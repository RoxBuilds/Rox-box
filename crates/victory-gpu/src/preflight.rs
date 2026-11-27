//! What the host has, asked before a run: the Windows build, the commit
//! memory left and what the default graphics adapter (or the software
//! adapter, when a run is asked to use it) can do. Nothing here
//! needs a title; `victory check` prints it and `victory run --execute`
//! refuses to start without what a run cannot do without.

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::{
    D3D_SHADER_MODEL, D3D_SHADER_MODEL_6_0, D3D_SHADER_MODEL_6_6, D3D12_FEATURE_D3D12_OPTIONS7,
    D3D12_FEATURE_DATA_D3D12_OPTIONS7, D3D12_FEATURE_DATA_SHADER_MODEL, D3D12_FEATURE_SHADER_MODEL,
    D3D12_MESH_SHADER_TIER_NOT_SUPPORTED, D3D12CreateDevice, ID3D12Device,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1,
    IDXGIDevice, IDXGIFactory4,
};
use windows::core::Interface;

/// The default graphics adapter and what a run needs of it.
#[derive(Debug, Clone)]
pub struct Gpu {
    /// The adapter's name as Windows reports it.
    pub name: String,
    /// Its driver's version (`a.b.c.d`), if Windows tells.
    pub driver: Option<String>,
    /// Its dedicated video memory, in MiB.
    pub dedicated_mb: u64,
    /// Whether it is a software rasteriser (no graphics card is used).
    pub software: bool,
    /// The highest shader model it runs, up to 6.6 (`(6, 6)`).
    pub shader_model: (u32, u32),
    /// Whether it runs mesh shaders.
    pub mesh_shaders: bool,
}

/// Why no Direct3D 12 device could be made.
#[derive(Debug, Clone)]
pub struct NoDevice {
    /// The name of the first adapter Windows lists, if there is one.
    pub adapter: Option<String>,
    /// What Windows said.
    pub reason: String,
}

/// The Windows version as `(major, minor, build)`: `(10, 0, 19045)`.
pub fn windows_version() -> Option<(u32, u32, u32)> {
    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
    }
    let mut info = OsVersionInfo {
        size: size_of::<OsVersionInfo>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // SAFETY: writes the structure whose size is given in its first field.
    let status = unsafe { RtlGetVersion(&mut info) };
    (status == 0).then_some((info.major, info.minor, info.build))
}

/// The commit memory (RAM plus page file) Windows can still give this
/// process and the limit, in MiB.
pub fn commit_memory_mb() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: all-zero is a valid value of this plain structure.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: writes the structure whose size is given in its first field.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some((m.ullAvailPageFile >> 20, m.ullTotalPageFile >> 20))
}

/// The installed memory, in MiB.
pub fn physical_memory_mb() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: all-zero is a valid value of this plain structure.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: writes the structure whose size is given in its first field.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some(m.ullTotalPhys >> 20)
}

fn adapter_name(description: &[u16]) -> String {
    let end = description.iter().position(|&c| c == 0).unwrap_or(description.len());
    String::from_utf16_lossy(&description[..end]).trim().to_owned()
}

/// Whether a run's host device is to be made on the software adapter
/// (`VICTORY_D3D12_WARP=1`, read as `host.rs` reads it): then that adapter,
/// not the default one, is the one a run needs ([`software_gpu`]).
pub fn software_asked() -> bool {
    crate::switch::on("VICTORY_D3D12_WARP")
}

/// Makes a Direct3D 12 device on the default adapter, as a run does, asks it
/// what it can do and lets it go again.
pub fn gpu() -> Result<Gpu, NoDevice> {
    ask(false)
}

/// [`gpu`], of the software adapter (WARP), on which a run with
/// [`software_asked`] makes its device.
pub fn software_gpu() -> Result<Gpu, NoDevice> {
    ask(true)
}

fn ask(software: bool) -> Result<Gpu, NoDevice> {
    // SAFETY: plain DXGI and D3D12 queries; each writes the structure it is
    // given, of the size it is given.
    unsafe {
        let factory = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)).ok();
        let warp: Option<IDXGIAdapter1> = if software {
            match factory.as_ref().map(|f| f.EnumWarpAdapter::<IDXGIAdapter1>()) {
                Some(Ok(warp)) => Some(warp),
                Some(Err(e)) => return Err(NoDevice { adapter: None, reason: e.to_string() }),
                None => return Err(NoDevice { adapter: None, reason: "no DXGI factory".into() }),
            }
        } else {
            None
        };
        let first = match &warp {
            Some(warp) => warp.GetDesc1().ok(),
            None => factory.as_ref().and_then(|f| f.EnumAdapters1(0).ok()).and_then(|a| a.GetDesc1().ok()),
        }
        .map(|d| adapter_name(&d.Description));
        let mut device: Option<ID3D12Device> = None;
        let made = match &warp {
            Some(warp) => D3D12CreateDevice(warp, D3D_FEATURE_LEVEL_12_0, &mut device),
            None => D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device),
        };
        if let Err(e) = made {
            return Err(NoDevice { adapter: first, reason: e.to_string() });
        }
        let Some(device) = device else {
            return Err(NoDevice { adapter: first, reason: "no device was returned".into() });
        };
        let adapter: Option<IDXGIAdapter1> =
            factory.as_ref().and_then(|f| f.EnumAdapterByLuid(device.GetAdapterLuid()).ok());
        let desc = adapter.as_ref().and_then(|a| a.GetDesc1().ok());
        let driver = adapter
            .as_ref()
            .and_then(|a| a.CheckInterfaceSupport(&IDXGIDevice::IID).ok())
            .map(|v| {
                let v = v as u64;
                format!("{}.{}.{}.{}", v >> 48, (v >> 32) & 0xffff, (v >> 16) & 0xffff, v & 0xffff)
            });

        // The query fails for a model the runtime does not know: ask down.
        let mut shader_model = (0, 0);
        for ask in (D3D_SHADER_MODEL_6_0.0..=D3D_SHADER_MODEL_6_6.0).rev() {
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL { HighestShaderModel: D3D_SHADER_MODEL(ask) };
            if device
                .CheckFeatureSupport(D3D12_FEATURE_SHADER_MODEL, (&raw mut model).cast(), size_of_val(&model) as u32)
                .is_ok()
            {
                let got = model.HighestShaderModel.0 as u32;
                shader_model = (got >> 4, got & 0xf);
                break;
            }
        }
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
        let mesh_shaders = device
            .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS7, (&raw mut options).cast(), size_of_val(&options) as u32)
            .is_ok()
            && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED;
        Ok(Gpu {
            name: desc
                .as_ref()
                .map(|d| adapter_name(&d.Description))
                .or(first)
                .unwrap_or_else(|| "(unnamed adapter)".into()),
            driver,
            dedicated_mb: desc.as_ref().map_or(0, |d| d.DedicatedVideoMemory as u64 >> 20),
            software: desc.as_ref().is_some_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0),
            shader_model,
            mesh_shaders,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_answers() {
        let (major, _, build) = windows_version().expect("a Windows version");
        assert!(major >= 10 && build > 0);
        let (free, total) = commit_memory_mb().expect("commit memory");
        assert!(free <= total && total > 0);
        assert!(physical_memory_mb().is_some_and(|mb| mb > 0));
    }

    #[test]
    fn an_adapter_name_ends_at_its_terminator() {
        let wide: Vec<u16> = "Some Adapter \0junk".encode_utf16().collect();
        assert_eq!(adapter_name(&wide), "Some Adapter");
        assert_eq!(adapter_name(&[]), "");
    }

    /// With or without a graphics card: an answer, and no panic.
    #[test]
    fn the_adapter_is_described_or_refused() {
        match gpu() {
            Ok(gpu) => assert!(!gpu.name.is_empty()),
            Err(none) => assert!(!none.reason.is_empty()),
        }
        // The one `VICTORY_D3D12_WARP=1` runs on, which is no graphics card.
        match software_gpu() {
            Ok(gpu) => assert!(gpu.software && !gpu.name.is_empty(), "{gpu:?}"),
            Err(none) => assert!(!none.reason.is_empty()),
        }
    }
}
