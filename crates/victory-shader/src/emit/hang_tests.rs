//! Console wave64 compute programs in two host waves of 32: what the forms with group
//! barriers cost the card and where they stop it (`DXGI_ERROR_DEVICE_HUNG`: a dispatch the
//! driver gives up on). The tools here run a title's program on the hardware device with
//! made-up resources; a run that stops the card takes the device from every test of the
//! process, so they are run alone (`--ignored`, one at a time) and grow a dispatch only
//! while what it took so far says the next one stays far below the driver's patience.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::bitcode::HeapView;
use super::host::Host;
use super::{HostShader, lower};
use crate::native::RootType;

/// The descriptors of the view heap a made-up run binds ([`made_up_run`]): every user-data
/// dword `d` is a table at descriptor `(d + 1) << 12` (`lower.rs` `Lowered::views`), the
/// raw twins of the buffer views start at [`RAW_TWINS`] as the layer's host has them
/// (`shader.rs` `raw_views`), and the last slot is the null one that indices past the heap
/// are clamped to.
const HEAP_VIEWS: u32 = 1 << 17;

/// Where a made-up run's raw twins start.
const RAW_TWINS: u32 = 1 << 16;

/// A made-up run's buffers, one a kind of view: 1 MiB of four dwords over and over.
const BUFFER_BYTES: u64 = 1 << 20;

/// What a made-up run's buffers hold ([`made_up_run`]): the four dwords repeated in the
/// buffer that constant buffers and root views show, in the one raw buffer views show, in
/// the one structured buffer views show and in the one typed buffer views show.
type Fill = [[u32; 4]; 4];

/// A [`Fill`] of hex dwords joined by colons: sixteen, or fewer taken over and over (one
/// dword everywhere; four: the same four in every buffer).
fn fill_of(text: &str) -> Fill {
    let w: Vec<u32> = text.split(':').map(|x| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).unwrap_or(0)).collect();
    std::array::from_fn(|kind| std::array::from_fn(|k| w[(kind * 4 + k) % w.len()]))
}

/// [`fill_of`]'s text for `fill`.
fn fill_text(fill: &Fill) -> String {
    fill.iter().flatten().map(|w| format!("{w:x}")).collect::<Vec<_>>().join(":")
}

fn text(e: windows::core::Error) -> String {
    e.message().to_string()
}

/// A program translated for a made-up run.
struct Made {
    shader: HostShader,
    views: Vec<HeapView>,
    /// The retry form `emit` gave it ("first": its first translation).
    form: String,
    /// `dxil_key` of its DXIL.
    key: u64,
}

/// `translate` for a host of 32-lane waves whose view heap is a made-up run's: indices past
/// it read its null slot and picked buffers are read through their raw twins, as on the
/// layer's host (the DXIL is therefore not the one a census of the folder hashes, which
/// translates for a heap without either).
fn made_up_translation(container: &[u8]) -> Result<Made, crate::ShaderError> {
    let limit = super::shader::view_heap_limit();
    super::shader::set_view_heap_limit(HEAP_VIEWS - 1);
    let twins = super::shader::RAW_VIEWS_HERE.replace(Some(RAW_TWINS));
    let wave = lower::TEST_HOST_WAVE.replace(Some(32));
    let shader = super::translate(container);
    lower::TEST_HOST_WAVE.set(wave);
    super::shader::RAW_VIEWS_HERE.set(twins);
    super::shader::set_view_heap_limit(limit);
    let shader = shader?;
    let key = super::dxil_key(&shader.dxil);
    let form = super::LAST_FORM.take().unwrap_or_else(|| "first".to_owned());
    let views = super::WAITS.with_borrow(|m| m.get(&key).cloned()).unwrap_or_default().1;
    super::WAITS.with_borrow_mut(|m| m.clear());
    Ok(Made { shader, views, form, key })
}

