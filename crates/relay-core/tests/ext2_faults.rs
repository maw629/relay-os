mod support;

use relay_core::{
    block::{BlockDevice, BlockError, BlockGeometry},
    ext2::{Ext2, Ext2Error, MountHealth, MountMode},
    fs::Name,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
};
use support::{
    ext2_image::{Ext2Fixture, fixture_with_files},
    fault_device::FaultDevice,
    file_device::FileDevice,
    power_cut_device::PowerCutDevice,
};

fn name(bytes: &[u8]) -> Name {
    Name::new(bytes).unwrap()
}

#[test]
fn unmount_marks_clean_and_blocks_later_mutation() {
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    fs.create_file(fs.root(), &name(b"a")).unwrap();

    fs.unmount().unwrap();

    assert_eq!(fs.health(), MountHealth::Unmounted);
    assert_eq!(
        fs.create_file(fs.root(), &name(b"b")),
        Err(Ext2Error::WriteDisabled)
    );
}

#[test]
fn every_write_fault_poisons_without_panic() {
    let setup = || fixture_with_files(&[]).unwrap();
    let (measured, _) = footprint(&setup, run_create_file);
    let mut saw_fault = false;
    let mut saw_success = false;
    for index in 0..measured + WRITE_MARGIN {
        let image = setup();
        let device = FaultDevice::fail_write(image.open().unwrap(), index);
        let Ok(mut fs) = Ext2::mount(device, MountMode::ReadWrite) else {
            continue;
        };
        let result = run_create_file(&mut fs);
        if result.is_err() {
            assert_eq!(fs.health(), MountHealth::WriteFailed);
            assert_eq!(
                fs.create_file(fs.root(), &name(b"b")),
                Err(Ext2Error::WriteDisabled)
            );
            saw_fault = true;
        } else {
            assert_eq!(fs.health(), MountHealth::Writable);
            saw_success = true;
        }
    }
    assert!(saw_fault);
    assert!(saw_success);
}

// Sweep bounds derive from measured unfaulted footprints (mount + op)
// plus a small margin; saw_fault/saw_success guards fail loudly if a
// bound no longer spans the full footprint.
const WRITE_MARGIN: usize = 4;
const FLUSH_MARGIN: usize = 4;

fn assert_clean_state(image: &Ext2Fixture) {
    let mut device = image.open().unwrap();
    let mut superblock = [0; 1024];
    device.read_sectors(2, &mut superblock).unwrap();
    assert_eq!(u16::from_le_bytes([superblock[58], superblock[59]]), 1);
    let mut raw = [0; 512];
    let mut file = fs::File::open(image.path()).unwrap();
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(1082)).unwrap();
    file.read_exact(&mut raw[..2]).unwrap();
    assert_eq!(u16::from_le_bytes([raw[0], raw[1]]), 1);
}

fn e2fsck_clean(path: &Path) {
    let output = Command::new("e2fsck")
        .args(["-fn", path.to_str().unwrap()])
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "e2fsck -fn failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stdout)
    );
}

fn e2fsck_repair(path: &Path) {
    let output = Command::new("e2fsck")
        .args(["-y", "-f", path.to_str().unwrap()])
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "e2fsck -y failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stdout)
    );
}

static NEXT_COPY: AtomicUsize = AtomicUsize::new(0);

fn disposable_copy(image: &Ext2Fixture) -> PathBuf {
    let id = NEXT_COPY.fetch_add(1, Ordering::Relaxed);
    let copy = std::env::temp_dir().join(format!(
        "relay-ext2-faults-{}-{id}.ext2",
        std::process::id()
    ));
    fs::copy(image.path(), &copy).unwrap();
    copy
}

