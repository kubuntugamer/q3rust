//! Zone allocator, ported from the `Z_*` code in `code/qcommon/common.c`.
//!
//! The C allocator is a single block of memory cut into `memblock_t`s with
//! intrusive headers, a rover, and coalescing of neighbouring free blocks.
//! This is a safe reimplementation of that behaviour: the block metadata
//! lives in a `Vec` and the payloads live in an owned arena, so the header
//! sizes below are illustrative and no memory is shared with the C engine.
//!
//! Preserved from the C code: first-fit scan starting at the rover and
//! wrapping once, the `MINFRAGMENT` split threshold, `used` accounting by
//! whole block size, free-block coalescing, zeroing for `Z_Malloc`, the
//! `0xaa` scribble and the trash marker checked on free, tags, and the
//! heap consistency checks from `Z_CheckHeap`.
//!
//! Not modelled: the separate main and small zones (`Z_Init` has one
//! instance per zone here), allocation failure messages, and debug labels.

/// Block id written into the trash marker, matching `ZONEID`.
pub const ZONEID: u32 = 0x1d4a11;

/// Smallest leftover block worth keeping as a separate free block,
/// matching `MINFRAGMENT`.
pub const MINFRAGMENT: usize = 64;

/// Tag values, matching `memtag_t` in `qcommon.h`.
pub const TAG_FREE: i32 = 0;
pub const TAG_GENERAL: i32 = 1;
pub const TAG_BOTLIB: i32 = 2;
pub const TAG_RENDERER: i32 = 3;
pub const TAG_SMALL: i32 = 4;
pub const TAG_STATIC: i32 = 5;

/// Bytes reserved at the start of the arena for the zone header, standing
/// in for `sizeof( memzone_t )`.
const ZONE_HEADER: usize = 64;

/// Bytes reserved before each payload, standing in for
/// `sizeof( memblock_t )`.
pub const BLOCK_HEADER: usize = 32;

/// Bytes reserved at the end of each block for the heap trash tester.
const TRASH_SIZE: usize = 4;

/// A block inside a [`Zone`]. `offset` is the block header's byte offset in
/// the arena; the payload starts `BLOCK_HEADER` bytes later.
#[derive(Debug, Clone, Copy)]
struct Block {
    offset: usize,
    size: usize,
    tag: i32,
}

impl Block {
    fn payload(&self) -> usize {
        self.offset + BLOCK_HEADER
    }

    fn end(&self) -> usize {
        self.offset + self.size
    }
}

/// A handle to an allocated block, the stand-in for the pointer returned by
/// `Z_Malloc`. It stays valid until the block is freed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZonePtr {
    offset: usize,
}

impl ZonePtr {
    /// Byte offset of the payload inside the zone's arena.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

/// Allocator errors, standing in for the `Com_Error` calls in `common.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneError {
    /// `Z_TagMalloc` was called with the free tag.
    ZeroTag,
    /// No free block was large enough.
    OutOfMemory { size: usize },
    /// The handle does not point at a live block.
    BadId,
    /// The block was already free.
    DoubleFree,
    /// The trash marker at the end of the block was overwritten.
    WrotePastEnd,
    /// `check_heap` found the block list in an inconsistent state.
    Corrupt(&'static str),
}

/// A zone: an arena plus the block list that cuts it up.
#[derive(Debug)]
pub struct Zone {
    arena: Vec<u8>,
    /// Blocks ordered by offset; neighbouring blocks are adjacent in the
    /// arena and two free blocks are never neighbours.
    blocks: Vec<Block>,
    /// Start of the block the next allocation scan begins at, the rover.
    rover_offset: usize,
    size: usize,
    used: usize,
}

impl Zone {
    /// Creates a zone of `size` bytes holding one free block, the state
    /// `Z_ClearZone` leaves behind.
    ///
    /// # Panics
    ///
    /// Panics if `size` is too small to hold the zone header and one block.
    pub fn new(size: usize) -> Self {
        assert!(
            size >= ZONE_HEADER + BLOCK_HEADER + TRASH_SIZE,
            "zone of {size} bytes is too small"
        );
        Zone {
            arena: vec![0; size],
            blocks: vec![Block {
                offset: ZONE_HEADER,
                size: size - ZONE_HEADER,
                tag: TAG_FREE,
            }],
            rover_offset: ZONE_HEADER,
            size,
            used: 0,
        }
    }

