//! `SimFs`: an in-memory filesystem that implements exactly the crash-consistency
//! contract C1–C5 (DESIGN.md §5) and is otherwise adversarial.
//!
//! * Every namespace mutation (create, mkdir, rename, unlink, rmdir) is recorded as a
//!   pending op. It becomes *guaranteed durable* once every directory it touches has been
//!   `sync_dir`ed after it (C2). Until then a power loss may keep or drop it, independently
//!   of every other pending op (each op atomic: C1). Under [`Profile::Weak`] a pending
//!   rename of a non-directory may also persist with *both* names.
//! * File data is durable only after `sync_file` (C3); unsynced data comes back as one of
//!   several adversarial variants (old, new, torn, garbage).
//! * Crashes are injected by a syscall budget; afterwards every call fails.
//!
//! The simulator also flags *protocol violations*: re-using a name whose removal is not
//! yet durable for a different inode within the same sync window.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::path::{PRIVATE_DIR, RelPath};
use crate::vfs::{FileId, Kind, LockGuard, Meta, Vfs, parent_mismatch};

/// How renames may persist across a power loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// C1: a rename persists entirely or not at all.
    Strict,
    /// C1 relaxed: a rename of a non-directory may also persist as "both names".
    Weak,
}

/// Simulator configuration; the flags model filesystems that fail capability probes.
#[derive(Clone, Copy, Debug)]
pub struct SimConfig {
    pub profile: Profile,
    /// `RENAME_NOREPLACE` supported.
    pub noreplace: bool,
    /// Inode numbers survive renames.
    pub stable_ids: bool,
    /// Name lookups fold ASCII case.
    pub case_insensitive: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig { profile: Profile::Strict, noreplace: true, stable_ids: true, case_insensitive: false }
    }
}

/// What a pending op does in a power-loss outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    Drop,
    Apply,
    /// Weak profile only: the rename adds the new name but keeps the old one.
    BothNames,
}

#[derive(Clone, Debug)]
enum Node {
    File { data: Vec<u8>, mode: u32 },
    Dir { entries: BTreeMap<OsString, u64>, mode: u32 },
    Symlink,
}

#[derive(Clone, Debug)]
struct Image {
    nodes: BTreeMap<u64, Node>,
    root: u64,
}

#[derive(Clone, Debug)]
enum Op {
    Link { dir: u64, name: OsString, ino: u64 },
    Unlink { dir: u64, name: OsString, ino: u64 },
    Rename { fdir: u64, fname: OsString, tdir: u64, tname: OsString, ino: u64, is_dir: bool },
}

impl Op {
    fn dirs(&self) -> BTreeSet<u64> {
        match self {
            Op::Link { dir, .. } | Op::Unlink { dir, .. } => BTreeSet::from([*dir]),
            Op::Rename { fdir, tdir, .. } => BTreeSet::from([*fdir, *tdir]),
        }
    }
}

#[derive(Clone, Debug)]
struct Pending {
    op: Op,
    unsynced: BTreeSet<u64>,
}

#[derive(Clone, Debug)]
struct State {
    cfg: SimConfig,
    vol: Image,
    dur: Image,
    pending: VecDeque<Pending>,
    dirty: BTreeSet<u64>,
    next_ino: u64,
    id_salt: BTreeMap<u64, u64>,
    budget: Option<u64>,
    calls: u64,
    mutations: u64,
    crashed: bool,
    events: Vec<&'static str>,
    violations: Vec<String>,
    stabilized: Option<Box<State>>,
}

/// A shared handle to one simulated filesystem (clones share state).
#[derive(Clone)]
pub struct SimFs {
    st: Arc<Mutex<State>>,
}

/// One entry of a [`SimFs::tree`] snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub kind: Kind,
    pub data: Vec<u8>,
    pub ino: u64,
}

fn crash_err() -> io::Error {
    io::Error::other("simulated crash")
}

fn err(kind: io::ErrorKind, what: &str) -> io::Error {
    io::Error::new(kind, what.to_string())
}

fn fold_name(s: &OsStr) -> Vec<u8> {
    crate::path::os_bytes(s).to_ascii_lowercase()
}

