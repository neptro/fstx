use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::caps::{self, Caps};
use crate::error::{Error, Result};
use crate::journal::{self, FORMAT, Journal};
use crate::overlay::Overlay;
use crate::path::{RelPath, validate_user_path};
use crate::plan::{self, Plan};
use crate::progress::Record;
use crate::recover::{self, RecoveryReport, append_progress, barrier};
use crate::state::{Marker, TX_PREFIX, TxDir, private_dir};
use crate::vfs::{FileId, Kind, LockGuard, Vfs};

/// Options for [`Transaction::begin_with`] and [`crate::recover_with`].
#[derive(Clone, Debug, Default)]
pub struct Options {
    allow_untested_fs: bool,
}

impl Options {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow network/FUSE filesystems. Their crash behaviour is outside the tested contract.
    pub(crate) fn allow_untested(&self) -> bool {
        self.allow_untested_fs
    }

    pub fn allow_untested_fs(mut self, yes: bool) -> Self {
        self.allow_untested_fs = yes;
        self
    }
}

/// A set of changes to a directory tree that becomes visible all at once, or not at all.
///
/// Operations are staged; nothing inside the tree changes until [`commit`](Self::commit).
/// Dropping the transaction without committing discards it.
pub struct Transaction {
    vfs: Box<dyn Vfs>,
    tx: TxDir,
    overlay: Overlay,
    blobs: BTreeMap<u32, FileId>,
    next_blob: u32,
    caps: Caps,
    recovered: RecoveryReport,
    finished: bool,
    // Declared last: released after the Drop impl has discarded the staging area.
    _lock: LockGuard,
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("tx", &self.tx.name)
            .finish_non_exhaustive()
    }
}

fn tx_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{TX_PREFIX}{nanos:x}-{:x}-{:x}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) fn ensure_private_dir(vfs: &dyn Vfs) -> Result<()> {
    match vfs.stat(&private_dir())? {
        Some(m) if m.kind == Kind::Dir => Ok(()),
        Some(_) => Err(Error::UnsupportedFileType(private_dir().to_path_buf())),
        None => {
            vfs.mkdir(&private_dir())?;
            vfs.sync_dir(&RelPath::root())?;
            Ok(())
        }
    }
}

impl Transaction {
    /// Starts a transaction on the directory `root`: takes the root's lock, probes the
    /// filesystem, and recovers any interrupted transaction first.
    pub fn begin(root: impl AsRef<Path>) -> Result<Transaction> {
        Self::begin_with(root, &Options::default())
    }

    pub fn begin_with(root: impl AsRef<Path>, opts: &Options) -> Result<Transaction> {
        #[cfg(target_os = "linux")]
        {
            let fs = crate::sys::linux::LinuxFs::open(root.as_ref())?;
            Self::begin_on(Box::new(fs), opts)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (root, opts);
            Err(Error::UnsupportedPlatform)
        }
    }

    /// Starts a transaction on a custom [`Vfs`] (the simulator, or another backend).
    pub fn begin_on(vfs: Box<dyn Vfs>, opts: &Options) -> Result<Transaction> {
        ensure_private_dir(&*vfs)?;
        let lock = vfs.lock(true)?;
        let caps = caps::probe(&*vfs, opts.allow_untested_fs)?;
        let recovered = recover::recover_locked(&*vfs)?;
        let tx = TxDir { name: tx_name() };
        vfs.mkdir(&tx.dir())?;
        vfs.mkdir(&tx.staged())?;
        vfs.mkdir(&tx.backup())?;
        let overlay = Overlay::new(vfs.root_id()?, caps.case_insensitive);
        Ok(Transaction {
            vfs,
            tx,
            overlay,
            blobs: BTreeMap::new(),
            next_blob: 0,
            caps,
            recovered,
            finished: false,
            _lock: lock,
        })
    }

    /// What `begin` recovered from earlier, interrupted transactions.
    pub fn recovered(&self) -> &RecoveryReport {
        &self.recovered
    }

    /// Whether the probe found a case-insensitive filesystem.
    pub fn case_insensitive(&self) -> bool {
        self.caps.case_insensitive
    }

    fn blob_path(&self, n: u32) -> RelPath {
        self.tx.staged().join(format!("b{n}"))
    }

