//! On-disk layout constants and structures.

use crate::crc;
use crate::error::{self, Result};
use crate::types::Timespec;

/// Image magic: `LNXFILE\0`.
pub const MAGIC: [u8; 8] = *b"LNXFILE\0";
pub const VERSION: u32 = 1;
pub const BLOCK_SIZE: u32 = 4096;
pub const BLOCK_SIZE_U64: u64 = BLOCK_SIZE as u64;
pub const INODE_SIZE: u32 = 256;
pub const INODES_PER_BLOCK: u32 = BLOCK_SIZE / INODE_SIZE;
pub const SUPERBLOCK_SIZE: usize = 4096;
pub const INLINE_MAX: usize = 128;
pub const MAX_DIRECT_EXTENTS: usize = 6;
pub const ROOT_INO: u64 = 1;
#[allow(dead_code)]
pub const FIRST_INO: u64 = 1;
pub const FIRST_USER_INO: u64 = 2;

pub const DEFAULT_JOURNAL_BLOCKS: u32 = 256; // 1 MiB
pub const DEFAULT_INITIAL_BLOCKS: u64 = 1024; // 4 MiB
pub const DEFAULT_CACHE_BLOCKS: usize = 4096; // 16 MiB
pub const SHRINK_SLACK_BLOCKS: u64 = 64;

/// Superblock lives at block 0; backup immediately after the journal.
#[derive(Debug, Clone)]
pub struct Superblock {
    pub magic: [u8; 8],
    pub version: u32,
    pub block_size: u32,
    pub journal_blocks: u32,
    pub first_data_block: u32,
    pub total_blocks: u64,
    pub inode_count: u64,
    pub free_inode_count: u64,
    pub next_inode: u64,
    pub root_ino: u64,
    pub generation: u64,
    pub uuid: [u8; 16],
    pub features: u64,
    pub mtime: Timespec,
    pub inode_table_extents: Vec<Extent>,
    pub alloc_block: u64,
    pub journal_seq: u64,
    pub journal_head: u32,
    pub journal_tail: u32,
    pub journal_committed: u64,
    /// Default compression algorithm for new inodes (`Compression` as u8).
    pub default_compression: u8,
    /// Logical blocks per compression record. 0 means the library default (8).
    pub record_blocks: u8,
}

impl Superblock {
    pub fn first_data(&self) -> u64 {
        self.first_data_block as u64
    }

    pub fn journal_start(&self) -> u64 {
        1
    }

    pub fn backup_block(&self) -> u64 {
        1 + self.journal_blocks as u64
    }

