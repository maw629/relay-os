#![allow(dead_code)] // Individual integration-test crates use distinct fixture mutations.

use std::{
    env,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use super::file_device::FileDevice;

const SUPERBLOCK_OFFSET: usize = 1024;
const GROUP_DESCRIPTOR_OFFSET: usize = 4096;
const MAGIC_OFFSET: usize = 56;
const STATE_OFFSET: usize = 58;
const REVISION_OFFSET: usize = 76;
const FIRST_DATA_BLOCK_OFFSET: usize = 20;
const LOG_BLOCK_SIZE_OFFSET: usize = 24;
const LOG_FRAGMENT_SIZE_OFFSET: usize = 28;
const BLOCKS_COUNT_OFFSET: usize = 4;
const BLOCKS_PER_GROUP_OFFSET: usize = 32;
const FRAGMENTS_PER_GROUP_OFFSET: usize = 36;
const INODES_COUNT_OFFSET: usize = 0;
const INODES_PER_GROUP_OFFSET: usize = 40;
const INODE_SIZE_OFFSET: usize = 88;
const FEATURE_COMPAT_OFFSET: usize = 92;
const FEATURE_INCOMPAT_OFFSET: usize = 96;
const FEATURE_RO_COMPAT_OFFSET: usize = 100;
const BLOCK_BITMAP_OFFSET: usize = 0;
const INODE_BITMAP_OFFSET: usize = 4;
const INODE_TABLE_OFFSET: usize = 8;
const INODE_BYTES: usize = 256;
const INODE_MODE_OFFSET: usize = 0;
const INODE_FILE_SIZE_OFFSET: usize = 4;
const INODE_FLAGS_OFFSET: usize = 32;
const INODE_BLOCK_OFFSET: usize = 40;
const INODE_FILE_SIZE_HIGH_OFFSET: usize = 108;
const DIRECTORY_RECORD_LENGTH_OFFSET: usize = 4;
const DIRECTORY_INODE_OFFSET: usize = 0;
const DIRECTORY_FILE_TYPE_OFFSET: usize = 7;
const DIRECTORY_NAME_LENGTH_OFFSET: usize = 6;
const DIRECTORY_NAME_OFFSET: usize = 8;

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
pub enum FeatureField {
    Compatible,
    Incompatible,
    ReadOnlyCompatible,
}

#[derive(Debug)]
pub enum FixtureError {
    Io(std::io::Error),
    InvalidName,
    CommandFailed(String),
}

impl core::fmt::Display for FixtureError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "fixture I/O failed: {error}"),
            Self::InvalidName => formatter.write_str("fixture name is not a valid path component"),
            Self::CommandFailed(error) => write!(formatter, "fixture command failed: {error}"),
        }
    }
}

impl std::error::Error for FixtureError {}

impl From<std::io::Error> for FixtureError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl FeatureField {
    pub const ALL: [Self; 3] = [
        Self::Compatible,
        Self::Incompatible,
        Self::ReadOnlyCompatible,
    ];

    const fn offset(self) -> usize {
        match self {
            Self::Compatible => FEATURE_COMPAT_OFFSET,
            Self::Incompatible => FEATURE_INCOMPAT_OFFSET,
            Self::ReadOnlyCompatible => FEATURE_RO_COMPAT_OFFSET,
        }
    }

    const fn approved_mask(self) -> u32 {
        match self {
            Self::Incompatible => 2,
            Self::Compatible | Self::ReadOnlyCompatible => 0,
        }
    }
}

pub fn unsupported_feature_bits(field: FeatureField) -> impl Iterator<Item = u32> {
    (0..32).filter_map(move |index| {
        let bit = 1_u32 << index;
        (!field.approved_mask() & bit != 0).then_some(bit)
    })
}

pub struct Ext2Fixture {
    directory: PathBuf,
    image: PathBuf,
}

