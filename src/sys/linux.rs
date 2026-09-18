//! Linux backend. The root is an owned directory fd; every operation resolves the parent
//! directory beneath it with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
//! RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV)` (or a per-component `O_NOFOLLOW` walk with
//! `st_dev` checks on kernels without `openat2`) and then acts with a single `*at` call on
//! that held parent fd and a one-component leaf name. See DESIGN.md §6.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, ResolveFlags, Stat, fchmod, fstat, fstatfs,
    fsync, mkdirat, openat, openat2, renameat_with, statat, unlinkat,
};
use rustix::io::Errno;

use crate::path::{PRIVATE_DIR, RelPath};
use crate::vfs::{FileId, Kind, LockGuard, Meta, Probe, Vfs, parent_mismatch};

pub(crate) struct LinuxFs {
    root: OwnedFd,
    root_dev: u64,
    has_openat2: bool,
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn resolve_flags() -> ResolveFlags {
    ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_MAGICLINKS
        | ResolveFlags::NO_XDEV
}

fn meta(st: &Stat) -> Meta {
    let kind = match FileType::from_raw_mode(st.st_mode) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    };
    Meta {
        kind,
        id: FileId {
            dev: st.st_dev,
            ino: st.st_ino,
        },
        len: st.st_size as u64,
        mode: st.st_mode & 0o7777,
        #[allow(clippy::useless_conversion)]
        nlink: u64::from(st.st_nlink),
    }
}

fn leaf(p: &RelPath) -> io::Result<&OsStr> {
    p.file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "operation on the root itself"))
}

fn is_absent(e: Errno) -> bool {
    e == Errno::NOENT || e == Errno::NOTDIR
}

impl LinuxFs {
    pub(crate) fn open(root: &Path) -> io::Result<LinuxFs> {
        let fd = rustix::fs::open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let root_dev = fstat(&fd)?.st_dev;
        let has_openat2 = match openat2(&fd, ".", DIR_FLAGS, Mode::empty(), resolve_flags()) {
            Ok(_) => true,
            Err(Errno::NOSYS) => false,
            Err(e) => return Err(e.into()),
        };
        Ok(LinuxFs {
            root: fd,
            root_dev,
            has_openat2,
        })
    }

    /// Opens the directory at `rel` beneath the root without following any symlink.
    fn open_dir_raw(&self, rel: &RelPath) -> Result<OwnedFd, Errno> {
        if rel.is_root() {
            return openat(&self.root, ".", DIR_FLAGS, Mode::empty());
        }
        if self.has_openat2 {
            let mut joined = OsString::new();
            for (i, c) in rel.components().iter().enumerate() {
                if i > 0 {
                    joined.push("/");
                }
                joined.push(c);
            }
            return openat2(
                &self.root,
                joined.as_os_str(),
                DIR_FLAGS,
                Mode::empty(),
                resolve_flags(),
            );
        }
        let mut cur = openat(&self.root, ".", DIR_FLAGS, Mode::empty())?;
        for c in rel.components() {
            let next = openat(&cur, c.as_os_str(), DIR_FLAGS, Mode::empty())?;
            if fstat(&next)?.st_dev != self.root_dev {
                return Err(Errno::XDEV);
            }
            cur = next;
        }
        Ok(cur)
    }

    fn open_dir(&self, rel: &RelPath) -> io::Result<OwnedFd> {
        self.open_dir_raw(rel).map_err(Into::into)
    }

    fn open_parent(&self, p: &RelPath, expect: Option<FileId>) -> io::Result<OwnedFd> {
        let fd = self.open_dir(&p.parent().unwrap_or_default())?;
        if let Some(want) = expect {
            let st = fstat(&fd)?;
            if (FileId {
                dev: st.st_dev,
                ino: st.st_ino,
            }) != want
            {
                return Err(parent_mismatch());
            }
        }
        Ok(fd)
    }

