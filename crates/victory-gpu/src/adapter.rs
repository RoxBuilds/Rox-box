//! Which graphics adapter a run uses, chosen in one place for the host
//! device, `victory check` and the replay player.
//!
//! `D3D12CreateDevice(None, ..)` takes Windows' default adapter, which on a
//! laptop with an integrated and a dedicated graphics chip and no hardware
//! switch is usually the integrated one. [`choose`] instead asks DXGI for the
//! adapters in high-performance order, skips software adapters, and takes the
//! first that makes a feature level 12_0 device and runs Shader Model 6.6.
//! The decision itself ([`pick`]) is a pure function over [`Info`] records so
//! that it is tested without a graphics card.
//!
//! The swap chain needs no choice of its own: it is made for the host's
//! command queue, so it is on this device's adapter by construction (DXGI
//! copies across adapters when the window's monitor hangs off another one).

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::{
    D3D_SHADER_MODEL_6_6, D3D12_FEATURE_ARCHITECTURE, D3D12_FEATURE_DATA_ARCHITECTURE, D3D12_FEATURE_DATA_SHADER_MODEL,
    D3D12_FEATURE_SHADER_MODEL, D3D12CreateDevice, ID3D12Device,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_CREATE_FACTORY_FLAGS, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter1, IDXGIAdapter3, IDXGIFactory4,
    IDXGIFactory6,
};

/// What one adapter is, as far as the choice needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// The name Windows reports.
    pub name: String,
    /// The PCI vendor id (0x1414 for Microsoft's software adapter).
    pub vendor: u32,
    /// A software rasteriser (`DXGI_ADAPTER_FLAG_SOFTWARE`, or the Microsoft
    /// basic renderer).
    pub software: bool,
    /// Dedicated video memory in bytes; tiny or zero on a chip that shares
    /// the PC's memory.
    pub dedicated: u64,
    /// System memory the adapter may share, in bytes.
    pub shared: u64,
    /// The adapter's LUID as `(high, low)`.
    pub luid: (i32, u32),
    /// A feature level 12_0 device could be made on it.
    pub made: bool,
    /// That device runs Shader Model 6.6.
    pub sm66: bool,
}

impl Info {
    /// The LUID as the log and `victory check` write it.
    pub fn luid_text(&self) -> String {
        format!("{:08x}:{:08x}", self.luid.0 as u32, self.luid.1)
    }
}

/// What `VICTORY_ADAPTER` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// Nothing: the best adapter.
    Auto,
    /// The adapter at this position of the list (the order `victory check`
    /// prints, the system's high-performance order).
    Index(usize),
    /// The first adapter whose name contains this, whatever the case.
    Name(String),
}

/// Reads the value of `VICTORY_ADAPTER`: empty is [`Ask::Auto`], digits are
/// an index, anything else a name part.
pub fn parse_ask(value: Option<&str>) -> Ask {
    match value.map(str::trim) {
        None | Some("") => Ask::Auto,
        Some(v) => v.parse::<usize>().map_or_else(|_| Ask::Name(v.to_owned()), Ask::Index),
    }
}

/// The position in `list` of the adapter to use, or `None` when there is
/// none (the caller then keeps Windows' default). An explicit ask wins when
/// that adapter makes a device; an ask that cannot be met falls back to the
/// automatic choice: among the adapters that make a device and have Shader
/// Model 6.6, software adapters left out unless `allow_software` (with it,
/// they are taken first), the one with the most dedicated video memory, and
/// of equal ones the first in `list` order (the system's high-performance
/// order). A chip that shares the PC's memory names a few hundred MB at most,
/// so a dedicated card always outranks it.
pub fn pick(list: &[Info], ask: &Ask, allow_software: bool) -> Option<usize> {
    pick_asked(list, ask, allow_software).or_else(|| pick_auto(list, allow_software))
}

/// The adapter an explicit ask names, if it can be used.
pub fn pick_asked(list: &[Info], ask: &Ask, allow_software: bool) -> Option<usize> {
    let usable = |i: &Info| i.made && (allow_software || !i.software);
    match ask {
        Ask::Auto => None,
        Ask::Index(n) => list.get(*n).filter(|i| usable(i)).map(|_| *n),
        Ask::Name(part) => {
            let part = part.to_lowercase();
            list.iter().position(|i| usable(i) && i.name.to_lowercase().contains(&part))
        }
    }
}

