//! Task 4 control/bulk/interrupt transfers plus slot bring-up.
//!
//! All DMA is leak-only via the Task 11 bump allocator; every physical or
//! offset computation uses checked arithmetic; every poll loop consults the
//! `TscClock` each iteration. Stall completions return
//! `XhciError::Stalled` without automatic recovery, other nonzero
//! completion codes become `XhciError::TransferFailed(code)`, and expiry
//! returns `XhciError::Timeout`.

use alloc::vec::Vec;

use relay_core::{
    dma::{DmaAllocation, DmaAllocator, DmaLayout},
    xhci::{
        BulkData, BulkDirection, Clock, ControlData, ControlDirection, Deadline, DeviceHandle,
        EndpointConfig, UsbSpeed, XhciError,
    },
};

use super::controller::XhciController;
use super::port::{PORT_CCS, PORT_PED, PORT_PR, PORT_PRSC};
use crate::arch::x86_64::memory::direct_slice_mut;

const TRANSFER_RING_TRBS: usize = 64;
const INPUT_CONTEXT_BYTES: usize = 4096;
const OUTPUT_CONTEXT_BYTES: usize = 2048;
const RING_SEGMENT_BYTES: usize = 1024;
const EP_TYPE_CONTROL: u8 = 4;
const EP_TYPE_BULK_OUT: u8 = 2;
const EP_TYPE_BULK_IN: u8 = 6;
const EP_TYPE_INT_OUT: u8 = 3;
const EP_TYPE_INT_IN: u8 = 7;
const TRB_TYPE_EVALUATE_CONTEXT: u32 = 13;

/// Default-pipe GET_DESCRIPTOR (device, first 8 bytes): bmRequestType
/// `0x80`, bRequest `6`, wValue `0x0100`, wIndex `0`, wLength `8`.
pub const GET_DESCRIPTOR_8: [u8; 8] = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x08, 0x00];

/// A single-segment DMA transfer ring with kernel-side enqueue state. The
/// Link TRB at index 63 has the toggle bit set; the producer cycle starts
/// at 1 to match the dequeue-cycle state programmed into endpoint
/// contexts.
struct TransferRing {
    phys: u64,
    enqueue: usize,
    cycle: bool,
}

impl TransferRing {
    fn push(&mut self, mut trb: [u8; 16]) -> Result<u64, XhciError> {
        if self.enqueue >= TRANSFER_RING_TRBS - 1 {
            let link = relay_core::xhci::encode_link(self.phys, true, (!self.cycle) as u8);
            write_trb(self.phys, TRANSFER_RING_TRBS - 1, &link)?;
            self.enqueue = 0;
            self.cycle = !self.cycle;
        }
        trb[12] = (trb[12] & 0xFE) | (self.cycle as u8);
        write_trb(self.phys, self.enqueue, &trb)?;
        // Publish the TRB before the caller rings the slot doorbell
        // (store-store ordering, see `dma_write_fence`). This also covers
        // input-context and data-buffer bytes written earlier.
        crate::arch::x86_64::memory::dma_write_fence();
        let phys = self
            .phys
            .checked_add(
                (self.enqueue as u64)
                    .checked_mul(16)
                    .ok_or(XhciError::InvalidRegister)?,
            )
            .ok_or(XhciError::InvalidRegister)?;
        self.enqueue += 1;
        Ok(phys)
    }

    /// Dequeue pointer for an idle ring: the consumer has drained every
    /// submitted TRB, so its position equals the producer position with
    /// the current producer cycle as the dequeue-cycle state.
    fn idle_dequeue(&self) -> Result<u64, XhciError> {
        if self.enqueue >= TRANSFER_RING_TRBS {
            return Err(XhciError::InvalidRegister);
        }
        let phys = self
            .phys
            .checked_add(
                (self.enqueue as u64)
                    .checked_mul(16)
                    .ok_or(XhciError::InvalidRegister)?,
            )
            .ok_or(XhciError::InvalidRegister)?;
        if phys & 0xF != 0 {
            return Err(XhciError::InvalidRegister);
        }
        Ok(phys | (self.cycle as u64))
    }
}

struct EndpointEntry {
    dci: u8,
    dir_in: bool,
    ring: TransferRing,
}