impl Image {
    fn empty() -> Image {
        let mut nodes = BTreeMap::new();
        nodes.insert(1, Node::Dir { entries: BTreeMap::new(), mode: 0o755 });
        Image { nodes, root: 1 }
    }

    fn entries(&self, dir: u64) -> Option<&BTreeMap<OsString, u64>> {
        match self.nodes.get(&dir) {
            Some(Node::Dir { entries, .. }) => Some(entries),
            _ => None,
        }
    }

    fn entries_mut(&mut self, dir: u64) -> Option<&mut BTreeMap<OsString, u64>> {
        match self.nodes.get_mut(&dir) {
            Some(Node::Dir { entries, .. }) => Some(entries),
            _ => None,
        }
    }

    fn lookup(&self, dir: u64, name: &OsStr, ci: bool) -> Option<(OsString, u64)> {
        let e = self.entries(dir)?;
        if let Some(&ino) = e.get(name) {
            return Some((name.to_os_string(), ino));
        }
        if ci {
            let f = fold_name(name);
            return e.iter().find(|(k, _)| fold_name(k) == f).map(|(k, v)| (k.clone(), *v));
        }
        None
    }

    /// Resolves a directory path; symlinks in components are refused like `RESOLVE_NO_SYMLINKS`.
    fn resolve_dir(&self, p: &RelPath, ci: bool) -> io::Result<u64> {
        let mut cur = self.root;
        for c in p.components() {
            let (_, ino) = self.lookup(cur, c, ci).ok_or_else(|| err(io::ErrorKind::NotFound, "no such directory"))?;
            match self.nodes.get(&ino) {
                Some(Node::Dir { .. }) => cur = ino,
                Some(Node::Symlink) => return Err(err(io::ErrorKind::InvalidInput, "symlink in path (ELOOP)")),
                _ => return Err(err(io::ErrorKind::NotADirectory, "not a directory")),
            }
        }
        Ok(cur)
    }

    fn walk(&self, dir: u64, prefix: &str, skip_private: bool, out: &mut BTreeMap<String, TreeEntry>) {
        let Some(entries) = self.entries(dir) else { return };
        for (name, &ino) in entries {
            if skip_private && prefix.is_empty() && name == PRIVATE_DIR {
                continue;
            }
            let path = if prefix.is_empty() {
                name.to_string_lossy().into_owned()
            } else {
                format!("{prefix}/{}", name.to_string_lossy())
            };
            let (kind, data) = match self.nodes.get(&ino) {
                Some(Node::File { data, .. }) => (Kind::File, data.clone()),
                Some(Node::Dir { .. }) => (Kind::Dir, Vec::new()),
                _ => (Kind::Symlink, Vec::new()),
            };
            out.insert(path.clone(), TreeEntry { kind, data, ino });
            if kind == Kind::Dir {
                self.walk(ino, &path, skip_private, out);
            }
        }
    }
}

impl State {
    fn count(&mut self) -> io::Result<()> {
        if self.crashed {
            return Err(crash_err());
        }
        if let Some(b) = self.budget {
            if self.calls >= b {
                self.crashed = true;
                return Err(crash_err());
            }
        }
        self.calls += 1;
        Ok(())
    }

    fn alive(&self) -> io::Result<()> {
        if self.crashed { Err(crash_err()) } else { Ok(()) }
    }

    fn ci(&self) -> bool {
        self.cfg.case_insensitive
    }

    fn file_id(&self, ino: u64) -> FileId {
        let salt = self.id_salt.get(&ino).copied().unwrap_or(0);
        FileId { dev: 1, ino: ino + salt * 1_000_000_000 }
    }

    fn meta(&self, ino: u64) -> Meta {
        let (kind, len, mode) = match self.vol.nodes.get(&ino) {
            Some(Node::File { data, mode }) => (Kind::File, data.len() as u64, *mode),
            Some(Node::Dir { mode, .. }) => (Kind::Dir, 0, *mode),
            _ => (Kind::Symlink, 0, 0o777),
        };
        let nlink = self
            .vol
            .nodes
            .values()
            .filter_map(|n| match n {
                Node::Dir { entries, .. } => Some(entries.values().filter(|&&i| i == ino).count() as u64),
                _ => None,
            })
            .sum();
        Meta { kind, id: self.file_id(ino), len, mode, nlink }
    }

