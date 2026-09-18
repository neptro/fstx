//! The journal: the full physical plan, written before any tree mutation and published
//! by an atomic rename (`journal.tmp` → `journal`). It is treated as untrusted input on
//! recovery: checksum (integrity only, not authenticity), path re-validation, and the
//! structural plan checks all run before recovery touches anything.

use serde::{Deserialize, Serialize};

use crate::path::{PRIVATE_DIR, hex};
use crate::plan::{Plan, validate};
use crate::vfs::FileId;

pub(crate) const FORMAT: u32 = 1;
const SEP: &[u8] = b"\nblake3:";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Journal {
    pub format: u32,
    pub tx: String,
    pub root: FileId,
    #[serde(flatten)]
    pub plan: Plan,
}

pub(crate) fn encode(j: &Journal) -> Vec<u8> {
    let mut out = serde_json::to_vec(j).expect("journal serializes");
    let sum = hex(blake3::hash(&out).as_bytes());
    out.extend_from_slice(SEP);
    out.extend_from_slice(sum.as_bytes());
    out.push(b'\n');
    out
}

/// Parses and validates a journal read from the directory `.fstx/<tx_name>`.
pub(crate) fn decode(bytes: &[u8], tx_name: &str, root: FileId) -> Result<Journal, String> {
    let cut = bytes
        .windows(SEP.len())
        .rposition(|w| w == SEP)
        .ok_or("journal has no checksum")?;
    let (body, rest) = bytes.split_at(cut);
    let sum = rest[SEP.len()..]
        .strip_suffix(b"\n")
        .ok_or("journal checksum line is torn")?;
    if sum != hex(blake3::hash(body).as_bytes()).as_bytes() {
        return Err("journal checksum mismatch".into());
    }
    let j: Journal =
        serde_json::from_slice(body).map_err(|e| format!("journal does not parse: {e}"))?;
    if j.format != FORMAT {
        return Err(format!("unknown journal format {}", j.format));
    }
    if j.tx != tx_name {
        return Err("journal belongs to a different transaction".into());
    }
    if j.root != root {
        return Err("journal belongs to a different root directory".into());
    }
    for tok in &j.plan.tokens {
        if tok.locs.len() < 2 {
            return Err("token with fewer than two locations".into());
        }
        for loc in &tok.locs {
            let c = loc.path.components();
            if loc.path.is_root() {
                return Err("location is the root".into());
            }
            if loc.path.is_private() {
                let ok = c.len() == 4
                    && c[0] == PRIVATE_DIR
                    && c[1] == tx_name
                    && (c[2] == "backup" || c[2] == "staged");
                if !ok {
                    return Err(format!(
                        "private location {:?} outside this transaction",
                        loc.path
                    ));
                }
            }
        }
    }
    validate(&j.plan)?;
    Ok(j)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::RelPath;
    use crate::plan::{Loc, Step, Token};
    use crate::vfs::Kind;
    use proptest::prelude::*;

    fn fid(i: u64) -> FileId {
        FileId { dev: 1, ino: i }
    }

    fn sample() -> Journal {
        Journal {
            format: FORMAT,
            tx: "tx-1".into(),
            root: fid(1),
            plan: Plan {
                tokens: vec![Token {
                    kind: Kind::File,
                    id: fid(5),
                    locs: vec![
                        Loc {
                            path: RelPath::from_parts([".fstx", "tx-1", "staged", "b0"]),
                            parent: fid(3),
                        },
                        Loc {
                            path: RelPath::from_parts(["a"]),
                            parent: fid(1),
                        },
                    ],
                }],
                waves: vec![vec![Step { token: 0, from: 0 }]],
            },
        }
    }

    #[test]
    fn roundtrip() {
        let j = sample();
        assert_eq!(decode(&encode(&j), "tx-1", fid(1)), Ok(j));
    }

    #[test]
    fn rejects_corruption_and_foreign_paths() {
        let bytes = encode(&sample());
        let mut flipped = bytes.clone();
        flipped[10] ^= 1;
        assert!(decode(&flipped, "tx-1", fid(1)).is_err());
        assert!(decode(&bytes[..bytes.len() - 3], "tx-1", fid(1)).is_err());
        assert!(decode(&bytes, "tx-2", fid(1)).is_err());
        assert!(decode(&bytes, "tx-1", fid(2)).is_err());

        let mut j = sample();
        j.plan.tokens[0].locs[1].path = RelPath::from_parts([".fstx", "lock"]);
        assert!(decode(&encode(&j), "tx-1", fid(1)).is_err());
        let mut j = sample();
        j.plan.tokens[0].locs[0].path = RelPath::from_parts([".fstx", "tx-9", "staged", "b0"]);
        assert!(decode(&encode(&j), "tx-1", fid(1)).is_err());
    }

    #[test]
    fn rejects_escaping_component_even_with_valid_checksum() {
        let body = String::from_utf8(encode(&sample())).unwrap();
        let body = body
            .split("\nblake3:")
            .next()
            .unwrap()
            .replace("\"61\"", "\"2e2e\"");
        let mut bytes = body.clone().into_bytes();
        bytes.extend_from_slice(SEP);
        bytes.extend_from_slice(hex(blake3::hash(body.as_bytes()).as_bytes()).as_bytes());
        bytes.push(b'\n');
        assert!(decode(&bytes, "tx-1", fid(1)).is_err());
    }

    proptest! {
        #[test]
        fn never_panics_on_garbage(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = decode(&bytes, "tx-1", fid(1));
        }

        #[test]
        fn never_panics_on_mutations(pos in 0usize..400, byte in any::<u8>()) {
            let mut bytes = encode(&sample());
            let i = pos % bytes.len();
            bytes[i] = byte;
            let _ = decode(&bytes, "tx-1", fid(1));
        }
    }
}
