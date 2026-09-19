//! Model-based test (plan: "Identity-aware diff" + "Overlay semantics"): random operation
//! sequences run against fstx on the simulator and against a small reference model that
//! tracks *identities*, not just bytes. Accept/reject decisions, reads, and the committed
//! tree (path → kind, content, inode) must match exactly.

use std::collections::{BTreeMap, BTreeSet};

use fstx::sim::{SimConfig, SimFs};
use fstx::vfs::Kind;
use fstx::{Options, Transaction};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Node {
    /// `tag` = base inode if this is an original entry, `None` if created by the tx.
    File {
        data: Vec<u8>,
        tag: Option<u64>,
    },
    Dir {
        tag: Option<u64>,
    },
}

type Model = BTreeMap<String, Node>;

#[derive(Clone, Debug)]
enum Op {
    Write(String, u8),
    Mkdirs(String),
    Rename(String, String),
    Remove(String),
    RemoveAll(String),
    Read(String),
}

fn parent(p: &str) -> Option<&str> {
    p.rfind('/').map(|i| &p[..i])
}

fn under(p: &str, dir: &str) -> bool {
    p.len() > dir.len() && p.starts_with(dir) && p.as_bytes()[dir.len()] == b'/'
}

fn is_dir(m: &Model, p: Option<&str>) -> bool {
    match p {
        None => true,
        Some(p) => matches!(m.get(p), Some(Node::Dir { .. })),
    }
}

/// Applies `op` to the model; `Err(())` if fstx must reject it.
fn apply(m: &mut Model, op: &Op) -> Result<Option<Vec<u8>>, ()> {
    match op {
        Op::Write(p, b) => {
            if !is_dir(m, parent(p)) || matches!(m.get(p), Some(Node::Dir { .. })) {
                return Err(());
            }
            m.insert(
                p.clone(),
                Node::File {
                    data: vec![*b; 3],
                    tag: None,
                },
            );
        }
        Op::Mkdirs(p) => {
            let parts: Vec<&str> = p.split('/').collect();
            for i in 1..=parts.len() {
                let q = parts[..i].join("/");
                match m.get(&q) {
                    Some(Node::Dir { .. }) => {}
                    Some(Node::File { .. }) => return Err(()),
                    None => {
                        m.insert(q, Node::Dir { tag: None });
                    }
                }
            }
        }
        Op::Rename(f, t) => {
            if !m.contains_key(f) {
                return Err(());
            }
            if f == t {
                return Ok(None);
            }
            if under(t, f) || !is_dir(m, parent(t)) || m.contains_key(t) {
                return Err(());
            }
            let moved: Vec<String> = m
                .keys()
                .filter(|k| *k == f || under(k, f))
                .cloned()
                .collect();
            for k in moved {
                let n = m.remove(&k).unwrap();
                m.insert(format!("{t}{}", &k[f.len()..]), n);
            }
        }
        Op::Remove(p) => match m.get(p) {
            None => return Err(()),
            Some(Node::Dir { .. }) if m.keys().any(|k| under(k, p)) => return Err(()),
            Some(_) => {
                m.remove(p);
            }
        },
        Op::RemoveAll(p) => match m.get(p) {
            Some(Node::Dir { .. }) => m.retain(|k, _| k != p && !under(k, p)),
            _ => return Err(()),
        },
        Op::Read(p) => {
            return match m.get(p) {
                Some(Node::File { data, .. }) => Ok(Some(data.clone())),
                _ => Err(()),
            };
        }
    }
    Ok(None)
}

fn run(tx: &mut Transaction, op: &Op) -> fstx::Result<Option<Vec<u8>>> {
    match op {
        Op::Write(p, b) => tx.write(p, [*b; 3]).map(|_| None),
        Op::Mkdirs(p) => tx.create_dir_all(p).map(|_| None),
        Op::Rename(f, t) => tx.rename(f, t).map(|_| None),
        Op::Remove(p) => tx.remove(p).map(|_| None),
        Op::RemoveAll(p) => tx.remove_dir_all(p).map(|_| None),
        Op::Read(p) => tx.read(p).map(Some),
    }
}

fn base() -> (SimFs, Model) {
    let fs = SimFs::new(SimConfig::default());
    fs.put_dir("a");
    fs.put_file("a/b", b"ab");
    fs.put_dir("a/c");
    fs.put_file("a/c/a", b"aca");
    fs.put_file("b", b"b");
    fs.put_hardlink("b", "a/l");
    fs.put_dir("c");
    let mut m = Model::new();
    for (path, e) in fs.tree() {
        let node = match e.kind {
            Kind::Dir => Node::Dir { tag: Some(e.ino) },
            _ => Node::File {
                data: e.data,
                tag: Some(e.ino),
            },
        };
        m.insert(path, node);
    }
    (fs, m)
}

fn path() -> impl Strategy<Value = String> {
    proptest::collection::vec(prop_oneof!["a", "b", "c", "l"], 1..=3).prop_map(|v| v.join("/"))
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (path(), any::<u8>()).prop_map(|(p, b)| Op::Write(p, b)),
        1 => path().prop_map(Op::Mkdirs),
        4 => (path(), path()).prop_map(|(f, t)| Op::Rename(f, t)),
        2 => path().prop_map(Op::Remove),
        1 => path().prop_map(Op::RemoveAll),
        2 => path().prop_map(Op::Read),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn fstx_matches_identity_model(ops in proptest::collection::vec(op(), 1..10)) {
        let (fs, mut model) = base();
        let base_inos: BTreeSet<u64> = fs.tree().values().map(|e| e.ino).collect();
        let mut tx = Transaction::begin_on(Box::new(fs.clone()), &Options::new()).unwrap();
        for (i, op) in ops.iter().enumerate() {
            let want = apply(&mut model, op);
            let got = run(&mut tx, op);
            prop_assert_eq!(want.is_ok(), got.is_ok(), "op {} {:?}: model {:?} fstx {:?}", i, op, want, got);
            if let (Ok(Some(w)), Ok(Some(g))) = (&want, &got) {
                prop_assert_eq!(w, g);
            }
        }
        tx.commit().unwrap();
        let tree = fs.tree();
        prop_assert_eq!(
            tree.keys().collect::<Vec<_>>(),
            model.keys().collect::<Vec<_>>(),
            "paths differ after {:?}", ops
        );
        for (p, node) in &model {
            let e = &tree[p];
            match node {
                Node::Dir { tag } => {
                    prop_assert_eq!(e.kind, Kind::Dir);
                    match tag {
                        Some(t) => prop_assert_eq!(e.ino, *t, "dir identity at {}", p),
                        None => prop_assert!(!base_inos.contains(&e.ino)),
                    }
                }
                Node::File { data, tag } => {
                    prop_assert_eq!(e.kind, Kind::File);
                    prop_assert_eq!(&e.data, data, "content at {}", p);
                    match tag {
                        Some(t) => prop_assert_eq!(e.ino, *t, "file identity at {}", p),
                        None => prop_assert!(!base_inos.contains(&e.ino), "new file at {} reuses a base inode", p),
                    }
                }
            }
        }
        prop_assert!(fs.private_leftovers().is_empty());
        prop_assert!(fs.violations().is_empty());
    }
}
