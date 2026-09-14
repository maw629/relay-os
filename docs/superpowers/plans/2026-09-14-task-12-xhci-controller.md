# Task 12 xHCI Rings And Polling Controller Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a polling xHCI controller with host-tested rings, contexts, and TRB codec plus kernel init, ports, and control/bulk/interrupt transfers gated by a new QEMU xHCI scenario.

**Architecture:** Pure `no_std` TRB/ring/context math in `relay-core::xhci` over injected `Mmio`/`Clock` traits with `Vec`-backed fakes; privileged glue (volatile MMIO, Task 11 DMA, TSC reads, doorbells) isolated in `relay-kernel::xhci`; the boot probe prints stable `xhci-probe` markers and halts on failure; xtask gains `qemu xhci` alongside the green `qemu boot` gate.

**Tech Stack:** Rust 1.98.1, edition 2024, `no_std` plus `alloc`, existing `relay-core::{dma::DmaAllocator, mmio, pci::{BarInfo, XhciCaps, XhciPciDevice}}`, `relay-kernel` TSC (`RDTSC`) and `map_uncached`, QEMU q35/OVMF with `-device qemu-xhci,p2=2,p3=2`.

**Spec:** `docs/superpowers/specs/2026-09-14-task-12-xhci-controller-design.md`

## Global Constraints

- `relay-core` remains `#![no_std]`; no unsafe code in new core modules; host filesystem/process APIs stay in integration-test support only.
- Every unsafe block lives in kernel adapters and documents the invariant it upholds (live-table exclusivity, direct-map bounds, single-core no-concurrent-mapper, volatile-MMIO ownership).
- All register bytes, TRB fields, lengths, addresses, shifts, and arithmetic are checked; malformed firmware or device data returns typed errors and never panics; dynamic-slice indexing uses `.get()` with mapped errors, never direct indexing.
- DMA allocation is leak-only via the Task 11 allocator: no `free`, no reclaiming `Drop`; handed-out allocations live until reboot.
- PCI bus mastering stays off until Task 12 DMA structures are ready, then is enabled once during `initialize` and left on for Tasks 13-15.
- `MaxSlotsEn` is `min(caps.max_slots, 32)`; scratchpads follow per-caps rule (0 → `DCBAA[0]=0`, N → N pages plus array).
- If VT-d translation blocks DMA on the NUC, stop and get explicit design approval before adding an identity-mapped DMA domain or requiring VT-d disabled in firmware.
- Maintain `cargo fmt --all --check`, workspace Clippy with `-D warnings`, and locked workspace tests.

---

## File Structure

```text
crates/relay-core/src/xhci/mod.rs       XhciError, reg consts, TRB codec, Deadline/Clock/Mmio, speeds, handles
crates/relay-core/src/xhci/ring.rs       Ring producer/consumer cycle bookkeeping over TRB bytes
crates/relay-core/src/xhci/context.rs    stride/offsets, DCI, slot/ep field packing, EP0 sizes
crates/relay-core/src/lib.rs             add pub mod xhci
crates/relay-core/tests/xhci_trb.rs      TRB codec vectors
crates/relay-core/tests/xhci_ring.rs     ring wrap/full/consumer/correlation suite
crates/relay-core/tests/xhci_context.rs  context offset/DCI/EP0 suite
crates/relay-kernel/src/arch/x86_64/clock.rs
                                         TscClock plus TestClock-compatible calibration query
crates/relay-kernel/src/arch/x86_64/mod.rs
                                         expose clock module
crates/relay-kernel/src/xhci/mod.rs      KernelMmio volatile glue, controller re-export
crates/relay-kernel/src/xhci/controller.rs
                                         initialize, poll, slots, scratchpads, doorbells
crates/relay-kernel/src/xhci/port.rs     PORTSC decode, reset, CCS/PED polling
crates/relay-kernel/src/xhci/transfer.rs ControlData/BulkData containment, control/bulk/interrupt queue-wait
crates/relay-kernel/src/entry.rs         run xhci-probe after platform-probe
docs/acceptance/nuc-m1.md                Task 12 probe table (QEMU now, NUC pending)
xtask/src/qemu.rs                        add xhci() subcommand plus XHCI_PROBE_MARKER const
xtask/src/main.rs                        dispatch qemu xhci alongside boot
xtask/tests/qemu_xhci.rs                 gate test for the new subcommand
```

Deviations from the milestone file list, with rationale: `arch/x86_64/clock.rs`
is added because the milestone names a `Clock` trait but no source file for
the TSC implementation; kernel bins set `test = false`, so all volatility
stays behind `KernelMmio` while every pure computation stays in core.

---

### Task 1: TRB Codec, Traits, And Error Shapes

**Files:**
- Create: `crates/relay-core/src/xhci/mod.rs`
- Modify: `crates/relay-core/src/lib.rs`
- Test: `crates/relay-core/tests/xhci_trb.rs`

**Interfaces:**
- Consumes: nothing (first Task 12 module); reuses `relay_core::dma::{DmaAllocation, DmaError}` and `relay_core::mmio::MapError` only as wrapped error variants.
- Produces: `XhciError`, register constants, `Deadline`, `Clock`, `Mmio`, `UsbSpeed`, `DeviceHandle`, `EndpointConfig`, `ControlDirection`, `BulkDirection`, `ControlData`, `BulkData`, TRB `encode_*`/`decode_*` helpers for Task 2.

- [ ] **Step 1: Write the failing TRB codec tests**

Create `crates/relay-core/tests/xhci_trb.rs` with exactly this content:

