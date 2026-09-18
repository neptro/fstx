//! # fstx — atomic, crash-safe multi-file transactions
//!
//! Stage any number of writes, renames and removals inside a directory tree, then
//! [`commit`](Transaction::commit): after any crash, [`recover`] leaves the tree exactly in
//! its before-state or exactly in its after-state (after only if `commit` returned `Ok`).
//!
//! ```no_run
//! # fn main() -> fstx::Result<()> {
//! let mut tx = fstx::Transaction::begin("./project")?;
//! tx.write("config.toml", b"version = 2\n")?;
//! tx.create_dir_all("src/gen")?;
//! tx.rename("old.rs", "src/gen/new.rs")?;
//! tx.remove("tmp.log")?;
//! tx.commit()?;
//! # Ok(()) }
//! ```
//!
//! How it works, the exact guarantees, and the assumptions they rest on are in
//! `DESIGN.md`. In short: every change to the tree is a no-replace rename of an entry
//! whose identity was recorded in a durable journal beforehand; renames run in waves
//! separated by fsync barriers; recovery locates entries by identity and undoes waves in
//! reverse under the same discipline.
//!
//! The real-filesystem backend is Linux-only in v0.1; on other platforms
//! [`Transaction::begin`] returns [`Error::UnsupportedPlatform`].

mod caps;
mod error;
mod inspect;
mod journal;
mod overlay;
mod path;
mod plan;
mod progress;
mod recover;
mod state;
mod sys;
mod transaction;
pub mod vfs;

#[cfg(feature = "sim")]
pub mod sim;

use std::path::Path;

pub use caps::Caps;
pub use error::{Error, Result};
pub use inspect::{Inspection, RecoveryAction, TokenInspection, TokenPosition, TxInspection};
pub use path::RelPath;
pub use recover::RecoveryReport;
pub use transaction::{Options, Transaction};

/// Recovers every interrupted transaction under `root` (also done by [`Transaction::begin`]).
pub fn recover(root: impl AsRef<Path>) -> Result<RecoveryReport> {
    recover_with(root, &Options::default())
}

pub fn recover_with(root: impl AsRef<Path>, opts: &Options) -> Result<RecoveryReport> {
    #[cfg(target_os = "linux")]
    {
        recover_on(&sys::linux::LinuxFs::open(root.as_ref())?, opts)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, opts);
        Err(Error::UnsupportedPlatform)
    }
}

/// [`recover`] on a custom [`vfs::Vfs`].
pub fn recover_on(vfs: &dyn vfs::Vfs, opts: &Options) -> Result<RecoveryReport> {
    if vfs.stat(&state::private_dir())?.is_none() {
        return Ok(RecoveryReport::default());
    }
    let _lock = vfs.lock(true)?;
    caps::probe(vfs, opts.allow_untested())?;
    recover::recover_locked(vfs)
}

/// Read-only report of what [`recover`] would do. Never modifies anything.
pub fn inspect(root: impl AsRef<Path>) -> Result<Inspection> {
    #[cfg(target_os = "linux")]
    {
        inspect_on(&sys::linux::LinuxFs::open(root.as_ref())?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        Err(Error::UnsupportedPlatform)
    }
}

/// [`inspect`] on a custom [`vfs::Vfs`].
pub fn inspect_on(vfs: &dyn vfs::Vfs) -> Result<Inspection> {
    inspect::inspect_on(vfs)
}
