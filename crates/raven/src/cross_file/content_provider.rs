//
// cross_file/content_provider.rs
//
// Unified file content provider for cross-file awareness
//

use std::collections::HashMap;
use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use super::file_cache::CrossFileFileCache;
use super::scope::ScopeArtifacts;
use super::types::CrossFileMetadata;
use super::workspace_index::CrossFileWorkspaceIndex;
use crate::state::OpenDocumentAliases;

/// Trait for content providers that respect open-docs-authoritative rule
pub trait ContentProvider {
    /// Get content for a URI, preferring open documents over disk.
    ///
    /// CRITICAL: If the URI is in `open_documents`, return the in-memory content.
    /// Never return disk content for an open document.
    fn get_content(&self, uri: &Url) -> Option<String>;

    /// Get metadata for a URI, preferring open documents over index.
    fn get_metadata(&self, uri: &Url) -> Option<Arc<CrossFileMetadata>>;

    /// Get scope artifacts for a URI, preferring open documents over cache.
    fn get_artifacts(&self, uri: &Url) -> Option<Arc<ScopeArtifacts>>;
}

/// Document content accessor (minimal interface for open documents)
pub trait DocumentContent {
    fn content(&self) -> String;
}

/// Unified content provider with precedence:
/// 1. Open document (in-memory)
/// 2. Workspace index (cached)
/// 3. Disk file cache (cached-only; no synchronous disk I/O)
pub struct CrossFileContentProvider<'a, D: DocumentContent> {
    /// Open documents (authoritative)
    pub open_documents: &'a HashMap<Url, D>,
    /// Workspace index for closed files
    pub workspace_index: &'a CrossFileWorkspaceIndex,
    /// Disk file cache for on-demand reads
    pub file_cache: &'a CrossFileFileCache,
    /// Open-document aliases keyed by canonical graph URI
    pub open_aliases: Option<&'a OpenDocumentAliases>,
}

impl<'a, D: DocumentContent> CrossFileContentProvider<'a, D> {
    pub fn new(
        open_documents: &'a HashMap<Url, D>,
        workspace_index: &'a CrossFileWorkspaceIndex,
        file_cache: &'a CrossFileFileCache,
    ) -> Self {
        Self {
            open_documents,
            workspace_index,
            file_cache,
            open_aliases: None,
        }
    }

    pub fn new_with_aliases(
        open_documents: &'a HashMap<Url, D>,
        workspace_index: &'a CrossFileWorkspaceIndex,
        file_cache: &'a CrossFileFileCache,
        open_aliases: &'a OpenDocumentAliases,
    ) -> Self {
        Self {
            open_documents,
            workspace_index,
            file_cache,
            open_aliases: Some(open_aliases),
        }
    }

    fn open_alias_uris(&self, uri: &Url) -> Option<&[Url]> {
        self.open_aliases
            .filter(|aliases| !aliases.is_empty())
            .and_then(|aliases| aliases.open_uris_for_canonical(uri))
    }

    /// Check if a URI is currently open
    pub fn is_open(&self, uri: &Url) -> bool {
        self.open_documents.contains_key(uri)
            || self.open_alias_uris(uri).is_some_and(|open_uris| {
                open_uris
                    .iter()
                    .any(|open_uri| self.open_documents.contains_key(open_uri))
            })
    }
}

impl<'a, D: DocumentContent> ContentProvider for CrossFileContentProvider<'a, D> {
    fn get_content(&self, uri: &Url) -> Option<String> {
        // 1. Open document is authoritative
        if let Some(doc) = self.open_documents.get(uri) {
            return Some(doc.content());
        }

        if let Some(open_uris) = self.open_alias_uris(uri) {
            for open_uri in open_uris {
                if let Some(doc) = self.open_documents.get(open_uri) {
                    return Some(doc.content());
                }
            }
        }

        // 2. Try workspace index
        // Note: We don't have content in the index, only metadata/artifacts
        // So we fall through to file cache

        // 3. Try file cache (no synchronous disk I/O)
        self.file_cache.get(uri)
    }

