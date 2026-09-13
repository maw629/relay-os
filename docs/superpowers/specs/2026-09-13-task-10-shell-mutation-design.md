# Task 10 Mutating Shell And Line Editor Design

## Status

Draft for review on 2026-09-13. Follows Approach A (Vfs owns path mutation logic, typed shutdown outcome, capacity-param line editor) approved in brainstorming.

## Objective

Task 10 completes the host-tested shell vertical slice above Task 9's durable ext2 mutation. It adds every documented mutating shell command (`touch`, `write`, `append`, `mkdir`, `rm`, `rmdir`, `sync`, `shutdown`) and a USB-independent line editor that later tasks feed with HID-derived semantic keys. Every successful mutation must persist across remount and remain readable and repairable by standard Linux ext2 tools. Any uncertain write continues to poison the mount per Task 9; the shell surfaces that as a stable error and blocks no read path.

Milestone One Task 10 (2026-09-07 plan, lines 920-996) is not specific enough for implementation: it omits where parent-split, trailing-slash, and cwd-ancestor checks live, omits the typed shutdown outcome shape, and contradicts itself on line-editor capacity (`new(128)` vs fixed 512-byte limit). This document resolves those gaps.

## Scope And Ruling

Task 10 adds exactly what shell dispatch needs and nothing else:

- Path-based mutation wrappers on `Vfs` (parent resolution, final-name validation, trailing-slash policy, cwd-ancestor protection).
- Full `Shell` dispatch for all eight mutating commands plus updated `help`, with a typed `ShellOutcome`.
- USB-independent `console::line_editor` consuming semantic `Key` values.

It adds no new `FileSystem`/`FsError` variants beyond Task 9, no ext2 allocation or ordering changes, no USB/xHCI/kernel runtime wiring (Tasks 13-15), no recursive create/remove, no pipes/redirection/globbing/expansion, and no timestamp management.

## Module Boundaries

```text
crates/relay-core/src/vfs/mod.rs            path mutation wrappers, parent split, ancestor guard
crates/relay-core/src/shell/commands.rs     full mutation dispatch, help update
crates/relay-core/src/shell/mod.rs          ShellOutcome, run() control flow
crates/relay-core/src/console/line_editor.rs Key, EditAction, LineEditor, redraw
crates/relay-core/src/console/mod.rs        re-export line_editor
crates/relay-core/src/lib.rs                expose console::line_editor
crates/relay-core/tests/shell_mutation.rs   end-to-end mutation integration tests
crates/relay-core/tests/line_editor.rs      editing unit tests
```

`relay-core` remains `#![no_std]` plus `alloc`. No unsafe code is required. All path lengths, name lengths, offsets, arithmetic, conversions, and allocations are checked; malformed input returns typed errors and never panics.

## Public API

```rust
pub enum ShellOutcome { Continue, Shutdown }

impl<F: FileSystem, O: TextOutput> Shell<F, O> {
    pub fn run(&mut self, line: &str) -> Result<ShellOutcome, ShellError>;
}

pub enum Key {
    Printable(u8),
    Backspace,
    Enter,
    Left,
    Right,
}

pub enum EditAction {
    Redraw,
    Submitted(alloc::string::String),
    Ignored,
}

impl LineEditor {
    pub fn new(capacity: usize) -> Self;
    pub fn handle_key(&mut self, key: Key) -> EditAction;
    pub fn line(&self) -> &str;
    pub fn take_submitted(&mut self) -> Option<alloc::string::String>;
}

pub fn redraw<O: TextOutput>(output: &mut O, prompt: &[u8], line: &str);
```

`Vfs<F: FileSystem>` gains exactly:

