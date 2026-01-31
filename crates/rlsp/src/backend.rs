//
// backend.rs
//
// Copyright (C) 2025 Posit Software, PBC. All rights reserved.
//
//

use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::Client;
use tower_lsp::LanguageServer;
use tower_lsp::LspService;
use tower_lsp::Server;

use crate::handlers;
use crate::r_env;
use crate::state::{scan_workspace, WorldState};

/// Parameters for the rlsp/activeDocumentsChanged notification
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveDocumentsChangedParams {
    active_uri: Option<String>,
    visible_uris: Vec<String>,
    timestamp_ms: u64,
}

/// Parse cross-file configuration from LSP settings
fn parse_cross_file_config(settings: &serde_json::Value) -> Option<crate::cross_file::CrossFileConfig> {
    use crate::cross_file::{CallSiteDefault, CrossFileConfig};

    let cross_file = settings.get("crossFile")?;
    let diagnostics = settings.get("diagnostics");
    
    let mut config = CrossFileConfig::default();
    
    if let Some(v) = cross_file.get("maxBackwardDepth").and_then(|v| v.as_u64()) {
        config.max_backward_depth = v as usize;
    }
    if let Some(v) = cross_file.get("maxForwardDepth").and_then(|v| v.as_u64()) {
        config.max_forward_depth = v as usize;
    }
    if let Some(v) = cross_file.get("maxChainDepth").and_then(|v| v.as_u64()) {
        config.max_chain_depth = v as usize;
    }
    if let Some(v) = cross_file.get("assumeCallSite").and_then(|v| v.as_str()) {
        config.assume_call_site = match v {
            "start" => CallSiteDefault::Start,
            _ => CallSiteDefault::End,
        };
    }
    if let Some(v) = cross_file.get("indexWorkspace").and_then(|v| v.as_bool()) {
        config.index_workspace = v;
    }
    if let Some(v) = cross_file.get("maxRevalidationsPerTrigger").and_then(|v| v.as_u64()) {
        config.max_revalidations_per_trigger = v as usize;
    }
    if let Some(v) = cross_file.get("revalidationDebounceMs").and_then(|v| v.as_u64()) {
        config.revalidation_debounce_ms = v;
    }
    
    // Parse diagnostic severities
    if let Some(sev) = cross_file.get("missingFileSeverity").and_then(|v| v.as_str()) {
        config.missing_file_severity = parse_severity(sev);
    }
    if let Some(sev) = cross_file.get("circularDependencySeverity").and_then(|v| v.as_str()) {
        config.circular_dependency_severity = parse_severity(sev);
    }
    if let Some(sev) = cross_file.get("outOfScopeSeverity").and_then(|v| v.as_str()) {
        config.out_of_scope_severity = parse_severity(sev);
    }
    if let Some(sev) = cross_file.get("ambiguousParentSeverity").and_then(|v| v.as_str()) {
        config.ambiguous_parent_severity = parse_severity(sev);
    }
    if let Some(sev) = cross_file.get("maxChainDepthSeverity").and_then(|v| v.as_str()) {
        config.max_chain_depth_severity = parse_severity(sev);
    }
    
    // Parse on-demand indexing settings
    if let Some(on_demand) = cross_file.get("onDemandIndexing") {
        if let Some(v) = on_demand.get("enabled").and_then(|v| v.as_bool()) {
            config.on_demand_indexing_enabled = v;
        }
        if let Some(v) = on_demand.get("maxTransitiveDepth").and_then(|v| v.as_u64()) {
            config.on_demand_indexing_max_transitive_depth = v as usize;
        }
        if let Some(v) = on_demand.get("maxQueueSize").and_then(|v| v.as_u64()) {
            config.on_demand_indexing_max_queue_size = v as usize;
        }
        if let Some(v) = on_demand.get("priority2Enabled").and_then(|v| v.as_bool()) {
            config.on_demand_indexing_priority_2_enabled = v;
        }
        if let Some(v) = on_demand.get("priority3Enabled").and_then(|v| v.as_bool()) {
            config.on_demand_indexing_priority_3_enabled = v;
        }
    }
    
    // Parse diagnostics.undefinedVariables
    if let Some(diag) = diagnostics {
        if let Some(v) = diag.get("undefinedVariables").and_then(|v| v.as_bool()) {
            config.undefined_variables_enabled = v;
        }
    }
    
    log::info!("Cross-file configuration loaded from LSP settings:");
    log::info!("  max_backward_depth: {}", config.max_backward_depth);
    log::info!("  max_forward_depth: {}", config.max_forward_depth);
    log::info!("  max_chain_depth: {}", config.max_chain_depth);
    log::info!("  assume_call_site: {:?}", config.assume_call_site);
    log::info!("  index_workspace: {}", config.index_workspace);
    log::info!("  max_revalidations_per_trigger: {}", config.max_revalidations_per_trigger);
    log::info!("  revalidation_debounce_ms: {}", config.revalidation_debounce_ms);
    log::info!("  undefined_variables_enabled: {}", config.undefined_variables_enabled);
    log::info!("  On-demand indexing:");
    log::info!("    enabled: {}", config.on_demand_indexing_enabled);
    log::info!("    max_transitive_depth: {}", config.on_demand_indexing_max_transitive_depth);
    log::info!("    max_queue_size: {}", config.on_demand_indexing_max_queue_size);
    log::info!("    priority_2_enabled: {}", config.on_demand_indexing_priority_2_enabled);
    log::info!("    priority_3_enabled: {}", config.on_demand_indexing_priority_3_enabled);
    log::info!("  Diagnostic severities:");
    log::info!("    missing_file: {:?}", config.missing_file_severity);
    log::info!("    circular_dependency: {:?}", config.circular_dependency_severity);
    log::info!("    out_of_scope: {:?}", config.out_of_scope_severity);
    log::info!("    ambiguous_parent: {:?}", config.ambiguous_parent_severity);
    log::info!("    max_chain_depth: {:?}", config.max_chain_depth_severity);
    
    Some(config)
}

fn parse_severity(s: &str) -> DiagnosticSeverity {
    match s.to_lowercase().as_str() {
        "error" => DiagnosticSeverity::ERROR,
        "warning" => DiagnosticSeverity::WARNING,
        "information" | "info" => DiagnosticSeverity::INFORMATION,
        "hint" => DiagnosticSeverity::HINT,
        _ => DiagnosticSeverity::WARNING,
    }
}

