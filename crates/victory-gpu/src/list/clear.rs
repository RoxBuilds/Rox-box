//! `ClearUnorderedAccessViewUint` and `ClearUnorderedAccessViewFloat` (list slots 53 and 54, 0x1a8 and 0x1b0).
//!
//! Upscaler middleware clears its history and recurrent resources with them,
//! passing `(list, GPU handle, CPU handle, resource, values, 0, null)` where
//! `values` points to four words. When unimplemented, history buffers stay
//! uninitialized or retain stale frames across camera cuts.
//!
//! The host wants both a handle in the shader-visible heap that is set and a handle in a heap the card does not
//! see, for the same view. The title's host heaps are not ours to write into, so the view is made here: in a
//! small heap of ours that is not shader-visible and copied into a slot of a shader-visible heap of ours, which
//! the clear then sets. Setting it displaces the heaps the title's draws set, so the draw state is told to set
//! them again ([`crate::draw::State::replay`]).
//!
//! shortcut: views of buffers and of memory the title placed itself are not cleared, nor those of depth textures
//! (their unordered access is their colour twin's); each is said once and counted as skipped.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

use super::{Rect, This};
use crate::descriptor::View;

/// Whether the clears are made (`VICTORY_CLEAR_UAV`, on unless `0`; off: they do nothing, as before).
pub(crate) fn wanted() -> bool {
    crate::toggled!("VICTORY_CLEAR_UAV")
}

/// Which clear.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Uint = 0,
    Float = 1,
}

impl Kind {
    pub(crate) fn of(word: u64) -> Kind {
        if word & 0xff == 1 { Kind::Float } else { Kind::Uint }
    }
}

/// Since the last line of the log: clears made by kind, and clears skipped.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// When the last line was logged, and the frame count then.
static LAST: Mutex<Option<(std::time::Instant, u64)>> = Mutex::new(None);

/// Counts one clear (`slot`: 0 uint, 1 float, 2 skipped) and logs the line once a second.
fn count(slot: usize) {
    COUNTS[slot].fetch_add(1, Relaxed);
    let Ok(mut last) = LAST.try_lock() else { return };
    let now = std::time::Instant::now();
    let frame = crate::frame::current();
    let Some((since, from)) = *last else {
        *last = Some((now, frame));
        return;
    };
    if now.duration_since(since) < std::time::Duration::from_secs(1) {
        return;
    }
    *last = Some((now, frame));
    drop(last);
    let frames = frame.saturating_sub(from).max(1) as f64;
    let [uint, float, skipped] = [0, 1, 2].map(|i| COUNTS[i].swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "unordered-access clears per frame: {uint:.2} uint, {float:.2} float, {skipped:.2} skipped (VICTORY_CLEAR_UAV)"
    );
}

/// A clear that cannot be made: counted, and said once for each `reason`.
pub(crate) fn skipped(reason: &'static str) {
    static SAID: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    {
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if !said.contains(&reason) {
            said.push(reason);
            tracing::warn!(reason, "an unordered-access clear is skipped (VICTORY_CLEAR_UAV)");
        }
    }
    count(2);
}

/// Descriptor slots the clears take in turn. The card reads a slot when the clear runs, not when it is recorded:
/// a slot is not written again until this many clears later, far more than the lists in the air hold (with three
/// frames in the air, some 2,700 clears a frame; an upscaler makes a few). 256 KiB a heap.
const SLOTS: u32 = 8192;

/// Our two heaps for the clears' views: one the card does not see, one it does, with the same slots.
pub(crate) struct Slots {
    hidden: ID3D12DescriptorHeap,
    visible: ID3D12DescriptorHeap,
    step: usize,
    next: AtomicU32,
}

// SAFETY: descriptor heaps are free-threaded; the slot taken is the one atomic.
unsafe impl Send for Slots {}
// SAFETY: as above.
unsafe impl Sync for Slots {}

