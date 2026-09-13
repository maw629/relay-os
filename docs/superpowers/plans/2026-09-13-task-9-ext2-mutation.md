# Task 9 Ext2 Mutation, Ordering, And Failure Poisoning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add durable host-tested ext2 file and directory mutation with strict write ordering and failure poisoning.

**Architecture:** `ext2::allocator` owns single-group bitmap claims and joint superblock plus group free-count updates; `ext2::mutation` implements ordered file and directory changes over exact eight-sector block I/O; `ext2::mod` owns `MountHealth`, the one health guard, mount clearing of `EXT2_VALID_FS`, `write_block`, `sync`, and `unmount`. `vfs/mod.rs` is extended minimally with mutation `FileSystem` methods and new `FsError` variants only.

**Tech Stack:** Rust 1.98.1, edition 2024, `no_std` plus `alloc`, existing `relay-core::{block, fs, vfs}`, host test support using `std`, `mke2fs`, `debugfs`, and `e2fsck` from e2fsprogs 1.47.2.

**Spec:** `docs/superpowers/specs/2026-09-13-task-9-ext2-mutation-design.md`

## Global Constraints

- `relay-core` remains `#![no_std]`; host filesystem and process APIs stay in integration-test support only.
- Support exactly 512-byte logical sectors and 4 KiB ext2 blocks as exact eight-sector transfers.
- The accepted profile is revision 1, 32,768 blocks, 32,768 blocks per group, one group, 4,096 inodes, 256-byte inodes, `s_first_data_block == 0`, exact incompatible `FILETYPE`, all compatible and read-only-compatible bits clear.
- Maximum regular-file size is exactly 4,243,456 bytes (twelve direct plus 1,024 singly-indirect 4 KiB blocks).
- All media lengths, offsets, block numbers, inode numbers, arithmetic, conversions, and allocations are checked; malformed media returns typed errors and never panics.
- Preserve all 256 inode bytes except explicitly updated fields; new inodes use zeroed times, uid and gid zero.
- Every mutation passes one health guard; any uncertain write or flush sets `WriteFailed` with no rollback.
- Read-write mount clears `EXT2_VALID_FS` and flushes before returning `Writable`; a clear or flush failure returns `Err` without an instance.
- No `Vfs` path-based mutation wrappers, shell dispatch, links, special files, sparse holes, in-OS repair, or USB and kernel integration belongs in this task.
- Maintain `cargo fmt --all --check`, workspace Clippy with `-D warnings`, and locked workspace tests.

---

## File Structure

```text
crates/relay-core/src/ext2/mod.rs            MountHealth, health guard, write_block, mutation methods, sync/unmount
crates/relay-core/src/ext2/allocator.rs      bitmap scan, claim, release, joint free-count updates
crates/relay-core/src/ext2/mutation.rs       ordered file and directory mutation
crates/relay-core/src/ext2/on_disk.rs        new superblock, group, and inode field offsets
crates/relay-core/src/vfs/mod.rs             extend FileSystem trait and FsError, implement for Ext2
crates/relay-core/tests/support/mod.rs       expose fault_device and power_cut_device
crates/relay-core/tests/support/fault_device.rs
                                             deterministic write and flush fault injection
crates/relay-core/tests/support/power_cut_device.rs
                                             unflushed-overlay discard
crates/relay-core/tests/ext2_files.rs        file mutation integration tests
crates/relay-core/tests/ext2_directories.rs  directory mutation integration tests
crates/relay-core/tests/ext2_faults.rs       fault-index and power-cut sweeps
```

### Task 1: Contract, Health, Mount Clearing, And Fault Support

**Files:**
- Modify: `crates/relay-core/src/ext2/on_disk.rs`
- Modify: `crates/relay-core/src/ext2/mod.rs`
- Modify: `crates/relay-core/src/vfs/mod.rs`
- Modify: `crates/relay-core/tests/support/mod.rs`
- Create: `crates/relay-core/tests/support/fault_device.rs`
- Create: `crates/relay-core/tests/support/power_cut_device.rs`
- Create: `crates/relay-core/tests/ext2_files.rs`

