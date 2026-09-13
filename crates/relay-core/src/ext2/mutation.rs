use crate::{
    block::BlockDevice,
    fs::{Name, NodeId, NodeKind},
};

use super::{
    Ext2, Ext2Error, allocator, directory, inode,
    on_disk::{self, BLOCK_BYTES},
};

fn flush<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<(), Ext2Error> {
    fs.device.flush().map_err(|error| {
        fs.poison();
        Ext2Error::Block(error)
    })
}

fn read_data_block<D: BlockDevice>(
    fs: &mut Ext2<D>,
    block: u32,
    out: &mut [u8; BLOCK_BYTES],
) -> Result<(), Ext2Error> {
    fs.read_block(block, out).inspect_err(|_| fs.poison())
}

fn write_data_block<D: BlockDevice>(
    fs: &mut Ext2<D>,
    block: u32,
    bytes: &[u8; BLOCK_BYTES],
) -> Result<(), Ext2Error> {
    let owned = *bytes;
    fs.write_block(block, &owned)?;
    flush(fs)?;
    Ok(())
}

fn read_indirect<D: BlockDevice>(
    fs: &mut Ext2<D>,
    indirect: u32,
    out: &mut [u32; 1024],
) -> Result<(), Ext2Error> {
    let mut bytes = [0; BLOCK_BYTES];
    read_data_block(fs, indirect, &mut bytes)?;
    for (index, slot) in out.iter_mut().enumerate() {
        let offset = index.checked_mul(4).ok_or(Ext2Error::CorruptMetadata {
            field: "indirect_pointer",
        })?;
        *slot = on_disk::u32(&bytes, offset, "indirect_pointer")?;
    }
    Ok(())
}

fn write_indirect<D: BlockDevice>(
    fs: &mut Ext2<D>,
    indirect: u32,
    pointers: &[u32; 1024],
) -> Result<(), Ext2Error> {
    let mut bytes = [0; BLOCK_BYTES];
    for (index, value) in pointers.iter().enumerate() {
        let offset = index.checked_mul(4).ok_or(Ext2Error::CorruptMetadata {
            field: "indirect_pointer",
        })?;
        let end = offset.checked_add(4).ok_or(Ext2Error::CorruptMetadata {
            field: "indirect_pointer",
        })?;
        let slot = bytes
            .get_mut(offset..end)
            .ok_or(Ext2Error::CorruptMetadata {
                field: "indirect_pointer",
            })?;
        slot.copy_from_slice(&value.to_le_bytes());
    }
    write_data_block(fs, indirect, &bytes)
}

fn direct_offset(index: usize) -> Result<usize, Ext2Error> {
    on_disk::INODE_BLOCK
        .checked_add(index.checked_mul(4).ok_or(Ext2Error::CorruptMetadata {
            field: "inode_pointer",
        })?)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "inode_pointer",
        })
}

fn get_direct(image: &[u8; 256], index: usize) -> Result<u32, Ext2Error> {
    on_disk::u32(image, direct_offset(index)?, "inode_pointer")
}

fn set_direct(image: &mut [u8; 256], index: usize, value: u32) -> Result<(), Ext2Error> {
    let offset = direct_offset(index)?;
    let end = offset.checked_add(4).ok_or(Ext2Error::CorruptMetadata {
        field: "inode_pointer",
    })?;
    let slot = image
        .get_mut(offset..end)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "inode_pointer",
        })?;
    slot.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn ceil_blocks(len: u64) -> Result<u32, Ext2Error> {
    if len == 0 {
        return Ok(0);
    }
    let blocks = len
        .checked_add(BLOCK_BYTES as u64 - 1)
        .and_then(|sum| sum.checked_div(BLOCK_BYTES as u64))
        .ok_or(Ext2Error::CorruptMetadata {
            field: "file_length",
        })?;
    u32::try_from(blocks).map_err(|_| Ext2Error::CorruptMetadata {
        field: "file_length",
    })
}

fn check_end(offset: u64, len: usize) -> Result<u64, Ext2Error> {
    let src_len = u64::try_from(len).map_err(|_| Ext2Error::CorruptMetadata {
        field: "write_length",
    })?;
    let end = offset
        .checked_add(src_len)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "write_offset",
        })?;
    if end > inode::MAX_FILE_BYTES {
        return Err(Ext2Error::FileTooLarge);
    }
    Ok(end)
}

