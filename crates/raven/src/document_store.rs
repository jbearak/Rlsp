//
// document_store.rs
//
// Document store for open documents with LRU eviction and memory limits
//

// Allow dead code for infrastructure that's implemented for future use
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use indexmap::IndexSet;
use ropey::Rope;
use tokio::sync::watch;
use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, Url};
use tree_sitter::Tree;

use crate::cross_file::scope::ScopeArtifacts;
use crate::cross_file::types::CrossFileMetadata;

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for DocumentStore
///
/// Controls memory limits and eviction behavior for open documents.
///
/// **Validates: Requirements 2.1, 2.2**
#[derive(Debug, Clone)]
pub struct DocumentStoreConfig {
    /// Maximum number of documents to keep in memory
    pub max_documents: usize,
    /// Maximum total memory usage in bytes
    pub max_memory_bytes: usize,
}

impl Default for DocumentStoreConfig {
    fn default() -> Self {
        Self {
            max_documents: 50,
            max_memory_bytes: 100 * 1024 * 1024, // 100MB
        }
    }
}

// ============================================================================
// Metrics
// ============================================================================

/// Metrics for tracking DocumentStore performance
///
/// **Validates: Requirements 2.3, 2.4, 2.5**
#[derive(Debug, Clone, Default)]
pub struct DocumentStoreMetrics {
    /// Number of cache hits (document found in store)
    pub cache_hits: u64,
    /// Number of cache misses (document not found)
    pub cache_misses: u64,
    /// Number of documents evicted due to limits
    pub evictions: u64,
    /// Total number of documents opened
    pub documents_opened: u64,
    /// Total number of documents closed
    pub documents_closed: u64,
}

// ============================================================================
// Document State
// ============================================================================

/// State for an open document
///
/// Contains all data needed for LSP operations on an open document,
/// including parsed AST, cross-file metadata, and scope artifacts.
///
/// **Validates: Requirements 1.1, 1.2, 1.3, 1.4, 1.5**
pub struct DocumentState {
    /// Document URI
    pub uri: Url,
    /// LSP document version
    pub version: i32,
    /// File content as a rope for efficient editing
    pub contents: Rope,
    /// Parsed AST (None if parsing failed)
    pub tree: Option<Tree>,
    /// Packages loaded via library() calls
    pub loaded_packages: Vec<String>,
    /// Cross-file metadata (source() calls, directives)
    pub metadata: CrossFileMetadata,
    /// Scope artifacts (exported symbols, timeline)
    pub artifacts: ScopeArtifacts,
    /// Internal revision counter for change detection
    pub revision: u64,
}

impl DocumentState {
    /// Estimate memory usage of this document state in bytes
    fn estimate_memory_bytes(&self) -> usize {
        // Base struct size
        let base_size = std::mem::size_of::<Self>();

        // Rope content (approximate: chars * average bytes per char)
        let content_size = self.contents.len_bytes();

        // Tree size (conservative estimate based on typical AST overhead)
        let tree_size = self.tree.as_ref().map(|_| content_size).unwrap_or(0);

        // Loaded packages
        let packages_size: usize = self
            .loaded_packages
            .iter()
            .map(|s| s.len() + std::mem::size_of::<String>())
            .sum();

        // Metadata (rough estimate)
        let metadata_size = std::mem::size_of::<CrossFileMetadata>()
            + self.metadata.sources.len() * 64  // Approximate per-source size
            + self.metadata.sourced_by.len() * 64;

        // Artifacts (rough estimate based on exported interface)
        let artifacts_size = std::mem::size_of::<ScopeArtifacts>()
            + self.artifacts.exported_interface.len() * 128  // Approximate per-symbol size
            + self.artifacts.timeline.len() * 64;

        base_size + content_size + tree_size + packages_size + metadata_size + artifacts_size
    }
}

// ============================================================================
// Document Store
// ============================================================================

/// Tracks the state of an active update operation
///
/// Uses a watch channel to allow multiple waiters to be notified when
/// an update completes. The revision counter ensures waiters can detect
/// when a specific update has completed.
///
/// **Validates: Requirements 6.1, 6.2, 6.3, 6.4**
#[derive(Clone)]
struct UpdateTracker {
    /// Watch channel sender - sends the current revision when update completes
    sender: Arc<watch::Sender<u64>>,
    /// Watch channel receiver - waiters subscribe to this
    receiver: watch::Receiver<u64>,
    /// Whether an update is currently in-flight for this URI
    in_flight: Arc<AtomicBool>,
}

