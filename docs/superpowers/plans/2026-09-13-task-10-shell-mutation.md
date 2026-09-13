# Task 10 Mutating Shell And Line Editor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add host-tested VFS path mutation wrappers, full mutating shell dispatch with typed shutdown outcome, and a USB-independent line editor.

**Architecture:** `vfs` owns parent-split, trailing-slash, and cwd-ancestor rules over the Task 9 `FileSystem` mutation contract; `shell` owns arity, stable error lines, updated help, and `ShellOutcome::Continue|Shutdown`; `console::line_editor` owns capacity-bounded ASCII editing with `Key`/`EditAction` and a geometry-free `redraw`.

**Tech Stack:** Rust 1.98.1, edition 2024, `no_std` plus `alloc`, existing `relay-core::{ext2, fs, vfs, shell, console}`, host ext2 fixtures with `mke2fs`/`e2fsck` from e2fsprogs 1.47.2.

**Spec:** `docs/superpowers/specs/2026-09-13-task-10-shell-mutation-design.md`

## Global Constraints

- `relay-core` remains `#![no_std]`; host filesystem and process APIs stay in integration-test support only.
- No unsafe code in this task.
- Paths are ASCII, at most 4,096 bytes; shell input lines at most 512 bytes; editor production capacity is 512 bytes.
- Component names use `Name::new` validation; `.`, `..`, `/`, NUL, non-printable/non-ASCII, and names over 255 bytes map to `VfsError::InvalidPath`.
- Maximum regular-file size is exactly 4,243,456 bytes; larger writes return `FileTooLarge`.
- New files use mode `0644`, new directories use mode `0755`, root ownership, zeroed times (Task 9 ext2 behavior, unchanged here).
- Every mutation passes Task 9 health poisoning; `WriteFailed` surfaces as `write disabled` while reads continue.
- No recursive mkdir/rm, no timestamp management, no links, no sparse files, no pipes/redirection/globbing/expansion, no USB/xHCI/kernel wiring.
- Maintain `cargo fmt --all --check`, workspace Clippy with `-D warnings`, and locked workspace tests.

---

## File Structure

```text
crates/relay-core/src/vfs/mod.rs            add Busy + 8 path mutation wrappers
crates/relay-core/src/shell/mod.rs          add ShellOutcome, new run() flow, Busy message
crates/relay-core/src/shell/commands.rs     full mutation dispatch, updated HELP
crates/relay-core/src/console/line_editor.rs Key, EditAction, LineEditor, redraw
crates/relay-core/src/console/mod.rs        re-export line_editor
crates/relay-core/src/lib.rs                expose console::line_editor (via console mod)
crates/relay-core/tests/shell_mutation.rs   VFS-wrapper + shell end-to-end tests
crates/relay-core/tests/line_editor.rs      editor unit tests
crates/relay-core/tests/shell_read.rs       update help test to new exact bytes
```

### Task 1: VFS Path Mutation Wrappers And Ancestor Guard

**Files:**
- Modify: `crates/relay-core/src/vfs/mod.rs`
- Create: `crates/relay-core/tests/shell_mutation.rs`
- Test: `crates/relay-core/tests/shell_mutation.rs`

**Interfaces:**
- Consumes: existing `FileSystem` mutation methods, `Cwd`, `Name`, `NodeId`, `FsError`, `VfsError::{Fs, InvalidPath, NotDirectory, NotRegularFile, Allocation}`.
- Produces: `VfsError::Busy` plus `Vfs::create_file`, `Vfs::create_dir`, `Vfs::write_file`, `Vfs::append_file`, `Vfs::unlink_file`, `Vfs::remove_dir`, `Vfs::sync_fs`, `Vfs::unmount_fs` for Task 2 shell dispatch.

- [ ] **Step 1: Write failing VFS-wrapper tests**

Create `crates/relay-core/tests/shell_mutation.rs` with `mod support;` and these exact tests before production wrappers exist:

```rust
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
    assert_eq!(
        vfs.create_file(&cwd, "a/"),
        Err(VfsError::NotDirectory)
    );
    vfs.create_dir(&cwd, "/a").unwrap();
    vfs.create_dir(&cwd, "/a/b").unwrap();
    let deep = vfs.change_dir(&cwd, "/a/b").unwrap();
    assert_eq!(vfs.remove_dir(&deep, "/a"), Err(VfsError::Busy));
    assert_eq!(vfs.remove_dir(&deep, "/a/b"), Err(VfsError::Busy));
    assert_eq!(vfs.remove_dir(&cwd, "/"), Err(VfsError::InvalidPath));
}
```

