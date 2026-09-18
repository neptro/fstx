//! Path types. `RelPath` is any path relative to the transaction root, as OS-native
//! components. `validate_user_path` turns a caller-supplied path into a `RelPath` that
//! can never escape the root or touch the private `.fstx` area.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// Name of the private bookkeeping directory inside the root.
pub(crate) const PRIVATE_DIR: &str = ".fstx";

/// A path relative to the root: a list of single, non-empty OS-native components.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RelPath(Vec<OsString>);

impl RelPath {
    pub fn root() -> Self {
        RelPath(Vec::new())
    }

    /// Builds a path from trusted internal components (no validation beyond debug checks).
    pub(crate) fn from_parts<I, S>(parts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let v: Vec<OsString> = parts.into_iter().map(Into::into).collect();
        debug_assert!(v.iter().all(|c| component_ok(c)));
        RelPath(v)
    }

    pub fn components(&self) -> &[OsString] {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn depth(&self) -> usize {
        self.0.len()
    }

    pub fn parent(&self) -> Option<RelPath> {
        if self.0.is_empty() {
            None
        } else {
            Some(RelPath(self.0[..self.0.len() - 1].to_vec()))
        }
    }

    pub fn file_name(&self) -> Option<&OsStr> {
        self.0.last().map(|s| s.as_os_str())
    }

    pub fn join(&self, name: impl Into<OsString>) -> RelPath {
        let mut v = self.0.clone();
        v.push(name.into());
        RelPath(v)
    }

    pub fn join_path(&self, rest: &RelPath) -> RelPath {
        let mut v = self.0.clone();
        v.extend(rest.0.iter().cloned());
        RelPath(v)
    }

    /// True if `self` is a strict ancestor of `other`.
    pub fn is_ancestor_of(&self, other: &RelPath) -> bool {
        self.0.len() < other.0.len() && other.0[..self.0.len()] == self.0[..]
    }

    /// True if `self == other` or one is an ancestor of the other.
    pub fn overlaps(&self, other: &RelPath) -> bool {
        self == other || self.is_ancestor_of(other) || other.is_ancestor_of(self)
    }

    /// If `self` starts with `prefix`, the remaining components.
    pub fn strip_prefix(&self, prefix: &RelPath) -> Option<RelPath> {
        if prefix.0.len() <= self.0.len() && self.0[..prefix.0.len()] == prefix.0[..] {
            Some(RelPath(self.0[prefix.0.len()..].to_vec()))
        } else {
            None
        }
    }

    /// All proper, non-root prefixes from shortest to longest.
    pub fn proper_prefixes(&self) -> impl Iterator<Item = RelPath> + '_ {
        (1..self.0.len()).map(|n| RelPath(self.0[..n].to_vec()))
    }

    pub fn to_path_buf(&self) -> PathBuf {
        self.0.iter().collect()
    }

    /// True if the first component is the private directory (case-insensitively).
    pub(crate) fn is_private(&self) -> bool {
        self.0.first().is_some_and(|c| {
            c.to_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(PRIVATE_DIR))
        })
    }
}

impl fmt::Debug for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.to_path_buf())
    }
}

fn component_ok(c: &OsStr) -> bool {
    let b = os_bytes(c);
    !b.is_empty() && b != b"." && b != b".." && !b.contains(&0) && !b.contains(&b'/')
}

/// Validates a caller-supplied path. Rejects: empty, absolute/prefixed, `..`, NUL bytes,
/// and anything whose first component is `.fstx` (any ASCII case). `.` components are dropped.
pub(crate) fn validate_user_path(p: &Path) -> Result<RelPath> {
    let bad = |reason| Error::InvalidPath {
        path: p.to_path_buf(),
        reason,
    };
    let mut parts = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => {
                if os_bytes(s).contains(&0) {
                    return Err(bad("contains a NUL byte"));
                }
                #[cfg(windows)]
                if s.to_str().is_none() {
                    return Err(bad("not valid Unicode"));
                }
                parts.push(s.to_os_string());
            }
            Component::CurDir => {}
            Component::ParentDir => return Err(bad("contains `..`")),
            Component::RootDir | Component::Prefix(_) => return Err(bad("must be relative")),
        }
    }
    if parts.is_empty() {
        return Err(bad("empty path"));
    }
    let rel = RelPath(parts);
    if rel.is_private() {
        return Err(bad("the `.fstx` directory is reserved"));
    }
    Ok(rel)
}

/// OS-native bytes of a component (raw bytes on Unix, UTF-8 elsewhere).
pub(crate) fn os_bytes(s: &OsStr) -> &[u8] {
    #[cfg(unix)]
    {
        std::os::unix::ffi::OsStrExt::as_bytes(s)
    }
    #[cfg(not(unix))]
    {
        s.as_encoded_bytes()
    }
}

/// Inverse of [`os_bytes`]. Returns `None` for bytes that are not a valid component.
pub(crate) fn os_from_bytes(b: Vec<u8>) -> Option<OsString> {
    #[cfg(unix)]
    let s: OsString = std::os::unix::ffi::OsStringExt::from_vec(b);
    #[cfg(not(unix))]
    let s = OsString::from(String::from_utf8(b).ok()?);
    component_ok(&s).then_some(s)
}

// Serialized as a list of hex-encoded components so non-UTF-8 names survive JSON.
impl serde::Serialize for RelPath {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter().map(|c| hex(os_bytes(c))))
    }
}

impl<'de> serde::Deserialize<'de> for RelPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let raw: Vec<String> = serde::Deserialize::deserialize(d)?;
        let mut v = Vec::with_capacity(raw.len());
        for h in raw {
            let bytes = unhex(&h).ok_or_else(|| serde::de::Error::custom("bad hex component"))?;
            v.push(os_from_bytes(bytes).ok_or_else(|| serde::de::Error::custom("bad component"))?);
        }
        Ok(RelPath(v))
    }
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escapes_and_reserved() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../b",
            "a/..",
            ".fstx",
            ".FSTX/x",
            ".",
            "./",
        ] {
            assert!(
                validate_user_path(Path::new(bad)).is_err(),
                "{bad:?} accepted"
            );
        }
    }

    #[test]
    fn accepts_normal_paths() {
        let p = validate_user_path(Path::new("./a/./b.txt")).unwrap();
        assert_eq!(p.components().len(), 2);
        assert!(validate_user_path(Path::new(".fstxfoo")).is_ok());
        assert!(validate_user_path(Path::new("a/.fstx")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_roundtrip() {
        use std::os::unix::ffi::OsStrExt;
        let p = validate_user_path(Path::new(OsStr::from_bytes(b"a/\xff\xfe"))).unwrap();
        let json = serde_json::to_string(&p).unwrap();
        let back: RelPath = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn serde_rejects_escape_components() {
        for bad in [
            r#"["2e2e"]"#,
            r#"["2f"]"#,
            r#"[""]"#,
            r#"["00"]"#,
            r#"["zz"]"#,
        ] {
            assert!(
                serde_json::from_str::<RelPath>(bad).is_err(),
                "{bad} accepted"
            );
        }
    }
}
