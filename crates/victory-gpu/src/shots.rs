//! A picture of the run every few seconds, in any build:
//! `VICTORY_CAPTURE=<folder>` with `VICTORY_CAPTURE_EVERY_S=<seconds>` saves
//! the presented frame at half size as `shot-<seconds>s.png`, the seconds
//! counted from the first presented frame, at most [`MAX_SHOTS`] per run. Each
//! run writes into a subfolder of its own, named by its start time in UTC
//! digits (`20261231235959`), so pictures of earlier runs in the folder are
//! left alone and names never collide.
//!
//! The pictures are the user's own data: they go only to the folder the user
//! names, which is refused when it lies inside a copy of the source tree (as
//! the other folders written to are), and nothing about them is put in the
//! run report; the log gives each file's path and nothing else. Nothing is
//! done at all unless both variables are set; then a present that is due
//! reads the frame back the way `capture.rs` does (the frame is not sent
//! ahead, so it waits for the card once, then and not before), and the
//! half-size picture is encoded and written by a thread of its own. Any
//! failure is logged once and ends the pictures for the run: it never fails
//! the run. The other capture switches (`capture.rs`) are for builds with
//! diagnostics only.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// The most pictures one run saves.
pub(crate) const MAX_SHOTS: u32 = 120;

/// When the next picture is due, and how many were taken.
#[derive(Debug)]
struct Plan {
    every: u64,
    /// The elapsed second from which the next picture is due.
    next: u64,
    taken: u32,
    stopped: bool,
}

impl Plan {
    fn new(every: u64) -> Plan {
        Plan { every, next: every, taken: 0, stopped: false }
    }

    /// With `elapsed` whole seconds since the run began: the picture's
    /// number in seconds when one is due now, which counts it as taken.
    /// The next one is due at the next multiple of the interval, so a slow
    /// frame does not bunch pictures up and no two share a second.
    fn due(&mut self, elapsed: u64) -> Option<u64> {
        if self.stopped || self.taken >= MAX_SHOTS || elapsed < self.next {
            return None;
        }
        self.taken += 1;
        self.next = (elapsed / self.every + 1) * self.every;
        Some(elapsed)
    }
}

/// `shot-0120s.png`: seconds, at least four digits.
fn shot_name(seconds: u64) -> String {
    format!("shot-{seconds:04}s.png")
}

/// The interval from `VICTORY_CAPTURE_EVERY_S`: whole seconds, one or more.
fn parse_every(value: &str) -> Option<u64> {
    value.trim().parse().ok().filter(|&s| s > 0)
}

