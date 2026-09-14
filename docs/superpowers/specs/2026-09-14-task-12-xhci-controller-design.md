# Task 12 xHCI Rings And Polling Controller Design

## Status

Draft for review on 2026-09-14. Follows Approach A (host-tested core plus
thin kernel adapters, Task 11 pattern) and scope A (full control/bulk/
interrupt transfers now) approved in brainstorming.

## Objective

Task 12 gives Relay OS a polling xHCI controller that Tasks 13-15 can use
for USB enumeration, HID input, and BOT storage. It initializes the
primary controller discovered by Task 11, owns its rings, contexts,
scratchpads, ports, and slots, and exposes blocking control, bulk, and
interrupt transfers over DMA-backed buffers with bounded deadlines. It
ends with a diagnostic QEMU/NUC probe that records the controller
assumptions Tasks 13-14 will build on.

Milestone One Task 12 (2026-09-07 plan, lines 1092-1195) is not specific
enough for implementation: it names no TRB layouts or type codes, no
register offsets or init order, no context/DCBAA/scratchpad sizes, no
DMA validation limits, no error shapes, no clock source, and no harness
command. This document resolves those gaps. Rulings from brainstorming,
all approved: full transfers now, allocate-scratchpads-per-caps,
TSC-calibrated clock, and a new `cargo xtask qemu xhci` gate.

## Scope And Ruling

Task 12 adds exactly what controller bring-up needs and nothing else:

- Pure `relay-core` xHCI math over injected traits: `xhci/mod.rs`
  (register constants, TRB encode/decode, `Deadline`/`Clock`/`Mmio`
  traits, `XhciError`), `xhci/ring.rs` (producer/consumer cycle
  bookkeeping), `xhci/context.rs` (32/64-byte offsets, DCI math, slot
  and endpoint field packing).
- Thin `relay-kernel` adapters: `arch/x86_64/clock.rs` (TSC clock),
  `xhci/mod.rs` (volatile `Mmio` over the mapped BAR), `xhci/
  controller.rs` (initialize, poll, slots, scratchpads), `xhci/port.rs`
  (PORTSC decode and reset), `xhci/transfer.rs` (control/bulk/interrupt
  queue plus poll-wait correlation).
- A diagnostic controller probe in the boot path plus a Task 12 section
  in `docs/acceptance/nuc-m1.md` and a `qemu xhci` harness gate.

It adds no USB descriptor, HID, BOT, or SCSI policy (Tasks 13-14), no
hub or hot-plug support, no USB interrupts or async I/O, no second-
controller bring-up, no IOMMU domains, no DMA free path, and no new
page-table code beyond reuse of the Task 11 `map_uncached` installer.

## Module Boundaries

```text
crates/relay-core/src/xhci/mod.rs        XhciError, reg consts, TRB codec, Deadline/Clock/Mmio
crates/relay-core/src/xhci/ring.rs       Ring with producer-cycle, Link TRB, full/empty, consumer wrap
crates/relay-core/src/xhci/context.rs    context stride/offsets, DCI, slot/ep field packing
crates/relay-core/src/lib.rs             add pub mod xhci
crates/relay-kernel/src/arch/x86_64/clock.rs
                                         TscClock over RDTSC plus calibration
crates/relay-kernel/src/xhci/mod.rs      KernelMmio volatile glue, re-exports
crates/relay-kernel/src/xhci/controller.rs
                                         initialize, poll, slots, scratchpads, doorbells
crates/relay-kernel/src/xhci/port.rs     PORTSC decode, reset, CCS/PED polling
crates/relay-kernel/src/xhci/transfer.rs control/bulk/interrupt queue and correlation
crates/relay-kernel/src/entry.rs         run xhci-probe after platform-probe
xtask/src/qemu.rs                        add qemu xhci subcommand
xtask/tests/qemu_xhci.rs                 gate test for the new subcommand
docs/acceptance/nuc-m1.md                Task 12 probe table (QEMU now, NUC columns pending)
crates/relay-core/tests/xhci_ring.rs     ring suite
crates/relay-core/tests/xhci_context.rs  context suite
crates/relay-core/tests/xhci_trb.rs      TRB codec suite
```

