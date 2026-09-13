use super::Ext2Error;

pub const SUPERBLOCK_BYTES: usize = 1024;
pub const BLOCK_BYTES: usize = 4096;
pub const GROUP_DESCRIPTOR_BYTES: usize = 32;

pub const SUPERBLOCK_INODES_COUNT: usize = 0;
pub const SUPERBLOCK_BLOCKS_COUNT: usize = 4;
pub const SUPERBLOCK_FIRST_DATA_BLOCK: usize = 20;
pub const SUPERBLOCK_LOG_BLOCK_SIZE: usize = 24;
pub const SUPERBLOCK_LOG_FRAGMENT_SIZE: usize = 28;
pub const SUPERBLOCK_BLOCKS_PER_GROUP: usize = 32;
pub const SUPERBLOCK_FRAGMENTS_PER_GROUP: usize = 36;
pub const SUPERBLOCK_INODES_PER_GROUP: usize = 40;
pub const SUPERBLOCK_MAGIC: usize = 56;
pub const SUPERBLOCK_STATE: usize = 58;
#[allow(dead_code)] // Tasks 2 and 3 account free blocks and inodes through this module.
pub const SUPERBLOCK_FREE_BLOCKS: usize = 12;
#[allow(dead_code)] // Tasks 2 and 3 account free blocks and inodes through this module.
pub const SUPERBLOCK_FREE_INODES: usize = 16;
#[allow(dead_code)] // Retained with the complete superblock field map for later mount-state support.
pub const SUPERBLOCK_ERRORS: usize = 60;
pub const SUPERBLOCK_REVISION_LEVEL: usize = 76;
pub const SUPERBLOCK_INODE_SIZE: usize = 88;
pub const SUPERBLOCK_FEATURE_COMPAT: usize = 92;
pub const SUPERBLOCK_FEATURE_INCOMPAT: usize = 96;
pub const SUPERBLOCK_FEATURE_RO_COMPAT: usize = 100;

pub const GROUP_DESCRIPTOR_BLOCK_BITMAP: usize = 0;
pub const GROUP_DESCRIPTOR_INODE_BITMAP: usize = 4;
pub const GROUP_DESCRIPTOR_INODE_TABLE: usize = 8;
#[allow(dead_code)] // Tasks 2 and 3 account free blocks and inodes through this module.
pub const GROUP_DESCRIPTOR_FREE_BLOCKS: usize = 12;
#[allow(dead_code)] // Tasks 2 and 3 account free blocks and inodes through this module.
pub const GROUP_DESCRIPTOR_FREE_INODES: usize = 14;
#[allow(dead_code)] // Tasks 2 and 3 account free blocks and inodes through this module.
pub const GROUP_DESCRIPTOR_USED_DIRS: usize = 16;

#[allow(dead_code)] // Tasks 2 and 3 decode these records through this module.
pub const INODE_MODE: usize = 0;
#[allow(dead_code)]
pub const INODE_UID: usize = 2;
#[allow(dead_code)]
pub const INODE_SIZE_LO: usize = 4;
#[allow(dead_code)]
pub const INODE_ATIME: usize = 8;
#[allow(dead_code)]
pub const INODE_CTIME: usize = 12;
#[allow(dead_code)]
pub const INODE_MTIME: usize = 16;
#[allow(dead_code)]
pub const INODE_DTIME: usize = 20;
#[allow(dead_code)]
pub const INODE_GID: usize = 24;
#[allow(dead_code)]
pub const INODE_LINKS: usize = 26;
#[allow(dead_code)]
pub const INODE_BLOCKS: usize = 28;
#[allow(dead_code)]
pub const INODE_FLAGS: usize = 32;
#[allow(dead_code)]
pub const INODE_BLOCK: usize = 40;
#[allow(dead_code)]
pub const INODE_SIZE_HIGH: usize = 108;
#[allow(dead_code)]
pub const DIRECTORY_INODE: usize = 0;
#[allow(dead_code)]
pub const DIRECTORY_RECORD_LENGTH: usize = 4;
#[allow(dead_code)]
pub const DIRECTORY_NAME_LENGTH: usize = 6;
#[allow(dead_code)]
pub const DIRECTORY_FILE_TYPE: usize = 7;
#[allow(dead_code)]
pub const DIRECTORY_NAME: usize = 8;

pub fn u16(bytes: &[u8], offset: usize, field: &'static str) -> Result<u16, Ext2Error> {
    let range = range(offset, 2, field)?;
    let value = bytes
        .get(range)
        .ok_or(Ext2Error::CorruptMetadata { field })?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

pub fn u32(bytes: &[u8], offset: usize, field: &'static str) -> Result<u32, Ext2Error> {
    let range = range(offset, 4, field)?;
    let value = bytes
        .get(range)
        .ok_or(Ext2Error::CorruptMetadata { field })?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

#[allow(dead_code)]
pub fn u64(bytes: &[u8], offset: usize, field: &'static str) -> Result<u64, Ext2Error> {
    let range = range(offset, 8, field)?;
    let value = bytes
        .get(range)
        .ok_or(Ext2Error::CorruptMetadata { field })?;
    Ok(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

fn range(
    offset: usize,
    length: usize,
    field: &'static str,
) -> Result<core::ops::Range<usize>, Ext2Error> {
    let end = offset
        .checked_add(length)
        .ok_or(Ext2Error::CorruptMetadata { field })?;
    Ok(offset..end)
}