impl Slots {
    pub(crate) fn new(device: &ID3D12Device) -> Option<Slots> {
        let made = |flags| {
            // SAFETY: plain heap creation.
            unsafe {
                device.CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: SLOTS,
                    Flags: flags,
                    NodeMask: 0,
                })
            }
            .ok()
        };
        let hidden = made(D3D12_DESCRIPTOR_HEAP_FLAG_NONE)?;
        let visible = made(D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE)?;
        // SAFETY: a plain query.
        let step = unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) } as usize;
        Some(Slots { hidden, visible, step, next: AtomicU32::new(0) })
    }

    /// The view `desc` of `texture` in the next slot of both heaps: its handles in the heap that is set and in the
    /// other.
    fn make(
        &self,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        desc: Option<&D3D12_UNORDERED_ACCESS_VIEW_DESC>,
    ) -> (D3D12_GPU_DESCRIPTOR_HANDLE, D3D12_CPU_DESCRIPTOR_HANDLE) {
        let slot = (self.next.fetch_add(1, Relaxed) % SLOTS) as usize;
        // SAFETY: slots inside our two heaps; a live texture; the copy is from a heap the card does not see.
        unsafe {
            let hidden = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: self.hidden.GetCPUDescriptorHandleForHeapStart().ptr + slot * self.step,
            };
            let shown = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: self.visible.GetCPUDescriptorHandleForHeapStart().ptr + slot * self.step,
            };
            device.CreateUnorderedAccessView(texture, None::<&ID3D12Resource>, desc.map(std::ptr::from_ref), hidden);
            device.CopyDescriptorsSimple(1, shown, hidden, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: self.visible.GetGPUDescriptorHandleForHeapStart().ptr + (slot * self.step) as u64,
            };
            (gpu, hidden)
        }
    }

    /// Records the clear of the view `desc` of `texture` (in the unordered access state) on `list`; `rects` are
    /// already inside the view. Leaves our heap set on the list.
    #[allow(clippy::too_many_arguments)] // the clear's own arguments, and the device and list it goes on
    pub(crate) fn clear(
        &self,
        device: &ID3D12Device,
        list: &ID3D12GraphicsCommandList,
        texture: &ID3D12Resource,
        desc: Option<&D3D12_UNORDERED_ACCESS_VIEW_DESC>,
        kind: Kind,
        values: [u32; 4],
        rects: &[RECT],
    ) {
        let (gpu, cpu) = self.make(device, texture, desc);
        // SAFETY: an open list; our heap and a view in it of a live texture.
        unsafe {
            list.SetDescriptorHeaps(&[Some(self.visible.clone())]);
            match kind {
                Kind::Uint => list.ClearUnorderedAccessViewUint(gpu, cpu, texture, &values, rects),
                Kind::Float => {
                    list.ClearUnorderedAccessViewFloat(gpu, cpu, texture, &values.map(f32::from_bits), rects)
                }
            }
        }
    }
}

/// The host view description of the title's words (the format as the host has it).
fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe { std::ptr::from_ref(&words).cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>().read_unaligned() }
}

/// The view of `texture` (the host copy of title resource `resource`) the title's description `words` asks for, in
/// the format the host allows (the checks the host's own views make: `descriptor/host.rs`, `fits`), and the mip
/// level it covers; or why the clear cannot be made.
fn view_of(
    texture: &ID3D12Resource,
    resource: This,
    words: Option<&[u32; 10]>,
) -> Result<(Option<D3D12_UNORDERED_ACCESS_VIEW_DESC>, u32), &'static str> {
    use crate::resource::{DEPTH_FORMATS, SRGB, family, target_format};
    if crate::resource::twin(resource).is_some() {
        return Err("a depth texture (its unordered access is its colour twin's)");
    }
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    if d.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
        return Err("a buffer");
    }
    if d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        return Err("a texture the host made without unordered access");
    }
    let own = d.Format.0 as u32;
    if DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        return Err("a typed depth texture");
    }
    let typeless = family(own) == own && family(own + 1) == own;
    let Some(words) = words else {
        return if typeless { Err("a typeless texture without a view description") } else { Ok((None, 0)) };
    };
    let mut desc = uav_desc(words);
    let mut view = desc.Format.0 as u32;
    if let Some(&(_, linear)) = SRGB.iter().find(|&&(s, _)| s == view) {
        view = linear;
    }
    if view == 0 {
        if typeless {
            return Err("a typeless texture viewed without a format");
        }
    } else if view != own && target_format(view) == own {
        view = own;
    } else if family(view) != family(own) {
        return Err("a view format of another family than the texture's");
    } else if !typeless {
        view = own;
    }
    desc.Format = DXGI_FORMAT(view as i32);
    // The first member of every texture view's union is its mip level.
    const TEXTURE_DIMENSIONS: std::ops::RangeInclusive<u32> = 2..=8;
    let mip = if TEXTURE_DIMENSIONS.contains(&(desc.ViewDimension.0 as u32)) { words[2] } else { 0 };
    Ok((Some(desc), mip))
}

