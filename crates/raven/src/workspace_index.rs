//
// workspace_index.rs
//
// Unified workspace index for closed files with debounced updates
//

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use lru::LruCache;
use ropey::Rope;
use tokio::time::Instant;
use tower_lsp::lsp_types::Url;
use tree_sitter::Tree;

use crate::cross_file::file_cache::FileSnapshot;
use crate::cross_file::scope::ScopeArtifacts;
use crate::cross_file::types::CrossFileMetadata;

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for WorkspaceIndex
///
/// Controls debouncing, file limits, and size limits for workspace indexing.
///
/// **Validates: Requirements 4.1, 5.3, 11.4, 11.5**
#[derive(Debug, Clone)]
pub struct WorkspaceIndexConfig {
    /// Debounce delay for file updates in milliseconds
    pub debounce_ms: u64,
    /// Maximum files to index
    pub max_files: usize,
    /// Maximum file size to index in bytes
    pub max_file_size_bytes: usize,
}

impl Default for WorkspaceIndexConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 200,
            max_files: 1000,
            max_file_size_bytes: 512 * 1024, // 512KB
        }
    }
}

// ============================================================================
// Metrics
// ============================================================================

/// Metrics for tracking WorkspaceIndex performance
///
/// **Validates: Requirements 4.4, 9.3**
#[derive(Debug, Clone, Default)]
pub struct WorkspaceIndexMetrics {
    /// Number of cache hits (entry found in index)
    pub cache_hits: u64,
    /// Number of cache misses (entry not found)
    pub cache_misses: u64,
    /// Number of entries invalidated
    pub invalidations: u64,
    /// Number of entries inserted
    pub insertions: u64,
    /// Number of debounced updates scheduled
    pub updates_scheduled: u64,
    /// Number of debounced updates processed
    pub updates_processed: u64,
}

// ============================================================================
// Index Entry
// ============================================================================

/// Entry in the workspace index
///
/// Contains all data needed for LSP operations on a closed file,
/// including parsed AST, cross-file metadata, and scope artifacts.
///
/// **Validates: Requirements 4.1, 4.2**
pub struct IndexEntry {
    /// File content as a rope for efficient access
    pub contents: Rope,
    /// Parsed AST (None if parsing failed)
    pub tree: Option<Tree>,
    /// Packages loaded via library() calls
    pub loaded_packages: Vec<String>,
    /// Packages named by `data(..., package = ...)` calls.
    pub data_packages: Vec<String>,
    /// File snapshot for freshness checking
    pub snapshot: FileSnapshot,
    /// Cross-file metadata (source() calls, directives)
    pub metadata: Arc<CrossFileMetadata>,
    /// Scope artifacts (exported symbols, timeline)
    pub artifacts: Arc<ScopeArtifacts>,
    /// Index version when this entry was created
    pub indexed_at_version: u64,
}

impl Clone for IndexEntry {
    fn clone(&self) -> Self {
        Self {
            contents: self.contents.clone(),
            tree: self.tree.clone(),
            loaded_packages: self.loaded_packages.clone(),
            data_packages: self.data_packages.clone(),
            snapshot: self.snapshot.clone(),
            metadata: self.metadata.clone(),
            artifacts: self.artifacts.clone(),
            indexed_at_version: self.indexed_at_version,
        }
    }
}

/// Explicit finalization state for a closed-file analysis.
///
/// A pending entry has parsed text and local metadata but still needs
/// context-dependent enrichment (for example inherited working directory).
/// Consumers that require stable cross-file metadata must use only
/// [`Self::Complete`] entries rather than inferring finalization from the
/// presence or absence of a second store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichmentStatus {
    /// Context-dependent enrichment has not completed.
    Pending,
    /// Metadata and artifacts are ready for cross-file consumers.
    Complete,
}

/// Artifact-only projection retained beyond the full-payload LRU.
///
/// This is a tier inside [`WorkspaceIndex`], not an independent authority.
#[derive(Debug, Clone)]
pub struct ArtifactEntry {
    pub snapshot: FileSnapshot,
    pub metadata: Arc<CrossFileMetadata>,
    pub artifacts: Arc<ScopeArtifacts>,
    pub indexed_at_version: u64,
    pub provenance: ClosedProvenance,
    pub(crate) record_generation: u64,
}

impl From<&IndexEntry> for ArtifactEntry {
    fn from(entry: &IndexEntry) -> Self {
        Self {
            snapshot: entry.snapshot.clone(),
            metadata: entry.metadata.clone(),
            artifacts: entry.artifacts.clone(),
            indexed_at_version: entry.indexed_at_version,
            provenance: ClosedProvenance::Dynamic,
            record_generation: 0,
        }
    }
}

/// Origin of a closed-file analysis record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedProvenance {
    /// Owned by a committed workspace scan.
    WorkspaceScan { generation: u64 },
    /// Installed by watcher, on-demand, resync, or external-file indexing.
    Dynamic,
}

#[derive(Debug, Clone)]
enum ArtifactSlot {
    Pending {
        claim_generation: u64,
        provenance: ClosedProvenance,
    },
    Complete(ArtifactEntry),
}

/// Never-reused token authorizing one Pending → Complete transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichmentClaim {
    uri: Url,
    generation: u64,
}

impl EnrichmentClaim {
    pub(crate) fn uri(&self) -> &Url {
        &self.uri
    }
}

/// Exact identity authorizing refresh of one existing Complete record.
///
/// Unlike the index-wide version, this remains current across unrelated URI
/// mutations. Any replacement, metadata refresh, removal, or Pending
/// transition for this URI invalidates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteRefreshToken {
    uri: Url,
    record_generation: u64,
}

/// Non-mutating identity for removing exactly the observed closed slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClosedRecordToken {
    uri: Url,
    identity: ClosedRecordIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosedRecordIdentity {
    Absent,
    Pending(u64),
    Complete(u64),
}

impl ClosedRecordToken {
    pub(crate) fn uri(&self) -> &Url {
        &self.uri
    }
}

impl CompleteRefreshToken {
    pub(crate) fn uri(&self) -> &Url {
        &self.uri
    }
}

/// Result of atomically claiming context-dependent enrichment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimEnrichment {
    /// A Complete artifact record already exists.
    AlreadyComplete(CompleteRefreshToken),
    /// Another worker owns the current Pending claim.
    AlreadyPending,
    /// The caller owns this claim and may commit or abort it.
    Claimed(EnrichmentClaim),
}

/// Rejection from a guarded enrichment transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichmentCommitError {
    /// The URI no longer has the claimed Pending generation.
    StaleClaim,
    /// The URI no longer has the exact Complete record being refreshed.
    StaleRefresh,
    /// The authority lock is poisoned.
    Unavailable,
}

/// Coherent read of the closed-document authority used to prepare a batch
/// replacement off-lock.
#[derive(Clone)]
pub(crate) struct WorkspaceIndexSnapshot {
    pub(crate) version: u64,
    pub(crate) artifacts: Vec<(Url, ArtifactEntry)>,
    pub(crate) full: Vec<(Url, IndexEntry)>,
    pub(crate) pinned: HashSet<Url>,
    pub(crate) artifact_capacity_limit: usize,
}

#[derive(Debug)]
struct IndexState {
    version: u64,
    next_claim_generation: u64,
    artifacts: LruCache<Url, ArtifactSlot>,
    full: LruCache<Url, IndexEntry>,
    pinned: HashSet<Url>,
    artifact_user_cap: usize,
}

fn push_with_pins<V>(
    cache: &mut LruCache<Url, V>,
    pinned: &HashSet<Url>,
    uri: Url,
    value: V,
) -> Option<Url> {
    let mut evicted = None;
    if !cache.contains(&uri) && cache.len() >= cache.cap().get() {
        let victim = cache
            .iter()
            .rev()
            .find(|(candidate, _)| !pinned.contains(*candidate))
            .map(|(candidate, _)| candidate.clone());
        if let Some(victim) = victim {
            cache.pop(&victim);
            evicted = Some(victim);
        } else {
            cache.resize(
                NonZeroUsize::new(cache.len().saturating_add(1))
                    .expect("len + 1 is always non-zero"),
            );
        }
    }
    cache.push(uri, value);
    evicted
}

/// Default capacity for artifact-only closed-file records.
pub const DEFAULT_ARTIFACT_CAPACITY: usize = 5000;

/// Process-wide Complete-record generation source.
///
/// A replacement `WorkspaceIndex` must not permit a stale refresh token from
/// its predecessor to pass an ABA check.
static NEXT_CLOSED_RECORD_GENERATION: AtomicU64 = AtomicU64::new(1);

// ============================================================================
// Workspace Index
// ============================================================================

/// Unified workspace index for closed files with LRU eviction.
///
/// Manages indexed files with configurable limits and debounced updates.
/// Uses RwLock for interior mutability to allow concurrent read access.
/// Uses `peek()` for reads (no LRU promotion) and `push()` for writes.
///
/// **Validates: Requirements 4.1, 4.2, 4.3, 4.4**
pub struct WorkspaceIndex {
    /// Single authority lock for status, both residency tiers, pins, and version.
    inner: RwLock<IndexState>,
    /// Configuration
    config: WorkspaceIndexConfig,
    /// Pending debounced updates (URI -> scheduled time)
    pending_updates: RwLock<std::collections::HashMap<Url, Instant>>,
    /// Update queue for batched processing
    update_queue: RwLock<HashSet<Url>>,
    /// Metrics
    metrics: RwLock<WorkspaceIndexMetrics>,
}

impl WorkspaceIndex {
    /// Create a new WorkspaceIndex with the given configuration
    ///
    /// # Arguments
    /// * `config` - Configuration for file limits and debouncing
    ///
    /// # Returns
    /// A new WorkspaceIndex instance
    pub fn new(config: WorkspaceIndexConfig) -> Self {
        let cap = Self::effective_cap_for(&config);
        let artifact_cap =
            NonZeroUsize::new(DEFAULT_ARTIFACT_CAPACITY).expect("non-zero artifact capacity");
        Self {
            inner: RwLock::new(IndexState {
                version: 0,
                next_claim_generation: 0,
                artifacts: LruCache::new(artifact_cap),
                full: LruCache::new(cap),
                pinned: HashSet::new(),
                artifact_user_cap: DEFAULT_ARTIFACT_CAPACITY,
            }),
            config,
            pending_updates: RwLock::new(std::collections::HashMap::new()),
            update_queue: RwLock::new(HashSet::new()),
            metrics: RwLock::new(WorkspaceIndexMetrics::default()),
        }
    }

    /// Effective runtime cap for a config — `max_files`, normalized to
    /// `NonZeroUsize` with the same default `new()` applies. Shared by
    /// the constructor and the shrink-back path so both interpret
    /// `max_files == 0` identically.
    fn effective_cap_for(config: &WorkspaceIndexConfig) -> NonZeroUsize {
        crate::cross_file::cache::non_zero_or(config.max_files, 1000)
    }

    fn mint_record_generation() -> u64 {
        NEXT_CLOSED_RECORD_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("closed analysis record generation exhausted")
    }

    /// Replace the set of URIs protected from LRU eviction.
    ///
    /// Pinned URIs are skipped during eviction; if every in-cache URI is
    /// pinned, the cache is allowed to grow past `max_files` rather than
    /// drop a reachable neighbor. Oversized files remain rejected by
    /// `insert`, and explicit `invalidate` / disk-update replacement still
    /// work for pinned URIs.
    ///
    /// Also opportunistically shrinks the runtime cap back to
    /// `config.max_files` when `len() <= max_files`, so repeated all-pinned
    /// overflow events followed by safe unpins don't ratchet the cap
    /// upward indefinitely (issue #128). The shrink only fires when it
    /// can't itself force eviction.
    ///
    pub fn set_pinned_uris(&self, uris: HashSet<Url>) {
        let Ok(mut state) = self.inner.write() else {
            return;
        };
        if state.pinned == uris {
            return;
        }
        state.pinned = uris;

        let user_cap_nz = Self::effective_cap_for(&self.config);
        let user_cap = user_cap_nz.get();
        if state.full.cap().get() > user_cap && state.full.len() <= user_cap {
            state.full.resize(user_cap_nz);
        }
        let artifact_user_cap = state.artifact_user_cap;
        if let Some(cap) = NonZeroUsize::new(artifact_user_cap)
            && state.artifacts.cap().get() > artifact_user_cap
            && state.artifacts.len() <= artifact_user_cap
        {
            state.artifacts.resize(cap);
        }
        state.version = state.version.wrapping_add(1);
    }

