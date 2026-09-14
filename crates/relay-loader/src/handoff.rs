use core::mem;

use relay_abi::{BootInfo, FramebufferInfo};
use uefi::{
    boot,
    mem::memory_map::{MemoryMap as UefiMemoryMap, MemoryType},
    proto::console::gop::{GraphicsOutput, PixelFormat},
    system,
    table::cfg::ConfigTableEntry,
};

use crate::{
    cpu::firmware_paging_depth,
    files,
    memory::{
        self, BootData, PHYSICAL_MEMORY_OFFSET, allocate_boot_data, allocate_transition_page,
        allocate_zeroed, load_kernel,
    },
    paging::PageTables,
    parse_config, parse_load_plan,
};

const STACK_PAGES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandoffError {
    Files,
    Config,
    Elf,
    Memory,
    Paging,
    Graphics,
    Acpi,
}

pub fn boot() -> Result<(), HandoffError> {
    let config = files::read_config().map_err(|_| HandoffError::Files)?;
    let root_guid = parse_config(&config)
        .map_err(|_| HandoffError::Config)?
        .root_guid;
    drop(config);

    let image = files::read_kernel().map_err(|_| HandoffError::Files)?;
    let plan = parse_load_plan(&image).map_err(|_| HandoffError::Elf)?;
    let kernel = load_kernel(&plan, &image).map_err(|_| HandoffError::Memory)?;
    drop(plan);
    drop(image);

    let framebuffer = capture_framebuffer()?;
    let acpi_rsdp_phys = capture_rsdp()?;
    let stack = allocate_zeroed(STACK_PAGES).map_err(|_| HandoffError::Memory)?;
    let boot_data = allocate_boot_data().map_err(|_| HandoffError::Memory)?;
    let transition_page = allocate_transition_page().map_err(|_| HandoffError::Memory)?;
    let mut tables = PageTables::new(firmware_paging_depth()).map_err(|_| HandoffError::Paging)?;
    tables
        .map_transition_page(transition_page)
        .map_err(|_| HandoffError::Paging)?;
    tables
        .map_identity_first_4g()
        .map_err(|_| HandoffError::Paging)?;
    tables
        .map_identity_allocation(stack)
        .map_err(|_| HandoffError::Paging)?;
    tables
        .map_identity_allocation(boot_data)
        .map_err(|_| HandoffError::Paging)?;

    let direct_map = boot::memory_map(MemoryType::LOADER_DATA).map_err(|_| HandoffError::Memory)?;
    for descriptor in direct_map.entries() {
        if is_direct_mapped(descriptor.ty) {
            let end = descriptor
                .phys_start
                .checked_add(
                    descriptor
                        .page_count
                        .checked_mul(memory::PAGE_SIZE)
                        .ok_or(HandoffError::Memory)?,
                )
                .ok_or(HandoffError::Memory)?;
            tables
                .map_usable_ram(descriptor.phys_start, end)
                .map_err(|_| HandoffError::Paging)?;
        }
    }
    drop(direct_map);

    for segment in &kernel.segments {
        tables
            .map_kernel_segment(
                segment.page_virtual_start,
                segment.physical_start,
                segment.pages,
                segment.flags,
            )
            .map_err(|_| HandoffError::Paging)?;
    }
    tables
        .map_framebuffer(framebuffer.physical_base, framebuffer.byte_len)
        .map_err(|_| HandoffError::Paging)?;
    let stack_top = stack.end().map_err(|_| HandoffError::Memory)?;
    let cr3 = tables.root_physical_address();
    let boot_info = boot_data.physical_start as *mut BootData;
    unsafe {
        (*boot_info).info.magic = relay_abi::BOOT_INFO_MAGIC;
        (*boot_info).info.abi_version = relay_abi::BOOT_ABI_VERSION;
        (*boot_info).info.struct_size = core::mem::size_of::<BootInfo>() as u32;
        (*boot_info).info.framebuffer = framebuffer;
        (*boot_info).info.root_partition_guid = root_guid;
        (*boot_info).info.physical_memory_offset = PHYSICAL_MEMORY_OFFSET;
        (*boot_info).info.acpi_rsdp_phys = acpi_rsdp_phys;
    }
    // SAFETY: the transition page is currently UEFI-mapped at its physical address and remains
    // executable at the same identity address after it loads the new CR3.
    let transition: unsafe extern "sysv64" fn(*const BootInfo, u64, u64, u64) -> ! =
        unsafe { mem::transmute(transition_page.physical_start as usize) };

    // All UEFI protocol handles and heap-backed values are released before this call.
    // The final map is the only firmware data retained past this point.
    let mut final_map = unsafe { boot::exit_boot_services(Some(MemoryType::LOADER_DATA)) };
    // No allocation or UEFI access is possible after exit. The non-returning transition leaves
    // its loader-owned tables and final map allocated for the kernel handoff.
    let normalization = unsafe { memory::normalize_final_map(boot_info, &mut final_map) };
    match normalization {
        Ok(()) => {}
        Err(memory::MemoryError::TooManyRegions) => halt(),
        Err(memory::MemoryError::UnsortedOrOverlapping) => halt(),
        Err(memory::MemoryError::DescriptorArithmetic) => halt(),
        Err(_) => halt(),
    }
    unsafe { transition(boot_info.cast::<BootInfo>(), cr3, stack_top, kernel.entry) }
}