pub(super) fn create_file<D: BlockDevice>(
    fs: &mut Ext2<D>,
    parent: NodeId,
    name: &Name,
) -> Result<NodeId, Ext2Error> {
    fs.require_writable()?;
    let parent_inode = inode::load(fs, parent)?;
    if parent_inode.metadata().kind != NodeKind::Directory {
        return Err(Ext2Error::WrongNodeKind);
    }
    match directory::lookup(fs, parent, name) {
        Ok(_) => return Err(Ext2Error::AlreadyExists),
        Err(Ext2Error::NotFound) => {}
        Err(error) => return Err(error),
    }
    let number = allocator::claim_inode(fs)?;
    let mut bytes = [0; 256];
    bytes[on_disk::INODE_MODE..on_disk::INODE_MODE + 2].copy_from_slice(&0x81A4u16.to_le_bytes());
    bytes[on_disk::INODE_LINKS..on_disk::INODE_LINKS + 2].copy_from_slice(&1u16.to_le_bytes());
    let fresh = inode::Inode { bytes };
    inode::store(fs, NodeId(number), &fresh)?;
    directory::insert(fs, parent, name, NodeId(number), NodeKind::Regular)?;
    Ok(NodeId(number))
}

pub(super) fn create_dir<D: BlockDevice>(
    fs: &mut Ext2<D>,
    parent: NodeId,
    name: &Name,
) -> Result<NodeId, Ext2Error> {
    fs.require_writable()?;
    let parent_inode = inode::load(fs, parent)?;
    if parent_inode.metadata().kind != NodeKind::Directory {
        return Err(Ext2Error::WrongNodeKind);
    }
    match directory::lookup(fs, parent, name) {
        Ok(_) => return Err(Ext2Error::AlreadyExists),
        Err(Ext2Error::NotFound) => {}
        Err(error) => return Err(error),
    }
    let number = allocator::claim_inode(fs)?;
    let data = allocator::claim_block(fs)?;
    let mut bytes = [0; 256];
    bytes[on_disk::INODE_MODE..on_disk::INODE_MODE + 2].copy_from_slice(&0x41EDu16.to_le_bytes());
    bytes[on_disk::INODE_LINKS..on_disk::INODE_LINKS + 2].copy_from_slice(&2u16.to_le_bytes());
    bytes[on_disk::INODE_SIZE_LO..on_disk::INODE_SIZE_LO + 4]
        .copy_from_slice(&4096u32.to_le_bytes());
    bytes[on_disk::INODE_BLOCKS..on_disk::INODE_BLOCKS + 4].copy_from_slice(&8u32.to_le_bytes());
    bytes[on_disk::INODE_BLOCK..on_disk::INODE_BLOCK + 4].copy_from_slice(&data.to_le_bytes());
    let fresh = inode::Inode { bytes };
    inode::store(fs, NodeId(number), &fresh)?;
    let mut block = [0; BLOCK_BYTES];
    write_dot_entry(&mut block, 0, 12, number, b".")?;
    write_dot_entry(&mut block, 12, BLOCK_BYTES - 12, parent.0, b"..")?;
    write_data_block(fs, data, &block)?;
    adjust_used_dirs(fs, true)?;
    adjust_parent_links(fs, parent, true)?;
    // Parent entry write is last on create.
    directory::insert(fs, parent, name, NodeId(number), NodeKind::Directory)?;
    Ok(NodeId(number))
}

fn write_dot_entry(
    block: &mut [u8; BLOCK_BYTES],
    offset: usize,
    rec_len: usize,
    child: u32,
    dot_name: &[u8],
) -> Result<(), Ext2Error> {
    let end = offset
        .checked_add(rec_len)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "directory_record_length",
        })?;
    if end > BLOCK_BYTES || rec_len < 8 || !rec_len.is_multiple_of(4) {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_record_length",
        });
    }
    let slot = block
        .get_mut(offset..end)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "directory_record_length",
        })?;
    slot.fill(0);
    slot[0..4].copy_from_slice(&child.to_le_bytes());
    slot[4..6].copy_from_slice(
        &u16::try_from(rec_len)
            .map_err(|_| Ext2Error::CorruptMetadata {
                field: "directory_record_length",
            })?
            .to_le_bytes(),
    );
    slot[6] = u8::try_from(dot_name.len()).map_err(|_| Ext2Error::CorruptMetadata {
        field: "directory_name",
    })?;
    // File type two: directory.
    slot[7] = 2;
    let name_end = 8usize
        .checked_add(dot_name.len())
        .ok_or(Ext2Error::CorruptMetadata {
            field: "directory_name",
        })?;
    if name_end > rec_len {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_name",
        });
    }
    slot[8..name_end].copy_from_slice(dot_name);
    Ok(())
}

