use relay_core::{
    dma::{DmaAllocator, DmaLayout},
    mmio::MapError,
    pci::{XhciCaps, XhciPciDevice},
    xhci::{Clock, Deadline, Mmio, XhciError},
};

use super::KernelMmio;
use crate::arch::x86_64::memory::direct_slice_mut;

const USBCMD_RS: u32 = 1 << 0;
const USBCMD_HCRST: u32 = 1 << 1;
const USBCMD_INTE: u32 = 1 << 2;
const USBSTS_HCH: u32 = 1 << 0;
const USBSTS_EHB: u32 = 1 << 3;
const USBSTS_CNR: u32 = 1 << 11;
const HCC_AC64: u32 = 1 << 0;
const LEGSUP_BIOS_OWNED: u32 = 1 << 16;
const LEGSUP_OS_OWNED: u32 = 1 << 24;
const LEGCTL_SMI_MASK: u32 = 0x1F;
const IMAN_IP: u32 = 1 << 0;
const CMD_RING_TRBS: usize = 64;
const EVT_RING_TRBS: usize = 64;
const TRB_BYTES: u64 = 16;

pub struct XhciController {
    mmio: KernelMmio,
    op_base: u32,
    db_base: u32,
    rt_base: u32,
    caps: XhciCaps,
    max_slots_en: u8,
    max_ports: u8,
    scratchpad_count: u16,
    dcbaa_phys: u64,
    cmd_phys: u64,
    cmd_enqueue: usize,
    cmd_cycle: bool,
    evt_phys: u64,
    #[allow(dead_code)]
    erst_phys: u64,
    evt_dequeue: usize,
    evt_cycle: bool,
}

