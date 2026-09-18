//! The logical layer (DESIGN.md §1): which *directory entry* occupies each path once the
//! transaction's operations are applied. Nothing on disk changes before commit.
//!
//! * `placed` holds explicit decisions at view paths (a moved/new entry, or `Absent`).
//!   Paths without an explicit decision inherit from the nearest ancestor: a base
//!   directory's untouched children stay inside it, even when that directory moved.
//! * `detached` holds base entries that no longer sit at their original base path
//!   (moved, replaced or removed), keyed by base path.
//!
//! Equality is identity, never content: a path is unchanged only if the same base entry
//! is still there (see `plan::compile`).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::ops::Bound;

use crate::error::{Error, Result};
use crate::path::RelPath;
use crate::vfs::{FileId, Kind, Vfs};

/// Where an entry's inode comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Src {
    /// An existing entry found at base path `origin`.
    Base {
        origin: RelPath,
        id: FileId,
        mode: u32,
    },
    /// A staged file `staged/b<n>`.
    Blob { n: u32, mode: Option<u32> },
    /// A directory created in this transaction, staged as `staged/d<n>`.
    NewDir { n: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EKind {
    File,
    Dir,
    /// Symlink, FIFO, socket or device: may live inside moved/removed directories, but
    /// cannot be operated on directly in v0.1.
    Special,
}

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub kind: EKind,
    pub src: Src,
}

#[derive(Clone, Debug)]
pub(crate) enum Slot {
    Absent,
    Present(Entry),
}

pub(crate) struct Overlay {
    pub placed: BTreeMap<RelPath, Slot>,
    pub detached: BTreeMap<RelPath, (FileId, EKind)>,
    root_id: FileId,
    case_insensitive: bool,
    next_dir: u32,
}

fn ekind(k: Kind) -> EKind {
    match k {
        Kind::File => EKind::File,
        Kind::Dir => EKind::Dir,
        Kind::Symlink | Kind::Other => EKind::Special,
    }
}

impl Overlay {
    pub fn new(root_id: FileId, case_insensitive: bool) -> Overlay {
        Overlay {
            placed: BTreeMap::new(),
            detached: BTreeMap::new(),
            root_id,
            case_insensitive,
            next_dir: 0,
        }
    }

    pub fn root_entry(&self) -> Entry {
        Entry {
            kind: EKind::Dir,
            src: Src::Base {
                origin: RelPath::root(),
                id: self.root_id,
                mode: 0,
            },
        }
    }

    fn base_lookup(&self, vfs: &dyn Vfs, b: &RelPath) -> Result<Option<Entry>> {
        if b.is_private() {
            return Ok(None);
        }
        if self.case_insensitive {
            let parent = b.parent().unwrap_or_default();
            let name = b.file_name().expect("non-root");
            let names = vfs.read_dir(&parent)?;
            if !names.iter().any(|n| n == name) {
                let folded = crate::path::os_bytes(name).to_ascii_lowercase();
                if let Some(other) = names
                    .iter()
                    .find(|n| crate::path::os_bytes(n).to_ascii_lowercase() == folded)
                {
                    return Err(Error::CaseCollision {
                        path: b.to_path_buf(),
                        existing: parent.join(other.clone()).to_path_buf(),
                    });
                }
                return Ok(None);
            }
        }
        Ok(vfs.stat(b)?.map(|m| Entry {
            kind: ekind(m.kind),
            src: Src::Base {
                origin: b.clone(),
                id: m.id,
                mode: m.mode,
            },
        }))
    }

