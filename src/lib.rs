//! `linuxfile` — a Linux filesystem with [`std::fs`] semantics, stored in one file.
//!
//! The host image grows when you write data and shrinks when trailing space is
//! freed. The API mirrors `std::fs` (`read`, `write`, `OpenOptions`, `rename`,
//! …) and also exposes a [`Context`] with file descriptors, `*at` syscalls,
//! `chdir`, and `chroot` for container-kernel use. Optional LZ4 record
//! compression (`CreateOptions::compression`) works like ZFS: each 32 KiB
//! record is compressed independently and stored raw when that would not save
//! a block.
//!
//! # Example
//!
//! ```no_run
//! use linuxfile::LinuxFile;
//!
//! let fs = LinuxFile::create("disk.img")?;
//! fs.create_dir_all("/etc")?;
//! fs.write("/etc/hostname", b"container\n")?;
//! assert_eq!(fs.read("/etc/hostname")?, b"container\n");
//! fs.sync()?;
//! # Ok::<(), linuxfile::Error>(())
//! ```

mod alloc;
mod cache;
mod compress;
mod context;
mod crc;
mod error;
mod file;
mod format;
mod fs;
mod inner;
mod journal;
mod path;
mod store;
mod types;

pub use compress::Compression;
pub use context::Context;
pub use error::{
    Error, Result, EACCES, EAGAIN, EBADF, EBUSY, EEXIST, EFBIG, EINVAL, EIO, EISDIR, ELOOP,
    EMLINK, ENOENT, ENOMEM, ENOSPC, ENOTDIR, ENOTEMPTY, ENOTSUP, EPERM, EPIPE, EROFS, EXDEV,
};
pub use file::{DirEntry, File, OpenOptions, ReadDir};
pub use fs::LinuxFile;
pub use path::{UnixPath, UnixPathBuf, NAME_MAX};
pub use types::{
    apply_umask, check_access, major, make_dev, minor, Access, Creds, FileTimes, FileType,
    Metadata, Permissions, Stat, StatFs, Timespec, AT_EMPTY_PATH, AT_FDCWD,
    AT_REMOVEDIR, AT_SYMLINK_FOLLOW, AT_SYMLINK_NOFOLLOW, F_OK, MODE_PERM, O_APPEND, O_CLOEXEC,
    O_CREAT, O_DIRECTORY, O_DSYNC, O_EXCL, O_NOATIME, O_NOCTTY, O_NOFOLLOW, O_NONBLOCK, O_PATH,
    O_RDONLY, O_RDWR, O_SYNC, O_TRUNC, O_WRONLY, R_OK, SEEK_CUR, SEEK_DATA, SEEK_END, SEEK_HOLE,
    SEEK_SET, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK, S_IRGRP,
    S_IROTH, S_IRUSR, S_IRWXG, S_IRWXO, S_IRWXU, S_ISGID, S_ISUID, S_ISVTX, S_IWGRP, S_IWOTH,
    S_IWUSR, S_IXGRP, S_IXOTH, S_IXUSR, W_OK, XATTR_CREATE, XATTR_REPLACE, X_OK,
};

/// How aggressively metadata is flushed to the host file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Buffer everything until [`LinuxFile::sync`] or drop.
    None,
    /// Journal metadata; write file data before the inode update (default).
    Ordered,
    /// `fsync` the image after each metadata transaction.
    Full,
}

/// Options for creating or opening an image.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    pub journal_blocks: u32,
    pub initial_blocks: u64,
    pub cache_blocks: usize,
    pub sync: SyncMode,
    /// Algorithm used for new files. Existing records keep their own algorithm.
    pub compression: Compression,
    /// Logical 4 KiB blocks per compression record (clamped to 2..=32, power of two).
    pub record_blocks: u32,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            journal_blocks: format::DEFAULT_JOURNAL_BLOCKS,
            initial_blocks: format::DEFAULT_INITIAL_BLOCKS,
            cache_blocks: format::DEFAULT_CACHE_BLOCKS,
            sync: SyncMode::Ordered,
            compression: Compression::Off,
            record_blocks: compress::DEFAULT_RECORD_BLOCKS,
        }
    }
}

