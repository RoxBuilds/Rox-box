//! The open demo title: an executable of our own that does what a small
//! console title does, so that `victory run <demo-title.exe> --execute` is a
//! whole run (loader, import binding, the title's thread, the runtime, the
//! graphics layer, the run report) that anyone may perform and CI performs
//! on every change (docs/demo-title.md).
//!
//! Like a title, it imports the console's graphics entry points from
//! `d3d12_xs.dll`, which no PC has, so only Rox-box can run it; it takes its
//! arguments from its C runtime (`__p___argc`, `__p___argv`), which Rox-box
//! gives the ones after `--`; it sets the graphics address range up
//! (`D3D12ConfigureVirtualMemoryX`) before it makes its device, paces its
//! frames with the device's frame interval and frame-origin waits, presents
//! each frame with the token its wait handed out, and ends by returning
//! from `main` (its C runtime then calls `exit`).
//!
//! Each frame is the self-test's scene in one list (`victory_standin`, the
//! code `victory selftest` drives too): a 1 MiB buffer the title changes in
//! three places, copied into another; a translated compute dispatch storing
//! into title memory, then a piece copied into the same buffer; the tiled
//! texture, partly rewritten by the title, copied into the render target;
//! part of the target cleared; two translated vertex and pixel draws, each
//! cut to a quarter by its scissor; then the present. One submission, one
//! dispatch and two draws a frame. After each frame it checks what a title
//! can check by its own means, the values that came back into its own
//! memory, and stops at the first mismatch with one line saying what and
//! where (exit code 1).
//!
//! `--frames <n>`: frames to render (default 300, five seconds at the 60 a
//! second it asks for).

use std::ffi::{CStr, c_char, c_void};
use std::process::ExitCode;

use victory_standin::pc::{
    COMMON, COPY_DEST, HRESULT, Iid, Obj, RENDER_TARGET, Rect, VirtualMemory,
};
use victory_standin::scene::{BufferCopy, Compute, DrawCall, Drawing, TiledTexture, step};
use victory_standin::{Failed, Imports, SIDE, Title, check};

#[link(name = "d3d12_xs", kind = "raw-dylib")]
unsafe extern "system" {
    fn D3D12ConfigureVirtualMemoryX(config: *mut VirtualMemory) -> HRESULT;
    fn D3D12XboxCreateDevice(
        adapter: *const c_void,
        parameters: *const u32,
        iid: *const Iid,
        out: *mut Obj,
    ) -> HRESULT;
    fn D3D12SerializeRootSignature(
        desc: *const c_void,
        version: u32,
        blob: *mut *mut c_void,
        error: *mut *mut c_void,
    ) -> HRESULT;
}

// The C runtime's argument list (`api-ms-win-crt-runtime`): where a
// title's arguments are, run by Rox-box or not. (Rust's own `std::env::args`
// reads the process's command line, which under Rox-box is Rox-box's.)
unsafe extern "C" {
    fn __p___argc() -> *mut i32;
    fn __p___argv() -> *mut *mut *mut c_char;
}

extern "system" fn create_device(
    adapter: *const c_void,
    parameters: *const u32,
    iid: *const Iid,
    out: *mut Obj,
) -> HRESULT {
    // SAFETY: the import, with the arguments the stand-in passes it.
    unsafe { D3D12XboxCreateDevice(adapter, parameters, iid, out) }
}

extern "system" fn serialize_root(
    desc: *const c_void,
    version: u32,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: as above.
    unsafe { D3D12SerializeRootSignature(desc, version, blob, error) }
}

const FRAMES: u32 = 300;
/// 60 frames a second.
const INTERVAL_US: u32 = 16_667;

fn main() -> ExitCode {
    let frames = match frames_asked() {
        Ok(frames) => frames,
        Err(problem) => {
            println!("demo title: {problem} (it takes --frames <n>, 1 or more)");
            return ExitCode::from(2);
        }
    };
    match run(frames) {
        Ok(()) => {
            println!(
                "demo title: {frames} frames rendered and presented; every value it read back from its own memory matched"
            );
            ExitCode::SUCCESS
        }
        Err(Failed(reason)) => {
            println!("demo title failed: {reason}");
            ExitCode::FAILURE
        }
    }
}

