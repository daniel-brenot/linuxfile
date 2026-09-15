//! Core filesystem engine: inodes, directories, path walk, and data I/O.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::alloc::{self, Allocator};
use crate::cache::BlockCache;
use crate::compress::{
    self, Compression, RecordHeader, DEFAULT_RECORD_BLOCKS, HEADER_SIZE,
};
use crate::error::{self, Result};
use crate::format::{
    get_u16, get_u64, DataKind, Extent, Inode, Superblock, BLOCK_SIZE, BLOCK_SIZE_U64,
    FEATURE_COMPRESSION, FIRST_USER_INO, FLAG_COMPRESSED, FLAG_HAS_XATTR, INLINE_MAX,
    INODES_PER_BLOCK, INODE_SIZE, MAGIC, MAX_DIRECT_EXTENTS, ROOT_INO, VERSION,
};
use crate::journal::{self, Journal};
use crate::path::{self, Component, UnixPath, UnixPathBuf, SYMLOOP_MAX};
use crate::store::Image;
use crate::types::{
    self, Access, Creds, FileTimes, FileType, Stat, Timespec, MAX_FILE_SIZE as MAX_SZ, MODE_PERM,
    S_IFDIR, S_ISVTX,
};
use crate::{CreateOptions, SyncMode};

pub struct Inner {
    pub image: Image,
    pub sb: Superblock,
    pub cache: BlockCache,
    pub alloc: Allocator,
    pub journal: Journal,
    pub inodes: HashMap<u64, Inode>,
    pub dirty_inodes: HashSet<u64>,
    pub free_inodes: Vec<u64>,
    pub open_count: HashMap<u64, u32>,
    pub orphans: HashSet<u64>,
    pub writable: bool,
    pub sync_mode: SyncMode,
    pub check_perm: bool,
    pub default_compression: Compression,
    pub record_blocks: u32,
    record_cache: Option<RecordCache>,
}

struct RecordCache {
    ino: u64,
    rec: u64,
    data: Vec<u8>,
    dirty: bool,
}

#[derive(Clone)]
pub struct Resolved {
    pub parent: u64,
    pub name: Vec<u8>,
    pub ino: Option<u64>,
}

impl Inner {
    pub fn create(path: impl AsRef<Path>, opts: &CreateOptions) -> Result<Self> {
        let journal_blocks = opts.journal_blocks.max(8);
        let first_data = 2 + journal_blocks; // sb + backup + journal
        let initial_blocks = opts.initial_blocks.max(first_data as u64 + 8);
        let initial_len = initial_blocks * BLOCK_SIZE_U64;
        let mut image = Image::create(path, initial_len)?;
        let record_blocks = compress::normalize_record_blocks(opts.record_blocks);
        let default_compression = opts.compression;
        let mut features = 0u64;
        if !default_compression.is_off() {
            features |= FEATURE_COMPRESSION;
        }

        let mut sb = Superblock {
            magic: MAGIC,
            version: VERSION,
            block_size: BLOCK_SIZE,
            journal_blocks,
            first_data_block: first_data,
            total_blocks: initial_blocks,
            inode_count: 0,
            free_inode_count: 0,
            next_inode: FIRST_USER_INO,
            root_ino: ROOT_INO,
            generation: 1,
            uuid: generate_uuid(),
            features,
            mtime: Timespec::now(),
            inode_table_extents: Vec::new(),
            alloc_block: 0,
            journal_seq: 0,
            journal_head: 0,
            journal_tail: 0,
            journal_committed: 0,
            default_compression: default_compression.as_u8(),
            record_blocks: record_blocks as u8,
        };

        let mut alloc = Allocator::new(first_data as u64, initial_blocks);
        // Inode table: 2 blocks (32 inodes) to start.
        let it_blocks = 2u64;
        let it_start = alloc.allocate(&mut image, it_blocks)?;
        sb.inode_table_extents.push(Extent::new(0, it_start, it_blocks as u32));

        let cache = BlockCache::new(opts.cache_blocks);
        let journal = Journal::new(&sb);

        let mut inner = Self {
            image,
            sb,
            cache,
            alloc,
            journal,
            inodes: HashMap::new(),
            dirty_inodes: HashSet::new(),
            free_inodes: Vec::new(),
            open_count: HashMap::new(),
            orphans: HashSet::new(),
            writable: true,
            sync_mode: opts.sync,
            check_perm: false,
            default_compression,
            record_blocks,
            record_cache: None,
        };

        let mut root = Inode::new(ROOT_INO, S_IFDIR | 0o755, 0, 0);
        root.compression = default_compression.as_u8();
        root.nlink = 2;
        inner.write_dir_entries(ROOT_INO, &mut root, &[(ROOT_INO, FileType::Directory, b".".to_vec()), (ROOT_INO, FileType::Directory, b"..".to_vec())])?;
        inner.inodes.insert(ROOT_INO, root);
        inner.dirty_inodes.insert(ROOT_INO);
        inner.sb.inode_count = 1;
        inner.flush_all()?;
        Ok(inner)
    }

    pub fn open(path: impl AsRef<Path>, writable: bool, cache_blocks: usize, sync: SyncMode) -> Result<Self> {
        let mut image = Image::open(path, writable)?;
        let mut sb_buf = [0u8; 4096];
        image.read_exact_at(&mut sb_buf, 0)?;
        let sb = match Superblock::decode(&sb_buf) {
            Ok(s) => s,
            Err(_) => {
                // Try backup after journal.
                let journal_blocks = crate::format::get_u32(&sb_buf, 16);
                let backup = 1 + journal_blocks as u64;
                image.read_block(backup, &mut sb_buf)?;
                Superblock::decode(&sb_buf)?
            }
        };
        let _ = journal::replay(&mut image, &sb)?;
        let alloc = if sb.alloc_block != 0 {
            alloc::read_alloc_blocks(&image, sb.alloc_block)?
        } else {
            Allocator::new(sb.first_data(), sb.total_blocks)
        };
        let default_compression = Compression::from_u8(sb.default_compression);
        let record_blocks = if sb.record_blocks == 0 {
            DEFAULT_RECORD_BLOCKS
        } else {
            compress::normalize_record_blocks(sb.record_blocks as u32)
        };
        let mut inner = Self {
            journal: Journal::new(&sb),
            cache: BlockCache::new(cache_blocks),
            image,
            sb,
            alloc,
            inodes: HashMap::new(),
            dirty_inodes: HashSet::new(),
            free_inodes: Vec::new(),
            open_count: HashMap::new(),
            orphans: HashSet::new(),
            writable,
            sync_mode: sync,
            check_perm: false,
            default_compression,
            record_blocks,
            record_cache: None,
        };
        inner.rebuild_free_inodes()?;
        Ok(inner)
    }

    fn rebuild_free_inodes(&mut self) -> Result<()> {
        let cap = self.inode_table_capacity();
        let mut used = 0u64;
        let mut free = Vec::new();
        let limit = self.sb.next_inode.min(cap);
        for ino in 1..limit {
            match self.read_inode_raw(ino) {
                Ok(Some(_)) => used += 1,
                Ok(None) => {
                    if ino >= FIRST_USER_INO {
                        free.push(ino);
                    }
                }
                Err(_) => {
                    if ino >= FIRST_USER_INO {
                        free.push(ino);
                    }
                }
            }
        }
        self.sb.inode_count = used;
        self.sb.free_inode_count = free.len() as u64;
        self.free_inodes = free;
        Ok(())
    }

    pub fn inode_table_capacity(&self) -> u64 {
        let blocks: u64 = self
            .sb
            .inode_table_extents
            .iter()
            .map(|e| e.length as u64)
            .sum();
        blocks * INODES_PER_BLOCK as u64
    }

    fn map_inode_slot(&self, ino: u64) -> Result<(u64, usize)> {
        let block_index = ino / INODES_PER_BLOCK as u64;
        let slot = (ino % INODES_PER_BLOCK as u64) as usize;
        let phys = map_extents(&self.sb.inode_table_extents, block_index)
            .ok_or_else(|| error::eio("inode table mapping failed"))?;
        Ok((phys, slot * INODE_SIZE as usize))
    }