    /// Replaces or creates the file at `path` (atomic-save semantics: a new inode).
    ///
    /// A replaced file keeps its permission bits; a new file gets the umask default.
    pub fn write(&mut self, path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> Result<()> {
        self.write_impl(path.as_ref(), data.as_ref(), None)
    }

    /// Like [`write`](Self::write), but sets the file's permission bits exactly
    /// (e.g. `0o755` for a script). Only the lower 12 bits (`0o7777`) are used.
    pub fn write_with_mode(
        &mut self,
        path: impl AsRef<Path>,
        data: impl AsRef<[u8]>,
        mode: u32,
    ) -> Result<()> {
        self.write_impl(path.as_ref(), data.as_ref(), Some(mode & 0o7777))
    }

    fn write_impl(&mut self, path: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
        let p = validate_user_path(path)?;
        let inherited = self.overlay.check_write(&*self.vfs, &p)?;
        let mode = mode.or(inherited);
        let n = self.next_blob;
        self.next_blob += 1;
        let meta = self.vfs.create_file(&self.blob_path(n), data, mode)?;
        self.blobs.insert(n, meta.id);
        self.overlay.apply_write(&*self.vfs, &p, n, mode)
    }

    /// Creates `path` and any missing parents as directories.
    pub fn create_dir_all(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let p = validate_user_path(path.as_ref())?;
        self.overlay.create_dir_all(&*self.vfs, &p)
    }

    /// Moves a file or directory. Fails if `to` exists. Moving a directory is O(1).
    pub fn rename(&mut self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
        let from = validate_user_path(from.as_ref())?;
        let to = validate_user_path(to.as_ref())?;
        self.overlay.rename(&*self.vfs, &from, &to)
    }

    /// Removes a file or an empty directory.
    pub fn remove(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let p = validate_user_path(path.as_ref())?;
        self.overlay.remove(&*self.vfs, &p)
    }

    /// Removes a directory and everything in it.
    pub fn remove_dir_all(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let p = validate_user_path(path.as_ref())?;
        self.overlay.remove_dir_all(&*self.vfs, &p)
    }

    /// Reads a file as the transaction currently sees it (read-your-writes).
    pub fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>> {
        let p = validate_user_path(path.as_ref())?;
        self.overlay.read(&*self.vfs, &p, |n| self.blob_path(n))
    }

    /// Whether something exists at `path` in the transaction's view.
    pub fn exists(&self, path: impl AsRef<Path>) -> Result<bool> {
        let p = validate_user_path(path.as_ref())?;
        Ok(self.overlay.resolve(&*self.vfs, &p)?.is_some())
    }

    /// Applies every staged change atomically. Returns `Ok` only after the commit is durable.
    ///
    /// On error the tree is back in its before-state, except for
    /// [`Error::CommitOutcomeUnknown`] and [`Error::RollbackFailed`], which leave the
    /// decision to the next [`crate::recover`].
    pub fn commit(mut self) -> Result<()> {
        self.finished = true;
        let vfs = &*self.vfs;
        let tx = self.tx.clone();
        let plan = match plan::compile(vfs, &self.overlay, &tx, &self.blobs) {
            Ok(p) => p,
            Err(e) => {
                let _ = tx.gc(vfs);
                return Err(e);
            }
        };
        if plan.waves.is_empty() {
            tx.gc(vfs)?;
            return Ok(());
        }
        let j = Journal {
            format: FORMAT,
            tx: tx.name.clone(),
            root: vfs.root_id()?,
            plan,
        };
        if let Err(e) = prepare(vfs, &tx, &j) {
            // The tree has not been touched yet.
            let _ = tx.gc(vfs);
            return Err(e.into());
        }
        vfs.event("prepared");
        if let Err(cause) = apply(vfs, &tx, &j.plan) {
            return match recover::rollback(vfs, &tx, &j) {
                Ok(()) => Err(cause.into()),
                Err(rb) => Err(Error::RollbackFailed {
                    cause: Box::new(cause.into()),
                    rollback: Box::new(rb),
                }),
            };
        }
        if let Err(source) = tx.set_marker(vfs, Marker::Committed) {
            return Err(Error::CommitOutcomeUnknown {
                tx: tx.name.clone(),
                source,
            });
        }
        vfs.event("committed");
        // Best effort: leftovers are settled by the next recovery otherwise.
        let _ = tx.gc(vfs);
        Ok(())
    }
}

/// Makes every token and the journal durable before the first tree mutation.
fn prepare(vfs: &dyn Vfs, tx: &TxDir, j: &Journal) -> io::Result<()> {
    for tok in &j.plan.tokens {
        if tok.kind == Kind::File && tok.locs[0].path.is_private() {
            vfs.sync_file(&tok.locs[0].path)?;
        }
    }
    vfs.sync_dir(&tx.staged())?;
    vfs.sync_dir(&tx.backup())?;
    vfs.create_file(&tx.progress(), b"", None)?;
    vfs.sync_file(&tx.progress())?;
    // Everything the journal refers to (the tx dir itself, staged/, backup/, progress)
    // must be durable before the journal can be: C2 lets unsynced entries of one
    // directory persist independently of each other.
    vfs.sync_dir(&tx.dir())?;
    vfs.sync_dir(&private_dir())?;
    vfs.create_file(&tx.journal_tmp(), &journal::encode(j), None)?;
    vfs.sync_file(&tx.journal_tmp())?;
    vfs.rename_noreplace(&tx.journal_tmp(), None, &tx.journal(), None)?;
    vfs.sync_dir(&tx.dir())
}

/// Runs the waves forward with a barrier after each.
fn apply(vfs: &dyn Vfs, tx: &TxDir, plan: &Plan) -> io::Result<()> {
    for (w, wave) in plan.waves.iter().enumerate() {
        for s in wave {
            let (src, dst) = (plan.src(*s), plan.dst(*s));
            vfs.rename_noreplace(&src.path, Some(src.parent), &dst.path, Some(dst.parent))?;
        }
        barrier(vfs, plan, wave)?;
        append_progress(vfs, tx, Record::ApplyDone(w as u32))?;
    }
    Ok(())
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if !self.finished {
            // Never prepared, so the tree was never touched.
            let _ = self.tx.gc(&*self.vfs);
        }
    }
}