/// `rects` cut to a view `width` by `height` texels, those left that are not empty.
fn clipped(rects: &[RECT], width: i64, height: i64) -> Vec<RECT> {
    rects
        .iter()
        .map(|r| RECT {
            left: r.left.max(0),
            top: r.top.max(0),
            right: (i64::from(r.right).min(width)) as i32,
            bottom: (i64::from(r.bottom).min(height)) as i32,
        })
        .filter(|r| r.left < r.right && r.top < r.bottom)
        .collect()
}

/// The size of mip level `mip` of a texture of `d`.
fn mip_size(d: &D3D12_RESOURCE_DESC, mip: u32) -> (i64, i64) {
    let level = mip.min(31);
    let width = ((d.Width >> level).max(1)).min(i64::MAX as u64) as i64;
    let height = i64::from((d.Height >> level).max(1));
    (width, height)
}

/// The title clears the unordered access view at `gpu` (and `cpu`) of `resource`, in list `this`, to `values`
/// (four words, the bits of four floats for [`Kind::Float`]) in the `count` rectangles `rects` (all of it for none).
#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(crate) fn run(
    this: This,
    kind: Kind,
    gpu: u64,
    cpu: u64,
    resource: This,
    values: *const [u32; 4],
    count: u32,
    rects: *const Rect,
) {
    if !wanted() {
        return;
    }
    let (Some(list), Some(host)) = (super::host_list(this), crate::host::host()) else {
        return;
    };
    // SAFETY: the title passes four values (null is tolerated).
    let Some(&values) = (unsafe { values.as_ref() }) else {
        return skipped("no values");
    };
    let title_rects: &[RECT] = if count > 0 && !rects.is_null() {
        // SAFETY: the title passes `count` rectangles, laid out as the host's.
        unsafe { std::slice::from_raw_parts(rects.cast(), count as usize) }
    } else {
        &[]
    };
    let Some(view) = crate::descriptor::view(gpu).or_else(|| crate::descriptor::view(cpu)) else {
        return skipped("no view recorded at the handles");
    };
    let placed = crate::descriptor::host::placed_texture(&view);
    let View::Unordered(named, _, words) = placed.as_ref().unwrap_or(&view) else {
        return skipped("the view is not an unordered access view");
    };
    let named = *named as This;
    if !resource.is_null() && resource != named {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            tracing::warn!(?resource, ?named, "an unordered-access clear names another resource than its view: the view's is cleared")
        });
    }
    let Some(texture) = crate::resource::host_resource(named) else {
        return skipped("a view of a buffer or of memory the title placed, or a resource without a host copy");
    };
    let (desc, mip) = match view_of(texture, named, words.as_ref()) {
        Ok(made) => made,
        Err(reason) => return skipped(reason),
    };
    // SAFETY: a live resource.
    let (width, height) = mip_size(&unsafe { texture.GetDesc() }, mip);
    let cut = clipped(title_rects, width, height);
    if !title_rects.is_empty() && cut.is_empty() {
        // Rectangles that hold no texel of the view: nothing to clear.
        count_kind(kind);
        return;
    }
    let Some(slots) = production().as_ref() else {
        return skipped("no descriptor heaps for the clears");
    };
    const UNORDERED_ACCESS: u32 = 0x8;
    #[cfg(feature = "diagnostics")]
    crate::capture::note_target(named);
    // Some compute-written textures are also read by the title processor.
    list.texture_written(named);
    super::ensure_state(list, named, UNORDERED_ACCESS);
    first_time(kind, texture, desc.as_ref(), cut.len());
    slots.clear(&host.device, &list.list, texture, desc.as_ref(), kind, values, &cut);
    // Our heap is set now: the next draw or dispatch sets the title's again.
    super::with_state(this, |list, state| state.replay(list));
    count_kind(kind);
}

fn count_kind(kind: Kind) {
    count(kind as usize);
}

