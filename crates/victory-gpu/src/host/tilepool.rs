//! Tiles of the card's memory for sparse windows of title memory
//! (`crate::shadow::sparse`): heaps of 16 MiB that hold buffers only, cut in
//! tiles of 64 KiB, and the three ways a reserved buffer's tiles are mapped
//! (`UpdateTileMappings`, on the queue): to tiles of the pool, to nothing,
//! and to one tile all of them share.
//!
//! As `crate::tiles` gives levels of textures their memory (`take` and
//! `give_back` there), with two differences: a tile is taken and given back
//! one by one (a window's tiles come and go with what is asked for), and a
//! pool is a value, so that a test's heaps on its own device never lie in the
//! pool the run uses.
//!
//! Never a heap opened on title memory: mapping tiles into one removes the
//! device (`crate::inplace`, at its head).

use windows::Win32::Graphics::Direct3D12::*;

/// The card's tile: 64 KiB.
pub(crate) const TILE: u64 = 64 << 10;

/// Tiles of a heap: 16 MiB.
pub(crate) const HEAP_TILES: u32 = 256;

/// A tile of a pool: its heap's place in the pool, and its place in the heap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct Tile {
    pub(crate) heap: u16,
    pub(crate) at: u16,
}

struct Heap {
    heap: ID3D12Heap,
    /// A bit for each tile that is free.
    free: [u64; 4],
}

impl Heap {
    fn free_tiles(&self) -> u32 {
        self.free.iter().map(|w| w.count_ones()).sum()
    }
}

/// Heaps of tiles, and how many tiles are given.
#[derive(Default)]
pub(crate) struct Pool {
    heaps: Vec<Option<Heap>>,
    given: u64,
}

impl Pool {
    pub(crate) const fn new() -> Pool {
        Pool {
            heaps: Vec::new(),
            given: 0,
        }
    }

    /// The bytes of the tiles given.
    pub(crate) fn given_bytes(&self) -> u64 {
        self.given * TILE
    }

    /// The bytes of the pool's heaps.
    pub(crate) fn heap_bytes(&self) -> u64 {
        self.heaps.iter().flatten().count() as u64 * u64::from(HEAP_TILES) * TILE
    }

    /// A tile, the lowest free one of the first heap that has one; a new heap
    /// is made on `device` when none has. What the tile holds is what its last
    /// owner left (zeros in a new heap).
    pub(crate) fn take(&mut self, device: &ID3D12Device) -> Option<Tile> {
        let found = self.heaps.iter().position(|h| h.as_ref().is_some_and(|h| h.free_tiles() > 0));
        let index = match found {
            Some(index) => index,
            None => {
                let desc = D3D12_HEAP_DESC {
                    SizeInBytes: u64::from(HEAP_TILES) * TILE,
                    Properties: D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    Alignment: 0,
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
                };
                let mut made: Option<ID3D12Heap> = None;
                // SAFETY: plain creation; the out pointer is ours.
                if let Err(e) = unsafe { device.CreateHeap::<ID3D12Heap>(&raw const desc, &raw mut made) } {
                    tracing::warn!(error = %e, "no heap for the tiles of sparse windows");
                    return None;
                }
                let fresh = Heap {
                    heap: made?,
                    free: [u64::MAX; 4],
                };
                match self.heaps.iter().position(Option::is_none) {
                    Some(index) => {
                        self.heaps[index] = Some(fresh);
                        index
                    }
                    None => {
                        if self.heaps.len() > usize::from(u16::MAX) {
                            return None;
                        }
                        self.heaps.push(Some(fresh));
                        self.heaps.len() - 1
                    }
                }
            }
        };
        let h = self.heaps[index].as_mut()?;
        let word = h.free.iter().position(|&w| w != 0)?;
        let bit = h.free[word].trailing_zeros();
        h.free[word] &= !(1 << bit);
        self.given += 1;
        Some(Tile {
            heap: index as u16,
            at: (word as u32 * 64 + bit) as u16,
        })
    }

