# Task 11 Platform Discovery And DMA Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a host-tested ACPI/PCI/DMA discovery layer plus thin kernel adapters and a diagnostic boot probe that records the xHCI platform assumptions.

**Architecture:** Pure `no_std` parsing and math in `relay-core` over injected traits (`PhysicalMemory`, `PciConfig`, `FrameSource`) with fakes in test support; privileged glue (CR3/CR4 reads, PTE install, ECAM MMIO, frame allocation) isolated in `relay-kernel` adapters; the boot probe prints stable markers and halts on failure; QEMU gains an explicit `qemu-xhci` device so the strict probe keeps the existing boot gate green.

**Tech Stack:** Rust 1.98.1, edition 2024, `no_std` plus `alloc`, existing `relay-core::{memory}`, `relay-abi::BootInfo`, `pci_types 0.10.1` definitions only, QEMU q35/OVMF, raw x86_64 PTE constants mirroring `relay-loader/src/paging.rs`.

**Spec:** `docs/superpowers/specs/2026-09-13-task-11-platform-discovery-design.md`

## Global Constraints

- `relay-core` remains `#![no_std]`; no unsafe code in new core modules; host filesystem/process APIs stay in integration-test support only.
- Every unsafe block lives in kernel adapters and documents the invariant it upholds (live-table exclusivity, direct-map bounds, single-core no-concurrent-mapper).
- All table bytes, lengths, addresses, shifts, and arithmetic are checked; malformed firmware returns typed errors and never panics; dynamic-slice indexing uses `.get()` with mapped errors, never direct indexing.
- MCFG policy is segment-0-only: anything other than exactly one segment-0 entry is `UnsupportedPlatform`, never silently accepted.
- DMA allocation is leak-only: no `free`, no reclaiming `Drop`; handed-out allocations live until reboot.
- BAR/command registers are restored on every probe return path, including error returns; bus mastering stays off until Task 12.
- If VT-d translation blocks DMA on the NUC, stop and get explicit design approval before adding an identity-mapped DMA domain or requiring VT-d disabled in firmware.
- Maintain `cargo fmt --all --check`, workspace Clippy with `-D warnings`, and locked workspace tests.

---

## File Structure

```text
crates/relay-core/src/acpi.rs             RSDP/XSDT/RSDT/MCFG parsing over PhysicalMemory
crates/relay-core/src/pci.rs              PciAddress, PciConfig, ECAM walk, xHCI match, BAR probe, caps decode
crates/relay-core/src/dma.rs              FrameSource, BumpDmaAllocator, DmaLayout/Allocator/Allocation/DmaError
crates/relay-core/src/mmio.rs             pure paging math: indices, canonical, cover range, flags, MapError
crates/relay-core/src/lib.rs              add pub mod acpi/pci/dma/mmio across tasks
crates/relay-core/tests/support/vec_memory.rs
                                          Vec-backed PhysicalMemory fake
crates/relay-core/tests/support/mod.rs    expose vec_memory
crates/relay-core/tests/acpi.rs           table-validation suite
crates/relay-core/tests/pci.rs            enumeration and BAR-probe suite (inline RecordingPciConfig fake)
crates/relay-core/tests/dma.rs            layout/allocation suite (inline VecFrames fake)
crates/relay-core/tests/mmio.rs           paging-math suite
crates/relay-kernel/src/arch/x86_64/mmio.rs
                                          runtime 4K UC installer glue
crates/relay-core/../relay-kernel/src/arch/x86_64/dma.rs
                                          KernelDma FrameSource over the static frame allocator
crates/relay-kernel/src/arch/x86_64/memory.rs
                                          add frame accessor + bounded direct-slice helper
crates/relay-kernel/src/pci.rs            EcamAccess, KernelMem, caps snapshot, probe() orchestrator
crates/relay-kernel/src/entry.rs          run probe after heap init, halt with marker on failure
docs/acceptance/nuc-m1.md                 Task 11 probe table (QEMU-observed now, NUC columns pending)
xtask/src/qemu.rs                         add -device qemu-xhci,p2=2,p3=2 to boot topology
```

Deviations from the milestone file list, with rationale: `mmio.rs` pure math
lives in core (kernel bins set `test = false`, so host coverage is only
possible in core); `entry.rs` is modified instead of `main.rs` because the
boot flow lives there (`main.rs` just jumps to `entry::enter`); `qemu.rs`
gains the xHCI device because the strict halt-on-`NoXhci` probe would
otherwise break the existing Task 4 boot gate, which runs without any USB
controller today.

### Task 1: ACPI MCFG Parsing Over Injected Memory

**Files:**
- Create: `crates/relay-core/src/acpi.rs`
- Modify: `crates/relay-core/src/lib.rs`
- Create: `crates/relay-core/tests/support/vec_memory.rs`
- Modify: `crates/relay-core/tests/support/mod.rs`
- Test: `crates/relay-core/tests/acpi.rs`

**Interfaces:**
- Consumes: nothing (first Task 11 module).
- Produces: `PhysicalMemory`, `MemoryError`, `AcpiError`, `McfgRegion`, `parse_mcfg`, `dmar_present` for Task 4; `VecMemory` test fake for Tasks 1-3 test support.

- [ ] **Step 1: Write the failing table tests**

Create `crates/relay-core/tests/support/vec_memory.rs`:

```rust
use relay_core::acpi::{MemoryError, PhysicalMemory};

pub struct VecMemory {
    base: u64,
    bytes: Vec<u8>,
}

impl VecMemory {
    pub fn new(base: u64, bytes: Vec<u8>) -> Self {
        Self { base, bytes }
    }

    pub fn write_at(&mut self, physical: u64, data: &[u8]) {
        let offset = (physical - self.base) as usize;
        self.bytes[offset..offset + data.len()].copy_from_slice(data);
    }
}

impl PhysicalMemory for VecMemory {
    fn read_exact(&self, physical: u64, output: &mut [u8]) -> Result<(), MemoryError> {
        let offset = physical.checked_sub(self.base).ok_or(MemoryError::OutOfRange)? as usize;
        let end = offset.checked_add(output.len()).ok_or(MemoryError::Overflow)?;
        let src = self.bytes.get(offset..end).ok_or(MemoryError::OutOfRange)?;
        output.copy_from_slice(src);
        Ok(())
    }
}
```

Add `pub mod vec_memory;` to `crates/relay-core/tests/support/mod.rs`.

Create `crates/relay-core/tests/acpi.rs` with `mod support;` and this exact
content (helpers first, then the matrix):

