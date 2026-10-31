//! Hunk allocator, ported from the `Hunk_*` code in
//! `code/qcommon/common.c`.
//!
//! The hunk is one block with two banks: permanent allocations grow from
//! one end and temporary allocations from the other, and the banks swap so
//! that whichever side has more wasted highwater space keeps taking the
//! permanent allocations. This port keeps that behaviour over an owned
//! arena, with handles instead of pointers.
//!
//! Preserved: 32-byte rounding of permanent allocations, the 4-byte
//! rounding plus 8-byte header of temporary allocations, the capacity checks,
//! LIFO-only freeing of temporary blocks, marks, highwater tracking and the
//! bank swap rule.
//!
//! Not ported: `Hunk_Trash` (disabled in the C code), `HUNK_DEBUG` block
//! logging, and the shutdown calls `Hunk_Clear` makes into the client and
//! server — clearing only resets the allocator here.

/// Header written in front of every temporary allocation, matching
/// `hunkHeader_t`.
const HEADER: usize = 8;
const HUNK_MAGIC: u32 = 0x8953_7892;
const HUNK_FREE_MAGIC: u32 = 0x8953_7893;

/// Where a permanent allocation should come from, matching `ha_pref`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pref {
    Low,
    High,
    DontCare,
}

/// Why an allocation failed, standing in for the `Com_Error` calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkError {
    /// The request plus what is already in use exceeds the hunk.
    OutOfMemory { size: usize },
    /// A temporary pointer did not carry `HUNK_MAGIC`.
    BadMagic,
    /// The pointer does not point inside this hunk.
    BadPointer,
}

/// What happened when a temporary block was freed. The C code only prints
/// when the block is not the most recent one, leaving it for
/// [`Hunk::clear_temp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempFree {
    Freed,
    NotFinal,
}

/// A handle to memory in the hunk, standing in for the pointer the C
/// functions return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HunkPtr {
    offset: usize,
}

impl HunkPtr {
    /// Byte offset of the allocation inside the hunk.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

/// One bank of the hunk, matching `hunkUsed_t`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Used {
    pub mark: usize,
    pub permanent: usize,
    pub temp: usize,
    pub temp_highwater: usize,
}

/// The hunk: an arena plus the two banks and which one is permanent.
#[derive(Debug)]
pub struct Hunk {
    data: Vec<u8>,
    low: Used,
    high: Used,
    /// True while `low` takes the permanent allocations; the other bank
    /// takes the temporary ones, as `hunk_permanent`/`hunk_temp` do.
    permanent_is_low: bool,
}

impl Hunk {
    /// Initialises a hunk of `total` bytes, as `Hunk_Init` followed by
    /// `Hunk_Clear` leaves it.
    ///
    /// # Panics
    ///
    /// Panics if `total` is smaller than one aligned allocation.
    pub fn new(total: usize) -> Self {
        assert!(total >= 32, "hunk of {total} bytes is too small");
        Hunk {
            data: vec![0; total],
            low: Used::default(),
            high: Used::default(),
            permanent_is_low: true,
        }
    }

    /// Total size of the hunk in bytes.
    pub fn size(&self) -> usize {
        self.data.len()
    }

    /// Bank taking the permanent allocations.
    pub fn permanent(&self) -> Used {
        self.permanent_bank()
    }

    /// Bank taking the temporary allocations.
    pub fn temp(&self) -> Used {
        self.temp_bank()
    }

    /// The low bank, whether it is permanent or temporary right now.
    pub fn low(&self) -> Used {
        self.low
    }

    /// The high bank, whether it is permanent or temporary right now.
    pub fn high(&self) -> Used {
        self.high
    }

    /// True when the low bank currently takes the permanent allocations.
    pub fn permanent_is_low(&self) -> bool {
        self.permanent_is_low
    }

    /// Uninitialised state used by `Hunk_Clear`: zero marks and the low
    /// bank taking permanent allocations.
    pub fn clear(&mut self) {
        self.low = Used::default();
        self.high = Used::default();
        self.permanent_is_low = true;
        self.data.fill(0);
    }

    /// Allocates `size` zeroed bytes that live until [`Hunk::clear`] or
    /// [`Hunk::clear_to_mark`], as `Hunk_Alloc` does. The size is rounded
    /// up to the 32-byte cache line.
    pub fn alloc(&mut self, size: usize, preference: Pref) -> Result<HunkPtr, HunkError> {
        if preference == Pref::DontCare || self.temp_bank().temp != self.temp_bank().permanent {
            self.swap_banks();
        } else {
            let want_low = preference == Pref::Low;
            if self.permanent_is_low != want_low {
                self.swap_banks();
            }
        }

        let size = align(size, 32).ok_or(HunkError::OutOfMemory { size })?;
        if self.low.temp + self.high.temp + size > self.data.len() {
            return Err(HunkError::OutOfMemory { size });
        }

        let offset = if self.permanent_is_low {
            let offset = self.low.permanent;
            self.low.permanent += size;
            self.low.temp = self.low.permanent;
            offset
        } else {
            self.high.permanent += size;
            self.high.temp = self.high.permanent;
            self.data.len() - self.high.permanent
        };

        self.data[offset..offset + size].fill(0);
        Ok(HunkPtr { offset })
    }

