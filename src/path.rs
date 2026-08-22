//! Unix path types. Names are raw bytes (any byte except `NUL` and `/`).

use std::fmt;
use std::ops::Deref;

use crate::error::{self, Error, Result};

/// Maximum filename length (Linux `NAME_MAX`).
pub const NAME_MAX: usize = 255;
/// Maximum symlink follow depth (Linux `SYMLOOP_MAX` / 40).
pub const SYMLOOP_MAX: usize = 40;

/// Borrowed Unix path (`/`-separated, no host `std::path` semantics).
#[derive(PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct UnixPath {
    inner: [u8],
}

/// Owned Unix path.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct UnixPathBuf {
    inner: Vec<u8>,
}

impl UnixPath {
    pub fn new<S: AsRef<[u8]> + ?Sized>(s: &S) -> &Self {
        Self::from_bytes(s.as_ref())
    }

    pub fn from_bytes(bytes: &[u8]) -> &Self {
        // SAFETY: UnixPath is a transparent wrapper over [u8].
        unsafe { &*(bytes as *const [u8] as *const UnixPath) }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.inner
    }

    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.inner).ok()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn is_absolute(&self) -> bool {
        self.inner.first() == Some(&b'/')
    }

    pub fn is_root(&self) -> bool {
        self.inner == *b"/"
    }

    pub fn parent(&self) -> Option<&UnixPath> {
        if self.is_root() || self.inner.is_empty() {
            return None;
        }
        let bytes = self.as_bytes();
        let end = bytes.iter().rposition(|&b| b != b'/').map(|i| i + 1)?;
        let slash = bytes[..end].iter().rposition(|&b| b == b'/')?;
        if slash == 0 {
            Some(UnixPath::from_bytes(b"/"))
        } else {
            Some(UnixPath::from_bytes(&bytes[..slash]))
        }
    }

    pub fn file_name(&self) -> Option<&[u8]> {
        if self.is_root() {
            return None;
        }
        let bytes = trim_trailing_slashes(self.as_bytes());
        if bytes.is_empty() {
            return None;
        }
        match bytes.iter().rposition(|&b| b == b'/') {
            Some(i) => Some(&bytes[i + 1..]),
            None => Some(bytes),
        }
    }

    pub fn components(&self) -> Components<'_> {
        Components {
            bytes: self.as_bytes(),
            abs: self.is_absolute(),
            started: false,
        }
    }

    pub fn to_path_buf(&self) -> UnixPathBuf {
        UnixPathBuf {
            inner: self.inner.to_vec(),
        }
    }

    pub fn join<P: AsRef<UnixPath>>(&self, other: P) -> UnixPathBuf {
        let other = other.as_ref();
        if other.is_absolute() {
            return other.to_path_buf();
        }
        let mut buf = self.to_path_buf();
        buf.push(other);
        buf
    }
}

impl fmt::Debug for UnixPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&String::from_utf8_lossy(&self.inner), f)
    }
}

impl fmt::Display for UnixPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.inner))
    }
}

impl AsRef<UnixPath> for UnixPath {
    fn as_ref(&self) -> &UnixPath {
        self
    }
}

impl AsRef<UnixPath> for UnixPathBuf {
    fn as_ref(&self) -> &UnixPath {
        self
    }
}

impl AsRef<UnixPath> for str {
    fn as_ref(&self) -> &UnixPath {
        UnixPath::from_bytes(self.as_bytes())
    }
}

impl AsRef<UnixPath> for String {
    fn as_ref(&self) -> &UnixPath {
        UnixPath::from_bytes(self.as_bytes())
    }
}

impl AsRef<UnixPath> for [u8] {
    fn as_ref(&self) -> &UnixPath {
        UnixPath::from_bytes(self)
    }
}

impl AsRef<UnixPath> for Vec<u8> {
    fn as_ref(&self) -> &UnixPath {
        UnixPath::from_bytes(self)
    }
}

impl Deref for UnixPathBuf {
    type Target = UnixPath;
    fn deref(&self) -> &UnixPath {
        UnixPath::from_bytes(&self.inner)
    }
}

impl UnixPathBuf {
    pub fn new() -> Self {
        Self { inner: Vec::new() }
    }

    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            inner: bytes.into(),
        }
    }

    pub fn as_path(&self) -> &UnixPath {
        self
    }

    pub fn push<P: AsRef<UnixPath>>(&mut self, path: P) {
        let path = path.as_ref();
        if path.is_absolute() {
            self.inner = path.as_bytes().to_vec();
            return;
        }
        if !self.inner.is_empty() && !self.inner.ends_with(&[b'/']) {
            self.inner.push(b'/');
        }
        self.inner.extend_from_slice(path.as_bytes());
    }

    pub fn pop(&mut self) -> bool {
        match self.parent() {
            Some(p) => {
                self.inner = p.as_bytes().to_vec();
                true
            }
            None => false,
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.inner
    }
}

impl fmt::Debug for UnixPathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_path(), f)
    }
}

impl fmt::Display for UnixPathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_path(), f)
    }
}

impl From<&str> for UnixPathBuf {
    fn from(s: &str) -> Self {
        Self::from_bytes(s.as_bytes())
    }
}

impl From<String> for UnixPathBuf {
    fn from(s: String) -> Self {
        Self::from_bytes(s.into_bytes())
    }
}

impl From<&UnixPath> for UnixPathBuf {
    fn from(p: &UnixPath) -> Self {
        p.to_path_buf()
    }
}

/// Iterator over path components (skips empty and `.` is yielded).
pub struct Components<'a> {
    bytes: &'a [u8],
    abs: bool,
    started: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component<'a> {
    RootDir,
    CurDir,
    ParentDir,
    Normal(&'a [u8]),
}

impl<'a> Iterator for Components<'a> {
    type Item = Component<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.started {
            self.started = true;
            if self.abs {
                self.bytes = trim_leading_slashes(self.bytes);
                return Some(Component::RootDir);
            }
        }
        if self.bytes.is_empty() {
            return None;
        }
        self.bytes = trim_leading_slashes(self.bytes);
        if self.bytes.is_empty() {
            return None;
        }
        let end = self
            .bytes
            .iter()
            .position(|&b| b == b'/')
            .unwrap_or(self.bytes.len());
        let part = &self.bytes[..end];
        self.bytes = &self.bytes[end..];
        Some(match part {
            b"." => Component::CurDir,
            b".." => Component::ParentDir,
            other => Component::Normal(other),
        })
    }
}

fn trim_leading_slashes(b: &[u8]) -> &[u8] {
    let n = b.iter().take_while(|&&c| c == b'/').count();
    &b[n..]
}

fn trim_trailing_slashes(b: &[u8]) -> &[u8] {
    let n = b.iter().rev().take_while(|&&c| c == b'/').count();
    &b[..b.len() - n]
}

pub fn validate_name(name: &[u8]) -> Result<()> {
    if name.is_empty() || name == b"." || name == b".." {
        return Err(error::einval("invalid filename"));
    }
    if name.len() > NAME_MAX {
        return Err(error::enametoolong());
    }
    if name.contains(&0) || name.contains(&b'/') {
        return Err(Error::from_errno(error::EINVAL));
    }
    Ok(())
}