impl XhciController {
    pub fn initialize(
        pci: XhciPciDevice,
        caps: XhciCaps,
        dma: &mut impl DmaAllocator,
        clock: &impl Clock,
    ) -> Result<Self, XhciError> {
        let bar_len = usize::try_from(pci.bar.size)
            .map_err(|_| XhciError::Map(relay_core::mmio::MapError::InvalidRange))?;
        if bar_len == 0 {
            return Err(XhciError::Map(MapError::InvalidRange));
        }
        let bar_ptr = crate::arch::x86_64::mmio::map_uncached(pci.bar.base, bar_len)
            .map_err(XhciError::Map)?;
        // SAFETY: `map_uncached` leaked a UC BAR window of `bar_len` bytes
        // exclusively for this controller; the accessor owns it for the
        // controller lifetime on the single boot core.
        let mut mmio = unsafe { KernelMmio::new(bar_ptr, bar_len) };

        // Validate CAPLENGTH / HCIVERSION / PAGESIZE / AC64.
        let cap0 = mmio.read32(relay_core::xhci::CAP_CAPLENGTH)?;
        let cap_length = (cap0 & 0xFF) as u8;
        let version = ((cap0 >> 16) & 0xFFFF) as u16;
        if cap_length < 32 {
            return Err(XhciError::InvalidRegister);
        }
        if (cap_length as usize) >= bar_len {
            return Err(XhciError::InvalidRegister);
        }
        if version < 0x0100 {
            return Err(XhciError::UnsupportedPlatform);
        }
        let hcc = mmio.read32(relay_core::xhci::CAP_HCC1)?;
        if hcc & HCC_AC64 == 0 {
            return Err(XhciError::UnsupportedPlatform);
        }
        let xecp = ((hcc >> 16) & 0xFFFF) as u16;
        let dboff = mmio.read32(relay_core::xhci::CAP_DBOFF)?;
        let rtsoff = mmio.read32(relay_core::xhci::CAP_RTSOFF)?;
        let op_base = cap_length as u32;
        let db_base = dboff;
        let rt_base = rtsoff;
        // Basic bounds: operational, doorbell, and runtime bases must lie
        // inside the BAR so later range-checked accesses cannot wrap.
        if (op_base as usize) >= bar_len
            || (db_base as usize) >= bar_len
            || (rt_base as usize) >= bar_len
        {
            return Err(XhciError::InvalidRegister);
        }

        // Ownership handoff with a 32-iteration cap and a 1-second deadline.
        legacy_handoff(&mut mmio, xecp, clock)?;

        // Halt: clear RS, poll HCHalted set with a 1-second deadline.
        let usbcmd_off = op_base
            .checked_add(relay_core::xhci::OP_USBCMD)
            .ok_or(XhciError::InvalidRegister)?;
        let usbsts_off = op_base
            .checked_add(relay_core::xhci::OP_USBSTS)
            .ok_or(XhciError::InvalidRegister)?;
        let pagesize_off = op_base
            .checked_add(relay_core::xhci::OP_PAGESIZE)
            .ok_or(XhciError::InvalidRegister)?;
        let usbcmd = mmio.read32(usbcmd_off)?;
        mmio.write32(usbcmd_off, usbcmd & !USBCMD_RS & !USBCMD_INTE)?;
        poll_until(clock, deadline_secs(clock, 1), || {
            Ok(mmio.read32(usbsts_off)? & USBSTS_HCH != 0)
        })?;

        // Reset: set HCRST, poll HCRST clear and CNR clear, 1 second each.
        let usbcmd = mmio.read32(usbcmd_off)?;
        mmio.write32(usbcmd_off, (usbcmd | USBCMD_HCRST) & !USBCMD_INTE)?;
        poll_until(clock, deadline_secs(clock, 1), || {
            Ok(mmio.read32(usbcmd_off)? & USBCMD_HCRST == 0)
        })?;
        poll_until(clock, deadline_secs(clock, 1), || {
            Ok(mmio.read32(usbsts_off)? & USBSTS_CNR == 0)
        })?;

        // Re-read bases after reset (reset must not change read-only caps,
        // but re-validating the validated offsets keeps the math checked).
        let pagesize = mmio.read32(pagesize_off)?;
        if pagesize & 0x1 == 0 {
            return Err(XhciError::UnsupportedPlatform);
        }

        let max_slots_en = relay_core::xhci::context::max_slots_en(caps.max_slots);
        let max_ports = caps.max_ports;
        if max_ports == 0 {
            return Err(XhciError::NoPorts);
        }

        // Allocate DCBAA (2048 bytes, align 64), command ring (1024, align
        // 64), event segment (1024, align 64) plus ERST (64, align 64),
        // scratchpads per caps. Leak-only via the Task 11 allocator.
        let dcbaa = alloc_dma(dma, 2048, 64)?;
        let dcbaa_phys = dcbaa.device_address;
        let cmd = alloc_dma(dma, 1024, 64)?;
        let cmd_phys = cmd.device_address;
        let evt = alloc_dma(dma, 1024, 64)?;
        let evt_phys = evt.device_address;
        let erst = alloc_dma(dma, 64, 64)?;
        let erst_phys = erst.device_address;

        // Scratchpads per caps: 0 -> DCBAA[0] = 0, N -> N pages plus array.
        let scratch_array_phys = if caps.scratchpad_count == 0 {
            0
        } else {
            let count = caps.scratchpad_count as usize;
            let array_bytes = count.checked_mul(8).ok_or(XhciError::Allocation)?;
            if array_bytes == 0 {
                return Err(XhciError::Allocation);
            }
            let array = alloc_dma(dma, array_bytes, 64)?;
            let array_phys = array.device_address;
            let array_cpu = array.cpu_address.as_ptr() as *mut u64;
            let mut index = 0usize;
            while index < count {
                let page = alloc_dma(dma, 4096, 4096)?;
                let page_phys = page.device_address;
                // SAFETY: leak-only DMA array, bounds checked by
                // `index < count` and `array_bytes = count * 8`; single
                // boot core owns the frames; volatile write publishes the
                // address before the controller is started.
                unsafe {
                    let slot = array_cpu.add(index);
                    slot.write_volatile(page_phys);
                }
                // Frames stay reserved via the bump source until reboot.
                let _ = page;
                index += 1;
            }
            // The array stays reserved via the bump source; DCBAA[0] keeps
            // the phys.
            let _ = array;
            array_phys
        };

        // Program DCBAA[0] (scratchpad array or zero). The allocation was
        // zeroed, so entries 1..=MaxSlotsEn already read zero.
        {
            let cpu = dcbaa.cpu_address.as_ptr() as *mut u64;
            // SAFETY: leak-only DCBAA page, entry 0 owned exclusively here
            // before the controller runs; single-core, direct-map bounds
            // hold via the DMA ceiling.
            unsafe {
                cpu.write_volatile(scratch_array_phys);
            }
        }
        // DMA frames stay reserved via the bump source until reboot; the
        // allocations themselves are dropped here after publishing phys.
        let _ = dcbaa;
        // Initialize command-ring Link TRB at index 63 (toggle, cycle 1).
        {
            let link = relay_core::xhci::encode_link(cmd_phys, true, 1);
            let link_off = (CMD_RING_TRBS - 1)
                .checked_mul(16)
                .ok_or(XhciError::InvalidRegister)?;
            write_dma_bytes(cmd_phys, link_off, &link)?;
        }
        let _ = cmd;
        let _ = evt;
        // Program the single ERST entry: base + size 64, rest zeroed.
        {
            let erst_cpu = erst.cpu_address.as_ptr();
            // SAFETY: leak-only 64-byte ERST, entry 0 written once before
            // start on the single boot core.
            unsafe {
                (erst_cpu as *mut u64).write_volatile(evt_phys);
                (erst_cpu.add(8) as *mut u32).write_volatile(EVT_RING_TRBS as u32);
            }
        }
        let _ = erst;

        // Program DCBAAP / CRCR (cycle 1). ERSTSZ/ERSTBA/ERDP are
        // programmed later by `program_event_ring`, immediately before
        // the first doorbell (see its docs for why).
        let dcbaap_off = op_base
            .checked_add(relay_core::xhci::OP_DCBAAP)
            .ok_or(XhciError::InvalidRegister)?;
        let crcr_off = op_base
            .checked_add(relay_core::xhci::OP_CRCR)
            .ok_or(XhciError::InvalidRegister)?;
        let config_off = op_base
            .checked_add(relay_core::xhci::OP_CONFIG)
            .ok_or(XhciError::InvalidRegister)?;
        let iman_off = rt_base
            .checked_add(relay_core::xhci::RT_IMAN)
            .ok_or(XhciError::InvalidRegister)?;

        if dcbaa_phys & 0x3F != 0
            || cmd_phys & 0x3F != 0
            || evt_phys & 0x3F != 0
            || erst_phys & 0x3F != 0
        {
            return Err(XhciError::Allocation);
        }
        // Device-context pointers above MaxSlotsEn stay zero; DCBAA covers
        // 256 entries regardless of the 32-slot bound. The fence makes
        // the DCBAA, command-ring Link, and ERST writes visible before
        // the registers below publish them to the controller.
        crate::arch::x86_64::memory::dma_write_fence();
        write64(&mut mmio, dcbaap_off, dcbaa_phys)?;
        write64(&mut mmio, crcr_off, cmd_phys | 0x1)?;
        // Clear interrupter IP while keeping IE=0 (polling).
        mmio.write32(iman_off, IMAN_IP)?;

        // Program CONFIG.MaxSlotsEn (low byte, preserve the rest).
        let config = mmio.read32(config_off)?;
        mmio.write32(config_off, (config & !0xFF) | max_slots_en as u32)?;

        // Start: set RS, keep INTE clear, poll halted-clear (1 second).
        let usbcmd = mmio.read32(usbcmd_off)?;
        mmio.write32(usbcmd_off, (usbcmd | USBCMD_RS) & !USBCMD_INTE)?;
        poll_until(clock, deadline_secs(clock, 1), || {
            Ok(mmio.read32(usbsts_off)? & USBSTS_HCH == 0)
        })?;

        // Enable PCI bus mastering only now, after DMA is ready.
        crate::pci::enable_bus_mastering(pci.address).map_err(|err| match err {
            crate::pci::ProbeError::Map(map) => XhciError::Map(map),
            crate::pci::ProbeError::Dma(dma_err) => XhciError::Dma(dma_err),
            crate::pci::ProbeError::Pci(_) | crate::pci::ProbeError::Acpi(_) => {
                XhciError::InvalidRegister
            }
        })?;

        Ok(Self {
            mmio,
            op_base,
            db_base,
            rt_base,
            caps,
            max_slots_en,
            max_ports,
            scratchpad_count: caps.scratchpad_count,
            dcbaa_phys,
            cmd_phys,
            cmd_enqueue: 0,
            cmd_cycle: true,
            evt_phys,
            erst_phys,
            evt_dequeue: 0,
            evt_cycle: true,
        })
    }