    fn parent_and_leaf(&self, p: &RelPath) -> io::Result<(u64, OsString)> {
        let leaf = p.file_name().ok_or_else(|| err(io::ErrorKind::InvalidInput, "root"))?.to_os_string();
        Ok((self.vol.resolve_dir(&p.parent().unwrap_or_default(), self.ci())?, leaf))
    }

    fn check_parent(&self, dir: u64, expect: Option<FileId>) -> io::Result<()> {
        match expect {
            Some(want) if self.file_id(dir) != want => Err(parent_mismatch()),
            _ => Ok(()),
        }
    }

    fn lookup_leaf(&self, p: &RelPath) -> io::Result<Option<u64>> {
        if p.is_root() {
            return Ok(Some(self.vol.root));
        }
        let (dir, leaf) = match self.parent_and_leaf(p) {
            Ok(x) => x,
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(self.vol.lookup(dir, &leaf, self.ci()).map(|(_, i)| i))
    }

    /// Records a namespace mutation and checks the name-reuse rule.
    fn record(&mut self, op: Op) {
        if let Op::Link { dir, name, ino } | Op::Rename { tdir: dir, tname: name, ino, .. } = &op {
            for p in &self.pending {
                let removed = match &p.op {
                    Op::Unlink { dir: d, name: n, ino: i } => (d, n, i),
                    Op::Rename { fdir: d, fname: n, ino: i, .. } => (d, n, i),
                    Op::Link { .. } => continue,
                };
                if !p.unsynced.is_empty() && removed.0 == dir && removed.1 == name && removed.2 != ino {
                    self.violations.push(format!(
                        "name {name:?} in dir {dir} reused for inode {ino} before the removal of inode {} was durable",
                        removed.2
                    ));
                }
            }
        }
        self.mutations += 1;
        let unsynced = op.dirs();
        self.pending.push_back(Pending { op, unsynced });
    }

    fn fold(&mut self) {
        while self.pending.front().is_some_and(|p| p.unsynced.is_empty()) {
            let p = self.pending.pop_front().unwrap();
            let vol = self.vol.clone();
            apply_op(&mut self.dur, &vol, &p.op, Fate::Apply, &mut Vec::new());
        }
    }

    fn fresh_ino(&mut self) -> u64 {
        let i = self.next_ino;
        self.next_ino += 1;
        i
    }
}

fn ensure_node(img: &mut Image, vol: &Image, ino: u64) {
    if img.nodes.contains_key(&ino) {
        return;
    }
    let node = match vol.nodes.get(&ino) {
        Some(Node::File { mode, .. }) => Node::File { data: Vec::new(), mode: *mode },
        Some(Node::Dir { mode, .. }) => Node::Dir { entries: BTreeMap::new(), mode: *mode },
        _ => Node::Symlink,
    };
    img.nodes.insert(ino, node);
}

fn apply_op(img: &mut Image, vol: &Image, op: &Op, fate: Fate, violations: &mut Vec<String>) {
    if fate == Fate::Drop {
        return;
    }
    let mut insert = |img: &mut Image, dir: u64, name: &OsString, ino: u64| {
        ensure_node(img, vol, dir);
        ensure_node(img, vol, ino);
        let entries = img.entries_mut(dir).expect("dir node");
        if let Some(&old) = entries.get(name) {
            if old != ino {
                violations.push(format!("durable name {name:?} in dir {dir} overwritten"));
            }
        }
        entries.insert(name.clone(), ino);
    };
    match op {
        Op::Link { dir, name, ino } => insert(img, *dir, name, *ino),
        Op::Unlink { dir, name, ino } => {
            if let Some(e) = img.entries_mut(*dir) {
                if e.get(name) == Some(ino) {
                    e.remove(name);
                }
            }
        }
        Op::Rename { fdir, fname, tdir, tname, ino, .. } => {
            if fate != Fate::BothNames {
                if let Some(e) = img.entries_mut(*fdir) {
                    if e.get(fname) == Some(ino) {
                        e.remove(fname);
                    }
                }
            }
            insert(img, *tdir, tname, *ino);
        }
    }
}

fn data_variant(old: &[u8], new: &[u8], seed: u8) -> Vec<u8> {
    let tail: &[u8] = if new.starts_with(old) { &new[old.len()..] } else { new };
    let base: &[u8] = if new.starts_with(old) { old } else { &[] };
    match seed % 4 {
        0 => old.to_vec(),
        1 => new.to_vec(),
        2 => [base, &tail[..tail.len() / 2]].concat(),
        _ => {
            let mut v = base.to_vec();
            v.extend(tail.iter().map(|b| b ^ 0x5A));
            if tail.is_empty() {
                v.extend_from_slice(b"\xde\xad");
            }
            v
        }
    }
}

impl SimFs {
    pub fn new(cfg: SimConfig) -> SimFs {
        SimFs {
            st: Arc::new(Mutex::new(State {
                cfg,
                vol: Image::empty(),
                dur: Image::empty(),
                pending: VecDeque::new(),
                dirty: BTreeSet::new(),
                next_ino: 2,
                id_salt: BTreeMap::new(),
                budget: None,
                calls: 0,
                mutations: 0,
                crashed: false,
                events: Vec::new(),
                violations: Vec::new(),
                stabilized: None,
            })),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn from_state(st: State) -> SimFs {
        SimFs { st: Arc::new(Mutex::new(st)) }
    }

    fn put(&self, path: &str, node: Node) {
        let mut guard = self.lock_state();
        let st = &mut *guard;
        let p = RelPath::from_parts(path.split('/'));
        let (dir, leaf) = st.parent_and_leaf(&p).expect("parent exists");
        let ino = st.fresh_ino();
        for img in [&mut st.vol, &mut st.dur] {
            // The parent may exist only in the volatile image (setup on a crashed state).
            if let Some(entries) = img.entries_mut(dir) {
                entries.insert(leaf.clone(), ino);
                img.nodes.insert(ino, node.clone());
            }
        }
    }

    /// Adds a durable file (test setup).
    pub fn put_file(&self, path: &str, data: &[u8]) {
        self.put(path, Node::File { data: data.to_vec(), mode: 0o644 });
    }

    /// Adds a durable directory (test setup).
    pub fn put_dir(&self, path: &str) {
        self.put(path, Node::Dir { entries: BTreeMap::new(), mode: 0o755 });
    }

    /// Adds a durable hard link `new` to the existing file `existing` (test setup).
    pub fn put_hardlink(&self, existing: &str, new: &str) {
        let mut guard = self.lock_state();
        let st = &mut *guard;
        let ino = st.lookup_leaf(&RelPath::from_parts(existing.split('/'))).unwrap().expect("exists");
        let p = RelPath::from_parts(new.split('/'));
        let (dir, leaf) = st.parent_and_leaf(&p).expect("parent exists");
        for img in [&mut st.vol, &mut st.dur] {
            if let Some(entries) = img.entries_mut(dir) {
                entries.insert(leaf.clone(), ino);
            }
        }
    }

    /// Adds a durable symlink (test setup).
    pub fn put_symlink(&self, path: &str) {
        self.put(path, Node::Symlink);
    }

    /// Overwrites a file's content in place, durably (simulates tampering / outside writers).
    pub fn tamper_file(&self, path: &str, data: &[u8]) {
        let mut guard = self.lock_state();
        let st = &mut *guard;
        let p = RelPath::from_parts(path.split('/'));
        let ino = st.lookup_leaf(&p).unwrap().expect("exists");
        for img in [&mut st.vol, &mut st.dur] {
            if let Some(Node::File { data: d, .. }) = img.nodes.get_mut(&ino) {
                *d = data.to_vec();
            }
        }
    }

    /// Visible tree (excluding `.fstx`).
    pub fn tree(&self) -> BTreeMap<String, TreeEntry> {
        let st = self.lock_state();
        let mut out = BTreeMap::new();
        st.vol.walk(st.vol.root, "", true, &mut out);
        out
    }

    /// Everything under `.fstx`, as paths relative to it.
    pub fn private_tree(&self) -> BTreeMap<String, TreeEntry> {
        let st = self.lock_state();
        let mut out = BTreeMap::new();
        if let Some((_, ino)) = st.vol.lookup(st.vol.root, OsStr::new(PRIVATE_DIR), false) {
            st.vol.walk(ino, "", false, &mut out);
        }
        out
    }

    /// Allows `n` more counted calls (mutations and syncs), then crashes.
    pub fn crash_after(&self, n: u64) {
        let mut st = self.lock_state();
        st.budget = Some(st.calls + n);
    }

    /// Number of counted calls (mutations and syncs) so far.
    pub fn calls(&self) -> u64 {
        self.lock_state().calls
    }

    /// Number of namespace mutations so far.
    pub fn mutations(&self) -> u64 {
        self.lock_state().mutations
    }

    pub fn crashed(&self) -> bool {
        self.lock_state().crashed
    }

    pub fn events(&self) -> Vec<&'static str> {
        self.lock_state().events.clone()
    }

    pub fn clear_events(&self) {
        self.lock_state().events.clear();
    }

    /// Protocol violations detected so far (must stay empty).
    pub fn violations(&self) -> Vec<String> {
        self.lock_state().violations.clone()
    }

    /// Pending ops that a power loss may drop, and whether each may persist as both names.
    pub fn volatile_ops(&self) -> Vec<bool> {
        let st = self.lock_state();
        let weak = st.cfg.profile == Profile::Weak;
        st.pending
            .iter()
            .filter(|p| !p.unsynced.is_empty())
            .map(|p| weak && matches!(p.op, Op::Rename { is_dir: false, .. }))
            .collect()
    }

    /// The process dies; the kernel (and its caches) survive. Returns a fresh handle.
    pub fn process_kill(&self) -> SimFs {
        let mut st = self.lock_state().clone();
        st.crashed = false;
        st.budget = None;
        st.stabilized = None;
        SimFs::from_state(st)
    }

    /// Power loss: the durable image plus the given fates for each volatile op (in the
    /// order of [`Self::volatile_ops`]); unsynced file data takes variant `data_seed`.
    pub fn power_loss(&self, fates: &[Fate], data_seed: u8) -> SimFs {
        let st = self.lock_state();
        let mut img = st.dur.clone();
        let mut violations = st.violations.clone();
        let mut fi = fates.iter();
        for p in &st.pending {
            let fate = if p.unsynced.is_empty() { Fate::Apply } else { *fi.next().unwrap_or(&Fate::Drop) };
            apply_op(&mut img, &st.vol, &p.op, fate, &mut violations);
        }
        for &ino in &st.dirty {
            let new = match st.vol.nodes.get(&ino) {
                Some(Node::File { data, .. }) => data.clone(),
                _ => continue,
            };
            if let Some(Node::File { data, .. }) = img.nodes.get_mut(&ino) {
                *data = data_variant(data, &new, data_seed);
            }
        }
        SimFs::from_state(State {
            cfg: st.cfg,
            vol: img.clone(),
            dur: img,
            pending: VecDeque::new(),
            dirty: BTreeSet::new(),
            next_ino: st.next_ino,
            id_salt: st.id_salt.clone(),
            budget: None,
            calls: 0,
            mutations: 0,
            crashed: false,
            events: Vec::new(),
            violations,
            stabilized: None,
        })
    }

    /// The state a power loss that drops every volatile op would leave.
    pub fn durable(&self) -> SimFs {
        self.power_loss(&[], 0)
    }

    /// The durable image at the moment recovery reported it had stabilized, if it did.
    pub fn durable_at_stabilize(&self) -> Option<SimFs> {
        let st = self.lock_state();
        st.stabilized.as_ref().map(|s| SimFs::from_state((**s).clone()).durable())
    }

    /// Canonical fingerprint of volatile + durable + pending state, for memoization.
    pub fn fingerprint(&self) -> String {
        let st = self.lock_state();
        format!("{:?}|{:?}|{:?}|{:?}", st.vol.nodes, st.dur.nodes, st.pending, st.dirty)
    }
}

impl Vfs for SimFs {
    fn root_id(&self) -> io::Result<FileId> {
        let st = self.lock_state();
        st.alive()?;
        Ok(st.file_id(st.vol.root))
    }

