//! Recovery (DESIGN.md §4): decide, classify, stabilize, undo.
//!
//! * **Classification** locates each token by identity: a location counts only if its
//!   parent directory has the journaled identity *and* the entry there has the token's
//!   `FileId`. Mere existence is never used.
//! * **Shape check** (Lemma 1): there must be a wave `w` with all earlier steps done and
//!   all later steps not done.
//! * **Stabilize** fsyncs every directory involved before the first mutation, so the
//!   observed state is the durable state; from then on only backward moves happen, and
//!   the potential Φ can only decrease (Lemma 3).
//! * **Undo** runs the waves in reverse with the same barrier discipline as apply.

use std::collections::BTreeSet;
use std::io;

use crate::error::{Error, Result};
use crate::journal::{self, Journal};
use crate::path::RelPath;
use crate::plan::{Plan, Step};
use crate::progress::{self, Record};
use crate::state::{GC_PREFIX, Marker, PROBE_PREFIX, TX_PREFIX, TxDir, delete_tree, private_dir};
use crate::vfs::{FileId, Probe, Vfs};

/// Where a token is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pos {
    /// At `locs[i]`.
    At(usize),
    /// At both `locs[i]` and `locs[i + 1]` (a rename persisted with both names; Weak only).
    Both(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StepState {
    Pre,
    Post,
    Inflight,
}

fn step_state(pos: Pos, s: Step) -> StepState {
    match pos {
        Pos::At(i) if i > s.from => StepState::Post,
        Pos::At(_) => StepState::Pre,
        Pos::Both(i) if i == s.from => StepState::Inflight,
        Pos::Both(i) if i > s.from => StepState::Post,
        Pos::Both(_) => StepState::Pre,
    }
}

/// Locates every token. Returns per-token results; `Err` = anomaly description.
pub(crate) fn classify(vfs: &dyn Vfs, plan: &Plan) -> io::Result<Vec<Result<Pos, String>>> {
    let known: BTreeSet<FileId> = plan.tokens.iter().map(|t| t.id).collect();
    let mut out = Vec::with_capacity(plan.tokens.len());
    for tok in &plan.tokens {
        let mut hits = Vec::new();
        let mut foreign = None;
        for (i, loc) in tok.locs.iter().enumerate() {
            match vfs.probe(&loc.path, loc.parent)? {
                Probe::ParentMismatch | Probe::Missing => {}
                Probe::Present(m) if m.id == tok.id && m.kind == tok.kind => hits.push(i),
                Probe::Present(m) if known.contains(&m.id) => {}
                Probe::Present(_) => foreign = Some(loc.path.clone()),
            }
        }
        out.push(match (hits.as_slice(), foreign) {
            (_, Some(p)) => Err(format!("unknown entry at {p:?}")),
            ([i], None) => Ok(Pos::At(*i)),
            ([i, j], None) if *j == i + 1 && tok.kind == crate::vfs::Kind::File => Ok(Pos::Both(*i)),
            ([], None) => Err(format!("token {:?} not found at any of its locations", tok.id)),
            (_, None) => Err(format!("token {:?} found at several locations {hits:?}", tok.id)),
        });
    }
    Ok(out)
}

/// Lemma 1 shape: at most one mixed wave, all earlier waves done, all later waves not done.
fn check_shape(plan: &Plan, pos: &[Pos]) -> Result<(), String> {
    let mut first_not_post = None;
    let mut last_not_pre = None;
    for (w, wave) in plan.waves.iter().enumerate() {
        for s in wave {
            let st = step_state(pos[s.token], *s);
            if st != StepState::Post && first_not_post.is_none() {
                first_not_post = Some(w);
            }
            if st != StepState::Pre {
                last_not_pre = Some(w);
            }
        }
    }
    match (first_not_post, last_not_pre) {
        (Some(a), Some(b)) if b > a => Err(format!("waves {a} and {b} both partially applied")),
        _ => Ok(()),
    }
}

/// The progress log is a lower bound: logged-done work must be observed as done.
fn check_progress(plan: &Plan, recs: &[Record], pos: &[Pos]) -> Result<(), String> {
    let n = plan.waves.len();
    let apply_done = recs.iter().filter_map(|r| match r { Record::ApplyDone(w) => Some(*w as usize), _ => None }).max();
    let undo_min = recs.iter().filter_map(|r| match r { Record::UndoDone(w) => Some(*w as usize), _ => None }).min();
    if apply_done.is_some_and(|w| w >= n) || undo_min.is_some_and(|w| w >= n) {
        return Err("progress names a wave that does not exist".into());
    }
    let undo_from = undo_min.unwrap_or(n);
    for (w, wave) in plan.waves.iter().enumerate() {
        for s in wave {
            let st = step_state(pos[s.token], *s);
            if w >= undo_from && st != StepState::Pre {
                return Err(format!("wave {w} logged as undone but not observed undone"));
            }
            if apply_done.is_some_and(|a| w <= a) && w + 1 < undo_from && st != StepState::Post {
                return Err(format!("wave {w} logged as applied but not observed applied"));
            }
        }
    }
    Ok(())
}

/// fsyncs the parent directory of `loc` if it resolves to the journaled identity.
/// An unresolvable parent means an ancestor token is elsewhere, which by wave ordering
/// implies this location's changes are already durable or never happened (DESIGN.md §4).
fn sync_parent(vfs: &dyn Vfs, path: &RelPath, expect: FileId, done: &mut BTreeSet<RelPath>) -> io::Result<()> {
    let parent = path.parent().unwrap_or_default();
    if done.contains(&parent) {
        return Ok(());
    }
    if let Some(m) = vfs.stat(&parent)? {
        if m.id == expect {
            vfs.sync_dir(&parent)?;
            done.insert(parent);
        }
    }
    Ok(())
}

/// Barrier for one wave: fsync every parent of every endpoint.
pub(crate) fn barrier(vfs: &dyn Vfs, plan: &Plan, wave: &[Step]) -> io::Result<()> {
    let mut done = BTreeSet::new();
    for s in wave {
        for loc in [plan.src(*s), plan.dst(*s)] {
            sync_parent(vfs, &loc.path, loc.parent, &mut done)?;
        }
    }
    Ok(())
}

pub(crate) fn append_progress(vfs: &dyn Vfs, tx: &TxDir, r: Record) -> io::Result<()> {
    vfs.append(&tx.progress(), &progress::encode(r))?;
    vfs.sync_file(&tx.progress())
}

fn read_progress(vfs: &dyn Vfs, tx: &TxDir) -> io::Result<Vec<Record>> {
    match vfs.stat(&tx.progress())? {
        Some(_) => Ok(progress::parse(&vfs.read_file(&tx.progress())?)),
        None => Ok(Vec::new()),
    }
}

fn required(tx: &TxDir, reason: impl Into<String>) -> Error {
    Error::RecoveryRequired { tx: tx.name.clone(), reason: reason.into() }
}

/// Classification + shape + progress checks. No mutation.
pub(crate) fn preflight(vfs: &dyn Vfs, tx: &TxDir, j: &Journal) -> Result<Vec<Pos>> {
    let mut pos = Vec::new();
    for r in classify(vfs, &j.plan)? {
        pos.push(r.map_err(|e| required(tx, e))?);
    }
    check_shape(&j.plan, &pos).map_err(|e| required(tx, e))?;
    check_progress(&j.plan, &read_progress(vfs, tx)?, &pos).map_err(|e| required(tx, e))?;
    Ok(pos)
}

/// Rolls a prepared-but-uncommitted transaction back to the before-state, then settles it.
pub(crate) fn rollback(vfs: &dyn Vfs, tx: &TxDir, j: &Journal) -> Result<()> {
    let mut pos = preflight(vfs, tx, j)?;
    let plan = &j.plan;

    // Stabilize: make the observed state durable before the first mutation.
    let mut done = BTreeSet::new();
    for tok in &plan.tokens {
        for loc in &tok.locs {
            sync_parent(vfs, &loc.path, loc.parent, &mut done)?;
        }
    }
    for d in [tx.staged(), tx.backup(), tx.dir(), private_dir()] {
        if !done.contains(&d) && vfs.stat(&d)?.is_some() {
            vfs.sync_dir(&d)?;
        }
    }
    vfs.event("stabilized");

    tx.set_marker(vfs, Marker::RollingBack)?;
    for (w, wave) in plan.waves.iter().enumerate().rev() {
        for s in wave {
            let (src, dst) = (plan.src(*s), plan.dst(*s));
            match pos[s.token] {
                Pos::At(i) if i == s.from + 1 => {
                    vfs.rename_noreplace(&dst.path, Some(dst.parent), &src.path, Some(src.parent))?;
                    pos[s.token] = Pos::At(s.from);
                }
                Pos::Both(i) if i == s.from => {
                    vfs.unlink(&dst.path, Some(dst.parent))?;
                    pos[s.token] = Pos::At(s.from);
                }
                Pos::At(i) if i <= s.from => {}
                Pos::Both(i) if i < s.from => {}
                other => return Err(required(tx, format!("token {} at {other:?} during undo of wave {w}", s.token))),
            }
        }
        barrier(vfs, plan, wave)?;
        append_progress(vfs, tx, Record::UndoDone(w as u32))?;
    }
    vfs.event("undone");
    tx.set_marker(vfs, Marker::RolledBack)?;
    tx.gc(vfs)?;
    Ok(())
}

/// What recovery will do with one transaction directory.
#[derive(Debug)]
pub(crate) enum Decision {
    /// Never prepared: the tree was never touched; delete.
    Discard,
    /// Committed: delete the leftovers only.
    Committed,
    /// Already rolled back: delete the leftovers only.
    RolledBack,
    /// Prepared but not committed: undo to the before-state.
    RollBack(Box<Journal>),
    /// Cannot interpret; touch nothing.
    Required(String),
}

pub(crate) fn decide(vfs: &dyn Vfs, tx: &TxDir) -> io::Result<Decision> {
    let committed = tx.has_marker(vfs, Marker::Committed)?;
    let rolling = tx.has_marker(vfs, Marker::RollingBack)?;
    let rolled = tx.has_marker(vfs, Marker::RolledBack)?;
    if committed && (rolling || rolled) {
        return Ok(Decision::Required("both COMMITTED and rollback markers present".into()));
    }
    if committed {
        return Ok(Decision::Committed);
    }
    if rolled {
        return Ok(Decision::RolledBack);
    }
    if vfs.stat(&tx.journal())?.is_none() {
        let backup_used = match vfs.stat(&tx.backup())? {
            Some(_) => !vfs.read_dir(&tx.backup())?.is_empty(),
            None => false,
        };
        return Ok(if backup_used || rolling {
            Decision::Required("no journal, but the transaction has progressed past preparation".into())
        } else {
            Decision::Discard
        });
    }
    let bytes = vfs.read_file(&tx.journal())?;
    Ok(match journal::decode(&bytes, &tx.name, vfs.root_id()?) {
        Ok(j) => Decision::RollBack(Box::new(j)),
        Err(e) => Decision::Required(e),
    })
}

/// What [`crate::recover`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveryReport {
    /// Prepared transactions that were rolled back to their before-state.
    pub rolled_back: Vec<String>,
    /// Committed transactions whose leftovers were removed.
    pub completed: Vec<String>,
    /// Transactions that never reached the journal and were discarded.
    pub discarded: Vec<String>,
    /// Leftover scratch directories removed.
    pub garbage_removed: usize,
}

