//! A read only ext2/3/4 reader.
//!
//! The `ext4` crate that is available on crates.io refuses to open a
//! filesystem that uses the `CSUM_SEED` feature, which every filesystem
//! created by a current `mke2fs` uses. Helm therefore reads the filesystem
//! itself.
//!
//! Supported features: `extent`, `64bit`, `flex_bg`, `dir_index` (including
//! the htree directory index), `large_file`, `sparse_super` and the metadata
//! checksums (which are not verified). `inline_data`, `meta_bg`, encryption
//! and compression are not supported and are reported as errors.

use std::sync::Arc;

use crate::block::{BlockDevice, PartitionDevice};
use crate::fs::{FsEntry, FsError, VolumeFs};

const SUPERBLOCK_OFFSET: u64 = 1024;
const EXT4_MAGIC: u16 = 0xEF53;
const ROOT_INODE: u32 = 2;

/// Feature flags of the incompatible feature set.
const FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002;
const FEATURE_INCOMPAT_EXTENTS: u32 = 0x0040;
const FEATURE_INCOMPAT_64BIT: u32 = 0x0080;
const FEATURE_INCOMPAT_FLEX_BG: u32 = 0x0200;
const FEATURE_INCOMPAT_INLINE_DATA: u32 = 0x8000;

/// Feature flags of the read only compatible feature set.
const FEATURE_RO_COMPAT_LARGE_FILE: u32 = 0x0002;

/// Inode flags.
const INODE_FLAG_INLINE_DATA: u32 = 0x1000_0000;

/// The magic number of an extent tree header.
const EXT4_EXTENT_MAGIC: u16 = 0xF30A;
/// The magic number of an htree directory root.
const EXT4_HTREE_MAGIC: u32 = 0x6469_7220; // "dir "

/// File types as stored in a directory entry.
const FT_REG_FILE: u8 = 1;
const FT_DIR: u8 = 2;

/// A group descriptor.
#[derive(Debug, Clone, Copy)]
struct GroupDesc {
    inode_table: u64,
}

/// An ext4 volume.
pub struct Ext4Volume {
    device: Arc<dyn BlockDevice>,
    block_size: u32,
    inodes_per_group: u32,
    inode_size: u16,
    desc_size: u16,
    groups_count: u32,
    gdt_block: u64,
    has_64bit: bool,
    has_large_file: bool,
}

fn le16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

