//
// cross_file/workspace_index.rs
//
// Workspace index for cross-file awareness
//

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

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
    pub metadata: CrossFileMetadata,
    /// Computed scope artifacts
    pub artifacts: ScopeArtifacts,
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
        let cap = non_zero_or(cap, DEFAULT_WORKSPACE_INDEX_CAPACITY);
        Self {
            inner: RwLock::new(LruCache::new(cap)),
            version: AtomicU64::new(0),
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
    pub fn get_metadata(&self, uri: &Url) -> Option<CrossFileMetadata> {
        self.inner
            .read()
            .ok()?
            .peek(uri)
            .map(|e| e.metadata.clone())
    }

    /// Get artifacts for a URI (without freshness check)
    pub fn get_artifacts(&self, uri: &Url) -> Option<ScopeArtifacts> {
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
        artifacts: ScopeArtifacts,
    ) {
        if open_documents.contains(uri) {
            log::trace!("Skipping disk update for open document: {}", uri);
            return;
        }

        let version = self.increment_version();
        let entry = IndexEntry {
            snapshot,
            metadata,
            artifacts,
            indexed_at_version: version,
        };

        if let Ok(mut guard) = self.inner.write() {
            guard.push(uri.clone(), entry);
        }
    }

    /// Insert entry directly (for testing or when open-docs check is done elsewhere)
    pub fn insert(&self, uri: Url, entry: IndexEntry) {
        self.increment_version();
        if let Ok(mut guard) = self.inner.write() {
            guard.push(uri, entry);
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

    /// Resize the cache capacity. If shrinking, LRU entries are evicted.
    pub fn resize(&self, cap: usize) {
        let cap = non_zero_or(cap, DEFAULT_WORKSPACE_INDEX_CAPACITY);
        if let Ok(mut guard) = self.inner.write() {
            guard.resize(cap);
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
            metadata: CrossFileMetadata::default(),
            artifacts: ScopeArtifacts::default(),
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

        index.insert(uri.clone(), test_entry(1));

        assert!(index.get_metadata(&uri).is_some());
        assert!(index.get_artifacts(&uri).is_some());
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
            ScopeArtifacts::default(),
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
            ScopeArtifacts::default(),
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
}
