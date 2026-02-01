//
// content_provider.rs
//
// Content provider abstraction for unified file access
//
// This module provides traits for accessing file content, metadata, and artifacts
// with a consistent interface that respects the open-docs-authoritative rule.
//

use std::collections::HashMap;

use async_trait::async_trait;
use tower_lsp::lsp_types::Url;

use crate::cross_file::file_cache::CrossFileFileCache;
use crate::cross_file::scope::{self, ScopeArtifacts};
use crate::cross_file::types::CrossFileMetadata;
use crate::cross_file::workspace_index::CrossFileWorkspaceIndex;
use crate::document_store::DocumentStore;
use crate::state::Document;
use crate::workspace_index::WorkspaceIndex;

/// Trait for content providers (sync operations)
///
/// This trait provides a unified interface for accessing file content,
/// metadata, and artifacts. Implementations should respect the
/// open-docs-authoritative rule: open documents always take precedence
/// over indexed data.
///
/// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
pub trait ContentProvider: Send + Sync {
    /// Get content for a URI (prefers open docs)
    ///
    /// Returns the file content as a String, or None if the file
    /// is not available. When a document is open, returns the
    /// in-memory content rather than disk content.
    fn get_content(&self, uri: &Url) -> Option<String>;

    /// Get metadata for a URI
    ///
    /// Returns the cross-file metadata (source() calls, directives, etc.)
    /// for the given URI, or None if not available.
    fn get_metadata(&self, uri: &Url) -> Option<CrossFileMetadata>;

    /// Get artifacts for a URI
    ///
    /// Returns the scope artifacts (exported interface, timeline, etc.)
    /// for the given URI, or None if not available.
    fn get_artifacts(&self, uri: &Url) -> Option<ScopeArtifacts>;

    /// Check if URI exists in cache (no I/O)
    ///
    /// Returns true if the URI is available in any cached source
    /// (DocumentStore, WorkspaceIndex, or file cache) without
    /// performing any filesystem I/O.
    fn exists_cached(&self, uri: &Url) -> bool;

    /// Check if URI is currently open
    ///
    /// Returns true if the document is currently open in the editor.
    /// Open documents are authoritative and take precedence over
    /// indexed data.
    #[allow(dead_code)]
    fn is_open(&self, uri: &Url) -> bool;
}

/// Async extension for file existence checking (non-blocking I/O)
///
/// This trait extends ContentProvider with async methods for
/// file existence checking that don't block the LSP main thread.
///
/// **Validates: Requirements 14.1, 14.2, 14.3, 14.4**
#[allow(dead_code)]
#[async_trait]
pub trait AsyncContentProvider: ContentProvider {
    /// Check if URIs exist on disk (batched, non-blocking)
    ///
    /// Returns a map of URI -> exists for all provided URIs.
    /// This method first checks cached sources (no I/O) and then
    /// uses spawn_blocking to check disk for uncached URIs.
    ///
    /// # Arguments
    /// * `uris` - Slice of URIs to check for existence
    ///
    /// # Returns
    /// HashMap mapping each URI to its existence status
    async fn check_existence_batch(&self, uris: &[Url]) -> HashMap<Url, bool>;

    /// Check if a single URI exists (non-blocking)
    ///
    /// Convenience method that wraps check_existence_batch for a single URI.
    /// Default implementation provided.
    async fn exists(&self, uri: &Url) -> bool {
        let result = self.check_existence_batch(std::slice::from_ref(uri)).await;
        result.get(uri).copied().unwrap_or(false)
    }
}

// ============================================================================
// Default Content Provider
// ============================================================================

/// Default content provider using DocumentStore and WorkspaceIndex
///
/// This implementation respects the open-docs-authoritative rule:
/// 1. Check DocumentStore first (open docs are authoritative)
/// 2. Check legacy documents HashMap (for migration compatibility)
/// 3. Check WorkspaceIndex (closed files)
/// 4. Check legacy workspace_index and cross_file_workspace_index (for migration compatibility)
/// 5. Check file cache (no synchronous disk I/O)
///
/// **Validates: Requirements 7.2, 13.2, 14.1, 14.2, 14.3, 14.4**
pub struct DefaultContentProvider<'a> {
    document_store: &'a DocumentStore,
    workspace_index: &'a WorkspaceIndex,
    file_cache: &'a CrossFileFileCache,
    // Legacy fields for migration compatibility
    legacy_documents: Option<&'a HashMap<Url, Document>>,
    legacy_workspace_index: Option<&'a HashMap<Url, Document>>,
    legacy_cross_file_workspace_index: Option<&'a CrossFileWorkspaceIndex>,
}

impl<'a> DefaultContentProvider<'a> {
    /// Create a new DefaultContentProvider
    ///
    /// # Arguments
    /// * `document_store` - Reference to the DocumentStore for open documents
    /// * `workspace_index` - Reference to the WorkspaceIndex for closed files
    /// * `file_cache` - Reference to the CrossFileFileCache for disk file caching
    #[allow(dead_code)]
    pub fn new(
        document_store: &'a DocumentStore,
        workspace_index: &'a WorkspaceIndex,
        file_cache: &'a CrossFileFileCache,
    ) -> Self {
        Self {
            document_store,
            workspace_index,
            file_cache,
            legacy_documents: None,
            legacy_workspace_index: None,
            legacy_cross_file_workspace_index: None,
        }
    }

    /// Create a new DefaultContentProvider with legacy field support
    ///
    /// This constructor includes references to legacy fields for migration compatibility.
    /// Use this during the migration period when both old and new fields are in use.
    ///
    /// # Arguments
    /// * `document_store` - Reference to the DocumentStore for open documents
    /// * `workspace_index` - Reference to the WorkspaceIndex for closed files
    /// * `file_cache` - Reference to the CrossFileFileCache for disk file caching
    /// * `legacy_documents` - Reference to the legacy documents HashMap
    /// * `legacy_workspace_index` - Reference to the legacy workspace_index HashMap
    /// * `legacy_cross_file_workspace_index` - Reference to the legacy CrossFileWorkspaceIndex
    pub fn with_legacy(
        document_store: &'a DocumentStore,
        workspace_index: &'a WorkspaceIndex,
        file_cache: &'a CrossFileFileCache,
        legacy_documents: &'a HashMap<Url, Document>,
        legacy_workspace_index: &'a HashMap<Url, Document>,
        legacy_cross_file_workspace_index: &'a CrossFileWorkspaceIndex,
    ) -> Self {
        Self {
            document_store,
            workspace_index,
            file_cache,
            legacy_documents: Some(legacy_documents),
            legacy_workspace_index: Some(legacy_workspace_index),
            legacy_cross_file_workspace_index: Some(legacy_cross_file_workspace_index),
        }
    }
}

