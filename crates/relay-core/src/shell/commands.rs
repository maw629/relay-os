use alloc::{string::String, vec::Vec};

use crate::{
    console::TextOutput,
    fs::NodeKind,
    vfs::{Cwd, FileSystem, FsError, Vfs, VfsError},
};

use super::{ParseError, ShellError, ShellOutcome, tokenize};

pub enum Command {
    Help,
    Pwd,
    Cd { path: String },
    Ls { path: Option<String> },
    Cat { path: String },
    Echo { text: Vec<String> },
    Touch { path: String },
    Write { path: String, text: String },
    Append { path: String, text: String },
    Mkdir { path: String },
    Rm { path: String },
    Rmdir { path: String },
    Sync,
    Shutdown,
}

pub fn parse_command(line: &str) -> Result<Option<Command>, ParseError> {
    let mut tokens = tokenize(line)?.into_iter();
    let Some(name) = tokens.next() else {
        return Ok(None);
    };

    let command = match name.as_str() {
        "help" => {
            require_no_arguments(&mut tokens, "help")?;
            Command::Help
        }
        "pwd" => {
            require_no_arguments(&mut tokens, "pwd")?;
            Command::Pwd
        }
        "cd" => Command::Cd {
            path: require_one_argument(&mut tokens, "cd")?,
        },
        "ls" => Command::Ls {
            path: optional_argument(&mut tokens, "ls")?,
        },
        "cat" => Command::Cat {
            path: require_one_argument(&mut tokens, "cat")?,
        },
        "echo" => Command::Echo {
            text: collect_arguments(&mut tokens)?,
        },
        "touch" => Command::Touch {
            path: require_one_argument(&mut tokens, "touch")?,
        },
        "write" => {
            let (path, text) = require_argument_then_join(&mut tokens, "write")?;
            Command::Write { path, text }
        }
        "append" => {
            let (path, text) = require_argument_then_join(&mut tokens, "append")?;
            Command::Append { path, text }
        }
        "mkdir" => Command::Mkdir {
            path: require_one_argument(&mut tokens, "mkdir")?,
        },
        "rm" => Command::Rm {
            path: require_one_argument(&mut tokens, "rm")?,
        },
        "rmdir" => Command::Rmdir {
            path: require_one_argument(&mut tokens, "rmdir")?,
        },
        "sync" => {
            require_no_arguments(&mut tokens, "sync")?;
            Command::Sync
        }
        "shutdown" => {
            require_no_arguments(&mut tokens, "shutdown")?;
            Command::Shutdown
        }
        _ => return Err(ParseError::UnknownCommand),
    };
    Ok(Some(command))
}

