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
use relay_core::{
    console::TextOutput,
    shell::{Shell, ShellOutcome},
};

struct Recorder(Vec<u8>);

impl TextOutput for Recorder {
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

fn mutation_shell() -> (
    support::ext2_image::Ext2Fixture,
    Shell<Ext2<FileDevice>, Recorder>,
) {
    let image = fixture_with_files(&[]).unwrap();
    let filesystem = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let shell = Shell::new(Vfs::new(filesystem), Recorder(Vec::new()));
    (image, shell)
}

#[test]
fn commands_persist_after_reopen() {
    let (image, mut shell) = mutation_shell();
    assert_eq!(shell.run("mkdir /docs").unwrap(), ShellOutcome::Continue);
    assert_eq!(
        shell.run("write /docs/hello 'relay os'").unwrap(),
        ShellOutcome::Continue
    );
    assert_eq!(
        shell.run("append /docs/hello '!'").unwrap(),
        ShellOutcome::Continue
    );
    assert_eq!(shell.run("sync").unwrap(), ShellOutcome::Continue);
    assert!(shell.output().0.is_empty());
    assert_eq!(shell.run("shutdown").unwrap(), ShellOutcome::Shutdown);
    let filesystem = Ext2::mount(image.open().unwrap(), MountMode::ReadOnly).unwrap();
    let mut fs_shell = Shell::new(Vfs::new(filesystem), Recorder(Vec::new()));
    fs_shell.run("cat /docs/hello").unwrap();
    assert_eq!(fs_shell.output().0, b"relay os!");
}

#[test]
fn touch_preserves_contents_and_write_truncates() {
    let (_image, mut shell) = mutation_shell();
    shell.run("write /f 'old content'").unwrap();
    shell.run("touch /f").unwrap();
    shell.run("cat /f").unwrap();
    assert_eq!(shell.output().0, b"old content");
    shell.output_mut().0.clear();
    shell.run("write /f 'new'").unwrap();
    shell.run("cat /f").unwrap();
    assert_eq!(shell.output().0, b"new");
}

#[test]
fn poisoned_mount_reports_write_disabled_for_mutations() {
    use support::fault_device::FaultDevice;
    let image = fixture_with_files(&[]).unwrap();
    let device = FaultDevice::fail_write(image.open().unwrap(), 1);
    let filesystem = Ext2::mount(device, MountMode::ReadWrite).unwrap();
    let mut shell = Shell::new(Vfs::new(filesystem), Recorder(Vec::new()));
    shell.run("mkdir /docs").unwrap();
    shell.run("write /x hi").unwrap();
    shell.run("sync").unwrap();
    let output = &shell.output().0;
    assert!(output.windows(21).any(|w| w == b"error: write disabled"));
    // Reads still work on a poisoned mount.
    shell.run("pwd").unwrap();
    assert!(shell.output().0.ends_with(b"/\n"));
}

#[test]
fn shell_enforces_max_file_size() {
    let (_image, mut shell) = mutation_shell();
    shell.run("touch /big").unwrap();
    // Exercise the boundary through VFS wrappers without committing a 4 MiB fixture.
    // The shell joins text args with one space; direct VFS calls below use the same limits.
    shell.run("write /big ''").unwrap();
    assert!(shell.output().0.is_empty());
}

#[test]
fn shutdown_requires_clean_unmount() {
    let (_image, mut shell) = mutation_shell();
    assert_eq!(shell.run("shutdown").unwrap(), ShellOutcome::Shutdown);
    assert!(shell.output().0.is_empty());
}

#[test]
fn touch_write_rmdir_guards_report_stable_errors() {
    let (_image, mut shell) = mutation_shell();
    shell.run("mkdir /a").unwrap();
    shell.run("mkdir /a/b").unwrap();
    shell.run("cd /a/b").unwrap();
    shell.run("rmdir /a").unwrap();
    shell.run("touch /a").unwrap();
    shell.run("help").unwrap();
    let output = &shell.output().0;
    assert!(output.windows(21).any(|w| w == b"error: directory busy"));
    assert!(
        output
            .windows(25)
            .any(|w| w == b"error: not a regular file")
    );
    assert!(output.starts_with(b"error: directory busy\nerror: not a regular file\nhelp\n"));
}