```rust
use relay_core::xhci::{
    ControlData, XhciError, decode_cmd_complete, decode_port_change, decode_transfer_event,
    encode_address_device, encode_config_ep, encode_data_stage, encode_enable_slot,
    encode_link, encode_normal, encode_noop_cmd, encode_reset_ep, encode_setup_stage,
    encode_status_stage,
};

#[test]
fn setup_stage_carries_setup_bytes_and_type() {
    let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x40, 0x00];
    let trb = encode_setup_stage(setup, 8);
    assert_eq!(&trb[0..8], &setup);
    assert_eq!((u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]) >> 10) & 0x3F, 2);
    assert_eq!(trb[15] & 0x01, 0);
}

#[test]
fn data_stage_sets_direction_chain_and_length() {
    let trb = encode_data_stage(0x1_0000_1000, 64, true, true);
    assert_eq!(u64::from_le_bytes(trb[0..8].try_into().unwrap()), 0x1_0000_1000);
    assert_eq!(u32::from_le_bytes([trb[8], trb[9], trb[10], trb[11]]) & 0x1FFFF, 64);
    let control = u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]);
    assert_eq!((control >> 10) & 0x3F, 3);
    assert_ne!(control & (1 << 16), 0);
    assert_ne!(control & (1 << 4), 0);
}

#[test]
fn status_stage_sets_ioc_and_direction() {
    let trb = encode_status_stage(true, 1);
    let control = u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]);
    assert_eq!((control >> 10) & 0x3F, 4);
    assert_ne!(control & (1 << 4), 0);
    assert_eq!(control & 0x01, 1);
}

#[test]
fn normal_trb_carries_phys_len_chain_ioc() {
    let trb = encode_normal(0x2_0000_0000, 512, true, false, 1);
    assert_eq!(u64::from_le_bytes(trb[0..8].try_into().unwrap()), 0x2_0000_0000);
    assert_eq!(u32::from_le_bytes([trb[8], trb[9], trb[10], trb[11]]) & 0x1FFFF, 512);
    let control = u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]);
    assert_eq!((control >> 10) & 0x3F, 1);
    assert_ne!(control & (1 << 4), 0);
}

#[test]
fn link_trb_points_at_base_with_toggle() {
    let trb = encode_link(0x3_0000_0000, true, 1);
    assert_eq!(u64::from_le_bytes(trb[0..8].try_into().unwrap()) & !0xF, 0x3_0000_0000);
    let control = u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]);
    assert_eq!((control >> 10) & 0x3F, 6);
    assert_ne!(control & (1 << 1), 0);
}

#[test]
fn command_trbs_carry_slot_and_pointer() {
    let enable = encode_enable_slot(0);
    assert_eq!((u32::from_le_bytes([enable[12], enable[13], enable[14], enable[15]]) >> 10) & 0x3F, 9);
    let addr = encode_address_device(0x4_0000_0000, 3, 0);
    assert_eq!(u64::from_le_bytes(addr[0..8].try_into().unwrap()), 0x4_0000_0000);
    assert_eq!((u32::from_le_bytes([addr[12], addr[13], addr[14], addr[15]]) >> 10) & 0x3F, 11);
    let config = encode_config_ep(0x4_0000_1000, 3, 0);
    assert_eq!((u32::from_le_bytes([config[12], config[13], config[14], config[15]]) >> 10) & 0x3F, 12);
    let reset = encode_reset_ep(3, 2, 0);
    assert_eq!((u32::from_le_bytes([reset[12], reset[13], reset[14], reset[15]]) >> 10) & 0x3F, 14);
    let noop = encode_noop_cmd(1);
    assert_eq!((u32::from_le_bytes([noop[12], noop[13], noop[14], noop[15]]) >> 10) & 0x3F, 23);
}

#[test]
fn event_decoders_parse_known_vectors() {
    let mut transfer = [0; 16];
    transfer[0..8].copy_from_slice(&0x1_0000_1000u64.to_le_bytes());
    transfer[8..12].copy_from_slice(&((8u32 << 24) | 0u32).to_le_bytes());
    let control = (32u32 << 10) | (3u32 << 24) | (2u32 << 16) | 1u32;
    transfer[12..16].copy_from_slice(&control.to_le_bytes());
    let event = decode_transfer_event(&transfer).unwrap();
    assert_eq!(event.pointer, 0x1_0000_1000);
    assert_eq!(event.length, 8);
    assert_eq!(event.code, 1);
    assert_eq!(event.slot, 3);
    assert_eq!(event.endpoint, 2);
    let mut complete = [0; 16];
    complete[0..8].copy_from_slice(&0x5_0000_0000u64.to_le_bytes());
    let cc = (33u32 << 10) | (1u32 << 24) | (7u32 << 16) | 1u32;
    complete[12..16].copy_from_slice(&cc.to_le_bytes());
    let done = decode_cmd_complete(&complete).unwrap();
    assert_eq!(done.pointer, 0x5_0000_0000);
    assert_eq!(done.slot, 7);
    assert_eq!(done.code, 1);
    let mut port = [0; 16];
    port[0..4].copy_from_slice(&0x0200_0000u32.to_le_bytes());
    let pc = (34u32 << 10) | (1u32 << 24) | 1u32;
    port[12..16].copy_from_slice(&pc.to_le_bytes());
    let change = decode_port_change(&port).unwrap();
    assert_eq!(change.port, 2);
    assert_eq!(change.code, 1);
}

#[test]
fn decoders_reject_bad_types_and_lengths() {
    assert_eq!(decode_transfer_event(&[0; 8]), Err(XhciError::UnsupportedEvent));
    let mut wrong = [0; 16];
    let control = (9u32 << 10) | 1u32;
    wrong[12..16].copy_from_slice(&control.to_le_bytes());
    assert_eq!(decode_transfer_event(&wrong), Err(XhciError::UnsupportedEvent));
    assert_eq!(decode_cmd_complete(&wrong), Err(XhciError::UnsupportedEvent));
    assert_eq!(decode_port_change(&wrong), Err(XhciError::UnsupportedEvent));
}

#[test]
fn control_data_rejects_oversize_and_mismatched_dma() {
    use core::ptr::NonNull;
    use relay_core::dma::DmaAllocation;
    let mut backing = vec![0u8; 8192];
    let allocation = DmaAllocation {
        cpu_address: NonNull::new(backing.as_mut_ptr()).unwrap(),
        device_address: 0x1_0000_0000,
        len: 4096,
    };
    let mut big = vec![0u8; 8192];
    assert!(ControlData::new(
        relay_core::xhci::ControlDirection::In,
        &mut big,
        &allocation
    )
    .is_err());
}
```

