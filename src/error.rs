//! Linux-style errors that also convert to [`std::io::Error`].

use std::fmt;
use std::io;

/// Filesystem error with a Linux errno and an [`io::ErrorKind`].
#[derive(Debug, Clone)]
pub struct Error {
    kind: io::ErrorKind,
    errno: i32,
    message: Option<String>,
}

impl Error {
    pub fn new(kind: io::ErrorKind, errno: i32, message: impl Into<String>) -> Self {
        Self {
            kind,
            errno,
            message: Some(message.into()),
        }
    }

    pub fn from_errno(errno: i32) -> Self {
        let (kind, default) = errno_info(errno);
        Self {
            kind,
            errno,
            message: Some(default.to_string()),
        }
    }

    pub fn kind(&self) -> io::ErrorKind {
        self.kind
    }

    /// Linux errno value (`ENOENT`, `EEXIST`, …).
    pub fn errno(&self) -> i32 {
        self.errno
    }

    pub fn is_not_found(&self) -> bool {
        self.errno == ENOENT
    }

    pub fn is_exists(&self) -> bool {
        self.errno == EEXIST
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.message {
            Some(m) => write!(f, "{m} (errno {})", self.errno),
            None => write!(f, "filesystem error (errno {})", self.errno),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for io::Error {
    fn from(err: Error) -> Self {
        io::Error::new(err.kind, err)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        let kind = err.kind();
        let errno = kind_to_errno(kind);
        Self {
            kind,
            errno,
            message: Some(err.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const EIO: i32 = 5;
pub const ENXIO: i32 = 6;
pub const EBADF: i32 = 9;
pub const EAGAIN: i32 = 11;
pub const ENOMEM: i32 = 12;
pub const EACCES: i32 = 13;
#[allow(dead_code)]
pub const EFAULT: i32 = 14;
pub const EBUSY: i32 = 16;
pub const EEXIST: i32 = 17;
pub const EXDEV: i32 = 18;
pub const ENODEV: i32 = 19;
pub const ENOTDIR: i32 = 20;
pub const EISDIR: i32 = 21;
pub const EINVAL: i32 = 22;
pub const ENFILE: i32 = 23;
pub const EMFILE: i32 = 24;
pub const ENOTTY: i32 = 25;
pub const EFBIG: i32 = 27;
pub const ENOSPC: i32 = 28;
pub const ESPIPE: i32 = 29;
pub const EROFS: i32 = 30;
pub const EMLINK: i32 = 31;
pub const EPIPE: i32 = 32;
pub const ERANGE: i32 = 34;
pub const ENAMETOOLONG: i32 = 36;
#[allow(dead_code)]
pub const ENOLCK: i32 = 37;
pub const ENOTEMPTY: i32 = 39;
pub const ELOOP: i32 = 40;
#[allow(dead_code)]
pub const ENOMSG: i32 = 42;
pub const EOVERFLOW: i32 = 75;
pub const ENOTSUP: i32 = 95;
#[allow(dead_code)]
pub const EOPNOTSUPP: i32 = 95;
pub const ENODATA: i32 = 61;
#[allow(dead_code)]
pub const ENOATTR: i32 = 61;

fn errno_info(errno: i32) -> (io::ErrorKind, &'static str) {
    match errno {
        EPERM => (io::ErrorKind::PermissionDenied, "operation not permitted"),
        ENOENT => (io::ErrorKind::NotFound, "no such file or directory"),
        EIO => (io::ErrorKind::Other, "input/output error"),
        ENXIO => (io::ErrorKind::NotFound, "no such device or address"),
        EBADF => (io::ErrorKind::InvalidInput, "bad file descriptor"),
        EAGAIN => (io::ErrorKind::WouldBlock, "resource temporarily unavailable"),
        ENOMEM => (io::ErrorKind::OutOfMemory, "out of memory"),
        EACCES => (io::ErrorKind::PermissionDenied, "permission denied"),
        EBUSY => (io::ErrorKind::Other, "device or resource busy"),
        EEXIST => (io::ErrorKind::AlreadyExists, "file exists"),
        EXDEV => (io::ErrorKind::Other, "invalid cross-device link"),
        ENODEV => (io::ErrorKind::NotFound, "no such device"),
        ENOTDIR => (io::ErrorKind::NotADirectory, "not a directory"),
        EISDIR => (io::ErrorKind::IsADirectory, "is a directory"),
        EINVAL => (io::ErrorKind::InvalidInput, "invalid argument"),
        ENFILE => (io::ErrorKind::Other, "too many open files in system"),
        EMFILE => (io::ErrorKind::Other, "too many open files"),
        ENOTTY => (io::ErrorKind::Other, "inappropriate ioctl"),
        EFBIG => (io::ErrorKind::FileTooLarge, "file too large"),
        ENOSPC => (io::ErrorKind::StorageFull, "no space left on device"),
        ESPIPE => (io::ErrorKind::InvalidInput, "illegal seek"),
        EROFS => (io::ErrorKind::Other, "read-only file system"),
        EMLINK => (io::ErrorKind::Other, "too many links"),
        EPIPE => (io::ErrorKind::BrokenPipe, "broken pipe"),
        ENAMETOOLONG => (io::ErrorKind::InvalidFilename, "file name too long"),
        ENOTEMPTY => (io::ErrorKind::DirectoryNotEmpty, "directory not empty"),
        ELOOP => (io::ErrorKind::InvalidData, "too many levels of symbolic links"),
        EOVERFLOW => (io::ErrorKind::InvalidData, "value too large"),
        ENOTSUP => (io::ErrorKind::Unsupported, "operation not supported"),
        ENODATA => (io::ErrorKind::NotFound, "no data available"),
        _ => (io::ErrorKind::Other, "filesystem error"),
    }
}

fn kind_to_errno(kind: io::ErrorKind) -> i32 {
    match kind {
        io::ErrorKind::NotFound => ENOENT,
        io::ErrorKind::PermissionDenied => EACCES,
        io::ErrorKind::AlreadyExists => EEXIST,
        io::ErrorKind::WouldBlock => EAGAIN,
        io::ErrorKind::InvalidInput => EINVAL,
        io::ErrorKind::InvalidData => EINVAL,
        io::ErrorKind::TimedOut => EAGAIN,
        io::ErrorKind::WriteZero => EIO,
        io::ErrorKind::Interrupted => EAGAIN,
        io::ErrorKind::Unsupported => ENOTSUP,
        io::ErrorKind::UnexpectedEof => EIO,
        io::ErrorKind::OutOfMemory => ENOMEM,
        io::ErrorKind::BrokenPipe => EPIPE,
        _ => EIO,
    }
}

pub fn enoent(msg: impl Into<String>) -> Error {
    Error::new(io::ErrorKind::NotFound, ENOENT, msg)
}

pub fn eexist(msg: impl Into<String>) -> Error {
    Error::new(io::ErrorKind::AlreadyExists, EEXIST, msg)
}

pub fn einval(msg: impl Into<String>) -> Error {
    Error::new(io::ErrorKind::InvalidInput, EINVAL, msg)
}

pub fn eio(msg: impl Into<String>) -> Error {
    Error::new(io::ErrorKind::Other, EIO, msg)
}

pub fn enotdir(msg: impl Into<String>) -> Error {
    Error::new(io::ErrorKind::NotADirectory, ENOTDIR, msg)
}

pub fn eisdir(msg: impl Into<String>) -> Error {
    Error::new(io::ErrorKind::IsADirectory, EISDIR, msg)
}

#[allow(dead_code)]
pub fn enospc() -> Error {
    Error::from_errno(ENOSPC)
}

pub fn eacces(msg: impl Into<String>) -> Error {
    Error::new(io::ErrorKind::PermissionDenied, EACCES, msg)
}

pub fn eperm(msg: impl Into<String>) -> Error {
    Error::new(io::ErrorKind::PermissionDenied, EPERM, msg)
}

pub fn ebadf() -> Error {
    Error::from_errno(EBADF)
}

pub fn eloop() -> Error {
    Error::from_errno(ELOOP)
}

pub fn enametoolong() -> Error {
    Error::from_errno(ENAMETOOLONG)
}

pub fn enotempty() -> Error {
    Error::from_errno(ENOTEMPTY)
}

#[allow(dead_code)]
pub fn enotsup(msg: impl Into<String>) -> Error {
    Error::new(io::ErrorKind::Unsupported, ENOTSUP, msg)
}

pub fn ebusy(msg: impl Into<String>) -> Error {
    Error::new(io::ErrorKind::Other, EBUSY, msg)
}