    /// Reserves `size` bytes at the temporary end of the hunk with a header,
    /// as `Hunk_AllocateTempMemory` does. The memory is not cleared, because
    /// a file is about to be read into it.
    pub fn allocate_temp(&mut self, size: usize) -> Result<HunkPtr, HunkError> {
        self.swap_banks();

        let payload = align(size, 4).ok_or(HunkError::OutOfMemory { size })?;
        let block = payload
            .checked_add(HEADER)
            .ok_or(HunkError::OutOfMemory { size })?;

        if self.temp().temp + self.permanent().permanent + block > self.data.len() {
            return Err(HunkError::OutOfMemory { size });
        }

        let header = if self.permanent_is_low {
            // The temporary bank is the high one, growing downwards.
            let header = self.data.len() - self.temp().temp - block;
            self.temp_mut().temp += block;
            header
        } else {
            let header = self.temp().temp;
            self.temp_mut().temp += block;
            header
        };

        if self.temp().temp > self.temp().temp_highwater {
            self.temp_mut().temp_highwater = self.temp().temp;
        }

        self.data[header..header + 4].copy_from_slice(&HUNK_MAGIC.to_le_bytes());
        self.data[header + 4..header + HEADER].copy_from_slice(&(block as u32).to_le_bytes());

        Ok(HunkPtr {
            offset: header + HEADER,
        })
    }

    /// Frees the most recent temporary block, as `Hunk_FreeTempMemory`
    /// does. Blocks freed out of order are marked and stay allocated until
    /// [`Hunk::clear_temp`].
    pub fn free_temp(&mut self, ptr: HunkPtr) -> Result<TempFree, HunkError> {
        if ptr.offset < HEADER || ptr.offset + HEADER > self.data.len() {
            return Err(HunkError::BadPointer);
        }
        let header = ptr.offset - HEADER;

        let magic = u32::from_le_bytes(self.data[header..header + 4].try_into().unwrap());
        if magic != HUNK_MAGIC {
            return Err(HunkError::BadMagic);
        }
        let block =
            u32::from_le_bytes(self.data[header + 4..header + HEADER].try_into().unwrap()) as usize;

        self.data[header..header + 4].copy_from_slice(&HUNK_FREE_MAGIC.to_le_bytes());

        // Only the most recent block can be popped, as in the C code.
        let is_final = if self.permanent_is_low {
            // Temporary bank is the high one: its blocks grow downwards.
            header == self.data.len() - self.temp().temp
        } else {
            // Temporary bank is the low one: its blocks grow upwards.
            header == self.temp().temp - block
        };
        if is_final {
            self.temp_mut().temp -= block;
            Ok(TempFree::Freed)
        } else {
            Ok(TempFree::NotFinal)
        }
    }

    /// Drops every temporary allocation, as `Hunk_ClearTempMemory` does.
    pub fn clear_temp(&mut self) {
        let permanent = self.temp_bank().permanent;
        self.temp_mut().temp = permanent;
    }

    /// Records the current permanent sizes on both banks, as
    /// `Hunk_SetMark` does.
    pub fn set_mark(&mut self) {
        self.low.mark = self.low.permanent;
        self.high.mark = self.high.permanent;
    }

    /// Releases everything allocated since the mark, as `Hunk_ClearToMark`
    /// does.
    pub fn clear_to_mark(&mut self) {
        self.low.permanent = self.low.mark;
        self.low.temp = self.low.mark;
        self.high.permanent = self.high.mark;
        self.high.temp = self.high.mark;
    }

    /// True when a mark has been set, as `Hunk_CheckMark` does.
    pub fn check_mark(&self) -> bool {
        self.low.mark != 0 || self.high.mark != 0
    }

    /// Free space between the two banks, as `Hunk_MemoryRemaining`.
    pub fn memory_remaining(&self) -> usize {
        let low = self.low.permanent.max(self.low.temp);
        let high = self.high.permanent.max(self.high.temp);
        self.data.len().saturating_sub(low + high)
    }

    /// Reads `len` bytes from an allocation.
    pub fn get(&self, ptr: HunkPtr, len: usize) -> Option<&[u8]> {
        self.data.get(ptr.offset..ptr.offset.checked_add(len)?)
    }