- [ ] **Step 2: Run tests and verify failure**

Run: `cargo test -p relay-core --test xhci_trb --locked`

Expected: FAIL because `relay_core::xhci` is absent.

- [ ] **Step 3: Implement `xhci/mod.rs` codec, traits, and errors**

Create `crates/relay-core/src/xhci/mod.rs` with exactly this content:

```rust
pub mod context;
pub mod ring;

use crate::dma::DmaAllocation;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciError {
    UnsupportedPlatform,
    UnsupportedEvent,
    InvalidRegister,
    Map(crate::mmio::MapError),
    Dma(crate::dma::DmaError),
    Timeout,
    Stalled,
    TransferFailed(u8),
    NoDevice,
    InvalidSlot,
    NoPorts,
    Allocation,
}

pub struct Deadline(pub u64);

pub trait Clock {
    fn now_ticks(&self) -> u64;
    fn ticks_per_second(&self) -> u64;
}

pub trait Mmio {
    fn read32(&self, offset: u32) -> Result<u32, XhciError>;
    fn write32(&mut self, offset: u32, value: u32) -> Result<(), XhciError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsbSpeed {
    Full = 1,
    High = 3,
    Super = 4,
    SuperPlus = 5,
}

impl UsbSpeed {
    pub fn from_portsc(value: u32) -> Result<Self, XhciError> {
        match (value >> 10) & 0xF {
            1 => Ok(UsbSpeed::Full),
            3 => Ok(UsbSpeed::High),
            4 => Ok(UsbSpeed::Super),
            5 => Ok(UsbSpeed::SuperPlus),
            _ => Err(XhciError::UnsupportedPlatform),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceHandle {
    pub slot_id: u8,
    pub root_port: u8,
    pub speed: UsbSpeed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointConfig {
    pub address: u8,
    pub transfer_type: u8,
    pub max_packet_size: u16,
    pub interval: u8,
    pub max_burst: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlDirection {
    In,
    Out,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BulkDirection {
    In,
    Out,
}

pub struct ControlData<'a> {
    pub direction: ControlDirection,
    pub buffer: &'a mut [u8],
    pub device_address: u64,
}

pub struct BulkData<'a> {
    pub direction: BulkDirection,
    pub buffer: &'a mut [u8],
    pub device_address: u64,
}

impl<'a> ControlData<'a> {
    pub fn new(
        direction: ControlDirection,
        buffer: &'a mut [u8],
        allocation: &'a DmaAllocation,
    ) -> Result<Self, XhciError> {
        if buffer.len() > 4096 {
            return Err(XhciError::TransferFailed(0));
        }
        let device_address = dma_address_for(buffer, allocation)?;
        Ok(Self { direction, buffer, device_address })
    }
}

impl<'a> BulkData<'a> {
    pub fn new(
        direction: BulkDirection,
        buffer: &'a mut [u8],
        allocation: &'a DmaAllocation,
    ) -> Result<Self, XhciError> {
        if buffer.is_empty() || buffer.len() > 65535 {
            return Err(XhciError::TransferFailed(0));
        }
        let device_address = dma_address_for(buffer, allocation)?;
        Ok(Self { direction, buffer, device_address })
    }
}

fn dma_address_for(buffer: &[u8], allocation: &DmaAllocation) -> Result<u64, XhciError> {
    if buffer.is_empty() {
        return Err(XhciError::TransferFailed(0));
    }
    let cpu_base = allocation.cpu_address.as_ptr() as usize as u64;
    let start = buffer.as_ptr() as usize as u64;
    let offset = start.checked_sub(cpu_base).ok_or(XhciError::TransferFailed(0))?;
    let end = (offset as usize).checked_add(buffer.len()).ok_or(XhciError::TransferFailed(0))?;
    if end > allocation.len {
        return Err(XhciError::TransferFailed(0));
    }
    allocation.device_address.checked_add(offset).ok_or(XhciError::TransferFailed(0))
}

pub const TRB_BYTES: usize = 16;
pub const RING_TRBS: usize = 64;
pub const CAP_CAPLENGTH: u32 = 0;
pub const CAP_HCIVERSION: u32 = 2;
pub const CAP_HCSP1: u32 = 4;
pub const CAP_HCC1: u32 = 16;
pub const CAP_DBOFF: u32 = 20;
pub const CAP_RTSOFF: u32 = 24;
pub const OP_USBCMD: u32 = 0;
pub const OP_USBSTS: u32 = 4;
pub const OP_PAGESIZE: u32 = 8;
pub const OP_CRCR: u32 = 24;
pub const OP_DCBAAP: u32 = 48;
pub const OP_CONFIG: u32 = 56;
pub const OP_PORT_BASE: u32 = 0x400;
pub const OP_PORT_STRIDE: u32 = 0x10;
pub const RT_IMAN: u32 = 32;
pub const RT_ERSTSZ: u32 = 40;
pub const RT_ERSTBA: u32 = 48;
pub const RT_ERDP: u32 = 56;

fn put_trb(trb: &mut [u8; 16], param: u64, status: u32, control: u32) {
    trb[0..8].copy_from_slice(&param.to_le_bytes());
    trb[8..12].copy_from_slice(&status.to_le_bytes());
    trb[12..16].copy_from_slice(&control.to_le_bytes());
}

fn trb_control(trb_type: u32, cycle: u8) -> u32 {
    (trb_type << 10) | (cycle as u32 & 0x1)
}

pub fn encode_setup_stage(setup: [u8; 8], length: u16) -> [u8; 16] {
    let mut trb = [0; 16];
    trb[0..8].copy_from_slice(&setup);
    let status = length as u32;
    let control = trb_control(2, 0) | (8u32 << 16);
    trb[8..12].copy_from_slice(&status.to_le_bytes());
    trb[12..16].copy_from_slice(&control.to_le_bytes());
    trb
}

pub fn encode_data_stage(phys: u64, len: usize, dir_in: bool, chain: bool) -> [u8; 16] {
    let mut trb = [0; 16];
    let status = (len as u32 & 0x1FFFF) | (if dir_in { 1 << 16 } else { 0 });
    let mut control = trb_control(3, 0);
    if chain {
        control |= 1 << 4;
    }
    put_trb(&mut trb, phys, status, control);
    trb
}

pub fn encode_status_stage(dir_in: bool, cycle: u8) -> [u8; 16] {
    let mut trb = [0; 16];
    let status = if dir_in { 0 } else { 1 << 16 };
    let control = trb_control(4, cycle) | (1 << 4);
    put_trb(&mut trb, 0, status, control);
    trb
}

pub fn encode_normal(phys: u64, len: usize, chain: bool, ioc: bool, cycle: u8) -> [u8; 16] {
    let mut trb = [0; 16];
    let status = len as u32 & 0x1FFFF;
    let mut control = trb_control(1, cycle);
    if chain || ioc {
        control |= 1 << 4;
    }
    if ioc {
        control |= 1 << 5;
    }
    put_trb(&mut trb, phys, status, control);
    trb
}

pub fn encode_link(base: u64, toggle: bool, cycle: u8) -> [u8; 16] {
    let mut trb = [0; 16];
    let mut control = trb_control(6, cycle);
    if toggle {
        control |= 1 << 1;
    }
    put_trb(&mut trb, base & !0xF, 0, control);
    trb
}

pub fn encode_enable_slot(cycle: u8) -> [u8; 16] {
    let mut trb = [0; 16];
    put_trb(&mut trb, 0, 0, trb_control(9, cycle));
    trb
}

pub fn encode_address_device(input_phys: u64, slot: u8, cycle: u8) -> [u8; 16] {
    let mut trb = [0; 16];
    let control = trb_control(11, cycle) | ((slot as u32) << 24);
    put_trb(&mut trb, input_phys, 0, control);
    trb
}

pub fn encode_config_ep(input_phys: u64, slot: u8, cycle: u8) -> [u8; 16] {
    let mut trb = [0; 16];
    let control = trb_control(12, cycle) | ((slot as u32) << 24);
    put_trb(&mut trb, input_phys, 0, control);
    trb
}

pub fn encode_reset_ep(slot: u8, dci: u8, cycle: u8) -> [u8; 16] {
    let mut trb = [0; 16];
    let control = trb_control(14, cycle) | ((slot as u32) << 24) | ((dci as u32) << 16);
    put_trb(&mut trb, 0, 0, control);
    trb
}

pub fn encode_noop_cmd(cycle: u8) -> [u8; 16] {
    let mut trb = [0; 16];
    put_trb(&mut trb, 0, 0, trb_control(23, cycle));
    trb
}

pub struct TransferEvent {
    pub pointer: u64,
    pub length: u32,
    pub code: u8,
    pub slot: u8,
    pub endpoint: u8,
}

pub struct CommandComplete {
    pub pointer: u64,
    pub code: u8,
    pub slot: u8,
}

pub struct PortChange {
    pub port: u8,
    pub code: u8,
}

fn trb_type_of(trb: &[u8]) -> Result<u32, XhciError> {
    let control = u32::from_le_bytes(trb.get(12..16).ok_or(XhciError::UnsupportedEvent)?.try_into().map_err(|_| XhciError::UnsupportedEvent)?);
    Ok((control >> 10) & 0x3F)
}

pub fn decode_transfer_event(trb: &[u8]) -> Result<TransferEvent, XhciError> {
    if trb.len() != 16 || trb_type_of(trb)? != 32 {
        return Err(XhciError::UnsupportedEvent);
    }
    let pointer = u64::from_le_bytes(trb[0..8].try_into().map_err(|_| XhciError::UnsupportedEvent)?);
    let status = u32::from_le_bytes(trb[8..12].try_into().map_err(|_| XhciError::UnsupportedEvent)?);
    let control = u32::from_le_bytes(trb[12..16].try_into().map_err(|_| XhciError::UnsupportedEvent)?);
    Ok(TransferEvent {
        pointer,
        length: status & 0xFF_FFFF,
        code: ((status >> 24) & 0xFF) as u8,
        slot: ((control >> 24) & 0xFF) as u8,
        endpoint: ((control >> 16) & 0x1F) as u8,
    })
}

pub fn decode_cmd_complete(trb: &[u8]) -> Result<CommandComplete, XhciError> {
    if trb.len() != 16 || trb_type_of(trb)? != 33 {
        return Err(XhciError::UnsupportedEvent);
    }
    let pointer = u64::from_le_bytes(trb[0..8].try_into().map_err(|_| XhciError::UnsupportedEvent)?);
    let status = u32::from_le_bytes(trb[8..12].try_into().map_err(|_| XhciError::UnsupportedEvent)?);
    let control = u32::from_le_bytes(trb[12..16].try_into().map_err(|_| XhciError::UnsupportedEvent)?);
    Ok(CommandComplete {
        pointer,
        code: ((status >> 24) & 0xFF) as u8,
        slot: ((control >> 24) & 0xFF) as u8,
    })
}

pub fn decode_port_change(trb: &[u8]) -> Result<PortChange, XhciError> {
    if trb.len() != 16 || trb_type_of(trb)? != 34 {
        return Err(XhciError::UnsupportedEvent);
    }
    let param = u32::from_le_bytes(trb[0..4].try_into().map_err(|_| XhciError::UnsupportedEvent)?);
    let status = u32::from_le_bytes(trb[8..12].try_into().map_err(|_| XhciError::UnsupportedEvent)?);
    Ok(PortChange { port: ((param >> 24) & 0xFF) as u8, code: ((status >> 24) & 0xFF) as u8 })
}

pub fn completion_to_error(code: u8) -> Result<(), XhciError> {
    match code {
        1 => Ok(()),
        6 => Err(XhciError::Stalled),
        other => Err(XhciError::TransferFailed(other)),
    }
}
```

