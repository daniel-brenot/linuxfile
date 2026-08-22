//! Open file handles, matching [`std::fs::File`] semantics.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, RwLock};

use crate::error::{self, Result};
use crate::inner::Inner;
use crate::types::{self, Metadata, Permissions, Stat, O_ACCMODE, O_APPEND, O_RDONLY, O_RDWR, O_WRONLY};

#[derive(Clone)]
pub struct File {
    inner: Arc<RwLock<Inner>>,
    ino: u64,
    flags: i32,
    pos: u64,
}

impl File {
    pub(crate) fn new(inner: Arc<RwLock<Inner>>, ino: u64, flags: i32) -> Result<Self> {
        {
            let mut g = inner.write().map_err(|_| error::eio("lock poisoned"))?;
            g.inc_open(ino);
        }
        Ok(Self {
            inner,
            ino,
            flags,
            pos: 0,
        })
    }

    pub fn ino(&self) -> u64 {
        self.ino
    }

    pub fn metadata(&self) -> Result<Metadata> {
        let mut g = self.inner.write().map_err(|_| error::eio("lock poisoned"))?;
        Ok(Metadata {
            stat: g.stat_ino(self.ino)?,
        })
    }

    pub fn stat(&self) -> Result<Stat> {
        let mut g = self.inner.write().map_err(|_| error::eio("lock poisoned"))?;
        g.stat_ino(self.ino)
    }

    pub fn set_len(&self, size: u64) -> Result<()> {
        self.require_write()?;
        let mut g = self.inner.write().map_err(|_| error::eio("lock poisoned"))?;
        g.truncate_data(self.ino, size)
    }

    pub fn set_permissions(&self, perm: Permissions) -> Result<()> {
        let mut g = self.inner.write().map_err(|_| error::eio("lock poisoned"))?;
        g.chmod(self.ino, perm.mode, &types::Creds::root())
    }

    pub fn sync_all(&self) -> Result<()> {
        let mut g = self.inner.write().map_err(|_| error::eio("lock poisoned"))?;
        g.sync()
    }

    pub fn sync_data(&self) -> Result<()> {
        let mut g = self.inner.write().map_err(|_| error::eio("lock poisoned"))?;
        g.flush_cache_data()
    }

    pub fn try_clone(&self) -> Result<File> {
        File::new(self.inner.clone(), self.ino, self.flags)
    }

    fn require_read(&self) -> Result<()> {
        let acc = self.flags & O_ACCMODE;
        if acc == O_WRONLY {
            Err(error::ebadf())
        } else {
            Ok(())
        }
    }

    fn require_write(&self) -> Result<()> {
        let acc = self.flags & O_ACCMODE;
        if acc == O_RDONLY {
            Err(error::ebadf())
        } else {
            Ok(())
        }
    }

    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.require_read()?;
        let mut g = self.inner.write().map_err(|_| error::eio("lock poisoned"))?;
        g.read_data(self.ino, offset, buf)
    }

    pub fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        self.require_write()?;
        let mut g = self.inner.write().map_err(|_| error::eio("lock poisoned"))?;
        g.write_data(self.ino, offset, buf)
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.require_read().map_err(io::Error::from)?;
        let mut g = self
            .inner
            .write()
            .map_err(|_| io::Error::other("lock poisoned"))?;
        let n = g.read_data(self.ino, self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.require_write().map_err(io::Error::from)?;
        let mut g = self
            .inner
            .write()
            .map_err(|_| io::Error::other("lock poisoned"))?;
        let off = if self.flags & O_APPEND != 0 {
            g.get_inode(self.ino)?.size
        } else {
            self.pos
        };
        let n = g.write_data(self.ino, off, buf)?;
        self.pos = off + n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut g = self
            .inner
            .write()
            .map_err(|_| io::Error::other("lock poisoned"))?;
        g.flush_cache()?;
        Ok(())
    }
}

impl Seek for File {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let mut g = self
            .inner
            .write()
            .map_err(|_| io::Error::other("lock poisoned"))?;
        let size = g.get_inode(self.ino)?.size;
        let new = match pos {
            SeekFrom::Start(o) => o as i128,
            SeekFrom::Current(o) => self.pos as i128 + o as i128,
            SeekFrom::End(o) => size as i128 + o as i128,
        };
        if new < 0 {
            return Err(io::Error::from(error::einval("negative seek")));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if let Ok(mut g) = self.inner.write() {
            let _ = g.dec_open(self.ino);
        }
    }
}

/// Options for opening a path inside the filesystem (like [`std::fs::OpenOptions`]).
#[derive(Debug, Clone)]
pub struct OpenOptions {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub truncate: bool,
    pub create: bool,
    pub create_new: bool,
    pub mode: u16,
    pub nofollow: bool,
    pub directory: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
            mode: 0o666,
            nofollow: false,
            directory: false,
        }
    }
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        if append {
            self.write = true;
        }
        self
    }
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        if create_new {
            self.create = true;
        }
        self
    }
    pub fn mode(&mut self, mode: u16) -> &mut Self {
        self.mode = mode;
        self
    }

    pub fn from_linux_flags(flags: i32, mode: u16) -> Self {
        let acc = flags & types::O_ACCMODE;
        Self {
            read: acc == O_RDONLY || acc == O_RDWR,
            write: acc == O_WRONLY || acc == O_RDWR,
            append: flags & types::O_APPEND != 0,
            truncate: flags & types::O_TRUNC != 0,
            create: flags & types::O_CREAT != 0,
            create_new: flags & types::O_CREAT != 0 && flags & types::O_EXCL != 0,
            mode,
            nofollow: flags & types::O_NOFOLLOW != 0,
            directory: flags & types::O_DIRECTORY != 0,
        }
    }

    pub fn as_flags(&self) -> i32 {
        let mut f = if self.read && self.write {
            O_RDWR
        } else if self.write {
            O_WRONLY
        } else {
            O_RDONLY
        };
        if self.append {
            f |= types::O_APPEND;
        }
        if self.truncate {
            f |= types::O_TRUNC;
        }
        if self.create {
            f |= types::O_CREAT;
        }
        if self.create_new {
            f |= types::O_EXCL;
        }
        if self.nofollow {
            f |= types::O_NOFOLLOW;
        }
        if self.directory {
            f |= types::O_DIRECTORY;
        }
        f
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: u64,
    pub file_type: crate::types::FileType,
    pub name: Vec<u8>,
}

impl DirEntry {
    pub fn file_name(&self) -> &[u8] {
        &self.name
    }

    pub fn file_name_str(&self) -> String {
        String::from_utf8_lossy(&self.name).into_owned()
    }

    pub fn path_name(&self) -> crate::path::UnixPathBuf {
        crate::path::UnixPathBuf::from_bytes(self.name.clone())
    }
}

pub struct ReadDir {
    iter: std::vec::IntoIter<DirEntry>,
}

impl ReadDir {
    pub(crate) fn new(entries: Vec<DirEntry>) -> Self {
        Self {
            iter: entries.into_iter(),
        }
    }
}

impl Iterator for ReadDir {
    type Item = Result<DirEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(Ok)
    }
}
