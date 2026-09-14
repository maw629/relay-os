use core::arch::asm;

use relay_core::mmio::{
    ENTRY_ADDR_MASK, ENTRY_HUGE, ENTRY_PRESENT, MapError, PCI_WINDOW_BASE, UC_MMIO_FLAGS,
    cover_range, is_canonical, page_indices,
};

use super::memory::{allocate_frame, direct_slice_mut};

static mut NEXT_WINDOW: u64 = PCI_WINDOW_BASE;

fn cr3() -> u64 {
    let value: u64;
    // SAFETY: reading CR3 is valid at CPL0 and has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

fn five_level() -> bool {
    let cr4: u64;
    // SAFETY: reading CR4 is valid at CPL0 and has no side effects.
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    cr4 & (1 << 12) != 0
}

fn invlpg(virt: u64) {
    // SAFETY: the caller just installed a mapping for this address on the
    // current core, which is the only core running.
    unsafe { asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags)) };
}

fn next_window(pages: u64) -> Result<u64, MapError> {
    let bytes = pages.checked_mul(4096).ok_or(MapError::InvalidRange)?;
    // SAFETY: single-core boot discipline; this is the only mapper.
    unsafe {
        let start = NEXT_WINDOW;
        NEXT_WINDOW = NEXT_WINDOW
            .checked_add(bytes)
            .ok_or(MapError::InvalidRange)?;
        Ok(start)
    }
}

/// Maps `byte_len` bytes at `phys_base` as uncached MMIO and returns the
/// caller-visible pointer (window base plus the original misalignment).
/// Refuses present leaves and huge-page conflicts without modifying them.
pub fn map_uncached(phys_base: u64, byte_len: usize) -> Result<*mut u8, MapError> {
    let (aligned, pages, offset) = cover_range(phys_base, byte_len)?;
    let five = five_level();
    let virt_base = next_window(pages)?;
    if !is_canonical(virt_base, five) {
        return Err(MapError::InvalidRange);
    }
    let mut page = 0;
    while page < pages {
        let phys = aligned
            .checked_add(page * 4096)
            .ok_or(MapError::InvalidRange)?;
        let virt = virt_base
            .checked_add(page * 4096)
            .ok_or(MapError::InvalidRange)?;
        if !is_canonical(virt, five) {
            return Err(MapError::InvalidRange);
        }
        unsafe { install_4k(virt, phys, five)? };
        invlpg(virt);
        page += 1;
    }
    Ok((virt_base + offset) as *mut u8)
}

/// # Safety
/// `virt`/`phys` are checked 4K-aligned canonical/mappable addresses and
/// the caller holds exclusive mapper ownership on the only running core.
unsafe fn install_4k(virt: u64, phys: u64, five: bool) -> Result<(), MapError> {
    let depth = if five { 5 } else { 4 };
    let indices = page_indices(virt, five);
    let start_level = 5 - depth;
    let mut table_phys = cr3() & ENTRY_ADDR_MASK;
    let mut level = start_level;
    while level < 4 {
        let table = direct_slice_mut(table_phys, 512 * 8).ok_or(MapError::NoMemory)?;
        let entry = unsafe { (table.as_mut_ptr() as *mut u64).add(indices[level] as usize) };
        let value = unsafe { entry.read_volatile() };
        if value & ENTRY_PRESENT == 0 {
            let frame = allocate_frame().ok_or(MapError::NoMemory)?;
            let fresh = direct_slice_mut(frame, 512 * 8).ok_or(MapError::NoMemory)?;
            for byte in fresh.iter_mut() {
                *byte = 0;
            }
            unsafe { entry.write_volatile(frame | ENTRY_PRESENT | (1 << 1)) };
            table_phys = frame;
        } else {
            if value & ENTRY_HUGE != 0 {
                return Err(MapError::AlreadyMapped);
            }
            table_phys = value & ENTRY_ADDR_MASK;
            if table_phys == 0 {
                return Err(MapError::AlreadyMapped);
            }
        }
        level += 1;
    }
    let table = direct_slice_mut(table_phys, 512 * 8).ok_or(MapError::NoMemory)?;
    let leaf = unsafe { (table.as_mut_ptr() as *mut u64).add(indices[4] as usize) };
    if unsafe { leaf.read_volatile() } & ENTRY_PRESENT != 0 {
        return Err(MapError::AlreadyMapped);
    }
    unsafe { leaf.write_volatile(phys | UC_MMIO_FLAGS) };
    Ok(())
}
