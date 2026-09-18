//! The progress log: fixed-size, checksummed records appended after each wave barrier.
//! It is only a lower bound and a consistency check; observation of the tree is
//! authoritative. Torn or corrupt regions are skipped.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    ApplyDone(u32),
    UndoDone(u32),
}

const MAGIC: &[u8; 4] = b"FXPR";
pub(crate) const LEN: usize = 16;

fn check(head: &[u8]) -> [u8; 4] {
    let h = blake3::hash(head);
    [
        h.as_bytes()[0],
        h.as_bytes()[1],
        h.as_bytes()[2],
        h.as_bytes()[3],
    ]
}

pub(crate) fn encode(r: Record) -> [u8; LEN] {
    let (kind, wave) = match r {
        Record::ApplyDone(w) => (1u8, w),
        Record::UndoDone(w) => (2u8, w),
    };
    let mut out = [0u8; LEN];
    out[..4].copy_from_slice(MAGIC);
    out[4] = kind;
    out[8..12].copy_from_slice(&wave.to_le_bytes());
    let c = check(&out[..12]);
    out[12..].copy_from_slice(&c);
    out
}

/// Returns every valid record. Corrupt or torn regions (e.g. an unsynced tail cut by a
/// power loss, followed by records appended by a later recovery) are skipped by scanning
/// forward byte by byte, so later records are never hidden behind garbage. Only the set of
/// records matters (recovery uses their min/max), not their order.
pub(crate) fn parse(bytes: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + LEN <= bytes.len() {
        let rec = &bytes[i..i + LEN];
        let valid = &rec[..4] == MAGIC && rec[5..8] == [0, 0, 0] && rec[12..] == check(&rec[..12]);
        let wave = u32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]]);
        match (valid, rec[4]) {
            (true, 1) => out.push(Record::ApplyDone(wave)),
            (true, 2) => out.push(Record::UndoDone(wave)),
            _ => {
                i += 1;
                continue;
            }
        }
        i += LEN;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn roundtrip_and_torn_tail() {
        let mut b = Vec::new();
        b.extend_from_slice(&encode(Record::ApplyDone(0)));
        b.extend_from_slice(&encode(Record::UndoDone(7)));
        b.extend_from_slice(&encode(Record::ApplyDone(3))[..9]);
        assert_eq!(parse(&b), vec![Record::ApplyDone(0), Record::UndoDone(7)]);
        // A record appended after a torn tail is still found.
        b.extend_from_slice(&encode(Record::UndoDone(2)));
        assert_eq!(
            parse(&b),
            vec![
                Record::ApplyDone(0),
                Record::UndoDone(7),
                Record::UndoDone(2)
            ]
        );
        b[20] ^= 0xff;
        assert_eq!(parse(&b), vec![Record::ApplyDone(0), Record::UndoDone(2)]);
    }

    proptest! {
        #[test]
        fn never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = parse(&bytes);
        }
    }
}
