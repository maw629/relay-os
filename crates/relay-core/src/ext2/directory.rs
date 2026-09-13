use crate::{
    block::BlockDevice,
    fs::{DirEntry, Name, NameError, NodeId, NodeKind},
};
use alloc::vec::Vec;

use super::{
    Ext2, Ext2Error, allocator, inode,
    on_disk::{self, BLOCK_BYTES},
};

const DIRECTORY_FILETYPE_REGULAR: u8 = 1;
const DIRECTORY_FILETYPE_DIRECTORY: u8 = 2;

pub(super) fn insert<D: BlockDevice>(
    fs: &mut Ext2<D>,
    parent: NodeId,
    name: &Name,
    child: NodeId,
    kind: NodeKind,
) -> Result<(), Ext2Error> {
    fs.require_writable()?;
    let parent_inode = inode::load(fs, parent)?;
    if parent_inode.metadata().kind != NodeKind::Directory {
        return Err(Ext2Error::WrongNodeKind);
    }
    let name_bytes = name.as_bytes();
    if name_bytes.is_empty() || name_bytes.len() > 255 {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_name",
        });
    }
    let needed = name_bytes
        .len()
        .checked_add(8)
        .and_then(|sum| sum.checked_add(3))
        .map(|sum| sum & !3)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "directory_name",
        })?;
    let file_type = match kind {
        NodeKind::Regular => DIRECTORY_FILETYPE_REGULAR,
        NodeKind::Directory => DIRECTORY_FILETYPE_DIRECTORY,
    };
    let parent_meta = parent_inode.metadata();
    let blocks = u32::try_from(parent_meta.len.div_ceil(BLOCK_BYTES as u64)).map_err(|_| {
        Ext2Error::CorruptMetadata {
            field: "directory_length",
        }
    })?;
    let mut block_bytes = [0; BLOCK_BYTES];
    for logical in 0..blocks {
        let block = match inode::resolve_block(fs, &parent_inode, logical) {
            Ok(block) => block,
            Err(Ext2Error::SparseFile) => {
                return Err(Ext2Error::CorruptMetadata {
                    field: "directory_block",
                });
            }
            Err(error) => return Err(error),
        };
        fs.read_block(block, &mut block_bytes)
            .inspect_err(|_| fs.poison())?;
        let limit = dir_block_limit(parent_meta.len, logical, blocks)?;
        // Duplicate check plus split search in one pass.
        let mut offset = 0;
        while offset < limit {
            let record = parse_record(&block_bytes, offset, limit)?;
            if record.inode != 0
                && let Some((_, Some(existing), _)) = record.live()?
                && existing.as_bytes() == name_bytes
            {
                return Err(Ext2Error::AlreadyExists);
            }
            offset = offset
                .checked_add(record.length)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "directory_record_length",
                })?;
        }
        // Second pass for insertion slot.
        let mut offset = 0;
        while offset < limit {
            let (rec_len, rec_inode, rec_name_len) = {
                let record = parse_record(&block_bytes, offset, limit)?;
                (record.length, record.inode, record.name_length)
            };
            if rec_inode != 0 {
                let actual = rec_name_len
                    .checked_add(8)
                    .and_then(|sum| sum.checked_add(3))
                    .map(|sum| sum & !3)
                    .ok_or(Ext2Error::CorruptMetadata {
                        field: "directory_name",
                    })?;
                if rec_len >= actual {
                    let slack = rec_len
                        .checked_sub(actual)
                        .ok_or(Ext2Error::CorruptMetadata {
                            field: "directory_record_length",
                        })?;
                    if slack >= needed {
                        let new_offset =
                            offset
                                .checked_add(actual)
                                .ok_or(Ext2Error::CorruptMetadata {
                                    field: "directory_record_length",
                                })?;
                        let old_len = rec_len;
                        // Shrink predecessor.
                        write_u16(
                            &mut block_bytes,
                            offset + on_disk::DIRECTORY_RECORD_LENGTH,
                            u16::try_from(actual).map_err(|_| Ext2Error::CorruptMetadata {
                                field: "directory_record_length",
                            })?,
                        )?;
                        // Write new record.
                        write_entry(
                            &mut block_bytes,
                            new_offset,
                            old_len - actual,
                            child.0,
                            file_type,
                            name_bytes,
                        )?;
                        let owned = block_bytes;
                        fs.write_block(block, &owned)?;
                        flush(fs)?;
                        return Ok(());
                    }
                }
            } else if rec_len >= needed {
                // Reuse deleted slot; split remainder if large enough.
                let remainder = rec_len
                    .checked_sub(needed)
                    .ok_or(Ext2Error::CorruptMetadata {
                        field: "directory_record_length",
                    })?;
                write_entry(
                    &mut block_bytes,
                    offset,
                    needed,
                    child.0,
                    file_type,
                    name_bytes,
                )?;
                if remainder >= 8 && remainder.is_multiple_of(4) {
                    let next = offset
                        .checked_add(needed)
                        .ok_or(Ext2Error::CorruptMetadata {
                            field: "directory_record_length",
                        })?;
                    // Mark remainder as deleted with valid shape.
                    write_u32(&mut block_bytes, next + on_disk::DIRECTORY_INODE, 0)?;
                    write_u16(
                        &mut block_bytes,
                        next + on_disk::DIRECTORY_RECORD_LENGTH,
                        u16::try_from(remainder).map_err(|_| Ext2Error::CorruptMetadata {
                            field: "directory_record_length",
                        })?,
                    )?;
                    if let Some(slot) = block_bytes.get_mut(next + on_disk::DIRECTORY_NAME_LENGTH) {
                        *slot = 0;
                    }
                    if let Some(slot) = block_bytes.get_mut(next + on_disk::DIRECTORY_FILE_TYPE) {
                        *slot = 0;
                    }
                } else if remainder > 0 {
                    // Use whole slot when remainder cannot hold a valid record.
                    write_u16(
                        &mut block_bytes,
                        offset + on_disk::DIRECTORY_RECORD_LENGTH,
                        u16::try_from(rec_len).map_err(|_| Ext2Error::CorruptMetadata {
                            field: "directory_record_length",
                        })?,
                    )?;
                }
                let owned = block_bytes;
                fs.write_block(block, &owned)?;
                flush(fs)?;
                return Ok(());
            }
            offset = offset
                .checked_add(rec_len)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "directory_record_length",
                })?;
        }
    }
    // No space: allocate a new directory block.
    let new_block = allocator::claim_block(fs)?;
    // Write an intermediate valid empty block so a crash before the parent
    // grows leaves valid media (single deleted record).
    let mut fresh = [0; BLOCK_BYTES];
    write_u32(&mut fresh, on_disk::DIRECTORY_INODE, 0)?;
    write_u16(
        &mut fresh,
        on_disk::DIRECTORY_RECORD_LENGTH,
        u16::try_from(BLOCK_BYTES).map_err(|_| Ext2Error::CorruptMetadata {
            field: "directory_record_length",
        })?,
    )?;
    let empty = fresh;
    fs.write_block(new_block, &empty)?;
    flush(fs)?;
    // Grow the parent size and block count.
    let mut parent_image = inode::load(fs, parent)?;
    let old_len = parent_image.metadata().len;
    let new_len = old_len
        .checked_add(BLOCK_BYTES as u64)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "directory_length",
        })?;
    let old_blocks = on_disk::u32(&parent_image.bytes, on_disk::INODE_BLOCKS, "inode_blocks")?;
    let new_blocks = old_blocks
        .checked_add(8)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "inode_blocks",
        })?;
    write_u32(
        &mut parent_image.bytes,
        on_disk::INODE_SIZE_LO,
        u32::try_from(new_len).map_err(|_| Ext2Error::CorruptMetadata {
            field: "directory_length",
        })?,
    )?;
    write_u32(&mut parent_image.bytes, on_disk::INODE_BLOCKS, new_blocks)?;
    // Publish the new data pointer before the entry becomes reachable.
    // Resolve the first zero direct slot.
    let mut placed = false;
    for index in 0..12 {
        let offset =
            on_disk::INODE_BLOCK
                .checked_add(index * 4)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "inode_pointer",
                })?;
        if on_disk::u32(&parent_image.bytes, offset, "inode_pointer")? == 0 {
            write_u32(&mut parent_image.bytes, offset, new_block)?;
            placed = true;
            break;
        }
    }
    if !placed {
        // Task 9 scope: directories grow through direct blocks only (no
        // indirect directory growth). Revisit NoSpace-vs-Corrupt only if a
        // later task needs it.
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_block",
        });
    }
    inode::store(fs, parent, &parent_image)?;
    // Parent entry write is last on create.
    let mut filled = [0; BLOCK_BYTES];
    write_entry(&mut filled, 0, BLOCK_BYTES, child.0, file_type, name_bytes)?;
    fs.write_block(new_block, &filled)?;
    flush(fs)?;
    Ok(())
}