/// What a made-up run puts in a slot of its view heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    /// The DXIL kind of the view (10 typed buffer, 11 raw, 12 structured, 13 constant
    /// buffer; a texture's otherwise) and whether it is an unordered one.
    kind: u32,
    uav: bool,
    /// The second word of the handle's annotation: a typed view's component type and count,
    /// a structure's stride, a constant buffer's size.
    of: u32,
    /// Which of the run's four buffers a buffer view shows ([`Fill`]).
    buffer: usize,
}

/// The slots of a made-up run's view heap for the descriptors `views` a module takes: one
/// view a slot (where the module takes a slot as two kinds, the one that is not raw bytes),
/// and each raw twin shows the buffer of the slot it is the twin of.
fn slots(views: &[HeapView]) -> BTreeMap<u32, Slot> {
    let buffer_of = |kind: u32| match kind {
        13 => 0,
        11 => 1,
        12 => 2,
        _ => 3,
    };
    let mut by_slot: Vec<&HeapView> = views.iter().filter(|v| !v.sampler && v.index.is_some_and(|i| i < HEAP_VIEWS - 1)).collect();
    by_slot.sort_by_key(|v| (v.index, v.props.0 & 0xFF != 11, v.props.0 & 0xFF));
    let mut plan = BTreeMap::new();
    for v in by_slot {
        let (Some(i), kind) = (v.index, v.props.0 & 0xFF) else { continue };
        plan.insert(i, Slot { kind, uav: v.props.0 & 0x1000 != 0, of: v.props.1, buffer: buffer_of(kind) });
    }
    let twins: Vec<u32> = plan.keys().copied().filter(|&i| i >= RAW_TWINS).collect();
    for i in twins {
        if let Some(of) = plan.get(&(i - RAW_TWINS)).copied().filter(|s| (10..=13).contains(&s.kind))
            && let Some(twin) = plan.get_mut(&i)
        {
            twin.buffer = of.buffer;
        }
    }
    plan
}

