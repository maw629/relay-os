use core::{cell::UnsafeCell, mem::size_of, slice};

use relay_abi::{BootInfo, MemoryRegion};

pub const PHYSICAL_MEMORY_OFFSET: u64 = 0xffff_8000_0000_0000;
const PAGE_SIZE: u64 = 4096;
const MEMORY_REGION_USABLE: u32 = 1;
const MAX_DIRECT_MAPPED_PHYSICAL_PLUS_ONE: u64 = 0x8000_0000_0000;

pub struct PhysicalFrameAllocator {
    regions: &'static [MemoryRegion],
    boot_info: (u64, u64),
    memory_map: (u64, u64),
    region_index: usize,
    next: u64,
}

struct FrameAllocatorStorage(UnsafeCell<Option<PhysicalFrameAllocator>>);

// Startup is single-core and initializes this once before any subsystem can allocate frames.
unsafe impl Sync for FrameAllocatorStorage {}

static FRAME_ALLOCATOR: FrameAllocatorStorage = FrameAllocatorStorage(UnsafeCell::new(None));

/// # Safety
/// `info` and its memory-map entries must be the validated, permanently retained handoff data.
pub unsafe fn initialize_frame_allocator(info: &BootInfo) -> Result<(), ()> {
    let entry_count = usize::try_from(info.memory_map.entry_count).map_err(|_| ())?;
    // SAFETY: entry validation established an aligned, in-bounds, retained normalized map.
    let regions = unsafe {
        slice::from_raw_parts(
            info.memory_map.entries_address as *const MemoryRegion,
            entry_count,
        )
    };
    let boot_info_start = info as *const BootInfo as u64;
    let boot_info_end = boot_info_start
        .checked_add(size_of::<BootInfo>() as u64)
        .ok_or(())?;
    let memory_map_bytes = (size_of::<MemoryRegion>() as u64)
        .checked_mul(info.memory_map.entry_count)
        .ok_or(())?;
    let memory_map_end = info
        .memory_map
        .entries_address
        .checked_add(memory_map_bytes)
        .ok_or(())?;
    let allocator = PhysicalFrameAllocator {
        // SAFETY: the loader intentionally retains normalized boot data after ExitBootServices.
        regions: unsafe { &*(regions as *const [MemoryRegion]) },
        boot_info: (boot_info_start, boot_info_end),
        memory_map: (info.memory_map.entries_address, memory_map_end),
        region_index: 0,
        next: regions.first().map_or(0, |region| region.start),
    };
    allocator.validate()?;
    // SAFETY: this is the sole initialization of the frame allocator during early startup.
    unsafe { *FRAME_ALLOCATOR.0.get() = Some(allocator) };
    Ok(())
}

impl PhysicalFrameAllocator {
    fn validate(&self) -> Result<(), ()> {
        let mut previous_end = 0;
        for region in self.regions {
            if region.start >= region.end
                || !region.start.is_multiple_of(PAGE_SIZE)
                || !region.end.is_multiple_of(PAGE_SIZE)
                || region.start < previous_end
                || region.reserved != 0
            {
                return Err(());
            }
            previous_end = region.end;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn allocate_frame(&mut self) -> Option<u64> {
        while let Some(region) = self.regions.get(self.region_index) {
            if region.kind != MEMORY_REGION_USABLE || self.next >= region.end {
                self.region_index += 1;
                self.next = self
                    .regions
                    .get(self.region_index)
                    .map_or(0, |next| next.start);
                continue;
            }
            let frame = self.next;
            self.next += PAGE_SIZE;
            if frame == 0 {
                // Physical page zero holds the real-mode IVT and doubles as
                // the walker's corrupt-entry sentinel; never hand it out.
                continue;
            }
            if overlaps((frame, frame + PAGE_SIZE), self.boot_info)
                || overlaps((frame, frame + PAGE_SIZE), self.memory_map)
            {
                continue;
            }
            return Some(frame);
        }
        None
    }
}

fn overlaps(left: (u64, u64), right: (u64, u64)) -> bool {
    left.0 < right.1 && right.0 < left.1
}

/// Returns one usable frame, skipping retained boot structures.
/// Single-core boot discipline makes locking unnecessary.
pub fn allocate_frame() -> Option<u64> {
    // SAFETY: called only on the boot core before any concurrent user exists,
    // and the allocator was initialized once from validated handoff data.
    unsafe { (*FRAME_ALLOCATOR.0.get()).as_mut()?.allocate_frame() }
}

/// Orders prior DMA descriptor stores before later MMIO doorbell writes.
///
/// x86 does not order WB (DRAM descriptor) stores against UC (MMIO)
/// stores, so without a fence the xHC can DMA-read stale zeros from a
/// table the CPU just wrote (observed: QEMU's event-ring reset read a
/// zeroed ERST entry despite a correct guest-side write, which disabled
/// the event ring and faulted the first command with HCE). SFENCE gives
/// the required store-store ordering.
pub fn dma_write_fence() {
    // SAFETY: SFENCE is unprivileged, takes no operands, touches no
    // memory itself, and only orders the calling core's stores.
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) }
}

/// Bounds-checked direct-map slice for DMA fills and table access.
/// Returns None instead of faulting on out-of-range requests.
pub fn direct_slice_mut(physical: u64, len: usize) -> Option<&'static mut [u8]> {
    let end = physical.checked_add(len as u64)?;
    if len == 0 || end - 1 > MAX_DIRECT_MAPPED_PHYSICAL_PLUS_ONE - 1 {
        return None;
    }
    let virt = physical.checked_add(PHYSICAL_MEMORY_OFFSET)?;
    // SAFETY: bounds were checked against the direct-map ceiling and the
    // caller owns the frames it fills; lifetime is static because physical
    // memory outlives every borrower.
    Some(unsafe { core::slice::from_raw_parts_mut(virt as *mut u8, len) })
}

/// Creates the only mutable framebuffer slice used by the runtime. The loader mapped every valid
/// GOP framebuffer through the direct physical map before exiting boot services.
///
/// # Safety
/// `physical_base..physical_base + byte_len` must be a validated, mapped framebuffer range that
/// remains exclusively owned by the caller for the returned lifetime.
pub unsafe fn framebuffer_slice(physical_base: u64, byte_len: usize) -> &'static mut [u8] {
    let virtual_base = physical_base
        .checked_add(PHYSICAL_MEMORY_OFFSET)
        .expect("validated framebuffer direct-map address");
    // SAFETY: caller upholds the loader mapping, bounds, lifetime, and exclusive ownership invariants.
    unsafe { slice::from_raw_parts_mut(virtual_base as *mut u8, byte_len) }
}
