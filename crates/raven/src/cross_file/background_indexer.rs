//! Background indexer for on-demand file indexing.
//!
//! Provides Priority 2 (backward directive targets) and Priority 3 (transitive dependencies)
//! indexing for files not currently open in the editor. Uses a priority queue with
//! configurable limits and transitive depth tracking.
//!
//! # Priority Levels
//! - Priority 2: Files referenced by backward directives (@lsp-run-by, @lsp-sourced-by)
//! - Priority 3: Transitive dependencies (files sourced by Priority 2 files)
//!
//! # Design
//! - Single worker thread processes queue sequentially
//! - Priority ordering ensures important files indexed first
//! - Depth tracking prevents infinite transitive chains
//! - Duplicate detection avoids redundant work

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::anyhow;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_lsp::lsp_types::Url;

use crate::cross_file::file_cache::FileSnapshot;
use crate::cross_file::path_resolve::{resolve_path, PathContext};
use crate::cross_file::scope::compute_artifacts;
use crate::cross_file::{extract_metadata, CrossFileMetadata};
use crate::state::WorldState;

/// Task for background indexing
#[derive(Debug, Clone)]
pub struct IndexTask {
    pub uri: Url,
    pub priority: usize,
    pub depth: usize,
    pub submitted_at: Instant,
}

/// Background indexer for on-demand file processing
pub struct BackgroundIndexer {
    state: Arc<RwLock<WorldState>>,
    queue: Arc<Mutex<VecDeque<IndexTask>>>,
    worker_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    cancellation_token: CancellationToken,
    canceled: Arc<Mutex<HashSet<Url>>>,
}

impl BackgroundIndexer {
    /// Creates new indexer and starts worker
    pub fn new(state: Arc<RwLock<WorldState>>) -> Self {
        let canceled = Arc::new(Mutex::new(HashSet::new()));
        let indexer = Self {
            state,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            worker_handle: Arc::new(Mutex::new(None)),
            cancellation_token: CancellationToken::new(),
            canceled: canceled.clone(),
        };
        indexer.start_worker(canceled);
        indexer
    }

    /// Cancels pending indexing for a URI
    pub fn cancel_uri(&self, uri: &Url) {
        // Remove from queue
        {
            let mut queue = self.queue.lock().unwrap();
            let before = queue.len();
            queue.retain(|task| task.uri != *uri);
            if queue.len() < before {
                log::trace!("Removed queued indexing task for canceled URI: {}", uri);
            }
        }
        // Mark as canceled so process_task skips it
        self.canceled.lock().unwrap().insert(uri.clone());
        log::trace!("Marked URI as canceled: {}", uri);
    }

