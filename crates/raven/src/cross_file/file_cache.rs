//
// cross_file/file_cache.rs
//
// Disk file cache for cross-file awareness
//

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use lru::LruCache;
use tower_lsp::lsp_types::Url;

use super::cache::non_zero_or;

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

/// Default capacity for the file content cache
const DEFAULT_FILE_CACHE_CAPACITY: usize = 500;

/// Default capacity for the existence cache
const DEFAULT_EXISTENCE_CACHE_CAPACITY: usize = 2000;

/// Disk file cache for closed files with LRU eviction.
///
/// Uses `peek()` for reads (no LRU promotion, works under read lock) and
/// `push()` for writes (promotes/evicts under write lock).
pub struct CrossFileFileCache {
    /// Cached file contents by URI (LRU-bounded)
    inner: RwLock<LruCache<Url, CachedFile>>,
    /// Cached file existence by path (LRU-bounded)
    existence: RwLock<LruCache<PathBuf, bool>>,
}

impl std::fmt::Debug for CrossFileFileCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossFileFileCache").finish_non_exhaustive()
    }
}

impl Default for CrossFileFileCache {
    fn default() -> Self {
        Self::with_capacities(
            DEFAULT_FILE_CACHE_CAPACITY,
            DEFAULT_EXISTENCE_CACHE_CAPACITY,
        )
    }
}

impl CrossFileFileCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacities(content_cap: usize, existence_cap: usize) -> Self {
        let content_cap = non_zero_or(content_cap, DEFAULT_FILE_CACHE_CAPACITY);
        let existence_cap = non_zero_or(existence_cap, DEFAULT_EXISTENCE_CACHE_CAPACITY);
        Self {
            inner: RwLock::new(LruCache::new(content_cap)),
            existence: RwLock::new(LruCache::new(existence_cap)),
        }
    }

    /// Check if a path exists (cached, non-blocking read)
    pub fn path_exists(&self, path: &Path) -> Option<bool> {
        self.existence.read().ok()?.peek(path).copied()
    }

    /// Update existence cache (called after background check).
    /// LRU eviction automatically bounds memory.
    pub fn cache_existence(&self, path: &Path, exists: bool) {
        if let Ok(mut guard) = self.existence.write() {
            guard.push(path.to_path_buf(), exists);
        }
    }

    /// Get cached content if snapshot is still fresh
    pub fn get_if_fresh(&self, uri: &Url, current_snapshot: &FileSnapshot) -> Option<String> {
        let guard = self.inner.read().ok()?;
        guard.peek(uri).and_then(|cached| {
            if cached.snapshot.matches_disk(current_snapshot) {
                Some(cached.content.clone())
            } else {
                None
            }
        })
    }

    /// Get cached content without freshness check
    pub fn get(&self, uri: &Url) -> Option<String> {
        self.inner.read().ok()?.peek(uri).map(|c| c.content.clone())
    }

    /// Insert content into cache. LRU eviction automatically bounds memory.
    pub fn insert(&self, uri: Url, snapshot: FileSnapshot, content: String) {
        if let Ok(mut guard) = self.inner.write() {
            guard.push(uri, CachedFile { snapshot, content });
        }
    }

    /// Invalidate cache entry for a URI
    pub fn invalidate(&self, uri: &Url) {
        if let Ok(mut guard) = self.inner.write() {
            guard.pop(uri);
        }
    }

    /// Invalidate all cache entries
    pub fn invalidate_all(&self) {
        if let Ok(mut guard) = self.inner.write() {
            guard.clear();
        }
        if let Ok(mut guard) = self.existence.write() {
            guard.clear();
        }
    }

    /// Read file from disk and cache it (synchronous, for use outside lock).
    ///
    /// **Warning**: This performs blocking filesystem I/O. Callers on LSP
    /// request threads should use `spawn_blocking` or an equivalent async
    /// wrapper to avoid blocking the event loop.
    pub fn read_and_cache(&self, uri: &Url) -> Option<String> {
        let path = uri.to_file_path().ok()?;
        let content = std::fs::read_to_string(&path).ok()?;
        let metadata = std::fs::metadata(&path).ok()?;
        let snapshot = FileSnapshot::with_content_hash(&metadata, &content);
        self.insert(uri.clone(), snapshot, content.clone());
        Some(content)
    }

    /// Resize both caches. If shrinking, LRU entries are evicted.
    pub fn resize(&self, content_cap: usize, existence_cap: usize) {
        let content_cap = non_zero_or(content_cap, DEFAULT_FILE_CACHE_CAPACITY);
        let existence_cap = non_zero_or(existence_cap, DEFAULT_EXISTENCE_CACHE_CAPACITY);
        if let Ok(mut guard) = self.inner.write() {
            guard.resize(content_cap);
        }
        if let Ok(mut guard) = self.existence.write() {
            guard.resize(existence_cap);
        }
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

    #[test]
    fn test_content_cache_lru_eviction() {
        let cache = CrossFileFileCache::with_capacities(2, 100);
        let uri1 = test_uri("a.R");
        let uri2 = test_uri("b.R");
        let uri3 = test_uri("c.R");
        let snap = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: 10,
            content_hash: None,
        };

        cache.insert(uri1.clone(), snap.clone(), "a".to_string());
        cache.insert(uri2.clone(), snap.clone(), "b".to_string());

        // Both present
        assert!(cache.get(&uri1).is_some());
        assert!(cache.get(&uri2).is_some());

        // Third evicts uri1 (oldest by insertion time)
        cache.insert(uri3.clone(), snap, "c".to_string());
        assert!(cache.get(&uri1).is_none(), "LRU entry should be evicted");
        assert!(cache.get(&uri2).is_some());
        assert!(cache.get(&uri3).is_some());
    }

    #[test]
    fn test_existence_cache_lru_eviction() {
        let cache = CrossFileFileCache::with_capacities(100, 2);

        cache.cache_existence(Path::new("/a"), true);
        cache.cache_existence(Path::new("/b"), false);
        assert_eq!(cache.path_exists(Path::new("/a")), Some(true));
        assert_eq!(cache.path_exists(Path::new("/b")), Some(false));

        // Third evicts /a (oldest)
        cache.cache_existence(Path::new("/c"), true);
        assert_eq!(cache.path_exists(Path::new("/a")), None);
        assert_eq!(cache.path_exists(Path::new("/b")), Some(false));
        assert_eq!(cache.path_exists(Path::new("/c")), Some(true));
    }
}
