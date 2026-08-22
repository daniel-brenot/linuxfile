//! Linux file types, modes, credentials, and stat structures.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// File type + permission bits (`stat.st_mode`).
pub const S_IFMT: u16 = 0o170000;
pub const S_IFSOCK: u16 = 0o140000;
pub const S_IFLNK: u16 = 0o120000;
pub const S_IFREG: u16 = 0o100000;
pub const S_IFBLK: u16 = 0o060000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFCHR: u16 = 0o020000;
pub const S_IFIFO: u16 = 0o010000;

pub const S_ISUID: u16 = 0o4000;
pub const S_ISGID: u16 = 0o2000;
pub const S_ISVTX: u16 = 0o1000;

pub const S_IRWXU: u16 = 0o700;
pub const S_IRUSR: u16 = 0o400;
pub const S_IWUSR: u16 = 0o200;
pub const S_IXUSR: u16 = 0o100;
pub const S_IRWXG: u16 = 0o070;
pub const S_IRGRP: u16 = 0o040;
pub const S_IWGRP: u16 = 0o020;
pub const S_IXGRP: u16 = 0o010;
pub const S_IRWXO: u16 = 0o007;
pub const S_IROTH: u16 = 0o004;
pub const S_IWOTH: u16 = 0o002;
pub const S_IXOTH: u16 = 0o001;

pub const MODE_PERM: u16 = 0o7777;

/// `access(2)` modes.
pub const F_OK: u32 = 0;
pub const X_OK: u32 = 1;
pub const W_OK: u32 = 2;
pub const R_OK: u32 = 4;

/// `lseek` whence.
pub const SEEK_SET: i32 = 0;
pub const SEEK_CUR: i32 = 1;
pub const SEEK_END: i32 = 2;
pub const SEEK_DATA: i32 = 3;
pub const SEEK_HOLE: i32 = 4;

/// `*at` flags.
pub const AT_FDCWD: i32 = -100;
pub const AT_SYMLINK_NOFOLLOW: i32 = 0x100;
pub const AT_REMOVEDIR: i32 = 0x200;
pub const AT_SYMLINK_FOLLOW: i32 = 0x400;
pub const AT_EMPTY_PATH: i32 = 0x1000;

/// Open flags (Linux).
pub const O_RDONLY: i32 = 0;
pub const O_WRONLY: i32 = 1;
pub const O_RDWR: i32 = 2;
pub const O_ACCMODE: i32 = 3;
pub const O_CREAT: i32 = 0o100;
pub const O_EXCL: i32 = 0o200;
pub const O_NOCTTY: i32 = 0o400;
pub const O_TRUNC: i32 = 0o1000;
pub const O_APPEND: i32 = 0o2000;
pub const O_NONBLOCK: i32 = 0o4000;
pub const O_DSYNC: i32 = 0o10000;
pub const O_SYNC: i32 = 0o4010000;
pub const O_DIRECTORY: i32 = 0o200000;
pub const O_NOFOLLOW: i32 = 0o400000;
pub const O_CLOEXEC: i32 = 0o2000000;
pub const O_NOATIME: i32 = 0o1000000;
pub const O_PATH: i32 = 0o10000000;

/// xattr flags.
pub const XATTR_CREATE: u32 = 1;
pub const XATTR_REPLACE: u32 = 2;

/// Default maximum file size (16 TiB). Soft cap for sanity.
pub const MAX_FILE_SIZE: u64 = 16 << 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Directory,
    Symlink,
    BlockDevice,
    CharDevice,
    Fifo,
    Socket,
}

impl FileType {
    pub fn from_mode(mode: u16) -> Self {
        match mode & S_IFMT {
            S_IFDIR => Self::Directory,
            S_IFLNK => Self::Symlink,
            S_IFBLK => Self::BlockDevice,
            S_IFCHR => Self::CharDevice,
            S_IFIFO => Self::Fifo,
            S_IFSOCK => Self::Socket,
            _ => Self::File,
        }
    }

    pub fn as_mode(self) -> u16 {
        match self {
            Self::File => S_IFREG,
            Self::Directory => S_IFDIR,
            Self::Symlink => S_IFLNK,
            Self::BlockDevice => S_IFBLK,
            Self::CharDevice => S_IFCHR,
            Self::Fifo => S_IFIFO,
            Self::Socket => S_IFSOCK,
        }
    }

    pub fn is_file(self) -> bool {
        matches!(self, Self::File)
    }
    pub fn is_dir(self) -> bool {
        matches!(self, Self::Directory)
    }
    pub fn is_symlink(self) -> bool {
        matches!(self, Self::Symlink)
    }
    pub fn is_block_device(self) -> bool {
        matches!(self, Self::BlockDevice)
    }
    pub fn is_char_device(self) -> bool {
        matches!(self, Self::CharDevice)
    }
    pub fn is_fifo(self) -> bool {
        matches!(self, Self::Fifo)
    }
    pub fn is_socket(self) -> bool {
        matches!(self, Self::Socket)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    pub mode: u16,
}

impl Permissions {
    pub fn from_mode(mode: u16) -> Self {
        Self {
            mode: mode & MODE_PERM,
        }
    }