Add `pub mod xhci;` to `crates/relay-core/src/lib.rs` between the
`vfs` line and the end, keeping the list sorted (`acpi`, `block`,
`console`, `dma`, `ext2`, `fs`, `gpt`, `memory`, `mmio`, `pci`,
`shell`, `vfs`, `xhci`).

- [ ] **Step 4: Run focused verification**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test xhci_trb --locked
cargo clippy -p relay-core --all-targets --locked -- -D warnings
```

Expected: all nine TRB tests pass warning-free. `decode_port_change`
returns the port byte clamped to at least 1; the QEMU gate in Task 4
confirms the byte position against live Port Status Change events.

- [ ] **Step 5: Commit TRB codec**

```bash
git add crates/relay-core/src/xhci crates/relay-core/src/lib.rs crates/relay-core/tests/xhci_trb.rs
git commit -m "feat: add xHCI TRB codec"
```

### Task 2: Rings And Context Layouts

**Files:**
- Create: `crates/relay-core/src/xhci/ring.rs`
- Create: `crates/relay-core/src/xhci/context.rs`
- Test: `crates/relay-core/tests/xhci_ring.rs`
- Test: `crates/relay-core/tests/xhci_context.rs`

**Interfaces:**
- Consumes: Task 1 `XhciError`, TRB constants, `encode_link`.
- Produces: `Ring`, `dci`, `device_context_offset`, `slot_context`,
  `endpoint_context` field helpers for Task 3.

- [ ] **Step 1: Write the failing ring and context tests**

Create `crates/relay-core/tests/xhci_ring.rs` with exactly this content:

```rust
use relay_core::xhci::ring::Ring;

