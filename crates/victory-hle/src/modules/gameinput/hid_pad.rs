//! Gamepads that Windows does not list through XInput, read straight from
//! HID (`VICTORY_HID_PAD`, on by default; `0` leaves XInput as the only
//! source). The pads meant are a family of HID gamepads with a touch surface
//! and motion sensors, over USB or Bluetooth; they are recognised by vendor
//! and product id ([`VENDOR`], [`PRODUCTS`]).
//!
//! Ported from trustno1-coder/VIctory-NVIDIA 025abd3 (by Blitzball996),
//! reworked: neutral names, our own stick mapping, no rumble.
//!
//! How it works: a scan thread lists the HID devices every [`SCAN`] and
//! opens the supported ones; each open pad has a reader thread that keeps the
//! latest parsed report. The title's input thread only copies those latest
//! states under a short lock, so it never waits for a device. XInput has
//! priority: HID pads are asked for only while no XInput slot answers, since
//! a remapper can show the same physical pad through both.
//!
//! There are no output reports here: no rumble, no lights, and so no output
//! checksum to compute.

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

/// The vendor id of the supported pads.
#[cfg(windows)]
const VENDOR: u16 = 0x054C;
/// Their product ids: the standard model and the one with extra controls.
#[cfg(windows)]
const PRODUCTS: [u16; 2] = [0x0CE6, 0x0DF2];

/// Whether HID pads are read at all.
#[cfg(windows)]
pub(super) fn enabled() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // Gamepads that XInput does not list are read from HID (USB or
        // Bluetooth) in the slots XInput leaves empty (`VICTORY_HID_PAD`, on
        // unless `0`, read once; `0`: XInput only). It adds a device and
        // changes nothing for XInput pads.
        std::env::var_os("VICTORY_HID_PAD").is_none_or(|v| v != "0")
    });
    *ON
}

/// The D-pad hat (low nibble of the first button byte): 0 up, clockwise in
/// eighths, 8 and above centred.
fn hat(value: u8) -> u32 {
    match value & 0xF {
        0 => UP,
        1 => UP | RIGHT,
        2 => RIGHT,
        3 => RIGHT | DOWN,
        4 => DOWN,
        5 => DOWN | LEFT,
        6 => LEFT,
        7 => LEFT | UP,
        _ => 0,
    }
}

/// Button bits: byte of the three button bytes, mask, our button. The pad's
/// home and microphone buttons have no counterpart.
const BUTTONS: [(usize, u8, u32); 11] = [
    (0, 0x10, X), // west face button
    (0, 0x20, A), // south
    (0, 0x40, B), // east
    (0, 0x80, Y), // north
    (1, 0x01, LEFT_SHOULDER),
    (1, 0x02, RIGHT_SHOULDER),
    (1, 0x10, VIEW),  // the small left button
    (1, 0x20, MENU),  // the small right button
    (1, 0x40, LEFT_STICK),
    (1, 0x80, RIGHT_STICK),
    (2, 0x02, VIEW), // press on the touch surface
];

/// A stick byte as -1 to 1 with 128 at rest, exactly 0 there and exactly
/// -1 and 1 at the ends (raw, like our XInput reading: titles apply their
/// own dead zone). The pad counts down as positive; callers flip that.
fn axis(value: u8) -> f32 {
    let from_rest = f32::from(value) - 128.0;
    if from_rest < 0.0 {
        from_rest / 128.0
    } else {
        from_rest / 127.0
    }
}