**Interfaces:**
- Consumes: existing `BlockDevice`, `NodeId`, `Name`, `Metadata`, `Ext2::mount`, `Ext2::read_at`.
- Produces: `MountHealth`, `Ext2::health`, `Ext2::sync`, `Ext2::unmount`, `write_block`, extended `Ext2Error`, extended `FsError`, extended `FileSystem`, `FaultDevice`, `PowerCutDevice` for Tasks 2 through 4.

- [ ] **Step 1: Write failing health and poison contract tests**

Create `tests/ext2_files.rs` with `mod support;` and these exact tests before production APIs exist:

```rust
mod support;

use relay_core::{
    ext2::{Ext2, Ext2Error, MountHealth, MountMode},
    fs::Name,
};
use support::{ext2_image::fixture_with_files, fault_device::FaultDevice};

fn name(bytes: &[u8]) -> Name {
    Name::new(bytes).unwrap()
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
```

- [ ] **Step 2: Run the new test target to verify it fails**

Run: `cargo test -p relay-core --test ext2_files --locked`

Expected: FAIL because `MountHealth`, `health`, `create_file`, `FaultDevice`, and new `Ext2Error` variants are absent.

- [ ] **Step 3: Add on-disk offsets for mutation**

In `src/ext2/on_disk.rs`, add these exact constants beside the existing map (`SUPERBLOCK_STATE` already exists at line 16 and is reused, do not duplicate it):

```rust
pub const SUPERBLOCK_FREE_BLOCKS: usize = 12;
pub const SUPERBLOCK_FREE_INODES: usize = 16;
pub const GROUP_DESCRIPTOR_FREE_BLOCKS: usize = 12;
pub const GROUP_DESCRIPTOR_FREE_INODES: usize = 14;
pub const GROUP_DESCRIPTOR_USED_DIRS: usize = 16;
pub const INODE_UID: usize = 2;
pub const INODE_ATIME: usize = 8;
pub const INODE_CTIME: usize = 12;
pub const INODE_MTIME: usize = 16;
pub const INODE_DTIME: usize = 20;
pub const INODE_GID: usize = 24;
pub const INODE_LINKS: usize = 26;
pub const INODE_BLOCKS: usize = 28;
```

Keep existing `SUPERBLOCK_STATE`, `INODE_MODE`, `INODE_SIZE_LO`, `INODE_FLAGS`, `INODE_BLOCK`, and `INODE_SIZE_HIGH`. Add `u16`/`u32` read helpers reuse only; add two small write helpers in `mutation.rs`, not here.

- [ ] **Step 4: Extend errors, health, and the filesystem contract**

In `src/ext2/mod.rs`, extend the error enum with exactly:

```rust
FileTooLarge,
NoSpace,
WriteDisabled,
ReadOnly,
AlreadyExists,
DirectoryNotEmpty,
```

