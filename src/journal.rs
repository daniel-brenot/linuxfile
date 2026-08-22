//! Metadata write-ahead log. Data blocks are written before the commit record.

use crate::crc;
use crate::error::{self, Result};
use crate::format::{get_u32, get_u64, BLOCK_SIZE, BLOCK_SIZE_U64, Superblock};
use crate::store::Image;

const JMAGIC: &[u8; 4] = b"JRNL";
const OP_BEGIN: u32 = 1;
const OP_INODE: u32 = 2;
const OP_BLOCK: u32 = 3;
const OP_COMMIT: u32 = 4;

pub struct Journal {
    start_block: u64,
    blocks: u32,
    seq: u64,
    head: u32, // byte offset within journal area
}

pub struct Transaction {
    seq: u64,
    buf: Vec<u8>,
}

impl Journal {
    pub fn new(sb: &Superblock) -> Self {
        Self {
            start_block: sb.journal_start(),
            blocks: sb.journal_blocks,
            seq: sb.journal_seq,
            head: sb.journal_head,
        }
    }

    pub fn area_bytes(&self) -> u64 {
        self.blocks as u64 * BLOCK_SIZE_U64
    }

    pub fn begin(&mut self) -> Transaction {
        self.seq += 1;
        let mut tx = Transaction {
            seq: self.seq,
            buf: Vec::with_capacity(256),
        };
        tx.buf.extend_from_slice(JMAGIC);
        put_u32_vec(&mut tx.buf, OP_BEGIN);
        put_u64_vec(&mut tx.buf, tx.seq);
        tx
    }

    pub fn commit(
        &mut self,
        image: &mut Image,
        sb: &mut Superblock,
        mut tx: Transaction,
        sync: bool,
    ) -> Result<()> {
        put_u32_vec(&mut tx.buf, OP_COMMIT);
        put_u64_vec(&mut tx.buf, tx.seq);
        let csum = crc::crc32(&tx.buf);
        put_u32_vec(&mut tx.buf, csum);

        let need = tx.buf.len() as u64 + 8;
        if need > self.area_bytes() {
            return Err(error::eio("journal transaction too large"));
        }
        if self.head as u64 + need > self.area_bytes() {
            self.head = 0;
        }
        let off = self.start_block * BLOCK_SIZE_U64 + self.head as u64;
        image.write_all_at(&tx.buf, off)?;
        if sync {
            image.sync_data()?;
        }
        self.head = self.head.saturating_add(tx.buf.len() as u32);
        sb.journal_seq = self.seq;
        sb.journal_head = self.head;
        sb.journal_tail = self.head;
        sb.journal_committed = self.seq;
        Ok(())
    }
}

impl Transaction {
    pub fn write_inode(&mut self, ino: u64, raw: &[u8]) {
        put_u32_vec(&mut self.buf, OP_INODE);
        put_u64_vec(&mut self.buf, ino);
        put_u32_vec(&mut self.buf, raw.len() as u32);
        self.buf.extend_from_slice(raw);
    }

    #[allow(dead_code)]
    pub fn write_block(&mut self, block: u64, data: &[u8]) {
        put_u32_vec(&mut self.buf, OP_BLOCK);
        put_u64_vec(&mut self.buf, block);
        put_u32_vec(&mut self.buf, data.len() as u32);
        self.buf.extend_from_slice(data);
    }
}

/// Replay committed transactions that were not checkpointed. Best-effort; a
/// truncated tail is ignored.
pub fn replay(image: &mut Image, sb: &Superblock) -> Result<u32> {
    let start = sb.journal_start() * BLOCK_SIZE_U64;
    let area = sb.journal_blocks as u64 * BLOCK_SIZE_U64;
    if area == 0 {
        return Ok(0);
    }
    let mut buf = vec![0u8; area as usize];
    image.read_exact_at(&mut buf, start)?;

    let mut applied = 0u32;
    let mut off = 0usize;
    while off + 16 <= buf.len() {
        if &buf[off..off + 4] != JMAGIC {
            break;
        }
        let begin = off;
        off += 4;
        if get_u32(&buf, off) != OP_BEGIN {
            break;
        }
        off += 4;
        let seq = get_u64(&buf, off);
        off += 8;
        if seq == 0 || seq > sb.journal_committed {
            break;
        }
        let mut ops: Vec<(u32, u64, Vec<u8>)> = Vec::new();
        let mut ok = false;
        while off + 4 <= buf.len() {
            let op = get_u32(&buf, off);
            off += 4;
            match op {
                OP_INODE => {
                    if off + 12 > buf.len() {
                        break;
                    }
                    let ino = get_u64(&buf, off);
                    off += 8;
                    let n = get_u32(&buf, off) as usize;
                    off += 4;
                    if off + n > buf.len() {
                        break;
                    }
                    ops.push((OP_INODE, ino, buf[off..off + n].to_vec()));
                    off += n;
                }
                OP_BLOCK => {
                    if off + 12 > buf.len() {
                        break;
                    }
                    let block = get_u64(&buf, off);
                    off += 8;
                    let n = get_u32(&buf, off) as usize;
                    off += 4;
                    if off + n > buf.len() {
                        break;
                    }
                    ops.push((OP_BLOCK, block, buf[off..off + n].to_vec()));
                    off += n;
                }
                OP_COMMIT => {
                    if off + 12 > buf.len() {
                        break;
                    }
                    let cseq = get_u64(&buf, off);
                    off += 8;
                    let csum = get_u32(&buf, off);
                    off += 4;
                    if cseq != seq {
                        break;
                    }
                    let crc_end = off;
                    let calc = crc::crc32(&buf[begin..crc_end - 4]);
                    if calc != csum {
                        break;
                    }
                    for (kind, key, data) in ops {
                        match kind {
                            OP_BLOCK => {
                                if data.len() == BLOCK_SIZE as usize {
                                    image.write_block(key, &data)?;
                                }
                            }
                            OP_INODE => {
                                let _ = (key, data);
                            }
                            _ => {}
                        }
                    }
                    applied += 1;
                    ok = true;
                    break;
                }
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            break;
        }
    }
    Ok(applied)
}

fn put_u32_vec(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u64_vec(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
