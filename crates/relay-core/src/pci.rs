use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PciAddress {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

pub trait PciConfig {
    fn read_u32(&self, address: PciAddress, offset: u16) -> Result<u32, PciError>;
    /// # Safety
    /// The caller must target a discovered device's validated ECAM
    /// offset and must have saved every register it mutates so the
    /// BAR probe's restore discipline holds.
    unsafe fn write_u32(
        &self,
        address: PciAddress,
        offset: u16,
        value: u32,
    ) -> Result<(), PciError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciError {
    OutOfRange,
    Transport,
    InvalidBar,
    NoXhci,
    MultipleXhci,
    UnsupportedPlatform,
    Allocation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarInfo {
    pub base: u64,
    pub size: u64,
    pub is_64: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciPciDevice {
    pub address: PciAddress,
    pub bar: BarInfo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciCaps {
    pub cap_length: u8,
    pub interface_version: u16,
    pub max_slots: u8,
    pub max_ports: u8,
    pub context_64: bool,
    pub addr_64: bool,
    pub scratchpad_count: u16,
    pub legacy_owned: bool,
    pub usb2_bdf_range: (u8, u8),
    pub usb3_bdf_range: (u8, u8),
}

pub use crate::acpi::McfgRegion;

const ECAM_OFFSET_MAX: u16 = 4096;
const COMMAND_MEM: u32 = 0x2;
const COMMAND_MASTER: u32 = 0x4;
const ALL_ONES: u32 = 0xFFFF_FFFF;
const XHCI_CLASS: u32 = 0x000C_0330;

pub fn ecam_address(
    region: &McfgRegion,
    address: PciAddress,
    offset: u16,
) -> Result<u64, PciError> {
    if address.segment != 0
        || address.device >= 32
        || address.function >= 8
        || offset >= ECAM_OFFSET_MAX
        || !offset.is_multiple_of(4)
        || address.bus < region.bus_start
        || address.bus > region.bus_end
    {
        return Err(PciError::OutOfRange);
    }
    let base = region.base;
    let bus_part = (address.bus as u64)
        .checked_mul(1 << 20)
        .ok_or(PciError::OutOfRange)?;
    let dev_part = (address.device as u64)
        .checked_mul(1 << 15)
        .ok_or(PciError::OutOfRange)?;
    let func_part = (address.function as u64)
        .checked_mul(1 << 12)
        .ok_or(PciError::OutOfRange)?;
    base.checked_add(bus_part)
        .and_then(|value| value.checked_add(dev_part))
        .and_then(|value| value.checked_add(func_part))
        .and_then(|value| value.checked_add(offset as u64))
        .ok_or(PciError::OutOfRange)
}

pub fn enumerate_functions(config: &impl PciConfig) -> Result<Vec<(PciAddress, u32)>, PciError> {
    let mut found = Vec::new();
    for bus in 0..=u8::MAX {
        for device in 0..32 {
            let first = PciAddress {
                segment: 0,
                bus,
                device,
                function: 0,
            };
            let vendor = match config.read_u32(first, 0x00) {
                Ok(value) => value & 0xFFFF,
                Err(PciError::OutOfRange) => continue,
                Err(error) => return Err(error),
            };
            if vendor == 0xFFFF {
                continue;
            }
            let header = config.read_u32(first, 0x0C).map_err(map_transport)?;
            let functions: u8 = if header & 0x0080_0000 != 0 { 8 } else { 1 };
            for function in 0..functions {
                let address = PciAddress {
                    segment: 0,
                    bus,
                    device,
                    function,
                };
                let id = config.read_u32(address, 0x00).map_err(map_transport)?;
                if id & 0xFFFF == 0xFFFF {
                    continue;
                }
                let class = config.read_u32(address, 0x08).map_err(map_transport)?;
                let code = ((class >> 24) & 0xFF) << 16
                    | ((class >> 16) & 0xFF) << 8
                    | ((class >> 8) & 0xFF);
                found.try_reserve(1).map_err(|_| PciError::Allocation)?;
                found.push((address, code));
            }
        }
    }
    Ok(found)
}

fn map_transport(_: PciError) -> PciError {
    PciError::Transport
}

pub fn find_xhci_controllers(config: &impl PciConfig) -> Result<Vec<PciAddress>, PciError> {
    let mut candidates = Vec::new();
    for (address, code) in &enumerate_functions(config)? {
        if *code != XHCI_CLASS {
            continue;
        }
        let header = config.read_u32(*address, 0x0C).map_err(map_transport)?;
        if (header >> 16) & 0xFF & 0x7F != 0 {
            continue;
        }
        candidates
            .try_reserve(1)
            .map_err(|_| PciError::Allocation)?;
        candidates.push(*address);
    }
    candidates.sort_by_key(|address| (address.bus, address.device, address.function));
    Ok(candidates)
}

pub fn find_xhci(config: &impl PciConfig) -> Result<PciAddress, PciError> {
    let mut candidates = find_xhci_controllers(config)?;
    match candidates.len() {
        0 => Err(PciError::NoXhci),
        1 => Ok(candidates.pop().unwrap()),
        _ => Err(PciError::MultipleXhci),
    }
}

/// M1 single-target rule: prefer the PCH xHCI at bus 0, device 0x14,
/// function 0 when it is among the sorted candidates, else fall back to
/// the lowest-BDF candidate.
///
/// Backed by Linux device-attachment evidence on the NUC: `lsusb -t` plus
/// the sysfs bus->PCI mapping show the Thunderbolt controller at
/// `00:0d.0` owns empty buses while the PCH controller at `00:14.0`
/// owns the keyboard and flash drive, so lowest-BDF-first picks the
/// wrong controller there. Task 13 enumeration remains the backstop for
/// bringing up secondary controllers.
pub fn prefer_pch_primary(candidates: &[PciAddress]) -> Option<PciAddress> {
    if candidates.is_empty() {
        return None;
    }
    candidates
        .iter()
        .find(|address| address.bus == 0 && address.device == 0x14 && address.function == 0)
        .copied()
        .or_else(|| candidates.first().copied())
}

pub fn probe_xhci_bar(config: &impl PciConfig, address: PciAddress) -> Result<BarInfo, PciError> {
    let command = config.read_u32(address, 0x04).map_err(map_transport)?;
    let bar0 = config.read_u32(address, 0x10).map_err(map_transport)?;
    let bar1 = config.read_u32(address, 0x14).map_err(map_transport)?;
    if bar0 & 0x1 != 0 {
        return Err(PciError::InvalidBar);
    }
    let is_64 = match (bar0 >> 1) & 0x3 {
        0b00 => false,
        0b10 => true,
        _ => return Err(PciError::InvalidBar),
    };
    unsafe {
        config
            .write_u32(address, 0x04, command & !(COMMAND_MEM | COMMAND_MASTER))
            .map_err(map_transport)?;
    }
    unsafe {
        config
            .write_u32(address, 0x10, ALL_ONES)
            .map_err(map_transport)?;
        if is_64 {
            config
                .write_u32(address, 0x14, ALL_ONES)
                .map_err(map_transport)?;
        }
    }
    let mask0 = config.read_u32(address, 0x10).map_err(map_transport)?;
    let mask1 = if is_64 {
        config.read_u32(address, 0x14).map_err(map_transport)?
    } else {
        0
    };
    unsafe {
        config
            .write_u32(address, 0x10, bar0)
            .map_err(map_transport)?;
        if is_64 {
            config
                .write_u32(address, 0x14, bar1)
                .map_err(map_transport)?;
        }
    }
    let mask: u64 = if is_64 {
        ((mask1 as u64) << 32) | (mask0 & !0xF) as u64
    } else {
        (mask0 & !0xF) as u64
    };
    if mask == 0 {
        return Err(PciError::InvalidBar);
    }
    let size = mask.wrapping_neg();
    if size == 0 || size & (size - 1) != 0 {
        return Err(PciError::InvalidBar);
    }
    let base: u64 = if is_64 {
        ((bar1 as u64) << 32) | (bar0 & !0xF) as u64
    } else {
        (bar0 & !0xF) as u64
    };
    if base == 0 || !base.is_multiple_of(size) {
        return Err(PciError::InvalidBar);
    }
    base.checked_add(size).ok_or(PciError::InvalidBar)?;
    unsafe {
        config
            .write_u32(address, 0x04, (command & !COMMAND_MASTER) | COMMAND_MEM)
            .map_err(map_transport)?;
    }
    Ok(BarInfo { base, size, is_64 })
}

pub fn decode_xhci_caps(header: &[u8; 32], ext_base: u64, ext: &[u8]) -> XhciCaps {
    let version = u16::from_le_bytes([header[2], header[3]]);
    let hcs1 = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let hcc = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
    let max_psa = (hcc >> 12) & 0xF;
    let scratchpad_count = if hcc & (1 << 9) != 0 {
        1u16 << max_psa.min(15)
    } else {
        0
    };
    let xecp = ((hcc >> 16) & 0xFFFF) as u64;
    let mut legacy_owned = false;
    let mut usb2 = (0u8, 0u8);
    let mut usb3 = (0u8, 0u8);
    if xecp != 0 {
        let mut offset = xecp * 4;
        for _ in 0..32 {
            let dword = read_ext(ext_base, ext, offset);
            // Extended-capability header per xHCI section 7: capability ID
            // in bits 7:0, next-capability stride in DWORDs in bits 15:8,
            // minor/major protocol revision in bits 23:16/31:24 for
            // Supported Protocol entries (Linux XHCI_EXT_CAPS_{ID,NEXT,VAL}
            // and the QEMU qemu-xhci model agree on this split).
            let next = ((dword >> 8) & 0xFF) as usize;
            let id = (dword & 0xFF) as u8;
            match id {
                1 => {
                    // USBLEGSUP occupies this same header DWORD: bit 16 is
                    // the BIOS-owned semaphore, bit 24 the OS-owned one.
                    if (dword >> 16) & 0x1 != 0 || (dword >> 24) & 0x1 != 0 {
                        legacy_owned = true;
                    }
                }
                2 => {
                    let major = ((dword >> 24) & 0xFF) as u8;
                    let port = read_ext(ext_base, ext, offset.saturating_add(8));
                    let range = ((port & 0xFF) as u8, ((port >> 8) & 0xFF) as u8);
                    if major == 2 && usb2 == (0, 0) {
                        usb2 = range;
                    } else if major >= 3 && usb3 == (0, 0) {
                        usb3 = range;
                    }
                }
                _ => {}
            }
            if next == 0 {
                break;
            }
            offset += (next as u64) * 4;
        }
    }
    XhciCaps {
        cap_length: header[0],
        interface_version: version,
        max_slots: (hcs1 & 0xFF) as u8,
        max_ports: ((hcs1 >> 24) & 0xFF) as u8,
        context_64: hcc & (1 << 2) != 0,
        addr_64: hcc & 0x1 != 0,
        scratchpad_count,
        legacy_owned,
        usb2_bdf_range: usb2,
        usb3_bdf_range: usb3,
    }
}

fn read_ext(ext_base: u64, ext: &[u8], offset: u64) -> u32 {
    let rel = match offset.checked_sub(ext_base) {
        Some(value) => value,
        None => return 0,
    };
    let rel = match usize::try_from(rel) {
        Ok(value) => value,
        Err(_) => return 0,
    };
    let end = match rel.checked_add(4) {
        Some(value) => value,
        None => return 0,
    };
    match ext.get(rel..end) {
        Some(bytes) => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        None => 0,
    }
}
