//! ZFS-style record compression.
//!
//! File data is grouped into records (default 32 KiB). Each record is compressed
//! independently with LZ4. The compressed form is kept only when it saves at
//! least one filesystem block (4 KiB), matching ZFS's "must save an ashift
//! unit" rule. Incompressible records are stored raw. All-zero records are
//! left as holes.

use crate::crc;
use crate::error::{self, Result};
use crate::format::{BLOCK_SIZE, BLOCK_SIZE_U64};

/// Compression algorithm for new writes. Existing records keep the algorithm
/// stored in their on-disk header, so changing this later does not rewrite data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Compression {
    /// Store records uncompressed (1:1 logical → physical).
    Off = 0,
    /// LZ4, the ZFS default for `compression=on`.
    Lz4 = 1,
}

impl Compression {
    /// ZFS `compression=on` — LZ4.
    pub const ON: Self = Self::Lz4;

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Lz4,
            _ => Self::Off,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn is_off(self) -> bool {
        matches!(self, Self::Off)
    }
}

impl Default for Compression {
    fn default() -> Self {
        Self::Off
    }
}

/// On-disk magic for a compressed record (`LFC1`).
pub const HEADER_MAGIC: [u8; 4] = *b"LFC1";
pub const HEADER_SIZE: usize = 20;

/// Default record size: 8 × 4 KiB = 32 KiB.
pub const DEFAULT_RECORD_BLOCKS: u32 = 8;
/// Maximum record size: 32 × 4 KiB = 128 KiB (ZFS default `recordsize`).
pub const MAX_RECORD_BLOCKS: u32 = 32;

pub fn normalize_record_blocks(n: u32) -> u32 {
    let n = n.max(2).min(MAX_RECORD_BLOCKS);
    let pow = n.next_power_of_two();
    pow.min(MAX_RECORD_BLOCKS).max(2)
}

#[derive(Debug, Clone, Copy)]
pub struct RecordHeader {
    pub algo: Compression,
    pub logical_blocks: u16,
    pub payload_len: u32,
    pub uncompressed_len: u32,
    pub crc: u32,
}

impl RecordHeader {
    pub fn phys_blocks(&self) -> u32 {
        let total = HEADER_SIZE as u32 + self.payload_len;
        total.div_ceil(BLOCK_SIZE)
    }

    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&HEADER_MAGIC);
        buf[4] = self.algo.as_u8();
        buf[5] = 0;
        buf[6..8].copy_from_slice(&self.logical_blocks.to_le_bytes());
        buf[8..12].copy_from_slice(&self.payload_len.to_le_bytes());
        buf[12..16].copy_from_slice(&self.uncompressed_len.to_le_bytes());
        buf[16..20].copy_from_slice(&self.crc.to_le_bytes());
        buf
    }

    /// Parse a header from the start of a physical block. Returns `None` if
    /// the bytes are not a valid compressed-record header (including the case
    /// where file data happens to begin with `LFC1`).
    pub fn parse(block: &[u8]) -> Option<Self> {
        if block.len() < HEADER_SIZE || block[0..4] != HEADER_MAGIC {
            return None;
        }
        let algo = match block[4] {
            1 => Compression::Lz4,
            _ => return None,
        };
        let logical_blocks = u16::from_le_bytes(block[6..8].try_into().ok()?);
        let payload_len = u32::from_le_bytes(block[8..12].try_into().ok()?);
        let uncompressed_len = u32::from_le_bytes(block[12..16].try_into().ok()?);
        let crc = u32::from_le_bytes(block[16..20].try_into().ok()?);
        if logical_blocks == 0 || logical_blocks as u32 > MAX_RECORD_BLOCKS {
            return None;
        }
        if uncompressed_len == 0 || uncompressed_len as u64 > logical_blocks as u64 * BLOCK_SIZE_U64
        {
            return None;
        }
        if payload_len == 0 || payload_len >= uncompressed_len {
            return None;
        }
        let phys = (HEADER_SIZE as u32 + payload_len).div_ceil(BLOCK_SIZE);
        if phys > logical_blocks as u32 {
            return None;
        }
        Some(Self {
            algo,
            logical_blocks,
            payload_len,
            uncompressed_len,
            crc,
        })
    }
}

/// Compress `src` with `algo`. Returns `(Off, empty)` when the result would
/// not save at least one 4 KiB block (caller should store the record raw).
pub fn try_compress(algo: Compression, src: &[u8]) -> (Compression, Vec<u8>) {
    if algo.is_off() || src.len() < BLOCK_SIZE as usize * 2 {
        return (Compression::Off, Vec::new());
    }
    match algo {
        Compression::Off => (Compression::Off, Vec::new()),
        Compression::Lz4 => match lz4_flex::block::compress(src) {
            payload if saves_a_block(src.len(), payload.len()) => (Compression::Lz4, payload),
            _ => (Compression::Off, Vec::new()),
        },
    }
}

fn saves_a_block(uncompressed: usize, compressed: usize) -> bool {
    HEADER_SIZE + compressed + BLOCK_SIZE as usize <= uncompressed
}

pub fn decompress(algo: Compression, src: &[u8], uncompressed_len: usize) -> Result<Vec<u8>> {
    match algo {
        Compression::Off => {
            let mut v = src.to_vec();
            v.resize(uncompressed_len, 0);
            Ok(v)
        }
        Compression::Lz4 => lz4_flex::block::decompress(src, uncompressed_len)
            .map_err(|e| error::eio(format!("lz4 decompress: {e}"))),
    }
}

pub fn payload_crc(payload: &[u8]) -> u32 {
    crc::crc32(payload)
}