```rust
mod support;

use relay_core::acpi::{AcpiError, McfgRegion, dmar_present, parse_mcfg};
use support::vec_memory::VecMemory;

const RSDP_PHYS: u64 = 0x1000;
const XSDT_PHYS: u64 = 0x2000;
const MCFG_PHYS: u64 = 0x3000;
const DMAR_PHYS: u64 = 0x4000;

fn fix_checksum(bytes: &mut [u8], at: usize) {
    bytes[at] = 0;
    let sum: u8 = bytes.iter().fold(0, |acc, &b| acc.wrapping_add(b));
    bytes[at] = 0u8.wrapping_sub(sum);
}

fn sdt_header(sig: &[u8; 4], total_len: u32) -> Vec<u8> {
    let mut header = vec![0; 36];
    header[0..4].copy_from_slice(sig);
    header[4..8].copy_from_slice(&total_len.to_le_bytes());
    header[8] = 1;
    header[10..16].copy_from_slice(b"RELAY ");
    fix_checksum(&mut header, 9);
    header
}

fn rsdp_v2(xsdt: u64) -> [u8; 36] {
    let mut rsdp = [0; 36];
    rsdp[0..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(b"RELAY ");
    rsdp[15] = 2;
    rsdp[20..24].copy_from_slice(&36u32.to_le_bytes());
    rsdp[24..32].copy_from_slice(&xsdt.to_le_bytes());
    fix_checksum(&mut rsdp[0..20], 8);
    fix_checksum(&mut rsdp, 32);
    rsdp
}

fn mcfg_bytes(base: u64, segment: u16, bus_start: u8, bus_end: u8) -> Vec<u8> {
    let mut body = vec![0; 16];
    body[0..8].copy_from_slice(&base.to_le_bytes());
    body[8..10].copy_from_slice(&segment.to_le_bytes());
    body[10] = bus_start;
    body[11] = bus_end;
    let mut table = sdt_header(b"MCFG", 60);
    table[36..44].copy_from_slice(&[0; 8]);
    table.extend_from_slice(&body);
    fix_checksum(&mut table, 9);
    table
}

fn image(entries: &[(u64, Vec<u8>)]) -> VecMemory {
    let mut bytes = vec![0; 0x5000];
    for (phys, table) in entries {
        let offset = (*phys - RSDP_PHYS) as usize;
        bytes[offset..offset + table.len()].copy_from_slice(table);
    }
    VecMemory::new(RSDP_PHYS, bytes)
}

fn good_image() -> VecMemory {
    let mcfg = mcfg_bytes(0xE000_0000, 0, 0, 255);
    let mut xsdt = sdt_header(b"XSDT", 44);
    xsdt.extend_from_slice(&MCFG_PHYS.to_le_bytes());
    fix_checksum(&mut xsdt, 9);
    image(&[(RSDP_PHYS, rsdp_v2(XSDT_PHYS).to_vec()), (XSDT_PHYS, xsdt), (MCFG_PHYS, mcfg)])
}

#[test]
fn mcfg_parses_known_good_vector() {
    let memory = good_image();
    assert_eq!(
        parse_mcfg(&memory, RSDP_PHYS),
        Ok(McfgRegion { base: 0xE000_0000, segment: 0, bus_start: 0, bus_end: 255 })
    );
    assert_eq!(dmar_present(&memory, RSDP_PHYS), Ok(false));
}

#[test]
fn rsdp_rejects_bad_signature() {
    let mut memory = good_image();
    memory.write_at(RSDP_PHYS, b"BAD PTR ");
    assert_eq!(parse_mcfg(&memory, RSDP_PHYS), Err(AcpiError::BadSignature));
}

#[test]
fn rsdp_rejects_bad_checksum() {
    let mut memory = good_image();
    memory.write_at(RSDP_PHYS + 10, b"X");
    assert_eq!(parse_mcfg(&memory, RSDP_PHYS), Err(AcpiError::Checksum));
}

#[test]
fn xsdt_rejects_bad_checksum() {
    let mut memory = good_image();
    memory.write_at(XSDT_PHYS + 10, b"X");
    assert_eq!(parse_mcfg(&memory, RSDP_PHYS), Err(AcpiError::Checksum));
}

#[test]
fn truncated_table_is_truncated() {
    let small = VecMemory::new(RSDP_PHYS, memory_bytes_truncated());
    assert_eq!(parse_mcfg(&small, RSDP_PHYS), Err(AcpiError::Truncated));
}

#[test]
fn missing_mcfg_is_unsupported_and_dmar_reports_true() {
    let mut dmar = sdt_header(b"DMAR", 48);
    dmar.extend_from_slice(&[0x11; 12]);
    fix_checksum(&mut dmar, 9);
    let mut xsdt = sdt_header(b"XSDT", 44);
    xsdt.extend_from_slice(&DMAR_PHYS.to_le_bytes());
    fix_checksum(&mut xsdt, 9);
    let memory = image(&[(RSDP_PHYS, rsdp_v2(XSDT_PHYS).to_vec()), (XSDT_PHYS, xsdt), (DMAR_PHYS, dmar)]);
    assert_eq!(parse_mcfg(&memory, RSDP_PHYS), Err(AcpiError::UnsupportedPlatform));
    assert_eq!(dmar_present(&memory, RSDP_PHYS), Ok(true));
}

#[test]
fn multi_entry_mcfg_is_unsupported() {
    let mut table = sdt_header(b"MCFG", 76);
    table[36..44].copy_from_slice(&[0; 8]);
    table.extend_from_slice(&mcfg_bytes(0xE000_0000, 0, 0, 127)[44..60]);
    table.extend_from_slice(&mcfg_bytes(0xF000_0000, 0, 128, 255)[44..60]);
    fix_checksum(&mut table, 9);
    let mut xsdt = sdt_header(b"XSDT", 44);
    xsdt.extend_from_slice(&MCFG_PHYS.to_le_bytes());
    fix_checksum(&mut xsdt, 9);
    let memory = image(&[(RSDP_PHYS, rsdp_v2(XSDT_PHYS).to_vec()), (XSDT_PHYS, xsdt), (MCFG_PHYS, table)]);
    assert_eq!(parse_mcfg(&memory, RSDP_PHYS), Err(AcpiError::UnsupportedPlatform));
}

#[test]
fn nonzero_segment_is_unsupported() {
    let mcfg = mcfg_bytes(0xE000_0000, 1, 0, 255);
    let mut xsdt = sdt_header(b"XSDT", 44);
    xsdt.extend_from_slice(&MCFG_PHYS.to_le_bytes());
    fix_checksum(&mut xsdt, 9);
    let memory = image(&[(RSDP_PHYS, rsdp_v2(XSDT_PHYS).to_vec()), (XSDT_PHYS, xsdt), (MCFG_PHYS, mcfg)]);
    assert_eq!(parse_mcfg(&memory, RSDP_PHYS), Err(AcpiError::UnsupportedPlatform));
}

#[test]
fn bad_mcfg_windows_are_invalid_range() {
    for (base, start, end) in [(0u64, 0u8, 255u8), (0x1234u64, 0, 0), (0x7FFF_FFFF_F000u64, 0, 255), (0xE000_0000u64, 5, 4)] {
        let mcfg = mcfg_bytes(base, 0, start, end);
        let mut xsdt = sdt_header(b"XSDT", 44);
        xsdt.extend_from_slice(&MCFG_PHYS.to_le_bytes());
        fix_checksum(&mut xsdt, 9);
        let memory = image(&[(RSDP_PHYS, rsdp_v2(XSDT_PHYS).to_vec()), (XSDT_PHYS, xsdt), (MCFG_PHYS, mcfg)]);
        assert_eq!(parse_mcfg(&memory, RSDP_PHYS), Err(AcpiError::InvalidRange), "base={base:#x}");
    }
    assert_eq!(parse_mcfg(&good_image(), 0), Err(AcpiError::InvalidRange));
}

fn memory_bytes_truncated() -> Vec<u8> {
    let full = good_image_bytes();
    full[..0x2008].to_vec()
}

fn good_image_bytes() -> Vec<u8> {
    let mcfg = mcfg_bytes(0xE000_0000, 0, 0, 255);
    let mut xsdt = sdt_header(b"XSDT", 44);
    xsdt.extend_from_slice(&MCFG_PHYS.to_le_bytes());
    fix_checksum(&mut xsdt, 9);
    let mut bytes = vec![0; 0x5000];
    bytes[0..36].copy_from_slice(&rsdp_v2(XSDT_PHYS));
    let xoff = (XSDT_PHYS - RSDP_PHYS) as usize;
    bytes[xoff..xoff + xsdt.len()].copy_from_slice(&xsdt);
    let moff = (MCFG_PHYS - RSDP_PHYS) as usize;
    bytes[moff..moff + mcfg.len()].copy_from_slice(&mcfg);
    bytes
}
```

- [ ] **Step 2: Run tests and verify failure**

Run: `cargo test -p relay-core --test acpi --locked`

Expected: FAIL because `relay_core::acpi`, `PhysicalMemory`, and
`VecMemory` are absent.

- [ ] **Step 3: Implement `acpi.rs`**

Create `crates/relay-core/src/acpi.rs` with exactly this content:

```rust
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

fn root_entries(memory: &impl PhysicalMemory, rsdp_phys: u64) -> Result<(bool, Vec<u64>), AcpiError> {
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
        Ok((true, sdt_entries(memory, xsdt, 8)?))
    } else {
        let rsdt = le_u32(&rsdp[16..20]) as u64;
        Ok((false, sdt_entries(memory, rsdt, 4)?))
    }
}

fn sdt_entries(memory: &impl PhysicalMemory, phys: u64, stride: usize) -> Result<Vec<u64>, AcpiError> {
    let header = read_exact_36(memory, phys)?;
    let total = le_u32(&header[4..8]) as usize;
    if total < 36 || total > MAX_TABLE_BYTES {
        return Err(AcpiError::BadLength);
    }
    if (total - 36) % stride != 0 {
        return Err(AcpiError::BadLength);
    }
    let body = read_table(memory, phys)?;
    if !checksum_ok(&body) {
        return Err(AcpiError::Checksum);
    }
    let mut entries = Vec::new();
    entries.try_reserve_exact((total - 36) / stride).map_err(|_| AcpiError::Allocation)?;
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
    if base == 0 || base % PAGE_BYTES != 0 {
        return Err(AcpiError::InvalidRange);
    }
    let buses = (bus_end as u64 - bus_start as u64) + 1;
    let window = buses.checked_mul(1 << 20).ok_or(AcpiError::InvalidRange)?;
    let last = base.checked_add(window).and_then(|end| end.checked_sub(1)).ok_or(AcpiError::InvalidRange)?;
    if last > MAX_MAPPABLE_PHYSICAL {
        return Err(AcpiError::InvalidRange);
    }
    Ok(McfgRegion { base, segment, bus_start, bus_end })
}

fn read_exact_36(memory: &impl PhysicalMemory, phys: u64) -> Result<[u8; 36], AcpiError> {
    let mut bytes = [0; 36];
    memory.read_exact(phys, &mut bytes).map_err(|_| AcpiError::Truncated)?;
    Ok(bytes)
}

fn read_table(memory: &impl PhysicalMemory, phys: u64) -> Result<Vec<u8>, AcpiError> {
    let header = read_exact_36(memory, phys)?;
    let total = le_u32(&header[4..8]) as usize;
    if total < 36 || total > MAX_TABLE_BYTES {
        return Err(AcpiError::BadLength);
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(total).map_err(|_| AcpiError::Allocation)?;
    bytes.resize(total, 0);
    memory.read_exact(phys, &mut bytes).map_err(|_| AcpiError::Truncated)?;
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
```

