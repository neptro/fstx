//! The change-set format and how it is applied inside one fstx transaction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use fstx::Transaction;

/// A list of changes applied all-or-nothing. See the README for the format.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeSet {
    /// Preconditions on the tree *before* any op runs, keyed by path.
    #[serde(default)]
    pub expect: BTreeMap<String, Expect>,
    pub ops: Vec<Op>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The path must (or must not) exist.
    pub exists: Option<bool>,
    /// The file's SHA-256, as lowercase or uppercase hex.
    pub sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    /// Create or replace a file. Exactly one of `content`, `content_base64`, `from_file`.
    Write {
        path: String,
        content: Option<String>,
        content_base64: Option<String>,
        /// A file to copy the content from, relative to the change set's directory.
        from_file: Option<String>,
        /// Octal permission bits, e.g. "755". Default: keep the replaced file's, or umask.
        mode: Option<String>,
    },
    /// Replace text in a UTF-8 file. `find` must occur exactly `count` times (default 1).
    Replace {
        path: String,
        find: String,
        replace: String,
        #[serde(default = "one")]
        count: usize,
    },
    Mkdir {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    /// Remove a file or an empty directory.
    Remove {
        path: String,
    },
    /// Remove a directory and everything in it.
    RemoveAll {
        path: String,
    },
}

fn one() -> usize {
    1
}

/// One applied (or, in a dry run, staged) change, for reporting.
#[derive(Debug, Serialize)]
pub struct Done {
    pub op: &'static str,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailKind {
    /// The input is malformed. Exit code 2.
    InvalidInput,
    /// The change set is well-formed but cannot be applied. Exit code 1.
    Refused,
    /// An I/O or recovery problem. Exit code 3.
    Io,
}

impl FailKind {
    pub fn exit_code(self) -> i32 {
        match self {
            FailKind::Refused => 1,
            FailKind::InvalidInput => 2,
            FailKind::Io => 3,
        }
    }
}

#[derive(Debug)]
pub struct Failure {
    pub kind: FailKind,
    /// Index of the op that failed, if any.
    pub op: Option<usize>,
    pub message: String,
}

impl Failure {
    pub fn new(kind: FailKind, op: Option<usize>, message: impl Into<String>) -> Failure {
        Failure {
            kind,
            op,
            message: message.into(),
        }
    }