Add exactly:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountHealth {
    ReadOnly,
    Writable,
    WriteFailed,
    Unmounted,
}
```

Store `health: MountHealth` in `Ext2<D>`. Set `ReadOnly` for `MountMode::ReadOnly` mounts and proceed to Step 5 for `ReadWrite`. Add:

```rust
pub fn health(&self) -> MountHealth;
fn require_writable(&self) -> Result<(), Ext2Error>;
fn poison(&mut self);
fn write_block(&mut self, block: u32, bytes: &[u8; BLOCK_BYTES]) -> Result<(), Ext2Error>;
pub fn sync(&mut self) -> Result<(), Ext2Error>;
pub fn unmount(&mut self) -> Result<(), Ext2Error>;
```

`require_writable` returns `Ok` for `Writable`, `Err(ReadOnly)` for `ReadOnly`, and `Err(WriteDisabled)` for `WriteFailed` or `Unmounted`. `poison` sets `WriteFailed`. `write_block` rejects `block >= 32_768` with `CorruptMetadata { field: "block_pointer" }`, converts to `first_lba = block * 8` checked, calls `write_sectors`, maps errors to `Block`, and poisons on failure. `sync` requires writable, then calls `device.flush`, poisons on failure. Reads (`metadata`, `lookup`, `read_dir`, `read_at`) return `WriteDisabled` when health is `Unmounted`, else existing behavior.

In `src/vfs/mod.rs`, extend `FsError` with exactly `FileTooLarge`, `WriteDisabled`, `ReadOnly`, `AlreadyExists`, `NotEmpty`, and `NoSpace`. Extend `FileSystem` with exactly:

```rust
fn write_at(&mut self, node: NodeId, offset: u64, src: &[u8]) -> Result<(), FsError>;
fn truncate(&mut self, node: NodeId, len: u64) -> Result<(), FsError>;
fn create_file(&mut self, parent: NodeId, name: &Name) -> Result<NodeId, FsError>;
fn create_dir(&mut self, parent: NodeId, name: &Name) -> Result<NodeId, FsError>;
fn unlink_file(&mut self, parent: NodeId, name: &Name) -> Result<(), FsError>;
fn remove_dir(&mut self, parent: NodeId, name: &Name) -> Result<(), FsError>;
fn sync(&mut self) -> Result<(), FsError>;
fn unmount(&mut self) -> Result<(), FsError>;
```

Map `Ext2Error::FileTooLarge` to `FsError::FileTooLarge`, `WriteDisabled` to `WriteDisabled`, `ReadOnly` to `ReadOnly`, `AlreadyExists` to `AlreadyExists`, `DirectoryNotEmpty` to `NotEmpty`, `NoSpace` to `NoSpace`, `Block` to `Io`, preserving the existing read mapping. Declare the eight inherent mutation methods on `Ext2<D>` now with `require_writable` plus `todo!`-free stub bodies returning `Err(Ext2Error::UnsupportedFile)` so Task 1 compiles; Tasks 2 and 3 replace each stub. Do not use `todo!` or `unimplemented!`.

- [ ] **Step 5: Clear dirty state on read-write mount**

In `Ext2::mount`, after existing `validate::mount` and clean check, if mode is `ReadWrite`, read superblock bytes at byte offset 1,024 through exact sectors, set u16 at superblock offset 58 to zero with checked ranges, write the affected sectors back, call `device.flush`, and only then return health `Writable`. Any write or flush failure returns `Err(Ext2Error::Block(_))` without an instance. `ReadOnly` performs no writes. Add a mount test asserting a `ReadWrite` mount leaves `s_state == 0` on media and a second `ReadWrite` mount then fails with `MountRequiresCleanFilesystem` until `unmount` sets it back to one.

- [ ] **Step 6: Implement deterministic fault and power-cut devices**

In `tests/support/mod.rs`, add `pub mod fault_device;` and `pub mod power_cut_device;`.

Implement `fault_device.rs` as test-only `std` code:

```rust
use relay_core::block::{BlockDevice, BlockError, BlockGeometry};
use std::cell::Cell;
use std::rc::Rc;

pub struct FaultDevice<D> {
    inner: D,
    writes: Rc<Cell<usize>>,
    flushes: Rc<Cell<usize>>,
    fail_write: Option<usize>,
    fail_flush: Option<usize>,
}

impl<D> FaultDevice<D> {
    pub fn fail_write(inner: D, index: usize) -> Self;
    pub fn fail_flush(inner: D, index: usize) -> Self;
    pub fn write_count(&self) -> usize;
    pub fn flush_count(&self) -> usize;
}

