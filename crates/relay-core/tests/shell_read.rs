mod support;

use relay_core::{
    console::TextOutput,
    ext2::{Ext2, MountMode},
    fs::{DirEntry, Metadata, Name, NodeId},
    shell::Shell,
    vfs::{FileSystem, FsError, Vfs},
};
use support::{
    ext2_image::{fixture_with_directory, fixture_with_files},
    file_device::FileDevice,
};

struct Recorder(Vec<u8>, usize);

impl TextOutput for Recorder {
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
        self.1 += 1;
    }
}

fn fixture_shell(files: &[(&str, &[u8])]) -> Shell<Ext2<FileDevice>, Recorder> {
    let image = fixture_with_files(files).unwrap();
    let filesystem = Ext2::mount(image.open().unwrap(), MountMode::ReadOnly).unwrap();
    Shell::new(Vfs::new(filesystem), Recorder(Vec::new(), 0))
}

fn directory_shell() -> Shell<Ext2<FileDevice>, Recorder> {
    let image = fixture_with_directory("docs", "guide", b"read me").unwrap();
    let filesystem = Ext2::mount(image.open().unwrap(), MountMode::ReadOnly).unwrap();
    Shell::new(Vfs::new(filesystem), Recorder(Vec::new(), 0))
}

#[test]
fn shell_runs_read_commands_against_an_ext2_fixture() {
    let mut shell = fixture_shell(&[("alpha", b"hello\n\x01")]);

    shell.run("pwd").unwrap();
    shell.run("ls").unwrap();
    shell.run("cat alpha").unwrap();
    shell.run("echo relay os").unwrap();

    assert_eq!(shell.output().0, b"/\nalpha file 7\nhello\n?relay os\n");
}

#[test]
fn failed_cd_preserves_the_working_directory_and_writes_one_error_line() {
    let mut shell = fixture_shell(&[("file", b"relay")]);

    shell.run("cd missing").unwrap();
    shell.run("pwd").unwrap();

    assert_eq!(shell.output().0, b"error: not found\n/\n");
}

#[test]
fn help_lists_all_available_commands() {
    let mut shell = fixture_shell(&[]);

    shell.run("help").unwrap();

    assert_eq!(
        shell.output().0,
        b"help\npwd\ncd PATH\nls [PATH]\ncat PATH\necho [TEXT ...]\ntouch PATH\nwrite PATH [TEXT ...]\nappend PATH [TEXT ...]\nmkdir PATH\nrm PATH\nrmdir PATH\nsync\nshutdown\n"
    );
}

#[test]
fn echo_preserves_parsed_empty_and_quoted_arguments() {
    let mut shell = fixture_shell(&[]);

    shell.run("echo").unwrap();
    shell.run("echo '' 'relay os' x\" y\"").unwrap();

    assert_eq!(shell.output().0, b"\n relay os x y\n");
}

#[test]
fn cd_and_ls_use_relative_absolute_and_current_directory_paths() {
    let mut shell = directory_shell();

    shell.run("cd docs").unwrap();
    shell.run("ls").unwrap();
    shell.run("ls /").unwrap();
    shell.run("cd /docs/../docs").unwrap();
    shell.run("pwd").unwrap();

    assert_eq!(shell.output().0, b"guide file 7\ndocs dir 4096\n/docs\n");
}

#[test]
fn cat_sanitizes_binary_bytes_without_adding_a_newline() {
    let mut shell = fixture_shell(&[("binary", b"a\t\r\n\x00\x7f\xffz")]);

    shell.run("cat binary").unwrap();

    assert_eq!(shell.output().0, b"a\t\r\n???z");
}

#[test]
fn command_errors_write_one_stable_line_and_preserve_cwd() {
    let mut shell = fixture_shell(&[("file", b"relay")]);

    shell.run("cat missing").unwrap();
    shell.run("ls file").unwrap();
    shell.run("cd file").unwrap();
    shell.run("unknown").unwrap();
    shell.run("cd").unwrap();
    shell.run("echo 'unterminated").unwrap();
    shell.run("pwd").unwrap();

    assert_eq!(
        shell.output().0,
        b"error: not found\nerror: not a directory\nerror: not a directory\nerror: invalid command\nerror: invalid arguments\nerror: invalid arguments\n/\n"
    );
}

#[test]
fn invalid_paths_and_parser_input_have_stable_error_lines() {
    let mut shell = fixture_shell(&[]);
    let overlong_component = "a".repeat(256);

    shell.run(&format!("ls {overlong_component}")).unwrap();
    shell.run("echo cafe\u{e9}").unwrap();
    shell.run("echo\0").unwrap();

    assert_eq!(
        shell.output().0,
        b"error: invalid path\nerror: invalid arguments\nerror: invalid arguments\n"
    );
}

#[test]
fn filesystem_error_categories_have_stable_error_lines() {
    let root = fixture_root();
    let mut output = Vec::new();
    for (error, expected) in [
        (FsError::WrongNodeKind, b"not a directory\n".as_slice()),
        (FsError::Corrupt, b"filesystem corrupt\n".as_slice()),
        (FsError::Unsupported, b"unsupported file\n".as_slice()),
        (FsError::Io, b"I/O failure\n".as_slice()),
    ] {
        let mut shell = Shell::new(
            Vfs::new(ErrorFileSystem { root, error }),
            Recorder(Vec::new(), 0),
        );

        shell.run("ls").unwrap();
        output.extend_from_slice(&shell.output().0);
        assert_eq!(shell.output().0, [b"error: ".as_slice(), expected].concat());
    }

    assert_eq!(
        output,
        b"error: not a directory\nerror: filesystem corrupt\nerror: unsupported file\nerror: I/O failure\n"
    );
}

#[test]
fn failed_cat_and_ls_preserve_an_existing_directory_cwd() {
    let mut shell = directory_shell();

    shell.run("cd docs").unwrap();
    shell.run("cat .").unwrap();
    shell.run("ls guide").unwrap();
    shell.run("pwd").unwrap();

    assert_eq!(
        shell.output().0,
        b"error: not a regular file\nerror: not a directory\n/docs\n"
    );
}

#[test]
fn cat_writes_a_file_larger_than_one_vfs_buffer() {
    let contents = vec![b'x'; 8193];
    let mut shell = fixture_shell(&[("large", &contents)]);

    shell.run("cat large").unwrap();

    assert_eq!(shell.output().0, contents);
    assert!(shell.output().1 > 1);
}

fn fixture_root() -> NodeId {
    let image = fixture_with_files(&[]).unwrap();
    Ext2::mount(image.open().unwrap(), MountMode::ReadOnly)
        .unwrap()
        .root()
}

struct ErrorFileSystem {
    root: NodeId,
    error: FsError,
}

impl FileSystem for ErrorFileSystem {
    fn root(&self) -> NodeId {
        self.root
    }

    fn metadata(&mut self, _: NodeId) -> Result<Metadata, FsError> {
        Err(self.error)
    }

    fn lookup(&mut self, _: NodeId, _: &Name) -> Result<NodeId, FsError> {
        Err(self.error)
    }

    fn read_dir(&mut self, _: NodeId) -> Result<Vec<DirEntry>, FsError> {
        Err(self.error)
    }

    fn read_at(&mut self, _: NodeId, _: u64, _: &mut [u8]) -> Result<usize, FsError> {
        Err(self.error)
    }
}