    fn stat(&self, p: &RelPath) -> io::Result<Option<Meta>> {
        let st = self.lock_state();
        st.alive()?;
        Ok(st.lookup_leaf(p)?.map(|i| st.meta(i)))
    }

    fn read_file(&self, p: &RelPath) -> io::Result<Vec<u8>> {
        let st = self.lock_state();
        st.alive()?;
        let ino = st.lookup_leaf(p)?.ok_or_else(|| err(io::ErrorKind::NotFound, "no such file"))?;
        match st.vol.nodes.get(&ino) {
            Some(Node::File { data, .. }) => Ok(data.clone()),
            _ => Err(err(io::ErrorKind::InvalidInput, "not a regular file")),
        }
    }

    fn read_dir(&self, p: &RelPath) -> io::Result<Vec<OsString>> {
        let st = self.lock_state();
        st.alive()?;
        let dir = st.vol.resolve_dir(p, st.ci())?;
        Ok(st.vol.entries(dir).map(|e| e.keys().cloned().collect()).unwrap_or_default())
    }

    fn create_file(&self, p: &RelPath, data: &[u8], mode: Option<u32>) -> io::Result<Meta> {
        let mut st = self.lock_state();
        st.count()?;
        let (dir, leaf) = st.parent_and_leaf(p)?;
        if st.vol.lookup(dir, &leaf, st.ci()).is_some() {
            return Err(err(io::ErrorKind::AlreadyExists, "exists"));
        }
        let ino = st.fresh_ino();
        st.vol.nodes.insert(ino, Node::File { data: data.to_vec(), mode: mode.unwrap_or(0o644) });
        st.vol.entries_mut(dir).unwrap().insert(leaf.clone(), ino);
        st.dirty.insert(ino);
        st.record(Op::Link { dir, name: leaf, ino });
        Ok(st.meta(ino))
    }