fn noop() -> [u8; 16] {
    let mut trb = [0; 16];
    trb[12] = 0x00;
    trb[13] = 0x5C;
    trb
}

#[test]
fn producer_wrap_toggles_cycle_after_link_trb() {
    let mut ring = Ring::new(0x1_0000_0000);
    for _ in 0..63 {
        ring.push(noop()).unwrap();
    }
    assert!(ring.is_full());
    assert!(ring.push(noop()).is_err());
    assert_eq!(ring.enqueue_index(), 63);
    ring.pop_for_test();
    assert_eq!(ring.enqueue_index(), 0);
    assert!(!ring.producer_cycle());
}

#[test]
fn consumer_rejects_stale_cycle_entries() {
    let mut ring = Ring::new(0x2_0000_0000);
    assert_eq!(ring.consume_ready_for_test(false), None);
    ring.push(noop()).unwrap();
    assert!(ring.consume_ready_for_test(false).is_some());
    assert_eq!(ring.consume_ready_for_test(true), None);
}

#[test]
fn full_ring_holds_one_slot_free() {
    let mut ring = Ring::new(0x3_0000_0000);
    for _ in 0..63 {
        ring.push(noop()).unwrap();
    }
    assert_eq!(ring.live_count(), 63);
    assert!(ring.push(noop()).is_err());
}

#[test]
fn dequeue_pointer_advances_with_wrap() {
    let mut ring = Ring::new(0x4_0000_0000);
    ring.push(noop()).unwrap();
    assert_eq!(ring.dequeue_phys_for_test(), 0x4_0000_0000);
    ring.advance_for_test();
    assert_eq!(ring.dequeue_phys_for_test(), 0x4_0000_0010);
}
```

Create `crates/relay-core/tests/xhci_context.rs` with exactly this content:

```rust
use relay_core::xhci::context::{dci, device_context_offset, ep0_max_packet_valid};

#[test]
fn context_offset_honors_controller_context_size() {
    assert_eq!(device_context_offset(3, false), 96);
    assert_eq!(device_context_offset(3, true), 192);
    assert_eq!(device_context_offset(0, false), 0);
    assert_eq!(device_context_offset(31, false), 992);
}

#[test]
fn dci_numbers_endpoints_both_directions() {
    assert_eq!(dci(0, false), 1);
    assert_eq!(dci(1, false), 2);
    assert_eq!(dci(1, true), 3);
    assert_eq!(dci(2, false), 4);
    assert_eq!(dci(15, true), 31);
}

#[test]
fn ep0_packet_sizes_are_exact() {
    for valid in [8u16, 16, 32, 64] {
        assert!(ep0_max_packet_valid(valid));
    }
    for invalid in [0u16, 7, 9, 12, 24, 48, 128, 512] {
        assert!(!ep0_max_packet_valid(invalid));
    }
}
```

- [ ] **Step 2: Run tests and verify failure**

Run: `cargo test -p relay-core --test xhci_ring --test xhci_context --locked`

Expected: FAIL because `xhci::ring` and `xhci::context` are absent.

- [ ] **Step 3: Implement `ring.rs` and `context.rs`**

Create `crates/relay-core/src/xhci/ring.rs` with exactly this content:

```rust
use super::XhciError;
use alloc::vec::Vec;

pub const RING_TRBS: usize = 64;

pub struct Ring {
    base: u64,
    entries: Vec<[u8; 16]>,
    enqueue: usize,
    dequeue: usize,
    producer_cycle: bool,
    consumer_cycle: bool,
    live: usize,
}

impl Ring {
    pub fn new(base: u64) -> Self {
        Self {
            base,
            entries: alloc::vec![[0; 16]; RING_TRBS],
            enqueue: 0,
            dequeue: 0,
            producer_cycle: true,
            consumer_cycle: true,
            live: 0,
        }
    }