    // Task 4 drives transfers and ports; allow dead here so the Task 3
    // boot gate stays warning-free.
    #[allow(dead_code)]
    pub fn poll(&mut self) -> Result<usize, XhciError> {
        let mut consumed = 0usize;
        let mut dequeue = self.evt_dequeue;
        let mut cycle = self.evt_cycle;
        while consumed < EVT_RING_TRBS {
            let trb = read_event_trb(self.evt_phys, dequeue)?;
            if trb[12] & 0x01 != cycle as u8 {
                break;
            }
            consumed += 1;
            dequeue += 1;
            if dequeue >= EVT_RING_TRBS {
                dequeue = 0;
                cycle = !cycle;
            }
        }
        if consumed == EVT_RING_TRBS {
            // 64 matched without an empty entry: check one more for overrun.
            // Design §Ring: >64 ready entries without empty = overrun, never silent.
            let trb = read_event_trb(self.evt_phys, dequeue)?;
            if trb[12] & 0x01 == cycle as u8 {
                return Err(XhciError::TransferFailed(0));
            }
        }
        if consumed > 0 {
            let new_phys = self
                .evt_phys
                .checked_add(
                    (dequeue as u64)
                        .checked_mul(TRB_BYTES)
                        .ok_or(XhciError::InvalidRegister)?,
                )
                .ok_or(XhciError::InvalidRegister)?;
            let erdp_off = self
                .rt_base
                .checked_add(relay_core::xhci::RT_ERDP)
                .ok_or(XhciError::InvalidRegister)?;
            write64(&mut self.mmio, erdp_off, new_phys)?;
            let usbsts_off = self
                .op_base
                .checked_add(relay_core::xhci::OP_USBSTS)
                .ok_or(XhciError::InvalidRegister)?;
            // Write-1-to-clear EHB; writing 0 elsewhere is a no-op.
            self.mmio.write32(usbsts_off, USBSTS_EHB)?;
            let iman_off = self
                .rt_base
                .checked_add(relay_core::xhci::RT_IMAN)
                .ok_or(XhciError::InvalidRegister)?;
            self.mmio.write32(iman_off, IMAN_IP)?;
            self.evt_dequeue = dequeue;
            self.evt_cycle = cycle;
        }
        Ok(consumed)
    }