/// How long `groups` thread groups of `shader` take the hardware device (the least of two
/// dispatches, from the list's execution to its fence) with made-up resources: every buffer
/// view the module takes from the heap (`views`, [`slots`]) shows the buffer of its kind
/// ([`Fill`]), root views too; textures and everything else are null views (reads give 0,
/// writes are dropped); dword `d` of a root constants parameter is `constants[d]` (the last
/// one for those past the end). `Err` when the device does not take it, or when a dispatch
/// is not done after `patience` (the run is then lost: the caller stops).
fn made_up_run(
    host: &Host,
    shader: &HostShader,
    views: &[HeapView],
    fill: &Fill,
    constants: &[u32],
    groups: [u32; 3],
    patience: Duration,
) -> Result<Duration, String> {
    let dev = &host.device;
    let (root, pso) = host.compute_pipeline(shader)?;
    // SAFETY: D3D12 calls with descriptions that outlive them; the resources live until
    // the fence says the card is done, or are leaked when it never does.
    unsafe {
        let make = |heap: D3D12_HEAP_TYPE, bytes: u64, flags, state| -> Result<ID3D12Resource, String> {
            let mut r: Option<ID3D12Resource> = None;
            dev.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: bytes,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: flags,
                    ..Default::default()
                },
                state,
                None,
                &mut r,
            )
            .map_err(text)?;
            r.ok_or_else(|| "no resource".to_owned())
        };
        let mut uploads = Vec::new();
        for words in fill {
            let upload = make(D3D12_HEAP_TYPE_UPLOAD, BUFFER_BYTES, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let mut p = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut p)).map_err(text)?;
            std::slice::from_raw_parts_mut(p.cast::<[u32; 4]>(), (BUFFER_BYTES / 16) as usize).fill(*words);
            upload.Unmap(0, None);
            uploads.push(upload);
        }
        let written = make(
            D3D12_HEAP_TYPE_DEFAULT,
            BUFFER_BYTES,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COMMON,
        )?;
        let heap = |ty, n: u32| -> Result<ID3D12DescriptorHeap, String> {
            dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: ty,
                NumDescriptors: n,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                ..Default::default()
            })
            .map_err(text)
        };
        let view_heap = heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, HEAP_VIEWS)?;
        let sampler_heap = heap(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, 2048)?;
        let view_size = dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
        let sampler_size = dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER) as usize;
        let slot = |i: u32| {
            let mut at = view_heap.GetCPUDescriptorHandleForHeapStart();
            at.ptr += i as usize * view_size;
            at
        };
        let mapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        let null_texture = |kind: u32| D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
            ViewDimension: match kind {
                4 => D3D12_SRV_DIMENSION_TEXTURE3D,
                5 => D3D12_SRV_DIMENSION_TEXTURECUBE,
                7 => D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
                9 => D3D12_SRV_DIMENSION_TEXTURECUBEARRAY,
                _ => D3D12_SRV_DIMENSION_TEXTURE2D,
            },
            Shader4ComponentMapping: mapping,
            ..Default::default()
        };
        // (the slots below the twins: textures; the twins: raw buffers, as the host's are)
        let plain = null_texture(2);
        let null_raw = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: mapping,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV { Flags: D3D12_BUFFER_SRV_FLAG_RAW, ..Default::default() },
            },
        };
        for i in 0..HEAP_VIEWS {
            dev.CreateShaderResourceView(None, Some(if (RAW_TWINS..HEAP_VIEWS - 1).contains(&i) { &null_raw } else { &plain }), slot(i));
        }
        for (i, s) in slots(views) {
            // a typed buffer's (format, bytes an element) by its component type and count
            let typed = match (s.of & 0xFF, s.of >> 8 & 0xFF) {
                (4 | 5, 1) => (DXGI_FORMAT_R32_UINT, 4),
                (4 | 5, 2) => (DXGI_FORMAT_R32G32_UINT, 8),
                (4 | 5, _) => (DXGI_FORMAT_R32G32B32A32_UINT, 16),
                (_, 1) => (DXGI_FORMAT_R32_FLOAT, 4),
                (_, 2) => (DXGI_FORMAT_R32G32_FLOAT, 8),
                _ => (DXGI_FORMAT_R32G32B32A32_FLOAT, 16),
            };
            // (format, bytes an element, structure stride, raw)
            let buffer = match s.kind {
                10 => Some((typed.0, typed.1, 0, false)),
                11 => Some((DXGI_FORMAT_R32_TYPELESS, 4, 0, true)),
                12 => Some((DXGI_FORMAT_UNKNOWN, s.of.max(4), s.of.max(4), false)),
                _ => None,
            };
            match (buffer, s.kind, s.uav) {
                (Some((format, bytes, stride, raw)), _, false) => dev.CreateShaderResourceView(
                    &uploads[s.buffer],
                    Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                        Shader4ComponentMapping: mapping,
                        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                            Buffer: D3D12_BUFFER_SRV {
                                FirstElement: 0,
                                NumElements: (BUFFER_BYTES / u64::from(bytes)) as u32,
                                StructureByteStride: stride,
                                Flags: if raw { D3D12_BUFFER_SRV_FLAG_RAW } else { D3D12_BUFFER_SRV_FLAG_NONE },
                            },
                        },
                    }),
                    slot(i),
                ),
                (Some((format, bytes, stride, raw)), _, true) => dev.CreateUnorderedAccessView(
                    &written,
                    None,
                    Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                            Buffer: D3D12_BUFFER_UAV {
                                FirstElement: 0,
                                NumElements: (BUFFER_BYTES / u64::from(bytes)) as u32,
                                StructureByteStride: stride,
                                CounterOffsetInBytes: 0,
                                Flags: if raw { D3D12_BUFFER_UAV_FLAG_RAW } else { D3D12_BUFFER_UAV_FLAG_NONE },
                            },
                        },
                    }),
                    slot(i),
                ),
                (None, 13, _) => dev.CreateConstantBufferView(
                    Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC {
                        BufferLocation: uploads[s.buffer].GetGPUVirtualAddress(),
                        SizeInBytes: s.of.clamp(256, 0x1_0000).next_multiple_of(256),
                    }),
                    slot(i),
                ),
                (None, _, true) => dev.CreateUnorderedAccessView(
                    None,
                    None,
                    Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                        Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                        ViewDimension: match s.kind {
                            4 => D3D12_UAV_DIMENSION_TEXTURE3D,
                            7 => D3D12_UAV_DIMENSION_TEXTURE2DARRAY,
                            _ => D3D12_UAV_DIMENSION_TEXTURE2D,
                        },
                        ..Default::default()
                    }),
                    slot(i),
                ),
                (None, _, false) => dev.CreateShaderResourceView(None, Some(&null_texture(s.kind)), slot(i)),
            }
        }
        let sampler = D3D12_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            ComparisonFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        for i in 0..2048usize {
            let mut at = sampler_heap.GetCPUDescriptorHandleForHeapStart();
            at.ptr += i * sampler_size;
            dev.CreateSampler(&sampler, at);
        }
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            ud[u.index as usize] = if u.root == RootType::Constants {
                constants.get(u.dword as usize).or(constants.last()).copied().unwrap_or(0)
            } else {
                (u.index + 1) << 17
            };
        }
        // root views, in the order `Host::root_signature_for` declares them
        let mut root_views: Vec<(u8, u8)> = shader
            .root_cbvs
            .iter()
            .map(|&r| (r, 0))
            .chain(shader.root_srvs.iter().map(|&r| (r, 1)))
            .chain(shader.root_uavs.iter().map(|&r| (r, 2)))
            .collect();
        root_views.sort_unstable();
        root_views.dedup();
        let queue: ID3D12CommandQueue = dev
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
            .map_err(text)?;
        let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(text)?;
        let mut best = Duration::MAX;
        for turn in 1..=2u64 {
            let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(text)?;
            let list: ID3D12GraphicsCommandList =
                dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, &pso).map_err(text)?;
            list.SetDescriptorHeaps(&[Some(view_heap.clone()), Some(sampler_heap.clone())]);
            list.SetComputeRootSignature(&root);
            list.SetComputeRoot32BitConstants(0, ud.len() as u32, ud.as_ptr().cast(), 0);
            for (n, &(_, kind)) in root_views.iter().enumerate() {
                let param = n as u32 + 1;
                match kind {
                    0 => list.SetComputeRootConstantBufferView(param, uploads[0].GetGPUVirtualAddress()),
                    1 => list.SetComputeRootShaderResourceView(param, uploads[0].GetGPUVirtualAddress()),
                    _ => list.SetComputeRootUnorderedAccessView(param, written.GetGPUVirtualAddress()),
                }
            }
            list.Dispatch(groups[0], groups[1], groups[2]);
            list.Close().map_err(text)?;
            let start = Instant::now();
            queue.ExecuteCommandLists(&[Some(list.cast().map_err(text)?)]);
            queue.Signal(&fence, turn).map_err(text)?;
            while fence.GetCompletedValue() < turn {
                if let Err(e) = dev.GetDeviceRemovedReason() {
                    return Err(format!("device removed: {}", text(e)));
                }
                if start.elapsed() > patience {
                    // (what the card still uses is not freed under it)
                    std::mem::forget((uploads, written, view_heap, sampler_heap, alloc, root, pso, queue, fence));
                    return Err(format!("not done after {patience:?}"));
                }
                std::thread::yield_now();
            }
            best = best.min(start.elapsed());
        }
        Ok(best)
    }
}