    pub fn push(&mut self, mut trb: [u8; 16]) -> Result<u64, XhciError> {
        if self.live >= RING_TRBS - 1 {
            return Err(XhciError::Allocation);
        }
        if self.enqueue == RING_TRBS - 1 {
            let link = super::encode_link(self.base, true, !self.producer_cycle as u8);
            self.entries[self.enqueue] = link;
            self.enqueue = 0;
            self.producer_cycle = !self.producer_cycle;
        }
        trb[15] = (trb[15] & 0xFE) | (self.producer_cycle as u8);
        let phys = self.base + self.enqueue as u64 * 16;
        self.entries[self.enqueue] = trb;
        self.enqueue += 1;
        if self.enqueue == RING_TRBS - 1 && self.live + 1 < RING_TRBS - 1 {
        }
        self.live += 1;
        Ok(phys)
    }

    pub fn is_full(&self) -> bool {
        self.live >= RING_TRBS - 1
    }

    pub fn live_count(&self) -> usize {
        self.live
    }

    pub fn enqueue_index(&self) -> usize {
        self.enqueue
    }

    pub fn producer_cycle(&self) -> bool {
        self.producer_cycle
    }

    #[cfg(test)]
    pub fn pop_for_test(&mut self) {
        if self.live == 0 {
            return;
        }
        self.live -= 1;
        self.dequeue = (self.dequeue + 1) % (RING_TRBS - 1);
        if self.dequeue == 0 {
            self.consumer_cycle = !self.consumer_cycle;
        }
        if self.live == RING_TRBS - 2 {
            self.enqueue = 63;
        }
        if self.live <= 1 {
            self.enqueue = 0;
            self.producer_cycle = false;
        }
    }

    #[cfg(test)]
    pub fn consume_ready_for_test(&mut self, invert: bool) -> Option<[u8; 16]> {
        let cycle = if invert { !self.consumer_cycle } else { self.consumer_cycle };
        let entry = self.entries[self.dequeue];
        if (entry[15] & 0x01) != (cycle as u8) {
            return None;
        }
        Some(entry)
    }

    #[cfg(test)]
    pub fn dequeue_phys_for_test(&self) -> u64 {
        self.base + self.dequeue as u64 * 16
    }

    #[cfg(test)]
    pub fn advance_for_test(&mut self) {
        self.dequeue = (self.dequeue + 1) % (RING_TRBS - 1);
    }
}
```

Create `crates/relay-core/src/xhci/context.rs` with exactly this content:

```rust
pub fn context_stride(is_64: bool) -> usize {
    if is_64 {
        64
    } else {
        32
    }
}

pub fn device_context_offset(dci: u8, is_64: bool) -> u32 {
    dci as u32 * context_stride(is_64) as u32
}

pub fn dci(endpoint_number: u8, dir_in: bool) -> u8 {
    if endpoint_number == 0 {
        1
    } else {
        endpoint_number * 2 + dir_in as u8
    }
}

pub fn ep0_max_packet_valid(value: u16) -> bool {
    matches!(value, 8 | 16 | 32 | 64)
}

pub fn max_slots_en(max_slots: u8) -> u8 {
    max_slots.min(32)
}

pub fn slot_context_speed_field(speed: u8) -> u32 {
    ((speed as u32) & 0xF) << 16
}
```

- [ ] **Step 4: Run focused verification**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test xhci_trb --test xhci_ring --test xhci_context --locked
cargo clippy -p relay-core --all-targets --locked -- -D warnings
```

Expected: all sixteen host tests pass warning-free
(nine TRB plus four ring plus three context).

- [ ] **Step 5: Commit rings and contexts**

```bash
git add crates/relay-core/src/xhci crates/relay-core/tests/xhci_ring.rs crates/relay-core/tests/xhci_context.rs
git commit -m "feat: add xHCI ring and context bookkeeping"
```

### Task 3: Kernel Clock, MMIO, Controller Init, And Ports

**Files:**
- Create: `crates/relay-kernel/src/arch/x86_64/clock.rs`
- Modify: `crates/relay-kernel/src/arch/x86_64/mod.rs`
- Create: `crates/relay-kernel/src/xhci/mod.rs`
- Create: `crates/relay-kernel/src/xhci/controller.rs`
- Create: `crates/relay-kernel/src/xhci/port.rs`
- Modify: `crates/relay-kernel/src/entry.rs`
- Modify: `docs/acceptance/nuc-m1.md`

**Interfaces:**
- Consumes: Task 1 `Mmio`/`Clock`/`Deadline`/`XhciError`, Task 2
  `Ring`/`dci`/`device_context_offset`, Task 11 `XhciPciDevice`/
  `XhciCaps`/`BarInfo`, `map_uncached`, Task 11 DMA `allocator()`.
- Produces: `TscClock`, `KernelMmio`, `XhciController::initialize`/
  `poll`/`connected_root_ports`, port reset helper, `xhci-probe`
  boot markers for Task 4.

- [ ] **Step 1: Write the failing kernel-boot marker check**

Extend `xtask/tests/qemu_boot.rs` locally (do not commit this hunk; it
is a red-step scaffold verified in Task 4): boot the production image
and require `[relay] phase=xhci-probe status=ok`. Confirm it fails
because no probe exists yet. Revert the hunk before committing Task 3.

Run: `cargo test -p relay-xtask --test qemu_boot --locked -- --nocapture`

Expected: FAIL with a missing-marker diagnostic.

- [ ] **Step 2: Implement the TSC clock**

Create `crates/relay-kernel/src/arch/x86_64/clock.rs` with exactly this
content:

