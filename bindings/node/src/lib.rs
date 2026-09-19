//! Node.js bindings for fstx. Errors carry a stable code (see `code_of`).
//! `index.js` wraps this module and adds `transaction()` and `commitAsync()` rejections.

use napi::bindgen_prelude::{AsyncTask, Buffer, Either, Env, Task, Uint8Array};
use napi_derive::napi;

fn code_of(e: &fstx::Error) -> &'static str {
    use fstx::Error as E;
    match e {
        E::InvalidPath { .. } => "INVALID_PATH",
        E::NotFound(_) => "NOT_FOUND",
        E::AlreadyExists(_) => "ALREADY_EXISTS",
        E::NotADirectory(_) => "NOT_A_DIRECTORY",
        E::IsADirectory(_) => "IS_A_DIRECTORY",
        E::DirectoryNotEmpty(_) => "DIRECTORY_NOT_EMPTY",
        E::UnsupportedFileType(_) => "UNSUPPORTED_FILE_TYPE",
        E::InvalidMove { .. } => "INVALID_MOVE",
        E::CaseCollision { .. } => "CASE_COLLISION",
        E::Conflict(_) => "CONFLICT",
        E::UnsupportedFilesystem { .. } => "UNSUPPORTED_FILESYSTEM",
        E::UnsupportedPlatform => "UNSUPPORTED_PLATFORM",
        E::RecoveryRequired { .. } => "RECOVERY_REQUIRED",
        E::CommitOutcomeUnknown { .. } => "COMMIT_OUTCOME_UNKNOWN",
        E::RollbackFailed { .. } => "ROLLBACK_FAILED",
        _ => "IO",
    }
}

fn js_err(e: fstx::Error) -> napi::Error {
    napi::Error::from_reason(format!("[{}] {e}", code_of(&e)))
}

fn finished() -> napi::Error {
    napi::Error::from_reason(
        "[TRANSACTION_FINISHED] this transaction was already committed or discarded",
    )
}

#[napi(object)]
#[derive(Default)]
pub struct BeginOptions {
    /// Allow network/FUSE filesystems (outside the tested crash contract).
    pub allow_untested_fs: Option<bool>,
}

#[napi(object)]
pub struct WriteOptions {
    /// Exact permission bits, e.g. 0o755.
    pub mode: Option<u32>,
}

fn opts(o: Option<BeginOptions>) -> fstx::Options {
    fstx::Options::new().allow_untested_fs(o.unwrap_or_default().allow_untested_fs.unwrap_or(false))
}

/// A set of changes to a directory tree that becomes visible all at once, or not at all.
#[napi]
pub struct Transaction {
    inner: Option<fstx::Transaction>,
}

impl Transaction {
    fn tx(&mut self) -> napi::Result<&mut fstx::Transaction> {
        self.inner.as_mut().ok_or_else(finished)
    }
    fn tx_ref(&self) -> napi::Result<&fstx::Transaction> {
        self.inner.as_ref().ok_or_else(finished)
    }
}

#[napi]
impl Transaction {
    /// Starts a transaction on `root`; recovers interrupted transactions first.
    #[napi(factory, js_name = "_begin")]
    pub fn begin(root: String, options: Option<BeginOptions>) -> napi::Result<Transaction> {
        let tx = fstx::Transaction::begin_with(root, &opts(options)).map_err(js_err)?;
        Ok(Transaction { inner: Some(tx) })
    }

    /// Creates or replaces a file.
    #[napi]
    pub fn write(
        &mut self,
        path: String,
        data: Either<String, Uint8Array>,
        options: Option<WriteOptions>,
    ) -> napi::Result<()> {
        let bytes: &[u8] = match &data {
            Either::A(s) => s.as_bytes(),
            Either::B(b) => b,
        };
        let tx = self.tx()?;
        match options.and_then(|o| o.mode) {
            Some(mode) => tx.write_with_mode(path, bytes, mode),
            None => tx.write(path, bytes),
        }
        .map_err(js_err)
    }

    /// Reads a file as this transaction sees it (read-your-writes).
    #[napi]
    pub fn read(&self, path: String) -> napi::Result<Buffer> {
        Ok(self.tx_ref()?.read(path).map_err(js_err)?.into())
    }

    #[napi]
    pub fn exists(&self, path: String) -> napi::Result<bool> {
        self.tx_ref()?.exists(path).map_err(js_err)
    }

