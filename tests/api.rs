#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use common::{leftovers, scratch, snapshot};
use fstx::{Error, Transaction};

#[test]
fn commit_makes_all_changes_visible_at_once() {
    let d = scratch();
    let root = d.path();
    fs::write(root.join("old.rs"), "old").unwrap();
    fs::write(root.join("tmp.log"), "log").unwrap();

    let mut tx = Transaction::begin(root).unwrap();
    tx.write("config.toml", "v = 2").unwrap();
    tx.create_dir_all("src/gen").unwrap();
    tx.rename("old.rs", "src/gen/new.rs").unwrap();
    tx.remove("tmp.log").unwrap();
    // Nothing visible before commit.
    assert!(!root.join("config.toml").exists());
    assert!(root.join("tmp.log").exists());
    tx.commit().unwrap();

    assert_eq!(fs::read_to_string(root.join("config.toml")).unwrap(), "v = 2");
    assert_eq!(fs::read_to_string(root.join("src/gen/new.rs")).unwrap(), "old");
    assert!(!root.join("old.rs").exists());
    assert!(!root.join("tmp.log").exists());
    assert!(leftovers(root).is_empty(), "{:?}", leftovers(root));
}

#[test]
fn drop_discards_everything() {
    let d = scratch();
    let root = d.path();
    fs::write(root.join("a"), "a").unwrap();
    let before = snapshot(root);
    {
        let mut tx = Transaction::begin(root).unwrap();
        tx.write("a", "changed").unwrap();
        tx.write("b", "new").unwrap();
        tx.remove("a").unwrap();
    }
    assert_eq!(snapshot(root), before);
    assert!(leftovers(root).is_empty());
}

#[test]
fn read_your_writes() {
    let d = scratch();
    let root = d.path();
    fs::write(root.join("a"), "base").unwrap();
    let mut tx = Transaction::begin(root).unwrap();
    assert_eq!(tx.read("a").unwrap(), b"base");
    tx.write("a", "staged").unwrap();
    assert_eq!(tx.read("a").unwrap(), b"staged");
    tx.rename("a", "b").unwrap();
    assert!(matches!(tx.read("a"), Err(Error::NotFound(_))));
    assert_eq!(tx.read("b").unwrap(), b"staged");
}

#[test]
fn directory_move_is_one_rename_and_keeps_identity() {
    let d = scratch();
    let root = d.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/f"), "f").unwrap();
    let ino_a = fs::metadata(root.join("a")).unwrap().ino();
    let ino_f = fs::metadata(root.join("a/b/f")).unwrap().ino();

    let mut tx = Transaction::begin(root).unwrap();
    tx.rename("a", "z").unwrap();
    tx.write("z/b/g", "g").unwrap();
    assert_eq!(tx.read("z/b/f").unwrap(), b"f");
    tx.commit().unwrap();

    assert_eq!(fs::metadata(root.join("z")).unwrap().ino(), ino_a);
    assert_eq!(fs::metadata(root.join("z/b/f")).unwrap().ino(), ino_f);
    assert_eq!(fs::read_to_string(root.join("z/b/g")).unwrap(), "g");
    assert!(!root.join("a").exists());
}

#[test]
fn swapping_identical_files_swaps_identities() {
    let d = scratch();
    let root = d.path();
    fs::write(root.join("a"), "same").unwrap();
    fs::write(root.join("b"), "same").unwrap();
    let (ia, ib) = (fs::metadata(root.join("a")).unwrap().ino(), fs::metadata(root.join("b")).unwrap().ino());
    let mut tx = Transaction::begin(root).unwrap();
    tx.rename("a", "tmp").unwrap();
    tx.rename("b", "a").unwrap();
    tx.rename("tmp", "b").unwrap();
    tx.commit().unwrap();
    assert_eq!(fs::metadata(root.join("a")).unwrap().ino(), ib);
    assert_eq!(fs::metadata(root.join("b")).unwrap().ino(), ia);
}

#[test]
fn round_trip_rename_is_a_no_op() {
    let d = scratch();
    let root = d.path();
    fs::write(root.join("a"), "a").unwrap();
    let before = snapshot(root);
    let mut tx = Transaction::begin(root).unwrap();
    tx.rename("a", "t").unwrap();
    tx.rename("t", "a").unwrap();
    tx.commit().unwrap();
    assert_eq!(snapshot(root), before);
}

#[test]
fn hard_links_elsewhere_keep_their_content() {
    let d = scratch();
    let root = d.path();
    fs::write(root.join("a"), "v1").unwrap();
    fs::hard_link(root.join("a"), root.join("link")).unwrap();
    let mut tx = Transaction::begin(root).unwrap();
    tx.write("a", "v2").unwrap();
    tx.commit().unwrap();
    assert_eq!(fs::read_to_string(root.join("a")).unwrap(), "v2");
    assert_eq!(fs::read_to_string(root.join("link")).unwrap(), "v1");
}

#[test]
fn replaced_file_keeps_its_permissions() {
    let d = scratch();
    let root = d.path();
    fs::write(root.join("s.sh"), "old").unwrap();
    fs::set_permissions(root.join("s.sh"), fs::Permissions::from_mode(0o750)).unwrap();
    let mut tx = Transaction::begin(root).unwrap();
    tx.write("s.sh", "new").unwrap();
    tx.commit().unwrap();
    assert_eq!(fs::metadata(root.join("s.sh")).unwrap().permissions().mode() & 0o7777, 0o750);
}

#[test]
fn remove_dir_all_and_errors() {
    let d = scratch();
    let root = d.path();
    fs::create_dir_all(root.join("d/e")).unwrap();
    fs::write(root.join("d/e/f"), "f").unwrap();
    fs::write(root.join("file"), "x").unwrap();
    let mut tx = Transaction::begin(root).unwrap();
    assert!(matches!(tx.remove("d"), Err(Error::DirectoryNotEmpty(_))));
    assert!(matches!(tx.rename("d", "file"), Err(Error::AlreadyExists(_))));
    assert!(matches!(tx.rename("d", "d/e/x"), Err(Error::InvalidMove { .. })));
    assert!(matches!(tx.write("file/x", "y"), Err(Error::NotADirectory(_))));
    assert!(matches!(tx.write("d", "y"), Err(Error::IsADirectory(_))));
    assert!(matches!(tx.write(".fstx/x", "y"), Err(Error::InvalidPath { .. })));
    tx.remove_dir_all("d").unwrap();
    tx.commit().unwrap();
    assert!(!root.join("d").exists());
    assert!(leftovers(root).is_empty());
}

#[test]
fn empty_commit_and_inspect() {
    let d = scratch();
    let root = d.path();
    Transaction::begin(root).unwrap().commit().unwrap();
    let insp = fstx::inspect(root).unwrap();
    assert!(insp.transactions.is_empty());
    assert!(fstx::recover(root).unwrap().rolled_back.is_empty());
}

#[test]
fn non_utf8_names() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let d = scratch();
    let root = d.path();
    let name = OsStr::from_bytes(b"caf\xe9");
    let mut tx = Transaction::begin(root).unwrap();
    tx.write(name, "x").unwrap();
    tx.commit().unwrap();
    assert_eq!(fs::read(root.join(name)).unwrap(), b"x");
}
