//! A read only btrfs reader.
//!
//! There is no btrfs crate on crates.io, so Helm reads the filesystem itself.
//! The implementation follows the on-disk format of the Linux kernel
//! (`include/uapi/linux/btrfs_tree.h`):
//!
//! * the superblock at 64 KiB holds the root of the tree, the root of the
//!   chunk tree and the system chunk array that is needed to bootstrap the
//!   chunk map,
//! * a tree block starts with a 101 byte header, followed by the items of a
//!   leaf (25 bytes each: a key, the offset and the size) or by the key
//!   pointers of an inner node,
//! * the items of a leaf are stored in the second half of the block, in the
//!   reverse order.
//!
//! Only the parts that Helm needs are implemented: the directory entries, the
//! inodes and the extents of a file.

use std::collections::HashMap;
use std::sync::Arc;

use crate::block::{BlockDevice, PartitionDevice};
use crate::fs::{FsEntry, FsError, VolumeFs};

/// The offset of the superblock.
const SUPERBLOCK_OFFSET: u64 = 65536;
/// The magic of a superblock and of a tree block.
const MAGIC: &[u8; 8] = b"_BHRfS_M";
/// The size of the header of a tree block.
const HEADER_SIZE: usize = 101;
/// The size of the header of an item of a leaf.
const ITEM_SIZE: usize = 25;

/// The key types Helm is interested in.
const KEY_INODE_ITEM: u8 = 1;
const KEY_DIR_ITEM: u8 = 84;
const KEY_DIR_INDEX: u8 = 96;
const KEY_ROOT_ITEM: u8 = 132;
const KEY_CHUNK_ITEM: u8 = 228;
const KEY_EXTENT_DATA: u8 = 108;

/// The object ids of the trees.
const ROOT_TREE_OBJECTID: u64 = 1;
const EXTENT_TREE_OBJECTID: u64 = 2;
const FS_TREE_OBJECTID: u64 = 5;
const FIRST_CHUNK_TREE_OBJECTID: u64 = 256;

/// The file modes of an inode.
const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;

/// The compression types of an extent.
const COMPRESS_NONE: u8 = 0;
const COMPRESS_ZLIB: u8 = 1;
const COMPRESS_LZO: u8 = 2;
const COMPRESS_ZSTD: u8 = 3;

/// A key of a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    objectid: u64,
    key_type: u8,
    offset: u64,
}

impl Key {
    fn new(objectid: u64, key_type: u8, offset: u64) -> Self {
        Self {
            objectid,
            key_type,
            offset,
        }
    }
}

/// A chunk of the filesystem: a range of logical addresses and the places
/// where they are stored.
#[derive(Debug, Clone)]
struct Chunk {
    logical: u64,
    length: u64,
    stripes: Vec<(u64, u64)>,
}

impl Chunk {
    /// The physical address of a logical address inside this chunk.
    fn physical(&self, logical: u64) -> Option<u64> {
        let offset = logical.checked_sub(self.logical)?;
        if offset >= self.length {
            return None;
        }
        // The first stripe is the primary copy; the other ones are mirrors.
        self.stripes.first().map(|(_, start)| start + offset)
    }
}

/// An item of a leaf.
#[derive(Debug, Clone)]
struct Item {
    key: Key,
    data: Vec<u8>,
}

/// The header of a tree block.
#[derive(Debug, Clone)]
struct TreeHeader {
    level: u8,
    nritems: u32,
}

