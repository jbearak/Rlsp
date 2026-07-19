//! Tier 3 encoding: a compact, memory-mapped, lazily-decoded `names.db`.
//!
//! Layout (all multibyte integers little-endian):
//!
//! ```text
//! MAGIC        8 bytes  = b"RAVNDB\0\0"
//! version      u32      = FORMAT_VERSION
//! header_len   u32      = byte length of the postcard-encoded header
//! header       header_len bytes (postcard ShippedDbHeader: provenance + checksum + index)
//! payload      rest of file (postcard PackageRecord per package, concatenated)
//! ```
//!
//! The header is decoded **once at open**; per-package payloads stay in the mmap
//! and are postcard-decoded **lazily** on lookup, off the LSP hot path. Integrity:
//! a `blake3` hash of the payload region is stored in the header and verified at
//! open; the index→record mapping is additionally bound by checking that each
//! decoded record's own `name` matches the index key it was reached through (see
//! `decode_at`), so a tampered/corrupt index can't silently remap a name onto a
//! different payload.

use std::collections::HashMap;
use std::path::Path;

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::package_db::PackageMetadataProvider;
use crate::package_db::model::PackageRecord;
use crate::package_library::PackageInfo;

const MAGIC: &[u8; 8] = b"RAVNDB\0\0";
const FORMAT_VERSION: u32 = 1;

/// Typed error so a present-but-unusable `names.db` (stale `RAVEN_NAMES_DB`, a
/// newer-format seed file, a truncated download) is explained, not silently
/// dropped (decision #9).
#[derive(Debug)]
pub enum ShippedDbError {
    /// No file present — normal, callers stay silent.
    Absent,
    /// File present but a different container format version.
    UnsupportedFormat { found: u32, supported: u32 },
    /// Bad magic, header decode failure, checksum mismatch, or truncation.
    Corrupt(String),
    /// The file was replaced or modified during both load attempts.
    ConcurrentModification,
    /// The provider generation's original routing deadline expired.
    DeadlineExpired,
}

impl std::fmt::Display for ShippedDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShippedDbError::Absent => write!(f, "no names.db present"),
            ShippedDbError::UnsupportedFormat { found, supported } => write!(
                f,
                "names.db has container format v{found}; this Raven understands v{supported}. \
                 The bundled database is incompatible with this build, so its package metadata \
                 is unavailable. Upgrade Raven to match the bundled database."
            ),
            ShippedDbError::Corrupt(d) => write!(f, "names.db is unreadable: {d}"),
            ShippedDbError::ConcurrentModification => write!(
                f,
                "names.db changed while Raven was reading it; retry the operation"
            ),
            ShippedDbError::DeadlineExpired => {
                write!(f, "names.db load exceeded the routing deadline")
            }
        }
    }
}

/// Provenance + integrity for the shipped DB (spec §8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShippedDbProvenance {
    pub source: String,
    pub snapshot_date: String,
    pub package_count: u32,
    pub raven_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    name: String,
    /// Offset of this package's payload, relative to the start of the payload region.
    offset: u64,
    len: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShippedDbHeader {
    provenance: ShippedDbProvenance,
    /// blake3 hex of the payload region.
    payload_checksum: String,
    index: Vec<IndexEntry>,
}

/// Write `records` (need not be pre-sorted) to a Tier 3 container at `path`.
pub fn write_shipped_db(
    path: &Path,
    records: &[PackageRecord],
    provenance: ShippedDbProvenance,
) -> anyhow::Result<()> {
    let mut sorted: Vec<&PackageRecord> = records.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    let mut payload: Vec<u8> = Vec::new();
    let mut index: Vec<IndexEntry> = Vec::with_capacity(sorted.len());
    for rec in sorted {
        let bytes = postcard::to_stdvec(rec)?;
        let offset = payload.len() as u64;
        let len = bytes.len() as u32;
        payload.extend_from_slice(&bytes);
        index.push(IndexEntry {
            name: rec.name.clone(),
            offset,
            len,
        });
    }

    let payload_checksum = blake3::hash(&payload).to_hex().to_string();
    let header = ShippedDbHeader {
        provenance,
        payload_checksum,
        index,
    };
    let header_bytes = postcard::to_stdvec(&header)?;

    let mut out: Vec<u8> = Vec::with_capacity(16 + header_bytes.len() + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(&payload);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &out)?;
    Ok(())
}

