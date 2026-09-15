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
    Low = 2,
    High = 3,
    Super = 4,
    SuperPlus = 5,
}

impl UsbSpeed {
    pub fn from_portsc(value: u32) -> Result<Self, XhciError> {
        match (value >> 10) & 0xF {
            1 => Ok(UsbSpeed::Full),
            2 => Ok(UsbSpeed::Low),
            3 => Ok(UsbSpeed::High),
            4 => Ok(UsbSpeed::Super),
            5 => Ok(UsbSpeed::SuperPlus),
            _ => Err(XhciError::UnsupportedPlatform),
        }
    }

    /// Initial EP0 max-packet size: low/full-speed control endpoints
    /// start at 8 bytes (refined later from the device descriptor),
    /// high-speed at 64, SuperSpeed(Plus) at 512.
    pub fn ep0_initial_max_packet(self) -> u16 {
        match self {
            UsbSpeed::Full | UsbSpeed::Low => 8,
            UsbSpeed::High => 64,
            UsbSpeed::Super | UsbSpeed::SuperPlus => 512,
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
        Ok(Self {
            direction,
            buffer,
            device_address,
        })
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
        Ok(Self {
            direction,
            buffer,
            device_address,
        })
    }
}

fn dma_address_for(buffer: &[u8], allocation: &DmaAllocation) -> Result<u64, XhciError> {
    if buffer.is_empty() {
        return Err(XhciError::TransferFailed(0));
    }
    let cpu_base = allocation.cpu_address.as_ptr() as usize as u64;
    let start = buffer.as_ptr() as usize as u64;
    let offset = start
        .checked_sub(cpu_base)
        .ok_or(XhciError::TransferFailed(0))?;
    let end = (offset as usize)
        .checked_add(buffer.len())
        .ok_or(XhciError::TransferFailed(0))?;
    if end > allocation.len {
        return Err(XhciError::TransferFailed(0));
    }
    allocation
        .device_address
        .checked_add(offset)
        .ok_or(XhciError::TransferFailed(0))
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
    let status = len as u32 & 0x1FFFF;
    let mut control = trb_control(3, 0);
    if dir_in {
        control |= 1 << 16;
    }
    if chain {
        control |= 1 << 4;
    }
    put_trb(&mut trb, phys, status, control);
    trb
}

pub fn encode_status_stage(dir_in: bool, cycle: u8) -> [u8; 16] {
    let mut trb = [0; 16];
    let status = if dir_in { 0 } else { 1 << 16 };
    // NOTE: sets CHAIN by default; callers clear it for terminal Status with IOC (Task 1 test pins this).
    let control = trb_control(4, cycle) | (1 << 4);
    put_trb(&mut trb, 0, status, control);
    trb
}

pub fn encode_normal(phys: u64, len: usize, chain: bool, ioc: bool, cycle: u8) -> [u8; 16] {
    let mut trb = [0; 16];
    let status = len as u32 & 0x1FFFF;
    let mut control = trb_control(1, cycle);
    if chain {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferEvent {
    pub pointer: u64,
    pub length: u32,
    pub code: u8,
    pub slot: u8,
    pub endpoint: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandComplete {
    pub pointer: u64,
    pub code: u8,
    pub slot: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortChange {
    pub port: u8,
    pub code: u8,
}

fn trb_type_of(trb: &[u8]) -> Result<u32, XhciError> {
    let control = u32::from_le_bytes(
        trb.get(12..16)
            .ok_or(XhciError::UnsupportedEvent)?
            .try_into()
            .map_err(|_| XhciError::UnsupportedEvent)?,
    );
    Ok((control >> 10) & 0x3F)
}

pub fn decode_transfer_event(trb: &[u8]) -> Result<TransferEvent, XhciError> {
    if trb.len() != 16 || trb_type_of(trb)? != 32 {
        return Err(XhciError::UnsupportedEvent);
    }
    let pointer = u64::from_le_bytes(
        trb[0..8]
            .try_into()
            .map_err(|_| XhciError::UnsupportedEvent)?,
    );
    let status = u32::from_le_bytes(
        trb[8..12]
            .try_into()
            .map_err(|_| XhciError::UnsupportedEvent)?,
    );
    let control = u32::from_le_bytes(
        trb[12..16]
            .try_into()
            .map_err(|_| XhciError::UnsupportedEvent)?,
    );
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
    let pointer = u64::from_le_bytes(
        trb[0..8]
            .try_into()
            .map_err(|_| XhciError::UnsupportedEvent)?,
    );
    let status = u32::from_le_bytes(
        trb[8..12]
            .try_into()
            .map_err(|_| XhciError::UnsupportedEvent)?,
    );
    let control = u32::from_le_bytes(
        trb[12..16]
            .try_into()
            .map_err(|_| XhciError::UnsupportedEvent)?,
    );
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
    let param = u32::from_le_bytes(
        trb[0..4]
            .try_into()
            .map_err(|_| XhciError::UnsupportedEvent)?,
    );
    let status = u32::from_le_bytes(
        trb[8..12]
            .try_into()
            .map_err(|_| XhciError::UnsupportedEvent)?,
    );
    Ok(PortChange {
        port: ((param >> 24) & 0xFF) as u8,
        code: ((status >> 24) & 0xFF) as u8,
    })
}

pub fn completion_to_error(code: u8) -> Result<(), XhciError> {
    match code {
        1 => Ok(()),
        6 => Err(XhciError::Stalled),
        other => Err(XhciError::TransferFailed(other)),
    }
}