    /// The entry at view path `p`, or `None` if nothing is there (or a parent is not a dir).
    pub fn resolve(&self, vfs: &dyn Vfs, p: &RelPath) -> Result<Option<Entry>> {
        let mut cur = self.root_entry();
        let mut view = RelPath::root();
        for c in p.components() {
            if cur.kind != EKind::Dir {
                return Ok(None);
            }
            view = view.join(c.clone());
            if let Some(slot) = self.placed.get(&view) {
                match slot {
                    Slot::Absent => return Ok(None),
                    Slot::Present(e) => {
                        cur = e.clone();
                        continue;
                    }
                }
            }
            let origin = match &cur.src {
                Src::Base { origin, .. } => origin.join(c.clone()),
                Src::Blob { .. } | Src::NewDir { .. } => return Ok(None),
            };
            if self.detached.contains_key(&origin) {
                return Ok(None);
            }
            match self.base_lookup(vfs, &origin)? {
                Some(e) => cur = e,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    fn require_parent_dir(&self, vfs: &dyn Vfs, p: &RelPath) -> Result<Entry> {
        let parent = p.parent().unwrap_or_default();
        match self.resolve(vfs, &parent)? {
            Some(e) if e.kind == EKind::Dir => Ok(e),
            Some(_) => Err(Error::NotADirectory(parent.to_path_buf())),
            None => Err(Error::NotFound(parent.to_path_buf())),
        }
    }

    fn children_of_placed(&self, dir: &RelPath) -> Vec<RelPath> {
        self.placed
            .range((Bound::Excluded(dir.clone()), Bound::Unbounded))
            .take_while(|(k, _)| dir.is_ancestor_of(k))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Names visible in the view directory `dir` (whose entry is `e`).
    pub fn list(&self, vfs: &dyn Vfs, dir: &RelPath, e: &Entry) -> Result<BTreeSet<OsString>> {
        let mut names = BTreeSet::new();
        for k in self.children_of_placed(dir) {
            if k.depth() == dir.depth() + 1 && matches!(self.placed.get(&k), Some(Slot::Present(_)))
            {
                names.insert(k.file_name().unwrap().to_os_string());
            }
        }
        if let Src::Base { origin, .. } = &e.src {
            for n in vfs.read_dir(origin)? {
                let v = dir.join(n.clone());
                if v.is_private()
                    || self.placed.contains_key(&v)
                    || self.detached.contains_key(&origin.join(n.clone()))
                {
                    continue;
                }
                names.insert(n);
            }
        }
        Ok(names)
    }

    /// On case-insensitive filesystems, refuses a new name that folds onto a sibling.
    fn check_new_name(&self, vfs: &dyn Vfs, p: &RelPath, parent: &Entry) -> Result<()> {
        if !self.case_insensitive {
            return Ok(());
        }
        let dir = p.parent().unwrap_or_default();
        let folded = crate::path::os_bytes(p.file_name().unwrap()).to_ascii_lowercase();
        for n in self.list(vfs, &dir, parent)? {
            if crate::path::os_bytes(&n).to_ascii_lowercase() == folded {
                return Err(Error::CaseCollision {
                    path: p.to_path_buf(),
                    existing: dir.join(n).to_path_buf(),
                });
            }
        }
        Ok(())
    }

    fn detach(&mut self, e: &Entry) {
        if let Src::Base { origin, id, .. } = &e.src {
            self.detached.insert(origin.clone(), (*id, e.kind));
        }
    }

    fn drop_placed_under(&mut self, p: &RelPath) -> Vec<(RelPath, Slot)> {
        let keys = self.children_of_placed(p);
        keys.into_iter()
            .map(|k| {
                let s = self.placed.remove(&k).unwrap();
                (k, s)
            })
            .collect()
    }

    /// Validates a write to `p`; returns the permission bits the new file must inherit.
    pub fn check_write(&self, vfs: &dyn Vfs, p: &RelPath) -> Result<Option<u32>> {
        let parent = self.require_parent_dir(vfs, p)?;
        match self.resolve(vfs, p)? {
            Some(Entry {
                kind: EKind::Dir, ..
            }) => Err(Error::IsADirectory(p.to_path_buf())),
            Some(Entry {
                kind: EKind::Special,
                ..
            }) => Err(Error::UnsupportedFileType(p.to_path_buf())),
            Some(Entry {
                src: Src::Base { mode, .. },
                ..
            }) => Ok(Some(mode)),
            Some(Entry {
                src: Src::Blob { mode, .. },
                ..
            }) => Ok(mode),
            Some(Entry {
                src: Src::NewDir { .. },
                ..
            }) => unreachable!("new dirs are dirs"),
            None => {
                self.check_new_name(vfs, p, &parent)?;
                Ok(None)
            }
        }
    }

    /// Records a write whose data is staged as blob `n` (after `check_write`).
    pub fn apply_write(
        &mut self,
        vfs: &dyn Vfs,
        p: &RelPath,
        n: u32,
        mode: Option<u32>,
    ) -> Result<()> {
        if let Some(e) = self.resolve(vfs, p)? {
            self.detach(&e);
        }
        self.placed.insert(
            p.clone(),
            Slot::Present(Entry {
                kind: EKind::File,
                src: Src::Blob { n, mode },
            }),
        );
        Ok(())
    }

    pub fn create_dir_all(&mut self, vfs: &dyn Vfs, p: &RelPath) -> Result<()> {
        for q in p.proper_prefixes().chain(std::iter::once(p.clone())) {
            match self.resolve(vfs, &q)? {
                Some(e) if e.kind == EKind::Dir => {}
                Some(_) => return Err(Error::NotADirectory(q.to_path_buf())),
                None => {
                    let parent = self.require_parent_dir(vfs, &q)?;
                    self.check_new_name(vfs, &q, &parent)?;
                    let n = self.next_dir;
                    self.next_dir += 1;
                    self.placed.insert(
                        q,
                        Slot::Present(Entry {
                            kind: EKind::Dir,
                            src: Src::NewDir { n },
                        }),
                    );
                }
            }
        }
        Ok(())
    }

    pub fn rename(&mut self, vfs: &dyn Vfs, from: &RelPath, to: &RelPath) -> Result<()> {
        let e = self
            .resolve(vfs, from)?
            .ok_or_else(|| Error::NotFound(from.to_path_buf()))?;
        if e.kind == EKind::Special {
            return Err(Error::UnsupportedFileType(from.to_path_buf()));
        }
        if from == to {
            return Ok(());
        }
        if from.is_ancestor_of(to) {
            return Err(Error::InvalidMove {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
            });
        }
        let parent = self.require_parent_dir(vfs, to)?;
        if self.resolve(vfs, to)?.is_some() {
            return Err(Error::AlreadyExists(to.to_path_buf()));
        }
        self.check_new_name(vfs, to, &parent)?;
        let moved = self.drop_placed_under(from);
        self.placed.insert(from.clone(), Slot::Absent);
        self.detach(&e);
        for (k, s) in moved {
            let rest = k.strip_prefix(from).expect("under from");
            self.placed.insert(to.join_path(&rest), s);
        }
        self.placed.insert(to.clone(), Slot::Present(e));
        Ok(())
    }

    /// Removes a file or an empty directory.
    pub fn remove(&mut self, vfs: &dyn Vfs, p: &RelPath) -> Result<()> {
        let e = self
            .resolve(vfs, p)?
            .ok_or_else(|| Error::NotFound(p.to_path_buf()))?;
        match e.kind {
            EKind::Special => return Err(Error::UnsupportedFileType(p.to_path_buf())),
            EKind::Dir if !self.list(vfs, p, &e)?.is_empty() => {
                return Err(Error::DirectoryNotEmpty(p.to_path_buf()));
            }
            _ => {}
        }
        self.drop_placed_under(p);
        self.placed.insert(p.clone(), Slot::Absent);
        self.detach(&e);
        Ok(())
    }

    pub fn remove_dir_all(&mut self, vfs: &dyn Vfs, p: &RelPath) -> Result<()> {
        let e = self
            .resolve(vfs, p)?
            .ok_or_else(|| Error::NotFound(p.to_path_buf()))?;
        match e.kind {
            EKind::Dir => {}
            EKind::File => return Err(Error::NotADirectory(p.to_path_buf())),
            EKind::Special => return Err(Error::UnsupportedFileType(p.to_path_buf())),
        }
        self.drop_placed_under(p);
        self.placed.insert(p.clone(), Slot::Absent);
        self.detach(&e);
        Ok(())
    }

    /// Reads the file at `p` through the overlay (read-your-writes).
    pub fn read(
        &self,
        vfs: &dyn Vfs,
        p: &RelPath,
        blob_path: impl Fn(u32) -> RelPath,
    ) -> Result<Vec<u8>> {
        match self.resolve(vfs, p)? {
            None => Err(Error::NotFound(p.to_path_buf())),
            Some(Entry {
                kind: EKind::Dir, ..
            }) => Err(Error::IsADirectory(p.to_path_buf())),
            Some(Entry {
                kind: EKind::Special,
                ..
            }) => Err(Error::UnsupportedFileType(p.to_path_buf())),
            Some(Entry {
                src: Src::Base { origin, .. },
                ..
            }) => Ok(vfs.read_file(&origin)?),
            Some(Entry {
                src: Src::Blob { n, .. },
                ..
            }) => Ok(vfs.read_file(&blob_path(n))?),
            Some(Entry {
                src: Src::NewDir { .. },
                ..
            }) => unreachable!(),
        }
    }
}
