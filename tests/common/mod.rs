#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// A scratch root on the build filesystem (not tmpfs), so fsync does real work.
pub fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("fstx-test-").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// path -> (is_dir, content, inode), excluding `.fstx`.
pub fn snapshot(root: &Path) -> BTreeMap<String, (bool, Vec<u8>, u64)> {
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (bool, Vec<u8>, u64)>) {
    for e in fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
        if rel == ".fstx" {
            continue;
        }
        let m = fs::symlink_metadata(&p).unwrap();
        if m.is_dir() {
            out.insert(rel, (true, Vec::new(), m.ino()));
            walk(root, &p, out);
        } else {
            out.insert(rel, (false, fs::read(&p).unwrap_or_default(), m.ino()));
        }
    }
}

/// Entries under `.fstx` other than the lock file.
pub fn leftovers(root: &Path) -> Vec<String> {
    match fs::read_dir(root.join(".fstx")) {
        Ok(rd) => rd
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "lock")
            .collect(),
        Err(_) => Vec::new(),
    }
}