pub(super) fn execute_command<F: FileSystem, O: TextOutput>(
    command: Command,
    vfs: &mut Vfs<F>,
    cwd: &mut Cwd,
    output: &mut O,
) -> Result<ShellOutcome, ShellError> {
    match command {
        Command::Help => output.write_bytes(HELP),
        Command::Pwd => {
            let path = vfs.cwd_path(cwd).map_err(ShellError::Vfs)?;
            output.write_bytes(&path);
            output.write_bytes(b"\n");
        }
        Command::Cd { path } => {
            let new_cwd = vfs.change_dir(cwd, &path).map_err(ShellError::Vfs)?;
            *cwd = new_cwd;
        }
        Command::Ls { path } => {
            let entries = vfs.list(cwd, path.as_deref()).map_err(ShellError::Vfs)?;
            for entry in entries {
                let metadata = vfs.metadata(entry.node).map_err(ShellError::Vfs)?;
                output.write_bytes(entry.name.as_bytes());
                output.write_bytes(b" ");
                output.write_bytes(match metadata.kind {
                    NodeKind::Regular => b"file",
                    NodeKind::Directory => b"dir",
                });
                output.write_bytes(b" ");
                write_number(output, metadata.len);
                output.write_bytes(b"\n");
            }
        }
        Command::Cat { path } => {
            vfs.read_file(cwd, &path, |bytes| {
                let mut rendered = [0; 4096];
                for (destination, &byte) in rendered.iter_mut().zip(bytes) {
                    *destination = if matches!(byte, b' '..=b'~' | b'\n' | b'\r' | b'\t') {
                        byte
                    } else {
                        b'?'
                    };
                }
                output.write_bytes(&rendered[..bytes.len()]);
                Ok(())
            })
            .map_err(ShellError::Vfs)?;
        }
        Command::Echo { text } => {
            for (index, argument) in text.iter().enumerate() {
                if index != 0 {
                    output.write_bytes(b" ");
                }
                output.write_bytes(argument.as_bytes());
            }
            output.write_bytes(b"\n");
        }
        Command::Touch { path } => match vfs.create_file(cwd, &path) {
            Ok(_) => {}
            Err(VfsError::Fs(FsError::AlreadyExists)) => {
                vfs.read_file(cwd, &path, |_| Ok(()))
                    .map_err(ShellError::Vfs)?;
            }
            Err(error) => return Err(ShellError::Vfs(error)),
        },
        Command::Write { path, text } => {
            vfs.write_file(cwd, &path, text.as_bytes())
                .map_err(ShellError::Vfs)?;
        }
        Command::Append { path, text } => {
            vfs.append_file(cwd, &path, text.as_bytes())
                .map_err(ShellError::Vfs)?;
        }
        Command::Mkdir { path } => {
            vfs.create_dir(cwd, &path).map_err(ShellError::Vfs)?;
        }
        Command::Rm { path } => {
            vfs.unlink_file(cwd, &path).map_err(ShellError::Vfs)?;
        }
        Command::Rmdir { path } => {
            vfs.remove_dir(cwd, &path).map_err(ShellError::Vfs)?;
        }
        Command::Sync => {
            vfs.sync_fs().map_err(ShellError::Vfs)?;
        }
        Command::Shutdown => {
            vfs.unmount_fs().map_err(ShellError::Vfs)?;
            return Ok(ShellOutcome::Shutdown);
        }
    }
    Ok(ShellOutcome::Continue)
}

const HELP: &[u8] = b"help\npwd\ncd PATH\nls [PATH]\ncat PATH\necho [TEXT ...]\ntouch PATH\nwrite PATH [TEXT ...]\nappend PATH [TEXT ...]\nmkdir PATH\nrm PATH\nrmdir PATH\nsync\nshutdown\n";

fn write_number(output: &mut impl TextOutput, value: u64) {
    let mut bytes = [0; 20];
    let mut value = value;
    let mut index = bytes.len();
    loop {
        index -= 1;
        bytes[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    output.write_bytes(&bytes[index..]);
}

fn require_one_argument(
    arguments: &mut alloc::vec::IntoIter<String>,
    command: &'static str,
) -> Result<String, ParseError> {
    let argument = arguments.next().ok_or(ParseError::Arity { command })?;
    require_no_arguments(arguments, command)?;
    Ok(argument)
}

fn optional_argument(
    arguments: &mut alloc::vec::IntoIter<String>,
    command: &'static str,
) -> Result<Option<String>, ParseError> {
    let argument = arguments.next();
    require_no_arguments(arguments, command)?;
    Ok(argument)
}

fn require_no_arguments(
    arguments: &mut alloc::vec::IntoIter<String>,
    command: &'static str,
) -> Result<(), ParseError> {
    if arguments.next().is_some() {
        Err(ParseError::Arity { command })
    } else {
        Ok(())
    }
}

fn collect_arguments(
    arguments: &mut alloc::vec::IntoIter<String>,
) -> Result<Vec<String>, ParseError> {
    let mut collected = Vec::new();
    for argument in arguments {
        collected
            .try_reserve(1)
            .map_err(|_| ParseError::Allocation)?;
        collected.push(argument);
    }
    Ok(collected)
}

fn require_argument_then_join(
    arguments: &mut alloc::vec::IntoIter<String>,
    command: &'static str,
) -> Result<(String, String), ParseError> {
    let path = arguments.next().ok_or(ParseError::Arity { command })?;
    let text = join_arguments(arguments)?;
    Ok((path, text))
}

fn join_arguments(arguments: &mut alloc::vec::IntoIter<String>) -> Result<String, ParseError> {
    let remaining = arguments.as_slice();
    let length =
        remaining.iter().map(String::len).sum::<usize>() + remaining.len().saturating_sub(1);
    let mut text = String::new();
    text.try_reserve_exact(length)
        .map_err(|_| ParseError::Allocation)?;
    for (index, argument) in arguments.enumerate() {
        if index != 0 {
            text.push(' ');
        }
        text.push_str(&argument);
    }
    Ok(text)
}