/// The pad in one input report, `None` for a report of another kind or one
/// that is too short. `bluetooth`: the full report there is 0x31, one byte
/// further in than over USB, and 0x01 is the short report the pad sends
/// before it was asked for the full one.
pub(super) fn parse(report: &[u8], bluetooth: bool) -> Option<PadState> {
    // Offsets of the sticks (LX LY RX RY), the triggers (L2 R2) and the
    // three button bytes.
    let (sticks, triggers, buttons) = match (report.first()?, bluetooth) {
        (0x01, false) => (1, 5, 8),
        (0x31, true) => (2, 6, 9),
        (0x01, true) => (1, 8, 5),
        _ => return None,
    };
    if report.len() < sticks + 4 || report.len() < triggers + 2 || report.len() < buttons + 3 {
        return None;
    }
    let bytes = &report[buttons..buttons + 3];
    Some(PadState {
        buttons: BUTTONS
            .iter()
            .filter(|&&(byte, mask, _)| bytes[byte] & mask != 0)
            .fold(hat(bytes[0]), |all, &(_, _, bit)| all | bit),
        left_trigger: f32::from(report[triggers]) / 255.0,
        right_trigger: f32::from(report[triggers + 1]) / 255.0,
        left_x: axis(report[sticks]),
        left_y: -axis(report[sticks + 1]),
        right_x: axis(report[sticks + 2]),
        right_y: -axis(report[sticks + 3]),
    })
}

/// The Bluetooth address in a pairing feature report (id 0x09, the address
/// at 1..7 over both transports), `None` when it is too short or all zeros.
/// It tells one pad seen over USB and over Bluetooth from two pads.
fn address(pairing: &[u8]) -> Option<[u8; 6]> {
    let address: [u8; 6] = pairing.get(1..7)?.try_into().ok()?;
    (address != [0; 6]).then_some(address)
}

/// The slots for this poll: XInput's, and where `enabled` and no XInput slot
/// answers, the HID pads (asked for only then) in the free slots. A HID pad
/// keeps its slot while it stays connected: `assigned` holds each slot's
/// owner (a connection id) between calls.
pub(super) fn slots(
    enabled: bool,
    mut xinput: [Option<PadState>; super::PADS],
    assigned: &mut [Option<u64>; super::PADS],
    hid: impl FnOnce() -> Vec<(u64, PadState)>,
) -> [Option<PadState>; super::PADS] {
    if !enabled || xinput.iter().any(Option::is_some) {
        assigned.fill(None);
        return xinput;
    }
    let pads = hid();
    for (slot, owner) in xinput.iter_mut().zip(assigned.iter_mut()) {
        match pads.iter().find(|(id, _)| Some(*id) == *owner) {
            Some((_, state)) => *slot = Some(*state),
            None => *owner = None,
        }
    }
    for (id, state) in pads {
        if assigned.contains(&Some(id)) {
            continue;
        }
        let Some(slot) = xinput.iter().position(Option::is_none) else {
            break;
        };
        xinput[slot] = Some(state);
        assigned[slot] = Some(id);
    }
    xinput
}

#[cfg(windows)]
pub(super) use host::pads;