    pub fn encode(&self) -> [u8; SUPERBLOCK_SIZE] {
        let mut buf = [0u8; SUPERBLOCK_SIZE];
        buf[0..8].copy_from_slice(&self.magic);
        put_u32(&mut buf, 8, self.version);
        put_u32(&mut buf, 12, self.block_size);
        put_u32(&mut buf, 16, self.journal_blocks);
        put_u32(&mut buf, 20, self.first_data_block);
        put_u64(&mut buf, 24, self.total_blocks);
        put_u64(&mut buf, 32, self.inode_count);
        put_u64(&mut buf, 40, self.free_inode_count);
        put_u64(&mut buf, 48, self.next_inode);
        put_u64(&mut buf, 56, self.root_ino);
        put_u64(&mut buf, 64, self.generation);
        buf[72..88].copy_from_slice(&self.uuid);
        put_u64(&mut buf, 88, self.features);
        put_i64(&mut buf, 96, self.mtime.sec);
        put_u32(&mut buf, 104, self.mtime.nsec);
        put_u64(&mut buf, 112, self.alloc_block);
        put_u64(&mut buf, 120, self.journal_seq);
        put_u32(&mut buf, 128, self.journal_head);
        put_u32(&mut buf, 132, self.journal_tail);
        put_u64(&mut buf, 136, self.journal_committed);
        buf[108] = self.default_compression;
        buf[109] = self.record_blocks;

        let n = self.inode_table_extents.len().min(16) as u32;
        put_u32(&mut buf, 144, n);
        let mut off = 148;
        for ext in self.inode_table_extents.iter().take(16) {
            put_u64(&mut buf, off, ext.logical);
            put_u64(&mut buf, off + 8, ext.physical);
            put_u32(&mut buf, off + 16, ext.length);
            off += 20;
        }

        let csum = crc::crc32(&buf[0..SUPERBLOCK_SIZE - 4]);
        put_u32(&mut buf, SUPERBLOCK_SIZE - 4, csum);
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < SUPERBLOCK_SIZE {
            return Err(error::eio("superblock too small"));
        }
        let stored = get_u32(buf, SUPERBLOCK_SIZE - 4);
        let calc = crc::crc32(&buf[0..SUPERBLOCK_SIZE - 4]);
        if stored != calc {
            return Err(error::eio("superblock checksum mismatch"));
        }
        let mut magic = [0u8; 8];
        magic.copy_from_slice(&buf[0..8]);
        if magic != MAGIC {
            return Err(error::eio("bad filesystem magic"));
        }
        let version = get_u32(buf, 8);
        if version != VERSION {
            return Err(error::eio("unsupported filesystem version"));
        }
        let block_size = get_u32(buf, 12);
        if block_size != BLOCK_SIZE {
            return Err(error::eio("unsupported block size"));
        }
        let n = get_u32(buf, 144) as usize;
        let mut inode_table_extents = Vec::with_capacity(n.min(16));
        let mut off = 148;
        for _ in 0..n.min(16) {
            inode_table_extents.push(Extent::new(
                get_u64(buf, off),
                get_u64(buf, off + 8),
                get_u32(buf, off + 16),
            ));
            off += 20;
        }
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&buf[72..88]);
        Ok(Self {
            magic,
            version,
            block_size,
            journal_blocks: get_u32(buf, 16),
            first_data_block: get_u32(buf, 20),
            total_blocks: get_u64(buf, 24),
            inode_count: get_u64(buf, 32),
            free_inode_count: get_u64(buf, 40),
            next_inode: get_u64(buf, 48),
            root_ino: get_u64(buf, 56),
            generation: get_u64(buf, 64),
            uuid,
            features: get_u64(buf, 88),
            mtime: Timespec {
                sec: get_i64(buf, 96),
                nsec: get_u32(buf, 104),
            },
            inode_table_extents,
            alloc_block: get_u64(buf, 112),
            journal_seq: get_u64(buf, 120),
            journal_head: get_u32(buf, 128),
            journal_tail: get_u32(buf, 132),
            journal_committed: get_u64(buf, 136),
            default_compression: buf[108],
            record_blocks: buf[109],
        })
    }
}

/// File data extent: logical file blocks → physical image blocks.
///
/// `length` is always the logical span. Compressed records set `phys_length`
/// (physical blocks used) and `compress` (algorithm). Both extra fields are
/// in-memory; on disk they are recovered from the record header. A zero
/// `phys_length` means "same as `length`" (legacy 1:1 mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub logical: u64,
    pub physical: u64,
    pub length: u32,
    pub phys_length: u32,
    pub compress: u8,
}

impl Extent {
    pub fn new(logical: u64, physical: u64, length: u32) -> Self {
        Self {
            logical,
            physical,
            length,
            phys_length: 0,
            compress: 0,
        }
    }

    pub fn with_phys(logical: u64, physical: u64, length: u32, phys_length: u32, compress: u8) -> Self {
        Self {
            logical,
            physical,
            length,
            phys_length,
            compress,
        }
    }

    pub fn phys_len(&self) -> u64 {
        if self.phys_length != 0 {
            self.phys_length as u64
        } else {
            self.length as u64
        }
    }

    pub fn is_compressed(&self) -> bool {
        self.compress != 0
    }

    pub fn logical_end(&self) -> u64 {
        self.logical + self.length as u64
    }

    pub fn physical_end(&self) -> u64 {
        self.physical + self.phys_len()
    }

    pub fn contains_logical(&self, block: u64) -> bool {
        block >= self.logical && block < self.logical_end()
    }

