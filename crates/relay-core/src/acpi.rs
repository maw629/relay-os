use alloc::vec::Vec;

pub trait PhysicalMemory {
    fn read_exact(&self, physical: u64, output: &mut [u8]) -> Result<(), MemoryError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryError {
    OutOfRange,
    Overflow,
    Transport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcpiError {
    Truncated,
    BadSignature,
    BadLength,
    Checksum,
    UnsupportedPlatform,
    InvalidRange,
    Allocation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct McfgRegion {
    pub base: u64,
    pub segment: u16,
    pub bus_start: u8,
    pub bus_end: u8,
}

const MAX_TABLE_BYTES: usize = 64 * 1024;
const MAX_MAPPABLE_PHYSICAL: u64 = 0x7FFF_FFFF_FFFF;
const PAGE_BYTES: u64 = 4096;

pub fn parse_mcfg(memory: &impl PhysicalMemory, rsdp_phys: u64) -> Result<McfgRegion, AcpiError> {
    let (_, entries) = root_entries(memory, rsdp_phys)?;
    let mut found: Option<McfgRegion> = None;
    for entry in entries {
        if sdt_signature(memory, entry)? != *b"MCFG" {
            continue;
        }
        if found.is_some() {
            return Err(AcpiError::UnsupportedPlatform);
        }
        found = Some(validate_mcfg(memory, entry)?);
    }
    found.ok_or(AcpiError::UnsupportedPlatform)
}

pub fn dmar_present(memory: &impl PhysicalMemory, rsdp_phys: u64) -> Result<bool, AcpiError> {
    let (_, entries) = root_entries(memory, rsdp_phys)?;
    for entry in entries {
        if sdt_signature(memory, entry)? != *b"DMAR" {
            continue;
        }
        read_table(memory, entry)?;
        return Ok(true);
    }
    Ok(false)
}

fn root_entries(
    memory: &impl PhysicalMemory,
    rsdp_phys: u64,
) -> Result<(bool, Vec<u64>), AcpiError> {
    if rsdp_phys == 0 {
        return Err(AcpiError::InvalidRange);
    }
    let rsdp = read_exact_36(memory, rsdp_phys)?;
    if rsdp[0..8] != *b"RSD PTR " {
        return Err(AcpiError::BadSignature);
    }
    if !checksum_ok(&rsdp[0..20]) {
        return Err(AcpiError::Checksum);
    }
    if rsdp[15] >= 2 {
        if !checksum_ok(&rsdp) {
            return Err(AcpiError::Checksum);
        }
        let xsdt = le_u64(&rsdp[24..32]);
        if sdt_signature(memory, xsdt)? != *b"XSDT" {
            return Err(AcpiError::BadSignature);
        }
        Ok((true, sdt_entries(memory, xsdt, 8)?))
    } else {
        let rsdt = le_u32(&rsdp[16..20]) as u64;
        if sdt_signature(memory, rsdt)? != *b"RSDT" {
            return Err(AcpiError::BadSignature);
        }
        Ok((false, sdt_entries(memory, rsdt, 4)?))
    }
}

fn sdt_entries(
    memory: &impl PhysicalMemory,
    phys: u64,
    stride: usize,
) -> Result<Vec<u64>, AcpiError> {
    let header = read_exact_36(memory, phys)?;
    let total = le_u32(&header[4..8]) as usize;
    if !(36..=MAX_TABLE_BYTES).contains(&total) {
        return Err(AcpiError::BadLength);
    }
    if !(total - 36).is_multiple_of(stride) {
        return Err(AcpiError::BadLength);
    }
    let body = read_table(memory, phys)?;
    if !checksum_ok(&body) {
        return Err(AcpiError::Checksum);
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact((total - 36) / stride)
        .map_err(|_| AcpiError::Allocation)?;
    let mut offset = 36;
    while offset < total {
        let entry = if stride == 8 {
            le_u64(body.get(offset..offset + 8).ok_or(AcpiError::BadLength)?)
        } else {
            le_u32(body.get(offset..offset + 4).ok_or(AcpiError::BadLength)?) as u64
        };
        entries.push(entry);
        offset += stride;
    }
    Ok(entries)
}

fn sdt_signature(memory: &impl PhysicalMemory, phys: u64) -> Result<[u8; 4], AcpiError> {
    let header = read_exact_36(memory, phys)?;
    Ok([header[0], header[1], header[2], header[3]])
}

fn validate_mcfg(memory: &impl PhysicalMemory, phys: u64) -> Result<McfgRegion, AcpiError> {
    let body = read_table(memory, phys)?;
    let total = le_u32(body.get(4..8).ok_or(AcpiError::BadLength)?) as usize;
    if total != 60 || body.len() != total {
        return Err(AcpiError::UnsupportedPlatform);
    }
    if !checksum_ok(&body) {
        return Err(AcpiError::Checksum);
    }
    let base = le_u64(body.get(44..52).ok_or(AcpiError::BadLength)?);
    let segment = le_u16(body.get(52..54).ok_or(AcpiError::BadLength)?);
    let bus_start = *body.get(54).ok_or(AcpiError::BadLength)?;
    let bus_end = *body.get(55).ok_or(AcpiError::BadLength)?;
    if segment != 0 {
        return Err(AcpiError::UnsupportedPlatform);
    }
    if bus_start > bus_end {
        return Err(AcpiError::InvalidRange);
    }
    if base == 0 || !base.is_multiple_of(PAGE_BYTES) {
        return Err(AcpiError::InvalidRange);
    }
    let buses = (bus_end as u64 - bus_start as u64) + 1;
    let window = buses.checked_mul(1 << 20).ok_or(AcpiError::InvalidRange)?;
    let last = base
        .checked_add(window)
        .and_then(|end| end.checked_sub(1))
        .ok_or(AcpiError::InvalidRange)?;
    if last > MAX_MAPPABLE_PHYSICAL {
        return Err(AcpiError::InvalidRange);
    }
    Ok(McfgRegion {
        base,
        segment,
        bus_start,
        bus_end,
    })
}

fn read_exact_36(memory: &impl PhysicalMemory, phys: u64) -> Result<[u8; 36], AcpiError> {
    let mut bytes = [0; 36];
    memory
        .read_exact(phys, &mut bytes)
        .map_err(|_| AcpiError::Truncated)?;
    Ok(bytes)
}

fn read_table(memory: &impl PhysicalMemory, phys: u64) -> Result<Vec<u8>, AcpiError> {
    let header = read_exact_36(memory, phys)?;
    let total = le_u32(&header[4..8]) as usize;
    if !(36..=MAX_TABLE_BYTES).contains(&total) {
        return Err(AcpiError::BadLength);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total)
        .map_err(|_| AcpiError::Allocation)?;
    bytes.resize(total, 0);
    memory
        .read_exact(phys, &mut bytes)
        .map_err(|_| AcpiError::Truncated)?;
    Ok(bytes)
}

fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |acc, &byte| acc.wrapping_add(byte)) == 0
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}