    fn get_metadata(&self, uri: &Url) -> Option<Arc<CrossFileMetadata>> {
        // 1. Open document - would need to extract metadata
        // For now, we don't store extracted metadata for open docs in this provider
        // The caller should handle open docs separately

        if self.is_open(uri) {
            // Open doc - caller should use their own metadata extraction
            return None;
        }

        // 2. Try workspace index
        if let Some(metadata) = self.workspace_index.get_metadata(uri) {
            return Some(metadata);
        }

        // 3. No metadata available - would need to read and parse
        None
    }

    fn get_artifacts(&self, uri: &Url) -> Option<Arc<ScopeArtifacts>> {
        // 1. Open document - would need to compute artifacts
        if self.is_open(uri) {
            // Open doc - caller should use their own artifacts computation
            return None;
        }

        // 2. Try workspace index
        if let Some(artifacts) = self.workspace_index.get_artifacts(uri) {
            return Some(artifacts);
        }

        // 3. No artifacts available - would need to read and compute
        None
    }
}

/// Check if a file exists on disk.
/// Converts URI to file path and checks filesystem existence.
pub fn file_exists(uri: &Url) -> bool {
    uri.to_file_path().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_file::file_cache::FileSnapshot;
    use std::io::Write;
    use tempfile::NamedTempFile;

    struct MockDocument {
        content: String,
    }

    impl DocumentContent for MockDocument {
        fn content(&self) -> String {
            self.content.clone()
        }
    }

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///{}", name)).unwrap()
    }

    #[test]
    fn test_open_doc_is_authoritative() {
        let mut open_docs = HashMap::new();
        let uri = test_uri("test.R");
        open_docs.insert(
            uri.clone(),
            MockDocument {
                content: "open content".to_string(),
            },
        );

        let index = CrossFileWorkspaceIndex::new();
        let cache = CrossFileFileCache::new();

        let provider = CrossFileContentProvider::new(&open_docs, &index, &cache);

        // Should return open document content
        assert_eq!(provider.get_content(&uri), Some("open content".to_string()));
    }

    #[test]
    fn test_is_open() {
        let mut open_docs = HashMap::new();
        let uri = test_uri("test.R");
        open_docs.insert(
            uri.clone(),
            MockDocument {
                content: "content".to_string(),
            },
        );

        let index = CrossFileWorkspaceIndex::new();
        let cache = CrossFileFileCache::new();

        let provider = CrossFileContentProvider::new(&open_docs, &index, &cache);

        assert!(provider.is_open(&uri));
        assert!(!provider.is_open(&test_uri("other.R")));
    }

    #[test]
    fn test_reads_from_cache_only() {
        let open_docs: HashMap<Url, MockDocument> = HashMap::new();
        let index = CrossFileWorkspaceIndex::new();
        let cache = CrossFileFileCache::new();
        // Create a temp file and seed the cache
        // Create a temp file
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "disk content").unwrap();
        let uri = Url::from_file_path(temp.path()).unwrap();
        let content = std::fs::read_to_string(temp.path()).unwrap();
        let metadata = std::fs::metadata(temp.path()).unwrap();
        let snapshot = FileSnapshot::with_content_hash(&metadata, &content);
        cache.insert(uri.clone(), snapshot, content.clone());

        let provider = CrossFileContentProvider::new(&open_docs, &index, &cache);

        // Should read from cache (no disk I/O in provider)
        let content = provider.get_content(&uri);
        assert!(content.is_some());
        assert!(content.unwrap().contains("disk content"));
    }

    #[test]
    fn test_file_exists() {
        let temp = NamedTempFile::new().unwrap();
        let uri = Url::from_file_path(temp.path()).unwrap();

        assert!(file_exists(&uri));
        assert!(!file_exists(&test_uri("nonexistent.R")));
    }
}