Add `pub mod acpi;` to `crates/relay-core/src/lib.rs` as the first module
line (before `pub mod block;`), keeping the list sorted.

- [ ] **Step 4: Run focused verification**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test acpi --locked
cargo clippy -p relay-core --all-targets --locked -- -D warnings
```

Expected: all ten ACPI tests pass warning-free.

- [ ] **Step 5: Commit ACPI parsing**

```bash
git add crates/relay-core/src/acpi.rs crates/relay-core/src/lib.rs crates/relay-core/tests/support crates/relay-core/tests/acpi.rs
git commit -m "feat: parse ACPI MCFG tables"
```

### Task 2: PCI ECAM Discovery And Safe BAR Probing

**Files:**
- Create: `crates/relay-core/src/pci.rs`
- Modify: `crates/relay-core/src/lib.rs`
- Test: `crates/relay-core/tests/pci.rs`

**Interfaces:**
- Consumes: Task 1 `McfgRegion` (constructed literally in tests; no
  `parse_mcfg` call needed here).
- Produces: `PciAddress`, `PciConfig`, `PciError`, `BarInfo`,
  `XhciPciDevice`, `ecam_address`, `enumerate_functions`, `find_xhci`,
  `probe_xhci_bar`, `XhciCaps`, `decode_xhci_caps` for Task 4.

- [ ] **Step 1: Write the failing PCI tests**

Create `crates/relay-core/tests/pci.rs` with `mod support;` unused
(omit it; this suite needs no support file) and exactly this content.
The fake models config space as register cells plus scripted BAR sizing:

```rust
use relay_core::pci::{
    BarInfo, McfgRegion, PciAddress, PciConfig, PciError,
    decode_xhci_caps, ecam_address, enumerate_functions, find_xhci, probe_xhci_bar,
};
use std::cell::RefCell;
use std::collections::BTreeMap;

const REGION: McfgRegion = McfgRegion { base: 0xE000_0000, segment: 0, bus_start: 0, bus_end: 1 };
const XHCI: PciAddress = PciAddress { segment: 0, bus: 0, device: 5, function: 0 };

fn address(bus: u8, device: u8, function: u8) -> PciAddress {
    PciAddress { segment: 0, bus, device, function }
}

struct Cell {
    current: u32,
    mask: u32,
    sizing: bool,
}

struct RecordingPciConfig {
    cells: RefCell<BTreeMap<(u8, u8, u8, u16), Cell>>,
    writes: RefCell<Vec<(PciAddress, u16, u32)>>,
}

impl RecordingPciConfig {
    fn new() -> Self {
        Self { cells: RefCell::new(BTreeMap::new()), writes: RefCell::new(Vec::new()) }
    }

    fn set(&self, address: PciAddress, offset: u16, value: u32) {
        self.cells.borrow_mut().insert(
            (address.bus, address.device, address.function, offset),
            Cell { current: value, mask: value, sizing: false },
        );
    }

    fn set_bar(&self, address: PciAddress, offset: u16, current: u32, mask: u32) {
        self.cells.borrow_mut().insert(
            (address.bus, address.device, address.function, offset),
            Cell { current, mask, sizing: false },
        );
    }

    fn with_64_bit_bar_above_4g() -> Self {
        let config = Self::new();
        config.set(XHCI, 0x00, 0x1234_5678);
        config.set(XHCI, 0x08, 0x0C03_3000);
        config.set(XHCI, 0x0C, 0x0000_0000);
        config.set(XHCI, 0x04, 0x0000_0006);
        config.set_bar(XHCI, 0x10, 0x0000_0004, 0xFFFF_0000);
        config.set_bar(XHCI, 0x14, 0x0000_0009, 0xFFFF_FFFF);
        config
    }

    fn writes_to(&self, offset: u16) -> Vec<u32> {
        self.writes.borrow().iter().filter(|(_, off, _)| *off == offset).map(|(_, _, value)| *value).collect()
    }

    fn original_command_and_bars_restored_before_enable(&self) -> bool {
        let writes = self.writes.borrow();
        let enables: Vec<usize> = writes.iter().enumerate()
            .filter(|(_, (_, off, value))| *off == 0x04 && (*value & 0x2) != 0)
            .map(|(index, _)| index).collect();
        if enables.len() != 1 {
            return false;
        }
        let before = &writes[..enables[0]];
        let mut saw_bar0 = false;
        let mut saw_bar1 = false;
        let mut saw_cmd_clear = false;
        for (_, off, value) in before {
            match *off {
                0x10 if *value == 0x0000_0004 => saw_bar0 = true,
                0x14 if *value == 0x0000_0009 => saw_bar1 = true,
                0x04 if (*value & 0x6) == 0 => saw_cmd_clear = true,
                _ => {}
            }
        }
        saw_bar0 && saw_bar1 && saw_cmd_clear
    }
}

impl PciConfig for RecordingPciConfig {
    fn read_u32(&self, address: PciAddress, offset: u16) -> Result<u32, PciError> {
        let mut cells = self.cells.borrow_mut();
        let cell = cells.get_mut(&(address.bus, address.device, address.function, offset));
        match cell {
            Some(cell) if cell.sizing => {
                cell.sizing = false;
                Ok(cell.mask)
            }
            Some(cell) => Ok(cell.current),
            None => Ok(0xFFFF_FFFF),
        }
    }

    unsafe fn write_u32(&self, address: PciAddress, offset: u16, value: u32) -> Result<(), PciError> {
        self.writes.borrow_mut().push((address, offset, value));
        if let Some(cell) = self.cells.borrow_mut().get_mut(&(address.bus, address.device, address.function, offset)) {
            if value == 0xFFFF_FFFF {
                cell.sizing = true;
            } else {
                cell.current = value;
                cell.sizing = false;
            }
        }
        Ok(())
    }
}

#[test]
fn ecam_address_uses_absolute_bus_and_rejects_out_of_range() {
    assert_eq!(ecam_address(&REGION, address(0, 5, 0), 0x10), Ok(0xE000_0000 + (5 << 15) + 0x10));
    assert_eq!(ecam_address(&REGION, address(1, 31, 7), 0xFFC), Ok(0xE000_0000 + (1 << 20) + (31 << 15) + (7 << 12) + 0xFFC));
    assert_eq!(ecam_address(&REGION, address(2, 0, 0), 0), Err(PciError::OutOfRange));
    assert_eq!(ecam_address(&REGION, address(0, 32, 0), 0), Err(PciError::OutOfRange));
    assert_eq!(ecam_address(&REGION, address(0, 0, 8), 0), Err(PciError::OutOfRange));
    assert_eq!(ecam_address(&REGION, address(0, 0, 0), 2), Err(PciError::OutOfRange));
    assert_eq!(ecam_address(&REGION, address(0, 0, 0), 0x1000), Err(PciError::OutOfRange));
    assert_eq!(ecam_address(&REGION, PciAddress { segment: 1, bus: 0, device: 0, function: 0 }, 0), Err(PciError::OutOfRange));
}

#[test]
fn enumeration_honors_multifunction_and_skips_absent() {
    let config = RecordingPciConfig::new();
    config.set(address(0, 1, 0), 0x00, 0x1111_2222);
    config.set(address(0, 1, 0), 0x08, 0x0C03_3000);
    config.set(address(0, 1, 0), 0x0C, 0x0080_0000);
    config.set(address(0, 1, 1), 0x00, 0x3333_4444);
    config.set(address(0, 1, 1), 0x08, 0x0200_0000);
    config.set(address(0, 2, 0), 0x00, 0x5555_6666);
    config.set(address(0, 2, 0), 0x08, 0x0106_0100);
    config.set(address(0, 2, 0), 0x0C, 0x0000_0000);
    let found = enumerate_functions(&config).unwrap();
    assert!(found.contains(&(address(0, 1, 0), 0x0C0330)));
    assert!(found.contains(&(address(0, 1, 1), 0x020000)));
    assert!(!found.iter().any(|(addr, _)| addr.device == 2 && addr.function != 0));
    assert!(!found.iter().any(|(addr, _)| addr.device == 0));
}

#[test]
fn bar_probe_restores_registers_on_error() {
    let config = RecordingPciConfig::with_64_bit_bar_above_4g();
    let result = probe_xhci_bar(&config, XHCI);
    assert!(result.is_ok());
    assert!(config.original_command_and_bars_restored_before_enable());
}

