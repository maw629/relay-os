# Task 11 ACPI MCFG, PCI Discovery, And DMA Boundary Design

## Status

Draft for review on 2026-09-13. Follows Approach A (host-tested core plus
thin kernel adapters reusing the Task 4 mapper) approved in brainstorming.

## Objective

Task 11 gives Relay OS a host-tested platform-discovery layer for the fixed
M1 hardware profile: parse ACPI to find the PCI ECAM window, enumerate PCI
through ECAM to locate the single xHCI controller, safely probe and map its
BAR, and establish the only DMA allocation boundary the xHCI driver
(Tasks 12-15) may use. It ends with a diagnostic NUC probe that records the
controller assumptions Task 12 will build on.

Milestone One Task 11 (2026-09-07 plan, lines 998-1090) is not specific
enough for implementation: it names no ACPI struct layouts or checksum
rules, no ECAM formula or BAR-probe error shapes, no DMA validation limits
or lifetime model, and no kernel mapping path. This document resolves those
gaps. Rulings from brainstorming, all approved: leak-only DMA allocation,
segment-0-only MCFG, and ECAM/BAR mapping through the existing Task 4
checked mapper with no new pager.

## Scope And Ruling

Task 11 adds exactly what xHCI bring-up needs and nothing else:

- Pure `relay-core` parsing over injected traits: `acpi.rs` (RSDP, XSDT,
  RSDT, MCFG), `pci.rs` (ECAM enumeration, xHCI match, safe BAR probe),
  `dma.rs` (leak-only frame-backed allocator contract).
- Thin `relay-kernel` adapters: `arch/x86_64/mmio.rs` (safe wrapper over
  the Task 4 checked mapper), `arch/x86_64/dma.rs` (frame-backed
  `DmaAllocator`), `pci.rs` (ECAM `PciConfig` impl, xHCI discovery, and a
  read-only capability snapshot for diagnostics).
- A diagnostic platform probe in the boot path plus a Task 11 section in
  `docs/acceptance/nuc-m1.md`.

It adds no xHCI operational-register logic or ring code (Task 12), no USB
class policy (Tasks 13-14), no new page-table code, no VT-d DMA-remapping
domain, no multi-segment PCI support, and no DMA free path.

## Module Boundaries

```text
crates/relay-core/src/acpi.rs            RSDP/XSDT/RSDT/MCFG parsing over PhysicalMemory
crates/relay-core/src/pci.rs             PciAddress, PciConfig trait, ECAM walk, xHCI match, BAR probe
crates/relay-core/src/dma.rs             DmaLayout, DmaAllocator, DmaAllocation, validation
crates/relay-core/src/lib.rs             add pub mod acpi/pci/dma
crates/relay-kernel/src/arch/x86_64/mmio.rs
                                         safe uncached-MMIO wrapper over the Task 4 mapper
crates/relay-kernel/src/arch/x86_64/dma.rs
                                         leak-only frame-backed DmaAllocator
crates/relay-kernel/src/pci.rs           ECAM PciConfig impl, discovery, read-only caps snapshot
crates/relay-kernel/src/main.rs          run the diagnostic probe after heap init
docs/acceptance/nuc-m1.md                Task 11 probe table and VT-d record
crates/relay-core/tests/acpi.rs          table-validation suite
crates/relay-core/tests/pci.rs           enumeration and BAR-probe suite
crates/relay-core/tests/dma.rs           layout-validation and allocation suite
```

`relay-core` stays `#![no_std]` plus `alloc`, with no unsafe code in the
three new modules. All table bytes, lengths, addresses, and arithmetic are
checked; malformed firmware data returns typed errors and never panics.
Every unsafe block lives in the kernel adapters and documents its
invariant. `pci_types 0.10.1` (already accepted in `docs/dependencies.md`)
may supply typed config-space definitions; it must not perform I/O.

## Public API