impl CreateOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn journal_blocks(mut self, n: u32) -> Self {
        self.journal_blocks = n;
        self
    }

    pub fn initial_blocks(mut self, n: u64) -> Self {
        self.initial_blocks = n;
        self
    }

    pub fn cache_blocks(mut self, n: usize) -> Self {
        self.cache_blocks = n;
        self
    }

    pub fn sync(mut self, mode: SyncMode) -> Self {
        self.sync = mode;
        self
    }

    /// Enable ZFS-style record compression (`Compression::Lz4` is `compression=on`).
    pub fn compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    pub fn record_blocks(mut self, n: u32) -> Self {
        self.record_blocks = n;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn tmp() -> (LinuxFile, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "linuxfile-{}-{}.img",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let fs = LinuxFile::create_with(
            &path,
            CreateOptions::default()
                .initial_blocks(256)
                .journal_blocks(32)
                .cache_blocks(64)
                .sync(SyncMode::None),
        )
        .unwrap();
        (fs, path)
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn write_read_roundtrip() {
        let (fs, path) = tmp();
        fs.write("/hello.txt", b"world").unwrap();
        assert_eq!(fs.read("/hello.txt").unwrap(), b"world");
        assert_eq!(fs.read_to_string("/hello.txt").unwrap(), "world");
        cleanup(&path);
    }

    #[test]
    fn directories() {
        let (fs, path) = tmp();
        fs.create_dir_all("/usr/bin").unwrap();
        fs.write("/usr/bin/true", b"").unwrap();
        assert!(fs.metadata("/usr/bin").unwrap().is_dir());
        let names: Vec<_> = fs
            .read_dir("/usr")
            .unwrap()
            .map(|e| e.unwrap().file_name_str())
            .collect();
        assert!(names.iter().any(|n| n == "bin"));
        cleanup(&path);
    }

    #[test]
    fn symlink_and_hardlink() {
        let (fs, path) = tmp();
        fs.write("/a", b"data").unwrap();
        fs.symlink("/a", "/b").unwrap();
        assert_eq!(fs.read("/b").unwrap(), b"data");
        assert!(fs.lstat("/b").unwrap().is_symlink());
        fs.hard_link("/a", "/c").unwrap();
        assert_eq!(fs.stat("/c").unwrap().nlink, 2);
        fs.remove_file("/a").unwrap();
        assert_eq!(fs.read("/c").unwrap(), b"data");
        cleanup(&path);
    }

    #[test]
    fn rename_and_remove() {
        let (fs, path) = tmp();
        fs.create_dir_all("/var/tmp").unwrap();
        fs.write("/var/tmp/x", b"1").unwrap();
        fs.rename("/var/tmp/x", "/var/y").unwrap();
        assert_eq!(fs.read("/var/y").unwrap(), b"1");
        fs.remove_dir_all("/var").unwrap();
        assert!(!fs.exists("/var"));
        cleanup(&path);
    }

    #[test]
    fn sparse_and_truncate() {
        let (fs, path) = tmp();
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(true);
        let mut f = fs.open_opts("/sparse", &opts).unwrap();
        f.seek(SeekFrom::Start(1 << 20)).unwrap();
        f.write_all(b"end").unwrap();
        assert_eq!(f.metadata().unwrap().len(), (1 << 20) + 3);
        let mut buf = [0u8; 3];
        f.seek(SeekFrom::Start(0)).unwrap();
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"\0\0\0");
        fs.truncate("/sparse", 2).unwrap();
        assert_eq!(fs.metadata("/sparse").unwrap().len(), 2);
        cleanup(&path);
    }

    #[test]
    fn grow_and_shrink_image() {
        let (fs, path) = tmp();
        let before = fs.image_len().unwrap();
        fs.write("/big", &vec![0xAB; 512 * 1024]).unwrap();
        fs.sync().unwrap();
        let mid = fs.image_len().unwrap();
        assert!(mid > before, "image should grow: {before} -> {mid}");
        fs.remove_file("/big").unwrap();
        fs.sync().unwrap();
        let after = fs.image_len().unwrap();
        assert!(after < mid, "image should shrink: {mid} -> {after}");
        cleanup(&path);
    }

    #[test]
    fn xattr_chmod_chown_mknod() {
        let (fs, path) = tmp();
        fs.write("/f", b"x").unwrap();
        fs.setxattr("/f", "user.foo", b"bar", 0).unwrap();
        assert_eq!(fs.getxattr("/f", "user.foo").unwrap(), b"bar");
        assert!(fs.listxattr("/f").unwrap().contains(&"user.foo".into()));
        fs.removexattr("/f", "user.foo").unwrap();
        fs.chmod("/f", 0o600).unwrap();
        assert_eq!(fs.metadata("/f").unwrap().permissions().mode & 0o777, 0o600);
        fs.chown("/f", 1000, 1000).unwrap();
        assert_eq!(fs.metadata("/f").unwrap().uid(), 1000);
        fs.mkfifo("/pipe", 0o644).unwrap();
        assert!(fs.metadata("/pipe").unwrap().file_type().is_fifo());
        fs.create_dir("/dev").unwrap();
        fs.mknod("/dev/null", S_IFCHR | 0o666, make_dev(1, 3)).unwrap();
        let st = fs.stat("/dev/null").unwrap();
        assert!(st.file_type().is_char_device());
        assert_eq!(major(st.rdev), 1);
        assert_eq!(minor(st.rdev), 3);
        cleanup(&path);
    }

    #[test]
    fn persist_reopen() {
        let (fs, path) = tmp();
        fs.create_dir_all("/etc").unwrap();
        fs.write("/etc/os-release", b"NAME=linuxfile\n").unwrap();
        fs.sync().unwrap();
        drop(fs);
        let fs = LinuxFile::open(&path).unwrap();
        assert_eq!(fs.read_to_string("/etc/os-release").unwrap(), "NAME=linuxfile\n");
        cleanup(&path);
    }

    #[test]
    fn context_fds_and_chroot() {
        let (fs, path) = tmp();
        fs.create_dir_all("/home/app").unwrap();
        fs.write("/home/app/main.rs", b"fn main() {}").unwrap();
        let mut ctx = fs.context(Creds::root());
        ctx.chdir("/home").unwrap();
        let fd = ctx.open("app/main.rs", O_RDONLY, 0).unwrap();
        let mut buf = [0u8; 12];
        let n = ctx.read(fd, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"fn main() {}");
        ctx.close(fd).unwrap();
        ctx.chroot("/home/app").unwrap();
        assert!(ctx.stat("/main.rs").is_ok());
        assert!(ctx.stat("/home").is_err());
        cleanup(&path);
    }

    #[test]
    fn unlink_while_open() {
        let (fs, path) = tmp();
        fs.write("/tmpfile", b"keep").unwrap();
        let mut f = fs.open_file("/tmpfile").unwrap();
        fs.remove_file("/tmpfile").unwrap();
        assert!(!fs.exists("/tmpfile"));
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"keep");
        drop(f);
        cleanup(&path);
    }

    fn tmp_lz4() -> (LinuxFile, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "linuxfile-lz4-{}-{}.img",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let fs = LinuxFile::create_with(
            &path,
            CreateOptions::default()
                .initial_blocks(256)
                .journal_blocks(32)
                .cache_blocks(64)
                .sync(SyncMode::None)
                .compression(Compression::Lz4),
        )
        .unwrap();
        (fs, path)
    }

    #[test]
    fn compression_roundtrip_and_saves_space() {
        let payload = vec![b'A'; 512 * 1024];

        let (plain, p1) = tmp();
        plain.write("/big", &payload).unwrap();
        plain.sync().unwrap();
        let plain_blocks = plain.metadata("/big").unwrap().stat.blocks;
        drop(plain);

        let (lz4, p2) = tmp_lz4();
        assert_eq!(lz4.compression().unwrap(), Compression::Lz4);
        lz4.write("/big", &payload).unwrap();
        lz4.sync().unwrap();
        assert_eq!(lz4.read("/big").unwrap(), payload);
        let lz4_blocks = lz4.metadata("/big").unwrap().stat.blocks;
        assert!(
            lz4_blocks < plain_blocks / 4,
            "lz4 should store far less: {lz4_blocks} vs {plain_blocks}"
        );
        drop(lz4);
        cleanup(&p1);
        cleanup(&p2);
    }

    #[test]
    fn compression_incompressible_and_partial_overwrite() {
        let (fs, path) = tmp_lz4();
        let mut data: Vec<u8> = (0..64 * 1024).map(|i: u32| (i.wrapping_mul(17) % 251) as u8).collect();
        fs.write("/rand", &data).unwrap();
        assert_eq!(fs.read("/rand").unwrap(), data);

        data[1000..1100].fill(b'Z');
        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        let mut f = fs.open_opts("/rand", &opts).unwrap();
        f.seek(SeekFrom::Start(1000)).unwrap();
        f.write_all(&[b'Z'; 100]).unwrap();
        drop(f);
        assert_eq!(fs.read("/rand").unwrap(), data);
        cleanup(&path);
    }

    #[test]
    fn compression_persist_sparse_truncate() {
        let (fs, path) = tmp_lz4();
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true);
        let mut f = fs.open_opts("/sparse", &opts).unwrap();
        f.seek(SeekFrom::Start(1 << 16)).unwrap();
        f.write_all(&vec![b'B'; 4096]).unwrap();
        drop(f);
        assert_eq!(fs.metadata("/sparse").unwrap().len(), (1 << 16) + 4096);
        let hole = fs.read("/sparse").unwrap();
        assert!(hole[..1 << 16].iter().all(|&b| b == 0));
        assert_eq!(&hole[1 << 16..], &vec![b'B'; 4096]);

        fs.truncate("/sparse", 100).unwrap();
        assert_eq!(fs.metadata("/sparse").unwrap().len(), 100);
        assert_eq!(fs.read("/sparse").unwrap(), vec![0u8; 100]);

        fs.write("/keep", &vec![b'C'; 8000]).unwrap();
        fs.sync().unwrap();
        drop(fs);
        let fs = LinuxFile::open(&path).unwrap();
        assert_eq!(fs.file_compression("/keep").unwrap(), Compression::Lz4);
        assert_eq!(fs.read("/keep").unwrap(), vec![b'C'; 8000]);
        cleanup(&path);
    }

    #[test]
    fn set_compression_on_existing_file() {
        let (fs, path) = tmp();
        fs.write("/f", &vec![b'D'; 16 * 1024]).unwrap();
        fs.set_compression("/f", Compression::Lz4).unwrap();
        assert_eq!(fs.file_compression("/f").unwrap(), Compression::Lz4);
        fs.write("/f", &vec![b'E'; 64 * 1024]).unwrap();
        fs.sync().unwrap();
        assert_eq!(fs.read("/f").unwrap(), vec![b'E'; 64 * 1024]);
        let blocks = fs.metadata("/f").unwrap().stat.blocks;
        assert!(blocks < (64 * 1024 / 512), "rewritten file should compress: {blocks}");
        cleanup(&path);
    }
}
