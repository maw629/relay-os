use crate::block::BlockDevice;

use super::{
    Ext2, Ext2Error,
    on_disk::{self, BLOCK_BYTES},
};

const PROFILE_BLOCKS: u32 = 32_768;
const PROFILE_INODES: u32 = 4_096;
const RESERVED_INODES: u32 = 10;
const SUPERBLOCK_BLOCK: u32 = 0;
const GROUP_DESCRIPTOR_BLOCK: u32 = 1;
const SUPERBLOCK_BASE: usize = 1024;

fn flush<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<(), Ext2Error> {
    fs.device.flush().map_err(|error| {
        fs.poison();
        Ext2Error::Block(error)
    })
}

fn read_bitmap<D: BlockDevice>(
    fs: &mut Ext2<D>,
    bitmap: u32,
) -> Result<[u8; BLOCK_BYTES], Ext2Error> {
    let mut bytes = [0; BLOCK_BYTES];
    fs.read_block(bitmap, &mut bytes)
        .inspect_err(|_| fs.poison())?;
    Ok(bytes)
}

fn adjust_block_counts<D: BlockDevice>(fs: &mut Ext2<D>, decrement: bool) -> Result<(), Ext2Error> {
    let mut superblock = [0; BLOCK_BYTES];
    fs.read_block(SUPERBLOCK_BLOCK, &mut superblock)
        .inspect_err(|_| fs.poison())?;
    let mut group = [0; BLOCK_BYTES];
    fs.read_block(GROUP_DESCRIPTOR_BLOCK, &mut group)
        .inspect_err(|_| fs.poison())?;
    let free_blocks = on_disk::u32(
        &superblock,
        SUPERBLOCK_BASE + on_disk::SUPERBLOCK_FREE_BLOCKS,
        "free_blocks",
    )?;
    let group_free = u32::from(on_disk::u16(
        &group,
        on_disk::GROUP_DESCRIPTOR_FREE_BLOCKS,
        "free_blocks",
    )?);
    let (new_free, new_group_free) = if decrement {
        (
            free_blocks
                .checked_sub(1)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "free_blocks",
                })?,
            group_free
                .checked_sub(1)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "free_blocks",
                })?,
        )
    } else {
        (
            free_blocks
                .checked_add(1)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "free_blocks",
                })?,
            group_free
                .checked_add(1)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "free_blocks",
                })?,
        )
    };
    if new_group_free > u32::from(u16::MAX) {
        return Err(Ext2Error::CorruptMetadata {
            field: "free_blocks",
        });
    }
    superblock[SUPERBLOCK_BASE + on_disk::SUPERBLOCK_FREE_BLOCKS
        ..SUPERBLOCK_BASE + on_disk::SUPERBLOCK_FREE_BLOCKS + 4]
        .copy_from_slice(&new_free.to_le_bytes());
    let narrow = u16::try_from(new_group_free).map_err(|_| Ext2Error::CorruptMetadata {
        field: "free_blocks",
    })?;
    group[on_disk::GROUP_DESCRIPTOR_FREE_BLOCKS..on_disk::GROUP_DESCRIPTOR_FREE_BLOCKS + 2]
        .copy_from_slice(&narrow.to_le_bytes());
    fs.write_block(SUPERBLOCK_BLOCK, &superblock)?;
    fs.write_block(GROUP_DESCRIPTOR_BLOCK, &group)?;
    Ok(())
}

fn adjust_inode_counts<D: BlockDevice>(fs: &mut Ext2<D>, decrement: bool) -> Result<(), Ext2Error> {
    let mut superblock = [0; BLOCK_BYTES];
    fs.read_block(SUPERBLOCK_BLOCK, &mut superblock)
        .inspect_err(|_| fs.poison())?;
    let mut group = [0; BLOCK_BYTES];
    fs.read_block(GROUP_DESCRIPTOR_BLOCK, &mut group)
        .inspect_err(|_| fs.poison())?;
    let free_inodes = on_disk::u32(
        &superblock,
        SUPERBLOCK_BASE + on_disk::SUPERBLOCK_FREE_INODES,
        "free_inodes",
    )?;
    let group_free = u32::from(on_disk::u16(
        &group,
        on_disk::GROUP_DESCRIPTOR_FREE_INODES,
        "free_inodes",
    )?);
    let (new_free, new_group_free) = if decrement {
        (
            free_inodes
                .checked_sub(1)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "free_inodes",
                })?,
            group_free
                .checked_sub(1)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "free_inodes",
                })?,
        )
    } else {
        (
            free_inodes
                .checked_add(1)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "free_inodes",
                })?,
            group_free
                .checked_add(1)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "free_inodes",
                })?,
        )
    };
    superblock[SUPERBLOCK_BASE + on_disk::SUPERBLOCK_FREE_INODES
        ..SUPERBLOCK_BASE + on_disk::SUPERBLOCK_FREE_INODES + 4]
        .copy_from_slice(&new_free.to_le_bytes());
    let narrow = u16::try_from(new_group_free).map_err(|_| Ext2Error::CorruptMetadata {
        field: "free_inodes",
    })?;
    group[on_disk::GROUP_DESCRIPTOR_FREE_INODES..on_disk::GROUP_DESCRIPTOR_FREE_INODES + 2]
        .copy_from_slice(&narrow.to_le_bytes());
    fs.write_block(SUPERBLOCK_BLOCK, &superblock)?;
    fs.write_block(GROUP_DESCRIPTOR_BLOCK, &group)?;
    Ok(())
}