```rust
pub trait PhysicalMemory {
    fn read_exact(&self, physical: u64, output: &mut [u8]) -> Result<(), MemoryError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryError { OutOfRange, Overflow, Transport }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcpiError {
    Truncated, BadSignature, BadLength, Checksum,
    UnsupportedPlatform, InvalidRange, Allocation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct McfgRegion { pub base: u64, pub segment: u16, pub bus_start: u8, pub bus_end: u8 }

pub fn parse_mcfg(memory: &impl PhysicalMemory, rsdp_phys: u64) -> Result<McfgRegion, AcpiError>;
pub fn dmar_present(memory: &impl PhysicalMemory, rsdp_phys: u64) -> Result<bool, AcpiError>;
```

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PciAddress { pub segment: u16, pub bus: u8, pub device: u8, pub function: u8 }

pub trait PciConfig {
    fn read_u32(&self, address: PciAddress, offset: u16) -> Result<u32, PciError>;
    unsafe fn write_u32(&self, address: PciAddress, offset: u16, value: u32) -> Result<(), PciError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciError {
    OutOfRange, Transport, InvalidBar,
    NoXhci, MultipleXhci, UnsupportedPlatform, Allocation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarInfo { pub base: u64, pub size: u64, pub is_64: bool }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciPciDevice { pub address: PciAddress, pub bar: BarInfo }

pub fn ecam_address(region: &McfgRegion, address: PciAddress, offset: u16) -> Result<u64, PciError>;
pub fn enumerate_functions(config: &impl PciConfig) -> Result<alloc::vec::Vec<(PciAddress, u32)>, PciError>;
pub fn find_xhci(config: &impl PciConfig) -> Result<PciAddress, PciError>;
pub fn probe_xhci_bar(config: &impl PciConfig, address: PciAddress) -> Result<BarInfo, PciError>;

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

pub fn decode_xhci_caps(header: &[u8; 32], ext_caps: &[u8]) -> Result<XhciCaps, PciError>;
```

`enumerate_functions` yields `(address, class_code)` with class code packed
as `(class << 16) | (subclass << 8) | prog_if`, skipping absent functions
(`0xFFFF` vendor) and honoring the multifunction bit. `find_xhci` matches
class `0x0C`, subclass `0x03`, prog-if `0x30` on header-type-`0x00`
endpoint devices: zero matches give `NoXhci`, more than one gives
`MultipleXhci`.

```rust
pub struct DmaLayout { pub size: usize, pub align: usize, pub max_address: u64, pub zeroed: bool }

pub trait DmaAllocator {
    fn allocate(&mut self, layout: DmaLayout) -> Result<DmaAllocation, DmaError>;
}

pub struct DmaAllocation {
    pub cpu_address: core::ptr::NonNull<u8>,
    pub device_address: u64,
    pub len: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaError { TooLarge, BadAlign, AddressLimit, NoMemory, Allocation }
```

Allocation is leak-only: there is deliberately no `free` and no `Drop`
that reclaims frames. Handed-out allocations live until reboot, matching
the bump-allocated kernel heap and the forever-lived xHCI structures of
Tasks 12-15.

## ACPI Semantics

`parse_mcfg` reads only through `PhysicalMemory::read_exact` with checked
physical arithmetic on every step:

1. Read 24 bytes at `rsdp_phys`: signature must be `RSD PTR `, else
   `BadSignature`; the 8-bit checksum of the first 20 bytes must be zero,
   else `Checksum`; revision byte selects the path (0-1: RSDT via the
   32-bit pointer at offset 16; 2 and up: XSDT via the 64-bit pointer at
   offset 24, after validating the 36-byte extended checksum).
2. Read the 36-byte SDT header at the chosen pointer: signature must be
   `XSDT` or `RSDT` respectively, else `BadSignature`; `length` must be
   at least 36 and at most 64 KiB, else `BadLength`; the checksum over
   `length` bytes must be zero, else `Checksum`.
3. Walk the entry array (`length - 36` divided by 8 for XSDT, by 4 for
   RSDT; a nonzero remainder is `BadLength`). For each entry pointer,
   read 44 header bytes; entries with signature `MCFG` are candidates.
   No `MCFG` anywhere is `UnsupportedPlatform` (the milestone's specific
   unsupported-platform error for a missing window).
4. A candidate MCFG validates as: `length` exactly `44 + 16` (one entry;
   anything else is `UnsupportedPlatform` per the segment-0-only ruling),
   checksum over `length` bytes zero, entry segment zero, `bus_start <=
   bus_end`, base 4 KiB-aligned and nonzero, and `base + ((bus_end -
   bus_start + 1) << 20)` free of overflow and within the kernel direct
   map (`end - 1 <= 0x7FFF_FFFF_FFFF`, the `MAX_DIRECT_MAPPED_PHYSICAL`
   ceiling already enforced at entry). Violations are `InvalidRange`,
   except multi-entry or nonzero-segment shapes which stay
   `UnsupportedPlatform` so the diagnostic clearly blames the platform
   profile rather than corruption.

`dmar_present` reuses the same validated walk and reports whether any
entry has signature `DMAR`. It returns `Ok(false)` (not an error) when
tables are valid but DMAR is absent; malformed tables propagate the same
`AcpiError`. Table reads are chunked through fixed small stack buffers
(header first, then bounded body); no allocation scales with firmware
claims beyond one entry-array `Vec` capped by the 64 KiB table limit.

## PCI Semantics

ECAM byte address for a 4-aligned `offset < 4096` on segment 0:

```text
addr = region.base + (bus << 20) + (device << 15) + (function << 12) + offset
```

All shifts and adds are checked; `device >= 32`, `function >= 8`,
unaligned or out-of-range offsets, and bus outside
`bus_start..=bus_end` are `OutOfRange` without I/O.

Enumeration brute-forces every bus/device/function in the region
(65,536 functions maximum per full-range MCFG): read vendor/device at
offset `0x00`, skip `0xFFFF` vendors, read header type at `0x0E` to honor
the multifunction bit (scan all 8 functions when set, else function 0
only), and read the class DWORD at `0x08`. Config-space transport
failures map to `Transport`.

`probe_xhci_bar` targets BAR0 (`0x10`, plus `0x14` when 64-bit) with this
exact order, restoring saved registers on every return path:

1. Save command (`0x04`), BAR0, and BAR1. Read BAR0: bit 0 set means I/O
   space and is `InvalidBar`. Bits 2:1 equal `0b10` select the 64-bit
   path; `0b00` selects 32-bit; `0b01` (16-bit legacy) and `0b11`
   (reserved) are `InvalidBar`.
2. Clear the memory-decode and bus-master bits in command, preserving all
   other bits, and write it back.
3. Write all-ones to the sizing registers (BAR0 alone, or BAR0 and BAR1
   for 64-bit), read back the masks, then immediately restore the saved
   BAR values before any validation return. Mask off the flag nibble,
   combine halves for 64-bit, compute `size = (!mask) + 1` checked.
   Reject zero size, non-power-of-two size, and (for 64-bit) a nonzero
   high half of the restored address combined above the direct-map
   ceiling. Reject a zero base and a base that is not a multiple of
   `size`. Prefetchable versus non-prefetchable is accepted either way;
   the kernel maps the BAR uncached regardless.
4. Map the BAR through the Task 4 checked mapper as uncached MMIO, then
   set the memory-decode bit while leaving bus-master clear. Bus
   mastering stays off until Task 12 has DMA structures ready, per the
   milestone constraint. The returned `BarInfo` carries the restored
   base, computed size, and width flag.

The `unsafe fn write_u32` invariant, documented at the trait: the caller
must target a validated ECAM offset for a discovered device and must have
saved any register it mutates so the probe's restore discipline holds.

## DMA Semantics

`allocate` validates before touching frames: `size` in `1..=4 MiB`
(`TooLarge` outside; the 4 MiB cap bounds the frame loop while covering
every Task 12 structure), `align` a power of two in `1..=65536`
(`BadAlign` otherwise), and `max_address >= 4095` with room for the
allocation (`AddressLimit` when `end - 1 > max_address`). Because frames
are 4 KiB-aligned, any `align <= 4096` is automatically satisfied by a
page-aligned run; for `align > 4096` the allocator over-allocates by
`align` bytes worth of frames and takes the first aligned run, leaking
the skipped prefix frames (bounded waste at boot, no correctness impact).

Frames must be physically contiguous with `device_address` equal to the
physical start (no IOMMU translation in M1; the VT-d record below is
diagnostic only). The run must additionally satisfy `end - 1 <=
0x7FFF_FFFF_FFFF` so the direct-map `cpu_address = physical +
0xFFFF_8000_0000_0000` stays valid. `zeroed` fills through the direct
map before returning. `len` echoes the requested `size` (not the
page-rounded footprint). Exhaustion is `NoMemory`. There is no free and
no reclaiming `Drop`; dropping a `DmaAllocation` intentionally leaks its
frames.

## Kernel Adapters And Diagnostic Probe

- `arch/x86_64/mmio.rs` exposes `map_uncached(phys_base, byte_len) ->
  Result<*mut u8, MapError>` as a minimal runtime 4K PTE installer.
  Amendment (2026-09-13): grounding showed the Task 4 `map_pci_bar`
  helper runs pre-handoff on loader-owned tables and the kernel has no
  page-table code, so a pure wrapper is impossible. The installer
  instead reuses what Task 4 left reusable — the
  `0xFFFF_C000_0000_0000` window (`PCI_BAR_VIRTUAL_START`), the
  align-and-cover range discipline of `bar_mapping_range`, and the same
  uncached flag set — and adds the small runtime walker the kernel was
  missing. Concretely it validates nonzero base, nonzero page-rounded
  length, overflow, and the direct-map/MMIO ceiling, then for each 4K
  page reads the live paging depth from `CR4.LA57`, walks the active
  hierarchy via the direct map (allocating missing intermediate-table
  pages from `PhysicalFrameAllocator`), refuses already-present leaf
  entries, installs `PRESENT | WRITABLE | NO_EXECUTE | CACHE_DISABLE |
  WRITE_THROUGH` PTEs, and flushes the TLB. Virtual addresses come
  from a kernel-side bump pointer starting at `PCI_BAR_VIRTUAL_START`,
  which is safe because the loader never calls `map_pci_bar` today
  (stated invariant; if the loader ever pre-maps a BAR it must publish
  its bump offset in `BootInfo`). Page-index math, entry-flag logic,
  and refuse-present behavior take the depth and table bytes as
  parameters so host tests cover them over a `Vec`-backed table fake;
  only the CR3 read, frame allocation, and flush stay behind the
  kernel boundary. Each unsafe block names the live-table,
  exclusivity, and single-core (no concurrent mapper) invariants. It
  propagates a small local `MapError` (`InvalidRange`, `AlreadyMapped`,
  `NoMemory`, `UnsupportedDepth`).
- `arch/x86_64/dma.rs` implements `DmaAllocator` over
  `PhysicalFrameAllocator::allocate_frame`, skipping frames that overlap
  the retained boot info and memory map exactly as the existing
  allocator does, then exposing them at `PHYSICAL_MEMORY_OFFSET`.
  `cpu_address` is a `NonNull` into that direct-map window with the
  allocation lifetime; `device_address` is the physical start.
- `kernel/pci.rs` implements `PhysicalMemory` (direct-map reads bounded
  by the entry ceiling) and `PciConfig` (ECAM MMIO through the mapped
  window) and adds a read-only `XhciCaps` snapshot taken with plain
  MMIO loads, no controller init: capabilities length, interface
  version, `HCSPARAMS1-3`, `HCCPARAMS1` (including 32/64-byte context
  size and 64-bit addressing), scratchpad count, legacy-support
  ownership bits from the first USB-legacy extended capability, and the
  USB2/USB3 port-protocol ranges. Pure field decoding lives in
  `relay-core::pci` over a byte slice so host tests cover it; the
  kernel only supplies the bytes.
- `main.rs` runs the probe after heap init and before the runtime
  banner: parse MCFG from `BootInfo.acpi_rsdp_phys`, map ECAM, find the
  single xHCI device, probe and map BAR0, take the caps snapshot, check
  DMAR presence, and print one stable serial-plus-framebuffer line per
  item prefixed `[relay] phase=platform-probe status=...`. Any failure
  prints `status=unsupported-platform` (or the specific error class)
  and halts; the kernel never falls back to UEFI services.

`docs/acceptance/nuc-m1.md` gains a Task 11 probe table: MCFG base and
bus range, xHCI BDF, BAR width/address/size, VT-d firmware state plus
DMAR presence, context size, scratchpad count, address-width
capability, legacy ownership bits, and USB2/USB3 protocol port ranges.
If VT-d translation remains active and blocks DMA on the NUC, execution
stops and explicit design approval is required before either adding an
identity-mapped DMA domain or requiring VT-d to be disabled in
firmware. That stop-and-ask rule is carried verbatim from the milestone
and is not resolved here.

## Error Handling And Diagnostics

Every subsystem exposes typed errors (`AcpiError`, `PciError`,
`DmaError`, `MemoryError`, `MapError`) and never panics on firmware,
device, or resource failures. Command and BAR registers are restored on
every probe return path, including error returns. Parsers treat RSDP,
XSDT/RSDT, MCFG, and config-space bytes as untrusted with bounds and
overflow checks at each dereference. Kernel invariant failures keep the
existing behavior: framebuffer plus QEMU-serial diagnostic, then halt.

## Testing

Host suites use fakes that stay in test support: `VecMemory`
(`PhysicalMemory` over a byte vector with a base address and injectable
transport faults) and `RecordingPciConfig` (scripted config space that
records every write for restore-order assertions).

- `acpi.rs`: known-good RSDP/XSDT/MCFG vector accepted with exact
  region bytes; bad RSDP signature, bad 20-byte checksum, bad extended
  checksum, bad XSDT signature, bad XSDT checksum, truncated RSDP/SDT
  body, `length < 36`, over-64 KiB length, missing MCFG
  (`UnsupportedPlatform`), two-entry MCFG (`UnsupportedPlatform`),
  nonzero segment (`UnsupportedPlatform`), zero base, unaligned base,
  and ECAM end overflow (`InvalidRange`); DMAR present/absent/invalid
  cases.
- `pci.rs`: ECAM address known vectors including bus/device/function
  edges; `OutOfRange` without I/O; enumeration honoring the
  multifunction bit with `0xFFFF` skips; xHCI match, `NoXhci`, and
  `MultipleXhci`; 64-bit above-4G BAR accepted with size math and
  command/BAR restore verified before enable; I/O BAR, reserved-type
  BAR, zero BAR, zero size, non-power-of-two size, and misaligned base
  each rejected with registers restored; memory-decode set with
  bus-master left clear.
- `dma.rs`: accepted allocation with exact `device_address`,
  `cpu_address == device_address + PHYSICAL_MEMORY_OFFSET`, and `len`;
  zeroed versus unzeroed contents; `max_address` boundary accepted and
  `max_address + 1` rejected; non-power-of-two and over-64K aligns
  rejected; zero and over-4 MiB sizes rejected; contiguity across a
  reserved-frame hole; exhaustion to `NoMemory`.
- `mmio.rs` host coverage (new file, no integration-test target
  needed): page-index vectors for 4- and 5-level depths, UC flag
  assembly, refuse-present-leaf, unaligned/overflow/ceiling rejections,
  and bump-window allocation order, all over the `Vec`-backed table
  fake; kernel glue (CR3 read, frame alloc, flush) is reviewed, not
  unit-tested.

Verification runs:

```bash
cargo fmt --all --check
cargo test -p relay-core --test acpi --test pci --test dma --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Non-Goals

Task 11 performs no xHCI resets, ownership handoffs, ring allocation, or
doorbell/event processing (Task 12); binds no HID or mass-storage
interface (Tasks 13-14); wires no shell persistence path (Task 15);
builds no general page-table management beyond the minimal MMIO
installer, no IOMMU domains, and no DMA free lists; supports no
multi-segment PCI, cardbus bridges, I/O-space BARs, or hot-plug; and
makes no VT-d firmware requirement beyond recording its state and
enforcing the stop-and-ask rule.