    /// Gives a tile back. Nothing may be mapped to it any more, or what is
    /// goes with its buffer.
    pub(crate) fn give_back(&mut self, tile: Tile) {
        let Some(Some(h)) = self.heaps.get_mut(usize::from(tile.heap)) else {
            return;
        };
        let (word, bit) = (usize::from(tile.at) / 64, tile.at % 64);
        if h.free[word] & (1 << bit) == 0 {
            h.free[word] |= 1 << bit;
            self.given -= 1;
        }
    }

    /// Takes the heaps with every tile free out of the pool, but for `keep`
    /// of them: the caller lets them go when the card no longer runs anything
    /// recorded while tiles of theirs were mapped.
    pub(crate) fn empty_heaps(&mut self, keep: usize) -> Vec<ID3D12Heap> {
        let mut kept = 0;
        let mut out = Vec::new();
        for slot in &mut self.heaps {
            if slot.as_ref().is_some_and(|h| h.free_tiles() == HEAP_TILES) {
                kept += 1;
                if kept > keep {
                    out.extend(slot.take().map(|h| h.heap));
                }
            }
        }
        out
    }

    fn heap(&self, index: u16) -> Option<&ID3D12Heap> {
        self.heaps.get(usize::from(index))?.as_ref().map(|h| &h.heap)
    }

    /// Maps tiles of the reserved buffer `buffer` to tiles of the pool:
    /// `pairs` are (the buffer's tile, the pool's). One call for each heap.
    pub(crate) fn map(&self, queue: &ID3D12CommandQueue, buffer: &ID3D12Resource, pairs: &[(u32, Tile)]) {
        let mut heaps: Vec<u16> = pairs.iter().map(|p| p.1.heap).collect();
        heaps.sort_unstable();
        heaps.dedup();
        for index in heaps {
            let Some(heap) = self.heap(index) else {
                continue;
            };
            let of_heap = || pairs.iter().filter(|p| p.1.heap == index);
            let at: Vec<D3D12_TILED_RESOURCE_COORDINATE> =
                of_heap().map(|p| D3D12_TILED_RESOURCE_COORDINATE { X: p.0, ..Default::default() }).collect();
            let sizes = vec![D3D12_TILE_REGION_SIZE { NumTiles: 1, ..Default::default() }; at.len()];
            let flags = vec![D3D12_TILE_RANGE_FLAG_NONE; at.len()];
            let starts: Vec<u32> = of_heap().map(|p| u32::from(p.1.at)).collect();
            let counts = vec![1u32; at.len()];
            // SAFETY: a live reserved buffer and a live heap of this pool;
            // regions of one tile each inside the buffer, and as many ranges
            // of one tile each inside the heap, in arrays of that length that
            // live through the call.
            unsafe {
                queue.UpdateTileMappings(
                    buffer,
                    at.len() as u32,
                    Some(at.as_ptr()),
                    Some(sizes.as_ptr()),
                    heap,
                    at.len() as u32,
                    Some(flags.as_ptr()),
                    Some(starts.as_ptr()),
                    Some(counts.as_ptr()),
                    D3D12_TILE_MAPPING_FLAG_NONE,
                );
            }
        }
    }

    /// Maps `count` tiles of `buffer` from tile `first` on to the one tile
    /// `zero` of the pool, all of them.
    pub(crate) fn map_all_to(&self, queue: &ID3D12CommandQueue, buffer: &ID3D12Resource, first: u32, count: u32, zero: Tile) {
        let Some(heap) = self.heap(zero.heap) else {
            return;
        };
        let at = D3D12_TILED_RESOURCE_COORDINATE { X: first, ..Default::default() };
        let size = D3D12_TILE_REGION_SIZE { NumTiles: count, ..Default::default() };
        let flag = D3D12_TILE_RANGE_FLAG_REUSE_SINGLE_TILE;
        let start = u32::from(zero.at);
        // SAFETY: a live reserved buffer and a live heap; one region of
        // `count` tiles inside the buffer and one range, the one tile.
        unsafe {
            queue.UpdateTileMappings(
                buffer,
                1,
                Some(&raw const at),
                Some(&raw const size),
                heap,
                1,
                Some(&raw const flag),
                Some(&raw const start),
                Some(&raw const count),
                D3D12_TILE_MAPPING_FLAG_NONE,
            );
        }
    }
}

