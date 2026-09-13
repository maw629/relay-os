mod support;

use relay_core::{
    block::BlockDevice,
    ext2::{Ext2, Ext2Error, MountHealth, MountMode},
    fs::Name,
};
use support::{ext2_image::fixture_with_files, fault_device::FaultDevice};

fn name(bytes: &[u8]) -> Name {
    Name::new(bytes).unwrap()
}

fn superblock_state(image: &support::ext2_image::Ext2Fixture) -> u16 {
    let mut device = image.open().unwrap();
    let mut superblock = [0; 1024];
    device.read_sectors(2, &mut superblock).unwrap();
    u16::from_le_bytes([superblock[58], superblock[59]])
}

#[test]
fn read_only_mount_reports_read_only_health_and_rejects_mutation() {
    let image = fixture_with_files(&[("file", b"relay")]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadOnly).unwrap();

    assert_eq!(fs.health(), MountHealth::ReadOnly);
    assert_eq!(
        fs.create_file(fs.root(), &name(b"a")),
        Err(Ext2Error::ReadOnly)
    );
}

#[test]
fn failed_metadata_write_disables_later_mutation() {
    let image = fixture_with_files(&[]).unwrap();
    let device = FaultDevice::fail_write(image.open().unwrap(), 1);
    let mut fs = Ext2::mount(device, MountMode::ReadWrite).unwrap();

    assert!(fs.create_file(fs.root(), &name(b"a")).is_err());
    assert_eq!(fs.health(), MountHealth::WriteFailed);
    assert_eq!(
        fs.create_file(fs.root(), &name(b"b")),
        Err(Ext2Error::WriteDisabled)
    );
}

#[test]
fn read_write_mount_clears_dirty_state_and_unmount_restores_clean() {
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();

    assert_eq!(fs.health(), MountHealth::Writable);
    assert_eq!(superblock_state(&image), 0);
    assert!(matches!(
        Ext2::mount(image.open().unwrap(), MountMode::ReadWrite),
        Err(Ext2Error::MountRequiresCleanFilesystem),
    ));

    fs.unmount().unwrap();

    assert_eq!(fs.health(), MountHealth::Unmounted);
    assert_eq!(superblock_state(&image), 1);
    assert!(Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).is_ok());
}

#[test]
fn create_write_and_reopen_survive_unmount() {
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let node = fs.create_file(fs.root(), &name(b"hello")).unwrap();

    fs.write_at(node, 0, b"relay os").unwrap();
    fs.unmount().unwrap();

    let mut reopened = Ext2::mount(image.open().unwrap(), MountMode::ReadOnly).unwrap();
    let found = reopened.lookup(reopened.root(), &name(b"hello")).unwrap();
    let mut bytes = [0; 8];

    assert_eq!(reopened.read_at(found, 0, &mut bytes).unwrap(), 8);
    assert_eq!(&bytes, b"relay os");
}

#[test]
fn maximum_file_size_is_exact() {
    let big = vec![0x5a; 4_243_456];
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let node = fs.create_file(fs.root(), &name(b"big")).unwrap();

    assert!(fs.write_at(node, 0, &big).is_ok());
    assert_eq!(
        fs.write_at(node, 4_243_456, &[0]),
        Err(Ext2Error::FileTooLarge)
    );
    assert_eq!(fs.truncate(node, 4_243_457), Err(Ext2Error::FileTooLarge));
}

#[test]
fn write_over_holey_pointers_zero_fills_without_garbage() {
    let contents = vec![0x41; 13 * 4096];
    let mut image = fixture_with_files(&[("hole", &contents)]).unwrap();
    image.clear_first_data_pointer("hole");
    image.set_first_indirect_data_pointer("hole", 0);
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let node = fs.lookup(fs.root(), &name(b"hole")).unwrap();

    fs.write_at(node, 0, b"hi").unwrap();
    fs.write_at(node, 12 * 4096, b"hi").unwrap();

    let mut first = [0xff; 16];
    assert_eq!(fs.read_at(node, 0, &mut first).unwrap(), 16);
    assert_eq!(&first[..2], b"hi");
    assert!(first[2..].iter().all(|byte| *byte == 0));

    let mut indirect = [0xff; 16];
    assert_eq!(fs.read_at(node, 12 * 4096, &mut indirect).unwrap(), 16);
    assert_eq!(&indirect[..2], b"hi");
    assert!(indirect[2..].iter().all(|byte| *byte == 0));

    let mut middle = [0; 16];
    assert_eq!(fs.read_at(node, 4096, &mut middle).unwrap(), 16);
    assert!(middle.iter().all(|byte| *byte == 0x41));
}

#[test]
fn write_crosses_direct_to_single_indirect_and_counts_indirect() {
    let bytes: Vec<u8> = (0..13 * 4096).map(|index| index as u8).collect();
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let node = fs.create_file(fs.root(), &name(b"boundary")).unwrap();

    fs.write_at(node, 0, &bytes).unwrap();

    let mut edge = [0; 2];
    assert_eq!(fs.read_at(node, 12 * 4096 - 1, &mut edge).unwrap(), 2);
    assert_eq!(edge, [0xff, 0x00]);
}
