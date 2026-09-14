#![no_std]
#![no_main]

extern crate alloc;

mod allocator;
mod arch;
mod console;
mod entry;
mod pci;
mod serial;
mod xhci;

#[global_allocator]
static ALLOCATOR: allocator::BumpAllocator = allocator::BumpAllocator::new();

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    console::panic_write(info);
    arch::x86_64::halt();
}

#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
/// # Safety
/// The loader must have exited UEFI boot services and supplied an identity-mapped,
/// initialized `BootInfo` conforming to the versioned ABI.
pub unsafe extern "C" fn _start(boot_info: *const relay_abi::BootInfo) -> ! {
    // SAFETY: the loader transfers control only through the documented BootInfo ABI.
    unsafe { entry::enter(boot_info) }
}