impl<D: BlockDevice> BlockDevice for FaultDevice<D> {
    fn geometry(&self) -> BlockGeometry;
    fn read_sectors(&mut self, first_lba: u64, dst: &mut [u8]) -> Result<(), BlockError>;
    fn write_sectors(&mut self, first_lba: u64, src: &[u8]) -> Result<(), BlockError>;
    fn flush(&mut self) -> Result<(), BlockError>;
}
```

Reads pass through without counting. Writes increment the counter then return `Err(BlockError::Transport)` when equal to `fail_write`; flushes increment then return `Err(BlockError::Flush)` when equal to `fail_flush`.

Implement `power_cut_device.rs`:

```rust
use relay_core::block::{BlockDevice, BlockError, BlockGeometry};
use std::collections::BTreeMap;

pub struct PowerCutDevice<D> {
    inner: D,
    overlay: BTreeMap<u64, Vec<u8>>,
}

impl<D> PowerCutDevice<D> {
    pub fn new(inner: D) -> Self;
    pub fn power_cut(&mut self);
}
```

`write_sectors` validates exact sectors against geometry, stores a copy in `overlay` keyed by `first_lba`, and returns `Ok` without touching inner. `flush` replays overlay entries in LBA order through inner `write_sectors`, calls inner `flush`, clears overlay on success, and preserves overlay on failure. `power_cut` discards the overlay. Reads check the overlay first for fully covered ranges, else read inner.

- [ ] **Step 7: Run formatting, focused tests, and lint**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test ext2_files --locked
cargo clippy -p relay-core --all-targets --locked -- -D warnings
```

Expected: the read-only health test passes; the poison test passes once stubs route through `require_writable` and `write_block` poisoning is wired for the mount-clear path. Full file mutation still returns `UnsupportedFile` until Task 2.

- [ ] **Step 8: Commit the contract and harness**

```bash
git add crates/relay-core/src/ext2/on_disk.rs crates/relay-core/src/ext2/mod.rs crates/relay-core/src/vfs/mod.rs crates/relay-core/tests/support crates/relay-core/tests/ext2_files.rs
git commit -m "feat: add ext2 health and fault harness"
```

### Task 2: Allocator And Ordered File Mutation

**Files:**
- Create: `crates/relay-core/src/ext2/allocator.rs`
- Create: `crates/relay-core/src/ext2/mutation.rs`
- Modify: `crates/relay-core/src/ext2/directory.rs`
- Modify: `crates/relay-core/src/ext2/mod.rs`
- Modify: `crates/relay-core/tests/ext2_files.rs`

**Interfaces:**
- Consumes: Task 1 `MountHealth`, `require_writable`, `write_block`, `sync`, fault and power-cut devices, validated `Geometry`.
- Produces: `create_file`, `write_at`, `truncate`, `unlink_file` with ordering and `i_blocks` accounting for Task 3 directory work and Task 4 sweeps.

- [ ] **Step 1: Write failing file mutation tests**

Append these exact tests to `tests/ext2_files.rs`:

```rust
#[test]
fn create_write_and_reopen_survive_unmount() {
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let node = fs.create_file(fs.root(), &name(b"hello")).unwrap();

    fs.write_at(node, 0, b"relay os").unwrap();
    fs.unmount().unwrap();

    let mut reopened = Ext2::mount(image.open().unwrap(), MountMode::ReadOnly).unwrap();
    let found = reopened.lookup(reopened.root(), &name(b"hello")).unwrap();
    let mut bytes = [0; 8];

    assert_eq!(reopened.read_at(found, 0, &mut bytes).unwrap(), 8);
    assert_eq!(&bytes, b"relay os");
}

#[test]
fn maximum_file_size_is_exact() {
    let big = vec![0x5a; 4_243_456];
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let node = fs.create_file(fs.root(), &name(b"big")).unwrap();

    assert!(fs.write_at(node, 0, &big).is_ok());
    assert_eq!(
        fs.write_at(node, 4_243_456, &[0]),
        Err(Ext2Error::FileTooLarge)
    );
    assert_eq!(fs.truncate(node, 4_243_457), Err(Ext2Error::FileTooLarge));
}

#[test]
fn write_crosses_direct_to_single_indirect_and_counts_indirect() {
    let bytes: Vec<u8> = (0..13 * 4096).map(|index| index as u8).collect();
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let node = fs.create_file(fs.root(), &name(b"boundary")).unwrap();

    fs.write_at(node, 0, &bytes).unwrap();

    let mut edge = [0; 2];
    assert_eq!(fs.read_at(node, 12 * 4096 - 1, &mut edge).unwrap(), 2);
    assert_eq!(edge, [0xff, 0x00]);
}
```

