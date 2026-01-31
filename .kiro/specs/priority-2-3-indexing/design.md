# Design: Priority 2 and 3 On-Demand Indexing

## Overview

This design extends the on-demand indexing system to handle Priority 2 (backward directive targets) and Priority 3 (transitive dependencies) files through a background indexing queue, without blocking diagnostics or requiring Backend to implement Clone.

## Architecture

### Component Structure

```text
Backend
  ├── state: Arc<RwLock<WorldState>>
  ├── client: Client
  └── background_indexer: Arc<BackgroundIndexer>

BackgroundIndexer
  ├── state: Arc<RwLock<WorldState>>
  ├── queue: Arc<Mutex<VecDeque<IndexTask>>>
  ├── worker_handle: Option<JoinHandle<()>>
  └── cancellation_token: CancellationToken

IndexTask
  ├── uri: Url
  ├── priority: usize (2 or 3)
  ├── depth: usize (for transitive tracking)
  └── submitted_at: Instant
```

### Key Design Decisions

1. **Separate BackgroundIndexer struct**: Owns the indexing queue and worker task, can be shared via Arc
2. **Single worker thread**: Processes queue sequentially to avoid resource contention
3. **Priority queue**: Ensures Priority 2 files are indexed before Priority 3
4. **Depth tracking**: Prevents infinite transitive indexing loops
5. **Cancellation support**: Worker can be stopped when Backend is dropped

## Detailed Design

### 1. BackgroundIndexer Implementation