    /// Writes to an allocation.
    pub fn get_mut(&mut self, ptr: HunkPtr, len: usize) -> Option<&mut [u8]> {
        let end = ptr.offset.checked_add(len)?;
        self.data.get_mut(ptr.offset..end)
    }

    /// `Hunk_SwapBanks`: keep taking permanent allocations from the side
    /// with the larger untouched highwater region, but never while temporary
    /// memory is in use.
    fn swap_banks(&mut self) {
        if self.temp_bank().temp != self.temp_bank().permanent {
            return;
        }
        // The C code does this in `int`, so a bank that is behind on
        // permanent use gives a negative waste and can be swapped out of.
        let temp_waste = self.temp_bank().temp_highwater as i64 - self.temp_bank().permanent as i64;
        let permanent_waste =
            self.permanent_bank().temp_highwater as i64 - self.permanent_bank().permanent as i64;
        if temp_waste > permanent_waste {
            self.permanent_is_low = !self.permanent_is_low;
        }
    }

    fn permanent_bank(&self) -> Used {
        if self.permanent_is_low {
            self.low
        } else {
            self.high
        }
    }

    fn temp_bank(&self) -> Used {
        if self.permanent_is_low {
            self.high
        } else {
            self.low
        }
    }

    fn temp_mut(&mut self) -> &mut Used {
        if self.permanent_is_low {
            &mut self.high
        } else {
            &mut self.low
        }
    }
}