- [ ] **Step 2: Run file tests to verify they fail**

Run: `cargo test -p relay-core --test ext2_files --locked`

Expected: FAIL because file mutation stubs return `UnsupportedFile`.

- [ ] **Step 3: Implement the single-group allocator**

Create `src/ext2/allocator.rs` with exactly:

```rust
pub(super) fn claim_block<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<u32, Ext2Error>;
pub(super) fn release_block<D: BlockDevice>(fs: &mut Ext2<D>, block: u32) -> Result<(), Ext2Error>;
pub(super) fn claim_inode<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<u32, Ext2Error>;
pub(super) fn release_inode<D: BlockDevice>(fs: &mut Ext2<D>, inode: u32) -> Result<(), Ext2Error>;
```

Each function reads its 4 KiB bitmap block, scans LSB-first with checked `block_or_inode` arithmetic, skips structural blocks through `geometry.is_structural_metadata_block` and inodes 1 through 10, flips one bit with read-modify-write through `write_block`, updates superblock and group free counts together in the same batch, and flushes. Exhaustion returns `NoSpace`. Any I/O failure poisons. Expose `pub(crate)` geometry fields needed by the allocator; keep bitmap layouts private to this module.

- [ ] **Step 4: Implement ordered file create, write, truncate, and unlink**

Create `src/ext2/mutation.rs` with file functions called by the inherent methods. Follow this exact order:

1. `create_file`: validate parent is a directory, confirm `lookup` is `NotFound` else `AlreadyExists`, claim inode, write zeroed inode with mode `0x81A4` at the validated table location, flush, update free-inode counts, insert the parent entry through `super::directory::insert` introduced in this task (supporting regular-file entries; Task 3 reuses it for directories and adds `remove`), flush.
2. `write_at`: reject directories with `WrongNodeKind`, check `offset + src.len()` for overflow and `FileTooLarge`, handle empty `src` as success, allocate each uncovered logical block by claiming, zeroing, writing, and flushing before publishing, claim and flush the indirect block before any indirect data pointer, publish pointers into the inode image, advance size, set `i_blocks`, flush inode and counts.
3. `truncate`: same maximum check, grow with zeroed blocks, shrink by clearing trailing pointers, flushing the inode, then freeing blocks and the unused indirect block.
4. `unlink_file`: resolve the name, require a regular child, remove the parent entry with `super::directory::remove` introduced in this task alongside `insert` and flush first, then clear child pointers, flush the child inode, free blocks and inode, update counts, flush.

Declare `mod allocator;` and `mod mutation;` in `src/ext2/mod.rs` and replace the four file stubs with delegating bodies. Introduce the narrow `pub(super) directory::insert` and `pub(super) directory::remove` functions in `directory.rs` in this task so file create and unlink have entry call sites; Task 3 reuses both for directories.