`relay-core` stays `#![no_std]` plus `alloc`, with no unsafe code in the
three new modules. All register bytes, TRB fields, lengths, addresses,
and arithmetic are checked; malformed firmware or device data returns
typed errors and never panics. Every unsafe block lives in the kernel
adapters and documents its invariant. No new crates are added; the
`xhci 0.9.2` definitions crate remains unused per `docs/dependencies.md`.

## Public API

```rust
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
pub enum UsbSpeed { Full = 1, High = 3, Super = 4, SuperPlus = 5 }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceHandle { pub slot_id: u8, pub root_port: u8, pub speed: UsbSpeed }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointConfig {
    pub address: u8,
    pub transfer_type: u8,
    pub max_packet_size: u16,
    pub interval: u8,
    pub max_burst: u8,
}

pub enum ControlDirection { In, Out, None }

pub struct ControlData<'a> {
    pub direction: ControlDirection,
    pub buffer: &'a mut [u8],
    pub allocation: &'a DmaAllocation,
}

pub enum BulkDirection { In, Out }

pub struct BulkData<'a> {
    pub direction: BulkDirection,
    pub buffer: &'a mut [u8],
    pub allocation: &'a DmaAllocation,
}

impl<'a> ControlData<'a> {
    pub fn new(
        direction: ControlDirection,
        buffer: &'a mut [u8],
        allocation: &'a DmaAllocation,
    ) -> Result<Self, XhciError>;
}

impl<'a> BulkData<'a> {
    pub fn new(
        direction: BulkDirection,
        buffer: &'a mut [u8],
        allocation: &'a DmaAllocation,
    ) -> Result<Self, XhciError>;
}

impl XhciController {
    pub fn initialize(
        pci: XhciPciDevice,
        caps: XhciCaps,
        dma: &mut impl DmaAllocator,
        clock: &impl Clock,
    ) -> Result<Self, XhciError>;
    pub fn poll(&mut self) -> Result<usize, XhciError>;
    pub fn connected_root_ports(&self, output: &mut [u8]) -> Result<usize, XhciError>;
    pub fn reset_and_address(
        &mut self,
        port: u8,
        deadline: Deadline,
    ) -> Result<DeviceHandle, XhciError>;
    pub fn configure_endpoints(
        &mut self,
        device: DeviceHandle,
        endpoints: &[EndpointConfig],
        deadline: Deadline,
    ) -> Result<(), XhciError>;
    pub fn control(
        &mut self,
        device: DeviceHandle,
        setup: [u8; 8],
        data: Option<ControlData<'_>>,
        deadline: Deadline,
    ) -> Result<usize, XhciError>;
    pub fn bulk(
        &mut self,
        device: DeviceHandle,
        endpoint: u8,
        data: BulkData<'_>,
        deadline: Deadline,
    ) -> Result<usize, XhciError>;
    pub fn interrupt(
        &mut self,
        device: DeviceHandle,
        endpoint: u8,
        buffer: &mut [u8],
        deadline: Deadline,
    ) -> Result<usize, XhciError>;
}
```

`ControlData` and `BulkData` borrow DMA-backed buffers only. Each
constructor verifies the buffer lies entirely within its `DmaAllocation`
(`allocation.cpu_address` range covers the buffer, `allocation.len`
bounds the length) and rejects empty control-data buffers over 4096
bytes or bulk buffers over 65535 bytes; neither type can be built from
an arbitrary kernel slice. `interrupt` takes a plain `&mut [u8]` plus
the caller-held `DmaAllocation` implicitly through the transfer-ring
requirement documented in `transfer.rs`: the implementation checks the
buffer against the endpoint's DMA window before queuing. `configure_endpoints`
programs EP0 max-packet updates and later bulk/interrupt contexts;
Tasks 13-14 supply descriptor-derived values and Task 12 only programs
contexts. `UsbSpeed` values are the PORTSC numeric codes so Tasks 13-14
share one encoding.

