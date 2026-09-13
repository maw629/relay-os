mod support;

use relay_core::{
    ext2::{Ext2, Ext2Error, MountMode},
    fs::{Name, NodeKind},
};
use support::ext2_image::fixture_with_files;

fn name(bytes: &[u8]) -> Name {
    Name::new(bytes).unwrap()
}

#[test]
fn mkdir_lists_and_persists_after_reopen() {
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let docs = fs.create_dir(fs.root(), &name(b"docs")).unwrap();

    assert_eq!(fs.metadata(docs).unwrap().kind, NodeKind::Directory);
    fs.create_file(docs, &name(b"hello")).unwrap();
    fs.unmount().unwrap();

    let mut reopened = Ext2::mount(image.open().unwrap(), MountMode::ReadOnly).unwrap();
    let found = reopened.lookup(reopened.root(), &name(b"docs")).unwrap();
    let entries = reopened.read_dir(found).unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, name(b"hello"));
}

#[test]
fn rmdir_rejects_non_empty_and_root() {
    let mut image = fixture_with_files(&[("link", b"relay")]).unwrap();
    // Point a constructible root entry at the root inode itself (inode and
    // directory file type, so lookup resolves it as the root directory).
    image.set_root_entry_inode("link", 2);
    image.set_root_entry_file_type("link", 2);
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let docs = fs.create_dir(fs.root(), &name(b"docs")).unwrap();
    fs.create_file(docs, &name(b"hello")).unwrap();

    assert_eq!(
        fs.remove_dir(fs.root(), &name(b"docs")),
        Err(Ext2Error::DirectoryNotEmpty)
    );
    // The entry now resolves to the root inode, so removal is rejected even
    // though the name itself is an ordinary constructible name.
    assert_eq!(
        fs.remove_dir(fs.root(), &name(b"link")),
        Err(Ext2Error::WrongNodeKind)
    );
}

#[test]
fn mkdir_rejects_duplicate_and_rmdir_empties() {
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    fs.create_dir(fs.root(), &name(b"docs")).unwrap();

    assert_eq!(
        fs.create_dir(fs.root(), &name(b"docs")),
        Err(Ext2Error::AlreadyExists)
    );
    assert_eq!(
        fs.create_file(fs.root(), &name(b"docs")),
        Err(Ext2Error::AlreadyExists)
    );

    fs.remove_dir(fs.root(), &name(b"docs")).unwrap();
    assert_eq!(
        fs.lookup(fs.root(), &name(b"docs")),
        Err(Ext2Error::NotFound)
    );

    // Slot coalescing reuse: recreate after removal.
    let docs = fs.create_dir(fs.root(), &name(b"docs")).unwrap();
    assert_eq!(fs.metadata(docs).unwrap().kind, NodeKind::Directory);
    assert!(fs.read_dir(docs).unwrap().is_empty());
    fs.unmount().unwrap();
}
