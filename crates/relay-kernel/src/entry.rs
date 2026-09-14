use core::{mem::size_of, ptr};

use relay_abi::{BOOT_ABI_VERSION, BOOT_INFO_MAGIC, BootInfo, MemoryRegion};

const PHYSICAL_MEMORY_OFFSET: u64 = 0xffff_8000_0000_0000;
const MAX_MEMORY_REGIONS: u64 = 512;
const MAX_DIRECT_MAPPED_PHYSICAL: u64 = 0x7fff_ffff_ffff;

/// # Safety
/// The loader must pass a non-null, aligned pointer to an initialized `BootInfo` that
/// remains identity-mapped for the lifetime of kernel entry. No untrusted pointer is
/// dereferenced until this contract has been established by the handoff ABI.
pub unsafe fn enter(info: *const BootInfo) -> ! {
    if !unsafe { valid_boot_info(info) } {
        crate::serial::write(b"[relay] phase=kernel-entry status=invalid\n");
        crate::arch::x86_64::halt();
    }
    // SAFETY: `valid_boot_info` checked this framebuffer's direct-map range and geometry.
    if unsafe { crate::console::initialize(&(*info).framebuffer) }.is_err() {
        crate::serial::write(b"[relay] phase=kernel-runtime status=invalid-framebuffer\n");
        crate::arch::x86_64::halt();
    }
    // SAFETY: diagnostics are available before descriptor-table setup can fault.
    unsafe { crate::arch::x86_64::initialize() };
    // SAFETY: validated handoff data remains resident and its normalized map is retained by the loader.
    if unsafe { crate::arch::x86_64::memory::initialize_frame_allocator(&*info) }.is_err() {
        crate::console::write(b"[relay] phase=kernel-runtime status=invalid-memory-map\n");
        crate::arch::x86_64::halt();
    }
    // SAFETY: the static bounded heap is initialized once before any allocation is attempted.
    unsafe { crate::ALLOCATOR.initialize() };
    // TEMPORARY debug: diagnose NUC page fault at first ACPI read; revert before any PR.
    // SAFETY: `valid_boot_info` established this BootInfo and its RSDP field.
    let rsdp = unsafe { (*info).acpi_rsdp_phys };
    let line = alloc::format!("[relay] phase=platform-probe-debug rsdp=0x{rsdp:x}\n");
    crate::console::write(line.as_bytes());
    // SAFETY: `valid_boot_info` validated the memory-map address, count, and ordering.
    let regions = unsafe {
        core::slice::from_raw_parts(
            (*info).memory_map.entries_address as *const MemoryRegion,
            (*info).memory_map.entry_count as usize,
        )
    };
    let mut found = false;
    for region in regions {
        if region.start <= rsdp && rsdp < region.end {
            let line = alloc::format!(
                "[relay] phase=platform-probe-debug rsdp_region start=0x{s:x} end=0x{e:x} kind={k} reserved={r}\n",
                s = region.start,
                e = region.end,
                k = region.kind,
                r = region.reserved,
            );
            crate::console::write(line.as_bytes());
            found = true;
            break;
        }
    }
    if !found {
        crate::console::write(b"[relay] phase=platform-probe-debug rsdp_region=none\n");
    }
    // SAFETY: `valid_boot_info` established this BootInfo and its RSDP field.
    match unsafe { crate::pci::probe(&*info) } {
        Ok(platform) => {
            let mut xhci_all = alloc::string::String::new();
            for (index, addr) in platform.xhci_all.iter().enumerate() {
                if index > 0 {
                    xhci_all.push(',');
                }
                xhci_all.push_str(&alloc::format!(
                    "{:02x}:{:02x}.{}",
                    addr.bus,
                    addr.device,
                    addr.function
                ));
            }
            for snapshot in &platform.snapshots {
                let candidate = alloc::format!(
                    "[relay] phase=platform-probe-candidate status=ok bdf={:02x}:{:02x}.{} bar_base={:#x} bar_size={:#x} bar64={} slots={} ports={} ctx64={} addr64={} scratch={} legacy={} usb2_off={} usb2_count={} usb3_off={} usb3_count={}\n",
                    snapshot.address.bus,
                    snapshot.address.device,
                    snapshot.address.function,
                    snapshot.bar.base,
                    snapshot.bar.size,
                    snapshot.bar.is_64 as u8,
                    snapshot.caps.max_slots,
                    snapshot.caps.max_ports,
                    snapshot.caps.context_64 as u8,
                    snapshot.caps.addr_64 as u8,
                    snapshot.caps.scratchpad_count,
                    snapshot.caps.legacy_owned as u8,
                    snapshot.caps.usb2_bdf_range.0,
                    snapshot.caps.usb2_bdf_range.1,
                    snapshot.caps.usb3_bdf_range.0,
                    snapshot.caps.usb3_bdf_range.1,
                );
                crate::console::write(candidate.as_bytes());
            }
            let line = alloc::format!(
                "[relay] phase=platform-probe status=ok mcfg_base={:#x} bus={}-{} xhci={:02x}:{:02x}.{} bar_base={:#x} bar_size={:#x} bar64={} slots={} ports={} ctx64={} addr64={} scratch={} legacy={} usb2_off={} usb2_count={} usb3_off={} usb3_count={} dmar={} xhci_all={}\n",
                platform.region.base,
                platform.region.bus_start,
                platform.region.bus_end,
                platform.xhci.bus,
                platform.xhci.device,
                platform.xhci.function,
                platform.bar.base,
                platform.bar.size,
                platform.bar.is_64 as u8,
                platform.caps.max_slots,
                platform.caps.max_ports,
                platform.caps.context_64 as u8,
                platform.caps.addr_64 as u8,
                platform.caps.scratchpad_count,
                platform.caps.legacy_owned as u8,
                platform.caps.usb2_bdf_range.0,
                platform.caps.usb2_bdf_range.1,
                platform.caps.usb3_bdf_range.0,
                platform.caps.usb3_bdf_range.1,
                platform.dmar as u8,
                xhci_all,
            );
            crate::console::write(line.as_bytes());
        }
        Err(error) => {
            let line = alloc::format!(
                "[relay] phase=platform-probe status={} detail={:?}\n",
                error.status(),
                error
            );
            crate::console::write(line.as_bytes());
            crate::arch::x86_64::halt();
        }
    }
    crate::console::write(b"[relay] phase=kernel-entry status=ok\n");
    crate::console::write(b"[relay] phase=kernel-runtime status=ok\n");
    crate::arch::x86_64::halt();
}

