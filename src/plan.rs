//! Net diff → physical plan (DESIGN.md §2–3).
//!
//! Every physical mutation is a no-replace rename of one *token* (a directory entry with a
//! known `FileId`) between two locations. Base entries that must leave their base path are
//! **detached** into `backup/` (deepest first); tokens are then **attached** at their final
//! paths from `backup/` or `staged/` (shallowest first). Each depth forms one wave.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::overlay::{EKind, Entry, Overlay, Slot, Src};
use crate::path::RelPath;
use crate::state::TxDir;
use crate::vfs::{FileId, Kind, Vfs};

/// A location a token can occupy, with the identity its parent directory must have.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Loc {
    pub path: RelPath,
    pub parent: FileId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Token {
    pub kind: Kind,
    pub id: FileId,
    /// Locations in the order the token visits them during apply.
    pub locs: Vec<Loc>,
}

/// Moves `token` from `locs[from]` to `locs[from + 1]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Step {
    #[serde(rename = "t")]
    pub token: usize,
    pub from: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Plan {
    pub tokens: Vec<Token>,
    pub waves: Vec<Vec<Step>>,
}

impl Plan {
    pub fn src(&self, s: Step) -> &Loc {
        &self.tokens[s.token].locs[s.from]
    }
    pub fn dst(&self, s: Step) -> &Loc {
        &self.tokens[s.token].locs[s.from + 1]
    }
}

fn base_origin(e: &Entry) -> Option<&RelPath> {
    match &e.src {
        Src::Base { origin, .. } => Some(origin),
        _ => None,
    }
}

fn token_kind(k: EKind, what: &RelPath) -> Result<Kind> {
    match k {
        EKind::File => Ok(Kind::File),
        EKind::Dir => Ok(Kind::Dir),
        EKind::Special => Err(Error::UnsupportedFileType(what.to_path_buf())),
    }
}

