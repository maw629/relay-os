use relay_core::mmio::{MapError, cover_range, is_canonical, page_indices};

#[test]
fn indices_split_canonical_addresses_for_both_depths() {
    assert_eq!(
        page_indices(0xFFFF_C000_0000_1000, false),
        [0, 384, 0, 0, 1]
    );
    assert_eq!(page_indices(0xFFFF_8000_0000_0000, false)[1], 256);
    let five = page_indices(0xFFFF_0000_0000_1000, true);
    assert_eq!(five[0], 0x1FF);
    assert_eq!(&five[1..], &[0, 0, 0, 1]);
    assert!(is_canonical(0xFFFF_C000_0000_1000, false));
    assert!(is_canonical(0x0000_7FFF_FFFF_FFFF, false));
    assert!(!is_canonical(0x1234_5678_9ABC_DEF0, false));
    assert!(is_canonical(0xFFFF_0000_0000_1000, true));
    assert!(!is_canonical(0x0100_0000_0000_0000, true));
}

#[test]
fn cover_range_aligns_and_counts_pages() {
    assert_eq!(cover_range(0x1234, 0x2000), Ok((0x1000, 3, 0x234)));
    assert_eq!(cover_range(0x4000, 0x1000), Ok((0x4000, 1, 0)));
    assert_eq!(
        cover_range(0x8000_0000_0000, 1),
        Err(MapError::InvalidRange)
    );
    assert_eq!(cover_range(0x1000, 0), Err(MapError::InvalidRange));
    assert_eq!(
        cover_range(u64::MAX - 0xFFF, 0x2000),
        Err(MapError::InvalidRange)
    );
}

#[test]
fn uc_flags_match_loader_bar_mappings() {
    assert_eq!(
        relay_core::mmio::UC_MMIO_FLAGS,
        1 | (1 << 1) | (1 << 3) | (1 << 4) | (1 << 63)
    );
}
