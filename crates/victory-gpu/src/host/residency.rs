//! How much of the card's memory each kind of ours should keep in the
//! card's own memory when the system is short of it (residency priorities),
//! and, for the memory line, what only this module's parent sees of the host
//! lists and of what was let go.
//!
//! The system moves memory of the card into the PC's when the process is
//! over its budget (at about 95 % in use on an 8 GB card), the lowest
//! priority first, and everything has the same
//! priority unless told. The hints ([`Host::hint_residency`]): high for
//! render targets, depth and unordered-access textures of their own memory,
//! and for textures the title fills (of their own memory) used in the last
//! second; low for those idle for five seconds; none (the system's normal)
//! for buffers in the card's memory whose bytes the PC's memory holds too (a
//! CPU twin), unless `VICTORY_RESIDENCY_CARD_COPIES=minimum` (or `low`) moves
//! them first: shaders read those every frame, so textures idle for five
//! seconds should leave the card's memory before them. Hints only:
//! nothing is made resident or evicted by them, and nothing the card reads
//! goes missing or changes. A placed resource has its heap's priority: blocks
//! of textures (`crate::pool`) keep the system's. `VICTORY_RESIDENCY_PRIORITY=0`
//! gives none, as before.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::Host;

/// The device's interface that takes the hints, asked once.
struct Device1(Option<ID3D12Device1>);
// SAFETY: D3D12 devices are free-threaded.
unsafe impl Send for Device1 {}
// SAFETY: as above.
unsafe impl Sync for Device1 {}

/// Hints given since the start, by class ([`Host::RESIDENCY_MINIMUM`] to
/// [`Host::RESIDENCY_HIGH`] at 1 to 4), and calls the system refused (at 0).
static HINTS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// How long a texture the title fills stays high after a use, and how long
/// it is idle before it is low ([`Host::filled_residency`]).
const USED_MS: u64 = 1_000;
const IDLE_MS: u64 = 5_000;

/// The system's priority for one of our classes; `None` for no hint.
fn priority(class: u8) -> Option<D3D12_RESIDENCY_PRIORITY> {
    Some(match class {
        Host::RESIDENCY_MINIMUM => D3D12_RESIDENCY_PRIORITY_MINIMUM,
        Host::RESIDENCY_LOW => D3D12_RESIDENCY_PRIORITY_LOW,
        Host::RESIDENCY_NORMAL => D3D12_RESIDENCY_PRIORITY_NORMAL,
        Host::RESIDENCY_HIGH => D3D12_RESIDENCY_PRIORITY_HIGH,
        _ => return None,
    })
}

/// [`Host::card_copy_residency`] of what the variable says.
fn card_copy_class(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("minimum") => Host::RESIDENCY_MINIMUM,
        Some("low") => Host::RESIDENCY_LOW,
        _ => Host::RESIDENCY_UNSET,
    }
}

/// [`Host::filled_residency`].
fn filled_after(had: u8, idle_ms: u64) -> u8 {
    if idle_ms < USED_MS {
        Host::RESIDENCY_HIGH
    } else if idle_ms >= IDLE_MS {
        Host::RESIDENCY_LOW
    } else if had == Host::RESIDENCY_UNSET {
        Host::RESIDENCY_NORMAL
    } else {
        had
    }
}

impl Host {
    /// Residency classes, as `crate::resource` keeps them per host texture:
    /// none given yet (the system's normal), then from the lowest priority
    /// to the highest.
    pub(crate) const RESIDENCY_UNSET: u8 = 0;
    pub(crate) const RESIDENCY_MINIMUM: u8 = 1;
    pub(crate) const RESIDENCY_LOW: u8 = 2;
    pub(crate) const RESIDENCY_NORMAL: u8 = 3;
    pub(crate) const RESIDENCY_HIGH: u8 = 4;

    /// Whether hints are given (`VICTORY_RESIDENCY_PRIORITY`; on unless `0`).
    pub(crate) fn residency_hints() -> bool {
        crate::toggled!("VICTORY_RESIDENCY_PRIORITY")
    }

    /// The class a texture the title fills, of its own memory, should be in
    /// when it was last used `idle_ms` ago and is in `had` now: high within a
    /// second of a use, low from five seconds without one, and in between as
    /// it was (normal if it had none), so that a texture used every few
    /// seconds is not moved back and forth.
    pub(crate) fn filled_residency(had: u8, idle_ms: u64) -> u8 {
        filled_after(had, idle_ms)
    }

    /// The class buffers in the card's memory with a CPU twin are given
    /// (`VICTORY_RESIDENCY_CARD_COPIES`): none by default, the system's
    /// normal, as before (shaders read them every frame, and idle textures,
    /// low, should go first); `minimum` or `low` to move them sooner.
    pub(crate) fn card_copy_residency() -> u8 {
        static CLASS: OnceLock<u8> = OnceLock::new();
        *CLASS.get_or_init(|| card_copy_class(std::env::var("VICTORY_RESIDENCY_CARD_COPIES").ok().as_deref()))
    }

    fn device1(&self) -> Option<&ID3D12Device1> {
        static DEVICE: OnceLock<Device1> = OnceLock::new();
        DEVICE.get_or_init(|| Device1(self.device.cast::<ID3D12Device1>().ok())).0.as_ref()
    }