    fn grow_inode_table(&mut self) -> Result<()> {
        let add = 2u64;
        let start = self.alloc.allocate(&mut self.image, add)?;
        let logical = self
            .sb
            .inode_table_extents
            .last()
            .map(|e| e.logical_end())
            .unwrap_or(0);
        if let Some(last) = self.sb.inode_table_extents.last_mut() {
            if last.physical_end() == start {
                last.length += add as u32;
                return Ok(());
            }
        }
        self.sb.inode_table_extents.push(Extent::new(logical, start, add as u32));
        Ok(())
    }

    fn read_inode_raw(&mut self, ino: u64) -> Result<Option<Inode>> {
        if ino == 0 || ino >= self.inode_table_capacity() {
            return Ok(None);
        }
        let (phys, off) = self.map_inode_slot(ino)?;
        let block = *self.cache.get(&mut self.image, phys)?;
        let slice = &block[off..off + INODE_SIZE as usize];
        if slice.iter().all(|&b| b == 0) || crate::format::get_u16(slice, 0) == 0 {
            return Ok(None);
        }
        match Inode::decode(ino, slice) {
            Ok(mut inode) => {
                self.load_xattrs(&mut inode)?;
                if inode.data_kind == DataKind::ExtentTree {
                    self.load_extent_tree(&mut inode)?;
                }
                if inode.uses_compression() {
                    self.resolve_all_extents(&mut inode)?;
                }
                Ok(Some(inode))
            }
            Err(_) => Ok(None),
        }
    }

    pub fn get_inode(&mut self, ino: u64) -> Result<Inode> {
        if let Some(i) = self.inodes.get(&ino) {
            return Ok(i.clone());
        }
        let inode = self
            .read_inode_raw(ino)?
            .ok_or_else(|| error::eio(format!("missing inode {ino}")))?;
        self.inodes.insert(ino, inode.clone());
        Ok(inode)
    }

    pub fn get_inode_mut(&mut self, ino: u64) -> Result<&mut Inode> {
        if !self.inodes.contains_key(&ino) {
            let inode = self
                .read_inode_raw(ino)?
                .ok_or_else(|| error::eio(format!("missing inode {ino}")))?;
            self.inodes.insert(ino, inode);
        }
        self.dirty_inodes.insert(ino);
        Ok(self.inodes.get_mut(&ino).unwrap())
    }

    fn write_inode_to_table(&mut self, inode: &Inode) -> Result<()> {
        while inode.ino >= self.inode_table_capacity() {
            self.grow_inode_table()?;
        }
        let (phys, off) = self.map_inode_slot(inode.ino)?;
        let block = self.cache.get_mut(&mut self.image, phys)?;
        let raw = inode.encode();
        block[off..off + INODE_SIZE as usize].copy_from_slice(&raw);
        Ok(())
    }

    fn persist_inode(&mut self, ino: u64) -> Result<()> {
        let inode = match self.inodes.get(&ino).cloned() {
            Some(i) => i,
            None => return Ok(()),
        };
        self.store_xattrs(&inode)?;
        self.store_extent_tree(&inode)?;
        // Re-fetch after xattr/extent may have mutated inode.
        let inode = self.inodes.get(&ino).cloned().unwrap_or(inode);
        self.write_inode_to_table(&inode)?;
        Ok(())
    }

    pub fn alloc_inode(&mut self, mode: u16, uid: u32, gid: u32) -> Result<u64> {
        self.require_write()?;
        let ino = if let Some(ino) = self.free_inodes.pop() {
            self.sb.free_inode_count = self.free_inodes.len() as u64;
            ino
        } else {
            if self.sb.next_inode >= self.inode_table_capacity() {
                self.grow_inode_table()?;
            }
            let ino = self.sb.next_inode;
            self.sb.next_inode += 1;
            ino
        };
        let mut inode = Inode::new(ino, mode, uid, gid);
        inode.compression = self.default_compression.as_u8();
        self.inodes.insert(ino, inode);
        self.dirty_inodes.insert(ino);
        self.sb.inode_count += 1;
        Ok(ino)
    }

    pub fn drop_inode(&mut self, ino: u64) -> Result<()> {
        self.invalidate_record_cache(ino);
        let inode = self.get_inode(ino)?;
        self.truncate_data(ino, 0)?;
        if inode.xattr_block != 0 {
            self.alloc.free(inode.xattr_block, 1);
        }
        let tree_blocks = inode.extent_tree_blocks.max(1);
        if inode.extent_tree != 0 && inode.data_kind == DataKind::ExtentTree {
            self.alloc.free(inode.extent_tree, tree_blocks);
        }
        self.inodes.remove(&ino);
        self.dirty_inodes.remove(&ino);
        // Zero the table slot.
        if ino < self.inode_table_capacity() {
            let (phys, off) = self.map_inode_slot(ino)?;
            let block = self.cache.get_mut(&mut self.image, phys)?;
            block[off..off + INODE_SIZE as usize].fill(0);
        }
        if ino >= FIRST_USER_INO {
            self.free_inodes.push(ino);
            self.sb.free_inode_count = self.free_inodes.len() as u64;
        }
        self.sb.inode_count = self.sb.inode_count.saturating_sub(1);
        Ok(())
    }

    fn require_write(&self) -> Result<()> {
        if !self.writable {
            Err(error::Error::from_errno(error::EROFS))
        } else {
            Ok(())
        }
    }

    fn access_ok(&self, inode: &Inode, creds: &Creds, acc: Access) -> Result<()> {
        if !self.check_perm {
            return Ok(());
        }
        if types::check_access(inode.mode, inode.uid, inode.gid, creds, acc) {
            Ok(())
        } else {
            Err(error::eacces("permission denied"))
        }
    }

    // --- extents / file data -------------------------------------------------

    fn load_extent_tree(&mut self, inode: &mut Inode) -> Result<()> {
        if inode.extent_tree == 0 {
            return Ok(());
        }
        let first = *self.cache.get(&mut self.image, inode.extent_tree)?;
        let n = crate::format::get_u32(&first, 0) as usize;
        inode.extents.clear();
        if &first[8..12] == b"EXT2" {
            let nblocks = crate::format::get_u32(&first, 4).max(1) as u64;
            inode.extent_tree_blocks = nblocks;
            let per = (BLOCK_SIZE as usize - 16) / 20;
            let mut remaining = n;
            for b in 0..nblocks {
                let block = if b == 0 {
                    first
                } else {
                    *self.cache.get(&mut self.image, inode.extent_tree + b)?
                };
                let mut off = if b == 0 { 16 } else { 0 };
                while remaining > 0 && off + 20 <= BLOCK_SIZE as usize {
                    inode.extents.push(Extent::new(
                        crate::format::get_u64(&block, off),
                        crate::format::get_u64(&block, off + 8),
                        crate::format::get_u32(&block, off + 16),
                    ));
                    off += 20;
                    remaining -= 1;
                    if b > 0 && off + 20 > BLOCK_SIZE as usize {
                        break;
                    }
                    if b == 0 && off >= 16 + per * 20 {
                        break;
                    }
                }
            }
        } else {
            inode.extent_tree_blocks = 1;
            let mut off = 8;
            for _ in 0..n.min(200) {
                if off + 20 > BLOCK_SIZE as usize {
                    break;
                }
                inode.extents.push(Extent::new(
                    crate::format::get_u64(&first, off),
                    crate::format::get_u64(&first, off + 8),
                    crate::format::get_u32(&first, off + 16),
                ));
                off += 20;
            }
        }
        Ok(())
    }