/// [`made_up_run`] grown from one thread group towards `target` groups, eight times as many
/// each turn, for as long as the time the last turn took says the next stays below `budget`:
/// (groups run, the time they took). A program that takes the card long is so never given
/// a dispatch the driver would give up on.
fn made_up_time(
    host: &Host,
    made: &Made,
    fill: &Fill,
    constants: &[u32],
    target: u32,
    budget: Duration,
) -> Result<(u32, Duration), String> {
    let mut groups = 1u32;
    loop {
        let wide = groups.min(256);
        let shape = [wide, groups.div_ceil(wide), 1];
        let ran = shape[0] * shape[1];
        let took = made_up_run(host, &made.shader, &made.views, fill, constants, shape, budget * 4)?;
        let next = (groups * 8).min(target);
        if groups >= target || took.mul_f64(f64::from(next) / f64::from(ran)) > budget {
            return Ok((ran, took));
        }
        groups = next;
    }
}

/// The containers (`*.cdxil`) of the folder `VICTORY_CENSUS_DIR` whose names hold one of the
/// comma-separated parts in `VICTORY_CENSUS_ONLY`, in order.
fn containers(dir: &std::ffi::OsStr) -> Vec<std::path::PathBuf> {
    let only = std::env::var("VICTORY_CENSUS_ONLY").unwrap_or_default();
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .expect("the folder")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "cdxil"))
        .filter(|p| only.split(',').any(|o| p.to_string_lossy().contains(o)))
        .collect();
    files.sort();
    files
}