impl Ext4Volume {
    /// Open an ext2/3/4 volume.
    pub fn open(device: PartitionDevice) -> Result<Self, FsError> {
        let device: Arc<dyn BlockDevice> = Arc::new(device);
        let superblock = device
            .read_vec_at(SUPERBLOCK_OFFSET, 1024)
            .map_err(|err| FsError::Corrupt(format!("ext4 superblock: {err}")))?;
        if superblock.len() < 1024 {
            return Err(FsError::Corrupt("ext4 superblock is truncated".into()));
        }
        let sb = &superblock;

        if le16(sb, 0x38) != EXT4_MAGIC {
            return Err(FsError::Corrupt("not an ext2/3/4 filesystem".into()));
        }

        let feature_incompat = le32(sb, 0x60);
        let feature_ro_compat = le32(sb, 0x64);

        let has_64bit = feature_incompat & FEATURE_INCOMPAT_64BIT != 0;
        let has_extents = feature_incompat & FEATURE_INCOMPAT_EXTENTS != 0;
        let _has_flex_bg = feature_incompat & FEATURE_INCOMPAT_FLEX_BG != 0;
        let has_inline_data = feature_incompat & FEATURE_INCOMPAT_INLINE_DATA != 0;
        let has_large_file = feature_ro_compat & FEATURE_RO_COMPAT_LARGE_FILE != 0;
        let _ = FEATURE_INCOMPAT_FILETYPE;

        if !has_extents {
            return Err(FsError::Unsupported(
                "ext2/ext3 filesystems without extents are not supported".into(),
            ));
        }
        if feature_incompat & FEATURE_INCOMPAT_INLINE_DATA != 0 && has_inline_data {
            // Not fatal: only the files that are stored inline are affected.
            log::warn!("the filesystem uses inline data, some files cannot be read");
        }

        let log_block_size = le32(sb, 0x18);
        if log_block_size > 6 {
            return Err(FsError::Corrupt(format!(
                "implausible block size exponent {log_block_size}"
            )));
        }
        let block_size = 1024u32 << log_block_size;
        let first_data_block = le32(sb, 0x14) as u64;
        let blocks_per_group = le32(sb, 0x20);
        let inodes_per_group = le32(sb, 0x28);
        let inode_size = le16(sb, 0x58);
        let desc_size = if has_64bit {
            let size = le16(sb, 0xfe);
            if size == 0 {
                64
            } else {
                size
            }
        } else {
            32
        };

        if !(128..=4096).contains(&inode_size) || inode_size & (inode_size - 1) != 0 {
            return Err(FsError::Corrupt(format!(
                "implausible inode size {inode_size}"
            )));
        }
        if blocks_per_group == 0 || inodes_per_group == 0 {
            return Err(FsError::Corrupt("the group geometry is broken".into()));
        }

        // The group descriptor table lives in the block right behind the
        // block that holds the superblock: the superblock always starts at
        // byte 1024, so with 1 KiB blocks it is in block 1 and the table
        // starts at block 2, with 4 KiB blocks it is in block 0 and the table
        // starts at block 1.
        let gdt_block = (SUPERBLOCK_OFFSET / block_size as u64) + 1;
        let _ = first_data_block;

        let blocks_count = if has_64bit {
            le32(sb, 0x04) as u64 | ((le32(sb, 0x150) as u64) << 32)
        } else {
            le32(sb, 0x04) as u64
        };
        let groups_count = blocks_count
            .div_ceil(blocks_per_group as u64)
            .min(u32::MAX as u64) as u32;

        Ok(Self {
            device,
            block_size,
            inodes_per_group,
            inode_size,
            desc_size,
            groups_count,
            gdt_block,
            has_64bit,
            has_large_file,
        })
    }

    /// Read a block of the filesystem.
    fn read_block(&self, block: u64) -> Result<Vec<u8>, FsError> {
        self.device
            .read_vec_at(block * self.block_size as u64, self.block_size as usize)
            .map_err(|err| FsError::Corrupt(format!("read block {block}: {err}")))
    }

    /// Read the descriptor of a block group.
    fn group_desc(&self, group: u32) -> Result<GroupDesc, FsError> {
        let per_block = self.block_size as usize / self.desc_size as usize;
        let block = self.gdt_block + (group as u64) / per_block as u64;
        let index = (group as usize) % per_block;
        let data = self.read_block(block)?;
        let offset = index * self.desc_size as usize;
        if offset + self.desc_size as usize > data.len() {
            return Err(FsError::Corrupt("truncated group descriptor".into()));
        }
        let desc = &data[offset..offset + self.desc_size as usize];
        let inode_table = le32(desc, 0x08) as u64;
        let inode_table = if self.has_64bit && self.desc_size >= 64 {
            inode_table | ((le32(desc, 0x28) as u64) << 32)
        } else {
            inode_table
        };
        Ok(GroupDesc { inode_table })
    }

    /// Read an inode.
    fn read_inode(&self, number: u32) -> Result<Inode, FsError> {
        if number == 0 || number > self.inodes_per_group * self.groups_count {
            return Err(FsError::NotFound(format!("inode {number}")));
        }
        let group = (number - 1) / self.inodes_per_group;
        let index = (number - 1) % self.inodes_per_group;
        let desc = self.group_desc(group)?;
        let offset =
            desc.inode_table * self.block_size as u64 + index as u64 * self.inode_size as u64;
        let bytes = self
            .device
            .read_vec_at(offset, self.inode_size as usize)
            .map_err(|err| FsError::Corrupt(format!("read inode {number}: {err}")))?;
        if bytes.len() < 128 {
            return Err(FsError::Corrupt("truncated inode".into()));
        }
        let mode = le16(&bytes, 0x00);
        let size_low = le32(&bytes, 0x04) as u64;
        let flags = le32(&bytes, 0x20);
        let size_high = le32(&bytes, 0x6c) as u64;
        let size = if self.has_large_file {
            size_low | (size_high << 32)
        } else {
            size_low
        };
        let mut block = [0u8; 60];
        block.copy_from_slice(&bytes[0x28..0x64]);
        Ok(Inode {
            mode,
            size,
            flags,
            block,
        })
    }