fn assert_no_duplicate_blocks(path: &Path) {
    let bytes = fs::read(path).unwrap();
    let table = u32::from_le_bytes(bytes[4096 + 8..4096 + 12].try_into().unwrap());
    let mut owned: HashSet<u32> = HashSet::new();
    for number in 2..=4096u32 {
        let offset = table as usize * 4096 + (number as usize - 1) * 256;
        let mode = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
        if !matches!(mode & 0xf000, 0x8000 | 0x4000) {
            continue;
        }
        let links = u16::from_le_bytes(bytes[offset + 26..offset + 28].try_into().unwrap());
        if links == 0 {
            continue;
        }
        for index in 0..12 {
            let pointer = u32::from_le_bytes(
                bytes[offset + 40 + index * 4..offset + 44 + index * 4]
                    .try_into()
                    .unwrap(),
            );
            if pointer != 0 {
                assert!(owned.insert(pointer), "duplicate block {pointer}");
            }
        }
        let indirect = u32::from_le_bytes(bytes[offset + 88..offset + 92].try_into().unwrap());
        assert_eq!(
            u32::from_le_bytes(bytes[offset + 92..offset + 96].try_into().unwrap()),
            0
        );
        assert_eq!(
            u32::from_le_bytes(bytes[offset + 96..offset + 100].try_into().unwrap()),
            0
        );
        if indirect != 0 {
            assert!(owned.insert(indirect), "duplicate block {indirect}");
            let start = indirect as usize * 4096;
            for index in 0..1024 {
                let pointer = u32::from_le_bytes(
                    bytes[start + index * 4..start + index * 4 + 4]
                        .try_into()
                        .unwrap(),
                );
                if pointer != 0 {
                    assert!(owned.insert(pointer), "duplicate block {pointer}");
                }
            }
        }
    }
}

struct Probe<D> {
    inner: D,
    writes: Rc<Cell<usize>>,
    flushes: Rc<Cell<usize>>,
}

impl<D: BlockDevice> BlockDevice for Probe<D> {
    fn geometry(&self) -> BlockGeometry {
        self.inner.geometry()
    }

    fn read_sectors(&mut self, first_lba: u64, dst: &mut [u8]) -> Result<(), BlockError> {
        self.inner.read_sectors(first_lba, dst)
    }

    fn write_sectors(&mut self, first_lba: u64, src: &[u8]) -> Result<(), BlockError> {
        self.writes.set(self.writes.get() + 1);
        self.inner.write_sectors(first_lba, src)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        self.flushes.set(self.flushes.get() + 1);
        self.inner.flush()
    }
}

fn footprint(
    setup: &impl Fn() -> Ext2Fixture,
    run: fn(&mut Ext2<Probe<FileDevice>>) -> Result<(), Ext2Error>,
) -> (usize, usize) {
    let image = setup();
    let probe = Probe {
        inner: image.open().unwrap(),
        writes: Rc::new(Cell::new(0)),
        flushes: Rc::new(Cell::new(0)),
    };
    let writes = probe.writes.clone();
    let flushes = probe.flushes.clone();
    let mut fs = Ext2::mount(probe, MountMode::ReadWrite).unwrap();
    run(&mut fs).unwrap();
    (writes.get(), flushes.get())
}

fn check_write_sweep(
    setup: impl Fn() -> Ext2Fixture,
    run: fn(&mut Ext2<FaultDevice<FileDevice>>) -> Result<(), Ext2Error>,
    bound: usize,
) {
    let mut saw_fault = false;
    let mut saw_success = false;
    for index in 0..bound {
        let image = setup();
        let device = FaultDevice::fail_write(image.open().unwrap(), index);
        let Ok(mut fs) = Ext2::mount(device, MountMode::ReadWrite) else {
            continue;
        };
        if run(&mut fs).is_err() {
            assert_eq!(fs.health(), MountHealth::WriteFailed);
            assert_eq!(
                fs.create_file(fs.root(), &name(b"z")),
                Err(Ext2Error::WriteDisabled)
            );
            saw_fault = true;
        } else {
            assert_eq!(fs.health(), MountHealth::Writable);
            saw_success = true;
        }
    }
    assert!(saw_fault);
    assert!(saw_success);
}

fn check_flush_sweep(
    setup: impl Fn() -> Ext2Fixture,
    run: fn(&mut Ext2<FaultDevice<FileDevice>>) -> Result<(), Ext2Error>,
    bound: usize,
) {
    let mut saw_fault = false;
    let mut saw_success = false;
    for index in 0..bound {
        let image = setup();
        let device = FaultDevice::fail_flush(image.open().unwrap(), index);
        let Ok(mut fs) = Ext2::mount(device, MountMode::ReadWrite) else {
            continue;
        };
        if run(&mut fs).is_err() {
            assert_eq!(fs.health(), MountHealth::WriteFailed);
            assert_eq!(
                fs.create_file(fs.root(), &name(b"z")),
                Err(Ext2Error::WriteDisabled)
            );
            saw_fault = true;
        } else {
            assert_eq!(fs.health(), MountHealth::Writable);
            saw_success = true;
        }
    }
    assert!(saw_fault);
    assert!(saw_success);
}