    fn store_extent_tree(&mut self, inode: &Inode) -> Result<()> {
        let old_tree = inode.extent_tree;
        let old_n = if old_tree != 0 {
            inode.extent_tree_blocks.max(1)
        } else {
            0
        };
        if inode.extents.len() <= MAX_DIRECT_EXTENTS {
            if old_tree != 0 {
                self.alloc.free(old_tree, old_n);
                if let Some(i) = self.inodes.get_mut(&inode.ino) {
                    i.extent_tree = 0;
                    i.extent_tree_blocks = 0;
                    i.data_kind = DataKind::Extents;
                }
            }
            return Ok(());
        }
        let per = (BLOCK_SIZE as usize - 16) / 20;
        let nblocks = (inode.extents.len().div_ceil(per)).max(1) as u64;
        if old_tree != 0 {
            self.alloc.free(old_tree, old_n);
        }
        let tree = self.alloc.allocate(&mut self.image, nblocks)?;
        let mut remaining = inode.extents.as_slice();
        for b in 0..nblocks {
            let mut block = [0u8; BLOCK_SIZE as usize];
            let take = remaining.len().min(if b == 0 { per } else { BLOCK_SIZE as usize / 20 });
            if b == 0 {
                crate::format::put_u32(&mut block, 0, inode.extents.len() as u32);
                crate::format::put_u32(&mut block, 4, nblocks as u32);
                block[8..12].copy_from_slice(b"EXT2");
            }
            let mut off = if b == 0 { 16 } else { 0 };
            for ext in &remaining[..take] {
                if off + 20 > BLOCK_SIZE as usize {
                    break;
                }
                crate::format::put_u64(&mut block, off, ext.logical);
                crate::format::put_u64(&mut block, off + 8, ext.physical);
                crate::format::put_u32(&mut block, off + 16, ext.length);
                off += 20;
            }
            remaining = &remaining[take.min(remaining.len())..];
            self.cache.insert_dirty(&mut self.image, tree + b, block)?;
        }
        if let Some(i) = self.inodes.get_mut(&inode.ino) {
            i.extent_tree = tree;
            i.extent_tree_blocks = nblocks;
            i.data_kind = DataKind::ExtentTree;
        }
        Ok(())
    }

    fn load_xattrs(&mut self, inode: &mut Inode) -> Result<()> {
        if inode.xattr_block == 0 {
            return Ok(());
        }
        let block = *self.cache.get(&mut self.image, inode.xattr_block)?;
        inode.xattr = decode_xattrs(&block);
        Ok(())
    }

    fn store_xattrs(&mut self, inode: &Inode) -> Result<()> {
        if inode.xattr.is_empty() {
            if inode.xattr_block != 0 {
                self.alloc.free(inode.xattr_block, 1);
                if let Some(i) = self.inodes.get_mut(&inode.ino) {
                    i.xattr_block = 0;
                    i.flags &= !FLAG_HAS_XATTR;
                }
            }
            return Ok(());
        }
        let blob = encode_xattrs(&inode.xattr);
        if blob.len() > BLOCK_SIZE as usize {
            return Err(error::Error::from_errno(error::ERANGE));
        }
        let mut block = [0u8; BLOCK_SIZE as usize];
        block[..blob.len()].copy_from_slice(&blob);
        let mut xb = inode.xattr_block;
        if xb == 0 {
            xb = self.alloc.allocate(&mut self.image, 1)?;
            if let Some(i) = self.inodes.get_mut(&inode.ino) {
                i.xattr_block = xb;
                i.flags |= FLAG_HAS_XATTR;
            }
        }
        self.cache.insert_dirty(&mut self.image, xb, block)?;
        Ok(())
    }

    pub fn map_file_block(&self, inode: &Inode, logical: u64) -> Option<u64> {
        for ext in &inode.extents {
            if let Some(p) = ext.map(logical) {
                return Some(p);
            }
        }
        None
    }

    fn add_extent(
        &mut self,
        ino: u64,
        logical: u64,
        physical: u64,
        length: u32,
        phys_length: u32,
        compress: u8,
    ) -> Result<()> {
        let inode = self.get_inode_mut(ino)?;
        let merge = compress == 0
            && phys_length == length
            && inode
                .extents
                .last()
                .map(|last| {
                    last.compress == 0
                        && last.logical_end() == logical
                        && last.physical + last.phys_len() == physical
                })
                .unwrap_or(false);
        if merge {
            if let Some(last) = inode.extents.last_mut() {
                last.length += length;
                if last.phys_length != 0 {
                    last.phys_length += phys_length;
                }
            }
        } else {
            inode.extents.push(Extent::with_phys(
                logical,
                physical,
                length,
                phys_length,
                compress,
            ));
            inode.extents.sort_by_key(|e| e.logical);
        }
        inode.data_kind = if inode.extents.len() > MAX_DIRECT_EXTENTS {
            DataKind::ExtentTree
        } else {
            DataKind::Extents
        };
        Ok(())
    }

    fn promote_inline(&mut self, ino: u64) -> Result<()> {
        let inode = self.get_inode(ino)?;
        if inode.data_kind != DataKind::Inline || inode.inline_data.is_empty() {
            if inode.data_kind == DataKind::Inline {
                let inode = self.get_inode_mut(ino)?;
                inode.data_kind = DataKind::Extents;
                inode.inline_data.clear();
            }
            return Ok(());
        }
        let data = inode.inline_data.clone();
        let need_blocks = (data.len() as u64 + BLOCK_SIZE_U64 - 1) / BLOCK_SIZE_U64;
        let need_blocks = need_blocks.max(1);
        let phys = self.alloc.allocate(&mut self.image, need_blocks)?;
        for i in 0..need_blocks {
            let mut blk = [0u8; BLOCK_SIZE as usize];
            let src = (i * BLOCK_SIZE_U64) as usize;
            if src < data.len() {
                let n = (data.len() - src).min(BLOCK_SIZE as usize);
                blk[..n].copy_from_slice(&data[src..src + n]);
            }
            self.cache.insert_dirty(&mut self.image, phys + i, blk)?;
        }
        {
            let inode = self.get_inode_mut(ino)?;
            inode.inline_data.clear();
            inode.data_kind = DataKind::Extents;
            inode.extents.clear();
            inode.blocks = need_blocks * (BLOCK_SIZE_U64 / 512);
        }
        self.add_extent(ino, 0, phys, need_blocks as u32, need_blocks as u32, 0)?;
        Ok(())
    }

    fn allocate_logical(&mut self, ino: u64, logical: u64) -> Result<u64> {
        let inode = self.get_inode(ino)?;
        if let Some(p) = self.map_file_block(&inode, logical) {
            return Ok(p);
        }
        if inode.data_kind == DataKind::Inline {
            self.promote_inline(ino)?;
        }
        let phys = self.alloc.allocate(&mut self.image, 1)?;
        self.add_extent(ino, logical, phys, 1, 1, 0)?;
        let inode = self.get_inode_mut(ino)?;
        inode.blocks += BLOCK_SIZE_U64 / 512;
        Ok(phys)
    }

    fn record_bytes(&self) -> u64 {
        self.record_blocks as u64 * BLOCK_SIZE_U64
    }

    fn record_index(&self, offset: u64) -> u64 {
        offset / self.record_bytes()
    }

    fn invalidate_record_cache(&mut self, ino: u64) {
        if self.record_cache.as_ref().is_some_and(|c| c.ino == ino) {
            self.record_cache = None;
        }
    }

    pub(crate) fn flush_record_cache(&mut self) -> Result<()> {
        let Some(cache) = self.record_cache.take() else {
            return Ok(());
        };
        if cache.dirty {
            self.store_record(cache.ino, cache.rec, &cache.data)?;
        }
        Ok(())
    }

    fn resolve_all_extents(&mut self, inode: &mut Inode) -> Result<()> {
        for ext in &mut inode.extents {
            if ext.phys_length != 0 || ext.physical == 0 {
                continue;
            }
            let block = *self.cache.get(&mut self.image, ext.physical)?;
            if let Some(h) = RecordHeader::parse(&block) {
                ext.phys_length = h.phys_blocks();
                ext.compress = h.algo.as_u8();
            } else {
                ext.phys_length = ext.length;
            }
        }
        Ok(())
    }

    fn free_extent_physical(&mut self, ext: &Extent) {
        self.alloc.free(ext.physical, ext.phys_len());
    }

