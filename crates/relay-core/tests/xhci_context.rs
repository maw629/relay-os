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