/// A btrfs volume.
pub struct BtrfsVolume {
    device: Arc<dyn BlockDevice>,
    nodesize: u32,
    chunks: Vec<Chunk>,
    /// The root of the tree that holds the files.
    fs_tree: u64,
    /// The object id of the root directory.
    root_dirid: u64,
    /// The root of the tree that holds the extents of the files.
    extent_tree: u64,
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

fn le64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn read_key(bytes: &[u8], offset: usize) -> Key {
    Key {
        objectid: le64(bytes, offset),
        key_type: bytes[offset + 8],
        offset: le64(bytes, offset + 9),
    }
}

impl BtrfsVolume {
    /// Open a btrfs volume.
    pub fn open(device: PartitionDevice) -> Result<Self, FsError> {
        let device: Arc<dyn BlockDevice> = Arc::new(device);
        let superblock = device
            .read_vec_at(SUPERBLOCK_OFFSET, 4096)
            .map_err(|err| FsError::Corrupt(format!("btrfs superblock: {err}")))?;
        if superblock.len() < 0x32B || &superblock[0x40..0x48] != MAGIC {
            return Err(FsError::Corrupt("not a btrfs filesystem".into()));
        }

        let root = le64(&superblock, 0x50);
        let chunk_root = le64(&superblock, 0x58);
        let root_dir_objectid = le64(&superblock, 0x80);
        let nodesize = le32(&superblock, 0x94);
        if !(4096..=65536).contains(&nodesize) || nodesize & (nodesize - 1) != 0 {
            return Err(FsError::Corrupt(format!(
                "implausible node size {nodesize}"
            )));
        }

        let mut volume = Self {
            device,
            nodesize,
            chunks: Vec::new(),
            fs_tree: 0,
            root_dirid: root_dir_objectid,
            extent_tree: 0,
        };

        // The system chunk array of the superblock holds the chunks that
        // contain the chunk tree: they are needed to read anything at all.
        volume.read_system_chunks(&superblock);
        if volume.chunks.is_empty() {
            return Err(FsError::Corrupt("the system chunk array is empty".into()));
        }

        // The chunk tree holds the rest of the chunks.
        volume.read_chunk_tree(chunk_root);

        // The root tree holds the roots of the other trees.
        let (fs_tree, root_dirid, extent_tree) = volume.read_root_tree(root)?;
        volume.fs_tree = fs_tree;
        if root_dirid != 0 {
            volume.root_dirid = root_dirid;
        }
        volume.extent_tree = extent_tree;
        Ok(volume)
    }

    /// Read the chunks of the system chunk array.
    fn read_system_chunks(&mut self, superblock: &[u8]) {
        // The array starts at 0x32B and holds [key][chunk] entries.
        let mut offset = 0x32B;
        while offset + 17 + 48 <= 0x32B + 2048 {
            let key = read_key(superblock, offset);
            if key.key_type != KEY_CHUNK_ITEM {
                break;
            }
            let chunk = &superblock[offset + 17..];
            if let Some(chunk) = self.parse_chunk(key.offset, chunk) {
                self.chunks.push(chunk);
            }
            // The size of the entry depends on the number of stripes.
            let num_stripes = le16(chunk, 40) as usize;
            if num_stripes == 0 {
                break;
            }
            offset += 17 + 48 + num_stripes * 32;
        }
    }

    /// Parse a chunk item.
    fn parse_chunk(&self, logical: u64, data: &[u8]) -> Option<Chunk> {
        if data.len() < 48 {
            return None;
        }
        let length = le64(data, 0);
        let num_stripes = le16(data, 44) as usize;
        if num_stripes == 0 || data.len() < 48 + num_stripes * 32 {
            return None;
        }
        let mut stripes = Vec::with_capacity(num_stripes);
        for index in 0..num_stripes {
            let base = 48 + index * 32;
            stripes.push((le64(data, base), le64(data, base + 8)));
        }
        Some(Chunk {
            logical,
            length,
            stripes,
        })
    }

    /// Read the chunk tree and add its chunks to the map.
    fn read_chunk_tree(&mut self, chunk_root: u64) {
        let Ok(block) = self.read_leaf(chunk_root) else {
            return;
        };
        for item in &block.items {
            if item.key.key_type != KEY_CHUNK_ITEM {
                continue;
            }
            if let Some(chunk) = self.parse_chunk(item.key.offset, &item.data) {
                self.chunks.push(chunk);
            }
        }
        self.chunks.sort_by_key(|chunk| chunk.logical);
    }

    /// Read the root tree and return the roots of the file and the extent
    /// tree.
    fn read_root_tree(&self, root: u64) -> Result<(u64, u64, u64), FsError> {
        let block = self.read_leaf(root)?;
        let mut fs_tree = 0;
        let mut root_dirid = 0;
        let mut extent_tree = 0;
        for item in &block.items {
            if item.key.key_type != KEY_ROOT_ITEM {
                continue;
            }
            let data = &item.data;
            if data.len() < 184 {
                continue;
            }
            let item_root_dirid = le64(data, 168);
            let bytenr = le64(data, 176);
            match item.key.objectid {
                FS_TREE_OBJECTID => {
                    fs_tree = bytenr;
                    root_dirid = item_root_dirid;
                }
                EXTENT_TREE_OBJECTID => extent_tree = bytenr,
                _ => {}
            }
        }
        if fs_tree == 0 {
            return Err(FsError::Corrupt(
                "the root tree does not hold the tree of the files".into(),
            ));
        }
        Ok((fs_tree, root_dirid, extent_tree))
    }

