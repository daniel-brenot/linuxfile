# linuxfile

A Linux filesystem with [`std::fs`](https://doc.rust-lang.org/std/fs/) semantics, stored in a **single host file**.

The image grows when you write data and shrinks when trailing space is freed. Use it as a normal filesystem library, or as the VFS layer for a container / kernel-like runtime.

## Why this exists

Typical disk images (ext4, raw `.img` files) are fixed-size and need a kernel or FUSE to mount. `linuxfile` is a userspace library:

- One file on the host is the entire filesystem
- Paths, inodes, permissions, and file types follow Linux — even on Windows
- The host file expands and contracts with the data you store
- You can open files with a `std::fs`-style API, or drive it with file descriptors, `*at` calls, `chdir`, and `chroot`

That makes it a fit for embedding a Linux root filesystem inside another program (for example a container runtime).

## Features

| Area | What you get |
|---|---|
| File types | Regular files, directories, symlinks, hard links, FIFOs, sockets, char/block devices |
| Metadata | uid/gid, mode (including setuid/setgid/sticky), atime/mtime/ctime/btime |
| I/O | Sparse files, truncate, append, unlink-while-open |
| Linux extras | xattrs, `mknod` / `mkfifo`, `chmod` / `chown` |
| Container VFS | `Context` with creds, cwd, root, umask, and an fd table |
| Reliability | Checksummed superblock + inodes, metadata journal, backup superblock |
| Performance | Extent allocator, inline small files, write-back block cache |

Paths inside the image are always Unix-style (`/etc/hostname`), on every host OS.

## Add it to a project

This repo is a normal Cargo library:

```toml
[dependencies]
linuxfile = "*"
```

## Quick start

Create an image, write a file, read it back:

```rust
use linuxfile::LinuxFile;

fn main() -> linuxfile::Result<()> {
    let fs = LinuxFile::create("disk.img")?;
    fs.create_dir_all("/etc")?;
    fs.write("/etc/hostname", b"container\n")?;

    assert_eq!(fs.read("/etc/hostname")?, b"container\n");
    assert_eq!(fs.read_to_string("/etc/hostname")?, "container\n");

    fs.sync()?;
    Ok(())
}
```

Reopen an existing image later:

```rust
let fs = LinuxFile::open("disk.img")?;
println!("{}", fs.read_to_string("/etc/hostname")?);
```

`LinuxFile::open_read_only` mounts the image without writes.

## `std::fs`-style API

Most operations take a Unix path and behave like the standard library:

```rust
use linuxfile::{LinuxFile, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

let fs = LinuxFile::create("disk.img")?;

fs.create_dir_all("/var/tmp")?;
fs.write("/var/tmp/note", b"hello")?;
fs.rename("/var/tmp/note", "/var/tmp/hello.txt")?;
fs.copy("/var/tmp/hello.txt", "/var/tmp/copy.txt")?;

assert!(fs.exists("/var/tmp/hello.txt"));
assert!(fs.metadata("/var/tmp")?.is_dir());

for entry in fs.read_dir("/var/tmp")? {
    let entry = entry?;
    println!("{} ino={}", entry.file_name_str(), entry.ino);
}

fs.remove_file("/var/tmp/copy.txt")?;
fs.remove_dir_all("/var")?;
```

Open a handle when you need `Read` / `Write` / `Seek`:

```rust
let mut opts = OpenOptions::new();
opts.read(true).write(true).create(true);

let mut file = fs.open_opts("/data.bin", &opts)?;
file.write_all(&[1, 2, 3])?;
file.seek(SeekFrom::Start(0))?;

let mut buf = [0u8; 3];
file.read_exact(&mut buf)?;
file.sync_all()?;
```

`open_file` is read-only (like `std::fs::File::open`). `create_file` is write-only and truncates (like `std::fs::File::create`).

## Linux filesystem operations

```rust
use linuxfile::{make_dev, LinuxFile, S_IFCHR};

let fs = LinuxFile::open("disk.img")?;

fs.write("/a", b"data")?;
fs.symlink("/a", "/link")?;          // symbolic link
fs.hard_link("/a", "/alias")?;       // hard link
assert_eq!(fs.read_link("/link")?, "/a".into());
assert_eq!(fs.lstat("/link")?.nlink, 1);
assert_eq!(fs.stat("/alias")?.nlink, 2);

fs.chmod("/a", 0o640)?;
fs.chown("/a", 1000, 1000)?;

fs.setxattr("/a", "user.comment", b"hi", 0)?;
assert_eq!(fs.getxattr("/a", "user.comment")?, b"hi");
fs.removexattr("/a", "user.comment")?;

fs.create_dir("/dev")?;
fs.mkfifo("/dev/log", 0o644)?;
fs.mknod("/dev/null", S_IFCHR | 0o666, make_dev(1, 3))?;
```

Holes are free: seeking past the end and writing creates a sparse file. Reads in the hole return zeros.

## Container / kernel-style API

`Context` is a process view over the same image: credentials, current directory, chroot, umask, and file descriptors.

```rust
use linuxfile::{Creds, LinuxFile, O_CREAT, O_RDWR, O_RDONLY};

let fs = LinuxFile::open("disk.img")?;
fs.create_dir_all("/home/app")?;
fs.write("/home/app/main.rs", b"fn main() {}")?;

let mut ctx = fs.context(Creds::new(1000, 1000));
ctx.enable_permission_checks(true)?;
ctx.set_umask(0o022);

ctx.chdir("/home")?;
let fd = ctx.open("app/main.rs", O_RDONLY, 0)?;
let mut buf = [0u8; 32];
let n = ctx.read(fd, &mut buf)?;
ctx.close(fd)?;
assert_eq!(&buf[..n], b"fn main() {}");

// Isolate the process at /home/app (like chroot).
ctx.chroot("/home/app")?;
assert!(ctx.stat("/main.rs").is_ok());
assert!(ctx.stat("/home").is_err());

let out = ctx.open("/out.txt", O_RDWR | O_CREAT, 0o644)?;
ctx.write(out, b"ok")?;
ctx.fsync(out)?;
ctx.close(out)?;
```

`Context` also implements the `*at` family used by a syscall layer: `openat`, `mkdirat`, `unlinkat`, `renameat`, `fstatat`, plus `dup`, `dup2`, `lseek`, `ftruncate`, `getdents`, and `fchdir`.

Errors carry a Linux errno (`ENOENT`, `EEXIST`, `EISDIR`, …) and convert to `std::io::Error`.

## Image growth and shrink

The host file is not a fixed-size disk:

1. Writes that need new blocks extend the image (rounded up in 1 MiB steps).
2. Deletes free extents; adjacent free ranges are merged.
3. If the tail of the file is unused, `sync` truncates it (keeping a little slack so the file does not thrash).

```rust
let before = fs.image_len()?;
fs.write("/big", &vec![0u8; 512 * 1024])?;
fs.sync()?;
let mid = fs.image_len()?;

fs.remove_file("/big")?;
fs.sync()?;
let after = fs.image_len()?;

assert!(mid > before);
assert!(after < mid);
```

`statfs` reports block size, used/free blocks, and inode counts.

## Creating an image with options

```rust
use linuxfile::{CreateOptions, LinuxFile, SyncMode};

let fs = LinuxFile::create_with(
    "disk.img",
    CreateOptions::new()
        .initial_blocks(1024) // 4 MiB to start (4 KiB blocks)
        .cache_blocks(4096)   // 16 MiB write-back cache
        .journal_blocks(256)  // 1 MiB metadata journal
        .sync(SyncMode::Ordered),
)?;
```

| `SyncMode` | Behavior |
|---|---|
| `None` | Fastest. Persist on `sync()` or when the last `LinuxFile` handle is dropped. |
| `Ordered` | Default. Journal metadata; file data is written before the inode update. |
| `Full` | `fsync` the image after each metadata transaction. |

Use `Ordered` for a container disk you care about. Use `None` while building an image, then call `sync()` once at the end.

## How it is laid out

The host file is a small custom filesystem (not ext4):

```
[ superblock 4 KiB ]
[ backup superblock ]
[ metadata journal  ]
[ inode table       ]
[ file data extents ]
```

- Block size is 4096 bytes; inodes are 256 bytes with CRC32.
- Files that fit in 128 bytes live in the inode (no extra block).
- Larger files use extents; holes are not allocated.
- Directories, xattrs, and the free-space map are stored in the same allocator.

You do not mount this with the Linux kernel. All access goes through this crate.

## Tests

```bash
cargo test
```

The suite covers create/read/write, directories, symlinks and hard links, sparse files, xattrs/devices, persist + reopen, unlink-while-open, image grow/shrink, and the `Context` fd/`chroot` path.