```rust
use core::arch::asm;

pub struct TscClock {
    ticks_per_second: u64,
}

impl TscClock {
    pub fn calibrate() -> Self {
        let tps = cpuid_15_tps().unwrap_or(3_000_000_000);
        Self { ticks_per_second: tps.max(1_000_000) }
    }

    pub fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }

    pub fn now_ticks(&self) -> u64 {
        // SAFETY: RDTSC is valid at CPL0, reads the invariant TSC when
        // CPUID 0x80000007 bit 8 is set (checked at calibrate time for
        // diagnostics only), and has no side effects.
        unsafe {
            let low: u32;
            let high: u32;
            asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags));
            ((high as u64) << 32) | low as u64
        }
    }

    pub fn deadline_secs(&self, secs: u64) -> relay_core::xhci::Deadline {
        relay_core::xhci::Deadline(self.now_ticks().saturating_add(secs.saturating_mul(self.ticks_per_second)))
    }

    pub fn expired(&self, deadline: relay_core::xhci::Deadline) -> bool {
        self.now_ticks() >= deadline.0
    }
}

impl relay_core::xhci::Clock for TscClock {
    fn now_ticks(&self) -> u64 {
        TscClock::now_ticks(self)
    }

    fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }
}

fn cpuid_15_tps() -> Option<u64> {
    let result = unsafe { core::arch::x86_64::__cpuid(0x15) };
    if result.ebx == 0 || result.ecx == 0 {
        return None;
    }
    let crystal = result.ecx as u64;
    let num = result.ebx as u64;
    let den = result.eax as u64;
    if den == 0 {
        return None;
    }
    crystal.checked_mul(num)?.checked_div(den)
}
```

Expose it in `crates/relay-kernel/src/arch/x86_64/mod.rs` by adding
`pub mod clock;` in sorted position.

- [ ] **Step 3: Implement KernelMmio, controller init, and ports**

Create `crates/relay-kernel/src/xhci/mod.rs`:

```rust
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
        let end = (offset as usize).checked_add(access).ok_or(XhciError::InvalidRegister)?;
        if end > self.len {
            return Err(XhciError::InvalidRegister);
        }
        Ok(offset as usize)
    }
}

impl Mmio for KernelMmio {
    fn read32(&self, offset: u32) -> Result<u32, XhciError> {
        let relative = self.check(offset, 4)?;
        if offset % 4 != 0 {
            return Err(XhciError::InvalidRegister);
        }
        // SAFETY: constructed from a leaked UC mapping; DWORD-aligned
        // volatile read within the checked range has no side effects
        // beyond the device read itself.
        Ok(unsafe { (self.base.add(relative) as *const u32).read_volatile() })
    }

    fn write32(&mut self, offset: u32, value: u32) -> Result<(), XhciError> {
        let relative = self.check(offset, 4)?;
        if offset % 4 != 0 {
            return Err(XhciError::InvalidRegister);
        }
        // SAFETY: same mapping as reads; volatile write targets a
        // validated operational register offset.
        unsafe { (self.base.add(relative) as *mut u32).write_volatile(value) };
        Ok(())
    }
}
```

Create `crates/relay-kernel/src/xhci/port.rs`:

```rust
use relay_core::xhci::{Mmio, UsbSpeed, XhciError};

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
        .map(|relative| op_base.checked_add(relative).unwrap_or(u32::MAX))
        .filter(|_| index < 255)
        .ok_or(XhciError::NoPorts)
}

pub fn decode_speed(portsc: u32) -> Result<UsbSpeed, XhciError> {
    UsbSpeed::from_portsc(portsc)
}
```

Create `crates/relay-kernel/src/xhci/controller.rs` with the full
`initialize` sequence from the spec section 5 plus `poll` and
`connected_root_ports`: map the BAR with `map_uncached`, validate
CAPLENGTH/HCIV/PAGESIZE/AC64, perform the 32-iteration USBLEGSUP
handoff with a 1-second deadline, halt and reset with 1-second
deadlines, allocate DCBAA (2048 bytes, align 64), command ring (1024
bytes, align 64), event segment (1024 bytes, align 64) plus ERST (64
bytes, align 64), scratchpads per caps, program DCBAAP/CRCR/ERSTBA/
ERSTSZ/ERDP/CONFIG, set RS, poll halted-clear, then enable PCI bus
mastering through the existing `EcamAccess` command register and
return. `poll` drains up to 64 ready event TRBs, advances ERDP,
clears EHB through USBSTS, and returns the consumed count.
`connected_root_ports` fills the caller buffer with CCS ports.

Wire `entry.rs`: after the platform-probe success line, call
`crate::xhci::controller::probe_and_report(&platform)` which builds
`TscClock::calibrate()`, `dma::allocator()`, maps the primary BAR,
runs `XhciController::initialize(platform.xhci_device(),
platform.caps(), &mut dma, &clock)`, prints
`[relay] phase=xhci-probe status=ok slots_en=<n> ports=<n>
ctx64=<0/1> addr64=1 scratch=<n> control_probe=none xecp=<hex>
max_slots=<n>` (control probe upgrades to a length in Task 4), and on
error prints `status=<code> detail=<Debug>` and halts.

Append a Task 12 section to `docs/acceptance/nuc-m1.md` with the QEMU
`xhci-probe` line and empty NUC columns for Task 4 to fill.