/// Rounds `size` up to a multiple of `step`, returning `None` on overflow.
fn align(size: usize, step: usize) -> Option<usize> {
    size.checked_add(step - 1).map(|n| n & !(step - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUNK: usize = 1024;

    #[test]
    fn new_hunk_is_empty_and_uses_the_low_bank() {
        let hunk = Hunk::new(HUNK);
        assert_eq!(hunk.size(), HUNK);
        assert_eq!(hunk.memory_remaining(), HUNK);
        assert!(hunk.permanent_is_low());
        assert!(!hunk.check_mark());
    }

    #[test]
    fn alloc_rounds_to_the_cacheline_and_zeroes() {
        let mut hunk = Hunk::new(HUNK);
        let a = hunk.alloc(10, Pref::DontCare).unwrap();
        assert_eq!(a.offset(), 0);
        assert_eq!(hunk.low().permanent, 32);

        let b = hunk.alloc(1, Pref::Low).unwrap();
        assert_eq!(b.offset(), 32);

        hunk.get_mut(a, 32).unwrap().fill(0xff);
        let c = hunk.alloc(8, Pref::Low).unwrap();
        assert!(hunk.get(c, 32).unwrap().iter().all(|&byte| byte == 0));
    }

    #[test]
    fn high_preference_allocates_from_the_end() {
        let mut hunk = Hunk::new(HUNK);
        // The preference only acts through the waste rule: with both sides
        // even, Hunk_SwapBanks declines and the first request stays low.
        let a = hunk.alloc(64, Pref::High).unwrap();
        assert_eq!(a.offset(), 0);
        assert!(hunk.permanent_is_low());

        // The low bank is now behind on highwater, so the next request
        // moves the permanent side over to the high bank.
        let b = hunk.alloc(64, Pref::High).unwrap();
        assert_eq!(b.offset(), HUNK - 64);
        assert!(!hunk.permanent_is_low());
        assert_eq!(hunk.memory_remaining(), HUNK - 128);
    }

    #[test]
    fn out_of_memory_is_reported() {
        let mut hunk = Hunk::new(HUNK);
        assert_eq!(
            hunk.alloc(HUNK + 32, Pref::DontCare),
            Err(HunkError::OutOfMemory { size: HUNK + 32 })
        );
        assert_eq!(hunk.low().permanent, 0);
        assert_eq!(hunk.memory_remaining(), HUNK);
    }

    #[test]
    fn temp_allocations_are_lifo() {
        let mut hunk = Hunk::new(HUNK);
        let a = hunk.allocate_temp(100).unwrap();
        // The temporary bank is the high one to start with.
        assert_eq!(a.offset(), HUNK - 100);
        assert_eq!(hunk.temp().temp_highwater, 108);

        let b = hunk.allocate_temp(12).unwrap();
        assert_eq!(b.offset(), HUNK - 100 - 8 - 12);

        assert_eq!(hunk.free_temp(b), Ok(TempFree::Freed));
        assert_eq!(hunk.free_temp(a), Ok(TempFree::Freed));
        assert_eq!(hunk.temp().temp, hunk.temp().permanent);
    }

    #[test]
    fn freeing_out_of_order_leaves_the_block() {
        let mut hunk = Hunk::new(HUNK);
        let a = hunk.allocate_temp(100).unwrap();
        let b = hunk.allocate_temp(100).unwrap();
        let used = hunk.temp().temp;

        assert_eq!(hunk.free_temp(a), Ok(TempFree::NotFinal));
        assert_eq!(hunk.temp().temp, used);

        assert_eq!(hunk.free_temp(b), Ok(TempFree::Freed));
        // The C code stamps the free magic before the final-block check, so
        // a second attempt at `a` fails the magic test rather than popping.
        assert_eq!(hunk.free_temp(a), Err(HunkError::BadMagic));
        assert_eq!(hunk.temp().temp, 108);
    }

    #[test]
    fn freeing_twice_or_garbage_is_reported() {
        let mut hunk = Hunk::new(HUNK);
        let a = hunk.allocate_temp(100).unwrap();
        assert_eq!(hunk.free_temp(a), Ok(TempFree::Freed));
        // The header now carries the free magic.
        assert_eq!(hunk.free_temp(a), Err(HunkError::BadMagic));
        assert_eq!(
            hunk.free_temp(HunkPtr { offset: 0 }),
            Err(HunkError::BadPointer)
        );
        assert_eq!(
            hunk.free_temp(HunkPtr { offset: HUNK }),
            Err(HunkError::BadPointer)
        );
    }

    #[test]
    fn clear_temp_drops_every_temporary_block() {
        let mut hunk = Hunk::new(HUNK);
        hunk.allocate_temp(100).unwrap();
        hunk.allocate_temp(100).unwrap();
        hunk.clear_temp();
        assert_eq!(hunk.temp().temp, hunk.temp().permanent);
        assert_eq!(hunk.memory_remaining(), HUNK - hunk.permanent().permanent);
    }

    #[test]
    fn marks_track_permanent_allocation() {
        let mut hunk = Hunk::new(HUNK);
        hunk.alloc(64, Pref::Low).unwrap();
        hunk.set_mark();
        assert!(hunk.check_mark());
        assert_eq!(hunk.low().mark, 64);

        hunk.alloc(64, Pref::Low).unwrap();
        assert_eq!(hunk.low().permanent, 128);
        hunk.clear_to_mark();
        assert_eq!(hunk.low().permanent, 64);
        assert_eq!(hunk.memory_remaining(), HUNK - 64);
    }

    #[test]
    fn clear_resets_both_banks() {
        let mut hunk = Hunk::new(HUNK);
        hunk.alloc(64, Pref::Low).unwrap();
        hunk.set_mark();
        hunk.clear();
        assert_eq!(hunk.low(), Used::default());
        assert_eq!(hunk.high(), Used::default());
        assert_eq!(hunk.memory_remaining(), HUNK);
        assert!(!hunk.check_mark());
        assert!(hunk.permanent_is_low());
    }

    #[test]
    fn temp_and_permanent_never_collide() {
        let mut hunk = Hunk::new(HUNK);
        // Fill most of the hunk from the permanent side, leaving a temp hole.
        hunk.alloc(HUNK - 128, Pref::Low).unwrap();
        let temp = hunk.allocate_temp(64).unwrap();
        assert!(hunk.get(temp, 64).is_some());
        // No room for anything else.
        assert!(matches!(
            hunk.alloc(64, Pref::DontCare),
            Err(HunkError::OutOfMemory { .. })
        ));
        assert!(hunk.memory_remaining() < 128);
    }

    #[test]
    fn temp_memory_moves_the_permanent_side() {
        let mut hunk = Hunk::new(HUNK);
        hunk.alloc(64, Pref::Low).unwrap();
        assert!(hunk.permanent_is_low());

        // Allocating temp leaves a highwater on this side, so the swap rule
        // hands the permanent side over to it.
        let temp = hunk.allocate_temp(64).unwrap();
        assert!(!hunk.permanent_is_low());

        // Temp memory is live, so the preference cannot swap back.
        let p = hunk.alloc(64, Pref::High).unwrap();
        assert_eq!(p.offset(), HUNK - 64);
        assert!(!hunk.permanent_is_low());

        assert_eq!(hunk.free_temp(temp), Ok(TempFree::Freed));
        assert_eq!(hunk.temp().temp, hunk.temp().permanent);
    }

    #[test]
    fn get_checks_bounds() {
        let mut hunk = Hunk::new(HUNK);
        let a = hunk.alloc(64, Pref::Low).unwrap();
        assert!(hunk.get(a, 64).is_some());
        assert!(hunk.get(a, HUNK + 1).is_none());
        assert!(hunk.get(HunkPtr { offset: HUNK - 1 }, 4).is_none());
        assert!(hunk.get(HunkPtr { offset: HUNK }, 1).is_none());
    }
}
