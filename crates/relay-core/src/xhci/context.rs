pub fn context_stride(is_64: bool) -> usize {
    if is_64 { 64 } else { 32 }
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