/// An addressed device: the slot handle plus the EP0 transfer ring and any
/// configured endpoint rings. Rings are DMA-resident and leak-only.
pub struct UsbDevice {
    pub handle: DeviceHandle,
    speed_raw: u8,
    root_port: u8,
    // Learned EP0 size for Task 13+ endpoint work; the Task 4 probe only
    // needs the successful 8-byte read itself.
    #[allow(dead_code)]
    ep0_max_packet: u16,
    ep0: TransferRing,
    endpoints: Vec<EndpointEntry>,
}

impl XhciController {
    /// Resets `port`, enables a slot, addresses the device with a 4 KiB
    /// input context, and performs a default-pipe 8-byte GET_DESCRIPTOR
    /// control read to learn the EP0 max-packet size. A differing valid
    /// size is applied with Evaluate Context. Port readiness (CCS and PED
    /// with PR cleared or PRSC latched) has a 1-second deadline; every
    /// command and transfer wait has a 5-second deadline.
    pub fn reset_and_address(
        &mut self,
        port: u8,
        dma: &mut impl DmaAllocator,
        clock: &impl Clock,
    ) -> Result<UsbDevice, XhciError> {
        let portsc = self.read_portsc(port)?;
        self.write_portsc(port, portsc | PORT_PR)?;
        let ready_deadline = deadline_secs(clock, 1);
        loop {
            let portsc = self.read_portsc(port)?;
            // Reset completion is CCS and PED set with either PR cleared
            // by the xHC (the spec USB2 completion signal) or PRSC
            // latched: QEMU's USB2 reset clears PR and latches PRC without
            // touching PRSC, while link-state-changing ports latch PRSC.
            let done = portsc & PORT_CCS != 0
                && portsc & PORT_PED != 0
                && (portsc & PORT_PR == 0 || portsc & PORT_PRSC != 0);
            if done {
                // Write back to clear latched change bits (1 clears); PR
                // reads 0 here so no new reset is triggered.
                self.write_portsc(port, portsc)?;
                break;
            }
            if clock.now_ticks() >= ready_deadline.0 {
                return Err(XhciError::Timeout);
            }
        }
        let portsc = self.read_portsc(port)?;
        let speed_raw = ((portsc >> 10) & 0xF) as u8;
        let speed = UsbSpeed::from_portsc(portsc)?;
        let initial_packet = initial_max_packet(speed)?;

        // Program the event ring now that the port is reset: the table
        // write at init has settled and the controller is running, so
        // the cache pass observes the published table (see
        // `program_event_ring`).
        self.program_event_ring()?;

        let enable = relay_core::xhci::encode_enable_slot(0);
        let enable_phys = self.submit_command(enable)?;
        self.ring_command_doorbell()?;
        let slot = self.wait_command(enable_phys, 0, deadline_secs(clock, 5), clock)?;
        if slot == 0 {
            return Err(XhciError::InvalidSlot);
        }

        let is_64 = self.caps().context_64;
        let output = alloc_dma(dma, OUTPUT_CONTEXT_BYTES, 64)?;
        let dcbaa_entry = self
            .dcbaa_phys()
            .checked_add(
                (slot as u64)
                    .checked_mul(8)
                    .ok_or(XhciError::InvalidRegister)?,
            )
            .ok_or(XhciError::InvalidRegister)?;
        write_u64_le(dcbaa_entry, output.device_address)?;

        let ep0 = alloc_ring(dma)?;
        let input = alloc_dma(dma, INPUT_CONTEXT_BYTES, 64)?;
        write_input_control(input.device_address, 0x3)?;
        write_slot_context(input.device_address, 32, speed_raw, port, 1, is_64)?;
        write_ep_context(
            input.device_address,
            input_ep_offset(1, is_64)?,
            initial_packet,
            0,
            0,
            EP_TYPE_CONTROL,
            ep0.phys | 0x1,
            8,
            initial_packet,
            is_64,
        )?;

        let address = relay_core::xhci::encode_address_device(input.device_address, slot, 0);
        let address_phys = self.submit_command(address)?;
        self.ring_command_doorbell()?;
        self.wait_command(address_phys, slot, deadline_secs(clock, 5), clock)?;

        let mut device = UsbDevice {
            handle: DeviceHandle {
                slot_id: slot,
                root_port: port,
                speed,
            },
            speed_raw,
            root_port: port,
            ep0_max_packet: initial_packet,
            ep0,
            endpoints: Vec::new(),
        };
        let desc_alloc = alloc_dma(dma, 64, 64)?;
        let reported = {
            let desc =
                direct_slice_mut(desc_alloc.device_address, 8).ok_or(XhciError::InvalidRegister)?;
            let data = ControlData::new(ControlDirection::In, desc, &desc_alloc)?;
            self.control(&mut device, GET_DESCRIPTOR_8, Some(data), clock)?;
            let desc =
                direct_slice_mut(desc_alloc.device_address, 8).ok_or(XhciError::InvalidRegister)?;
            u16::from(desc[7])
        };
        if reported != initial_packet && relay_core::xhci::context::ep0_max_packet_valid(reported) {
            let eval_input = alloc_dma(dma, INPUT_CONTEXT_BYTES, 64)?;
            write_input_control(eval_input.device_address, 0x2)?;
            write_ep_context(
                eval_input.device_address,
                input_ep_offset(1, is_64)?,
                reported,
                0,
                0,
                EP_TYPE_CONTROL,
                device.ep0.idle_dequeue()?,
                8,
                reported,
                is_64,
            )?;
            let evaluate = encode_evaluate_context(eval_input.device_address, slot);
            let evaluate_phys = self.submit_command(evaluate)?;
            self.ring_command_doorbell()?;
            self.wait_command(evaluate_phys, slot, deadline_secs(clock, 5), clock)?;
            device.ep0_max_packet = reported;
            let _ = eval_input;
        }
        // Leak-only: input/output contexts, rings, and the descriptor
        // buffer stay reserved for the hardware; handles drop here.
        let _ = (input, output, desc_alloc);
        Ok(device)
    }