    pub fn connected_root_ports(&self, output: &mut [u8]) -> Result<usize, XhciError> {
        let mut count = 0usize;
        let mut port: u16 = 1;
        let max = self.max_ports as u16;
        while port <= max {
            let port_u8 = port as u8;
            let offset = super::port::portsc_offset(self.op_base, port_u8)?;
            let portsc = self.mmio.read32(offset)?;
            if portsc & super::port::PORT_CCS != 0 {
                let slot = output.get_mut(count).ok_or(XhciError::Allocation)?;
                *slot = port_u8;
                count += 1;
            }
            port += 1;
        }
        if count == 0 {
            return Err(XhciError::NoPorts);
        }
        Ok(count)
    }

    /// Slot doorbell for Task 4: `db + slot * 4` with the endpoint target
    /// in bits 7:0 and stream 0 in bits 31:16.
    pub(crate) fn slot_doorbell(&mut self, slot: u8, target: u8) -> Result<(), XhciError> {
        let slot_off = (slot as u32)
            .checked_mul(4)
            .ok_or(XhciError::InvalidRegister)?;
        let offset = self
            .db_base
            .checked_add(slot_off)
            .ok_or(XhciError::InvalidRegister)?;
        self.mmio.write32(offset, target as u32)
    }

    /// Command doorbell (`db + 0`) for Task 4 command submission.
    pub(crate) fn ring_command_doorbell(&mut self) -> Result<(), XhciError> {
        self.mmio.write32(self.db_base, 0)
    }