    fn punch_logical_range(&mut self, ino: u64, start: u64, end: u64) -> Result<()> {
        if start >= end {
            return Ok(());
        }
        let inode = self.get_inode(ino)?;
        let extents = inode.extents.clone();
        let mut kept = Vec::new();
        for ext in extents {
            let e0 = ext.logical;
            let e1 = ext.logical_end();
            if e1 <= start || e0 >= end {
                kept.push(ext);
                continue;
            }
            if ext.is_compressed() {
                self.free_extent_physical(&ext);
                continue;
            }
            if e0 < start {
                let left = (start - e0) as u32;
                kept.push(Extent::with_phys(
                    e0,
                    ext.physical,
                    left,
                    left,
                    0,
                ));
            }
            let mid0 = start.max(e0);
            let mid1 = end.min(e1);
            if mid1 > mid0 {
                self.alloc.free(ext.physical + (mid0 - e0), mid1 - mid0);
            }
            if e1 > end {
                let right = (e1 - end) as u32;
                kept.push(Extent::with_phys(
                    end,
                    ext.physical + (end - e0),
                    right,
                    right,
                    0,
                ));
            }
        }
        let inode = self.get_inode_mut(ino)?;
        inode.extents = kept;
        inode.extents.sort_by_key(|e| e.logical);
        inode.blocks = inode
            .extents
            .iter()
            .map(|e| e.phys_len() * (BLOCK_SIZE_U64 / 512))
            .sum();
        inode.data_kind = if inode.extents.len() > MAX_DIRECT_EXTENTS {
            DataKind::ExtentTree
        } else {
            DataKind::Extents
        };
        Ok(())
    }

    fn write_physical_bytes(&mut self, phys: u64, data: &[u8]) -> Result<()> {
        let nblocks = (data.len() as u64).div_ceil(BLOCK_SIZE_U64).max(1);
        for i in 0..nblocks {
            let mut blk = [0u8; BLOCK_SIZE as usize];
            let src = (i * BLOCK_SIZE_U64) as usize;
            if src < data.len() {
                let n = (data.len() - src).min(BLOCK_SIZE as usize);
                blk[..n].copy_from_slice(&data[src..src + n]);
            }
            self.cache.insert_dirty(&mut self.image, phys + i, blk)?;
        }
        Ok(())
    }

    fn store_record(&mut self, ino: u64, rec: u64, data: &[u8]) -> Result<()> {
        let rec_blocks = self.record_blocks as u64;
        let logical = rec * rec_blocks;
        if data.iter().all(|&b| b == 0) {
            self.punch_logical_range(ino, logical, logical + rec_blocks)?;
            return Ok(());
        }
        let logical_len = (data.len() as u64).div_ceil(BLOCK_SIZE_U64).max(1) as u32;
        let algo = Compression::from_u8(self.get_inode(ino)?.compression);
        let (used, payload) = compress::try_compress(algo, data);
        if !used.is_off() {
            let hdr = RecordHeader {
                algo: used,
                logical_blocks: logical_len as u16,
                payload_len: payload.len() as u32,
                uncompressed_len: data.len() as u32,
                crc: compress::payload_crc(&payload),
            };
            let phys_len = hdr.phys_blocks();
            let mut raw = vec![0u8; phys_len as usize * BLOCK_SIZE as usize];
            raw[..HEADER_SIZE].copy_from_slice(&hdr.encode());
            raw[HEADER_SIZE..HEADER_SIZE + payload.len()].copy_from_slice(&payload);
            let phys = self.alloc.allocate(&mut self.image, phys_len as u64)?;
            self.write_physical_bytes(phys, &raw)?;
            self.punch_logical_range(ino, logical, logical + rec_blocks)?;
            self.add_extent(ino, logical, phys, logical_len, phys_len, used.as_u8())?;
            let inode = self.get_inode_mut(ino)?;
            inode.flags |= FLAG_COMPRESSED;
        } else {
            let phys = self.alloc.allocate(&mut self.image, logical_len as u64)?;
            self.write_physical_bytes(phys, data)?;
            self.punch_logical_range(ino, logical, logical + rec_blocks)?;
            self.add_extent(ino, logical, phys, logical_len, logical_len, 0)?;
        }
        let inode = self.get_inode_mut(ino)?;
        inode.blocks = inode
            .extents
            .iter()
            .map(|e| e.phys_len() * (BLOCK_SIZE_U64 / 512))
            .sum();
        Ok(())
    }

    fn read_compressed_extent(&mut self, ext: &Extent) -> Result<Vec<u8>> {
        let phys_len = ext.phys_len().max(1);
        let mut raw = vec![0u8; phys_len as usize * BLOCK_SIZE as usize];
        for i in 0..phys_len {
            let block = *self.cache.get(&mut self.image, ext.physical + i)?;
            let dst = (i as usize) * BLOCK_SIZE as usize;
            raw[dst..dst + BLOCK_SIZE as usize].copy_from_slice(&block);
        }
        let hdr = RecordHeader::parse(&raw).ok_or_else(|| error::eio("bad compressed record"))?;
        let end = HEADER_SIZE + hdr.payload_len as usize;
        if end > raw.len() {
            return Err(error::eio("compressed record truncated"));
        }
        let payload = &raw[HEADER_SIZE..end];
        if compress::payload_crc(payload) != hdr.crc {
            return Err(error::eio("compressed record checksum mismatch"));
        }
        compress::decompress(hdr.algo, payload, hdr.uncompressed_len as usize)
    }

    fn load_record(&mut self, ino: u64, rec: u64) -> Result<Vec<u8>> {
        let rec_blocks = self.record_blocks as u64;
        let rec_bytes = self.record_bytes() as usize;
        let logical = rec * rec_blocks;
        let inode = self.get_inode(ino)?;
        let mut buf = vec![0u8; rec_bytes];
        if let Some(ext) = inode
            .extents
            .iter()
            .copied()
            .find(|e| e.contains_logical(logical))
        {
            if ext.is_compressed() {
                let data = self.read_compressed_extent(&ext)?;
                let n = data.len().min(rec_bytes);
                buf[..n].copy_from_slice(&data[..n]);
                return Ok(buf);
            }
        }
        let file_off = logical * BLOCK_SIZE_U64;
        if file_off < inode.size {
            let n = ((inode.size - file_off) as usize).min(rec_bytes);
            self.read_data_raw(ino, file_off, &mut buf[..n])?;
        }
        Ok(buf)
    }

    fn cached_record(&mut self, ino: u64, rec: u64) -> Result<&mut RecordCache> {
        let hit = self
            .record_cache
            .as_ref()
            .is_some_and(|c| c.ino == ino && c.rec == rec);
        if !hit {
            self.flush_record_cache()?;
            let data = self.load_record(ino, rec)?;
            self.record_cache = Some(RecordCache {
                ino,
                rec,
                data,
                dirty: false,
            });
        }
        Ok(self.record_cache.as_mut().unwrap())
    }

    fn read_data_raw(&mut self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inode = self.get_inode(ino)?;
        if offset >= inode.size {
            return Ok(0);
        }
        let n = ((inode.size - offset) as usize).min(buf.len());
        if n == 0 {
            return Ok(0);
        }
        if inode.data_kind == DataKind::Inline {
            let start = offset as usize;
            buf[..n].copy_from_slice(&inode.inline_data[start..start + n]);
            return Ok(n);
        }
        let mut done = 0;
        while done < n {
            let pos = offset + done as u64;
            let logical = pos / BLOCK_SIZE_U64;
            let within = (pos % BLOCK_SIZE_U64) as usize;
            let chunk = (BLOCK_SIZE as usize - within).min(n - done);
            match self.map_file_block(&inode, logical) {
                Some(phys) => {
                    let block = *self.cache.get(&mut self.image, phys)?;
                    buf[done..done + chunk].copy_from_slice(&block[within..within + chunk]);
                }
                None => buf[done..done + chunk].fill(0),
            }
            done += chunk;
        }
        Ok(n)
    }

