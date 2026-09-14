use relay_core::xhci::ring::Ring;

fn noop() -> [u8; 16] {
    let mut trb = [0; 16];
    trb[12] = 0x00;
    trb[13] = 0x5C;
    trb
}

#[test]
fn producer_wrap_toggles_cycle_after_link_trb() {
    let mut ring = Ring::new(0x1_0000_0000);
    for _ in 0..63 {
        ring.push(noop()).unwrap();
    }
    assert!(ring.is_full());
    assert!(ring.push(noop()).is_err());
    assert_eq!(ring.enqueue_index(), 63);
    ring.pop_for_test();
    assert_eq!(ring.enqueue_index(), 0);
    assert!(!ring.producer_cycle());
}

#[test]
fn consumer_rejects_stale_cycle_entries() {
    let mut ring = Ring::new(0x2_0000_0000);
    assert_eq!(ring.consume_ready_for_test(false), None);
    ring.push(noop()).unwrap();
    assert!(ring.consume_ready_for_test(false).is_some());
    assert_eq!(ring.consume_ready_for_test(true), None);
}

#[test]
fn full_ring_holds_one_slot_free() {
    let mut ring = Ring::new(0x3_0000_0000);
    for _ in 0..63 {
        ring.push(noop()).unwrap();
    }
    assert_eq!(ring.live_count(), 63);
    assert!(ring.push(noop()).is_err());
}

#[test]
fn dequeue_pointer_advances_with_wrap() {
    let mut ring = Ring::new(0x4_0000_0000);
    ring.push(noop()).unwrap();
    assert_eq!(ring.dequeue_phys_for_test(), 0x4_0000_0000);
    ring.advance_for_test();
    assert_eq!(ring.dequeue_phys_for_test(), 0x4_0000_0010);
}