impl<'a> ContentProvider for DefaultContentProvider<'a> {
    /// Get content for a URI (prefers open docs)
    ///
    /// Checks sources in order:
    /// 1. DocumentStore (open docs are authoritative)
    /// 2. Legacy documents HashMap (for migration compatibility)
    /// 3. WorkspaceIndex (closed files)
    /// 4. Legacy workspace_index (for migration compatibility)
    /// 5. File cache (no synchronous disk I/O)
    ///
    /// **Validates: Requirements 3.1, 7.2, 13.2**
    fn get_content(&self, uri: &Url) -> Option<String> {
        // 1. Check DocumentStore (open docs are authoritative)
        if let Some(doc) = self.document_store.get_without_touch(uri) {
            return Some(doc.contents.to_string());
        }

        // 2. Check legacy documents HashMap (for migration compatibility)
        if let Some(legacy_docs) = self.legacy_documents {
            if let Some(doc) = legacy_docs.get(uri) {
                return Some(doc.text());
            }
        }

        // 3. Check WorkspaceIndex
        if let Some(entry) = self.workspace_index.get(uri) {
            return Some(entry.contents.to_string());
        }

        // 4. Check legacy workspace_index (for migration compatibility)
        if let Some(legacy_ws) = self.legacy_workspace_index {
            if let Some(doc) = legacy_ws.get(uri) {
                return Some(doc.text());
            }
        }

        // 5. Check file cache (no synchronous disk I/O)
        self.file_cache.get(uri)
    }

    /// Get metadata for a URI
    ///
    /// Checks sources in order:
    /// 1. DocumentStore (open docs are authoritative)
    /// 2. Legacy documents HashMap (for migration compatibility)
    /// 3. WorkspaceIndex (closed files)
    /// 4. Legacy cross_file_workspace_index (for migration compatibility)
    /// 5. Legacy workspace_index (for migration compatibility)
    ///
    /// **Validates: Requirements 3.1, 7.2, 13.2**
    fn get_metadata(&self, uri: &Url) -> Option<CrossFileMetadata> {
        // 1. Check DocumentStore
        if let Some(doc) = self.document_store.get_without_touch(uri) {
            return Some(doc.metadata.clone());
        }

        // 2. Check legacy documents HashMap (for migration compatibility)
        if let Some(legacy_docs) = self.legacy_documents {
            if let Some(doc) = legacy_docs.get(uri) {
                let text = doc.text();
                return Some(crate::cross_file::extract_metadata(&text));
            }
        }

        // 3. Check WorkspaceIndex
        if let Some(metadata) = self.workspace_index.get_metadata(uri) {
            return Some(metadata);
        }

        // 4. Check legacy cross_file_workspace_index (for migration compatibility)
        if let Some(legacy_cf_ws) = self.legacy_cross_file_workspace_index {
            if let Some(metadata) = legacy_cf_ws.get_metadata(uri) {
                return Some(metadata);
            }
        }

        // 5. Check legacy workspace_index (for migration compatibility)
        if let Some(legacy_ws) = self.legacy_workspace_index {
            if let Some(doc) = legacy_ws.get(uri) {
                let text = doc.text();
                return Some(crate::cross_file::extract_metadata(&text));
            }
        }

        None
    }

    /// Get artifacts for a URI
    ///
    /// Checks sources in order:
    /// 1. DocumentStore (open docs are authoritative)
    /// 2. Legacy documents HashMap (for migration compatibility)
    /// 3. WorkspaceIndex (closed files)
    /// 4. Legacy cross_file_workspace_index (for migration compatibility)
    /// 5. Legacy workspace_index (for migration compatibility)
    ///
    /// **Validates: Requirements 3.1, 7.2, 13.2**
    fn get_artifacts(&self, uri: &Url) -> Option<ScopeArtifacts> {
        // 1. Check DocumentStore
        if let Some(doc) = self.document_store.get_without_touch(uri) {
            return Some(doc.artifacts.clone());
        }

        // 2. Check legacy documents HashMap (for migration compatibility)
        if let Some(legacy_docs) = self.legacy_documents {
            if let Some(doc) = legacy_docs.get(uri) {
                if let Some(tree) = &doc.tree {
                    let text = doc.text();
                    return Some(scope::compute_artifacts(uri, tree, &text));
                }
            }
        }

        // 3. Check WorkspaceIndex
        if let Some(artifacts) = self.workspace_index.get_artifacts(uri) {
            return Some(artifacts);
        }

        // 4. Check legacy cross_file_workspace_index (for migration compatibility)
        if let Some(legacy_cf_ws) = self.legacy_cross_file_workspace_index {
            if let Some(artifacts) = legacy_cf_ws.get_artifacts(uri) {
                return Some(artifacts);
            }
        }

        // 5. Check legacy workspace_index (for migration compatibility)
        if let Some(legacy_ws) = self.legacy_workspace_index {
            if let Some(doc) = legacy_ws.get(uri) {
                if let Some(tree) = &doc.tree {
                    let text = doc.text();
                    return Some(scope::compute_artifacts(uri, tree, &text));
                }
            }
        }

        None
    }

    /// Check if URI exists in cache (no I/O)
    ///
    /// Returns true if the URI is available in any cached source
    /// without performing filesystem I/O.
    ///
    /// **Validates: Requirements 14.3**
    fn exists_cached(&self, uri: &Url) -> bool {
        self.document_store.contains(uri)
            || self
                .legacy_documents
                .is_some_and(|docs: &HashMap<Url, Document>| docs.contains_key(uri))
            || self.workspace_index.contains(uri)
            || self
                .legacy_workspace_index
                .is_some_and(|ws: &HashMap<Url, Document>| ws.contains_key(uri))
            || self
                .legacy_cross_file_workspace_index
                .is_some_and(|cf_ws| cf_ws.contains(uri))
            || self.file_cache.get(uri).is_some()
    }

    /// Check if URI is currently open
    ///
    /// Returns true if the document is currently open in the editor.
    ///
    /// **Validates: Requirements 3.3, 7.1**
    fn is_open(&self, uri: &Url) -> bool {
        self.document_store.contains(uri)
            || self
                .legacy_documents
                .is_some_and(|docs: &HashMap<Url, Document>| docs.contains_key(uri))
    }
}