    pub fn map(&self, logical: u64) -> Option<u64> {
        if self.contains_logical(logical) {
            Some(self.physical + (logical - self.logical))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataKind {
    Inline = 0,
    Extents = 1,
    ExtentTree = 2,
}

impl DataKind {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Extents,
            2 => Self::ExtentTree,
            _ => Self::Inline,
        }
    }
}

/// In-memory inode (256-byte on-disk image).
#[derive(Debug, Clone)]
pub struct Inode {
    pub ino: u64,
    pub mode: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub blocks: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub btime: Timespec,
    pub rdev: u64,
    pub flags: u32,
    pub generation: u32,
    pub data_kind: DataKind,
    /// Algorithm used for new extent-backed writes (`Compression` as u8).
    pub compression: u8,
    pub inline_data: Vec<u8>,
    pub extents: Vec<Extent>,
    pub extent_tree: u64,
    /// Number of blocks in the extent-tree allocation (in-memory; stored in the tree header).
    pub extent_tree_blocks: u64,
    pub xattr_block: u64,
    pub xattr: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Inode {
    pub fn new(ino: u64, mode: u16, uid: u32, gid: u32) -> Self {
        let now = Timespec::now();
        Self {
            ino,
            mode,
            nlink: 1,
            uid,
            gid,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            btime: now,
            rdev: 0,
            flags: 0,
            generation: 1,
            data_kind: DataKind::Inline,
            compression: 0,
            inline_data: Vec::new(),
            extents: Vec::new(),
            extent_tree: 0,
            extent_tree_blocks: 0,
            xattr_block: 0,
            xattr: Vec::new(),
        }
    }

    pub fn file_type(&self) -> crate::types::FileType {
        crate::types::FileType::from_mode(self.mode)
    }

    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    pub fn is_file(&self) -> bool {
        self.file_type().is_file()
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }

    pub fn uses_compression(&self) -> bool {
        self.compression != 0 || self.flags & FLAG_COMPRESSED != 0
    }

    pub fn touch_mtime(&mut self) {
        let now = Timespec::now();
        self.mtime = now;
        self.ctime = now;
    }

    pub fn touch_ctime(&mut self) {
        self.ctime = Timespec::now();
    }

    pub fn encode(&self) -> [u8; INODE_SIZE as usize] {
        let mut buf = [0u8; INODE_SIZE as usize];
        put_u16(&mut buf, 0, self.mode);
        put_u32(&mut buf, 2, self.nlink);
        put_u32(&mut buf, 6, self.uid);
        put_u32(&mut buf, 10, self.gid);
        put_u64(&mut buf, 14, self.size);
        put_u64(&mut buf, 22, self.blocks);
        put_i64(&mut buf, 30, self.atime.sec);
        put_u32(&mut buf, 38, self.atime.nsec);
        put_i64(&mut buf, 42, self.mtime.sec);
        put_u32(&mut buf, 50, self.mtime.nsec);
        put_i64(&mut buf, 54, self.ctime.sec);
        put_u32(&mut buf, 62, self.ctime.nsec);
        put_i64(&mut buf, 66, self.btime.sec);
        put_u32(&mut buf, 74, self.btime.nsec);
        put_u64(&mut buf, 78, self.rdev);
        put_u32(&mut buf, 86, self.flags);
        put_u32(&mut buf, 90, self.generation);
        buf[94] = self.data_kind as u8;
        buf[95] = self.compression;
        put_u64(&mut buf, 96, self.extent_tree);
        put_u64(&mut buf, 240, self.xattr_block);

        // Bytes 104..248: inline data or extents (144 bytes).
        // Bytes 248..252: reserved. 252..256: checksum.
        match self.data_kind {
            DataKind::Inline => {
                let n = self.inline_data.len().min(INLINE_MAX) as u16;
                put_u16(&mut buf, 104, n);
                let copy = n as usize;
                buf[106..106 + copy].copy_from_slice(&self.inline_data[..copy]);
            }
            DataKind::Extents | DataKind::ExtentTree => {
                let n = self.extents.len().min(MAX_DIRECT_EXTENTS) as u16;
                put_u16(&mut buf, 104, n);
                let mut off = 106;
                for ext in self.extents.iter().take(MAX_DIRECT_EXTENTS) {
                    put_u64(&mut buf, off, ext.logical);
                    put_u64(&mut buf, off + 8, ext.physical);
                    put_u32(&mut buf, off + 16, ext.length);
                    off += 20;
                }
            }
        }

        // Pack a few small xattrs after extents if space remains is tight;
        // full xattr set is stored in a dedicated block pointed by flags bit + extent_tree unused.
        // We store xattrs in a trailing encoded blob when they fit in leftover, else a block.
        // For encode of the inode body, xattrs go in a separate helper (see inode module).
        let csum = crc::crc32(&buf[0..252]);
        put_u32(&mut buf, 252, csum);
        buf
    }

    pub fn decode(ino: u64, buf: &[u8]) -> Result<Self> {
        if buf.len() < INODE_SIZE as usize {
            return Err(error::eio("inode truncated"));
        }
        let stored = get_u32(buf, 252);
        let calc = crc::crc32(&buf[0..252]);
        if stored != calc {
            return Err(error::eio(format!("inode {ino} checksum mismatch")));
        }
        let mode = get_u16(buf, 0);
        if mode == 0 {
            return Err(error::eio(format!("inode {ino} unused")));
        }
        let data_kind = DataKind::from_u8(buf[94]);
        let mut inode = Self {
            ino,
            mode,
            nlink: get_u32(buf, 2),
            uid: get_u32(buf, 6),
            gid: get_u32(buf, 10),
            size: get_u64(buf, 14),
            blocks: get_u64(buf, 22),
            atime: Timespec {
                sec: get_i64(buf, 30),
                nsec: get_u32(buf, 38),
            },
            mtime: Timespec {
                sec: get_i64(buf, 42),
                nsec: get_u32(buf, 50),
            },
            ctime: Timespec {
                sec: get_i64(buf, 54),
                nsec: get_u32(buf, 62),
            },
            btime: Timespec {
                sec: get_i64(buf, 66),
                nsec: get_u32(buf, 74),
            },
            rdev: get_u64(buf, 78),
            flags: get_u32(buf, 86),
            generation: get_u32(buf, 90),
            data_kind,
            compression: buf[95],
            inline_data: Vec::new(),
            extents: Vec::new(),
            extent_tree: get_u64(buf, 96),
            extent_tree_blocks: 0,
            xattr_block: get_u64(buf, 240),
            xattr: Vec::new(),
        };
        match data_kind {
            DataKind::Inline => {
                let n = get_u16(buf, 104) as usize;
                let n = n.min(INLINE_MAX);
                inode.inline_data = buf[106..106 + n].to_vec();
            }
            DataKind::Extents | DataKind::ExtentTree => {
                let n = get_u16(buf, 104) as usize;
                let n = n.min(MAX_DIRECT_EXTENTS);
                let mut off = 106;
                for _ in 0..n {
                    inode.extents.push(Extent::new(
                        get_u64(buf, off),
                        get_u64(buf, off + 8),
                        get_u32(buf, off + 16),
                    ));
                    off += 20;
                }
            }
        }
        Ok(inode)
    }
}

pub const FLAG_HAS_XATTR: u32 = 1 << 0;
#[allow(dead_code)]
pub const FLAG_IMMUTABLE: u32 = 1 << 1;
#[allow(dead_code)]
pub const FLAG_APPEND: u32 = 1 << 2;
/// Inode has (or may have) compressed records; reads must check headers.
pub const FLAG_COMPRESSED: u32 = 1 << 3;

/// Superblock feature: image understands compression records.
pub const FEATURE_COMPRESSION: u64 = 1 << 0;

pub fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
pub fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
pub fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
pub fn put_i64(buf: &mut [u8], off: usize, v: i64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
pub fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}
pub fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
pub fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}
pub fn get_i64(buf: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}