    #[cfg(test)]
    pub(crate) fn pinned_uris_for_test(&self) -> HashSet<Url> {
        self.inner
            .read()
            .map(|state| state.pinned.clone())
            .unwrap_or_default()
    }

    // ========================================================================
    // Read Operations
    // ========================================================================

    /// Get entry for a URI
    ///
    /// Returns a clone of the entry if it exists.
    ///
    /// **Validates: Requirements 4.1, 4.3**
    ///
    /// # Arguments
    /// * `uri` - URI to look up
    ///
    /// # Returns
    /// Clone of IndexEntry if found, None otherwise
    pub fn get(&self, uri: &Url) -> Option<IndexEntry> {
        let guard = self.inner.read().ok()?;
        let entry = guard.full.peek(uri).cloned();

        // Update metrics
        if let Ok(mut metrics) = self.metrics.write() {
            if entry.is_some() {
                metrics.cache_hits += 1;
            } else {
                metrics.cache_misses += 1;
            }
        }

        entry
    }

    /// Get entry only if fresh
    ///
    /// Returns the entry only if its snapshot matches the provided snapshot.
    ///
    /// **Validates: Requirements 8.1, 8.2, 8.3**
    ///
    /// # Arguments
    /// * `uri` - URI to look up
    /// * `snapshot` - Expected file snapshot for freshness check
    ///
    /// # Returns
    /// Clone of IndexEntry if found and fresh, None otherwise
    pub fn get_if_fresh(&self, uri: &Url, snapshot: &FileSnapshot) -> Option<IndexEntry> {
        let guard = self.inner.read().ok()?;
        guard.full.peek(uri).and_then(|entry| {
            if entry.snapshot.matches_disk(snapshot) {
                Some(entry.clone())
            } else {
                None
            }
        })
    }

    /// Get metadata for a URI
    ///
    /// Returns just the cross-file metadata without the full entry.
    ///
    /// **Validates: Requirements 4.1**
    ///
    /// # Arguments
    /// * `uri` - URI to look up
    ///
    /// # Returns
    /// Clone of CrossFileMetadata if found, None otherwise
    pub fn get_metadata(&self, uri: &Url) -> Option<Arc<CrossFileMetadata>> {
        let guard = self.inner.read().ok()?;
        guard.artifacts.peek(uri).and_then(|slot| match slot {
            ArtifactSlot::Complete(entry) => Some(entry.metadata.clone()),
            ArtifactSlot::Pending { .. } => None,
        })
    }

    /// Get artifacts for a URI
    ///
    /// Returns just the scope artifacts without the full entry.
    ///
    /// **Validates: Requirements 4.1**
    ///
    /// # Arguments
    /// * `uri` - URI to look up
    ///
    /// # Returns
    /// Clone of ScopeArtifacts if found, None otherwise
    pub fn get_artifacts(&self, uri: &Url) -> Option<Arc<ScopeArtifacts>> {
        let guard = self.inner.read().ok()?;
        guard.artifacts.peek(uri).and_then(|slot| match slot {
            ArtifactSlot::Complete(entry) => Some(entry.artifacts.clone()),
            ArtifactSlot::Pending { .. } => None,
        })
    }

    /// Get an artifact-tier entry regardless of enrichment state.
    pub fn get_artifact_entry(&self, uri: &Url) -> Option<ArtifactEntry> {
        self.inner
            .read()
            .ok()?
            .artifacts
            .peek(uri)
            .and_then(|slot| match slot {
                ArtifactSlot::Complete(entry) => Some(entry.clone()),
                ArtifactSlot::Pending { .. } => None,
            })
    }

    /// Coherent artifact/full/version snapshot for guarded consumers.
    #[cfg(test)]
    pub(crate) fn get_complete_views(
        &self,
        uri: &Url,
    ) -> Option<(ArtifactEntry, Option<IndexEntry>, u64)> {
        let state = self.inner.read().ok()?;
        let artifact = match state.artifacts.peek(uri)? {
            ArtifactSlot::Complete(entry) => entry.clone(),
            ArtifactSlot::Pending { .. } => return None,
        };
        Some((artifact, state.full.peek(uri).cloned(), state.version))
    }

    /// Snapshot both residency tiers and their shared authority identity under
    /// one read lock. Pending slots are intentionally omitted: a scan prepared
    /// from this snapshot cannot adopt another worker's unfinished record.
    pub(crate) fn authority_snapshot(&self) -> WorkspaceIndexSnapshot {
        let Ok(state) = self.inner.read() else {
            return WorkspaceIndexSnapshot {
                version: 0,
                artifacts: Vec::new(),
                full: Vec::new(),
                pinned: HashSet::new(),
                artifact_capacity_limit: DEFAULT_ARTIFACT_CAPACITY,
            };
        };
        WorkspaceIndexSnapshot {
            version: state.version,
            artifacts: state
                .artifacts
                .iter()
                .filter_map(|(uri, slot)| match slot {
                    ArtifactSlot::Complete(entry) => Some((uri.clone(), entry.clone())),
                    ArtifactSlot::Pending { .. } => None,
                })
                .collect(),
            full: state
                .full
                .iter()
                .map(|(uri, entry)| (uri.clone(), entry.clone()))
                .collect(),
            pinned: state.pinned.clone(),
            artifact_capacity_limit: state.artifact_user_cap,
        }
    }

    /// Return the origin of a finalized record.
    pub(crate) fn provenance(&self, uri: &Url) -> Option<ClosedProvenance> {
        self.get_artifact_entry(uri).map(|entry| entry.provenance)
    }

    /// Return the explicit enrichment state for a URI.
    pub fn enrichment_status(&self, uri: &Url) -> Option<EnrichmentStatus> {
        self.inner
            .read()
            .ok()?
            .artifacts
            .peek(uri)
            .map(|slot| match slot {
                ArtifactSlot::Pending { .. } => EnrichmentStatus::Pending,
                ArtifactSlot::Complete(_) => EnrichmentStatus::Complete,
            })
    }

    /// Whether the URI has a finalized artifact-tier record.
    pub fn is_complete(&self, uri: &Url) -> bool {
        self.enrichment_status(uri) == Some(EnrichmentStatus::Complete)
    }

    /// Whether any artifact-tier record exists for the URI.
    pub fn contains_artifacts(&self, uri: &Url) -> bool {
        self.inner
            .read()
            .map(|guard| guard.artifacts.contains(uri))
            .unwrap_or(false)
    }