```rust
pub fn create_file(&mut self, cwd: &Cwd, path: &str) -> Result<NodeId, VfsError>;
pub fn create_dir(&mut self, cwd: &Cwd, path: &str) -> Result<NodeId, VfsError>;
pub fn write_file(&mut self, cwd: &Cwd, path: &str, text: &[u8]) -> Result<(), VfsError>;
pub fn append_file(&mut self, cwd: &Cwd, path: &str, text: &[u8]) -> Result<(), VfsError>;
pub fn unlink_file(&mut self, cwd: &Cwd, path: &str) -> Result<(), VfsError>;
pub fn remove_dir(&mut self, cwd: &Cwd, path: &str) -> Result<(), VfsError>;
pub fn sync_fs(&mut self) -> Result<(), VfsError>;
pub fn unmount_fs(&mut self) -> Result<(), VfsError>;
```

`VfsError` gains one variant:

```rust
pub enum VfsError {
    Fs(FsError),
    InvalidPath,
    NotDirectory,
    NotRegularFile,
    Allocation,
    Busy,
}
```

`Busy` covers current-directory and cwd-ancestor removal attempts. `ShellError` is unchanged; `Busy` maps to a stable `error: directory busy` line.

## VFS Mutation Semantics

All wrappers enforce the existing 4,096-byte path cap and ASCII-only rule before any filesystem access. Resolution is semantic and left-to-right, reusing `Vfs::resolve` for the parent portion so `missing/../file` behavior stays consistent with reads.

Parent split procedure for every path-taking mutation:

1. Reject paths longer than 4,096 bytes or containing non-ASCII with `InvalidPath`.
2. For file ops (`create_file`, `write_file`, `append_file`, `unlink_file`) a trailing `/` returns `NotDirectory` without I/O, matching read trailing-slash policy. For `create_dir` and `remove_dir`, strip all trailing `/` bytes first (so `/a/b/` and `/a/b//` both target `b`); if stripping leaves an empty string (input was `/` or `//`), return `InvalidPath`.
3. Split at the last `/`. The portion before it is the parent path: empty means `/` when the original path started with `/`, else it means the current `cwd`. The portion after it is the final name. An empty final name (path was empty after stripping) returns `InvalidPath`.
4. Resolve the parent through existing `resolve` (absolute from root, relative from `cwd`), requiring a directory, else propagate `NotDirectory`/`NotFound`/`InvalidPath`.
5. Convert the final name through `Name::new`; map `Empty`/`TooLong`/`InvalidByte` to `InvalidPath`. This rejects `.`, `..`, `/`, NUL, non-printable/non-ASCII, and names over 255 bytes.
6. Call the corresponding `FileSystem` method with the resolved parent node and validated `Name`.

Per-operation rules:

- `create_file`: `lookup` miss then `FileSystem::create_file`. Duplicate returns `Fs(AlreadyExists)`. Ext2 assigns mode `0644`, root ownership, zeroed times per Task 9.
- `create_dir`: same with `create_dir`. Duplicate returns `AlreadyExists`. Ext2 assigns mode `0755`, `.`/`..`, link counts, `used_dirs` per Task 9. Parent must already exist; no recursive creation.
- `write_file`: resolve-or-create then `truncate(node, 0)` then `write_at(node, 0, text)`. Existing directory returns `NotRegularFile` without mutation. Empty `text` truncates to zero length. No trailing newline is added. Enforces the 4,243,456-byte maximum via `FileTooLarge` from ext2.
- `append_file`: resolve-or-create then read `metadata(node).len` then `write_at(node, len, text)`. Same type and size rules. Empty `text` is a success no-op (creates the file if absent).
- `unlink_file`: resolve parent plus child, require a regular child else `NotRegularFile` (directory child) or `Fs(NotFound)`. Delegates to `FileSystem::unlink_file`.
- `remove_dir`: resolve parent plus child, require a directory child else `NotDirectory`. Reject inode-2 root via ext2 `WrongNodeKind` mapping. Verify emptiness in ext2 (`DirectoryNotEmpty` to `Fs(NotEmpty)`). Then enforce the ancestor guard: if the resolved target canonical components equal `cwd` components or are a strict prefix of them, return `Busy` without mutation. This prevents removing the shell's current directory or any ancestor that would strand `Cwd`.
- `sync_fs` / `unmount_fs`: thin wrappers over `FileSystem::sync` / `unmount`, mapping errors unchanged. `unmount` success is the only path that later permits `ShellOutcome::Shutdown`.

