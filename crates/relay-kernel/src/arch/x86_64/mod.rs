pub mod dma;
pub mod exception_stubs;
pub mod exceptions;
pub mod memory;
pub mod mmio;

pub fn halt() -> ! {
    loop {
        // SAFETY: fatal kernel paths disable interrupts before halting so no interrupt can resume execution.
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// # Safety
/// Called once from kernel entry with interrupts disabled, before fault-prone runtime setup.
pub unsafe fn initialize() {
    // SAFETY: entry owns descriptor-table initialization before any interrupts are enabled.
    unsafe { exceptions::install() };
}