fn two_block_contents() -> Vec<u8> {
    (0..5000).map(|index| index as u8).collect()
}

fn two_block_grow() -> Vec<u8> {
    (0..8192).map(|index| index as u8).collect()
}

fn run_create_file<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<(), Ext2Error> {
    fs.create_file(fs.root(), &name(b"a")).map(|_| ())
}

fn run_write_at_grow<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<(), Ext2Error> {
    let node = fs.lookup(fs.root(), &name(b"f")).unwrap();
    fs.write_at(node, 0, &two_block_grow())
}

fn run_truncate_grow<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<(), Ext2Error> {
    let node = fs.lookup(fs.root(), &name(b"f")).unwrap();
    fs.truncate(node, 8192)
}

fn run_truncate_shrink<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<(), Ext2Error> {
    let node = fs.lookup(fs.root(), &name(b"f")).unwrap();
    fs.truncate(node, 0)
}

fn run_unlink_file<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<(), Ext2Error> {
    fs.unlink_file(fs.root(), &name(b"f"))
}

fn run_create_dir<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<(), Ext2Error> {
    fs.create_dir(fs.root(), &name(b"d")).map(|_| ())
}

fn run_remove_dir<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<(), Ext2Error> {
    fs.remove_dir(fs.root(), &name(b"d"))
}

fn setup_empty_file() -> Ext2Fixture {
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    fs.create_file(fs.root(), &name(b"f")).unwrap();
    fs.unmount().unwrap();
    image
}

fn setup_empty_dir() -> Ext2Fixture {
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    fs.create_dir(fs.root(), &name(b"d")).unwrap();
    fs.unmount().unwrap();
    image
}

#[test]
fn sweep_create_file_poisons() {
    let setup = || fixture_with_files(&[]).unwrap();
    let (writes, flushes) = footprint(&setup, run_create_file);
    check_write_sweep(setup, run_create_file, writes + WRITE_MARGIN);
    let setup = || fixture_with_files(&[]).unwrap();
    check_flush_sweep(setup, run_create_file, flushes + FLUSH_MARGIN);
}

#[test]
fn sweep_write_at_poisons() {
    let (writes, flushes) = footprint(&setup_empty_file, run_write_at_grow);
    check_write_sweep(setup_empty_file, run_write_at_grow, writes + WRITE_MARGIN);
    check_flush_sweep(setup_empty_file, run_write_at_grow, flushes + FLUSH_MARGIN);
}

#[test]
fn sweep_write_at_grow_repairs_at_every_fault() {
    let (writes, _) = footprint(&setup_empty_file, run_write_at_grow);
    let mut saw_fault = false;
    let mut saw_success = false;
    for index in 0..writes + WRITE_MARGIN {
        let image = setup_empty_file();
        let device = FaultDevice::fail_write(image.open().unwrap(), index);
        let Ok(mut fs) = Ext2::mount(device, MountMode::ReadWrite) else {
            continue;
        };
        if run_write_at_grow(&mut fs).is_err() {
            assert_eq!(fs.health(), MountHealth::WriteFailed);
            assert_eq!(
                fs.create_file(fs.root(), &name(b"z")),
                Err(Ext2Error::WriteDisabled)
            );
            saw_fault = true;
            drop(fs);
            let copy = disposable_copy(&image);
            e2fsck_repair(&copy);
            e2fsck_clean(&copy);
            assert_no_duplicate_blocks(&copy);
            fs::remove_file(&copy).unwrap();
        } else {
            assert_eq!(fs.health(), MountHealth::Writable);
            saw_success = true;
        }
    }
    assert!(saw_fault);
    assert!(saw_success);
}