All `FsError` mappings reuse Task 8/9 categories: `FileTooLarge`, `WriteDisabled` (poisoned), `ReadOnly`, `AlreadyExists`, `NotEmpty`, `NoSpace`, `Io`, `Corrupt`, `Unsupported`, `Allocation`. A poisoned mount surfaces as `write disabled` for every later mutation while reads continue.

## Shell Dispatch

`parse_command` is unchanged from Task 8; all milestone variants already parse with exact arity. `write`/`append` join zero or more trailing text tokens with one ASCII space, producing empty text when none are present.

`execute` replaces `execute_read_command` with full dispatch. Read commands keep exact Task 8 output. Mutating commands:

- `touch PATH`: `vfs.create_file(cwd, path)` on miss; on `AlreadyExists`, `metadata` the existing node and succeed silently if regular (leave contents, size, and times unchanged), else `NotRegularFile`. No output on success.
- `write PATH [TEXT ...]`: `vfs.write_file(cwd, path, text.as_bytes())`. No output on success, no added newline.
- `append PATH [TEXT ...]`: `vfs.append_file(cwd, path, text.as_bytes())`. Same output rule.
- `mkdir PATH`: `vfs.create_dir(cwd, path)`. No output on success.
- `rm PATH`: `vfs.unlink_file(cwd, path)`. No output on success.
- `rmdir PATH`: `vfs.remove_dir(cwd, path)`. No output on success.
- `sync`: `vfs.sync_fs()`. No output on success.
- `shutdown`: `vfs.unmount_fs()`; on `Ok` return `Ok(Shutdown)` with no output; on `Err` write one error line and return `Ok(Continue)`. The kernel must halt only on `Shutdown`, never on a failed unmount.

`run` writes exactly one `error: <stable message>\n` line for handled parse/VFS failures and returns `Ok(Continue)`. It returns `Ok(Shutdown)` only for successful shutdown. It returns `Err(ShellError::Allocation)` only if error-line rendering itself cannot allocate. `Cwd` is never partially updated; a failed `cd` or mutation leaves it unchanged.

Stable messages reuse Task 8 text plus the one new mapping: `Busy` to `directory busy`. Existing `FileTooLarge` to `file too large`, `WriteDisabled` to `write disabled`, `ReadOnly` to `read-only filesystem`, `AlreadyExists` to `already exists`, `NotEmpty` to `directory not empty`, `NoSpace` to `no space left` are already implemented in `shell/mod.rs` and stay unchanged.

`help` is updated to the full available list with no `unavailable` markers:

```text
help
pwd
cd PATH
ls [PATH]
cat PATH
echo [TEXT ...]
touch PATH
write PATH [TEXT ...]
append PATH [TEXT ...]
mkdir PATH
rm PATH
rmdir PATH
sync
shutdown
```

Each line is the literal usage string followed by LF, in this order, preserving Task 8 help order with mutation rows now available (the milestone contract does not mandate help order).

## Line Editing

`LineEditor` is USB-, framebuffer-, and serial-independent. It stores an ASCII byte `Vec<u8>` plus a byte cursor index and a `capacity` in bytes, plus an optional pending submission. Production shell wiring uses capacity 512 to match the parser `MAX_INPUT_BYTES`; tests may use smaller capacities to cover limit edges.

