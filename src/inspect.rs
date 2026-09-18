//! Read-only diagnostics: what recovery would do, and where every token is.
//! `inspect` only ever calls `stat`, `probe`, `read_dir` and `read_file` (plus a shared
//! lock on real filesystems); it never mutates.

use std::path::PathBuf;

use crate::error::Result;
use crate::recover::{Decision, Pos, classify, decide};
use crate::state::{GC_PREFIX, PROBE_PREFIX, TX_PREFIX, TxDir, private_dir};
use crate::vfs::{FileId, Kind, Vfs};

/// State of the root's private area.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Inspection {
    pub transactions: Vec<TxInspection>,
    /// Scratch directories (`gc-*`, `probe-*`) that recovery will delete.
    pub garbage: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TxInspection {
    pub name: String,
    pub action: RecoveryAction,
    /// Why recovery cannot proceed, for `RecoveryAction::RecoveryRequired`.
    pub reason: Option<String>,
    pub tokens: Vec<TokenInspection>,
}

/// What [`crate::recover`] would do with a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Never prepared; the tree was not touched. Delete it.
    Discard,
    /// Committed; delete leftovers.
    CleanUpCommitted,
    /// Already rolled back; delete leftovers.
    CleanUpRolledBack,
    /// Prepared, not committed: roll back.
    RollBack,
    /// Recovery will not touch anything.
    RecoveryRequired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TokenInspection {
    pub kind: Kind,
    pub id: FileId,
    /// Paths relative to the root, in apply order.
    pub locations: Vec<PathBuf>,
    /// Index into `locations`, or the anomaly found.
    pub position: std::result::Result<TokenPosition, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenPosition {
    pub index: usize,
    /// The entry is present at both `index` and `index + 1` (Weak rename).
    pub both_names: bool,
}

pub(crate) fn inspect_on(vfs: &dyn Vfs) -> Result<Inspection> {
    let mut out = Inspection::default();
    if vfs.stat(&private_dir())?.is_none() {
        return Ok(out);
    }
    let _lock = vfs.lock(false)?;
    let mut names: Vec<String> = vfs
        .read_dir(&private_dir())?
        .into_iter()
        .filter_map(|n| n.into_string().ok())
        .collect();
    names.sort();
    for name in names {
        if name.starts_with(GC_PREFIX) || name.starts_with(PROBE_PREFIX) {
            out.garbage.push(name);
            continue;
        }
        if !name.starts_with(TX_PREFIX) {
            continue;
        }
        let tx = TxDir { name: name.clone() };
        let (action, reason, tokens) = match decide(vfs, &tx)? {
            Decision::Discard => (RecoveryAction::Discard, None, Vec::new()),
            Decision::Committed => (RecoveryAction::CleanUpCommitted, None, Vec::new()),
            Decision::RolledBack => (RecoveryAction::CleanUpRolledBack, None, Vec::new()),
            Decision::Required(r) => (RecoveryAction::RecoveryRequired, Some(r), Vec::new()),
            Decision::RollBack(j) => {
                let classes = classify(vfs, &j.plan)?;
                let mut reason = None;
                let tokens = j
                    .plan
                    .tokens
                    .iter()
                    .zip(classes)
                    .map(|(t, c)| TokenInspection {
                        kind: t.kind,
                        id: t.id,
                        locations: t.locs.iter().map(|l| l.path.to_path_buf()).collect(),
                        position: c
                            .map(|p| match p {
                                Pos::At(i) => TokenPosition {
                                    index: i,
                                    both_names: false,
                                },
                                Pos::Both(i) => TokenPosition {
                                    index: i,
                                    both_names: true,
                                },
                            })
                            .inspect_err(|e| {
                                reason.get_or_insert_with(|| e.clone());
                            }),
                    })
                    .collect();
                let action = if reason.is_some() {
                    RecoveryAction::RecoveryRequired
                } else {
                    RecoveryAction::RollBack
                };
                (action, reason, tokens)
            }
        };
        out.transactions.push(TxInspection {
            name,
            action,
            reason,
            tokens,
        });
    }
    Ok(out)
}