pub(super) fn remove<D: BlockDevice>(
    fs: &mut Ext2<D>,
    parent: NodeId,
    name: &Name,
) -> Result<NodeId, Ext2Error> {
    fs.require_writable()?;
    let parent_inode = inode::load(fs, parent)?;
    if parent_inode.metadata().kind != NodeKind::Directory {
        return Err(Ext2Error::WrongNodeKind);
    }
    let wanted = name.as_bytes();
    let parent_meta = parent_inode.metadata();
    let blocks = u32::try_from(parent_meta.len.div_ceil(BLOCK_BYTES as u64)).map_err(|_| {
        Ext2Error::CorruptMetadata {
            field: "directory_length",
        }
    })?;
    let mut block_bytes = [0; BLOCK_BYTES];
    for logical in 0..blocks {
        let block = match inode::resolve_block(fs, &parent_inode, logical) {
            Ok(block) => block,
            Err(Ext2Error::SparseFile) => {
                return Err(Ext2Error::CorruptMetadata {
                    field: "directory_block",
                });
            }
            Err(error) => return Err(error),
        };
        fs.read_block(block, &mut block_bytes)
            .inspect_err(|_| fs.poison())?;
        let limit = dir_block_limit(parent_meta.len, logical, blocks)?;
        let mut offset = 0;
        let mut previous: Option<usize> = None;
        while offset < limit {
            let record = parse_record(&block_bytes, offset, limit)?;
            let is_target = if record.inode == 0 {
                false
            } else {
                match record.live()? {
                    Some((_, Some(existing), _)) => existing.as_bytes() == wanted,
                    _ => false,
                }
            };
            if is_target {
                let child = NodeId(record.inode);
                let target_len = record.length;
                if let Some(previous_offset) = previous {
                    let previous_len = usize::from(on_disk::u16(
                        &block_bytes,
                        previous_offset + on_disk::DIRECTORY_RECORD_LENGTH,
                        "directory_record_length",
                    )?);
                    let merged =
                        previous_len
                            .checked_add(target_len)
                            .ok_or(Ext2Error::CorruptMetadata {
                                field: "directory_record_length",
                            })?;
                    write_u16(
                        &mut block_bytes,
                        previous_offset + on_disk::DIRECTORY_RECORD_LENGTH,
                        u16::try_from(merged).map_err(|_| Ext2Error::CorruptMetadata {
                            field: "directory_record_length",
                        })?,
                    )?;
                } else {
                    write_u32(&mut block_bytes, offset + on_disk::DIRECTORY_INODE, 0)?;
                }
                let owned = block_bytes;
                fs.write_block(block, &owned)?;
                flush(fs)?;
                // Flush the parent inode so removal persists before the
                // caller frees the child.
                let parent_refresh = inode::load(fs, parent)?;
                inode::store(fs, parent, &parent_refresh)?;
                return Ok(child);
            }
            previous = Some(offset);
            offset = offset
                .checked_add(record.length)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "directory_record_length",
                })?;
        }
    }
    Err(Ext2Error::NotFound)
}