#[test]
fn sweep_truncate_poisons() {
    let (grow_writes, grow_flushes) = footprint(&setup_empty_file, run_truncate_grow);
    check_write_sweep(
        setup_empty_file,
        run_truncate_grow,
        grow_writes + WRITE_MARGIN,
    );
    check_flush_sweep(
        setup_empty_file,
        run_truncate_grow,
        grow_flushes + FLUSH_MARGIN,
    );
    let contents = two_block_contents();
    let setup = move || fixture_with_files(&[("f", &contents)]).unwrap();
    let (shrink_writes, shrink_flushes) = footprint(&setup, run_truncate_shrink);
    check_write_sweep(setup, run_truncate_shrink, shrink_writes + WRITE_MARGIN);
    let contents = two_block_contents();
    let setup = move || fixture_with_files(&[("f", &contents)]).unwrap();
    check_flush_sweep(setup, run_truncate_shrink, shrink_flushes + FLUSH_MARGIN);
}

#[test]
fn sweep_unlink_file_poisons() {
    let contents = two_block_contents();
    let setup = move || fixture_with_files(&[("f", &contents)]).unwrap();
    let (writes, flushes) = footprint(&setup, run_unlink_file);
    check_write_sweep(setup, run_unlink_file, writes + WRITE_MARGIN);
    let contents = two_block_contents();
    let setup = move || fixture_with_files(&[("f", &contents)]).unwrap();
    check_flush_sweep(setup, run_unlink_file, flushes + FLUSH_MARGIN);
}

#[test]
fn sweep_create_dir_poisons() {
    let setup = || fixture_with_files(&[]).unwrap();
    let (writes, flushes) = footprint(&setup, run_create_dir);
    check_write_sweep(setup, run_create_dir, writes + WRITE_MARGIN);
    let setup = || fixture_with_files(&[]).unwrap();
    check_flush_sweep(setup, run_create_dir, flushes + FLUSH_MARGIN);
}

#[test]
fn sweep_remove_dir_poisons() {
    let (writes, flushes) = footprint(&setup_empty_dir, run_remove_dir);
    check_write_sweep(setup_empty_dir, run_remove_dir, writes + WRITE_MARGIN);
    check_flush_sweep(setup_empty_dir, run_remove_dir, flushes + FLUSH_MARGIN);
}

#[test]
fn sync_requires_writable_and_preserves_health() {
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadOnly).unwrap();
    assert_eq!(fs.sync(), Err(Ext2Error::ReadOnly));

    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    fs.sync().unwrap();
    assert_eq!(fs.health(), MountHealth::Writable);
    fs.unmount().unwrap();
    assert_eq!(fs.sync(), Err(Ext2Error::WriteDisabled));
}

#[test]
fn unmount_marks_clean_state_and_disables_reads() {
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    fs.create_file(fs.root(), &name(b"a")).unwrap();
    fs.sync().unwrap();
    fs.unmount().unwrap();

    assert_clean_state(&image);
    assert_eq!(fs.health(), MountHealth::Unmounted);
    assert_eq!(fs.read_dir(fs.root()), Err(Ext2Error::WriteDisabled));
    let mut byte = [0; 1];
    assert_eq!(
        fs.read_at(fs.root(), 0, &mut byte),
        Err(Ext2Error::WriteDisabled)
    );
    assert_eq!(fs.sync(), Err(Ext2Error::WriteDisabled));
    assert_eq!(
        fs.create_file(fs.root(), &name(b"b")),
        Err(Ext2Error::WriteDisabled)
    );
}

#[test]
fn power_cut_overlay_discards_unflushed_sectors() {
    let image = fixture_with_files(&[]).unwrap();
    let mut overlay = PowerCutDevice::new(image.open().unwrap());
    let mut original = [0; 512];
    overlay.read_sectors(100, &mut original).unwrap();

    let pattern = [0xA5; 512];
    overlay.write_sectors(100, &pattern).unwrap();

    // The inner media still holds the original bytes: unflushed overlay
    // writes are invisible to a direct reader.
    let mut inner = [0; 512];
    image.open().unwrap().read_sectors(100, &mut inner).unwrap();
    assert_eq!(inner, original);
    // ...but visible through the overlay itself.
    let mut shadowed = [0; 512];
    overlay.read_sectors(100, &mut shadowed).unwrap();
    assert_eq!(shadowed, pattern);

    overlay.power_cut();

    let mut after_cut = [0; 512];
    overlay.read_sectors(100, &mut after_cut).unwrap();
    assert_eq!(after_cut, original);
    let mut inner_after_cut = [0; 512];
    image
        .open()
        .unwrap()
        .read_sectors(100, &mut inner_after_cut)
        .unwrap();
    assert_eq!(inner_after_cut, original);

    // Write + flush persists to the inner media.
    overlay.write_sectors(100, &pattern).unwrap();
    overlay.flush().unwrap();
    let mut persisted = [0; 512];
    image
        .open()
        .unwrap()
        .read_sectors(100, &mut persisted)
        .unwrap();
    assert_eq!(persisted, pattern);
}

