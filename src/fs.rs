//! High-level [`std::fs`]-style API bound to one image file.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::error::{self, Result};
use crate::file::{DirEntry, File, OpenOptions, ReadDir};
use crate::format::{BLOCK_SIZE, ROOT_INO};
use crate::inner::Inner;
use crate::path::{UnixPath, UnixPathBuf};
use crate::types::{
    apply_umask, Creds, FileTimes, Metadata, Permissions, Stat, StatFs, S_IFDIR, S_IFIFO,
    S_IFLNK, S_IFREG,
};
use crate::{Compression, Context, CreateOptions};

/// A mounted Linux filesystem stored in a single host file.
///
/// The host file grows when the filesystem needs space and shrinks when
/// trailing blocks are freed.
#[derive(Clone)]
pub struct LinuxFile {
    inner: Arc<RwLock<Inner>>,
}

impl LinuxFile {
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        Self::create_with(path, CreateOptions::default())
    }

    pub fn create_with(path: impl AsRef<Path>, opts: CreateOptions) -> Result<Self> {
        let inner = Inner::create(path, &opts)?;
        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, true, CreateOptions::default())
    }

    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, false, CreateOptions::default())
    }

    pub fn open_with(path: impl AsRef<Path>, writable: bool, opts: CreateOptions) -> Result<Self> {
        let inner = Inner::open(path, writable, opts.cache_blocks, opts.sync)?;
        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    pub(crate) fn lock(&self) -> Result<std::sync::RwLockWriteGuard<'_, Inner>> {
        self.inner.write().map_err(|_| error::eio("lock poisoned"))
    }

    fn creds() -> Creds {
        Creds::root()
    }

    /// Process-like view: credentials, cwd, root, and a file-descriptor table.
    pub fn context(&self, creds: Creds) -> Context {
        Context::new(self.clone(), creds)
    }

    pub fn sync(&self) -> Result<()> {
        self.lock()?.sync()
    }

    /// Filesystem-wide default compression for new files.
    pub fn compression(&self) -> Result<Compression> {
        Ok(self.lock()?.default_compression)
    }

    /// Logical 4 KiB blocks per compression record.
    pub fn record_blocks(&self) -> Result<u32> {
        Ok(self.lock()?.record_blocks)
    }

    /// Compression algorithm used for new writes to `path`.
    pub fn file_compression<P: AsRef<UnixPath>>(&self, path: P) -> Result<Compression> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        Ok(Compression::from_u8(g.get_inode(ino)?.compression))
    }

    /// Change the algorithm used for future writes to `path`. Existing records are left as-is
    /// until they are overwritten (same as ZFS `compression=`).
    pub fn set_compression<P: AsRef<UnixPath>>(&self, path: P, compression: Compression) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        g.set_inode_compression(ino, compression)
    }

    /// Current size of the host image file in bytes.
    pub fn image_len(&self) -> Result<u64> {
        Ok(self.lock()?.image.len())
    }

    pub fn statfs(&self) -> Result<StatFs> {
        let g = self.lock()?;
        Ok(StatFs {
            block_size: BLOCK_SIZE,
            total_blocks: g.alloc.total_blocks(),
            free_blocks: g.alloc.free_blocks(),
            avail_blocks: g.alloc.free_blocks(),
            total_inodes: g.inode_table_capacity(),
            free_inodes: g.sb.free_inode_count + 16,
            namelen: crate::path::NAME_MAX as u32,
            fsid: u64::from_le_bytes(g.sb.uuid[0..8].try_into().unwrap()),
        })
    }

    pub fn read<P: AsRef<UnixPath>>(&self, path: P) -> Result<Vec<u8>> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        let mut f = self.open_opts(path, &opts)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }

    pub fn read_to_string<P: AsRef<UnixPath>>(&self, path: P) -> Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes).map_err(|e| error::einval(e.to_string()))
    }

    pub fn write<P: AsRef<UnixPath>>(&self, path: P, contents: &[u8]) -> Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        let mut f = self.open_opts(path, &opts)?;
        f.write_all(contents)?;
        Ok(())
    }

    /// Open a path inside the filesystem for reading (like [`std::fs::File::open`]).
    pub fn open_file<P: AsRef<UnixPath>>(&self, path: P) -> Result<File> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        self.open_opts(path, &opts)
    }

    pub fn create_file<P: AsRef<UnixPath>>(&self, path: P) -> Result<File> {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        self.open_opts(path, &opts)
    }

    pub fn open_opts<P: AsRef<UnixPath>>(&self, path: P, opts: &OpenOptions) -> Result<File> {
        let path = path.as_ref();
        let creds = Self::creds();
        let mut g = self.lock()?;
        let follow = !opts.nofollow;
        let resolved = g.walk(ROOT_INO, ROOT_INO, path, follow, &creds)?;
        let ino = match resolved.ino {
            Some(ino) => {
                if opts.create_new {
                    return Err(error::eexist("file exists"));
                }
                let inode = g.get_inode(ino)?;
                if opts.directory && !inode.is_dir() {
                    return Err(error::enotdir("not a directory"));
                }
                if inode.is_dir() && opts.write && !opts.directory {
                    return Err(error::eisdir("is a directory"));
                }
                if opts.truncate {
                    if !inode.is_file() {
                        return Err(error::einval("truncate on non-file"));
                    }
                    g.truncate_data(ino, 0)?;
                }
                ino
            }
            None => {
                if !opts.create && !opts.create_new {
                    return Err(error::enoent("no such file or directory"));
                }
                let mode = S_IFREG | apply_umask(opts.mode, 0o022);
                g.create_child(
                    resolved.parent,
                    &resolved.name,
                    mode,
                    creds.fsuid,
                    creds.fsgid,
                    0,
                    &creds,
                )?
            }
        };
        drop(g);
        File::new(self.inner.clone(), ino, opts.as_flags())
    }

    pub fn create_dir<P: AsRef<UnixPath>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        let creds = Self::creds();
        let mut g = self.lock()?;
        let resolved = g.walk(ROOT_INO, ROOT_INO, path, true, &creds)?;
        if resolved.ino.is_some() {
            return Err(error::eexist("file exists"));
        }
        let mode = S_IFDIR | apply_umask(0o777, 0o022);
        g.create_child(
            resolved.parent,
            &resolved.name,
            mode,
            creds.fsuid,
            creds.fsgid,
            0,
            &creds,
        )?;
        Ok(())
    }

    pub fn create_dir_all<P: AsRef<UnixPath>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        if path.is_root() || path.is_empty() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            if !parent.is_empty() && !parent.is_root() {
                let _ = self.create_dir_all(parent);
            }
        }
        match self.create_dir(path) {
            Ok(()) => Ok(()),
            Err(e) if e.is_exists() => {
                if self.metadata(path)?.is_dir() {
                    Ok(())
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
    }

    pub fn remove_file<P: AsRef<UnixPath>>(&self, path: P) -> Result<()> {
        self.unlink(path)
    }

    pub fn unlink<P: AsRef<UnixPath>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        let creds = Self::creds();
        let mut g = self.lock()?;
        let resolved = g.walk(ROOT_INO, ROOT_INO, path, false, &creds)?;
        let Some(_) = resolved.ino else {
            return Err(error::enoent("no such file or directory"));
        };
        g.unlink_name(resolved.parent, &resolved.name, false, &creds)
    }

    pub fn remove_dir<P: AsRef<UnixPath>>(&self, path: P) -> Result<()> {
        self.rmdir(path)
    }

    pub fn rmdir<P: AsRef<UnixPath>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        let creds = Self::creds();
        let mut g = self.lock()?;
        let resolved = g.walk(ROOT_INO, ROOT_INO, path, false, &creds)?;
        if resolved.ino.is_none() {
            return Err(error::enoent("no such file or directory"));
        }
        if resolved.ino == Some(ROOT_INO) {
            return Err(error::ebusy("cannot remove root"));
        }
        g.unlink_name(resolved.parent, &resolved.name, true, &creds)
    }

    pub fn remove_dir_all<P: AsRef<UnixPath>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        let meta = match self.symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.is_not_found() => return Ok(()),
            Err(e) => return Err(e),
        };
        if meta.is_dir() && !meta.is_symlink() {
            let children: Vec<_> = self
                .read_dir(path)?
                .filter_map(|e| e.ok())
                .filter(|e| e.name.as_slice() != b"." && e.name.as_slice() != b"..")
                .collect();
            for e in children {
                let child = path.join(UnixPath::from_bytes(&e.name));
                if e.file_type.is_dir() {
                    self.remove_dir_all(&child)?;
                } else {
                    self.remove_file(&child)?;
                }
            }
            self.remove_dir(path)
        } else {
            self.remove_file(path)
        }
    }

    pub fn rename<P: AsRef<UnixPath>, Q: AsRef<UnixPath>>(&self, from: P, to: Q) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let src = g.walk(ROOT_INO, ROOT_INO, from.as_ref(), false, &creds)?;
        if src.ino.is_none() {
            return Err(error::enoent("no such file or directory"));
        }
        let dst = g.walk(ROOT_INO, ROOT_INO, to.as_ref(), false, &creds)?;
        g.rename(src.parent, &src.name, dst.parent, &dst.name, &creds)
    }

    pub fn copy<P: AsRef<UnixPath>, Q: AsRef<UnixPath>>(&self, from: P, to: Q) -> Result<u64> {
        let data = self.read(from.as_ref())?;
        let meta = self.metadata(from.as_ref())?;
        self.write(to.as_ref(), &data)?;
        self.set_permissions(to.as_ref(), meta.permissions())?;
        Ok(data.len() as u64)
    }

    pub fn metadata<P: AsRef<UnixPath>>(&self, path: P) -> Result<Metadata> {
        Ok(Metadata {
            stat: self.stat(path)?,
        })
    }

    pub fn symlink_metadata<P: AsRef<UnixPath>>(&self, path: P) -> Result<Metadata> {
        Ok(Metadata {
            stat: self.lstat(path)?,
        })
    }

    pub fn stat<P: AsRef<UnixPath>>(&self, path: P) -> Result<Stat> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        g.stat_ino(ino)
    }

    pub fn lstat<P: AsRef<UnixPath>>(&self, path: P) -> Result<Stat> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), false, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        g.stat_ino(ino)
    }

    pub fn exists<P: AsRef<UnixPath>>(&self, path: P) -> bool {
        self.metadata(path).is_ok()
    }

    pub fn canonicalize<P: AsRef<UnixPath>>(&self, path: P) -> Result<UnixPathBuf> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        drop(g);
        self.path_of(ino)
    }

    fn path_of(&self, target: u64) -> Result<UnixPathBuf> {
        if target == ROOT_INO {
            return Ok(UnixPathBuf::from("/"));
        }
        let mut components = Vec::new();
        let mut cur = target;
        let mut g = self.lock()?;
        for _ in 0..4096 {
            if cur == ROOT_INO {
                break;
            }
            let parent = g
                .dir_lookup(cur, b"..")?
                .map(|(p, _)| p)
                .unwrap_or(ROOT_INO);
            let entries = g.read_dir_entries(parent)?;
            let name = entries
                .into_iter()
                .find(|(ino, _, n)| *ino == cur && n.as_slice() != b"." && n.as_slice() != b"..")
                .map(|(_, _, n)| n)
                .ok_or_else(|| error::enoent("orphaned inode"))?;
            components.push(name);
            cur = parent;
        }
        drop(g);
        let mut buf = UnixPathBuf::from("/");
        for name in components.into_iter().rev() {
            buf.push(UnixPath::from_bytes(&name));
        }
        Ok(buf)
    }

    pub fn read_link<P: AsRef<UnixPath>>(&self, path: P) -> Result<UnixPathBuf> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), false, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        let inode = g.get_inode(ino)?;
        if !inode.is_symlink() {
            return Err(error::einval("not a symbolic link"));
        }
        let mut buf = vec![0u8; inode.size as usize];
        g.read_data(ino, 0, &mut buf)?;
        Ok(UnixPathBuf::from_bytes(buf))
    }

    pub fn hard_link<P: AsRef<UnixPath>, Q: AsRef<UnixPath>>(&self, original: P, link: Q) -> Result<()> {
        self.link(original, link)
    }

    pub fn link<P: AsRef<UnixPath>, Q: AsRef<UnixPath>>(&self, old: P, new: Q) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let src = g.walk(ROOT_INO, ROOT_INO, old.as_ref(), true, &creds)?;
        let ino = src.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        let dst = g.walk(ROOT_INO, ROOT_INO, new.as_ref(), false, &creds)?;
        if dst.ino.is_some() {
            return Err(error::eexist("file exists"));
        }
        g.link_into(ino, dst.parent, &dst.name, &creds)
    }

    pub fn symlink<P: AsRef<UnixPath>, Q: AsRef<UnixPath>>(&self, target: P, link: Q) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let dst = g.walk(ROOT_INO, ROOT_INO, link.as_ref(), false, &creds)?;
        if dst.ino.is_some() {
            return Err(error::eexist("file exists"));
        }
        let mode = S_IFLNK | 0o777;
        let ino = g.create_child(
            dst.parent,
            &dst.name,
            mode,
            creds.fsuid,
            creds.fsgid,
            0,
            &creds,
        )?;
        let bytes = target.as_ref().as_bytes();
        g.write_data(ino, 0, bytes)?;
        Ok(())
    }

    pub fn read_dir<P: AsRef<UnixPath>>(&self, path: P) -> Result<ReadDir> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        let entries = g.read_dir_entries(ino)?;
        Ok(ReadDir::new(
            entries
                .into_iter()
                .map(|(ino, file_type, name)| DirEntry {
                    ino,
                    file_type,
                    name,
                })
                .collect(),
        ))
    }

    pub fn set_permissions<P: AsRef<UnixPath>>(&self, path: P, perm: Permissions) -> Result<()> {
        self.chmod(path, perm.mode)
    }

    pub fn chmod<P: AsRef<UnixPath>>(&self, path: P, mode: u16) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        g.chmod(ino, mode, &creds)
    }

    pub fn chown<P: AsRef<UnixPath>>(&self, path: P, uid: u32, gid: u32) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        g.chown(ino, Some(uid), Some(gid), &creds)
    }

    pub fn lchown<P: AsRef<UnixPath>>(&self, path: P, uid: u32, gid: u32) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), false, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        g.chown(ino, Some(uid), Some(gid), &creds)
    }

    pub fn set_times<P: AsRef<UnixPath>>(&self, path: P, times: FileTimes) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        g.set_times(ino, times)
    }

    pub fn set_len<P: AsRef<UnixPath>>(&self, path: P, size: u64) -> Result<()> {
        self.truncate(path, size)
    }

    pub fn truncate<P: AsRef<UnixPath>>(&self, path: P, size: u64) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        let inode = g.get_inode(ino)?;
        if inode.is_dir() {
            return Err(error::eisdir("is a directory"));
        }
        g.truncate_data(ino, size)
    }

    pub fn mknod<P: AsRef<UnixPath>>(&self, path: P, mode: u16, dev: u64) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), false, &creds)?;
        if r.ino.is_some() {
            return Err(error::eexist("file exists"));
        }
        g.create_child(r.parent, &r.name, mode, creds.fsuid, creds.fsgid, dev, &creds)?;
        Ok(())
    }

    pub fn mkfifo<P: AsRef<UnixPath>>(&self, path: P, mode: u16) -> Result<()> {
        self.mknod(path, S_IFIFO | (mode & 0o777), 0)
    }

    pub fn getxattr<P: AsRef<UnixPath>>(&self, path: P, name: &str) -> Result<Vec<u8>> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        g.getxattr(ino, name.as_bytes())
    }

    pub fn setxattr<P: AsRef<UnixPath>>(&self, path: P, name: &str, value: &[u8], flags: u32) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        g.setxattr(ino, name.as_bytes(), value, flags)
    }

    pub fn listxattr<P: AsRef<UnixPath>>(&self, path: P) -> Result<Vec<String>> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        Ok(g.listxattr(ino)?
            .into_iter()
            .map(|n| String::from_utf8_lossy(&n).into_owned())
            .collect())
    }

    pub fn removexattr<P: AsRef<UnixPath>>(&self, path: P, name: &str) -> Result<()> {
        let creds = Self::creds();
        let mut g = self.lock()?;
        let r = g.walk(ROOT_INO, ROOT_INO, path.as_ref(), true, &creds)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        g.removexattr(ino, name.as_bytes())
    }

    pub fn access<P: AsRef<UnixPath>>(&self, path: P, mode: u32) -> Result<()> {
        let meta = self.metadata(path)?;
        if mode == crate::types::F_OK {
            return Ok(());
        }
        let creds = Self::creds();
        let accs = [
            (crate::types::R_OK, crate::types::Access::Read),
            (crate::types::W_OK, crate::types::Access::Write),
            (crate::types::X_OK, crate::types::Access::Exec),
        ];
        for (bit, acc) in accs {
            if mode & bit != 0
                && !crate::types::check_access(meta.stat.mode, meta.stat.uid, meta.stat.gid, &creds, acc)
            {
                return Err(error::eacces("access"));
            }
        }
        Ok(())
    }

    pub(crate) fn inner(&self) -> Arc<RwLock<Inner>> {
        self.inner.clone()
    }
}

impl Drop for LinuxFile {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            if let Ok(mut g) = self.inner.write() {
                let _ = g.flush_all();
            }
        }
    }
}
