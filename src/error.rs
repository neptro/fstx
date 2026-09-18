use std::io;
use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every error `fstx` can return.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The path is not a valid transaction path (absolute, `..`, NUL, reserved `.fstx`, ...).
    #[error("invalid path {path:?}: {reason}")]
    InvalidPath { path: PathBuf, reason: &'static str },

    #[error("not found: {0:?}")]
    NotFound(PathBuf),

    #[error("already exists: {0:?}")]
    AlreadyExists(PathBuf),

    #[error("not a directory: {0:?}")]
    NotADirectory(PathBuf),

    #[error("is a directory: {0:?}")]
    IsADirectory(PathBuf),

    #[error("directory not empty: {0:?}")]
    DirectoryNotEmpty(PathBuf),

    /// Symlinks, FIFOs, sockets and devices cannot be the target of an operation in v0.1.
    #[error("unsupported file type at {0:?} (symlinks and special files are out of scope)")]
    UnsupportedFileType(PathBuf),

    /// A directory cannot be moved into itself.
    #[error("cannot move {from:?} into its own subtree {to:?}")]
    InvalidMove { from: PathBuf, to: PathBuf },

    /// On a case-insensitive filesystem the name collides with an existing sibling.
    #[error("{path:?} collides with existing entry {existing:?} on a case-insensitive filesystem")]
    CaseCollision { path: PathBuf, existing: PathBuf },

    /// An entry changed on disk between the operation and commit (a writer outside fstx).
    #[error("{0:?} was changed outside the transaction")]
    Conflict(PathBuf),

    /// The filesystem failed a required capability probe.
    #[error("unsupported filesystem: missing {missing:?}")]
    UnsupportedFilesystem { missing: Vec<&'static str> },

    /// The real-filesystem backend is only implemented for Linux in v0.1.
    #[error("fstx v0.1 only has a Linux backend")]
    UnsupportedPlatform,

    /// Recovery found a state it cannot interpret. Nothing was touched; see [`crate::inspect`].
    #[error("recovery required for {tx}: {reason}")]
    RecoveryRequired { tx: String, reason: String },

    /// Writing or syncing the COMMITTED marker failed. The transaction is either fully
    /// applied or will be rolled back by the next [`crate::recover`]; which one is unknown.
    #[error("commit outcome unknown for {tx}; run fstx::recover")]
    CommitOutcomeUnknown {
        tx: String,
        #[source]
        source: io::Error,
    },

    /// The commit failed and the in-process rollback failed too. The durable state is left
    /// for [`crate::recover`].
    #[error("commit failed ({cause}) and rollback failed ({rollback}); run fstx::recover")]
    RollbackFailed { cause: Box<Error>, rollback: Box<Error> },

    #[error(transparent)]
    Io(#[from] io::Error),
}