- `Printable(b)`: if `b` is not in `0x20..=0x7E`, return `Ignored`. If `line.len() >= capacity`, return `Ignored`. Else insert at cursor, advance cursor by one, return `Redraw`.
- `Backspace`: if cursor is zero, `Ignored`. Else remove the byte before the cursor, move cursor back by one, `Redraw`.
- `Left`: if cursor is zero, `Ignored`, else decrement and `Redraw`.
- `Right`: if cursor equals length, `Ignored`, else increment and `Redraw`.
- `Enter`: move the current line bytes into a pending submission (as `String`, always valid UTF-8 because only printable ASCII is ever stored), clear the buffer, reset cursor to zero, return `Submitted(text)`. An empty buffer submits `""`; the shell treats it as `Ok(Continue)` no-op, matching empty-line parsing.

Cursor arithmetic is always on ASCII byte boundaries because only single-byte printable characters are storable; no multi-byte handling is needed. All growth uses fallible `try_reserve` and maps failure to `Ignored` without panic; the shell input cap makes host OOM unreachable in practice.

`line()` returns the current buffer as `&str`. `take_submitted()` takes the pending submission if any. `redraw(output, prompt, line)` writes `prompt` bytes then `line` bytes through `TextOutput` with no newline and no geometry knowledge; the kernel framebuffer adapter decides cursor glyphs and wrapping. The editor never touches USB scan codes, HID reports, or framebuffer pixels.

## Testing

`shell_mutation` uses Task 7/9 file-backed ext2 fixtures mounted `ReadWrite` through `Vfs`, plus the `Recorder` `TextOutput` from `shell_read`:

- persist-after-reopen: `mkdir /docs`, `write /docs/hello 'relay os'`, `append /docs/hello '!'`, `sync`, reopen read-only, assert `b"relay os!"`; unmount then `e2fsck -fn` exit 0 on a disposable copy.
- per-command success output: each mutating success writes no bytes; `ls`/`cat`/`pwd` confirm effects with exact bytes.
- `touch` leaves existing regular contents unchanged; `touch` on a directory errors `not a regular file`.
- `write` truncates before writing; `append` starts at prior length; both create absent files, add no newline, handle empty text.
- trailing-slash file ops error `not a directory`; `mkdir` with trailing slashes succeeds after stripping.
- `rm` on a directory errors; `rmdir` on a file errors; `rmdir /` errors `invalid path`; `rmdir` non-empty errors `directory not empty`.
- cwd-ancestor guard: `mkdir /a`, `mkdir /a/b`, `cd /a/b`, `rmdir /a` and `rmdir /a/b` both yield `error: directory busy` and `pwd` still prints `/a/b`.
- max-file boundary through shell: writing 4,243,456 bytes succeeds, 4,243,457 yields `file too large` (via a generated buffer, not committed as a large fixture).
- poisoned mount: fault-injected `WriteFailed` fixture yields `write disabled` for later `write`/`mkdir`/`sync` while `ls`/`cat` still succeed.
- `sync` flushes; `shutdown` returns `Shutdown` only after clean unmount and second mount requires clean state; failed `shutdown` returns `Continue` plus one error line.
- `help` exact bytes match the thirteen-line list above.

`line_editor` covers insert-at-end, middle insert via `Left`, cursor clamp at both ends, backspace at zero ignored, delete-before-cursor, capacity enforcement (fills to capacity then `Ignored`), non-printable `Printable(0x01)` ignored, `Enter` submits and clears, `take_submitted` drains exactly once, and `redraw` writes prompt plus line bytes.

Verification runs:

```bash
cargo fmt --all --check
cargo test -p relay-core --test shell_mutation --test line_editor --locked
cargo test -p relay-core --test vfs --test shell_parser --test shell_read --locked
LC_ALL=C e2fsck -fn target/root.ext2
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Non-Goals

Task 10 adds no recursive `mkdir -p` or `rm -r`, no timestamp or ownership management beyond Task 9's fixed modes, no symbolic or hard links, no sparse files, no pipes/redirection/globbing/expansion/environment variables, no executable loading, no in-OS repair, and no USB, xHCI, framebuffer-runtime, or kernel-harness integration. Keyboard-to-`Key` mapping belongs to Task 13; block-device and persistence wiring belongs to Tasks 14-15.