    /// Configures endpoints from `configs`, allocating one transfer ring
    /// per endpoint and issuing ConfigEP with a 5-second poll-wait. EP0
    /// (address 0) is rejected; MaxPacket, interval, burst, and type are
    /// packed per `context.rs`. Task 13+ calls this; the Task 4 probe only
    /// needs the default pipe.
    #[allow(dead_code)]
    pub fn configure_endpoints(
        &mut self,
        device: &mut UsbDevice,
        configs: &[EndpointConfig],
        dma: &mut impl DmaAllocator,
        clock: &impl Clock,
    ) -> Result<(), XhciError> {
        if configs.is_empty() {
            return Err(XhciError::TransferFailed(0));
        }
        let is_64 = self.caps().context_64;
        let mut add_flags: u32 = 1 << 0;
        let mut max_dci: u8 = 1;
        let mut staged: Vec<(EndpointConfig, u8, bool, u8, TransferRing)> = Vec::new();
        for config in configs {
            let dir_in = config.address & 0x80 != 0;
            let number = config.address & 0x0F;
            if number == 0 || number > 15 || config.max_packet_size == 0 {
                return Err(XhciError::TransferFailed(0));
            }
            let dci = relay_core::xhci::context::dci(number, dir_in);
            if dci < 2 {
                return Err(XhciError::TransferFailed(0));
            }
            let ep_type = xhci_ep_type(config.transfer_type, dir_in)?;
            if staged
                .iter()
                .any(|(_, staged_dci, _, _, _)| *staged_dci == dci)
            {
                return Err(XhciError::TransferFailed(0));
            }
            let ring = alloc_ring(dma)?;
            add_flags |= 1u32
                .checked_shl(u32::from(dci))
                .ok_or(XhciError::TransferFailed(0))?;
            max_dci = max_dci.max(dci);
            staged.push((config.clone(), dci, dir_in, ep_type, ring));
        }
        let input = alloc_dma(dma, INPUT_CONTEXT_BYTES, 64)?;
        write_input_control(input.device_address, add_flags)?;
        write_slot_context(
            input.device_address,
            32,
            device.speed_raw,
            device.root_port,
            max_dci,
            is_64,
        )?;
        for (config, dci, _, ep_type, ring) in &staged {
            write_ep_context(
                input.device_address,
                input_ep_offset(*dci, is_64)?,
                config.max_packet_size,
                config.interval,
                config.max_burst,
                *ep_type,
                ring.phys | 0x1,
                config.max_packet_size.min(1024),
                config.max_packet_size,
                is_64,
            )?;
        }
        let slot = device.handle.slot_id;
        let config_ep = relay_core::xhci::encode_config_ep(input.device_address, slot, 0);
        let config_phys = self.submit_command(config_ep)?;
        self.ring_command_doorbell()?;
        self.wait_command(config_phys, slot, deadline_secs(clock, 5), clock)?;
        for (_, dci, dir_in, _, ring) in staged {
            device.endpoints.push(EndpointEntry { dci, dir_in, ring });
        }
        let _ = input;
        Ok(())
    }