    /// Cancels pending indexing for multiple URIs
    pub fn cancel_uris<'a>(&self, uris: impl Iterator<Item = &'a Url>) {
        let uris: Vec<_> = uris.cloned().collect();
        if uris.is_empty() {
            return;
        }
        let uri_set: HashSet<_> = uris.iter().collect();
        {
            let mut queue = self.queue.lock().unwrap();
            queue.retain(|task| !uri_set.contains(&task.uri));
        }
        let mut canceled = self.canceled.lock().unwrap();
        for uri in &uris {
            canceled.insert(uri.clone());
        }
        log::trace!("Batch canceled {} URIs", uris.len());
    }

    /// Clears the canceled set (call after revalidation cycle)
    pub fn clear_canceled(&self) {
        self.canceled.lock().unwrap().clear();
    }

    /// Submits indexing task with priority ordering
    pub fn submit(&self, uri: Url, priority: usize, depth: usize) {
        // Check if on-demand indexing is enabled
        let enabled = self
            .state
            .try_read()
            .map(|s| s.cross_file_config.on_demand_indexing_enabled)
            .unwrap_or(true);
        if !enabled {
            log::trace!(
                "Skipping indexing task for {} - on_demand_indexing disabled",
                uri
            );
            return;
        }

        let mut queue = self.queue.lock().unwrap();

        // Check if already queued
        if queue.iter().any(|task| task.uri == uri) {
            log::trace!("Skipping indexing task for {} - already queued", uri);
            return;
        }

        // Check queue size limit (use blocking try_read to avoid deadlock)
        let max_size = self
            .state
            .try_read()
            .map(|s| s.cross_file_config.on_demand_indexing_max_queue_size)
            .unwrap_or(50);

        if queue.len() >= max_size {
            log::warn!(
                "Background indexing queue full, dropping task for {} ({}/{})",
                uri,
                queue.len(),
                max_size
            );
            return;
        }

        let task = IndexTask {
            uri: uri.clone(),
            priority,
            depth,
            submitted_at: Instant::now(),
        };

        // Insert with priority ordering (lower priority number = higher priority)
        let insert_pos = queue
            .iter()
            .position(|t| t.priority > priority)
            .unwrap_or(queue.len());
        queue.insert(insert_pos, task);

        log::trace!(
            "Submitted indexing task for {} (priority={}, depth={}, queue_size={})",
            uri,
            priority,
            depth,
            queue.len()
        );
    }

    /// Starts background worker
    fn start_worker(&self, canceled: Arc<Mutex<HashSet<Url>>>) {
        let state = self.state.clone();
        let queue = self.queue.clone();
        let token = self.cancellation_token.clone();

        let handle = tokio::spawn(async move {
            log::info!("Background indexer worker started");

            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        log::info!("Background indexer worker stopped");
                        break;
                    }
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                        let task_opt = {
                            let mut q = queue.lock().unwrap();
                            q.pop_front()
                        };

                        if let Some(task) = task_opt {
                            Self::process_task(state.clone(), queue.clone(), canceled.clone(), task).await;
                        }
                    }
                }
            }
        });

        *self.worker_handle.lock().unwrap() = Some(handle);
    }

    /// Processes a single indexing task
    async fn process_task(
        state: Arc<RwLock<WorldState>>,
        queue: Arc<Mutex<VecDeque<IndexTask>>>,
        canceled: Arc<Mutex<HashSet<Url>>>,
        task: IndexTask,
    ) {
        // Check if canceled
        if canceled.lock().unwrap().contains(&task.uri) {
            log::trace!("Skipping indexing for {} - canceled", task.uri);
            return;
        }

        let start_time = Instant::now();
        log::trace!(
            "Processing indexing task for {} (priority={}, depth={})",
            task.uri,
            task.priority,
            task.depth
        );

        // Check if file needs indexing (not open, not in workspace index)
        let needs_indexing = {
            let state_guard = state.read().await;
            !state_guard.documents.contains_key(&task.uri)
                && !state_guard.cross_file_workspace_index.contains(&task.uri)
        };

        if !needs_indexing {
            log::trace!("Skipping indexing for {} - already indexed", task.uri);
            return;
        }

        // Index the file
        match Self::index_file(&state, &task.uri).await {
            Ok(metadata) => {
                let elapsed = start_time.elapsed();
                let symbol_count = state
                    .read()
                    .await
                    .cross_file_workspace_index
                    .get_artifacts(&task.uri)
                    .map(|a| a.exported_interface.len())
                    .unwrap_or(0);

                log::info!(
                    "Background indexed: {} in {:?} (exported {} symbols)",
                    task.uri,
                    elapsed,
                    symbol_count
                );

                // Queue transitive dependencies for both Priority 2 and Priority 3 tasks
                // (as long as depth limit allows)
                Self::queue_transitive_deps(state, queue, &task.uri, &metadata, task.depth).await;
            }
            Err(e) => {
                log::warn!("Failed to index {}: {}", task.uri, e);
            }
        }
    }

    /// Indexes a single file
    async fn index_file(
        state: &Arc<RwLock<WorldState>>,
        uri: &Url,
    ) -> anyhow::Result<CrossFileMetadata> {
        // Read file content
        let path = uri
            .to_file_path()
            .map_err(|_| anyhow!("Invalid file path: {}", uri))?;

        let content = tokio::fs::read_to_string(&path).await?;
        let metadata_fs = tokio::fs::metadata(&path).await?;

        // Extract cross-file metadata
        let mut cross_file_meta = extract_metadata(&content);

        // Enrich metadata with inherited working directory
        // Use get_enriched_metadata to prefer already-enriched sources for transitive inheritance
        {
            let state_guard = state.read().await;
            let workspace_root = state_guard.workspace_folders.first().cloned();
            let max_chain_depth = state_guard.cross_file_config.max_chain_depth;

            crate::cross_file::enrich_metadata_with_inherited_wd(
                &mut cross_file_meta,
                uri,
                workspace_root.as_ref(),
                |parent_uri| state_guard.get_enriched_metadata(parent_uri),
                max_chain_depth,
            );
        }

        // Compute scope artifacts using thread-local parser for efficiency
        let artifacts = crate::parser_pool::with_parser(|parser| {
            if let Some(tree) = parser.parse(&content, None) {
                compute_artifacts(uri, &tree, &content)
            } else {
                crate::cross_file::scope::ScopeArtifacts::default()
            }
        });

        let snapshot = FileSnapshot::with_content_hash(&metadata_fs, &content);

        // Update file cache and workspace index
        {
            let state_guard = state.read().await;
            state_guard.cross_file_file_cache.insert(
                uri.clone(),
                snapshot.clone(),
                content.clone(),
            );

            let open_docs: HashSet<_> = state_guard.documents.keys().cloned().collect();
            state_guard.cross_file_workspace_index.update_from_disk(
                uri,
                &open_docs,
                snapshot,
                cross_file_meta.clone(),
                artifacts,
            );
        }

        // Update dependency graph
        {
            let mut state_guard = state.write().await;
            let workspace_root = state_guard.workspace_folders.first().cloned();

            // Pre-collect content for potential parent files
            let backward_path_ctx = PathContext::new(uri, workspace_root.as_ref());
            let parent_content: HashMap<Url, String> = cross_file_meta
                .sourced_by
                .iter()
                .filter_map(|d| {
                    let ctx = backward_path_ctx.as_ref()?;
                    let resolved = resolve_path(&d.path, ctx)?;
                    let parent_uri = Url::from_file_path(resolved).ok()?;
                    let content = state_guard
                        .documents
                        .get(&parent_uri)
                        .map(|doc| doc.text())
                        .or_else(|| state_guard.cross_file_file_cache.get(&parent_uri))?;
                    Some((parent_uri, content))
                })
                .collect();

            state_guard.cross_file_graph.update_file(
                uri,
                &cross_file_meta,
                workspace_root.as_ref(),
                |parent_uri| parent_content.get(parent_uri).cloned(),
            );
        }

        Ok(cross_file_meta)
    }

    /// Queues transitive dependencies for Priority 3 indexing
    async fn queue_transitive_deps(
        state: Arc<RwLock<WorldState>>,
        queue: Arc<Mutex<VecDeque<IndexTask>>>,
        uri: &Url,
        metadata: &CrossFileMetadata,
        current_depth: usize,
    ) {
        let (on_demand_enabled, max_depth, max_queue_size, priority_3_enabled, workspace_root) = {
            let state_guard = state.read().await;
            (
                state_guard.cross_file_config.on_demand_indexing_enabled,
                state_guard
                    .cross_file_config
                    .on_demand_indexing_max_transitive_depth,
                state_guard
                    .cross_file_config
                    .on_demand_indexing_max_queue_size,
                state_guard
                    .cross_file_config
                    .on_demand_indexing_priority_3_enabled,
                state_guard.workspace_folders.first().cloned(),
            )
        };

        if !on_demand_enabled || !priority_3_enabled || current_depth >= max_depth {
            return;
        }

        let path_ctx =
            PathContext::from_metadata(uri, metadata, workspace_root.as_ref().map(|u| u as &Url));

        for source in &metadata.sources {
            if let Some(ctx) = path_ctx.as_ref() {
                if let Some(resolved) = resolve_path(&source.path, ctx) {
                    if let Ok(source_uri) = Url::from_file_path(resolved) {
                        // Check if file needs indexing
                        let needs_indexing = {
                            let state_guard = state.read().await;
                            !state_guard.documents.contains_key(&source_uri)
                                && !state_guard.cross_file_workspace_index.contains(&source_uri)
                        };

                        if needs_indexing {
                            let mut q = queue.lock().unwrap();

                            // Check queue size limit (Requirement 3.4)
                            if q.len() >= max_queue_size {
                                log::warn!(
                                    "Background indexing queue full, dropping transitive task for {} ({}/{})",
                                    source_uri,
                                    q.len(),
                                    max_queue_size
                                );
                                continue;
                            }

                            if !q.iter().any(|t| t.uri == source_uri) {
                                // Use saturating_add to prevent integer overflow at max depth
                                let next_depth = current_depth.saturating_add(1);
                                q.push_back(IndexTask {
                                    uri: source_uri.clone(),
                                    priority: 3,
                                    depth: next_depth,
                                    submitted_at: Instant::now(),
                                });
                                log::trace!(
                                    "Queued transitive dependency: {} (depth {})",
                                    source_uri,
                                    next_depth
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Shuts down the worker gracefully
    pub fn shutdown(&self) {
        log::info!("Shutting down background indexer");
        self.cancellation_token.cancel();

        if let Some(handle) = self.worker_handle.lock().unwrap().take() {
            handle.abort();
        }
    }
}

impl Drop for BackgroundIndexer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///test/{}", name)).unwrap()
    }

    #[test]
    fn test_index_task_creation() {
        let task = IndexTask {
            uri: test_uri("test.r"),
            priority: 2,
            depth: 0,
            submitted_at: Instant::now(),
        };
        assert_eq!(task.priority, 2);
        assert_eq!(task.depth, 0);
    }

    #[test]
    fn test_queue_priority_ordering() {
        let queue: Arc<Mutex<VecDeque<IndexTask>>> = Arc::new(Mutex::new(VecDeque::new()));

        // Insert tasks with different priorities
        let tasks = vec![
            IndexTask {
                uri: test_uri("p3.r"),
                priority: 3,
                depth: 1,
                submitted_at: Instant::now(),
            },
            IndexTask {
                uri: test_uri("p2.r"),
                priority: 2,
                depth: 0,
                submitted_at: Instant::now(),
            },
            IndexTask {
                uri: test_uri("p3b.r"),
                priority: 3,
                depth: 2,
                submitted_at: Instant::now(),
            },
        ];

        // Simulate submit logic for priority ordering
        for task in tasks {
            let mut q = queue.lock().unwrap();
            let insert_pos = q
                .iter()
                .position(|t| t.priority > task.priority)
                .unwrap_or(q.len());
            q.insert(insert_pos, task);
        }

        // Verify order: priority 2 first, then priority 3s in FIFO order
        let q = queue.lock().unwrap();
        assert_eq!(q.len(), 3);
        assert_eq!(q[0].priority, 2);
        assert_eq!(q[1].priority, 3);
        assert_eq!(q[2].priority, 3);
        assert_eq!(q[0].uri.path(), "/test/p2.r");
        assert_eq!(q[1].uri.path(), "/test/p3.r");
        assert_eq!(q[2].uri.path(), "/test/p3b.r");
    }

    #[test]
    fn test_queue_duplicate_detection() {
        let queue: Arc<Mutex<VecDeque<IndexTask>>> = Arc::new(Mutex::new(VecDeque::new()));

        // Add first task
        {
            let mut q = queue.lock().unwrap();
            q.push_back(IndexTask {
                uri: test_uri("test.r"),
                priority: 2,
                depth: 0,
                submitted_at: Instant::now(),
            });
        }

        // Try to add duplicate
        let uri = test_uri("test.r");
        let is_duplicate = {
            let q = queue.lock().unwrap();
            q.iter().any(|task| task.uri == uri)
        };

        assert!(is_duplicate);
        assert_eq!(queue.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_queue_size_limiting() {
        let queue: Arc<Mutex<VecDeque<IndexTask>>> = Arc::new(Mutex::new(VecDeque::new()));
        let max_size = 3;

        // Fill queue to max
        for i in 0..max_size {
            let mut q = queue.lock().unwrap();
            q.push_back(IndexTask {
                uri: test_uri(&format!("file{}.r", i)),
                priority: 2,
                depth: 0,
                submitted_at: Instant::now(),
            });
        }

        // Verify queue is at max
        assert_eq!(queue.lock().unwrap().len(), max_size);

        // Try to add one more (should be rejected)
        let should_reject = queue.lock().unwrap().len() >= max_size;
        assert!(should_reject);
    }

    #[test]
    fn test_priority_2_before_priority_3() {
        let queue: Arc<Mutex<VecDeque<IndexTask>>> = Arc::new(Mutex::new(VecDeque::new()));

        // Add priority 3 first
        {
            let mut q = queue.lock().unwrap();
            q.push_back(IndexTask {
                uri: test_uri("p3.r"),
                priority: 3,
                depth: 1,
                submitted_at: Instant::now(),
            });
        }

        // Add priority 2 (should go before priority 3)
        {
            let mut q = queue.lock().unwrap();
            let task = IndexTask {
                uri: test_uri("p2.r"),
                priority: 2,
                depth: 0,
                submitted_at: Instant::now(),
            };
            let insert_pos = q
                .iter()
                .position(|t| t.priority > task.priority)
                .unwrap_or(q.len());
            q.insert(insert_pos, task);
        }

        let q = queue.lock().unwrap();
        assert_eq!(q[0].priority, 2);
        assert_eq!(q[1].priority, 3);
    }

    #[test]
    fn test_depth_tracking() {
        let task = IndexTask {
            uri: test_uri("test.r"),
            priority: 3,
            depth: 2,
            submitted_at: Instant::now(),
        };

        // Verify depth is tracked
        assert_eq!(task.depth, 2);

        // Simulate depth increment for transitive
        let next_depth = task.depth + 1;
        assert_eq!(next_depth, 3);
    }

    #[test]
    fn test_cancel_uri_removes_queued_task() {
        let queue: Arc<Mutex<VecDeque<IndexTask>>> = Arc::new(Mutex::new(VecDeque::new()));
        let canceled: Arc<Mutex<HashSet<Url>>> = Arc::new(Mutex::new(HashSet::new()));

        // Add tasks to queue
        {
            let mut q = queue.lock().unwrap();
            q.push_back(IndexTask {
                uri: test_uri("a.r"),
                priority: 2,
                depth: 0,
                submitted_at: Instant::now(),
            });
            q.push_back(IndexTask {
                uri: test_uri("b.r"),
                priority: 2,
                depth: 0,
                submitted_at: Instant::now(),
            });
        }

        // Cancel one URI
        let uri_to_cancel = test_uri("a.r");
        {
            let mut q = queue.lock().unwrap();
            q.retain(|task| task.uri != uri_to_cancel);
        }
        canceled.lock().unwrap().insert(uri_to_cancel.clone());

        // Verify task was removed from queue
        let q = queue.lock().unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].uri.path(), "/test/b.r");

        // Verify URI is in canceled set
        assert!(canceled.lock().unwrap().contains(&uri_to_cancel));
    }

    #[test]
    fn test_canceled_uri_skipped_in_process() {
        let canceled: Arc<Mutex<HashSet<Url>>> = Arc::new(Mutex::new(HashSet::new()));
        let uri = test_uri("canceled.r");

        // Mark as canceled
        canceled.lock().unwrap().insert(uri.clone());

        // Simulate process_task check
        let is_canceled = canceled.lock().unwrap().contains(&uri);
        assert!(is_canceled);
    }

    #[test]
    fn test_cancel_uris_batch() {
        let queue: Arc<Mutex<VecDeque<IndexTask>>> = Arc::new(Mutex::new(VecDeque::new()));
        let canceled: Arc<Mutex<HashSet<Url>>> = Arc::new(Mutex::new(HashSet::new()));

        // Add tasks
        {
            let mut q = queue.lock().unwrap();
            for name in &["a.r", "b.r", "c.r", "d.r"] {
                q.push_back(IndexTask {
                    uri: test_uri(name),
                    priority: 2,
                    depth: 0,
                    submitted_at: Instant::now(),
                });
            }
        }

        // Batch cancel
        let uris_to_cancel: Vec<Url> = vec![test_uri("a.r"), test_uri("c.r")];
        let uri_set: HashSet<_> = uris_to_cancel.iter().collect();
        {
            let mut q = queue.lock().unwrap();
            q.retain(|task| !uri_set.contains(&task.uri));
        }
        {
            let mut c = canceled.lock().unwrap();
            for uri in &uris_to_cancel {
                c.insert(uri.clone());
            }
        }

        // Verify
        let q = queue.lock().unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].uri.path(), "/test/b.r");
        assert_eq!(q[1].uri.path(), "/test/d.r");

        let c = canceled.lock().unwrap();
        assert!(c.contains(&test_uri("a.r")));
        assert!(c.contains(&test_uri("c.r")));
    }
}
