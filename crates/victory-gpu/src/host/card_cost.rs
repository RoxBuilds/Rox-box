//! What our passes over copies in the card's memory cost the card: the
//! compare after a submission's lists ([`CARD_COMPARE`], mode 2 as the relay
//! runs it) over a title heap of some 170 MB, once and as a busy frame has it
//! (13 times a frame, a list each, behind the lists of a submission), and the
//! hash passes after ranges were sent up (146 a frame over 769 pieces). On a
//! device of its own on the PC's own card (the
//! high-performance adapter), so never beside the other tests:
//! `cargo test -p victory-gpu --lib card_cost -- --ignored --nocapture`.

use super::*;

/// The heap: 2,784 pieces.
const HEAP: u64 = 0xae0_0000;
/// Compares of it a frame (36.9k pieces compared a frame, of 72 passes).
const COMPARES: usize = 13;
/// Hash passes a frame after ranges were sent up, and their pieces.
const HASH_PASSES: u32 = 146;
const HASH_PIECES: u32 = 769;
const RUNS: usize = 9;

fn buffer(device: &ID3D12Device, heap: D3D12_HEAP_PROPERTIES, size: u64, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: plain creation with a valid description.
    unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made) }.unwrap();
    made.unwrap()
}

fn heap_of(kind: D3D12_HEAP_TYPE) -> D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }
}

/// The card's work for one case: lists run one after another as the queue
/// gets them, a timestamp at the start and the end of each; the card's ms
/// from the first to the last, and the sum of each list's own.
struct Bench {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    lists: Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)>,
    fence: ID3D12Fence,
    event: HANDLE,
    sent: u64,
    stamps: ID3D12QueryHeap,
    back: ID3D12Resource,
    ticks_per_ms: f64,
}

impl Bench {
    const MOST: usize = 64;

    fn new(device: ID3D12Device) -> Bench {
        // SAFETY: plain object creation with valid descriptions.
        unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                .unwrap();
            let lists = (0..Self::MOST)
                .map(|_| {
                    let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                    let list: ID3D12GraphicsCommandList =
                        device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
                    list.Close().unwrap();
                    (allocator, list)
                })
                .collect();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let event = CreateEventW(None, false, false, None).unwrap();
            let mut stamps: Option<ID3D12QueryHeap> = None;
            device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 2 * Self::MOST as u32, NodeMask: 0 },
                    &mut stamps,
                )
                .unwrap();
            let back = buffer(
                &device,
                heap_of(D3D12_HEAP_TYPE_READBACK),
                16 * Self::MOST as u64,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            let ticks_per_ms = queue.GetTimestampFrequency().unwrap() as f64 / 1e3;
            Bench { device, queue, lists, fence, event, sent: 0, stamps: stamps.unwrap(), back, ticks_per_ms }
        }
    }

    /// Runs `n` lists, list `i` recorded by `record(i, list)` between its two
    /// timestamps; waits. Returns (first to last, sum of the lists').
    fn run(&mut self, n: usize, record: &mut dyn FnMut(usize, &ID3D12GraphicsCommandList)) -> (f64, f64) {
        assert!(n <= Self::MOST);
        // SAFETY: the lists' last runs were waited for; live objects.
        unsafe {
            for (i, (allocator, list)) in self.lists[..n].iter().enumerate() {
                allocator.Reset().unwrap();
                list.Reset(allocator, None).unwrap();
                list.EndQuery(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 2 * i as u32);
                record(i, list);
                list.EndQuery(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 2 * i as u32 + 1);
                if i + 1 == n {
                    list.ResolveQueryData(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2 * n as u32, &self.back, 0);
                }
                list.Close().unwrap();
            }
            // One list a call, as the host sends them.
            for (_, list) in &self.lists[..n] {
                self.queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().unwrap())]);
            }
            self.sent += 1;
            self.queue.Signal(&self.fence, self.sent).unwrap();
            self.fence.SetEventOnCompletion(self.sent, self.event).unwrap();
            WaitForSingleObject(self.event, INFINITE);
            assert!(self.device.GetDeviceRemovedReason().is_ok());
            let mut data = std::ptr::null_mut();
            self.back.Map(0, None, Some(&mut data)).unwrap();
            let t = std::slice::from_raw_parts(data.cast::<u64>(), 2 * n).to_vec();
            self.back.Unmap(0, None);
            let ms = |a: u64, b: u64| b.saturating_sub(a) as f64 / self.ticks_per_ms;
            let own: f64 = (0..n).map(|i| ms(t[2 * i], t[2 * i + 1])).sum();
            (ms(t[0], t[2 * n - 1]), own)
        }
    }
}