    fn write_data_raw(&mut self, ino: u64, offset: u64, buf: &[u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let inode = self.get_inode(ino)?;
        if inode.data_kind == DataKind::Inline {
            self.promote_inline(ino)?;
        }
        let mut done = 0;
        while done < buf.len() {
            let pos = offset + done as u64;
            let logical = pos / BLOCK_SIZE_U64;
            let within = (pos % BLOCK_SIZE_U64) as usize;
            let chunk = (BLOCK_SIZE as usize - within).min(buf.len() - done);
            let phys = self.allocate_logical(ino, logical)?;
            let block = self.cache.get_mut(&mut self.image, phys)?;
            block[within..within + chunk].copy_from_slice(&buf[done..done + chunk]);
            done += chunk;
        }
        Ok(buf.len())
    }

    fn write_data_compressed(&mut self, ino: u64, offset: u64, buf: &[u8]) -> Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            let pos = offset + done as u64;
            let rec = self.record_index(pos);
            let rec_bytes = self.record_bytes();
            let within = (pos % rec_bytes) as usize;
            let chunk = (rec_bytes as usize - within).min(buf.len() - done);
            {
                let cache = self.cached_record(ino, rec)?;
                let need = within + chunk;
                if cache.data.len() < need {
                    cache.data.resize(need, 0);
                }
                cache.data[within..within + chunk].copy_from_slice(&buf[done..done + chunk]);
                cache.dirty = true;
            }
            done += chunk;
        }
        Ok(buf.len())
    }

    pub fn read_data(&mut self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inode = self.get_inode(ino)?;
        if offset >= inode.size {
            return Ok(0);
        }
        let n = ((inode.size - offset) as usize).min(buf.len());
        if n == 0 {
            return Ok(0);
        }
        if inode.data_kind == DataKind::Inline {
            let start = offset as usize;
            buf[..n].copy_from_slice(&inode.inline_data[start..start + n]);
            return Ok(n);
        }
        if inode.uses_compression() {
            let mut done = 0;
            while done < n {
                let pos = offset + done as u64;
                let rec = self.record_index(pos);
                let rec_bytes = self.record_bytes();
                let within = (pos % rec_bytes) as usize;
                let chunk = (rec_bytes as usize - within).min(n - done);
                let cache = self.cached_record(ino, rec)?;
                let avail = cache.data.len().saturating_sub(within);
                let take = chunk.min(avail);
                if take > 0 {
                    buf[done..done + take].copy_from_slice(&cache.data[within..within + take]);
                }
                if take < chunk {
                    buf[done + take..done + chunk].fill(0);
                }
                done += chunk;
            }
            return Ok(n);
        }
        self.read_data_raw(ino, offset, buf)
    }

    pub fn write_data(&mut self, ino: u64, offset: u64, buf: &[u8]) -> Result<usize> {
        self.require_write()?;
        if buf.is_empty() {
            return Ok(0);
        }
        let end = offset.saturating_add(buf.len() as u64);
        if end > MAX_SZ {
            return Err(error::Error::from_errno(error::EFBIG));
        }
        let inode = self.get_inode(ino)?;
        if inode.data_kind == DataKind::Inline
            && end as usize <= INLINE_MAX
            && inode.extents.is_empty()
        {
            let inode = self.get_inode_mut(ino)?;
            if inode.inline_data.len() < end as usize {
                inode.inline_data.resize(end as usize, 0);
            }
            let start = offset as usize;
            inode.inline_data[start..start + buf.len()].copy_from_slice(buf);
            if end > inode.size {
                inode.size = end;
            }
            inode.touch_mtime();
            return Ok(buf.len());
        }
        if inode.uses_compression() {
            if inode.data_kind == DataKind::Inline {
                let mut full = inode.inline_data.clone();
                if full.len() < end as usize {
                    full.resize(end as usize, 0);
                }
                full[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
                {
                    let inode = self.get_inode_mut(ino)?;
                    inode.inline_data.clear();
                    inode.data_kind = DataKind::Extents;
                    inode.size = end.max(inode.size);
                    inode.touch_mtime();
                }
                let mut off = 0u64;
                while off < full.len() as u64 {
                    let rec = self.record_index(off);
                    let rec_bytes = self.record_bytes() as usize;
                    let start = off as usize;
                    let n = (full.len() - start).min(rec_bytes);
                    self.store_record(ino, rec, &full[start..start + n])?;
                    off += n as u64;
                }
                return Ok(buf.len());
            }
            self.write_data_compressed(ino, offset, buf)?;
            let inode = self.get_inode_mut(ino)?;
            if end > inode.size {
                inode.size = end;
            }
            inode.touch_mtime();
            return Ok(buf.len());
        }
        if inode.data_kind == DataKind::Inline {
            self.promote_inline(ino)?;
        }
        self.write_data_raw(ino, offset, buf)?;
        let inode = self.get_inode_mut(ino)?;
        if end > inode.size {
            inode.size = end;
        }
        inode.touch_mtime();
        Ok(buf.len())
    }

    pub fn truncate_data(&mut self, ino: u64, new_size: u64) -> Result<()> {
        self.require_write()?;
        if new_size > MAX_SZ {
            return Err(error::Error::from_errno(error::EFBIG));
        }
        self.flush_record_cache()?;
        let inode = self.get_inode(ino)?;
        if new_size == inode.size {
            return Ok(());
        }
        if inode.data_kind == DataKind::Inline {
            let inode = self.get_inode_mut(ino)?;
            inode.inline_data.resize(new_size as usize, 0);
            inode.size = new_size;
            inode.touch_mtime();
            return Ok(());
        }
        if new_size < inode.size {
            if inode.uses_compression() {
                let rec_blocks = self.record_blocks as u64;
                if new_size == 0 {
                    let extents = inode.extents.clone();
                    for ext in extents {
                        self.free_extent_physical(&ext);
                    }
                    let inode = self.get_inode_mut(ino)?;
                    inode.extents.clear();
                    inode.size = 0;
                    inode.blocks = 0;
                    inode.data_kind = DataKind::Inline;
                    inode.inline_data.clear();
                    inode.touch_mtime();
                    return Ok(());
                }
                let last_off = new_size - 1;
                let last_rec = self.record_index(last_off);
                let rec_start = last_rec * rec_blocks;
                let keep = (new_size - rec_start * BLOCK_SIZE_U64) as usize;
                let mut rec_data = self.load_record(ino, last_rec)?;
                rec_data.truncate(keep);
                self.store_record(ino, last_rec, &rec_data)?;
                self.punch_logical_range(ino, rec_start + rec_blocks, u64::MAX / 2)?;
                let inode = self.get_inode_mut(ino)?;
                inode.size = new_size;
                inode.touch_mtime();
            } else {
                let first_drop = new_size.div_ceil(BLOCK_SIZE_U64);
                let extents = inode.extents.clone();
                let mut kept = Vec::new();
                for ext in extents {
                    if ext.logical_end() <= first_drop {
                        kept.push(ext);
                        continue;
                    }
                    if ext.logical >= first_drop {
                        self.free_extent_physical(&ext);
                        continue;
                    }
                    let keep_len = (first_drop - ext.logical) as u32;
                    let drop_len = ext.length - keep_len;
                    self.alloc
                        .free(ext.physical + keep_len as u64, drop_len as u64);
                    kept.push(Extent::with_phys(
                        ext.logical,
                        ext.physical,
                        keep_len,
                        keep_len,
                        0,
                    ));
                }
                let inode = self.get_inode_mut(ino)?;
                inode.extents = kept;
                inode.size = new_size;
                inode.blocks = inode
                    .extents
                    .iter()
                    .map(|e| e.phys_len() * (BLOCK_SIZE_U64 / 512))
                    .sum();
                inode.touch_mtime();
            }
            let inode = self.get_inode(ino)?;
            if new_size as usize <= INLINE_MAX && inode.extents.len() <= 1 {
                let mut tmp = vec![0u8; new_size as usize];
                let _ = self.read_data(ino, 0, &mut tmp)?;
                self.flush_record_cache()?;
                let extents = self.get_inode(ino)?.extents.clone();
                for ext in extents {
                    self.free_extent_physical(&ext);
                }
                let inode = self.get_inode_mut(ino)?;
                inode.extents.clear();
                inode.data_kind = DataKind::Inline;
                inode.inline_data = tmp;
                inode.blocks = 0;
                inode.touch_mtime();
            }
        } else {
            let inode = self.get_inode_mut(ino)?;
            inode.size = new_size;
            inode.touch_mtime();
        }
        Ok(())
    }

    pub fn set_inode_compression(&mut self, ino: u64, compression: Compression) -> Result<()> {
        self.require_write()?;
        self.flush_record_cache()?;
        let inode = self.get_inode_mut(ino)?;
        inode.compression = compression.as_u8();
        if !compression.is_off() {
            inode.flags |= FLAG_COMPRESSED;
        }
        inode.touch_ctime();
        Ok(())
    }

    // --- directories ---------------------------------------------------------

    fn parse_dir(&mut self, ino: u64) -> Result<Vec<(u64, FileType, Vec<u8>)>> {
        let inode = self.get_inode(ino)?;
        if !inode.is_dir() {
            return Err(error::enotdir("not a directory"));
        }
        let mut raw = vec![0u8; inode.size as usize];
        if inode.size > 0 {
            self.read_data(ino, 0, &mut raw)?;
        }
        decode_dir(&raw)
    }

    fn write_dir_entries(
        &mut self,
        ino: u64,
        inode: &mut Inode,
        entries: &[(u64, FileType, Vec<u8>)],
    ) -> Result<()> {
        let raw = encode_dir(entries);
        self.inodes.insert(ino, inode.clone());
        self.dirty_inodes.insert(ino);
        self.truncate_data(ino, 0)?;
        self.write_data(ino, 0, &raw)?;
        if let Some(i) = self.inodes.get(&ino) {
            *inode = i.clone();
        }
        Ok(())
    }

    pub fn dir_lookup(&mut self, dir_ino: u64, name: &[u8]) -> Result<Option<(u64, FileType)>> {
        for (ino, ft, n) in self.parse_dir(dir_ino)? {
            if n == name {
                return Ok(Some((ino, ft)));
            }
        }
        Ok(None)
    }

    pub fn dir_insert(
        &mut self,
        dir_ino: u64,
        name: &[u8],
        child: u64,
        ft: FileType,
    ) -> Result<()> {
        path::validate_name(name)?;
        let mut entries = self.parse_dir(dir_ino)?;
        if entries.iter().any(|(_, _, n)| n == name) {
            return Err(error::eexist("file exists"));
        }
        entries.push((child, ft, name.to_vec()));
        let mut inode = self.get_inode(dir_ino)?;
        self.write_dir_entries(dir_ino, &mut inode, &entries)?;
        let inode = self.get_inode_mut(dir_ino)?;
        inode.touch_mtime();
        Ok(())
    }

    pub fn dir_remove(&mut self, dir_ino: u64, name: &[u8]) -> Result<(u64, FileType)> {
        let mut entries = self.parse_dir(dir_ino)?;
        let pos = entries
            .iter()
            .position(|(_, _, n)| n == name)
            .ok_or_else(|| error::enoent("no such file or directory"))?;
        let (ino, ft, _) = entries.remove(pos);
        let mut inode = self.get_inode(dir_ino)?;
        self.write_dir_entries(dir_ino, &mut inode, &entries)?;
        let inode = self.get_inode_mut(dir_ino)?;
        inode.touch_mtime();
        Ok((ino, ft))
    }

    pub fn read_dir_entries(&mut self, dir_ino: u64) -> Result<Vec<(u64, FileType, Vec<u8>)>> {
        self.parse_dir(dir_ino)
    }

    // --- path walk -----------------------------------------------------------

    pub fn walk(
        &mut self,
        root: u64,
        cwd: u64,
        path: &UnixPath,
        follow_last: bool,
        creds: &Creds,
    ) -> Result<Resolved> {
        if path.is_empty() {
            return Err(error::einval("empty path"));
        }
        let mut dir = if path.is_absolute() { root } else { cwd };
        let comps: Vec<Component> = path.components().collect();
        if comps.is_empty() {
            return Ok(Resolved {
                parent: root,
                name: Vec::new(),
                ino: Some(dir),
            });
        }
        let mut hops = 0u32;
        let mut i = 0;
        while i < comps.len() {
            let last = i + 1 == comps.len();
            match comps[i] {
                Component::RootDir => {
                    dir = root;
                    i += 1;
                    continue;
                }
                Component::CurDir => {
                    i += 1;
                    continue;
                }
                Component::ParentDir => {
                    let inode = self.get_inode(dir)?;
                    self.access_ok(&inode, creds, Access::Exec)?;
                    if dir != root {
                        if let Some((p, _)) = self.dir_lookup(dir, b"..")? {
                            dir = p;
                        }
                    }
                    if last {
                        return Ok(Resolved {
                            parent: dir,
                            name: b"..".to_vec(),
                            ino: Some(dir),
                        });
                    }
                    i += 1;
                    continue;
                }
                Component::Normal(name) => {
                    let inode = self.get_inode(dir)?;
                    if !inode.is_dir() {
                        return Err(error::enotdir("not a directory"));
                    }
                    self.access_ok(&inode, creds, Access::Exec)?;
                    match self.dir_lookup(dir, name)? {
                        None => {
                            if last {
                                return Ok(Resolved {
                                    parent: dir,
                                    name: name.to_vec(),
                                    ino: None,
                                });
                            }
                            return Err(error::enoent("no such file or directory"));
                        }
                        Some((ino, _)) => {
                            let child = self.get_inode(ino)?;
                            if child.is_symlink() && (!last || follow_last) {
                                hops += 1;
                                if hops as usize > SYMLOOP_MAX {
                                    return Err(error::eloop());
                                }
                                let mut target = vec![0u8; child.size as usize];
                                self.read_data(ino, 0, &mut target)?;
                                let tpath = UnixPath::from_bytes(&target);
                                let rest = {
                                    let mut buf = tpath.to_path_buf();
                                    for c in &comps[i + 1..] {
                                        match c {
                                            Component::Normal(n) => buf.push(UnixPath::from_bytes(n)),
                                            Component::ParentDir => {
                                                buf.push(UnixPath::from_bytes(b".."));
                                            }
                                            Component::CurDir => {}
                                            Component::RootDir => {
                                                buf = UnixPathBuf::from("/");
                                            }
                                        }
                                    }
                                    buf
                                };
                                return self.walk(root, dir, &rest, follow_last, creds);
                            }
                            if last {
                                return Ok(Resolved {
                                    parent: dir,
                                    name: name.to_vec(),
                                    ino: Some(ino),
                                });
                            }
                            dir = ino;
                            i += 1;
                        }
                    }
                }
            }
        }
        Ok(Resolved {
            parent: dir,
            name: Vec::new(),
            ino: Some(dir),
        })
    }

    pub fn stat_ino(&mut self, ino: u64) -> Result<Stat> {
        let i = self.get_inode(ino)?;
        Ok(Stat {
            ino: i.ino,
            mode: i.mode,
            nlink: i.nlink,
            uid: i.uid,
            gid: i.gid,
            size: i.size,
            blocks: i.blocks,
            blksize: BLOCK_SIZE,
            rdev: i.rdev,
            atime: i.atime,
            mtime: i.mtime,
            ctime: i.ctime,
            btime: i.btime,
        })
    }

    pub fn create_child(
        &mut self,
        parent: u64,
        name: &[u8],
        mode: u16,
        uid: u32,
        gid: u32,
        rdev: u64,
        creds: &Creds,
    ) -> Result<u64> {
        self.require_write()?;
        path::validate_name(name)?;
        let dir = self.get_inode(parent)?;
        if !dir.is_dir() {
            return Err(error::enotdir("not a directory"));
        }
        self.access_ok(&dir, creds, Access::Write)?;
        if self.dir_lookup(parent, name)?.is_some() {
            return Err(error::eexist("file exists"));
        }
        let ino = self.alloc_inode(mode, uid, gid)?;
        {
            let child = self.get_inode_mut(ino)?;
            child.rdev = rdev;
            if child.is_dir() {
                child.nlink = 2;
            }
        }
        if FileType::from_mode(mode).is_dir() {
            let mut child = self.get_inode(ino)?;
            self.write_dir_entries(
                ino,
                &mut child,
                &[
                    (ino, FileType::Directory, b".".to_vec()),
                    (parent, FileType::Directory, b"..".to_vec()),
                ],
            )?;
            let p = self.get_inode_mut(parent)?;
            p.nlink += 1;
            p.touch_mtime();
        }
        let ft = FileType::from_mode(mode);
        self.dir_insert(parent, name, ino, ft)?;
        Ok(ino)
    }

    pub fn unlink_name(
        &mut self,
        parent: u64,
        name: &[u8],
        rmdir: bool,
        creds: &Creds,
    ) -> Result<()> {
        self.require_write()?;
        path::validate_name(name)?;
        let dir = self.get_inode(parent)?;
        self.access_ok(&dir, creds, Access::Write)?;
        if self.check_perm && dir.mode & S_ISVTX != 0 && !creds.is_superuser() {
            let (ino, _) = self
                .dir_lookup(parent, name)?
                .ok_or_else(|| error::enoent("no such file or directory"))?;
            let child = self.get_inode(ino)?;
            if creds.fsuid != dir.uid && creds.fsuid != child.uid {
                return Err(error::eacces("sticky bit"));
            }
        }
        let (ino, ft) = match self.dir_lookup(parent, name)? {
            Some(v) => v,
            None => return Err(error::enoent("no such file or directory")),
        };
        if rmdir {
            if !ft.is_dir() {
                return Err(error::enotdir("not a directory"));
            }
            let entries = self.parse_dir(ino)?;
            let extra = entries
                .iter()
                .any(|(_, _, n)| n.as_slice() != b"." && n.as_slice() != b"..");
            if extra {
                return Err(error::enotempty());
            }
        } else if ft.is_dir() {
            return Err(error::eisdir("is a directory"));
        }
        self.dir_remove(parent, name)?;
        if ft.is_dir() {
            let p = self.get_inode_mut(parent)?;
            p.nlink = p.nlink.saturating_sub(1);
        }
        let nlink = {
            let child = self.get_inode_mut(ino)?;
            child.nlink = child.nlink.saturating_sub(1);
            child.touch_ctime();
            child.nlink
        };
        let opens = self.open_count.get(&ino).copied().unwrap_or(0);
        if nlink == 0 {
            if opens == 0 {
                self.drop_inode(ino)?;
            } else {
                self.orphans.insert(ino);
            }
        }
        Ok(())
    }

    pub fn link_into(
        &mut self,
        target: u64,
        parent: u64,
        name: &[u8],
        creds: &Creds,
    ) -> Result<()> {
        self.require_write()?;
        path::validate_name(name)?;
        let t = self.get_inode(target)?;
        if t.is_dir() {
            return Err(error::eperm("hard link to directory"));
        }
        let dir = self.get_inode(parent)?;
        self.access_ok(&dir, creds, Access::Write)?;
        if self.dir_lookup(parent, name)?.is_some() {
            return Err(error::eexist("file exists"));
        }
        self.dir_insert(parent, name, target, t.file_type())?;
        let t = self.get_inode_mut(target)?;
        t.nlink += 1;
        t.touch_ctime();
        Ok(())
    }

    pub fn rename(
        &mut self,
        old_parent: u64,
        old_name: &[u8],
        new_parent: u64,
        new_name: &[u8],
        creds: &Creds,
    ) -> Result<()> {
        self.require_write()?;
        path::validate_name(old_name)?;
        path::validate_name(new_name)?;
        let od = self.get_inode(old_parent)?;
        let nd = self.get_inode(new_parent)?;
        self.access_ok(&od, creds, Access::Write)?;
        self.access_ok(&nd, creds, Access::Write)?;
        let (src_ino, src_ft) = self
            .dir_lookup(old_parent, old_name)?
            .ok_or_else(|| error::enoent("no such file or directory"))?;
        if old_parent == new_parent && old_name == new_name {
            return Ok(());
        }
        if src_ft.is_dir() && self.is_ancestor(src_ino, new_parent)? {
            return Err(error::einval("rename into descendant"));
        }
        if let Some((dest_ino, dest_ft)) = self.dir_lookup(new_parent, new_name)? {
            if src_ino == dest_ino {
                return Ok(());
            }
            if src_ft.is_dir() != dest_ft.is_dir() {
                return if src_ft.is_dir() {
                    Err(error::enotdir("target is not a directory"))
                } else {
                    Err(error::eisdir("target is a directory"))
                };
            }
            if dest_ft.is_dir() {
                let entries = self.parse_dir(dest_ino)?;
                if entries
                    .iter()
                    .any(|(_, _, n)| n.as_slice() != b"." && n.as_slice() != b"..")
                {
                    return Err(error::enotempty());
                }
            }
            self.unlink_name(new_parent, new_name, dest_ft.is_dir(), creds)?;
        }
        self.dir_remove(old_parent, old_name)?;
        self.dir_insert(new_parent, new_name, src_ino, src_ft)?;
        if src_ft.is_dir() && old_parent != new_parent {
            // Update ..
            let mut entries = self.parse_dir(src_ino)?;
            for e in &mut entries {
                if e.2 == b".." {
                    e.0 = new_parent;
                }
            }
            let mut inode = self.get_inode(src_ino)?;
            self.write_dir_entries(src_ino, &mut inode, &entries)?;
            let op = self.get_inode_mut(old_parent)?;
            op.nlink = op.nlink.saturating_sub(1);
            let np = self.get_inode_mut(new_parent)?;
            np.nlink += 1;
        }
        Ok(())
    }

    fn is_ancestor(&mut self, dir: u64, mut child: u64) -> Result<bool> {
        let mut guard = 0;
        while child != ROOT_INO && guard < 4096 {
            if child == dir {
                return Ok(true);
            }
            match self.dir_lookup(child, b"..")? {
                Some((p, _)) => child = p,
                None => break,
            }
            guard += 1;
        }
        Ok(child == dir)
    }

    pub fn chmod(&mut self, ino: u64, mode: u16, creds: &Creds) -> Result<()> {
        self.require_write()?;
        let inode = self.get_inode(ino)?;
        if self.check_perm && !creds.is_superuser() && creds.fsuid != inode.uid {
            return Err(error::eperm("chmod"));
        }
        let inode = self.get_inode_mut(ino)?;
        inode.mode = (inode.mode & types::S_IFMT) | (mode & MODE_PERM);
        inode.touch_ctime();
        Ok(())
    }

    pub fn chown(&mut self, ino: u64, uid: Option<u32>, gid: Option<u32>, creds: &Creds) -> Result<()> {
        self.require_write()?;
        let inode = self.get_inode(ino)?;
        if self.check_perm && !creds.is_superuser() {
            if uid.is_some() && uid != Some(inode.uid) {
                return Err(error::eperm("chown"));
            }
            if let Some(g) = gid {
                if creds.fsuid != inode.uid || (g != creds.fsgid && !creds.groups.contains(&g)) {
                    return Err(error::eperm("chown"));
                }
            }
        }
        let inode = self.get_inode_mut(ino)?;
        if let Some(u) = uid {
            inode.uid = u;
        }
        if let Some(g) = gid {
            inode.gid = g;
        }
        inode.touch_ctime();
        Ok(())
    }

    pub fn set_times(&mut self, ino: u64, times: FileTimes) -> Result<()> {
        self.require_write()?;
        let inode = self.get_inode_mut(ino)?;
        if let Some(a) = times.accessed {
            inode.atime = a;
        }
        if let Some(m) = times.modified {
            inode.mtime = m;
        }
        if let Some(c) = times.created {
            inode.btime = c;
        }
        inode.ctime = Timespec::now();
        Ok(())
    }

    pub fn getxattr(&mut self, ino: u64, name: &[u8]) -> Result<Vec<u8>> {
        let inode = self.get_inode(ino)?;
        inode
            .xattr
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| error::Error::from_errno(error::ENODATA))
    }

    pub fn setxattr(&mut self, ino: u64, name: &[u8], value: &[u8], flags: u32) -> Result<()> {
        self.require_write()?;
        if name.is_empty() || name.len() > 255 {
            return Err(error::einval("xattr name"));
        }
        let inode = self.get_inode(ino)?;
        let exists = inode.xattr.iter().any(|(n, _)| n == name);
        if flags & types::XATTR_CREATE != 0 && exists {
            return Err(error::eexist("xattr exists"));
        }
        if flags & types::XATTR_REPLACE != 0 && !exists {
            return Err(error::Error::from_errno(error::ENODATA));
        }
        let inode = self.get_inode_mut(ino)?;
        if let Some(ent) = inode.xattr.iter_mut().find(|(n, _)| n == name) {
            ent.1 = value.to_vec();
        } else {
            inode.xattr.push((name.to_vec(), value.to_vec()));
        }
        inode.touch_ctime();
        Ok(())
    }

    pub fn listxattr(&mut self, ino: u64) -> Result<Vec<Vec<u8>>> {
        let inode = self.get_inode(ino)?;
        Ok(inode.xattr.iter().map(|(n, _)| n.clone()).collect())
    }

    pub fn removexattr(&mut self, ino: u64, name: &[u8]) -> Result<()> {
        self.require_write()?;
        let inode = self.get_inode_mut(ino)?;
        let before = inode.xattr.len();
        inode.xattr.retain(|(n, _)| n != name);
        if inode.xattr.len() == before {
            return Err(error::Error::from_errno(error::ENODATA));
        }
        inode.touch_ctime();
        Ok(())
    }

    pub fn inc_open(&mut self, ino: u64) {
        *self.open_count.entry(ino).or_insert(0) += 1;
    }

    pub fn dec_open(&mut self, ino: u64) -> Result<()> {
        if let Some(c) = self.open_count.get_mut(&ino) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                self.open_count.remove(&ino);
                if self.orphans.remove(&ino) {
                    let nlink = self.get_inode(ino).map(|i| i.nlink).unwrap_or(0);
                    if nlink == 0 {
                        self.drop_inode(ino)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn flush_all(&mut self) -> Result<()> {
        if !self.writable {
            return Ok(());
        }
        self.flush_record_cache()?;
        let dirty: Vec<u64> = self.dirty_inodes.iter().copied().collect();
        if self.sync_mode != SyncMode::None {
            let mut tx = self.journal.begin();
            for ino in &dirty {
                if let Some(i) = self.inodes.get(ino) {
                    tx.write_inode(*ino, &i.encode());
                }
            }
            self.journal.commit(
                &mut self.image,
                &mut self.sb,
                tx,
                self.sync_mode == SyncMode::Full,
            )?;
        }
        for ino in dirty {
            self.persist_inode(ino)?;
        }
        self.dirty_inodes.clear();
        self.cache.flush(&mut self.image)?;
        self.sb.mtime = Timespec::now();
        self.sb.total_blocks = self.alloc.total_blocks();
        let alloc_block = alloc::write_alloc_blocks(&mut self.image, &mut self.alloc, self.sb.alloc_block)?;
        self.sb.alloc_block = alloc_block;
        self.sb.total_blocks = self.alloc.total_blocks();
        let encoded = self.sb.encode();
        self.image.write_all_at(&encoded, 0)?;
        let backup = self.sb.backup_block();
        self.image.write_block(backup, &encoded)?;
        let _ = self.alloc.maybe_shrink(&mut self.image)?;
        self.cache.drop_from(self.alloc.total_blocks());
        self.sb.total_blocks = self.alloc.total_blocks();
        if self.sync_mode == SyncMode::Full {
            self.image.sync()?;
        }
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.flush_all()?;
        self.image.sync()
    }

    pub fn flush_cache(&mut self) -> Result<()> {
        self.flush_record_cache()?;
        self.cache.flush(&mut self.image)
    }

    pub fn flush_cache_data(&mut self) -> Result<()> {
        self.flush_record_cache()?;
        self.cache.flush(&mut self.image)?;
        self.image.sync_data()
    }
}

fn map_extents(extents: &[Extent], logical: u64) -> Option<u64> {
    for e in extents {
        if let Some(p) = e.map(logical) {
            return Some(p);
        }
    }
    None
}

fn encode_dir(entries: &[(u64, FileType, Vec<u8>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DIR1");
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (ino, ft, name) in entries {
        buf.extend_from_slice(&ino.to_le_bytes());
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.push(dir_ft(*ft));
        buf.push(0);
        buf.extend_from_slice(name);
        let pad = (4 - (name.len() % 4)) % 4;
        buf.extend(std::iter::repeat(0).take(pad));
    }
    buf
}

fn decode_dir(buf: &[u8]) -> Result<Vec<(u64, FileType, Vec<u8>)>> {
    if buf.is_empty() {
        return Ok(Vec::new());
    }
    if buf.len() < 8 || &buf[0..4] != b"DIR1" {
        return Err(error::eio("corrupt directory"));
    }
    let n = crate::format::get_u32(buf, 4) as usize;
    let mut off = 8;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if off + 12 > buf.len() {
            return Err(error::eio("corrupt directory entry"));
        }
        let ino = get_u64(buf, off);
        let nlen = get_u16(buf, off + 8) as usize;
        let ft = from_dir_ft(buf[off + 10]);
        off += 12;
        if off + nlen > buf.len() {
            return Err(error::eio("corrupt directory name"));
        }
        let name = buf[off..off + nlen].to_vec();
        off += nlen;
        let pad = (4 - (nlen % 4)) % 4;
        off += pad;
        out.push((ino, ft, name));
    }
    Ok(out)
}

fn dir_ft(ft: FileType) -> u8 {
    match ft {
        FileType::File => 1,
        FileType::Directory => 2,
        FileType::CharDevice => 3,
        FileType::BlockDevice => 4,
        FileType::Fifo => 5,
        FileType::Socket => 6,
        FileType::Symlink => 7,
    }
}

fn from_dir_ft(v: u8) -> FileType {
    match v {
        2 => FileType::Directory,
        3 => FileType::CharDevice,
        4 => FileType::BlockDevice,
        5 => FileType::Fifo,
        6 => FileType::Socket,
        7 => FileType::Symlink,
        _ => FileType::File,
    }
}

fn encode_xattrs(attrs: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"XAT1");
    buf.extend_from_slice(&(attrs.len() as u32).to_le_bytes());
    for (n, v) in attrs {
        buf.extend_from_slice(&(n.len() as u16).to_le_bytes());
        buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        buf.extend_from_slice(n);
        buf.extend_from_slice(v);
    }
    buf
}

fn decode_xattrs(buf: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    if buf.len() < 8 || &buf[0..4] != b"XAT1" {
        return Vec::new();
    }
    let n = crate::format::get_u32(buf, 4) as usize;
    let mut off = 8;
    let mut out = Vec::new();
    for _ in 0..n {
        if off + 6 > buf.len() {
            break;
        }
        let nl = get_u16(buf, off) as usize;
        let vl = crate::format::get_u32(buf, off + 2) as usize;
        off += 6;
        if off + nl + vl > buf.len() {
            break;
        }
        out.push((buf[off..off + nl].to_vec(), buf[off + nl..off + nl + vl].to_vec()));
        off += nl + vl;
    }
    out
}

fn generate_uuid() -> [u8; 16] {
    let now = Timespec::now();
    let mut u = [0u8; 16];
    u[0..8].copy_from_slice(&(now.sec as u64).to_le_bytes());
    u[8..12].copy_from_slice(&now.nsec.to_le_bytes());
    let mix = &u as *const _ as u64 ^ (now.sec as u64).wrapping_mul(0x9E37_79B9);
    u[12..16].copy_from_slice(&(mix as u32).to_le_bytes());
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    u
}
