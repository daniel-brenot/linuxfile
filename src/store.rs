//! Host-file I/O with grow and shrink.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::Path;

use crate::error::{self, Result};
use crate::format::BLOCK_SIZE_U64;

pub struct Image {
    file: File,
    len: u64,
}

impl Image {
    pub fn create(path: impl AsRef<Path>, initial_len: u64) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.set_len(initial_len)?;
        Ok(Self {
            file,
            len: initial_len,
        })
    }

    pub fn open(path: impl AsRef<Path>, writable: bool) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)?;
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn grow_to(&mut self, new_len: u64) -> Result<()> {
        if new_len <= self.len {
            return Ok(());
        }
        // Grow in 1 MiB chunks to reduce syscall chatter.
        let aligned = ((new_len + (1 << 20) - 1) >> 20) << 20;
        let aligned = aligned.max(new_len);
        self.file.set_len(aligned)?;
        self.len = aligned;
        Ok(())
    }

    pub fn shrink_to(&mut self, new_len: u64) -> Result<()> {
        if new_len >= self.len {
            return Ok(());
        }
        self.file.set_len(new_len)?;
        self.len = new_len;
        Ok(())
    }

    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        if offset.saturating_add(buf.len() as u64) > self.len {
            return Err(error::eio("read past end of image"));
        }
        read_at(&self.file, buf, offset)
    }

    pub fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        let end = offset.saturating_add(buf.len() as u64);
        if end > self.len {
            self.grow_to(end)?;
        }
        write_at(&self.file, buf, offset)
    }

    pub fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<()> {
        if buf.len() != BLOCK_SIZE_U64 as usize {
            return Err(error::einval("block buffer size"));
        }
        self.read_exact_at(buf, block * BLOCK_SIZE_U64)
    }

    pub fn write_block(&mut self, block: u64, buf: &[u8]) -> Result<()> {
        if buf.len() != BLOCK_SIZE_U64 as usize {
            return Err(error::einval("block buffer size"));
        }
        self.write_all_at(buf, block * BLOCK_SIZE_U64)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    pub fn sync_data(&mut self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)?;
    Ok(())
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)?;
    Ok(())
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(error::eio("unexpected eof"));
        }
        done += n;
    }
    Ok(())
}

#[cfg(windows)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_write(&buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(error::eio("short write"));
        }
        done += n;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> Result<()> {
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(buf)?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn write_at(file: &File, buf: &[u8], offset: u64) -> Result<()> {
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(buf)?;
    Ok(())
}

// Silence unused warnings on unix/windows where seek helpers are unused.
#[allow(dead_code)]
fn _seek_helpers(_: impl Read + Write + Seek) {}