/// The arguments after the program's name, from the C runtime.
fn title_args() -> Vec<String> {
    // SAFETY: the C runtime's own list, made before `main` and never
    // changed: `argc` NUL-terminated strings.
    unsafe {
        let (argc, argv) = (*__p___argc(), *__p___argv());
        if argv.is_null() {
            return Vec::new();
        }
        (1..argc.max(0) as usize)
            .map(|i| *argv.add(i))
            .filter(|a| !a.is_null())
            .map(|a| CStr::from_ptr(a).to_string_lossy().into_owned())
            .collect()
    }
}

fn frames_asked() -> Result<u32, String> {
    let args = title_args();
    let mut args = args.iter();
    let mut frames = FRAMES;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--frames" => {
                frames = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .filter(|&n| n > 0)
                    .ok_or("--frames needs a number")?;
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(frames)
}

/// The quarters of the target: the two draws take the top ones, the clear
/// the bottom left, and the texture shows in the bottom right.
const HALF: i32 = SIDE as i32 / 2;
const TOP_LEFT: Rect = Rect {
    left: 0,
    top: 0,
    right: HALF,
    bottom: HALF,
};
const TOP_RIGHT: Rect = Rect {
    left: HALF,
    top: 0,
    right: 2 * HALF,
    bottom: HALF,
};
const BOTTOM_LEFT: Rect = Rect {
    left: 0,
    top: HALF,
    right: HALF,
    bottom: 2 * HALF,
};

fn run(frames: u32) -> Result<(), Failed> {
    let mut range = VirtualMemory {
        flags: 0,
        page_table_pages: 0,
        preserved_bytes: 0,
        lowest: 0,
        highest: 0,
    };
    // SAFETY: the import, given a structure of its layout.
    check("D3D12ConfigureVirtualMemoryX", unsafe {
        D3D12ConfigureVirtualMemoryX(&mut range)
    })?;
    if range.lowest >= range.highest {
        return Err(format!(
            "D3D12ConfigureVirtualMemoryX gave no address range ({:#x} to {:#x})",
            range.lowest, range.highest
        )
        .into());
    }
    let imports = Imports {
        create_device,
        serialize_root,
    };
    let device = Title::create_device(&imports)?;
    let title = Title::on(device, imports, || {})?;
    title.pace(INTERVAL_US)?;
    // Loading: everything the frames use is made before the first.
    let mut buffers = BufferCopy::new(&title, 1 << 20)?;
    let mut texture = TiledTexture::new(&title)?;
    let mut compute = Compute::new(&title)?;
    let drawing = Drawing::new(&title)?;
    for f in 0..frames {
        let token = title.next_frame(1_000)?;
        // What the title writes into its memory this frame.
        if f > 0 {
            buffers.change(f);
            texture.rewrite_block([(f * 7 % 256) as u8, 128, (255 - f * 3 % 256) as u8, 255])?;
        }
        compute.write_piece(f);
        let constants = [
            0x1234_5678 ^ f.wrapping_mul(0x9e37_79b9),
            7 + f % 1000,
            0,
            0,
        ];
        let draws = [
            DrawCall {
                vs: [step(f * 5 % 256), step(200), step(90), step(17)],
                ps: step(230),
                scissor: TOP_RIGHT,
            },
            DrawCall {
                vs: [step(250), step(f * 3 % 256), step(128), step(0)],
                ps: step(255),
                scissor: TOP_LEFT,
            },
        ];
        let clear = [0.1, (f % 60) as f32 / 60.0, 0.4, 1.0];
        title.frame(|t| {
            buffers.record(t);
            compute.record_dispatch(t, constants);
            compute.record_copy_in(t);
            t.transition(t.target, COMMON, COPY_DEST);
            texture.record_copy(t);
            t.transition(t.target, COPY_DEST, RENDER_TARGET);
            t.clear_rects(clear, &[BOTTOM_LEFT]);
            drawing.record(t, &draws);
            t.transition(t.target, RENDER_TARGET, COMMON);
            Ok(())
        })?;
        title.present_with(token)?;
        title.sync()?;
        // What came back into the title's own memory.
        let frame = f + 1;
        if let Some(at) = buffers.differs() {
            return Err(format!(
                "frame {frame}: the buffer copy differs from what the title wrote at byte {at}"
            )
            .into());
        }
        compute.check_stores(constants, &format!("frame {frame}: the dispatch"))?;
        compute
            .check_copy_in()
            .map_err(|Failed(e)| format!("frame {frame}: {e}"))?;
    }
    Ok(())
}
