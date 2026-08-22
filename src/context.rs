//! Process-like context for a container kernel: creds, root, cwd, and fds.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{self, Result};
use crate::file::{File, OpenOptions};
use crate::format::ROOT_INO;
use crate::fs::LinuxFile;
use crate::path::UnixPath;
use crate::types::{
    apply_umask, Creds, Stat, AT_EMPTY_PATH, AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_NOFOLLOW, O_DIRECTORY,
    S_IFDIR, S_IFREG, SEEK_CUR, SEEK_END, SEEK_SET,
};

struct Fd {
    file: File,
    flags: i32,
    dir_ino: Option<u64>,
}

/// Isolated namespace + file-descriptor table over a [`LinuxFile`].
pub struct Context {
    fs: LinuxFile,
    creds: Creds,
    cwd: u64,
    root: u64,
    umask: u16,
    fds: BTreeMap<i32, Fd>,
    next_fd: i32,
}

impl Context {
    pub fn new(fs: LinuxFile, creds: Creds) -> Self {
        Self {
            fs,
            creds,
            cwd: ROOT_INO,
            root: ROOT_INO,
            umask: 0o022,
            fds: BTreeMap::new(),
            next_fd: 3,
        }
    }

    pub fn creds(&self) -> &Creds {
        &self.creds
    }

    pub fn set_creds(&mut self, creds: Creds) {
        self.creds = creds;
    }

    pub fn umask(&self) -> u16 {
        self.umask
    }

    pub fn set_umask(&mut self, mask: u16) -> u16 {
        let old = self.umask;
        self.umask = mask & 0o777;
        old
    }

    pub fn enable_permission_checks(&self, enable: bool) -> Result<()> {
        let mut g = self.fs.lock()?;
        g.check_perm = enable;
        Ok(())
    }

    fn alloc_fd(&mut self, fd: Fd) -> i32 {
        let mut n = self.next_fd;
        while self.fds.contains_key(&n) {
            n += 1;
        }
        self.fds.insert(n, fd);
        self.next_fd = n + 1;
        n
    }

    fn resolve_dirfd(&self, dirfd: i32) -> Result<u64> {
        if dirfd == AT_FDCWD {
            return Ok(self.cwd);
        }
        let fd = self.fds.get(&dirfd).ok_or_else(error::ebadf)?;
        fd.dir_ino.ok_or_else(|| error::enotdir("dirfd is not a directory"))
    }

    fn walk_at(
        &self,
        dirfd: i32,
        path: &UnixPath,
        follow_last: bool,
    ) -> Result<crate::inner::Resolved> {
        let mut g = self.fs.lock()?;
        if path.is_empty() {
            return Err(error::einval("empty path"));
        }
        let start = if path.is_absolute() {
            self.root
        } else {
            self.resolve_dirfd(dirfd)?
        };
        g.walk(self.root, start, path, follow_last, &self.creds)
    }

    pub fn open(&mut self, path: impl AsRef<UnixPath>, flags: i32, mode: u16) -> Result<i32> {
        self.openat(AT_FDCWD, path, flags, mode)
    }

    pub fn openat(
        &mut self,
        dirfd: i32,
        path: impl AsRef<UnixPath>,
        flags: i32,
        mode: u16,
    ) -> Result<i32> {
        let path = path.as_ref();
        let opts = OpenOptions::from_linux_flags(flags, mode);
        let follow = !opts.nofollow;
        let mut g = self.fs.lock()?;
        let start = if path.is_absolute() {
            self.root
        } else if dirfd == AT_FDCWD {
            self.cwd
        } else {
            self.fds
                .get(&dirfd)
                .and_then(|f| f.dir_ino)
                .ok_or_else(error::ebadf)?
        };
        let resolved = g.walk(self.root, start, path, follow, &self.creds)?;
        let ino = match resolved.ino {
            Some(ino) => {
                if opts.create_new {
                    return Err(error::eexist("file exists"));
                }
                let inode = g.get_inode(ino)?;
                if opts.directory && !inode.is_dir() {
                    return Err(error::enotdir("not a directory"));
                }
                if inode.is_dir() && opts.write && !opts.directory && flags & O_DIRECTORY == 0 {
                    let acc = flags & crate::types::O_ACCMODE;
                    if acc != crate::types::O_RDONLY && acc != crate::types::O_PATH {
                        return Err(error::eisdir("is a directory"));
                    }
                }
                if opts.truncate && inode.is_file() {
                    g.truncate_data(ino, 0)?;
                }
                ino
            }
            None => {
                if !opts.create && !opts.create_new {
                    return Err(error::enoent("no such file or directory"));
                }
                let mode = S_IFREG | apply_umask(opts.mode, self.umask);
                g.create_child(
                    resolved.parent,
                    &resolved.name,
                    mode,
                    self.creds.fsuid,
                    self.creds.fsgid,
                    0,
                    &self.creds,
                )?
            }
        };
        let inode = g.get_inode(ino)?;
        let dir_ino = if inode.is_dir() { Some(ino) } else { None };
        drop(g);
        let file = File::new(self.fs.inner(), ino, flags)?;
        Ok(self.alloc_fd(Fd {
            file,
            flags,
            dir_ino,
        }))
    }