    /// Snapshot all artifact-tier URIs.
    pub fn artifact_uris(&self) -> Vec<Url> {
        self.inner
            .read()
            .map(|guard| {
                guard
                    .artifacts
                    .iter()
                    .filter(|(_, slot)| matches!(slot, ArtifactSlot::Complete(_)))
                    .map(|(uri, _)| uri.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Snapshot all artifact-tier entries.
    pub fn artifact_iter(&self) -> Vec<(Url, ArtifactEntry)> {
        self.inner
            .read()
            .map(|guard| {
                guard
                    .artifacts
                    .iter()
                    .filter_map(|(uri, slot)| match slot {
                        ArtifactSlot::Complete(entry) => Some((uri.clone(), entry.clone())),
                        ArtifactSlot::Pending { .. } => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Snapshot finalized artifact entries satisfying `pred`.
    pub(crate) fn artifact_entries_matching<F>(&self, pred: F) -> Vec<(Url, ArtifactEntry)>
    where
        F: Fn(&ArtifactEntry) -> bool,
    {
        self.inner
            .read()
            .map(|guard| {
                guard
                    .artifacts
                    .iter()
                    .filter_map(|(uri, slot)| match slot {
                        ArtifactSlot::Complete(entry) if pred(entry) => {
                            Some((uri.clone(), entry.clone()))
                        }
                        ArtifactSlot::Complete(_) | ArtifactSlot::Pending { .. } => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether any finalized artifact entry satisfies `pred`.
    pub(crate) fn any_artifact<F>(&self, pred: F) -> bool
    where
        F: Fn(&ArtifactEntry) -> bool,
    {
        self.inner
            .read()
            .map(|guard| {
                guard.artifacts.iter().any(|(_, slot)| match slot {
                    ArtifactSlot::Complete(entry) => pred(entry),
                    ArtifactSlot::Pending { .. } => false,
                })
            })
            .unwrap_or(false)
    }

    /// Check if URI is indexed
    ///
    /// # Arguments
    /// * `uri` - URI to check
    ///
    /// # Returns
    /// true if the URI is in the index
    pub fn contains(&self, uri: &Url) -> bool {
        self.inner
            .read()
            .map(|guard| guard.full.contains(uri))
            .unwrap_or(false)
    }

    /// Get all indexed URIs
    ///
    /// **Validates: Requirements 10.1**
    ///
    /// # Returns
    /// Vector of all indexed URIs
    pub fn uris(&self) -> Vec<Url> {
        self.inner
            .read()
            .map(|guard| guard.full.iter().map(|(k, _)| k.clone()).collect())
            .unwrap_or_default()
    }

    /// Iterate over all entries
    ///
    /// Returns a snapshot of all entries as a vector of (URI, entry) pairs.
    ///
    /// **Validates: Requirements 10.1, 10.2, 10.3**
    ///
    /// # Returns
    /// Vector of (Url, IndexEntry) pairs
    pub fn iter(&self) -> Vec<(Url, IndexEntry)> {
        self.inner
            .read()
            .map(|guard| {
                guard
                    .full
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Snapshot every Complete artifact resident as a full-shaped graph
    /// derivation input. Full payloads win when resident; artifact-only
    /// records use empty content while retaining their authoritative metadata
    /// and artifacts.
    pub(crate) fn graph_derivation_entries(&self) -> Vec<(Url, IndexEntry)> {
        self.inner
            .read()
            .map(|guard| {
                guard
                    .artifacts
                    .iter()
                    .filter_map(|(uri, slot)| {
                        let ArtifactSlot::Complete(artifact) = slot else {
                            return None;
                        };
                        let entry = guard.full.peek(uri).cloned().unwrap_or_else(|| IndexEntry {
                            contents: Rope::new(),
                            tree: None,
                            loaded_packages: Vec::new(),
                            data_packages: Vec::new(),
                            snapshot: artifact.snapshot.clone(),
                            metadata: artifact.metadata.clone(),
                            artifacts: artifact.artifacts.clone(),
                            indexed_at_version: artifact.indexed_at_version,
                        });
                        Some((uri.clone(), entry))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether any entry satisfies `pred`, without cloning entries.
    ///
    /// Read-lock + iteration only (no LRU promotion); use as a cheap
    /// pre-check before paying for a snapshot or a stronger lock.
    pub fn any_entry<F>(&self, pred: F) -> bool
    where
        F: Fn(&IndexEntry) -> bool,
    {
        self.inner
            .read()
            .map(|guard| guard.full.iter().any(|(_, v)| pred(v)))
            .unwrap_or(false)
    }

    /// Snapshot of the entries satisfying `pred`.
    ///
    /// Like [`Self::iter`] but clones only the matching subset, so a sparse
    /// predicate over a large index avoids the full O(N) entry clone.
    pub fn entries_matching<F>(&self, pred: F) -> Vec<(Url, IndexEntry)>
    where
        F: Fn(&IndexEntry) -> bool,
    {
        self.inner
            .read()
            .map(|guard| {
                guard
                    .full
                    .iter()
                    .filter(|(_, v)| pred(v))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get current version
    ///
    /// Returns the current monotonic version counter value.
    ///
    /// **Validates: Requirements 4.4**
    ///
    /// # Returns
    /// Current version number
    pub fn version(&self) -> u64 {
        self.inner.read().map(|state| state.version).unwrap_or(0)
    }

    /// Get the number of indexed entries
    pub fn len(&self) -> usize {
        self.inner.read().map(|guard| guard.full.len()).unwrap_or(0)
    }

    /// Get the current cache capacity.
    ///
    /// May exceed `config.max_files` after an all-pinned overflow has
    /// forced the underlying LRU to grow (see `pin_aware_push`).
    /// `set_pinned_uris` opportunistically restores the cap to
    /// `config.max_files` when shrinking is safe (`len() <= max_files`).
    pub fn cap(&self) -> usize {
        self.inner
            .read()
            .map(|guard| guard.full.cap().get())
            .unwrap_or(0)
    }

    /// Current artifact-tier capacity.
    pub fn artifact_cap(&self) -> usize {
        self.inner
            .read()
            .map(|guard| guard.artifacts.cap().get())
            .unwrap_or(0)
    }

    /// Check if the index is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get current metrics
    pub fn metrics(&self) -> WorkspaceIndexMetrics {
        self.metrics
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Get current configuration
    pub fn config(&self) -> &WorkspaceIndexConfig {
        &self.config
    }

    // ========================================================================
    // Write Operations
    // ========================================================================

    /// Insert entry directly
    ///
    /// Inserts an entry into the index and increments the version counter.
    /// If at capacity, the least-recently-used entry is evicted (LRU).
    /// Oversized files (exceeding max_file_size_bytes) are still rejected.
    ///
    /// **Validates: Requirements 4.2, 4.4, 12.1, 12.2, 12.3**
    ///
    /// # Arguments
    /// * `uri` - URI for the entry
    /// * `entry` - IndexEntry to insert
    ///
    /// # Returns
    /// true if inserted, false if rejected due to file size limit
    pub fn insert(&self, uri: Url, entry: IndexEntry) -> bool {
        self.install_complete(uri, entry, ClosedProvenance::Dynamic)
    }

    /// Atomically install one Complete record and derive both projections.
    ///
    /// The artifact projection always commits. The full payload is admitted
    /// only when it satisfies the configured size limit; either way this is
    /// one locked transaction and one version increment.
    pub fn install_complete(
        &self,
        uri: Url,
        mut entry: IndexEntry,
        provenance: ClosedProvenance,
    ) -> bool {
        let Ok(mut state) = self.inner.write() else {
            return false;
        };
        let next_version = state.version.wrapping_add(1);
        entry.indexed_at_version = next_version;
        let mut artifact = ArtifactEntry::from(&entry);
        artifact.indexed_at_version = next_version;
        artifact.provenance = provenance;

        Self::install_complete_locked(
            &mut state,
            uri.clone(),
            artifact,
            Some(entry),
            self.config.max_file_size_bytes,
        );
        state.version = next_version;
        let full_resident = state.full.contains(&uri);
        drop(state);

        if let Ok(mut metrics) = self.metrics.write() {
            metrics.insertions += 1;
        }
        full_resident
    }

    /// Refresh a finalized record without changing who owns its lifecycle.
    pub(crate) fn install_complete_preserving_provenance(
        &self,
        uri: Url,
        entry: IndexEntry,
    ) -> bool {
        let provenance = self.provenance(&uri).unwrap_or(ClosedProvenance::Dynamic);
        self.install_complete(uri, entry, provenance)
    }

    /// Replace metadata for one finalized record in both residency tiers.
    ///
    /// Artifact-only Complete records remain artifact-only. Full residents
    /// keep sharing the exact metadata `Arc` with their artifact projection.
    pub(crate) fn replace_complete_metadata(
        &self,
        uri: &Url,
        metadata: Arc<CrossFileMetadata>,
    ) -> bool {
        let Ok(mut state) = self.inner.write() else {
            return false;
        };
        let Some(ArtifactSlot::Complete(existing)) = state.artifacts.peek(uri) else {
            return false;
        };
        let mut artifact = existing.clone();
        let next_version = state.version.wrapping_add(1);
        artifact.metadata = metadata.clone();
        artifact.indexed_at_version = next_version;
        artifact.record_generation = Self::mint_record_generation();
        state
            .artifacts
            .push(uri.clone(), ArtifactSlot::Complete(artifact));
        if let Some(mut full) = state.full.pop(uri) {
            full.metadata = metadata;
            full.indexed_at_version = next_version;
            state.full.push(uri.clone(), full);
        }
        state.version = next_version;
        true
    }

    fn install_complete_locked(
        state: &mut IndexState,
        uri: Url,
        mut artifact: ArtifactEntry,
        full: Option<IndexEntry>,
        max_file_size_bytes: usize,
    ) {
        artifact.record_generation = Self::mint_record_generation();
        let mut protected = state.pinned.clone();
        protected.extend(
            state
                .artifacts
                .iter()
                .filter(|(_, slot)| matches!(slot, ArtifactSlot::Pending { .. }))
                .map(|(pending_uri, _)| pending_uri.clone()),
        );
        if let Some(evicted) = push_with_pins(
            &mut state.artifacts,
            &protected,
            uri.clone(),
            ArtifactSlot::Complete(artifact),
        ) {
            state.full.pop(&evicted);
        }

        if let Some(full) = full {
            if max_file_size_bytes > 0 && full.snapshot.size > max_file_size_bytes as u64 {
                state.full.pop(&uri);
            } else {
                push_with_pins(&mut state.full, &state.pinned, uri, full);
            }
        }
    }

    /// Atomically claim enrichment for an absent URI.
    pub fn claim_enrichment(&self, uri: Url, provenance: ClosedProvenance) -> ClaimEnrichment {
        let Ok(mut state) = self.inner.write() else {
            return ClaimEnrichment::AlreadyPending;
        };
        match state.artifacts.peek(&uri) {
            Some(ArtifactSlot::Complete(entry)) => {
                return ClaimEnrichment::AlreadyComplete(CompleteRefreshToken {
                    uri,
                    record_generation: entry.record_generation,
                });
            }
            Some(ArtifactSlot::Pending { .. }) => return ClaimEnrichment::AlreadyPending,
            None => {}
        }

        state.next_claim_generation = state.next_claim_generation.wrapping_add(1);
        let generation = state.next_claim_generation;
        let mut protected = state.pinned.clone();
        protected.extend(
            state
                .artifacts
                .iter()
                .filter(|(_, slot)| matches!(slot, ArtifactSlot::Pending { .. }))
                .map(|(pending_uri, _)| pending_uri.clone()),
        );
        if let Some(evicted) = push_with_pins(
            &mut state.artifacts,
            &protected,
            uri.clone(),
            ArtifactSlot::Pending {
                claim_generation: generation,
                provenance,
            },
        ) {
            state.full.pop(&evicted);
        }
        state.version = state.version.wrapping_add(1);
        ClaimEnrichment::Claimed(EnrichmentClaim { uri, generation })
    }

    /// Commit a claimed Pending record atomically into both projections.
    pub fn commit_enrichment(
        &self,
        claim: &EnrichmentClaim,
        mut entry: IndexEntry,
    ) -> Result<bool, EnrichmentCommitError> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| EnrichmentCommitError::Unavailable)?;
        let provenance = match state.artifacts.peek(&claim.uri) {
            Some(ArtifactSlot::Pending {
                claim_generation,
                provenance,
            }) if *claim_generation == claim.generation => *provenance,
            _ => return Err(EnrichmentCommitError::StaleClaim),
        };
        let next_version = state.version.wrapping_add(1);
        entry.indexed_at_version = next_version;
        let mut artifact = ArtifactEntry::from(&entry);
        artifact.indexed_at_version = next_version;
        artifact.provenance = provenance;
        Self::install_complete_locked(
            &mut state,
            claim.uri.clone(),
            artifact,
            Some(entry),
            self.config.max_file_size_bytes,
        );
        state.version = next_version;
        Ok(state.full.contains(&claim.uri))
    }

    pub(crate) fn enrichment_claim_is_current(&self, claim: &EnrichmentClaim) -> bool {
        self.inner.read().is_ok_and(|state| {
            matches!(
                state.artifacts.peek(&claim.uri),
                Some(ArtifactSlot::Pending { claim_generation, .. })
                    if *claim_generation == claim.generation
            )
        })
    }

    pub(crate) fn complete_refresh_is_current(&self, token: &CompleteRefreshToken) -> bool {
        self.inner.read().is_ok_and(|state| {
            matches!(
                state.artifacts.peek(&token.uri),
                Some(ArtifactSlot::Complete(entry))
                    if entry.record_generation == token.record_generation
            )
        })
    }

    pub(crate) fn closed_record_token(&self, uri: &Url) -> ClosedRecordToken {
        let identity = self
            .inner
            .read()
            .ok()
            .and_then(|state| {
                state.artifacts.peek(uri).map(|slot| match slot {
                    ArtifactSlot::Pending {
                        claim_generation, ..
                    } => ClosedRecordIdentity::Pending(*claim_generation),
                    ArtifactSlot::Complete(entry) => {
                        ClosedRecordIdentity::Complete(entry.record_generation)
                    }
                })
            })
            .unwrap_or(ClosedRecordIdentity::Absent);
        ClosedRecordToken {
            uri: uri.clone(),
            identity,
        }
    }

    pub(crate) fn closed_record_token_is_current(&self, token: &ClosedRecordToken) -> bool {
        self.closed_record_token(&token.uri).identity == token.identity
    }

    pub(crate) fn closed_record_token_is_present(&self, token: &ClosedRecordToken) -> bool {
        !matches!(token.identity, ClosedRecordIdentity::Absent)
    }

    /// Replace the exact Complete record captured by `token`.
    ///
    /// The comparison and replacement occur under the authority write lock.
    /// Unrelated index mutations do not invalidate the token, while every
    /// replacement of this URI does. Callers must separately validate any
    /// surrounding authority such as open-document ownership while holding
    /// their encompassing state lock.
    pub fn commit_complete_refresh(
        &self,
        token: &CompleteRefreshToken,
        mut entry: IndexEntry,
    ) -> Result<bool, EnrichmentCommitError> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| EnrichmentCommitError::Unavailable)?;
        let provenance = match state.artifacts.peek(&token.uri) {
            Some(ArtifactSlot::Complete(current))
                if current.record_generation == token.record_generation =>
            {
                current.provenance
            }
            _ => return Err(EnrichmentCommitError::StaleRefresh),
        };
        let next_version = state.version.wrapping_add(1);
        entry.indexed_at_version = next_version;
        let mut artifact = ArtifactEntry::from(&entry);
        artifact.indexed_at_version = next_version;
        artifact.provenance = provenance;
        Self::install_complete_locked(
            &mut state,
            token.uri.clone(),
            artifact,
            Some(entry),
            self.config.max_file_size_bytes,
        );
        state.version = next_version;
        Ok(state.full.contains(&token.uri))
    }

    /// Abort a Pending claim; stale claims cannot remove a newer generation.
    pub fn abort_enrichment(&self, claim: &EnrichmentClaim) -> Result<(), EnrichmentCommitError> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| EnrichmentCommitError::Unavailable)?;
        let current = matches!(
            state.artifacts.peek(&claim.uri),
            Some(ArtifactSlot::Pending { claim_generation, .. })
                if *claim_generation == claim.generation
        );
        if !current {
            return Err(EnrichmentCommitError::StaleClaim);
        }
        state.artifacts.pop(&claim.uri);
        state.full.pop(&claim.uri);
        state.version = state.version.wrapping_add(1);
        Ok(())
    }

    /// Atomically replace all Complete records, both residency tiers, and pins.
    ///
    /// `artifact_only` represents Complete records whose full payload is not
    /// resident. Full records always derive and overwrite their own artifact
    /// projection, so callers cannot provide divergent metadata/artifacts for
    /// the same URI. URI ordering is canonical before both LRU admissions.
    pub(crate) fn replace_all_complete(
        &self,
        artifact_only: Vec<(Url, ArtifactEntry)>,
        full_records: Vec<(Url, IndexEntry, ClosedProvenance)>,
        pins: HashSet<Url>,
    ) -> Result<(), EnrichmentCommitError> {
        self.replace_all_complete_if_version(None, artifact_only, full_records, pins)
            .map(|replaced| {
                debug_assert!(replaced);
            })
    }

    /// Atomically replace all Complete records only while the exact authority
    /// version captured by a detached transaction remains installed.
    pub(crate) fn replace_all_complete_if_current(
        &self,
        expected_version: u64,
        artifact_only: Vec<(Url, ArtifactEntry)>,
        full_records: Vec<(Url, IndexEntry, ClosedProvenance)>,
        pins: HashSet<Url>,
    ) -> Result<bool, EnrichmentCommitError> {
        self.replace_all_complete_if_version(
            Some(expected_version),
            artifact_only,
            full_records,
            pins,
        )
    }

    fn replace_all_complete_if_version(
        &self,
        expected_version: Option<u64>,
        artifact_only: Vec<(Url, ArtifactEntry)>,
        full_records: Vec<(Url, IndexEntry, ClosedProvenance)>,
        pins: HashSet<Url>,
    ) -> Result<bool, EnrichmentCommitError> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| EnrichmentCommitError::Unavailable)?;
        if expected_version.is_some_and(|expected| state.version != expected) {
            return Ok(false);
        }
        let next_version = state.version.wrapping_add(1);
        let mut artifacts: std::collections::HashMap<Url, ArtifactEntry> =
            artifact_only.into_iter().collect();
        let mut full_records = full_records;
        for (uri, entry, provenance) in &full_records {
            let mut artifact = ArtifactEntry::from(entry);
            artifact.indexed_at_version = next_version;
            artifact.provenance = *provenance;
            artifacts.insert(uri.clone(), artifact);
        }

        state.artifacts.clear();
        state.full.clear();
        state.pinned = pins;

        let mut artifacts: Vec<_> = artifacts.into_iter().collect();
        artifacts.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        for (uri, mut artifact) in artifacts {
            artifact.indexed_at_version = next_version;
            artifact.record_generation = Self::mint_record_generation();
            let protected = state.pinned.clone();
            if let Some(evicted) = push_with_pins(
                &mut state.artifacts,
                &protected,
                uri,
                ArtifactSlot::Complete(artifact),
            ) {
                state.full.pop(&evicted);
            }
        }

        full_records.sort_by(|(left, _, _), (right, _, _)| left.as_str().cmp(right.as_str()));
        for (uri, mut entry, _) in full_records {
            if !matches!(state.artifacts.peek(&uri), Some(ArtifactSlot::Complete(_))) {
                continue;
            }
            if self.config.max_file_size_bytes > 0
                && entry.snapshot.size > self.config.max_file_size_bytes as u64
            {
                continue;
            }
            entry.indexed_at_version = next_version;
            let pins = state.pinned.clone();
            push_with_pins(&mut state.full, &pins, uri, entry);
        }
        state.version = next_version;
        Ok(true)
    }

    /// Invalidate entry for a URI
    ///
    /// Removes the entry and increments the version counter.
    ///
    /// **Validates: Requirements 9.1, 9.3**
    ///
    /// # Arguments
    /// * `uri` - URI to invalidate
    ///
    /// # Returns
    /// true if an entry was removed, false otherwise
    pub fn invalidate(&self, uri: &Url) -> bool {
        let Ok(mut guard) = self.inner.write() else {
            return false;
        };

        let removed_full = guard.full.pop(uri).is_some();
        let removed_artifacts = guard.artifacts.pop(uri).is_some();
        let removed = removed_full || removed_artifacts;

        if removed {
            guard.version = guard.version.wrapping_add(1);
        }
        drop(guard);

        if removed && let Ok(mut metrics) = self.metrics.write() {
            metrics.invalidations += 1;
        }

        removed
    }

    /// Invalidate all entries
    ///
    /// Clears all entries and increments the version counter.
    ///
    /// **Validates: Requirements 9.2, 9.3**
    pub fn invalidate_all(&self) {
        let Ok(mut guard) = self.inner.write() else {
            return;
        };

        let count = guard.full.len();
        let artifact_count = guard.artifacts.len();
        guard.full.clear();
        guard.artifacts.clear();
        guard.version = guard.version.wrapping_add(1);
        drop(guard);

        // Update metrics
        if count.saturating_add(artifact_count) > 0
            && let Ok(mut metrics) = self.metrics.write()
        {
            metrics.invalidations += count.saturating_add(artifact_count) as u64;
        }
    }

    /// Clear only full payloads while retaining artifact reachability.
    pub fn invalidate_all_full(&self) {
        if let Ok(mut state) = self.inner.write()
            && !state.full.is_empty()
        {
            state.full.clear();
            state.version = state.version.wrapping_add(1);
        }
    }

    /// Resize the artifact-only tier.
    pub fn resize_artifacts(&self, cap: usize) {
        let cap = crate::cross_file::cache::non_zero_or(cap, DEFAULT_ARTIFACT_CAPACITY);
        if let Ok(mut state) = self.inner.write() {
            state.artifact_user_cap = cap.get();
            while state.artifacts.len() > cap.get() {
                let victim = state
                    .artifacts
                    .iter()
                    .rev()
                    .find(|(uri, slot)| {
                        !state.pinned.contains(*uri)
                            && !matches!(slot, ArtifactSlot::Pending { .. })
                    })
                    .map(|(uri, _)| uri.clone());
                let Some(victim) = victim else {
                    break;
                };
                state.artifacts.pop(&victim);
                state.full.pop(&victim);
            }
            let runtime_cap = cap.get().max(state.artifacts.len());
            if let Some(runtime_cap) = NonZeroUsize::new(runtime_cap)
                && state.artifacts.cap() != runtime_cap
            {
                state.artifacts.resize(runtime_cap);
            }
            let artifact_residents: HashSet<_> =
                state.artifacts.iter().map(|(uri, _)| uri.clone()).collect();
            let full_uris: Vec<_> = state.full.iter().map(|(uri, _)| uri.clone()).collect();
            for full_uri in full_uris {
                if !artifact_residents.contains(&full_uri) {
                    state.full.pop(&full_uri);
                }
            }
            state.version = state.version.wrapping_add(1);
        }
    }

    // ========================================================================
    // Debounced Update Operations
    // ========================================================================

    /// Schedule a debounced update
    ///
    /// Adds the URI to the update queue with a debounce timer.
    /// If the URI is already scheduled, resets the timer to the current time,
    /// effectively extending the debounce period.
    ///
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// # Arguments
    /// * `uri` - URI to schedule for update
    ///
    /// # Behavior
    /// - If URI is not in the queue, adds it with current timestamp
    /// - If URI is already in the queue, resets its timestamp (debounce reset)
    /// - Multiple rapid calls for the same URI will batch into one update
    pub fn schedule_update(&self, uri: Url) {
        let now = Instant::now();

        // Add/update pending updates with current timestamp
        // This resets the debounce timer if the URI is already scheduled
        if let Ok(mut pending) = self.pending_updates.write() {
            pending.insert(uri.clone(), now);
        }

        // Add to update queue (HashSet handles deduplication)
        if let Ok(mut queue) = self.update_queue.write() {
            queue.insert(uri);
        }

        // Update metrics
        if let Ok(mut metrics) = self.metrics.write() {
            metrics.updates_scheduled += 1;
        }

        log::trace!("WorkspaceIndex: Scheduled update for URI (debounce timer reset)");
    }

    /// Get URIs that are ready for processing
    ///
    /// Returns URIs that have been pending longer than the debounce period
    /// and are not currently open.
    ///
    /// **Validates: Requirements 5.1, 5.2, 5.3, 5.4**
    ///
    /// # Arguments
    /// * `open_uris` - Set of URIs that are currently open (to skip)
    ///
    /// # Returns
    /// Vector of URIs ready for processing (debounce period elapsed, not open)
    pub fn get_ready_updates(&self, open_uris: &HashSet<Url>) -> Vec<Url> {
        let now = Instant::now();
        let debounce_duration = std::time::Duration::from_millis(self.config.debounce_ms);

        let Ok(pending) = self.pending_updates.read() else {
            return Vec::new();
        };

        pending
            .iter()
            .filter(|(uri, scheduled_at)| {
                // Skip open URIs - they are managed by open-document authority
                if open_uris.contains(*uri) {
                    return false;
                }
                // Check if debounce period has elapsed
                now.duration_since(**scheduled_at) >= debounce_duration
            })
            .map(|(uri, _)| uri.clone())
            .collect()
    }

    /// Process pending updates (called periodically)
    ///
    /// Processes URIs that have been in the queue longer than debounce_ms.
    /// Skips URIs that are currently open (they are managed by open-document authority).
    ///
    /// **Validates: Requirements 5.1, 5.2, 5.3, 5.4**
    ///
    /// # Arguments
    /// * `open_uris` - Set of URIs that are currently open (to skip)
    ///
    /// # Returns
    /// Vector of URIs that were processed (ready for re-indexing)
    ///
    /// # Note
    /// This method removes URIs from the pending queue and returns them.
    /// The caller is responsible for actually re-indexing the files.
    pub async fn process_update_queue(&self, open_uris: &HashSet<Url>) -> Vec<Url> {
        let now = Instant::now();
        let debounce_duration = std::time::Duration::from_millis(self.config.debounce_ms);
        let mut ready_uris = Vec::new();

        // Determine readiness and remove ready URIs atomically under write lock.
        let Ok(mut pending) = self.pending_updates.write() else {
            return Vec::new();
        };
        pending.retain(|uri, scheduled_at| {
            if open_uris.contains(uri) {
                return true;
            }
            if now.duration_since(*scheduled_at) >= debounce_duration {
                ready_uris.push(uri.clone());
                return false;
            }
            true
        });

        if ready_uris.is_empty() {
            return Vec::new();
        }

        // Remove processed URIs from update_queue
        if let Ok(mut queue) = self.update_queue.write() {
            for uri in &ready_uris {
                queue.remove(uri);
            }
        }

        // Update metrics
        if let Ok(mut metrics) = self.metrics.write() {
            metrics.updates_processed += ready_uris.len() as u64;
        }

        log::trace!(
            "WorkspaceIndex: Processed {} URIs from update queue",
            ready_uris.len()
        );

        ready_uris
    }

    /// Remove a URI from the pending update queue
    ///
    /// Used when a file is opened (becomes managed by open-document authority)
    /// or when a file is deleted.
    ///
    /// # Arguments
    /// * `uri` - URI to remove from the queue
    ///
    /// # Returns
    /// true if the URI was in the queue and removed
    pub fn cancel_pending_update(&self, uri: &Url) -> bool {
        let mut removed = false;

        if let Ok(mut pending) = self.pending_updates.write() {
            removed = pending.remove(uri).is_some();
        }

        if let Ok(mut queue) = self.update_queue.write() {
            queue.remove(uri);
        }

        if removed {
            log::trace!("WorkspaceIndex: Cancelled pending update for URI");
        }

        removed
    }

    /// Check if a URI has a pending update
    ///
    /// # Arguments
    /// * `uri` - URI to check
    ///
    /// # Returns
    /// true if the URI is in the pending update queue
    pub fn has_pending_update(&self, uri: &Url) -> bool {
        self.update_queue
            .read()
            .map(|guard| guard.contains(uri))
            .unwrap_or(false)
    }

    /// Get the number of pending updates
    pub fn pending_update_count(&self) -> usize {
        self.update_queue
            .read()
            .map(|guard| guard.len())
            .unwrap_or(0)
    }

    /// Get the scheduled time for a pending update
    ///
    /// # Arguments
    /// * `uri` - URI to check
    ///
    /// # Returns
    /// The Instant when the update was scheduled, if pending
    pub fn get_pending_update_time(&self, uri: &Url) -> Option<Instant> {
        self.pending_updates
            .read()
            .ok()
            .and_then(|guard| guard.get(uri).copied())
    }
}

impl Default for WorkspaceIndex {
    fn default() -> Self {
        Self::new(WorkspaceIndexConfig::default())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn make_test_config() -> WorkspaceIndexConfig {
        WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 10,
            max_file_size_bytes: 1024,
        }
    }

    fn make_test_snapshot() -> FileSnapshot {
        FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 100,
            content_hash: Some(12345),
        }
    }

    fn make_test_entry(version: u64) -> IndexEntry {
        IndexEntry {
            contents: Rope::from_str("x <- 1"),
            tree: None,
            loaded_packages: vec!["dplyr".to_string()],
            data_packages: vec![],
            snapshot: make_test_snapshot(),
            metadata: std::sync::Arc::new(CrossFileMetadata::default()),
            artifacts: Arc::new(ScopeArtifacts::default()),
            indexed_at_version: version,
        }
    }

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///{}", name)).unwrap()
    }

    /// `any_entry` short-circuits without cloning entries; `entries_matching`
    /// clones only the matching subset. Both are read-lock (`peek`-discipline)
    /// operations used by the system.file resolution pre-checks.
    #[test]
    fn any_entry_and_entries_matching_filter_without_full_snapshot() {
        let index = WorkspaceIndex::new(make_test_config());
        index.insert(test_uri("a.R"), make_test_entry(1));
        let mut tagged = make_test_entry(2);
        tagged.loaded_packages = vec!["special".to_string()];
        index.insert(test_uri("b.R"), tagged);

        assert!(index.any_entry(|e| e.loaded_packages.iter().any(|p| p == "special")));
        assert!(!index.any_entry(|e| e.loaded_packages.iter().any(|p| p == "absent")));

        let matched = index.entries_matching(|e| e.loaded_packages.iter().any(|p| p == "special"));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].0, test_uri("b.R"));
        assert!(
            index
                .entries_matching(|e| e.loaded_packages.iter().any(|p| p == "absent"))
                .is_empty()
        );
    }

    #[test]
    fn test_config_default() {
        let config = WorkspaceIndexConfig::default();
        assert_eq!(config.debounce_ms, 200);
        assert_eq!(config.max_files, 1000);
        assert_eq!(config.max_file_size_bytes, 512 * 1024);
        let index = WorkspaceIndex::new(config);
        assert_eq!(index.cap(), 1000);
        assert_eq!(index.artifact_cap(), 5000);
    }

    #[test]
    fn test_metrics_default() {
        let metrics = WorkspaceIndexMetrics::default();
        assert_eq!(metrics.cache_hits, 0);
        assert_eq!(metrics.cache_misses, 0);
        assert_eq!(metrics.invalidations, 0);
        assert_eq!(metrics.insertions, 0);
    }

    #[test]
    fn test_new_workspace_index() {
        let index = WorkspaceIndex::new(make_test_config());
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
        assert_eq!(index.version(), 0);
    }

    #[test]
    fn test_insert_and_get() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("test.R");
        let entry = make_test_entry(0);

        assert!(index.insert(uri.clone(), entry));
        assert!(index.contains(&uri));
        assert_eq!(index.len(), 1);
        assert_eq!(index.version(), 1);

        let retrieved = index.get(&uri).unwrap();
        assert_eq!(retrieved.contents.to_string(), "x <- 1");
        assert_eq!(retrieved.loaded_packages, vec!["dplyr".to_string()]);
    }

    #[test]
    fn full_install_projects_once_and_shares_arcs() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("shared.R");
        let entry = make_test_entry(0);
        let metadata = entry.metadata.clone();
        let artifacts = entry.artifacts.clone();
        let before = index.version();

        assert!(index.install_complete(uri.clone(), entry, ClosedProvenance::Dynamic));

        let (artifact_view, full_view, version) =
            index.get_complete_views(&uri).expect("complete record");
        let full_view = full_view.expect("full payload resident");
        assert_eq!(version, before + 1, "one logical install bumps once");
        assert!(Arc::ptr_eq(&metadata, &artifact_view.metadata));
        assert!(Arc::ptr_eq(&artifacts, &artifact_view.artifacts));
        assert!(Arc::ptr_eq(&full_view.metadata, &artifact_view.metadata));
        assert!(Arc::ptr_eq(&full_view.artifacts, &artifact_view.artifacts));
    }

    #[test]
    fn pending_is_invisible_and_generation_guarded() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("pending.R");
        let claim_a = match index.claim_enrichment(uri.clone(), ClosedProvenance::Dynamic) {
            ClaimEnrichment::Claimed(claim) => claim,
            other => panic!("expected first claim, got {other:?}"),
        };
        assert_eq!(
            index.enrichment_status(&uri),
            Some(EnrichmentStatus::Pending)
        );
        assert!(index.get_metadata(&uri).is_none());
        assert!(index.get_artifacts(&uri).is_none());
        assert!(index.get(&uri).is_none());
        assert_eq!(
            index.claim_enrichment(uri.clone(), ClosedProvenance::Dynamic),
            ClaimEnrichment::AlreadyPending
        );

        index.abort_enrichment(&claim_a).expect("claim A abort");
        let claim_b = match index.claim_enrichment(uri.clone(), ClosedProvenance::Dynamic) {
            ClaimEnrichment::Claimed(claim) => claim,
            other => panic!("expected replacement claim, got {other:?}"),
        };
        assert_ne!(claim_a.generation, claim_b.generation);
        assert_eq!(
            index.commit_enrichment(&claim_a, make_test_entry(0)),
            Err(EnrichmentCommitError::StaleClaim)
        );
        assert_eq!(
            index.abort_enrichment(&claim_a),
            Err(EnrichmentCommitError::StaleClaim)
        );
        assert!(
            index
                .commit_enrichment(&claim_b, make_test_entry(0))
                .expect("claim B commit")
        );
        assert_eq!(
            index.enrichment_status(&uri),
            Some(EnrichmentStatus::Complete)
        );
    }

    #[test]
    fn complete_refresh_token_is_per_record_and_invalidated_by_replacement() {
        let index = WorkspaceIndex::new(make_test_config());
        let target = test_uri("refresh-target.R");
        index.insert(target.clone(), make_test_entry(0));
        let token = match index.claim_enrichment(target.clone(), ClosedProvenance::Dynamic) {
            ClaimEnrichment::AlreadyComplete(token) => token,
            other => panic!("expected Complete token, got {other:?}"),
        };

        index.insert(test_uri("unrelated.R"), make_test_entry(0));
        let mut refreshed = make_test_entry(0);
        refreshed.contents = Rope::from_str("refreshed <- 1");
        assert!(
            index
                .commit_complete_refresh(&token, refreshed)
                .expect("unrelated mutation must not invalidate target token")
        );

        let stale = match index.claim_enrichment(target.clone(), ClosedProvenance::Dynamic) {
            ClaimEnrichment::AlreadyComplete(token) => token,
            other => panic!("expected replacement token, got {other:?}"),
        };
        index.replace_complete_metadata(
            &target,
            Arc::new(CrossFileMetadata {
                working_directory: Some("/new-basis".to_string()),
                ..Default::default()
            }),
        );
        assert_eq!(
            index.commit_complete_refresh(&stale, make_test_entry(0)),
            Err(EnrichmentCommitError::StaleRefresh)
        );
        assert_eq!(
            index
                .get_metadata(&target)
                .unwrap()
                .working_directory
                .as_deref(),
            Some("/new-basis")
        );

        let predecessor_token =
            match index.claim_enrichment(target.clone(), ClosedProvenance::Dynamic) {
                ClaimEnrichment::AlreadyComplete(token) => token,
                other => panic!("expected predecessor token, got {other:?}"),
            };
        let replacement_index = WorkspaceIndex::new(make_test_config());
        replacement_index.insert(target, make_test_entry(0));
        assert_eq!(
            replacement_index.commit_complete_refresh(&predecessor_token, make_test_entry(0)),
            Err(EnrichmentCommitError::StaleRefresh),
            "replacing the entire authority must not permit token ABA"
        );
    }

    #[test]
    fn dual_capacity_and_pins_preserve_artifact_only_complete() {
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 1,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);
        index.resize_artifacts(3);
        let uris: Vec<_> = (0..3).map(|i| test_uri(&format!("cap{i}.R"))).collect();
        for (i, uri) in uris.iter().enumerate() {
            assert!(index.insert(uri.clone(), make_test_entry(i as u64)));
        }

        assert_eq!(index.artifact_uris().len(), 3);
        assert_eq!(index.len(), 1);
        let evicted_full = &uris[0];
        assert!(index.get(evicted_full).is_none());
        assert!(index.get_metadata(evicted_full).is_some());
        assert!(index.get_artifacts(evicted_full).is_some());
        assert!(index.is_complete(evicted_full));

        let pinned = uris[2].clone();
        index.resize_artifacts(1);
        index.set_pinned_uris(HashSet::from([pinned.clone()]));
        let newcomer = test_uri("newcomer.R");
        assert!(index.insert(newcomer.clone(), make_test_entry(4)));
        assert!(index.contains_artifacts(&pinned));
        assert!(index.contains(&pinned));
        assert!(index.contains_artifacts(&newcomer));
        assert!(index.contains(&newcomer));
    }

    #[test]
    fn artifact_eviction_cascades_to_matching_full_payload() {
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 3,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);
        index.resize_artifacts(1);
        let evicted = test_uri("artifact-victim.R");
        let survivor = test_uri("artifact-survivor.R");
        assert!(index.insert(evicted.clone(), make_test_entry(0)));
        assert!(index.contains(&evicted));

        assert!(index.insert(survivor.clone(), make_test_entry(1)));

        assert!(!index.contains_artifacts(&evicted));
        assert!(
            !index.contains(&evicted),
            "a full payload may never outlive its artifact authority"
        );
        assert!(index.is_complete(&survivor));
        assert!(index.contains(&survivor));
    }

    #[test]
    fn batch_replace_is_atomic_and_versioned_once() {
        use std::sync::mpsc;

        let index = Arc::new(WorkspaceIndex::new(make_test_config()));
        let uri = test_uri("batch.R");
        assert!(index.insert(uri.clone(), make_test_entry(0)));
        let before = index.version();

        let state_guard = index.inner.write().expect("authority lock");
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let reader = {
            let index = index.clone();
            let uri = uri.clone();
            std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                done_tx.send(index.get_complete_views(&uri)).unwrap();
            })
        };
        started_rx.recv().unwrap();
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err(),
            "reader must block while the atomic authority lock is held"
        );
        drop(state_guard);
        assert!(done_rx.recv().unwrap().is_some());
        reader.join().unwrap();

        index
            .replace_all_complete(
                Vec::new(),
                vec![(
                    uri.clone(),
                    make_test_entry(1),
                    ClosedProvenance::WorkspaceScan { generation: 7 },
                )],
                HashSet::new(),
            )
            .unwrap();
        assert_eq!(index.version(), before + 1);
        let (artifact, full, _) = index.get_complete_views(&uri).unwrap();
        let full = full.unwrap();
        assert!(Arc::ptr_eq(&artifact.metadata, &full.metadata));
        assert!(Arc::ptr_eq(&artifact.artifacts, &full.artifacts));
    }

    #[test]
    fn racing_batch_observers_see_only_old_or_new_sets() {
        let index = Arc::new(WorkspaceIndex::new(make_test_config()));
        let old = [test_uri("old-a.R"), test_uri("old-b.R")];
        let new = [test_uri("new-a.R"), test_uri("new-b.R")];
        index
            .replace_all_complete(
                Vec::new(),
                old.iter()
                    .cloned()
                    .map(|uri| (uri, make_test_entry(0), ClosedProvenance::Dynamic))
                    .collect(),
                HashSet::new(),
            )
            .unwrap();

        let writer_index = index.clone();
        let old_writer = old.clone();
        let new_writer = new.clone();
        let writer = std::thread::spawn(move || {
            for generation in 0..500 {
                let selected = if generation % 2 == 0 {
                    &new_writer
                } else {
                    &old_writer
                };
                writer_index
                    .replace_all_complete(
                        Vec::new(),
                        selected
                            .iter()
                            .cloned()
                            .map(|uri| {
                                (
                                    uri,
                                    make_test_entry(generation),
                                    ClosedProvenance::WorkspaceScan { generation },
                                )
                            })
                            .collect(),
                        HashSet::new(),
                    )
                    .unwrap();
            }
        });

        let old: HashSet<_> = old.into_iter().collect();
        let new: HashSet<_> = new.into_iter().collect();
        for _ in 0..2_000 {
            let observed: HashSet<_> = index
                .authority_snapshot()
                .artifacts
                .into_iter()
                .map(|(uri, _)| uri)
                .collect();
            assert!(
                observed == old || observed == new,
                "batch observer saw a torn authority set: {observed:?}"
            );
        }
        writer.join().unwrap();
    }

    #[test]
    fn test_get_nonexistent() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("nonexistent.R");

        assert!(index.get(&uri).is_none());
        assert!(!index.contains(&uri));
    }

    #[test]
    fn test_get_if_fresh_matching() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("test.R");
        let snapshot = make_test_snapshot();
        let entry = make_test_entry(0);

        index.insert(uri.clone(), entry);

        // Same snapshot should return entry
        let retrieved = index.get_if_fresh(&uri, &snapshot);
        assert!(retrieved.is_some());
    }

    #[test]
    fn test_get_if_fresh_stale() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("test.R");
        let entry = make_test_entry(0);

        index.insert(uri.clone(), entry);

        // Different snapshot should return None
        let different_snapshot = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 200, // Different size
            content_hash: Some(99999),
        };
        let retrieved = index.get_if_fresh(&uri, &different_snapshot);
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_get_metadata() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("test.R");
        let entry = make_test_entry(0);

        index.insert(uri.clone(), entry);

        let metadata = index.get_metadata(&uri);
        assert!(metadata.is_some());
    }

    #[test]
    fn test_get_artifacts() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("test.R");
        let entry = make_test_entry(0);

        index.insert(uri.clone(), entry);

        let artifacts = index.get_artifacts(&uri);
        assert!(artifacts.is_some());
    }

    #[test]
    fn test_uris() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");

        index.insert(uri1.clone(), make_test_entry(0));
        index.insert(uri2.clone(), make_test_entry(1));

        let uris = index.uris();
        assert_eq!(uris.len(), 2);
        assert!(uris.contains(&uri1));
        assert!(uris.contains(&uri2));
    }

