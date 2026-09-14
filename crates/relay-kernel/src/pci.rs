use relay_abi::BootInfo;
use relay_core::{
    acpi::{McfgRegion, PhysicalMemory, dmar_present, parse_mcfg},
    dma::DmaError,
    mmio::MapError,
    pci::{
        BarInfo, PciAddress, PciConfig, PciError, XhciCaps, decode_xhci_caps, find_xhci,
        probe_xhci_bar,
    },
};

use crate::arch::x86_64::{dma, mmio};

struct KernelMem;

impl PhysicalMemory for KernelMem {
    fn read_exact(
        &self,
        physical: u64,
        output: &mut [u8],
    ) -> Result<(), relay_core::acpi::MemoryError> {
        use relay_core::acpi::MemoryError;
        let end = physical
            .checked_add(output.len() as u64)
            .ok_or(MemoryError::Overflow)?;
        if !output.is_empty() && end - 1 > 0x7FFF_FFFF_FFFF {
            return Err(MemoryError::OutOfRange);
        }
        let virt = physical
            .checked_add(crate::arch::x86_64::memory::PHYSICAL_MEMORY_OFFSET)
            .ok_or(MemoryError::Overflow)?;
        // SAFETY: bounds were checked against the direct-map ceiling and
        // firmware tables are mapped RAM; only reads are performed.
        let bytes = unsafe { core::slice::from_raw_parts(virt as *const u8, output.len()) };
        output.copy_from_slice(bytes);
        Ok(())
    }
}

struct EcamAccess {
    mapped: u64,
    region: McfgRegion,
}

impl EcamAccess {
    fn register(&self, address: PciAddress, offset: u16) -> Result<*mut u32, PciError> {
        let phys = relay_core::pci::ecam_address(&self.region, address, offset)?;
        let virt = self
            .mapped
            .checked_add(phys - self.region.base)
            .ok_or(PciError::OutOfRange)?;
        Ok(virt as *mut u32)
    }
}

impl PciConfig for EcamAccess {
    fn read_u32(&self, address: PciAddress, offset: u16) -> Result<u32, PciError> {
        let register = self.register(address, offset)?;
        // SAFETY: the register lies in the mapped ECAM window owned
        // exclusively by this accessor; volatile read has no side effects.
        Ok(unsafe { register.read_volatile() })
    }

    unsafe fn write_u32(
        &self,
        address: PciAddress,
        offset: u16,
        value: u32,
    ) -> Result<(), PciError> {
        let register = self.register(address, offset)?;
        // SAFETY: upheld by the trait contract — the caller probed a
        // discovered device at a validated offset with saved registers.
        unsafe { register.write_volatile(value) };
        Ok(())
    }
}

pub struct PlatformInfo {
    pub region: McfgRegion,
    pub xhci: PciAddress,
    pub bar: BarInfo,
    pub caps: XhciCaps,
    pub dmar: bool,
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum ProbeError {
    Acpi(relay_core::acpi::AcpiError),
    Pci(PciError),
    Map(MapError),
    Dma(DmaError),
}

impl ProbeError {
    pub fn status(&self) -> &'static str {
        match self {
            ProbeError::Acpi(_) => "bad-acpi",
            ProbeError::Pci(PciError::NoXhci)
            | ProbeError::Pci(PciError::MultipleXhci)
            | ProbeError::Pci(PciError::UnsupportedPlatform) => "unsupported-platform",
            ProbeError::Pci(_) => "bad-pci",
            ProbeError::Map(_) => "map-failed",
            ProbeError::Dma(_) => "dma-failed",
        }
    }
}

pub fn probe(boot_info: &BootInfo) -> Result<PlatformInfo, ProbeError> {
    let memory = KernelMem;
    let region = parse_mcfg(&memory, boot_info.acpi_rsdp_phys).map_err(ProbeError::Acpi)?;
    let buses = (region.bus_end as u64 - region.bus_start as u64) + 1;
    let ecam_len =
        usize::try_from(buses * (1 << 20)).map_err(|_| ProbeError::Map(MapError::InvalidRange))?;
    let ecam = mmio::map_uncached(region.base, ecam_len).map_err(ProbeError::Map)? as u64;
    let config = EcamAccess {
        mapped: ecam,
        region,
    };
    let xhci = find_xhci(&config).map_err(ProbeError::Pci)?;
    let bar = probe_xhci_bar(&config, xhci).map_err(ProbeError::Pci)?;
    let bar_len = usize::try_from(bar.size).map_err(|_| ProbeError::Map(MapError::InvalidRange))?;
    let bar_ptr = mmio::map_uncached(bar.base, bar_len).map_err(ProbeError::Map)?;
    let caps = snapshot_caps(bar_ptr);
    let dmar = dmar_present(&memory, boot_info.acpi_rsdp_phys).map_err(ProbeError::Acpi)?;
    let _ = dma::allocator();
    Ok(PlatformInfo {
        region,
        xhci,
        bar,
        caps,
        dmar,
    })
}

fn snapshot_caps(bar: *mut u8) -> XhciCaps {
    let mut header = [0; 32];
    let mut ext = [0; 256];
    // SAFETY: the BAR was just mapped uncached and exclusively for this
    // probe; only volatile reads are performed within the mapped prefix.
    // Reads are DWORD-wide and DWORD-aligned: the capability registers are
    // 32-bit and sub-DWORD reads do not return the upper bytes on the QEMU
    // xHCI model (its capability handler only answers aligned DWORD reads).
    unsafe {
        let mut word = 0;
        while word < 8 {
            let value = (bar.add(word * 4) as *mut u32).read_volatile();
            header[word * 4..word * 4 + 4].copy_from_slice(&value.to_le_bytes());
            word += 1;
        }
        word = 0;
        while word < 64 {
            let value = (bar.add(word * 4) as *mut u32).read_volatile();
            ext[word * 4..word * 4 + 4].copy_from_slice(&value.to_le_bytes());
            word += 1;
        }
    }
    decode_xhci_caps(&header, &ext)
}
