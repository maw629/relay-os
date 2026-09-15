pub mod controller;
pub mod port;
pub mod transfer;

use relay_core::xhci::{Mmio, XhciError};

pub struct KernelMmio {
    base: *mut u8,
    len: usize,
}

impl KernelMmio {
    /// # Safety
    /// `base` must be a leaked UC BAR mapping of `len` bytes owned
    /// exclusively by this accessor for the controller lifetime.
    pub unsafe fn new(base: *mut u8, len: usize) -> Self {
        Self { base, len }
    }

    fn check(&self, offset: u32, access: usize) -> Result<usize, XhciError> {
        let end = (offset as usize)
            .checked_add(access)
            .ok_or(XhciError::InvalidRegister)?;
        if end > self.len {
            return Err(XhciError::InvalidRegister);
        }
        Ok(offset as usize)
    }
}

impl Mmio for KernelMmio {
    fn read32(&self, offset: u32) -> Result<u32, XhciError> {
        let relative = self.check(offset, 4)?;
        if !offset.is_multiple_of(4) {
            return Err(XhciError::InvalidRegister);
        }
        // SAFETY: constructed from a leaked UC mapping; DWORD-aligned
        // volatile read within the checked range has no side effects
        // beyond the device read itself.
        Ok(unsafe { (self.base.add(relative) as *const u32).read_volatile() })
    }

    fn write32(&mut self, offset: u32, value: u32) -> Result<(), XhciError> {
        let relative = self.check(offset, 4)?;
        if !offset.is_multiple_of(4) {
            return Err(XhciError::InvalidRegister);
        }
        // SAFETY: same mapping as reads; volatile write targets a
        // validated operational register offset.
        unsafe { (self.base.add(relative) as *mut u32).write_volatile(value) };
        Ok(())
    }
}
