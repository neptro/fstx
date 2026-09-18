//! Layout of the private area and the durable markers (DESIGN.md §4).
//!
//! ```text
//! <root>/.fstx/lock                 advisory lock
//! <root>/.fstx/tx-<id>/             one transaction
//!     staged/b<n>, staged/d<n>      new files / new directories (tokens)
//!     backup/<k>                    detached base entries (tokens)
//!     journal (via journal.tmp)     the plan, published by atomic rename
//!     progress                      APPLY/UNDO wave records (lower bound only)
//!     COMMITTED | ROLLING_BACK | ROLLED_BACK   markers (O_EXCL + dir fsync)
//! <root>/.fstx/gc-tx-<id>/          settled transaction being deleted
//! <root>/.fstx/probe-*/             capability probe scratch space
//! ```

use std::io;

use crate::path::{PRIVATE_DIR, RelPath};
use crate::vfs::{Kind, Vfs};

pub(crate) const TX_PREFIX: &str = "tx-";
pub(crate) const GC_PREFIX: &str = "gc-";
pub(crate) const PROBE_PREFIX: &str = "probe-";

pub(crate) fn private_dir() -> RelPath {
    RelPath::from_parts([PRIVATE_DIR])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Marker {
    Committed,
    RollingBack,
    RolledBack,
}

impl Marker {
    fn name(self) -> &'static str {
        match self {
            Marker::Committed => "COMMITTED",
            Marker::RollingBack => "ROLLING_BACK",
            Marker::RolledBack => "ROLLED_BACK",
        }
    }
}

/// Paths of one transaction directory.
#[derive(Clone, Debug)]
pub(crate) struct TxDir {
    pub name: String,
}

impl TxDir {
    pub fn dir(&self) -> RelPath {
        private_dir().join(&self.name)
    }
    pub fn staged(&self) -> RelPath {
        self.dir().join("staged")
    }
    pub fn backup(&self) -> RelPath {
        self.dir().join("backup")
    }
    pub fn journal(&self) -> RelPath {
        self.dir().join("journal")
    }
    pub fn journal_tmp(&self) -> RelPath {
        self.dir().join("journal.tmp")
    }
    pub fn progress(&self) -> RelPath {
        self.dir().join("progress")
    }
    pub fn marker(&self, m: Marker) -> RelPath {
        self.dir().join(m.name())
    }

    pub fn has_marker(&self, vfs: &dyn Vfs, m: Marker) -> io::Result<bool> {
        Ok(vfs.stat(&self.marker(m))?.is_some())
    }

    /// Creates a marker durably (C4: exclusive create + fsync of the tx directory).
    pub fn set_marker(&self, vfs: &dyn Vfs, m: Marker) -> io::Result<()> {
        match vfs.create_file(&self.marker(m), b"", None) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        vfs.sync_dir(&self.dir())
    }

    /// Settles the transaction: one atomic rename to `gc-<name>` (after which its contents
    /// are garbage), then deletion.
    pub fn gc(&self, vfs: &dyn Vfs) -> io::Result<()> {
        let gc = private_dir().join(format!("{GC_PREFIX}{}", self.name));
        vfs.rename_noreplace(&self.dir(), None, &gc, None)?;
        vfs.sync_dir(&private_dir())?;
        delete_tree(vfs, &gc)?;
        vfs.sync_dir(&private_dir())
    }
}

/// Recursively deletes `p` without following symlinks. Only used inside `.fstx/`.
pub(crate) fn delete_tree(vfs: &dyn Vfs, p: &RelPath) -> io::Result<()> {
    debug_assert!(p.is_private());
    match vfs.stat(p)? {
        None => Ok(()),
        Some(m) if m.kind == Kind::Dir => {
            for name in vfs.read_dir(p)? {
                delete_tree(vfs, &p.join(name))?;
            }
            vfs.rmdir(p)
        }
        Some(_) => vfs.unlink(p, None),
    }
}