    /// Total size of the zone in bytes, as `zone->size`.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Bytes held by allocated blocks, including their headers.
    pub fn used(&self) -> usize {
        self.used
    }

    /// `size - used`, as `Z_AvailableZoneMemory` reports. Note that block
    /// headers of free blocks count as available, as in the C code.
    pub fn available(&self) -> usize {
        self.size - self.used
    }

    /// Allocates a block of `size` bytes with `tag`, like `Z_TagMalloc`:
    /// the memory is not cleared and the block carries the trash marker.
    pub fn alloc(&mut self, size: usize, tag: i32) -> Result<ZonePtr, ZoneError> {
        if tag == TAG_FREE {
            return Err(ZoneError::ZeroTag);
        }

        // Header, trash marker and 4-byte alignment, as in Z_TagMalloc.
        let need = size
            .checked_add(BLOCK_HEADER + TRASH_SIZE)
            .and_then(|n| n.checked_add(3))
            .map(|n| n & !3)
            .ok_or(ZoneError::OutOfMemory { size })?;

        let start = self.rover_index();
        let mut found = None;
        for step in 0..self.blocks.len() {
            let i = (start + step) % self.blocks.len();
            let block = self.blocks[i];
            if block.tag == TAG_FREE && block.size >= need {
                found = Some(i);
                break;
            }
        }
        let index = found.ok_or(ZoneError::OutOfMemory { size })?;

        // Split off a free fragment when the leftover is worth keeping.
        let block = self.blocks[index];
        if block.size - need > MINFRAGMENT {
            let fragment = Block {
                offset: block.offset + need,
                size: block.size - need,
                tag: TAG_FREE,
            };
            self.blocks[index].size = need;
            self.blocks.insert(index + 1, fragment);
        }

        self.blocks[index].tag = tag;
        self.used += self.blocks[index].size;
        // The next allocation starts after this block, as in Z_TagMalloc.
        self.rover_offset = self.blocks[index].end();

        let end = self.blocks[index].end();
        self.arena[end - TRASH_SIZE..end].copy_from_slice(&ZONEID.to_le_bytes());

        Ok(ZonePtr {
            offset: block.payload(),
        })
    }

    /// Allocates a zero-filled block, like `Z_Malloc`.
    pub fn alloc_zeroed(&mut self, size: usize, tag: i32) -> Result<ZonePtr, ZoneError> {
        let ptr = self.alloc(size, tag)?;
        self.arena[ptr.offset..ptr.offset + size].fill(0);
        Ok(ptr)
    }

    /// Frees a block, like `Z_Free`: static blocks are ignored, the payload
    /// is scribbled with `0xaa`, and free neighbours are coalesced.
    pub fn free(&mut self, ptr: ZonePtr) -> Result<(), ZoneError> {
        let mut index = self.block_index_for(ptr)?;

        match self.blocks[index].tag {
            TAG_FREE => return Err(ZoneError::DoubleFree),
            TAG_STATIC => return Ok(()),
            _ => {}
        }

        let block = self.blocks[index];
        let trash = block.end() - TRASH_SIZE;
        if u32::from_le_bytes(self.arena[trash..block.end()].try_into().unwrap()) != ZONEID {
            return Err(ZoneError::WrotePastEnd);
        }

        self.used -= block.size;
        self.arena[block.payload()..block.end()].fill(0xaa);
        self.blocks[index].tag = TAG_FREE;

        if index > 0 && self.blocks[index - 1].tag == TAG_FREE {
            self.blocks[index - 1].size += self.blocks[index].size;
            self.blocks.remove(index);
            index -= 1;
        }
        if index + 1 < self.blocks.len() && self.blocks[index + 1].tag == TAG_FREE {
            let next = self.blocks[index + 1];
            self.blocks[index].size += next.size;
            self.blocks.remove(index + 1);
        }

        self.rover_offset = self.blocks[index].offset;
        Ok(())
    }