unsafe fn valid_boot_info(info: *const BootInfo) -> bool {
    if info.is_null() || !(info as usize).is_multiple_of(core::mem::align_of::<BootInfo>()) {
        return false;
    }
    // SAFETY: `enter` requires this pointer to name an initialized, identity-mapped BootInfo.
    let info = unsafe { &*info };
    if info.magic != BOOT_INFO_MAGIC
        || info.abi_version != BOOT_ABI_VERSION
        || info.struct_size != size_of::<BootInfo>() as u32
        || info.physical_memory_offset != PHYSICAL_MEMORY_OFFSET
        || info.acpi_rsdp_phys == 0
        || !info.acpi_rsdp_phys.is_multiple_of(4)
        || info.root_partition_guid.0 == [0; 16]
    {
        return false;
    }
    valid_framebuffer(&info.framebuffer) && valid_memory_map(info)
}

fn valid_framebuffer(framebuffer: &relay_abi::FramebufferInfo) -> bool {
    if framebuffer.physical_base == 0
        || framebuffer.byte_len == 0
        || framebuffer.width == 0
        || framebuffer.height == 0
        || framebuffer.stride_pixels < framebuffer.width
        || framebuffer.bytes_per_pixel != 4
    {
        return false;
    }
    u64::from(framebuffer.stride_pixels)
        .checked_mul(u64::from(framebuffer.height))
        .and_then(|pixels| pixels.checked_mul(u64::from(framebuffer.bytes_per_pixel)))
        .is_some_and(|required| required <= framebuffer.byte_len)
        && framebuffer
            .physical_base
            .checked_add(framebuffer.byte_len)
            .is_some_and(|end| {
                end > framebuffer.physical_base && end - 1 <= MAX_DIRECT_MAPPED_PHYSICAL
            })
}

fn valid_memory_map(info: &BootInfo) -> bool {
    let map = &info.memory_map;
    if map.entry_count == 0
        || map.entry_count > MAX_MEMORY_REGIONS
        || map.entries_address == 0
        || !map
            .entries_address
            .is_multiple_of(core::mem::align_of::<MemoryRegion>() as u64)
        || map
            .entry_count
            .checked_mul(size_of::<MemoryRegion>() as u64)
            .and_then(|bytes| map.entries_address.checked_add(bytes))
            .is_none()
    {
        return false;
    }

    let entries = ptr::slice_from_raw_parts(
        map.entries_address as *const MemoryRegion,
        map.entry_count as usize,
    );
    // SAFETY: the checked count and address range are supplied by the loader's boot-data page.
    let entries = unsafe { &*entries };
    let mut previous_end = 0_u64;
    for entry in entries {
        if entry.start >= entry.end || entry.start < previous_end || entry.reserved != 0 {
            return false;
        }
        previous_end = entry.end;
    }
    true
}
