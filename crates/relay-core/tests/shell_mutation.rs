mod support;

use relay_core::{
    ext2::{Ext2, MountMode},
    fs::Name,
    vfs::{FsError, Vfs, VfsError},
};
use support::{ext2_image::fixture_with_files, file_device::FileDevice};

fn fixture_vfs(files: &[(&str, &[u8])]) -> Vfs<Ext2<FileDevice>> {
    let image = fixture_with_files(files).unwrap();
    let filesystem = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    Vfs::new(filesystem)
}

#[allow(dead_code)]
fn name(bytes: &[u8]) -> Name {
    Name::new(bytes).unwrap()
}

#[test]
fn vfs_creates_and_rejects_duplicates() {
    let mut vfs = fixture_vfs(&[]);
    let cwd = vfs.initial_cwd();
    vfs.create_dir(&cwd, "docs").unwrap();
    assert_eq!(
        vfs.create_dir(&cwd, "docs"),
        Err(VfsError::Fs(FsError::AlreadyExists))
    );
    vfs.create_file(&cwd, "docs/hello").unwrap();
    assert_eq!(
        vfs.create_file(&cwd, "docs/hello"),
        Err(VfsError::Fs(FsError::AlreadyExists))
    );
}

#[test]
fn vfs_rejects_trailing_slash_file_ops_and_cwd_ancestor_removal() {
    let mut vfs = fixture_vfs(&[]);
    let cwd = vfs.initial_cwd();
    assert_eq!(vfs.create_file(&cwd, "a/"), Err(VfsError::NotDirectory));
    vfs.create_dir(&cwd, "/a").unwrap();
    vfs.create_dir(&cwd, "/a/b").unwrap();
    let deep = vfs.change_dir(&cwd, "/a/b").unwrap();
    assert_eq!(vfs.remove_dir(&deep, "/a"), Err(VfsError::Busy));
    assert_eq!(vfs.remove_dir(&deep, "/a/b"), Err(VfsError::Busy));
    assert_eq!(vfs.remove_dir(&cwd, "/"), Err(VfsError::InvalidPath));
}