    /// The physical address of a logical address.
    fn physical(&self, logical: u64) -> Result<u64, FsError> {
        for chunk in &self.chunks {
            if let Some(physical) = chunk.physical(logical) {
                return Ok(physical);
            }
        }
        Err(FsError::Corrupt(format!(
            "no chunk holds the address {logical}"
        )))
    }

    /// Read a tree block and parse its items.
    fn read_leaf(&self, bytenr: u64) -> Result<Leaf, FsError> {
        let physical = self.physical(bytenr)?;
        let raw = self
            .device
            .read_vec_at(physical, self.nodesize as usize)
            .map_err(|err| FsError::Corrupt(format!("read block {bytenr}: {err}")))?;
        if raw.len() < HEADER_SIZE {
            return Err(FsError::Corrupt("truncated tree block".into()));
        }
        let header = TreeHeader {
            level: raw[0x64],
            nritems: le32(&raw, 0x60),
        };
        let mut items = Vec::with_capacity(header.nritems as usize);
        if header.level == 0 {
            for index in 0..header.nritems as usize {
                let base = HEADER_SIZE + index * ITEM_SIZE;
                if base + ITEM_SIZE > raw.len() {
                    break;
                }
                let key = read_key(&raw, base);
                // The offset of an item is relative to the data area, which
                // starts right behind the header.
                let offset = HEADER_SIZE + le32(&raw, base + 17) as usize;
                let size = le32(&raw, base + 21) as usize;
                if offset + size > raw.len() {
                    continue;
                }
                items.push(Item {
                    key,
                    data: raw[offset..offset + size].to_vec(),
                });
            }
        }
        Ok(Leaf { header, raw, items })
    }

    /// Descend the tree to the leaf that holds `key`.
    ///
    /// The index of the first item that is not before `key` is returned as
    /// well, so that the caller can start there.
    fn descend(&self, bytenr: u64, key: Key) -> Result<(Leaf, TreePath, usize), FsError> {
        let mut path: TreePath = Vec::new();
        let mut current = bytenr;
        loop {
            let leaf = self.read_leaf(current)?;
            if leaf.header.level == 0 {
                let start = leaf
                    .items
                    .iter()
                    .position(|item| item.key >= key)
                    .unwrap_or(leaf.items.len());
                return Ok((leaf, path, start));
            }
            // An inner node: the key pointers follow the header.
            let count = leaf.header.nritems as usize;
            let mut chosen = 0usize;
            for index in 0..count {
                let base = HEADER_SIZE + index * 33;
                if base + 33 > leaf.raw.len() {
                    break;
                }
                let pointer_key = read_key(&leaf.raw, base);
                if pointer_key <= key {
                    chosen = index;
                } else {
                    break;
                }
            }
            let base = HEADER_SIZE + chosen * 33;
            if base + 33 > leaf.raw.len() {
                return Err(FsError::Corrupt("broken inner node".into()));
            }
            let child = le64(&leaf.raw, base + 17);
            path.push((current, chosen));
            current = child;
        }
    }