#[test]
fn bar_probe_reports_above_4g_base_and_size() {
    let config = RecordingPciConfig::with_64_bit_bar_above_4g();
    assert_eq!(
        probe_xhci_bar(&config, XHCI),
        Ok(BarInfo { base: 0x9_0000_0000, size: 0x1_0000, is_64: true })
    );
}

#[test]
fn bar_probe_rejects_io_zero_and_misaligned_bars() {
    let io = RecordingPciConfig::new();
    io.set(XHCI, 0x04, 0x6);
    io.set_bar(XHCI, 0x10, 0x0000_0001, 0xFFFF_FFFC);
    assert_eq!(probe_xhci_bar(&io, XHCI), Err(PciError::InvalidBar));
    assert_eq!(io.writes_to(0x10), Vec::<u32>::new());

    let zero = RecordingPciConfig::new();
    zero.set(XHCI, 0x04, 0x6);
    zero.set_bar(XHCI, 0x10, 0x0000_0000, 0x0000_0000);
    assert_eq!(probe_xhci_bar(&zero, XHCI), Err(PciError::InvalidBar));
    assert_eq!(zero.writes_to(0x10), [0xFFFF_FFFF, 0x0000_0000]);

    let misaligned = RecordingPciConfig::new();
    misaligned.set(XHCI, 0x04, 0x6);
    misaligned.set_bar(XHCI, 0x10, 0xD000_1000, 0xFFFF_C000);
    assert_eq!(probe_xhci_bar(&misaligned, XHCI), Err(PciError::InvalidBar));
    let writes = misaligned.writes_to(0x10);
    assert_eq!(*writes.last().unwrap(), 0xD000_1000);
}

#[test]
fn bar_probe_rejects_non_power_of_two_size() {
    let config = RecordingPciConfig::new();
    config.set(XHCI, 0x04, 0x6);
    config.set_bar(XHCI, 0x10, 0xD000_0000, 0xF0F0_FFF0);
    assert_eq!(probe_xhci_bar(&config, XHCI), Err(PciError::InvalidBar));
}

#[test]
fn find_xhci_matches_single_endpoint_controller() {
    let config = RecordingPciConfig::with_64_bit_bar_above_4g();
    assert_eq!(find_xhci(&config), Ok(XHCI));

    let empty = RecordingPciConfig::new();
    assert_eq!(find_xhci(&empty), Err(PciError::NoXhci));

    let bridge = RecordingPciConfig::new();
    bridge.set(XHCI, 0x00, 0x1234_5678);
    bridge.set(XHCI, 0x08, 0x0C03_3000);
    bridge.set(XHCI, 0x0C, 0x0001_0000);
    assert_eq!(find_xhci(&bridge), Err(PciError::NoXhci));
}

#[test]
fn caps_decode_reports_slots_ports_width_and_protocols() {
    let mut header = [0; 32];
    header[0] = 32;
    header[2..4].copy_from_slice(&0x0100u16.to_le_bytes());
    header[4..8].copy_from_slice(&0x0400_0008u32.to_le_bytes());
    header[8..12].copy_from_slice(&0x0000_0042u32.to_le_bytes());
    header[16..20].copy_from_slice(&0x0001_2205u32.to_le_bytes());
    let mut ext = vec![0; 64];
    ext[4..8].copy_from_slice(&0x0000_0204u32.to_le_bytes());
    ext[8..10].copy_from_slice(&0x0200u16.to_le_bytes());
    ext[12] = 1;
    ext[13] = 2;
    ext[20..24].copy_from_slice(&0x0000_0204u32.to_le_bytes());
    ext[24..26].copy_from_slice(&0x0310u16.to_le_bytes());
    ext[28] = 3;
    ext[29] = 2;
    ext[36..40].copy_from_slice(&0x0000_0100u32.to_le_bytes());
    ext[40..44].copy_from_slice(&0x0300_0000u32.to_le_bytes());
    let caps = decode_xhci_caps(&header, &ext);
    assert_eq!(caps.cap_length, 32);
    assert_eq!(caps.interface_version, 0x0100);
    assert_eq!(caps.max_slots, 8);
    assert_eq!(caps.max_ports, 4);
    assert!(caps.addr_64);
    assert!(caps.context_64);
    assert_eq!(caps.scratchpad_count, 4);
    assert!(caps.legacy_owned);
    assert_eq!(caps.usb2_bdf_range, (1, 2));
    assert_eq!(caps.usb3_bdf_range, (3, 2));
    let plain = decode_xhci_caps(&[0; 32], &[]);
    assert_eq!(plain.usb2_bdf_range, (0, 0));
    assert_eq!(plain.usb3_bdf_range, (0, 0));
    assert!(!plain.legacy_owned);
}
```

Numeric cross-checks used above: `0x0400_0008` gives slots 8 and
ports 4; `0x42` gives ERST exponent 4; `0x0001_0205` gives AC64,
CSZ, SPC, MaxPSA 1, xECP 1 (DWORD units, byte offset 4);
first protocol DWORD `0x0000_0204` means ID 2 with next 4 (DWORDs);
legacy DWORD `0x0000_0100` means ID 1 with next 0. The revision,
offset, and count byte positions follow xHCI section 7.2.2 and the
QEMU cross-check in Task 4 must confirm them against the configured
`p2=2,p3=2` topology before this test is treated as authoritative.

- [ ] **Step 2: Run tests and verify failure**

Run: `cargo test -p relay-core --test pci --locked`

Expected: FAIL because `relay_core::pci` is absent.

- [ ] **Step 3: Implement `pci.rs`**

Create `crates/relay-core/src/pci.rs` with exactly this content:

```rust
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
    unsafe fn write_u32(&self, address: PciAddress, offset: u16, value: u32) -> Result<(), PciError>;
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

use crate::acpi::McfgRegion;

const ECAM_OFFSET_MAX: u16 = 4096;
const COMMAND_MEM: u32 = 0x2;
const COMMAND_MASTER: u32 = 0x4;
const ALL_ONES: u32 = 0xFFFF_FFFF;