    fn append(&self, p: &RelPath, data: &[u8]) -> io::Result<()> {
        let mut st = self.lock_state();
        st.count()?;
        let ino = st.lookup_leaf(p)?.ok_or_else(|| err(io::ErrorKind::NotFound, "no such file"))?;
        match st.vol.nodes.get_mut(&ino) {
            Some(Node::File { data: d, .. }) => d.extend_from_slice(data),
            _ => return Err(err(io::ErrorKind::InvalidInput, "not a regular file")),
        }
        st.dirty.insert(ino);
        Ok(())
    }

    fn mkdir(&self, p: &RelPath) -> io::Result<Meta> {
        let mut st = self.lock_state();
        st.count()?;
        let (dir, leaf) = st.parent_and_leaf(p)?;
        if st.vol.lookup(dir, &leaf, st.ci()).is_some() {
            return Err(err(io::ErrorKind::AlreadyExists, "exists"));
        }
        let ino = st.fresh_ino();
        st.vol.nodes.insert(ino, Node::Dir { entries: BTreeMap::new(), mode: 0o755 });
        st.vol.entries_mut(dir).unwrap().insert(leaf.clone(), ino);
        st.record(Op::Link { dir, name: leaf, ino });
        Ok(st.meta(ino))
    }

    fn rename_noreplace(
        &self,
        from: &RelPath,
        expect_from_parent: Option<FileId>,
        to: &RelPath,
        expect_to_parent: Option<FileId>,
    ) -> io::Result<()> {
        let mut st = self.lock_state();
        st.count()?;
        if !st.cfg.noreplace {
            return Err(err(io::ErrorKind::Unsupported, "RENAME_NOREPLACE unsupported (EINVAL)"));
        }
        let (fdir, fleaf) = st.parent_and_leaf(from)?;
        let (tdir, tleaf) = st.parent_and_leaf(to)?;
        st.check_parent(fdir, expect_from_parent)?;
        st.check_parent(tdir, expect_to_parent)?;
        let ci = st.ci();
        let (fname, ino) = st.vol.lookup(fdir, &fleaf, ci).ok_or_else(|| err(io::ErrorKind::NotFound, "no source"))?;
        if st.vol.lookup(tdir, &tleaf, ci).is_some() {
            return Err(err(io::ErrorKind::AlreadyExists, "destination exists"));
        }
        let is_dir = matches!(st.vol.nodes.get(&ino), Some(Node::Dir { .. }));
        if is_dir {
            // Refuse moving a directory into its own subtree.
            let mut cur = tdir;
            let mut guard = 0;
            loop {
                if cur == ino {
                    return Err(err(io::ErrorKind::InvalidInput, "into own subtree"));
                }
                if cur == st.vol.root || guard > 10_000 {
                    break;
                }
                guard += 1;
                let parent = st.vol.nodes.iter().find_map(|(k, n)| match n {
                    Node::Dir { entries, .. } if entries.values().any(|&v| v == cur) => Some(*k),
                    _ => None,
                });
                match parent {
                    Some(p) => cur = p,
                    None => break,
                }
            }
        }
        st.vol.entries_mut(fdir).unwrap().remove(&fname);
        st.vol.entries_mut(tdir).unwrap().insert(tleaf.clone(), ino);
        if !st.cfg.stable_ids {
            *st.id_salt.entry(ino).or_insert(0) += 1;
        }
        st.record(Op::Rename { fdir, fname, tdir, tname: tleaf, ino, is_dir });
        Ok(())
    }