/// The medians of `RUNS` runs of a case, after one to warm up.
fn median(mut case: impl FnMut() -> (f64, f64)) -> (f64, f64) {
    case();
    let mut all: Vec<(f64, f64)> = (0..RUNS).map(|_| case()).collect();
    let mid = |k: fn(&(f64, f64)) -> f64, all: &mut Vec<(f64, f64)>| {
        all.sort_by(|a, b| k(a).total_cmp(&k(b)));
        k(&all[all.len() / 2])
    };
    (mid(|t| t.0, &mut all), mid(|t| t.1, &mut all))
}

#[test]
#[ignore = "opens the PC's own card; see the module notes"]
fn card_compare_cost() {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: plain DXGI queries and device creation on the adapter the
    // system names for high performance (the discrete card, where there are two).
    let name = unsafe {
        let factory = CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter = factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE).unwrap();
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).unwrap();
        adapter.GetDesc1().map(|d| String::from_utf16_lossy(&d.Description)).unwrap_or_default()
    };
    let device = device.unwrap();
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; the out pointer
    // is ours; the blob's bytes; the root signature is in the shader.
    let (root, pso) = unsafe {
        D3DCompile(
            CARD_COMPARE.as_ptr().cast(),
            CARD_COMPARE.len(),
            windows::core::s!("card compare"),
            None,
            None,
            windows::core::s!("main"),
            windows::core::s!("cs_5_1"),
            0,
            0,
            &mut code,
            None,
        )
        .unwrap();
        let code = code.unwrap();
        let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).unwrap();
        let pso: ID3D12PipelineState = device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                ..Default::default()
            })
            .unwrap();
        (root, pso)
    };
    let uav = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    // As `Host::uav_buffer`, `Host::shared_buffer` have them.
    let now = buffer(&device, heap_of(D3D12_HEAP_TYPE_DEFAULT), HEAP, uav, D3D12_RESOURCE_STATE_COMMON);
    let hashes = buffer(&device, heap_of(D3D12_HEAP_TYPE_DEFAULT), crate::shadow::card_hash_bytes(HEAP), uav, D3D12_RESOURCE_STATE_COMMON);
    let shared = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_CUSTOM,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
        MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
        ..Default::default()
    };
    let differs = buffer(&device, shared, 1 << 20, uav, D3D12_RESOURCE_STATE_COMMON);
    // The same heap in the PC's memory: what the passes cost if the card's
    // copy were moved out of its memory.
    let far = buffer(&device, shared, HEAP, uav, D3D12_RESOURCE_STATE_COMMON);
    let mut bench = Bench::new(device.clone());
    // Bytes that are not all alike (no compression to help), in 16 MiB turns.
    const TURN: u64 = 16 << 20;
    let upload = buffer(&device, heap_of(D3D12_HEAP_TYPE_UPLOAD), TURN, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ);
    // SAFETY: the mapping of the upload buffer, `TURN` bytes.
    unsafe {
        let mut data = std::ptr::null_mut();
        upload.Map(0, None, Some(&mut data)).unwrap();
        let words = std::slice::from_raw_parts_mut(data.cast::<u64>(), (TURN / 8) as usize);
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for w in words {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *w = seed;
        }
        upload.Unmap(0, None);
    }
    let turns = HEAP.div_ceil(TURN) as usize;
    // SAFETY: open lists; live buffers.
    bench.run(1, &mut |_, list| unsafe {
        for t in 0..turns as u64 {
            let n = TURN.min(HEAP - t * TURN);
            list.CopyBufferRegion(&now, t * TURN, &upload, 0, n);
            list.CopyBufferRegion(&far, t * TURN, &upload, 0, n);
        }
    });
    let pieces = (HEAP / 65536) as u32;
    let whole = crate::shadow::CardSpan { from: 0, to: HEAP as u32, first: 0, pieces, skew: 0 };
    // SAFETY (each pass below): an open list with the compare's root
    // signature, pipeline and buffers set; the pieces lie in the buffer.
    let set = |list: &ID3D12GraphicsCommandList, of: &ID3D12Resource| unsafe {
        list.SetComputeRootSignature(&root);
        list.SetPipelineState(&pso);
        list.SetComputeRootUnorderedAccessView(3, differs.GetGPUVirtualAddress());
        list.SetComputeRootShaderResourceView(1, of.GetGPUVirtualAddress());
        list.SetComputeRootUnorderedAccessView(2, hashes.GetGPUVirtualAddress());
    };
    let uav_barrier = |list: &ID3D12GraphicsCommandList| unsafe {
        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
            },
        }]);
    };
    // The first time: every hash stored.
    bench.run(1, &mut |_, list| {
        set(list, &now);
        unsafe { card_pass(list, &whole, HEAP as u32, 0, 0) };
    });
    // One compare of the whole heap, in one list.
    let one = median(|| {
        bench.run(1, &mut |_, list| {
            uav_barrier(list);
            set(list, &now);
            unsafe { card_pass(list, &whole, HEAP as u32, 0, 2) };
        })
    });
    // Thirteen, a list each, each behind a stand-in for a submission's lists
    // (a list of one small copy): as the host sends them.
    let scratch = buffer(&device, heap_of(D3D12_HEAP_TYPE_DEFAULT), 1 << 20, uav, D3D12_RESOURCE_STATE_COMMON);
    let small = buffer(&device, heap_of(D3D12_HEAP_TYPE_DEFAULT), 1 << 20, uav, D3D12_RESOURCE_STATE_COMMON);
    let (scratch, small, heap) = (&scratch, &small, &now);
    let pattern = |with: bool| {
        move |i: usize, list: &ID3D12GraphicsCommandList| unsafe {
            if i.is_multiple_of(2) {
                list.CopyBufferRegion(scratch, 0, small, 0, 1 << 20);
            } else if with {
                uav_barrier(list);
                set(list, heap);
                card_pass(list, &whole, HEAP as u32, 0, 2);
            }
        }
    };
    let frame = median(|| bench.run(2 * COMPARES, &mut pattern(true)));
    let without = median(|| bench.run(2 * COMPARES, &mut pattern(false)));
    // The stand-ins and the empty lists between them alone.
    // One piece: what a pass costs however few its pieces are.
    let piece = crate::shadow::CardSpan { from: 0, to: 65536, first: 0, pieces: 1, skew: 0 };
    let tiny = median(|| {
        bench.run(1, &mut |_, list| {
            set(list, &now);
            unsafe { card_pass(list, &piece, HEAP as u32, 0, 2) };
        })
    });
    // The hash passes after ranges were sent up: runs of five or six pieces
    // spread over the heap, in one list.
    let hashed = median(|| {
        bench.run(1, &mut |_, list| {
            set(list, &now);
            let mut left = HASH_PIECES;
            for p in 0..HASH_PASSES {
                let n = left / (HASH_PASSES - p);
                left -= n;
                let first = (p * 19) % (pieces - n);
                let span = crate::shadow::CardSpan { from: first * 65536, to: (first + n) * 65536, first, pieces: n, skew: 0 };
                unsafe { card_pass(list, &span, HEAP as u32, 0, 0) };
            }
        })
    });
    // The heap in the PC's memory (moved out of the card's).
    let far_one = median(|| {
        bench.run(1, &mut |_, list| {
            uav_barrier(list);
            set(list, &far);
            unsafe { card_pass(list, &whole, HEAP as u32, 0, 2) };
        })
    });
    let gb_s = |ms: f64| HEAP as f64 / 1e6 / ms;
    let name = name.trim_end_matches('\0');
    eprintln!("our passes over a copy of {} MB in the card's memory on {name}, medians of {RUNS} runs:", HEAP >> 20);
    eprintln!("  one compare of the whole heap ({pieces} pieces): {:.3} ms ({:.0} GB/s)", one.1, gb_s(one.1));
    eprintln!(
        "  {COMPARES} compares a list each behind a stand-in list: {:.2} ms first to last, {:.2} ms in the compares' own lists; \
         without the compares {:.2} ms: {:.2} ms of the card a frame for them",
        frame.0,
        frame.1,
        without.0,
        frame.0 - without.0
    );
    eprintln!("  a compare of one piece: {:.3} ms", tiny.1);
    eprintln!("  {HASH_PASSES} hash passes over {HASH_PIECES} pieces in one list: {:.3} ms", hashed.1);
    eprintln!("  one compare of the heap in the PC's memory: {:.2} ms ({:.1} GB/s)", far_one.1, gb_s(far_one.1));
    // SAFETY: our own event.
    unsafe { windows::Win32::Foundation::CloseHandle(bench.event) }.ok();
}