/// `unix` seconds as UTC `YYYYMMDDHHMMSS`.
fn utc_digits(unix: u64) -> String {
    let (days, rest) = (unix / 86_400, unix % 86_400);
    // Days since 1970-01-01 as a civil date (the proleptic Gregorian
    // calendar, counted from a March-based year).
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// The run's own subfolder of `base`: named by its start, with `-2`, `-3`
/// and so on after it when another run of the same second has it already.
fn run_folder(base: &Path, start_unix: u64, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let stamp = utc_digits(start_unix);
    let first = base.join(&stamp);
    if !exists(&first) {
        return first;
    }
    (2..)
        .map(|n| base.join(format!("{stamp}-{n}")))
        .find(|p| !exists(p))
        .unwrap_or(first)
}

/// What the two variables ask for: `None` when the pictures are not asked
/// for, the run's folder and the interval otherwise, or why they are refused.
fn request(
    folder: Option<&std::ffi::OsStr>,
    every: Option<&str>,
    start_unix: u64,
    outside_repo: impl Fn(&Path) -> Result<(), String>,
    exists: impl Fn(&Path) -> bool,
) -> Result<Option<(PathBuf, u64)>, String> {
    let Some(every) = every else { return Ok(None) };
    let every = parse_every(every)
        .ok_or_else(|| format!("VICTORY_CAPTURE_EVERY_S={every:?} ignored: expected whole seconds, 1 or more"))?;
    let folder = folder
        .filter(|f| !f.is_empty())
        .map(Path::new)
        .ok_or("VICTORY_CAPTURE_EVERY_S ignored: it needs VICTORY_CAPTURE=<folder>")?;
    outside_repo(folder).map_err(|why| format!("VICTORY_CAPTURE_EVERY_S ignored: {why}"))?;
    Ok(Some((run_folder(folder, start_unix, exists), every)))
}

struct Shots {
    folder: PathBuf,
    started: Instant,
    plan: Mutex<Plan>,
}

/// The pictures asked for, read once; `None` when they are not (the only cost
/// of the feature then is this lookup).
fn shots() -> Option<&'static Shots> {
    static SHOTS: OnceLock<Option<Shots>> = OnceLock::new();
    SHOTS
        .get_or_init(|| {
            // With `VICTORY_CAPTURE=<folder>`: a half-size picture of the
            // presented frame every this many seconds (whole seconds, 1 or
            // more), at most 120 per run, into a subfolder of the folder named
            // by the run's start in UTC digits (any build; unset: nothing is
            // done). A folder inside the source tree is refused.
            let every = std::env::var("VICTORY_CAPTURE_EVERY_S").ok();
            let folder = std::env::var_os("VICTORY_CAPTURE");
            let start = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let asked = request(
                folder.as_deref(),
                every.as_deref(),
                start,
                |dir| victory_pipeline::ensure_outside_repo(dir).map_err(|e| e.to_string()),
                Path::exists,
            );
            match asked {
                Ok(None) => None,
                Ok(Some((folder, every))) => {
                    tracing::info!(every, folder = %folder.display(), max = MAX_SHOTS, "a picture of the run is saved at this interval, in seconds");
                    Some(Shots { folder, started: Instant::now(), plan: Mutex::new(Plan::new(every)) })
                }
                Err(why) => {
                    tracing::warn!("{why}");
                    None
                }
            }
        })
        .as_ref()
}

/// At a present: the number of seconds to name the picture by when one is
/// due now. Cheap: a lookup, and with the pictures asked for a lock.
pub(crate) fn due() -> Option<u64> {
    let shots = shots()?;
    let elapsed = shots.started.elapsed().as_secs();
    shots.plan.lock().unwrap_or_else(|e| e.into_inner()).due(elapsed)
}

/// Logs why the pictures end, once, and ends them.
fn fail(why: &str) {
    let Some(shots) = shots() else { return };
    let mut plan = shots.plan.lock().unwrap_or_else(|e| e.into_inner());
    if !plan.stopped {
        plan.stopped = true;
        tracing::warn!(why, "no more pictures of the run are saved");
    }
}

/// [`read_back`] for a picture that is due; a failure ends the pictures.
pub(crate) fn take(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    let read = read_back(device, list, texture);
    if read.is_none() {
        fail("the frame could not be read back");
    }
    read
}

/// The picture read back by [`take`] (finished: every submission is waited
/// for), made half size here and written by a thread of its own as the
/// picture named `seconds`.
pub(crate) fn save(seconds: u64, buffer: &ID3D12Resource, footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT) {
    let Some(shots) = shots() else { return };
    let (w, h, raw) = match half_of_read_back(buffer, footprint) {
        Ok(picture) => picture,
        Err(why) => return fail(&why),
    };
    let path = shots.folder.join(shot_name(seconds));
    let started = std::thread::Builder::new().name("shot writer".into()).spawn(move || match write(&path, w, h, &raw) {
        Ok(()) => tracing::info!(path = %path.display(), "picture of the run saved"),
        Err(e) => fail(&format!("a picture could not be written: {e}")),
    });
    if let Err(e) = started {
        fail(&format!("no thread to write with: {e}"));
    }
}