    /// Runs a control transfer on EP0 (DCI 1): Setup plus an optional
    /// single Data TRB of up to 4096 bytes plus a Status TRB carrying IOC
    /// without CHAIN. Rings doorbell target 1 and poll-waits 5 seconds on
    /// the Status TRB pointer plus the slot match.
    pub fn control(
        &mut self,
        device: &mut UsbDevice,
        setup: [u8; 8],
        data: Option<ControlData<'_>>,
        clock: &impl Clock,
    ) -> Result<(), XhciError> {
        let (has_data, data_in, data_phys, data_len) = match &data {
            Some(stage) => {
                if stage.buffer.len() > 4096 {
                    return Err(XhciError::TransferFailed(0));
                }
                let inbound = matches!(stage.direction, ControlDirection::In);
                (true, inbound, stage.device_address, stage.buffer.len())
            }
            None => (false, false, 0, 0),
        };
        let length = u16::try_from(data_len).map_err(|_| XhciError::TransferFailed(0))?;
        let mut setup_trb = relay_core::xhci::encode_setup_stage(setup, length);
        setup_trb[12] |= 1 << 4;
        // Immediate Data: the 8 setup bytes travel in the TRB itself.
        setup_trb[12] |= 1 << 6;
        let _ = device.ep0.push(setup_trb)?;
        if has_data {
            let data_trb = relay_core::xhci::encode_data_stage(data_phys, data_len, data_in, true);
            let _ = device.ep0.push(data_trb)?;
        }
        let mut status_trb = relay_core::xhci::encode_status_stage(data_in, 0);
        status_trb[12] &= !(1 << 4);
        status_trb[12] |= 1 << 5;
        let status_phys = device.ep0.push(status_trb)?;
        let slot = device.handle.slot_id;
        self.slot_doorbell(slot, 1)?;
        self.wait_transfer(status_phys, slot, 1, deadline_secs(clock, 5), clock)?;
        Ok(())
    }

    /// Runs a bulk transfer as a single Normal TRB of up to 65535 bytes
    /// with IOC on the endpoint's ring. Rings the endpoint DCI and
    /// poll-waits 5 seconds on the TRB pointer plus the slot and endpoint
    /// match. Task 13+ calls this; the Task 4 probe only needs control.
    #[allow(dead_code)]
    pub fn bulk(
        &mut self,
        device: &mut UsbDevice,
        dci: u8,
        data: BulkData<'_>,
        clock: &impl Clock,
    ) -> Result<(), XhciError> {
        let inbound = matches!(data.direction, BulkDirection::In);
        self.normal_transfer(
            device,
            dci,
            inbound,
            data.device_address,
            data.buffer.len(),
            clock,
        )
    }

    /// Runs an interrupt transfer as a single Normal TRB sized to the
    /// caller buffer with IOC on the endpoint's ring. Rings the endpoint
    /// DCI and poll-waits 5 seconds. Task 13+ calls this; the Task 4 probe
    /// only needs control.
    #[allow(dead_code)]
    pub fn interrupt(
        &mut self,
        device: &mut UsbDevice,
        dci: u8,
        data: BulkData<'_>,
        clock: &impl Clock,
    ) -> Result<(), XhciError> {
        let inbound = matches!(data.direction, BulkDirection::In);
        self.normal_transfer(
            device,
            dci,
            inbound,
            data.device_address,
            data.buffer.len(),
            clock,
        )
    }