    /// Poll-wait helper for Task 4: drains events until the CmdComplete
    /// with `ptr` arrives or `deadline` expires. Returns the slot field
    /// from the matching completion on success; maps non-success codes
    /// through `completion_to_error`. `slot == 0` is the EnableSlot case
    /// (the returned slot is the newly assigned one); any other command
    /// must complete for its own slot or the wait fails with
    /// `InvalidSlot`.
    pub(crate) fn wait_command(
        &mut self,
        ptr: u64,
        slot: u8,
        deadline: Deadline,
        clock: &impl Clock,
    ) -> Result<u8, XhciError> {
        loop {
            // Drain up to 64 entries per iteration, decoding on the fly so
            // no heap allocation happens in the poll path.
            let mut iter = 0usize;
            while iter < EVT_RING_TRBS {
                let trb = read_event_trb(self.evt_phys, self.evt_dequeue)?;
                if trb[12] & 0x01 != self.evt_cycle as u8 {
                    break;
                }
                let control = u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]);
                let trb_type = (control >> 10) & 0x3F;
                let mut matched_slot: Option<u8> = None;
                let mut matched_code: Option<u8> = None;
                match trb_type {
                    33 => {
                        let event = relay_core::xhci::decode_cmd_complete(&trb)?;
                        if event.pointer == ptr {
                            matched_slot = Some(event.slot);
                            matched_code = Some(event.code);
                        }
                    }
                    32 | 34 => {
                        // Transfer / port-change events are consumed and
                        // counted but never delivered to a command waiter.
                    }
                    _ => {
                        // Advance past the offending entry so the ring does
                        // not stick, then report the type error.
                        self.advance_event(1)?;
                        return Err(XhciError::UnsupportedEvent);
                    }
                }
                self.advance_event(1)?;
                if let (Some(found_slot), Some(code)) = (matched_slot, matched_code) {
                    if slot != 0 && found_slot != slot {
                        return Err(XhciError::InvalidSlot);
                    }
                    relay_core::xhci::completion_to_error(code)?;
                    return Ok(found_slot);
                }
                if clock.now_ticks() >= deadline.0 {
                    return Err(XhciError::Timeout);
                }
                iter += 1;
            }
            if clock.now_ticks() >= deadline.0 {
                return Err(XhciError::Timeout);
            }
        }
    }

    /// Poll-wait helper for Task 4 transfers: drains events until the
    /// Transfer Event for the TRB at `ptr` on `slot` endpoint `dci`
    /// arrives or `deadline` expires. Stall completions surface as
    /// `XhciError::Stalled` via `completion_to_error` (no automatic
    /// recovery); other nonzero codes become `TransferFailed(code)`;
    /// expiry returns `Timeout`. The clock is consulted every iteration.
    pub(crate) fn wait_transfer(
        &mut self,
        ptr: u64,
        slot: u8,
        dci: u8,
        deadline: Deadline,
        clock: &impl Clock,
    ) -> Result<(), XhciError> {
        loop {
            let mut iter = 0usize;
            while iter < EVT_RING_TRBS {
                let trb = read_event_trb(self.evt_phys, self.evt_dequeue)?;
                if trb[12] & 0x01 != self.evt_cycle as u8 {
                    break;
                }
                let control = u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]);
                let trb_type = (control >> 10) & 0x3F;
                match trb_type {
                    32 => {
                        let event = relay_core::xhci::decode_transfer_event(&trb)?;
                        self.advance_event(1)?;
                        if event.pointer == ptr {
                            if event.slot != slot || event.endpoint != dci {
                                return Err(XhciError::InvalidSlot);
                            }
                            relay_core::xhci::completion_to_error(event.code)?;
                            return Ok(());
                        }
                    }
                    33 | 34 => {
                        // Command completions and port-change events are
                        // consumed but never delivered to a transfer waiter.
                        self.advance_event(1)?;
                    }
                    _ => {
                        self.advance_event(1)?;
                        return Err(XhciError::UnsupportedEvent);
                    }
                }
                if clock.now_ticks() >= deadline.0 {
                    return Err(XhciError::Timeout);
                }
                iter += 1;
            }
            if clock.now_ticks() >= deadline.0 {
                return Err(XhciError::Timeout);
            }
        }
    }

    /// Enqueues a command TRB on the command ring with the current
    /// producer cycle and returns its device physical address for
    /// `wait_command` correlation. The single-segment ring never wraps
    /// in Task 4 (fewer than 63 commands); a full ring is an error.
    pub(crate) fn submit_command(&mut self, mut trb: [u8; 16]) -> Result<u64, XhciError> {
        if self.cmd_enqueue >= CMD_RING_TRBS - 1 {
            return Err(XhciError::Allocation);
        }
        trb[12] = (trb[12] & 0xFE) | (self.cmd_cycle as u8);
        let byte_off = (self.cmd_enqueue as u64)
            .checked_mul(TRB_BYTES)
            .ok_or(XhciError::InvalidRegister)?;
        let phys = self
            .cmd_phys
            .checked_add(byte_off)
            .ok_or(XhciError::InvalidRegister)?;
        write_dma_bytes(phys, 0, &trb)?;
        // Publish the TRB to the controller before the caller rings the
        // command doorbell (store-store ordering, see `dma_write_fence`).
        crate::arch::x86_64::memory::dma_write_fence();
        self.cmd_enqueue += 1;
        Ok(phys)
    }

    /// Programs ERSTSZ/ERDP/ERSTBA from the stored ring addresses. This
    /// runs immediately before the first doorbell rather than at init:
    /// programming the event ring early leaves it uncached (first
    /// command's completion never arrives, HCE asserts), while the same
    /// values programmed late cache reliably and completions flow. The
    /// table itself is written at init; only the register publish is
    /// deferred. Safe to call once the controller is running and no
    /// events are outstanding.
    pub(crate) fn program_event_ring(&mut self) -> Result<(), XhciError> {
        let erstsz_off = self
            .rt_base
            .checked_add(relay_core::xhci::RT_ERSTSZ)
            .ok_or(XhciError::InvalidRegister)?;
        let erstba_off = self
            .rt_base
            .checked_add(relay_core::xhci::RT_ERSTBA)
            .ok_or(XhciError::InvalidRegister)?;
        let erdp_off = self
            .rt_base
            .checked_add(relay_core::xhci::RT_ERDP)
            .ok_or(XhciError::InvalidRegister)?;
        crate::arch::x86_64::memory::dma_write_fence();
        self.mmio.write32(erstsz_off, 1)?;
        write64(&mut self.mmio, erdp_off, self.evt_phys)?;
        write64(&mut self.mmio, erstba_off, self.erst_phys)?;
        Ok(())
    }

    /// Reads the PORTSC register for a 1-based root-hub port.
    pub(crate) fn read_portsc(&self, port: u8) -> Result<u32, XhciError> {
        let offset = super::port::portsc_offset(self.op_base, port)?;
        self.mmio.read32(offset)
    }

    /// Writes the PORTSC register for a 1-based root-hub port. Callers
    /// pass a read-modify-write value; writing 1 to change bits clears
    /// them per the xHCI rules.
    pub(crate) fn write_portsc(&mut self, port: u8, value: u32) -> Result<(), XhciError> {
        let offset = super::port::portsc_offset(self.op_base, port)?;
        self.mmio.write32(offset, value)
    }

    fn advance_event(&mut self, count: usize) -> Result<(), XhciError> {
        let mut dequeue = self.evt_dequeue;
        let mut cycle = self.evt_cycle;
        let mut remaining = count;
        while remaining > 0 {
            dequeue += 1;
            if dequeue >= EVT_RING_TRBS {
                dequeue = 0;
                cycle = !cycle;
            }
            remaining -= 1;
        }
        let new_phys = self
            .evt_phys
            .checked_add(
                (dequeue as u64)
                    .checked_mul(TRB_BYTES)
                    .ok_or(XhciError::InvalidRegister)?,
            )
            .ok_or(XhciError::InvalidRegister)?;
        let erdp_off = self
            .rt_base
            .checked_add(relay_core::xhci::RT_ERDP)
            .ok_or(XhciError::InvalidRegister)?;
        write64(&mut self.mmio, erdp_off, new_phys)?;
        let usbsts_off = self
            .op_base
            .checked_add(relay_core::xhci::OP_USBSTS)
            .ok_or(XhciError::InvalidRegister)?;
        self.mmio.write32(usbsts_off, USBSTS_EHB)?;
        let iman_off = self
            .rt_base
            .checked_add(relay_core::xhci::RT_IMAN)
            .ok_or(XhciError::InvalidRegister)?;
        self.mmio.write32(iman_off, IMAN_IP)?;
        self.evt_dequeue = dequeue;
        self.evt_cycle = cycle;
        Ok(())
    }

    pub fn max_slots_en(&self) -> u8 {
        self.max_slots_en
    }

    pub fn max_ports(&self) -> u8 {
        self.max_ports
    }

    pub fn caps(&self) -> XhciCaps {
        self.caps
    }

    pub fn scratchpad_count(&self) -> u16 {
        self.scratchpad_count
    }

    /// DCBAA device address for Task 4 slot bring-up (slot entries live
    /// at `dcbaa_phys + slot * 8`).
    pub fn dcbaa_phys(&self) -> u64 {
        self.dcbaa_phys
    }

    #[allow(dead_code)]
    pub fn cmd_phys(&self) -> u64 {
        self.cmd_phys
    }

    #[allow(dead_code)]
    pub fn evt_phys(&self) -> u64 {
        self.evt_phys
    }

    #[allow(dead_code)]
    pub fn erst_phys(&self) -> u64 {
        self.erst_phys
    }
}

