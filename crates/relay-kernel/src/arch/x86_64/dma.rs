use core::ptr::NonNull;

use relay_core::dma::FrameSource;

use super::memory::{allocate_frame, direct_slice_mut};

pub struct KernelDma;

impl FrameSource for KernelDma {
    fn allocate_frame(&mut self) -> Option<u64> {
        allocate_frame()
    }

    fn fill_range(&mut self, phys: u64, len: usize, byte: u8) {
        if let Some(slice) = direct_slice_mut(phys, len) {
            slice.fill(byte);
        }
    }

    fn cpu_address(&self, phys: u64) -> NonNull<u8> {
        NonNull::new((phys + super::memory::PHYSICAL_MEMORY_OFFSET) as *mut u8)
            .expect("DMA physical address is nonzero")
    }
}

pub fn allocator() -> relay_core::dma::BumpDmaAllocator<KernelDma> {
    relay_core::dma::BumpDmaAllocator::new(KernelDma)
}
