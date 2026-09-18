#![cfg(target_os = "linux")]
//! Confinement on the real Linux backend: no operation may follow a symlink or leave the root.

mod common;

use std::fs;
use std::os::unix::fs::symlink;

use common::scratch;
use fstx::{Error, Transaction};

#[test]
fn lexically_invalid_paths_are_rejected() {
    let d = scratch();
    let mut tx = Transaction::begin(d.path()).unwrap();
    for bad in ["", "/etc/passwd", "../escape", "a/../../b", ".fstx/lock", ".FSTX/x"] {
        assert!(matches!(tx.write(bad, "x"), Err(Error::InvalidPath { .. })), "{bad:?}");
        assert!(matches!(tx.rename("a", bad), Err(Error::InvalidPath { .. })), "{bad:?}");
    }
}

#[test]
fn symlinked_directory_component_is_never_followed() {
    let d = scratch();
    let outside = scratch();
    symlink(outside.path(), d.path().join("link")).unwrap();
    let mut tx = Transaction::begin(d.path()).unwrap();
    assert!(tx.write("link/pwned", "x").is_err());
    assert!(tx.create_dir_all("link/sub").is_err());
    assert!(tx.read("link/anything").is_err());
    tx.commit().unwrap();
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn symlink_targets_are_refused() {
    let d = scratch();
    let outside = scratch();
    fs::write(outside.path().join("secret"), "s").unwrap();
    symlink(outside.path().join("secret"), d.path().join("sym")).unwrap();
    let mut tx = Transaction::begin(d.path()).unwrap();
    assert!(matches!(tx.write("sym", "x"), Err(Error::UnsupportedFileType(_))));
    assert!(matches!(tx.remove("sym"), Err(Error::UnsupportedFileType(_))));
    assert!(matches!(tx.rename("sym", "t"), Err(Error::UnsupportedFileType(_))));
    assert!(matches!(tx.read("sym"), Err(Error::UnsupportedFileType(_))));
    drop(tx);
    assert_eq!(fs::read_to_string(outside.path().join("secret")).unwrap(), "s");
}

#[test]
fn removing_a_tree_with_symlinks_never_touches_their_targets() {
    let d = scratch();
    let outside = scratch();
    fs::write(outside.path().join("keep"), "k").unwrap();
    fs::create_dir(d.path().join("t")).unwrap();
    symlink(outside.path(), d.path().join("t/dirlink")).unwrap();
    symlink(outside.path().join("keep"), d.path().join("t/filelink")).unwrap();
    let mut tx = Transaction::begin(d.path()).unwrap();
    tx.remove_dir_all("t").unwrap();
    tx.commit().unwrap();
    assert!(!d.path().join("t").exists());
    assert_eq!(fs::read_to_string(outside.path().join("keep")).unwrap(), "k");
}

#[test]
fn private_dir_replaced_by_symlink_is_refused() {
    let d = scratch();
    let outside = scratch();
    symlink(outside.path(), d.path().join(".fstx")).unwrap();
    assert!(Transaction::begin(d.path()).is_err());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}