- [ ] **Step 5: Run file tests plus existing read and mount suites**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test ext2_files --test ext2_mount --test ext2_read --locked
cargo clippy -p relay-core --all-targets --locked -- -D warnings
```

Expected: new file tests pass; mount and read suites pass unchanged; reopened images list the created file with exact bytes.

- [ ] **Step 6: Commit file mutation**

```bash
git add crates/relay-core/src/ext2/allocator.rs crates/relay-core/src/ext2/mutation.rs crates/relay-core/src/ext2/directory.rs crates/relay-core/src/ext2/mod.rs crates/relay-core/tests/ext2_files.rs
git commit -m "feat: add ordered ext2 file mutation"
```

### Task 3: Directory Create, Insert, And Removal

**Files:**
- Modify: `crates/relay-core/src/ext2/directory.rs`
- Modify: `crates/relay-core/src/ext2/mutation.rs`
- Modify: `crates/relay-core/src/ext2/mod.rs`
- Create: `crates/relay-core/tests/ext2_directories.rs`

**Interfaces:**
- Consumes: Task 1 health guard and `write_block`, Task 2 allocator and file-mutation ordering.
- Produces: `create_dir`, `remove_dir`, validated entry insert and coalesce for Task 4 persistence sweeps.

- [ ] **Step 1: Write failing directory mutation tests**

Create `tests/ext2_directories.rs` with `mod support;` and these exact tests:

```rust
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
    let image = fixture_with_files(&[]).unwrap();
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();
    let docs = fs.create_dir(fs.root(), &name(b"docs")).unwrap();
    fs.create_file(docs, &name(b"hello")).unwrap();

    assert_eq!(
        fs.remove_dir(fs.root(), &name(b"docs")),
        Err(Ext2Error::DirectoryNotEmpty)
    );

    let mut image = fixture_with_files(&[("link", b"relay")]).unwrap();
    image.set_root_entry_inode("link", 2);
    image.set_root_entry_file_type("link", 2);
    let mut fs = Ext2::mount(image.open().unwrap(), MountMode::ReadWrite).unwrap();

    assert_eq!(
        fs.remove_dir(fs.root(), &name(b"link")),
        Err(Ext2Error::WrongNodeKind)
    );
}
```

- [ ] **Step 2: Run directory tests to verify they fail**

Run: `cargo test -p relay-core --test ext2_directories --locked`

Expected: FAIL because `create_dir` and `remove_dir` are absent or stubbed.

- [ ] **Step 3: Implement validated entry insert and coalesce**

In `src/ext2/directory.rs`, reuse the `insert` function introduced in Task 2 with this exact signature (extend it to directory file type two if Task 2 only covered regular files) and add exactly one new function:

```rust
pub(super) fn remove<D: BlockDevice>(
    fs: &mut Ext2<D>,
    dir: NodeId,
    name: &Name,
) -> Result<NodeId, Ext2Error>;
```

Task 2 introduced exactly:

```rust
pub(super) fn insert<D: BlockDevice>(
    fs: &mut Ext2<D>,
    dir: NodeId,
    name: &Name,
    child: NodeId,
    kind: NodeKind,
) -> Result<(), Ext2Error>;
```

`insert` validates the parent is a directory, rejects duplicates with `AlreadyExists`, computes `needed = (8 + name_len + 3) & !3` checked, scans each directory block for a valid deleted record or slack space large enough to split by shrinking the predecessor `rec_len`, writes the new record with file type one for regular and two for directory, flushes the block, or allocates a new zeroed directory block, appends the record, grows parent size and `i_blocks`, and flushes. `remove` scans with full validation, merges the target `rec_len` into its predecessor or zeroes a leading inode while preserving a valid shape, flushes the block, and returns the removed child ID. Both map device failures to poisoned `Block` errors.

- [ ] **Step 4: Implement mkdir and removal ordering**

In `mutation.rs`, implement `create_dir` by claiming an inode, writing a directory inode with mode `0x41ED`, link count two, size 4,096, `i_blocks` eight, and zeroed times, writing one data block with `.` pointing to self and `..` pointing to the parent with file type two, flushing both, incrementing group used directories and the parent link count, flushing counts and inodes, then calling `directory::insert` last. Implement `remove_dir` by resolving the child, requiring a directory, verifying emptiness by scanning for anything other than valid `.` and `..`, calling `directory::remove` and flushing first, then clearing child pointers, flushing the child inode, freeing blocks and inode, decrementing used directories and parent links and free counts, and flushing. Replace the `create_dir` and `remove_dir` stubs from Task 1 in `mod.rs` with delegating bodies; file and sync stubs were or will be replaced in Tasks 2 and 4.

- [ ] **Step 5: Run directory plus file and read suites**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test ext2_directories --test ext2_files --test ext2_read --locked
cargo clippy -p relay-core --all-targets --locked -- -D warnings
```