/// How long the programs of the folder `VICTORY_CENSUS_DIR` (those whose names hold one of
/// the comma-separated parts in `VICTORY_CENSUS_ONLY`) take the hardware device as `translate`
/// gives them for a host of 32-lane waves with the switches of this process
/// ([`made_up_translation`]), with made-up resources ([`made_up_run`]). A line for every
/// program and fill: its form, the hash of its DXIL, the groups run and their time, and that
/// time scaled to `VICTORY_HANG_GROUPS` groups (32,400: 1920x1080 in tiles of 8x8). The fills
/// are those of `VICTORY_HANG_FILLS` ([`fill_of`], comma-separated; 0 unless set) with root
/// constants `VICTORY_HANG_CONSTANTS` (hex, 0), and for a program named in the file
/// `VICTORY_HANG_FILLS_FILE` (lines of a part of a name, a fill and the constants, tabs
/// between) those lines' instead. No dispatch is grown past `VICTORY_HANG_BUDGET_MS` (250).
/// Lines go to `VICTORY_CENSUS_OUT` too when set. Not run unless asked for (`--ignored`): a
/// measurement, and it checks nothing.
#[test]
#[ignore = "a measurement: needs VICTORY_CENSUS_DIR, a folder of containers"]
fn how_long_a_folder_of_containers_takes_the_card() {
    let Some(dir) = std::env::var_os("VICTORY_CENSUS_DIR") else {
        eprintln!("VICTORY_CENSUS_DIR is not set; nothing run");
        return;
    };
    let hex = |x: &str| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).unwrap_or(0);
    let number = |name: &str, unset: u32| std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(unset);
    let constants = std::env::var("VICTORY_HANG_CONSTANTS").ok().map_or(0, |v| hex(&v));
    let fills: Vec<(Fill, u32)> = std::env::var("VICTORY_HANG_FILLS")
        .ok()
        .map(|v| v.split(',').map(|f| (fill_of(f), constants)).collect())
        .unwrap_or_else(|| vec![([[0; 4]; 4], constants)]);
    let named: Vec<(String, Fill, u32)> = std::env::var_os("VICTORY_HANG_FILLS_FILE")
        .map(|f| std::fs::read_to_string(f).expect("the file of fills"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut parts = l.split('\t');
            Some((parts.next()?.to_owned(), fill_of(parts.next()?), parts.next().map_or(0, hex)))
        })
        .collect();
    let target = number("VICTORY_HANG_GROUPS", 32_400);
    let budget = Duration::from_millis(u64::from(number("VICTORY_HANG_BUDGET_MS", 250)));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let mut lines = Vec::new();
    'files: for path in &containers(&dir) {
        let name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes = std::fs::read(path).expect("a container");
        let made = match made_up_translation(&bytes) {
            Ok(m) if m.shader.stage == crate::native::HwStage::Cs => m,
            Ok(_) => continue,
            Err(e) => {
                lines.push(format!("{name}\terr {e}"));
                continue;
            }
        };
        let own: Vec<(Fill, u32)> = named.iter().filter(|n| name.contains(&n.0)).map(|n| (n.1, n.2)).collect();
        for (fill, constants) in if own.is_empty() { &fills } else { &own } {
            let fill_name = fill_text(fill);
            let line = match made_up_time(&host, &made, fill, &[*constants], target, budget) {
                Ok((groups, took)) => {
                    let ms = took.as_secs_f64() * 1e3;
                    format!(
                        "{name}\tfill={fill_name} groups={groups} ms={ms:.3} scaled={:.1} dxil={:016x} form={}",
                        ms * f64::from(target) / f64::from(groups),
                        made.key,
                        made.form
                    )
                }
                Err(e) => format!("{name}\tfill={fill_name} stopped: {e} dxil={:016x} form={}", made.key, made.form),
            };
            eprintln!("{line}");
            let lost = line.contains("device removed") || line.contains("not done after");
            lines.push(line);
            if lost {
                // (the device is gone, or still busy with a dispatch that may never end)
                break 'files;
            }
        }
    }
    if let Some(out) = std::env::var_os("VICTORY_CENSUS_OUT") {
        std::fs::write(out, lines.join("\n") + "\n").expect("the file");
    }
}