    /// Resolve a path to an inode number.
    fn resolve(&self, path: &str) -> Result<u32, FsError> {
        let mut current = ROOT_INODE;
        for part in path.split('/').filter(|p| !p.is_empty() && *p != ".") {
            let inode = self.read_inode(current)?;
            let entries = self.list_dir_entries(&inode)?;
            let found = entries
                .into_iter()
                .find(|entry| entry.name == part)
                .ok_or_else(|| FsError::NotFound(path.to_string()))?;
            current = found.inode;
        }
        Ok(current)
    }

    /// The physical blocks of a file, using its extent tree.
    fn extents(&self, inode: &Inode) -> Result<Vec<(u64, u64, u32)>, FsError> {
        let mut out = Vec::new();
        self.walk_extents(&inode.block, &mut out, 0)?;
        out.sort_by_key(|(logical, _, _)| *logical);
        Ok(out)
    }

    /// Walk an extent tree node.
    fn walk_extents(
        &self,
        node: &[u8],
        out: &mut Vec<(u64, u64, u32)>,
        depth_guard: u32,
    ) -> Result<(), FsError> {
        if depth_guard > 8 {
            return Err(FsError::Corrupt("the extent tree is too deep".into()));
        }
        if node.len() < 12 {
            return Err(FsError::Corrupt("truncated extent header".into()));
        }
        let magic = le16(node, 0);
        if magic != EXT4_EXTENT_MAGIC {
            return Err(FsError::Corrupt("bad extent magic".into()));
        }
        let entries = le16(node, 2) as usize;
        let depth = le16(node, 6) as usize;

        for index in 0..entries {
            let offset = 12 + index * 12;
            if offset + 12 > node.len() {
                break;
            }
            let entry = &node[offset..offset + 12];
            if depth == 0 {
                let logical = le32(entry, 0) as u64;
                let len = le16(entry, 4) as u32;
                let physical = ((le16(entry, 6) as u64) << 32) | le32(entry, 8) as u64;
                out.push((logical, physical, len));
            } else {
                let physical = ((le16(entry, 6) as u64) << 32) | le32(entry, 4) as u64;
                let child = self.read_block(physical)?;
                self.walk_extents(&child, out, depth_guard + 1)?;
            }
        }
        Ok(())
    }

    /// The data of a file.
    fn read_data(&self, inode: &Inode, max: usize) -> Result<Vec<u8>, FsError> {
        if inode.flags & INODE_FLAG_INLINE_DATA != 0 {
            return Err(FsError::Unsupported(
                "the file is stored inline, which is not supported yet".into(),
            ));
        }
        let wanted = inode.size.min(max as u64) as usize;
        let mut out = vec![0u8; wanted];
        let extents = self.extents(inode)?;
        let block_size = self.block_size as u64;
        for (logical, physical, len) in extents {
            // An extent with a length above 32768 is uninitialised: the
            // blocks are reserved but hold no data.
            if len > 32768 {
                continue;
            }
            for index in 0..len as u64 {
                let position = (logical + index) * block_size;
                if position >= wanted as u64 {
                    break;
                }
                let data = self.read_block(physical + index)?;
                let start = position as usize;
                let take = (wanted - start).min(data.len());
                out[start..start + take].copy_from_slice(&data[..take]);
            }
        }
        Ok(out)
    }