fn deadline_secs(clock: &impl Clock, secs: u64) -> Deadline {
    Deadline(
        clock
            .now_ticks()
            .saturating_add(secs.saturating_mul(clock.ticks_per_second())),
    )
}

fn poll_until(
    clock: &impl Clock,
    deadline: Deadline,
    mut ready: impl FnMut() -> Result<bool, XhciError>,
) -> Result<(), XhciError> {
    loop {
        if ready()? {
            return Ok(());
        }
        if clock.now_ticks() >= deadline.0 {
            return Err(XhciError::Timeout);
        }
        core::hint::spin_loop();
    }
}

fn legacy_handoff(mmio: &mut KernelMmio, xecp: u16, clock: &impl Clock) -> Result<(), XhciError> {
    if xecp == 0 {
        return Ok(());
    }
    let mut offset = (xecp as u32)
        .checked_mul(4)
        .ok_or(XhciError::InvalidRegister)?;
    let mut iter = 0usize;
    while iter < 32 {
        let dword = mmio.read32(offset)?;
        let next = (dword >> 8) & 0xFF;
        let id = (dword & 0xFF) as u8;
        if id == 1 {
            mmio.write32(offset, dword | LEGSUP_OS_OWNED)?;
            poll_until(clock, deadline_secs(clock, 1), || {
                Ok(mmio.read32(offset)? & LEGSUP_BIOS_OWNED == 0)
            })?;
            let ctl_off = offset.checked_add(4).ok_or(XhciError::InvalidRegister)?;
            let ctl = mmio.read32(ctl_off)?;
            mmio.write32(ctl_off, ctl & !LEGCTL_SMI_MASK)?;
        }
        if next == 0 {
            break;
        }
        let stride = next.checked_mul(4).ok_or(XhciError::InvalidRegister)?;
        offset = offset
            .checked_add(stride)
            .ok_or(XhciError::InvalidRegister)?;
        iter += 1;
    }
    Ok(())
}