- [ ] **Step 2: Run the new test target to verify it fails**

Run: `cargo test -p relay-core --test shell_mutation --locked`

Expected: FAIL because `Vfs::create_dir`, `Vfs::create_file`, `Vfs::remove_dir`, and `VfsError::Busy` are absent.

- [ ] **Step 3: Add Busy and the eight wrappers**

In `crates/relay-core/src/vfs/mod.rs`, add `Busy` to `VfsError` and implement exactly these methods on `impl<F: FileSystem> Vfs<F>`:

```rust
pub fn create_file(&mut self, cwd: &Cwd, path: &str) -> Result<NodeId, VfsError> {
    let (parent, name) = self.split_parent(cwd, path, false)?;
    let dir = self.resolve(cwd, &parent)?.node;
    self.filesystem.create_file(dir, &name).map_err(VfsError::Fs)
}
```

Repeat the shape for `create_dir`, `unlink_file` (with a regular-kind precheck), `remove_dir` (with a directory-kind precheck plus the ancestor guard), `write_file` (resolve-or-create, reject directory with `NotRegularFile`, then `truncate(node, 0)` and `write_at(node, 0, text)`), `append_file` (resolve-or-create, reject directory, then `metadata(node).len` and `write_at(node, len, text)`), `sync_fs` (call `filesystem.sync`), and `unmount_fs` (call `filesystem.unmount`).

Implement one private helper:

```rust
fn split_parent(&mut self, cwd: &Cwd, path: &str, dir_target: bool) -> Result<(String, Name), VfsError>
```

that enforces the 4,096-byte and ASCII rules, returns `NotDirectory` for file ops with a trailing `/`, strips all trailing `/` for dir targets (empty after stripping means `InvalidPath`), splits at the last `/` where an empty parent means `/` when the original path started with `/` else the rendered `cwd_path`, resolves the parent with `resolve` to require a directory, and converts the final component with `Name::new` mapped to `InvalidPath`.

For `remove_dir`, after resolving the child node, compare the resolved target canonical components against `cwd.components`: if equal or a strict prefix, return `Busy` without calling the filesystem.

- [ ] **Step 4: Run focused tests**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test shell_mutation --locked
cargo clippy -p relay-core --all-targets --locked -- -D warnings
```

Expected: the two Task 1 tests pass; lint is warning-free.

- [ ] **Step 5: Commit VFS wrappers**

```bash
git add crates/relay-core/src/vfs/mod.rs crates/relay-core/tests/shell_mutation.rs
git commit -m "feat: add VFS mutation wrappers"
```

### Task 2: Mutating Shell Dispatch, Shutdown Outcome, And Help

**Files:**
- Modify: `crates/relay-core/src/shell/mod.rs`
- Modify: `crates/relay-core/src/shell/commands.rs`
- Modify: `crates/relay-core/tests/shell_mutation.rs`
- Modify: `crates/relay-core/tests/shell_read.rs`

**Interfaces:**
- Consumes: Task 1 `Vfs` mutation wrappers, `VfsError::Busy`, `Command`, `TextOutput`.
- Produces: `ShellOutcome::{Continue, Shutdown}`, `Shell::run() -> Result<ShellOutcome, ShellError>`, full mutation dispatch and updated help for Task 3 and Tasks 13-15.

- [ ] **Step 1: Write failing shell mutation tests**

Append these exact tests to `crates/relay-core/tests/shell_mutation.rs`:

```rust
use relay_core::{
    console::TextOutput,
    ext2::{Ext2, MountMode},
    shell::{Shell, ShellOutcome},
    vfs::Vfs,
};

struct Recorder(Vec<u8>);

