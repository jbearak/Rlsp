//
// cross_file/content_provider.rs
//
// Unified file content provider for cross-file awareness
//

use std::collections::HashMap;
use std::path::Path;

use tower_lsp::lsp_types::Url;

use super::file_cache::{get_file_snapshot, CrossFileFileCache};
use super::scope::ScopeArtifacts;
use super::types::CrossFileMetadata;
use super::workspace_index::CrossFileWorkspaceIndex;

/// Trait for content providers that respect open-docs-authoritative rule
pub trait ContentProvider {
    /// Get content for a URI, preferring open documents over disk.
    ///
    /// CRITICAL: If the URI is in `open_documents`, return the in-memory content.
    /// Never return disk content for an open document.
    fn get_content(&self, uri: &Url) -> Option<String>;

    /// Get metadata for a URI, preferring open documents over index.
    fn get_metadata(&self, uri: &Url) -> Option<CrossFileMetadata>;

    /// Get scope artifacts for a URI, preferring open documents over cache.
    fn get_artifacts(&self, uri: &Url) -> Option<ScopeArtifacts>;
}

/// Document content accessor (minimal interface for open documents)
pub trait DocumentContent {
    fn content(&self) -> String;
}

/// Unified content provider with precedence:
/// 1. Open document (in-memory)
/// 2. Workspace index (cached)
/// 3. Disk file cache (on-demand)
pub struct CrossFileContentProvider<'a, D: DocumentContent> {
    /// Open documents (authoritative)
    pub open_documents: &'a HashMap<Url, D>,
    /// Workspace index for closed files
    pub workspace_index: &'a CrossFileWorkspaceIndex,
    /// Disk file cache for on-demand reads
    pub file_cache: &'a CrossFileFileCache,
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
        }
    }

    /// Check if a URI is currently open
    pub fn is_open(&self, uri: &Url) -> bool {
        self.open_documents.contains_key(uri)
    }
}

impl<'a, D: DocumentContent> ContentProvider for CrossFileContentProvider<'a, D> {
    fn get_content(&self, uri: &Url) -> Option<String> {
        // 1. Open document is authoritative
        if let Some(doc) = self.open_documents.get(uri) {
            return Some(doc.content());
        }

        // 2. Try workspace index
        // Note: We don't have content in the index, only metadata/artifacts
        // So we fall through to file cache

        // 3. Try file cache or read from disk
        let path = uri.to_file_path().ok()?;
        if let Some(snapshot) = get_file_snapshot(&path) {
            if let Some(content) = self.file_cache.get_if_fresh(uri, &snapshot) {
                return Some(content);
            }
        }

        // Read from disk and cache
        self.file_cache.read_and_cache(uri)
    }

    fn get_metadata(&self, uri: &Url) -> Option<CrossFileMetadata> {
        // 1. Open document - would need to extract metadata
        // For now, we don't store extracted metadata for open docs in this provider
        // The caller should handle open docs separately

        if self.open_documents.contains_key(uri) {
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

    fn get_artifacts(&self, uri: &Url) -> Option<ScopeArtifacts> {
        // 1. Open document - would need to compute artifacts
        if self.open_documents.contains_key(uri) {
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
    uri.to_file_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Check if a path exists on disk.
/// Direct filesystem existence check for Path objects.
pub fn path_exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
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
        open_docs.insert(uri.clone(), MockDocument {
            content: "open content".to_string(),
        });

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
        open_docs.insert(uri.clone(), MockDocument {
            content: "content".to_string(),
        });

        let index = CrossFileWorkspaceIndex::new();
        let cache = CrossFileFileCache::new();

        let provider = CrossFileContentProvider::new(&open_docs, &index, &cache);

        assert!(provider.is_open(&uri));
        assert!(!provider.is_open(&test_uri("other.R")));
    }

    #[test]
    fn test_reads_from_disk() {
        let open_docs: HashMap<Url, MockDocument> = HashMap::new();
        let index = CrossFileWorkspaceIndex::new();
        let cache = CrossFileFileCache::new();

        // Create a temp file
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "disk content").unwrap();
        let uri = Url::from_file_path(temp.path()).unwrap();

        let provider = CrossFileContentProvider::new(&open_docs, &index, &cache);

        // Should read from disk
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