```rust
pub struct BackgroundIndexer {
    state: Arc<RwLock<WorldState>>,
    queue: Arc<Mutex<VecDeque<IndexTask>>>,
    worker_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    cancellation_token: CancellationToken,
}

struct IndexTask {
    uri: Url,
    priority: usize,
    depth: usize,
    submitted_at: Instant,
}

impl BackgroundIndexer {
    pub fn new(state: Arc<RwLock<WorldState>>) -> Self {
        let indexer = Self {
            state: state.clone(),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            worker_handle: Arc::new(Mutex::new(None)),
            cancellation_token: CancellationToken::new(),
        };
        
        // Start worker thread
        indexer.start_worker();
        indexer
    }
    
    pub fn submit(&self, uri: Url, priority: usize, depth: usize) {
        let mut queue = self.queue.lock().unwrap();
        
        // Check if already queued
        if queue.iter().any(|task| task.uri == uri) {
            return;
        }
        
        // Check queue size limit
        let max_size = {
            let state = self.state.read().unwrap();
            state.cross_file_config.on_demand_indexing_max_queue_size
        };
        
        if queue.len() >= max_size {
            log::warn!("Background indexing queue full, dropping task for {}", uri);
            return;
        }
        
        // Insert with priority ordering (lower priority number = higher priority)
        let task = IndexTask {
            uri,
            priority,
            depth,
            submitted_at: Instant::now(),
        };
        
        let insert_pos = queue.iter()
            .position(|t| t.priority > priority)
            .unwrap_or(queue.len());
        
        queue.insert(insert_pos, task);
        log::trace!("Queued background indexing: {} (priority {}, depth {})", 
            task.uri, priority, depth);
    }
    
    fn start_worker(&self) {
        let state = self.state.clone();
        let queue = self.queue.clone();
        let token = self.cancellation_token.clone();
        
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        log::info!("Background indexer worker stopped");
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        // Check for work
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
    
    async fn process_task(
        state: Arc<RwLock<WorldState>>,
        queue: Arc<Mutex<VecDeque<IndexTask>>>,
        task: IndexTask,
    ) {
        let start = Instant::now();
        log::trace!("Processing background indexing task: {} (priority {})", 
            task.uri, task.priority);
        
        // Check if file needs indexing
        let needs_indexing = {
            let state = state.read().unwrap();
            !state.documents.contains_key(&task.uri) 
                && !state.cross_file_workspace_index.contains(&task.uri)
        };
        
        if !needs_indexing {
            log::trace!("Skipping already indexed file: {}", task.uri);
            return;
        }
        
        // Perform indexing (same logic as index_file_on_demand)
        match Self::index_file(&state, &task.uri).await {
            Ok(metadata) => {
                let elapsed = start.elapsed();
                log::info!("Background indexed: {} in {:?} (exported {} symbols)", 
                    task.uri, elapsed,
                    state.read().unwrap().cross_file_workspace_index
                        .get_artifacts(&task.uri)
                        .map(|a| a.exported_interface.len())
                        .unwrap_or(0)
                );
                
                // Queue transitive dependencies if Priority 2 or 3 and depth allows
                if task.priority == 2 || task.priority == 3 {
                    Self::queue_transitive_deps(
                        state.clone(),
                        queue,
                        &task.uri,
                        &metadata,
                        task.depth,
                    ).await;
                }
            }
            Err(e) => {
                log::warn!("Failed to index {}: {}", task.uri, e);
            }
        }
    }
    
    async fn index_file(
        state: &Arc<RwLock<WorldState>>,
        uri: &Url,
    ) -> anyhow::Result<CrossFileMetadata> {
        // Read file
        let path = uri.to_file_path()
            .map_err(|_| anyhow!("Invalid file path"))?;
        let content = tokio::fs::read_to_string(&path).await?;
        let metadata_fs = tokio::fs::metadata(&path).await?;
        
        // Extract metadata
        let cross_file_meta = crate::cross_file::extract_metadata(&content);
        
        // Compute artifacts
        let artifacts = {
            let mut parser = tree_sitter::Parser::new();
            if parser.set_language(&tree_sitter_r::LANGUAGE.into()).is_ok() {
                if let Some(tree) = parser.parse(&content, None) {
                    crate::cross_file::scope::compute_artifacts(uri, &tree, &content)
                } else {
                    crate::cross_file::scope::ScopeArtifacts::default()
                }
            } else {
                crate::cross_file::scope::ScopeArtifacts::default()
            }
        };
        
        let snapshot = crate::cross_file::file_cache::FileSnapshot::with_content_hash(
            &metadata_fs,
            &content,
        );
        
        // Update caches and index
        {
            let state = state.read().unwrap();
            state.cross_file_file_cache.insert(
                uri.clone(),
                snapshot.clone(),
                content.clone(),
            );
            
            let open_docs: std::collections::HashSet<_> = 
                state.documents.keys().cloned().collect();
            state.cross_file_workspace_index.update_from_disk(
                uri,
                &open_docs,
                snapshot,
                cross_file_meta.clone(),
                artifacts,
            );
        }
        
        // Update dependency graph
        {
            let mut state = state.write().unwrap();
            let workspace_root = state.workspace_folders.first().cloned();
            
            let backward_path_ctx = crate::cross_file::path_resolve::PathContext::new(
                uri, workspace_root.as_ref()
            );
            
            let parent_content: std::collections::HashMap<Url, String> = 
                cross_file_meta.sourced_by.iter()
                    .filter_map(|d| {
                        let ctx = backward_path_ctx.as_ref()?;
                        let resolved = crate::cross_file::path_resolve::resolve_path(&d.path, ctx)?;
                        let parent_uri = Url::from_file_path(resolved).ok()?;
                        let content = state.documents.get(&parent_uri)
                            .map(|doc| doc.text())
                            .or_else(|| state.cross_file_file_cache.get(&parent_uri))?;
                        Some((parent_uri, content))
                    })
                    .collect();
            
            state.cross_file_graph.update_file(
                uri,
                &cross_file_meta,
                workspace_root.as_ref(),
                |parent_uri| parent_content.get(parent_uri).cloned(),
            );
        }
        
        Ok(cross_file_meta)
    }
    
    async fn queue_transitive_deps(
        state: Arc<RwLock<WorldState>>,
        queue: Arc<Mutex<VecDeque<IndexTask>>>,
        uri: &Url,
        metadata: &CrossFileMetadata,
        current_depth: usize,
    ) {
        let (max_depth, workspace_root, priority_3_enabled) = {
            let state = state.read().unwrap();
            (
                state.cross_file_config.on_demand_indexing_max_transitive_depth,
                state.workspace_folders.first().cloned(),
                state.cross_file_config.on_demand_indexing_priority_3_enabled,
            )
        };
        
        if !priority_3_enabled || current_depth >= max_depth {
            return;
        }
        
        let path_ctx = crate::cross_file::path_resolve::PathContext::from_metadata(
            uri, metadata, workspace_root.as_ref()
        );
        
        for source in &metadata.sources {
            if let Some(ctx) = path_ctx.as_ref() {
                if let Some(resolved) = crate::cross_file::path_resolve::resolve_path(&source.path, ctx) {
                    if let Ok(source_uri) = Url::from_file_path(resolved) {
                        let needs_indexing = {
                            let state = state.read().unwrap();
                            !state.documents.contains_key(&source_uri) 
                                && !state.cross_file_workspace_index.contains(&source_uri)
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
                                log::trace!("Queued transitive dependency: {} (depth {})", 
                                    source_uri, current_depth + 1);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl BackgroundIndexer {
    /// Gracefully shutdown the background worker.
    /// Callers should call this method before dropping to ensure clean termination.
    pub async fn shutdown(&self) {
        self.cancellation_token.cancel();
        if let Some(handle) = self.worker_handle.lock().unwrap().take() {
            let _ = handle.await;
        }
    }
}

impl Drop for BackgroundIndexer {
    fn drop(&mut self) {
        // Cancel the worker as a last resort; callers should call shutdown() for clean termination.
        // Note: We cannot await the handle in Drop (sync context), so this only signals cancellation.
        if !self.cancellation_token.is_cancelled() {
            log::warn!("BackgroundIndexer dropped without calling shutdown(); worker may terminate abruptly");
            self.cancellation_token.cancel();
        }
    }
}
```