pub(super) fn claim_block<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<u32, Ext2Error> {
    fs.require_writable()?;
    let bitmap_block = fs.geometry.block_bitmap;
    let mut bitmap = read_bitmap(fs, bitmap_block)?;
    for block in 0..PROFILE_BLOCKS {
        if fs.geometry.is_structural_metadata_block(block) {
            continue;
        }
        let byte = usize::try_from(block / 8).map_err(|_| Ext2Error::CorruptMetadata {
            field: "block_bitmap",
        })?;
        let bit = block % 8;
        let slot = bitmap.get_mut(byte).ok_or(Ext2Error::CorruptMetadata {
            field: "block_bitmap",
        })?;
        if (*slot >> bit) & 1 == 1 {
            continue;
        }
        *slot |= 1 << bit;
        let bitmap_copy = bitmap;
        fs.write_block(bitmap_block, &bitmap_copy)?;
        adjust_block_counts(fs, true)?;
        flush(fs)?;
        return Ok(block);
    }
    Err(Ext2Error::NoSpace)
}

pub(super) fn release_block<D: BlockDevice>(fs: &mut Ext2<D>, block: u32) -> Result<(), Ext2Error> {
    fs.require_writable()?;
    if block >= PROFILE_BLOCKS {
        return Err(Ext2Error::CorruptMetadata {
            field: "block_pointer",
        });
    }
    if fs.geometry.is_structural_metadata_block(block) {
        return Err(Ext2Error::CorruptMetadata {
            field: "block_pointer",
        });
    }
    let bitmap_block = fs.geometry.block_bitmap;
    let mut bitmap = read_bitmap(fs, bitmap_block)?;
    let byte = usize::try_from(block / 8).map_err(|_| Ext2Error::CorruptMetadata {
        field: "block_bitmap",
    })?;
    let bit = block % 8;
    let slot = bitmap.get_mut(byte).ok_or(Ext2Error::CorruptMetadata {
        field: "block_bitmap",
    })?;
    *slot &= !(1 << bit);
    let bitmap_copy = bitmap;
    fs.write_block(bitmap_block, &bitmap_copy)?;
    adjust_block_counts(fs, false)?;
    flush(fs)?;
    Ok(())
}

pub(super) fn claim_inode<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<u32, Ext2Error> {
    fs.require_writable()?;
    let bitmap_block = fs.geometry.inode_bitmap;
    let mut bitmap = read_bitmap(fs, bitmap_block)?;
    for inode in 1..=PROFILE_INODES {
        if inode <= RESERVED_INODES {
            continue;
        }
        let bit_index = inode.checked_sub(1).ok_or(Ext2Error::CorruptMetadata {
            field: "inode_bitmap",
        })?;
        let byte = usize::try_from(bit_index / 8).map_err(|_| Ext2Error::CorruptMetadata {
            field: "inode_bitmap",
        })?;
        let bit = bit_index % 8;
        let slot = bitmap.get_mut(byte).ok_or(Ext2Error::CorruptMetadata {
            field: "inode_bitmap",
        })?;
        if (*slot >> bit) & 1 == 1 {
            continue;
        }
        *slot |= 1 << bit;
        let bitmap_copy = bitmap;
        fs.write_block(bitmap_block, &bitmap_copy)?;
        adjust_inode_counts(fs, true)?;
        flush(fs)?;
        return Ok(inode);
    }
    Err(Ext2Error::NoSpace)
}

pub(super) fn release_inode<D: BlockDevice>(fs: &mut Ext2<D>, inode: u32) -> Result<(), Ext2Error> {
    fs.require_writable()?;
    if !(1..=PROFILE_INODES).contains(&inode) || inode <= RESERVED_INODES {
        return Err(Ext2Error::CorruptMetadata {
            field: "inode_bitmap",
        });
    }
    let bit_index = inode.checked_sub(1).ok_or(Ext2Error::CorruptMetadata {
        field: "inode_bitmap",
    })?;
    let bitmap_block = fs.geometry.inode_bitmap;
    let mut bitmap = read_bitmap(fs, bitmap_block)?;
    let byte = usize::try_from(bit_index / 8).map_err(|_| Ext2Error::CorruptMetadata {
        field: "inode_bitmap",
    })?;
    let bit = bit_index % 8;
    let slot = bitmap.get_mut(byte).ok_or(Ext2Error::CorruptMetadata {
        field: "inode_bitmap",
    })?;
    *slot &= !(1 << bit);
    let bitmap_copy = bitmap;
    fs.write_block(bitmap_block, &bitmap_copy)?;
    adjust_inode_counts(fs, false)?;
    flush(fs)?;
    Ok(())
}