    /// Frees every live block with `tag`, like `Z_FreeTags`, returning how
    /// many blocks were freed.
    pub fn free_tags(&mut self, tag: i32) -> usize {
        if tag == TAG_FREE {
            return 0;
        }
        let ptrs: Vec<ZonePtr> = self
            .blocks
            .iter()
            .filter(|b| b.tag == tag)
            .map(|b| ZonePtr {
                offset: b.payload(),
            })
            .collect();
        for ptr in &ptrs {
            // A static block ignores free, so the count matches the C code,
            // which counts the tag hit before Z_Free returns early.
            let _ = self.free(*ptr);
        }
        ptrs.len()
    }

    /// The bytes after a block's header. The span is at least the requested
    /// size plus the 4-byte trash marker at the end; only the requested
    /// bytes belong to the caller.
    pub fn payload(&self, ptr: ZonePtr) -> Result<&[u8], ZoneError> {
        let index = self.block_index_for(ptr)?;
        Ok(&self.arena[ptr.offset..self.blocks[index].end()])
    }

    /// Mutable form of [`Zone::payload`].
    pub fn payload_mut(&mut self, ptr: ZonePtr) -> Result<&mut [u8], ZoneError> {
        let index = self.block_index_for(ptr)?;
        let end = self.blocks[index].end();
        Ok(&mut self.arena[ptr.offset..end])
    }

    /// Checks the block list the way `Z_CheckHeap` does: blocks must touch
    /// their neighbours, no two free blocks may be adjacent, and the list
    /// must cover the arena.
    pub fn check_heap(&self) -> Result<(), ZoneError> {
        let mut expected = ZONE_HEADER;
        for (i, block) in self.blocks.iter().enumerate() {
            if block.offset != expected {
                return Err(ZoneError::Corrupt(
                    "block does not start where the previous block ended",
                ));
            }
            if block.size == 0 {
                return Err(ZoneError::Corrupt("zero-sized block"));
            }
            if block.tag == TAG_FREE
                && i + 1 < self.blocks.len()
                && self.blocks[i + 1].tag == TAG_FREE
            {
                return Err(ZoneError::Corrupt("two consecutive free blocks"));
            }
            expected = block.end();
        }
        if expected != self.arena.len() {
            return Err(ZoneError::Corrupt("blocks do not cover the zone"));
        }
        let used: usize = self
            .blocks
            .iter()
            .filter(|b| b.tag != TAG_FREE)
            .map(|b| b.size)
            .sum();
        if used != self.used {
            return Err(ZoneError::Corrupt("used byte count is wrong"));
        }
        Ok(())
    }

    fn block_index_for(&self, ptr: ZonePtr) -> Result<usize, ZoneError> {
        self.blocks
            .iter()
            .position(|b| b.payload() == ptr.offset)
            .ok_or(ZoneError::BadId)
    }

