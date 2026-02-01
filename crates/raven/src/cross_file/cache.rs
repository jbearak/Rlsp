//
// cross_file/cache.rs
//
// Caching structures with interior mutability for cross-file awareness
//

use std::collections::HashMap;
use std::sync::RwLock;

use tower_lsp::lsp_types::Url;

use super::scope::ScopeArtifacts;
use super::types::CrossFileMetadata;

/// Fingerprint for cache validity checking
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScopeFingerprint {
    /// Hash of the file's own contents
    pub self_hash: u64,
    /// Hash of the dependency edge set
    pub edges_hash: u64,
    /// Hash of upstream exported interfaces
    pub upstream_interfaces_hash: u64,
    /// Workspace index version
    pub workspace_index_version: u64,
}

/// Metadata cache with interior mutability
#[derive(Debug, Default)]
pub struct MetadataCache {
    inner: RwLock<HashMap<Url, CrossFileMetadata>>,
}

impl MetadataCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, uri: &Url) -> Option<CrossFileMetadata> {
        self.inner.read().ok()?.get(uri).cloned()
    }

    pub fn insert(&self, uri: Url, meta: CrossFileMetadata) {
        if let Ok(mut guard) = self.inner.write() {
            guard.insert(uri, meta);
        }
    }

    pub fn remove(&self, uri: &Url) {
        if let Ok(mut guard) = self.inner.write() {
            guard.remove(uri);
        }
    }

    /// Invalidate (remove) multiple metadata cache entries at once.
    ///
    /// This is more efficient than calling `remove` multiple times when
    /// invalidating several entries, as it only acquires the write lock once.
    ///
    /// # Arguments
    /// * `uris` - Iterator of URIs whose cache entries should be invalidated
    ///
    /// # Returns
    /// The number of entries that were actually removed from the cache.
    ///
    /// _Requirements: 8.3_
    pub fn invalidate_many<'a>(&self, uris: impl IntoIterator<Item = &'a Url>) -> usize {
        if let Ok(mut guard) = self.inner.write() {
            let mut count = 0;
            for uri in uris {
                if guard.remove(uri).is_some() {
                    count += 1;
                }
            }
            count
        } else {
            0
        }
    }
}

/// Artifacts cache with interior mutability
#[derive(Debug, Default)]
pub struct ArtifactsCache {
    inner: RwLock<HashMap<Url, (ScopeFingerprint, ScopeArtifacts)>>,
}

impl ArtifactsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get cached artifacts if fingerprint matches
    pub fn get_if_fresh(&self, uri: &Url, fp: &ScopeFingerprint) -> Option<ScopeArtifacts> {
        let guard = self.inner.read().ok()?;
        guard.get(uri).and_then(|(cached_fp, artifacts)| {
            if cached_fp == fp {
                Some(artifacts.clone())
            } else {
                None
            }
        })
    }

    /// Get cached artifacts without fingerprint check
    pub fn get(&self, uri: &Url) -> Option<ScopeArtifacts> {
        self.inner.read().ok()?.get(uri).map(|(_, a)| a.clone())
    }

    /// Insert or update cache entry
    pub fn insert(&self, uri: Url, fp: ScopeFingerprint, artifacts: ScopeArtifacts) {
        if let Ok(mut guard) = self.inner.write() {
            guard.insert(uri, (fp, artifacts));
        }
    }

    /// Invalidate a specific entry
    pub fn invalidate(&self, uri: &Url) {
        if let Ok(mut guard) = self.inner.write() {
            guard.remove(uri);
        }
    }

    /// Invalidate all entries
    pub fn invalidate_all(&self) {
        if let Ok(mut guard) = self.inner.write() {
            guard.clear();
        }
    }
}

/// Cache key for parent selection stability
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParentCacheKey {
    /// Hash of the child's CrossFileMetadata (backward directives)
    pub metadata_fingerprint: u64,
    /// Hash of the reverse edges pointing to this child
    pub reverse_edges_hash: u64,
}

/// Result of parent resolution
#[derive(Debug, Clone)]
pub enum ParentResolution {
    /// Single unambiguous parent
    Single {
        parent_uri: Url,
        call_site_line: Option<u32>,
        call_site_column: Option<u32>,
    },
    /// Multiple possible parents - deterministic but ambiguous
    Ambiguous {
        selected_uri: Url,
        selected_line: Option<u32>,
        selected_column: Option<u32>,
        alternatives: Vec<Url>,
    },
    /// No parent found
    None,
}

/// Parent selection cache with interior mutability
#[derive(Debug, Default)]
pub struct ParentSelectionCache {
    inner: RwLock<HashMap<(Url, ParentCacheKey), ParentResolution>>,
}