## Register Model

All offsets are byte offsets from the mapped BAR base. Capability
registers at `base + 0`:

```text
CAPLENGTH  byte  0      operational base = base + CAPLENGTH
HCIVERSION u16   2      must be >= 0x0100
HCSPARAMS1 u32   4      bits 7:0 MaxSlots, bits 31:24 MaxPorts
HCSPARAMS2 u32   8      informational only
HCSPARAMS3 u32   12     informational only
HCCPARAMS1 u32   16     bit 0 AC64 (required true), bit 2 CSZ, bits 15:12 MaxPSASize,
                       bit 9 SPC, bits 31:16 xECP (DWORD units)
DBOFF      u32   20     doorbell array byte offset
RTSOFF     u32   24     runtime register byte offset
```

Operational registers at `op = base + CAPLENGTH`:

```text
USBCMD     u32   op+0    RS bit 0, HCRST bit 1, INTE bit 2 (kept 0: polling)
USBSTS     u32   op+4    HCHalted bit 0, CNR bit 11, EHB bit 3 (write-1-to-clear)
PAGESIZE   u32   op+8    bit 0 must be set (4 KiB supported)
DNCTRL     u32   op+20   kept 0 (no device-notification use)
CRCR       u64   op+24   command-ring phys bits 63:6 plus cycle in bit 0
DCBAAP     u64   op+48   DCBAA phys, 64-byte aligned
CONFIG     u32   op+56   bits 7:0 MaxSlotsEn
PORTSC     u32   op+0x400+(port-1)*0x10   CCS bit 0, PED bit 1, PR bit 4, PRSC bit 5,
                                         speed bits 13:10, PP bit 9
```

Runtime registers at `rt = base + RTSOFF`, interrupter 0 only:

```text
MFINDEX    u32   rt+0    informational only
IMAN       u32   rt+32   IP bit 0 (write-1-to-clear), IE bit 1 (kept 0: polling)
IMOD       u32   rt+36   kept 0
ERSTSZ     u32   rt+40   kept 1 (one segment)
ERSTBA     u64   rt+48   ERST phys, 64-byte aligned
ERDP       u64   rt+56   event-ring dequeue phys bits 63:4 plus EHB-equivalent handling
```

Doorbells at `db = base + DBOFF`: `db + 0` is the command doorbell
(target 0); `db + slot * 4` is the slot doorbell where the written value
is the endpoint target (0 for command completion accounting, 1 for EP0
control, DCI for other endpoints) with stream ID 0 in bits 31:16.

Every MMIO offset is range-checked against `bar.size` before access;
out-of-range offsets are `XhciError::InvalidRegister` without I/O.
Kernel `KernelMmio` performs DWORD-aligned volatile reads and writes
only, matching the QEMU xHCI model constraint already documented in
Task 11.

## Ring Semantics

Each ring holds exactly 64 TRBs of 16 bytes (1024 bytes, 64-byte
aligned, DMA, zeroed). The last TRB of every command and transfer ring
is a Link TRB pointing at the ring base with toggle-cycle set.

Producer state is `enqueue_index` in `0..64` plus `producer_cycle`
(0 or 1). Enqueue writes the TRB with cycle equal to `producer_cycle`,
advances the index, and when the index reaches 63 writes the Link TRB
with the toggled cycle, wraps the index to 0, and flips
`producer_cycle`. A ring holding 63 live TRBs reports full; the 64th
slot is never used for payload so the empty/full distinction never
needs an extra counter.

Consumer state for the single event ring is `dequeue_index` plus
`consumer_cycle`. `poll()` reads the entry at `dequeue_index`: a cycle
mismatch means empty and stops the scan; a match is processed, the
index advances with Link-toggle handling identical to the producer, and
ERDP is advanced to the consumed phys plus EHB cleared through USBSTS.
More than 64 matched entries without an empty entry is
`XhciError::TransferFailed` (ring overrun, never silent).