fn power_cut_roundtrip(
    setup: impl FnOnce() -> Ext2Fixture,
    mutate: impl FnOnce(&mut Ext2<SharedCut>),
) {
    let image = setup();
    let shared = Rc::new(RefCell::new(PowerCutDevice::new(image.open().unwrap())));
    {
        let mut fs = Ext2::mount(
            SharedCut {
                inner: shared.clone(),
            },
            MountMode::ReadWrite,
        )
        .unwrap();
        mutate(&mut fs);
        shared.borrow_mut().power_cut();
    }
    // Reopen the underlying media after the cut.
    let copy = disposable_copy(&image);
    e2fsck_repair(&copy);
    e2fsck_clean(&copy);
    assert_no_duplicate_blocks(&copy);
    fs::remove_file(&copy).unwrap();
}

struct SharedCut {
    inner: Rc<RefCell<PowerCutDevice<FileDevice>>>,
}

impl BlockDevice for SharedCut {
    fn geometry(&self) -> BlockGeometry {
        self.inner.borrow().geometry()
    }

    fn read_sectors(&mut self, first_lba: u64, dst: &mut [u8]) -> Result<(), BlockError> {
        self.inner.borrow_mut().read_sectors(first_lba, dst)
    }

    fn write_sectors(&mut self, first_lba: u64, src: &[u8]) -> Result<(), BlockError> {
        self.inner.borrow_mut().write_sectors(first_lba, src)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        self.inner.borrow_mut().flush()
    }
}

#[test]
fn power_cut_create_file_repairs_clean() {
    power_cut_roundtrip(
        || fixture_with_files(&[]).unwrap(),
        |fs| {
            fs.create_file(fs.root(), &name(b"pc-a")).unwrap();
        },
    );
}

#[test]
fn power_cut_write_at_repairs_clean() {
    power_cut_roundtrip(
        || fixture_with_files(&[("f", b"hello")]).unwrap(),
        |fs| {
            let node = fs.lookup(fs.root(), &name(b"f")).unwrap();
            fs.write_at(node, 0, b"relay os power cut").unwrap();
        },
    );
}

#[test]
fn power_cut_truncate_repairs_clean() {
    let contents = two_block_contents();
    power_cut_roundtrip(
        move || fixture_with_files(&[("f", &contents)]).unwrap(),
        |fs| {
            let node = fs.lookup(fs.root(), &name(b"f")).unwrap();
            fs.truncate(node, 0).unwrap();
        },
    );
}

#[test]
fn power_cut_unlink_repairs_clean() {
    let contents = two_block_contents();
    power_cut_roundtrip(
        move || fixture_with_files(&[("f", &contents)]).unwrap(),
        |fs| {
            fs.unlink_file(fs.root(), &name(b"f")).unwrap();
        },
    );
}

#[test]
fn power_cut_create_dir_repairs_clean() {
    power_cut_roundtrip(
        || fixture_with_files(&[]).unwrap(),
        |fs| {
            fs.create_dir(fs.root(), &name(b"pc-d")).unwrap();
        },
    );
}

#[test]
fn power_cut_remove_dir_repairs_clean() {
    power_cut_roundtrip(
        || {
            let image = fixture_with_files(&[]).unwrap();
            let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
            fs.create_dir(fs.root(), &name(b"pc-d")).unwrap();
            fs.unmount().unwrap();
            image
        },
        |fs| {
            fs.remove_dir(fs.root(), &name(b"pc-d")).unwrap();
        },
    );
}

