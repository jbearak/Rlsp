//
// cross_file/file_cache.rs
//
// Disk file cache for cross-file awareness
//

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::RwLock;
use std::time::SystemTime;

use tower_lsp::lsp_types::Url;

/// Snapshot metadata for a closed file, used to determine cache validity
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileSnapshot {
    /// File modification time (from filesystem metadata)
    pub mtime: SystemTime,
    /// File size in bytes
    pub size: u64,
    /// Content hash (computed on first read)
    pub content_hash: Option<u64>,
}

impl FileSnapshot {
    /// Create snapshot from filesystem metadata
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            mtime: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: metadata.len(),
            content_hash: None,
        }
    }

    /// Create snapshot with content hash
    pub fn with_content_hash(metadata: &std::fs::Metadata, content: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        Self {
            mtime: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: metadata.len(),
            content_hash: Some(hasher.finish()),
        }
    }

    /// Check if this snapshot matches current disk state
    pub fn matches_disk(&self, current: &FileSnapshot) -> bool {
        self.mtime == current.mtime && self.size == current.size
    }
}

/// Cached file entry
#[derive(Debug, Clone)]
struct CachedFile {
    snapshot: FileSnapshot,
    content: String,
}

/// Disk file cache for closed files
#[derive(Debug, Default)]
pub struct CrossFileFileCache {
    /// Cached file contents by URI
    inner: RwLock<HashMap<Url, CachedFile>>,
}

impl CrossFileFileCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get cached content if snapshot is still fresh
    pub fn get_if_fresh(&self, uri: &Url, current_snapshot: &FileSnapshot) -> Option<String> {
        let guard = self.inner.read().ok()?;
        guard.get(uri).and_then(|cached| {
            if cached.snapshot.matches_disk(current_snapshot) {
                Some(cached.content.clone())
            } else {
                None
            }
        })
    }

    /// Get cached content without freshness check
    pub fn get(&self, uri: &Url) -> Option<String> {
        self.inner.read().ok()?.get(uri).map(|c| c.content.clone())
    }

    /// Insert content into cache
    pub fn insert(&self, uri: Url, snapshot: FileSnapshot, content: String) {
        if let Ok(mut guard) = self.inner.write() {
            guard.insert(uri, CachedFile { snapshot, content });
        }
    }

    /// Invalidate cache entry for a URI
    pub fn invalidate(&self, uri: &Url) {
        if let Ok(mut guard) = self.inner.write() {
            guard.remove(uri);
        }
    }

    /// Invalidate all cache entries
    pub fn invalidate_all(&self) {
        if let Ok(mut guard) = self.inner.write() {
            guard.clear();
        }
    }

    /// Read file from disk and cache it (synchronous, for use outside lock)
    pub fn read_and_cache(&self, uri: &Url) -> Option<String> {
        let path = uri.to_file_path().ok()?;
        let content = std::fs::read_to_string(&path).ok()?;
        let metadata = std::fs::metadata(&path).ok()?;
        let snapshot = FileSnapshot::with_content_hash(&metadata, &content);
        self.insert(uri.clone(), snapshot, content.clone());
        Some(content)
    }
}

/// Get file snapshot from disk (synchronous).
/// Reads filesystem metadata to create a snapshot for change detection.
pub fn get_file_snapshot(path: &Path) -> Option<FileSnapshot> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(FileSnapshot::from_metadata(&metadata))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///{}", name)).unwrap()
    }

    #[test]
    fn test_file_snapshot_matches() {
        let snap1 = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 100,
            content_hash: None,
        };
        let snap2 = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 100,
            content_hash: Some(12345),
        };
        // Matches based on mtime and size, not content_hash
        assert!(snap1.matches_disk(&snap2));
    }

    #[test]
    fn test_file_snapshot_mismatch_size() {
        let snap1 = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 100,
            content_hash: None,
        };
        let snap2 = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 200,
            content_hash: None,
        };
        assert!(!snap1.matches_disk(&snap2));
    }

    #[test]
    fn test_cache_insert_and_get() {
        let cache = CrossFileFileCache::new();
        let uri = test_uri("test.R");
        let snapshot = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 10,
            content_hash: None,
        };

        cache.insert(uri.clone(), snapshot.clone(), "content".to_string());
        assert_eq!(cache.get(&uri), Some("content".to_string()));
    }

    #[test]
    fn test_cache_get_if_fresh() {
        let cache = CrossFileFileCache::new();
        let uri = test_uri("test.R");
        let snapshot = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 10,
            content_hash: None,
        };

        cache.insert(uri.clone(), snapshot.clone(), "content".to_string());

        // Same snapshot should return content
        assert_eq!(
            cache.get_if_fresh(&uri, &snapshot),
            Some("content".to_string())
        );

        // Different snapshot should return None
        let new_snapshot = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 20,
            content_hash: None,
        };
        assert_eq!(cache.get_if_fresh(&uri, &new_snapshot), None);
    }

    #[test]
    fn test_cache_invalidate() {
        let cache = CrossFileFileCache::new();
        let uri = test_uri("test.R");
        let snapshot = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 10,
            content_hash: None,
        };

        cache.insert(uri.clone(), snapshot, "content".to_string());
        assert!(cache.get(&uri).is_some());

        cache.invalidate(&uri);
        assert!(cache.get(&uri).is_none());
    }

    #[test]
    fn test_read_and_cache() {
        let cache = CrossFileFileCache::new();

        // Create a temp file
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "x <- 1").unwrap();
        let path = temp.path();
        let uri = Url::from_file_path(path).unwrap();

        // Read and cache
        let content = cache.read_and_cache(&uri);
        assert!(content.is_some());
        assert!(content.unwrap().contains("x <- 1"));

        // Should be cached now
        assert!(cache.get(&uri).is_some());
    }
}