    pub fn close(&mut self, fd: i32) -> Result<()> {
        self.fds.remove(&fd).ok_or_else(error::ebadf)?;
        Ok(())
    }

    pub fn read(&mut self, fd: i32, buf: &mut [u8]) -> Result<usize> {
        let f = self.fds.get_mut(&fd).ok_or_else(error::ebadf)?;
        Ok(f.file.read(buf)?)
    }

    pub fn write(&mut self, fd: i32, buf: &[u8]) -> Result<usize> {
        let f = self.fds.get_mut(&fd).ok_or_else(error::ebadf)?;
        Ok(f.file.write(buf)?)
    }

    pub fn lseek(&mut self, fd: i32, off: i64, whence: i32) -> Result<i64> {
        let f = self.fds.get_mut(&fd).ok_or_else(error::ebadf)?;
        let pos = match whence {
            SEEK_SET => SeekFrom::Start(off as u64),
            SEEK_CUR => SeekFrom::Current(off),
            SEEK_END => SeekFrom::End(off),
            _ => return Err(error::einval("bad whence")),
        };
        Ok(f.file.seek(pos)? as i64)
    }

    pub fn fstat(&mut self, fd: i32) -> Result<Stat> {
        let f = self.fds.get(&fd).ok_or_else(error::ebadf)?;
        f.file.stat()
    }

    pub fn ftruncate(&mut self, fd: i32, size: u64) -> Result<()> {
        let f = self.fds.get(&fd).ok_or_else(error::ebadf)?;
        f.file.set_len(size)
    }

    pub fn fsync(&mut self, fd: i32) -> Result<()> {
        let f = self.fds.get(&fd).ok_or_else(error::ebadf)?;
        f.file.sync_all()
    }

    pub fn dup(&mut self, fd: i32) -> Result<i32> {
        let src = self.fds.get(&fd).ok_or_else(error::ebadf)?;
        let file = src.file.try_clone()?;
        let flags = src.flags;
        let dir_ino = src.dir_ino;
        Ok(self.alloc_fd(Fd {
            file,
            flags,
            dir_ino,
        }))
    }

    pub fn dup2(&mut self, old: i32, new: i32) -> Result<i32> {
        if old == new {
            if !self.fds.contains_key(&old) {
                return Err(error::ebadf());
            }
            return Ok(new);
        }
        let src = self.fds.get(&old).ok_or_else(error::ebadf)?;
        let file = src.file.try_clone()?;
        let flags = src.flags;
        let dir_ino = src.dir_ino;
        self.fds.remove(&new);
        self.fds.insert(
            new,
            Fd {
                file,
                flags,
                dir_ino,
            },
        );
        Ok(new)
    }

    pub fn getdents(&mut self, fd: i32) -> Result<Vec<crate::file::DirEntry>> {
        let ino = self
            .fds
            .get(&fd)
            .and_then(|f| f.dir_ino)
            .ok_or_else(error::ebadf)?;
        let mut g = self.fs.lock()?;
        let entries = g.read_dir_entries(ino)?;
        Ok(entries
            .into_iter()
            .map(|(ino, file_type, name)| crate::file::DirEntry {
                ino,
                file_type,
                name,
            })
            .collect())
    }

    pub fn mkdir(&mut self, path: impl AsRef<UnixPath>, mode: u16) -> Result<()> {
        self.mkdirat(AT_FDCWD, path, mode)
    }