#[async_trait]
impl<'a> AsyncContentProvider for DefaultContentProvider<'a> {
    /// Check if URIs exist on disk (batched, non-blocking)
    ///
    /// First checks cached sources (no I/O needed), then uses
    /// spawn_blocking to check disk for uncached URIs in batch.
    ///
    /// **Validates: Requirements 14.1, 14.2, 14.3, 14.4**
    async fn check_existence_batch(&self, uris: &[Url]) -> HashMap<Url, bool> {
        // First check cached sources (no I/O needed)
        let mut results = HashMap::new();
        let mut uncached_uris = Vec::new();

        for uri in uris {
            if self.exists_cached(uri) {
                results.insert(uri.clone(), true);
            } else {
                uncached_uris.push(uri.clone());
            }
        }

        // Batch check uncached URIs on blocking thread
        if !uncached_uris.is_empty() {
            let paths: Vec<_> = uncached_uris
                .iter()
                .filter_map(|u| u.to_file_path().ok())
                .collect();

            let existence_results = match tokio::task::spawn_blocking(move || {
                paths.iter().map(|p| p.exists()).collect::<Vec<_>>()
            })
            .await
            {
                Ok(v) => v,
                Err(err) => {
                    log::warn!("Existence check failed: {err}");
                    return results;
                }
            };

            // Map results back to URIs
            // Note: We need to handle the case where some URIs couldn't be converted to paths
            let mut path_idx = 0;
            for uri in &uncached_uris {
                if uri.to_file_path().is_ok() {
                    let exists = existence_results.get(path_idx).copied().unwrap_or(false);
                    results.insert(uri.clone(), exists);
                    path_idx += 1;
                } else {
                    // URI couldn't be converted to path, mark as not existing
                    results.insert(uri.clone(), false);
                }
            }
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_store::DocumentStoreConfig;
    use crate::workspace_index::WorkspaceIndexConfig;
    use proptest::prelude::*;

    /// Mock content provider for testing
    struct MockContentProvider {
        content: HashMap<Url, String>,
        metadata: HashMap<Url, CrossFileMetadata>,
        artifacts: HashMap<Url, ScopeArtifacts>,
        open_uris: std::collections::HashSet<Url>,
    }

    impl MockContentProvider {
        fn new() -> Self {
            Self {
                content: HashMap::new(),
                metadata: HashMap::new(),
                artifacts: HashMap::new(),
                open_uris: std::collections::HashSet::new(),
            }
        }

        fn with_content(mut self, uri: Url, content: String) -> Self {
            self.content.insert(uri, content);
            self
        }

        fn with_open(mut self, uri: Url) -> Self {
            self.open_uris.insert(uri);
            self
        }
    }

    impl ContentProvider for MockContentProvider {
        fn get_content(&self, uri: &Url) -> Option<String> {
            self.content.get(uri).cloned()
        }

        fn get_metadata(&self, uri: &Url) -> Option<CrossFileMetadata> {
            self.metadata.get(uri).cloned()
        }

        fn get_artifacts(&self, uri: &Url) -> Option<ScopeArtifacts> {
            self.artifacts.get(uri).cloned()
        }

        fn exists_cached(&self, uri: &Url) -> bool {
            self.content.contains_key(uri)
                || self.metadata.contains_key(uri)
                || self.artifacts.contains_key(uri)
        }

        fn is_open(&self, uri: &Url) -> bool {
            self.open_uris.contains(uri)
        }
    }

    #[async_trait]
    impl AsyncContentProvider for MockContentProvider {
        async fn check_existence_batch(&self, uris: &[Url]) -> HashMap<Url, bool> {
            uris.iter()
                .map(|uri| (uri.clone(), self.exists_cached(uri)))
                .collect()
        }
    }

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///{}", name)).unwrap()
    }

    #[test]
    fn test_content_provider_trait_is_object_safe() {
        // Verify that ContentProvider can be used as a trait object
        let provider = MockContentProvider::new();
        let _boxed: Box<dyn ContentProvider> = Box::new(provider);
    }

    #[test]
    fn test_mock_get_content() {
        let uri = test_uri("test.R");
        let provider =
            MockContentProvider::new().with_content(uri.clone(), "test content".to_string());

        assert_eq!(provider.get_content(&uri), Some("test content".to_string()));
        assert_eq!(provider.get_content(&test_uri("other.R")), None);
    }

    #[test]
    fn test_mock_exists_cached() {
        let uri = test_uri("test.R");
        let provider = MockContentProvider::new().with_content(uri.clone(), "content".to_string());

        assert!(provider.exists_cached(&uri));
        assert!(!provider.exists_cached(&test_uri("other.R")));
    }

    #[test]
    fn test_mock_is_open() {
        let uri = test_uri("test.R");
        let provider = MockContentProvider::new().with_open(uri.clone());

        assert!(provider.is_open(&uri));
        assert!(!provider.is_open(&test_uri("other.R")));
    }

    #[tokio::test]
    async fn test_async_check_existence_batch() {
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");
        let uri3 = test_uri("test3.R");

        let provider = MockContentProvider::new()
            .with_content(uri1.clone(), "content1".to_string())
            .with_content(uri2.clone(), "content2".to_string());

        let results = provider
            .check_existence_batch(&[uri1.clone(), uri2.clone(), uri3.clone()])
            .await;

        assert_eq!(results.get(&uri1), Some(&true));
        assert_eq!(results.get(&uri2), Some(&true));
        assert_eq!(results.get(&uri3), Some(&false));
    }

    #[tokio::test]
    async fn test_async_exists_single() {
        let uri = test_uri("test.R");
        let provider = MockContentProvider::new().with_content(uri.clone(), "content".to_string());

        assert!(provider.exists(&uri).await);
        assert!(!provider.exists(&test_uri("other.R")).await);
    }

    #[tokio::test]
    async fn test_async_exists_empty_batch() {
        let provider = MockContentProvider::new();
        let results = provider.check_existence_batch(&[]).await;
        assert!(results.is_empty());
    }

    // ========================================================================
    // DefaultContentProvider Tests
    // ========================================================================

    fn make_test_document_store() -> DocumentStore {
        DocumentStore::new(DocumentStoreConfig {
            max_documents: 10,
            max_memory_bytes: 10 * 1024 * 1024,
        })
    }

    fn make_test_workspace_index() -> WorkspaceIndex {
        WorkspaceIndex::new(WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 100,
            max_file_size_bytes: 1024 * 1024,
        })
    }