fn adjust_used_dirs<D: BlockDevice>(fs: &mut Ext2<D>, increment: bool) -> Result<(), Ext2Error> {
    fs.require_writable()?;
    let mut group = [0; BLOCK_BYTES];
    fs.read_block(1, &mut group).inspect_err(|_| fs.poison())?;
    let used = on_disk::u16(&group, on_disk::GROUP_DESCRIPTOR_USED_DIRS, "used_dirs")?;
    let updated = if increment {
        used.checked_add(1)
            .ok_or(Ext2Error::CorruptMetadata { field: "used_dirs" })?
    } else {
        used.checked_sub(1)
            .ok_or(Ext2Error::CorruptMetadata { field: "used_dirs" })?
    };
    group[on_disk::GROUP_DESCRIPTOR_USED_DIRS..on_disk::GROUP_DESCRIPTOR_USED_DIRS + 2]
        .copy_from_slice(&updated.to_le_bytes());
    let owned = group;
    fs.write_block(1, &owned)?;
    flush(fs)?;
    Ok(())
}

fn adjust_parent_links<D: BlockDevice>(
    fs: &mut Ext2<D>,
    parent: NodeId,
    increment: bool,
) -> Result<(), Ext2Error> {
    fs.require_writable()?;
    let image = inode::load(fs, parent)?;
    if image.metadata().kind != NodeKind::Directory {
        return Err(Ext2Error::WrongNodeKind);
    }
    let links = on_disk::u16(&image.bytes, on_disk::INODE_LINKS, "inode_links")?;
    let updated = if increment {
        links.checked_add(1).ok_or(Ext2Error::CorruptMetadata {
            field: "inode_links",
        })?
    } else {
        links.checked_sub(1).ok_or(Ext2Error::CorruptMetadata {
            field: "inode_links",
        })?
    };
    let mut fresh = inode::Inode { bytes: image.bytes };
    fresh.bytes[on_disk::INODE_LINKS..on_disk::INODE_LINKS + 2]
        .copy_from_slice(&updated.to_le_bytes());
    inode::store(fs, parent, &fresh)?;
    Ok(())
}