pub fn fixture_with_files(files: &[(&str, &[u8])]) -> Result<Ext2Fixture, FixtureError> {
    if files.iter().any(|(name, _)| !valid_component(name)) {
        return Err(FixtureError::InvalidName);
    }
    let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    let directory = env::temp_dir().join(format!("relay-core-ext2-{}-{id}", std::process::id()));
    fs::create_dir(&directory)?;
    let image = directory.join("root.ext2");
    File::create(&image)?.set_len(128 * 1024 * 1024)?;

    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("mke2fs")
        .current_dir(&repository)
        .args([
            "-F",
            "-t",
            "ext2",
            "-b",
            "4096",
            "-g",
            "32768",
            "-I",
            "256",
            "-N",
            "4096",
            "-m",
            "0",
            "-O",
            "none,filetype",
            "-E",
            "lazy_itable_init=0,nodiscard,root_owner=0:0",
            "-U",
            "52454c41-5900-4000-8000-000000000004",
        ])
        .arg(&image)
        .arg("32768")
        .env("MKE2FS_CONFIG", repository.join("tools/mke2fs.conf"))
        .env("E2FSPROGS_FAKE_TIME", "1788739200")
        .output()?;
    if !output.status.success() {
        return Err(FixtureError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }

    let output = Command::new("debugfs")
        .args(["-w", "-R", "rmdir /lost+found"])
        .arg(&image)
        .env("E2FSPROGS_FAKE_TIME", "1788739200")
        .output()?;
    if !output.status.success() {
        return Err(FixtureError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }

    for (name, contents) in files {
        let source = directory.join(format!("source-{name}"));
        fs::write(&source, contents)?;
        let output = Command::new("debugfs")
            .args(["-w", "-R"])
            .arg(format!(
                "write \"{}\" \"/{}\"",
                debugfs_string(&source.display().to_string()),
                debugfs_string(name)
            ))
            .arg(&image)
            .env("E2FSPROGS_FAKE_TIME", "1788739200")
            .output()?;
        if !output.status.success() {
            return Err(FixtureError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
    }

    Ok(Ext2Fixture { directory, image })
}

pub fn fixture_with_directory(
    directory: &str,
    file: &str,
    contents: &[u8],
) -> Result<Ext2Fixture, FixtureError> {
    if !valid_component(directory) || !valid_component(file) {
        return Err(FixtureError::InvalidName);
    }
    let fixture = fixture_with_files(&[])?;
    let output = Command::new("debugfs")
        .args(["-w", "-R"])
        .arg(format!("mkdir /{}", debugfs_string(directory)))
        .arg(&fixture.image)
        .env("E2FSPROGS_FAKE_TIME", "1788739200")
        .output()?;
    if !output.status.success() {
        return Err(FixtureError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }

    let source = fixture.directory.join(format!("source-{file}"));
    fs::write(&source, contents)?;
    let output = Command::new("debugfs")
        .args(["-w", "-R"])
        .arg(format!(
            "write \"{}\" \"/{}/{}\"",
            debugfs_string(&source.display().to_string()),
            debugfs_string(directory),
            debugfs_string(file)
        ))
        .arg(&fixture.image)
        .env("E2FSPROGS_FAKE_TIME", "1788739200")
        .output()?;
    if !output.status.success() {
        return Err(FixtureError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(fixture)
}

pub fn fixture_with_feature(field: FeatureField, bit: u32) -> Result<Ext2Fixture, FixtureError> {
    let mut fixture = fixture_with_files(&[])?;
    fixture.set_feature(field, field.approved_mask() | bit);
    Ok(fixture)
}

impl Ext2Fixture {
    pub fn path(&self) -> &Path {
        &self.image
    }

    pub fn open(&self) -> std::io::Result<FileDevice> {
        FileDevice::open(&self.image)
    }

    pub fn open_read_only(&self) -> std::io::Result<FileDevice> {
        FileDevice::open_read_only(&self.image)
    }

    pub fn contains_file(&self, name: &str) -> Result<bool, FixtureError> {
        if !valid_component(name) {
            return Err(FixtureError::InvalidName);
        }
        let output = Command::new("debugfs")
            .args(["-R"])
            .arg(format!("stat \"/{}\"", debugfs_string(name)))
            .arg(&self.image)
            .output()?;
        Ok(output.status.success())
    }

    pub fn mark_dirty(&mut self) {
        self.set_superblock_u16(STATE_OFFSET, 0);
    }

    pub fn mark_error(&mut self) {
        self.set_superblock_u16(STATE_OFFSET, 2);
    }

    pub fn set_magic(&mut self, value: u16) {
        self.set_superblock_u16(MAGIC_OFFSET, value);
    }

    pub fn set_revision(&mut self, value: u32) {
        self.set_superblock_u32(REVISION_OFFSET, value);
    }

    pub fn set_log_block_size(&mut self, value: u32) {
        self.set_superblock_u32(LOG_BLOCK_SIZE_OFFSET, value);
    }

    pub fn set_log_fragment_size(&mut self, value: u32) {
        self.set_superblock_u32(LOG_FRAGMENT_SIZE_OFFSET, value);
    }

    pub fn set_inode_size(&mut self, value: u16) {
        self.set_superblock_u16(INODE_SIZE_OFFSET, value);
    }

    pub fn set_block_count(&mut self, value: u32) {
        self.set_superblock_u32(BLOCKS_COUNT_OFFSET, value);
    }

    pub fn set_inode_count(&mut self, value: u32) {
        self.set_superblock_u32(INODES_COUNT_OFFSET, value);
    }

    pub fn set_blocks_per_group(&mut self, value: u32) {
        self.set_superblock_u32(BLOCKS_PER_GROUP_OFFSET, value);
    }

    pub fn set_fragments_per_group(&mut self, value: u32) {
        self.set_superblock_u32(FRAGMENTS_PER_GROUP_OFFSET, value);
    }

    pub fn set_inodes_per_group(&mut self, value: u32) {
        self.set_superblock_u32(INODES_PER_GROUP_OFFSET, value);
    }

    pub fn set_first_data_block(&mut self, value: u32) {
        self.set_superblock_u32(FIRST_DATA_BLOCK_OFFSET, value);
    }

    pub fn set_feature(&mut self, field: FeatureField, value: u32) {
        self.set_superblock_u32(field.offset(), value);
    }

    pub fn set_block_bitmap_block(&mut self, value: u32) {
        self.set_group_descriptor_u32(BLOCK_BITMAP_OFFSET, value);
    }

    pub fn set_inode_bitmap_block(&mut self, value: u32) {
        self.set_group_descriptor_u32(INODE_BITMAP_OFFSET, value);
    }

    pub fn set_inode_table_block(&mut self, value: u32) {
        self.set_group_descriptor_u32(INODE_TABLE_OFFSET, value);
    }

    pub fn clear_first_data_pointer(&mut self, name: &str) {
        self.with_named_inode_mut(name, |bytes, inode| {
            set_u32(bytes, inode + INODE_BLOCK_OFFSET, 0);
        });
    }

    pub fn set_first_data_pointer(&mut self, name: &str, value: u32) {
        self.with_named_inode_mut(name, |bytes, inode| {
            set_u32(bytes, inode + INODE_BLOCK_OFFSET, value);
        });
    }

    pub fn set_indirect_pointer(&mut self, name: &str, value: u32) {
        self.with_named_inode_mut(name, |bytes, inode| {
            set_u32(bytes, inode + INODE_BLOCK_OFFSET + 12 * 4, value);
        });
    }

    pub fn set_first_indirect_data_pointer(&mut self, name: &str, value: u32) {
        self.with_named_inode_mut(name, |bytes, inode| {
            let indirect = u32_at(bytes, inode + INODE_BLOCK_OFFSET + 12 * 4);
            let indirect = usize::try_from(indirect).unwrap();
            set_u32(bytes, indirect * 4096, value);
        });
    }

    pub fn structural_metadata_blocks(&self) -> Vec<u32> {
        self.with_bytes(|bytes| {
            let block_bitmap = u32_at(bytes, GROUP_DESCRIPTOR_OFFSET + BLOCK_BITMAP_OFFSET);
            let inode_bitmap = u32_at(bytes, GROUP_DESCRIPTOR_OFFSET + INODE_BITMAP_OFFSET);
            let inode_table = u32_at(bytes, GROUP_DESCRIPTOR_OFFSET + INODE_TABLE_OFFSET);
            vec![
                0,
                1,
                block_bitmap,
                inode_bitmap,
                inode_table,
                inode_table + 255,
            ]
        })
    }

    pub fn set_inode_file_size(&mut self, name: &str, value: u32) {
        self.with_named_inode_mut(name, |bytes, inode| {
            set_u32(bytes, inode + INODE_FILE_SIZE_OFFSET, value);
        });
    }

    pub fn set_inode_file_size_high(&mut self, name: &str, value: u32) {
        self.with_named_inode_mut(name, |bytes, inode| {
            set_u32(bytes, inode + INODE_FILE_SIZE_HIGH_OFFSET, value);
        });
    }

    pub fn set_inode_mode(&mut self, name: &str, value: u16) {
        self.with_named_inode_mut(name, |bytes, inode| {
            set_u16(bytes, inode + INODE_MODE_OFFSET, value);
        });
    }

    pub fn set_inode_flags(&mut self, name: &str, value: u32) {
        self.with_named_inode_mut(name, |bytes, inode| {
            set_u32(bytes, inode + INODE_FLAGS_OFFSET, value);
        });
    }

    pub fn set_double_indirect_pointer(&mut self, name: &str, value: u32) {
        self.with_named_inode_mut(name, |bytes, inode| {
            set_u32(bytes, inode + INODE_BLOCK_OFFSET + 13 * 4, value);
        });
    }

    pub fn set_triple_indirect_pointer(&mut self, name: &str, value: u32) {
        self.with_named_inode_mut(name, |bytes, inode| {
            set_u32(bytes, inode + INODE_BLOCK_OFFSET + 14 * 4, value);
        });
    }

    pub fn set_root_entry_inode(&mut self, name: &str, value: u32) {
        self.with_root_entry_mut(name, |bytes, entry| {
            set_u32(bytes, entry + DIRECTORY_INODE_OFFSET, value);
        });
    }

    pub fn set_root_entry_record_length(&mut self, name: &str, value: u16) {
        self.with_root_entry_mut(name, |bytes, entry| {
            set_u16(bytes, entry + DIRECTORY_RECORD_LENGTH_OFFSET, value);
        });
    }

    pub fn set_root_entry_file_type(&mut self, name: &str, value: u8) {
        self.with_root_entry_mut(name, |bytes, entry| {
            bytes[entry + DIRECTORY_FILE_TYPE_OFFSET] = value;
        });
    }

    pub fn set_root_entry_name_length(&mut self, name: &str, value: u8) {
        self.with_root_entry_mut(name, |bytes, entry| {
            bytes[entry + DIRECTORY_NAME_LENGTH_OFFSET] = value;
        });
    }

    pub fn set_root_entry_name_byte(&mut self, name: &str, offset: usize, value: u8) {
        self.with_root_entry_mut(name, |bytes, entry| {
            bytes[entry + DIRECTORY_NAME_OFFSET + offset] = value;
        });
    }

    pub fn rename_root_entry(&mut self, name: &str, replacement: &str) {
        assert_eq!(name.len(), replacement.len());
        self.with_root_entry_mut(name, |bytes, entry| {
            let start = entry + DIRECTORY_NAME_OFFSET;
            bytes[start..start + replacement.len()].copy_from_slice(replacement.as_bytes());
        });
    }

    fn set_superblock_u16(&mut self, offset: usize, value: u16) {
        self.with_bytes_mut(|bytes| {
            bytes[SUPERBLOCK_OFFSET + offset..SUPERBLOCK_OFFSET + offset + 2]
                .copy_from_slice(&value.to_le_bytes());
        });
    }

    fn set_superblock_u32(&mut self, offset: usize, value: u32) {
        self.with_bytes_mut(|bytes| {
            bytes[SUPERBLOCK_OFFSET + offset..SUPERBLOCK_OFFSET + offset + 4]
                .copy_from_slice(&value.to_le_bytes());
        });
    }

    fn set_group_descriptor_u32(&mut self, offset: usize, value: u32) {
        self.with_bytes_mut(|bytes| {
            bytes[GROUP_DESCRIPTOR_OFFSET + offset..GROUP_DESCRIPTOR_OFFSET + offset + 4]
                .copy_from_slice(&value.to_le_bytes());
        });
    }

    fn with_bytes_mut(&mut self, mutate: impl FnOnce(&mut [u8])) {
        let mut file = File::options()
            .read(true)
            .write(true)
            .open(&self.image)
            .unwrap();
        let len = usize::try_from(file.metadata().unwrap().len()).unwrap();
        let mut bytes = vec![0; len];
        file.read_exact(&mut bytes).unwrap();
        mutate(&mut bytes);
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn with_bytes<T>(&self, inspect: impl FnOnce(&[u8]) -> T) -> T {
        let mut file = File::open(&self.image).unwrap();
        let len = usize::try_from(file.metadata().unwrap().len()).unwrap();
        let mut bytes = vec![0; len];
        file.read_exact(&mut bytes).unwrap();
        inspect(&bytes)
    }

    fn with_named_inode_mut(&mut self, name: &str, mutate: impl FnOnce(&mut [u8], usize)) {
        self.with_bytes_mut(|bytes| {
            let entry = root_entry(bytes, name);
            let inode = inode_offset(bytes, u32_at(bytes, entry + DIRECTORY_INODE_OFFSET));
            mutate(bytes, inode);
        });
    }

    fn with_root_entry_mut(&mut self, name: &str, mutate: impl FnOnce(&mut [u8], usize)) {
        self.with_bytes_mut(|bytes| mutate(bytes, root_entry(bytes, name)));
    }
}

fn root_entry(bytes: &[u8], wanted: &str) -> usize {
    let root = inode_offset(bytes, 2);
    let data_block = usize::try_from(u32_at(bytes, root + INODE_BLOCK_OFFSET)).unwrap();
    let mut offset = data_block * 4096;
    let end = offset + 4096;
    while offset < end {
        let length = usize::from(u16_at(bytes, offset + DIRECTORY_RECORD_LENGTH_OFFSET));
        let name_length = usize::from(bytes[offset + DIRECTORY_NAME_LENGTH_OFFSET]);
        if &bytes[offset + DIRECTORY_NAME_OFFSET..offset + DIRECTORY_NAME_OFFSET + name_length]
            == wanted.as_bytes()
        {
            return offset;
        }
        offset += length;
    }
    panic!("fixture root entry does not exist: {wanted}");
}

fn inode_offset(bytes: &[u8], inode: u32) -> usize {
    let table =
        usize::try_from(u32_at(bytes, GROUP_DESCRIPTOR_OFFSET + INODE_TABLE_OFFSET)).unwrap();
    table * 4096 + usize::try_from(inode - 1).unwrap() * INODE_BYTES
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn set_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn valid_component(name: &str) -> bool {
    name.len() <= 255
        && !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| (b' '..=b'~').contains(&byte) && byte != b'/')
}

fn debugfs_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

impl Drop for Ext2Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