    pub fn readonly(&self) -> bool {
        self.mode & 0o222 == 0
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        if readonly {
            self.mode &= !0o222;
        } else {
            self.mode |= 0o200;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: u32,
}

impl Timespec {
    pub fn now() -> Self {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => Self {
                sec: d.as_secs() as i64,
                nsec: d.subsec_nanos(),
            },
            Err(_) => Self { sec: 0, nsec: 0 },
        }
    }

    pub fn zero() -> Self {
        Self { sec: 0, nsec: 0 }
    }

    pub fn to_system_time(self) -> SystemTime {
        if self.sec >= 0 {
            UNIX_EPOCH + Duration::new(self.sec as u64, self.nsec)
        } else {
            UNIX_EPOCH
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileTimes {
    pub accessed: Option<Timespec>,
    pub modified: Option<Timespec>,
    pub created: Option<Timespec>,
}

impl Default for FileTimes {
    fn default() -> Self {
        Self {
            accessed: None,
            modified: None,
            created: None,
        }
    }
}

/// Full Linux `stat` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub ino: u64,
    pub mode: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub blocks: u64,
    pub blksize: u32,
    pub rdev: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub btime: Timespec,
}

impl Stat {
    pub fn file_type(&self) -> FileType {
        FileType::from_mode(self.mode)
    }

    pub fn permissions(&self) -> Permissions {
        Permissions::from_mode(self.mode)
    }

    pub fn is_file(&self) -> bool {
        self.file_type().is_file()
    }
    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }
    pub fn is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }
}

/// `std::fs::Metadata`-like view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub stat: Stat,
}

impl Metadata {
    pub fn file_type(&self) -> FileType {
        self.stat.file_type()
    }
    pub fn is_dir(&self) -> bool {
        self.stat.is_dir()
    }
    pub fn is_file(&self) -> bool {
        self.stat.is_file()
    }
    pub fn is_symlink(&self) -> bool {
        self.stat.is_symlink()
    }
    pub fn len(&self) -> u64 {
        self.stat.size
    }
    pub fn permissions(&self) -> Permissions {
        self.stat.permissions()
    }
    pub fn modified(&self) -> SystemTime {
        self.stat.mtime.to_system_time()
    }
    pub fn accessed(&self) -> SystemTime {
        self.stat.atime.to_system_time()
    }
    pub fn created(&self) -> SystemTime {
        self.stat.btime.to_system_time()
    }
    pub fn uid(&self) -> u32 {
        self.stat.uid
    }
    pub fn gid(&self) -> u32 {
        self.stat.gid
    }
    pub fn ino(&self) -> u64 {
        self.stat.ino
    }
    pub fn nlink(&self) -> u32 {
        self.stat.nlink
    }
    pub fn rdev(&self) -> u64 {
        self.stat.rdev
    }
}

/// Process credentials used for permission checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Creds {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub fsuid: u32,
    pub fsgid: u32,
}

impl Creds {
    pub fn root() -> Self {
        Self {
            uid: 0,
            gid: 0,
            groups: vec![0],
            fsuid: 0,
            fsgid: 0,
        }
    }

    pub fn new(uid: u32, gid: u32) -> Self {
        Self {
            uid,
            gid,
            groups: vec![gid],
            fsuid: uid,
            fsgid: gid,
        }
    }

    pub fn is_superuser(&self) -> bool {
        self.fsuid == 0
    }
}

impl Default for Creds {
    fn default() -> Self {
        Self::root()
    }
}

/// Filesystem-wide statistics (`statfs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatFs {
    pub block_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub avail_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
    pub namelen: u32,
    pub fsid: u64,
}

/// Permission bits needed for an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Exec,
}

pub fn check_access(mode: u16, uid: u32, gid: u32, creds: &Creds, access: Access) -> bool {
    if creds.is_superuser() {
        // Root can read/write anything; execute requires at least one +x bit.
        return match access {
            Access::Exec => (mode & 0o111) != 0 || FileType::from_mode(mode).is_dir(),
            _ => true,
        };
    }
    let bits = if creds.fsuid == uid {
        (mode >> 6) & 7
    } else if creds.fsgid == gid || creds.groups.contains(&gid) {
        (mode >> 3) & 7
    } else {
        mode & 7
    };
    let need = match access {
        Access::Read => 4,
        Access::Write => 2,
        Access::Exec => 1,
    };
    bits & need == need
}

pub fn apply_umask(mode: u16, umask: u16) -> u16 {
    (mode & MODE_PERM) & !umask
}

pub fn make_dev(major: u32, minor: u32) -> u64 {
    // Linux dev_t encoding (glibc 64-bit).
    let major = major as u64;
    let minor = minor as u64;
    ((major & 0xfff) << 8)
        | (minor & 0xff)
        | ((minor & 0xffff_ff00) << 12)
        | ((major & 0xffff_f000) << 32)
}

pub fn major(dev: u64) -> u32 {
    (((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000)) as u32
}

pub fn minor(dev: u64) -> u32 {
    ((dev & 0xff) | ((dev >> 12) & 0xffff_ff00)) as u32
}
