use crate::{
    block::BlockDevice,
    fs::{Metadata, NodeId, NodeKind},
};

use super::{
    Ext2, Ext2Error,
    on_disk::{self, BLOCK_BYTES},
};

const PROFILE_INODES: u32 = 4_096;
const INODE_BYTES: usize = 256;
const DIRECT_BLOCKS: u32 = 12;
const INDIRECT_BLOCKS: u32 = 1_024;
const MAX_FILE_BYTES: u64 = ((DIRECT_BLOCKS + INDIRECT_BLOCKS) as usize * BLOCK_BYTES) as u64;
const S_IFMT: u16 = 0xf000;
const S_IFREG: u16 = 0x8000;
const S_IFDIR: u16 = 0x4000;

pub(super) struct Inode {
    bytes: [u8; INODE_BYTES],
}

impl Inode {
    pub(super) fn metadata(&self) -> Result<Metadata, Ext2Error> {
        let mode = u16::from_le_bytes([
            self.bytes[on_disk::INODE_MODE],
            self.bytes[on_disk::INODE_MODE + 1],
        ]);
        let kind = match mode & S_IFMT {
            S_IFREG => NodeKind::Regular,
            S_IFDIR => NodeKind::Directory,
            // Note: load() validates first, so live load()->metadata() surfaces UnsupportedFile; this arm is defense-in-depth.
            _ => {
                return Err(Ext2Error::CorruptMetadata {
                    field: "inode_mode",
                });
            }
        };
        let len = u32::from_le_bytes([
            self.bytes[on_disk::INODE_SIZE_LO],
            self.bytes[on_disk::INODE_SIZE_LO + 1],
            self.bytes[on_disk::INODE_SIZE_LO + 2],
            self.bytes[on_disk::INODE_SIZE_LO + 3],
        ]);
        Ok(Metadata {
            kind,
            len: u64::from(len),
            mode: mode & !S_IFMT,
        })
    }

    fn pointer(&self, index: usize) -> Result<u32, Ext2Error> {
        on_disk::u32(
            &self.bytes,
            on_disk::INODE_BLOCK
                .checked_add(index.checked_mul(4).ok_or(Ext2Error::CorruptMetadata {
                    field: "inode_pointer",
                })?)
                .ok_or(Ext2Error::CorruptMetadata {
                    field: "inode_pointer",
                })?,
            "inode_pointer",
        )
    }
}

pub(super) fn load<D: BlockDevice>(fs: &mut Ext2<D>, node: NodeId) -> Result<Inode, Ext2Error> {
    if !(2..=PROFILE_INODES).contains(&node.0) {
        return Err(Ext2Error::InvalidNode);
    }
    let inode_offset = usize::try_from(node.0 - 1)
        .ok()
        .and_then(|index| index.checked_mul(INODE_BYTES))
        .ok_or(Ext2Error::CorruptMetadata {
            field: "inode_offset",
        })?;
    let table_block =
        u32::try_from(inode_offset / BLOCK_BYTES).map_err(|_| Ext2Error::CorruptMetadata {
            field: "inode_offset",
        })?;
    if table_block >= fs.geometry.inode_table_blocks {
        return Err(Ext2Error::CorruptMetadata {
            field: "inode_table",
        });
    }
    let block =
        fs.geometry
            .inode_table
            .checked_add(table_block)
            .ok_or(Ext2Error::CorruptMetadata {
                field: "inode_table",
            })?;
    let mut bytes = [0; BLOCK_BYTES];
    fs.read_block(block, &mut bytes)?;
    let start = inode_offset % BLOCK_BYTES;
    let end = start
        .checked_add(INODE_BYTES)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "inode_offset",
        })?;
    let record = bytes.get(start..end).ok_or(Ext2Error::CorruptMetadata {
        field: "inode_offset",
    })?;
    let mut inode = Inode {
        bytes: [0; INODE_BYTES],
    };
    inode.bytes.copy_from_slice(record);
    validate(&inode)?;
    Ok(inode)
}

pub(super) fn read_at<D: BlockDevice>(
    fs: &mut Ext2<D>,
    node: NodeId,
    offset: u64,
    dst: &mut [u8],
) -> Result<usize, Ext2Error> {
    if dst.is_empty() {
        return Ok(0);
    }
    let inode = load(fs, node)?;
    let metadata = inode.metadata()?;
    if metadata.kind != NodeKind::Regular {
        return Err(Ext2Error::WrongNodeKind);
    }
    if offset >= metadata.len {
        return Ok(0);
    }

    let requested = u64::try_from(dst.len()).map_err(|_| Ext2Error::CorruptMetadata {
        field: "read_length",
    })?;
    let available = metadata
        .len
        .checked_sub(offset)
        .ok_or(Ext2Error::CorruptMetadata {
            field: "read_offset",
        })?;
    let length =
        usize::try_from(available.min(requested)).map_err(|_| Ext2Error::CorruptMetadata {
            field: "read_length",
        })?;
    let mut copied = 0;
    let mut position = offset;
    let mut data = [0; BLOCK_BYTES];
    while copied < length {
        let logical_block = u32::try_from(position / BLOCK_BYTES as u64).map_err(|_| {
            Ext2Error::CorruptMetadata {
                field: "logical_block",
            }
        })?;
        let block_offset = usize::try_from(position % BLOCK_BYTES as u64).map_err(|_| {
            Ext2Error::CorruptMetadata {
                field: "block_offset",
            }
        })?;
        let block = resolve_block(fs, &inode, logical_block)?;
        fs.read_block(block, &mut data)?;
        let count = (length - copied).min(BLOCK_BYTES - block_offset);
        let end = copied
            .checked_add(count)
            .ok_or(Ext2Error::CorruptMetadata {
                field: "read_length",
            })?;
        dst.get_mut(copied..end)
            .ok_or(Ext2Error::CorruptMetadata {
                field: "read_length",
            })?
            .copy_from_slice(data.get(block_offset..block_offset + count).ok_or(
                Ext2Error::CorruptMetadata {
                    field: "block_offset",
                },
            )?);
        copied = end;
        position = position
            .checked_add(
                u64::try_from(count).map_err(|_| Ext2Error::CorruptMetadata {
                    field: "read_length",
                })?,
            )
            .ok_or(Ext2Error::CorruptMetadata {
                field: "read_offset",
            })?;
    }
    Ok(copied)
}

