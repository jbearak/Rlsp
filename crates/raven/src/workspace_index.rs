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
    /// Index entries by URI (LRU-bounded)
    inner: RwLock<LruCache<Url, IndexEntry>>,
    /// Monotonic version counter
    version: AtomicU64,
    /// Configuration
    config: WorkspaceIndexConfig,
    /// Pending debounced updates (URI -> scheduled time)
    pending_updates: RwLock<std::collections::HashMap<Url, Instant>>,
    /// Update queue for batched processing
    update_queue: RwLock<HashSet<Url>>,
    /// Metrics
    metrics: RwLock<WorkspaceIndexMetrics>,
    /// URIs protected from LRU eviction.
    ///
    /// Mirrors the open documents' graph neighborhood so closed-but-reachable
    /// neighbors are not silently dropped under cache pressure, which would
    /// force `compute_artifacts_with_metadata`
    /// recomputation on the fallback path. Lock order: when both `inner`
    /// and `pinned` need to be held simultaneously, acquire `inner` first.
    pinned: RwLock<HashSet<Url>>,
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
        Self {
            inner: RwLock::new(LruCache::new(cap)),
            version: AtomicU64::new(0),
            config,
            pending_updates: RwLock::new(std::collections::HashMap::new()),
            update_queue: RwLock::new(HashSet::new()),
            metrics: RwLock::new(WorkspaceIndexMetrics::default()),
            pinned: RwLock::new(HashSet::new()),
        }
    }

    /// Effective runtime cap for a config — `max_files`, normalized to
    /// `NonZeroUsize` with the same default `new()` applies. Shared by
    /// the constructor and the shrink-back path so both interpret
    /// `max_files == 0` identically.
    fn effective_cap_for(config: &WorkspaceIndexConfig) -> NonZeroUsize {
        crate::cross_file::cache::non_zero_or(config.max_files, 1000)
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
    /// Lock order: acquires `inner.write()` before `pinned.write()`,
    /// matching the order established by `pin_aware_push` (which holds
    /// `inner.write()` and then takes `pinned.read()`).
    pub fn set_pinned_uris(&self, uris: HashSet<Url>) {
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        if let Ok(mut pinned) = self.pinned.write() {
            *pinned = uris;
        }

        let user_cap_nz = Self::effective_cap_for(&self.config);
        let user_cap = user_cap_nz.get();
        if guard.cap().get() > user_cap && guard.len() <= user_cap {
            guard.resize(user_cap_nz);
        }
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
        let entry = guard.peek(uri).cloned();

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
        guard.peek(uri).and_then(|entry| {
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
        guard.peek(uri).map(|entry| entry.metadata.clone())
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
        guard.peek(uri).map(|entry| entry.artifacts.clone())
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
            .map(|guard| guard.contains(uri))
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
            .map(|guard| guard.iter().map(|(k, _)| k.clone()).collect())
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
            .map(|guard| guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
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
            .map(|guard| guard.iter().any(|(_, v)| pred(v)))
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
        self.version.load(Ordering::SeqCst)
    }

    /// Get the number of indexed entries
    pub fn len(&self) -> usize {
        self.inner.read().map(|guard| guard.len()).unwrap_or(0)
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
            .map(|guard| guard.cap().get())
            .unwrap_or(0)
    }

    /// Snapshot the URIs protected from LRU eviction.
    pub(crate) fn pinned_uris(&self) -> HashSet<Url> {
        self.pinned
            .read()
            .map(|pinned| pinned.clone())
            .unwrap_or_default()
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
        // Check max_file_size_bytes limit for all entries (cheap; no need
        // to acquire the inner lock for an oversized rejection).
        if self.config.max_file_size_bytes > 0
            && entry.snapshot.size > self.config.max_file_size_bytes as u64
        {
            log::info!(
                "WorkspaceIndex rejecting oversized file {} ({} bytes > {} limit)",
                uri,
                entry.snapshot.size,
                self.config.max_file_size_bytes
            );
            return false;
        }

        let Ok(mut guard) = self.inner.write() else {
            return false;
        };

        crate::cross_file::cache::pin_aware_push(&mut guard, &self.pinned, uri, entry);
        drop(guard);

        // Increment version counter
        self.version.fetch_add(1, Ordering::SeqCst);

        // Update metrics
        if let Ok(mut metrics) = self.metrics.write() {
            metrics.insertions += 1;
        }

        true
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

        let removed = guard.pop(uri).is_some();
        drop(guard);

        if removed {
            // Increment version counter
            self.version.fetch_add(1, Ordering::SeqCst);

            // Update metrics
            if let Ok(mut metrics) = self.metrics.write() {
                metrics.invalidations += 1;
            }
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

        let count = guard.len();
        guard.clear();
        drop(guard);

        // Always increment version counter
        self.version.fetch_add(1, Ordering::SeqCst);

        // Update metrics
        if count > 0
            && let Ok(mut metrics) = self.metrics.write()
        {
            metrics.invalidations += count as u64;
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