/// The DXIL of a made-up run ([`made_up_translation`]) of every container of the folder
/// `VICTORY_CENSUS_DIR` whose name holds one of the comma-separated parts in
/// `VICTORY_CENSUS_ONLY`, written to the folder `VICTORY_DXIL_OUT` as `<name>.dxil` (for `dxc
/// -dumpbin`), with `<name>.slots` beside it: the user-data dwords that are tables, and
/// which buffer of the run each slot of the view heap shows ([`slots`]). Not run unless
/// asked for (`--ignored`).
#[test]
#[ignore = "needs VICTORY_CENSUS_DIR, a folder of containers"]
fn dxil_of_a_folder_of_containers() {
    let Some(dir) = std::env::var_os("VICTORY_CENSUS_DIR") else {
        eprintln!("VICTORY_CENSUS_DIR is not set; nothing written");
        return;
    };
    let out = std::path::PathBuf::from(std::env::var_os("VICTORY_DXIL_OUT").expect("VICTORY_DXIL_OUT names a folder"));
    for path in containers(&dir) {
        let name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes = std::fs::read(&path).expect("a container");
        match made_up_translation(&bytes) {
            Ok(made) => {
                eprintln!("{name}: {} bytes, {}", made.shader.dxil.len(), made.form);
                std::fs::write(out.join(format!("{name}.dxil")), &made.shader.dxil).expect("the file");
                let tables: Vec<String> =
                    made.shader.user_data.iter().filter(|u| u.root != RootType::Constants).map(|u| u.index.to_string()).collect();
                let mut text = format!("tables {}\n", tables.join(","));
                for (i, s) in slots(&made.views) {
                    if (10..=13).contains(&s.kind) && !s.uav {
                        text += &format!("{i} {}\n", s.buffer);
                    }
                }
                std::fs::write(out.join(format!("{name}.slots")), text).expect("the file");
            }
            Err(e) => eprintln!("{name}: {e}"),
        }
    }
}

/// The container `VICTORY_HANG_FILE` translated for a host of 32-lane waves: its form, what
/// its group barriers and meetings may wait for (`bitcode::Waits`), what the host binds for
/// it and the descriptors it takes from the heaps (`lower.rs` `Lowered::views`).
#[test]
#[ignore = "a tool: VICTORY_HANG_FILE=<container>"]
fn what_a_container_waits_for() {
    let Some(file) = std::env::var_os("VICTORY_HANG_FILE") else {
        eprintln!("VICTORY_HANG_FILE is not set; nothing translated");
        return;
    };
    let bytes = std::fs::read(&file).expect("the container");
    lower::TEST_HOST_WAVE.set(Some(32));
    let shader = super::translate(&bytes).expect("a translation");
    lower::TEST_HOST_WAVE.set(None);
    let key = super::dxil_key(&shader.dxil);
    let form = super::LAST_FORM.take().unwrap_or_else(|| "first".to_owned());
    let (waits, views) = super::WAITS.with_borrow(|m| m.get(&key).cloned()).unwrap_or_default();
    println!("dxil={key:016x} bytes={} form={form}", shader.dxil.len());
    println!("waits: {waits:?}");
    println!("user data: {:?}", shader.user_data);
    println!(
        "root cbvs {:?} srvs {:?} uavs {:?} indexed {:?} samplers {}",
        shader.root_cbvs,
        shader.root_srvs,
        shader.root_uavs,
        shader.root_indexed_views,
        shader.samplers.len()
    );
    for v in &views {
        println!("view {:?} sampler={} props={:#x},{:#x}", v.index, v.sampler, v.props.0, v.props.1);
    }
}