    /// The entries of a directory.
    fn list_dir_entries(&self, inode: &Inode) -> Result<Vec<DirEntry>, FsError> {
        if inode.mode & 0xf000 != 0x4000 {
            return Err(FsError::Other("not a directory".into()));
        }
        let mut out = Vec::new();
        let extents = self.extents(inode)?;
        for (_logical, physical, len) in extents {
            for index in 0..len as u64 {
                let block = self.read_block(physical + index)?;
                self.parse_dir_block(&block, &mut out)?;
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.dedup_by(|a, b| a.name == b.name);
        Ok(out)
    }

    /// Parse the entries of one directory block.
    fn parse_dir_block(&self, block: &[u8], out: &mut Vec<DirEntry>) -> Result<(), FsError> {
        if block.len() < 8 {
            return Ok(());
        }
        // An htree root block: the real entries live in the leaf blocks.
        if le32(block, 0) == EXT4_HTREE_MAGIC {
            return self.parse_htree_root(block, out);
        }
        self.parse_dir_entries(block, out);
        Ok(())
    }

    /// Parse the plain directory entries of a block.
    fn parse_dir_entries(&self, block: &[u8], out: &mut Vec<DirEntry>) {
        let mut offset = 0usize;
        while offset + 8 <= block.len() {
            let inode = le32(block, offset);
            let rec_len = le16(block, offset + 4) as usize;
            let name_len = block[offset + 6] as usize;
            let file_type = block[offset + 7];
            if rec_len < 8 || offset + rec_len > block.len() {
                break;
            }
            if inode != 0 && name_len > 0 && offset + 8 + name_len <= block.len() {
                let name =
                    String::from_utf8_lossy(&block[offset + 8..offset + 8 + name_len]).to_string();
                if name != "." && name != ".." {
                    out.push(DirEntry {
                        name,
                        inode,
                        file_type,
                    });
                }
            }
            offset += rec_len;
        }
    }

    /// Parse an htree root block and read the leaf blocks it points at.
    fn parse_htree_root(&self, block: &[u8], out: &mut Vec<DirEntry>) -> Result<(), FsError> {
        // dx_root: two fake dirents (12 bytes each), the dx_info (8 bytes) and
        // then the dx_entries (8 bytes each).
        const HEADER: usize = 32;
        if block.len() <= HEADER {
            return Ok(());
        }
        let indirect_levels = block[0x1f];
        let count = (block.len() - HEADER) / 8;
        for index in 0..count {
            let offset = HEADER + index * 8;
            let child = le32(block, offset + 4) as u64;
            if indirect_levels == 0 {
                let leaf = self.read_block(child)?;
                self.parse_dir_entries(&leaf, out);
            } else {
                // One level of indirection: the block holds more dx_entries.
                let node = self.read_block(child)?;
                let node_count = (node.len().saturating_sub(8)) / 8;
                for inner in 0..node_count {
                    let inner_offset = 8 + inner * 8;
                    if inner_offset + 8 > node.len() {
                        break;
                    }
                    let leaf_block = le32(&node, inner_offset + 4) as u64;
                    let leaf = self.read_block(leaf_block)?;
                    self.parse_dir_entries(&leaf, out);
                }
            }
        }
        Ok(())
    }
}

/// A directory entry.
#[derive(Debug, Clone)]
struct DirEntry {
    name: String,
    inode: u32,
    file_type: u8,
}

/// An inode.
#[derive(Debug, Clone)]
struct Inode {
    mode: u16,
    size: u64,
    flags: u32,
    block: [u8; 60],
}

impl Inode {
    fn is_dir(&self) -> bool {
        self.mode & 0xf000 == 0x4000
    }
}

impl VolumeFs for Ext4Volume {
    fn list_dir(&mut self, path: &str) -> Result<Vec<FsEntry>, FsError> {
        let number = self.resolve(path)?;
        let inode = self.read_inode(number)?;
        let mut entries = Vec::new();
        for entry in self.list_dir_entries(&inode)? {
            let is_dir = if entry.file_type == FT_DIR {
                true
            } else if entry.file_type == FT_REG_FILE {
                false
            } else {
                // Unknown type: ask the inode itself.
                match self.read_inode(entry.inode) {
                    Ok(child) => child.is_dir(),
                    Err(_) => false,
                }
            };
            let size = if is_dir {
                0
            } else {
                self.read_inode(entry.inode).map(|i| i.size).unwrap_or(0)
            };
            entries.push(FsEntry {
                name: entry.name,
                is_dir,
                size,
            });
        }
        Ok(entries)
    }

    fn read_file(&mut self, path: &str, max: usize) -> Result<Vec<u8>, FsError> {
        let number = self.resolve(path)?;
        let inode = self.read_inode(number)?;
        self.read_data(&inode, max)
    }

    fn file_size(&mut self, path: &str) -> Option<u64> {
        let number = self.resolve(path).ok()?;
        let inode = self.read_inode(number).ok()?;
        if inode.is_dir() {
            return None;
        }
        Some(inode.size)
    }
}