fn pick_auto(list: &[Info], allow_software: bool) -> Option<usize> {
    let ok = |i: &Info| i.made && i.sm66;
    if allow_software && let Some(at) = list.iter().position(|i| i.software && ok(i)) {
        return Some(at);
    }
    // `max_by_key` keeps the last of equal keys: walk the list backwards.
    list.iter().enumerate().rev().filter(|(_, i)| !i.software && ok(i)).max_by_key(|(_, i)| i.dedicated).map(|(at, _)| at)
}

/// The memory, in bytes, that decides whether a card is "small": its
/// dedicated memory, but for a chip that shares the PC's memory (`uma`, or no
/// dedicated memory at all) the local budget the system gives this process,
/// since its dedicated figure is a few hundred MB or zero whatever the PC has.
pub fn card_bytes_of(dedicated: u64, uma: bool, local_budget: Option<u64>) -> u64 {
    if uma || dedicated == 0 { local_budget.unwrap_or(dedicated) } else { dedicated }
}

/// [`card_bytes_of`] for the adapter `device` is on.
pub fn card_bytes(device: &ID3D12Device) -> Option<u64> {
    // SAFETY: plain queries that fill structures of the size they are given.
    unsafe {
        let factory = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter = factory.EnumAdapterByLuid::<IDXGIAdapter3>(device.GetAdapterLuid()).ok()?;
        let dedicated = adapter.GetDesc().ok()?.DedicatedVideoMemory as u64;
        let mut local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let budget = adapter
            .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local)
            .ok()
            .map(|()| local.Budget);
        Some(card_bytes_of(dedicated, is_uma(device), budget))
    }
}

fn is_uma(device: &ID3D12Device) -> bool {
    let mut arch = D3D12_FEATURE_DATA_ARCHITECTURE::default();
    // SAFETY: fills the structure it is given, of the size it is given.
    unsafe {
        device
            .CheckFeatureSupport(D3D12_FEATURE_ARCHITECTURE, (&raw mut arch).cast(), size_of_val(&arch) as u32)
            .is_ok()
            && arch.UMA.as_bool()
    }
}

fn runs_sm66(device: &ID3D12Device) -> bool {
    let mut model = D3D12_FEATURE_DATA_SHADER_MODEL { HighestShaderModel: D3D_SHADER_MODEL_6_6 };
    // SAFETY: fills the structure it is given, of the size it is given. The
    // query fails for a runtime that does not know the model.
    unsafe {
        device
            .CheckFeatureSupport(D3D12_FEATURE_SHADER_MODEL, (&raw mut model).cast(), size_of_val(&model) as u32)
            .is_ok()
            && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0
    }
}

/// The adapter a run uses, with its device.
pub struct Chosen {
    /// The adapter.
    pub adapter: IDXGIAdapter1,
    /// The device made on it.
    pub device: ID3D12Device,
    /// Every adapter found, in the system's order.
    pub list: Vec<Info>,
    /// This adapter's position in `list`.
    pub index: usize,
}

impl Chosen {
    /// The adapters not chosen.
    pub fn others(&self) -> Vec<Info> {
        self.list.iter().enumerate().filter(|(i, _)| *i != self.index).map(|(_, a)| a.clone()).collect()
    }
}

fn enumerate() -> Vec<IDXGIAdapter1> {
    // SAFETY: plain DXGI queries.
    unsafe {
        let mut found = Vec::new();
        if let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) {
            let mut i = 0;
            while let Ok(a) = factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(i, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE) {
                found.push(a);
                i += 1;
            }
        }
        if found.is_empty()
            && let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
        {
            let mut i = 0;
            while let Ok(a) = factory.EnumAdapters1(i) {
                found.push(a);
                i += 1;
            }
        }
        found
    }
}

