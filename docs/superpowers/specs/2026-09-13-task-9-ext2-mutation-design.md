# Task 9 Ext2 Mutation, Ordering, And Failure Poisoning Design

## Status

Draft for review on 2026-09-13. Follows Approach A (ordered in-place writes with poisoning) approved in brainstorming.

## Objective

Task 9 adds durable, host-tested ext2 mutation for Relay OS's fixed root-filesystem profile. It builds on Task 7's read path and Task 8's read-only `FileSystem` contract to provide create, truncate, write, delete, sync, and unmount with strict write ordering and failure poisoning. Every successful mutation must remain readable and repairable by standard Linux ext2 tools. Any uncertain write or flush must permanently disable later mutation for that mount.

## Scope And Ruling

Task 9 extends the `FileSystem`/`FsError` contract in `vfs/mod.rs` minimally and implements mutation on `Ext2`. It adds no new `Vfs` path-based wrappers; Task 10 adds exactly what shell dispatch needs. This keeps the allocator, ordering, and poisoning review surface focused and avoids designing path APIs without shell requirements.

## Module Boundaries

```text
crates/relay-core/src/ext2/allocator.rs   block/inode bitmap scan, claim, free-count updates
crates/relay-core/src/ext2/mutation.rs    file and directory mutation with ordering
crates/relay-core/src/ext2/mod.rs         MountHealth, health(), mutation methods, sync/unmount
crates/relay-core/src/vfs/mod.rs          extend FileSystem + FsError only
crates/relay-core/tests/support/fault_device.rs
                                          deterministic write/flush fault injection
crates/relay-core/tests/support/power_cut_device.rs
                                          unflushed-overlay discard
crates/relay-core/tests/ext2_files.rs     file mutation integration tests
crates/relay-core/tests/ext2_directories.rs
                                          directory mutation integration tests
crates/relay-core/tests/ext2_faults.rs    fault-index and power-cut sweeps
```

`relay-core` remains `#![no_std]`; host filesystem and process access stay in integration-test support. No unsafe code is required. All media lengths, offsets, arithmetic, conversions, and allocations are checked; malformed media returns typed errors and never panics. All 256 inode bytes are preserved except explicitly updated fields.

## Public API

```rust
pub enum MountHealth { ReadOnly, Writable, WriteFailed, Unmounted }

impl<D: BlockDevice> Ext2<D> {
    pub fn health(&self) -> MountHealth;
    pub fn create_file(&mut self, parent: NodeId, name: &Name) -> Result<NodeId, Ext2Error>;
    pub fn create_dir(&mut self, parent: NodeId, name: &Name) -> Result<NodeId, Ext2Error>;
    pub fn write_at(&mut self, node: NodeId, offset: u64, src: &[u8]) -> Result<(), Ext2Error>;
    pub fn truncate(&mut self, node: NodeId, len: u64) -> Result<(), Ext2Error>;
    pub fn unlink_file(&mut self, parent: NodeId, name: &Name) -> Result<(), Ext2Error>;
    pub fn remove_dir(&mut self, parent: NodeId, name: &Name) -> Result<(), Ext2Error>;
    pub fn sync(&mut self) -> Result<(), Ext2Error>;
    pub fn unmount(&mut self) -> Result<(), Ext2Error>;
}
```

`Ext2Error` gains `FileTooLarge`, `NoSpace`, `WriteDisabled`, `ReadOnly`, `AlreadyExists`, and `DirectoryNotEmpty`. `FsError` gains `FileTooLarge`, `WriteDisabled`, `ReadOnly`, `AlreadyExists`, `NotEmpty`, and `NoSpace`. The `FileSystem` trait gains the same eight mutation methods returning `FsError`; `Ext2<D>` implements them by calling the inherent methods and mapping `AlreadyExists` to `AlreadyExists`, `DirectoryNotEmpty` to `NotEmpty`, `NoSpace` to `NoSpace`, and `Block` to `Io`, preserving the existing read-error mapping.

## Allocator

Single-group profile only: 32,768 blocks, 4,096 inodes, block bitmap at block 2, inode bitmap at block 3, inode table at blocks 4 through 259. The allocator reads a full 4 KiB bitmap block, scans LSB-first, and skips structural blocks via the existing `Geometry::is_structural_metadata_block` check plus reserved inodes 1 through 10. Setting or clearing a bit uses one read-modify-write bitmap block through exact eight-sector transfers. Superblock `free_blocks` at byte 1036 and `free_inodes` at byte 1040 (u32) plus group `free_blocks` at byte 4108, `free_inodes` at byte 4110, and `used_dirs` at byte 4112 (u16) are updated together in the same dirty batch before flush. Count underflow or overflow returns `Corrupt`. Exhaustion returns `NoSpace`. A new checked `write_block(block, &[u8; 4096])` rejects `block >= 32_768` and maps device errors to `Block`. Any allocator I/O failure poisons health.

## File Mutation And Ordering

Every mutation first passes one health guard: `Writable` proceeds, `ReadOnly` returns `ReadOnly`, and `WriteFailed` or `Unmounted` returns `WriteDisabled`.

`create_file` requires a directory parent and a missing name, else `WrongNodeKind`, `AlreadyExists`, or `NotFound`. It claims a free inode bit, writes a zeroed 256-byte inode with mode `0x81A4`, uid and gid zero, size zero, link count one at inode offset 26 (u16), `i_blocks` zero at inode offset 28 (u32), null pointers, flags zero, and zeroed times, flushes it, updates free-inode counts, then inserts the parent directory entry and flushes. A post-claim entry failure poisons health without rollback.