    /// Gives each of `objects` (committed resources) the priority of its
    /// class, in one call; nothing without [`Host::residency_hints`].
    pub(crate) fn hint_residency(&self, objects: &[(&ID3D12Resource, u8)]) {
        if objects.is_empty() || !Host::residency_hints() {
            return;
        }
        let Some(device) = self.device1() else {
            return;
        };
        let (mut pageables, mut priorities) = (Vec::with_capacity(objects.len()), Vec::with_capacity(objects.len()));
        for &(object, class) in objects {
            if let (Some(p), Ok(pageable)) = (priority(class), object.cast::<ID3D12Pageable>()) {
                pageables.push(Some(pageable));
                priorities.push(p);
                HINTS[usize::from(class)].fetch_add(1, Ordering::Relaxed);
            }
        }
        if pageables.is_empty() {
            return;
        }
        // SAFETY: two arrays of the same length, of live objects of the
        // device and their priorities.
        let done = unsafe { device.SetResidencyPriority(pageables.len() as u32, pageables.as_ptr(), priorities.as_ptr()) };
        if let Err(e) = done
            && HINTS[0].fetch_add(1, Ordering::Relaxed) == 0
        {
            tracing::warn!(error = %e, "the system refused residency priorities (VICTORY_RESIDENCY_PRIORITY=0: none are given)");
        }
    }

    /// For the memory line: residency hints given since the start, by class
    /// (lowest, low, normal, high), and calls the system refused.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn residency_census() -> [u64; 5] {
        [1, 2, 3, 4, 0].map(|i| HINTS[i].load(Ordering::Relaxed))
    }

    /// For the memory line: the host lists of flights on the card, and those
    /// of flights that have landed and wait out the bury lag before they are
    /// reset and kept (`super::land`); `None` while the line is busy (not
    /// waited for: the line is said from threads that may hold it).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn lists_in_the_air(&self) -> Option<[u64; 2]> {
        let line = match super::LINE.try_lock() {
            Ok(line) => line,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        let flying = line
            .line
            .iter()
            .map(|late| match late {
                super::Late::Run(flight) => flight.lists.len() as u64,
                _ => 0,
            })
            .sum();
        let buried = line.landed.iter().map(|(lists, _)| lists.len() as u64).sum();
        Some([flying, buried])
    }

    /// For the memory line: the objects let go that wait for the submissions
    /// that may still use them (`super::retire`), and the addresses of the
    /// buffers among them (resources, and the buffers of host copies of
    /// title memory), which the census of buffers the CPU sees
    /// (`crate::book::census`) finds; `None` while the list is busy.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn buried(&self) -> Option<(u64, Vec<usize>)> {
        let mut raws = Vec::new();
        let n = super::RETIRED.try_each(|what| match what {
            super::Retired::Resource(resource) => raws.push(resource.as_raw() as usize),
            super::Retired::Any(any) => {
                if let Some(shadow) = any.downcast_ref::<std::sync::Arc<crate::shadow::Shadow>>() {
                    raws.push(shadow.buffer.as_raw() as usize);
                    if let Some((Some(staging), _, _)) = shadow.card_sides() {
                        raws.push(staging.as_raw() as usize);
                    }
                }
            }
        })?;
        Some((n, raws))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// High within a second of a use, low from five seconds idle, and in
    /// between whatever it was (normal when it had nothing yet).
    #[test]
    fn a_filled_texture_is_high_when_used_and_low_when_idle() {
        let all = [
            Host::RESIDENCY_UNSET,
            Host::RESIDENCY_MINIMUM,
            Host::RESIDENCY_LOW,
            Host::RESIDENCY_NORMAL,
            Host::RESIDENCY_HIGH,
        ];
        for had in all {
            assert_eq!(filled_after(had, 0), Host::RESIDENCY_HIGH);
            assert_eq!(filled_after(had, 999), Host::RESIDENCY_HIGH);
            assert_eq!(filled_after(had, 5_000), Host::RESIDENCY_LOW);
            assert_eq!(filled_after(had, 600_000), Host::RESIDENCY_LOW);
        }
        for idle in [1_000, 2_500, 4_999] {
            assert_eq!(filled_after(Host::RESIDENCY_UNSET, idle), Host::RESIDENCY_NORMAL);
            assert_eq!(filled_after(Host::RESIDENCY_HIGH, idle), Host::RESIDENCY_HIGH);
            assert_eq!(filled_after(Host::RESIDENCY_LOW, idle), Host::RESIDENCY_LOW);
        }
    }

    /// Each class has its own priority, in the order of the classes; no
    /// class, no hint.
    #[test]
    fn classes_map_to_rising_priorities() {
        assert_eq!(priority(Host::RESIDENCY_UNSET), None);
        assert_eq!(priority(9), None);
        let ranks: Vec<u32> = [Host::RESIDENCY_MINIMUM, Host::RESIDENCY_LOW, Host::RESIDENCY_NORMAL, Host::RESIDENCY_HIGH]
            .iter()
            .map(|&c| priority(c).map_or(0, |p| p.0 as u32))
            .collect();
        assert!(ranks.windows(2).all(|w| w[0] < w[1]), "{ranks:x?}");
        assert_eq!(priority(Host::RESIDENCY_NORMAL), Some(D3D12_RESIDENCY_PRIORITY_NORMAL));
    }

    /// Card buffers with a CPU twin get no hint (the system's normal, above
    /// idle textures' low) unless told to go sooner.
    #[test]
    fn card_copies_keep_the_normal_priority_unless_told() {
        assert_eq!(card_copy_class(None), Host::RESIDENCY_UNSET);
        assert_eq!(priority(card_copy_class(None)), None);
        assert_eq!(card_copy_class(Some("")), Host::RESIDENCY_UNSET);
        assert_eq!(card_copy_class(Some("normal")), Host::RESIDENCY_UNSET);
        assert_eq!(card_copy_class(Some(" minimum ")), Host::RESIDENCY_MINIMUM);
        assert_eq!(card_copy_class(Some("low")), Host::RESIDENCY_LOW);
    }
}