    fn open_regular(&self, p: &RelPath, flags: OFlags) -> io::Result<File> {
        let pfd = self.open_parent(p, None)?;
        let fd = openat(
            &pfd,
            leaf(p)?,
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?;
        if FileType::from_raw_mode(fstat(&fd)?.st_mode) != FileType::RegularFile {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok(File::from(fd))
    }
}

impl Vfs for LinuxFs {
    fn root_id(&self) -> io::Result<FileId> {
        let st = fstat(&self.root)?;
        Ok(FileId {
            dev: st.st_dev,
            ino: st.st_ino,
        })
    }

    fn stat(&self, p: &RelPath) -> io::Result<Option<Meta>> {
        if p.is_root() {
            return Ok(Some(meta(&fstat(&self.root)?)));
        }
        let pfd = match self.open_dir_raw(&p.parent().unwrap_or_default()) {
            Ok(fd) => fd,
            Err(e) if is_absent(e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match statat(&pfd, leaf(p)?, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => Ok(Some(meta(&st))),
            Err(e) if is_absent(e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn probe(&self, p: &RelPath, expect_parent: FileId) -> io::Result<Probe> {
        let pfd = match self.open_dir_raw(&p.parent().unwrap_or_default()) {
            Ok(fd) => fd,
            Err(e) if is_absent(e) => return Ok(Probe::ParentMismatch),
            Err(e) => return Err(e.into()),
        };
        let pst = fstat(&pfd)?;
        if (FileId {
            dev: pst.st_dev,
            ino: pst.st_ino,
        }) != expect_parent
        {
            return Ok(Probe::ParentMismatch);
        }
        match statat(&pfd, leaf(p)?, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => Ok(Probe::Present(meta(&st))),
            Err(e) if is_absent(e) => Ok(Probe::Missing),
            Err(e) => Err(e.into()),
        }
    }

    fn read_file(&self, p: &RelPath) -> io::Result<Vec<u8>> {
        let mut f = self.open_regular(p, OFlags::RDONLY)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn read_dir(&self, p: &RelPath) -> io::Result<Vec<OsString>> {
        let fd = self.open_dir(p)?;
        let mut names = Vec::new();
        for entry in Dir::read_from(&fd)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                names
                    .push(<OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(name).to_os_string());
            }
        }
        Ok(names)
    }

    fn create_file(&self, p: &RelPath, data: &[u8], mode: Option<u32>) -> io::Result<Meta> {
        let pfd = self.open_parent(p, None)?;
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = openat(&pfd, leaf(p)?, flags, Mode::from_bits_truncate(0o666))?;
        if let Some(m) = mode {
            fchmod(&fd, Mode::from_bits_truncate(m))?;
        }
        let mut f = File::from(fd);
        f.write_all(data)?;
        Ok(meta(&fstat(&f)?))
    }

    fn append(&self, p: &RelPath, data: &[u8]) -> io::Result<()> {
        let mut f = self.open_regular(p, OFlags::WRONLY | OFlags::APPEND)?;
        f.write_all(data)
    }

    fn mkdir(&self, p: &RelPath) -> io::Result<Meta> {
        let pfd = self.open_parent(p, None)?;
        mkdirat(&pfd, leaf(p)?, Mode::from_bits_truncate(0o777))?;
        Ok(meta(&statat(&pfd, leaf(p)?, AtFlags::SYMLINK_NOFOLLOW)?))
    }

    fn rename_noreplace(
        &self,
        from: &RelPath,
        expect_from_parent: Option<FileId>,
        to: &RelPath,
        expect_to_parent: Option<FileId>,
    ) -> io::Result<()> {
        let from_fd = self.open_parent(from, expect_from_parent)?;
        let to_fd = self.open_parent(to, expect_to_parent)?;
        renameat_with(
            &from_fd,
            leaf(from)?,
            &to_fd,
            leaf(to)?,
            RenameFlags::NOREPLACE,
        )?;
        Ok(())
    }

    fn unlink(&self, p: &RelPath, expect_parent: Option<FileId>) -> io::Result<()> {
        let pfd = self.open_parent(p, expect_parent)?;
        unlinkat(&pfd, leaf(p)?, AtFlags::empty())?;
        Ok(())
    }

    fn rmdir(&self, p: &RelPath) -> io::Result<()> {
        let pfd = self.open_parent(p, None)?;
        unlinkat(&pfd, leaf(p)?, AtFlags::REMOVEDIR)?;
        Ok(())
    }

    fn sync_file(&self, p: &RelPath) -> io::Result<()> {
        let f = self.open_regular(p, OFlags::RDONLY)?;
        fsync(&f)?;
        Ok(())
    }

    fn sync_dir(&self, p: &RelPath) -> io::Result<()> {
        fsync(self.open_dir(p)?)?;
        Ok(())
    }

    fn lock(&self, exclusive: bool) -> io::Result<LockGuard> {
        let dir = match self.open_dir_raw(&RelPath::from_parts([PRIVATE_DIR])) {
            Ok(fd) => fd,
            Err(e) if !exclusive && is_absent(e) => return Ok(Box::new(())),
            Err(e) => return Err(e.into()),
        };
        let mut flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        if exclusive {
            flags |= OFlags::CREATE;
        }
        let fd = match openat(&dir, "lock", flags, Mode::from_bits_truncate(0o600)) {
            Ok(fd) => fd,
            Err(e) if !exclusive && is_absent(e) => return Ok(Box::new(())),
            Err(e) => return Err(e.into()),
        };
        let f = File::from(fd);
        if exclusive {
            f.lock()?
        } else {
            f.lock_shared()?
        }
        Ok(Box::new(f))
    }

    fn untrusted_fs_type(&self) -> io::Result<Option<&'static str>> {
        let magic = (fstatfs(&self.root)?.f_type as u64) & 0xFFFF_FFFF;
        Ok(match magic {
            0x6969 => Some("nfs"),
            0x517B => Some("smb"),
            0xFF53_4D42 => Some("cifs"),
            0xFE53_4D42 => Some("smb2"),
            0x6573_5546 => Some("fuse"),
            0x0102_1997 => Some("9p"),
            0x00C3_6400 => Some("ceph"),
            0x5346_414F => Some("afs"),
            _ => None,
        })
    }

    fn probe_cache_key(&self) -> Option<FileId> {
        self.root_id().ok()
    }
}