/// An opened, memory-mapped Tier 3 database.
pub struct ShippedDb {
    mmap: Mmap,
    /// payload region start, as an absolute file offset.
    payload_start: usize,
    provenance: ShippedDbProvenance,
    /// name -> (payload-relative offset, len)
    index: HashMap<String, (u64, u32)>,
}

impl std::fmt::Debug for ShippedDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShippedDb")
            .field("provenance", &self.provenance)
            .field("payload_start", &self.payload_start)
            .field("index_len", &self.index.len())
            .finish_non_exhaustive()
    }
}

impl ShippedDb {
    /// Open + verify a `names.db`. Synchronous; runtime consumers call it from
    /// the dedicated, capacity-bounded provider thread. Returns a typed error
    /// so the caller can explain-and-continue.
    pub fn open(path: &Path) -> Result<Self, ShippedDbError> {
        Self::open_with_deadline(path, None)
    }

    fn open_with_deadline(
        path: &Path,
        deadline: Option<std::time::Instant>,
    ) -> Result<Self, ShippedDbError> {
        match crate::package_db::load_thin_file_with_retry(path, deadline, Self::open_file) {
            Ok(db) => Ok(db),
            Err(crate::package_db::ThinFileLoadError::Absent) => Err(ShippedDbError::Absent),
            Err(crate::package_db::ThinFileLoadError::Load(error)) => Err(error),
            Err(crate::package_db::ThinFileLoadError::Io(error)) => {
                Err(ShippedDbError::Corrupt(error.to_string()))
            }
            Err(crate::package_db::ThinFileLoadError::DeadlineExpired) => {
                Err(ShippedDbError::DeadlineExpired)
            }
            Err(crate::package_db::ThinFileLoadError::ConcurrentModification) => {
                Err(ShippedDbError::ConcurrentModification)
            }
        }
    }

    fn open_file(file: &std::fs::File) -> Result<Self, ShippedDbError> {
        // SAFETY: the mapping is read-only. The shared thin-file loader retains
        // this opened handle and compares its identity with the addressed path
        // after parsing before accepting the database. That detects replacement
        // and metadata-visible mutation races and retries from a new handle.
        let mmap = match unsafe { Mmap::map(file) } {
            Ok(m) => m,
            Err(e) => return Err(ShippedDbError::Corrupt(e.to_string())),
        };

        if mmap.len() < 16 || &mmap[0..8] != MAGIC {
            return Err(ShippedDbError::Corrupt("bad magic".into()));
        }
        let version = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(ShippedDbError::UnsupportedFormat {
                found: version,
                supported: FORMAT_VERSION,
            });
        }
        let header_len = u32::from_le_bytes(mmap[12..16].try_into().unwrap()) as usize;
        let header_start = 16;
        let payload_start = header_start + header_len;
        if payload_start > mmap.len() {
            return Err(ShippedDbError::Corrupt(
                "header length exceeds file size".into(),
            ));
        }
        let header: ShippedDbHeader = postcard::from_bytes(&mmap[header_start..payload_start])
            .map_err(|e| ShippedDbError::Corrupt(format!("header decode: {e}")))?;

        let payload = &mmap[payload_start..];
        let actual = blake3::hash(payload).to_hex().to_string();
        if actual != header.payload_checksum {
            return Err(ShippedDbError::Corrupt(
                "payload checksum mismatch (corrupt or tampered)".into(),
            ));
        }

        // The payload checksum covers the payload region but NOT the header
        // (which holds the index). So a corrupt/tampered index can survive the
        // checks above. Validate every index entry's [offset, offset+len) lies
        // within the payload here — rejecting an out-of-range index loudly
        // (decision #9) rather than letting `decode_at` overflow or mis-read.
        let payload_len = payload.len() as u64;
        for e in &header.index {
            let out_of_bounds = match e.offset.checked_add(e.len as u64) {
                Some(end) => end > payload_len,
                None => true, // offset + len overflowed u64
            };
            if out_of_bounds {
                return Err(ShippedDbError::Corrupt(format!(
                    "index entry '{}' is out of bounds (offset {}, len {}, payload {})",
                    e.name, e.offset, e.len, payload_len
                )));
            }
        }

        let index = header
            .index
            .iter()
            .map(|e| (e.name.clone(), (e.offset, e.len)))
            .collect();