/// Finds the adapter to use and makes the device on it. `None` when DXGI
/// lists nothing or no adapter qualifies: the caller then makes its device on
/// Windows' default adapter as it did before, and reports what that gives.
///
/// Logs the choice once per process.
pub fn choose() -> Option<Chosen> {
    // `VICTORY_ADAPTER=<index or name>`: the graphics adapter to run on instead of the best one: its position in the list
    // `victory check` prints (0 is the system's high-performance choice), or part of its name whatever the case
    // (`VICTORY_ADAPTER=geforce`). An adapter that is not there or makes no Direct3D 12 device is said in the log and the best one is
    // used. Unset, the first non-software adapter with feature level 12_0 and Shader Model 6.6 is used, in the system's
    // high-performance order, so a laptop runs on its dedicated chip.
    let ask = parse_ask(std::env::var("VICTORY_ADAPTER").ok().as_deref());
    choose_for(&ask, crate::switch::on("VICTORY_D3D12_WARP"))
}

fn choose_for(ask: &Ask, allow_software: bool) -> Option<Chosen> {
    let adapters = enumerate();
    let mut devices: Vec<Option<ID3D12Device>> = Vec::new();
    let mut list = Vec::new();
    for adapter in &adapters {
        // SAFETY: plain queries and device creation.
        let (info, device) = unsafe {
            let desc = adapter.GetDesc1().ok();
            let end = |d: &[u16]| d.iter().position(|&c| c == 0).unwrap_or(d.len());
            let vendor = desc.as_ref().map_or(0, |d| d.VendorId);
            let software = desc.as_ref().is_some_and(|d| {
                d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || (d.VendorId == 0x1414 && d.DeviceId == 0x8c)
            });
            let mut device: Option<ID3D12Device> = None;
            // A software adapter that will not be used is not worth a device.
            if allow_software || !software {
                let _ = D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_0, &mut device);
            }
            let info = Info {
                name: desc
                    .as_ref()
                    .map(|d| String::from_utf16_lossy(&d.Description[..end(&d.Description)]).trim().to_owned())
                    .unwrap_or_default(),
                vendor,
                software,
                dedicated: desc.as_ref().map_or(0, |d| d.DedicatedVideoMemory as u64),
                shared: desc.as_ref().map_or(0, |d| d.SharedSystemMemory as u64),
                luid: desc.as_ref().map_or((0, 0), |d| (d.AdapterLuid.HighPart, d.AdapterLuid.LowPart)),
                made: device.is_some(),
                sm66: device.as_ref().is_some_and(runs_sm66),
            };
            (info, device)
        };
        list.push(info);
        devices.push(device);
    }
    let asked = pick_asked(&list, ask, allow_software);
    let index = asked.or_else(|| pick_auto(&list, allow_software));
    if *ask != Ask::Auto && asked.is_none() {
        tracing::warn!(?ask, "VICTORY_ADAPTER names no adapter that makes a Direct3D 12 device: the best one is used");
    }
    let index = index?;
    let device = devices.swap_remove(index)?;
    let adapter = adapters.into_iter().nth(index)?;
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        let at = &list[index];
        tracing::info!(
            name = %at.name,
            vendor = format_args!("{:#06x}", at.vendor),
            dedicated_mb = at.dedicated >> 20,
            shared_mb = at.shared >> 20,
            luid = %at.luid_text(),
            index,
            adapters = list.len(),
            "graphics adapter chosen (VICTORY_ADAPTER picks another)"
        );
    });
    Some(Chosen { adapter, device, list, index })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, vendor: u32, software: bool, mb: u64, sm66: bool) -> Info {
        Info {
            name: name.into(),
            vendor,
            software,
            dedicated: mb << 20,
            shared: 0,
            luid: (0, 0),
            made: true,
            sm66,
        }
    }

    fn hybrid() -> Vec<Info> {
        // The order DXGI gives for high performance on a laptop is already
        // dedicated first; the tests also cover the other order.
        vec![
            rec("Intel(R) UHD Graphics", 0x8086, false, 128, true),
            rec("Example Discrete Laptop GPU", 0x10de, false, 8188, true),
        ]
    }

    #[test]
    fn a_hybrid_laptop_gets_its_dedicated_chip() {
        // Integrated chip first (Windows' default order), dedicated second.
        assert_eq!(pick(&hybrid(), &Ask::Auto, false), Some(1));
        let mut list = hybrid();
        list.reverse();
        assert_eq!(pick(&list, &Ask::Auto, false), Some(0));
    }

    #[test]
    fn equal_cards_keep_the_systems_order() {
        let list = vec![rec("Card A", 0x10de, false, 8188, true), rec("Card B", 0x1002, false, 8188, true)];
        assert_eq!(pick(&list, &Ask::Auto, false), Some(0));
    }

    #[test]
    fn a_single_integrated_chip_is_used() {
        let list = vec![rec("Intel(R) UHD Graphics", 0x8086, false, 128, true)];
        assert_eq!(pick(&list, &Ask::Auto, false), Some(0));
    }

    #[test]
    fn an_adapter_without_sm66_is_passed_over() {
        let mut list = hybrid();
        list[1].sm66 = false;
        assert_eq!(pick(&list, &Ask::Auto, false), Some(0));
    }

    #[test]
    fn software_adapters_are_skipped_unless_allowed() {
        let list = vec![rec("Microsoft Basic Render Driver", 0x1414, true, 0, true), hybrid()[1].clone()];
        assert_eq!(pick(&list, &Ask::Auto, false), Some(1));
        assert_eq!(pick(&list, &Ask::Auto, true), Some(0));
        let only = vec![list[0].clone()];
        assert_eq!(pick(&only, &Ask::Auto, false), None);
        assert_eq!(pick(&only, &Ask::Index(0), false), None);
        assert_eq!(pick(&only, &Ask::Index(0), true), Some(0));
    }

    #[test]
    fn an_explicit_index_or_name_wins() {
        let list = hybrid();
        assert_eq!(pick(&list, &Ask::Index(1), false), Some(1));
        assert_eq!(pick(&list, &parse_ask(Some(" 1 ")), false), Some(1));
        assert_eq!(pick(&list, &parse_ask(Some("intel")), false), Some(0));
        assert_eq!(pick(&list, &parse_ask(Some("geforce")), false), Some(1));
        assert_eq!(pick(&list, &parse_ask(Some("GEFORCE rtx")), false), Some(1));
    }

    #[test]
    fn an_ask_that_cannot_be_met_falls_back_to_the_best() {
        let mut list = hybrid();
        list[1].made = false;
        assert_eq!(pick(&list, &Ask::Index(9), false), Some(0));
        assert_eq!(pick(&list, &parse_ask(Some("nothing like it")), false), Some(0));
        // An adapter that makes no device is not taken even when named.
        assert_eq!(pick(&list, &Ask::Index(1), false), Some(0));
        assert_eq!(pick_asked(&list, &Ask::Index(1), false), None);
    }

    #[test]
    fn nothing_usable_is_none() {
        assert_eq!(pick(&[], &Ask::Auto, false), None);
        let mut list = hybrid();
        list.iter_mut().for_each(|a| a.sm66 = false);
        assert_eq!(pick(&list, &Ask::Auto, false), None);
        list.iter_mut().for_each(|a| a.made = false);
        assert_eq!(pick(&list, &Ask::Auto, false), None);
    }

    #[test]
    fn the_ask_is_read_as_index_or_name() {
        assert_eq!(parse_ask(None), Ask::Auto);
        assert_eq!(parse_ask(Some("  ")), Ask::Auto);
        assert_eq!(parse_ask(Some("0")), Ask::Index(0));
        assert_eq!(parse_ask(Some("Radeon")), Ask::Name("Radeon".into()));
    }

    #[test]
    fn a_shared_memory_chip_is_sized_by_its_budget() {
        const GIB: u64 = 1 << 30;
        // A card with its own memory: the dedicated figure, as before.
        assert_eq!(card_bytes_of(8 * GIB, false, Some(7 * GIB)), 8 * GIB);
        // A chip sharing a 64 GB PC's memory: not "small" because it names 128 MB.
        assert_eq!(card_bytes_of(128 << 20, true, Some(32 * GIB)), 32 * GIB);
        assert_eq!(card_bytes_of(0, false, Some(16 * GIB)), 16 * GIB);
        assert_eq!(card_bytes_of(0, true, None), 0);
    }

    /// With or without a card: a choice is made or refused, never a panic, and
    /// the chosen device is on the chosen adapter.
    #[test]
    fn choosing_on_this_pc_is_consistent() {
        if let Some(c) = choose_for(&Ask::Auto, false) {
            assert!(!c.list[c.index].software);
            // SAFETY: plain queries.
            let luid = unsafe { c.device.GetAdapterLuid() };
            assert_eq!((luid.HighPart, luid.LowPart), c.list[c.index].luid);
            assert_eq!(c.others().len() + 1, c.list.len());
        }
    }
}