#[test]
fn success_paths_persist_and_pass_host_fsck() {
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let hello = fs.create_file(fs.root(), &name(b"hello")).unwrap();
    fs.write_at(hello, 0, b"relay os").unwrap();
    let docs = fs.create_dir(fs.root(), &name(b"docs")).unwrap();
    let note = fs.create_file(docs, &name(b"note")).unwrap();
    fs.write_at(note, 0, b"nested bytes").unwrap();
    fs.truncate(hello, 5).unwrap();
    fs.unmount().unwrap();

    assert_clean_state(&image);
    e2fsck_clean(image.path());

    let mut reopened = Ext2::mount(image.open().unwrap(), MountMode::ReadOnly).unwrap();
    let found = reopened.lookup(reopened.root(), &name(b"hello")).unwrap();
    let mut bytes = [0; 5];
    assert_eq!(reopened.read_at(found, 0, &mut bytes).unwrap(), 5);
    assert_eq!(&bytes, b"relay");
    let dir = reopened.lookup(reopened.root(), &name(b"docs")).unwrap();
    let entries = reopened.read_dir(dir).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, name(b"note"));
    let mut nested = [0; 12];
    assert_eq!(
        reopened.read_at(entries[0].node, 0, &mut nested).unwrap(),
        12
    );
    assert_eq!(&nested, b"nested bytes");
}

#[test]
fn success_unlink_and_rmdir_pass_host_fsck() {
    let contents = two_block_contents();
    let image = fixture_with_files(&[("gone", &contents)]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let keep = fs.create_dir(fs.root(), &name(b"keep")).unwrap();
    let _ = keep;
    let temp = fs.create_dir(fs.root(), &name(b"temp")).unwrap();
    let inner = fs.create_file(temp, &name(b"inner")).unwrap();
    fs.write_at(inner, 0, b"scratch").unwrap();
    fs.unlink_file(temp, &name(b"inner")).unwrap();
    fs.remove_dir(fs.root(), &name(b"temp")).unwrap();
    fs.unlink_file(fs.root(), &name(b"gone")).unwrap();
    fs.unmount().unwrap();

    assert_clean_state(&image);
    e2fsck_clean(image.path());

    let mut reopened = Ext2::mount(image.open().unwrap(), MountMode::ReadOnly).unwrap();
    let entries = reopened.read_dir(reopened.root()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, name(b"keep"));
}

#[test]
fn failed_read_does_not_poison_mutation() {
    use std::cell::Cell;
    use std::rc::Rc;
    struct FailSecondRead<D> {
        inner: D,
        reads_after_arm: Rc<Cell<usize>>,
    }
    impl<D: BlockDevice> BlockDevice for FailSecondRead<D> {
        fn geometry(&self) -> BlockGeometry {
            self.inner.geometry()
        }
        fn read_sectors(&mut self, first_lba: u64, dst: &mut [u8]) -> Result<(), BlockError> {
            let n = self.reads_after_arm.get();
            // Before arming the counter is usize::MAX (pass-through).
            if n != usize::MAX {
                self.reads_after_arm.set(n + 1);
                // write_at does inode::load (1 read) then read_indirect
                // via read_data_block; fail exactly the indirect read.
                if n == 1 {
                    return Err(BlockError::Transport);
                }
            }
            self.inner.read_sectors(first_lba, dst)
        }
        fn write_sectors(&mut self, first_lba: u64, src: &[u8]) -> Result<(), BlockError> {
            self.inner.write_sectors(first_lba, src)
        }
        fn flush(&mut self) -> Result<(), BlockError> {
            self.inner.flush()
        }
    }
    // Overwrite inside the indirect region of a 13-block file: the second
    // mutation read is the indirect block via read_data_block, before any
    // writes. Failing exactly that read must not poison the mount (M3).
    let contents = vec![0xAB; 13 * 4096];
    let image = fixture_with_files(&[("big", &contents)]).unwrap();
    let reads_after_arm = Rc::new(Cell::new(usize::MAX));
    let device = FailSecondRead {
        inner: image.open().unwrap(),
        reads_after_arm: reads_after_arm.clone(),
    };
    let mut fs = Ext2::mount(device, MountMode::ReadWrite).unwrap();
    let node = fs.lookup(fs.root(), &name(b"big")).unwrap();
    reads_after_arm.set(0);
    let result = fs.write_at(node, 12 * 4096, b"x");
    assert!(result.is_err());
    assert_eq!(fs.health(), MountHealth::Writable);
    // Later mutation must still be allowed.
    fs.create_file(fs.root(), &name(b"still-ok")).unwrap();
}

#[test]
fn generated_root_is_clean_after_xtask_image() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/root.ext2");
    if !root.exists() {
        return;
    }
    e2fsck_clean(&root);
}