Command completions are matched by command-TRB physical pointer;
transfer events are matched by the triple of slot ID, endpoint DCI, and
transfer-TRB physical pointer. Non-matching events are consumed,
counted in the `poll()` return, and never delivered to the wrong
waiter. Unknown TRB types and completion codes outside the supported
set are `UnsupportedEvent` or `TransferFailed(code)` respectively.

## TRB Codec

A TRB is 16 little-endian bytes `[param_lo32, param_hi32, status32,
control32]` with the cycle in control bit 0. Supported type codes in
control bits 15:10 per xHCI section 6.4:

```text
Normal=1  Setup=2  Data=3  Status=4  Link=6  Transfer-NoOp=8
EnableSlot=9  DisableSlot=10  AddressDevice=11  ConfigEP=12
EvalContext=13  ResetEP=14  StopEP=15  SetTRDeq=16  NoOpCmd=23
TransferEvent=32  CmdComplete=33  PortChange=34
```

Encode helpers validate every field: Setup carries the 8-byte setup
packet inline with transfer-type and direction bits from `bmRequestType`
and `wLength`; Data carries a DMA phys plus length below 64 KiB with
direction and chain bits; Status carries IOC on the terminal TRB only;
Normal carries a DMA phys plus length with chain and IOC as directed;
Link carries the ring-base phys with toggle set; EnableSlot carries the
slot type byte 0 (kept 0); AddressDevice and ConfigEP carry the input-
context phys with BSA clear; ResetEP and StopEP carry slot and endpoint
DCI; SetTRDeq carries the transfer-ring phys plus DCI and cycle state.
Decode helpers parse TransferEvent (pointer, length, code, slot,
endpoint), CmdComplete (pointer, code, slot), and PortChange (port ID)
with bounds-checked slice reads. Completion code 1 (success) continues;
code 6 (stalled) maps to `XhciError::Stalled`; any other nonzero code
maps to `XhciError::TransferFailed(code)`.

## Context, DCBAA, And Scratchpad Semantics

The context stride comes from Task 11 caps: 32 bytes when
`context_64` is false, 64 bytes when true. Both observed targets report
32-byte contexts and the design programs either stride with one code
path. `dci(endpoint_number, direction_out)` returns `endpoint_number *
2 + direction_out as u8` with EP0 fixed at DCI 1. `device_context_offset
(dci, stride)` returns `dci as u32 * stride as u32` with the slot
context at offset 0.

DCBAA is 2048 bytes (256 eight-byte entries, 64-byte aligned, DMA,
zeroed). Entry 0 is the scratchpad array pointer; entries 1 through
`MaxSlotsEn` are device-context pointers programmed on EnableSlot;
entries above `MaxSlotsEn` stay zero. Each device context occupies one
4 KiB DMA page (covers 1024 bytes at 32-byte stride and 2048 bytes at
64-byte stride). Each input context occupies one 4 KiB DMA page (covers
the control plus slot plus 31 endpoint contexts at either stride).
Input-context pages are leak-only within Milestone One; no reuse or
free list is provided.

Scratchpads follow the approved per-caps rule. When
`scratchpad_count` is 0 (QEMU), `DCBAA[0]` is programmed zero and no
pages are allocated. When nonzero (NUC reports 128), the driver
allocates exactly that many 4 KiB DMA pages plus one array of `count *
8` bytes (64-byte aligned, DMA) holding their physical addresses and
programs `DCBAA[0]` with the array phys. Allocation failure at any
point aborts `initialize` with the DMA error before the controller is
started.

`MaxSlotsEn` is `min(caps.max_slots, 32)`. QEMU reports 64 so it is
bounded to 32; the NUC value is recorded by the probe and the same bound
applies. DCBAA storage always covers 256 entries regardless of the
bound so `CONFIG` never exceeds the allocated table.

## Controller Initialization Order

`initialize` takes the Task 11 `XhciPciDevice` (BAR base and size) plus
the Task 11 `XhciCaps` snapshot (stride, slot, port, and scratchpad
inputs) so it never re-decodes capabilities from MMIO. It maps the BAR
once through `mmio::map_uncached` and performs these steps in order, each poll
bounded by the stated deadline:

1. Validate CAPLENGTH (at least 32 and within the BAR), HCIVERSION (at
   least 0x0100), PAGESIZE bit 0 set, and HCCPARAMS1 AC64 set. A 64-bit
   addressing capability of zero is `UnsupportedPlatform` because the
   Task 11 DMA boundary hands out 64-bit physical addresses.
2. Walk extended capabilities with a 32-iteration cap from xECP using
   the Task 11 base-relative rule. When USBLEGSUP is present, set the
   OS-owned semaphore (bit 24 of DWORD 0), then poll the BIOS-owned
   semaphore (bit 16) clear with a 1-second deadline, then clear SMI
   enables. Absent USBLEGSUP skips this step.
3. Halt: clear `USBCMD.RS`, poll `USBSTS.HCHalted` set with a 1-second
   deadline. Reset: set `USBCMD.HCRST`, poll `HCRST` clear and `CNR`
   clear with 1-second deadlines each.
4. Allocate DCBAA, command ring, event segment plus ERST, and
   scratchpads per caps. Program DCBAAP, CRCR (command-ring phys with
   cycle 1), ERSTBA, ERSTSZ 1, and ERDP (event-segment phys with no
   pending EHB).
5. Program `CONFIG.MaxSlotsEn`, set `USBCMD.RS`, keep `INTE` clear for
   polling, poll `HCHalted` clear with a 1-second deadline.
6. Enable PCI bus mastering only now, after DMA structures are ready,
   per the Task 11 constraint that left it clear. Return the controller
   with producer cycle 1, consumer cycle 1, zeroed event count, and the
   recorded `max_ports`, `max_slots_en`, and scratchpad count.

Any step failure leaves the controller halted and returns the typed
error; the caller prints the diagnostic marker and halts per the
diagnostic-probe rule below.

## Transfer, Port, And Clock Semantics

`TscClock` reads `RDTSC` at CPL0. Calibration prefers CPUID leaf 0x15
denominator/numerator/crystal ratio when nonzero; otherwise it uses a
nominal 3 GHz with a documented tolerance of plus or minus 30 percent.
Every deadline in this design carries at least a factor-of-two margin
over the fastest correct hardware response, so the tolerance cannot
cause a false timeout on conforming hardware. Host tests use
`TestClock` with manual tick advance and fixed ticks per second.

Deadline constants: ownership handoff 1 second, halt 1 second, reset
1 second each for `HCRST` and `CNR`, start-halted-clear 1 second,
port reset 1 second, address and configure 5 seconds each, control and
bulk and interrupt 5 seconds each. Expiry returns `XhciError::Timeout`;
no loop spins without consulting the clock.

`connected_root_ports` reads PORTSC for ports 1 through `max_ports`
and writes each connected (CCS set) port number into the caller buffer,
returning the count or `XhciError::NoPorts` when none are connected.

`reset_and_address(port)` asserts the port-reset bit, polls reset-
complete plus connected-plus-enabled with the port deadline, issues
EnableSlot to obtain a slot ID, reads the first 8 device bytes through
the default control pipe to learn the EP0 max packet size, then issues
AddressDevice with an input context carrying that size. It returns the
`DeviceHandle` with the PORTSC speed field mapped through `UsbSpeed`;
an unmapped speed value is `UnsupportedPlatform`.

Control transfers enqueue Setup plus an optional single Data TRB (data
length at most 4096 bytes from one DMA buffer) plus Status with IOC on
the terminal TRB only. Bulk transfers enqueue one Normal TRB (length at
most 65535 bytes to fit the transfer-length field in one TRB) with IOC.
Interrupt transfers enqueue one Normal TRB sized to the caller buffer
(HID boot reports use 8 bytes) with IOC. Each method rings the slot
doorbell with target 1 for EP0 or the endpoint DCI otherwise, then
loops on `poll()` until its correlated completion arrives or the
deadline expires. A stalled completion returns `Stalled` without
automatic recovery; Task 14 performs Mass-Storage-Reset plus ResetEP
plus a single safe retry, and Task 13 treats keyboard stalls as fatal
to that device.