/// The half-size RGB rows (behind their filter bytes) of a read-back frame.
fn half_of_read_back(
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) -> Result<(u32, u32, Vec<u8>), String> {
    let f = footprint.Footprint;
    let bgra = match f.Format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => false,
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => true,
        other => return Err(format!("the frame is not 8-bit color (format {})", other.0)),
    };
    if f.Width == 0 || f.Height == 0 {
        return Err("the frame is empty".into());
    }
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished (every submission is waited for); the mapping
    // spans the footprint's rows and is released before returning.
    unsafe {
        buffer.Map(0, None, Some(&mut data)).map_err(|e| format!("the read-back could not be mapped: {e}"))?;
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), f.RowPitch as usize * f.Height as usize);
        let picture = half_rgb(f.Width, f.Height, f.RowPitch as usize, bgra, rows);
        buffer.Unmap(0, None);
        Ok(picture)
    }
}

/// Writes the picture, making the run's folder first.
fn write(path: &Path, w: u32, h: u32, raw: &[u8]) -> std::io::Result<()> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder)?;
    }
    std::fs::write(path, encode(w, h, raw))
}

/// 8-bit RGBA or BGRA rows (`pitch` bytes apart) at half size: the width,
/// the height and RGB rows each behind filter byte 0, as [`encode`] takes
/// them (every other pixel of every other row).
pub(crate) fn half_rgb(width: u32, height: u32, pitch: usize, bgra: bool, rows: &[u8]) -> (u32, u32, Vec<u8>) {
    let (w, h) = ((width / 2).max(1), (height / 2).max(1));
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        let row = &rows[(2 * y).min(height as usize - 1) * pitch..];
        for x in 0..w as usize {
            let p = &row[(2 * x).min(width as usize - 1) * 4..][..3];
            if bgra {
                raw.extend([p[2], p[1], p[0]]);
            } else {
                raw.extend_from_slice(p);
            }
        }
    }
    (w, h, raw)
}