fn dir_block_limit(len: u64, logical: u32, blocks: u32) -> Result<usize, Ext2Error> {
    if blocks == 0 {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_length",
        });
    }
    if logical + 1 == blocks {
        let remainder =
            usize::try_from(len % BLOCK_BYTES as u64).map_err(|_| Ext2Error::CorruptMetadata {
                field: "directory_length",
            })?;
        if remainder == 0 {
            Ok(BLOCK_BYTES)
        } else {
            Ok(remainder)
        }
    } else {
        Ok(BLOCK_BYTES)
    }
}

fn flush<D: BlockDevice>(fs: &mut Ext2<D>) -> Result<(), Ext2Error> {
    fs.device.flush().map_err(|error| {
        fs.poison();
        Ext2Error::Block(error)
    })
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) -> Result<(), Ext2Error> {
    let end = offset.checked_add(2).ok_or(Ext2Error::CorruptMetadata {
        field: "directory_record_length",
    })?;
    let slot = bytes
        .get_mut(offset..end)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "directory_record_length",
        })?;
    slot.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), Ext2Error> {
    let end = offset.checked_add(4).ok_or(Ext2Error::CorruptMetadata {
        field: "directory_inode",
    })?;
    let slot = bytes
        .get_mut(offset..end)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "directory_inode",
        })?;
    slot.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_entry(
    bytes: &mut [u8],
    offset: usize,
    rec_len: usize,
    child: u32,
    file_type: u8,
    name: &[u8],
) -> Result<(), Ext2Error> {
    write_u32(bytes, offset + on_disk::DIRECTORY_INODE, child)?;
    write_u16(
        bytes,
        offset + on_disk::DIRECTORY_RECORD_LENGTH,
        u16::try_from(rec_len).map_err(|_| Ext2Error::CorruptMetadata {
            field: "directory_record_length",
        })?,
    )?;
    let name_len = u8::try_from(name.len()).map_err(|_| Ext2Error::CorruptMetadata {
        field: "directory_name",
    })?;
    if let Some(slot) = bytes.get_mut(offset + on_disk::DIRECTORY_NAME_LENGTH) {
        *slot = name_len;
    } else {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_name",
        });
    }
    if let Some(slot) = bytes.get_mut(offset + on_disk::DIRECTORY_FILE_TYPE) {
        *slot = file_type;
    } else {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_file_type",
        });
    }
    let end = offset
        .checked_add(on_disk::DIRECTORY_NAME + name.len())
        .ok_or(Ext2Error::CorruptMetadata {
            field: "directory_name",
        })?;
    let limit = offset
        .checked_add(rec_len)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "directory_record_length",
        })?;
    if end > limit {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_name",
        });
    }
    let slot =
        bytes
            .get_mut(offset + on_disk::DIRECTORY_NAME..end)
            .ok_or(Ext2Error::CorruptMetadata {
                field: "directory_name",
            })?;
    slot.copy_from_slice(name);
    // Zero padding.
    if let Some(padding) = bytes.get_mut(end..limit) {
        padding.fill(0);
    } else {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_record_length",
        });
    }
    Ok(())
}