pub(super) fn write_at<D: BlockDevice>(
    fs: &mut Ext2<D>,
    node: NodeId,
    offset: u64,
    src: &[u8],
) -> Result<(), Ext2Error> {
    fs.require_writable()?;
    let end = check_end(offset, src.len())?;
    let image = inode::load(fs, node)?;
    if image.metadata().kind != NodeKind::Regular {
        return Err(Ext2Error::WrongNodeKind);
    }
    if src.is_empty() {
        return Ok(());
    }
    let old_size = image.metadata().len;
    let new_size = old_size.max(end);
    let needed = ceil_blocks(new_size)?;
    if needed > inode::DIRECT_BLOCKS + inode::INDIRECT_BLOCKS {
        return Err(Ext2Error::FileTooLarge);
    }
    let mut direct = [0; 12];
    for (index, slot) in direct.iter_mut().enumerate() {
        *slot = get_direct(&image.bytes, index)?;
    }
    let mut indirect = get_direct(&image.bytes, 12)?;
    let mut indirect_data = [0; 1024];
    let mut indirect_present = indirect != 0;
    if indirect_present {
        read_indirect(fs, indirect, &mut indirect_data)?;
    }
    // Retain the exact on-media pointers before mutation so holey images
    // (zero pointers below size) are detected by pointer check, never by
    // size heuristic.
    let old_direct = direct;
    let old_indirect = get_direct(&image.bytes, 12)?;
    let old_indirect_data = indirect_data;
    let needs_indirect = needed > inode::DIRECT_BLOCKS;
    if needs_indirect && !indirect_present {
        let claimed = allocator::claim_block(fs)?;
        let zero = [0; BLOCK_BYTES];
        write_data_block(fs, claimed, &zero)?;
        indirect = claimed;
        indirect_present = true;
        indirect_data = [0; 1024];
    }
    // Claim missing data blocks in logical order.
    let mut indirect_dirty = needs_indirect && !indirect_present;
    for logical in 0..needed {
        let allocated = if logical < inode::DIRECT_BLOCKS {
            direct[usize::try_from(logical).map_err(|_| Ext2Error::CorruptMetadata {
                field: "logical_block",
            })?] != 0
        } else {
            let index = usize::try_from(logical - inode::DIRECT_BLOCKS).map_err(|_| {
                Ext2Error::CorruptMetadata {
                    field: "logical_block",
                }
            })?;
            indirect_data.get(index).is_some_and(|value| *value != 0)
        };
        if !allocated {
            let claimed = allocator::claim_block(fs)?;
            if logical < inode::DIRECT_BLOCKS {
                let index = usize::try_from(logical).map_err(|_| Ext2Error::CorruptMetadata {
                    field: "logical_block",
                })?;
                direct[index] = claimed;
            } else {
                let index = usize::try_from(logical - inode::DIRECT_BLOCKS).map_err(|_| {
                    Ext2Error::CorruptMetadata {
                        field: "logical_block",
                    }
                })?;
                if let Some(slot) = indirect_data.get_mut(index) {
                    *slot = claimed;
                } else {
                    return Err(Ext2Error::CorruptMetadata {
                        field: "logical_block",
                    });
                }
                indirect_dirty = true;
            }
        }
    }
    // Write data for new blocks (zero-padded) and overlapping existing blocks.
    let mut block_buf = [0; BLOCK_BYTES];
    for logical in 0..needed {
        let block_start = u64::from(logical).checked_mul(BLOCK_BYTES as u64).ok_or(
            Ext2Error::CorruptMetadata {
                field: "logical_block",
            },
        )?;
        let block_end =
            block_start
                .checked_add(BLOCK_BYTES as u64)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "logical_block",
                })?;
        if end <= block_start || offset >= block_end {
            // No overlap with the write range; if this block is newly
            // allocated as a gap, ensure it is zeroed on media.
            let is_new = old_pointer(&old_direct, old_indirect, &old_indirect_data, logical)? == 0;
            if is_new {
                let target = if logical < inode::DIRECT_BLOCKS {
                    direct[usize::try_from(logical).map_err(|_| Ext2Error::CorruptMetadata {
                        field: "logical_block",
                    })?]
                } else {
                    let index = usize::try_from(logical - inode::DIRECT_BLOCKS).map_err(|_| {
                        Ext2Error::CorruptMetadata {
                            field: "logical_block",
                        }
                    })?;
                    indirect_data[index]
                };
                let zero = [0; BLOCK_BYTES];
                write_data_block(fs, target, &zero)?;
            }
            continue;
        }
        let target = if logical < inode::DIRECT_BLOCKS {
            direct[usize::try_from(logical).map_err(|_| Ext2Error::CorruptMetadata {
                field: "logical_block",
            })?]
        } else {
            let index = usize::try_from(logical - inode::DIRECT_BLOCKS).map_err(|_| {
                Ext2Error::CorruptMetadata {
                    field: "logical_block",
                }
            })?;
            indirect_data[index]
        };
        if target == 0 {
            return Err(Ext2Error::CorruptMetadata {
                field: "block_pointer",
            });
        }
        // Determine if this block existed before (to decide read-modify-write).
        let existed = old_pointer(&old_direct, old_indirect, &old_indirect_data, logical)? != 0;
        if existed {
            read_data_block(fs, target, &mut block_buf)?;
        } else {
            block_buf = [0; BLOCK_BYTES];
        }
        let copy_start = offset.max(block_start) - block_start;
        let copy_end = end.min(block_end) - block_start;
        let dest_start = usize::try_from(copy_start).map_err(|_| Ext2Error::CorruptMetadata {
            field: "block_offset",
        })?;
        let dest_end = usize::try_from(copy_end).map_err(|_| Ext2Error::CorruptMetadata {
            field: "block_offset",
        })?;
        let src_start = offset.max(block_start);
        let src_offset = usize::try_from(src_start.checked_sub(offset).ok_or(
            Ext2Error::CorruptMetadata {
                field: "write_offset",
            },
        )?)
        .map_err(|_| Ext2Error::CorruptMetadata {
            field: "write_offset",
        })?;
        let length = dest_end
            .checked_sub(dest_start)
            .ok_or(Ext2Error::CorruptMetadata {
                field: "write_length",
            })?;
        let src_end = src_offset
            .checked_add(length)
            .ok_or(Ext2Error::CorruptMetadata {
                field: "write_length",
            })?;
        let src_slice = src
            .get(src_offset..src_end)
            .ok_or(Ext2Error::CorruptMetadata {
                field: "write_length",
            })?;
        let dest_slice =
            block_buf
                .get_mut(dest_start..dest_end)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "block_offset",
                })?;
        dest_slice.copy_from_slice(src_slice);
        write_data_block(fs, target, &block_buf)?;
    }
    if indirect_dirty {
        // Write the indirect block when it is new or gained pointers.
        write_indirect(fs, indirect, &indirect_data)?;
    }
    // Publish pointers, size, and block count into the inode image.
    let mut updated = image;
    for (index, value) in direct.iter().enumerate() {
        set_direct(&mut updated.bytes, index, *value)?;
    }
    set_direct(&mut updated.bytes, 12, indirect)?;
    let size_u32 = u32::try_from(new_size).map_err(|_| Ext2Error::CorruptMetadata {
        field: "file_length",
    })?;
    updated.bytes[on_disk::INODE_SIZE_LO..on_disk::INODE_SIZE_LO + 4]
        .copy_from_slice(&size_u32.to_le_bytes());
    let data_blocks: usize = direct.iter().filter(|value| **value != 0).count()
        + indirect_data.iter().filter(|value| **value != 0).count();
    let indirect_blocks = usize::from(indirect != 0);
    let total = data_blocks
        .checked_add(indirect_blocks)
        .and_then(|sum| sum.checked_mul(8))
        .ok_or(Ext2Error::CorruptMetadata {
            field: "inode_blocks",
        })?;
    let total_u32 = u32::try_from(total).map_err(|_| Ext2Error::CorruptMetadata {
        field: "inode_blocks",
    })?;
    updated.bytes[on_disk::INODE_BLOCKS..on_disk::INODE_BLOCKS + 4]
        .copy_from_slice(&total_u32.to_le_bytes());
    inode::store(fs, node, &updated)?;
    Ok(())
}