#[cfg(windows)]
mod host {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{LazyLock, Mutex, PoisonError};
    use std::time::Duration;

    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
        SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList,
        SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    };
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FreePreparsedData,
        HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid, HidD_GetPreparsedData, HidP_GetCaps,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        ReadFile,
    };
    use windows_sys::Win32::System::IO::{
        CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
    };
    use windows_sys::Win32::System::Threading::CreateEventW;

    use super::{PRODUCTS, PadState, VENDOR, address, parse};

    /// How often to look for newly connected pads.
    const SCAN: Duration = Duration::from_secs(2);

    /// A pad in full-report mode sends reports all the time; this long
    /// without one and we ask it whether it is still there. Over Bluetooth
    /// Windows can keep the device after the pad is switched off or out of
    /// range, and a read then waits for ever.
    const QUIET_MS: u32 = 500;

    struct Pad {
        /// This connection's: a pad that reconnects gets a new one.
        id: u64,
        path: Vec<u16>,
        /// The pad's Bluetooth address, the same over USB and Bluetooth:
        /// one pad seen through both is opened once.
        address: Option<[u8; 6]>,
        /// `None` until the first report, and while the pad is silent.
        state: Option<PadState>,
    }

    /// The open pads, in the order they were found.
    static PADS: Mutex<Vec<Pad>> = Mutex::new(Vec::new());

    fn lock() -> std::sync::MutexGuard<'static, Vec<Pad>> {
        PADS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Paths found to be a pad that is already open over its other
    /// transport, with its address: not opened again on every scan while
    /// that pad stays open.
    static TWINS: Mutex<Vec<(Vec<u16>, [u8; 6])>> = Mutex::new(Vec::new());

    fn twins() -> std::sync::MutexGuard<'static, Vec<(Vec<u16>, [u8; 6])>> {
        TWINS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The open pads' latest states with their connection ids, oldest
    /// connection first, without the pads that are silent or yet to report.
    /// Never touches a device; the first call starts the scan thread.
    pub(in super::super) fn pads() -> Vec<(u64, PadState)> {
        static STARTED: LazyLock<()> = LazyLock::new(|| {
            let started = std::thread::Builder::new()
                .name("HID pad scan".into())
                .spawn(|| {
                    loop {
                        scan();
                        std::thread::sleep(SCAN);
                    }
                });
            if let Err(error) = started {
                tracing::warn!(%error, "could not start the HID pad scan");
            }
        });
        LazyLock::force(&STARTED);
        lock()
            .iter()
            .filter_map(|pad| Some((pad.id, pad.state?)))
            .collect()
    }

    /// Windows sizes each report buffer for its top-level HID collection.
    struct Reports {
        input: Vec<u8>,
        feature: Vec<u8>,
    }

    impl Reports {
        fn for_device(handle: HANDLE) -> Option<Self> {
            let mut data = 0;
            // SAFETY: the device is open and the output is writable.
            if !unsafe { HidD_GetPreparsedData(handle, &mut data) } {
                return None;
            }
            let mut caps = HIDP_CAPS::default();
            // SAFETY: `data` was obtained above and is released after its last use.
            let status = unsafe { HidP_GetCaps(data, &mut caps) };
            // SAFETY: as above; not used again.
            unsafe { HidD_FreePreparsedData(data) };
            if status != HIDP_STATUS_SUCCESS
                || caps.InputReportByteLength == 0
                || caps.FeatureReportByteLength == 0
            {
                return None;
            }
            Some(Self {
                input: vec![0; usize::from(caps.InputReportByteLength)],
                feature: vec![0; usize::from(caps.FeatureReportByteLength)],
            })
        }
    }

    /// Feature report `id` into `buffer`, `false` when the pad did not
    /// answer. A round trip to the pad, so over Bluetooth it also tells
    /// whether the pad is still there.
    fn feature(handle: HANDLE, id: u8, buffer: &mut [u8]) -> bool {
        buffer.fill(0);
        buffer[0] = id;
        // SAFETY: the buffer is as long as we say.
        unsafe { HidD_GetFeature(handle, buffer.as_mut_ptr().cast(), buffer.len() as u32) }
    }

    /// Asks for the calibration report (id 0x05): it switches a pad on
    /// Bluetooth to full reports, again after it reconnected; over USB it
    /// changes nothing. `false` when the pad did not answer.
    fn full_reports(handle: HANDLE, buffer: &mut [u8]) -> bool {
        feature(handle, 0x05, buffer)
    }

    /// Opens the pads not open yet, each with a reader thread.
    fn scan() {
        let paths = hid_paths();
        {
            let pads = lock();
            // A twin whose pad went away is opened again: over USB, say,
            // after the Bluetooth link dropped with the cable still in.
            twins().retain(|(path, address)| {
                paths.contains(path) && pads.iter().any(|pad| pad.address == Some(*address))
            });
        }
        for path in paths {
            // Cheap filter before opening anything: the vendor id is in the
            // path, as `vid_054c` (USB) or `vid&0002054c` (Bluetooth).
            let lower = String::from_utf16_lossy(&path).to_ascii_lowercase();
            if !lower.contains(&format!("{VENDOR:04x}")) {
                continue;
            }
            if lock().iter().any(|pad| pad.path == path)
                || twins().iter().any(|(twin, _)| *twin == path)
            {
                continue;
            }
            let Some((handle, mut reports)) = open(&path) else {
                continue;
            };
            let bluetooth = lower.contains("{00001124-");
            // A paired pad that is switched off can still be listed over
            // Bluetooth: it does not answer.
            if !full_reports(handle, &mut reports.feature) {
                tracing::debug!("HID pad listed but not answering");
                // SAFETY: just opened, nothing else has it.
                unsafe { CloseHandle(handle) };
                continue;
            }
            let address = if feature(handle, 0x09, &mut reports.feature) {
                address(&reports.feature)
            } else {
                None
            };
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = {
                let mut pads = lock();
                let twin = address.is_some() && pads.iter().any(|pad| pad.address == address);
                (!twin).then(|| {
                    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                    pads.push(Pad {
                        id,
                        path: path.clone(),
                        address,
                        state: None,
                    });
                    id
                })
            };
            let Some(id) = id else {
                tracing::info!(bluetooth, "HID pad already open over the other transport");
                if let Some(address) = address {
                    twins().push((path, address));
                }
                // SAFETY: just opened, nothing else has it.
                unsafe { CloseHandle(handle) };
                continue;
            };
            tracing::info!(bluetooth, "HID pad connected");
            let raw = handle as usize;
            if let Err(error) = std::thread::Builder::new()
                .name("HID pad reader".into())
                .spawn(move || read(raw as HANDLE, id, bluetooth, reports))
            {
                lock().retain(|pad| pad.id != id);
                // SAFETY: just opened, and no reader has it.
                unsafe { CloseHandle(handle) };
                tracing::warn!(%error, "could not start a HID pad reader");
            }
        }
    }

    /// Reads reports until the pad goes away, then forgets it.
    fn read(handle: HANDLE, id: u64, bluetooth: bool, mut reports: Reports) {
        let set = |state: Option<PadState>| {
            if let Some(pad) = lock().iter_mut().find(|pad| pad.id == id) {
                pad.state = state;
            }
        };
        // SAFETY: an unnamed manual-reset event; no security attributes.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        let why = 'pad: loop {
            if event.is_null() {
                break 'pad "no event";
            }
            // SAFETY: plain data; only the event is set.
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            overlapped.hEvent = event;
            let mut got = 0;
            // SAFETY: the buffer and `overlapped` outlive the read: every way
            // out below waits for it to end first.
            let started = unsafe {
                ReadFile(
                    handle,
                    reports.input.as_mut_ptr(),
                    reports.input.len() as u32,
                    std::ptr::null_mut(),
                    &mut overlapped,
                )
            };
            // SAFETY: plain query.
            if started == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
                break 'pad "read failed";
            }
            loop {
                // SAFETY: the read started above.
                let done =
                    unsafe { GetOverlappedResultEx(handle, &overlapped, &mut got, QUIET_MS, 0) };
                if done != 0 {
                    if got == 0 {
                        break 'pad "read nothing";
                    }
                    break;
                }
                // SAFETY: plain query.
                if unsafe { GetLastError() } != WAIT_TIMEOUT {
                    break 'pad "read failed";
                }
                // Silent: whatever was held is not held any more, and the
                // pad may be gone without the read failing.
                set(None);
                if !full_reports(handle, &mut reports.feature) {
                    // SAFETY: cancel the read and wait for it to end, so
                    // nothing writes into the input buffer once we return.
                    unsafe {
                        CancelIoEx(handle, &overlapped);
                        GetOverlappedResult(handle, &overlapped, &mut got, 1);
                    }
                    break 'pad "not answering";
                }
            }
            if let Some(state) = parse(&reports.input[..got as usize], bluetooth) {
                set(Some(state));
            }
        };
        tracing::info!(why, bluetooth, "HID pad disconnected");
        lock().retain(|pad| pad.id != id);
        // SAFETY: ours, and nothing uses them any more.
        unsafe {
            if !event.is_null() {
                CloseHandle(event);
            }
            CloseHandle(handle);
        }
    }

    /// The pad at `path` opened for reading, `None` when it is something else
    /// or cannot be opened.
    fn open(path: &[u16]) -> Option<(HANDLE, Reports)> {
        let mut name = path.to_vec();
        name.push(0);
        // SAFETY: a terminated path; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut attributes = HIDD_ATTRIBUTES {
            Size: size_of::<HIDD_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        // SAFETY: an open device and room for its attributes.
        let ours = unsafe { HidD_GetAttributes(handle, &mut attributes) }
            && attributes.VendorID == VENDOR
            && PRODUCTS.contains(&attributes.ProductID);
        if ours && let Some(reports) = Reports::for_device(handle) {
            return Some((handle, reports));
        }
        // SAFETY: just opened.
        unsafe { CloseHandle(handle) };
        None
    }

    /// The device paths of the HID devices present, without terminators.
    fn hid_paths() -> Vec<Vec<u16>> {
        let mut paths = Vec::new();
        // SAFETY: plain queries into buffers we own; the list is destroyed
        // before returning.
        unsafe {
            let mut guid = std::mem::zeroed();
            HidD_GetHidGuid(&mut guid);
            let set = SetupDiGetClassDevsW(
                &guid,
                std::ptr::null(),
                std::ptr::null_mut(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            );
            if set == INVALID_HANDLE_VALUE as isize {
                return paths;
            }
            for index in 0.. {
                let mut interface = SP_DEVICE_INTERFACE_DATA {
                    cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                    ..std::mem::zeroed()
                };
                if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, index, &mut interface)
                    == 0
                {
                    break;
                }
                let mut size = 0;
                SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &interface,
                    std::ptr::null_mut(),
                    0,
                    &mut size,
                    std::ptr::null_mut(),
                );
                if size == 0 {
                    continue;
                }
                // u64s keep the detail's alignment.
                let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
                let detail = buffer
                    .as_mut_ptr()
                    .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
                (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
                if SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &interface,
                    detail,
                    size,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ) == 0
                {
                    continue;
                }
                let start = std::ptr::addr_of!((*detail).DevicePath).cast::<u16>();
                let room = (size as usize
                    - std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath))
                    / 2;
                let wide = std::slice::from_raw_parts(start, room);
                let end = wide.iter().position(|&c| c == 0).unwrap_or(room);
                paths.push(wide[..end].to_vec());
            }
            SetupDiDestroyDeviceInfoList(set);
        }
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PADS: usize = super::super::PADS;

    fn pressing(buttons: u32) -> PadState {
        PadState {
            buttons,
            ..PadState::default()
        }
    }

    fn usb(sticks: [u8; 4], triggers: [u8; 2], buttons: [u8; 3]) -> [u8; 64] {
        let mut report = [0u8; 64];
        report[0] = 0x01;
        report[1..5].copy_from_slice(&sticks);
        report[5..7].copy_from_slice(&triggers);
        report[8..11].copy_from_slice(&buttons);
        report
    }

    fn bluetooth(sticks: [u8; 4], triggers: [u8; 2], buttons: [u8; 3]) -> [u8; 78] {
        let mut report = [0u8; 78];
        report[0] = 0x31;
        report[2..6].copy_from_slice(&sticks);
        report[6..8].copy_from_slice(&triggers);
        report[9..12].copy_from_slice(&buttons);
        report
    }

    const REST: [u8; 4] = [128; 4];
    const CENTRED: u8 = 8;

    #[test]
    fn every_button_reads_on_both_transports() {
        for &(byte, mask, button) in &BUTTONS {
            let mut held = [CENTRED, 0, 0];
            held[byte] |= mask;
            let over_usb = parse(&usb(REST, [0; 2], held), false).unwrap();
            let over_bluetooth = parse(&bluetooth(REST, [0; 2], held), true).unwrap();
            assert_eq!(over_usb.buttons, button, "byte {byte} mask {mask:#x}");
            assert_eq!(over_bluetooth, over_usb);
        }
        // all at once, apart from the shared one
        let all = [0xF0 | CENTRED, 0xF3, 0x02];
        let pad = parse(&usb(REST, [0; 2], all), false).unwrap();
        assert_eq!(
            pad.buttons,
            X | A
                | B
                | Y
                | LEFT_SHOULDER
                | RIGHT_SHOULDER
                | VIEW
                | MENU
                | LEFT_STICK
                | RIGHT_STICK
        );
        // bits that mean nothing here
        let pad = parse(&usb(REST, [0; 2], [CENTRED, 0x0C, 0xFD]), false).unwrap();
        assert_eq!(pad.buttons, 0);
    }

    #[test]
    fn the_hat_reads_all_eight_directions_and_rest() {
        let expected = [
            UP,
            UP | RIGHT,
            RIGHT,
            RIGHT | DOWN,
            DOWN,
            DOWN | LEFT,
            LEFT,
            LEFT | UP,
        ];
        for (value, buttons) in (0u8..).zip(expected) {
            let report = usb(REST, [0; 2], [value, 0, 0]);
            assert_eq!(parse(&report, false).unwrap().buttons, buttons);
        }
        for value in 8..16 {
            let report = usb(REST, [0; 2], [value, 0, 0]);
            assert_eq!(parse(&report, false).unwrap().buttons, 0);
        }
        // the face buttons in the high nibble do not disturb the hat
        let report = usb(REST, [0; 2], [0x20 | 2, 0, 0]);
        assert_eq!(parse(&report, false).unwrap().buttons, A | RIGHT);
    }

    #[test]
    fn sticks_read_raw_with_exact_rest_and_ends() {
        // rest: no dead zone is applied, so exactly zero
        let pad = parse(&usb(REST, [0; 2], [CENTRED, 0, 0]), false).unwrap();
        assert_eq!(
            (pad.left_x, pad.left_y, pad.right_x, pad.right_y),
            (0.0, 0.0, 0.0, 0.0)
        );
        // left and up are the low bytes; up is positive for us
        let pad = parse(&usb([0, 0, 255, 255], [0; 2], [CENTRED, 0, 0]), false).unwrap();
        assert_eq!(
            (pad.left_x, pad.left_y, pad.right_x, pad.right_y),
            (-1.0, 1.0, 1.0, -1.0)
        );
        let pad = parse(&usb([255, 255, 0, 0], [0; 2], [CENTRED, 0, 0]), false).unwrap();
        assert_eq!(
            (pad.left_x, pad.left_y, pad.right_x, pad.right_y),
            (1.0, -1.0, -1.0, 1.0)
        );
        // every byte stays within range and rises monotonically
        let mut last = -2.0;
        for value in 0..=255u8 {
            let v = axis(value);
            assert!((-1.0..=1.0).contains(&v) && v > last);
            last = v;
        }
        // the Bluetooth report carries the same values one byte further in
        let pad = parse(&bluetooth([0, 255, 128, 64], [0; 2], [CENTRED, 0, 0]), true).unwrap();
        assert_eq!(
            (pad.left_x, pad.left_y, pad.right_x, pad.right_y),
            (-1.0, -1.0, 0.0, 0.5)
        );
    }

    #[test]
    fn triggers_read_zero_to_one() {
        let b = [CENTRED, 0, 0];
        let pad = parse(&usb(REST, [0, 255], b), false).unwrap();
        assert_eq!((pad.left_trigger, pad.right_trigger), (0.0, 1.0));
        let pad = parse(&usb(REST, [255, 0], b), false).unwrap();
        assert_eq!((pad.left_trigger, pad.right_trigger), (1.0, 0.0));
        let pad = parse(&bluetooth(REST, [51, 255], b), true).unwrap();
        assert_eq!((pad.left_trigger, pad.right_trigger), (0.2, 1.0));
    }

    #[test]
    fn the_short_bluetooth_report_reads_too() {
        let mut report = [0u8; 78];
        report[0] = 0x01;
        report[1..5].copy_from_slice(&[0, 255, 128, 128]);
        report[5] = 0x40 | 4; // east face button, hat down
        report[6] = 0x20; // small right button
        report[7] = 0x02; // touch surface press
        report[8] = 255; // left trigger
        report[9] = 0; // right trigger
        let pad = parse(&report, true).unwrap();
        assert_eq!(pad.buttons, B | DOWN | MENU | VIEW);
        assert_eq!(
            (pad.left_x, pad.left_y, pad.left_trigger, pad.right_trigger),
            (-1.0, -1.0, 1.0, 0.0)
        );
    }

    #[test]
    fn other_or_short_reports_are_not_pads() {
        let report = bluetooth(REST, [0; 2], [CENTRED, 0, 0]);
        // a Bluetooth report on a USB device, and the reverse
        assert_eq!(parse(&report, false), None);
        assert_eq!(parse(&[], false), None);
        assert_eq!(parse(&[0x01; 10], false), None);
        assert_eq!(parse(&[0x31; 11], true), None);
        assert_eq!(parse(&[0x02; 64], false), None);
        assert_eq!(parse(&[0x01; 7], true), None);
    }

    #[test]
    fn the_pairing_report_holds_the_address() {
        let mut pairing = [0u8; 20];
        pairing[0] = 0x09;
        pairing[1..7].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(address(&pairing), Some([1, 2, 3, 4, 5, 6]));
        assert_eq!(address(&[0x09; 1]), None);
        assert_eq!(address(&[0x09, 0, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn xinput_comes_first_and_off_means_xinput_only() {
        let hid_pad = pressing(A);
        let xinput_pad = pressing(Y);
        let none = [None; PADS];
        let mut assigned = [None; PADS];

        // XInput answering anywhere: HID is not even asked
        let xinput = [None, None, Some(xinput_pad), None];
        let got = slots(true, xinput, &mut assigned, || panic!("HID not needed"));
        assert_eq!(got, xinput);
        // switched off: the same, with nothing to fill
        assert_eq!(slots(false, none, &mut assigned, || panic!("off")), none);
        // nothing from XInput: the HID pad takes the first slot
        let got = slots(true, none, &mut assigned, || vec![(1, hid_pad)]);
        assert_eq!(got, [Some(hid_pad), None, None, None]);
    }

    #[test]
    fn a_hid_pad_keeps_its_slot_through_another_pads_disconnect() {
        let (a, b, c) = (pressing(A), pressing(B), pressing(X));
        let none = [None; PADS];
        let mut assigned = [None; PADS];
        let got = slots(true, none, &mut assigned, || vec![(7, a), (8, b)]);
        assert_eq!(got, [Some(a), Some(b), None, None]);
        // the first goes away: the second stays in slot 1
        let got = slots(true, none, &mut assigned, || vec![(8, b)]);
        assert_eq!(got, [None, Some(b), None, None]);
        // a new connection takes the free slot, the old one stays
        let got = slots(true, none, &mut assigned, || vec![(8, b), (9, c)]);
        assert_eq!(got, [Some(c), Some(b), None, None]);
        // more pads than slots: the extra one waits
        let many = vec![(8, b), (9, c), (10, a), (11, a), (12, a)];
        let got = slots(true, none, &mut assigned, || many);
        assert_eq!(got.iter().flatten().count(), PADS);
        // XInput appearing releases every HID slot
        let xinput = [Some(a), None, None, None];
        slots(true, xinput, &mut assigned, || panic!("not needed"));
        assert_eq!(assigned, [None; PADS]);
    }
}
