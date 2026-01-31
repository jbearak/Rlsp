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
}

impl BackgroundIndexer {
    /// Creates new indexer and starts worker
    pub fn new(state: Arc<RwLock<WorldState>>) -> Self {
        let indexer = Self {
            state,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            worker_handle: Arc::new(Mutex::new(None)),
            cancellation_token: CancellationToken::new(),
        };
        indexer.start_worker();
        indexer
    }

    /// Submits indexing task with priority ordering
    pub fn submit(&self, uri: Url, priority: usize, depth: usize) {
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
    fn start_worker(&self) {
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
                            Self::process_task(state.clone(), queue.clone(), task).await;
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
        task: IndexTask,
    ) {
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

                // Queue transitive dependencies for Priority 2 tasks
                if task.priority == 2 {
                    Self::queue_transitive_deps(state, queue, &task.uri, &metadata, task.depth)
                        .await;
                }
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
        let cross_file_meta = extract_metadata(&content);

        // Compute scope artifacts
        let artifacts = {
            let mut parser = tree_sitter::Parser::new();
            if parser
                .set_language(&tree_sitter_r::LANGUAGE.into())
                .is_ok()
            {
                if let Some(tree) = parser.parse(&content, None) {
                    compute_artifacts(uri, &tree, &content)
                } else {
                    crate::cross_file::scope::ScopeArtifacts::default()
                }
            } else {
                crate::cross_file::scope::ScopeArtifacts::default()
            }
        };

        let snapshot = FileSnapshot::with_content_hash(&metadata_fs, &content);

        // Update file cache and workspace index
        {
            let state_guard = state.read().await;
            state_guard
                .cross_file_file_cache
                .insert(uri.clone(), snapshot.clone(), content.clone());

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
        let (max_depth, priority_3_enabled, workspace_root) = {
            let state_guard = state.read().await;
            (
                state_guard
                    .cross_file_config
                    .on_demand_indexing_max_transitive_depth,
                state_guard
                    .cross_file_config
                    .on_demand_indexing_priority_3_enabled,
                state_guard.workspace_folders.first().cloned(),
            )
        };

        if !priority_3_enabled || current_depth >= max_depth {
            return;
        }

        let path_ctx = PathContext::from_metadata(uri, metadata, workspace_root.as_ref().map(|u| u as &Url));

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
                            if !q.iter().any(|t| t.uri == source_uri) {
                                q.push_back(IndexTask {
                                    uri: source_uri.clone(),
                                    priority: 3,
                                    depth: current_depth + 1,
                                    submitted_at: Instant::now(),
                                });
                                log::trace!(
                                    "Queued transitive dependency: {} (depth {})",
                                    source_uri,
                                    current_depth + 1
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