    fn unlink(&self, p: &RelPath, expect_parent: Option<FileId>) -> io::Result<()> {
        let mut st = self.lock_state();
        st.count()?;
        let (dir, leaf) = st.parent_and_leaf(p)?;
        st.check_parent(dir, expect_parent)?;
        let (name, ino) = st.vol.lookup(dir, &leaf, st.ci()).ok_or_else(|| err(io::ErrorKind::NotFound, "no entry"))?;
        if matches!(st.vol.nodes.get(&ino), Some(Node::Dir { .. })) {
            return Err(err(io::ErrorKind::IsADirectory, "is a directory"));
        }
        st.vol.entries_mut(dir).unwrap().remove(&name);
        st.record(Op::Unlink { dir, name, ino });
        Ok(())
    }

    fn rmdir(&self, p: &RelPath) -> io::Result<()> {
        let mut st = self.lock_state();
        st.count()?;
        let (dir, leaf) = st.parent_and_leaf(p)?;
        let (name, ino) = st.vol.lookup(dir, &leaf, st.ci()).ok_or_else(|| err(io::ErrorKind::NotFound, "no entry"))?;
        match st.vol.entries(ino) {
            None => return Err(err(io::ErrorKind::NotADirectory, "not a directory")),
            Some(e) if !e.is_empty() => return Err(err(io::ErrorKind::DirectoryNotEmpty, "not empty")),
            Some(_) => {}
        }
        st.vol.entries_mut(dir).unwrap().remove(&name);
        st.record(Op::Unlink { dir, name, ino });
        Ok(())
    }