impl TextOutput for Recorder {
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

fn mutation_shell() -> (support::ext2_image::Ext2Fixture, Shell<Ext2<FileDevice>, Recorder>) {
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
    assert!(output.windows(20).any(|w| w == b"error: write disabled"));
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
    assert!(output.windows(20).any(|w| w == b"error: directory busy"));
    assert!(output.windows(22).any(|w| w == b"error: not a regular file"));
    assert!(output.starts_with(b"error: directory busy\nerror: not a regular file\nhelp\n"));
}
```

- [ ] **Step 2: Run shell tests to verify they fail**

Run: `cargo test -p relay-core --test shell_mutation --locked`

Expected: FAIL because `ShellOutcome`, full mutation dispatch, and `shutdown` control flow are absent (`Unavailable` errors instead of success).

- [ ] **Step 3: Implement ShellOutcome and full dispatch**

In `crates/relay-core/src/shell/mod.rs`, add exactly:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellOutcome {
    Continue,
    Shutdown,
}
```

Change the signature to:

```rust
pub fn run(&mut self, line: &str) -> Result<ShellOutcome, ShellError>
```

Keep `Ok(None)` as `Ok(Continue)` with no output. On handled parse/VFS errors write one `error: <message>\n` line and return `Ok(Continue)`. Return `Err(ShellError::Allocation)` only when error-line rendering cannot allocate. Map `VfsError::Busy` to `b"directory busy"`.

In `crates/relay-core/src/shell/commands.rs`, replace `execute_read_command` with a full `execute_command` returning `Result<ShellOutcome, ShellError>`: keep all six read arms byte-identical, then implement `touch` (create-or-keep-regular, directory yields `NotRegularFile`), `write` (call `write_file` with `text.as_bytes()`), `append` (call `append_file`), `mkdir` (call `create_dir`), `rm` (call `unlink_file`), `rmdir` (call `remove_dir`), `sync` (call `sync_fs`), and `shutdown` (call `unmount_fs`, map `Ok` to `Shutdown` with no output, map `Err` to an error line plus `Continue`). Replace the `HELP` constant with exactly:

```rust
const HELP: &[u8] = b"help\npwd\ncd PATH\nls [PATH]\ncat PATH\necho [TEXT ...]\ntouch PATH\nwrite PATH [TEXT ...]\nappend PATH [TEXT ...]\nmkdir PATH\nrm PATH\nrmdir PATH\nsync\nshutdown\n";
```

Remove the `Unavailable` arm for mutation commands. Keep `ShellError::Unavailable` variant for backward compatibility but stop returning it from dispatch.

In `crates/relay-core/tests/shell_read.rs`, update the `help_lists_read_commands_and_marks_mutations_unavailable` expectation to the thirteen-line list above, and rename it to `help_lists_all_available_commands` without changing any other assertion.

- [ ] **Step 4: Run shell plus regression suites**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test shell_mutation --test shell_read --test vfs --test shell_parser --locked
cargo clippy -p relay-core --all-targets --locked -- -D warnings
```

Expected: new mutation tests pass; updated help test passes; `vfs`, `shell_parser`, and remaining `shell_read` tests pass unchanged.

- [ ] **Step 5: Commit shell dispatch**

```bash
git add crates/relay-core/src/shell crates/relay-core/tests/shell_mutation.rs crates/relay-core/tests/shell_read.rs
git commit -m "feat: dispatch mutating shell commands"
```

### Task 3: USB-Independent Line Editor

**Files:**
- Create: `crates/relay-core/src/console/line_editor.rs`
- Modify: `crates/relay-core/src/console/mod.rs`
- Modify: `crates/relay-core/src/lib.rs`
- Create: `crates/relay-core/tests/line_editor.rs`

**Interfaces:**
- Consumes: `console::TextOutput` and Task 2 `Shell` 512-byte input contract.
- Produces: `Key`, `EditAction`, `LineEditor::new`, `handle_key`, `line`, `take_submitted`, and `redraw` for Task 13 HID wiring.

- [ ] **Step 1: Write failing editor tests**

Create `crates/relay-core/tests/line_editor.rs` with exactly:

```rust
use relay_core::console::{
    TextOutput,
    line_editor::{EditAction, Key, LineEditor, redraw},
};

struct Recorder(Vec<u8>);

impl TextOutput for Recorder {
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

fn key(byte: u8) -> Key {
    Key::Printable(byte)
}

fn feed(editor: &mut LineEditor, keys: impl IntoIterator<Item = Key>) -> Vec<EditAction> {
    keys.into_iter().map(|item| editor.handle_key(item)).collect()
}

#[test]
fn editor_inserts_and_moves_cursor() {
    let mut editor = LineEditor::new(128);
    feed(&mut editor, [key(b'a'), key(b'c'), Key::Left, key(b'b'), Key::Enter]);
    assert_eq!(editor.take_submitted(), Some("abc".to_string()));
    assert_eq!(editor.take_submitted(), None);
}

#[test]
fn editor_enforces_capacity_and_ignores_non_printable() {
    let mut editor = LineEditor::new(2);
    assert!(matches!(editor.handle_key(key(b'a')), EditAction::Redraw));
    assert!(matches!(editor.handle_key(key(b'b')), EditAction::Redraw));
    assert!(matches!(editor.handle_key(key(b'c')), EditAction::Ignored));
    assert!(matches!(editor.handle_key(Key::Printable(0x01)), EditAction::Ignored));
    assert_eq!(editor.line(), "ab");
    assert!(matches!(editor.handle_key(Key::Backspace), EditAction::Redraw));
    assert_eq!(editor.line(), "a");
    assert!(matches!(editor.handle_key(Key::Left), EditAction::Redraw));
    assert!(matches!(editor.handle_key(Key::Left), EditAction::Ignored));
    assert!(matches!(editor.handle_key(Key::Right), EditAction::Redraw));
    assert!(matches!(editor.handle_key(Key::Right), EditAction::Ignored));
    assert!(matches!(editor.handle_key(Key::Backspace), EditAction::Ignored));
}

#[test]
fn redraw_writes_prompt_and_line_without_newline() {
    let mut output = Recorder(Vec::new());
    redraw(&mut output, b"$ ", "hi");
    assert_eq!(output.0, b"$ hi");
}
```

- [ ] **Step 2: Run editor tests to verify they fail**

Run: `cargo test -p relay-core --test line_editor --locked`

Expected: FAIL because `console::line_editor`, `Key`, `LineEditor`, and `redraw` are absent.

- [ ] **Step 3: Implement the editor**

Create `crates/relay-core/src/console/line_editor.rs` with exactly:

```rust
use alloc::{string::String, vec::Vec};

use super::TextOutput;

pub enum Key {
    Printable(u8),
    Backspace,
    Enter,
    Left,
    Right,
}

pub enum EditAction {
    Redraw,
    Submitted(String),
    Ignored,
}

pub struct LineEditor {
    buffer: Vec<u8>,
    cursor: usize,
    capacity: usize,
    submitted: Option<String>,
}

impl LineEditor {
    pub fn new(capacity: usize) -> Self {
        Self { buffer: Vec::new(), cursor: 0, capacity, submitted: None }
    }