fn old_pointer(
    old_direct: &[u32; 12],
    old_indirect: u32,
    old_indirect_data: &[u32; 1024],
    logical: u32,
) -> Result<u32, Ext2Error> {
    // Exact on-media pointer before mutation; zero means hole (new).
    if logical < inode::DIRECT_BLOCKS {
        let index = usize::try_from(logical).map_err(|_| Ext2Error::CorruptMetadata {
            field: "logical_block",
        })?;
        return old_direct
            .get(index)
            .copied()
            .ok_or(Ext2Error::CorruptMetadata {
                field: "logical_block",
            });
    }
    if old_indirect == 0 {
        return Ok(0);
    }
    let index = usize::try_from(logical.checked_sub(inode::DIRECT_BLOCKS).ok_or(
        Ext2Error::CorruptMetadata {
            field: "logical_block",
        },
    )?)
    .map_err(|_| Ext2Error::CorruptMetadata {
        field: "logical_block",
    })?;
    old_indirect_data
        .get(index)
        .copied()
        .ok_or(Ext2Error::CorruptMetadata {
            field: "logical_block",
        })
}

pub(super) fn truncate<D: BlockDevice>(
    fs: &mut Ext2<D>,
    node: NodeId,
    len: u64,
) -> Result<(), Ext2Error> {
    fs.require_writable()?;
    if len > inode::MAX_FILE_BYTES {
        return Err(Ext2Error::FileTooLarge);
    }
    let image = inode::load(fs, node)?;
    if image.metadata().kind != NodeKind::Regular {
        return Err(Ext2Error::WrongNodeKind);
    }
    let old_size = image.metadata().len;
    if len == old_size {
        return Ok(());
    }
    if len > old_size {
        return grow(fs, node, &image, len);
    }
    shrink(fs, node, &image, len)
}