    #[tokio::test]
    async fn test_default_provider_open_doc_takes_precedence() {
        // Test that open documents take precedence over workspace index
        let mut doc_store = make_test_document_store();
        let workspace_index = make_test_workspace_index();
        let file_cache = CrossFileFileCache::new();

        let uri = test_uri("test.R");

        // Add to workspace index first
        let index_entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str("workspace_content"),
            tree: None,
            loaded_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 17,
                content_hash: None,
            },
            metadata: CrossFileMetadata::default(),
            artifacts: ScopeArtifacts::default(),
            indexed_at_version: 0,
        };
        workspace_index.insert(uri.clone(), index_entry);

        // Open document with different content
        doc_store.open(uri.clone(), "open_doc_content", 1).await;

        // Create provider and verify open doc takes precedence
        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        let content = provider.get_content(&uri);
        assert_eq!(content, Some("open_doc_content".to_string()));
    }

    #[tokio::test]
    async fn test_default_provider_falls_back_to_workspace_index() {
        // Test that workspace index is used when document is not open
        let doc_store = make_test_document_store();
        let workspace_index = make_test_workspace_index();
        let file_cache = CrossFileFileCache::new();

        let uri = test_uri("test.R");

        // Add to workspace index only
        let index_entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str("workspace_content"),
            tree: None,
            loaded_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 17,
                content_hash: None,
            },
            metadata: CrossFileMetadata::default(),
            artifacts: ScopeArtifacts::default(),
            indexed_at_version: 0,
        };
        workspace_index.insert(uri.clone(), index_entry);

        // Create provider and verify workspace index is used
        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        let content = provider.get_content(&uri);
        assert_eq!(content, Some("workspace_content".to_string()));
    }

    #[tokio::test]
    async fn test_default_provider_is_open() {
        let mut doc_store = make_test_document_store();
        let workspace_index = make_test_workspace_index();
        let file_cache = CrossFileFileCache::new();

        let uri = test_uri("test.R");

        // Open document
        doc_store.open(uri.clone(), "content", 1).await;

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        assert!(provider.is_open(&uri));
        assert!(!provider.is_open(&test_uri("other.R")));
    }

    #[tokio::test]
    async fn test_default_provider_exists_cached() {
        let mut doc_store = make_test_document_store();
        let workspace_index = make_test_workspace_index();
        let file_cache = CrossFileFileCache::new();

        let uri1 = test_uri("open.R");
        let uri2 = test_uri("indexed.R");
        let uri3 = test_uri("cached.R");
        let uri4 = test_uri("nowhere.R");

        // Open document
        doc_store.open(uri1.clone(), "content", 1).await;

        // Add to workspace index
        let index_entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str("indexed"),
            tree: None,
            loaded_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 7,
                content_hash: None,
            },
            metadata: CrossFileMetadata::default(),
            artifacts: ScopeArtifacts::default(),
            indexed_at_version: 0,
        };
        workspace_index.insert(uri2.clone(), index_entry);

        // Add to file cache
        file_cache.insert(
            uri3.clone(),
            crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 6,
                content_hash: None,
            },
            "cached".to_string(),
        );

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        assert!(provider.exists_cached(&uri1)); // Open doc
        assert!(provider.exists_cached(&uri2)); // Workspace index
        assert!(provider.exists_cached(&uri3)); // File cache
        assert!(!provider.exists_cached(&uri4)); // Not found
    }

    #[tokio::test]
    async fn test_default_provider_get_metadata_open_doc_precedence() {
        let mut doc_store = make_test_document_store();
        let workspace_index = make_test_workspace_index();
        let file_cache = CrossFileFileCache::new();

        let uri = test_uri("test.R");

        // Add to workspace index with metadata
        let mut index_metadata = CrossFileMetadata::default();
        index_metadata.working_directory = Some("workspace_wd".to_string());
        let index_entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str("content"),
            tree: None,
            loaded_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 7,
                content_hash: None,
            },
            metadata: index_metadata,
            artifacts: ScopeArtifacts::default(),
            indexed_at_version: 0,
        };
        workspace_index.insert(uri.clone(), index_entry);

        // Open document (will have different metadata from parsing)
        doc_store
            .open(uri.clone(), "# @lsp-cd: open_wd\nx <- 1", 1)
            .await;

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        // Should get metadata from open doc, not workspace index
        let metadata = provider.get_metadata(&uri).unwrap();
        assert_eq!(metadata.working_directory, Some("open_wd".to_string()));
    }

    #[tokio::test]
    async fn test_default_provider_get_artifacts_open_doc_precedence() {
        let mut doc_store = make_test_document_store();
        let workspace_index = make_test_workspace_index();
        let file_cache = CrossFileFileCache::new();

        let uri = test_uri("test.R");

        // Add to workspace index with artifacts
        let mut index_artifacts = ScopeArtifacts::default();
        index_artifacts.exported_interface.insert(
            "workspace_func".to_string(),
            crate::cross_file::scope::ScopedSymbol {
                name: "workspace_func".to_string(),
                kind: crate::cross_file::scope::SymbolKind::Function,
                source_uri: uri.clone(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
            },
        );
        let index_entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str("workspace_func <- function() {}"),
            tree: None,
            loaded_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 31,
                content_hash: None,
            },
            metadata: CrossFileMetadata::default(),
            artifacts: index_artifacts,
            indexed_at_version: 0,
        };
        workspace_index.insert(uri.clone(), index_entry);

        // Open document with different function
        doc_store
            .open(uri.clone(), "open_func <- function() {}", 1)
            .await;

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        // Should get artifacts from open doc, not workspace index
        let artifacts = provider.get_artifacts(&uri).unwrap();
        assert!(artifacts.exported_interface.contains_key("open_func"));
        assert!(!artifacts.exported_interface.contains_key("workspace_func"));
    }

    #[tokio::test]
    async fn test_default_provider_async_check_existence_cached() {
        let mut doc_store = make_test_document_store();
        let workspace_index = make_test_workspace_index();
        let file_cache = CrossFileFileCache::new();

        let uri1 = test_uri("open.R");
        let uri2 = test_uri("indexed.R");

        // Open document
        doc_store.open(uri1.clone(), "content", 1).await;

        // Add to workspace index
        let index_entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str("indexed"),
            tree: None,
            loaded_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 7,
                content_hash: None,
            },
            metadata: CrossFileMetadata::default(),
            artifacts: ScopeArtifacts::default(),
            indexed_at_version: 0,
        };
        workspace_index.insert(uri2.clone(), index_entry);

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        // Check existence - should find cached URIs without disk I/O
        let results = provider
            .check_existence_batch(&[uri1.clone(), uri2.clone()])
            .await;

        assert_eq!(results.get(&uri1), Some(&true));
        assert_eq!(results.get(&uri2), Some(&true));
    }

    #[tokio::test]
    async fn test_default_provider_async_check_existence_disk() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let doc_store = make_test_document_store();
        let workspace_index = make_test_workspace_index();
        let file_cache = CrossFileFileCache::new();

        // Create a real temp file
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "x <- 1").unwrap();
        let path = temp.path();
        let existing_uri = Url::from_file_path(path).unwrap();

        // Non-existent file
        let nonexistent_uri = test_uri("nonexistent_file_12345.R");

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        // Check existence - should check disk for uncached URIs
        let results = provider
            .check_existence_batch(&[existing_uri.clone(), nonexistent_uri.clone()])
            .await;

        assert_eq!(results.get(&existing_uri), Some(&true));
        assert_eq!(results.get(&nonexistent_uri), Some(&false));
    }

    #[tokio::test]
    async fn test_default_provider_async_exists_single() {
        let mut doc_store = make_test_document_store();
        let workspace_index = make_test_workspace_index();
        let file_cache = CrossFileFileCache::new();

        let uri = test_uri("test.R");
        doc_store.open(uri.clone(), "content", 1).await;

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        assert!(provider.exists(&uri).await);
        assert!(!provider.exists(&test_uri("nonexistent.R")).await);
    }

    #[tokio::test]
    async fn test_default_provider_content_not_found() {
        let doc_store = make_test_document_store();
        let workspace_index = make_test_workspace_index();
        let file_cache = CrossFileFileCache::new();

        let uri = test_uri("nonexistent.R");

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        assert!(provider.get_content(&uri).is_none());
        assert!(provider.get_metadata(&uri).is_none());
        assert!(provider.get_artifacts(&uri).is_none());
    }

    // ========================================================================
    // Property-Based Tests
    // ========================================================================

    // Feature: workspace-index-consolidation, Property 1: Open Documents Are Authoritative
    // **Validates: Requirements 3.1, 3.2, 3.4**
    //
    // Property: For any URI that exists in both DocumentStore and WorkspaceIndex,
    // the ContentProvider SHALL always return data from DocumentStore.

    /// Strategy for generating valid R code content
    fn r_content_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            // Simple assignment
            "[a-z][a-z0-9_]{0,10}".prop_map(|name| format!("{} <- 1", name)),
            // Function definition
            "[a-z][a-z0-9_]{0,10}".prop_map(|name| format!("{} <- function(x) {{ x + 1 }}", name)),
            // Multiple assignments
            (1usize..=5).prop_map(|n| {
                (0..n)
                    .map(|i| format!("var_{} <- {}", i, i))
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
            // With working directory directive
            "[a-z][a-z0-9_]{0,10}".prop_map(|dir| format!("# @lsp-cd: {}\nx <- 1", dir)),
        ]
    }

    /// Strategy for generating a unique file index
    fn file_index_strategy() -> impl Strategy<Value = usize> {
        0usize..100
    }

    /// Helper to create a URI from an index
    fn uri_from_index(idx: usize) -> Url {
        Url::parse(&format!("file:///test{}.R", idx)).unwrap()
    }

    /// Helper to create an IndexEntry with given content
    fn make_index_entry_with_content(content: &str) -> crate::workspace_index::IndexEntry {
        crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str(content),
            tree: None,
            loaded_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: content.len() as u64,
                content_hash: None,
            },
            metadata: CrossFileMetadata::default(),
            artifacts: ScopeArtifacts::default(),
            indexed_at_version: 0,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 1: Open Documents Are Authoritative
        ///
        /// For any URI that exists in both DocumentStore and WorkspaceIndex,
        /// the ContentProvider SHALL always return data from DocumentStore.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.4**
        #[test]
        fn prop_open_documents_are_authoritative_content(
            file_idx in file_index_strategy(),
            open_content in r_content_strategy(),
            index_content in r_content_strategy()
        ) {
            // Skip if contents are identical (can't distinguish source)
            prop_assume!(open_content != index_content);

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut doc_store = make_test_document_store();
                let workspace_index = make_test_workspace_index();
                let file_cache = CrossFileFileCache::new();

                let uri = uri_from_index(file_idx);

                // Add to workspace index first with index_content
                let index_entry = make_index_entry_with_content(&index_content);
                workspace_index.insert(uri.clone(), index_entry);

                // Open document with different content (open_content)
                doc_store.open(uri.clone(), &open_content, 1).await;

                // Create provider
                let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

                // INVARIANT 1: get_content() must return open document content
                let content = provider.get_content(&uri);
                assert_eq!(
                    content,
                    Some(open_content.clone()),
                    "get_content() should return open document content, not workspace index content"
                );

                // INVARIANT 2: is_open() must return true
                assert!(
                    provider.is_open(&uri),
                    "is_open() should return true for open document"
                );

                // INVARIANT 3: exists_cached() must return true
                assert!(
                    provider.exists_cached(&uri),
                    "exists_cached() should return true for open document"
                );
            });
        }

        /// Property 1 extended: Open documents are authoritative for metadata
        ///
        /// When a document is open, get_metadata() must return metadata from
        /// DocumentStore, not WorkspaceIndex.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.4**
        #[test]
        fn prop_open_documents_are_authoritative_metadata(
            file_idx in file_index_strategy(),
            open_wd in "[a-z][a-z0-9_]{1,10}",
            index_wd in "[a-z][a-z0-9_]{1,10}"
        ) {
            // Skip if working directories are identical (can't distinguish source)
            prop_assume!(open_wd != index_wd);

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut doc_store = make_test_document_store();
                let workspace_index = make_test_workspace_index();
                let file_cache = CrossFileFileCache::new();

                let uri = uri_from_index(file_idx);

                // Add to workspace index with index_wd in metadata
                let mut index_metadata = CrossFileMetadata::default();
                index_metadata.working_directory = Some(index_wd.clone());
                let index_entry = crate::workspace_index::IndexEntry {
                    contents: ropey::Rope::from_str("x <- 1"),
                    tree: None,
                    loaded_packages: vec![],
                    snapshot: crate::cross_file::file_cache::FileSnapshot {
                        mtime: std::time::SystemTime::UNIX_EPOCH,
                        size: 6,
                        content_hash: None,
                    },
                    metadata: index_metadata,
                    artifacts: ScopeArtifacts::default(),
                    indexed_at_version: 0,
                };
                workspace_index.insert(uri.clone(), index_entry);

                // Open document with different working directory (open_wd)
                let open_content = format!("# @lsp-cd: {}\nx <- 1", open_wd);
                doc_store.open(uri.clone(), &open_content, 1).await;

                // Create provider
                let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

                // INVARIANT: get_metadata() must return open document metadata
                let metadata = provider.get_metadata(&uri).unwrap();
                assert_eq!(
                    metadata.working_directory,
                    Some(open_wd.clone()),
                    "get_metadata() should return open document metadata (wd={}), not workspace index metadata (wd={})",
                    open_wd,
                    index_wd
                );
            });
        }

        /// Property 1 extended: Open documents are authoritative for artifacts
        ///
        /// When a document is open, get_artifacts() must return artifacts from
        /// DocumentStore, not WorkspaceIndex.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.4**
        #[test]
        fn prop_open_documents_are_authoritative_artifacts(
            file_idx in file_index_strategy(),
            // Use "func_" prefix to avoid R reserved words like "if", "for", "in"
            open_func in "func_[a-z0-9]{1,8}",
            index_func in "idx_[a-z0-9]{1,8}"
        ) {
            // Skip if function names are identical (can't distinguish source)
            prop_assume!(open_func != index_func);

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut doc_store = make_test_document_store();
                let workspace_index = make_test_workspace_index();
                let file_cache = CrossFileFileCache::new();

                let uri = uri_from_index(file_idx);

                // Add to workspace index with index_func in artifacts
                let mut index_artifacts = ScopeArtifacts::default();
                index_artifacts.exported_interface.insert(
                    index_func.clone(),
                    crate::cross_file::scope::ScopedSymbol {
                        name: index_func.clone(),
                        kind: crate::cross_file::scope::SymbolKind::Function,
                        source_uri: uri.clone(),
                        defined_line: 0,
                        defined_column: 0,
                        signature: None,
                    },
                );
                let index_content = format!("{} <- function() {{}}", index_func);
                let index_entry = crate::workspace_index::IndexEntry {
                    contents: ropey::Rope::from_str(&index_content),
                    tree: None,
                    loaded_packages: vec![],
                    snapshot: crate::cross_file::file_cache::FileSnapshot {
                        mtime: std::time::SystemTime::UNIX_EPOCH,
                        size: index_content.len() as u64,
                        content_hash: None,
                    },
                    metadata: CrossFileMetadata::default(),
                    artifacts: index_artifacts,
                    indexed_at_version: 0,
                };
                workspace_index.insert(uri.clone(), index_entry);

                // Open document with different function (open_func)
                let open_content = format!("{} <- function() {{}}", open_func);
                doc_store.open(uri.clone(), &open_content, 1).await;

                // Create provider
                let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

                // INVARIANT: get_artifacts() must return open document artifacts
                let artifacts = provider.get_artifacts(&uri).unwrap();
                assert!(
                    artifacts.exported_interface.contains_key(&open_func),
                    "get_artifacts() should contain open document function '{}', but it doesn't",
                    open_func
                );
                assert!(
                    !artifacts.exported_interface.contains_key(&index_func),
                    "get_artifacts() should NOT contain workspace index function '{}', but it does",
                    index_func
                );
            });
        }

        /// Property 1 extended: Consistency across all accessor methods
        ///
        /// For any URI that exists in both DocumentStore and WorkspaceIndex,
        /// all accessor methods (get_content, get_metadata, get_artifacts)
        /// must return data from the same source (DocumentStore).
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.4**
        #[test]
        fn prop_open_documents_consistency_across_accessors(
            file_idx in file_index_strategy(),
            // Use "func_" prefix to avoid R reserved words like "if", "for", "in"
            open_func in "func_[a-z0-9]{1,8}",
            open_wd in "wd_[a-z0-9]{1,8}",
            index_func in "idx_[a-z0-9]{1,8}",
            index_wd in "iwd_[a-z0-9]{1,8}"
        ) {
            // Skip if values are identical (can't distinguish source)
            prop_assume!(open_func != index_func);
            prop_assume!(open_wd != index_wd);

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut doc_store = make_test_document_store();
                let workspace_index = make_test_workspace_index();
                let file_cache = CrossFileFileCache::new();

                let uri = uri_from_index(file_idx);

                // Add to workspace index with index values
                let mut index_metadata = CrossFileMetadata::default();
                index_metadata.working_directory = Some(index_wd.clone());
                let mut index_artifacts = ScopeArtifacts::default();
                index_artifacts.exported_interface.insert(
                    index_func.clone(),
                    crate::cross_file::scope::ScopedSymbol {
                        name: index_func.clone(),
                        kind: crate::cross_file::scope::SymbolKind::Function,
                        source_uri: uri.clone(),
                        defined_line: 0,
                        defined_column: 0,
                        signature: None,
                    },
                );
                let index_content = format!("# @lsp-cd: {}\n{} <- function() {{}}", index_wd, index_func);
                let index_entry = crate::workspace_index::IndexEntry {
                    contents: ropey::Rope::from_str(&index_content),
                    tree: None,
                    loaded_packages: vec![],
                    snapshot: crate::cross_file::file_cache::FileSnapshot {
                        mtime: std::time::SystemTime::UNIX_EPOCH,
                        size: index_content.len() as u64,
                        content_hash: None,
                    },
                    metadata: index_metadata,
                    artifacts: index_artifacts,
                    indexed_at_version: 0,
                };
                workspace_index.insert(uri.clone(), index_entry);

                // Open document with different values
                let open_content = format!("# @lsp-cd: {}\n{} <- function() {{}}", open_wd, open_func);
                doc_store.open(uri.clone(), &open_content, 1).await;

                // Create provider
                let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

                // INVARIANT: All accessors must return data from DocumentStore

                // Check content
                let content = provider.get_content(&uri).unwrap();
                assert!(
                    content.contains(&open_func),
                    "Content should contain open doc function '{}', got: {}",
                    open_func,
                    content
                );

                // Check metadata
                let metadata = provider.get_metadata(&uri).unwrap();
                assert_eq!(
                    metadata.working_directory,
                    Some(open_wd.clone()),
                    "Metadata should have open doc working directory"
                );

                // Check artifacts
                let artifacts = provider.get_artifacts(&uri).unwrap();
                assert!(
                    artifacts.exported_interface.contains_key(&open_func),
                    "Artifacts should contain open doc function"
                );

                // All three accessors returned data from DocumentStore - consistent!
            });
        }

        /// Property 1 extended: Closed documents fall back to WorkspaceIndex
        ///
        /// When a document is NOT open but exists in WorkspaceIndex,
        /// ContentProvider should return data from WorkspaceIndex.
        ///
        /// **Validates: Requirements 3.4**
        #[test]
        fn prop_closed_documents_use_workspace_index(
            file_idx in file_index_strategy(),
            index_content in r_content_strategy()
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let doc_store = make_test_document_store();
                let workspace_index = make_test_workspace_index();
                let file_cache = CrossFileFileCache::new();

                let uri = uri_from_index(file_idx);

                // Add to workspace index only (document is NOT open)
                let index_entry = make_index_entry_with_content(&index_content);
                workspace_index.insert(uri.clone(), index_entry);

                // Create provider
                let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

                // INVARIANT 1: is_open() must return false
                assert!(
                    !provider.is_open(&uri),
                    "is_open() should return false for closed document"
                );

                // INVARIANT 2: get_content() must return workspace index content
                let content = provider.get_content(&uri);
                assert_eq!(
                    content,
                    Some(index_content.clone()),
                    "get_content() should return workspace index content for closed document"
                );

                // INVARIANT 3: exists_cached() must return true
                assert!(
                    provider.exists_cached(&uri),
                    "exists_cached() should return true for indexed document"
                );
            });
        }

        // ====================================================================
        // Feature: workspace-index-consolidation, Property 8: Content Provider Consistency
        // **Validates: Requirements 7.1, 7.2, 7.3**
        //
        // Property: For any URI, the ContentProvider SHALL return consistent data
        // across get_content, get_metadata, and get_artifacts calls (all from same source).
        // ====================================================================

        /// Property 8: Content Provider Consistency - Open Documents
        ///
        /// When a document is open, all accessor methods must return data
        /// from the DocumentStore, ensuring consistency.
        ///
        /// **Validates: Requirements 7.1, 7.2, 7.3**
        #[test]
        fn prop_content_provider_consistency_open_docs(
            file_idx in file_index_strategy(),
            // Use "func_" prefix to avoid R reserved words like "if", "for", "in"
            func_name in "func_[a-z0-9]{1,8}",
            wd_name in "wd_[a-z0-9]{1,8}"
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut doc_store = make_test_document_store();
                let workspace_index = make_test_workspace_index();
                let file_cache = CrossFileFileCache::new();

                let uri = uri_from_index(file_idx);

                // Open document with specific content
                let content = format!("# @lsp-cd: {}\n{} <- function() {{}}", wd_name, func_name);
                doc_store.open(uri.clone(), &content, 1).await;

                // Create provider
                let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

                // Get all data
                let got_content = provider.get_content(&uri);
                let got_metadata = provider.get_metadata(&uri);
                let got_artifacts = provider.get_artifacts(&uri);

                // INVARIANT 1: All methods return Some (consistent availability)
                assert!(got_content.is_some(), "get_content should return Some for open doc");
                assert!(got_metadata.is_some(), "get_metadata should return Some for open doc");
                assert!(got_artifacts.is_some(), "get_artifacts should return Some for open doc");

                // INVARIANT 2: Content matches what we opened
                let content_str = got_content.unwrap();
                assert!(
                    content_str.contains(&func_name),
                    "Content should contain function name '{}', got: {}",
                    func_name,
                    content_str
                );

                // INVARIANT 3: Metadata is consistent with content
                let metadata = got_metadata.unwrap();
                assert_eq!(
                    metadata.working_directory,
                    Some(wd_name.clone()),
                    "Metadata working_directory should match content"
                );

                // INVARIANT 4: Artifacts are consistent with content
                let artifacts = got_artifacts.unwrap();
                assert!(
                    artifacts.exported_interface.contains_key(&func_name),
                    "Artifacts should contain function '{}' from content",
                    func_name
                );
            });
        }

        /// Property 8: Content Provider Consistency - Workspace Index
        ///
        /// When a document is only in WorkspaceIndex (not open), all accessor
        /// methods must return data from WorkspaceIndex, ensuring consistency.
        ///
        /// **Validates: Requirements 7.1, 7.2, 7.3**
        #[test]
        fn prop_content_provider_consistency_workspace_index(
            file_idx in file_index_strategy(),
            func_name in "[a-z][a-z0-9_]{1,10}",
            wd_name in "[a-z][a-z0-9_]{1,10}"
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let doc_store = make_test_document_store();
                let workspace_index = make_test_workspace_index();
                let file_cache = CrossFileFileCache::new();

                let uri = uri_from_index(file_idx);

                // Create index entry with specific content, metadata, and artifacts
                let content = format!("# @lsp-cd: {}\n{} <- function() {{}}", wd_name, func_name);
                let mut metadata = CrossFileMetadata::default();
                metadata.working_directory = Some(wd_name.clone());
                let mut artifacts = ScopeArtifacts::default();
                artifacts.exported_interface.insert(
                    func_name.clone(),
                    crate::cross_file::scope::ScopedSymbol {
                        name: func_name.clone(),
                        kind: crate::cross_file::scope::SymbolKind::Function,
                        source_uri: uri.clone(),
                        defined_line: 1,
                        defined_column: 0,
                        signature: None,
                    },
                );

                let index_entry = crate::workspace_index::IndexEntry {
                    contents: ropey::Rope::from_str(&content),
                    tree: None,
                    loaded_packages: vec![],
                    snapshot: crate::cross_file::file_cache::FileSnapshot {
                        mtime: std::time::SystemTime::UNIX_EPOCH,
                        size: content.len() as u64,
                        content_hash: None,
                    },
                    metadata,
                    artifacts,
                    indexed_at_version: 0,
                };
                workspace_index.insert(uri.clone(), index_entry);

                // Create provider
                let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

                // Get all data
                let got_content = provider.get_content(&uri);
                let got_metadata = provider.get_metadata(&uri);
                let got_artifacts = provider.get_artifacts(&uri);

                // INVARIANT 1: All methods return Some (consistent availability)
                assert!(got_content.is_some(), "get_content should return Some for indexed doc");
                assert!(got_metadata.is_some(), "get_metadata should return Some for indexed doc");
                assert!(got_artifacts.is_some(), "get_artifacts should return Some for indexed doc");

                // INVARIANT 2: Content matches what we indexed
                let content_str = got_content.unwrap();
                assert!(
                    content_str.contains(&func_name),
                    "Content should contain function name '{}', got: {}",
                    func_name,
                    content_str
                );

                // INVARIANT 3: Metadata is consistent with indexed data
                let got_meta = got_metadata.unwrap();
                assert_eq!(
                    got_meta.working_directory,
                    Some(wd_name.clone()),
                    "Metadata working_directory should match indexed data"
                );

                // INVARIANT 4: Artifacts are consistent with indexed data
                let got_arts = got_artifacts.unwrap();
                assert!(
                    got_arts.exported_interface.contains_key(&func_name),
                    "Artifacts should contain function '{}' from indexed data",
                    func_name
                );
            });
        }

        /// Property 8: Content Provider Consistency - Not Found
        ///
        /// When a URI is not found in any source, all accessor methods must
        /// consistently return None.
        ///
        /// **Validates: Requirements 7.1, 7.2, 7.3**
        #[test]
        fn prop_content_provider_consistency_not_found(
            file_idx in file_index_strategy()
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let doc_store = make_test_document_store();
                let workspace_index = make_test_workspace_index();
                let file_cache = CrossFileFileCache::new();

                let uri = uri_from_index(file_idx);

                // Don't add URI to any source

                // Create provider
                let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

                // Get all data
                let _got_content = provider.get_content(&uri);
                let got_metadata = provider.get_metadata(&uri);
                let got_artifacts = provider.get_artifacts(&uri);

                // INVARIANT: All methods return None (consistent unavailability)
                assert!(got_metadata.is_none(), "get_metadata should return None for unknown URI");
                assert!(got_artifacts.is_none(), "get_artifacts should return None for unknown URI");

                // exists_cached should return false
                assert!(
                    !provider.exists_cached(&uri),
                    "exists_cached should return false for unknown URI"
                );

                // is_open should return false
                assert!(
                    !provider.is_open(&uri),
                    "is_open should return false for unknown URI"
                );
            });
        }

        /// Property 8: Content Provider Consistency - Source Determination
        ///
        /// For any URI, the source used by get_content, get_metadata, and
        /// get_artifacts must be the same. This tests the invariant that
        /// we don't mix data from different sources.
        ///
        /// **Validates: Requirements 7.1, 7.2, 7.3**
        #[test]
        fn prop_content_provider_consistency_source_determination(
            file_idx in file_index_strategy(),
            // Use unique markers that won't be substrings of each other
            open_marker in 1000u32..2000,
            index_marker in 2000u32..3000,
            is_open in proptest::bool::ANY
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut doc_store = make_test_document_store();
                let workspace_index = make_test_workspace_index();
                let file_cache = CrossFileFileCache::new();

                let uri = uri_from_index(file_idx);

                // Use unique markers in function names and working directories
                let open_func = format!("open_func_{}", open_marker);
                let open_wd = format!("open_wd_{}", open_marker);
                let index_func = format!("index_func_{}", index_marker);
                let index_wd = format!("index_wd_{}", index_marker);

                // Always add to workspace index
                let index_content = format!("# @lsp-cd: {}\n{} <- function() {{}}", index_wd, index_func);
                let mut index_metadata = CrossFileMetadata::default();
                index_metadata.working_directory = Some(index_wd.clone());
                let mut index_artifacts = ScopeArtifacts::default();
                index_artifacts.exported_interface.insert(
                    index_func.clone(),
                    crate::cross_file::scope::ScopedSymbol {
                        name: index_func.clone(),
                        kind: crate::cross_file::scope::SymbolKind::Function,
                        source_uri: uri.clone(),
                        defined_line: 1,
                        defined_column: 0,
                        signature: None,
                    },
                );
                let index_entry = crate::workspace_index::IndexEntry {
                    contents: ropey::Rope::from_str(&index_content),
                    tree: None,
                    loaded_packages: vec![],
                    snapshot: crate::cross_file::file_cache::FileSnapshot {
                        mtime: std::time::SystemTime::UNIX_EPOCH,
                        size: index_content.len() as u64,
                        content_hash: None,
                    },
                    metadata: index_metadata,
                    artifacts: index_artifacts,
                    indexed_at_version: 0,
                };
                workspace_index.insert(uri.clone(), index_entry);

                // Conditionally open document
                if is_open {
                    let open_content = format!("# @lsp-cd: {}\n{} <- function() {{}}", open_wd, open_func);
                    doc_store.open(uri.clone(), &open_content, 1).await;
                }

                // Create provider
                let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

                // Get all data
                let got_content = provider.get_content(&uri).unwrap();
                let got_metadata = provider.get_metadata(&uri).unwrap();
                let got_artifacts = provider.get_artifacts(&uri).unwrap();

                // Determine which source was used based on content
                // Using unique markers ensures no false positives from substring matches
                let content_from_open = got_content.contains(&open_func);
                let content_from_index = got_content.contains(&index_func);

                // INVARIANT: Content must come from exactly one source
                assert!(
                    content_from_open != content_from_index,
                    "Content must come from exactly one source (open={}, index={})",
                    content_from_open,
                    content_from_index
                );

                // Determine expected source
                let expected_from_open = is_open;

                // INVARIANT: All accessors must use the same source
                if expected_from_open {
                    // All should come from DocumentStore
                    assert!(
                        content_from_open,
                        "Content should come from open doc when is_open=true"
                    );
                    assert_eq!(
                        got_metadata.working_directory,
                        Some(open_wd.clone()),
                        "Metadata should come from open doc when is_open=true"
                    );
                    assert!(
                        got_artifacts.exported_interface.contains_key(&open_func),
                        "Artifacts should come from open doc when is_open=true"
                    );
                } else {
                    // All should come from WorkspaceIndex
                    assert!(
                        content_from_index,
                        "Content should come from workspace index when is_open=false"
                    );
                    assert_eq!(
                        got_metadata.working_directory,
                        Some(index_wd.clone()),
                        "Metadata should come from workspace index when is_open=false"
                    );
                    assert!(
                        got_artifacts.exported_interface.contains_key(&index_func),
                        "Artifacts should come from workspace index when is_open=false"
                    );
                }
            });
        }
    }
}