pub(super) fn resolve_block<D: BlockDevice>(
    fs: &mut Ext2<D>,
    inode: &Inode,
    logical_block: u32,
) -> Result<u32, Ext2Error> {
    let pointer = if logical_block < DIRECT_BLOCKS {
        inode.pointer(usize::try_from(logical_block).map_err(|_| {
            Ext2Error::CorruptMetadata {
                field: "logical_block",
            }
        })?)?
    } else if logical_block < DIRECT_BLOCKS + INDIRECT_BLOCKS {
        let indirect = inode.pointer(12)?;
        require_data_block(fs, indirect)?;
        let mut pointers = [0; BLOCK_BYTES];
        fs.read_block(indirect, &mut pointers)?;
        let index = usize::try_from(logical_block - DIRECT_BLOCKS).map_err(|_| {
            Ext2Error::CorruptMetadata {
                field: "logical_block",
            }
        })?;
        on_disk::u32(
            &pointers,
            index.checked_mul(4).ok_or(Ext2Error::CorruptMetadata {
                field: "indirect_pointer",
            })?,
            "indirect_pointer",
        )?
    } else {
        return Err(Ext2Error::UnsupportedFile);
    };
    require_data_block(fs, pointer)?;
    Ok(pointer)
}

fn validate(inode: &Inode) -> Result<(), Ext2Error> {
    let mode = on_disk::u16(&inode.bytes, on_disk::INODE_MODE, "inode_mode")?;
    if !matches!(mode & S_IFMT, S_IFREG | S_IFDIR) {
        return Err(Ext2Error::UnsupportedFile);
    }
    if on_disk::u32(&inode.bytes, on_disk::INODE_FLAGS, "inode_flags")? != 0 {
        return Err(Ext2Error::UnsupportedFile);
    }
    let len = u64::from(on_disk::u32(
        &inode.bytes,
        on_disk::INODE_SIZE_LO,
        "inode_size",
    )?);
    if mode & S_IFMT == S_IFREG
        && on_disk::u32(&inode.bytes, on_disk::INODE_SIZE_HIGH, "inode_size")? != 0
    {
        return Err(Ext2Error::UnsupportedFile);
    }
    if len > MAX_FILE_BYTES {
        return Err(Ext2Error::UnsupportedFile);
    }
    if inode.pointer(13)? != 0 || inode.pointer(14)? != 0 {
        return Err(Ext2Error::UnsupportedFile);
    }
    Ok(())
}

fn require_data_block<D: BlockDevice>(fs: &Ext2<D>, block: u32) -> Result<(), Ext2Error> {
    if block == 0 {
        return Err(Ext2Error::SparseFile);
    }
    if fs.geometry.is_structural_metadata_block(block) {
        return Err(Ext2Error::CorruptMetadata {
            field: "block_pointer",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::NodeKind;

    fn inode_with_mode(mode: u16) -> Inode {
        let mut bytes = [0; INODE_BYTES];
        bytes[on_disk::INODE_MODE..on_disk::INODE_MODE + 2].copy_from_slice(&mode.to_le_bytes());
        Inode { bytes }
    }

    #[test]
    fn metadata_rejects_bad_inode_mode_without_panicking() {
        for bad in [0x0000_u16, 0xa000_u16, 0x2000_u16, 0xffff_u16] {
            let inode = inode_with_mode(bad);
            assert_eq!(
                inode.metadata(),
                Err(Ext2Error::CorruptMetadata {
                    field: "inode_mode"
                }),
                "bad mode {bad:#06x} must return CorruptMetadata",
            );
        }
    }

    #[test]
    fn metadata_reports_regular_and_directory_kinds() {
        let regular = inode_with_mode(S_IFREG | 0o644);
        let metadata = regular.metadata().unwrap();
        assert_eq!(metadata.kind, NodeKind::Regular);
        assert_eq!(metadata.mode, 0o644);

        let dir = inode_with_mode(S_IFDIR | 0o755);
        let metadata = dir.metadata().unwrap();
        assert_eq!(metadata.kind, NodeKind::Directory);
        assert_eq!(metadata.mode, 0o755);
    }
}