    /// The items of a directory.
    fn directory_entries(&self, objectid: u64) -> Result<Vec<DirEntry>, FsError> {
        let mut entries: Vec<DirEntry> = Vec::new();
        let mut seen: HashMap<String, ()> = HashMap::new();

        // The entries are in the items of the type `DIR_ITEM`.
        let start = Key::new(objectid, KEY_DIR_ITEM, 0);
        let (leaf, path, index) = self.descend(self.fs_tree, start)?;
        let mut cursor = Cursor {
            volume: self,
            leaf,
            path,
            index,
            done: false,
        };
        while let Some(item) = cursor.next_item() {
            if item.key.objectid != objectid {
                break;
            }
            if item.key.key_type != KEY_DIR_ITEM {
                continue;
            }
            if let Some(entry) = parse_dir_item(&item.data) {
                if seen.insert(entry.name.clone(), ()).is_none() {
                    entries.push(entry);
                }
            }
        }

        // A big directory keeps its names in the leaves of the index.
        let start = Key::new(objectid, KEY_DIR_INDEX, 0);
        if let Ok((leaf, path, index)) = self.descend(self.fs_tree, start) {
            let mut cursor = Cursor {
                volume: self,
                leaf,
                path,
                index,
                done: false,
            };
            let mut index_leaves: Vec<u64> = Vec::new();
            while let Some(item) = cursor.next_item() {
                if item.key.objectid != objectid {
                    break;
                }
                if item.key.key_type != KEY_DIR_INDEX {
                    continue;
                }
                // The index leaf block is at the end of the item.
                if item.data.len() >= 33 {
                    index_leaves.push(le64(&item.data, 17));
                }
            }
            for leaf_bytenr in index_leaves {
                let Ok(leaf) = self.read_leaf(leaf_bytenr) else {
                    continue;
                };
                for item in &leaf.items {
                    if item.key.objectid != objectid || item.key.key_type != KEY_DIR_ITEM {
                        continue;
                    }
                    if let Some(entry) = parse_dir_item(&item.data) {
                        if seen.insert(entry.name.clone(), ()).is_none() {
                            entries.push(entry);
                        }
                    }
                }
            }
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Resolve a path to the object id of the file or the directory.
    fn resolve(&self, path: &str) -> Result<(u64, bool), FsError> {
        let mut objectid = self.root_dirid;
        let mut is_dir = true;
        for part in path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
        {
            if !is_dir {
                return Err(FsError::NotFound(path.to_string()));
            }
            let entries = self.directory_entries(objectid)?;
            let found = entries
                .into_iter()
                .find(|entry| entry.name == part)
                .ok_or_else(|| FsError::NotFound(path.to_string()))?;
            objectid = found.objectid;
            is_dir = found.is_dir;
        }
        Ok((objectid, is_dir))
    }

    /// The inode of a file.
    fn inode(&self, objectid: u64) -> Result<Inode, FsError> {
        let key = Key::new(objectid, KEY_INODE_ITEM, 0);
        let (leaf, _, _) = self.descend(self.fs_tree, key)?;
        for item in &leaf.items {
            if item.key.objectid != objectid || item.key.key_type != KEY_INODE_ITEM {
                continue;
            }
            let data = &item.data;
            if data.len() < 64 {
                continue;
            }
            return Ok(Inode {
                size: le64(data, 16),
                mode: le32(data, 52),
            });
        }
        Err(FsError::NotFound(format!("inode {objectid}")))
    }

    /// The extent items of a file.
    ///
    /// The extents of a file are stored as `EXTENT_DATA` items. On a
    /// filesystem created by a current `mkfs.btrfs` they live in the tree of
    /// the files, on an older one they live in the extent tree; both are
    /// looked at.
    fn file_extents(&self, objectid: u64) -> Result<Vec<Item>, FsError> {
        let mut out = Vec::new();

        // The tree of the files.
        let key = Key::new(objectid, KEY_EXTENT_DATA, 0);
        if let Ok((leaf, _, index)) = self.descend(self.fs_tree, key) {
            let mut cursor = Cursor {
                volume: self,
                leaf,
                path: Vec::new(),
                index,
                done: false,
            };
            while let Some(item) = cursor.next_item() {
                if item.key.objectid != objectid {
                    break;
                }
                if item.key.key_type == KEY_EXTENT_DATA {
                    out.push(item);
                }
            }
        }

        // The extent tree.
        if out.is_empty() && self.extent_tree != 0 {
            let key = Key::new(EXTENT_TREE_OBJECTID, KEY_EXTENT_DATA, objectid);
            if let Ok((leaf, _, index)) = self.descend(self.extent_tree, key) {
                let mut cursor = Cursor {
                    volume: self,
                    leaf,
                    path: Vec::new(),
                    index,
                    done: false,
                };
                while let Some(item) = cursor.next_item() {
                    if item.key.objectid != EXTENT_TREE_OBJECTID {
                        break;
                    }
                    if item.key.key_type == KEY_EXTENT_DATA && item.key.offset == objectid {
                        out.push(item);
                    }
                }
            }
        }

        Ok(out)
    }

    /// The content of a file.
    fn file_data(&self, objectid: u64, max: usize) -> Result<Vec<u8>, FsError> {
        let wanted = self.inode(objectid)?.size.min(max as u64) as usize;
        let mut out = vec![0u8; wanted];

        for extent in self.file_extents(objectid)? {
            let data = &extent.data;
            if data.len() < 21 {
                continue;
            }
            let ram_bytes = le64(data, 8) as usize;
            let compression = data[16];
            let extent_type = data[20];
            match extent_type {
                // The content is stored inside the item.
                0 => {
                    let raw = &data[21..data.len().min(21 + ram_bytes)];
                    let content = decompress(raw, compression, wanted)?;
                    let end = content.len().min(wanted);
                    out[..end].copy_from_slice(&content[..end]);
                }
                // A regular extent, stored on the disk.
                1 => {
                    if data.len() < 53 {
                        continue;
                    }
                    let disk_bytenr = le64(data, 21);
                    let disk_num_bytes = le64(data, 29) as usize;
                    let offset = le64(data, 37) as usize;
                    let num_bytes = le64(data, 45) as usize;
                    if offset >= wanted {
                        continue;
                    }
                    let physical = self.physical(disk_bytenr)?;
                    let raw = self
                        .device
                        .read_vec_at(physical, disk_num_bytes)
                        .map_err(|err| FsError::Corrupt(format!("read extent: {err}")))?;
                    let content = decompress(&raw, compression, num_bytes)?;
                    let start = offset.min(wanted);
                    let end = (offset + content.len()).min(wanted);
                    if start < end {
                        out[start..end].copy_from_slice(&content[..end - start]);
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

/// The header and the items of a tree block.
struct Leaf {
    header: TreeHeader,
    raw: Vec<u8>,
    items: Vec<Item>,
}

/// A cursor that walks over the items of a tree.
struct Cursor<'a> {
    volume: &'a BtrfsVolume,
    leaf: Leaf,
    path: TreePath,
    index: usize,
    done: bool,
}

impl<'a> Cursor<'a> {
    /// The next item of the tree, crossing the leaf boundaries.
    fn next_item(&mut self) -> Option<Item> {
        if self.done {
            return None;
        }
        loop {
            if self.index < self.leaf.items.len() {
                let item = self.leaf.items[self.index].clone();
                self.index += 1;
                return Some(item);
            }
            // The leaf is exhausted: go up and take the next subtree.
            let (parent, position) = self.path.pop()?;
            let parent_leaf = self.volume.read_leaf(parent).ok()?;
            if position + 1 >= parent_leaf.header.nritems as usize {
                // The parent is exhausted too: continue upwards.
                self.leaf = parent_leaf;
                self.index = self.leaf.items.len();
                continue;
            }
            let next = position + 1;
            let base = HEADER_SIZE + next * 33;
            if base + 33 > parent_leaf.raw.len() {
                return None;
            }
            let child = le64(&parent_leaf.raw, base + 17);
            self.path.push((parent, next));
            // Descend to the first leaf of the subtree.
            let mut current = child;
            loop {
                let leaf = self.volume.read_leaf(current).ok()?;
                if leaf.header.level == 0 {
                    self.leaf = leaf;
                    self.index = 0;
                    break;
                }
                let base = HEADER_SIZE;
                if base + 33 > leaf.raw.len() {
                    return None;
                }
                let child = le64(&leaf.raw, base + 17);
                self.path.push((current, 0));
                current = child;
            }
        }
    }
}

/// An entry of a directory.
#[derive(Debug, Clone)]
struct DirEntry {
    name: String,
    objectid: u64,
    is_dir: bool,
}

/// Parse a directory item.
fn parse_dir_item(data: &[u8]) -> Option<DirEntry> {
    if data.len() < 30 {
        return None;
    }
    let location = read_key(data, 0);
    let name_len = le16(data, 27) as usize;
    let file_type = data[29];
    if name_len == 0 || data.len() < 30 + name_len {
        return None;
    }
    // The length of the name includes the terminating zero.
    let raw = &data[30..30 + name_len];
    let name = String::from_utf8_lossy(raw)
        .trim_end_matches('\0')
        .to_string();
    Some(DirEntry {
        name,
        objectid: location.objectid,
        is_dir: file_type == 2,
    })
}

/// An inode.
struct Inode {
    size: u64,
    mode: u32,
}

impl Inode {
    fn is_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }
}

/// Decompress the content of an extent.
fn decompress(data: &[u8], compression: u8, wanted: usize) -> Result<Vec<u8>, FsError> {
    let mut out = match compression {
        COMPRESS_NONE => data.to_vec(),
        COMPRESS_ZLIB => {
            let mut decoder = flate2::read::ZlibDecoder::new(data);
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut out)
                .map_err(|err| FsError::Corrupt(format!("zlib: {err}")))?;
            out
        }
        COMPRESS_ZSTD => {
            let mut decoder = ruzstd::decoding::StreamingDecoder::new(data)
                .map_err(|err| FsError::Corrupt(format!("zstd: {err}")))?;
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut out)
                .map_err(|err| FsError::Corrupt(format!("zstd: {err}")))?;
            out
        }
        COMPRESS_LZO => {
            return Err(FsError::Unsupported(
                "lzo compressed files are not supported".into(),
            ))
        }
        other => return Err(FsError::Unsupported(format!("unknown compression {other}"))),
    };
    Ok(if out.len() > wanted {
        out.truncate(wanted);
        out
    } else {
        out
    })
}

impl VolumeFs for BtrfsVolume {
    fn list_dir(&mut self, path: &str) -> Result<Vec<FsEntry>, FsError> {
        let (objectid, is_dir) = self.resolve(path)?;
        if !is_dir {
            return Err(FsError::Other(format!("{path} is not a directory")));
        }
        let entries = self.directory_entries(objectid)?;
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let size = if entry.is_dir {
                0
            } else {
                self.inode(entry.objectid)
                    .map(|inode| inode.size)
                    .unwrap_or(0)
            };
            out.push(FsEntry {
                name: entry.name,
                is_dir: entry.is_dir,
                size,
            });
        }
        Ok(out)
    }

    fn read_file(&mut self, path: &str, max: usize) -> Result<Vec<u8>, FsError> {
        let (objectid, is_dir) = self.resolve(path)?;
        if is_dir {
            return Err(FsError::Other(format!("{path} is a directory")));
        }
        let inode = self.inode(objectid)?;
        if !inode.is_file() {
            return Err(FsError::Other(format!("{path} is not a regular file")));
        }
        self.file_data(objectid, max)
    }

    fn file_size(&mut self, path: &str) -> Option<u64> {
        let (objectid, is_dir) = self.resolve(path).ok()?;
        if is_dir {
            return None;
        }
        let inode = self.inode(objectid).ok()?;
        if !inode.is_file() {
            return None;
        }
        Some(inode.size)
    }
}

/// The object ids that are used by the trees.
pub const KNOWN_OBJECTIDS: [(&str, u64); 5] = [
    ("ROOT_TREE", ROOT_TREE_OBJECTID),
    ("EXTENT_TREE", EXTENT_TREE_OBJECTID),
    ("FS_TREE", FS_TREE_OBJECTID),
    ("FIRST_CHUNK_TREE", FIRST_CHUNK_TREE_OBJECTID),
    ("DEV_TREE", 4),
];

/// The path of a descent through the tree: the blocks that were walked and
/// the index that was taken in each of them.
type TreePath = Vec<(u64, usize)>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The decompression of an extent.
    #[test]
    fn decompresses_the_content_of_an_extent() {
        let content = b"NAME=\"Arch Linux\"\nID=arch\n";

        // Stored as it is.
        assert_eq!(decompress(content, COMPRESS_NONE, 100).unwrap(), content);

        // Compressed with zlib.
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, content).unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(
            decompress(&compressed, COMPRESS_ZLIB, 100).unwrap(),
            content
        );

        // Compressed with zstd.
        let compressed = zstd::encode_all(std::io::Cursor::new(content), 3).unwrap();
        assert_eq!(
            decompress(&compressed, COMPRESS_ZSTD, 100).unwrap(),
            content
        );

        // The result is truncated to the size of the file.
        assert_eq!(decompress(content, COMPRESS_NONE, 4).unwrap(), b"NAME");
    }

    /// The parser of a directory item.
    #[test]
    fn parses_a_directory_item() {
        let mut data = Vec::new();
        data.extend_from_slice(&le64_bytes(257)); // the location: the inode
        data.push(KEY_INODE_ITEM); // the location: the type
        data.extend_from_slice(&le64_bytes(0)); // the location: the offset
        data.extend_from_slice(&le64_bytes(6)); // the transaction
        data.extend_from_slice(&0u16.to_le_bytes()); // the length of the data
        data.extend_from_slice(&5u16.to_le_bytes()); // the length of the name + NUL
        data.push(2); // a directory
        data.extend_from_slice(b"boot\0");

        let entry = parse_dir_item(&data).unwrap();
        assert_eq!(entry.name, "boot");
        assert_eq!(entry.objectid, 257);
        assert!(entry.is_dir);
    }

    fn le64_bytes(value: u64) -> [u8; 8] {
        value.to_le_bytes()
    }
}