### 2. Backend Integration

```rust
impl Backend {
    pub fn new(client: Client) -> Self {
        let library_paths = r_env::find_library_paths();
        let state = Arc::new(RwLock::new(WorldState::new(library_paths)));
        let background_indexer = Arc::new(BackgroundIndexer::new(state.clone()));
        
        Self {
            client,
            state,
            background_indexer,
        }
    }
}
```

### 3. Modified did_open() Handler

```rust
async fn did_open(&self, params: DidOpenTextDocumentParams) {
    // ... existing code to extract metadata and collect files_to_index ...
    
    // Priority 1: Synchronous indexing (existing code)
    let priority_1_files: Vec<Url> = files_to_index.iter()
        .filter(|(_, priority)| *priority == 1)
        .map(|(uri, _)| uri.clone())
        .collect();
    
    if !priority_1_files.is_empty() {
        for file_uri in priority_1_files {
            self.index_file_on_demand(&file_uri).await;
        }
    }
    
    // Priority 2: Background indexing for backward directive targets
    let priority_2_files: Vec<Url> = files_to_index.iter()
        .filter(|(_, priority)| *priority == 2)
        .map(|(uri, _)| uri.clone())
        .collect();
    
    if !priority_2_files.is_empty() {
        let enabled = {
            let state = self.state.read().unwrap();
            state.cross_file_config.on_demand_indexing_priority_2_enabled
        };
        
        if enabled {
            for file_uri in priority_2_files {
                self.background_indexer.submit(file_uri, 2, 0);
            }
        }
    }
    
    // Priority 3 files are queued automatically by BackgroundIndexer
    // after Priority 2 files are indexed
    
    // ... existing code to schedule diagnostics ...
}
```

### 4. Configuration Extension

```rust
pub struct CrossFileConfig {
    // ... existing fields ...
    
    // On-demand indexing settings
    pub on_demand_indexing_enabled: bool,
    pub on_demand_indexing_max_transitive_depth: usize,
    pub on_demand_indexing_max_queue_size: usize,
    pub on_demand_indexing_priority_2_enabled: bool,
    pub on_demand_indexing_priority_3_enabled: bool,
}

impl Default for CrossFileConfig {
    fn default() -> Self {
        Self {
            // ... existing defaults ...
            on_demand_indexing_enabled: true,
            on_demand_indexing_max_transitive_depth: 2,
            on_demand_indexing_max_queue_size: 50,
            on_demand_indexing_priority_2_enabled: true,
            on_demand_indexing_priority_3_enabled: true,
        }
    }
}
```

## Correctness Properties

### Property 1: Queue Ordering
**Statement**: Tasks with lower priority numbers are always processed before tasks with higher priority numbers.

**Validation**: Property-based test that submits random tasks and verifies processing order.

### Property 2: No Duplicate Indexing
**Statement**: A file is never indexed more than once concurrently.

**Validation**: Property-based test that submits duplicate tasks and verifies only one indexing occurs.

### Property 3: Depth Limiting
**Statement**: Transitive indexing never exceeds configured max depth.

**Validation**: Property-based test with deep dependency chains verifies depth limit is respected.

### Property 4: Queue Size Limiting
**Statement**: Queue never exceeds configured maximum size.

**Validation**: Property-based test that submits many tasks verifies queue size limit.

### Property 5: Cancellation Safety
**Statement**: Worker stops cleanly when cancellation token is triggered.

**Validation**: Integration test that cancels worker and verifies no panics or hangs.

## Testing Strategy

### Unit Tests
- BackgroundIndexer::submit() with various priorities
- Queue ordering logic
- Depth tracking and limiting
- Queue size limiting

### Integration Tests
- End-to-end Priority 2 indexing (backward directives)
- End-to-end Priority 3 indexing (transitive dependencies)
- Cancellation and cleanup
- Error handling (file read failures, parse errors)

### Property Tests
- All correctness properties listed above
- Random task submission patterns
- Random dependency graphs

## Performance Considerations

1. **Single worker thread**: Avoids lock contention and resource competition
2. **Priority queue**: Ensures important files are indexed first
3. **Depth limiting**: Prevents excessive indexing of deep dependency chains
4. **Queue size limiting**: Prevents memory exhaustion
5. **Async I/O**: File reads don't block the worker thread

## Migration Path

1. Implement BackgroundIndexer struct
2. Add to Backend initialization
3. Wire up Priority 2 submission in did_open()
4. Add configuration options
5. Add tests
6. Enable by default after validation

## Alternatives Considered

### Alternative 1: Make Backend Clone
**Rejected**: Would require significant refactoring and Arc wrapping of all fields.

### Alternative 2: Parallel indexing
**Rejected**: Adds complexity with minimal benefit. Sequential is simpler and sufficient.

### Alternative 3: Synchronous Priority 2 indexing
**Rejected**: Would block diagnostics for files with backward directives.

## Open Issues

None at this time.
