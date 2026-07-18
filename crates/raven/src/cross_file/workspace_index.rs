//
// cross_file/workspace_index.rs
//
// Workspace index for cross-file awareness
//

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use lru::LruCache;
use tower_lsp::lsp_types::Url;

use super::cache::non_zero_or;
use super::file_cache::FileSnapshot;
use super::scope::ScopeArtifacts;
use super::types::CrossFileMetadata;

/// Entry in the workspace index
#[derive(Debug, Clone)]
pub struct IndexEntry {
    /// File snapshot for freshness checking
    pub snapshot: FileSnapshot,
    /// Extracted cross-file metadata
    pub metadata: Arc<CrossFileMetadata>,
    /// Computed scope artifacts
    pub artifacts: Arc<ScopeArtifacts>,
    /// Index version when this entry was created
    pub indexed_at_version: u64,
}

/// Default capacity for the cross-file workspace index
const DEFAULT_WORKSPACE_INDEX_CAPACITY: usize = 5000;

/// Workspace index for closed files with LRU eviction.
///
/// Uses `peek()` for reads (no LRU promotion, works under read lock) and
/// `push()` for writes (promotes/evicts under write lock).
pub struct CrossFileWorkspaceIndex {
    /// Index entries by URI (LRU-bounded)
    inner: RwLock<LruCache<Url, IndexEntry>>,
    /// Monotonic version counter
    version: AtomicU64,
    /// URIs protected from LRU eviction.
    ///
    /// Mirrors the open documents' graph neighborhood so closed-but-reachable
    /// neighbors are not silently dropped under cache pressure. Lock order:
    /// acquire `inner` before `pinned` when both are needed.
    pinned: RwLock<HashSet<Url>>,
    /// User-configured baseline capacity.
    ///
    /// `inner.cap()` may temporarily exceed this when an all-pinned
    /// overflow forces growth (see `pin_aware_push`). `set_pinned_uris`
    /// uses this value to opportunistically shrink the cache back when
    /// `len() <= user_cap` so the runtime cap doesn't ratchet upward
    /// across repeated overflow/unpin cycles (issue #128).
    ///
    /// Mutable so `resize()` can keep it in sync with the new runtime
    /// cap; otherwise a post-resize shrink-back would silently revert
    /// to the constructor value rather than the configured one. Stored
    /// as `AtomicUsize` and updated under `inner.write()` so writers
    /// see a consistent (`inner.cap()`, `user_cap`) pair.
    user_cap: AtomicUsize,
}

impl std::fmt::Debug for CrossFileWorkspaceIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossFileWorkspaceIndex")
            .finish_non_exhaustive()
    }
}

impl Default for CrossFileWorkspaceIndex {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_WORKSPACE_INDEX_CAPACITY)
    }
}

