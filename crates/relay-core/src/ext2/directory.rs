use crate::{
    block::BlockDevice,
    fs::{DirEntry, Name, NameError, NodeId, NodeKind},
};
use alloc::vec::Vec;

use super::{
    Ext2, Ext2Error, inode,
    on_disk::{self, BLOCK_BYTES},
};

const DIRECTORY_FILETYPE_REGULAR: u8 = 1;
const DIRECTORY_FILETYPE_DIRECTORY: u8 = 2;

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