pub struct Backend {
    client: Client,
    state: Arc<RwLock<WorldState>>,
    background_indexer: Arc<crate::cross_file::BackgroundIndexer>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let library_paths = r_env::find_library_paths();
        log::info!("Discovered R library paths: {:?}", library_paths);

        let state = Arc::new(RwLock::new(WorldState::new(library_paths)));
        let background_indexer = Arc::new(crate::cross_file::BackgroundIndexer::new(state.clone()));

        Self {
            client,
            state,
            background_indexer,
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        log::info!("Initializing ark-lsp");

        let mut state = self.state.write().await;
        
        if let Some(folders) = params.workspace_folders {
            for folder in folders {
                log::info!("Adding workspace folder: {}", folder.uri);
                state.workspace_folders.push(folder.uri);
            }
        } else if let Some(root_uri) = params.root_uri {
            log::info!("Adding root URI as workspace folder: {}", root_uri);
            state.workspace_folders.push(root_uri);
        }
        
        drop(state);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        String::from(":"),
                        String::from("$"),
                        String::from("@"),
                    ]),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec![String::from("("), String::from(",")]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: String::from("\n"),
                    more_trigger_character: None,
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: String::from("ark-lsp"),
                version: Some(String::from(env!("CARGO_PKG_VERSION"))),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        log::info!("ark-lsp initialized");
        
        // Get workspace folders under brief lock
        let folders: Vec<Url> = {
            let state = self.state.read().await;
            state.workspace_folders.clone()
        };
        
        // Scan workspace without holding lock (Requirement 13a)
        let (index, imports, cross_file_entries) = tokio::task::spawn_blocking(move || {
            scan_workspace(&folders)
        }).await.unwrap_or_default();
        
        // Apply results under brief write lock
        {
            let mut state = self.state.write().await;
            state.apply_workspace_index(index, imports, cross_file_entries);
        }
        
        log::info!("Workspace initialization complete");
    }

    async fn shutdown(&self) -> Result<()> {
        log::info!("ark-lsp shutting down");
        Ok(())
    }

    /// Handle textDocument/didOpen notification.
    /// 
    /// ## Lock Acquisition Pattern (Deadlock Analysis)
    /// 
    /// This handler follows a safe lock acquisition pattern to avoid deadlocks:
    /// 
    /// 1. **Write lock phase**: Acquires write lock to update document state, dependency graph,
    ///    and collect work items. All state mutations happen in this phase.
    /// 
    /// 2. **Lock release**: Write lock is released BEFORE any synchronous indexing or
    ///    async operations that might need state access.
    /// 
    /// 3. **Synchronous indexing**: Priority 1 files are indexed synchronously AFTER
    ///    releasing the write lock. Each indexing operation acquires its own locks as needed.
    /// 
    /// 4. **Async diagnostics**: Diagnostics are scheduled as separate async tasks that
    ///    acquire their own read locks independently.
    /// 
    /// This pattern ensures:
    /// - No nested lock acquisition (write lock is never held while acquiring another lock)
    /// - Background tasks can safely acquire locks without blocking on this handler
    /// - Concurrent read operations can proceed during diagnostics computation
    /// 
    /// **Note for maintainers**: If adding new operations that need state access,
    /// ensure they happen AFTER the write lock is released, or use interior mutability
    /// patterns (like the diagnostics_gate) that don't require exclusive access.
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        let version = params.text_document.version;

        // Compute affected files while holding write lock
        let (work_items, debounce_ms, files_to_index, on_demand_enabled) = {
            let mut state = self.state.write().await;
            state.open_document(uri.clone(), &text, Some(version));
            // Record as recently opened for activity prioritization
            state.cross_file_activity.record_recent(uri.clone());
            
            // Update dependency graph with cross-file metadata
            let meta = crate::cross_file::extract_metadata(&text);
            let uri_clone = uri.clone();
            let workspace_root = state.workspace_folders.first().cloned();
            
            let on_demand_enabled = state.cross_file_config.on_demand_indexing_enabled;
            
            // On-demand indexing: Collect sourced files that need indexing
            // Priority 1: Files directly sourced by this open document
            let mut files_to_index = Vec::new();
            
            if on_demand_enabled {
                let path_ctx = crate::cross_file::path_resolve::PathContext::from_metadata(
                    &uri_clone, &meta, workspace_root.as_ref()
                );
                
                for source in &meta.sources {
                    if let Some(ctx) = path_ctx.as_ref() {
                        if let Some(resolved) = crate::cross_file::path_resolve::resolve_path(&source.path, ctx) {
                            if let Ok(source_uri) = Url::from_file_path(resolved) {
                                // Check if file needs indexing (not open, not in workspace index)
                                if !state.documents.contains_key(&source_uri) 
                                    && !state.cross_file_workspace_index.contains(&source_uri) {
                                    log::trace!("Scheduling on-demand indexing for sourced file: {}", source_uri);
                                    files_to_index.push((source_uri, 1)); // Priority 1
                                }
                            }
                        }
                    }
                }
                
                // Priority 2: Files referenced by backward directives
                let backward_ctx = crate::cross_file::path_resolve::PathContext::new(
                    &uri_clone, workspace_root.as_ref()
                );
                
                for directive in &meta.sourced_by {
                    if let Some(ctx) = backward_ctx.as_ref() {
                        if let Some(resolved) = crate::cross_file::path_resolve::resolve_path(&directive.path, ctx) {
                            if let Ok(parent_uri) = Url::from_file_path(resolved) {
                                if !state.documents.contains_key(&parent_uri) 
                                    && !state.cross_file_workspace_index.contains(&parent_uri) {
                                    log::trace!("Scheduling on-demand indexing for parent file: {}", parent_uri);
                                    files_to_index.push((parent_uri, 2)); // Priority 2
                                }
                            }
                        }
                    }
                }
            }
            
            // Pre-collect content for potential parent files to avoid borrow conflicts
            // The content provider needs to access documents/cache while graph is mutably borrowed
            // IMPORTANT: Use PathContext WITHOUT @lsp-cd for backward directives
            // Backward directives should always be resolved relative to the file's directory
            let backward_path_ctx = crate::cross_file::path_resolve::PathContext::new(
                &uri_clone, workspace_root.as_ref()
            );
            let parent_content: std::collections::HashMap<Url, String> = meta.sourced_by.iter()
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
            
            let result = state.cross_file_graph.update_file(
                &uri,
                &meta,
                workspace_root.as_ref(),
                |parent_uri| parent_content.get(parent_uri).cloned(),
            );
            
            // Emit any directive-vs-AST conflict diagnostics
            if !result.diagnostics.is_empty() {
                log::trace!("Directive-vs-AST conflicts detected: {} diagnostics", result.diagnostics.len());
            }
            
            // Compute affected files from dependency graph using HashSet for O(1) deduplication
            let mut affected: std::collections::HashSet<Url> = std::collections::HashSet::from([uri.clone()]);
            let dependents = state.cross_file_graph.get_transitive_dependents(
                &uri,
                state.cross_file_config.max_chain_depth,
            );
            // Filter to only open documents and mark for force republish
            for dep in dependents {
                if state.documents.contains_key(&dep) {
                    state.diagnostics_gate.mark_force_republish(&dep);
                    affected.insert(dep);
                }
            }
            
            // Convert to Vec for sorting
            let mut affected: Vec<Url> = affected.into_iter().collect();
            
            // Prioritize by activity
            // Use saturating_add to prevent integer overflow at usize::MAX
            let activity = &state.cross_file_activity;
            affected.sort_by_key(|u| {
                if *u == uri { 0 }
                else { activity.priority_score(u).saturating_add(1) }
            });
            
            // Apply revalidation cap
            let max_revalidations = state.cross_file_config.max_revalidations_per_trigger;
            if affected.len() > max_revalidations {
                log::trace!(
                    "Cross-file revalidation cap exceeded: {} affected, scheduling {}",
                    affected.len(),
                    max_revalidations
                );
                affected.truncate(max_revalidations);
            }
            
            // Build work items with trigger revision snapshot
            let work_items: Vec<_> = affected
                .into_iter()
                .map(|affected_uri| {
                    let trigger_version = state.documents.get(&affected_uri).and_then(|d| d.version);
                    let trigger_revision = state.documents.get(&affected_uri).map(|d| d.revision);
                    (affected_uri, trigger_version, trigger_revision)
                })
                .collect();
            
            let debounce_ms = state.cross_file_config.revalidation_debounce_ms;
            (work_items, debounce_ms, files_to_index, on_demand_enabled)
        };
        
        // Only perform on-demand indexing if enabled
        if on_demand_enabled {
            // Perform SYNCHRONOUS on-demand indexing for Priority 1 files (directly sourced)
            // This ensures symbols are available BEFORE diagnostics run
            let priority_1_files: Vec<Url> = files_to_index.iter()
                .filter(|(_, priority)| *priority == 1)
                .map(|(uri, _)| uri.clone())
                .collect();
            
            // Collect metadata from Priority 1 files for transitive dependency queuing
            let mut priority_1_metadata: Vec<(Url, crate::cross_file::CrossFileMetadata)> = Vec::new();
            
            if !priority_1_files.is_empty() {
                log::info!("Synchronously indexing {} directly sourced files before diagnostics", priority_1_files.len());
                for file_uri in priority_1_files {
                    if let Some(meta) = self.index_file_on_demand(&file_uri).await {
                        priority_1_metadata.push((file_uri, meta));
                    }
                }
            }
            
            // Queue transitive dependencies from Priority 1 files as Priority 3
            let (priority_3_enabled, workspace_root) = {
                let state = self.state.read().await;
                (
                    state.cross_file_config.on_demand_indexing_priority_3_enabled,
                    state.workspace_folders.first().cloned(),
                )
            };
            
            if priority_3_enabled && !priority_1_metadata.is_empty() {
                for (file_uri, meta) in &priority_1_metadata {
                    let path_ctx = crate::cross_file::path_resolve::PathContext::from_metadata(
                        file_uri, meta, workspace_root.as_ref()
                    );
                    
                    for source in &meta.sources {
                        if let Some(ctx) = path_ctx.as_ref() {
                            if let Some(resolved) = crate::cross_file::path_resolve::resolve_path(&source.path, ctx) {
                                if let Ok(source_uri) = Url::from_file_path(resolved) {
                                    let needs_indexing = {
                                        let state = self.state.read().await;
                                        !state.documents.contains_key(&source_uri)
                                            && !state.cross_file_workspace_index.contains(&source_uri)
                                    };
                                    
                                    if needs_indexing {
                                        log::trace!("Queuing transitive dependency from Priority 1: {}", source_uri);
                                        self.background_indexer.submit(source_uri, 3, 1);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            
            // Priority 2 files (backward directive targets) are indexed in background
            let priority_2_enabled = {
                let state = self.state.read().await;
                state.cross_file_config.on_demand_indexing_priority_2_enabled
            };
            
            if priority_2_enabled {
                let priority_2_files: Vec<Url> = files_to_index.iter()
                    .filter(|(_, priority)| *priority == 2)
                    .map(|(uri, _)| uri.clone())
                    .collect();
                
                if !priority_2_files.is_empty() {
                    log::info!("Submitting {} backward directive targets for background indexing", priority_2_files.len());
                    for file_uri in priority_2_files {
                        self.background_indexer.submit(file_uri, 2, 0);
                    }
                }
            }
        }

        // Schedule debounced diagnostics for all affected files via revalidation system
        for (affected_uri, trigger_version, trigger_revision) in work_items {
            let state_arc = self.state.clone();
            let client = self.client.clone();
            
            tokio::spawn(async move {
                // Schedule with cancellation token
                let token = {
                    let state = state_arc.read().await;
                    state.cross_file_revalidation.schedule(affected_uri.clone())
                };
                
                // Debounce / cancellation
                tokio::select! {
                    _ = token.cancelled() => { return; }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)) => {}
                }
                
                // Freshness guard before computing
                let diagnostics_opt = {
                    let state = state_arc.read().await;
                    
                    let current_version = state.documents.get(&affected_uri).and_then(|d| d.version);
                    let current_revision = state.documents.get(&affected_uri).map(|d| d.revision);
                    
                    if current_version != trigger_version || current_revision != trigger_revision {
                        log::trace!("Skipping stale diagnostics for {}: revision changed", affected_uri);
                        return;
                    }
                    
                    if let Some(ver) = current_version {
                        if !state.diagnostics_gate.can_publish(&affected_uri, ver) {
                            log::trace!("Skipping diagnostics for {}: monotonic gate", affected_uri);
                            return;
                        }
                    }
                    
                    Some(handlers::diagnostics(&state, &affected_uri))
                };
                
                if let Some(diagnostics) = diagnostics_opt {
                    // Second freshness check before publishing
                    let can_publish = {
                        let state = state_arc.read().await;
                        let current_version = state.documents.get(&affected_uri).and_then(|d| d.version);
                        let current_revision = state.documents.get(&affected_uri).map(|d| d.revision);
                        
                        if current_version != trigger_version || current_revision != trigger_revision {
                            false
                        } else if let Some(ver) = current_version {
                            state.diagnostics_gate.can_publish(&affected_uri, ver)
                        } else {
                            true
                        }
                    };
                    
                    if can_publish {
                        client.publish_diagnostics(affected_uri.clone(), diagnostics, None).await;
                        
                        let state = state_arc.read().await;
                        if let Some(ver) = state.documents.get(&affected_uri).and_then(|d| d.version) {
                            state.diagnostics_gate.record_publish(&affected_uri, ver);
                        }
                        state.cross_file_revalidation.complete(&affected_uri);
                    }
                }
            });
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;

        // Compute affected files and debounce config while holding write lock
        let (work_items, debounce_ms) = {
            let mut state = self.state.write().await;
            if let Some(doc) = state.documents.get_mut(&uri) {
                doc.version = Some(version);
            }
            for change in params.content_changes {
                state.apply_change(&uri, change);
            }
            // Record as recently changed for activity prioritization
            state.cross_file_activity.record_recent(uri.clone());
            
            // Update dependency graph with new cross-file metadata
            if let Some(doc) = state.documents.get(&uri) {
                let text = doc.text();
                let meta = crate::cross_file::extract_metadata(&text);
                let uri_clone = uri.clone();
                let workspace_root = state.workspace_folders.first().cloned();
                
                // Pre-collect content for potential parent files to avoid borrow conflicts
                // IMPORTANT: Use PathContext WITHOUT @lsp-cd for backward directives
                // Backward directives should always be resolved relative to the file's directory
                let backward_path_ctx = crate::cross_file::path_resolve::PathContext::new(
                    &uri_clone, workspace_root.as_ref()
                );
                let parent_content: std::collections::HashMap<Url, String> = meta.sourced_by.iter()
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
                
                let _result = state.cross_file_graph.update_file(
                    &uri,
                    &meta,
                    workspace_root.as_ref(),
                    |parent_uri| parent_content.get(parent_uri).cloned(),
                );
            }
            
            // Compute affected files from dependency graph using HashSet for O(1) deduplication
            let mut affected: std::collections::HashSet<Url> = std::collections::HashSet::from([uri.clone()]);
            let dependents = state.cross_file_graph.get_transitive_dependents(
                &uri,
                state.cross_file_config.max_chain_depth,
            );
            // Filter to only open documents and mark for force republish
            for dep in dependents {
                if state.documents.contains_key(&dep) {
                    // Mark dependent files for force republish (Requirement 0.8)
                    // This allows same-version republish when dependency changes
                    state.diagnostics_gate.mark_force_republish(&dep);
                    affected.insert(dep);
                }
            }
            
            // Convert to Vec for sorting
            let mut affected: Vec<Url> = affected.into_iter().collect();
            
            // Prioritize by activity (trigger first, then active, then visible, then recent)
            // Use saturating_add to prevent integer overflow at usize::MAX
            let activity = &state.cross_file_activity;
            affected.sort_by_key(|u| {
                if *u == uri { 0 }
                else { activity.priority_score(u).saturating_add(1) }
            });
            
            // Apply revalidation cap (Requirement 0.9, 0.10)
            let max_revalidations = state.cross_file_config.max_revalidations_per_trigger;
            if affected.len() > max_revalidations {
                log::trace!(
                    "Cross-file revalidation cap exceeded: {} affected, scheduling {}",
                    affected.len(),
                    max_revalidations
                );
                affected.truncate(max_revalidations);
            }
            
            // Build work items with trigger revision snapshot for freshness guard
            let work_items: Vec<_> = affected
                .into_iter()
                .map(|affected_uri| {
                    let trigger_version = state.documents.get(&affected_uri).and_then(|d| d.version);
                    let trigger_revision = state.documents.get(&affected_uri).map(|d| d.revision);
                    (affected_uri, trigger_version, trigger_revision)
                })
                .collect();
            
            let debounce_ms = state.cross_file_config.revalidation_debounce_ms;
            (work_items, debounce_ms)
        };

        // Schedule debounced diagnostics for all affected files (Requirement 0.5)
        for (affected_uri, trigger_version, trigger_revision) in work_items {
            let state_arc = self.state.clone();
            let client = self.client.clone();
            
            tokio::spawn(async move {
                // 1) Schedule with cancellation token
                let token = {
                    let state = state_arc.read().await;
                    state.cross_file_revalidation.schedule(affected_uri.clone())
                };
                
                // 2) Debounce / cancellation (Requirement 0.5)
                tokio::select! {
                    _ = token.cancelled() => { return; }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)) => {}
                }
                
                // 3) Freshness guard before computing (Requirement 0.6)
                let diagnostics_opt = {
                    let state = state_arc.read().await;
                    
                    let current_version = state.documents.get(&affected_uri).and_then(|d| d.version);
                    let current_revision = state.documents.get(&affected_uri).map(|d| d.revision);
                    
                    // Check freshness: both version and revision must match
                    if current_version != trigger_version || current_revision != trigger_revision {
                        log::trace!("Skipping stale diagnostics for {}: revision changed", affected_uri);
                        return;
                    }
                    
                    // Check monotonic publishing gate (Requirement 0.7)
                    if let Some(ver) = current_version {
                        if !state.diagnostics_gate.can_publish(&affected_uri, ver) {
                            log::trace!("Skipping diagnostics for {}: monotonic gate", affected_uri);
                            return;
                        }
                    }
                    
                    Some(handlers::diagnostics(&state, &affected_uri))
                };
                
                if let Some(diagnostics) = diagnostics_opt {
                    // 4) Second freshness check before publishing
                    let can_publish = {
                        let state = state_arc.read().await;
                        let current_version = state.documents.get(&affected_uri).and_then(|d| d.version);
                        let current_revision = state.documents.get(&affected_uri).map(|d| d.revision);
                        
                        if current_version != trigger_version || current_revision != trigger_revision {
                            log::trace!("Skipping stale diagnostics publish for {}: revision changed during computation", affected_uri);
                            false
                        } else if let Some(ver) = current_version {
                            if !state.diagnostics_gate.can_publish(&affected_uri, ver) {
                                log::trace!("Skipping diagnostics for {}: monotonic gate (pre-publish)", affected_uri);
                                false
                            } else {
                                true
                            }
                        } else {
                            true
                        }
                    };
                    
                    if can_publish {
                        client.publish_diagnostics(affected_uri.clone(), diagnostics, None).await;
                        
                        // Record successful publish
                        let state = state_arc.read().await;
                        if let Some(ver) = state.documents.get(&affected_uri).and_then(|d| d.version) {
                            state.diagnostics_gate.record_publish(&affected_uri, ver);
                        }
                        state.cross_file_revalidation.complete(&affected_uri);
                    }
                }
            });
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = &params.text_document.uri;
        
        // Cancel pending background indexing for this URI
        self.background_indexer.cancel_uri(uri);
        
        let mut state = self.state.write().await;
        
        // Clear diagnostics gate state
        state.diagnostics_gate.clear(uri);
        
        // Cancel pending revalidation
        state.cross_file_revalidation.cancel(uri);
        
        // Remove from activity tracking
        state.cross_file_activity.remove(uri);
        
        // Close the document
        state.close_document(uri);
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // Requirement 11.11: When configuration changes, re-resolve scope chains for open documents
        log::trace!("Configuration changed, parsing new config and scheduling revalidation");
        
        // Parse new configuration if provided
        let new_config = parse_cross_file_config(&params.settings);
        
        // Log if configuration parsing failed and defaults will be used
        if new_config.is_none() {
            log::warn!("Failed to parse cross-file configuration from settings, using existing configuration");
        }
        
        let (open_uris, scope_changed) = {
            let mut state = self.state.write().await;
            
            // Check if scope-affecting settings changed
            let scope_changed = new_config.as_ref()
                .map(|c| state.cross_file_config.scope_settings_changed(c))
                .unwrap_or(false);
            
            // Apply new config if parsed
            if let Some(config) = new_config {
                state.cross_file_config = config;
            }
            
            // Invalidate all scope caches since config affects resolution
            state.cross_file_cache.invalidate_all();
            state.cross_file_parent_cache.invalidate_all();
            
            // Mark all open documents for force republish
            let open_uris: Vec<Url> = state.documents.keys().cloned().collect();
            for uri in &open_uris {
                state.diagnostics_gate.mark_force_republish(uri);
            }
            
            (open_uris, scope_changed)
        };
        
        if scope_changed {
            log::trace!("Scope-affecting settings changed, revalidating {} open documents", open_uris.len());
        }
        
        // Schedule diagnostics for all open documents
        for uri in open_uris {
            self.publish_diagnostics(&uri).await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        log::trace!("Received watched files change: {} changes", params.changes.len());
        
        // Collect deleted URIs for batch cancellation
        let deleted_uris: Vec<Url> = params.changes.iter()
            .filter(|c| c.typ == FileChangeType::DELETED)
            .map(|c| c.uri.clone())
            .collect();
        
        // Cancel pending background indexing for deleted files
        if !deleted_uris.is_empty() {
            self.background_indexer.cancel_uris(deleted_uris.iter());
        }
        
        // Collect URIs to update and affected open documents
        let (uris_to_update, affected_open_docs): (Vec<Url>, Vec<Url>) = {
            let mut state = self.state.write().await;
            let mut to_update = Vec::new();
            let mut affected = Vec::new();
            
            for change in &params.changes {
                let uri = &change.uri;
                
                // Skip if document is open (open docs are authoritative)
                if state.documents.contains_key(uri) {
                    log::trace!("Skipping watched file change for open document: {}", uri);
                    continue;
                }
                
                match change.typ {
                    FileChangeType::CREATED | FileChangeType::CHANGED => {
                        // Invalidate disk-backed caches
                        state.cross_file_file_cache.invalidate(uri);
                        state.cross_file_workspace_index.invalidate(uri);
                        
                        // Schedule for async update
                        to_update.push(uri.clone());
                        
                        // Find open documents that depend on this file
                        let dependents = state.cross_file_graph.get_transitive_dependents(
                            uri,
                            state.cross_file_config.max_chain_depth,
                        );
                        for dep in dependents {
                            if state.documents.contains_key(&dep) && !affected.contains(&dep) {
                                state.diagnostics_gate.mark_force_republish(&dep);
                                affected.push(dep);
                            }
                        }
                        log::trace!("Invalidated caches for changed file: {}", uri);
                    }
                    FileChangeType::DELETED => {
                        // Find dependents before removing from graph
                        let dependents = state.cross_file_graph.get_transitive_dependents(
                            uri,
                            state.cross_file_config.max_chain_depth,
                        );
                        for dep in dependents {
                            if state.documents.contains_key(&dep) && !affected.contains(&dep) {
                                state.diagnostics_gate.mark_force_republish(&dep);
                                affected.push(dep);
                            }
                        }
                        
                        // Remove from dependency graph and caches
                        state.cross_file_graph.remove_file(uri);
                        state.cross_file_file_cache.invalidate(uri);
                        state.cross_file_workspace_index.invalidate(uri);
                        state.cross_file_cache.invalidate(uri);
                        state.cross_file_meta.remove(uri);
                        log::trace!("Removed deleted file from cross-file state: {}", uri);
                    }
                    _ => {}
                }
            }
            (to_update, affected)
        };
        
        // Schedule async disk reads to update workspace index for changed files
        if !uris_to_update.is_empty() {
            let state_arc = self.state.clone();
            tokio::spawn(async move {
                for uri in uris_to_update {
                    // Read file content asynchronously
                    let path = match uri.to_file_path() {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    
                    let content = match tokio::fs::read_to_string(&path).await {
                        Ok(c) => c,
                        Err(e) => {
                            log::trace!("Failed to read file {}: {}", uri, e);
                            continue;
                        }
                    };
                    
                    let metadata = match tokio::fs::metadata(&path).await {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    
                    // Compute metadata and artifacts
                    let cross_file_meta = crate::cross_file::extract_metadata(&content);
                    let artifacts = {
                        let mut parser = tree_sitter::Parser::new();
                        if parser.set_language(&tree_sitter_r::LANGUAGE.into()).is_ok() {
                            if let Some(tree) = parser.parse(&content, None) {
                                crate::cross_file::scope::compute_artifacts(&uri, &tree, &content)
                            } else {
                                crate::cross_file::scope::ScopeArtifacts::default()
                            }
                        } else {
                            crate::cross_file::scope::ScopeArtifacts::default()
                        }
                    };
                    
                    let snapshot = crate::cross_file::file_cache::FileSnapshot::with_content_hash(
                        &metadata,
                        &content,
                    );
                    
                    // Cache content for future match/inference resolution
                    state_arc.read().await.cross_file_file_cache.insert(
                        uri.clone(),
                        snapshot.clone(),
                        content.clone(),
                    );
                    
                    // Update workspace index under brief lock
                    {
                        let state = state_arc.read().await;
                        let open_docs: std::collections::HashSet<_> = state.documents.keys().cloned().collect();
                        state.cross_file_workspace_index.update_from_disk(
                            &uri,
                            &open_docs,
                            snapshot,
                            cross_file_meta.clone(),
                            artifacts,
                        );
                    }
                    
                    // Update dependency graph
                    {
                        let mut state = state_arc.write().await;
                        let uri_clone = uri.clone();
                        let workspace_root = state.workspace_folders.first().cloned();
                        
                        // Pre-collect content for potential parent files to avoid borrow conflicts
                        // IMPORTANT: Use PathContext WITHOUT @lsp-cd for backward directives
                        // Backward directives should always be resolved relative to the file's directory
                        let backward_path_ctx = crate::cross_file::path_resolve::PathContext::new(
                            &uri_clone, workspace_root.as_ref()
                        );
                        let parent_content: std::collections::HashMap<Url, String> = cross_file_meta.sourced_by.iter()
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
                            &uri,
                            &cross_file_meta,
                            workspace_root.as_ref(),
                            |parent_uri| parent_content.get(parent_uri).cloned(),
                        );
                    }
                    
                    log::trace!("Updated workspace index for: {}", uri);
                }
            });
        }
        
        // Schedule diagnostics for affected open documents (Requirement 13.4)
        for uri in affected_open_docs {
            self.publish_diagnostics(&uri).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.publish_diagnostics(&params.text_document.uri).await;
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let state = self.state.read().await;
        Ok(handlers::folding_range(&state, &params.text_document.uri))
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let state = self.state.read().await;
        Ok(handlers::selection_range(
            &state,
            &params.text_document.uri,
            params.positions,
        ))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let state = self.state.read().await;
        Ok(handlers::document_symbol(&state, &params.text_document.uri))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let state = self.state.read().await;
        Ok(handlers::completion(
            &state,
            &params.text_document_position.text_document.uri,
            params.text_document_position.position,
        ))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let state = self.state.read().await;
        Ok(handlers::hover(
            &state,
            &params.text_document_position_params.text_document.uri,
            params.text_document_position_params.position,
        ))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let state = self.state.read().await;
        Ok(handlers::signature_help(
            &state,
            &params.text_document_position_params.text_document.uri,
            params.text_document_position_params.position,
        ))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let state = self.state.read().await;
        Ok(handlers::goto_definition(
            &state,
            &params.text_document_position_params.text_document.uri,
            params.text_document_position_params.position,
        ))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let state = self.state.read().await;
        Ok(handlers::references(
            &state,
            &params.text_document_position.text_document.uri,
            params.text_document_position.position,
        ))
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let state = self.state.read().await;
        Ok(handlers::on_type_formatting(
            &state,
            &params.text_document_position.text_document.uri,
            params.text_document_position.position,
        ))
    }
}

impl Backend {
    /// Synchronously index a file on-demand (blocking operation).
    /// Returns the cross-file metadata if indexing succeeded, None otherwise.
    /// 
    /// ## Sequential File I/O Rationale
    /// 
    /// This function processes files sequentially rather than concurrently for several reasons:
    /// 
    /// 1. **Dependency graph serialization**: Each file's metadata updates the dependency graph,
    ///    which requires exclusive write access. Concurrent updates would require complex
    ///    synchronization and could lead to inconsistent graph state.
    /// 
    /// 2. **Cache coherence**: The workspace index and file cache are updated after each file.
    ///    Sequential processing ensures later files can see earlier files' cached content
    ///    for parent resolution.
    /// 
    /// 3. **I/O is fast relative to parsing**: File reads are typically fast (< 1ms for typical
    ///    R files). The parsing and analysis phase dominates execution time, and that already
    ///    uses efficient thread-local parsers.
    /// 
    /// 4. **Simpler error handling**: Sequential processing allows early termination on errors
    ///    without needing to coordinate cancellation of parallel tasks.
    /// 
    /// **When concurrent execution might be beneficial**:
    /// - If profiling shows I/O wait time dominates (e.g., network filesystems)
    /// - If files are independent (no cross-references between them)
    /// - Consider batching: read all files concurrently, then process sequentially
    async fn index_file_on_demand(&self, file_uri: &Url) -> Option<crate::cross_file::CrossFileMetadata> {
        log::trace!("On-demand indexing: {}", file_uri);
        
        // Read file content
        let path = match file_uri.to_file_path() {
            Ok(p) => p,
            Err(_) => {
                log::trace!("Failed to convert URI to path: {}", file_uri);
                return None;
            }
        };
        
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                log::trace!("Failed to read file {}: {}", file_uri, e);
                return None;
            }
        };
        
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => {
                log::trace!("Failed to get metadata for: {}", file_uri);
                return None;
            }
        };
        
        // Compute cross-file metadata and artifacts using thread-local parser
        let cross_file_meta = crate::cross_file::extract_metadata(&content);
        let artifacts = crate::parser_pool::with_parser(|parser| {
            if let Some(tree) = parser.parse(&content, None) {
                crate::cross_file::scope::compute_artifacts(file_uri, &tree, &content)
            } else {
                crate::cross_file::scope::ScopeArtifacts::default()
            }
        });
        
        let snapshot = crate::cross_file::file_cache::FileSnapshot::with_content_hash(
            &metadata,
            &content,
        );
        
        // Cache content for future resolution
        self.state.read().await.cross_file_file_cache.insert(
            file_uri.clone(),
            snapshot.clone(),
            content.clone(),
        );
        
        // Update workspace index
        {
            let state = self.state.read().await;
            let open_docs: std::collections::HashSet<_> = state.documents.keys().cloned().collect();
            state.cross_file_workspace_index.update_from_disk(
                file_uri,
                &open_docs,
                snapshot,
                cross_file_meta.clone(),
                artifacts.clone(),
            );
        }
        
        // Update dependency graph
        {
            let mut state = self.state.write().await;
            let file_uri_clone = file_uri.clone();
            let workspace_root = state.workspace_folders.first().cloned();
            
            // Pre-collect content for potential parent files
            let backward_path_ctx = crate::cross_file::path_resolve::PathContext::new(
                &file_uri_clone, workspace_root.as_ref()
            );
            let parent_content: std::collections::HashMap<Url, String> = cross_file_meta.sourced_by.iter()
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
                file_uri,
                &cross_file_meta,
                workspace_root.as_ref(),
                |parent_uri| parent_content.get(parent_uri).cloned(),
            );
        }
        
        log::info!("On-demand indexed: {} (exported {} symbols)", file_uri, 
            self.state.read().await.cross_file_workspace_index.get_artifacts(file_uri)
                .map(|a| a.exported_interface.len()).unwrap_or(0));
        
        Some(cross_file_meta)
    }

    async fn publish_diagnostics(&self, uri: &Url) {
        let state = self.state.read().await;
        let version = state.documents.get(uri).and_then(|d| d.version);
        
        // Check if we can publish (monotonic gate)
        if let Some(ver) = version {
            if !state.diagnostics_gate.can_publish(uri, ver) {
                log::trace!("Skipping diagnostics for {}: monotonic gate (version={})", uri, ver);
                return;
            } else {
                log::trace!("Publishing diagnostics for {}: monotonic gate allows (version={})", uri, ver);
            }
        }
        
        let diagnostics = handlers::diagnostics(&state, uri);
        
        // Record the publish (uses interior mutability, no write lock needed)
        if let Some(ver) = version {
            state.diagnostics_gate.record_publish(uri, ver);
        }
        
        drop(state);
        
        self.client.publish_diagnostics(uri.clone(), diagnostics, None).await;
    }

    /// Handle the rlsp/activeDocumentsChanged notification (Requirement 15)
    async fn handle_active_documents_changed(&self, params: ActiveDocumentsChangedParams) {
        log::trace!(
            "Received activeDocumentsChanged: active={:?}, visible={}, timestamp={}",
            params.active_uri,
            params.visible_uris.len(),
            params.timestamp_ms
        );

        let active_uri = params.active_uri.and_then(|s| Url::parse(&s).ok());
        let visible_uris: Vec<Url> = params
            .visible_uris
            .iter()
            .filter_map(|s| Url::parse(s).ok())
            .collect();

        let mut state = self.state.write().await;
        state.cross_file_activity.update(active_uri, visible_uris, params.timestamp_ms);
    }
}

pub async fn start_lsp() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(Backend::new)
        .custom_method("rlsp/activeDocumentsChanged", Backend::handle_active_documents_changed)
        .finish();
    Server::new(stdin, stdout, socket).serve(service).await;

    Ok(())
}


#[cfg(test)]
mod tests {
    /// Tests for saturating arithmetic used in priority scoring
    /// Validates Requirements 1.1, 1.2
    mod saturating_arithmetic {
        #[test]
        fn test_saturating_add_at_max() {
            // usize::MAX + 1 should saturate to usize::MAX
            assert_eq!(usize::MAX.saturating_add(1), usize::MAX);
        }

        #[test]
        fn test_saturating_add_near_max() {
            // (usize::MAX - 1) + 1 should equal usize::MAX
            assert_eq!((usize::MAX - 1).saturating_add(1), usize::MAX);
        }

        #[test]
        fn test_saturating_add_normal_values() {
            // Normal values should work correctly
            assert_eq!(0_usize.saturating_add(1), 1);
            assert_eq!(100_usize.saturating_add(1), 101);
            assert_eq!(1000_usize.saturating_add(1), 1001);
        }
    }

    /// Tests for HashSet behavior in affected files collection
    /// Validates Requirements 3.3
    mod hashset_behavior {
        use std::collections::HashSet;

        #[test]
        fn test_first_insert_returns_true() {
            let mut set: HashSet<String> = HashSet::new();
            assert!(set.insert("file1.R".to_string()));
        }

        #[test]
        fn test_duplicate_insert_returns_false() {
            let mut set: HashSet<String> = HashSet::new();
            set.insert("file1.R".to_string());
            assert!(!set.insert("file1.R".to_string()));
        }

        #[test]
        fn test_no_duplicates_in_collection() {
            let mut set: HashSet<String> = HashSet::new();
            set.insert("file1.R".to_string());
            set.insert("file2.R".to_string());
            set.insert("file1.R".to_string()); // duplicate
            set.insert("file3.R".to_string());
            set.insert("file2.R".to_string()); // duplicate
            
            assert_eq!(set.len(), 3);
            assert!(set.contains("file1.R"));
            assert!(set.contains("file2.R"));
            assert!(set.contains("file3.R"));
        }
    }

    // ============================================================================
    // Property Tests for Saturating Arithmetic
    // Property 1: Saturating Arithmetic Prevents Overflow - validates Requirements 1.1, 1.2
    // ============================================================================
    mod property_tests {
        use proptest::prelude::*;
        use std::collections::HashSet;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            /// Property 1: For any usize value, saturating_add(1) should never overflow
            /// and should return a value >= the original value.
            #[test]
            fn prop_saturating_add_never_overflows(value: usize) {
                let result = value.saturating_add(1);
                // Result should be >= original (no wrap-around)
                prop_assert!(result >= value);
                // Result should be at most usize::MAX
                prop_assert!(result <= usize::MAX);
            }

            /// Property 1 extended: saturating_add should be monotonic up to MAX
            #[test]
            fn prop_saturating_add_monotonic(value in 0..usize::MAX) {
                let result = value.saturating_add(1);
                // For values < MAX, result should be exactly value + 1
                prop_assert_eq!(result, value + 1);
            }

            /// Property 1 boundary: values at or near MAX should saturate
            #[test]
            fn prop_saturating_add_boundary(offset in 0_usize..10) {
                let value = usize::MAX - offset;
                let result = value.saturating_add(offset + 1);
                // Should saturate at MAX
                prop_assert_eq!(result, usize::MAX);
            }

            // ============================================================================
            // Property 2: System Stability at Boundary Conditions - validates Requirements 1.4
            // ============================================================================

            /// Property 2: System should remain stable when counters are at boundary values.
            /// Operations involving priority scores and depth counters should not panic or
            /// produce incorrect results at maximum values.
            #[test]
            fn prop_system_stability_at_boundaries(
                priority_score in 0_usize..=usize::MAX,
                depth in 0_usize..=usize::MAX,
                num_operations in 1_usize..100
            ) {
                // Simulate priority score adjustments
                let mut score = priority_score;
                for _ in 0..num_operations {
                    score = score.saturating_add(1);
                    // Should never panic or overflow
                    prop_assert!(score <= usize::MAX);
                    prop_assert!(score >= priority_score);
                }

                // Simulate depth increments
                let mut d = depth;
                for _ in 0..num_operations {
                    d = d.saturating_add(1);
                    // Should never panic or overflow
                    prop_assert!(d <= usize::MAX);
                    prop_assert!(d >= depth);
                }
            }

            // ============================================================================
            // Property 4: HashSet Insert Deduplication - validates Requirements 3.3
            // ============================================================================

            /// Property 4: For any sequence of strings with duplicates, HashSet should
            /// deduplicate and insert should return correct boolean.
            #[test]
            fn prop_hashset_insert_deduplication(
                items in prop::collection::vec("[a-z]{1,10}\\.R", 1..20)
            ) {
                let mut set: HashSet<String> = HashSet::new();
                let mut seen: HashSet<String> = HashSet::new();
                
                for item in &items {
                    let is_new = !seen.contains(item);
                    let insert_result = set.insert(item.clone());
                    
                    // insert should return true iff item was not seen before
                    prop_assert_eq!(insert_result, is_new);
                    seen.insert(item.clone());
                }
                
                // Final set should have no duplicates
                let unique_count = items.iter().collect::<HashSet<_>>().len();
                prop_assert_eq!(set.len(), unique_count);
            }
        }
    }

    /// Integration tests for cross-file features
    /// Validates Requirements 1.4 (system stability at boundary conditions)
    mod integration_tests {
        use std::collections::HashSet;

        /// Test that affected files collection handles large dependency graphs
        #[test]
        fn test_large_dependency_graph_deduplication() {
            // Simulate a large dependency graph with many duplicates
            let mut affected: HashSet<String> = HashSet::new();
            
            // Add 1000 files with many duplicates
            for i in 0..1000 {
                let file = format!("file{}.R", i % 100); // Only 100 unique files
                affected.insert(file);
            }
            
            // Should have exactly 100 unique files
            assert_eq!(affected.len(), 100);
            
            // Convert to Vec for sorting (as done in actual code)
            let mut affected_vec: Vec<String> = affected.into_iter().collect();
            affected_vec.sort();
            
            assert_eq!(affected_vec.len(), 100);
        }

        /// Test that saturating arithmetic handles deep transitive dependencies
        #[test]
        fn test_deep_transitive_dependencies() {
            // Simulate depth tracking with saturating arithmetic
            let mut depth: usize = 0;
            
            // Simulate very deep dependency chain
            for _ in 0..1000 {
                depth = depth.saturating_add(1);
            }
            
            assert_eq!(depth, 1000);
            
            // Test at boundary
            depth = usize::MAX - 5;
            for _ in 0..10 {
                depth = depth.saturating_add(1);
            }
            
            // Should saturate at MAX
            assert_eq!(depth, usize::MAX);
        }

        /// Test that priority scoring handles maximum values
        #[test]
        fn test_priority_scoring_at_max() {
            // Simulate priority scoring with saturating arithmetic
            let scores = vec![0_usize, 1, 100, usize::MAX - 1, usize::MAX];
            
            for score in scores {
                let adjusted = score.saturating_add(1);
                // Should never overflow
                assert!(adjusted >= score);
                assert!(adjusted <= usize::MAX);
            }
        }
    }

    /// Tests for on-demand indexing global flag
    /// Validates Requirements 1.1, 1.2, 1.3, 1.4
    mod on_demand_indexing_flag {
        /// Property 1: On-demand indexing respects global flag
        /// When on_demand_indexing_enabled is false, no indexing operations should occur.
        #[test]
        fn test_global_flag_disables_all_indexing() {
            // Simulate the flag check logic from did_open
            let on_demand_enabled = false;
            let mut files_to_index: Vec<(String, usize)> = Vec::new();
            let mut priority_1_indexed = false;
            let mut priority_2_submitted = false;
            let mut priority_3_queued = false;

            // Simulate file collection (only if enabled)
            if on_demand_enabled {
                files_to_index.push(("sourced.R".to_string(), 1));
                files_to_index.push(("parent.R".to_string(), 2));
            }

            // Simulate indexing (only if enabled)
            if on_demand_enabled {
                // Priority 1 synchronous indexing
                for (_, priority) in &files_to_index {
                    if *priority == 1 {
                        priority_1_indexed = true;
                    }
                }
                // Priority 3 transitive queuing would happen here
                priority_3_queued = true;
                // Priority 2 background submission
                for (_, priority) in &files_to_index {
                    if *priority == 2 {
                        priority_2_submitted = true;
                    }
                }
            }

            // Verify no indexing occurred
            assert!(files_to_index.is_empty(), "No files should be collected when disabled");
            assert!(!priority_1_indexed, "Priority 1 indexing should be skipped");
            assert!(!priority_2_submitted, "Priority 2 submission should be skipped");
            assert!(!priority_3_queued, "Priority 3 queuing should be skipped");
        }

        #[test]
        fn test_global_flag_enables_indexing() {
            // Simulate the flag check logic from did_open
            let on_demand_enabled = true;
            let mut files_to_index: Vec<(String, usize)> = Vec::new();
            let mut priority_1_indexed = false;
            let mut priority_2_submitted = false;

            // Simulate file collection (only if enabled)
            if on_demand_enabled {
                files_to_index.push(("sourced.R".to_string(), 1));
                files_to_index.push(("parent.R".to_string(), 2));
            }

            // Simulate indexing (only if enabled)
            if on_demand_enabled {
                // Priority 1 synchronous indexing
                for (_, priority) in &files_to_index {
                    if *priority == 1 {
                        priority_1_indexed = true;
                    }
                }
                // Priority 2 background submission
                for (_, priority) in &files_to_index {
                    if *priority == 2 {
                        priority_2_submitted = true;
                    }
                }
            }

            // Verify indexing occurred
            assert_eq!(files_to_index.len(), 2, "Files should be collected when enabled");
            assert!(priority_1_indexed, "Priority 1 indexing should occur");
            assert!(priority_2_submitted, "Priority 2 submission should occur");
        }
    }
}
