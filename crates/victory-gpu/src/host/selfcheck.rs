//! What `crate::selftest` asks of the host that `Host` keeps to itself:
//! which adapter the device is on, and what the swap chain made of the
//! presents. Read-only; nothing a title run calls.

use super::*;

/// The adapter of the host device, as DXGI names it.
pub(crate) struct Adapter {
    pub(crate) name: String,
    pub(crate) software: bool,
    /// The user-mode driver's version, `a.b.c.d` (empty: not known).
    pub(crate) driver: String,
    pub(crate) vram_mb: u64,
}

/// The presents the swap chain took.
pub(crate) struct Presents {
    /// `IDXGISwapChain::GetLastPresentCount`: the presents it was given.
    pub(crate) count: u32,
    /// What its last `Present` returned.
    pub(crate) last: windows::core::HRESULT,
    pub(crate) size: (u32, u32),
    /// Whether our window is the one it shows in.
    pub(crate) window: bool,
}

impl Host {
    pub(crate) fn adapter_facts(&self) -> Option<Adapter> {
        // SAFETY: plain queries of a live adapter.
        let desc = unsafe { self.adapter.as_ref()?.GetDesc1().ok()? };
        let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
        let v = self.driver();
        let driver = if v == 0 {
            String::new()
        } else {
            format!("{}.{}.{}.{}", v >> 48 & 0xffff, v >> 32 & 0xffff, v >> 16 & 0xffff, v & 0xffff)
        };
        Some(Adapter {
            name: String::from_utf16_lossy(&desc.Description[..len]),
            software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
            driver,
            vram_mb: desc.DedicatedVideoMemory as u64 >> 20,
        })
    }

    /// `None` before the first present made the swap chain.
    pub(crate) fn presents(&self) -> Option<Presents> {
        let presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        let p = presenter.as_ref()?;
        // SAFETY: a plain query of our swap chain.
        let count = unsafe { p.swap_chain.GetLastPresentCount() }.unwrap_or(0);
        Some(Presents {
            count,
            last: p.meter.last_present,
            size: (p.width, p.height),
            // SAFETY: a plain query of a window handle.
            window: unsafe { IsWindow(Some(p.hwnd)) }.as_bool(),
        })
    }
}