fn halt() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// RAM the kernel reaches through the direct physical map: usable memory,
/// the loader's own retained pages (page tables the runtime MMIO installer
/// walks, boot data, kernel image), boot-services memory freed by
/// ExitBootServices, and the firmware ACPI/runtime tables the platform
/// probe reads. MMIO, NVS, reserved, and unusable ranges stay unmapped so
/// stray direct-map accesses fault instead of silently hitting devices.
fn is_direct_mapped(ty: MemoryType) -> bool {
    matches!(
        ty,
        MemoryType::CONVENTIONAL
            | MemoryType::LOADER_DATA
            | MemoryType::BOOT_SERVICES_CODE
            | MemoryType::BOOT_SERVICES_DATA
            | MemoryType::RUNTIME_SERVICES_CODE
            | MemoryType::RUNTIME_SERVICES_DATA
            | MemoryType::ACPI_RECLAIM
    )
}

fn capture_framebuffer() -> Result<FramebufferInfo, HandoffError> {
    let handle =
        boot::get_handle_for_protocol::<GraphicsOutput>().map_err(|_| HandoffError::Graphics)?;
    let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle)
        .map_err(|_| HandoffError::Graphics)?;
    let info = gop.current_mode_info();
    let (width, height) = info.resolution();
    let stride = info.stride();
    let (pixel_format, red_mask, green_mask, blue_mask, reserved_mask) = match info.pixel_format() {
        PixelFormat::Rgb => (0, 0, 0, 0, 0),
        PixelFormat::Bgr => (1, 0, 0, 0, 0),
        PixelFormat::Bitmask => {
            let mask = info.pixel_bitmask().ok_or(HandoffError::Graphics)?;
            (2, mask.red, mask.green, mask.blue, mask.reserved)
        }
        PixelFormat::BltOnly => return Err(HandoffError::Graphics),
    };
    let mut buffer = gop.frame_buffer();
    let byte_len = u64::try_from(buffer.size()).map_err(|_| HandoffError::Graphics)?;
    Ok(FramebufferInfo {
        physical_base: buffer.as_mut_ptr() as u64,
        byte_len,
        width: u32::try_from(width).map_err(|_| HandoffError::Graphics)?,
        height: u32::try_from(height).map_err(|_| HandoffError::Graphics)?,
        stride_pixels: u32::try_from(stride).map_err(|_| HandoffError::Graphics)?,
        bytes_per_pixel: 4,
        pixel_format,
        red_mask,
        green_mask,
        blue_mask,
        reserved_mask,
    })
}

fn capture_rsdp() -> Result<u64, HandoffError> {
    system::with_config_table(|entries| {
        let address = entries
            .iter()
            .find(|entry| entry.guid == ConfigTableEntry::ACPI2_GUID)
            .or_else(|| {
                entries
                    .iter()
                    .find(|entry| entry.guid == ConfigTableEntry::ACPI_GUID)
            })
            .map(|entry| entry.address as u64)
            .ok_or(HandoffError::Acpi)?;
        (address != 0).then_some(address).ok_or(HandoffError::Acpi)
    })
}
