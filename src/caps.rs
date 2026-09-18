//! Capability probing (DESIGN.md §5). Support is decided by testing the required
//! semantics on the root's own filesystem, never by filesystem name. Probes cannot test
//! crash durability; that remains a documented assumption.

use std::collections::HashMap;
use std::io;
use std::sync::{Mutex, OnceLock};

use crate::error::{Error, Result};
use crate::path::RelPath;
use crate::state::{delete_tree, private_dir};
use crate::vfs::{FileId, Vfs};

/// What the probe learned about the filesystem.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    /// Name lookups ignore ASCII case (fstx then rejects colliding names).
    pub case_insensitive: bool,
}

fn cache() -> &'static Mutex<HashMap<FileId, Caps>> {
    static CACHE: OnceLock<Mutex<HashMap<FileId, Caps>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Probes the filesystem under `.fstx/` (which must exist and be locked).
pub(crate) fn probe(vfs: &dyn Vfs, allow_untested_fs: bool) -> Result<Caps> {
    if vfs.untrusted_fs_type()?.is_some() && !allow_untested_fs {
        return Err(Error::UnsupportedFilesystem {
            missing: vec![
                "a local filesystem (network/FUSE filesystem detected; see Options::allow_untested_fs)",
            ],
        });
    }
    let key = vfs.probe_cache_key();
    if let Some(k) = key
        && let Some(c) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(&k)
    {
        return Ok(*c);
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = private_dir().join(format!("probe-{}-{nonce:x}", std::process::id()));
    vfs.mkdir(&dir)?;
    let result = run_probe(vfs, &dir);
    let _ = delete_tree(vfs, &dir);
    let caps = result?;
    if let Some(k) = key {
        cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(k, caps);
    }
    Ok(caps)
}

/// EINVAL/EOPNOTSUPP/ENOSYS mean "not supported"; any other error (EIO, ENOSPC, ...) is a
/// real failure and must not be mistaken for a missing capability.
fn unsupported(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput
    )
}

fn run_probe(vfs: &dyn Vfs, dir: &RelPath) -> Result<Caps> {
    let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
    vfs.create_file(&a, b"a", None)?;
    vfs.create_file(&b, b"b", None)?;
    let mut missing = Vec::new();
    match vfs.rename_noreplace(&a, None, &b, None) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Ok(()) => missing.push("no-replace rename (an existing destination was replaced)"),
        Err(e) if unsupported(&e) => missing.push("no-replace rename (RENAME_NOREPLACE)"),
        Err(e) => return Err(e.into()),
    }
    if missing.is_empty() {
        let before = vfs.stat(&a)?.map(|m| m.id);
        match vfs.rename_noreplace(&a, None, &c, None) {
            Ok(()) => {
                if before.is_none() || vfs.stat(&c)?.map(|m| m.id) != before {
                    missing.push("file identities that are stable across rename");
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    match vfs.sync_dir(dir) {
        Ok(()) => {}
        Err(e) if unsupported(&e) => missing.push("directory fsync"),
        Err(e) => return Err(e.into()),
    }
    if !missing.is_empty() {
        return Err(Error::UnsupportedFilesystem { missing });
    }
    vfs.create_file(&dir.join("CaseProbe"), b"", None)?;
    let case_insensitive = vfs.stat(&dir.join("caseprobe"))?.is_some();
    Ok(Caps { case_insensitive })
}
