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
    table.extend_from_slice(&[0; 8]);
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
    image(&[
        (RSDP_PHYS, rsdp_v2(XSDT_PHYS).to_vec()),
        (XSDT_PHYS, xsdt),
        (MCFG_PHYS, mcfg),
    ])
}

#[test]
fn mcfg_parses_known_good_vector() {
    let memory = good_image();
    assert_eq!(
        parse_mcfg(&memory, RSDP_PHYS),
        Ok(McfgRegion {
            base: 0xE000_0000,
            segment: 0,
            bus_start: 0,
            bus_end: 255
        })
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
    let memory = image(&[
        (RSDP_PHYS, rsdp_v2(XSDT_PHYS).to_vec()),
        (XSDT_PHYS, xsdt),
        (DMAR_PHYS, dmar),
    ]);
    assert_eq!(
        parse_mcfg(&memory, RSDP_PHYS),
        Err(AcpiError::UnsupportedPlatform)
    );
    assert_eq!(dmar_present(&memory, RSDP_PHYS), Ok(true));
}

#[test]
fn multi_entry_mcfg_is_unsupported() {
    let mut table = sdt_header(b"MCFG", 76);
    table.extend_from_slice(&[0; 8]);
    table.extend_from_slice(&mcfg_bytes(0xE000_0000, 0, 0, 127)[44..60]);
    table.extend_from_slice(&mcfg_bytes(0xF000_0000, 0, 128, 255)[44..60]);
    fix_checksum(&mut table, 9);
    let mut xsdt = sdt_header(b"XSDT", 44);
    xsdt.extend_from_slice(&MCFG_PHYS.to_le_bytes());
    fix_checksum(&mut xsdt, 9);
    let memory = image(&[
        (RSDP_PHYS, rsdp_v2(XSDT_PHYS).to_vec()),
        (XSDT_PHYS, xsdt),
        (MCFG_PHYS, table),
    ]);
    assert_eq!(
        parse_mcfg(&memory, RSDP_PHYS),
        Err(AcpiError::UnsupportedPlatform)
    );
}

#[test]
fn nonzero_segment_is_unsupported() {
    let mcfg = mcfg_bytes(0xE000_0000, 1, 0, 255);
    let mut xsdt = sdt_header(b"XSDT", 44);
    xsdt.extend_from_slice(&MCFG_PHYS.to_le_bytes());
    fix_checksum(&mut xsdt, 9);
    let memory = image(&[
        (RSDP_PHYS, rsdp_v2(XSDT_PHYS).to_vec()),
        (XSDT_PHYS, xsdt),
        (MCFG_PHYS, mcfg),
    ]);
    assert_eq!(
        parse_mcfg(&memory, RSDP_PHYS),
        Err(AcpiError::UnsupportedPlatform)
    );
}

#[test]
fn bad_mcfg_windows_are_invalid_range() {
    for (base, start, end) in [
        (0u64, 0u8, 255u8),
        (0x1234u64, 0, 0),
        (0x7FFF_FFFF_F000u64, 0, 255),
        (0xE000_0000u64, 5, 4),
    ] {
        let mcfg = mcfg_bytes(base, 0, start, end);
        let mut xsdt = sdt_header(b"XSDT", 44);
        xsdt.extend_from_slice(&MCFG_PHYS.to_le_bytes());
        fix_checksum(&mut xsdt, 9);
        let memory = image(&[
            (RSDP_PHYS, rsdp_v2(XSDT_PHYS).to_vec()),
            (XSDT_PHYS, xsdt),
            (MCFG_PHYS, mcfg),
        ]);
        assert_eq!(
            parse_mcfg(&memory, RSDP_PHYS),
            Err(AcpiError::InvalidRange),
            "base={base:#x}"
        );
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
