use relay_core::xhci::{
    ControlData, UsbSpeed, XhciError, decode_cmd_complete, decode_port_change,
    decode_transfer_event, encode_address_device, encode_config_ep, encode_data_stage,
    encode_enable_slot, encode_link, encode_noop_cmd, encode_normal, encode_reset_ep,
    encode_setup_stage, encode_status_stage,
};

#[test]
fn portsc_speed_decodes_all_valid_psivs_including_low() {
    let cases = [
        (1u32, UsbSpeed::Full),
        (2u32, UsbSpeed::Low),
        (3u32, UsbSpeed::High),
        (4u32, UsbSpeed::Super),
        (5u32, UsbSpeed::SuperPlus),
    ];
    for (psiv, expected) in cases {
        assert_eq!(UsbSpeed::from_portsc(psiv << 10), Ok(expected));
    }
    for psiv in [0u32, 6, 7, 8, 15] {
        assert_eq!(
            UsbSpeed::from_portsc(psiv << 10),
            Err(XhciError::UnsupportedPlatform)
        );
    }
}

#[test]
fn ep0_initial_packet_size_covers_low_speed_at_8_bytes() {
    assert_eq!(UsbSpeed::Low.ep0_initial_max_packet(), 8);
    assert_eq!(UsbSpeed::Full.ep0_initial_max_packet(), 8);
    assert_eq!(UsbSpeed::High.ep0_initial_max_packet(), 64);
    assert_eq!(UsbSpeed::Super.ep0_initial_max_packet(), 512);
    assert_eq!(UsbSpeed::SuperPlus.ep0_initial_max_packet(), 512);
}

#[test]
fn setup_stage_carries_setup_bytes_and_type() {
    let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x40, 0x00];
    let trb = encode_setup_stage(setup, 8);
    assert_eq!(&trb[0..8], &setup);
    assert_eq!(
        (u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]) >> 10) & 0x3F,
        2
    );
    assert_eq!(trb[15] & 0x01, 0);
}

#[test]
fn data_stage_sets_direction_chain_and_length() {
    let trb = encode_data_stage(0x1_0000_1000, 64, true, true);
    assert_eq!(
        u64::from_le_bytes(trb[0..8].try_into().unwrap()),
        0x1_0000_1000
    );
    assert_eq!(
        u32::from_le_bytes([trb[8], trb[9], trb[10], trb[11]]) & 0x1FFFF,
        64
    );
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
    assert_eq!(
        u64::from_le_bytes(trb[0..8].try_into().unwrap()),
        0x2_0000_0000
    );
    assert_eq!(
        u32::from_le_bytes([trb[8], trb[9], trb[10], trb[11]]) & 0x1FFFF,
        512
    );
    let control = u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]);
    assert_eq!((control >> 10) & 0x3F, 1);
    assert_ne!(control & (1 << 4), 0);
}

#[test]
fn normal_trb_ioc_without_chain_leaves_chain_clear() {
    let trb = encode_normal(0x2_0000_0000, 512, false, true, 1);
    let control = u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]);
    assert_eq!((control >> 10) & 0x3F, 1);
    assert_eq!(control & (1 << 4), 0);
    assert_ne!(control & (1 << 5), 0);
}

#[test]
fn link_trb_points_at_base_with_toggle() {
    let trb = encode_link(0x3_0000_0000, true, 1);
    assert_eq!(
        u64::from_le_bytes(trb[0..8].try_into().unwrap()) & !0xF,
        0x3_0000_0000
    );
    let control = u32::from_le_bytes([trb[12], trb[13], trb[14], trb[15]]);
    assert_eq!((control >> 10) & 0x3F, 6);
    assert_ne!(control & (1 << 1), 0);
}

#[test]
fn command_trbs_carry_slot_and_pointer() {
    let enable = encode_enable_slot(0);
    assert_eq!(
        (u32::from_le_bytes([enable[12], enable[13], enable[14], enable[15]]) >> 10) & 0x3F,
        9
    );
    let addr = encode_address_device(0x4_0000_0000, 3, 0);
    assert_eq!(
        u64::from_le_bytes(addr[0..8].try_into().unwrap()),
        0x4_0000_0000
    );
    assert_eq!(
        (u32::from_le_bytes([addr[12], addr[13], addr[14], addr[15]]) >> 10) & 0x3F,
        11
    );
    let config = encode_config_ep(0x4_0000_1000, 3, 0);
    assert_eq!(
        (u32::from_le_bytes([config[12], config[13], config[14], config[15]]) >> 10) & 0x3F,
        12
    );
    let reset = encode_reset_ep(3, 2, 0);
    assert_eq!(
        (u32::from_le_bytes([reset[12], reset[13], reset[14], reset[15]]) >> 10) & 0x3F,
        14
    );
    let noop = encode_noop_cmd(1);
    assert_eq!(
        (u32::from_le_bytes([noop[12], noop[13], noop[14], noop[15]]) >> 10) & 0x3F,
        23
    );
}

#[test]
fn event_decoders_parse_known_vectors() {
    let mut transfer = [0; 16];
    transfer[0..8].copy_from_slice(&0x1_0000_1000u64.to_le_bytes());
    transfer[8..12].copy_from_slice(&((1u32 << 24) | 8u32).to_le_bytes());
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
    complete[8..12].copy_from_slice(&(1u32 << 24).to_le_bytes());
    let cc = (33u32 << 10) | (7u32 << 24) | 1u32;
    complete[12..16].copy_from_slice(&cc.to_le_bytes());
    let done = decode_cmd_complete(&complete).unwrap();
    assert_eq!(done.pointer, 0x5_0000_0000);
    assert_eq!(done.slot, 7);
    assert_eq!(done.code, 1);
    let mut port = [0; 16];
    port[0..4].copy_from_slice(&0x0200_0000u32.to_le_bytes());
    port[8..12].copy_from_slice(&(1u32 << 24).to_le_bytes());
    let pc = (34u32 << 10) | (1u32 << 24) | 1u32;
    port[12..16].copy_from_slice(&pc.to_le_bytes());
    let change = decode_port_change(&port).unwrap();
    assert_eq!(change.port, 2);
    assert_eq!(change.code, 1);
}

#[test]
fn decoders_reject_bad_types_and_lengths() {
    assert_eq!(
        decode_transfer_event(&[0; 8]),
        Err(XhciError::UnsupportedEvent)
    );
    let mut wrong = [0; 16];
    let control = (9u32 << 10) | 1u32;
    wrong[12..16].copy_from_slice(&control.to_le_bytes());
    assert_eq!(
        decode_transfer_event(&wrong),
        Err(XhciError::UnsupportedEvent)
    );
    assert_eq!(
        decode_cmd_complete(&wrong),
        Err(XhciError::UnsupportedEvent)
    );
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
    assert!(
        ControlData::new(
            relay_core::xhci::ControlDirection::In,
            &mut big,
            &allocation
        )
        .is_err()
    );
}
