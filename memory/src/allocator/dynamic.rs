
use std::{ops::Range, ptr::NonNull};
use veclist::VecList;
use hibitset::{BitSet, BitSetLike};

use allocator::Allocator;
use block::Block;
use device::Device;
use error::*;
use mapping::*;
use memory::*;
use util::*;

#[derive(Debug)]
pub struct DynamicBlock<T> {
    index: u32,
    memory: *const Memory<T>,
    ptr: Option<NonNull<u8>>,
    range: Range<u64>,
}

impl<T> DynamicBlock<T> {
    fn shared_memory(&self) -> &Memory<T> {
        // Memory won't be freed until last block created from it deallocated.
        unsafe { &*self.memory }
    }

    fn size(&self) -> u64 {
        self.range.end - self.range.start
    }
}

impl<T: 'static> Block for DynamicBlock<T> {

    type Memory = T;

    #[inline]
    fn properties(&self) -> Properties {
        self.shared_memory().properties()
    }

    #[inline]
    fn memory(&self) -> &T {
        self.shared_memory().raw()
    }

    #[inline]
    fn range(&self) -> Range<u64> {
        self.range.clone()
    }

    #[inline]
    fn map<'a, D>(&'a mut self, _device: &D, range: Range<u64>) -> Result<MappedRange<'a, T>, MappingError> {
        debug_assert!(self.shared_memory().host_visible());
        if let Some(ptr) = self.ptr.clone() {
            let mapping = unsafe {
                MappedRange::from_raw(self.shared_memory(), ptr, self.range.clone())
            };
            Ok(mapping)
        } else {
            assert!(!self.shared_memory().host_visible());
            Err(MappingError::HostInvisible)
        }
    }

    #[inline]
    fn unmap<D>(&mut self, _device: &D, range: Range<u64>) {
        debug_assert!(self.shared_memory().host_visible());
    }
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DynamicConfig {
    /// Number of blocks per chunk.
    pub blocks_per_chunk: u32,

    /// All requests are rounded up to multiple of this value.
    pub block_size_granularity: u64,

    /// Maximum block size.
    /// For any request larger than this won't be allocated with this allocator.
    pub max_block_size: u64,
}

pub struct DynamicAllocator<T> {
    /// Memory type that this allocator allocates.
    memory_type: u32,

    /// Memory properties of the memory type.
    memory_properties: Properties,

    /// Number of blocks per chunk.
    blocks_per_chunk: u32,

    /// All requests are rounded up to multiple of this value.
    block_size_granularity: u64,

    /// List of chunk lists.
    /// Each index corresponds to `block_size_granularity * index` size.
    sizes: Vec<Size<T>>,
}

/// List of chunks
struct Size<T> {
    /// List of chunks.
    chunks: VecList<Chunk<T>>,

    /// Total chunks count.
    total_chunks: u32,

    /// Bits per free blocks.
    blocks: BitSet,
}