fn grow<D: BlockDevice>(
    fs: &mut Ext2<D>,
    node: NodeId,
    image: &inode::Inode,
    len: u64,
) -> Result<(), Ext2Error> {
    let needed = ceil_blocks(len)?;
    let mut direct = [0; 12];
    for (index, slot) in direct.iter_mut().enumerate() {
        *slot = get_direct(&image.bytes, index)?;
    }
    let mut indirect = get_direct(&image.bytes, 12)?;
    let mut indirect_data = [0; 1024];
    let was_present = indirect != 0;
    if was_present {
        read_indirect(fs, indirect, &mut indirect_data)?;
    }
    let mut indirect_dirty = false;
    if needed > inode::DIRECT_BLOCKS && !was_present {
        let claimed = allocator::claim_block(fs)?;
        let zero = [0; BLOCK_BYTES];
        write_data_block(fs, claimed, &zero)?;
        indirect = claimed;
        indirect_dirty = true;
    }
    for logical in 0..needed {
        let allocated = if logical < inode::DIRECT_BLOCKS {
            direct[usize::try_from(logical).map_err(|_| Ext2Error::CorruptMetadata {
                field: "logical_block",
            })?] != 0
        } else {
            let index = usize::try_from(logical - inode::DIRECT_BLOCKS).map_err(|_| {
                Ext2Error::CorruptMetadata {
                    field: "logical_block",
                }
            })?;
            indirect_data[index] != 0
        };
        if !allocated {
            let claimed = allocator::claim_block(fs)?;
            let zero = [0; BLOCK_BYTES];
            write_data_block(fs, claimed, &zero)?;
            if logical < inode::DIRECT_BLOCKS {
                let index = usize::try_from(logical).map_err(|_| Ext2Error::CorruptMetadata {
                    field: "logical_block",
                })?;
                direct[index] = claimed;
            } else {
                let index = usize::try_from(logical - inode::DIRECT_BLOCKS).map_err(|_| {
                    Ext2Error::CorruptMetadata {
                        field: "logical_block",
                    }
                })?;
                indirect_data[index] = claimed;
                indirect_dirty = true;
            }
        }
    }
    if indirect_dirty {
        write_indirect(fs, indirect, &indirect_data)?;
    }
    let mut updated = inode::Inode { bytes: image.bytes };
    for (index, value) in direct.iter().enumerate() {
        set_direct(&mut updated.bytes, index, *value)?;
    }
    set_direct(&mut updated.bytes, 12, indirect)?;
    let size_u32 = u32::try_from(len).map_err(|_| Ext2Error::CorruptMetadata {
        field: "file_length",
    })?;
    updated.bytes[on_disk::INODE_SIZE_LO..on_disk::INODE_SIZE_LO + 4]
        .copy_from_slice(&size_u32.to_le_bytes());
    let data_blocks: usize = direct.iter().filter(|value| **value != 0).count()
        + indirect_data.iter().filter(|value| **value != 0).count();
    let total = data_blocks
        .checked_add(usize::from(indirect != 0))
        .and_then(|sum| sum.checked_mul(8))
        .ok_or(Ext2Error::CorruptMetadata {
            field: "inode_blocks",
        })?;
    updated.bytes[on_disk::INODE_BLOCKS..on_disk::INODE_BLOCKS + 4].copy_from_slice(
        &u32::try_from(total)
            .map_err(|_| Ext2Error::CorruptMetadata {
                field: "inode_blocks",
            })?
            .to_le_bytes(),
    );
    inode::store(fs, node, &updated)?;
    Ok(())
}