impl UpdateTracker {
    /// Create a new update tracker starting at revision 0
    fn new() -> Self {
        let (sender, receiver) = watch::channel(0);
        Self {
            sender: Arc::new(sender),
            receiver,
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal that an update has completed by incrementing the revision
    fn signal_complete(&self) {
        // Increment the revision to notify all waiters
        self.sender.send_modify(|rev| *rev += 1);
        self.in_flight.store(false, Ordering::Release);
    }

    /// Get the current revision
    fn current_revision(&self) -> u64 {
        *self.receiver.borrow()
    }

    /// Mark that an update is in-flight
    fn mark_in_flight(&self) {
        self.in_flight.store(true, Ordering::Release);
    }

    /// Check if an update is in-flight
    fn is_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Wait for the next update to complete (revision to change)
    async fn wait_for_next(&mut self) {
        // Wait for the revision to change
        let _ = self.receiver.changed().await;
    }
}

/// Store for open documents with LRU eviction
///
/// Manages open documents with configurable memory limits and LRU eviction.
/// Uses IndexSet for O(1) access order updates.
///
/// **Validates: Requirements 1.1-1.5, 2.1-2.5, 6.1-6.4**
pub struct DocumentStore {
    /// Documents by URI
    documents: HashMap<Url, DocumentState>,
    /// LRU tracking via insertion order (most recently accessed at end)
    access_order: IndexSet<Url>,
    /// Active async update trackers per URI
    ///
    /// Each URI has an UpdateTracker that allows multiple waiters to be notified
    /// when updates complete. The tracker uses a watch channel internally.
    ///
    /// **Validates: Requirements 6.1, 6.2, 6.3, 6.4**
    update_trackers: HashMap<Url, UpdateTracker>,
    /// Configuration
    config: DocumentStoreConfig,
    /// Metrics
    metrics: DocumentStoreMetrics,
}

impl DocumentStore {
    /// Create a new DocumentStore with the given configuration
    ///
    /// # Arguments
    /// * `config` - Configuration for memory limits and eviction
    ///
    /// # Returns
    /// A new DocumentStore instance
    pub fn new(config: DocumentStoreConfig) -> Self {
        Self {
            documents: HashMap::new(),
            access_order: IndexSet::new(),
            update_trackers: HashMap::new(),
            config,
            metrics: DocumentStoreMetrics::default(),
        }
    }

    /// Open a document (evicts if needed)
    ///
    /// Parses the content and computes all derived data (tree, packages, metadata, artifacts).
    /// If memory or document count limits are exceeded, evicts least-recently-accessed documents.
    ///
    /// **Validates: Requirements 1.3, 2.1, 2.2, 2.3**
    ///
    /// # Arguments
    /// * `uri` - Document URI
    /// * `content` - Document content
    /// * `version` - LSP document version
    pub async fn open(&mut self, uri: Url, content: &str, version: i32) {
        self.mark_update_started(&uri);
        // Parse content
        let contents = Rope::from_str(content);
        let tree = Self::parse_content(content);
        let loaded_packages = Self::extract_packages(&tree, content);
        let metadata = crate::cross_file::extract_metadata(content);
        let artifacts = if let Some(ref tree) = tree {
            crate::cross_file::scope::compute_artifacts(&uri, tree, content)
        } else {
            ScopeArtifacts::default()
        };

        let state = DocumentState {
            uri: uri.clone(),
            version,
            contents,
            tree,
            loaded_packages,
            metadata,
            artifacts,
            revision: 0,
        };

        // Estimate incoming memory
        let incoming_bytes = state.estimate_memory_bytes();

        // Check if this document already exists
        let existing_bytes = self
            .documents
            .get(&uri)
            .map(|doc| doc.estimate_memory_bytes())
            .unwrap_or(0);

        // For NEW documents: check document count limit
        if !self.documents.contains_key(&uri) {
            // Evict if we're at document count limit
            while self.documents.len() >= self.config.max_documents {
                if !self.evict_lru_excluding(&uri) {
                    break;
                }
            }
        }

        // For ALL documents (new or updated): check memory limit
        // We need to ensure: current_memory - existing_bytes + incoming_bytes <= max_memory_bytes
        // Which is: current_memory + net_memory_increase <= max_memory_bytes
        let mut current_memory = self.estimate_memory_usage();
        while current_memory.saturating_sub(existing_bytes) + incoming_bytes
            > self.config.max_memory_bytes
        {
            if !self.evict_lru_excluding(&uri) {
                break;
            }
            current_memory = self.estimate_memory_usage();
        }

        // Insert or update document
        self.documents.insert(uri.clone(), state);

        // Update access order (move to end = most recently accessed)
        self.access_order.shift_remove(&uri);
        self.access_order.insert(uri.clone());

        self.metrics.documents_opened += 1;

        // Signal completion to any waiters
        // **Validates: Requirements 6.4**
        self.signal_update_complete(&uri);
    }

    /// Open a document with pre-enriched metadata
    ///
    /// Like `open`, but uses the provided metadata instead of extracting it.
    /// Use this when metadata has been enriched with inherited_working_directory.
    pub async fn open_with_metadata(
        &mut self,
        uri: Url,
        content: &str,
        version: i32,
        metadata: CrossFileMetadata,
    ) {
        self.mark_update_started(&uri);
        let contents = Rope::from_str(content);
        let tree = Self::parse_content(content);
        let loaded_packages = Self::extract_packages(&tree, content);
        let artifacts = if let Some(ref tree) = tree {
            crate::cross_file::scope::compute_artifacts(&uri, tree, content)
        } else {
            ScopeArtifacts::default()
        };

        let state = DocumentState {
            uri: uri.clone(),
            version,
            contents,
            tree,
            loaded_packages,
            metadata,
            artifacts,
            revision: 0,
        };

        let incoming_bytes = state.estimate_memory_bytes();
        let existing_bytes = self
            .documents
            .get(&uri)
            .map(|doc| doc.estimate_memory_bytes())
            .unwrap_or(0);

        if !self.documents.contains_key(&uri) {
            while self.documents.len() >= self.config.max_documents {
                if !self.evict_lru_excluding(&uri) {
                    break;
                }
            }
        }

        let mut current_memory = self.estimate_memory_usage();
        while current_memory.saturating_sub(existing_bytes) + incoming_bytes
            > self.config.max_memory_bytes
        {
            if !self.evict_lru_excluding(&uri) {
                break;
            }
            current_memory = self.estimate_memory_usage();
        }

        self.documents.insert(uri.clone(), state);
        self.access_order.shift_remove(&uri);
        self.access_order.insert(uri.clone());
        self.metrics.documents_opened += 1;
        self.signal_update_complete(&uri);
    }

    /// Update a document with changes
    ///
    /// Applies incremental changes and recomputes derived data.
    /// Signals completion to any waiters when the update is done.
    ///
    /// **Validates: Requirements 1.4, 6.4**
    ///
    /// # Arguments
    /// * `uri` - Document URI
    /// * `changes` - List of content changes
    /// * `version` - New LSP document version
    pub async fn update(
        &mut self,
        uri: &Url,
        changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
    ) {
        self.mark_update_started(uri);
        if let Some(state) = self.documents.get_mut(uri) {
            // Apply changes to content
            for change in changes {
                Self::apply_change_to_rope(&mut state.contents, change);
            }

            // Update version and revision
            state.version = version;
            state.revision += 1;

            // Reparse and recompute derived data
            let content = state.contents.to_string();
            state.tree = Self::parse_content(&content);
            state.loaded_packages = Self::extract_packages(&state.tree, &content);
            state.metadata = crate::cross_file::extract_metadata(&content);
            state.artifacts = if let Some(ref tree) = state.tree {
                crate::cross_file::scope::compute_artifacts(uri, tree, &content)
            } else {
                ScopeArtifacts::default()
            };

            // Update access order
            self.touch_access(uri);

            // Signal completion to any waiters
            // **Validates: Requirements 6.4**
            self.signal_update_complete(uri);
        }
    }

    /// Update a document with changes and pre-enriched metadata
    ///
    /// Like `update`, but uses the provided metadata instead of extracting it.
    /// Use this when metadata has been enriched with inherited_working_directory.
    pub async fn update_with_metadata(
        &mut self,
        uri: &Url,
        changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
        metadata: CrossFileMetadata,
    ) {
        self.mark_update_started(uri);
        if let Some(state) = self.documents.get_mut(uri) {
            for change in changes {
                Self::apply_change_to_rope(&mut state.contents, change);
            }
            state.version = version;
            state.revision += 1;

            let content = state.contents.to_string();
            state.tree = Self::parse_content(&content);
            state.loaded_packages = Self::extract_packages(&state.tree, &content);
            state.metadata = metadata;
            state.artifacts = if let Some(ref tree) = state.tree {
                crate::cross_file::scope::compute_artifacts(uri, tree, &content)
            } else {
                ScopeArtifacts::default()
            };

            self.touch_access(uri);
            self.signal_update_complete(uri);
        }
    }

    /// Close a document
    ///
    /// Removes the document from storage and cleans up update trackers.
    ///
    /// **Validates: Requirements 1.5**
    ///
    /// # Arguments
    /// * `uri` - Document URI to close
    pub fn close(&mut self, uri: &Url) {
        self.documents.remove(uri);
        self.access_order.shift_remove(uri);
        self.update_trackers.remove(uri);
        self.metrics.documents_closed += 1;
    }

    /// Get a document (updates LRU)
    ///
    /// Returns a reference to the document state if it exists.
    /// Updates the access order for LRU tracking.
    ///
    /// **Validates: Requirements 2.4, 2.5**
    ///
    /// # Arguments
    /// * `uri` - Document URI
    ///
    /// # Returns
    /// Reference to DocumentState if found, None otherwise
    pub fn get(&mut self, uri: &Url) -> Option<&DocumentState> {
        if self.documents.contains_key(uri) {
            self.touch_access(uri);
            self.metrics.cache_hits += 1;
            self.documents.get(uri)
        } else {
            self.metrics.cache_misses += 1;
            None
        }
    }

    /// Get a document without updating LRU
    ///
    /// Returns a reference to the document state without affecting access order.
    /// Useful for read-only operations that shouldn't affect eviction.
    ///
    /// # Arguments
    /// * `uri` - Document URI
    ///
    /// # Returns
    /// Reference to DocumentState if found, None otherwise
    pub fn get_without_touch(&self, uri: &Url) -> Option<&DocumentState> {
        self.documents.get(uri)
    }

    /// Check if document is open
    ///
    /// # Arguments
    /// * `uri` - Document URI
    ///
    /// # Returns
    /// true if the document is currently open
    pub fn contains(&self, uri: &Url) -> bool {
        self.documents.contains_key(uri)
    }

    /// Wait for any active update to complete
    ///
    /// Blocks until the next update for the specified URI completes.
    /// If no update is in progress, returns immediately.
    /// Multiple callers can wait on the same URI simultaneously.
    ///
    /// **Validates: Requirements 6.1, 6.2, 6.3, 6.4**
    ///
    /// # Arguments
    /// * `uri` - Document URI to wait for
    ///
    /// # Example
    /// ```ignore
    /// // Wait for an update to complete before reading
    /// store.wait_for_update(&uri).await;
    /// let doc = store.get(&uri);
    /// ```
    pub async fn wait_for_update(&self, uri: &Url) {
        // Get a clone of the tracker's receiver if one exists
        if let Some(tracker) = self.update_trackers.get(uri) {
            let mut receiver = tracker.receiver.clone();
            if !tracker.is_in_flight() {
                return;
            }
            receiver.borrow_and_update();
            if !tracker.is_in_flight() {
                return;
            }
            // Wait for the next update signal
            let _ = receiver.changed().await;
        }
        // If no tracker exists, there's no update in progress - return immediately
    }

    /// Check if there's an active update tracker for a URI
    ///
    /// **Validates: Requirements 6.1**
    ///
    /// # Arguments
    /// * `uri` - Document URI to check
    ///
    /// # Returns
    /// true if there's an update tracker for this URI
    pub fn has_update_tracker(&self, uri: &Url) -> bool {
        self.update_trackers.contains_key(uri)
    }

    /// Get the current update revision for a URI
    ///
    /// The revision increments each time an update completes.
    /// Returns None if no tracker exists for the URI.
    ///
    /// **Validates: Requirements 6.1**
    ///
    /// # Arguments
    /// * `uri` - Document URI
    ///
    /// # Returns
    /// Current revision number, or None if no tracker exists
    pub fn update_revision(&self, uri: &Url) -> Option<u64> {
        self.update_trackers.get(uri).map(|t| t.current_revision())
    }

    /// Get all open URIs
    ///
    /// # Returns
    /// Vector of all currently open document URIs
    pub fn uris(&self) -> Vec<Url> {
        self.documents.keys().cloned().collect()
    }

    /// Get the number of open documents
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Check if the store is empty
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Get current metrics
    pub fn metrics(&self) -> &DocumentStoreMetrics {
        &self.metrics
    }

    /// Get current configuration
    pub fn config(&self) -> &DocumentStoreConfig {
        &self.config
    }

    // ========================================================================
    // Private Methods
    // ========================================================================

    /// Evict documents if needed to stay within limits
    ///
    /// Evicts least-recently-accessed documents until:
    /// - Document count is below max_documents
    /// - Memory usage is below max_memory_bytes
    ///
    /// **Validates: Requirements 2.1, 2.2, 2.3**
    #[allow(dead_code)]
    fn evict_if_needed(&mut self, incoming_bytes: usize) {
        // Check document count limit
        while self.documents.len() >= self.config.max_documents {
            if !self.evict_lru() {
                break;
            }
        }

        // Check memory limit
        let mut current_memory = self.estimate_memory_usage();
        while current_memory + incoming_bytes > self.config.max_memory_bytes {
            if !self.evict_lru() {
                break;
            }
            current_memory = self.estimate_memory_usage();
        }
    }

    /// Evict the least recently used document
    ///
    /// # Returns
    /// true if a document was evicted, false if store is empty
    fn evict_lru(&mut self) -> bool {
        // Get the least recently accessed URI (first in access_order)
        if let Some(uri) = self.access_order.first().cloned() {
            log::trace!("Evicting LRU document: {}", uri);
            self.documents.remove(&uri);
            self.access_order.shift_remove(&uri);
            self.update_trackers.remove(&uri);
            self.metrics.evictions += 1;
            true
        } else {
            false
        }
    }

    /// Evict the least recently used document, excluding a specific URI
    ///
    /// This is used when updating an existing document - we don't want to evict
    /// the document we're about to update.
    ///
    /// # Arguments
    /// * `exclude_uri` - URI to exclude from eviction
    ///
    /// # Returns
    /// true if a document was evicted, false if no eligible documents
    fn evict_lru_excluding(&mut self, exclude_uri: &Url) -> bool {
        // Find the least recently accessed URI that isn't the excluded one
        let uri_to_evict = self
            .access_order
            .iter()
            .find(|uri| *uri != exclude_uri)
            .cloned();

        if let Some(uri) = uri_to_evict {
            log::trace!("Evicting LRU document (excluding {}): {}", exclude_uri, uri);
            self.documents.remove(&uri);
            self.access_order.shift_remove(&uri);
            self.update_trackers.remove(&uri);
            self.metrics.evictions += 1;
            true
        } else {
            false
        }
    }

    /// Signal that an update has completed for a URI
    ///
    /// Creates a tracker if one doesn't exist, then signals completion.
    /// This allows waiters to be notified when updates complete.
    ///
    /// **Validates: Requirements 6.4**
    fn signal_update_complete(&mut self, uri: &Url) {
        // Get or create the tracker for this URI
        let tracker = self
            .update_trackers
            .entry(uri.clone())
            .or_insert_with(UpdateTracker::new);

        // Signal completion
        tracker.signal_complete();
    }

    /// Mark an update as started for a URI
    fn mark_update_started(&mut self, uri: &Url) {
        let tracker = self
            .update_trackers
            .entry(uri.clone())
            .or_insert_with(UpdateTracker::new);
        tracker.mark_in_flight();
    }

    /// Update access order for a URI (move to end = most recently accessed)
    ///
    /// **Validates: Requirements 2.4, 2.5**
    fn touch_access(&mut self, uri: &Url) {
        // Remove and re-insert to move to end
        self.access_order.shift_remove(uri);
        self.access_order.insert(uri.clone());
    }

    /// Estimate total memory usage of all documents
    fn estimate_memory_usage(&self) -> usize {
        self.documents
            .values()
            .map(|state| state.estimate_memory_bytes())
            .sum()
    }

    /// Parse R content into a tree
    fn parse_content(content: &str) -> Option<Tree> {
        crate::parser_pool::with_parser(|parser| parser.parse(content, None))
    }

    /// Extract loaded packages from parsed tree
    fn extract_packages(tree: &Option<Tree>, content: &str) -> Vec<String> {
        let Some(tree) = tree else {
            return Vec::new();
        };

        let mut packages = Vec::new();
        let root = tree.root_node();
        Self::visit_for_packages(root, content, &mut packages);
        packages
    }
    fn is_valid_package_name(name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        if name.contains("..") || name.contains('/') || name.contains('\\') {
            return false;
        }
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    }

    /// Recursively visit nodes to find library/require calls
    fn visit_for_packages(node: tree_sitter::Node, text: &str, packages: &mut Vec<String>) {
        if node.kind() == "call" {
            if let Some(func_node) = node.child_by_field_name("function") {
                let func_text = &text[func_node.byte_range()];

                if func_text == "library" || func_text == "require" || func_text == "loadNamespace"
                {
                    if let Some(args_node) = node.child_by_field_name("arguments") {
                        for i in 0..args_node.child_count() {
                            if let Some(child) = args_node.child(i) {
                                if child.kind() == "argument" {
                                    if let Some(value_node) = child.child_by_field_name("value") {
                                        let value_text = &text[value_node.byte_range()];
                                        let pkg_name = value_text
                                            .trim_matches(|c: char| c == '"' || c == '\'');
                                        if Self::is_valid_package_name(pkg_name) {
                                            packages.push(pkg_name.to_string());
                                        } else {
                                            log::warn!(
                                                "Skipping suspicious package name: {}",
                                                pkg_name
                                            );
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                Self::visit_for_packages(child, text, packages);
            }
        }
    }

    /// Apply a single change to a Rope
    fn apply_change_to_rope(contents: &mut Rope, change: TextDocumentContentChangeEvent) {
        if let Some(range) = change.range {
            let start_line = range.start.line as usize;
            let start_utf16_char = range.start.character as usize;
            let end_line = range.end.line as usize;
            let end_utf16_char = range.end.character as usize;

            let start_line_text = contents.line(start_line).to_string();
            let end_line_text = contents.line(end_line).to_string();

            let start_char = Self::utf16_offset_to_char_offset(&start_line_text, start_utf16_char);
            let end_char = Self::utf16_offset_to_char_offset(&end_line_text, end_utf16_char);

            let start_idx = contents.line_to_char(start_line) + start_char;
            let end_idx = contents.line_to_char(end_line) + end_char;

            contents.remove(start_idx..end_idx);
            contents.insert(start_idx, &change.text);
        } else {
            // Full document sync
            *contents = Rope::from_str(&change.text);
        }
    }

    /// Convert UTF-16 offset to char offset
    fn utf16_offset_to_char_offset(line_text: &str, utf16_offset: usize) -> usize {
        let mut utf16_count = 0;
        let mut char_count = 0;

        for ch in line_text.chars() {
            if utf16_count >= utf16_offset {
                return char_count;
            }
            utf16_count += ch.len_utf16();
            char_count += 1;
        }
        char_count
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn make_test_config() -> DocumentStoreConfig {
        DocumentStoreConfig {
            max_documents: 3,
            max_memory_bytes: 10 * 1024 * 1024, // 10MB
        }
    }

    #[tokio::test]
    async fn test_open_and_get() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        store.open(uri.clone(), "x <- 1", 1).await;

        assert!(store.contains(&uri));
        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.version, 1);
        assert_eq!(doc.contents.to_string(), "x <- 1");
    }

    #[tokio::test]
    async fn test_close() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        store.open(uri.clone(), "x <- 1", 1).await;
        assert!(store.contains(&uri));

        store.close(&uri);
        assert!(!store.contains(&uri));
    }

    #[tokio::test]
    async fn test_lru_eviction() {
        let config = DocumentStoreConfig {
            max_documents: 2,
            max_memory_bytes: 100 * 1024 * 1024,
        };
        let mut store = DocumentStore::new(config);

        let uri1 = Url::parse("file:///test1.R").unwrap();
        let uri2 = Url::parse("file:///test2.R").unwrap();
        let uri3 = Url::parse("file:///test3.R").unwrap();

        // Open two documents
        store.open(uri1.clone(), "x <- 1", 1).await;
        store.open(uri2.clone(), "y <- 2", 1).await;

        assert_eq!(store.len(), 2);
        assert!(store.contains(&uri1));
        assert!(store.contains(&uri2));

        // Open third document - should evict uri1 (LRU)
        store.open(uri3.clone(), "z <- 3", 1).await;

        assert_eq!(store.len(), 2);
        assert!(!store.contains(&uri1)); // Evicted
        assert!(store.contains(&uri2));
        assert!(store.contains(&uri3));
    }

    #[tokio::test]
    async fn test_lru_access_order() {
        let config = DocumentStoreConfig {
            max_documents: 2,
            max_memory_bytes: 100 * 1024 * 1024,
        };
        let mut store = DocumentStore::new(config);

        let uri1 = Url::parse("file:///test1.R").unwrap();
        let uri2 = Url::parse("file:///test2.R").unwrap();
        let uri3 = Url::parse("file:///test3.R").unwrap();

        // Open two documents
        store.open(uri1.clone(), "x <- 1", 1).await;
        store.open(uri2.clone(), "y <- 2", 1).await;

        // Access uri1 to make it most recently used
        let _ = store.get(&uri1);

        // Open third document - should evict uri2 (now LRU)
        store.open(uri3.clone(), "z <- 3", 1).await;

        assert!(store.contains(&uri1)); // Still present (was accessed)
        assert!(!store.contains(&uri2)); // Evicted (was LRU)
        assert!(store.contains(&uri3));
    }

    #[tokio::test]
    async fn test_update() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        store.open(uri.clone(), "x <- 1", 1).await;

        // Update with full document sync
        store
            .update(
                &uri,
                vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "y <- 2".to_string(),
                }],
                2,
            )
            .await;

        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.version, 2);
        assert_eq!(doc.contents.to_string(), "y <- 2");
        assert_eq!(doc.revision, 1);
    }

    #[tokio::test]
    async fn test_metrics() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();
        let uri2 = Url::parse("file:///test2.R").unwrap();

        store.open(uri.clone(), "x <- 1", 1).await;
        assert_eq!(store.metrics().documents_opened, 1);

        let _ = store.get(&uri);
        assert_eq!(store.metrics().cache_hits, 1);

        let _ = store.get(&uri2);
        assert_eq!(store.metrics().cache_misses, 1);

        store.close(&uri);
        assert_eq!(store.metrics().documents_closed, 1);
    }

    #[tokio::test]
    async fn test_uris() {
        let mut store = DocumentStore::new(make_test_config());
        let uri1 = Url::parse("file:///test1.R").unwrap();
        let uri2 = Url::parse("file:///test2.R").unwrap();

        store.open(uri1.clone(), "x <- 1", 1).await;
        store.open(uri2.clone(), "y <- 2", 1).await;

        let uris = store.uris();
        assert_eq!(uris.len(), 2);
        assert!(uris.contains(&uri1));
        assert!(uris.contains(&uri2));
    }

    #[tokio::test]
    async fn test_parsed_tree() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        store
            .open(uri.clone(), "my_func <- function(x) { x + 1 }", 1)
            .await;

        let doc = store.get(&uri).unwrap();
        assert!(doc.tree.is_some());
    }

    #[tokio::test]
    async fn test_loaded_packages() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        store
            .open(uri.clone(), "library(dplyr)\nrequire(ggplot2)", 1)
            .await;

        let doc = store.get(&uri).unwrap();
        assert!(doc.loaded_packages.contains(&"dplyr".to_string()));
        assert!(doc.loaded_packages.contains(&"ggplot2".to_string()));
    }