impl ParentSelectionCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get cached parent resolution if available
    pub fn get(&self, child_uri: &Url, cache_key: &ParentCacheKey) -> Option<ParentResolution> {
        let guard = self.inner.read().ok()?;
        guard.get(&(child_uri.clone(), cache_key.clone())).cloned()
    }

    /// Insert parent resolution into cache
    pub fn insert(&self, child_uri: Url, cache_key: ParentCacheKey, resolution: ParentResolution) {
        if let Ok(mut guard) = self.inner.write() {
            guard.insert((child_uri, cache_key), resolution);
        }
    }

    /// Invalidate cache for a child
    pub fn invalidate(&self, child_uri: &Url) {
        if let Ok(mut guard) = self.inner.write() {
            guard.retain(|(uri, _), _| uri != child_uri);
        }
    }

    /// Invalidate all entries
    pub fn invalidate_all(&self) {
        if let Ok(mut guard) = self.inner.write() {
            guard.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///{}", name)).unwrap()
    }

    #[test]
    fn test_metadata_cache() {
        let cache = MetadataCache::new();
        let uri = test_uri("test.R");
        let meta = CrossFileMetadata::default();

        cache.insert(uri.clone(), meta);
        assert!(cache.get(&uri).is_some());

        cache.remove(&uri);
        assert!(cache.get(&uri).is_none());
    }

    #[test]
    fn test_artifacts_cache_fresh() {
        let cache = ArtifactsCache::new();
        let uri = test_uri("test.R");
        let fp = ScopeFingerprint {
            self_hash: 123,
            edges_hash: 456,
            upstream_interfaces_hash: 789,
            workspace_index_version: 1,
        };
        let artifacts = ScopeArtifacts::default();

        cache.insert(uri.clone(), fp.clone(), artifacts);

        // Same fingerprint should return cached
        assert!(cache.get_if_fresh(&uri, &fp).is_some());

        // Different fingerprint should not return cached
        let fp2 = ScopeFingerprint {
            self_hash: 999,
            ..fp
        };
        assert!(cache.get_if_fresh(&uri, &fp2).is_none());
    }

    #[test]
    fn test_artifacts_cache_invalidate() {
        let cache = ArtifactsCache::new();
        let uri = test_uri("test.R");
        let fp = ScopeFingerprint {
            self_hash: 123,
            edges_hash: 456,
            upstream_interfaces_hash: 789,
            workspace_index_version: 1,
        };

        cache.insert(uri.clone(), fp, ScopeArtifacts::default());
        assert!(cache.get(&uri).is_some());

        cache.invalidate(&uri);
        assert!(cache.get(&uri).is_none());
    }

    #[test]
    fn test_parent_selection_cache() {
        let cache = ParentSelectionCache::new();
        let child = test_uri("child.R");
        let parent = test_uri("parent.R");
        let key = ParentCacheKey {
            metadata_fingerprint: 123,
            reverse_edges_hash: 456,
        };
        let resolution = ParentResolution::Single {
            parent_uri: parent,
            call_site_line: Some(10),
            call_site_column: Some(0),
        };

        cache.insert(child.clone(), key.clone(), resolution);
        assert!(cache.get(&child, &key).is_some());

        cache.invalidate(&child);
        assert!(cache.get(&child, &key).is_none());
    }

    #[test]
    fn test_metadata_cache_invalidate_many() {
        let cache = MetadataCache::new();
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");
        let uri3 = test_uri("test3.R");
        let uri4 = test_uri("test4.R"); // Not in cache

        // Insert some entries
        cache.insert(uri1.clone(), CrossFileMetadata::default());
        cache.insert(uri2.clone(), CrossFileMetadata::default());
        cache.insert(uri3.clone(), CrossFileMetadata::default());

        // Verify all are present
        assert!(cache.get(&uri1).is_some());
        assert!(cache.get(&uri2).is_some());
        assert!(cache.get(&uri3).is_some());

        // Invalidate uri1 and uri2 (and uri4 which doesn't exist)
        let uris_to_invalidate = vec![uri1.clone(), uri2.clone(), uri4.clone()];
        let count = cache.invalidate_many(&uris_to_invalidate);

        // Should have invalidated 2 entries (uri1 and uri2, not uri4)
        assert_eq!(count, 2);

        // uri1 and uri2 should be gone
        assert!(cache.get(&uri1).is_none());
        assert!(cache.get(&uri2).is_none());

        // uri3 should still be present
        assert!(cache.get(&uri3).is_some());
    }

    #[test]
    fn test_metadata_cache_invalidate_many_empty() {
        let cache = MetadataCache::new();
        let uri1 = test_uri("test1.R");

        cache.insert(uri1.clone(), CrossFileMetadata::default());

        // Invalidate with empty iterator
        let count = cache.invalidate_many(&Vec::<Url>::new());
        assert_eq!(count, 0);

        // Entry should still be present
        assert!(cache.get(&uri1).is_some());
    }
}
