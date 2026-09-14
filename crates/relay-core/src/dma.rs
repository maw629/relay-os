use core::ptr::NonNull;

pub struct DmaLayout {
    pub size: usize,
    pub align: usize,
    pub max_address: u64,
    pub zeroed: bool,
}

pub trait DmaAllocator {
    fn allocate(&mut self, layout: DmaLayout) -> Result<DmaAllocation, DmaError>;
}

#[derive(Debug, Eq, PartialEq)]
pub struct DmaAllocation {
    pub cpu_address: NonNull<u8>,
    pub device_address: u64,
    pub len: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaError {
    TooLarge,
    BadAlign,
    AddressLimit,
    NoMemory,
    Allocation,
}

pub trait FrameSource {
    fn allocate_frame(&mut self) -> Option<u64>;
    fn fill_range(&mut self, phys: u64, len: usize, byte: u8);
    fn cpu_address(&self, phys: u64) -> NonNull<u8>;
}

pub struct BumpDmaAllocator<S> {
    source: S,
}

const PAGE_BYTES: u64 = 4096;
const MAX_ALLOCATION_BYTES: usize = 4 * 1024 * 1024;
const MAX_ALIGN_BYTES: usize = 65536;
const MAX_DIRECT_PHYSICAL: u64 = 0x7FFF_FFFF_FFFF;

impl<S> BumpDmaAllocator<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }

    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }
}

impl<S: FrameSource> DmaAllocator for BumpDmaAllocator<S> {
    fn allocate(&mut self, layout: DmaLayout) -> Result<DmaAllocation, DmaError> {
        if layout.size == 0 || layout.size > MAX_ALLOCATION_BYTES {
            return Err(DmaError::TooLarge);
        }
        if layout.align == 0 || layout.align > MAX_ALIGN_BYTES || !layout.align.is_power_of_two() {
            return Err(DmaError::BadAlign);
        }
        let needed = layout.size.div_ceil(PAGE_BYTES as usize) as u64;
        let extra = if layout.align as u64 <= PAGE_BYTES {
            0
        } else {
            layout.align as u64 / PAGE_BYTES
        };
        let total = needed.checked_add(extra).ok_or(DmaError::TooLarge)?;
        let mut run_start: Option<u64> = None;
        let mut run_prev = 0u64;
        let mut run_count = 0u64;
        loop {
            if run_count == total {
                break;
            }
            let frame = self.source.allocate_frame().ok_or(DmaError::NoMemory)?;
            match run_start {
                Some(_) if frame == run_prev + PAGE_BYTES => {
                    run_count += 1;
                    run_prev = frame;
                }
                _ => {
                    run_start = Some(frame);
                    run_prev = frame;
                    run_count = 1;
                }
            }
        }
        let first = run_start.ok_or(DmaError::NoMemory)?;
        let start = align_up(first, layout.align as u64).ok_or(DmaError::AddressLimit)?;
        let end = start
            .checked_add(layout.size as u64)
            .ok_or(DmaError::AddressLimit)?;
        // `max_address` bounds the last byte inclusively; the direct-map
        // ceiling bounds the exclusive end so no byte reaches 0x8000_0000_0000.
        if end - 1 > layout.max_address || end > MAX_DIRECT_PHYSICAL {
            return Err(DmaError::AddressLimit);
        }
        if layout.zeroed {
            self.source.fill_range(start, layout.size, 0);
        }
        Ok(DmaAllocation {
            cpu_address: self.source.cpu_address(start),
            device_address: start,
            len: layout.size,
        })
    }
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}