fn shrink<D: BlockDevice>(
    fs: &mut Ext2<D>,
    node: NodeId,
    image: &inode::Inode,
    len: u64,
) -> Result<(), Ext2Error> {
    let keep = ceil_blocks(len)?;
    let mut direct = [0; 12];
    for (index, slot) in direct.iter_mut().enumerate() {
        *slot = get_direct(&image.bytes, index)?;
    }
    let indirect = get_direct(&image.bytes, 12)?;
    let mut indirect_data = [0; 1024];
    if indirect != 0 {
        read_indirect(fs, indirect, &mut indirect_data)?;
    }
    let mut freed_data = [0; 1036];
    let mut freed_count = 0;
    for logical in keep..ceil_blocks(image.metadata().len)? {
        let block = if logical < inode::DIRECT_BLOCKS {
            direct[usize::try_from(logical).map_err(|_| Ext2Error::CorruptMetadata {
                field: "logical_block",
            })?]
        } else {
            let index = usize::try_from(logical - inode::DIRECT_BLOCKS).map_err(|_| {
                Ext2Error::CorruptMetadata {
                    field: "logical_block",
                }
            })?;
            indirect_data[index]
        };
        if block != 0
            && let Some(slot) = freed_data.get_mut(freed_count)
        {
            *slot = block;
            freed_count += 1;
        }
    }
    let indirect_free = indirect != 0 && keep <= inode::DIRECT_BLOCKS;
    // Clear trailing direct pointers in the inode image; the inode store
    // below is the unreference flush and always comes first.
    let mut updated = inode::Inode { bytes: image.bytes };
    for logical in keep..inode::DIRECT_BLOCKS {
        let index = usize::try_from(logical).map_err(|_| Ext2Error::CorruptMetadata {
            field: "logical_block",
        })?;
        set_direct(&mut updated.bytes, index, 0)?;
    }
    if indirect_free {
        set_direct(&mut updated.bytes, 12, 0)?;
    }
    let size_u32 = u32::try_from(len).map_err(|_| Ext2Error::CorruptMetadata {
        field: "file_length",
    })?;
    updated.bytes[on_disk::INODE_SIZE_LO..on_disk::INODE_SIZE_LO + 4]
        .copy_from_slice(&size_u32.to_le_bytes());
    let remaining_indirect: usize = if keep > inode::DIRECT_BLOCKS && indirect != 0 {
        1
    } else {
        0
    };
    let kept_data: usize = {
        let mut count: usize = 0;
        for logical in 0..keep {
            let present = if logical < inode::DIRECT_BLOCKS {
                direct[usize::try_from(logical).map_err(|_| Ext2Error::CorruptMetadata {
                    field: "logical_block",
                })?] != 0
            } else {
                let index = usize::try_from(logical - inode::DIRECT_BLOCKS).map_err(|_| {
                    Ext2Error::CorruptMetadata {
                        field: "logical_block",
                    }
                })?;
                indirect_data[index] != 0
            };
            if present {
                count += 1;
            }
        }
        count
    };
    let total = kept_data
        .checked_add(remaining_indirect)
        .and_then(|sum| sum.checked_mul(8))
        .ok_or(Ext2Error::CorruptMetadata {
            field: "inode_blocks",
        })?;
    updated.bytes[on_disk::INODE_BLOCKS..on_disk::INODE_BLOCKS + 4].copy_from_slice(
        &u32::try_from(total)
            .map_err(|_| Ext2Error::CorruptMetadata {
                field: "inode_blocks",
            })?
            .to_le_bytes(),
    );
    inode::store(fs, node, &updated)?;
    if !indirect_free && indirect != 0 && keep > inode::DIRECT_BLOCKS {
        // Trim trailing indirect entries after the shrunk size is durable.
        let mut trimmed = indirect_data;
        for logical in keep..(inode::DIRECT_BLOCKS + inode::INDIRECT_BLOCKS) {
            let index = usize::try_from(logical.checked_sub(inode::DIRECT_BLOCKS).ok_or(
                Ext2Error::CorruptMetadata {
                    field: "logical_block",
                },
            )?)
            .map_err(|_| Ext2Error::CorruptMetadata {
                field: "logical_block",
            })?;
            trimmed[index] = 0;
        }
        write_indirect(fs, indirect, &trimmed)?;
    }
    for block in freed_data.iter().take(freed_count) {
        allocator::release_block(fs, *block)?;
    }
    if indirect_free {
        allocator::release_block(fs, indirect)?;
    }
    Ok(())
}