/// What `emit` gives `native` on a host of 32-lane waves with its guard on or off: the
/// shader, the name of its form (none: its first translation), and its wave size and group
/// barriers some thread may miss.
fn guarded(native: &crate::native::NativeProgram, guard: bool) -> (HostShader, Option<String>, Option<(Option<u32>, usize)>) {
    use crate::{ir, resource, structure};
    let bytes = crate::dxbc::build(&[]);
    let c = crate::Container::parse(&bytes).expect("container");
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = crate::io::raise(&p, native).expect("io::raise");
    let was = (lower::TEST_HOST_WAVE.replace(Some(32)), super::WAVE_GUARD_TEST.replace(Some(guard)));
    let shader = super::emit(&c, native, &p, &st, &io, &res);
    let form = super::LAST_FORM.take();
    lower::TEST_HOST_WAVE.set(was.0);
    super::WAVE_GUARD_TEST.set(was.1);
    let shader = shader.expect("emit");
    let got = super::EMITTED.with_borrow(|m| m.get(&super::dxil_key(&shader.dxil)).copied());
    if let Ok(mut v) = super::validator::Validator::load(super::validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    (shader, form, got)
}

/// What counts as costly: a barrier or a meeting in a loop, one after a loop that lanes
/// leave at the limit, and a console wave a host thread; barriers in straight code do not.
#[test]
fn barriers_in_loops_and_lanes_in_turn_count_as_costly() {
    use super::bitcode::Waits;
    use super::{Exact, costly};
    let straight = Waits::default();
    assert!(!costly(Exact::Checked, &straight) && !costly(Exact::Pair, &straight));
    assert!(costly(Exact::Serial, &straight));
    let looped = Waits { in_loops: vec![(3, 1)], loops: 1, ..Waits::default() };
    assert!(costly(Exact::Checked, &looped) && costly(Exact::Pair, &looped));
    let cut = Waits { cuts: vec![2], after_cut: vec![5], loops: 1, ..Waits::default() };
    assert!(costly(Exact::Checked, &cut) && costly(Exact::Pair, &cut));
}

/// `meetings_tests::FLAG_LOOP` (the loop of a tiling compute program): its
/// exact form has its group barriers in the loop, taken wave-wide, and the form it had
/// before the exact forms came first has them where the lanes leave one by one. The guard
/// gives it the form of before, whose DXIL is the one the chain gives with the barrier
/// check off; with the guard off it keeps the exact one, as before.
#[test]
fn a_form_with_barriers_in_a_loop_gives_way_to_the_form_of_before() {
    let native = super::meetings_tests::native_lds(&super::meetings_tests::FLAG_LOOP);
    let (exact, form, got) = guarded(&native, false);
    assert_eq!(form.as_deref(), Some("wave-wide branches, meetings checked"));
    assert_eq!(got, Some((Some(32), 0)), "waves of 32, no barrier some thread misses");
    let (before, form, got) = guarded(&native, true);
    assert_ne!(form.as_deref(), Some("wave-wide branches, meetings checked"), "the guard takes the form of before");
    assert!(matches!(got, Some((Some(32), missed)) if missed > 0), "the form of before: waves of 32, barriers the lanes reach apart: {got:?}");
    assert_ne!(before.dxil, exact.dxil);
    lower::TEST_BARRIER_CHECK.set(Some(false));
    let (unchecked, ..) = guarded(&native, false);
    lower::TEST_BARRIER_CHECK.set(None);
    assert_eq!(before.dxil, unchecked.dxil, "the form of before is the one the chain gives without the barrier check");
}

/// A program with no form but an exact one keeps it whatever it costs: the guard has
/// nothing to give `pair_tests::several_waves_native` (meetings of two host waves in its
/// loops) but a refusal.
#[test]
fn a_program_with_no_other_form_keeps_its_exact_one() {
    let native = super::pair_tests::several_waves_native();
    for guard in [false, true] {
        super::LAST_KEPT.set(None);
        let (_, form, got) = guarded(&native, guard);
        assert_eq!(form.as_deref(), Some("wave-wide branches, the halves of each console wave meeting by themselves"), "guard {guard}");
        assert_eq!(got, Some((Some(32), 0)));
        // The guard says so, for the log: which program, which form, and what of it costs.
        let kept = super::LAST_KEPT.take();
        assert_eq!(kept.is_some(), guard, "{kept:?}");
        if let Some(kept) = kept {
            assert_eq!(kept.form, "halves meeting by themselves");
            assert!(kept.in_loops > 0 && kept.digest.len() == 32, "{kept:?}");
        }
    }
    // A form that gives way is not one kept, and neither is one that costs nothing.
    let native = super::meetings_tests::native_lds(&super::meetings_tests::FLAG_LOOP);
    super::LAST_KEPT.set(None);
    let _ = guarded(&native, true);
    assert_eq!(super::LAST_KEPT.take(), None);
}

/// The wait of a half whose other half never comes, at the limit a translation has unless
/// told otherwise (`lower.rs` `pair_rounds`): 4,096 groups of two console waves whose upper
/// halves never count their arrivals are done in half the driver's two seconds or less
/// (with 2^20 rounds this dispatch lost the device), and the time is the limit's: no more
/// than in step with it from a sixteenth of it on, so a limit that costs more than its
/// rounds shows here before it is the default.
#[test]
fn a_half_that_never_comes_costs_little_at_the_default_limit() {
    let native = super::pair_tests::several_waves_native();
    let was = (super::WAVE_PAIR_TEST.replace(Some(true)), lower::TEST_PAIR_ONE_HALF.replace(true));
    let made = |rounds: Option<u32>| {
        lower::TEST_PAIR_ROUNDS.set(rounds);
        let (shader, form, _) = guarded(&native, true);
        assert_eq!(form.as_deref(), Some("wave-wide branches, the halves of each console wave meeting by themselves"));
        let views = super::WAITS.with_borrow(|m| m.get(&super::dxil_key(&shader.dxil)).cloned()).unwrap_or_default().1;
        (shader, views)
    };
    let (short, default) = (made(Some(lower::PAIR_ROUNDS / 16)), made(Some(lower::PAIR_ROUNDS)));
    lower::TEST_PAIR_ROUNDS.set(None);
    super::WAVE_PAIR_TEST.set(was.0);
    lower::TEST_PAIR_ONE_HALF.set(was.1);
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let time = |made: &(HostShader, Vec<HeapView>), groups: u32| {
        made_up_run(&host, &made.0, &made.1, &[[0; 4]; 4], &[3, 1, 64, 1], [groups.min(256), groups.div_ceil(256), 1], Duration::from_secs(1))
    };
    // (grown from few groups: a limit that is not what this test takes it for shows there)
    let mut last = (Duration::ZERO, Duration::ZERO);
    for groups in [64u32, 512, 4096] {
        let (Ok(a), Ok(b)) = (time(&short, groups), time(&default, groups)) else {
            eprintln!("the hardware device does not take this program; not run");
            return;
        };
        let at = "at a sixteenth of the default limit";
        eprintln!("{groups} groups whose upper halves never come: {a:?} {at}, {b:?} at it");
        assert!(b < Duration::from_secs(1), "{groups} groups: {b:?}");
        last = (a, b);
    }
    assert!(last.1 < last.0 * 32 + Duration::from_millis(50), "the default limit's wait: {last:?}");
}