`write_at` accepts regular files only. `offset + len` overflow and lengths above 4,243,456 bytes return `Corrupt` or `FileTooLarge`. Empty writes succeed. Growth allocates each new logical block in order: blocks 0 through 11 direct, 12 through 1,035 through one singly indirect block. Each new block is claimed in the bitmap, zeroed, written, and flushed before its pointer is published; the indirect block itself is claimed, zeroed, and flushed before any data pointer it holds. Pointers are published into the in-memory inode image, then `i_size` advances to `max(old, offset + len)`, `i_blocks` becomes `(data_blocks + indirect_blocks) * 8`, and the inode, counts, and data are flushed together.

`truncate` enforces the same maximum. Growth behaves like `write_at` with zeroed blocks so no sparse hole below `i_size` is ever created. Shrink clears trailing direct and indirect pointers in the inode image, flushes the inode, then frees the now-unreferenced data and indirect blocks and updates counts. Bits are never freed before the unreference flush.

## Directory Mutation

Records remain linear `FILETYPE` with four-byte-aligned `rec_len` wholly inside one 4 KiB block, reusing read-path validation. Insertion splits a valid deleted or slack slot (`needed = 8 + name_len` rounded up to four) by shrinking the predecessor `rec_len` and writing the new record, or allocates a new zeroed directory block, appends the entry, and grows parent `size` and `i_blocks`. The parent entry write and flush is always last on create.

`mkdir` claims an inode, writes a directory inode with mode `0x41ED`, link count two at offset 26, size 4,096, and `i_blocks` eight at offset 28, writes one data block containing valid `.` and `..` records, flushes both, increments `bg_used_dirs_count` and the parent link count, flushes counts and inodes, then inserts the parent entry.

Removal deletes and coalesces the parent entry by merging its `rec_len` into the predecessor or zeroing a leading inode while keeping a valid shape, flushes the parent block and inode, then clears child pointers, flushes the child inode, frees child blocks and the inode bit, decrements counts and link or directory counters, and flushes. `unlink_file` requires a regular child; `remove_dir` requires a directory child that contains only `.` and `..`, rejects inode 2 with `WrongNodeKind` or `Corrupt`, rejects non-empty directories with `DirectoryNotEmpty`, and rejects missing names with `NotFound`. Current-directory and ancestor checks belong to a later VFS path layer, not to these `NodeId` APIs.

## Health, Mount Clearing, Sync, And Unmount

`mount(device, ReadWrite)` performs existing profile validation and the clean check (`s_state == 1`), then clears `EXT2_VALID_FS` by writing zero to superblock byte 1082 (u16 at offset 58) through exact sectors and flushing before returning `Writable`. A clear or flush failure returns `Err` without exposing an instance. `mount(device, ReadOnly)` performs no writes and yields `ReadOnly`; all mutation returns `ReadOnly`.

Any uncertain block write or flush during mutation sets `WriteFailed` and returns the mapped error without rollback. `sync` requires `Writable`, relies on per-operation flush ordering for dirty superblock, group, inode, bitmap, directory, and data blocks, then performs one final device flush; failure poisons. `unmount` runs `sync`, writes `s_state = 1` to the superblock, writes the block, flushes again, and only then becomes `Unmounted`. Any failure leaves `WriteFailed` and never claims clean shutdown. After `Unmounted`, reads and mutations return `WriteDisabled`.

## Fault And Power-Cut Harness

`FaultDevice<D>` passes reads through and counts mutating calls, failing exactly the configured write with `Transport` or flush with `Flush`. `PowerCutDevice<D>` keeps unflushed writes in an in-memory overlay keyed by LBA, persists the overlay on `flush`, and discards it on `power_cut`. Both are test-only and may use `std`.

## Testing

`ext2_files` covers create, write, read-reopen exact bytes, truncate growth and shrink, the exact boundary of 4,243,456 accepted and 4,243,457 rejected as `FileTooLarge`, direct-to-indirect crossover, `i_blocks` including the indirect block, mode `0644`, `NoSpace`, and poison-then-`WriteDisabled`. `ext2_directories` covers `mkdir`, listing, reopen persistence, mode `0755`, link counts and `used_dirs`, on-disk entry order, duplicate `AlreadyExists`, non-empty `DirectoryNotEmpty`, root-removal rejection, and slot coalescing reuse. `ext2_faults` sweeps every write and flush index for each mutation, asserts poisoning and `WriteDisabled` on faulted paths, discards overlays for power cuts, reopens disposable copies, requires `LC_ALL=C e2fsck -fn` exit status zero after success paths and after host repair of cut copies, and checks no duplicate allocated block ownership by walking all inodes.

Verification runs:

```bash
cargo fmt --all --check
cargo test -p relay-core --test ext2_files --test ext2_directories --test ext2_faults --locked -- --nocapture
LC_ALL=C e2fsck -fn target/root.ext2
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Non-Goals

Task 9 adds no `Vfs` path-based mutation wrappers, no shell dispatch or line editing, no symbolic or hard links, no special files, no sparse-file support, no in-OS repair utility, and no USB or kernel runtime integration. Timestamp policy is zeroed times on creation; wall-clock management remains out of scope.