pub(super) fn lookup<D: BlockDevice>(
    fs: &mut Ext2<D>,
    dir: NodeId,
    wanted: &Name,
) -> Result<NodeId, Ext2Error> {
    let mut found = None;
    visit(fs, dir, |node, name, _| {
        if found.is_none() && name.is_some_and(|name| name.as_bytes() == wanted.as_bytes()) {
            found = Some(node);
        }
        Ok(())
    })?;
    found.ok_or(Ext2Error::NotFound)
}

pub(super) fn read_dir<D: BlockDevice>(
    fs: &mut Ext2<D>,
    dir: NodeId,
) -> Result<Vec<DirEntry>, Ext2Error> {
    let mut entries = Vec::new();
    visit(fs, dir, |node, name, kind| {
        let Some(name) = name else {
            return Ok(());
        };
        entries.try_reserve(1).map_err(|_| Ext2Error::Allocation)?;
        entries.push(DirEntry { name, node, kind });
        Ok(())
    })?;
    Ok(entries)
}

fn visit<D: BlockDevice>(
    fs: &mut Ext2<D>,
    dir: NodeId,
    mut visitor: impl FnMut(NodeId, Option<Name>, NodeKind) -> Result<(), Ext2Error>,
) -> Result<(), Ext2Error> {
    let inode = inode::load(fs, dir)?;
    let metadata = inode.metadata()?;
    if metadata.kind != NodeKind::Directory {
        return Err(Ext2Error::WrongNodeKind);
    }
    let blocks = u32::try_from(metadata.len.div_ceil(BLOCK_BYTES as u64)).map_err(|_| {
        Ext2Error::CorruptMetadata {
            field: "directory_length",
        }
    })?;
    let mut bytes = [0; BLOCK_BYTES];
    for logical_block in 0..blocks {
        let block = inode::resolve_block(fs, &inode, logical_block)?;
        fs.read_block(block, &mut bytes)?;
        let limit = if logical_block + 1 == blocks {
            usize::try_from(metadata.len % BLOCK_BYTES as u64).map_err(|_| {
                Ext2Error::CorruptMetadata {
                    field: "directory_length",
                }
            })?
        } else {
            BLOCK_BYTES
        };
        let limit = if limit == 0 { BLOCK_BYTES } else { limit };
        let mut offset = 0;
        while offset < limit {
            let record = parse_record(&bytes, offset, limit)?;
            offset = offset
                .checked_add(record.length)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "directory_record_length",
                })?;
            let Some((node, name, kind)) = record.live()? else {
                continue;
            };
            let child = inode::load(fs, node)?;
            if child.metadata()?.kind != kind {
                return Err(Ext2Error::CorruptMetadata {
                    field: "directory_file_type",
                });
            }
            visitor(node, name, kind)?;
        }
    }
    Ok(())
}

