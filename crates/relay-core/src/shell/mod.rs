mod commands;
mod parser;

pub use commands::{Command, parse_command};
pub use parser::{ParseError, tokenize};

use crate::{
    console::TextOutput,
    vfs::{Cwd, FileSystem, FsError, Vfs, VfsError},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellError {
    Parse(ParseError),
    Vfs(VfsError),
    Unavailable,
    Allocation,
}

pub struct Shell<F, O> {
    vfs: Vfs<F>,
    cwd: Cwd,
    output: O,
}

impl<F: FileSystem, O: TextOutput> Shell<F, O> {
    pub fn new(vfs: Vfs<F>, output: O) -> Self {
        let cwd = vfs.initial_cwd();
        Self { vfs, cwd, output }
    }

    pub fn run(&mut self, line: &str) -> Result<(), ShellError> {
        let result = match parse_command(line) {
            Ok(Some(command)) => commands::execute_read_command(
                command,
                &mut self.vfs,
                &mut self.cwd,
                &mut self.output,
            ),
            Ok(None) => Ok(()),
            Err(error) => Err(ShellError::Parse(error)),
        };
        if let Err(error) = result {
            self.write_error(error);
        }
        Ok(())
    }

    pub fn output(&self) -> &O {
        &self.output
    }

    pub fn output_mut(&mut self) -> &mut O {
        &mut self.output
    }

    fn write_error(&mut self, error: ShellError) {
        self.output.write_bytes(b"error: ");
        self.output.write_bytes(error_message(error));
        self.output.write_bytes(b"\n");
    }
}

fn error_message(error: ShellError) -> &'static [u8] {
    match error {
        ShellError::Parse(ParseError::UnknownCommand) => b"invalid command",
        ShellError::Parse(ParseError::Allocation) | ShellError::Allocation => b"out of memory",
        ShellError::Parse(_) => b"invalid arguments",
        ShellError::Vfs(VfsError::InvalidPath) => b"invalid path",
        ShellError::Vfs(VfsError::NotDirectory | VfsError::Fs(FsError::WrongNodeKind)) => {
            b"not a directory"
        }
        ShellError::Vfs(VfsError::NotRegularFile) => b"not a regular file",
        ShellError::Vfs(VfsError::Allocation | VfsError::Fs(FsError::Allocation)) => {
            b"out of memory"
        }
        ShellError::Vfs(VfsError::Fs(FsError::NotFound)) => b"not found",
        ShellError::Vfs(VfsError::Fs(FsError::Corrupt)) => b"filesystem corrupt",
        ShellError::Vfs(VfsError::Fs(FsError::Unsupported)) => b"unsupported file",
        ShellError::Vfs(VfsError::Fs(FsError::Io)) => b"I/O failure",
        ShellError::Vfs(VfsError::Fs(FsError::FileTooLarge)) => b"file too large",
        ShellError::Vfs(VfsError::Fs(FsError::WriteDisabled)) => b"write disabled",
        ShellError::Vfs(VfsError::Fs(FsError::ReadOnly)) => b"read-only filesystem",
        ShellError::Vfs(VfsError::Fs(FsError::AlreadyExists)) => b"already exists",
        ShellError::Vfs(VfsError::Fs(FsError::NotEmpty)) => b"directory not empty",
        ShellError::Vfs(VfsError::Fs(FsError::NoSpace)) => b"no space left",
        ShellError::Vfs(VfsError::Busy) => b"directory busy",
        ShellError::Unavailable => b"command unavailable",
    }
}