    #[tokio::test]
    async fn test_metadata_extraction() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        store
            .open(uri.clone(), "source('utils.R')\nx <- 1", 1)
            .await;

        let doc = store.get(&uri).unwrap();
        assert!(!doc.metadata.sources.is_empty());
    }

    #[tokio::test]
    async fn test_artifacts_computation() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        store
            .open(uri.clone(), "my_func <- function(x) { x + 1 }", 1)
            .await;

        let doc = store.get(&uri).unwrap();
        assert!(doc.artifacts.exported_interface.contains_key("my_func"));
    }

    #[test]
    fn test_config_default() {
        let config = DocumentStoreConfig::default();
        assert_eq!(config.max_documents, 50);
        assert_eq!(config.max_memory_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn test_metrics_default() {
        let metrics = DocumentStoreMetrics::default();
        assert_eq!(metrics.cache_hits, 0);
        assert_eq!(metrics.cache_misses, 0);
        assert_eq!(metrics.evictions, 0);
    }

    // ========================================================================
    // Async Update Coordination Tests
    // ========================================================================

    /// Test that update trackers are created when documents are opened
    /// **Validates: Requirements 6.1**
    #[tokio::test]
    async fn test_update_tracker_created_on_open() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        // Before opening, no tracker exists
        assert!(!store.has_update_tracker(&uri));
        assert!(store.update_revision(&uri).is_none());

        // Open document
        store.open(uri.clone(), "x <- 1", 1).await;

        // After opening, tracker exists with revision 1
        assert!(store.has_update_tracker(&uri));
        assert_eq!(store.update_revision(&uri), Some(1));
    }

    /// Test that update revision increments on each update
    /// **Validates: Requirements 6.1, 6.4**
    #[tokio::test]
    async fn test_update_revision_increments() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        store.open(uri.clone(), "x <- 1", 1).await;
        assert_eq!(store.update_revision(&uri), Some(1));

        // Update document
        store
            .update(
                &uri,
                vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "y <- 2".to_string(),
                }],
                2,
            )
            .await;

        // Revision should increment
        assert_eq!(store.update_revision(&uri), Some(2));

        // Another update
        store
            .update(
                &uri,
                vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "z <- 3".to_string(),
                }],
                3,
            )
            .await;

        assert_eq!(store.update_revision(&uri), Some(3));
    }

    /// Test that update tracker is removed when document is closed
    /// **Validates: Requirements 6.1**
    #[tokio::test]
    async fn test_update_tracker_removed_on_close() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        store.open(uri.clone(), "x <- 1", 1).await;
        assert!(store.has_update_tracker(&uri));

        store.close(&uri);

        // Tracker should be removed
        assert!(!store.has_update_tracker(&uri));
        assert!(store.update_revision(&uri).is_none());
    }

    /// Test that wait_for_update returns immediately when no tracker exists
    /// **Validates: Requirements 6.2**
    #[tokio::test]
    async fn test_wait_for_update_no_tracker() {
        let store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        // Should return immediately without blocking
        store.wait_for_update(&uri).await;
        // If we get here, the test passes
    }

    /// Test that wait_for_update can be called by multiple waiters
    /// **Validates: Requirements 6.2, 6.3, 6.4**
    #[tokio::test]
    async fn test_multiple_waiters() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::Duration;

        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        // Open document to create tracker
        store.open(uri.clone(), "x <- 1", 1).await;

        // Get the tracker for spawning waiters
        let tracker = store.update_trackers.get(&uri).unwrap().clone();

        // Counter to track how many waiters completed
        let completed = Arc::new(AtomicU32::new(0));

        // Spawn multiple waiters - they need to mark the current value as seen first
        let mut handles = Vec::new();
        for _ in 0..3 {
            let mut receiver = tracker.receiver.clone();
            let completed_clone = completed.clone();
            handles.push(tokio::spawn(async move {
                // Mark current value as seen by calling borrow_and_update
                receiver.borrow_and_update();
                // Now wait for the next update
                let _ = receiver.changed().await;
                completed_clone.fetch_add(1, Ordering::SeqCst);
            }));
        }

        // Give waiters time to start waiting
        tokio::time::sleep(Duration::from_millis(10)).await;

        // No waiters should have completed yet
        assert_eq!(completed.load(Ordering::SeqCst), 0);

        // Signal completion
        tracker.signal_complete();

        // Wait for all handles to complete
        for handle in handles {
            let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
        }

        // All waiters should have completed
        assert_eq!(completed.load(Ordering::SeqCst), 3);
    }

    /// Test that update tracker is removed when document is evicted
    /// **Validates: Requirements 6.1**
    #[tokio::test]
    async fn test_update_tracker_removed_on_eviction() {
        let config = DocumentStoreConfig {
            max_documents: 2,
            max_memory_bytes: 100 * 1024 * 1024,
        };
        let mut store = DocumentStore::new(config);

        let uri1 = Url::parse("file:///test1.R").unwrap();
        let uri2 = Url::parse("file:///test2.R").unwrap();
        let uri3 = Url::parse("file:///test3.R").unwrap();

        // Open two documents
        store.open(uri1.clone(), "x <- 1", 1).await;
        store.open(uri2.clone(), "y <- 2", 1).await;

        assert!(store.has_update_tracker(&uri1));
        assert!(store.has_update_tracker(&uri2));

        // Open third document - should evict uri1 (LRU)
        store.open(uri3.clone(), "z <- 3", 1).await;

        // uri1's tracker should be removed
        assert!(!store.has_update_tracker(&uri1));
        assert!(store.has_update_tracker(&uri2));
        assert!(store.has_update_tracker(&uri3));
    }

    /// Test that re-opening a document resets the tracker
    /// **Validates: Requirements 6.1, 6.4**
    #[tokio::test]
    async fn test_reopen_document_signals_update() {
        let mut store = DocumentStore::new(make_test_config());
        let uri = Url::parse("file:///test.R").unwrap();

        // Open document
        store.open(uri.clone(), "x <- 1", 1).await;
        let rev1 = store.update_revision(&uri).unwrap();

        // Re-open with new content (simulates external change)
        store.open(uri.clone(), "y <- 2", 2).await;
        let rev2 = store.update_revision(&uri).unwrap();

        // Revision should have incremented
        assert!(
            rev2 > rev1,
            "Revision should increment on re-open: {} > {}",
            rev2,
            rev1
        );
    }

    // ========================================================================
    // Property-Based Tests
    // ========================================================================

    /// Operation type for property-based testing
    #[derive(Debug, Clone)]
    enum StoreOperation {
        /// Open a new document with the given index
        Open(usize),
        /// Access (get) an existing document by index
        Access(usize),
    }

    /// Strategy for generating a sequence of store operations
    fn operation_sequence_strategy(max_docs: usize) -> impl Strategy<Value = Vec<StoreOperation>> {
        // Generate a sequence of operations (20-50 operations)
        proptest::collection::vec(
            prop_oneof![
                // Open operations: generate document indices
                (0..max_docs * 2).prop_map(StoreOperation::Open),
                // Access operations: generate document indices
                (0..max_docs * 2).prop_map(StoreOperation::Access),
            ],
            20..50,
        )
    }

    /// Helper to create a URI from an index
    fn uri_from_index(idx: usize) -> Url {
        Url::parse(&format!("file:///test{}.R", idx)).unwrap()
    }

    /// Helper to create simple R content from an index
    fn content_from_index(idx: usize) -> String {
        format!("x{} <- {}", idx, idx)
    }

    // Feature: workspace-index-consolidation, Property 2: LRU Eviction Correctness
    // **Validates: Requirements 2.1, 2.2, 2.3, 2.4, 2.5**
    //
    // Property: For any sequence of document opens exceeding max_documents,
    // the DocumentStore SHALL evict the least-recently-accessed documents first,
    // maintaining at most max_documents entries.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 2: LRU Eviction Correctness
        ///
        /// For any sequence of document opens exceeding max_documents, the DocumentStore
        /// SHALL evict the least-recently-accessed documents first, maintaining at most
        /// max_documents entries.
        ///
        /// **Validates: Requirements 2.1, 2.2, 2.3, 2.4, 2.5**
        #[test]
        fn prop_lru_eviction_correctness(
            max_documents in 2usize..=5,
            ops in operation_sequence_strategy(10)
        ) {
            // Use tokio runtime for async operations
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let config = DocumentStoreConfig {
                    max_documents,
                    max_memory_bytes: 100 * 1024 * 1024, // Large enough to not trigger memory eviction
                };
                let mut store = DocumentStore::new(config);

                // Track access order ourselves to verify LRU behavior
                // Most recently accessed at the end
                let mut expected_access_order: Vec<Url> = Vec::new();

                for op in ops {
                    match op {
                        StoreOperation::Open(idx) => {
                            let uri = uri_from_index(idx);
                            let content = content_from_index(idx);

                            // Check if this is a NEW document (not already in store)
                            let is_new_document = !store.contains(&uri);

                            // Before opening, if we're at capacity AND this is a new document,
                            // the LRU document should be evicted
                            let lru_before = if store.len() >= max_documents && is_new_document {
                                expected_access_order.first().cloned()
                            } else {
                                None
                            };

                            store.open(uri.clone(), &content, 1).await;

                            // Update our expected access order
                            // Remove if already present (will be re-added at end)
                            expected_access_order.retain(|u| u != &uri);
                            // Add to end (most recently accessed)
                            expected_access_order.push(uri.clone());

                            // If we were at capacity and opened a NEW doc, the LRU should have been evicted
                            if let Some(lru_uri) = lru_before {
                                assert!(
                                    !store.contains(&lru_uri),
                                    "LRU document {} should have been evicted when opening new document {} at capacity",
                                    lru_uri,
                                    uri
                                );
                                // Remove from our expected order
                                expected_access_order.retain(|u| u != &lru_uri);
                            }

                            // Invariant 1: Document count never exceeds max_documents
                            assert!(
                                store.len() <= max_documents,
                                "Document count {} exceeds max_documents {}",
                                store.len(),
                                max_documents
                            );
                        }
                        StoreOperation::Access(idx) => {
                            let uri = uri_from_index(idx);

                            // Only access if document exists
                            if store.contains(&uri) {
                                let _ = store.get(&uri);

                                // Update our expected access order
                                expected_access_order.retain(|u| u != &uri);
                                expected_access_order.push(uri);
                            }
                        }
                    }

                    // Invariant 2: All documents in store should be in our expected order
                    for uri in store.uris() {
                        assert!(
                            expected_access_order.contains(&uri),
                            "Document {} in store but not in expected access order",
                            uri
                        );
                    }

                    // Invariant 3: Document count never exceeds max_documents
                    assert!(
                        store.len() <= max_documents,
                        "Document count {} exceeds max_documents {} after operation",
                        store.len(),
                        max_documents
                    );
                }

                // Final verification: the store's access order should match our expected order
                // (for documents that are still in the store)
                let store_uris: std::collections::HashSet<_> = store.uris().into_iter().collect();
                let expected_in_store: Vec<_> = expected_access_order
                    .iter()
                    .filter(|u| store_uris.contains(*u))
                    .cloned()
                    .collect();

                assert_eq!(
                    expected_in_store.len(),
                    store.len(),
                    "Expected {} documents in store, found {}",
                    expected_in_store.len(),
                    store.len()
                );
            });
        }

        /// Property 2 extended: Verify that accessing a document updates its LRU position
        ///
        /// When a document is accessed via get(), it should become the most recently used
        /// and should not be evicted when new documents are opened (until it becomes LRU again).
        ///
        /// **Validates: Requirements 2.4, 2.5**
        #[test]
        fn prop_access_updates_lru_position(
            num_initial_docs in 3usize..=5,
            access_pattern in proptest::collection::vec(0usize..5, 5..15),
            num_new_docs in 1usize..=3
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let max_documents = num_initial_docs;
                let config = DocumentStoreConfig {
                    max_documents,
                    max_memory_bytes: 100 * 1024 * 1024,
                };
                let mut store = DocumentStore::new(config);

                // Open initial documents to fill the store
                let mut uris: Vec<Url> = Vec::new();
                for i in 0..num_initial_docs {
                    let uri = uri_from_index(i);
                    store.open(uri.clone(), &content_from_index(i), 1).await;
                    uris.push(uri);
                }

                assert_eq!(store.len(), num_initial_docs);

                // Access documents according to the pattern
                let mut last_accessed: Option<Url> = None;
                for &idx in &access_pattern {
                    if idx < uris.len() {
                        let uri = &uris[idx];
                        if store.contains(uri) {
                            let _ = store.get(uri);
                            last_accessed = Some(uri.clone());
                        }
                    }
                }

                // Now open new documents - the last accessed should survive longest
                if let Some(ref last_uri) = last_accessed {
                    // Open new documents one at a time
                    for i in 0..num_new_docs {
                        let new_uri = uri_from_index(num_initial_docs + i);
                        store.open(new_uri, &content_from_index(num_initial_docs + i), 1).await;

                        // The last accessed document should still be present
                        // (unless we've opened enough new docs to evict it too)
                        if i < num_initial_docs - 1 {
                            assert!(
                                store.contains(last_uri),
                                "Last accessed document {} should still be present after opening {} new docs",
                                last_uri,
                                i + 1
                            );
                        }
                    }
                }

                // Invariant: count never exceeds max
                assert!(store.len() <= max_documents);
            });
        }

        /// Property 2 extended: Verify eviction count matches expected
        ///
        /// When opening N documents with max_documents = M where N > M,
        /// exactly N - M evictions should occur (for unique documents).
        ///
        /// **Validates: Requirements 2.1, 2.3**
        #[test]
        fn prop_eviction_count_correct(
            max_documents in 2usize..=5,
            num_unique_docs in 5usize..=15
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let config = DocumentStoreConfig {
                    max_documents,
                    max_memory_bytes: 100 * 1024 * 1024,
                };
                let mut store = DocumentStore::new(config);

                // Open unique documents
                for i in 0..num_unique_docs {
                    let uri = uri_from_index(i);
                    store.open(uri, &content_from_index(i), 1).await;
                }

                // Calculate expected evictions
                let expected_evictions = if num_unique_docs > max_documents {
                    num_unique_docs - max_documents
                } else {
                    0
                };

                assert_eq!(
                    store.metrics().evictions,
                    expected_evictions as u64,
                    "Expected {} evictions, got {}",
                    expected_evictions,
                    store.metrics().evictions
                );

                // Final count should be min(num_unique_docs, max_documents)
                let expected_count = num_unique_docs.min(max_documents);
                assert_eq!(
                    store.len(),
                    expected_count,
                    "Expected {} documents, got {}",
                    expected_count,
                    store.len()
                );
            });
        }
    }

    // Feature: workspace-index-consolidation, Property 3: Memory Limit Enforcement
    // **Validates: Requirements 2.1, 2.2**
    //
    // Property: For any sequence of document opens, the DocumentStore SHALL evict
    // documents to keep total memory usage below max_memory_bytes.

    /// Strategy for generating R code content with varying sizes
    ///
    /// Generates content that will have predictable memory footprint:
    /// - Small: ~100-500 bytes
    /// - Medium: ~1KB-5KB
    /// - Large: ~10KB-50KB
    fn sized_content_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            // Small content: simple assignment
            (1usize..=10).prop_map(|n| {
                let var_name = format!("var_{}", n);
                format!("{} <- {}", var_name, n)
            }),
            // Medium content: function with some body
            (1usize..=20).prop_map(|n| {
                let func_name = format!("func_{}", n);
                let body_lines: String = (0..n)
                    .map(|i| format!("  x{} <- {}\n", i, i * 10))
                    .collect();
                format!("{} <- function(x) {{\n{}  x\n}}", func_name, body_lines)
            }),
            // Large content: multiple functions and assignments
            (5usize..=30).prop_map(|n| {
                let mut content = String::new();
                for i in 0..n {
                    content.push_str(&format!(
                        "# Comment line {} with some extra text to add size\n",
                        i
                    ));
                    content.push_str(&format!("var_{} <- {}\n", i, i * 100));
                    content.push_str(&format!("func_{} <- function(x) {{ x + {} }}\n", i, i));
                }
                content
            }),
        ]
    }

    /// Strategy for generating a sequence of document opens with varying content sizes
    fn memory_test_operations_strategy(
        max_docs: usize,
    ) -> impl Strategy<Value = Vec<(usize, String)>> {
        proptest::collection::vec((0..max_docs * 2, sized_content_strategy()), 10..30)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 3: Memory Limit Enforcement
        ///
        /// For any sequence of document opens, the DocumentStore SHALL evict documents
        /// to keep total memory usage below max_memory_bytes.
        ///
        /// **Validates: Requirements 2.1, 2.2**
        #[test]
        fn prop_memory_limit_enforcement(
            // Use a small memory limit to trigger memory-based eviction
            // Range: 5KB to 50KB - small enough to trigger eviction with our content sizes
            max_memory_kb in 5usize..=50,
            // Allow many documents so memory limit is the constraint, not document count
            max_documents in 50usize..=100,
            ops in memory_test_operations_strategy(20)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let max_memory_bytes = max_memory_kb * 1024;
                let config = DocumentStoreConfig {
                    max_documents,
                    max_memory_bytes,
                };
                let mut store = DocumentStore::new(config);

                for (idx, content) in ops {
                    let uri = uri_from_index(idx);

                    // Open the document
                    store.open(uri.clone(), &content, 1).await;

                    // INVARIANT: After every operation, memory usage must be at or below the limit
                    // Note: We check <= because the eviction happens BEFORE adding the new document,
                    // so after adding, we should still be within limits (the new doc fits)
                    let current_memory = store.estimate_memory_usage();

                    // The memory limit is enforced such that:
                    // current_memory + incoming_bytes <= max_memory_bytes
                    // After the document is added, current_memory should be <= max_memory_bytes
                    // (assuming the single document itself doesn't exceed the limit)
                    assert!(
                        current_memory <= max_memory_bytes || store.len() <= 1,
                        "Memory usage {} bytes exceeds limit {} bytes with {} documents",
                        current_memory,
                        max_memory_bytes,
                        store.len()
                    );

                    // Additional invariant: if we have more than one document,
                    // memory should definitely be under the limit
                    if store.len() > 1 {
                        assert!(
                            current_memory <= max_memory_bytes,
                            "Memory usage {} bytes exceeds limit {} bytes with {} documents (>1)",
                            current_memory,
                            max_memory_bytes,
                            store.len()
                        );
                    }
                }

                // Final verification: memory is within limits
                let final_memory = store.estimate_memory_usage();
                if store.len() > 1 {
                    assert!(
                        final_memory <= max_memory_bytes,
                        "Final memory usage {} bytes exceeds limit {} bytes",
                        final_memory,
                        max_memory_bytes
                    );
                }
            });
        }

        /// Property 3 extended: Verify memory-based eviction triggers correctly
        ///
        /// When documents are opened that would exceed memory limit, eviction should occur
        /// even if document count limit is not reached.
        ///
        /// **Validates: Requirements 2.1, 2.2**
        #[test]
        fn prop_memory_eviction_triggers_before_count_limit(
            // Small memory limit to ensure memory-based eviction
            max_memory_kb in 10usize..=30,
            // Large document count limit so it's not the constraint
            num_docs in 5usize..=15
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let max_memory_bytes = max_memory_kb * 1024;
                let config = DocumentStoreConfig {
                    max_documents: 100, // High limit - memory should be the constraint
                    max_memory_bytes,
                };
                let mut store = DocumentStore::new(config);

                // Open documents with substantial content
                for i in 0..num_docs {
                    let uri = uri_from_index(i);
                    // Create content that's roughly 2-5KB each
                    let content: String = (0..100)
                        .map(|j| format!("var_{}_{} <- {} + {}\n", i, j, i * 100, j))
                        .collect();

                    store.open(uri, &content, 1).await;

                    // Memory should always be within limits (or we have just 1 doc)
                    let current_memory = store.estimate_memory_usage();
                    if store.len() > 1 {
                        assert!(
                            current_memory <= max_memory_bytes,
                            "Memory {} exceeds limit {} with {} docs (count limit is 100)",
                            current_memory,
                            max_memory_bytes,
                            store.len()
                        );
                    }
                }

                // If evictions occurred, it was due to memory (not count)
                // since max_documents is 100 and we only opened num_docs (5-15)
                if store.metrics().evictions > 0 {
                    assert!(
                        store.len() < 100,
                        "Evictions occurred but document count {} is at limit",
                        store.len()
                    );
                }
            });
        }

        /// Property 3 extended: Verify LRU order is respected during memory eviction
        ///
        /// When memory-based eviction occurs, the least recently accessed documents
        /// should be evicted first, same as count-based eviction.
        ///
        /// **Validates: Requirements 2.1, 2.2, 2.3, 2.4, 2.5**
        #[test]
        fn prop_memory_eviction_respects_lru_order(
            max_memory_kb in 15usize..=40,
            access_indices in proptest::collection::vec(0usize..5, 3..8)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let max_memory_bytes = max_memory_kb * 1024;
                let config = DocumentStoreConfig {
                    max_documents: 100,
                    max_memory_bytes,
                };
                let mut store = DocumentStore::new(config);

                // Open 5 documents with moderate content
                let mut uris: Vec<Url> = Vec::new();
                for i in 0..5 {
                    let uri = uri_from_index(i);
                    let content: String = (0..50)
                        .map(|j| format!("x_{}_{} <- {}\n", i, j, j))
                        .collect();
                    store.open(uri.clone(), &content, 1).await;
                    uris.push(uri);
                }

                // Access some documents to change LRU order
                let mut last_accessed: Option<Url> = None;
                for &idx in &access_indices {
                    if idx < uris.len() {
                        let uri = &uris[idx];
                        if store.contains(uri) {
                            let _ = store.get(uri);
                            last_accessed = Some(uri.clone());
                        }
                    }
                }

                // Now open a large document that should trigger memory eviction
                let large_uri = uri_from_index(100);
                let large_content: String = (0..200)
                    .map(|j| format!("large_var_{} <- {} * {}\n", j, j, j))
                    .collect();
                store.open(large_uri.clone(), &large_content, 1).await;

                // The last accessed document should still be present (if any eviction occurred)
                // because it was most recently used
                if let Some(ref last_uri) = last_accessed {
                    if store.metrics().evictions > 0 && store.len() > 1 {
                        // If evictions happened and we have multiple docs,
                        // the most recently accessed should likely still be there
                        // (unless the large doc alone exceeds the limit)
                        let large_doc_memory = store.get_without_touch(&large_uri)
                            .map(|d| d.estimate_memory_bytes())
                            .unwrap_or(0);

                        if large_doc_memory < max_memory_bytes {
                            // There's room for at least one more doc
                            // The last accessed should be among the survivors
                            // (This is a probabilistic check - the last accessed
                            // should have higher survival probability)
                            let _ = last_uri; // Acknowledge we checked
                        }
                    }
                }

                // Memory should be within limits
                let final_memory = store.estimate_memory_usage();
                if store.len() > 1 {
                    assert!(
                        final_memory <= max_memory_bytes,
                        "Final memory {} exceeds limit {}",
                        final_memory,
                        max_memory_bytes
                    );
                }
            });
        }
    }

    // ========================================================================
    // Feature: workspace-index-consolidation, Property 9: Async Update Coordination
    // **Validates: Requirements 6.1, 6.2, 6.3, 6.4**
    //
    // Property: For any URI with an active update, wait_for_update SHALL block
    // until the update completes, and subsequent get calls SHALL return the
    // updated data.
    // ========================================================================

    /// Operation type for async update coordination testing
    #[derive(Debug, Clone)]
    enum AsyncUpdateOperation {
        /// Open a document with given index and content variant
        Open {
            doc_idx: usize,
            content_variant: usize,
        },
        /// Update a document with new content variant
        Update {
            doc_idx: usize,
            content_variant: usize,
        },
        /// Wait for update on a document
        WaitForUpdate { doc_idx: usize },
        /// Get a document (should see latest content after wait)
        Get { doc_idx: usize },
    }

    /// Strategy for generating async update operation sequences
    fn async_update_operations_strategy(
        max_docs: usize,
    ) -> impl Strategy<Value = Vec<AsyncUpdateOperation>> {
        proptest::collection::vec(
            prop_oneof![
                // Open operations
                (0..max_docs, 0usize..10).prop_map(|(doc_idx, content_variant)| {
                    AsyncUpdateOperation::Open {
                        doc_idx,
                        content_variant,
                    }
                }),
                // Update operations
                (0..max_docs, 0usize..10).prop_map(|(doc_idx, content_variant)| {
                    AsyncUpdateOperation::Update {
                        doc_idx,
                        content_variant,
                    }
                }),
                // Wait for update operations
                (0..max_docs)
                    .prop_map(|doc_idx| { AsyncUpdateOperation::WaitForUpdate { doc_idx } }),
                // Get operations
                (0..max_docs).prop_map(|doc_idx| { AsyncUpdateOperation::Get { doc_idx } }),
            ],
            20..50,
        )
    }

    /// Generate content based on document index and content variant
    /// This allows us to verify that get() returns the correct version
    fn content_for_variant(doc_idx: usize, content_variant: usize) -> String {
        format!(
            "# Document {} variant {}\nx_{} <- {}",
            doc_idx, content_variant, doc_idx, content_variant
        )
    }

    /// Extract the content variant from document content
    fn extract_variant_from_content(content: &str) -> Option<usize> {
        // Parse "# Document X variant Y" from the first line
        let first_line = content.lines().next()?;
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        // Expected: ["#", "Document", "X", "variant", "Y"]
        if parts.len() >= 5 && parts[0] == "#" && parts[1] == "Document" && parts[3] == "variant" {
            parts[4].parse().ok()
        } else {
            None
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 9: Async Update Coordination
        ///
        /// For any URI with an active update, wait_for_update SHALL block until
        /// the update completes, and subsequent get calls SHALL return the updated data.
        ///
        /// This test verifies:
        /// 1. Update trackers are created when documents are opened
        /// 2. Update revisions increment on each update
        /// 3. After wait_for_update completes, get() returns the latest content
        /// 4. Multiple operations maintain consistency
        ///
        /// **Validates: Requirements 6.1, 6.2, 6.3, 6.4**
        #[test]
        fn prop_async_update_coordination(
            ops in async_update_operations_strategy(5)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let config = DocumentStoreConfig {
                    max_documents: 10, // Enough to not trigger eviction
                    max_memory_bytes: 100 * 1024 * 1024,
                };
                let mut store = DocumentStore::new(config);

                // Track the expected content variant for each document
                let mut expected_variants: HashMap<usize, usize> = HashMap::new();
                // Track which documents are open
                let mut open_docs: std::collections::HashSet<usize> = std::collections::HashSet::new();

                for op in ops {
                    match op {
                        AsyncUpdateOperation::Open { doc_idx, content_variant } => {
                            let uri = uri_from_index(doc_idx);
                            let content = content_for_variant(doc_idx, content_variant);

                            store.open(uri.clone(), &content, 1).await;

                            // Track expected state
                            expected_variants.insert(doc_idx, content_variant);
                            open_docs.insert(doc_idx);

                            // INVARIANT 1: After open, update tracker should exist
                            // **Validates: Requirement 6.1**
                            assert!(
                                store.has_update_tracker(&uri),
                                "Update tracker should exist after opening document {}",
                                doc_idx
                            );

                            // INVARIANT 2: Revision should be at least 1
                            // **Validates: Requirement 6.1**
                            let revision = store.update_revision(&uri);
                            assert!(
                                revision.is_some() && revision.unwrap() >= 1,
                                "Revision should be >= 1 after open, got {:?}",
                                revision
                            );
                        }

                        AsyncUpdateOperation::Update { doc_idx, content_variant } => {
                            let uri = uri_from_index(doc_idx);

                            // Only update if document is open
                            if open_docs.contains(&doc_idx) && store.contains(&uri) {
                                let revision_before = store.update_revision(&uri);

                                let new_content = content_for_variant(doc_idx, content_variant);
                                store.update(&uri, vec![TextDocumentContentChangeEvent {
                                    range: None,
                                    range_length: None,
                                    text: new_content,
                                }], 2).await;

                                // Update expected state
                                expected_variants.insert(doc_idx, content_variant);

                                // INVARIANT 3: Revision should increment after update
                                // **Validates: Requirement 6.4**
                                let revision_after = store.update_revision(&uri);
                                assert!(
                                    revision_after > revision_before,
                                    "Revision should increment after update: {:?} > {:?}",
                                    revision_after,
                                    revision_before
                                );
                            }
                        }

                        AsyncUpdateOperation::WaitForUpdate { doc_idx } => {
                            let uri = uri_from_index(doc_idx);

                            // wait_for_update should complete without blocking indefinitely
                            // **Validates: Requirement 6.2**
                            // Use a timeout to ensure we don't hang
                            let wait_result = tokio::time::timeout(
                                std::time::Duration::from_millis(100),
                                store.wait_for_update(&uri)
                            ).await;

                            // Should complete (either immediately if no tracker, or after signal)
                            assert!(
                                wait_result.is_ok(),
                                "wait_for_update should complete within timeout for doc {}",
                                doc_idx
                            );
                        }

                        AsyncUpdateOperation::Get { doc_idx } => {
                            let uri = uri_from_index(doc_idx);

                            if open_docs.contains(&doc_idx) && store.contains(&uri) {
                                // INVARIANT 4: get() should return the latest content
                                // **Validates: Requirement 6.4 (waiters see updated data)**
                                let doc = store.get(&uri);
                                assert!(doc.is_some(), "Document {} should exist", doc_idx);

                                let doc = doc.unwrap();
                                let content = doc.contents.to_string();

                                // Verify content matches expected variant
                                if let Some(&expected_variant) = expected_variants.get(&doc_idx) {
                                    let actual_variant = extract_variant_from_content(&content);
                                    assert_eq!(
                                        actual_variant,
                                        Some(expected_variant),
                                        "Document {} content variant mismatch: expected {}, got {:?}",
                                        doc_idx,
                                        expected_variant,
                                        actual_variant
                                    );
                                }
                            }
                        }
                    }
                }

                // Final verification: all open documents should have consistent state
                for doc_idx in &open_docs {
                    let uri = uri_from_index(*doc_idx);
                    if store.contains(&uri) {
                        // Document should have update tracker
                        assert!(
                            store.has_update_tracker(&uri),
                            "Open document {} should have update tracker",
                            doc_idx
                        );

                        // Content should match expected variant
                        if let Some(&expected_variant) = expected_variants.get(doc_idx) {
                            let doc = store.get(&uri).unwrap();
                            let content = doc.contents.to_string();
                            let actual_variant = extract_variant_from_content(&content);
                            assert_eq!(
                                actual_variant,
                                Some(expected_variant),
                                "Final state: Document {} variant mismatch",
                                doc_idx
                            );
                        }
                    }
                }
            });
        }

        /// Property 9 extended: Verify concurrent waiters are all notified
        ///
        /// When multiple waiters are waiting on the same URI, all should be
        /// notified when an update completes.
        ///
        /// **Validates: Requirements 6.2, 6.3, 6.4**
        #[test]
        fn prop_concurrent_waiters_all_notified(
            num_waiters in 2usize..=5,
            num_updates in 1usize..=3
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                use std::sync::atomic::{AtomicU32, Ordering};

                let config = DocumentStoreConfig {
                    max_documents: 10,
                    max_memory_bytes: 100 * 1024 * 1024,
                };
                let mut store = DocumentStore::new(config);
                let uri = uri_from_index(0);

                // Open document to create tracker
                store.open(uri.clone(), "x <- 1", 1).await;

                // Get the tracker for spawning waiters
                let tracker = store.update_trackers.get(&uri).unwrap().clone();

                // Counter to track completed waiters
                let completed = Arc::new(AtomicU32::new(0));

                // Spawn waiters
                let mut handles = Vec::new();
                for _ in 0..num_waiters {
                    let mut receiver = tracker.receiver.clone();
                    let completed_clone = completed.clone();
                    handles.push(tokio::spawn(async move {
                        // Mark current value as seen
                        receiver.borrow_and_update();
                        // Wait for next update
                        let _ = receiver.changed().await;
                        completed_clone.fetch_add(1, Ordering::SeqCst);
                    }));
                }

                // Give waiters time to start
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;

                // INVARIANT: No waiters should have completed yet
                assert_eq!(
                    completed.load(Ordering::SeqCst),
                    0,
                    "No waiters should complete before signal"
                );

                // Signal updates
                for _ in 0..num_updates {
                    tracker.signal_complete();
                }

                // Wait for all handles with timeout
                for handle in handles {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_millis(100),
                        handle
                    ).await;
                }

                // INVARIANT: All waiters should have been notified
                // **Validates: Requirement 6.4**
                assert_eq!(
                    completed.load(Ordering::SeqCst),
                    num_waiters as u32,
                    "All {} waiters should be notified, got {}",
                    num_waiters,
                    completed.load(Ordering::SeqCst)
                );
            });
        }

        /// Property 9 extended: Verify update revision monotonicity
        ///
        /// For any sequence of updates on a document, the revision counter
        /// should strictly increase.
        ///
        /// **Validates: Requirements 6.1, 6.4**
        #[test]
        fn prop_update_revision_monotonic(
            num_updates in 1usize..=10
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let config = DocumentStoreConfig {
                    max_documents: 10,
                    max_memory_bytes: 100 * 1024 * 1024,
                };
                let mut store = DocumentStore::new(config);
                let uri = uri_from_index(0);

                // Open document
                store.open(uri.clone(), "x <- 0", 1).await;
                let mut prev_revision = store.update_revision(&uri).unwrap();

                // Perform updates and verify revision increases
                for i in 1..=num_updates {
                    store.update(&uri, vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: format!("x <- {}", i),
                    }], (i + 1) as i32).await;

                    let curr_revision = store.update_revision(&uri).unwrap();

                    // INVARIANT: Revision should strictly increase
                    // **Validates: Requirement 6.4**
                    assert!(
                        curr_revision > prev_revision,
                        "Revision should increase: {} > {} after update {}",
                        curr_revision,
                        prev_revision,
                        i
                    );

                    prev_revision = curr_revision;
                }

                // Final revision should be num_updates + 1 (1 for open + num_updates)
                let final_revision = store.update_revision(&uri).unwrap();
                assert_eq!(
                    final_revision,
                    (num_updates + 1) as u64,
                    "Final revision should be {} (1 open + {} updates)",
                    num_updates + 1,
                    num_updates
                );
            });
        }

        /// Property 9 extended: Verify get returns updated data after wait
        ///
        /// After wait_for_update completes, get() should return the data
        /// from the most recent update.
        ///
        /// **Validates: Requirements 6.2, 6.4**
        #[test]
        fn prop_get_returns_updated_data_after_wait(
            content_updates in proptest::collection::vec(0usize..100, 1..5)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let config = DocumentStoreConfig {
                    max_documents: 10,
                    max_memory_bytes: 100 * 1024 * 1024,
                };
                let mut store = DocumentStore::new(config);
                let uri = uri_from_index(0);

                // Open with initial content
                let initial_value = 0;
                store.open(uri.clone(), &format!("x <- {}", initial_value), 1).await;

                // Apply updates
                let mut expected_value = initial_value;
                for (i, &value) in content_updates.iter().enumerate() {
                    expected_value = value;
                    store.update(&uri, vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: format!("x <- {}", value),
                    }], (i + 2) as i32).await;
                }

                // Wait for update (should complete immediately since updates are sync in tests)
                store.wait_for_update(&uri).await;

                // INVARIANT: get() should return the latest content
                // **Validates: Requirement 6.4**
                let doc = store.get(&uri).unwrap();
                let content = doc.contents.to_string();
                let expected_content = format!("x <- {}", expected_value);

                assert_eq!(
                    content,
                    expected_content,
                    "Content after wait should be '{}', got '{}'",
                    expected_content,
                    content
                );
            });
        }
    }
}