struct Record<'a> {
    bytes: &'a [u8],
    inode: u32,
    length: usize,
    name_length: usize,
    file_type: u8,
}

impl Record<'_> {
    fn live(&self) -> Result<Option<(NodeId, Option<Name>, NodeKind)>, Ext2Error> {
        if self.inode == 0 {
            return Ok(None);
        }
        let kind = match self.file_type {
            DIRECTORY_FILETYPE_REGULAR => NodeKind::Regular,
            DIRECTORY_FILETYPE_DIRECTORY => NodeKind::Directory,
            _ => {
                return Err(Ext2Error::CorruptMetadata {
                    field: "directory_file_type",
                });
            }
        };
        let name = self
            .bytes
            .get(on_disk::DIRECTORY_NAME..on_disk::DIRECTORY_NAME + self.name_length)
            .ok_or(Ext2Error::CorruptMetadata {
                field: "directory_name",
            })?;
        let name = if name == b"." || name == b".." {
            None
        } else {
            Some(match Name::new(name) {
                Ok(name) => name,
                Err(NameError::Allocation) => return Err(Ext2Error::Allocation),
                Err(_) => {
                    return Err(Ext2Error::CorruptMetadata {
                        field: "directory_name",
                    });
                }
            })
        };
        if !(2..=4_096).contains(&self.inode) {
            return Err(Ext2Error::CorruptMetadata {
                field: "directory_inode",
            });
        }
        Ok(Some((NodeId(self.inode), name, kind)))
    }
}

fn parse_record<'a>(
    bytes: &'a [u8; BLOCK_BYTES],
    offset: usize,
    limit: usize,
) -> Result<Record<'a>, Ext2Error> {
    let header_end =
        offset
            .checked_add(on_disk::DIRECTORY_NAME)
            .ok_or(Ext2Error::CorruptMetadata {
                field: "directory_record_length",
            })?;
    if header_end > limit {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_record_length",
        });
    }
    let length = usize::from(on_disk::u16(
        bytes,
        offset + on_disk::DIRECTORY_RECORD_LENGTH,
        "directory_record_length",
    )?);
    if length < on_disk::DIRECTORY_NAME || !length.is_multiple_of(4) || length > limit - offset {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_record_length",
        });
    }
    let name_length = usize::from(*bytes.get(offset + on_disk::DIRECTORY_NAME_LENGTH).ok_or(
        Ext2Error::CorruptMetadata {
            field: "directory_name_length",
        },
    )?);
    if name_length > length - on_disk::DIRECTORY_NAME {
        return Err(Ext2Error::CorruptMetadata {
            field: "directory_name_length",
        });
    }
    Ok(Record {
        bytes: bytes
            .get(offset..offset + length)
            .ok_or(Ext2Error::CorruptMetadata {
                field: "directory_record_length",
            })?,
        inode: on_disk::u32(bytes, offset + on_disk::DIRECTORY_INODE, "directory_inode")?,
        length,
        name_length,
        file_type: *bytes.get(offset + on_disk::DIRECTORY_FILE_TYPE).ok_or(
            Ext2Error::CorruptMetadata {
                field: "directory_file_type",
            },
        )?,
    })
}