pub fn ecam_address(region: &McfgRegion, address: PciAddress, offset: u16) -> Result<u64, PciError> {
    if address.segment != 0
        || address.device >= 32
        || address.function >= 8
        || offset >= ECAM_OFFSET_MAX
        || offset % 4 != 0
        || address.bus < region.bus_start
        || address.bus > region.bus_end
    {
        return Err(PciError::OutOfRange);
    }
    let base = region.base;
    let bus_part = (address.bus as u64).checked_mul(1 << 20).ok_or(PciError::OutOfRange)?;
    let dev_part = (address.device as u64).checked_mul(1 << 15).ok_or(PciError::OutOfRange)?;
    let func_part = (address.function as u64).checked_mul(1 << 12).ok_or(PciError::OutOfRange)?;
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
            let first = PciAddress { segment: 0, bus, device, function: 0 };
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
                let address = PciAddress { segment: 0, bus, device, function };
                let id = config.read_u32(address, 0x00).map_err(map_transport)?;
                if id & 0xFFFF == 0xFFFF {
                    continue;
                }
                let class = config.read_u32(address, 0x08).map_err(map_transport)?;
                let code = ((class >> 24) & 0xFF) << 16 | ((class >> 16) & 0xFF) << 8 | ((class >> 8) & 0xFF);
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

pub fn find_xhci(config: &impl PciConfig) -> Result<PciAddress, PciError> {
    let mut candidates = Vec::new();
    for (address, code) in &enumerate_functions(config)? {
        if *code != XHCI_CLASS {
            continue;
        }
        let header = config.read_u32(*address, 0x0C).map_err(map_transport)?;
        if (header >> 16) & 0xFF & 0x7F != 0 {
            continue;
        }
        candidates.try_reserve(1).map_err(|_| PciError::Allocation)?;
        candidates.push(*address);
    }
    match candidates.len() {
        0 => Err(PciError::NoXhci),
        1 => Ok(candidates[0]),
        _ => Err(PciError::MultipleXhci),
    }
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
        config.write_u32(address, 0x04, command & !(COMMAND_MEM | COMMAND_MASTER)).map_err(map_transport)?;
    }
    unsafe {
        config.write_u32(address, 0x10, ALL_ONES).map_err(map_transport)?;
        if is_64 {
            config.write_u32(address, 0x14, ALL_ONES).map_err(map_transport)?;
        }
    }
    let mask0 = config.read_u32(address, 0x10).map_err(map_transport)?;
    let mask1 = if is_64 { config.read_u32(address, 0x14).map_err(map_transport)? } else { 0 };
    unsafe {
        config.write_u32(address, 0x10, bar0).map_err(map_transport)?;
        if is_64 {
            config.write_u32(address, 0x14, bar1).map_err(map_transport)?;
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
    if base == 0 || base % size != 0 {
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

pub fn decode_xhci_caps(header: &[u8; 32], ext: &[u8]) -> XhciCaps {
    let version = u16::from_le_bytes([header[2], header[3]]);
    let hcs1 = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let hcc = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
    let max_psa = ((hcc >> 12) & 0xF) as u32;
    let scratchpad_count = if hcc & (1 << 9) != 0 { 1u16 << max_psa.min(15) } else { 0 };
    let xecp = ((hcc >> 16) & 0xFFFF) as usize;
    let mut legacy_owned = false;
    let mut usb2 = (0u8, 0u8);
    let mut usb3 = (0u8, 0u8);
    if xecp != 0 {
        let mut offset = xecp * 4;
        for _ in 0..32 {
            let dword = read_ext(ext, offset);
            let next = (dword & 0xFF) as usize;
            let id = ((dword >> 8) & 0xFF) as u8;
            match id {
                1 => {
                    let sup = read_ext(ext, offset + 4);
                    if (sup >> 16) & 0x1 != 0 || (sup >> 24) & 0x1 != 0 {
                        legacy_owned = true;
                    }
                }
                2 => {
                    let rev = read_ext(ext, offset + 4) & 0xFFFF;
                    let major = ((rev >> 8) & 0xFF) as u8;
                    let port = read_ext(ext, offset + 8);
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
            offset += next * 4;
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

fn read_ext(ext: &[u8], offset: usize) -> u32 {
    match ext.get(offset..offset + 4) {
        Some(bytes) => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        None => 0,
    }
}
```

Add `pub mod pci;` to `crates/relay-core/src/lib.rs` keeping the list
sorted (`acpi`, `block`, `console`, `dma` comes later, `ext2`, `fs`,
`gpt`, `memory`, `mmio` comes later, `pci`, `shell`, `vfs` — insert
`pci` between `memory` and `shell`).

- [ ] **Step 4: Run focused verification**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test acpi --test pci --locked
cargo clippy -p relay-core --all-targets --locked -- -D warnings
```

Expected: all eight PCI tests plus the ten ACPI tests pass
warning-free.

- [ ] **Step 5: Commit PCI discovery**

```bash
git add crates/relay-core/src/pci.rs crates/relay-core/src/lib.rs crates/relay-core/tests/pci.rs
git commit -m "feat: discover PCI xHCI BAR"
```

### Task 3: Leak-Only DMA Allocation Boundary

**Files:**
- Create: `crates/relay-core/src/dma.rs`
- Modify: `crates/relay-core/src/lib.rs`
- Test: `crates/relay-core/tests/dma.rs`

**Interfaces:**
- Consumes: nothing from Tasks 1-2 (independent layer).
- Produces: `FrameSource`, `BumpDmaAllocator`, `DmaLayout`,
  `DmaAllocator`, `DmaAllocation`, `DmaError` for Task 4's kernel
  wrapper. The `FrameSource` + `BumpDmaAllocator` pair is a plan-level
  testability seam: the spec names the layout/allocator/allocation/error
  API, and the seam keeps every name and shape while letting host tests
  drive the alignment, limit, and contiguity logic through a `Vec`-backed
  frame source.

- [ ] **Step 1: Write the failing DMA tests**

Create `crates/relay-core/tests/dma.rs` with exactly this content. The
fake serves frames from a scripted list over `Vec`-backed bytes:

```rust
use relay_core::dma::{
    BumpDmaAllocator, DmaAllocator, DmaError, DmaLayout, FrameSource,
};
use core::ptr::NonNull;

const CPU_BASE: u64 = 0xFFFF_9000_0000_0000;

struct VecFrames {
    frames: Vec<u64>,
    next: usize,
    base: u64,
    bytes: Vec<u8>,
}

impl VecFrames {
    fn contiguous(base: u64, count: usize) -> Self {
        let frames = (0..count).map(|index| base + index as u64 * 4096).collect::<Vec<_>>();
        let bytes = vec![0xAA; count * 4096];
        Self { frames, next: 0, base, bytes }
    }

    fn contents(&self, phys: u64, len: usize) -> Vec<u8> {
        let offset = (phys - self.base) as usize;
        self.bytes[offset..offset + len].to_vec()
    }
}

impl FrameSource for VecFrames {
    fn allocate_frame(&mut self) -> Option<u64> {
        let frame = *self.frames.get(self.next)?;
        self.next += 1;
        Some(frame)
    }

    fn fill_range(&mut self, phys: u64, len: usize, byte: u8) {
        let offset = (phys - self.base) as usize;
        self.bytes[offset..offset + len].fill(byte);
    }

    fn cpu_address(&self, phys: u64) -> NonNull<u8> {
        NonNull::new((CPU_BASE + (phys - self.base)) as *mut u8).unwrap()
    }
}

fn layout(size: usize, align: usize, max_address: u64, zeroed: bool) -> DmaLayout {
    DmaLayout { size, align, max_address, zeroed }
}

#[test]
fn allocation_reports_exact_addresses_and_length() {
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x1_0000_0000, 4));
    let allocation = allocator.allocate(layout(8192, 4096, u64::MAX, false)).unwrap();
    assert_eq!(allocation.device_address, 0x1_0000_0000);
    assert_eq!(allocation.cpu_address.as_ptr() as u64, CPU_BASE);
    assert_eq!(allocation.len, 8192);
    assert_eq!(allocator.source_mut().contents(0x1_0000_0000, 16), vec![0xAA; 16]);
}

#[test]
fn zeroed_allocation_clears_exactly_its_bytes() {
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x2_0000_0000, 2));
    let allocation = allocator.allocate(layout(4096, 4096, u64::MAX, true)).unwrap();
    assert_eq!(allocation.device_address, 0x2_0000_0000);
    assert_eq!(allocator.source_mut().contents(0x2_0000_0000, 4096), vec![0; 4096]);
    assert_eq!(allocator.source_mut().contents(0x2_0000_1000, 16), vec![0xAA; 16]);
}

#[test]
fn allocation_validates_alignment_with_over_alloc() {
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x1_0000_0000, 8));
    let allocation = allocator.allocate(layout(4096, 8192, u64::MAX, false)).unwrap();
    assert_eq!(allocation.device_address % 8192, 0);
}

#[test]
fn bad_layouts_are_rejected_without_consuming_frames() {
    for (size, align) in [(0usize, 4096usize), (4_194_305, 4096), (4096, 0), (4096, 3), (4096, 131_072)] {
        let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x3_0000_0000, 2));
        let before = allocator.source_mut().next;
        let error = allocator.allocate(layout(size, align, u64::MAX, false)).unwrap_err();
        assert!(matches!(error, DmaError::TooLarge | DmaError::BadAlign));
        assert_eq!(allocator.source_mut().next, before);
    }
}

#[test]
fn dma_respects_max_address() {
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x1000, 8));
    assert!(allocator.allocate(layout(4096, 4096, 0x1FFF, false)).is_ok());
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x1000, 8));
    assert_eq!(
        allocator.allocate(layout(4096, 4096, 0x1FFE, false)),
        Err(DmaError::AddressLimit)
    );
}

#[test]
fn dma_skips_reserved_hole_and_reports_exhaustion() {
    let source = VecFrames {
        frames: vec![0x1000, 0x2000, 0x3000, 0x5000, 0x6000],
        next: 0,
        base: 0x1000,
        bytes: vec![0; 0x6000],
    };
    let mut allocator = BumpDmaAllocator::new(source);
    let allocation = allocator.allocate(layout(8192, 4096, u64::MAX, false)).unwrap();
    assert_eq!(allocation.device_address, 0x1000);
    assert_eq!(allocator.allocate(layout(3 * 4096, 4096, u64::MAX, false)), Err(DmaError::NoMemory));
}

#[test]
fn direct_map_ceiling_is_enforced() {
    let mut allocator = BumpDmaAllocator::new(VecFrames::contiguous(0x7FFF_FFFF_E000, 4));
    assert_eq!(
        allocator.allocate(layout(8192, 4096, u64::MAX, false)),
        Err(DmaError::AddressLimit)
    );
}

#[test]
fn dma_error_values_are_stable() {
    assert_ne!(DmaError::TooLarge, DmaError::BadAlign);
    assert_ne!(DmaError::AddressLimit, DmaError::NoMemory);
    assert_ne!(DmaError::NoMemory, DmaError::Allocation);
}
```

- [ ] **Step 2: Run tests and verify failure**

Run: `cargo test -p relay-core --test dma --locked`

Expected: FAIL because `relay_core::dma` is absent.

- [ ] **Step 3: Implement `dma.rs`**

Create `crates/relay-core/src/dma.rs` with exactly this content:

```rust
use core::ptr::NonNull;

pub struct DmaLayout {
    pub size: usize,
    pub align: usize,
    pub max_address: u64,
    pub zeroed: bool,
}

pub trait DmaAllocator {
    fn allocate(&mut self, layout: DmaLayout) -> Result<DmaAllocation, DmaError>;
}

pub struct DmaAllocation {
    pub cpu_address: NonNull<u8>,
    pub device_address: u64,
    pub len: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaError {
    TooLarge,
    BadAlign,
    AddressLimit,
    NoMemory,
    Allocation,
}

pub trait FrameSource {
    fn allocate_frame(&mut self) -> Option<u64>;
    fn fill_range(&mut self, phys: u64, len: usize, byte: u8);
    fn cpu_address(&self, phys: u64) -> NonNull<u8>;
}

pub struct BumpDmaAllocator<S> {
    source: S,
}

const PAGE_BYTES: u64 = 4096;
const MAX_ALLOCATION_BYTES: usize = 4 * 1024 * 1024;
const MAX_ALIGN_BYTES: usize = 65536;
const MAX_DIRECT_PHYSICAL: u64 = 0x7FFF_FFFF_FFFF;

impl<S> BumpDmaAllocator<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }

    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }
}

impl<S: FrameSource> DmaAllocator for BumpDmaAllocator<S> {
    fn allocate(&mut self, layout: DmaLayout) -> Result<DmaAllocation, DmaError> {
        if layout.size == 0 || layout.size > MAX_ALLOCATION_BYTES {
            return Err(DmaError::TooLarge);
        }
        if layout.align == 0 || layout.align > MAX_ALIGN_BYTES || !layout.align.is_power_of_two() {
            return Err(DmaError::BadAlign);
        }
        let needed = layout.size.div_ceil(PAGE_BYTES as usize) as u64;
        let extra = if layout.align as u64 <= PAGE_BYTES {
            0
        } else {
            layout.align as u64 / PAGE_BYTES
        };
        let total = needed.checked_add(extra).ok_or(DmaError::TooLarge)?;
        let mut run_start: Option<u64> = None;
        let mut run_prev = 0u64;
        let mut run_count = 0u64;
        loop {
            if run_count == total {
                break;
            }
            let frame = self.source.allocate_frame().ok_or(DmaError::NoMemory)?;
            match run_start {
                Some(_) if frame == run_prev + PAGE_BYTES => {
                    run_count += 1;
                    run_prev = frame;
                }
                _ => {
                    run_start = Some(frame);
                    run_prev = frame;
                    run_count = 1;
                }
            }
        }
        let first = run_start.ok_or(DmaError::NoMemory)?;
        let start = align_up(first, layout.align as u64).ok_or(DmaError::AddressLimit)?;
        let end = start.checked_add(layout.size as u64).ok_or(DmaError::AddressLimit)?;
        if end - 1 > layout.max_address || end - 1 > MAX_DIRECT_PHYSICAL {
            return Err(DmaError::AddressLimit);
        }
        if layout.zeroed {
            self.source.fill_range(start, layout.size, 0);
        }
        Ok(DmaAllocation {
            cpu_address: self.source.cpu_address(start),
            device_address: start,
            len: layout.size,
        })
    }
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    value.checked_add(align - 1).map(|value| value & !(align - 1))
}
```

Add `pub mod dma;` to `crates/relay-core/src/lib.rs` between the
`console` and `ext2` lines, keeping the list sorted.

- [ ] **Step 4: Run focused verification**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test acpi --test pci --test dma --locked
cargo clippy -p relay-core --all-targets --locked -- -D warnings
```

Expected: all eight DMA tests plus the ACPI/PCI suites pass
warning-free.

- [ ] **Step 5: Commit the DMA boundary**

```bash
git add crates/relay-core/src/dma.rs crates/relay-core/src/lib.rs crates/relay-core/tests/dma.rs
git commit -m "feat: add DMA allocation boundary"
```

### Task 4: Kernel Adapters, Boot Probe, And Platform Evidence

**Files:**
- Create: `crates/relay-core/src/mmio.rs`
- Modify: `crates/relay-core/src/lib.rs`
- Test: `crates/relay-core/tests/mmio.rs`
- Create: `crates/relay-kernel/src/arch/x86_64/mmio.rs`
- Create: `crates/relay-kernel/src/arch/x86_64/dma.rs`
- Modify: `crates/relay-kernel/src/arch/x86_64/memory.rs`
- Modify: `crates/relay-kernel/src/arch/x86_64/mod.rs`
- Create: `crates/relay-kernel/src/pci.rs`
- Modify: `crates/relay-kernel/src/entry.rs`
- Modify: `crates/relay-kernel/src/main.rs`
- Modify: `docs/acceptance/nuc-m1.md`
- Modify: `xtask/src/qemu.rs`

**Interfaces:**
- Consumes: Task 1 `parse_mcfg`/`dmar_present`/`McfgRegion`/`AcpiError`,
  Task 2 `PciAddress`/`find_xhci`/`probe_xhci_bar`/`decode_xhci_caps`/
  `XhciCaps`/`BarInfo`, Task 3 `FrameSource`/`BumpDmaAllocator`/
  `DmaLayout`, plus `BootInfo.acpi_rsdp_phys`, `PHYSICAL_MEMORY_OFFSET`,
  `PCI_BAR_VIRTUAL_START`, and the UC flag set from
  `relay-loader/src/paging.rs`.
- Produces: the Task 11 release gate — mapped ECAM/BAR, dumped
  capabilities, `platform-probe` serial markers, the QEMU topology with
  an explicit xHCI controller, and the probe evidence table.

- [ ] **Step 1: Write the failing paging-math tests**

Create `crates/relay-core/tests/mmio.rs` with exactly this content:

```rust
use relay_core::mmio::{MapError, cover_range, is_canonical, page_indices};

#[test]
fn indices_split_canonical_addresses_for_both_depths() {
    assert_eq!(page_indices(0xFFFF_C000_0000_1000, false), [0, 384, 0, 0, 1]);
    assert_eq!(page_indices(0xFFFF_8000_0000_0000, false)[1], 256);
    let five = page_indices(0xFFFF_0000_0000_1000, true);
    assert_eq!(five[0], 0x1FF);
    assert_eq!(&five[1..], &[0, 0, 0, 1]);
    assert!(is_canonical(0xFFFF_C000_0000_1000, false));
    assert!(is_canonical(0x0000_7FFF_FFFF_FFFF, false));
    assert!(!is_canonical(0x1234_5678_9ABC_DEF0, false));
    assert!(is_canonical(0xFFFF_0000_0000_1000, true));
    assert!(!is_canonical(0x0100_0000_0000_0000, true));
}

#[test]
fn cover_range_aligns_and_counts_pages() {
    assert_eq!(cover_range(0x1234, 0x2000), Ok((0x1000, 3, 0x234)));
    assert_eq!(cover_range(0x4000, 0x1000), Ok((0x4000, 1, 0)));
    assert_eq!(cover_range(0x8000_0000_0000, 1), Err(MapError::InvalidRange));
    assert_eq!(cover_range(0x1000, 0), Err(MapError::InvalidRange));
    assert_eq!(cover_range(u64::MAX - 0xFFF, 0x2000), Err(MapError::InvalidRange));
}

#[test]
fn uc_flags_match_loader_bar_mappings() {
    assert_eq!(relay_core::mmio::UC_MMIO_FLAGS, 1 | (1 << 1) | (1 << 3) | (1 << 4) | (1 << 63));
}
```

- [ ] **Step 2: Run tests and verify failure**

Run: `cargo test -p relay-core --test mmio --locked`

Expected: FAIL because `relay_core::mmio` is absent.

- [ ] **Step 3: Implement pure paging math in core**

Create `crates/relay-core/src/mmio.rs` with exactly this content
(kernel bins set `test = false`, so this logic lives in core to stay
host-coverable; the kernel glue in Step 4 only adds CR3 reads, frame
allocation, and TLB flushes):

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapError {
    InvalidRange,
    AlreadyMapped,
    NoMemory,
    UnsupportedDepth,
}

pub const PAGE_BYTES: u64 = 4096;
pub const MAX_DIRECT_PHYSICAL: u64 = 0x7FFF_FFFF_FFFF;
pub const PCI_WINDOW_BASE: u64 = 0xFFFF_C000_0000_0000;
pub const UC_MMIO_FLAGS: u64 = 1 | (1 << 1) | (1 << 3) | (1 << 4) | (1 << 63);
pub const ENTRY_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
pub const ENTRY_PRESENT: u64 = 1;
pub const ENTRY_HUGE: u64 = 1 << 7;

pub fn page_indices(virt: u64, five_level: bool) -> [u16; 5] {
    let shift = [48u32, 39, 30, 21, 12];
    let mut indices = [0; 5];
    for (slot, bit) in indices.iter_mut().zip(shift) {
        *slot = ((virt >> bit) & 0x1FF) as u16;
    }
    if !five_level {
        indices[0] = 0;
    }
    indices
}

pub fn is_canonical(virt: u64, five_level: bool) -> bool {
    if five_level {
        let sign = (virt >> 56) & 1;
        let high = virt >> 57;
        (sign == 1 && high == 0x7F) || (sign == 0 && high == 0)
    } else {
        let sign = (virt >> 47) & 1;
        let high = virt >> 48;
        (sign == 1 && high == 0xFFFF) || (sign == 0 && high == 0)
    }
}

pub fn cover_range(base: u64, len: usize) -> Result<(u64, u64, u64), MapError> {
    if len == 0 {
        return Err(MapError::InvalidRange);
    }
    let end = (base as u64).checked_add(len as u64).ok_or(MapError::InvalidRange)?;
    if end - 1 > MAX_DIRECT_PHYSICAL {
        return Err(MapError::InvalidRange);
    }
    let aligned = base & !(PAGE_BYTES - 1);
    let offset = base - aligned;
    let span = offset.checked_add(len as u64).ok_or(MapError::InvalidRange)?;
    let pages = span.div_ceil(PAGE_BYTES);
    Ok((aligned, pages, offset))
}
```

Add `pub mod mmio;` to `crates/relay-core/src/lib.rs` between the
`memory` and `pci` lines, keeping the list sorted. Run
`cargo test -p relay-core --test mmio --locked` and require
all three tests to pass before continuing within this same step.

- [ ] **Step 4: Implement kernel MMIO installer, DMA wrapper, and frame accessors**

In `crates/relay-kernel/src/arch/x86_64/memory.rs` add, after the
existing allocator code:

```rust
/// Returns one usable frame, skipping retained boot structures.
/// Single-core boot discipline makes locking unnecessary.
pub fn allocate_frame() -> Option<u64> {
    // SAFETY: called only on the boot core before any concurrent user exists,
    // and the allocator was initialized once from validated handoff data.
    unsafe { (*FRAME_ALLOCATOR.0.get()).as_mut()?.allocate_frame() }
}

/// Bounds-checked direct-map slice for DMA fills and table access.
/// Returns None instead of faulting on out-of-range requests.
pub fn direct_slice_mut(physical: u64, len: usize) -> Option<&'static mut [u8]> {
    let end = physical.checked_add(len as u64)?;
    if len == 0 || end - 1 > MAX_DIRECT_MAPPED_PHYSICAL_PLUS_ONE - 1 {
        return None;
    }
    let virt = physical.checked_add(PHYSICAL_MEMORY_OFFSET)?;
    // SAFETY: bounds were checked against the direct-map ceiling and the
    // caller owns the frames it fills; lifetime is static because physical
    // memory outlives every borrower.
    Some(unsafe { core::slice::from_raw_parts_mut(virt as *mut u8, len) })
}
```

with `const MAX_DIRECT_MAPPED_PHYSICAL_PLUS_ONE: u64 = 0x8000_0000_0000;`
beside the existing constants. (`end - 1 > ceiling` is written this way
so `len == 0` is rejected before the subtraction can underflow.)

Create `crates/relay-kernel/src/arch/x86_64/mmio.rs` with exactly this
content:

```rust
use core::arch::asm;

use relay_core::mmio::{
    ENTRY_ADDR_MASK, ENTRY_HUGE, ENTRY_PRESENT, MapError, PCI_WINDOW_BASE, UC_MMIO_FLAGS,
    cover_range, is_canonical, page_indices,
};

use super::memory::{allocate_frame, direct_slice_mut};

static mut NEXT_WINDOW: u64 = PCI_WINDOW_BASE;

fn cr3() -> u64 {
    let value: u64;
    // SAFETY: reading CR3 is valid at CPL0 and has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

fn five_level() -> bool {
    let cr4: u64;
    // SAFETY: reading CR4 is valid at CPL0 and has no side effects.
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    cr4 & (1 << 12) != 0
}

fn invlpg(virt: u64) {
    // SAFETY: the caller just installed a mapping for this address on the
    // current core, which is the only core running.
    unsafe { asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags)) };
}

fn next_window(pages: u64) -> Result<u64, MapError> {
    let bytes = pages.checked_mul(4096).ok_or(MapError::InvalidRange)?;
    // SAFETY: single-core boot discipline; this is the only mapper.
    unsafe {
        let start = NEXT_WINDOW;
        NEXT_WINDOW = NEXT_WINDOW.checked_add(bytes).ok_or(MapError::InvalidRange)?;
        Ok(start)
    }
}

/// Maps `byte_len` bytes at `phys_base` as uncached MMIO and returns the
/// caller-visible pointer (window base plus the original misalignment).
/// Refuses present leaves and huge-page conflicts without modifying them.
pub fn map_uncached(phys_base: u64, byte_len: usize) -> Result<*mut u8, MapError> {
    let (aligned, pages, offset) = cover_range(phys_base, byte_len)?;
    let five = five_level();
    let virt_base = next_window(pages)?;
    if !is_canonical(virt_base, five) {
        return Err(MapError::InvalidRange);
    }
    let mut page = 0;
    while page < pages {
        let phys = aligned.checked_add(page * 4096).ok_or(MapError::InvalidRange)?;
        let virt = virt_base.checked_add(page * 4096).ok_or(MapError::InvalidRange)?;
        if !is_canonical(virt, five) {
            return Err(MapError::InvalidRange);
        }
        unsafe { install_4k(virt, phys, five)? };
        invlpg(virt);
        page += 1;
    }
    Ok((virt_base + offset) as *mut u8)
}

/// # Safety
/// `virt`/`phys` are checked 4K-aligned canonical/mappable addresses and
/// the caller holds exclusive mapper ownership on the only running core.
unsafe fn install_4k(virt: u64, phys: u64, five: bool) -> Result<(), MapError> {
    let depth = if five { 5 } else { 4 };
    let indices = page_indices(virt, five);
    let start_level = 5 - depth;
    let mut table_phys = cr3() & ENTRY_ADDR_MASK;
    let mut level = start_level;
    while level < 4 {
        let table = direct_slice_mut(table_phys, 512 * 8).ok_or(MapError::NoMemory)?;
        let entry = unsafe { (table.as_mut_ptr() as *mut u64).add(indices[level] as usize) };
        let value = unsafe { entry.read_volatile() };
        if value & ENTRY_PRESENT == 0 {
            let frame = allocate_frame().ok_or(MapError::NoMemory)?;
            let fresh = direct_slice_mut(frame, 512 * 8).ok_or(MapError::NoMemory)?;
            for byte in fresh.iter_mut() {
                *byte = 0;
            }
            unsafe { entry.write_volatile(frame | ENTRY_PRESENT | (1 << 1)) };
            table_phys = frame;
        } else {
            if value & ENTRY_HUGE != 0 {
                return Err(MapError::AlreadyMapped);
            }
            table_phys = value & ENTRY_ADDR_MASK;
            if table_phys == 0 {
                return Err(MapError::AlreadyMapped);
            }
        }
        level += 1;
    }
    let table = direct_slice_mut(table_phys, 512 * 8).ok_or(MapError::NoMemory)?;
    let leaf = unsafe { (table.as_mut_ptr() as *mut u64).add(indices[4] as usize) };
    if unsafe { leaf.read_volatile() } & ENTRY_PRESENT != 0 {
        return Err(MapError::AlreadyMapped);
    }
    unsafe { leaf.write_volatile(phys | UC_MMIO_FLAGS) };
    Ok(())
}
```

The mid-walk `table_phys == 0` guard refuses corrupt present entries
before the direct-map read can fault. `depth` is 4 or 5, so `level`
starts at 1 or 0 and the leaf index is always `indices[4]`; a
`debug_assert` is unnecessary because both values are constructed, not
parsed.

Create `crates/relay-kernel/src/arch/x86_64/dma.rs` with exactly this
content:

```rust
use core::ptr::NonNull;

use relay_core::dma::FrameSource;

use super::memory::{allocate_frame, direct_slice_mut};

pub struct KernelDma;

impl FrameSource for KernelDma {
    fn allocate_frame(&mut self) -> Option<u64> {
        allocate_frame()
    }

    fn fill_range(&mut self, phys: u64, len: usize, byte: u8) {
        if let Some(slice) = direct_slice_mut(phys, len) {
            slice.fill(byte);
        }
    }

    fn cpu_address(&self, phys: u64) -> NonNull<u8> {
        NonNull::new((phys + super::memory::PHYSICAL_MEMORY_OFFSET) as *mut u8)
            .expect("DMA physical address is nonzero")
    }
}

pub fn allocator() -> relay_core::dma::BumpDmaAllocator<KernelDma> {
    relay_core::dma::BumpDmaAllocator::new(KernelDma)
}
```

`fill_range` silently skips out-of-range fills because `BumpDmaAllocator`
only passes validated in-range runs; the `direct_slice_mut` bound is
defense in depth, not a silent error path. Add `pub mod dma;` and
`pub mod mmio;` to `crates/relay-kernel/src/arch/x86_64/mod.rs`.

- [ ] **Step 5: Implement the kernel PCI probe, wire it into entry, extend QEMU, record evidence**

Create `crates/relay-kernel/src/pci.rs` with exactly this content:

```rust
use relay_abi::BootInfo;
use relay_core::{
    acpi::{McfgRegion, PhysicalMemory, dmar_present, parse_mcfg},
    dma::DmaError,
    mmio::MapError,
    pci::{BarInfo, PciAddress, PciConfig, PciError, XhciCaps, decode_xhci_caps, find_xhci, probe_xhci_bar},
};

use crate::arch::x86_64::{dma, mmio};

struct KernelMem;

impl PhysicalMemory for KernelMem {
    fn read_exact(&self, physical: u64, output: &mut [u8]) -> Result<(), relay_core::acpi::MemoryError> {
        use relay_core::acpi::MemoryError;
        let end = physical.checked_add(output.len() as u64).ok_or(MemoryError::Overflow)?;
        if !output.is_empty() && end - 1 > 0x7FFF_FFFF_FFFF {
            return Err(MemoryError::OutOfRange);
        }
        let virt = physical.checked_add(crate::arch::x86_64::memory::PHYSICAL_MEMORY_OFFSET).ok_or(MemoryError::Overflow)?;
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
        let virt = self.mapped.checked_add(phys - self.region.base).ok_or(PciError::OutOfRange)?;
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

    unsafe fn write_u32(&self, address: PciAddress, offset: u16, value: u32) -> Result<(), PciError> {
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
    let ecam_len = usize::try_from(buses * (1 << 20)).map_err(|_| ProbeError::Map(MapError::InvalidRange))?;
    let ecam = mmio::map_uncached(region.base, ecam_len).map_err(ProbeError::Map)? as u64;
    let config = EcamAccess { mapped: ecam, region };
    let xhci = find_xhci(&config).map_err(ProbeError::Pci)?;
    let bar = probe_xhci_bar(&config, xhci).map_err(ProbeError::Pci)?;
    let bar_len = usize::try_from(bar.size).map_err(|_| ProbeError::Map(MapError::InvalidRange))?;
    let bar_ptr = mmio::map_uncached(bar.base, bar_len).map_err(ProbeError::Map)?;
    let caps = snapshot_caps(bar_ptr);
    let dmar = dmar_present(&memory, boot_info.acpi_rsdp_phys).map_err(ProbeError::Acpi)?;
    let _ = dma::allocator();
    Ok(PlatformInfo { region, xhci, bar, caps, dmar })
}

fn snapshot_caps(bar: *mut u8) -> XhciCaps {
    let mut header = [0; 32];
    let mut ext = [0; 256];
    // SAFETY: the BAR was just mapped uncached and exclusively for this
    // probe; only volatile reads are performed within the mapped prefix.
    unsafe {
        let mut index = 0;
        while index < 32 {
            header[index] = (bar.add(index)).read_volatile();
            index += 1;
        }
        index = 0;
        while index < 256 {
            ext[index] = (bar.add(index)).read_volatile();
            index += 1;
        }
    }
    decode_xhci_caps(&header, &ext)
}
```

`let _ = dma::allocator();` proves the DMA boundary links into the
kernel build now; Task 12 consumes it for real. It allocates nothing,
so probe behavior is unchanged.

In `crates/relay-kernel/src/entry.rs`, after the heap-initialization
line and before the `kernel-entry status=ok` line, insert exactly:

```rust
    // SAFETY: `valid_boot_info` established this BootInfo and its RSDP field.
    match unsafe { crate::pci::probe(&*info) } {
        Ok(platform) => {
            let line = alloc::format!(
                "[relay] phase=platform-probe status=ok mcfg_base={:#x} bus={}-{} xhci={:02x}:{:02x}.{} bar_base={:#x} bar_size={:#x} bar64={} slots={} ports={} ctx64={} addr64={} scratch={} legacy={} usb2_off={} usb2_count={} usb3_off={} usb3_count={} dmar={}\n",
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
            );
            crate::console::write(line.as_bytes());
        }
        Err(error) => {
            let line = alloc::format!(
                "[relay] phase=platform-probe status={}\n",
                error.status()
            );
            crate::console::write(line.as_bytes());
            crate::arch::x86_64::halt();
        }
    }
```

`alloc::format!` needs `extern crate alloc;` at the binary crate root:
in `crates/relay-kernel/src/main.rs`, add `extern crate alloc;` after
the `#![no_main]` line and add `mod pci;` to the module list (after
`mod entry;`, keeping the existing order otherwise unchanged).

In `xtask/src/qemu.rs`, extend the boot argument list with an explicit
xHCI controller and fixed port counts immediately after `"-machine",
"q35",`:

```rust
                "-device",
                "qemu-xhci,p2=2,p3=2",
```

This keeps the strict halt-on-`NoXhci` probe green in the existing
Task 4 boot gate (which runs without USB today) and fixes the expected
protocol ranges for the Task 4 cross-check: USB2 offset 1 count 2,
USB3 offset 3 count 2.

In `docs/acceptance/nuc-m1.md`, append a Task 11 section:

```markdown
## Task 11 Platform Probe

QEMU-observed values come from `target/qemu/serial.log` after
`cargo xtask qemu boot target/relay-os.img --display none --accel tcg`
with `-device qemu-xhci,p2=2,p3=2` (USB2 ports 1-2, USB3 ports 3-4).

| Field | QEMU observed | NUC target |
| --- | --- | --- |
| MCFG base | <fill from serial> | pending physical probe |
| MCFG bus range | <fill from serial> | pending physical probe |
| xHCI BDF | <fill from serial> | pending physical probe |
| BAR width/address/size | <fill from serial> | pending physical probe |
| Context size / addr width | <fill from serial> | pending physical probe |
| Scratchpad count | <fill from serial> | pending physical probe |
| Legacy ownership bits | <fill from serial> | pending physical probe |
| USB2/USB3 protocol ranges | 1:2 / 3:2 expected | pending physical probe |
| VT-d firmware state + DMAR | <fill from serial> | pending physical probe |

Byte-position field legend: `usb2_off`/`usb2_count` and
`usb3_off`/`usb3_count` come straight from the Supported Protocol
capability dwords. Do not proceed to BOT storage work on the NUC until
this table's NUC column is filled. If VT-d translation blocks DMA
there, stop and get explicit design approval before adding an
identity-mapped DMA domain or requiring VT-d disabled in firmware.
```

The `<fill from serial>` cells are evidence transcribed from a real run
in this same task — not placeholders: the implementer runs QEMU,
copies the values, and commits the filled table.

- [ ] **Step 6: Run the full Task 11 verification**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test acpi --test pci --test dma --test mmio --locked
cargo test -p relay-core --test block --test gpt --test vfs --test shell_parser --test shell_read --test shell_mutation --test line_editor --test ext2_mount --test ext2_read --test ext2_files --test ext2_directories --locked
cargo build -p relay-loader --target x86_64-unknown-uefi --locked
cargo build -p relay-kernel --target x86_64-unknown-none --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo xtask image --output target/relay-os.img
cargo xtask qemu boot target/relay-os.img --display none --accel tcg
```

Then confirm `target/qemu/serial.log` contains all three markers:

```bash
grep -c "phase=platform-probe status=ok" target/qemu/serial.log
grep -c "phase=kernel-entry status=ok" target/qemu/serial.log
grep -c "phase=kernel-runtime status=ok" target/qemu/serial.log
```

Expected: fmt clean; all host suites pass; both target builds pass;
Clippy reports no warnings; QEMU prints `platform-probe status=ok`
with `usb2_off=1 usb2_count=2 usb3_off=3 usb3_count=2` matching the
configured `p2=2,p3=2` topology (this is the layout cross-check for the
section 7.2.2 byte positions — a mismatch here means the decode offsets
are wrong and must be fixed against the xHCI spec before committing);
both pre-existing kernel markers still print (Task 4 gate unbroken).

- [ ] **Step 7: Commit platform discovery**

```bash
git add crates/relay-core/src/mmio.rs crates/relay-core/src/acpi.rs crates/relay-core/src/pci.rs crates/relay-core/src/dma.rs crates/relay-core/src/lib.rs crates/relay-core/tests crates/relay-kernel/src/arch/x86_64/mmio.rs crates/relay-kernel/src/arch/x86_64/dma.rs crates/relay-kernel/src/arch/x86_64/memory.rs crates/relay-kernel/src/arch/x86_64/mod.rs crates/relay-kernel/src/pci.rs crates/relay-kernel/src/entry.rs crates/relay-kernel/src/main.rs docs/acceptance/nuc-m1.md xtask/src/qemu.rs
git commit -m "feat: discover xHCI platform resources"
```
...[truncated 17904 chars]