## Error Handling And Diagnostics

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciError {
    UnsupportedPlatform,
    UnsupportedEvent,
    InvalidRegister,
    Map(MapError),
    Dma(DmaError),
    Timeout,
    Stalled,
    TransferFailed(u8),
    NoDevice,
    InvalidSlot,
    NoPorts,
    Allocation,
}
```

Every variant is returned, never panicked, for firmware, device, or
resource failures. Saved PCI command state is not disturbed by Task 12;
bus mastering enabled here stays enabled for Tasks 13-15. Kernel
invariant failures keep the existing framebuffer plus QEMU-serial
diagnostic and halt.

`entry.rs` runs the controller probe after the Task 11 platform probe
and before the runtime banner. Success prints one stable line plus one
line per connected port, all mirrored to framebuffer and serial:

```text
[relay] phase=xhci-probe status=ok slots_en=32 ports=4 ctx64=0 addr64=1 scratch=0 control_probe=8 xecp=0x... max_slots=64
[relay] phase=xhci-port status=ok port=1 speed=4
```

Failure prints `phase=xhci-probe status=<code> detail=<Debug>` and
halts; the kernel never falls back to UEFI services and never claims a
clean controller on a failed probe.

## Testing

Host suites use fakes in test support: `VecMmio` (register map with
injectable transport faults) and `TestClock` (manual ticks) plus the
Task 11 `VecFrames` DMA fake.

- `xhci_ring.rs`: producer wrap toggles cycle after the Link TRB,
  63-entry full rejection with one slot held free, consumer wrap
  advances ERDP, stale-cycle entries stop the scan, and command versus
  transfer correlation delivers each completion to the correct waiter.
- `xhci_context.rs`: device-context offsets for DCI 0 through 31 at
  both 32- and 64-byte strides with known vectors, DCI calculation for
  EP0 through EP15 in both directions, and EP0 packet-size acceptance
  for 8, 16, 32, and 64 with rejection of all other values.
- `xhci_trb.rs`: byte-exact encode vectors for Setup, Data, Status,
  Normal, Link, EnableSlot, AddressDevice, ConfigEP, ResetEP, and
  NoOpCmd plus decode vectors for TransferEvent, CmdComplete, and
  PortChange; unknown type codes and truncated slices are rejected.

Verification runs:

```bash
cargo fmt --all --check
cargo test -p relay-core --test xhci_ring --test xhci_context --test xhci_trb --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

QEMU verification runs `cargo xtask qemu xhci target/relay-os.img
--display none --accel tcg` with `-device qemu-xhci,p2=2,p3=2`. It
requires ownership handoff, reset, capabilities, four directly
connected ports with USB3-first ordering, and an 8-byte control-probe
descriptor read, all without controller errors. The existing `qemu
boot` gate must stay green in the same PR.

NUC verification records the `xhci-probe` line and per-port lines in
`docs/acceptance/nuc-m1.md`, compares slots, ports, scratchpad count,
context size, address width, and USB2/USB3 ranges against the Task 11
table, and requires the 128-scratchpad allocation plus primary
`00:14.0` control-probe success. If VT-d translation blocks DMA on the
NUC, execution stops and explicit design approval is required before
either adding an identity-mapped DMA domain or requiring VT-d disabled
in firmware, carried verbatim from the milestone.

## Non-Goals

Task 12 performs no USB descriptor parsing, HID report decoding, BOT
framing, or SCSI commands (Tasks 13-14); wires no shell persistence
path (Task 15); supports no hubs, hot-plug, multiple simultaneous
slot bring-up beyond one diagnostic address, isochronous transfers,
streams, 64 KiB-plus single transfers, virtualization-based IOMMU
domains, DMA free lists, or MSI-X interrupts; and makes no VT-d
firmware requirement beyond recording its state and enforcing the
stop-and-ask rule.