    #[test]
    fn test_iter() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");

        index.insert(uri1.clone(), make_test_entry(0));
        index.insert(uri2.clone(), make_test_entry(1));

        let entries = index.iter();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_invalidate() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("test.R");

        index.insert(uri.clone(), make_test_entry(0));
        assert!(index.contains(&uri));
        assert_eq!(index.version(), 1);

        assert!(index.invalidate(&uri));
        assert!(!index.contains(&uri));
        assert_eq!(index.version(), 2);

        // Invalidating again should return false
        assert!(!index.invalidate(&uri));
        assert_eq!(index.version(), 2); // Version unchanged
    }

    #[test]
    fn test_invalidate_all() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");

        index.insert(uri1.clone(), make_test_entry(0));
        index.insert(uri2.clone(), make_test_entry(1));
        assert_eq!(index.len(), 2);
        assert_eq!(index.version(), 2);

        index.invalidate_all();
        assert!(index.is_empty());
        assert_eq!(index.version(), 3);
    }

    #[test]
    fn test_max_files_lru_eviction() {
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 2,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);

        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");
        let uri3 = test_uri("test3.R");

        assert!(index.insert(uri1.clone(), make_test_entry(0)));
        assert!(index.insert(uri2.clone(), make_test_entry(1)));

        // Third insert should evict LRU (uri1), not be rejected
        assert!(index.insert(uri3.clone(), make_test_entry(2)));
        assert_eq!(index.len(), 2);
        assert!(!index.contains(&uri1), "LRU entry should be evicted");
        assert!(index.contains(&uri2));
        assert!(index.contains(&uri3));
    }

    #[test]
    fn test_pinned_uris_are_protected_from_eviction() {
        // Cache at capacity, pinned URI is the LRU candidate.
        // Inserting a new entry must evict an unpinned LRU instead of the pin.
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 2,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);

        let uri1 = test_uri("pinned.R");
        let uri2 = test_uri("lru_unpinned.R");
        let uri3 = test_uri("mru.R");

        assert!(index.insert(uri1.clone(), make_test_entry(0)));
        assert!(index.insert(uri2.clone(), make_test_entry(1)));

        let mut pinned = HashSet::new();
        pinned.insert(uri1.clone());
        index.set_pinned_uris(pinned);

        // uri1 is LRU but pinned, uri2 is MRU but unpinned.
        // Inserting uri3 should evict uri2 (LRU non-pinned), not uri1.
        assert!(index.insert(uri3.clone(), make_test_entry(2)));
        assert_eq!(index.len(), 2);
        assert!(index.contains(&uri1), "pinned URI must not be evicted");
        assert!(
            !index.contains(&uri2),
            "least-recently-used unpinned URI must be evicted"
        );
        assert!(index.contains(&uri3));
    }

    #[test]
    fn test_pinned_uris_can_exceed_max_files() {
        // When every in-cache entry is pinned, eviction is skipped and the
        // cache is allowed to grow past its configured capacity rather than
        // evict a reachable neighbor of an open document.
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 2,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);

        let uri1 = test_uri("a.R");
        let uri2 = test_uri("b.R");
        let uri3 = test_uri("c.R");

        assert!(index.insert(uri1.clone(), make_test_entry(0)));
        assert!(index.insert(uri2.clone(), make_test_entry(1)));

        let mut pinned = HashSet::new();
        pinned.insert(uri1.clone());
        pinned.insert(uri2.clone());
        index.set_pinned_uris(pinned);

        assert!(index.insert(uri3.clone(), make_test_entry(2)));
        assert!(index.contains(&uri1));
        assert!(index.contains(&uri2));
        assert!(index.contains(&uri3));
        assert_eq!(index.len(), 3);
    }

    #[test]
    fn test_pinned_set_held_across_lru_search_and_pop() {
        // Concurrency guard for the pin-aware eviction path: under
        // contended `set_pinned_uris` + `insert`, no panic, no deadlock,
        // no data corruption, and a URI that was pinned before the race
        // began must still be present after.
        //
        // This test does NOT deterministically distinguish a buggy
        // (read dropped before pop) implementation from a correct
        // (read held across pop) one. Verified empirically by running
        // an `assert!(index.contains(&uri2))` variant against both: in
        // five 200-iteration runs of each, eviction rates of `uri2`
        // ranged 0–31/200 (buggy) vs 4–53/200 (fixed) — overlapping
        // ranges, no clean statistical signal. The fix's contribution
        // is consistency of A's pin-set view across find+pop (a
        // composability property for any future concurrent reader of
        // `pinned`), not a different cache outcome. Strict TOCTOU
        // reproduction would require test hooks in the production
        // helper, which we don't add.
        for _ in 0..200 {
            let config = WorkspaceIndexConfig {
                debounce_ms: 50,
                max_files: 2,
                max_file_size_bytes: 1024,
            };
            let index = std::sync::Arc::new(WorkspaceIndex::new(config));
            let uri1 = test_uri("pinned.R");
            let uri2 = test_uri("unpinned.R");
            let uri3 = test_uri("new.R");

            assert!(index.insert(uri1.clone(), make_test_entry(0)));
            assert!(index.insert(uri2.clone(), make_test_entry(1)));

            // Pre-state: uri1 pinned, uri2 unpinned (LRU non-pinned).
            let mut pin = HashSet::new();
            pin.insert(uri1.clone());
            index.set_pinned_uris(pin);

            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

            let index_a = index.clone();
            let barrier_a = barrier.clone();
            let uri3_a = uri3.clone();
            let a = std::thread::spawn(move || {
                barrier_a.wait();
                index_a.insert(uri3_a, make_test_entry(2));
            });

            let index_b = index.clone();
            let barrier_b = barrier.clone();
            let uri1_b = uri1.clone();
            let uri2_b = uri2.clone();
            let b = std::thread::spawn(move || {
                barrier_b.wait();
                let mut p = HashSet::new();
                p.insert(uri1_b);
                p.insert(uri2_b);
                index_b.set_pinned_uris(p);
            });

            a.join().unwrap();
            b.join().unwrap();

            // uri1 was pinned before the race began; whichever order the
            // racing operations resolve in, the eviction's view of the
            // pin set always includes uri1, so uri1 must still be present.
            assert!(
                index.contains(&uri1),
                "uri1 was pinned before the race; eviction must never select it"
            );
        }
    }

    #[test]
    fn test_cap_shrinks_back_when_max_files_is_zero_and_runtime_cap_is_default() {
        // Issue #128 review finding 1: `new()` normalizes `max_files == 0`
        // to the default runtime cap (1000) via `non_zero_or`, so the
        // shrink-back path must compare and resize against that same
        // normalized value — not the raw `config.max_files == 0`,
        // which would disable shrink entirely.
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 0,
            // No file-size cap so we can insert tiny entries freely.
            max_file_size_bytes: 0,
        };
        let index = WorkspaceIndex::new(config);
        // Effective runtime cap is the default (1000) because of the
        // `non_zero_or(max_files, 1000)` normalization in `new()`.
        assert_eq!(index.cap(), 1000, "runtime cap should default to 1000");

        // Pin every entry up to the effective cap, then trigger an
        // all-pinned overflow.
        let mut pin_set = HashSet::new();
        for i in 0..1000 {
            let uri = test_uri(&format!("pre_{}.R", i));
            assert!(index.insert(uri.clone(), make_test_entry(i as u64)));
            pin_set.insert(uri);
        }
        assert_eq!(index.len(), 1000);
        index.set_pinned_uris(pin_set.clone());

        // All-pinned overflow grows cap to 1001.
        let overflow_uri = test_uri("overflow.R");
        assert!(index.insert(overflow_uri.clone(), make_test_entry(9999)));
        assert_eq!(index.cap(), 1001);

        // Drain back to the effective cap (1000).
        assert!(index.invalidate(&overflow_uri));
        assert_eq!(index.len(), 1000);

        // Clearing pins should now shrink cap back to 1000 — the
        // effective user_cap, not the raw `0` from config.
        index.set_pinned_uris(HashSet::new());
        assert_eq!(
            index.cap(),
            1000,
            "shrink-back must use the normalized cap, not raw max_files"
        );
    }

    #[test]
    fn test_cap_shrinks_back_to_user_cap_when_safe() {
        // Issue #128: after an all-pinned overflow grew the cap, calling
        // `set_pinned_uris` while `len() <= user_cap` should restore the
        // cap to its configured value, so repeated overflow/unpin cycles
        // don't grow `cap()` monotonically beyond `user_cap`.
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 2,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);
        let uri1 = test_uri("a.R");
        let uri2 = test_uri("b.R");
        let uri3 = test_uri("c.R");

        assert!(index.insert(uri1.clone(), make_test_entry(0)));
        assert!(index.insert(uri2.clone(), make_test_entry(1)));

        let mut pinned = HashSet::new();
        pinned.insert(uri1.clone());
        pinned.insert(uri2.clone());
        index.set_pinned_uris(pinned);
        assert!(index.insert(uri3.clone(), make_test_entry(2)));
        assert_eq!(index.cap(), 3, "all-pinned overflow grew the cap");
        assert_eq!(index.len(), 3);

        // Drop one entry so len falls back to user_cap.
        assert!(index.invalidate(&uri3));
        assert_eq!(index.len(), 2);

        // Clearing the pin set should now opportunistically shrink cap
        // back to user_cap (precondition: len() <= user_cap).
        index.set_pinned_uris(HashSet::new());
        assert_eq!(index.cap(), 2, "cap should shrink back to user_cap");
    }

    #[test]
    fn test_cap_does_not_shrink_when_len_still_exceeds_user_cap() {
        // Issue #128: shrinking must never force eviction. If `len()`
        // currently exceeds `user_cap`, leave the cap alone — the next
        // safe call to `set_pinned_uris` (after `len()` falls back) will
        // shrink it.
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 2,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);
        let uri1 = test_uri("a.R");
        let uri2 = test_uri("b.R");
        let uri3 = test_uri("c.R");

        assert!(index.insert(uri1.clone(), make_test_entry(0)));
        assert!(index.insert(uri2.clone(), make_test_entry(1)));

        let mut pinned = HashSet::new();
        pinned.insert(uri1.clone());
        pinned.insert(uri2.clone());
        index.set_pinned_uris(pinned);
        assert!(index.insert(uri3.clone(), make_test_entry(2)));
        assert_eq!(index.cap(), 3);
        assert_eq!(index.len(), 3);

        // Clear pins. len(3) > user_cap(2): shrinking would force eviction,
        // so cap must stay at 3.
        index.set_pinned_uris(HashSet::new());
        assert_eq!(index.cap(), 3, "cap must not shrink when len > user_cap");
        assert_eq!(index.len(), 3);
    }

    #[test]
    fn test_repeated_overflow_then_unpin_does_not_grow_cap_monotonically() {
        // Issue #128: the worst-case trace from the issue. Repeated
        // all-pinned overflow events, each followed by an unpin to a
        // safe state, must not let `cap()` ratchet upward forever.
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 2,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);

        for cycle in 0..5 {
            let a = test_uri(&format!("cycle{}_a.R", cycle));
            let b = test_uri(&format!("cycle{}_b.R", cycle));
            let c = test_uri(&format!("cycle{}_c.R", cycle));

            assert!(index.insert(a.clone(), make_test_entry(cycle * 3)));
            assert!(index.insert(b.clone(), make_test_entry(cycle * 3 + 1)));

            let mut pinned = HashSet::new();
            pinned.insert(a.clone());
            pinned.insert(b.clone());
            index.set_pinned_uris(pinned);

            assert!(index.insert(c.clone(), make_test_entry(cycle * 3 + 2)));

            // Drain back to a safe state so cap-shrink can fire.
            index.invalidate(&a);
            index.invalidate(&b);
            index.invalidate(&c);
            assert_eq!(index.len(), 0);

            index.set_pinned_uris(HashSet::new());
            assert_eq!(
                index.cap(),
                2,
                "cap must shrink back to user_cap on cycle {}",
                cycle
            );
        }
    }

    #[test]
    fn test_unpinning_restores_normal_eviction() {
        // After clearing the pin set, the cache should evict normally on the
        // next insert that exceeds the configured capacity. This documents
        // that overflow is transient — once entries are no longer pinned,
        // the cache shrinks back toward its cap on subsequent inserts.
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 2,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);

        let uri1 = test_uri("a.R");
        let uri2 = test_uri("b.R");
        let uri3 = test_uri("c.R");
        let uri4 = test_uri("d.R");

        assert!(index.insert(uri1.clone(), make_test_entry(0)));
        assert!(index.insert(uri2.clone(), make_test_entry(1)));

        // Pin both, then exceed cap.
        let mut pinned = HashSet::new();
        pinned.insert(uri1.clone());
        pinned.insert(uri2.clone());
        index.set_pinned_uris(pinned);
        assert!(index.insert(uri3.clone(), make_test_entry(2)));
        assert_eq!(index.len(), 3);

        // Clear pins; next insert must evict an unpinned LRU.
        index.set_pinned_uris(HashSet::new());
        assert!(index.insert(uri4.clone(), make_test_entry(3)));
        // uri1 is the LRU after the prior overflow (uri3 was MRU,
        // then we touched no entries before insert of uri4).
        assert!(!index.contains(&uri1), "LRU non-pinned entry should evict");
        assert!(index.contains(&uri2));
        assert!(index.contains(&uri3));
        assert!(index.contains(&uri4));
    }

    #[test]
    fn test_update_existing_at_capacity() {
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 2,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);

        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");

        assert!(index.insert(uri1.clone(), make_test_entry(0)));
        assert!(index.insert(uri2.clone(), make_test_entry(1)));

        // Updating existing entry should succeed at capacity (no eviction needed)
        let updated_entry = IndexEntry {
            contents: Rope::from_str("y <- 2"),
            ..make_test_entry(2)
        };
        assert!(index.insert(uri1.clone(), updated_entry));

        let retrieved = index.get(&uri1).unwrap();
        assert_eq!(retrieved.contents.to_string(), "y <- 2");
        // Both entries still present
        assert!(index.contains(&uri2));
    }

    #[test]
    fn test_version_monotonicity() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");

        assert_eq!(index.version(), 0);

        index.insert(uri1.clone(), make_test_entry(0));
        assert_eq!(index.version(), 1);

        index.insert(uri2.clone(), make_test_entry(1));
        assert_eq!(index.version(), 2);

        index.invalidate(&uri1);
        assert_eq!(index.version(), 3);

        index.invalidate_all();
        assert_eq!(index.version(), 4);
    }

    #[test]
    fn test_metrics_tracking() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("test.R");
        let uri2 = test_uri("test2.R");

        // Insert
        index.insert(uri.clone(), make_test_entry(0));
        assert_eq!(index.metrics().insertions, 1);

        // Cache hit
        let _ = index.get(&uri);
        assert_eq!(index.metrics().cache_hits, 1);

        // Cache miss
        let _ = index.get(&uri2);
        assert_eq!(index.metrics().cache_misses, 1);

        // Invalidation
        index.invalidate(&uri);
        assert_eq!(index.metrics().invalidations, 1);
    }

    #[test]
    fn test_schedule_update() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("test.R");

        assert_eq!(index.pending_update_count(), 0);

        index.schedule_update(uri.clone());
        assert_eq!(index.pending_update_count(), 1);
        assert_eq!(index.metrics().updates_scheduled, 1);

        // Scheduling same URI again should not increase count
        index.schedule_update(uri.clone());
        assert_eq!(index.pending_update_count(), 1);
        assert_eq!(index.metrics().updates_scheduled, 2);
    }

    #[test]
    fn test_schedule_update_resets_timer() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("test.R");

        // Schedule initial update
        index.schedule_update(uri.clone());
        let first_time = index.get_pending_update_time(&uri).unwrap();

        // Wait a tiny bit
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Schedule again - should reset timer
        index.schedule_update(uri.clone());
        let second_time = index.get_pending_update_time(&uri).unwrap();

        // Second time should be later than first
        assert!(second_time > first_time);
    }

    #[test]
    fn test_schedule_multiple_uris() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");
        let uri3 = test_uri("test3.R");

        index.schedule_update(uri1.clone());
        index.schedule_update(uri2.clone());
        index.schedule_update(uri3.clone());

        assert_eq!(index.pending_update_count(), 3);
        assert!(index.has_pending_update(&uri1));
        assert!(index.has_pending_update(&uri2));
        assert!(index.has_pending_update(&uri3));
    }

    #[test]
    fn test_has_pending_update() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("test.R");
        let other_uri = test_uri("other.R");

        assert!(!index.has_pending_update(&uri));

        index.schedule_update(uri.clone());
        assert!(index.has_pending_update(&uri));
        assert!(!index.has_pending_update(&other_uri));
    }

    #[test]
    fn test_cancel_pending_update() {
        let index = WorkspaceIndex::new(make_test_config());
        let uri = test_uri("test.R");

        // Cancel non-existent should return false
        assert!(!index.cancel_pending_update(&uri));

        // Schedule and then cancel
        index.schedule_update(uri.clone());
        assert!(index.has_pending_update(&uri));
        assert_eq!(index.pending_update_count(), 1);

        assert!(index.cancel_pending_update(&uri));
        assert!(!index.has_pending_update(&uri));
        assert_eq!(index.pending_update_count(), 0);

        // Cancel again should return false
        assert!(!index.cancel_pending_update(&uri));
    }

    #[test]
    fn test_get_ready_updates_respects_debounce() {
        let config = WorkspaceIndexConfig {
            debounce_ms: 100, // 100ms debounce
            max_files: 10,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);
        let uri = test_uri("test.R");
        let open_uris = HashSet::new();

        index.schedule_update(uri.clone());

        // Immediately after scheduling, should not be ready (debounce not elapsed)
        let ready = index.get_ready_updates(&open_uris);
        assert!(ready.is_empty());

        // Wait for debounce period
        std::thread::sleep(std::time::Duration::from_millis(110));

        // Now should be ready
        let ready = index.get_ready_updates(&open_uris);
        assert_eq!(ready.len(), 1);
        assert!(ready.contains(&uri));
    }

    #[test]
    fn test_get_ready_updates_skips_open_uris() {
        let config = WorkspaceIndexConfig {
            debounce_ms: 10, // Short debounce for test
            max_files: 10,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");

        index.schedule_update(uri1.clone());
        index.schedule_update(uri2.clone());

        // Wait for debounce
        std::thread::sleep(std::time::Duration::from_millis(20));

        // Mark uri1 as open
        let mut open_uris = HashSet::new();
        open_uris.insert(uri1.clone());

        // Only uri2 should be ready
        let ready = index.get_ready_updates(&open_uris);
        assert_eq!(ready.len(), 1);
        assert!(ready.contains(&uri2));
        assert!(!ready.contains(&uri1));
    }

    #[tokio::test]
    async fn test_process_update_queue_removes_processed() {
        let config = WorkspaceIndexConfig {
            debounce_ms: 10, // Short debounce for test
            max_files: 10,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);
        let uri = test_uri("test.R");
        let open_uris = HashSet::new();

        index.schedule_update(uri.clone());
        assert_eq!(index.pending_update_count(), 1);

        // Wait for debounce
        std::thread::sleep(std::time::Duration::from_millis(20));

        // Process queue
        let processed = index.process_update_queue(&open_uris).await;
        assert_eq!(processed.len(), 1);
        assert!(processed.contains(&uri));

        // Queue should be empty now
        assert_eq!(index.pending_update_count(), 0);
        assert!(!index.has_pending_update(&uri));
    }

    #[tokio::test]
    async fn test_process_update_queue_skips_open_uris() {
        let config = WorkspaceIndexConfig {
            debounce_ms: 10,
            max_files: 10,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");

        index.schedule_update(uri1.clone());
        index.schedule_update(uri2.clone());

        std::thread::sleep(std::time::Duration::from_millis(20));

        // Mark uri1 as open
        let mut open_uris = HashSet::new();
        open_uris.insert(uri1.clone());

        // Process - should only process uri2
        let processed = index.process_update_queue(&open_uris).await;
        assert_eq!(processed.len(), 1);
        assert!(processed.contains(&uri2));

        // uri1 should still be pending
        assert!(index.has_pending_update(&uri1));
        assert!(!index.has_pending_update(&uri2));
    }

    #[tokio::test]
    async fn test_process_update_queue_updates_metrics() {
        let config = WorkspaceIndexConfig {
            debounce_ms: 10,
            max_files: 10,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");
        let open_uris = HashSet::new();

        index.schedule_update(uri1.clone());
        index.schedule_update(uri2.clone());

        std::thread::sleep(std::time::Duration::from_millis(20));

        let _ = index.process_update_queue(&open_uris).await;

        let metrics = index.metrics();
        assert_eq!(metrics.updates_scheduled, 2);
        assert_eq!(metrics.updates_processed, 2);
    }

    #[tokio::test]
    async fn test_debounce_batching() {
        // Test that rapid updates for the same URI result in only one processing
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 10,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);
        let uri = test_uri("test.R");
        let open_uris = HashSet::new();

        // Schedule multiple rapid updates
        for _ in 0..5 {
            index.schedule_update(uri.clone());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Should still only have 1 pending update
        assert_eq!(index.pending_update_count(), 1);

        // Wait for debounce
        std::thread::sleep(std::time::Duration::from_millis(60));

        // Process - should only process once
        let processed = index.process_update_queue(&open_uris).await;
        assert_eq!(processed.len(), 1);
        assert_eq!(index.metrics().updates_processed, 1);
    }

    #[tokio::test]
    async fn test_debounce_timer_reset_delays_processing() {
        let config = WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 10,
            max_file_size_bytes: 1024,
        };
        let index = WorkspaceIndex::new(config);
        let uri = test_uri("test.R");
        let open_uris = HashSet::new();

        // Schedule initial update
        index.schedule_update(uri.clone());

        // Wait 30ms (not enough for debounce)
        std::thread::sleep(std::time::Duration::from_millis(30));

        // Schedule again - resets timer
        index.schedule_update(uri.clone());

        // Wait another 30ms (60ms total, but only 30ms since last schedule)
        std::thread::sleep(std::time::Duration::from_millis(30));

        // Should NOT be ready yet (timer was reset)
        let ready = index.get_ready_updates(&open_uris);
        assert!(ready.is_empty());

        // Wait another 30ms (60ms since last schedule)
        std::thread::sleep(std::time::Duration::from_millis(30));

        // Now should be ready
        let ready = index.get_ready_updates(&open_uris);
        assert_eq!(ready.len(), 1);
    }

    #[tokio::test]
    async fn test_process_empty_queue() {
        let index = WorkspaceIndex::new(make_test_config());
        let open_uris = HashSet::new();

        // Processing empty queue should return empty vec
        let processed = index.process_update_queue(&open_uris).await;
        assert!(processed.is_empty());
        assert_eq!(index.metrics().updates_processed, 0);
    }

    #[test]
    fn test_default_impl() {
        let index = WorkspaceIndex::default();
        assert!(index.is_empty());
        assert_eq!(index.config().debounce_ms, 200);
        assert_eq!(index.config().max_files, 1000);
    }

    // ========================================================================
    // Property-Based Tests
    // ========================================================================

    use proptest::prelude::*;

    /// Operations that can modify the WorkspaceIndex
    #[derive(Debug, Clone)]
    enum IndexOperation {
        /// Insert an entry at the given URI index
        Insert(usize),
        /// Invalidate an entry at the given URI index
        Invalidate(usize),
        /// Invalidate all entries
        InvalidateAll,
    }

    /// Strategy to generate a sequence of index operations
    fn index_operation_sequence_strategy(
        max_uri_idx: usize,
    ) -> impl Strategy<Value = Vec<IndexOperation>> {
        prop::collection::vec(
            prop_oneof![
                // Insert operations: generate URI indices
                (0..max_uri_idx).prop_map(IndexOperation::Insert),
                // Invalidate operations: generate URI indices
                (0..max_uri_idx).prop_map(IndexOperation::Invalidate),
                // InvalidateAll operations
                Just(IndexOperation::InvalidateAll),
            ],
            10..50,
        )
    }

    /// Helper to create a URI from an index
    fn uri_from_idx(idx: usize) -> Url {
        Url::parse(&format!("file:///test{}.R", idx)).unwrap()
    }

    /// Helper to create a test entry for property tests
    fn make_prop_test_entry(version: u64) -> IndexEntry {
        IndexEntry {
            contents: Rope::from_str("x <- 1"),
            tree: None,
            loaded_packages: vec![],
            data_packages: vec![],
            snapshot: FileSnapshot {
                mtime: SystemTime::UNIX_EPOCH,
                size: 6,
                content_hash: Some(12345),
            },
            metadata: std::sync::Arc::new(CrossFileMetadata::default()),
            artifacts: Arc::new(ScopeArtifacts::default()),
            indexed_at_version: version,
        }
    }

    /// Operations for debounce batching property test
    #[derive(Debug, Clone)]
    enum DebounceOperation {
        /// Schedule an update for a URI (identified by index)
        ScheduleUpdate(usize),
        /// Wait for a short time (simulates rapid updates)
        ShortWait,
        /// Wait for debounce period to elapse
        WaitDebounce,
    }

    /// Strategy to generate a sequence of debounce operations
    /// Generates sequences that include rapid updates to the same URI
    fn debounce_operation_sequence_strategy(
        max_uri_idx: usize,
    ) -> impl Strategy<Value = Vec<DebounceOperation>> {
        prop::collection::vec(
            prop_oneof![
                // Schedule update operations (weighted higher for more rapid updates)
                3 => (0..max_uri_idx).prop_map(DebounceOperation::ScheduleUpdate),
                // Short waits (less than debounce period)
                2 => Just(DebounceOperation::ShortWait),
                // Wait for debounce period
                1 => Just(DebounceOperation::WaitDebounce),
            ],
            10..30,
        )
    }

    // Feature: workspace-index-consolidation, Property 5: Debounce Batching
    // **Validates: Requirements 5.1, 5.2, 5.3**
    //
    // Property: For any sequence of rapid schedule_update calls for the same URI
    // within debounce_ms, only one actual update SHALL be performed.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 5: Debounce Batching
        ///
        /// For any sequence of rapid schedule_update calls for the same URI
        /// within debounce_ms, only one actual update SHALL be performed.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.3**
        #[test]
        fn prop_debounce_batching(
            num_uris in 1usize..=5,
            ops in debounce_operation_sequence_strategy(5)
        ) {
            // Use a runtime for async operations
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();

            rt.block_on(async {
                // Pause time so tests run instantly with simulated time
                tokio::time::pause();

                // Use a short debounce for faster tests
                let debounce_ms = 50u64;
                let config = WorkspaceIndexConfig {
                    debounce_ms,
                    max_files: 100,
                    max_file_size_bytes: 1024,
                };
                let index = WorkspaceIndex::new(config);
                let open_uris = HashSet::new();

                // Track how many times each URI was scheduled
                let mut schedule_counts: std::collections::HashMap<Url, usize> = std::collections::HashMap::new();
                // Track how many times each URI was processed
                let mut process_counts: std::collections::HashMap<Url, usize> = std::collections::HashMap::new();

                for op in &ops {
                    match op {
                        DebounceOperation::ScheduleUpdate(idx) => {
                            let uri = uri_from_idx(*idx % num_uris);
                            index.schedule_update(uri.clone());
                            *schedule_counts.entry(uri).or_insert(0) += 1;
                        }
                        DebounceOperation::ShortWait => {
                            // Wait less than debounce period
                            tokio::time::sleep(std::time::Duration::from_millis(debounce_ms / 5)).await;
                        }
                        DebounceOperation::WaitDebounce => {
                            // Wait for debounce period to elapse
                            tokio::time::sleep(std::time::Duration::from_millis(debounce_ms + 10)).await;

                            // Process the queue
                            let processed = index.process_update_queue(&open_uris).await;
                            for uri in processed {
                                *process_counts.entry(uri).or_insert(0) += 1;
                            }
                        }
                    }
                }

                // Final processing after all operations
                tokio::time::sleep(std::time::Duration::from_millis(debounce_ms + 10)).await;
                let final_processed = index.process_update_queue(&open_uris).await;
                for uri in final_processed {
                    *process_counts.entry(uri).or_insert(0) += 1;
                }

                // Property verification:
                // 1. Each URI that was scheduled should be processed at least once
                //    (unless it was scheduled after the final processing)
                // 2. The number of times a URI is processed should be <= number of
                //    "batches" (groups of rapid updates separated by debounce waits)
                // 3. Pending update count should be 0 after final processing
                prop_assert_eq!(
                    index.pending_update_count(),
                    0,
                    "Pending updates should be 0 after final processing"
                );

                // For each URI that was scheduled, verify batching occurred
                for (uri, scheduled_count) in &schedule_counts {
                    let processed_count = process_counts.get(uri).copied().unwrap_or(0);

                    // Key property: processed_count should be much less than scheduled_count
                    // when there are rapid updates (batching is working)
                    // At minimum, processed_count should be >= 1 if scheduled_count >= 1
                    if *scheduled_count > 0 {
                        prop_assert!(
                            processed_count >= 1,
                            "URI {:?} was scheduled {} times but never processed",
                            uri,
                            scheduled_count
                        );
                    }

                    // The number of processed updates should be <= scheduled updates
                    // (batching means we process fewer times than we schedule)
                    prop_assert!(
                        processed_count <= *scheduled_count,
                        "URI {:?} was processed {} times but only scheduled {} times",
                        uri,
                        processed_count,
                        scheduled_count
                    );
                }

                Ok(())
            })?;
        }

        /// Property 5b: Debounce Timer Reset
        ///
        /// For any URI, scheduling an update while one is pending SHALL reset
        /// the debounce timer, delaying processing.
        ///
        /// **Validates: Requirements 5.1, 5.2**
        #[test]
        fn prop_debounce_timer_reset(
            num_rapid_updates in 2usize..=10
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();

            rt.block_on(async {
                // Pause time so tests run instantly with simulated time
                tokio::time::pause();

                let debounce_ms = 100u64;
                let config = WorkspaceIndexConfig {
                    debounce_ms,
                    max_files: 100,
                    max_file_size_bytes: 1024,
                };
                let index = WorkspaceIndex::new(config);
                let uri = uri_from_idx(0);
                let open_uris = HashSet::new();

                // Schedule rapid updates with short waits between them
                // Each update should reset the timer
                for _ in 0..num_rapid_updates {
                    index.schedule_update(uri.clone());
                    // Wait less than debounce period
                    tokio::time::sleep(std::time::Duration::from_millis(debounce_ms / 4)).await;
                }

                // Immediately after the last rapid update, check if ready
                // Should NOT be ready because timer was just reset
                let ready = index.get_ready_updates(&open_uris);
                prop_assert!(
                    ready.is_empty(),
                    "URI should not be ready immediately after rapid updates (timer should have been reset)"
                );

                // Should still have exactly 1 pending update (batched)
                prop_assert_eq!(
                    index.pending_update_count(),
                    1,
                    "Should have exactly 1 pending update after {} rapid updates",
                    num_rapid_updates
                );

                // Wait for full debounce period
                tokio::time::sleep(std::time::Duration::from_millis(debounce_ms + 10)).await;

                // Now should be ready
                let ready = index.get_ready_updates(&open_uris);
                prop_assert_eq!(
                    ready.len(),
                    1,
                    "URI should be ready after debounce period elapsed"
                );

                // Process and verify only one update
                let processed = index.process_update_queue(&open_uris).await;
                prop_assert_eq!(
                    processed.len(),
                    1,
                    "Should process exactly 1 update after {} rapid schedule_update calls",
                    num_rapid_updates
                );

                Ok(())
            })?;
        }

        /// Property 5c: Multiple URIs Debounce Independence
        ///
        /// For any set of URIs, debouncing for one URI SHALL NOT affect
        /// the debounce timing of other URIs.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.3**
        #[test]
        fn prop_debounce_uri_independence(
            num_uris in 2usize..=5,
            updates_per_uri in 1usize..=5
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();

            rt.block_on(async {
                // Pause time so tests run instantly with simulated time
                tokio::time::pause();

                let debounce_ms = 50u64;
                let config = WorkspaceIndexConfig {
                    debounce_ms,
                    max_files: 100,
                    max_file_size_bytes: 1024,
                };
                let index = WorkspaceIndex::new(config);
                let open_uris = HashSet::new();

                // Schedule updates for multiple URIs
                for uri_idx in 0..num_uris {
                    let uri = uri_from_idx(uri_idx);
                    for _ in 0..updates_per_uri {
                        index.schedule_update(uri.clone());
                    }
                }

                // Should have exactly num_uris pending (one per URI, batched)
                prop_assert_eq!(
                    index.pending_update_count(),
                    num_uris,
                    "Should have {} pending updates (one per URI)",
                    num_uris
                );

                // Wait for debounce
                tokio::time::sleep(std::time::Duration::from_millis(debounce_ms + 10)).await;

                // Process all
                let processed = index.process_update_queue(&open_uris).await;

                // Should process exactly num_uris (one per URI)
                prop_assert_eq!(
                    processed.len(),
                    num_uris,
                    "Should process exactly {} URIs (one per URI)",
                    num_uris
                );

                // Verify each URI was processed exactly once
                let processed_set: std::collections::HashSet<_> = processed.into_iter().collect();
                for uri_idx in 0..num_uris {
                    let uri = uri_from_idx(uri_idx);
                    prop_assert!(
                        processed_set.contains(&uri),
                        "URI {} should have been processed",
                        uri_idx
                    );
                }

                Ok(())
            })?;
        }
    }

    // Feature: workspace-index-consolidation, Property 4: Version Monotonicity
    // **Validates: Requirements 4.4, 9.3, 12.3**
    //
    // Property: For any sequence of modification operations on WorkspaceIndex,
    // the version counter SHALL strictly increase after each operation.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 4: Version Monotonicity
        ///
        /// For any sequence of modification operations on WorkspaceIndex,
        /// the version counter SHALL strictly increase after each operation.
        ///
        /// **Validates: Requirements 4.4, 9.3, 12.3**
        #[test]
        fn prop_version_monotonicity(
            max_files in 5usize..=20,
            ops in index_operation_sequence_strategy(10)
        ) {
            let config = WorkspaceIndexConfig {
                debounce_ms: 50,
                max_files,
                max_file_size_bytes: 1024,
            };
            let index = WorkspaceIndex::new(config);

            // Track the previous version
            let mut prev_version = index.version();

            // Track which URIs are currently in the index (for determining if operations modify state)
            let mut indexed_uris: std::collections::HashSet<Url> = std::collections::HashSet::new();

            for op in ops {
                let version_before = index.version();

                match op {
                    IndexOperation::Insert(idx) => {
                        let uri = uri_from_idx(idx);
                        let entry = make_prop_test_entry(version_before);

                        let inserted = index.insert(uri.clone(), entry);

                        // With LRU eviction, insert always succeeds (no file size issue)
                        prop_assert!(
                            inserted,
                            "Insert should always succeed with LRU eviction"
                        );

                        // Insert succeeded - version MUST have increased
                        let version_after = index.version();
                        prop_assert!(
                            version_after > version_before,
                            "Version did not increase after successful insert: before={}, after={}",
                            version_before,
                            version_after
                        );
                        prop_assert!(
                            version_after > prev_version,
                            "Version is not monotonically increasing: prev={}, current={}",
                            prev_version,
                            version_after
                        );
                        prev_version = version_after;
                        indexed_uris.insert(uri);

                        // With LRU, old entries may have been evicted — trim our tracking set
                        // to match the actual index state
                        if indexed_uris.len() > max_files {
                            // Some entries were evicted; sync our tracking
                            indexed_uris.retain(|u| index.contains(u));
                        }
                    }
                    IndexOperation::Invalidate(idx) => {
                        let uri = uri_from_idx(idx);

                        let removed = index.invalidate(&uri);

                        if removed {
                            // Invalidate succeeded - version MUST have increased
                            let version_after = index.version();
                            prop_assert!(
                                version_after > version_before,
                                "Version did not increase after successful invalidate: before={}, after={}",
                                version_before,
                                version_after
                            );
                            prop_assert!(
                                version_after > prev_version,
                                "Version is not monotonically increasing: prev={}, current={}",
                                prev_version,
                                version_after
                            );
                            prev_version = version_after;
                            indexed_uris.remove(&uri);
                        } else {
                            // Invalidate failed (entry not present) - version should NOT change
                            let version_after = index.version();
                            prop_assert_eq!(
                                version_after,
                                version_before,
                                "Version changed after failed invalidate"
                            );
                        }
                    }
                    IndexOperation::InvalidateAll => {
                        index.invalidate_all();

                        let version_after = index.version();

                        // invalidate_all always increments version
                        prop_assert!(
                            version_after > version_before,
                            "Version did not increase after invalidate_all: before={}, after={}",
                            version_before,
                            version_after
                        );
                        prop_assert!(
                            version_after > prev_version,
                            "Version is not monotonically increasing: prev={}, current={}",
                            prev_version,
                            version_after
                        );
                        prev_version = version_after;

                        indexed_uris.clear();
                    }
                }

                // Invariant: version should never decrease
                prop_assert!(
                    index.version() >= prev_version,
                    "Version decreased: prev={}, current={}",
                    prev_version,
                    index.version()
                );
            }
        }
    }
}