fn alloc_dma(
    dma: &mut impl DmaAllocator,
    size: usize,
    align: usize,
) -> Result<relay_core::dma::DmaAllocation, XhciError> {
    dma.allocate(DmaLayout {
        size,
        align,
        max_address: u64::MAX,
        zeroed: true,
    })
    .map_err(XhciError::Dma)
}

fn write64(mmio: &mut KernelMmio, offset: u32, value: u64) -> Result<(), XhciError> {
    let high_off = offset.checked_add(4).ok_or(XhciError::InvalidRegister)?;
    mmio.write32(offset, value as u32)?;
    mmio.write32(high_off, (value >> 32) as u32)
}

fn write_dma_bytes(phys: u64, byte_offset: usize, bytes: &[u8; 16]) -> Result<(), XhciError> {
    let dest = phys
        .checked_add(byte_offset as u64)
        .ok_or(XhciError::InvalidRegister)?;
    let slice = direct_slice_mut(dest, 16).ok_or(XhciError::InvalidRegister)?;
    let dest_bytes = slice.get_mut(0..16).ok_or(XhciError::InvalidRegister)?;
    dest_bytes.copy_from_slice(bytes);
    Ok(())
}

fn read_event_trb(evt_phys: u64, index: usize) -> Result<[u8; 16], XhciError> {
    let byte_off = (index as u64)
        .checked_mul(TRB_BYTES)
        .ok_or(XhciError::InvalidRegister)?;
    let trb_phys = evt_phys
        .checked_add(byte_off)
        .ok_or(XhciError::InvalidRegister)?;
    // Validate against the direct-map ceiling before the volatile read.
    direct_slice_mut(trb_phys, 16).ok_or(XhciError::InvalidRegister)?;
    let virt = trb_phys
        .checked_add(crate::arch::x86_64::memory::PHYSICAL_MEMORY_OFFSET)
        .ok_or(XhciError::InvalidRegister)?;
    // SAFETY: leak-only DMA event segment, bounds checked above against
    // the direct-map ceiling; single boot core; volatile read observes
    // hardware-written bytes without caching.
    Ok(unsafe { (virt as *const [u8; 16]).read_volatile() })
}