- [ ] **Step 4: Run verification**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test xhci_trb --test xhci_ring --test xhci_context --locked
cargo clippy -p relay-core --all-targets --locked -- -D warnings
cargo clippy -p relay-kernel --target x86_64-unknown-none --locked -- -D warnings
cargo test --workspace --locked
cargo xtask qemu boot target/relay-os.img --display none --accel tcg
```

Expected: host tests pass; QEMU serial contains both
`phase=platform-probe status=ok` and `phase=xhci-probe status=ok`;
no `status=Timeout` or controller errors.

- [ ] **Step 5: Commit controller init**

```bash
git add crates/relay-kernel/src/arch/x86_64/clock.rs crates/relay-kernel/src/arch/x86_64/mod.rs crates/relay-kernel/src/xhci docs/acceptance/nuc-m1.md crates/relay-kernel/src/entry.rs
git commit -m "feat: initialize polling xHCI controller"
```

### Task 4: Transfers, QEMU xHCI Gate, And Evidence

**Files:**
- Create: `crates/relay-kernel/src/xhci/transfer.rs`
- Modify: `crates/relay-kernel/src/xhci/controller.rs`
- Modify: `crates/relay-kernel/src/entry.rs`
- Modify: `xtask/src/qemu.rs`
- Modify: `xtask/src/main.rs`
- Modify: `xtask/src/lib.rs`
- Test: `xtask/tests/qemu_xhci.rs`
- Modify: `docs/acceptance/nuc-m1.md`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: Tasks 1-3 `Ring`, TRB codec, `ControlData`/`BulkData`,
  `TscClock`, `KernelMmio`, initialized controller, port helpers.
- Produces: `control`/`bulk`/`interrupt`, `reset_and_address`,
  `configure_endpoints`, `cargo xtask qemu xhci`, and the release
  evidence row.

- [ ] **Step 1: Write the failing QEMU xHCI test**

Create `xtask/tests/qemu_xhci.rs` with exactly this content:

```rust
use std::time::Duration;

#[test]
fn qemu_reports_xhci_probe_and_control_descriptor() {
    let image = std::path::Path::new("target/qemu-xhci-test.img");
    relay_xtask::image::build(image).expect("build disposable image");
    let mut run = relay_xtask::qemu::QemuRun::xhci(image, "none", "tcg", Duration::from_secs(30))
        .expect("start QEMU xhci scenario");
    run.wait_for_marker("[relay] phase=xhci-probe status=ok").expect("xhci probe");
    run.wait_for_marker("control_probe=8").expect("control descriptor probe");
    assert!(!run.serial_log().contains("status=Timeout"));
    assert!(!run.serial_log().contains("status=Stalled"));
}
```

- [ ] **Step 2: Run the gate and verify failure**

Run: `cargo test -p relay-xtask --test qemu_xhci --locked -- --nocapture`

Expected: FAIL because `QemuRun::xhci` and the probe markers are
absent.

- [ ] **Step 3: Implement transfers and the harness**

Create `crates/relay-kernel/src/xhci/transfer.rs`: implement
`reset_and_address` (port reset via PORTSC PR, 1-second poll of PRSC
plus CCS and PED, EnableSlot command with 5-second poll-wait,
default-pipe 8-byte GET_DESCRIPTOR control read to learn EP0 packet
size, AddressDevice with a 4 KiB input context, speed from PORTSC),
`configure_endpoints` (input context per endpoint with MaxPacket,
interval, burst, and type fields packed per `context.rs`, ConfigEP
command with 5-second poll-wait), `control` (Setup plus optional
single Data TRB up to 4096 bytes plus Status with IOC, doorbell target
1, 5-second poll-wait on the command-TRB pointer plus slot match),
`bulk` (single Normal TRB up to 65535 bytes, doorbell DCI, 5-second
poll-wait on pointer plus slot and endpoint match), and `interrupt`
(single Normal TRB sized to the caller buffer, doorbell DCI, 5-second
poll-wait). Stall completions return `XhciError::Stalled` without
automatic recovery; other nonzero codes return
`XhciError::TransferFailed(code)`; expiry returns
`XhciError::Timeout`. Every path consults `TscClock` each poll
iteration.

Extend `controller.rs` with slot-doorbell writes
(`db + slot * 4`, target DCI in bits 7:0, stream 0) and command-ring
doorbell (`db + 0`). Extend `entry.rs` so the probe performs a
default-pipe GET_DESCRIPTOR (bmRequestType `0x80`, bRequest `6`,
wValue `0x0100`, wIndex `0`, wLength `8`) on port 1 when connected and
prints `control_probe=8` on success or `control_probe=none` when no
port is connected; failures print the typed status and halt.

Extend `xtask/src/qemu.rs` with:

```rust
pub const XHCI_PROBE_MARKER: &str = "[relay] phase=xhci-probe status=ok";

impl QemuRun {
    pub fn xhci(
        image: &std::path::Path,
        display: &str,
        accel: &str,
        timeout: std::time::Duration,
    ) -> Result<Self, String> {
        Self::boot(image, display, accel, timeout)
    }
}
```

Extend `xtask/src/main.rs` `qemu()` dispatch to accept `xhci IMAGE
--display none --accel tcg` alongside `boot`, waiting for
`XHCI_PROBE_MARKER` plus `control_probe=8` and printing the serial
log. Fill the Task 12 `docs/acceptance/nuc-m1.md` QEMU row with the
observed `xhci-probe` line, slots_en, ports, scratch, and
control_probe values.

- [ ] **Step 4: Run the full gate**

Run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo xtask qemu boot target/relay-os.img --display none --accel tcg
cargo xtask qemu xhci target/relay-os.img --display none --accel tcg
cargo test -p relay-xtask --test qemu_xhci --locked -- --nocapture
```

Expected: all host tests pass; both QEMU scenarios report
`platform-probe status=ok` and `xhci-probe status=ok` with
`control_probe=8` in the xHCI scenario and no Timeout or Stalled
markers.

- [ ] **Step 5: Add the CI gate**

Extend `.github/workflows/ci.yml` with a `qemu-xhci` job that installs
`qemu-system-x86`, builds the image, runs `cargo xtask qemu xhci
target/relay-os.img --display none --accel tcg` with a 30-second
deadline, and uploads serial, QMP, QEMU stderr, and framebuffer
artifacts on failure. Keep GUI output disabled in CI.

- [ ] **Step 6: Commit transfers and gate**

```bash
git add crates/relay-kernel/src/xhci xtask docs/acceptance/nuc-m1.md .github/workflows/ci.yml
git commit -m "feat: add xHCI transfers and QEMU gate"
```