// ============================================================================
// Integration Tests for Workspace Index Consolidation
// ============================================================================

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::document_store::DocumentStoreConfig;
    use crate::workspace_index::WorkspaceIndexConfig;

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///{}", name)).unwrap()
    }

    /// Integration test for full document lifecycle
    /// Tests: open → edit → close → workspace index update flow
    ///
    /// **Validates: Requirements 1.3, 1.4, 1.5, 3.4**
    #[tokio::test]
    async fn test_document_lifecycle() {
        let mut doc_store = DocumentStore::new(DocumentStoreConfig {
            max_documents: 10,
            max_memory_bytes: 10 * 1024 * 1024,
        });
        let workspace_index = WorkspaceIndex::new(WorkspaceIndexConfig {
            debounce_ms: 50,
            max_files: 100,
            max_file_size_bytes: 1024 * 1024,
        });
        let file_cache = CrossFileFileCache::new();

        let uri = test_uri("lifecycle_test.R");

        // Phase 1: Document not open, not in workspace index
        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);
        assert!(!provider.is_open(&uri));
        assert!(!provider.exists_cached(&uri));
        assert!(provider.get_content(&uri).is_none());

        // Phase 2: Open document
        let initial_content = "my_func <- function(x) { x + 1 }";
        doc_store.open(uri.clone(), initial_content, 1).await;

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);
        assert!(provider.is_open(&uri));
        assert!(provider.exists_cached(&uri));
        assert_eq!(
            provider.get_content(&uri),
            Some(initial_content.to_string())
        );

        // Phase 3: Edit document
        let changes = vec![tower_lsp::lsp_types::TextDocumentContentChangeEvent {
            range: Some(tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: tower_lsp::lsp_types::Position {
                    line: 0,
                    character: 7,
                },
            }),
            range_length: None,
            text: "new_func".to_string(),
        }];
        doc_store.update(&uri, changes, 2).await;

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);
        let content = provider.get_content(&uri).unwrap();
        assert!(content.contains("new_func"), "Content should reflect edit");

        // Phase 4: Close document
        doc_store.close(&uri);

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);
        assert!(!provider.is_open(&uri));

        // Phase 5: Add to workspace index (simulating file watcher update)
        let index_entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str("workspace_func <- function() {}"),
            tree: None,
            loaded_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 31,
                content_hash: None,
            },
            metadata: CrossFileMetadata::default(),
            artifacts: ScopeArtifacts::default(),
            indexed_at_version: 0,
        };
        workspace_index.insert(uri.clone(), index_entry);

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);
        assert!(provider.exists_cached(&uri));
        assert_eq!(
            provider.get_content(&uri),
            Some("workspace_func <- function() {}".to_string())
        );
    }

    /// Integration test for cross-file resolution
    /// Tests that cross-file features work with new architecture
    ///
    /// **Validates: Requirements 7.2, 13.2**
    #[tokio::test]
    async fn test_cross_file_resolution() {
        let mut doc_store = DocumentStore::new(DocumentStoreConfig::default());
        let workspace_index = WorkspaceIndex::new(WorkspaceIndexConfig::default());
        let file_cache = CrossFileFileCache::new();

        let main_uri = test_uri("main.R");
        let utils_uri = test_uri("utils.R");

        // Add utils.R to workspace index with exported function
        let mut utils_artifacts = ScopeArtifacts::default();
        utils_artifacts.exported_interface.insert(
            "helper_func".to_string(),
            crate::cross_file::scope::ScopedSymbol {
                name: "helper_func".to_string(),
                kind: crate::cross_file::scope::SymbolKind::Function,
                source_uri: utils_uri.clone(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
            },
        );
        let utils_entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str("helper_func <- function() {}"),
            tree: None,
            loaded_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 28,
                content_hash: None,
            },
            metadata: CrossFileMetadata::default(),
            artifacts: utils_artifacts.clone(),
            indexed_at_version: 0,
        };
        workspace_index.insert(utils_uri.clone(), utils_entry);

        // Open main.R that sources utils.R
        let main_content = "source('utils.R')\nhelper_func()";
        doc_store.open(main_uri.clone(), main_content, 1).await;

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        // Verify main.R is open and utils.R is in workspace index
        assert!(provider.is_open(&main_uri));
        assert!(!provider.is_open(&utils_uri));
        assert!(provider.exists_cached(&utils_uri));

        // Verify we can get artifacts from utils.R via ContentProvider
        let utils_artifacts_from_provider = provider.get_artifacts(&utils_uri);
        assert!(utils_artifacts_from_provider.is_some());
        assert!(utils_artifacts_from_provider
            .unwrap()
            .exported_interface
            .contains_key("helper_func"));
    }

    /// Integration test for async diagnostics
    /// Tests that missing file diagnostics work with async existence checks
    ///
    /// **Validates: Requirements 14.2, 14.5**
    #[tokio::test]
    async fn test_async_existence_checking() {
        let doc_store = DocumentStore::new(DocumentStoreConfig::default());
        let workspace_index = WorkspaceIndex::new(WorkspaceIndexConfig::default());
        let file_cache = CrossFileFileCache::new();

        let uri1 = test_uri("exists_in_store.R");
        let uri2 = test_uri("exists_in_index.R");
        let uri3 = test_uri("not_exists.R");

        // Add uri2 to workspace index
        let entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str("x <- 1"),
            tree: None,
            loaded_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 6,
                content_hash: None,
            },
            metadata: CrossFileMetadata::default(),
            artifacts: ScopeArtifacts::default(),
            indexed_at_version: 0,
        };
        workspace_index.insert(uri2.clone(), entry);

        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);

        // Test batch existence checking
        let uris = vec![uri1.clone(), uri2.clone(), uri3.clone()];
        let results = provider.check_existence_batch(&uris).await;

        // uri1 not in any cache, uri2 in workspace index, uri3 not anywhere
        assert_eq!(results.get(&uri1), Some(&false)); // Not in cache
        assert_eq!(results.get(&uri2), Some(&true)); // In workspace index
        assert_eq!(results.get(&uri3), Some(&false)); // Not anywhere
    }

    /// Test that open documents take precedence over workspace index
    ///
    /// **Validates: Requirements 3.1, 3.2, 3.4**
    #[tokio::test]
    async fn test_open_docs_precedence() {
        let mut doc_store = DocumentStore::new(DocumentStoreConfig::default());
        let workspace_index = WorkspaceIndex::new(WorkspaceIndexConfig::default());
        let file_cache = CrossFileFileCache::new();

        let uri = test_uri("precedence_test.R");

        // Add to workspace index first
        let index_entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str("old_content"),
            tree: None,
            loaded_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 11,
                content_hash: None,
            },
            metadata: CrossFileMetadata::default(),
            artifacts: ScopeArtifacts::default(),
            indexed_at_version: 0,
        };
        workspace_index.insert(uri.clone(), index_entry);

        // Verify workspace index content is returned when not open
        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);
        assert_eq!(provider.get_content(&uri), Some("old_content".to_string()));

        // Open document with different content
        doc_store.open(uri.clone(), "new_content", 1).await;

        // Verify open document content takes precedence
        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);
        assert_eq!(provider.get_content(&uri), Some("new_content".to_string()));

        // Close document
        doc_store.close(&uri);

        // Verify workspace index content is returned again
        let provider = DefaultContentProvider::new(&doc_store, &workspace_index, &file_cache);
        assert_eq!(provider.get_content(&uri), Some("old_content".to_string()));
    }
}
