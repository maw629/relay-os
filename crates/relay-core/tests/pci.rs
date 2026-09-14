use relay_core::pci::{
    BarInfo, McfgRegion, PciAddress, PciConfig, PciError, decode_xhci_caps, ecam_address,
    enumerate_functions, find_xhci, probe_xhci_bar,
};
use std::cell::RefCell;
use std::collections::BTreeMap;

const REGION: McfgRegion = McfgRegion {
    base: 0xE000_0000,
    segment: 0,
    bus_start: 0,
    bus_end: 1,
};
const XHCI: PciAddress = PciAddress {
    segment: 0,
    bus: 0,
    device: 5,
    function: 0,
};

fn address(bus: u8, device: u8, function: u8) -> PciAddress {
    PciAddress {
        segment: 0,
        bus,
        device,
        function,
    }
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
        Self {
            cells: RefCell::new(BTreeMap::new()),
            writes: RefCell::new(Vec::new()),
        }
    }

    fn set(&self, address: PciAddress, offset: u16, value: u32) {
        self.cells.borrow_mut().insert(
            (address.bus, address.device, address.function, offset),
            Cell {
                current: value,
                mask: value,
                sizing: false,
            },
        );
    }

    fn set_bar(&self, address: PciAddress, offset: u16, current: u32, mask: u32) {
        self.cells.borrow_mut().insert(
            (address.bus, address.device, address.function, offset),
            Cell {
                current,
                mask,
                sizing: false,
            },
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
        self.writes
            .borrow()
            .iter()
            .filter(|(_, off, _)| *off == offset)
            .map(|(_, _, value)| *value)
            .collect()
    }

    fn original_command_and_bars_restored_before_enable(&self) -> bool {
        let writes = self.writes.borrow();
        let enables: Vec<usize> = writes
            .iter()
            .enumerate()
            .filter(|(_, (_, off, value))| *off == 0x04 && (*value & 0x2) != 0)
            .map(|(index, _)| index)
            .collect();
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

    unsafe fn write_u32(
        &self,
        address: PciAddress,
        offset: u16,
        value: u32,
    ) -> Result<(), PciError> {
        self.writes.borrow_mut().push((address, offset, value));
        if let Some(cell) = self.cells.borrow_mut().get_mut(&(
            address.bus,
            address.device,
            address.function,
            offset,
        )) {
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
    assert_eq!(
        ecam_address(&REGION, address(0, 5, 0), 0x10),
        Ok(0xE000_0000 + (5 << 15) + 0x10)
    );
    assert_eq!(
        ecam_address(&REGION, address(1, 31, 7), 0xFFC),
        Ok(0xE000_0000 + (1 << 20) + (31 << 15) + (7 << 12) + 0xFFC)
    );
    assert_eq!(
        ecam_address(&REGION, address(2, 0, 0), 0),
        Err(PciError::OutOfRange)
    );
    assert_eq!(
        ecam_address(&REGION, address(0, 32, 0), 0),
        Err(PciError::OutOfRange)
    );
    assert_eq!(
        ecam_address(&REGION, address(0, 0, 8), 0),
        Err(PciError::OutOfRange)
    );
    assert_eq!(
        ecam_address(&REGION, address(0, 0, 0), 2),
        Err(PciError::OutOfRange)
    );
    assert_eq!(
        ecam_address(&REGION, address(0, 0, 0), 0x1000),
        Err(PciError::OutOfRange)
    );
    assert_eq!(
        ecam_address(
            &REGION,
            PciAddress {
                segment: 1,
                bus: 0,
                device: 0,
                function: 0
            },
            0
        ),
        Err(PciError::OutOfRange)
    );
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
    assert!(
        !found
            .iter()
            .any(|(addr, _)| addr.device == 2 && addr.function != 0)
    );
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
        Ok(BarInfo {
            base: 0x9_0000_0000,
            size: 0x1_0000,
            is_64: true
        })
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