/// A PNG of rows of RGB, each behind its filter byte.
pub(crate) fn encode(w: u32, h: u32, raw: &[u8]) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend(w.to_be_bytes());
    header.extend(h.to_be_bytes());
    header.extend([8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut png, b"IHDR", &header);
    let mut compressed = vec![0u8; zlib_rs::compress_bound(raw.len())];
    let (idat, rc) = zlib_rs::compress_slice(&mut compressed, raw, zlib_rs::DeflateConfig::new(3));
    debug_assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    chunk(&mut png, b"IDAT", idat);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend((data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend(kind);
    png.extend(data);
    let crc = crc32(&png[start..]);
    png.extend(crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// Records a copy of `texture` (in the copy-source state) into a
/// readback buffer.
pub(crate) fn read_back(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    read_back_of(device, list, texture, 0)
}

/// As [`read_back`], of subresource `sub` (the stencil plane of a
/// one-level, one-slice depth-stencil texture is subresource 1).
pub(crate) fn read_back_of(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    sub: u32,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0;
    // SAFETY: a live texture; outputs for one subresource.
    unsafe {
        let desc = texture.GetDesc();
        device.GetCopyableFootprints(
            &desc,
            sub,
            1,
            0,
            Some(&mut footprint),
            None,
            None,
            Some(&mut total),
        );
    }
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: total,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; an open list; locations naming live resources.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
        let buffer = buffer?;
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: sub,
            },
        };
        list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        Some((buffer, footprint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_png_check_value() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
    }

    #[test]
    fn pictures_are_named_by_their_seconds() {
        assert_eq!(shot_name(0), "shot-0000s.png");
        assert_eq!(shot_name(120), "shot-0120s.png");
        assert_eq!(shot_name(12_345), "shot-12345s.png");
    }

    #[test]
    fn the_interval_is_whole_seconds_of_one_or_more() {
        assert_eq!(parse_every("5"), Some(5));
        assert_eq!(parse_every(" 30 "), Some(30));
        for bad in ["", "0", "-1", "1.5", "x", "5s", "5,5"] {
            assert_eq!(parse_every(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn utc_digits_are_the_calendar_date_and_time() {
        assert_eq!(utc_digits(0), "19700101000000");
        assert_eq!(utc_digits(951_782_400), "20000229000000"); // a leap day
        assert_eq!(utc_digits(1_000_000_000), "20010909014640");
        assert_eq!(utc_digits(1_700_000_000), "20231114221320");
        assert_eq!(utc_digits(4_102_444_799), "20991231235959");
    }

    #[test]
    fn a_run_gets_a_folder_of_its_own_that_no_earlier_run_has() {
        let base = Path::new("shots");
        let stamp = utc_digits(1_700_000_000);
        assert_eq!(run_folder(base, 1_700_000_000, |_| false), base.join(&stamp));
        let taken = [base.join(&stamp), base.join(format!("{stamp}-2"))];
        let got = run_folder(base, 1_700_000_000, |p| taken.iter().any(|t| t == p));
        assert_eq!(got, base.join(format!("{stamp}-3")));
    }

    #[test]
    fn pictures_are_due_on_the_interval_up_to_the_cap() {
        let mut plan = Plan::new(5);
        assert_eq!((0..5).filter_map(|s| plan.due(s)).count(), 0);
        assert_eq!(plan.due(5), Some(5));
        assert_eq!(plan.due(5), None, "not twice in a second");
        assert_eq!(plan.due(9), None);
        // a late frame takes one picture, and the next stays on the grid
        assert_eq!(plan.due(17), Some(17));
        assert_eq!(plan.due(19), None);
        assert_eq!(plan.due(20), Some(20));
        // the cap
        let mut plan = Plan::new(1);
        let taken = (0..10_000).filter_map(|s| plan.due(s)).count();
        assert_eq!(taken, MAX_SHOTS as usize);
        assert_eq!(plan.due(20_000), None);
        // a stopped plan takes none
        let mut plan = Plan::new(1);
        plan.stopped = true;
        assert_eq!(plan.due(100), None);
    }

    #[test]
    fn the_request_needs_both_variables_and_a_folder_outside_the_source_tree() {
        let ok = |_: &Path| Ok(());
        let none = |_: &Path| false;
        let folder = std::ffi::OsString::from("out");
        // not asked for
        assert_eq!(request(Some(&folder), None, 0, ok, none), Ok(None));
        assert_eq!(request(None, None, 0, ok, none), Ok(None));
        // asked for
        let asked = request(Some(&folder), Some("10"), 0, ok, none).unwrap();
        assert_eq!(asked, Some((Path::new("out").join("19700101000000"), 10)));
        // refused
        assert!(request(Some(&folder), Some("0"), 0, ok, none).is_err());
        assert!(request(None, Some("10"), 0, ok, none).is_err());
        assert!(request(Some(std::ffi::OsStr::new("")), Some("10"), 0, ok, none).is_err());
        let inside = |_: &Path| Err("inside the source tree".to_string());
        assert!(request(Some(&folder), Some("10"), 0, inside, none).unwrap_err().contains("inside the source tree"));
    }

    /// The real check refuses a folder in this source tree, and takes one
    /// that is in none.
    #[test]
    fn the_real_check_refuses_a_folder_inside_the_source_tree() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("shots");
        assert!(victory_pipeline::ensure_outside_repo(&inside).is_err());
        let outside = tempfile::tempdir().unwrap();
        assert!(victory_pipeline::ensure_outside_repo(&outside.path().join("shots")).is_ok());
    }

    #[test]
    fn half_size_rows_take_every_other_pixel_in_rgb_order() {
        // 4 x 2 pixels: pixel (x, y) = [x, y, 9, 255]
        let pitch = 20; // padded rows
        let mut rows = vec![0u8; pitch * 2];
        for y in 0..2 {
            for x in 0..4 {
                rows[y * pitch + x * 4..][..4].copy_from_slice(&[x as u8, y as u8, 9, 255]);
            }
        }
        let (w, h, raw) = half_rgb(4, 2, pitch, true, &rows);
        assert_eq!((w, h), (2, 1));
        // filter byte, then (r, g, b) of x = 0 and x = 2, blue first in the source
        assert_eq!(raw, [0, 9, 0, 0, 9, 0, 2]);
        let (_, _, rgba) = half_rgb(4, 2, pitch, false, &rows);
        assert_eq!(rgba, [0, 0, 0, 9, 2, 0, 9]);
        // one pixel is one pixel
        let (w, h, raw) = half_rgb(1, 1, 4, false, &[1, 2, 3, 255]);
        assert_eq!((w, h, raw), (1, 1, vec![0, 1, 2, 3]));
    }

    /// One picture through the software adapter: texels copied into a
    /// texture, read back by [`read_back`] as the present does, made half
    /// size and written as a PNG named by its seconds.
    #[test]
    fn warp_one_picture_is_read_back_and_written_as_a_half_size_png() {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
        use windows::core::Interface;

        let _turn = crate::indirect::tests::warp_turn();
        let Ok(adapter) = crate::host::warp_adapter() else {
            eprintln!("no software adapter: the test is not run");
            return;
        };
        let (width, height) = (8u32, 4u32);
        // SAFETY: plain object creation on a device of our own; the card is
        // waited for before anything is read or released.
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
                eprintln!("no software device: the test is not run");
                return;
            }
            let device = device.unwrap();
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
            let committed = |kind, desc: &D3D12_RESOURCE_DESC, state| {
                let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
                let mut made: Option<ID3D12Resource> = None;
                device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, desc, state, None, &mut made).unwrap();
                made.unwrap()
            };
            // The texture, and an upload buffer with its rows 256 bytes apart.
            let texture_desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(width),
                Height: height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                ..Default::default()
            };
            let upload_desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 256 * u64::from(height),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let texture = committed(D3D12_HEAP_TYPE_DEFAULT, &texture_desc, D3D12_RESOURCE_STATE_COPY_DEST);
            let upload = committed(D3D12_HEAP_TYPE_UPLOAD, &upload_desc, D3D12_RESOURCE_STATE_GENERIC_READ);
            let mut data = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut data)).unwrap();
            let bytes = std::slice::from_raw_parts_mut(data.cast::<u8>(), 256 * height as usize);
            for y in 0..height as usize {
                for x in 0..width as usize {
                    bytes[y * 256 + x * 4..][..4].copy_from_slice(&[x as u8 * 10, y as u8 * 10, 77, 255]);
                }
            }
            upload.Unmap(0, None);
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&upload),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            Width: width,
                            Height: height,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
            list.ResourceBarrier(&[crate::host::transition(
                &texture,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            let (buffer, footprint) = read_back(&device, &list, &texture).expect("a read-back buffer");
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let event: HANDLE = CreateEventW(None, false, false, None).unwrap();
            queue.Signal(&fence, 1).unwrap();
            fence.SetEventOnCompletion(1, event).unwrap();
            WaitForSingleObject(event, INFINITE);

            let (w, h, raw) = half_of_read_back(&buffer, &footprint).unwrap();
            assert_eq!((w, h), (4, 2));
            // the first row's first two pixels: source texels (0, 0) and (2, 0)
            assert_eq!(&raw[1..7], &[0, 0, 77, 20, 0, 77]);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("run").join(shot_name(120));
            write(&path, w, h, &raw).unwrap();
            assert!(path.ends_with("shot-0120s.png"));
            let png = std::fs::read(&path).unwrap();
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            let size = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().unwrap());
            assert_eq!((size(16), size(20)), (4, 2));
        }
    }
}