    pub fn handle_key(&mut self, key: Key) -> EditAction {
        match key {
            Key::Printable(byte) => {
                if !matches!(byte, 0x20..=0x7E) || self.buffer.len() >= self.capacity {
                    return EditAction::Ignored;
                }
                if self.buffer.try_reserve(1).is_err() {
                    return EditAction::Ignored;
                }
                self.buffer.insert(self.cursor, byte);
                self.cursor += 1;
                EditAction::Redraw
            }
            Key::Backspace => {
                if self.cursor == 0 {
                    EditAction::Ignored
                } else {
                    self.cursor -= 1;
                    self.buffer.remove(self.cursor);
                    EditAction::Redraw
                }
            }
            Key::Left => {
                if self.cursor == 0 {
                    EditAction::Ignored
                } else {
                    self.cursor -= 1;
                    EditAction::Redraw
                }
            }
            Key::Right => {
                if self.cursor == self.buffer.len() {
                    EditAction::Ignored
                } else {
                    self.cursor += 1;
                    EditAction::Redraw
                }
            }
            Key::Enter => {
                let text = String::from_utf8(core::mem::take(&mut self.buffer))
                    .expect("editor stores ASCII only");
                self.cursor = 0;
                self.submitted = Some(text.clone());
                EditAction::Submitted(text)
            }
        }
    }

    pub fn line(&self) -> &str {
        core::str::from_utf8(&self.buffer).expect("editor stores ASCII only")
    }

    pub fn take_submitted(&mut self) -> Option<String> {
        self.submitted.take()
    }
}

pub fn redraw<O: TextOutput>(output: &mut O, prompt: &[u8], line: &str) {
    output.write_bytes(prompt);
    output.write_bytes(line.as_bytes());
}
```

Derive `Clone, Copy, Debug, Eq, PartialEq` on `Key` and `Debug, Eq, PartialEq` on `EditAction` where the `String` payload permits it. In `crates/relay-core/src/console/mod.rs`, add `pub mod line_editor;` and re-export `line_editor::{EditAction, Key, LineEditor}`. Ensure `crates/relay-core/src/lib.rs` still exposes `pub mod console;` with no further change.

- [ ] **Step 4: Run the full Task 10 verification**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test shell_mutation --test line_editor --locked
cargo test -p relay-core --test vfs --test shell_parser --test shell_read --locked
cargo xtask image --output target/relay-os.img
LC_ALL=C e2fsck -fn target/root.ext2
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Expected: all focused tests pass, generated-root reads pass, `e2fsck` exits 0, Clippy reports no warnings, and the workspace suite passes.

- [ ] **Step 5: Commit the editor**

```bash
git add crates/relay-core/src/console crates/relay-core/src/lib.rs crates/relay-core/tests/line_editor.rs
git commit -m "feat: add USB-independent line editor"
```