/// Compiles the overlay into a plan. Creates the staged directories for new dirs (in the
/// private area only) and re-verifies every base entry it will move (`Error::Conflict`).
pub(crate) fn compile(
    vfs: &dyn Vfs,
    ov: &Overlay,
    tx: &TxDir,
    blob_ids: &BTreeMap<u32, FileId>,
) -> Result<Plan> {
    let placements: Vec<(RelPath, Entry)> = ov
        .placed
        .iter()
        .filter_map(|(p, s)| match s {
            Slot::Present(e) => Some((p.clone(), e.clone())),
            Slot::Absent => None,
        })
        .collect();

    // Identity no-ops: the same base entry is back at its base path under an unchanged chain.
    let mut noop = BTreeSet::new();
    for (p, e) in &placements {
        if base_origin(e) != Some(p) {
            continue;
        }
        let mut in_place = true;
        for q in p.proper_prefixes() {
            match ov.resolve(vfs, &q)? {
                Some(a) if base_origin(&a) == Some(&q) => {}
                _ => {
                    in_place = false;
                    break;
                }
            }
        }
        if in_place {
            noop.insert(p.clone());
        }
    }

    let attach: Vec<(RelPath, Entry)> = placements.into_iter().filter(|(p, _)| !noop.contains(p)).collect();
    let attached_origin: BTreeMap<RelPath, RelPath> = attach
        .iter()
        .filter_map(|(p, e)| base_origin(e).map(|o| (o.clone(), p.clone())))
        .collect();

    let mut detach: Vec<(RelPath, FileId, EKind)> = Vec::new();
    for (b, &(id, kind)) in &ov.detached {
        if noop.contains(b) {
            continue;
        }
        if !attached_origin.contains_key(b) {
            // Removed; skip if it will leave together with a removed ancestor.
            let covered = b.proper_prefixes().any(|a| {
                ov.detached.contains_key(&a) && !attached_origin.contains_key(&a) && !noop.contains(&a)
            });
            if covered {
                continue;
            }
        }
        match vfs.stat(b)? {
            Some(m) if m.id == id => {}
            _ => return Err(Error::Conflict(b.to_path_buf())),
        }
        detach.push((b.clone(), id, kind));
    }

    let root_id = vfs.root_id()?;
    let dir_id = |p: &RelPath| -> Result<FileId> {
        match vfs.stat(p)? {
            Some(m) if m.kind == Kind::Dir => Ok(m.id),
            _ => Err(Error::Conflict(p.to_path_buf())),
        }
    };
    let staged_id = dir_id(&tx.staged())?;
    let backup_id = dir_id(&tx.backup())?;

    // Stage new directories so every token's identity is known before the journal exists.
    let mut new_dir_ids: BTreeMap<u32, FileId> = BTreeMap::new();
    for (_, e) in &attach {
        if let Src::NewDir { n } = e.src {
            let m = vfs.mkdir(&tx.staged().join(format!("d{n}")))?;
            new_dir_ids.insert(n, m.id);
        }
    }

    let view_parent_id = |p: &RelPath| -> Result<FileId> {
        let q = p.parent().unwrap_or_default();
        if q.is_root() {
            return Ok(root_id);
        }
        match ov.resolve(vfs, &q)? {
            Some(Entry { kind: EKind::Dir, src: Src::Base { id, .. } }) => Ok(id),
            Some(Entry { kind: EKind::Dir, src: Src::NewDir { n } }) => Ok(new_dir_ids[&n]),
            _ => Err(Error::NotFound(q.to_path_buf())),
        }
    };

    let mut plan = Plan::default();
    let mut detach_steps: BTreeMap<usize, Vec<Step>> = BTreeMap::new();
    let mut attach_steps: BTreeMap<usize, Vec<Step>> = BTreeMap::new();

    for (k, (b, id, kind)) in detach.iter().enumerate() {
        let parent = b.parent().unwrap_or_default();
        let parent_id = if parent.is_root() { root_id } else { dir_id(&parent)? };
        let mut locs = vec![
            Loc { path: b.clone(), parent: parent_id },
            Loc { path: tx.backup().join(k.to_string()), parent: backup_id },
        ];
        let t = plan.tokens.len();
        if let Some(p) = attached_origin.get(b) {
            locs.push(Loc { path: p.clone(), parent: view_parent_id(p)? });
            attach_steps.entry(p.depth()).or_default().push(Step { token: t, from: 1 });
        }
        detach_steps.entry(b.depth()).or_default().push(Step { token: t, from: 0 });
        plan.tokens.push(Token { kind: token_kind(*kind, b)?, id: *id, locs });
    }

    for (p, e) in &attach {
        let (staged_name, id, kind) = match e.src {
            Src::Base { .. } => continue,
            Src::Blob { n, .. } => (format!("b{n}"), blob_ids[&n], Kind::File),
            Src::NewDir { n } => (format!("d{n}"), new_dir_ids[&n], Kind::Dir),
        };
        let t = plan.tokens.len();
        plan.tokens.push(Token {
            kind,
            id,
            locs: vec![
                Loc { path: tx.staged().join(staged_name), parent: staged_id },
                Loc { path: p.clone(), parent: view_parent_id(p)? },
            ],
        });
        attach_steps.entry(p.depth()).or_default().push(Step { token: t, from: 0 });
    }

    plan.waves.extend(detach_steps.into_values().rev());
    plan.waves.extend(attach_steps.into_values());
    debug_assert_eq!(validate(&plan), Ok(()));
    Ok(plan)
}