/// Maps `count` tiles of the reserved buffer `buffer` from tile `first` on to
/// nothing: reads of them give zeros and writes are dropped on a card of
/// tiled resources tier 2 or more.
pub(crate) fn unmap(queue: &ID3D12CommandQueue, buffer: &ID3D12Resource, first: u32, count: u32) {
    let at = D3D12_TILED_RESOURCE_COORDINATE { X: first, ..Default::default() };
    let size = D3D12_TILE_REGION_SIZE { NumTiles: count, ..Default::default() };
    let flag = D3D12_TILE_RANGE_FLAG_NULL;
    // SAFETY: a live reserved buffer; one region of `count` tiles inside it
    // and one range with no heap.
    unsafe {
        queue.UpdateTileMappings(
            buffer,
            1,
            Some(&raw const at),
            Some(&raw const size),
            None,
            1,
            Some(&raw const flag),
            None,
            Some(&raw const count),
            D3D12_TILE_MAPPING_FLAG_NONE,
        );
    }
}

/// A reserved buffer of `size` bytes shaders may write, with no memory, in
/// the common state.
pub(crate) fn reserved_buffer(device: &ID3D12Device, size: u64) -> windows::core::Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; a buffer has no clear value.
    unsafe { device.CreateReservedResource(&raw const desc, D3D12_RESOURCE_STATE_COMMON, None, &mut made)? };
    made.ok_or_else(windows::core::Error::empty)
}

/// The tier of tiled resources of `device` (0: none; from 2 on a tile with no
/// memory reads zeros and drops writes).
pub(crate) fn tier(device: &ID3D12Device) -> i32 {
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    // SAFETY: a plain query into a struct of its own size.
    let asked = unsafe {
        device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, (&raw mut options).cast(), size_of_val(&options) as u32)
    };
    if asked.is_ok() { options.TiledResourcesTier.0 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiles are taken lowest first, given back once, and a heap with every
    /// tile free leaves the pool only past what is kept.
    #[test]
    fn warp_tiles_are_taken_and_given_back() {
        let Some(device) = crate::shadow::sparse::tests::warp() else {
            return;
        };
        let mut pool = Pool::new();
        let first: Vec<Tile> = (0..HEAP_TILES).map(|_| pool.take(&device).expect("a tile")).collect();
        assert!(first.iter().enumerate().all(|(i, t)| (t.heap, u32::from(t.at)) == (0, i as u32)));
        assert_eq!((pool.given_bytes(), pool.heap_bytes()), (16 << 20, 16 << 20));
        let next = pool.take(&device).expect("a tile of a second heap");
        assert_eq!((next.heap, next.at), (1, 0));
        pool.give_back(first[7]);
        pool.give_back(first[7]);
        assert_eq!(pool.given_bytes(), u64::from(HEAP_TILES) * TILE);
        assert_eq!(pool.take(&device), Some(first[7]));
        pool.give_back(next);
        assert!(pool.empty_heaps(1).is_empty());
        let gone = pool.empty_heaps(0);
        assert_eq!((gone.len(), pool.heap_bytes()), (1, 16 << 20));
        // The place of the heap that went is used again.
        for tile in first {
            pool.give_back(tile);
        }
        assert_eq!(pool.given_bytes(), 0);
        assert_eq!(pool.empty_heaps(0).len(), 1);
        assert_eq!(pool.take(&device).map(|t| t.heap), Some(0));
    }
}