        Ok(Self {
            mmap,
            payload_start,
            provenance: header.provenance,
            index,
        })
    }

    /// Decode one package's record, lazily, from the mmap, and verify the
    /// decoded record's own `name` matches the index key it was reached through.
    ///
    /// The payload checksum authenticates the record BYTES; this binding
    /// authenticates the index → record MAPPING. So a corrupt/tampered index
    /// that remaps a name onto a different (valid, in-bounds, unchanged) payload
    /// is rejected (fails closed) instead of silently returning the wrong
    /// package. An unkeyed payload checksum can't catch that on its own — and,
    /// unlike extending the checksum to cover the index, this binding still
    /// holds if an attacker recomputes the hash, because each record carries its
    /// own name. Index bounds are validated at `open`, so the checked arithmetic
    /// here is defense-in-depth.
    fn decode_at(&self, name: &str, offset: u64, len: u32) -> Option<PackageRecord> {
        let start = self
            .payload_start
            .checked_add(usize::try_from(offset).ok()?)?;
        let end = start.checked_add(len as usize)?;
        let slice = self.mmap.get(start..end)?;
        match postcard::from_bytes::<PackageRecord>(slice) {
            Ok(rec) if rec.name == name => Some(rec),
            Ok(rec) => {
                log::warn!(
                    "names.db: index entry '{}' points at a record named '{}' \
                     (corrupt or tampered index); ignoring",
                    name,
                    rec.name
                );
                None
            }
            Err(e) => {
                log::warn!("names.db: failed to decode a record: {}", e);
                None
            }
        }
    }

    /// Look up and decode one package's `PackageInfo`, lazily, from the mmap.
    pub fn lookup(&self, name: &str) -> Option<PackageInfo> {
        let (offset, len) = *self.index.get(name)?;
        self.decode_at(name, offset, len).map(|rec| rec.into_info())
    }

    /// Decode and return EVERY record. Used by the Tier 3 build to seed the merge
    /// from the prior DB (Task 4.2). Not on any hot path.
    pub fn all_records(&self) -> Vec<PackageRecord> {
        self.index
            .iter()
            .filter_map(|(name, &(offset, len))| self.decode_at(name, offset, len))
            .collect()
    }

    pub fn provenance(&self) -> &ShippedDbProvenance {
        &self.provenance
    }

    /// True when the DB carries no package records. A successfully-opened but
    /// empty DB resolves nothing, so callers gating on "real package coverage"
    /// (e.g. the degraded-environment note in `cli::check`) must treat it as no
    /// coverage rather than as a usable provider.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
}

/// Tier 3 provider over an opened `ShippedDb`.
pub struct ShippedDbProvider {
    db: ShippedDb,
}

impl ShippedDbProvider {
    pub fn new(db: ShippedDb) -> Self {
        Self { db }
    }

    /// Open a `names.db` as a provider. `Ok(None)` when absent (silent); `Err(e)`
    /// for the loud cases (`UnsupportedFormat`, `Corrupt`, or concurrent
    /// modification) so the caller can explain-and-continue (decision #9).
    pub fn from_file(path: &Path) -> Result<Option<Self>, ShippedDbError> {
        Self::from_file_with_deadline(path, None)
    }

    pub(crate) fn from_file_with_deadline(
        path: &Path,
        deadline: Option<std::time::Instant>,
    ) -> Result<Option<Self>, ShippedDbError> {
        match ShippedDb::open_with_deadline(path, deadline) {
            Ok(db) => Ok(Some(Self::new(db))),
            Err(ShippedDbError::Absent) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// True when the underlying DB carries no packages — see [`ShippedDb::is_empty`].
    pub fn is_empty(&self) -> bool {
        self.db.is_empty()
    }
}

impl PackageMetadataProvider for ShippedDbProvider {
    fn lookup(&self, name: &str) -> Option<PackageInfo> {
        self.db.lookup(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package_db::model::PackageRecord;

    fn records() -> Vec<PackageRecord> {
        vec![
            PackageRecord {
                name: "dplyr".into(),
                version: "1.1.4".into(),
                exports: vec!["filter".into(), "mutate".into()],
                depends: vec!["R".into()],
                lazy_data: vec!["starwars".into()],
            },
            PackageRecord {
                name: "ggplot2".into(),
                version: "3.5.1".into(),
                exports: vec!["aes".into(), "ggplot".into()],
                depends: vec![],
                lazy_data: vec![],
            },
        ]
    }

    fn provenance() -> ShippedDbProvenance {
        ShippedDbProvenance {
            source: "test".into(),
            snapshot_date: "2026-05-30".into(),
            package_count: 2,
            raven_version: "9.9.9".into(),
        }
    }

    #[test]
    fn write_open_lookup_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("names.db");
        write_shipped_db(&path, &records(), provenance()).unwrap();

        let db = ShippedDb::open(&path).unwrap();
        let dplyr = db.lookup("dplyr").expect("dplyr present");
        assert!(dplyr.exports.contains("mutate"));
        assert_eq!(dplyr.depends, vec!["R".to_string()]);
        assert!(db.lookup("nonexistent").is_none());
        assert_eq!(db.provenance.snapshot_date, "2026-05-30");
    }

    #[test]
    fn all_records_returns_every_package() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("names.db");
        write_shipped_db(&path, &records(), provenance()).unwrap();
        let db = ShippedDb::open(&path).unwrap();
        let mut all = db.all_records();
        all.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "dplyr");
        assert_eq!(all[0].version, "1.1.4");
        assert_eq!(all[1].name, "ggplot2");
    }

    #[test]
    fn rejects_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.db");
        std::fs::write(&path, b"NOT A RAVEN DB").unwrap();
        assert!(ShippedDb::open(&path).is_err());
    }