    /// Index of the block the allocation scan starts at. When the rover is
    /// past the last block the scan wraps to the first one, like the C
    /// rover returning to the block list head.
    fn rover_index(&self) -> usize {
        self.blocks
            .iter()
            .position(|b| b.offset >= self.rover_offset)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZONE_SIZE: usize = 4096;

    fn zone_with_three() -> (Zone, ZonePtr, ZonePtr, ZonePtr) {
        let mut zone = Zone::new(ZONE_SIZE);
        let a = zone.alloc(200, TAG_GENERAL).unwrap();
        let b = zone.alloc(200, TAG_GENERAL).unwrap();
        let c = zone.alloc(200, TAG_GENERAL).unwrap();
        (zone, a, b, c)
    }

    #[test]
    fn new_zone_is_one_free_block() {
        let zone = Zone::new(ZONE_SIZE);
        assert_eq!(zone.size(), ZONE_SIZE);
        assert_eq!(zone.used(), 0);
        assert_eq!(zone.available(), ZONE_SIZE);
        assert_eq!(zone.blocks.len(), 1);
        assert_eq!(zone.blocks[0].tag, TAG_FREE);
        assert_eq!(zone.blocks[0].size, ZONE_SIZE - ZONE_HEADER);
        zone.check_heap().unwrap();
    }

    #[test]
    #[should_panic(expected = "too small")]
    fn tiny_zone_panics() {
        Zone::new(8);
    }

    #[test]
    fn alloc_zeroed_clears_the_requested_bytes() {
        let mut zone = Zone::new(ZONE_SIZE);
        let ptr = zone.alloc_zeroed(100, TAG_GENERAL).unwrap();

        let payload = zone.payload(ptr).unwrap();
        assert!(payload.len() >= 100);
        assert!(payload[..100].iter().all(|&b| b == 0));
        assert!(zone.used() >= 100 + BLOCK_HEADER);
        assert!(zone.available() < ZONE_SIZE);
        zone.check_heap().unwrap();
    }

    #[test]
    fn alloc_splits_off_a_free_fragment() {
        let mut zone = Zone::new(ZONE_SIZE);
        let ptr = zone.alloc(100, TAG_GENERAL).unwrap();
        assert_eq!(ptr.offset(), ZONE_HEADER + BLOCK_HEADER);

        assert_eq!(zone.blocks.len(), 2);
        assert_eq!(zone.blocks[0].tag, TAG_GENERAL);
        assert_eq!(zone.blocks[1].tag, TAG_FREE);
        assert_eq!(
            zone.blocks[0].size + zone.blocks[1].size,
            ZONE_SIZE - ZONE_HEADER
        );
        zone.check_heap().unwrap();
    }

    #[test]
    fn small_leftover_is_not_split() {
        let mut zone = Zone::new(ZONE_SIZE);
        let free_size = ZONE_SIZE - ZONE_HEADER;
        // need = 4028, leaving 4 bytes, which is below MINFRAGMENT.
        let ptr = zone
            .alloc(free_size - BLOCK_HEADER - TRASH_SIZE - 4, TAG_GENERAL)
            .unwrap();

        assert_eq!(zone.blocks.len(), 1);
        assert_eq!(zone.blocks[0].tag, TAG_GENERAL);
        assert_eq!(zone.blocks[0].size, free_size);
        assert!(zone.payload(ptr).unwrap().len() > 1000);
        zone.check_heap().unwrap();
    }

    #[test]
    fn zero_sized_allocation_works() {
        let mut zone = Zone::new(ZONE_SIZE);
        let ptr = zone.alloc(0, TAG_GENERAL).unwrap();
        assert_eq!(zone.payload(ptr).unwrap().len(), TRASH_SIZE);
        zone.free(ptr).unwrap();
        assert_eq!(zone.used(), 0);
    }

    #[test]
    fn alloc_requires_a_real_tag() {
        let mut zone = Zone::new(ZONE_SIZE);
        assert_eq!(zone.alloc(16, TAG_FREE), Err(ZoneError::ZeroTag));
        assert_eq!(zone.used(), 0);
    }

    #[test]
    fn alloc_beyond_the_zone_fails_without_changing_it() {
        let mut zone = Zone::new(ZONE_SIZE);
        assert!(matches!(
            zone.alloc(ZONE_SIZE, TAG_GENERAL),
            Err(ZoneError::OutOfMemory { size: ZONE_SIZE })
        ));
        assert_eq!(zone.used(), 0);
        assert_eq!(zone.available(), ZONE_SIZE);
        assert_eq!(zone.blocks.len(), 1);
    }

    #[test]
    fn free_coalesces_with_both_neighbours() {
        let (mut zone, a, b, c) = zone_with_three();
        zone.free(b).unwrap();
        zone.free(a).unwrap();
        zone.free(c).unwrap();

        assert_eq!(zone.used(), 0);
        assert_eq!(zone.available(), ZONE_SIZE);
        assert_eq!(zone.blocks.len(), 1);
        assert_eq!(zone.blocks[0].tag, TAG_FREE);
        zone.check_heap().unwrap();
    }

    #[test]
    fn free_scribbles_the_payload() {
        let mut zone = Zone::new(ZONE_SIZE);
        let ptr = zone.alloc(100, TAG_GENERAL).unwrap();
        let payload_len = zone.payload(ptr).unwrap().len();
        // Leave the trash marker at the end of the payload alone.
        zone.payload_mut(ptr).unwrap()[..payload_len - TRASH_SIZE].fill(7);
        zone.free(ptr).unwrap();

        // The freed block merges into a larger free block, so only the part
        // that was scribbled is checked.
        let payload = zone.payload(ptr).unwrap();
        assert!(payload[..payload_len].iter().all(|&b| b == 0xaa));
        zone.check_heap().unwrap();
    }

    #[test]
    fn double_free_is_reported() {
        let mut zone = Zone::new(ZONE_SIZE);
        let a = zone.alloc(100, TAG_GENERAL).unwrap();
        let _b = zone.alloc(100, TAG_GENERAL).unwrap();
        zone.free(a).unwrap();
        assert_eq!(zone.free(a), Err(ZoneError::DoubleFree));
    }

    #[test]
    fn unknown_pointer_is_reported() {
        let mut zone = Zone::new(ZONE_SIZE);
        assert_eq!(zone.free(ZonePtr { offset: 0 }), Err(ZoneError::BadId));
    }

    #[test]
    fn overwritten_trash_marker_is_caught() {
        let mut zone = Zone::new(ZONE_SIZE);
        let ptr = zone.alloc(100, TAG_GENERAL).unwrap();
        let len = zone.payload(ptr).unwrap().len();
        // Corrupt the high byte of the little-endian marker; writing zero
        // there would match the marker and go unnoticed.
        zone.payload_mut(ptr).unwrap()[len - 1] = 0xff;

        assert_eq!(zone.free(ptr), Err(ZoneError::WrotePastEnd));
        assert_eq!(zone.used(), 136);
    }

    #[test]
    fn static_blocks_ignore_free() {
        let mut zone = Zone::new(ZONE_SIZE);
        let ptr = zone.alloc(64, TAG_STATIC).unwrap();
        let used = zone.used();
        zone.free(ptr).unwrap();
        assert_eq!(zone.free(ptr), Ok(()));
        assert_eq!(zone.used(), used);
        assert_eq!(zone.free_tags(TAG_STATIC), 1);
        assert_eq!(zone.used(), used);
    }

    #[test]
    fn free_tags_only_releases_matching_blocks() {
        let mut zone = Zone::new(ZONE_SIZE);
        zone.alloc(100, TAG_RENDERER).unwrap();
        let b = zone.alloc(100, TAG_GENERAL).unwrap();
        zone.alloc(100, TAG_RENDERER).unwrap();

        assert_eq!(zone.free_tags(TAG_RENDERER), 2);
        assert_eq!(zone.free_tags(TAG_RENDERER), 0);
        assert!(zone.payload(b).is_ok());
        assert!(zone.payload(b).unwrap().len() >= 100);
        assert!(zone.blocks.iter().all(|blk| blk.tag != TAG_RENDERER));
        zone.check_heap().unwrap();
    }

    #[test]
    fn freed_blocks_are_reused() {
        let mut zone = Zone::new(ZONE_SIZE);
        let a = zone.alloc(200, TAG_GENERAL).unwrap();
        let _b = zone.alloc(200, TAG_GENERAL).unwrap();
        zone.free(a).unwrap();

        let again = zone.alloc(100, TAG_GENERAL).unwrap();
        assert_eq!(again.offset(), a.offset());
        zone.check_heap().unwrap();
    }

    #[test]
    fn scan_wraps_past_the_rover() {
        let mut zone = Zone::new(2048);
        let a = zone.alloc(400, TAG_GENERAL).unwrap();
        let b = zone.alloc(200, TAG_GENERAL).unwrap();
        zone.free(a).unwrap();

        // This request does not fit in the freed first block, so it moves
        // the rover to the end and leaves a small free block there.
        let _big = zone.alloc(1200, TAG_GENERAL).unwrap();
        assert_eq!(zone.blocks.last().unwrap().tag, TAG_FREE);

        // The scan starts after that small block, wraps once, and finds the
        // free block at the front of the zone.
        let again = zone.alloc(400, TAG_GENERAL).unwrap();
        assert_eq!(again.offset(), a.offset());
        assert!(zone.payload(b).is_ok());
        zone.check_heap().unwrap();
    }

    #[test]
    fn check_heap_catches_adjacent_free_blocks() {
        let mut zone = Zone::new(ZONE_SIZE);
        let a = zone.alloc(200, TAG_GENERAL).unwrap();
        zone.free(a).unwrap();

        // Split the single free block in two, which is the corruption
        // `Z_CheckHeap` rejects.
        let block = zone.blocks[0];
        zone.blocks[0].size = BLOCK_HEADER + TRASH_SIZE;
        zone.blocks.insert(
            1,
            Block {
                offset: block.offset + zone.blocks[0].size,
                size: block.size - zone.blocks[0].size,
                tag: TAG_FREE,
            },
        );
        assert_eq!(
            zone.check_heap(),
            Err(ZoneError::Corrupt("two consecutive free blocks"))
        );
    }
}