/// Recovers every transaction under `.fstx`. The caller holds the exclusive lock.
pub(crate) fn recover_locked(vfs: &dyn Vfs) -> Result<RecoveryReport> {
    let mut report = RecoveryReport::default();
    let mut first_err = None;
    let mut names: Vec<String> = vfs
        .read_dir(&private_dir())?
        .into_iter()
        .filter_map(|n| n.into_string().ok())
        .collect();
    names.sort();
    for name in names {
        if name.starts_with(GC_PREFIX) || name.starts_with(PROBE_PREFIX) {
            // The rename to gc-* may be visible but not yet durable (the process died
            // before `gc` synced .fstx). Make it durable before deleting anything inside,
            // or a power loss could keep the deletions and revert the rename.
            vfs.sync_dir(&private_dir())?;
            delete_tree(vfs, &private_dir().join(&name))?;
            vfs.sync_dir(&private_dir())?;
            report.garbage_removed += 1;
            continue;
        }
        if !name.starts_with(TX_PREFIX) {
            continue;
        }
        let tx = TxDir { name: name.clone() };
        match decide(vfs, &tx)? {
            Decision::Discard => {
                tx.gc(vfs)?;
                report.discarded.push(name);
            }
            Decision::Committed => {
                tx.gc(vfs)?;
                report.completed.push(name);
            }
            Decision::RolledBack => {
                tx.gc(vfs)?;
                report.rolled_back.push(name);
            }
            Decision::RollBack(j) => match rollback(vfs, &tx, &j) {
                Ok(()) => report.rolled_back.push(name),
                Err(e @ Error::RecoveryRequired { .. }) => {
                    first_err.get_or_insert(e);
                }
                Err(e) => return Err(e),
            },
            Decision::Required(reason) => {
                first_err.get_or_insert(Error::RecoveryRequired { tx: name, reason });
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(report),
    }
}