    #[allow(dead_code)]
    fn normal_transfer(
        &mut self,
        device: &mut UsbDevice,
        dci: u8,
        dir_in: bool,
        phys: u64,
        len: usize,
        clock: &impl Clock,
    ) -> Result<(), XhciError> {
        if !(1..=65535).contains(&len) {
            return Err(XhciError::TransferFailed(0));
        }
        let entry = device
            .endpoints
            .iter_mut()
            .find(|entry| entry.dci == dci)
            .ok_or(XhciError::InvalidSlot)?;
        if entry.dir_in != dir_in {
            return Err(XhciError::TransferFailed(0));
        }
        let trb = relay_core::xhci::encode_normal(phys, len, false, true, 0);
        let trb_phys = entry.ring.push(trb)?;
        let slot = device.handle.slot_id;
        self.slot_doorbell(slot, dci)?;
        self.wait_transfer(trb_phys, slot, dci, deadline_secs(clock, 5), clock)
    }
}

fn deadline_secs(clock: &impl Clock, secs: u64) -> Deadline {
    Deadline(
        clock
            .now_ticks()
            .saturating_add(secs.saturating_mul(clock.ticks_per_second())),
    )
}

fn alloc_dma(
    dma: &mut impl DmaAllocator,
    size: usize,
    align: usize,
) -> Result<DmaAllocation, XhciError> {
    dma.allocate(DmaLayout {
        size,
        align,
        max_address: u64::MAX,
        zeroed: true,
    })
    .map_err(XhciError::Dma)
}

fn alloc_ring(dma: &mut impl DmaAllocator) -> Result<TransferRing, XhciError> {
    let segment = alloc_dma(dma, RING_SEGMENT_BYTES, 64)?;
    if segment.device_address & 0x3F != 0 {
        return Err(XhciError::Allocation);
    }
    let link = relay_core::xhci::encode_link(segment.device_address, true, 1);
    write_trb(segment.device_address, TRANSFER_RING_TRBS - 1, &link)?;
    let ring = TransferRing {
        phys: segment.device_address,
        enqueue: 0,
        cycle: true,
    };
    let _ = segment;
    Ok(ring)
}

fn write_trb(ring_phys: u64, index: usize, trb: &[u8; 16]) -> Result<(), XhciError> {
    let phys = ring_phys
        .checked_add(
            (index as u64)
                .checked_mul(16)
                .ok_or(XhciError::InvalidRegister)?,
        )
        .ok_or(XhciError::InvalidRegister)?;
    let slice = direct_slice_mut(phys, 16).ok_or(XhciError::InvalidRegister)?;
    slice.copy_from_slice(trb);
    Ok(())
}