    pub fn mkdirat(&mut self, dirfd: i32, path: impl AsRef<UnixPath>, mode: u16) -> Result<()> {
        let path = path.as_ref();
        let start = if path.is_absolute() {
            self.root
        } else {
            self.resolve_dirfd(dirfd)?
        };
        let mut g = self.fs.lock()?;
        let r = g.walk(self.root, start, path, true, &self.creds)?;
        if r.ino.is_some() {
            return Err(error::eexist("file exists"));
        }
        let mode = S_IFDIR | apply_umask(mode, self.umask);
        g.create_child(
            r.parent,
            &r.name,
            mode,
            self.creds.fsuid,
            self.creds.fsgid,
            0,
            &self.creds,
        )?;
        Ok(())
    }

    pub fn unlink(&mut self, path: impl AsRef<UnixPath>) -> Result<()> {
        self.unlinkat(AT_FDCWD, path, 0)
    }

    pub fn rmdir(&mut self, path: impl AsRef<UnixPath>) -> Result<()> {
        self.unlinkat(AT_FDCWD, path, AT_REMOVEDIR)
    }

    pub fn unlinkat(&mut self, dirfd: i32, path: impl AsRef<UnixPath>, flags: i32) -> Result<()> {
        let r = self.walk_at(dirfd, path.as_ref(), false)?;
        if r.ino.is_none() {
            return Err(error::enoent("no such file or directory"));
        }
        let mut g = self.fs.lock()?;
        g.unlink_name(r.parent, &r.name, flags & AT_REMOVEDIR != 0, &self.creds)
    }

    pub fn rename(
        &mut self,
        old: impl AsRef<UnixPath>,
        new: impl AsRef<UnixPath>,
    ) -> Result<()> {
        self.renameat(AT_FDCWD, old, AT_FDCWD, new)
    }

    pub fn renameat(
        &mut self,
        olddirfd: i32,
        old: impl AsRef<UnixPath>,
        newdirfd: i32,
        new: impl AsRef<UnixPath>,
    ) -> Result<()> {
        let src = self.walk_at(olddirfd, old.as_ref(), false)?;
        let dst = self.walk_at(newdirfd, new.as_ref(), false)?;
        if src.ino.is_none() {
            return Err(error::enoent("no such file or directory"));
        }
        let mut g = self.fs.lock()?;
        g.rename(src.parent, &src.name, dst.parent, &dst.name, &self.creds)
    }

    pub fn chdir(&mut self, path: impl AsRef<UnixPath>) -> Result<()> {
        let r = self.walk_at(AT_FDCWD, path.as_ref(), true)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        let mut g = self.fs.lock()?;
        let inode = g.get_inode(ino)?;
        if !inode.is_dir() {
            return Err(error::enotdir("not a directory"));
        }
        self.cwd = ino;
        Ok(())
    }

    pub fn fchdir(&mut self, fd: i32) -> Result<()> {
        let ino = self
            .fds
            .get(&fd)
            .and_then(|f| f.dir_ino)
            .ok_or_else(error::ebadf)?;
        self.cwd = ino;
        Ok(())
    }

    pub fn chroot(&mut self, path: impl AsRef<UnixPath>) -> Result<()> {
        let r = self.walk_at(AT_FDCWD, path.as_ref(), true)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        let mut g = self.fs.lock()?;
        let inode = g.get_inode(ino)?;
        if !inode.is_dir() {
            return Err(error::enotdir("not a directory"));
        }
        if self.creds.check_chroot() {
            self.root = ino;
            self.cwd = ino;
            Ok(())
        } else {
            Err(error::eperm("chroot"))
        }
    }

    pub fn stat(&mut self, path: impl AsRef<UnixPath>) -> Result<Stat> {
        self.fstatat(AT_FDCWD, path, 0)
    }

    pub fn lstat(&mut self, path: impl AsRef<UnixPath>) -> Result<Stat> {
        self.fstatat(AT_FDCWD, path, AT_SYMLINK_NOFOLLOW)
    }

    pub fn fstatat(&mut self, dirfd: i32, path: impl AsRef<UnixPath>, flags: i32) -> Result<Stat> {
        let path = path.as_ref();
        if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
            return self.fstat(dirfd);
        }
        let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
        let r = self.walk_at(dirfd, path, follow)?;
        let ino = r.ino.ok_or_else(|| error::enoent("no such file or directory"))?;
        let mut g = self.fs.lock()?;
        g.stat_ino(ino)
    }

    pub fn fs(&self) -> &LinuxFile {
        &self.fs
    }
}

impl Creds {
    fn check_chroot(&self) -> bool {
        self.is_superuser()
    }
}
