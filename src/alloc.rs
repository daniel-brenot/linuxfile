//! Extent allocator with coalescing. Trailing free space shrinks the image.

use std::collections::BTreeMap;

use crate::error::{self, Result};
use crate::format::{
    get_u32, get_u64, put_u32, BLOCK_SIZE, BLOCK_SIZE_U64, SHRINK_SLACK_BLOCKS,
};
use crate::store::Image;

/// Sorted free extents: start block → length in blocks.
#[derive(Debug, Clone, Default)]
pub struct Allocator {
    free: BTreeMap<u64, u64>,
    first_data: u64,
    total_blocks: u64,
}

impl Allocator {
    pub fn new(first_data: u64, total_blocks: u64) -> Self {
        let mut free = BTreeMap::new();
        if total_blocks > first_data {
            free.insert(first_data, total_blocks - first_data);
        }
        Self {
            free,
            first_data,
            total_blocks,
        }
    }

    pub fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    pub fn free_blocks(&self) -> u64 {
        self.free.values().sum()
    }

    #[allow(dead_code)]
    pub fn set_total(&mut self, total: u64) {
        self.total_blocks = total;
    }

    /// Mark a range as used (removed from the free map). Used at mkfs.
    #[allow(dead_code)]
    pub fn reserve(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        self.remove_range(start, len);
    }

    pub fn allocate(&mut self, image: &mut Image, want: u64) -> Result<u64> {
        if want == 0 {
            return Err(error::einval("zero-length allocation"));
        }
        // Best-fit among extents that can satisfy `want`.
        let mut best: Option<(u64, u64)> = None; // start, len
        for (&start, &len) in &self.free {
            if len >= want && best.map(|(_, bl)| len < bl).unwrap_or(true) {
                best = Some((start, len));
                if len == want {
                    break;
                }
            }
        }
        if let Some((start, len)) = best {
            self.free.remove(&start);
            if len > want {
                self.free.insert(start + want, len - want);
            }
            return Ok(start);
        }
        // Grow the image and allocate from the new tail.
        let start = self.total_blocks;
        let new_total = start + want;
        image.grow_to(new_total * BLOCK_SIZE_U64)?;
        // Image may have grown more than `want` due to 1MiB rounding.
        let image_blocks = image.len() / BLOCK_SIZE_U64;
        if image_blocks > new_total {
            self.free.insert(new_total, image_blocks - new_total);
        }
        self.total_blocks = image_blocks.max(new_total);
        Ok(start)
    }

    pub fn free(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let mut start = start;
        let mut len = len;

        if let Some((&ps, &pl)) = self.free.range(..start).next_back() {
            if ps + pl == start {
                self.free.remove(&ps);
                start = ps;
                len += pl;
            }
        }
        if let Some(&nl) = self.free.get(&(start + len)) {
            self.free.remove(&(start + len));
            len += nl;
        }
        self.free.insert(start, len);
    }

    /// Truncate trailing free space, keeping slack to avoid resize thrash.
    pub fn maybe_shrink(&mut self, image: &mut Image) -> Result<bool> {
        let Some((&start, &len)) = self.free.iter().next_back() else {
            return Ok(false);
        };
        if start + len != self.total_blocks {
            return Ok(false);
        }
        if len <= SHRINK_SLACK_BLOCKS {
            return Ok(false);
        }
        let keep = SHRINK_SLACK_BLOCKS;
        let new_total = (start + keep).max(self.first_data);
        if new_total >= self.total_blocks {
            return Ok(false);
        }
        let new_len = new_total * BLOCK_SIZE_U64;
        image.shrink_to(new_len)?;
        self.free.remove(&start);
        let leftover = new_total.saturating_sub(start);
        if leftover > 0 {
            self.free.insert(start, leftover);
        }
        self.total_blocks = new_total;
        Ok(true)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"LNXA");
        buf.extend_from_slice(&(self.first_data).to_le_bytes());
        buf.extend_from_slice(&(self.total_blocks).to_le_bytes());
        buf.extend_from_slice(&(self.free.len() as u32).to_le_bytes());
        for (&start, &len) in &self.free {
            buf.extend_from_slice(&start.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
        }
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < 4 + 8 + 8 + 4 || &buf[0..4] != b"LNXA" {
            return Err(error::eio("bad allocator state"));
        }
        let first_data = get_u64(buf, 4);
        let total_blocks = get_u64(buf, 12);
        let n = get_u32(buf, 20) as usize;
        let mut free = BTreeMap::new();
        let mut off = 24;
        for _ in 0..n {
            if off + 16 > buf.len() {
                return Err(error::eio("allocator state truncated"));
            }
            let start = get_u64(buf, off);
            let len = get_u64(buf, off + 8);
            if len > 0 {
                free.insert(start, len);
            }
            off += 16;
        }
        Ok(Self {
            free,
            first_data,
            total_blocks,
        })
    }

