//! Write-back block cache. Dirty blocks are flushed on eviction, sync, and unmount.

use std::collections::{HashMap, VecDeque};

use crate::error::Result;
use crate::format::{BLOCK_SIZE, BLOCK_SIZE_U64};
use crate::store::Image;

pub struct BlockCache {
    map: HashMap<u64, Entry>,
    lru: VecDeque<u64>,
    capacity: usize,
}

struct Entry {
    data: [u8; BLOCK_SIZE as usize],
    dirty: bool,
}

impl BlockCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity.min(1024)),
            lru: VecDeque::with_capacity(capacity.min(1024)),
            capacity: capacity.max(16),
        }
    }

    fn touch(&mut self, block: u64) {
        if let Some(pos) = self.lru.iter().position(|&b| b == block) {
            self.lru.remove(pos);
        }
        self.lru.push_back(block);
    }

    fn evict_one(&mut self, image: &mut Image) -> Result<()> {
        while let Some(old) = self.lru.pop_front() {
            if let Some(ent) = self.map.remove(&old) {
                if ent.dirty {
                    image.write_block(old, &ent.data)?;
                }
                return Ok(());
            }
        }
        Ok(())
    }

    pub fn get(&mut self, image: &mut Image, block: u64) -> Result<&[u8; BLOCK_SIZE as usize]> {
        if !self.map.contains_key(&block) {
            if self.map.len() >= self.capacity {
                self.evict_one(image)?;
            }
            let mut data = [0u8; BLOCK_SIZE as usize];
            let offset = block * BLOCK_SIZE_U64;
            if offset + BLOCK_SIZE_U64 <= image.len() {
                image.read_block(block, &mut data)?;
            } else {
                image.grow_to(offset + BLOCK_SIZE_U64)?;
            }
            self.map.insert(block, Entry { data, dirty: false });
        }
        self.touch(block);
        Ok(&self.map.get(&block).unwrap().data)
    }

    pub fn get_mut(
        &mut self,
        image: &mut Image,
        block: u64,
    ) -> Result<&mut [u8; BLOCK_SIZE as usize]> {
        let _ = self.get(image, block)?;
        self.map.get_mut(&block).unwrap().dirty = true;
        Ok(&mut self.map.get_mut(&block).unwrap().data)
    }

    pub fn insert_dirty(&mut self, image: &mut Image, block: u64, data: [u8; BLOCK_SIZE as usize]) -> Result<()> {
        if !self.map.contains_key(&block) && self.map.len() >= self.capacity {
            self.evict_one(image)?;
        }
        self.map.insert(block, Entry { data, dirty: true });
        self.touch(block);
        Ok(())
    }

    #[allow(dead_code)]
    pub fn invalidate(&mut self, block: u64) {
        self.map.remove(&block);
        if let Some(pos) = self.lru.iter().position(|&b| b == block) {
            self.lru.remove(pos);
        }
    }

    pub fn flush(&mut self, image: &mut Image) -> Result<()> {
        for (block, ent) in self.map.iter_mut() {
            if ent.dirty {
                image.write_block(*block, &ent.data)?;
                ent.dirty = false;
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn flush_block(&mut self, image: &mut Image, block: u64) -> Result<()> {
        if let Some(ent) = self.map.get_mut(&block) {
            if ent.dirty {
                image.write_block(block, &ent.data)?;
                ent.dirty = false;
            }
        }
        Ok(())
    }

    pub fn drop_from(&mut self, min_block: u64) {
        self.map.retain(|&b, _| b < min_block);
        self.lru.retain(|&b| b < min_block);
    }
}
