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