impl CrossFileWorkspaceIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(cap: usize) -> Self {
        let cap_nz = non_zero_or(cap, DEFAULT_WORKSPACE_INDEX_CAPACITY);
        Self {
            inner: RwLock::new(LruCache::new(cap_nz)),
            version: AtomicU64::new(0),
            pinned: RwLock::new(HashSet::new()),
            user_cap: AtomicUsize::new(cap_nz.get()),
        }
    }

    /// Replace the set of URIs protected from LRU eviction.
    ///
    /// Pinned URIs are skipped during eviction; if every in-cache URI is
    /// pinned, the cache is allowed to grow past its configured capacity.
    /// Explicit `invalidate` and `update_from_disk` still work for pinned
    /// URIs (the open-document gate in `update_from_disk` is unchanged).
    ///
    /// Also opportunistically shrinks the runtime cap back to `user_cap`
    /// when `len() <= user_cap`, so repeated all-pinned overflow events
    /// followed by safe unpins don't ratchet the cap upward indefinitely
    /// (issue #128). The shrink only fires when it can't itself force
    /// eviction.
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

        let user_cap = self.user_cap.load(Ordering::Relaxed);
        if let Some(user_cap_nz) = NonZeroUsize::new(user_cap)
            && guard.cap().get() > user_cap
            && guard.len() <= user_cap
        {
            guard.resize(user_cap_nz);
        }
    }

    /// Get current version
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Increment version and return new value
    pub fn increment_version(&self) -> u64 {
        self.version.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Get index entry if it exists and is fresh
    pub fn get_if_fresh(&self, uri: &Url, current_snapshot: &FileSnapshot) -> Option<IndexEntry> {
        let guard = self.inner.read().ok()?;
        guard.peek(uri).and_then(|entry| {
            if entry.snapshot.matches_disk(current_snapshot) {
                Some(entry.clone())
            } else {
                None
            }
        })
    }

    /// Get metadata for a URI (without freshness check)
    pub fn get_metadata(&self, uri: &Url) -> Option<Arc<CrossFileMetadata>> {
        self.inner
            .read()
            .ok()?
            .peek(uri)
            .map(|e| e.metadata.clone())
    }

    /// Get artifacts for a URI (without freshness check)
    pub fn get_artifacts(&self, uri: &Url) -> Option<Arc<ScopeArtifacts>> {
        self.inner
            .read()
            .ok()?
            .peek(uri)
            .map(|e| e.artifacts.clone())
    }

    /// Update index entry for a URI.
    ///
    /// CRITICAL: If the URI is currently open, this is a no-op.
    /// Open documents are authoritative; disk changes are ignored until close.
    pub fn update_from_disk(
        &self,
        uri: &Url,
        open_documents: &HashSet<Url>,
        snapshot: FileSnapshot,
        metadata: CrossFileMetadata,
        artifacts: Arc<ScopeArtifacts>,
    ) {
        if open_documents.contains(uri) {
            log::trace!("Skipping disk update for open document: {}", uri);
            return;
        }

        let version = self.increment_version();
        let entry = IndexEntry {
            snapshot,
            metadata: Arc::new(metadata),
            artifacts,
            indexed_at_version: version,
        };

        if let Ok(mut guard) = self.inner.write() {
            super::cache::pin_aware_push(&mut guard, &self.pinned, uri.clone(), entry);
        }
    }

    /// Insert entry directly (for testing or when open-docs check is done elsewhere)
    pub fn insert(&self, uri: Url, entry: IndexEntry) {
        self.increment_version();
        if let Ok(mut guard) = self.inner.write() {
            super::cache::pin_aware_push(&mut guard, &self.pinned, uri, entry);
        }
    }

    /// Invalidate index entry for a URI
    pub fn invalidate(&self, uri: &Url) {
        self.increment_version();
        if let Ok(mut guard) = self.inner.write() {
            guard.pop(uri);
        }
    }

    /// Invalidate all entries
    pub fn invalidate_all(&self) {
        self.increment_version();
        if let Ok(mut guard) = self.inner.write() {
            guard.clear();
        }
    }

    /// Check if URI is in index
    pub fn contains(&self, uri: &Url) -> bool {
        self.inner
            .read()
            .ok()
            .map(|g| g.contains(uri))
            .unwrap_or(false)
    }

    /// Get all indexed URIs
    pub fn uris(&self) -> Vec<Url> {
        self.inner
            .read()
            .ok()
            .map(|g| g.iter().map(|(k, _)| k.clone()).collect())
            .unwrap_or_default()
    }

    /// Get the current cache capacity.
    ///
    /// May exceed the user-configured capacity after an all-pinned
    /// overflow has forced the underlying LRU to grow.
    pub fn cap(&self) -> usize {
        self.inner.read().ok().map(|g| g.cap().get()).unwrap_or(0)
    }

    /// Resize the cache capacity. If shrinking, LRU entries are evicted.
    ///
    /// Also updates the `user_cap` baseline so `set_pinned_uris` will
    /// shrink back to this resized value after a future all-pinned
    /// overflow, rather than reverting to the constructor capacity.
    /// `inner` and `user_cap` are written together under
    /// `inner.write()` so concurrent readers see a consistent pair.
    pub fn resize(&self, cap: usize) {
        let cap = non_zero_or(cap, DEFAULT_WORKSPACE_INDEX_CAPACITY);
        if let Ok(mut guard) = self.inner.write() {
            guard.resize(cap);
            self.user_cap.store(cap.get(), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///{}", name)).unwrap()
    }

    fn test_snapshot() -> FileSnapshot {
        FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 100,
            content_hash: None,
        }
    }

    fn test_entry(version: u64) -> IndexEntry {
        IndexEntry {
            snapshot: test_snapshot(),
            metadata: std::sync::Arc::new(CrossFileMetadata::default()),
            artifacts: Arc::new(ScopeArtifacts::default()),
            indexed_at_version: version,
        }
    }

    #[test]
    fn test_version_monotonic() {
        let index = CrossFileWorkspaceIndex::new();
        let v1 = index.version();
        let v2 = index.increment_version();
        let v3 = index.increment_version();

        assert!(v2 > v1);
        assert!(v3 > v2);
    }

    #[test]
    fn test_insert_and_get() {
        let index = CrossFileWorkspaceIndex::new();
        let uri = test_uri("test.R");

        let entry = test_entry(1);
        let expected_artifacts = entry.artifacts.clone();
        index.insert(uri.clone(), entry);

        assert!(index.get_metadata(&uri).is_some());
        let actual_artifacts = index.get_artifacts(&uri).unwrap();
        // Verify pointer sharing: get_artifacts should return the same Arc allocation,
        // not a deep clone (regression test for O(1) Arc sharing).
        assert!(
            Arc::ptr_eq(&actual_artifacts, &expected_artifacts),
            "get_artifacts should return the same Arc, not a deep clone"
        );
    }

    #[test]
    fn test_get_if_fresh() {
        let index = CrossFileWorkspaceIndex::new();
        let uri = test_uri("test.R");
        let snapshot = test_snapshot();

        index.insert(uri.clone(), test_entry(1));

        // Same snapshot should return entry
        assert!(index.get_if_fresh(&uri, &snapshot).is_some());

        // Different snapshot should return None
        let new_snapshot = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 200,
            content_hash: None,
        };
        assert!(index.get_if_fresh(&uri, &new_snapshot).is_none());
    }

    #[test]
    fn test_update_from_disk_skips_open() {
        let index = CrossFileWorkspaceIndex::new();
        let uri = test_uri("test.R");
        let mut open_docs = HashSet::new();
        open_docs.insert(uri.clone());

        // Should be skipped because document is open
        index.update_from_disk(
            &uri,
            &open_docs,
            test_snapshot(),
            CrossFileMetadata::default(),
            Arc::new(ScopeArtifacts::default()),
        );

        assert!(!index.contains(&uri));
    }

    #[test]
    fn test_update_from_disk_succeeds_when_closed() {
        let index = CrossFileWorkspaceIndex::new();
        let uri = test_uri("test.R");
        let open_docs = HashSet::new(); // Empty - no open docs

        index.update_from_disk(
            &uri,
            &open_docs,
            test_snapshot(),
            CrossFileMetadata::default(),
            Arc::new(ScopeArtifacts::default()),
        );

        assert!(index.contains(&uri));
    }

    #[test]
    fn test_invalidate() {
        let index = CrossFileWorkspaceIndex::new();
        let uri = test_uri("test.R");

        index.insert(uri.clone(), test_entry(1));
        assert!(index.contains(&uri));

        index.invalidate(&uri);
        assert!(!index.contains(&uri));
    }

    #[test]
    fn test_version_increments_on_operations() {
        let index = CrossFileWorkspaceIndex::new();
        let uri = test_uri("test.R");

        let v1 = index.version();
        index.insert(uri.clone(), test_entry(1));
        let v2 = index.version();
        index.invalidate(&uri);
        let v3 = index.version();

        assert!(v2 > v1);
        assert!(v3 > v2);
    }

    #[test]
    fn test_lru_eviction() {
        let index = CrossFileWorkspaceIndex::with_capacity(2);
        let uri1 = test_uri("a.R");
        let uri2 = test_uri("b.R");
        let uri3 = test_uri("c.R");

        index.insert(uri1.clone(), test_entry(1));
        index.insert(uri2.clone(), test_entry(2));

        assert!(index.contains(&uri1));
        assert!(index.contains(&uri2));

        // Third insert evicts uri1 (oldest by insertion time)
        index.insert(uri3.clone(), test_entry(3));
        assert!(!index.contains(&uri1), "LRU entry should be evicted");
        assert!(index.contains(&uri2));
        assert!(index.contains(&uri3));
    }

    #[test]
    fn test_pinned_uris_are_protected_from_eviction() {
        // Cache at capacity, pinned URI is the LRU candidate.
        // Insert of a new entry must evict an unpinned LRU instead of the pin.
        let index = CrossFileWorkspaceIndex::with_capacity(2);
        let uri1 = test_uri("pinned.R");
        let uri2 = test_uri("lru_unpinned.R");
        let uri3 = test_uri("mru.R");

        index.insert(uri1.clone(), test_entry(1));
        index.insert(uri2.clone(), test_entry(2));

        let mut pinned = HashSet::new();
        pinned.insert(uri1.clone());
        index.set_pinned_uris(pinned);

        index.insert(uri3.clone(), test_entry(3));

        assert!(index.contains(&uri1), "pinned URI must not be evicted");
        assert!(
            !index.contains(&uri2),
            "least-recently-used unpinned URI must be evicted"
        );
        assert!(index.contains(&uri3));
    }

    #[test]
    fn test_pinned_uris_can_exceed_capacity() {
        // When every in-cache entry is pinned, eviction is skipped and the
        // cache is allowed to grow past its configured capacity rather than
        // evict a reachable neighbor of an open document.
        let index = CrossFileWorkspaceIndex::with_capacity(2);
        let uri1 = test_uri("a.R");
        let uri2 = test_uri("b.R");
        let uri3 = test_uri("c.R");

        index.insert(uri1.clone(), test_entry(1));
        index.insert(uri2.clone(), test_entry(2));

        let mut pinned = HashSet::new();
        pinned.insert(uri1.clone());
        pinned.insert(uri2.clone());
        index.set_pinned_uris(pinned);

        index.insert(uri3.clone(), test_entry(3));

        assert!(index.contains(&uri1));
        assert!(index.contains(&uri2));
        assert!(index.contains(&uri3));
        assert_eq!(index.uris().len(), 3);
    }

    #[test]
    fn test_cap_shrinks_back_to_user_cap_when_safe() {
        // Issue #128: after an all-pinned overflow grew the cap, calling
        // `set_pinned_uris` while `uris().len() <= user_cap` should
        // restore the cap to its configured value.
        let index = CrossFileWorkspaceIndex::with_capacity(2);
        let uri1 = test_uri("a.R");
        let uri2 = test_uri("b.R");
        let uri3 = test_uri("c.R");

        index.insert(uri1.clone(), test_entry(1));
        index.insert(uri2.clone(), test_entry(2));

        let mut pinned = HashSet::new();
        pinned.insert(uri1.clone());
        pinned.insert(uri2.clone());
        index.set_pinned_uris(pinned);
        index.insert(uri3.clone(), test_entry(3));
        assert_eq!(index.cap(), 3, "all-pinned overflow grew the cap");
        assert_eq!(index.uris().len(), 3);

        // Drop one entry so len falls back to user_cap.
        index.invalidate(&uri3);
        assert_eq!(index.uris().len(), 2);

        // Clearing the pin set should now opportunistically shrink cap.
        index.set_pinned_uris(HashSet::new());
        assert_eq!(index.cap(), 2, "cap should shrink back to user_cap");
    }

    #[test]
    fn test_cap_does_not_shrink_when_len_still_exceeds_user_cap() {
        // Issue #128: shrinking must never force eviction.
        let index = CrossFileWorkspaceIndex::with_capacity(2);
        let uri1 = test_uri("a.R");
        let uri2 = test_uri("b.R");
        let uri3 = test_uri("c.R");

        index.insert(uri1.clone(), test_entry(1));
        index.insert(uri2.clone(), test_entry(2));

        let mut pinned = HashSet::new();
        pinned.insert(uri1.clone());
        pinned.insert(uri2.clone());
        index.set_pinned_uris(pinned);
        index.insert(uri3.clone(), test_entry(3));
        assert_eq!(index.cap(), 3);
        assert_eq!(index.uris().len(), 3);

        // Clear pins; len(3) > user_cap(2) so cap must remain 3.
        index.set_pinned_uris(HashSet::new());
        assert_eq!(index.cap(), 3, "cap must not shrink when len > user_cap");
        assert_eq!(index.uris().len(), 3);
    }

    #[test]
    fn test_update_from_disk_skips_pinned_open_check() {
        // Pinning does not change the open-document contract: a URI that is
        // both pinned and open is still skipped by update_from_disk, because
        // the open document is authoritative on disk reads.
        let index = CrossFileWorkspaceIndex::with_capacity(4);
        let uri = test_uri("open_and_pinned.R");

        let mut open_docs = HashSet::new();
        open_docs.insert(uri.clone());
        let mut pinned = HashSet::new();
        pinned.insert(uri.clone());
        index.set_pinned_uris(pinned);

        index.update_from_disk(
            &uri,
            &open_docs,
            test_snapshot(),
            CrossFileMetadata::default(),
            Arc::new(ScopeArtifacts::default()),
        );

        assert!(
            !index.contains(&uri),
            "open document must remain authoritative even when pinned"
        );
    }

    #[test]
    fn test_resize() {
        let index = CrossFileWorkspaceIndex::with_capacity(5);
        for i in 0..5 {
            index.insert(test_uri(&format!("{}.R", i)), test_entry(i as u64));
        }

        // Shrink to 2
        index.resize(2);
        assert!(!index.contains(&test_uri("0.R")));
        assert!(!index.contains(&test_uri("1.R")));
        assert!(!index.contains(&test_uri("2.R")));
        assert!(index.contains(&test_uri("3.R")));
        assert!(index.contains(&test_uri("4.R")));
    }

    #[test]
    fn test_resize_updates_shrink_baseline() {
        // Issue #128 review finding 2: `resize()` is called from
        // production at startup (`state::resize_caches`). After a resize
        // grows the cache, `set_pinned_uris` must shrink back to the
        // *resized* baseline, not the original constructor value, or
        // shrink-back would silently override the user's configured
        // `cache_workspace_index_max_entries`.
        let index = CrossFileWorkspaceIndex::with_capacity(2);
        index.resize(5);
        assert_eq!(index.cap(), 5);

        // Fill to the resized capacity and pin everything.
        let mut pin_set = HashSet::new();
        for i in 0..5 {
            let uri = test_uri(&format!("a{}.R", i));
            index.insert(uri.clone(), test_entry(i));
            pin_set.insert(uri);
        }
        index.set_pinned_uris(pin_set);

        // Trigger an all-pinned overflow.
        let overflow = test_uri("overflow.R");
        index.insert(overflow.clone(), test_entry(99));
        assert_eq!(index.cap(), 6);

        // Drain back below the resized baseline.
        index.invalidate(&overflow);
        assert_eq!(index.uris().len(), 5);

        // Clearing pins should shrink to the resized baseline (5),
        // not the constructor value (2).
        index.set_pinned_uris(HashSet::new());
        assert_eq!(
            index.cap(),
            5,
            "shrink-back must track the resized baseline, not the constructor cap"
        );
    }
}
