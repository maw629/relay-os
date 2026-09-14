#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapError {
    InvalidRange,
    AlreadyMapped,
    NoMemory,
    UnsupportedDepth,
}

pub const PAGE_BYTES: u64 = 4096;
pub const MAX_DIRECT_PHYSICAL: u64 = 0x7FFF_FFFF_FFFF;
pub const PCI_WINDOW_BASE: u64 = 0xFFFF_C000_0000_0000;
pub const UC_MMIO_FLAGS: u64 = 1 | (1 << 1) | (1 << 3) | (1 << 4) | (1 << 63);
pub const ENTRY_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
pub const ENTRY_PRESENT: u64 = 1;
pub const ENTRY_HUGE: u64 = 1 << 7;

pub fn page_indices(virt: u64, five_level: bool) -> [u16; 5] {
    let shift = [48u32, 39, 30, 21, 12];
    let mut indices = [0; 5];
    for (slot, bit) in indices.iter_mut().zip(shift) {
        *slot = ((virt >> bit) & 0x1FF) as u16;
    }
    if !five_level {
        indices[0] = 0;
    }
    indices
}

pub fn is_canonical(virt: u64, five_level: bool) -> bool {
    if five_level {
        let sign = (virt >> 56) & 1;
        let high = virt >> 57;
        (sign == 1 && high == 0x7F) || (sign == 0 && high == 0)
    } else {
        let sign = (virt >> 47) & 1;
        let high = virt >> 48;
        (sign == 1 && high == 0xFFFF) || (sign == 0 && high == 0)
    }
}

pub fn cover_range(base: u64, len: usize) -> Result<(u64, u64, u64), MapError> {
    if len == 0 {
        return Err(MapError::InvalidRange);
    }
    let end = base.checked_add(len as u64).ok_or(MapError::InvalidRange)?;
    if end - 1 > MAX_DIRECT_PHYSICAL {
        return Err(MapError::InvalidRange);
    }
    let aligned = base & !(PAGE_BYTES - 1);
    let offset = base - aligned;
    let span = offset
        .checked_add(len as u64)
        .ok_or(MapError::InvalidRange)?;
    let pages = span.div_ceil(PAGE_BYTES);
    Ok((aligned, pages, offset))
}
