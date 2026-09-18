//! The filesystem abstraction the protocol is written against.
//!
//! Every method takes paths relative to the root. Implementations must resolve them
//! without following symlinks in any component and must never leave the root (see
//! DESIGN.md §6). The real backend is `sys::linux::LinuxFs`; the crash tests use
//! `sim::SimFs`, which implements exactly the crash-consistency contract C1–C5.

use std::any::Any;
use std::ffi::OsString;
use std::io;

use serde::{Deserialize, Serialize};

use crate::path::RelPath;

/// Stable identity of a filesystem object: `(st_dev, st_ino)`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Symlink,
    /// FIFO, socket, device.
    Other,
}

/// `lstat`-style metadata (never follows a final symlink).
#[derive(Clone, Copy, Debug)]
pub struct Meta {
    pub kind: Kind,
    pub id: FileId,
    pub len: u64,
    /// Permission bits (`st_mode & 0o7777`).
    pub mode: u32,
    pub nlink: u64,
}

/// Result of probing a location while also checking the identity of its parent directory.
#[derive(Clone, Copy, Debug)]
pub enum Probe {
    /// The parent does not resolve, or resolves to a different directory than expected.
    ParentMismatch,
    Missing,
    Present(Meta),
}

/// Opaque guard for the root's `.fstx/lock`; dropping it releases the lock.
pub type LockGuard = Box<dyn Any + Send>;

/// Error payload used when a parent directory's identity does not match the journal.
#[derive(Debug)]
pub struct ParentMismatch;

impl std::fmt::Display for ParentMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("parent directory identity does not match the journal")
    }
}

impl std::error::Error for ParentMismatch {}

pub(crate) fn parent_mismatch() -> io::Error {
    io::Error::other(ParentMismatch)
}

/// Filesystem primitives used by fstx.
///
/// Mutating calls are the only ones that change state; "sync" calls are the only ones
/// that make changes durable (contract C2/C3). `expect_parent` arguments make the
/// implementation verify, on the same directory handle it then mutates through, that the
/// parent is the directory recorded in the journal.
pub trait Vfs: Send + Sync {
    /// Identity of the root directory.
    fn root_id(&self) -> io::Result<FileId>;

    /// `lstat`. `Ok(None)` if the entry or any parent component does not exist.
    fn stat(&self, p: &RelPath) -> io::Result<Option<Meta>>;

    /// Reads a regular file (refuses symlinks, FIFOs, devices).
    fn read_file(&self, p: &RelPath) -> io::Result<Vec<u8>>;

    /// Lists the names in a directory (without `.` and `..`).
    fn read_dir(&self, p: &RelPath) -> io::Result<Vec<OsString>>;

    /// Creates a new regular file exclusively (`O_EXCL`) with `data`. Not synced.
    /// `mode` = exact permission bits, or `None` for the umask default.
    fn create_file(&self, p: &RelPath, data: &[u8], mode: Option<u32>) -> io::Result<Meta>;

    /// Appends to an existing regular file. Not synced.
    fn append(&self, p: &RelPath, data: &[u8]) -> io::Result<()>;

    /// Creates a directory (umask default permissions). Not synced.
    fn mkdir(&self, p: &RelPath) -> io::Result<Meta>;

    /// Atomic rename that fails with `AlreadyExists` if `to` exists (never replaces).
    fn rename_noreplace(
        &self,
        from: &RelPath,
        expect_from_parent: Option<FileId>,
        to: &RelPath,
        expect_to_parent: Option<FileId>,
    ) -> io::Result<()>;

    /// Removes a non-directory entry (a symlink itself, never its target).
    fn unlink(&self, p: &RelPath, expect_parent: Option<FileId>) -> io::Result<()>;

    /// Removes an empty directory.
    fn rmdir(&self, p: &RelPath) -> io::Result<()>;

    /// `fsync` of a regular file: its data and size become durable (C3).
    fn sync_file(&self, p: &RelPath) -> io::Result<()>;

    /// `fsync` of a directory: its entry mutations become durable (C2).
    fn sync_dir(&self, p: &RelPath) -> io::Result<()>;

    /// Takes the `.fstx/lock` lock (exclusive or shared). `.fstx` must exist for exclusive
    /// locks; a shared lock on a root without `.fstx` returns a no-op guard.
    fn lock(&self, exclusive: bool) -> io::Result<LockGuard>;

    /// Name of the filesystem type if it is a network/FUSE filesystem fstx does not trust.
    fn untrusted_fs_type(&self) -> io::Result<Option<&'static str>> {
        Ok(None)
    }

    /// Key for caching capability probes, or `None` to probe every time.
    fn probe_cache_key(&self) -> Option<FileId> {
        None
    }

    /// Stats `p`, but only if its parent is the directory `expect_parent`.
    fn probe(&self, p: &RelPath, expect_parent: FileId) -> io::Result<Probe> {
        let parent = p.parent().unwrap_or_default();
        match self.stat(&parent)? {
            Some(m) if m.kind == Kind::Dir && m.id == expect_parent => {}
            _ => return Ok(Probe::ParentMismatch),
        }
        Ok(match self.stat(p)? {
            Some(m) => Probe::Present(m),
            None => Probe::Missing,
        })
    }

    /// Test hook: notes a protocol milestone (the simulator records these).
    fn event(&self, _name: &'static str) {}
}