fn write_u32_le(base: u64, byte_offset: u32, value: u32) -> Result<(), XhciError> {
    let phys = base
        .checked_add(u64::from(byte_offset))
        .ok_or(XhciError::InvalidRegister)?;
    let slice = direct_slice_mut(phys, 4).ok_or(XhciError::InvalidRegister)?;
    slice.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u64_le(phys: u64, value: u64) -> Result<(), XhciError> {
    let slice = direct_slice_mut(phys, 8).ok_or(XhciError::InvalidRegister)?;
    slice.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_input_control(input_phys: u64, add_flags: u32) -> Result<(), XhciError> {
    write_u32_le(input_phys, 0, 0)?;
    write_u32_le(input_phys, 4, add_flags)
}

fn write_slot_context(
    input_phys: u64,
    slot_offset: u32,
    speed_raw: u8,
    port: u8,
    entries: u8,
    _is_64: bool,
) -> Result<(), XhciError> {
    let dword0 = u32::from(entries & 0x1F)
        .checked_shl(27)
        .ok_or(XhciError::InvalidRegister)?
        | u32::from(speed_raw & 0xF)
            .checked_shl(16)
            .ok_or(XhciError::InvalidRegister)?;
    let dword1 = u32::from(port)
        .checked_shl(16)
        .ok_or(XhciError::InvalidRegister)?;
    write_u32_le(input_phys, slot_offset, dword0)?;
    write_u32_le(
        input_phys,
        slot_offset
            .checked_add(4)
            .ok_or(XhciError::InvalidRegister)?,
        dword1,
    )?;
    write_u32_le(
        input_phys,
        slot_offset
            .checked_add(8)
            .ok_or(XhciError::InvalidRegister)?,
        0,
    )?;
    write_u32_le(
        input_phys,
        slot_offset
            .checked_add(12)
            .ok_or(XhciError::InvalidRegister)?,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_ep_context(
    input_phys: u64,
    ep_offset: u32,
    max_packet: u16,
    interval: u8,
    burst: u8,
    ep_type: u8,
    dequeue: u64,
    avg_length: u16,
    esit_payload_lo: u16,
    _is_64: bool,
) -> Result<(), XhciError> {
    let dword0 = u32::from(interval)
        .checked_shl(16)
        .ok_or(XhciError::InvalidRegister)?;
    let dword1 = u32::from(max_packet)
        .checked_shl(16)
        .ok_or(XhciError::InvalidRegister)?
        | u32::from(burst)
            .checked_shl(8)
            .ok_or(XhciError::InvalidRegister)?
        | u32::from(ep_type & 0x7)
            .checked_shl(3)
            .ok_or(XhciError::InvalidRegister)?
        | (3u32).checked_shl(1).ok_or(XhciError::InvalidRegister)?;
    let dword3 = u32::from(esit_payload_lo)
        .checked_shl(16)
        .ok_or(XhciError::InvalidRegister)?
        | u32::from(avg_length);
    write_u32_le(input_phys, ep_offset, dword0)?;
    write_u32_le(
        input_phys,
        ep_offset.checked_add(4).ok_or(XhciError::InvalidRegister)?,
        dword1,
    )?;
    let dequeue_phys = dequeue & !0xF;
    let dequeue_off = ep_offset.checked_add(8).ok_or(XhciError::InvalidRegister)?;
    let dequeue_addr = input_phys
        .checked_add(u64::from(dequeue_off))
        .ok_or(XhciError::InvalidRegister)?;
    let slice = direct_slice_mut(dequeue_addr, 8).ok_or(XhciError::InvalidRegister)?;
    slice.copy_from_slice(&(dequeue_phys | (dequeue & 0x1)).to_le_bytes());
    // Average TRB Length / Max ESIT Payload Lo live at offset+16, past the
    // 8-byte dequeue pointer (offset+8); offset+12 is dequeue-high.
    write_u32_le(
        input_phys,
        ep_offset
            .checked_add(16)
            .ok_or(XhciError::InvalidRegister)?,
        dword3,
    )
}

/// Offset of endpoint DCI `dci` inside an *input* context: 32 bytes of
/// input control context plus `dci` strides. (`device_context_offset`
/// alone is relative to a device/output context, where the slot lives
/// at 0; using it directly for EP0 would overwrite the input slot
/// context at offset 32.)
fn input_ep_offset(dci: u8, is_64: bool) -> Result<u32, XhciError> {
    32u32
        .checked_add(relay_core::xhci::context::device_context_offset(dci, is_64))
        .ok_or(XhciError::InvalidRegister)
}

fn encode_evaluate_context(input_phys: u64, slot: u8) -> [u8; 16] {
    let mut trb = [0u8; 16];
    trb[0..8].copy_from_slice(&input_phys.to_le_bytes());
    let control = (TRB_TYPE_EVALUATE_CONTEXT << 10) | ((u32::from(slot)) << 24);
    trb[12..16].copy_from_slice(&control.to_le_bytes());
    trb
}

fn initial_max_packet(speed: UsbSpeed) -> Result<u16, XhciError> {
    match speed {
        UsbSpeed::Full => Ok(8),
        UsbSpeed::High => Ok(64),
        UsbSpeed::Super | UsbSpeed::SuperPlus => Ok(512),
    }
}

fn xhci_ep_type(transfer_type: u8, dir_in: bool) -> Result<u8, XhciError> {
    match (transfer_type, dir_in) {
        (2, false) => Ok(EP_TYPE_BULK_OUT),
        (2, true) => Ok(EP_TYPE_BULK_IN),
        (3, false) => Ok(EP_TYPE_INT_OUT),
        (3, true) => Ok(EP_TYPE_INT_IN),
        _ => Err(XhciError::TransferFailed(0)),
    }
}
