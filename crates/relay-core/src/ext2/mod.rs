mod directory;
mod inode;
mod on_disk;
mod validate;

use crate::block::{BlockDevice, BlockError};
use crate::fs::{DirEntry, Metadata, Name, NodeId};
use alloc::vec::Vec;
use on_disk::BLOCK_BYTES;
use validate::Geometry;

const PROFILE_BLOCKS: u32 = 32_768;
const SECTORS_PER_BLOCK: u64 = 8;

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
}

#[allow(dead_code)] // Tasks 2 and 3 consume the mounted device and validated geometry.
pub struct Ext2<D> {
    device: D,
    mode: MountMode,
    geometry: Geometry,
}

impl<D: BlockDevice> Ext2<D> {
    pub fn mount(mut device: D, mode: MountMode) -> Result<Self, Ext2Error> {
        let geometry = validate::mount(&mut device, mode)?;
        Ok(Self {
            device,
            mode,
            geometry,
        })
    }

    pub fn root(&self) -> NodeId {
        NodeId(2)
    }

    pub fn metadata(&mut self, node: NodeId) -> Result<Metadata, Ext2Error> {
        inode::load(self, node)?.metadata()
    }

    pub fn lookup(&mut self, dir: NodeId, name: &Name) -> Result<NodeId, Ext2Error> {
        directory::lookup(self, dir, name)
    }

    pub fn read_dir(&mut self, dir: NodeId) -> Result<Vec<DirEntry>, Ext2Error> {
        directory::read_dir(self, dir)
    }

    pub fn read_at(
        &mut self,
        node: NodeId,
        offset: u64,
        dst: &mut [u8],
    ) -> Result<usize, Ext2Error> {
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