    #[test]
    fn detects_payload_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("names.db");
        write_shipped_db(&path, &records(), provenance()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            ShippedDb::open(&path).is_err(),
            "checksum mismatch must be rejected"
        );
    }

    #[test]
    fn rejects_out_of_bounds_index_entry() {
        // The payload checksum does NOT cover the header/index, so a tampered
        // index with a valid (unchanged) payload survives the checksum. open()
        // must still reject an out-of-range index entry rather than overflow or
        // mis-read in decode_at.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("names.db");
        write_shipped_db(&path, &records(), provenance()).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let header_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let header_bytes = &bytes[16..16 + header_len];
        let payload = bytes[16 + header_len..].to_vec(); // unchanged → checksum stays valid

        let mut header: ShippedDbHeader = postcard::from_bytes(header_bytes).unwrap();
        // Point the first record far past the payload, keeping payload (and thus
        // its checksum) intact.
        header.index[0].offset = payload.len() as u64 + 1_000;

        let new_header = postcard::to_stdvec(&header).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(new_header.len() as u32).to_le_bytes());
        out.extend_from_slice(&new_header);
        out.extend_from_slice(&payload);
        std::fs::write(&path, &out).unwrap();

        match ShippedDb::open(&path) {
            Err(ShippedDbError::Corrupt(msg)) => {
                assert!(msg.contains("out of bounds"), "got {msg}")
            }
            other => panic!("expected Corrupt(out of bounds), got {other:?}"),
        }
    }

    #[test]
    fn rejects_index_remapping_to_wrong_record() {
        // The payload checksum covers the record BYTES but not the index→record
        // mapping. Swap the two entries' (offset,len) so each name points at the
        // OTHER package's (valid, unchanged) payload: the payload — and thus its
        // checksum — is untouched and offsets stay in-bounds, so open() succeeds.
        // A lookup must then fail closed rather than return the wrong package.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("names.db");
        write_shipped_db(&path, &records(), provenance()).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let header_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let header_bytes = &bytes[16..16 + header_len];
        let payload = bytes[16 + header_len..].to_vec(); // unchanged → checksum valid

        let mut header: ShippedDbHeader = postcard::from_bytes(header_bytes).unwrap();
        assert_eq!(header.index.len(), 2);
        // Swap (offset,len) between the two entries, keeping their names — so
        // "dplyr" now points at ggplot2's payload and vice versa.
        let zero = (header.index[0].offset, header.index[0].len);
        header.index[0].offset = header.index[1].offset;
        header.index[0].len = header.index[1].len;
        header.index[1].offset = zero.0;
        header.index[1].len = zero.1;

        let new_header = postcard::to_stdvec(&header).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(new_header.len() as u32).to_le_bytes());
        out.extend_from_slice(&new_header);
        out.extend_from_slice(&payload);
        std::fs::write(&path, &out).unwrap();

        let db = ShippedDb::open(&path).expect("payload checksum + bounds still pass");
        assert!(
            db.lookup("dplyr").is_none(),
            "remapped name must not resolve to the wrong record"
        );
        assert!(db.lookup("ggplot2").is_none());
        assert!(
            db.all_records().is_empty(),
            "every record fails the index→name binding check"
        );
    }

    #[test]
    fn absent_file_maps_to_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.db");
        match ShippedDb::open(&path) {
            Err(ShippedDbError::Absent) => {}
            other => panic!("expected Absent, got {other:?}"),
        }
        assert!(ShippedDbProvider::from_file(&path).unwrap().is_none());
    }
}
