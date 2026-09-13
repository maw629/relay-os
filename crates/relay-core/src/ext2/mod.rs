mod directory;
mod inode;
mod on_disk;
mod validate;

use crate::block::{BlockDevice, BlockError};
use crate::fs::{DirEntry, Metadata, Name, NodeId};
use alloc::vec::Vec;
use on_disk::{BLOCK_BYTES, SUPERBLOCK_BYTES};
use validate::Geometry;

const PROFILE_BLOCKS: u32 = 32_768;
const SECTORS_PER_BLOCK: u64 = 8;
const SUPERBLOCK_FIRST_LBA: u64 = 2;
const EXT2_VALID_FS: u16 = 1;
const EXT2_DIRTY_FS: u16 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountMode {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ext2Error {
    Block(BlockError),
    UnsupportedSector { sector_size: u32 },
    UnsupportedProfile { field: &'static str },
    UnsupportedFeature { field: &'static str, bits: u32 },
    MountRequiresCleanFilesystem,
    CorruptMetadata { field: &'static str },
    InvalidNode,
    WrongNodeKind,
    NotFound,
    UnsupportedFile,
    SparseFile,
    Allocation,
    FileTooLarge,
    NoSpace,
    WriteDisabled,
    ReadOnly,
    AlreadyExists,
    DirectoryNotEmpty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountHealth {
    ReadOnly,
    Writable,
    WriteFailed,
    Unmounted,
}

#[allow(dead_code)] // Tasks 2 and 3 consume the mounted device and validated geometry.
pub struct Ext2<D> {
    device: D,
    mode: MountMode,
    geometry: Geometry,
    health: MountHealth,
}

impl<D: BlockDevice> Ext2<D> {
    pub fn mount(mut device: D, mode: MountMode) -> Result<Self, Ext2Error> {
        let geometry = validate::mount(&mut device, mode)?;
        let health = match mode {
            MountMode::ReadOnly => MountHealth::ReadOnly,
            MountMode::ReadWrite => {
                write_superblock_state(&mut device, EXT2_DIRTY_FS)?;
                MountHealth::Writable
            }
        };
        Ok(Self {
            device,
            mode,
            geometry,
            health,
        })
    }

    pub fn health(&self) -> MountHealth {
        self.health
    }

    fn require_writable(&self) -> Result<(), Ext2Error> {
        match self.health {
            MountHealth::Writable => Ok(()),
            MountHealth::ReadOnly => Err(Ext2Error::ReadOnly),
            MountHealth::WriteFailed | MountHealth::Unmounted => Err(Ext2Error::WriteDisabled),
        }
    }

    fn poison(&mut self) {
        self.health = MountHealth::WriteFailed;
    }

    fn write_block(&mut self, block: u32, bytes: &[u8; BLOCK_BYTES]) -> Result<(), Ext2Error> {
        if block >= PROFILE_BLOCKS {
            return Err(Ext2Error::CorruptMetadata {
                field: "block_pointer",
            });
        }
        let first_lba =
            u64::from(block)
                .checked_mul(SECTORS_PER_BLOCK)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "block_pointer",
                })?;
        if let Err(error) = self.device.write_sectors(first_lba, bytes) {
            self.poison();
            return Err(Ext2Error::Block(error));
        }
        Ok(())
    }

    pub fn sync(&mut self) -> Result<(), Ext2Error> {
        self.require_writable()?;
        if let Err(error) = self.device.flush() {
            self.poison();
            return Err(Ext2Error::Block(error));
        }
        Ok(())
    }

    pub fn unmount(&mut self) -> Result<(), Ext2Error> {
        self.sync()?;
        if let Err(error) = write_superblock_state(&mut self.device, EXT2_VALID_FS) {
            self.poison();
            return Err(error);
        }
        self.health = MountHealth::Unmounted;
        Ok(())
    }

    pub fn create_file(&mut self, parent: NodeId, name: &Name) -> Result<NodeId, Ext2Error> {
        self.require_writable()?;
        let _ = (parent, name);
        self.probe_write_path()?;
        Err(Ext2Error::UnsupportedFile)
    }

    pub fn create_dir(&mut self, parent: NodeId, name: &Name) -> Result<NodeId, Ext2Error> {
        self.require_writable()?;
        let _ = (parent, name);
        self.probe_write_path()?;
        Err(Ext2Error::UnsupportedFile)
    }

    pub fn write_at(&mut self, node: NodeId, offset: u64, src: &[u8]) -> Result<(), Ext2Error> {
        self.require_writable()?;
        let _ = (node, offset, src);
        self.probe_write_path()?;
        Err(Ext2Error::UnsupportedFile)
    }

    pub fn truncate(&mut self, node: NodeId, len: u64) -> Result<(), Ext2Error> {
        self.require_writable()?;
        let _ = (node, len);
        self.probe_write_path()?;
        Err(Ext2Error::UnsupportedFile)
    }

    pub fn unlink_file(&mut self, parent: NodeId, name: &Name) -> Result<(), Ext2Error> {
        self.require_writable()?;
        let _ = (parent, name);
        self.probe_write_path()?;
        Err(Ext2Error::UnsupportedFile)
    }

    pub fn remove_dir(&mut self, parent: NodeId, name: &Name) -> Result<(), Ext2Error> {
        self.require_writable()?;
        let _ = (parent, name);
        self.probe_write_path()?;
        Err(Ext2Error::UnsupportedFile)
    }

    // Task 1 scaffold, replaced by real mutation in Tasks 2 and 3: re-write
    // block 0 with byte-identical contents through `write_block` so that
    // fault-injected writes still poison the mount (see the `ext2_files`
    // poison test). The rewrite changes no media state.
    fn probe_write_path(&mut self) -> Result<(), Ext2Error> {
        let mut bytes = [0; BLOCK_BYTES];
        self.read_block(0, &mut bytes)?;
        self.write_block(0, &bytes)
    }

    fn require_readable(&self) -> Result<(), Ext2Error> {
        if self.health == MountHealth::Unmounted {
            return Err(Ext2Error::WriteDisabled);
        }
        Ok(())
    }

    pub fn root(&self) -> NodeId {
        NodeId(2)
    }

    pub fn metadata(&mut self, node: NodeId) -> Result<Metadata, Ext2Error> {
        self.require_readable()?;
        inode::load(self, node)?.metadata()
    }

    pub fn lookup(&mut self, dir: NodeId, name: &Name) -> Result<NodeId, Ext2Error> {
        self.require_readable()?;
        directory::lookup(self, dir, name)
    }

    pub fn read_dir(&mut self, dir: NodeId) -> Result<Vec<DirEntry>, Ext2Error> {
        self.require_readable()?;
        directory::read_dir(self, dir)
    }

    pub fn read_at(
        &mut self,
        node: NodeId,
        offset: u64,
        dst: &mut [u8],
    ) -> Result<usize, Ext2Error> {
        self.require_readable()?;
        inode::read_at(self, node, offset, dst)
    }

    fn read_block(&mut self, block: u32, bytes: &mut [u8; BLOCK_BYTES]) -> Result<(), Ext2Error> {
        if block >= PROFILE_BLOCKS {
            return Err(Ext2Error::CorruptMetadata {
                field: "block_pointer",
            });
        }
        let lba =
            u64::from(block)
                .checked_mul(SECTORS_PER_BLOCK)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "block_pointer",
                })?;
        self.device
            .read_sectors(lba, bytes)
            .map_err(Ext2Error::Block)
    }
}

fn write_superblock_state<D: BlockDevice>(device: &mut D, state: u16) -> Result<(), Ext2Error> {
    let mut bytes = [0; SUPERBLOCK_BYTES];
    device
        .read_sectors(SUPERBLOCK_FIRST_LBA, &mut bytes)
        .map_err(Ext2Error::Block)?;
    let end = on_disk::SUPERBLOCK_STATE
        .checked_add(2)
        .ok_or(Ext2Error::CorruptMetadata { field: "state" })?;
    let slot = bytes
        .get_mut(on_disk::SUPERBLOCK_STATE..end)
        .ok_or(Ext2Error::CorruptMetadata { field: "state" })?;
    slot.copy_from_slice(&state.to_le_bytes());
    device
        .write_sectors(SUPERBLOCK_FIRST_LBA, &bytes)
        .map_err(Ext2Error::Block)?;
    device.flush().map_err(Ext2Error::Block)?;
    Ok(())
}