/// Checks the structural proof obligations of a plan (DESIGN.md §3):
///
/// 1. every token's steps are `from = 0..n-1`, each in a strictly later wave;
/// 2. **wave invariant**: within a wave, no endpoint of one step equals, contains or is
///    contained in an endpoint of another (shared *ancestors* are read-only and allowed);
/// 3. symbolic execution: a step's source holds its token, its destination is free of
///    tokens, and a parent that is a token directory is present there (placed by an
///    *earlier* wave) with the identity recorded in the location;
/// 4. every token ends at its last location, and final locations are distinct.
pub(crate) fn validate(plan: &Plan) -> Result<(), String> {
    let n = plan.tokens.len();
    let mut step_wave: Vec<BTreeMap<usize, usize>> = vec![BTreeMap::new(); n];
    for (w, wave) in plan.waves.iter().enumerate() {
        for s in wave {
            let tok = plan.tokens.get(s.token).ok_or("step names an unknown token")?;
            if s.from + 1 >= tok.locs.len() {
                return Err("step moves beyond the token's last location".into());
            }
            if step_wave[s.token].insert(s.from, w).is_some() {
                return Err("token moved twice from the same location".into());
            }
        }
        for (i, a) in wave.iter().enumerate() {
            for b in &wave[i + 1..] {
                for pa in [&plan.src(*a).path, &plan.dst(*a).path] {
                    for pb in [&plan.src(*b).path, &plan.dst(*b).path] {
                        if pa.overlaps(pb) {
                            return Err(format!("wave {w}: steps overlap at {pa:?} / {pb:?}"));
                        }
                    }
                }
            }
        }
    }
    for (t, tok) in plan.tokens.iter().enumerate() {
        let waves: Vec<usize> = (0..tok.locs.len() - 1)
            .map(|f| step_wave[t].get(&f).copied().ok_or_else(|| format!("token {t} misses step {f}")))
            .collect::<Result<_, _>>()?;
        if waves.windows(2).any(|w| w[0] >= w[1]) {
            return Err(format!("token {t}: steps not in strictly increasing waves"));
        }
    }

    // Symbolic execution over token positions.
    let mut pos = vec![0usize; n];
    let at = |pos: &Vec<usize>, path: &RelPath| -> Option<usize> {
        (0..n).find(|&t| plan.tokens[t].locs[pos[t]].path == *path)
    };
    for (w, wave) in plan.waves.iter().enumerate() {
        let snapshot = pos.clone();
        for s in wave {
            if snapshot[s.token] != s.from {
                return Err(format!("wave {w}: token {} not at its source", s.token));
            }
            let (src, dst) = (plan.src(*s), plan.dst(*s));
            if let Some(t) = at(&snapshot, &dst.path) {
                return Err(format!("wave {w}: destination {:?} occupied by token {t}", dst.path));
            }
            for loc in [src, dst] {
                let parent = loc.path.parent().unwrap_or_default();
                // A token directory whose journey includes this parent path.
                for (t, tok) in plan.tokens.iter().enumerate() {
                    if tok.kind != Kind::Dir || tok.id != loc.parent {
                        continue;
                    }
                    if tok.locs[snapshot[t]].path != parent {
                        return Err(format!("wave {w}: parent {parent:?} of {:?} not in place yet", loc.path));
                    }
                }
                if let Some(t) = at(&snapshot, &parent) {
                    if plan.tokens[t].id != loc.parent {
                        return Err(format!("wave {w}: parent identity mismatch at {parent:?}"));
                    }
                }
            }
        }
        for s in wave {
            pos[s.token] = s.from + 1;
        }
    }
    let mut finals = BTreeSet::new();
    for (t, tok) in plan.tokens.iter().enumerate() {
        if pos[t] != tok.locs.len() - 1 {
            return Err(format!("token {t} does not reach its final location"));
        }
        if !finals.insert(&tok.locs[pos[t]].path) {
            return Err(format!("two tokens end at {:?}", tok.locs[pos[t]].path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn p(s: &str) -> RelPath {
        RelPath::from_parts(s.split('/'))
    }
    fn id(i: u64) -> FileId {
        FileId { dev: 1, ino: i }
    }
    const ROOT: u64 = 1;
    const BK: u64 = 2;
    const ST: u64 = 3;

    /// rename a→z (dir, id 10) then write z/new (blob id 20) and remove a/old (id 11).
    fn sample() -> Plan {
        Plan {
            tokens: vec![
                Token { kind: Kind::File, id: id(11), locs: vec![
                    Loc { path: p("a/old"), parent: id(10) },
                    Loc { path: p(".fstx/tx-1/backup/0"), parent: id(BK) },
                ]},
                Token { kind: Kind::Dir, id: id(10), locs: vec![
                    Loc { path: p("a"), parent: id(ROOT) },
                    Loc { path: p(".fstx/tx-1/backup/1"), parent: id(BK) },
                    Loc { path: p("z"), parent: id(ROOT) },
                ]},
                Token { kind: Kind::File, id: id(20), locs: vec![
                    Loc { path: p(".fstx/tx-1/staged/b0"), parent: id(ST) },
                    Loc { path: p("z/new"), parent: id(10) },
                ]},
            ],
            waves: vec![
                vec![Step { token: 0, from: 0 }],
                vec![Step { token: 1, from: 0 }],
                vec![Step { token: 1, from: 1 }],
                vec![Step { token: 2, from: 0 }],
            ],
        }
    }

    #[test]
    fn sample_is_valid() {
        assert_eq!(validate(&sample()), Ok(()));
    }

    #[test]
    fn rejects_ancestor_overlap_in_one_wave() {
        // Detaching a/old and a in the same wave: a is an ancestor of a/old.
        let mut plan = sample();
        plan.waves = vec![
            vec![Step { token: 0, from: 0 }, Step { token: 1, from: 0 }],
            vec![Step { token: 1, from: 1 }],
            vec![Step { token: 2, from: 0 }],
        ];
        assert!(validate(&plan).unwrap_err().contains("overlap"));
    }

    #[test]
    fn rejects_parent_produced_in_same_wave() {
        // z (attach of dir 10) and z/new in one wave.
        let mut plan = sample();
        plan.waves = vec![
            vec![Step { token: 0, from: 0 }],
            vec![Step { token: 1, from: 0 }],
            vec![Step { token: 1, from: 1 }, Step { token: 2, from: 0 }],
        ];
        assert!(validate(&plan).is_err());
    }

    #[test]
    fn rejects_swapped_waves() {
        let mut plan = sample();
        plan.waves.swap(2, 3);
        assert!(validate(&plan).is_err());
        let mut plan = sample();
        plan.waves.swap(0, 1);
        assert!(validate(&plan).is_err());
    }

    #[test]
    fn rejects_occupied_destination_and_duplicate_finals() {
        let mut plan = sample();
        plan.tokens[2].locs[1].path = p("z");
        assert!(validate(&plan).is_err());
    }

    #[test]
    fn shared_ancestors_are_allowed() {
        let plan = Plan {
            tokens: vec![
                Token { kind: Kind::File, id: id(11), locs: vec![
                    Loc { path: p("d/x"), parent: id(9) },
                    Loc { path: p(".fstx/t/backup/0"), parent: id(BK) },
                ]},
                Token { kind: Kind::File, id: id(12), locs: vec![
                    Loc { path: p("d/y"), parent: id(9) },
                    Loc { path: p(".fstx/t/backup/1"), parent: id(BK) },
                ]},
            ],
            waves: vec![vec![Step { token: 0, from: 0 }, Step { token: 1, from: 0 }]],
        };
        assert_eq!(validate(&plan), Ok(()));
    }

    proptest! {
        /// Random reorderings of a valid plan's waves are valid only if they keep
        /// each token's steps ordered and never merge dependent steps.
        #[test]
        fn random_wave_merges_are_caught(merge in 0usize..3) {
            let mut plan = sample();
            let w = plan.waves.remove(merge + 1);
            plan.waves[merge].extend(w);
            prop_assert!(validate(&plan).is_err());
        }
    }
}