Expected: mkdir, listing, reopen, duplicate, non-empty, and root-rejection behavior pass; file tests still pass.

- [ ] **Step 6: Commit directory mutation**

```bash
git add crates/relay-core/src/ext2/directory.rs crates/relay-core/src/ext2/mutation.rs crates/relay-core/src/ext2/mod.rs crates/relay-core/tests/ext2_directories.rs
git commit -m "feat: add ext2 directory mutation"
```

### Task 4: Clean Unmount, Fault Sweeps, And Host Compatibility

**Files:**
- Modify: `crates/relay-core/src/ext2/mod.rs`
- Create: `crates/relay-core/tests/ext2_faults.rs`

**Interfaces:**
- Consumes: Tasks 1 through 3 mutation, ordering, health, fault and power-cut devices.
- Produces: verified `sync` and `unmount`, poisoned-mount guarantees, `e2fsck` compatibility, and the Task 9 release gate.

- [ ] **Step 1: Write failing sweep and unmount tests**

Create `tests/ext2_faults.rs` with `mod support;` and these exact tests:

```rust
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
    for index in 0..16 {
        let image = fixture_with_files(&[]).unwrap();
        let device = FaultDevice::fail_write(image.open().unwrap(), index);
        let Ok(mut fs) = Ext2::mount(device, MountMode::ReadWrite) else {
            continue;
        };
        let _ = fs.create_file(fs.root(), &name(b"a"));
        assert_ne!(fs.health(), MountHealth::Writable);
    }
}
```

- [ ] **Step 2: Run sweep tests to verify gaps**

Run: `cargo test -p relay-core --test ext2_faults --locked`

Expected: FAIL or incomplete because `unmount` clean marking and full-index poisoning are not yet verified together.

- [ ] **Step 3: Finalize sync and clean unmount**

Verify `sync` requires writable, performs no media shape changes beyond already-flushed per-operation writes, calls one final device `flush`, and poisons on failure. Verify `unmount` calls `sync`, writes u16 one at superblock offset 58 through exact sectors, writes the block, flushes again, and only then sets `Unmounted`. Confirm reads after `Unmounted` return `WriteDisabled`. Add a test helper that asserts `s_state == 1` on media after `unmount` by reopening the file and checking byte 1082.

- [ ] **Step 4: Sweep all persistence cut points**

Extend `tests/ext2_faults.rs` with loops that fail every write index and every flush index for `create_file`, `write_at`, `truncate`, `unlink_file`, `create_dir`, and `remove_dir`. For each faulted run assert the operation errors, health is `WriteFailed`, and the next mutation is `WriteDisabled`. Add power-cut tests that wrap `FileDevice` in `PowerCutDevice`, perform each mutation without flushing, call `power_cut`, reopen, copy the image to a disposable path, run host repair on the copy, then require a second `LC_ALL=C e2fsck -fn` exit status zero and no duplicate allocated block ownership by walking inodes 2 through 4,096 and collecting referenced blocks into a checked set. Add success-path tests that mutate, `unmount`, reopen read-only, verify exact bytes and listings, and require `LC_ALL=C e2fsck -fn target/root.ext2` exit status zero after `cargo xtask image --output target/relay-os.img`.

- [ ] **Step 5: Run the full Task 9 verification**

Run:

```bash
cargo fmt --all --check
cargo test -p relay-core --test ext2_files --test ext2_directories --test ext2_faults --locked -- --nocapture
cargo xtask image --output target/relay-os.img
cargo test -p relay-core --test ext2_read --locked
LC_ALL=C e2fsck -fn target/root.ext2
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Expected: all focused tests pass, generated-root reads pass, `e2fsck` exits zero, Clippy reports no warnings, and the workspace suite passes.

- [ ] **Step 6: Commit durable mutation**

```bash
git add crates/relay-core
git commit -m "feat: add durable ext2 mutation"
```