/// The first clear of each kind and format, with what the title asked.
fn first_time(kind: Kind, texture: &ID3D12Resource, desc: Option<&D3D12_UNORDERED_ACCESS_VIEW_DESC>, rects: usize) {
    static SEEN: Mutex<Vec<(Kind, i32)>> = Mutex::new(Vec::new());
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let format = desc.map_or(d.Format.0, |d| d.Format.0);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() < 32 && !seen.contains(&(kind, format)) {
        seen.push((kind, format));
        tracing::info!(
            ?kind,
            format,
            width = d.Width,
            height = d.Height,
            slices = d.DepthOrArraySize,
            rects,
            "first unordered-access clear of this kind and format (VICTORY_CLEAR_UAV)"
        );
    }
}

/// The heaps of the host's device.
fn production() -> &'static Option<Slots> {
    static SLOTS: OnceLock<Option<Slots>> = OnceLock::new();
    SLOTS.get_or_init(|| Slots::new(&crate::host::host()?.device))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::core::Interface;

    use crate::host::{transition, warp_adapter};

    const SIDE: u32 = 64;

    /// A device of the software adapter (the debug layer on with `VICTORY_CLEAR_TEST_DEBUG=1`) and a list to record
    /// on.
    struct Rig {
        device: ID3D12Device,
        queue: ID3D12CommandQueue,
        _allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        messages: Option<ID3D12InfoQueue>,
    }

    impl Rig {
        /// `None` (the test passes, saying so) without a software adapter or a device on it.
        fn new() -> Option<Rig> {
            let made = Rig::make();
            if made.is_none() {
                eprintln!("no software D3D12 device: the test is not run");
            }
            made
        }

        fn make() -> Option<Rig> {
            // SAFETY: plain object creation on a device of our own.
            unsafe {
                // The debug layer only when asked (`VICTORY_CLEAR_TEST_DEBUG=1`, with these tests alone): turned
                // on while other tests of the process hold devices, it removes their devices (the runtime's rule),
                // and they fail (device tests of other modules did).
                let mut layer: Option<ID3D12Debug> = None;
                if std::env::var_os("VICTORY_CLEAR_TEST_DEBUG").is_some_and(|v| v == "1")
                    && D3D12GetDebugInterface(&mut layer).is_ok()
                {
                    layer.iter().for_each(|l| l.EnableDebugLayer());
                }
                let adapter = warp_adapter().ok()?;
                let mut device: Option<ID3D12Device> = None;
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
                let device = device?;
                let queue = device
                    .CreateCommandQueue::<ID3D12CommandQueue>(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                        ..Default::default()
                    })
                    .ok()?;
                let allocator = device.CreateCommandAllocator::<ID3D12CommandAllocator>(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
                let list = device
                    .CreateCommandList::<_, _, ID3D12GraphicsCommandList>(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .ok()?;
                let messages = device.cast::<ID3D12InfoQueue>().ok();
                Some(Rig { device, queue, _allocator: allocator, list, messages })
            }
        }

        /// The debug layer's errors since the last call.
        fn errors(&self) -> Vec<String> {
            let mut errors = Vec::new();
            let Some(queue) = self.messages.as_ref() else { return errors };
            // SAFETY: each message is read into a buffer of the size the queue reports.
            unsafe {
                for i in 0..queue.GetNumStoredMessages() {
                    let mut size = 0;
                    if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                        continue;
                    }
                    let mut buffer = vec![0u64; size.div_ceil(8)];
                    let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                    if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                        let m = &*message;
                        if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 {
                            let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                            errors.push(String::from_utf8_lossy(text).trim_end_matches('\0').to_owned());
                        }
                    }
                }
                queue.ClearStoredMessages();
            }
            errors
        }

        fn texture(&self, format: DXGI_FORMAT) -> ID3D12Resource {
            let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(SIDE),
                Height: SIDE,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            };
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain texture creation.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        None,
                        &mut made,
                    )
                    .unwrap();
            }
            made.unwrap()
        }

        /// Runs what `record` puts on the list, then copies `texture` into a read-back buffer and returns its
        /// texels row by row (`bytes` a texel, rows of `SIDE` texels); also that the list closed and the layer
        /// said nothing.
        fn run_and_read(&self, texture: &ID3D12Resource, bytes: usize, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> Vec<u8> {
            const PITCH: u64 = 256;
            let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: PITCH * u64::from(SIDE),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut back: Option<ID3D12Resource> = None;
            // SAFETY: plain buffer creation; the list, queue and fence are ours and the card is waited for
            // before anything recorded is released or read.
            unsafe {
                self.device
                    .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut back)
                    .unwrap();
                let back = back.unwrap();
                record(&self.list);
                self.list.ResourceBarrier(&[transition(
                    texture,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                let place = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: texture.GetDesc().Format,
                        Width: SIDE,
                        Height: SIDE,
                        Depth: 1,
                        RowPitch: PITCH as u32,
                    },
                };
                let to = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&back),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: place },
                };
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(texture),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                self.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
                self.list.Close().expect("the list closes");
                let run: ID3D12CommandList = self.list.cast().unwrap();
                self.queue.ExecuteCommandLists(&[Some(run)]);
                let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
                let event: HANDLE = CreateEventW(None, false, false, None).unwrap();
                self.queue.Signal(&fence, 1).unwrap();
                fence.SetEventOnCompletion(1, event).unwrap();
                assert_eq!(WaitForSingleObject(event, 20_000), WAIT_OBJECT_0, "the card finished");
                assert!(self.device.GetDeviceRemovedReason().is_ok(), "the device is lost");
                let errors = self.errors();
                assert!(errors.is_empty(), "the debug layer reports: {errors:?}");
                let mut data = std::ptr::null_mut();
                back.Map(0, None, Some(&mut data)).unwrap();
                let mut out = Vec::new();
                for row in 0..SIDE as usize {
                    let at = data.cast::<u8>().add(row * PITCH as usize);
                    out.extend_from_slice(std::slice::from_raw_parts(at, SIDE as usize * bytes));
                }
                back.Unmap(0, None);
                out
            }
        }
    }

    fn view(format: DXGI_FORMAT) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
        D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: format,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_UAV { MipSlice: 0, PlaneSlice: 0 } },
        }
    }

    const RECT_IN: RECT = RECT { left: 8, top: 4, right: 24, bottom: 12 };

    fn inside(x: usize, y: usize) -> bool {
        (8..24).contains(&x) && (4..12).contains(&y)
    }

    /// A float clear of an R16_FLOAT view reads back as the value, in a rectangle only inside it.
    #[test]
    fn a_float_clear_of_a_half_float_texture_reads_back() {
        let Some(rig) = Rig::new() else { return };
        let slots = Slots::new(&rig.device).expect("heaps");
        let texture = rig.texture(DXGI_FORMAT_R16_FLOAT);
        let desc = view(DXGI_FORMAT_R16_FLOAT);
        let texels = rig.run_and_read(&texture, 2, |list| {
            let half = [0.5f32, 0.0, 0.0, 0.0].map(f32::to_bits);
            slots.clear(&rig.device, list, &texture, Some(&desc), Kind::Float, half, &[]);
            let two = [2.0f32, 0.0, 0.0, 0.0].map(f32::to_bits);
            slots.clear(&rig.device, list, &texture, Some(&desc), Kind::Float, two, &[RECT_IN]);
        });
        for y in 0..SIDE as usize {
            for x in 0..SIDE as usize {
                let at = (y * SIDE as usize + x) * 2;
                let got = u16::from_le_bytes([texels[at], texels[at + 1]]);
                // Half floats: 0.5 is 0x3800, 2.0 is 0x4000.
                assert_eq!(got, if inside(x, y) { 0x4000 } else { 0x3800 }, "texel {x},{y}");
            }
        }
    }

    /// A uint clear of an R32_UINT view reads back as the value, in a rectangle only inside it; a view made
    /// without a description of a typed texture clears it too.
    #[test]
    fn a_uint_clear_of_a_uint_texture_reads_back() {
        let Some(rig) = Rig::new() else { return };
        let slots = Slots::new(&rig.device).expect("heaps");
        let texture = rig.texture(DXGI_FORMAT_R32_UINT);
        let desc = view(DXGI_FORMAT_R32_UINT);
        let texels = rig.run_and_read(&texture, 4, |list| {
            slots.clear(&rig.device, list, &texture, None, Kind::Uint, [0xabcd_0005, 1, 2, 3], &[]);
            slots.clear(&rig.device, list, &texture, Some(&desc), Kind::Uint, [0x1234_0009, 0, 0, 0], &[RECT_IN]);
        });
        for y in 0..SIDE as usize {
            for x in 0..SIDE as usize {
                let at = (y * SIDE as usize + x) * 4;
                let got = u32::from_le_bytes(texels[at..at + 4].try_into().unwrap());
                assert_eq!(got, if inside(x, y) { 0x1234_0009 } else { 0xabcd_0005 }, "texel {x},{y}");
            }
        }
    }

    /// More clears than slots go round the ring; each still reads back.
    #[test]
    fn the_ring_of_slots_goes_round() {
        let Some(rig) = Rig::new() else { return };
        let slots = Slots::new(&rig.device).expect("heaps");
        let texture = rig.texture(DXGI_FORMAT_R32_UINT);
        let texels = rig.run_and_read(&texture, 4, |list| {
            for i in 0..SLOTS + 8 {
                slots.clear(&rig.device, list, &texture, None, Kind::Uint, [i, 0, 0, 0], &[]);
            }
        });
        let want = SLOTS + 7;
        assert!(texels.chunks(4).all(|t| u32::from_le_bytes(t.try_into().unwrap()) == want));
    }

    /// Rectangles are cut to the view and the empty ones dropped.
    #[test]
    fn rectangles_are_cut_to_the_view() {
        let r = |left, top, right, bottom| RECT { left, top, right, bottom };
        let cut = clipped(&[r(-5, -5, 10, 10), r(60, 60, 100, 100), r(70, 0, 80, 5), r(5, 5, 5, 9), r(9, 9, 3, 3)], 64, 64);
        let got: Vec<_> = cut.iter().map(|r| (r.left, r.top, r.right, r.bottom)).collect();
        assert_eq!(got, vec![(0, 0, 10, 10), (60, 60, 64, 64)]);
        assert!(clipped(&[], 64, 64).is_empty());
        let d = D3D12_RESOURCE_DESC { Width: 100, Height: 50, ..Default::default() };
        assert_eq!(mip_size(&d, 0), (100, 50));
        assert_eq!(mip_size(&d, 2), (25, 12));
        assert_eq!(mip_size(&d, 9), (1, 1));
    }

    /// The format a view is made in: the texture's for a typed one, what the title names for a typeless one,
    /// nothing for what the host cannot view.
    #[test]
    fn views_are_made_in_the_formats_the_host_allows() {
        let Some(rig) = Rig::new() else { return };
        let words = |format: u32| [format, 4, 0, 0, 0, 0, 0, 0, 0, 0];
        let none = std::ptr::null();
        // R32_UINT (42) texture: typed, so the view takes its format, as does one in another of its family.
        let typed = rig.texture(DXGI_FORMAT_R32_UINT);
        let (desc, mip) = view_of(&typed, none, Some(&words(42))).unwrap();
        assert_eq!((desc.unwrap().Format, mip), (DXGI_FORMAT_R32_UINT, 0));
        let (desc, _) = view_of(&typed, none, Some(&words(41))).unwrap();
        assert_eq!(desc.unwrap().Format, DXGI_FORMAT_R32_UINT);
        assert!(view_of(&typed, none, None).unwrap().0.is_none());
        // Another family is refused.
        assert!(view_of(&typed, none, Some(&words(54))).is_err());
        // A typeless texture needs a format.
        let typeless = rig.texture(DXGI_FORMAT_R32_TYPELESS);
        assert!(view_of(&typeless, none, None).is_err());
        assert!(view_of(&typeless, none, Some(&words(0))).is_err());
        let (desc, _) = view_of(&typeless, none, Some(&[41, 4, 3, 0, 0, 0, 0, 0, 0, 0])).unwrap();
        assert_eq!(desc.unwrap().Format, DXGI_FORMAT_R32_FLOAT);
        // The mip level is the view's first word.
        assert_eq!(view_of(&typeless, none, Some(&[41, 4, 3, 0, 0, 0, 0, 0, 0, 0])).unwrap().1, 3);
        // A buffer's view has no mip.
        assert_eq!(view_of(&typed, none, Some(&[42, 1, 3, 0, 0, 0, 0, 0, 0, 0])).unwrap().1, 0);
    }

    /// The counters and the line they make.
    #[test]
    fn skipped_clears_are_counted() {
        let before = COUNTS[2].load(Relaxed);
        skipped("a reason of the test");
        skipped("a reason of the test");
        assert!(COUNTS[2].load(Relaxed) >= before + 2);
        assert_eq!(Kind::of(0), Kind::Uint);
        assert_eq!(Kind::of(1 | 7 << 8), Kind::Float);
    }
}
