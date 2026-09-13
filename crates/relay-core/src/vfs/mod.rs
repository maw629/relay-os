mod path;

pub use path::{Cwd, ResolvedPath};

use crate::{
    block::BlockDevice,
    ext2::{Ext2, Ext2Error},
    fs::{DirEntry, Metadata, Name, NodeId, NodeKind},
};
use alloc::vec::Vec;

const MAX_PATH_BYTES: usize = 4096;
const READ_BUFFER_BYTES: usize = 4096;

pub trait FileSystem {
    fn root(&self) -> NodeId;
    fn metadata(&mut self, node: NodeId) -> Result<Metadata, FsError>;
    fn lookup(&mut self, dir: NodeId, name: &Name) -> Result<NodeId, FsError>;
    fn read_dir(&mut self, dir: NodeId) -> Result<Vec<DirEntry>, FsError>;
    fn read_at(&mut self, node: NodeId, offset: u64, dst: &mut [u8]) -> Result<usize, FsError>;
    fn write_at(&mut self, node: NodeId, offset: u64, src: &[u8]) -> Result<(), FsError> {
        let _ = (node, offset, src);
        Err(FsError::Unsupported)
    }
    fn truncate(&mut self, node: NodeId, len: u64) -> Result<(), FsError> {
        let _ = (node, len);
        Err(FsError::Unsupported)
    }
    fn create_file(&mut self, parent: NodeId, name: &Name) -> Result<NodeId, FsError> {
        let _ = (parent, name);
        Err(FsError::Unsupported)
    }
    fn create_dir(&mut self, parent: NodeId, name: &Name) -> Result<NodeId, FsError> {
        let _ = (parent, name);
        Err(FsError::Unsupported)
    }
    fn unlink_file(&mut self, parent: NodeId, name: &Name) -> Result<(), FsError> {
        let _ = (parent, name);
        Err(FsError::Unsupported)
    }
    fn remove_dir(&mut self, parent: NodeId, name: &Name) -> Result<(), FsError> {
        let _ = (parent, name);
        Err(FsError::Unsupported)
    }
    fn sync(&mut self) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }
    fn unmount(&mut self) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsError {
    NotFound,
    WrongNodeKind,
    Corrupt,
    Unsupported,
    Allocation,
    Io,
    FileTooLarge,
    WriteDisabled,
    ReadOnly,
    AlreadyExists,
    NotEmpty,
    NoSpace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfsError {
    Fs(FsError),
    InvalidPath,
    NotDirectory,
    NotRegularFile,
    Allocation,
}

pub struct Vfs<F> {
    filesystem: F,
}

impl<D: BlockDevice> FileSystem for Ext2<D> {
    fn root(&self) -> NodeId {
        self.root()
    }

    fn metadata(&mut self, node: NodeId) -> Result<Metadata, FsError> {
        self.metadata(node).map_err(map_ext2_error)
    }

    fn lookup(&mut self, dir: NodeId, name: &Name) -> Result<NodeId, FsError> {
        self.lookup(dir, name).map_err(map_ext2_error)
    }

    fn read_dir(&mut self, dir: NodeId) -> Result<Vec<DirEntry>, FsError> {
        self.read_dir(dir).map_err(map_ext2_error)
    }

    fn read_at(&mut self, node: NodeId, offset: u64, dst: &mut [u8]) -> Result<usize, FsError> {
        self.read_at(node, offset, dst).map_err(map_ext2_error)
    }

    fn write_at(&mut self, node: NodeId, offset: u64, src: &[u8]) -> Result<(), FsError> {
        self.write_at(node, offset, src).map_err(map_ext2_error)
    }

    fn truncate(&mut self, node: NodeId, len: u64) -> Result<(), FsError> {
        self.truncate(node, len).map_err(map_ext2_error)
    }

    fn create_file(&mut self, parent: NodeId, name: &Name) -> Result<NodeId, FsError> {
        self.create_file(parent, name).map_err(map_ext2_error)
    }

    fn create_dir(&mut self, parent: NodeId, name: &Name) -> Result<NodeId, FsError> {
        self.create_dir(parent, name).map_err(map_ext2_error)
    }

    fn unlink_file(&mut self, parent: NodeId, name: &Name) -> Result<(), FsError> {
        self.unlink_file(parent, name).map_err(map_ext2_error)
    }

    fn remove_dir(&mut self, parent: NodeId, name: &Name) -> Result<(), FsError> {
        self.remove_dir(parent, name).map_err(map_ext2_error)
    }

    fn sync(&mut self) -> Result<(), FsError> {
        self.sync().map_err(map_ext2_error)
    }

    fn unmount(&mut self) -> Result<(), FsError> {
        self.unmount().map_err(map_ext2_error)
    }
}

fn map_ext2_error(error: Ext2Error) -> FsError {
    match error {
        Ext2Error::NotFound => FsError::NotFound,
        Ext2Error::WrongNodeKind => FsError::WrongNodeKind,
        Ext2Error::Allocation => FsError::Allocation,
        Ext2Error::Block(_) => FsError::Io,
        Ext2Error::UnsupportedFile => FsError::Unsupported,
        Ext2Error::FileTooLarge => FsError::FileTooLarge,
        Ext2Error::WriteDisabled => FsError::WriteDisabled,
        Ext2Error::ReadOnly => FsError::ReadOnly,
        Ext2Error::AlreadyExists => FsError::AlreadyExists,
        Ext2Error::DirectoryNotEmpty => FsError::NotEmpty,
        Ext2Error::NoSpace => FsError::NoSpace,
        Ext2Error::UnsupportedSector { .. }
        | Ext2Error::UnsupportedProfile { .. }
        | Ext2Error::UnsupportedFeature { .. }
        | Ext2Error::MountRequiresCleanFilesystem
        | Ext2Error::CorruptMetadata { .. }
        | Ext2Error::InvalidNode
        | Ext2Error::SparseFile => FsError::Corrupt,
    }
}

impl<F: FileSystem> Vfs<F> {
    pub fn new(filesystem: F) -> Self {
        Self { filesystem }
    }

    pub fn initial_cwd(&self) -> Cwd {
        Cwd::root()
    }

    pub fn metadata(&mut self, node: NodeId) -> Result<Metadata, VfsError> {
        self.filesystem.metadata(node).map_err(VfsError::Fs)
    }

    pub fn cwd_path(&self, cwd: &Cwd) -> Result<Vec<u8>, VfsError> {
        render_cwd_path(cwd, |path, capacity| path.try_reserve_exact(capacity))
    }

    pub fn resolve(&mut self, cwd: &Cwd, path: &str) -> Result<ResolvedPath, VfsError> {
        let bytes = path.as_bytes();
        if bytes.len() > MAX_PATH_BYTES || !bytes.is_ascii() {
            return Err(VfsError::InvalidPath);
        }

        let absolute = bytes.first() == Some(&b'/');
        let mut components = if absolute {
            Vec::new()
        } else {
            clone_components(&cwd.components)?
        };
        let mut node = if absolute {
            self.filesystem.root()
        } else {
            self.resolve_components(&components)?
        };

        let mut raw_components = bytes.split(|&byte| byte == b'/').peekable();
        while let Some(component) = raw_components.next() {
            if component.is_empty() {
                continue;
            }
            if component == b"." {
                if raw_components
                    .clone()
                    .any(|component| !component.is_empty())
                    && self.filesystem.metadata(node).map_err(VfsError::Fs)?.kind
                        != NodeKind::Directory
                {
                    return Err(VfsError::NotDirectory);
                }
                continue;
            }
            if component == b".." {
                if self.filesystem.metadata(node).map_err(VfsError::Fs)?.kind != NodeKind::Directory
                {
                    return Err(VfsError::NotDirectory);
                }
                if components.pop().is_some() {
                    node = self.resolve_components(&components)?;
                }
                continue;
            }

            if self.filesystem.metadata(node).map_err(VfsError::Fs)?.kind != NodeKind::Directory {
                return Err(VfsError::NotDirectory);
            }
            if canonical_path_len_after_push(&components, component.len()) > MAX_PATH_BYTES {
                return Err(VfsError::InvalidPath);
            }
            let name = Name::new(component).map_err(map_name_error)?;
            node = self.filesystem.lookup(node, &name).map_err(VfsError::Fs)?;
            components
                .try_reserve(1)
                .map_err(|_| VfsError::Allocation)?;
            components.push(name);
        }

        if bytes.last() == Some(&b'/')
            && self.filesystem.metadata(node).map_err(VfsError::Fs)?.kind != NodeKind::Directory
        {
            return Err(VfsError::NotDirectory);
        }

        Ok(ResolvedPath { node, components })
    }

    pub fn change_dir(&mut self, cwd: &Cwd, path: &str) -> Result<Cwd, VfsError> {
        let resolved = self.resolve(cwd, path)?;
        if self
            .filesystem
            .metadata(resolved.node)
            .map_err(VfsError::Fs)?
            .kind
            != NodeKind::Directory
        {
            return Err(VfsError::NotDirectory);
        }
        Ok(Cwd {
            components: resolved.components,
        })
    }

    pub fn list(&mut self, cwd: &Cwd, path: Option<&str>) -> Result<Vec<DirEntry>, VfsError> {
        let resolved = match path {
            Some(path) => self.resolve(cwd, path)?,
            None => ResolvedPath {
                node: self.resolve_components(&cwd.components)?,
                components: clone_components(&cwd.components)?,
            },
        };
        if self
            .filesystem
            .metadata(resolved.node)
            .map_err(VfsError::Fs)?
            .kind
            != NodeKind::Directory
        {
            return Err(VfsError::NotDirectory);
        }
        self.filesystem
            .read_dir(resolved.node)
            .map_err(VfsError::Fs)
    }

    pub fn read_file(
        &mut self,
        cwd: &Cwd,
        path: &str,
        mut output: impl FnMut(&[u8]) -> Result<(), VfsError>,
    ) -> Result<(), VfsError> {
        let resolved = self.resolve(cwd, path)?;
        let metadata = self
            .filesystem
            .metadata(resolved.node)
            .map_err(VfsError::Fs)?;
        if metadata.kind != NodeKind::Regular {
            return Err(VfsError::NotRegularFile);
        }

        let mut buffer = [0; READ_BUFFER_BYTES];
        let mut offset = 0;
        while offset < metadata.len {
            let remaining = metadata.len - offset;
            let read_len = usize::try_from(remaining)
                .unwrap_or(READ_BUFFER_BYTES)
                .min(READ_BUFFER_BYTES);
            let read = self
                .filesystem
                .read_at(resolved.node, offset, &mut buffer[..read_len])
                .map_err(VfsError::Fs)?;
            if read > read_len {
                return Err(VfsError::Fs(FsError::Corrupt));
            }
            if read == 0 {
                break;
            }
            output(&buffer[..read])?;
            offset = offset
                .checked_add(u64::try_from(read).expect("buffer length fits u64"))
                .ok_or(VfsError::Fs(FsError::Corrupt))?;
        }
        Ok(())
    }

    fn resolve_components(&mut self, components: &[Name]) -> Result<NodeId, VfsError> {
        let mut node = self.filesystem.root();
        for component in components {
            if self.filesystem.metadata(node).map_err(VfsError::Fs)?.kind != NodeKind::Directory {
                return Err(VfsError::NotDirectory);
            }
            node = self
                .filesystem
                .lookup(node, component)
                .map_err(VfsError::Fs)?;
        }
        Ok(node)
    }
}

fn render_cwd_path<E>(
    cwd: &Cwd,
    reserve: impl FnOnce(&mut Vec<u8>, usize) -> Result<(), E>,
) -> Result<Vec<u8>, VfsError> {
    let mut path = Vec::new();
    reserve(&mut path, canonical_path_len(&cwd.components)).map_err(|_| VfsError::Allocation)?;
    path.push(b'/');
    for (index, component) in cwd.components.iter().enumerate() {
        if index != 0 {
            path.push(b'/');
        }
        path.extend_from_slice(component.as_bytes());
    }
    Ok(path)
}

fn clone_components(components: &[Name]) -> Result<Vec<Name>, VfsError> {
    let mut cloned = Vec::new();
    cloned
        .try_reserve_exact(components.len())
        .map_err(|_| VfsError::Allocation)?;
    cloned.extend_from_slice(components);
    Ok(cloned)
}

fn canonical_path_len(components: &[Name]) -> usize {
    1 + components
        .iter()
        .map(|component| component.as_bytes().len() + 1)
        .sum::<usize>()
        .saturating_sub(1)
}

fn canonical_path_len_after_push(components: &[Name], component_len: usize) -> usize {
    canonical_path_len(components) + usize::from(!components.is_empty()) + component_len
}

fn map_name_error(error: crate::fs::NameError) -> VfsError {
    match error {
        crate::fs::NameError::Allocation => VfsError::Allocation,
        crate::fs::NameError::Empty
        | crate::fs::NameError::TooLong
        | crate::fs::NameError::InvalidByte => VfsError::InvalidPath,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_rendering_maps_capacity_reservation_failure_to_allocation() {
        let cwd = Cwd {
            components: alloc::vec![Name::new(b"docs").unwrap()],
        };

        assert_eq!(
            render_cwd_path(&cwd, |_, _| Err(())),
            Err(VfsError::Allocation)
        );
    }
}