    #[napi]
    pub fn create_dir_all(&mut self, path: String) -> napi::Result<()> {
        self.tx()?.create_dir_all(path).map_err(js_err)
    }

    #[napi]
    pub fn rename(&mut self, from: String, to: String) -> napi::Result<()> {
        self.tx()?.rename(from, to).map_err(js_err)
    }

    /// Removes a file or an empty directory.
    #[napi]
    pub fn remove(&mut self, path: String) -> napi::Result<()> {
        self.tx()?.remove(path).map_err(js_err)
    }

    #[napi]
    pub fn remove_dir_all(&mut self, path: String) -> napi::Result<()> {
        self.tx()?.remove_dir_all(path).map_err(js_err)
    }

    /// Applies every change atomically; returns once the commit is durable.
    #[napi]
    pub fn commit(&mut self) -> napi::Result<()> {
        self.inner
            .take()
            .ok_or_else(finished)?
            .commit()
            .map_err(js_err)
    }

    /// Discards the transaction now and releases the lock (also happens on garbage collection).
    #[napi]
    pub fn discard(&mut self) {
        self.inner = None;
    }

    /// Whether the transaction is still open.
    #[napi(getter)]
    pub fn finished(&self) -> bool {
        self.inner.is_none()
    }

    /// Commits on a worker thread. Resolves to an outcome that `index.js` turns into a
    /// resolved or rejected promise with the same error codes as `commit()`.
    #[napi(js_name = "_commitInWorker")]
    pub fn commit_in_worker(&mut self) -> AsyncTask<CommitTask> {
        AsyncTask::new(CommitTask {
            tx: self.inner.take(),
        })
    }
}

#[napi(object)]
pub struct CommitOutcome {
    pub ok: bool,
    pub code: Option<String>,
    pub message: Option<String>,
}

pub struct CommitTask {
    tx: Option<fstx::Transaction>,
}

impl Task for CommitTask {
    type Output = CommitOutcome;
    type JsValue = CommitOutcome;

    fn compute(&mut self) -> napi::Result<CommitOutcome> {
        let fail = |code: &str, message: String| CommitOutcome {
            ok: false,
            code: Some(code.into()),
            message: Some(message),
        };
        Ok(match self.tx.take() {
            None => fail(
                "TRANSACTION_FINISHED",
                "this transaction was already committed or discarded".into(),
            ),
            Some(tx) => match tx.commit() {
                Ok(()) => CommitOutcome {
                    ok: true,
                    code: None,
                    message: None,
                },
                Err(e) => fail(code_of(&e), e.to_string()),
            },
        })
    }

    fn resolve(&mut self, _env: Env, output: CommitOutcome) -> napi::Result<CommitOutcome> {
        Ok(output)
    }
}

#[napi(object)]
pub struct RecoveryReport {
    pub rolled_back: Vec<String>,
    pub completed: Vec<String>,
    pub discarded: Vec<String>,
    pub garbage_removed: u32,
}

/// Recovers every interrupted transaction under `root` (`Transaction.begin` does this too).
#[napi]
pub fn recover(root: String, options: Option<BeginOptions>) -> napi::Result<RecoveryReport> {
    let r = fstx::recover_with(root, &opts(options)).map_err(js_err)?;
    Ok(RecoveryReport {
        rolled_back: r.rolled_back,
        completed: r.completed,
        discarded: r.discarded,
        garbage_removed: r.garbage_removed as u32,
    })
}

#[napi(object)]
pub struct TxInspection {
    pub name: String,
    /// "Discard" | "CleanUpCommitted" | "CleanUpRolledBack" | "RollBack" | "RecoveryRequired"
    pub action: String,
    pub reason: Option<String>,
    /// Number of entries the transaction moves.
    pub entries: u32,
}

#[napi(object)]
pub struct Inspection {
    pub transactions: Vec<TxInspection>,
    pub garbage: Vec<String>,
}

/// Read-only report of interrupted transactions and what recovery would do.
#[napi]
pub fn inspect(root: String) -> napi::Result<Inspection> {
    let i = fstx::inspect(root).map_err(js_err)?;
    Ok(Inspection {
        transactions: i
            .transactions
            .into_iter()
            .map(|t| TxInspection {
                name: t.name,
                action: format!("{:?}", t.action),
                reason: t.reason,
                entries: t.tokens.len() as u32,
            })
            .collect(),
        garbage: i.garbage,
    })
}
