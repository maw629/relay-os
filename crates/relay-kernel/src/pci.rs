use alloc::vec::Vec;
use relay_abi::BootInfo;
use relay_core::{
    acpi::{McfgRegion, PhysicalMemory, dmar_present, parse_mcfg},
    dma::DmaError,
    mmio::MapError,
    pci::{
        BarInfo, PciAddress, PciConfig, PciError, XhciCaps, decode_xhci_caps,
        find_xhci_controllers, prefer_pch_primary, probe_xhci_bar,
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
        let len = output.len();
        if len == 0 {
            return Ok(());
        }
        let _end = physical
            .checked_add(len as u64)
            .ok_or(MemoryError::Overflow)?;
        let window = mmio::map_uncached(physical, len).map_err(|err| match err {
            MapError::InvalidRange => MemoryError::OutOfRange,
            MapError::AlreadyMapped | MapError::NoMemory | MapError::UnsupportedDepth => {
                MemoryError::Transport
            }
        })?;
        // SAFETY: `window` is a fresh UC mapping owned exclusively by this
        // read; `cover_range` inside the mapper bounded the range, `len > 0`
        // was checked above, and the window is leaked per the leak-only
        // discipline so it stays valid for the copy.
        unsafe { core::ptr::copy_nonoverlapping(window as *const u8, output.as_mut_ptr(), len) };
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
    pub xhci_all: Vec<PciAddress>,
    pub bar: BarInfo,
    pub caps: XhciCaps,
    pub xecp: u16,
    pub dmar: bool,
    pub snapshots: Vec<CandidateSnapshot>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateSnapshot {
    pub address: PciAddress,
    pub bar: BarInfo,
    pub caps: XhciCaps,
    pub xecp: u16,
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
    let candidates = find_xhci_controllers(&config).map_err(ProbeError::Pci)?;
    if candidates.is_empty() {
        return Err(ProbeError::Pci(PciError::NoXhci));
    }
    let mut snapshots = Vec::new();
    for candidate in &candidates {
        snapshots
            .try_reserve(1)
            .map_err(|_| ProbeError::Pci(PciError::Allocation))?;
        let bar = match probe_xhci_bar(&config, *candidate) {
            Ok(bar) => bar,
            Err(error) => {
                let failure = ProbeError::Pci(error);
                let line = alloc::format!(
                    "[relay] phase=platform-probe-candidate status={} detail={:?} bdf={:02x}:{:02x}.{}\n",
                    failure.status(),
                    failure,
                    candidate.bus,
                    candidate.device,
                    candidate.function,
                );
                crate::console::write(line.as_bytes());
                return Err(failure);
            }
        };
        let bar_len = match usize::try_from(bar.size) {
            Ok(len) => len,
            Err(_) => {
                let failure = ProbeError::Map(MapError::InvalidRange);
                let line = alloc::format!(
                    "[relay] phase=platform-probe-candidate status={} detail={:?} bdf={:02x}:{:02x}.{}\n",
                    failure.status(),
                    failure,
                    candidate.bus,
                    candidate.device,
                    candidate.function,
                );
                crate::console::write(line.as_bytes());
                return Err(failure);
            }
        };
        let bar_ptr = match mmio::map_uncached(bar.base, bar_len) {
            Ok(ptr) => ptr,
            Err(error) => {
                let failure = ProbeError::Map(error);
                let line = alloc::format!(
                    "[relay] phase=platform-probe-candidate status={} detail={:?} bdf={:02x}:{:02x}.{}\n",
                    failure.status(),
                    failure,
                    candidate.bus,
                    candidate.device,
                    candidate.function,
                );
                crate::console::write(line.as_bytes());
                return Err(failure);
            }
        };
        match snapshot_caps(bar_ptr, bar_len) {
            Ok((caps, xecp)) => {
                snapshots.push(CandidateSnapshot {
                    address: *candidate,
                    bar,
                    caps,
                    xecp,
                });
            }
            Err(error) => {
                let line = alloc::format!(
                    "[relay] phase=platform-probe-candidate status={} detail={:?} bdf={:02x}:{:02x}.{}\n",
                    error.status(),
                    error,
                    candidate.bus,
                    candidate.device,
                    candidate.function,
                );
                crate::console::write(line.as_bytes());
                return Err(error);
            }
        };
    }
    let xhci = prefer_pch_primary(&candidates).ok_or(ProbeError::Pci(PciError::NoXhci))?;
    let primary = snapshots
        .iter()
        .find(|snapshot| snapshot.address == xhci)
        .copied()
        .ok_or(ProbeError::Pci(PciError::NoXhci))?;
    let bar = primary.bar;
    let caps = primary.caps;
    let xecp = primary.xecp;
    let dmar = dmar_present(&memory, boot_info.acpi_rsdp_phys).map_err(ProbeError::Acpi)?;
    let _ = dma::allocator();
    Ok(PlatformInfo {
        region,
        xhci,
        xhci_all: candidates,
        bar,
        caps,
        xecp,
        dmar,
        snapshots,
    })
}

fn snapshot_caps(bar: *mut u8, bar_len: usize) -> Result<(XhciCaps, u16), ProbeError> {
    let mut header = [0; 32];
    // Up-to-4 KiB heap window bounded by the mapped BAR length: real Intel
    // controllers place extended capabilities beyond 256 B, while the
    // existing OOB-zero termination plus the 32-iteration cap in
    // `decode_xhci_caps` keeps the parse bounded for any xECP value.
    let window = core::cmp::min(4096, bar_len);
    let mut ext = Vec::new();
    ext.try_reserve(window)
        .map_err(|_| ProbeError::Map(MapError::NoMemory))?;
    ext.resize(window, 0);
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
        while word < window / 4 {
            let value = (bar.add(word * 4) as *mut u32).read_volatile();
            ext[word * 4..word * 4 + 4].copy_from_slice(&value.to_le_bytes());
            word += 1;
        }
    }
    let hcc = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
    let xecp = ((hcc >> 16) & 0xFFFF) as u16;
    Ok((decode_xhci_caps(&header, &ext), xecp))
}
