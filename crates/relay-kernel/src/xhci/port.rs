#![allow(dead_code)]
// Task 4 uses the reset/status bits and helpers; allow dead here so the
// Task 3 boot gate stays warning-free.

use relay_core::xhci::{UsbSpeed, XhciError};

pub const PORT_CCS: u32 = 1 << 0;
pub const PORT_PED: u32 = 1 << 1;
pub const PORT_PR: u32 = 1 << 4;
pub const PORT_PRSC: u32 = 1 << 5;
pub const PORT_PP: u32 = 1 << 9;

pub fn portsc_offset(op_base: u32, port: u8) -> Result<u32, XhciError> {
    if port == 0 {
        return Err(XhciError::NoPorts);
    }
    let index = port as u32 - 1;
    index
        .checked_mul(relay_core::xhci::OP_PORT_STRIDE)
        .and_then(|stride| relay_core::xhci::OP_PORT_BASE.checked_add(stride))
        .and_then(|base| base.checked_add(0))
        .and_then(|relative| op_base.checked_add(relative))
        .filter(|_| index < 255)
        .ok_or(XhciError::NoPorts)
}

pub fn decode_speed(portsc: u32) -> Result<UsbSpeed, XhciError> {
    UsbSpeed::from_portsc(portsc)
}
