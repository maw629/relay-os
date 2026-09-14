use core::ptr::NonNull;
use relay_core::dma::{BumpDmaAllocator, DmaAllocator, DmaError, DmaLayout, FrameSource};

const CPU_BASE: u64 = 0xFFFF_9000_0000_0000;

struct VecFrames {
    frames: Vec<u64>,
    next: usize,
    base: u64,
    bytes: Vec<u8>,
}

impl VecFrames {
    fn contiguous(base: u64, count: usize) -> Self {
        let frames = (0..count)
            .map(|index| base + index as u64 * 4096)
            .collect::<Vec<_>>();
        let bytes = vec![0xAA; count * 4096];
        Self {
            frames,
            next: 0,
            base,
            bytes,
        }
    }

    fn contents(&self, phys: u64, len: usize) -> Vec<u8> {
        let offset = (phys - self.base) as usize;
        self.bytes[offset..offset + len].to_vec()
    }
}

impl FrameSource for VecFrames {
    fn allocate_frame(&mut self) -> Option<u64> {
        let frame = *self.frames.get(self.next)?;
        self.next += 1;
        Some(frame)
    }

    fn fill_range(&mut self, phys: u64, len: usize, byte: u8) {
        let offset = (phys - self.base) as usize;
        self.bytes[offset..offset + len].fill(byte);
    }

    fn cpu_address(&self, phys: u64) -> NonNull<u8> {
        NonNull::new((CPU_BASE + (phys - self.base)) as *mut u8).unwrap()
    }
}

fn layout(size: usize, align: usize, max_address: u64, zeroed: bool) -> DmaLayout {
    DmaLayout {
        size,
        align,
        max_address,
        zeroed,
    }
}

#[test]
fn allocation_reports_exact_addresses_and_length() {
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x1_0000_0000, 4));
    let allocation = allocator
        .allocate(layout(8192, 4096, u64::MAX, false))
        .unwrap();
    assert_eq!(allocation.device_address, 0x1_0000_0000);
    assert_eq!(allocation.cpu_address.as_ptr() as u64, CPU_BASE);
    assert_eq!(allocation.len, 8192);
    assert_eq!(
        allocator.source_mut().contents(0x1_0000_0000, 16),
        vec![0xAA; 16]
    );
}

#[test]
fn zeroed_allocation_clears_exactly_its_bytes() {
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x2_0000_0000, 2));
    let allocation = allocator
        .allocate(layout(4096, 4096, u64::MAX, true))
        .unwrap();
    assert_eq!(allocation.device_address, 0x2_0000_0000);
    assert_eq!(
        allocator.source_mut().contents(0x2_0000_0000, 4096),
        vec![0; 4096]
    );
    assert_eq!(
        allocator.source_mut().contents(0x2_0000_1000, 16),
        vec![0xAA; 16]
    );
}

#[test]
fn allocation_validates_alignment_with_over_alloc() {
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x1_0000_0000, 8));
    let allocation = allocator
        .allocate(layout(4096, 8192, u64::MAX, false))
        .unwrap();
    assert_eq!(allocation.device_address % 8192, 0);
}

#[test]
fn bad_layouts_are_rejected_without_consuming_frames() {
    for (size, align) in [
        (0usize, 4096usize),
        (4_194_305, 4096),
        (4096, 0),
        (4096, 3),
        (4096, 131_072),
    ] {
        let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x3_0000_0000, 2));
        let before = allocator.source_mut().next;
        let error = allocator
            .allocate(layout(size, align, u64::MAX, false))
            .unwrap_err();
        assert!(matches!(error, DmaError::TooLarge | DmaError::BadAlign));
        assert_eq!(allocator.source_mut().next, before);
    }
}

#[test]
fn dma_respects_max_address() {
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x1000, 8));
    assert!(
        allocator
            .allocate(layout(4096, 4096, 0x1FFF, false))
            .is_ok()
    );
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x1000, 8));
    assert_eq!(
        allocator.allocate(layout(4096, 4096, 0x1FFE, false)),
        Err(DmaError::AddressLimit)
    );
}

#[test]
fn dma_skips_reserved_hole_and_reports_exhaustion() {
    let source = VecFrames {
        frames: vec![0x1000, 0x2000, 0x3000, 0x5000, 0x6000],
        next: 0,
        base: 0x1000,
        bytes: vec![0; 0x6000],
    };
    let mut allocator = BumpDmaAllocator::new(source);
    let allocation = allocator
        .allocate(layout(8192, 4096, u64::MAX, false))
        .unwrap();
    assert_eq!(allocation.device_address, 0x1000);
    assert_eq!(
        allocator.allocate(layout(3 * 4096, 4096, u64::MAX, false)),
        Err(DmaError::NoMemory)
    );
}

#[test]
fn direct_map_ceiling_is_enforced() {
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x7FFF_FFFF_E000, 4));
    // Run's last byte is 0x8000_0000_0FFF, genuinely crossing the ceiling.
    assert_eq!(
        allocator.allocate(layout(12288, 4096, u64::MAX, false)),
        Err(DmaError::AddressLimit)
    );
}

#[test]
fn dma_error_values_are_stable() {
    assert_ne!(DmaError::TooLarge, DmaError::BadAlign);
    assert_ne!(DmaError::AddressLimit, DmaError::NoMemory);
    assert_ne!(DmaError::NoMemory, DmaError::Allocation);
}