pub(crate) fn xhci_status(error: &XhciError) -> &'static str {
    // NOTE: Timeout/Stalled capitalized; qemu_xhci gate greps them.
    match error {
        XhciError::UnsupportedPlatform => "unsupported-platform",
        XhciError::UnsupportedEvent => "unsupported-event",
        XhciError::InvalidRegister => "invalid-register",
        XhciError::Map(_) => "map-failed",
        XhciError::Dma(_) => "dma-failed",
        XhciError::Timeout => "Timeout",
        XhciError::Stalled => "Stalled",
        XhciError::TransferFailed(_) => "transfer-failed",
        XhciError::NoDevice => "no-device",
        XhciError::InvalidSlot => "invalid-slot",
        XhciError::NoPorts => "no-ports",
        XhciError::Allocation => "allocation-failed",
    }
}

/// Outcome of the default-pipe control probe: `Eight` means the 8-byte
/// GET_DESCRIPTOR read succeeded, `None` means no root-hub port was
/// connected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlProbe {
    Eight,
    None,
}

impl ControlProbe {
    pub fn as_str(self) -> &'static str {
        match self {
            ControlProbe::Eight => "8",
            ControlProbe::None => "none",
        }
    }
}

/// Values collected by the Task 4 probe for the `xhci-probe` marker line.
pub struct ProbeReport {
    pub slots_en: u8,
    pub ports: u8,
    pub ctx64: u8,
    pub scratch: u16,
    pub max_slots: u8,
    pub control_probe: ControlProbe,
}

/// Builds the TSC clock and Task 11 DMA allocator, initializes the primary
/// controller, then probes the first connected root-hub port with a port
/// reset, EnableSlot, AddressDevice, and a default-pipe 8-byte
/// GET_DESCRIPTOR control read. Returns the values for the `xhci-probe`
/// marker line; the caller prints the line and halts on error. All DMA is
/// leak-only, so the hardware keeps running after this function returns.
pub fn probe_and_collect(platform: &crate::pci::PlatformInfo) -> Result<ProbeReport, XhciError> {
    let clock = crate::arch::x86_64::clock::TscClock::calibrate();
    let mut dma = crate::arch::x86_64::dma::allocator();
    let pci = XhciPciDevice {
        address: platform.xhci,
        bar: platform.bar,
    };
    let mut controller = XhciController::initialize(pci, platform.caps, &mut dma, &clock)?;
    let mut report = ProbeReport {
        slots_en: controller.max_slots_en(),
        ports: controller.max_ports(),
        ctx64: controller.caps().context_64 as u8,
        scratch: controller.scratchpad_count(),
        max_slots: controller.caps().max_slots,
        control_probe: ControlProbe::None,
    };
    // A full 256-entry buffer always fits `max_ports <= 255`, so an
    // `Allocation` error here is impossible; `NoPorts` means no device is
    // connected and the probe correctly reports `control_probe=none`.
    let mut connected = [0u8; 256];
    match controller.connected_root_ports(&mut connected) {
        Ok(_) => {}
        Err(XhciError::NoPorts) => return Ok(report),
        Err(other) => return Err(other),
    };
    // First connected port, not hardcoded port 1: QEMU numbers USB3 ports
    // first, so a full-speed keyboard lands on a USB2 port (3 or 4).
    // `connected_root_ports` guarantees at least one entry on success.
    let port = connected[0];
    let mut device = controller.reset_and_address(port, &mut dma, &clock)?;
    // NOTE: reset learns EP0 size internally; probe re-reads 8B for the marker (harmless, one extra control transfer).
    let desc_alloc = alloc_dma(&mut dma, 64, 64)?;
    let desc = crate::arch::x86_64::memory::direct_slice_mut(desc_alloc.device_address, 8)
        .ok_or(XhciError::InvalidRegister)?;
    let data = relay_core::xhci::ControlData::new(
        relay_core::xhci::ControlDirection::In,
        desc,
        &desc_alloc,
    )?;
    controller.control(
        &mut device,
        super::transfer::GET_DESCRIPTOR_8,
        Some(data),
        &clock,
    )?;
    // Leak-only DMA keeps the controller alive in hardware; the structs
    // drop here and the caller prints the collected marker line.
    let _ = (desc_alloc, device, controller);
    report.control_probe = ControlProbe::Eight;
    Ok(report)
}