impl<T: 'static> DynamicAllocator<T> {
    pub fn properties_required() -> Properties {
        Properties::HOST_VISIBLE
    }

    pub fn max_allocation(&self) -> u64 {
        self.max_block_size()
    }

    pub fn new(
        memory_type: u32,
        memory_properties: Properties,
        config: DynamicConfig,
    ) -> Self {
        assert_ne!(config.block_size_granularity, 0, "Allocation granularity can't be 0");

        let max_chunk_size = config.max_block_size.checked_mul(config.blocks_per_chunk.into()).expect("Max chunk size must fit u64 to allocate it from Vulkan");
        if memory_properties.host_visible() {
            assert!(fits_usize(max_chunk_size), "Max chunk size must fit usize for mapping");
        }
        assert_eq!(config.max_block_size % config.block_size_granularity, 0, "Max block size must be multiple of granularity");

        let sizes = (config.max_block_size / config.block_size_granularity);
        assert!(fits_usize(sizes), "Number of possible must fit usize");
        let sizes = sizes as usize;

        DynamicAllocator {
            memory_type,
            memory_properties,
            block_size_granularity: config.block_size_granularity,
            blocks_per_chunk: config.blocks_per_chunk,
            sizes: (0 .. sizes).map(|_| Size {
                chunks: VecList::new(),
                blocks: BitSet::new(),
                total_chunks: 0,
            }).collect()
        }
    }

    /// Maximum block size.
    /// Any request bigger will be answered with `Err(OutOfMemoryError::OutOfDeviceMemory)`.
    pub fn max_block_size(&self) -> u64 {
        (self.block_size_granularity * self.sizes.len() as u64)
    }

    fn max_chunks_per_size(&self) -> u32 {
        max_blocks_per_size() / self.blocks_per_chunk
    }

    /// Returns size index.
    fn size_index(&self, size: u64) -> usize {
        debug_assert!(size <= self.max_block_size());
        ((size - 1) / self.block_size_granularity) as usize
    }

    /// Get block size for the size index.
    fn block_size(&self, index: usize) -> u64 {
        // Index must be acquired from `size_index` methods. Hence result is less than `max_block_size` and fits u64
        (self.block_size_granularity * (index as u64 + 1))
    }

    /// Allocate super-block to use as chunk memory.
    fn alloc_chunk<D>(&mut self, device: &D, size: u64) -> Result<(Chunk<T>, u64), MemoryError>
    where
        D: Device<Memory = T>,
    {
        if size > self.max_block_size() {
            // Allocate from device.
            let (memory, mapping) = unsafe { // Valid memory type specified.
                let raw = device.allocate(self.memory_type, size)?;

                let mapping = if self.memory_properties.host_visible() {
                    match device.map(&raw, 0 .. size) {
                        Ok(mapping) => Some(mapping),
                        Err(error) => {
                            device.free(raw);
                            return Err(error.into())
                        }
                    }
                } else {
                    None
                };
                let memory = Memory::from_raw(raw, size, self.memory_properties);
                (memory, mapping)
            };
            Ok((Chunk::Dedicated(Box::new(memory), mapping), size))
        } else {
            // Allocate from larger chunks.
            let (dynamic_block, allocated) = self.alloc_from_chunk(device, size)?;
            Ok((Chunk::Dynamic(dynamic_block), allocated))
        }
    }

    /// Allocate super-block to use as chunk memory.
    fn free_chunk<D>(&mut self, device: &D, chunk: Chunk<T>) -> u64
    where
        D: Device<Memory = T>,
    {
        match chunk {
            Chunk::Dedicated(boxed, _) => {
                let size = boxed.size();
                unsafe {
                    device.unmap(boxed.raw());
                    device.free(boxed.into_raw());
                }
                size
            }
            Chunk::Dynamic(dynamic_block) => {
                self.free(device, dynamic_block)
            }
        }
    }

    /// Allocate from chunk.
    fn alloc_from_chunk<D>(&mut self, device: &D, size: u64) -> Result<(DynamicBlock<T>, u64), MemoryError>
    where
        D: Device<Memory = T>,
    {
        let size_index = self.size_index(size);
        let (block_index, allocated) = match (&self.sizes[size_index].blocks).iter().next() {
            Some(block_index) => {
                self.sizes[size_index].blocks.remove(block_index);
                (block_index, 0)
            }
            None => {
                if self.sizes[self.size_index(size)].total_chunks == self.max_chunks_per_size() {
                    return Err(OutOfMemoryError::OutOfHostMemory.into());
                }
                let chunk_size = size * self.blocks_per_chunk as u64;
                let (chunk, allocated) = self.alloc_chunk(device, chunk_size)?;
                let chunk_index = self.sizes[size_index].chunks.push(chunk) as u32;
                self.sizes[size_index].total_chunks += 1;
                let block_index_start = chunk_index * self.blocks_per_chunk;
                let block_index_end = block_index_start + self.blocks_per_chunk;
                for block_index in block_index_start + 1 .. block_index_end {
                    let old = self.sizes[size_index].blocks.add(block_index);
                    debug_assert!(!old);
                }
                (block_index_start, chunk_size)
            }
        };

        let chunk_index = block_index / self.blocks_per_chunk;

        let ref chunk = self.sizes[size_index].chunks[chunk_index as usize];
        let chunk_range = chunk.range();
        let block_size = self.block_size(size_index);
        let block_offset = chunk_range.start + (block_index % self.blocks_per_chunk) as u64 * block_size;
        let block_range = block_offset .. block_offset + block_size;

        Ok((DynamicBlock {
            range: block_range.clone(),
            memory: chunk.shared_memory(),
            index: block_index,
            ptr: chunk.ptr().map(|ptr| mapped_fitting_range(ptr, chunk.range(), block_range).expect("Block must be in sub-range of chunk")),
        }, allocated))
    }
}

impl<T: 'static> Allocator for DynamicAllocator<T> {

    type Memory = T;
    type Block = DynamicBlock<T>;

    fn alloc<D>(&mut self, device: &D, size: u64, align: u64) -> Result<(DynamicBlock<T>, u64), MemoryError>
    where
        D: Device<Memory = T>,
    {
        use std::cmp::max;
        let size = max(size, align);

        if size > self.max_block_size() {
            // Too big block requested.
            Err(OutOfMemoryError::OutOfDeviceMemory.into())
        } else {
            self.alloc_from_chunk(device, size)
        }
    }

    fn free<D>(&mut self, device: &D, block: DynamicBlock<T>) -> u64
    where
        D: Device<Memory = T>,
    {
        let size_index = self.size_index(block.size());
        let block_index = block.index;

        let old = self.sizes[size_index].blocks.add(block_index);
        debug_assert!(!old);

        // TODO: Free chunks.
        0
    }
}

/// Block allocated for chunk.
enum Chunk<T> {
    /// Allocated from device.
    Dedicated(Box<Memory<T>>, Option<NonNull<u8>>),

    /// Allocated from chunk of bigger blocks.
    Dynamic(DynamicBlock<T>),
}

impl<T: 'static> Chunk<T> {
    fn shared_memory(&self) -> &Memory<T> {
        match self {
            Chunk::Dedicated(boxed, _) => &*boxed,
            Chunk::Dynamic(chunk_block) => chunk_block.shared_memory(),
        }
    }

    fn range(&self) -> Range<u64> {
        match self {
            Chunk::Dedicated(boxed, _) => 0 .. boxed.size(),
            Chunk::Dynamic(chunk_block) => chunk_block.range(),
        }
    }

    fn ptr(&self) -> Option<NonNull<u8>> {
        match self {
            Chunk::Dedicated(_, ptr) => *ptr,
            Chunk::Dynamic(chunk_block) => chunk_block.ptr,
        }
    }
}

fn max_blocks_per_size() -> u32 {
    let value = (::std::mem::size_of::<usize>() * 8).pow(4);
    assert!(fits_u32(value));
    value as u32
}