    fn sync_file(&self, p: &RelPath) -> io::Result<()> {
        let mut st = self.lock_state();
        st.count()?;
        let ino = st.lookup_leaf(p)?.ok_or_else(|| err(io::ErrorKind::NotFound, "no such file"))?;
        let node = st.vol.nodes.get(&ino).cloned();
        match node {
            Some(Node::File { data, mode }) => {
                match st.dur.nodes.get_mut(&ino) {
                    Some(Node::File { data: d, .. }) => *d = data,
                    _ => {
                        st.dur.nodes.insert(ino, Node::File { data, mode });
                    }
                }
                st.dirty.remove(&ino);
                Ok(())
            }
            _ => Err(err(io::ErrorKind::InvalidInput, "not a regular file")),
        }
    }

    fn sync_dir(&self, p: &RelPath) -> io::Result<()> {
        let mut st = self.lock_state();
        st.count()?;
        let dir = st.vol.resolve_dir(p, st.ci())?;
        for pend in st.pending.iter_mut() {
            pend.unsynced.remove(&dir);
        }
        st.fold();
        Ok(())
    }

    fn lock(&self, _exclusive: bool) -> io::Result<LockGuard> {
        self.lock_state().alive()?;
        Ok(Box::new(()))
    }

    fn event(&self, name: &'static str) {
        let mut st = self.lock_state();
        st.events.push(name);
        if name == "stabilized" {
            let mut snap = st.clone();
            snap.stabilized = None;
            st.stabilized = Some(Box::new(snap));
        }
    }
}