    #[allow(dead_code)]
    fn remove_range(&mut self, start: u64, len: u64) {
        let end = start + len;
        let keys: Vec<u64> = self
            .free
            .range(..end)
            .filter_map(|(&s, &l)| {
                if s < end && s + l > start {
                    Some(s)
                } else {
                    None
                }
            })
            .collect();
        for s in keys {
            let l = self.free.remove(&s).unwrap();
            let e = s + l;
            if s < start {
                self.free.insert(s, start - s);
            }
            if e > end {
                self.free.insert(end, e - end);
            }
        }
    }
}

/// Persist allocator into one or more dedicated blocks. First block number is stored in the superblock.
pub fn write_alloc_blocks(
    image: &mut Image,
    alloc: &mut Allocator,
    existing: u64,
) -> Result<u64> {
    if existing != 0 {
        let mut hdr = [0u8; BLOCK_SIZE as usize];
        if existing * BLOCK_SIZE_U64 + BLOCK_SIZE_U64 <= image.len() {
            image.read_block(existing, &mut hdr)?;
            if &hdr[0..4] == b"ALN\0" {
                let old_n = get_u32(&hdr, 4) as u64;
                if old_n > 0 {
                    alloc.free(existing, old_n);
                }
            }
        }
    }

    // Conservative upper bound so the allocation itself is reflected in the snapshot.
    let estimate = alloc.encode().len() + 64;
    let per = BLOCK_SIZE as usize - 16;
    let nblocks = ((estimate + per - 1) / per).max(1) as u64;
    let start = alloc.allocate(image, nblocks)?;
    let bytes = alloc.encode();
    for i in 0..nblocks {
        let mut blk = [0u8; BLOCK_SIZE as usize];
        if i == 0 {
            blk[0..4].copy_from_slice(b"ALN\0");
            put_u32(&mut blk, 4, nblocks as u32);
            put_u32(&mut blk, 8, bytes.len() as u32);
        }
        let src_off = (i as usize) * per;
        if src_off < bytes.len() {
            let take = (bytes.len() - src_off).min(per);
            let dst = if i == 0 { 16 } else { 0 };
            let room = BLOCK_SIZE as usize - dst;
            let take = take.min(room);
            blk[dst..dst + take].copy_from_slice(&bytes[src_off..src_off + take]);
        }
        image.write_block(start + i, &blk)?;
    }
    Ok(start)
}

pub fn read_alloc_blocks(image: &Image, start: u64) -> Result<Allocator> {
    if start == 0 {
        return Err(error::eio("missing allocator"));
    }
    let mut hdr = [0u8; BLOCK_SIZE as usize];
    image.read_block(start, &mut hdr)?;
    if &hdr[0..4] != b"ALN\0" {
        return Err(error::eio("bad allocator header"));
    }
    let nblocks = get_u32(&hdr, 4) as u64;
    let nbytes = get_u32(&hdr, 8) as usize;
    let per = BLOCK_SIZE as usize - 16;
    let mut bytes = vec![0u8; nbytes];
    for i in 0..nblocks {
        let mut blk = [0u8; BLOCK_SIZE as usize];
        image.read_block(start + i, &mut blk)?;
        let src_off = (i as usize) * per;
        if src_off >= nbytes {
            break;
        }
        let take = (nbytes - src_off).min(per);
        let src = if i == 0 { 16 } else { 0 };
        bytes[src_off..src_off + take].copy_from_slice(&blk[src..src + take]);
    }
    Allocator::decode(&bytes)
}
