//! The value a commit is compared against (ADR-016 §3).

use std::fs::Metadata;
use std::hash::{Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// A fingerprint of one file's raw bytes and identity: `sha256`, size, mtime
/// and inode.
///
/// The hash is of the raw bytes — never of newline- or Unicode-normalized
/// text. Size, mtime and inode are checked alongside it so a swap that happens
/// to produce identical bytes is still visible as a change of file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub sha256: [u8; 32],
    pub size: u64,
    /// Modification time, nanoseconds since the Unix epoch. `i64` because a
    /// filesystem may legitimately hold a pre-epoch timestamp.
    pub mtime_ns: i64,
    pub inode: u64,
}

impl Fingerprint {
    /// Hash `bytes` and take size, mtime and inode from `meta`, which must
    /// describe the file the bytes came from.
    pub fn of(bytes: &[u8], meta: &Metadata) -> Self {
        let mut sha = Sha256::new();
        sha.update(bytes);
        Self {
            sha256: sha.finalize().into(),
            size: meta.len(),
            mtime_ns: mtime_ns(meta),
            inode: inode_of(meta),
        }
    }

    /// The hash of raw bytes with no file behind it yet — what a create or an
    /// update promises to produce.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut sha = Sha256::new();
        sha.update(bytes);
        Self {
            sha256: sha.finalize().into(),
            size: bytes.len() as u64,
            mtime_ns: 0,
            inode: 0,
        }
    }

    /// Lowercase hex of the digest, the spelling used in the journal and the
    /// operation log.
    pub fn hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for b in self.sha256 {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
        }
        out
    }

    /// Compare digests only: mtime, size and inode are identity, and a
    /// classification step (journal recovery, undo verification) asks "are
    /// these the same bytes".
    pub fn same_bytes(&self, other: &Self) -> bool {
        self.sha256 == other.sha256
    }

    /// Full comparison, for a commit precondition.
    pub fn matches(&self, other: &Self) -> bool {
        self.sha256 == other.sha256
            && self.size == other.size
            && self.mtime_ns == other.mtime_ns
            && self.inode == other.inode
    }
}

/// Hashes and compares by digest, so a fingerprint can key a map of "the file
/// I already read".
impl Hash for Fingerprint {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write(&self.sha256);
    }
}

#[cfg(unix)]
fn inode_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn inode_of(_meta: &Metadata) -> u64 {
    0
}

fn mtime_ns(meta: &Metadata) -> i64 {
    let Ok(stamp) = meta.modified() else {
        return 0;
    };
    match stamp.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MIN),
    }
}

/// The timestamp used in the journal and the operation log.
pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// RFC 3339 UTC of `now_ns`-style nanoseconds, for the operation log.
pub fn iso_utc(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let nanos = ns.rem_euclid(1_000_000_000) as u32;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_and_iso_utc() {
        let ns = now_ns();
        assert!(ns > 0);
        let s = iso_utc(ns);
        assert!(!s.is_empty());
    }

    #[test]
    fn hex_is_lowercase_and_stable() {
        let f = Fingerprint::of_bytes(b"abc");
        assert_eq!(
            f.hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(f.size, 3);
    }

    #[test]
    fn same_bytes_ignores_identity() {
        let a = Fingerprint::of_bytes(b"abc");
        let b = Fingerprint {
            size: 9,
            mtime_ns: 9,
            inode: 9,
            ..a
        };
        assert!(a.same_bytes(&b));
        assert!(!a.matches(&b));
    }
}