    pub fn from_fstx(op: Option<usize>, e: fstx::Error) -> Failure {
        let kind = match e {
            fstx::Error::Io(_)
            | fstx::Error::RecoveryRequired { .. }
            | fstx::Error::CommitOutcomeUnknown { .. }
            | fstx::Error::RollbackFailed { .. }
            | fstx::Error::UnsupportedFilesystem { .. }
            | fstx::Error::UnsupportedPlatform => FailKind::Io,
            _ => FailKind::Refused,
        };
        Failure::new(kind, op, e.to_string())
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn parse_mode(s: &str) -> Option<u32> {
    let digits = s.strip_prefix("0o").unwrap_or(s);
    u32::from_str_radix(digits, 8).ok().filter(|m| *m <= 0o7777)
}

/// Checks every precondition against the tree as the transaction sees it (before any op).
pub fn check_expectations(
    tx: &Transaction,
    expect: &BTreeMap<String, Expect>,
) -> Result<(), Failure> {
    for (path, e) in expect {
        let refused =
            |msg: String| Failure::new(FailKind::Refused, None, format!("expect {path}: {msg}"));
        let exists = tx.exists(path).map_err(|e| Failure::from_fstx(None, e))?;
        if let Some(want) = e.exists
            && want != exists
        {
            return Err(refused(if want {
                "does not exist".into()
            } else {
                "exists".into()
            }));
        }
        if let Some(want) = &e.sha256 {
            if !exists {
                return Err(refused("does not exist".into()));
            }
            let got = sha256_hex(&tx.read(path).map_err(|e| Failure::from_fstx(None, e))?);
            if !got.eq_ignore_ascii_case(want) {
                return Err(refused(format!("sha256 is {got}, expected {want}")));
            }
        }
    }
    Ok(())
}

/// Stages every op in `cs` into `tx`. `base` resolves relative `from_file` paths.
pub fn stage(tx: &mut Transaction, cs: &ChangeSet, base: &Path) -> Result<Vec<Done>, Failure> {
    check_expectations(tx, &cs.expect)?;
    let mut done = Vec::with_capacity(cs.ops.len());
    for (i, op) in cs.ops.iter().enumerate() {
        let fx = |e| Failure::from_fstx(Some(i), e);
        let input = |msg: &str| Failure::new(FailKind::InvalidInput, Some(i), msg.to_string());
        match op {
            Op::Write {
                path,
                content,
                content_base64,
                from_file,
                mode,
            } => {
                let data = match (content, content_base64, from_file) {
                    (Some(c), None, None) => c.clone().into_bytes(),
                    (None, Some(b), None) => base64::engine::general_purpose::STANDARD
                        .decode(b)
                        .map_err(|e| input(&format!("content_base64: {e}")))?,
                    (None, None, Some(f)) => {
                        let src: PathBuf = base.join(f);
                        std::fs::read(&src).map_err(|e| {
                            Failure::new(
                                FailKind::Refused,
                                Some(i),
                                format!("from_file {}: {e}", src.display()),
                            )
                        })?
                    }
                    _ => {
                        return Err(input(
                            "write needs exactly one of content, content_base64, from_file",
                        ));
                    }
                };
                match mode {
                    Some(m) => {
                        let m = parse_mode(m)
                            .ok_or_else(|| input("mode must be octal, e.g. \"644\""))?;
                        tx.write_with_mode(path, data, m).map_err(fx)?;
                    }
                    None => tx.write(path, data).map_err(fx)?,
                }
                done.push(Done {
                    op: "write",
                    path: path.clone(),
                    to: None,
                });
            }
            Op::Replace {
                path,
                find,
                replace,
                count,
            } => {
                if find.is_empty() {
                    return Err(input("replace: find must not be empty"));
                }
                let bytes = tx.read(path).map_err(fx)?;
                let text = String::from_utf8(bytes).map_err(|_| {
                    Failure::new(
                        FailKind::Refused,
                        Some(i),
                        format!("{path} is not UTF-8 text"),
                    )
                })?;
                let found = text.matches(find.as_str()).count();
                if found != *count {
                    return Err(Failure::new(
                        FailKind::Refused,
                        Some(i),
                        format!(
                            "replace in {path}: found {found} occurrence(s) of the text, expected {count}"
                        ),
                    ));
                }
                tx.write(path, text.replace(find.as_str(), replace))
                    .map_err(fx)?;
                done.push(Done {
                    op: "replace",
                    path: path.clone(),
                    to: None,
                });
            }
            Op::Mkdir { path } => {
                tx.create_dir_all(path).map_err(fx)?;
                done.push(Done {
                    op: "mkdir",
                    path: path.clone(),
                    to: None,
                });
            }
            Op::Rename { from, to } => {
                tx.rename(from, to).map_err(fx)?;
                done.push(Done {
                    op: "rename",
                    path: from.clone(),
                    to: Some(to.clone()),
                });
            }
            Op::Remove { path } => {
                tx.remove(path).map_err(fx)?;
                done.push(Done {
                    op: "remove",
                    path: path.clone(),
                    to: None,
                });
            }
            Op::RemoveAll { path } => {
                tx.remove_dir_all(path).map_err(fx)?;
                done.push(Done {
                    op: "remove_all",
                    path: path.clone(),
                    to: None,
                });
            }
        }
    }
    Ok(done)
}

/// Permission bits of a source file (Unix only).
#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode())
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Stages a copy of every file and directory under `src` (dotfiles-style sync).
/// Symlinks in `src` are skipped and reported; names in `exclude` are skipped at any depth.
pub fn stage_sync(
    tx: &mut Transaction,
    src: &Path,
    exclude: &[String],
) -> Result<(Vec<Done>, Vec<String>), Failure> {
    let mut done = Vec::new();
    let mut skipped = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = src.join(&rel);
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .map_err(|e| Failure::new(FailKind::Refused, None, format!("{}: {e}", dir.display())))?
            .collect::<Result<_, _>>()
            .map_err(|e| Failure::new(FailKind::Io, None, format!("{}: {e}", dir.display())))?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            if exclude.iter().any(|x| x.as_str() == name) {
                continue;
            }
            let child = rel.join(&name);
            let shown = child.to_string_lossy().into_owned();
            let meta = entry
                .path()
                .symlink_metadata()
                .map_err(|e| Failure::new(FailKind::Io, None, format!("{shown}: {e}")))?;
            let fx = |e| Failure::from_fstx(None, e);
            if meta.is_dir() {
                tx.create_dir_all(&child).map_err(fx)?;
                done.push(Done {
                    op: "mkdir",
                    path: shown,
                    to: None,
                });
                stack.push(child);
            } else if meta.is_file() {
                let data = std::fs::read(entry.path())
                    .map_err(|e| Failure::new(FailKind::Io, None, format!("{shown}: {e}")))?;
                match file_mode(&meta) {
                    Some(mode) => tx.write_with_mode(&child, data, mode),
                    None => tx.write(&child, data),
                }
                .map_err(fx)?;
                done.push(Done {
                    op: "write",
                    path: shown,
                    to: None,
                });
            } else {
                skipped.push(shown);
            }
        }
    }
    Ok((done, skipped))
}