pub(super) fn unlink_file<D: BlockDevice>(
    fs: &mut Ext2<D>,
    parent: NodeId,
    name: &Name,
) -> Result<(), Ext2Error> {
    fs.require_writable()?;
    let child = directory::lookup(fs, parent, name)?;
    let child_image = inode::load(fs, child)?;
    if child_image.metadata().kind != NodeKind::Regular {
        return Err(Ext2Error::WrongNodeKind);
    }
    let removed = directory::remove(fs, parent, name)?;
    if removed != child {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_inode",
        });
    }
    let mut direct = [0; 12];
    for (index, slot) in direct.iter_mut().enumerate() {
        *slot = get_direct(&child_image.bytes, index)?;
    }
    let indirect = get_direct(&child_image.bytes, 12)?;
    let mut indirect_data = [0; 1024];
    if indirect != 0 {
        read_indirect(fs, indirect, &mut indirect_data)?;
    }
    let mut cleared = inode::Inode {
        bytes: child_image.bytes,
    };
    for index in 0..12 {
        set_direct(&mut cleared.bytes, index, 0)?;
    }
    set_direct(&mut cleared.bytes, 12, 0)?;
    cleared.bytes[on_disk::INODE_SIZE_LO..on_disk::INODE_SIZE_LO + 4]
        .copy_from_slice(&0u32.to_le_bytes());
    cleared.bytes[on_disk::INODE_BLOCKS..on_disk::INODE_BLOCKS + 4]
        .copy_from_slice(&0u32.to_le_bytes());
    // A freed inode must read as unlinked (links zero, mode zero; dtime stays
    // zero per the zeroed-times policy) so Linux fsck accepts the deletion.
    cleared.bytes[on_disk::INODE_LINKS..on_disk::INODE_LINKS + 2]
        .copy_from_slice(&0u16.to_le_bytes());
    cleared.bytes[on_disk::INODE_MODE..on_disk::INODE_MODE + 2]
        .copy_from_slice(&0u16.to_le_bytes());
    inode::store(fs, child, &cleared)?;
    for value in direct.iter().chain(indirect_data.iter()) {
        if *value != 0 {
            allocator::release_block(fs, *value)?;
        }
    }
    if indirect != 0 {
        allocator::release_block(fs, indirect)?;
    }
    allocator::release_inode(fs, child.0)?;
    Ok(())
}

pub(super) fn remove_dir<D: BlockDevice>(
    fs: &mut Ext2<D>,
    parent: NodeId,
    name: &Name,
) -> Result<(), Ext2Error> {
    fs.require_writable()?;
    let child = directory::lookup(fs, parent, name)?;
    if child == fs.root() {
        return Err(Ext2Error::WrongNodeKind);
    }
    let child_image = inode::load(fs, child)?;
    if child_image.metadata().kind != NodeKind::Directory {
        return Err(Ext2Error::WrongNodeKind);
    }
    if !directory::read_dir(fs, child)?.is_empty() {
        return Err(Ext2Error::DirectoryNotEmpty);
    }
    // Parent entry removal and flush come first.
    let removed = directory::remove(fs, parent, name)?;
    if removed != child {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_inode",
        });
    }
    let mut direct = [0; 12];
    for (index, slot) in direct.iter_mut().enumerate() {
        *slot = get_direct(&child_image.bytes, index)?;
    }
    let indirect = get_direct(&child_image.bytes, 12)?;
    let mut indirect_data = [0; 1024];
    if indirect != 0 {
        read_indirect(fs, indirect, &mut indirect_data)?;
    }
    let mut cleared = inode::Inode {
        bytes: child_image.bytes,
    };
    for index in 0..12 {
        set_direct(&mut cleared.bytes, index, 0)?;
    }
    set_direct(&mut cleared.bytes, 12, 0)?;
    cleared.bytes[on_disk::INODE_SIZE_LO..on_disk::INODE_SIZE_LO + 4]
        .copy_from_slice(&0u32.to_le_bytes());
    cleared.bytes[on_disk::INODE_BLOCKS..on_disk::INODE_BLOCKS + 4]
        .copy_from_slice(&0u32.to_le_bytes());
    // A freed inode must read as unlinked (links zero; no clock for dtime)
    // so Linux fsck accepts the deletion. The zeroed mode below returns the
    // record to pristine free-inode state instead of faking a timestamp.
    cleared.bytes[on_disk::INODE_LINKS..on_disk::INODE_LINKS + 2]
        .copy_from_slice(&0u16.to_le_bytes());
    cleared.bytes[on_disk::INODE_MODE..on_disk::INODE_MODE + 2]
        .copy_from_slice(&0u16.to_le_bytes());
    inode::store(fs, child, &cleared)?;
    for value in direct.iter().chain(indirect_data.iter()) {
        if *value != 0 {
            allocator::release_block(fs, *value)?;
        }
    }
    if indirect != 0 {
        allocator::release_block(fs, indirect)?;
    }
    allocator::release_inode(fs, child.0)?;
    adjust_used_dirs(fs, false)?;
    adjust_parent_links(fs, parent, false)?;
    Ok(())
}
