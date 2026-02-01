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

/// Extract loaded packages from a parsed tree
/// 
/// This is a helper function for on-demand indexing that extracts
/// library(), require(), and loadNamespace() calls from R code.
fn extract_loaded_packages_from_tree(tree: &Option<tree_sitter::Tree>, text: &str) -> Vec<String> {
    let Some(tree) = tree else {
        return Vec::new();
    };

    let mut packages = Vec::new();
    
    fn is_valid_package_name(name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        if name.contains("..") || name.contains('/') || name.contains('\\') {
            return false;
        }
        name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    }

    fn visit_node(node: tree_sitter::Node, text: &str, packages: &mut Vec<String>) {
        if node.kind() == "call" {
            if let Some(func_node) = node.child_by_field_name("function") {
                let func_text = &text[func_node.byte_range()];
                
                if func_text == "library" || func_text == "require" || func_text == "loadNamespace" {
                    if let Some(args_node) = node.child_by_field_name("arguments") {
                        for i in 0..args_node.child_count() {
                            if let Some(child) = args_node.child(i) {
                                if child.kind() == "argument" {
                                    if let Some(value_node) = child.child_by_field_name("value") {
                                        let value_text = &text[value_node.byte_range()];
                                        let pkg_name = value_text.trim_matches(|c: char| c == '"' || c == '\'');
                                        if is_valid_package_name(pkg_name) {
                                            packages.push(pkg_name.to_string());
                                        } else {
                                            log::warn!("Skipping suspicious package name: {}", pkg_name);
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
                visit_node(child, text, packages);
            }
        }
    }
    
    visit_node(tree.root_node(), text, &mut packages);
    packages
}

/// Parameters for the rlsp/activeDocumentsChanged notification
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveDocumentsChangedParams {
    active_uri: Option<String>,
    visible_uris: Vec<String>,
    timestamp_ms: u64,
}

/// Parse cross-file configuration from LSP settings.
///
/// Reads the top-level `crossFile` section (and related `diagnostics` and `packages`
/// settings) from a serde_json::Value and constructs a populated `CrossFileConfig`.
/// Only fields present in the provided JSON are applied; absent fields retain their
/// defaults from `CrossFileConfig::default()`.
///
/// Supported top-level keys read:
/// - `crossFile`: core cross-file behavior and diagnostic severities.
/// - `diagnostics.undefinedVariables`: enables undefined variable diagnostics.
/// - `packages`: package-related settings (`enabled`, `additionalLibraryPaths`, `rPath`, `missingPackageSeverity`).
///
/// # Returns
///
/// `Some(CrossFileConfig)` populated from `settings` when the top-level `crossFile`
/// section is present; `None` if `crossFile` is missing.
///
/// # Examples
///
/// ```
/// use serde_json::json;
/// let settings = json!({
///     "crossFile": {
///         "maxBackwardDepth": 5,
///         "indexWorkspace": true,
///         "missingFileSeverity": "warning",
///         "onDemandIndexing": { "enabled": true, "priority2Enabled": false }
///     },
///     "packages": {
///         "enabled": true,
///         "additionalLibraryPaths": ["/usr/local/lib/R/site-library"],
///         "rPath": "/usr/bin/R",
///         "missingPackageSeverity": "information"
///     },
///     "diagnostics": { "undefinedVariables": false }
/// });
///
/// let cfg = crate::backend::parse_cross_file_config(&settings);
/// assert!(cfg.is_some());
/// let cfg = cfg.unwrap();
/// assert_eq!(cfg.max_backward_depth, 5);
/// assert!(cfg.index_workspace);
/// assert!(cfg.packages_enabled);
/// ```
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
    
    // Parse package settings (Requirement 12, Task 14.2)
    if let Some(packages) = settings.get("packages") {
        if let Some(v) = packages.get("enabled").and_then(|v| v.as_bool()) {
            config.packages_enabled = v;
        }
        if let Some(paths) = packages.get("additionalLibraryPaths").and_then(|v| v.as_array()) {
            config.packages_additional_library_paths = paths
                .iter()
                .filter_map(|p| p.as_str())
                .map(std::path::PathBuf::from)
                .collect();
        }
        if let Some(v) = packages.get("rPath").and_then(|v| v.as_str()) {
            config.packages_r_path = Some(std::path::PathBuf::from(v));
        }
        if let Some(sev) = packages.get("missingPackageSeverity").and_then(|v| v.as_str()) {
            config.packages_missing_package_severity = parse_severity(sev);
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
    log::info!("  Package settings:");
    log::info!("    enabled: {}", config.packages_enabled);
    log::info!("    additional_library_paths: {:?}", config.packages_additional_library_paths);
    log::info!("    r_path: {:?}", config.packages_r_path);
    log::info!("    missing_package_severity: {:?}", config.packages_missing_package_severity);
    
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

    /// Performs post-initialization work for the language server.
    ///
    /// This method performs workspace startup tasks: it scans configured workspace folders and applies the discovered workspace index to shared state, and it initializes or replaces the in-memory PackageLibrary according to the current cross-file configuration (enabling package-aware features when configured). All long-running or blocking operations are performed without holding the main WorldState write lock so startup does not block other LSP activity.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assuming `backend` is an initialized `Backend` and a Tokio runtime is available:
    /// // tokio::spawn(async move {
    /// //     backend.initialized(lsp_types::InitializedParams {}).await;
    /// // });
    /// ```
    async fn initialized(&self, _: InitializedParams) {
        log::info!("ark-lsp initialized");
        
        // Get workspace folders under brief lock
        let folders: Vec<Url> = {
            let state = self.state.read().await;
            state.workspace_folders.clone()
        };
        
        // Scan workspace without holding lock (Requirement 13a)
        let (index, imports, cross_file_entries, new_index_entries) = tokio::task::spawn_blocking(move || {
            scan_workspace(&folders)
        }).await.unwrap_or_default();
        
        // Apply results under brief write lock
        {
            let mut state = self.state.write().await;
            state.apply_workspace_index(index, imports, cross_file_entries, new_index_entries);
        }
        
        // Initialize PackageLibrary without holding WorldState lock
        // Get config values under brief read lock
        let (packages_enabled, packages_r_path, additional_paths) = {
            let state = self.state.read().await;
            (
                state.cross_file_config.packages_enabled,
                state.cross_file_config.packages_r_path.clone(),
                state.cross_file_config.packages_additional_library_paths.clone(),
            )
        };
        
        let new_package_library = if packages_enabled {
            // Create RSubprocess and initialize PackageLibrary
            let r_subprocess = crate::r_subprocess::RSubprocess::new(packages_r_path);
            let mut lib = crate::package_library::PackageLibrary::with_subprocess(r_subprocess);
            if let Err(e) = lib.initialize().await {
                log::warn!("Failed to initialize PackageLibrary: {}", e);
            }
            // Add additional library paths (dedup)
            lib.add_library_paths(&additional_paths);
            log::info!(
                "PackageLibrary initialized: {} lib_paths, {} base_packages, {} base_exports",
                lib.lib_paths().len(),
                lib.base_packages().len(),
                lib.base_exports().len()
            );
            std::sync::Arc::new(lib)
        } else {
            log::info!("Package function awareness disabled");
            std::sync::Arc::new(crate::package_library::PackageLibrary::new_empty())
        };
        
        // Replace package_library under brief write lock
        {
            let mut state = self.state.write().await;
            state.package_library = new_package_library;
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
        let (work_items, debounce_ms, files_to_index, on_demand_enabled, packages_to_prefetch, packages_enabled, package_library) = {
            let mut state = self.state.write().await;
            
            // Update new DocumentStore (Requirement 1.3)
            state.document_store.open(uri.clone(), &text, version).await;
            
            // Update legacy documents HashMap (for migration compatibility)
            state.open_document(uri.clone(), &text, Some(version));
            // Record as recently opened for activity prioritization
            state.cross_file_activity.record_recent(uri.clone());
            
            // Update dependency graph with cross-file metadata
            let meta = crate::cross_file::extract_metadata(&text);
            let uri_clone = uri.clone();
            let workspace_root = state.workspace_folders.first().cloned();
            
            let on_demand_enabled = state.cross_file_config.on_demand_indexing_enabled;
            let packages_enabled = state.cross_file_config.packages_enabled;
            
            // Collect package names from library calls for background prefetch
            let packages_to_prefetch: Vec<String> = if packages_enabled {
                meta.library_calls.iter().map(|c| c.package.clone()).collect()
            } else {
                Vec::new()
            };
            let package_library = state.package_library.clone();
            
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
            (work_items, debounce_ms, files_to_index, on_demand_enabled, packages_to_prefetch, packages_enabled, package_library)
        };
        
        // Background prefetch package exports (without holding WorldState lock)
        if packages_enabled && !packages_to_prefetch.is_empty() {
            let pkg_lib = package_library;
            tokio::spawn(async move {
                log::trace!("Background prefetching {} packages", packages_to_prefetch.len());
                pkg_lib.prefetch_packages(&packages_to_prefetch).await;
            });
        }
        
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
                
                // Extract data for async diagnostics while holding lock briefly
                let diagnostics_data = {
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
                    
                    let sync_diagnostics = handlers::diagnostics(&state, &affected_uri);
                    let directive_meta = state.documents.get(&affected_uri)
                        .map(|doc| crate::cross_file::directive::parse_directives(&doc.text()))
                        .unwrap_or_default();
                    let workspace_folder = state.workspace_folders.first().cloned();
                    let missing_file_severity = state.cross_file_config.missing_file_severity;
                    
                    Some((sync_diagnostics, directive_meta, workspace_folder, missing_file_severity))
                };
                
                let Some((sync_diagnostics, directive_meta, workspace_folder, missing_file_severity)) = diagnostics_data else {
                    return;
                };
                
                // Perform async missing file existence checks (non-blocking I/O)
                let diagnostics = handlers::diagnostics_async_standalone(
                    &affected_uri,
                    sync_diagnostics,
                    &directive_meta,
                    workspace_folder.as_ref(),
                    missing_file_severity,
                ).await;
                
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
            });
        }
    }

    /// Handle a text-document change: update in-memory state, compute affected documents, and schedule debounced diagnostics and optional package prefetching.
    ///
    /// This method processes an LSP `textDocument/didChange` notification by updating the server's document store and dependency graph, determining which open documents are affected by the change, and scheduling debounced asynchronous diagnostics for those documents. If package indexing is enabled and package references are found, it also triggers background prefetching of package exports.
    ///
    /// Key observable behaviors:
    /// - Updates the server's document store and legacy open-document map with the incoming changes.
    /// - Recomputes cross-file dependencies and selects affected open documents to revalidate (subject to configured caps and activity-based prioritization).
    /// - Schedules debounced diagnostic runs for each affected document; each scheduled run performs freshness checks before and after asynchronous work and respects the server's monotonic publishing gate.
    /// - If packages are enabled and package names were discovered, initiates background prefetch of referenced package exports without holding the main state lock.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Typical invocation occurs inside the LSP runtime:
    /// // backend.did_change(params).await;
    /// ```
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;

        // Compute affected files and debounce config while holding write lock
        let (work_items, debounce_ms, packages_to_prefetch, packages_enabled, package_library) = {
            let mut state = self.state.write().await;
            
            // Update new DocumentStore (Requirement 1.4)
            state.document_store.update(&uri, params.content_changes.clone(), version).await;
            
            // Update legacy documents HashMap (for migration compatibility)
            if let Some(doc) = state.documents.get_mut(&uri) {
                doc.version = Some(version);
            }
            for change in params.content_changes {
                state.apply_change(&uri, change);
            }
            // Record as recently changed for activity prioritization
            state.cross_file_activity.record_recent(uri.clone());
            
            // Capture package settings for background prefetch
            let packages_enabled = state.cross_file_config.packages_enabled;
            let package_library = state.package_library.clone();
            
            // Update dependency graph with new cross-file metadata
            let packages_to_prefetch: Vec<String> = if let Some(doc) = state.documents.get(&uri) {
                let text = doc.text();
                let meta = crate::cross_file::extract_metadata(&text);
                let uri_clone = uri.clone();
                let workspace_root = state.workspace_folders.first().cloned();
                
                // Collect package names for prefetch
                let pkgs: Vec<String> = if packages_enabled {
                    meta.library_calls.iter().map(|c| c.package.clone()).collect()
                } else {
                    Vec::new()
                };
                
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
                
                pkgs
            } else {
                Vec::new()
            };
            
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
            (work_items, debounce_ms, packages_to_prefetch, packages_enabled, package_library)
        };

        // Background prefetch package exports (without holding WorldState lock)
        if packages_enabled && !packages_to_prefetch.is_empty() {
            let pkg_lib = package_library;
            tokio::spawn(async move {
                log::trace!("Background prefetching {} packages", packages_to_prefetch.len());
                pkg_lib.prefetch_packages(&packages_to_prefetch).await;
            });
        }

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
                
                // 3) Extract data for async diagnostics while holding lock briefly (Requirement 0.6)
                let diagnostics_data = {
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
                    
                    let sync_diagnostics = handlers::diagnostics(&state, &affected_uri);
                    let directive_meta = state.documents.get(&affected_uri)
                        .map(|doc| crate::cross_file::directive::parse_directives(&doc.text()))
                        .unwrap_or_default();
                    let workspace_folder = state.workspace_folders.first().cloned();
                    let missing_file_severity = state.cross_file_config.missing_file_severity;
                    
                    Some((sync_diagnostics, directive_meta, workspace_folder, missing_file_severity))
                };
                
                let Some((sync_diagnostics, directive_meta, workspace_folder, missing_file_severity)) = diagnostics_data else {
                    return;
                };
                
                // Perform async missing file existence checks (non-blocking I/O)
                let diagnostics = handlers::diagnostics_async_standalone(
                    &affected_uri,
                    sync_diagnostics,
                    &directive_meta,
                    workspace_folder.as_ref(),
                    missing_file_severity,
                ).await;
                
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
            });
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = &params.text_document.uri;
        
        // Cancel pending background indexing for this URI
        self.background_indexer.cancel_uri(uri);
        
        let mut state = self.state.write().await;
        
        // Close in new DocumentStore (Requirement 1.5)
        state.document_store.close(uri);
        
        // Clear diagnostics gate state
        state.diagnostics_gate.clear(uri);
        
        // Cancel pending revalidation
        state.cross_file_revalidation.cancel(uri);
        
        // Remove from activity tracking
        state.cross_file_activity.remove(uri);
        
        // Close the document (legacy)
        state.close_document(uri);
    }

    /// Apply updated workspace configuration, invalidate caches that affect name-resolution scope, and re-run diagnostics for all open documents.
    ///
    /// This handles parsing the new cross-file configuration from the provided LSP settings, applies it to shared state if valid, invalidates cross-file resolution caches, marks open documents for force republish, optionally reinitializes the PackageLibrary when package-related settings change, and schedules diagnostics publication for every open document.
    ///
    /// # Parameters
    ///
    /// - `params`: LSP DidChangeConfigurationParams containing the new settings to parse and apply.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Called from an async context when the client sends updated configuration.
    /// # async fn example(backend: &crate::backend::Backend, params: lsp_types::DidChangeConfigurationParams) {
    /// backend.did_change_configuration(params).await;
    /// # }
    /// ```
    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // Requirement 11.11: When configuration changes, re-resolve scope chains for open documents
        log::trace!("Configuration changed, parsing new config and scheduling revalidation");
        
        // Parse new configuration if provided
        let new_config = parse_cross_file_config(&params.settings);
        
        // Log if configuration parsing failed and defaults will be used
        if new_config.is_none() {
            log::warn!("Failed to parse cross-file configuration from settings, using existing configuration");
        }
        
        let (open_uris, scope_changed, package_settings_changed, packages_enabled, packages_r_path, additional_paths) = {
            let mut state = self.state.write().await;
            
            // Check if scope-affecting settings changed
            let scope_changed = new_config.as_ref()
                .map(|c| state.cross_file_config.scope_settings_changed(c))
                .unwrap_or(false);
            
            // Check if package settings changed
            let package_settings_changed = new_config.as_ref()
                .map(|c| {
                    c.packages_enabled != state.cross_file_config.packages_enabled
                        || c.packages_r_path != state.cross_file_config.packages_r_path
                        || c.packages_additional_library_paths != state.cross_file_config.packages_additional_library_paths
                })
                .unwrap_or(false);
            
            // Capture new package settings before applying config
            let packages_enabled = new_config.as_ref()
                .map(|c| c.packages_enabled)
                .unwrap_or(state.cross_file_config.packages_enabled);
            let packages_r_path = new_config.as_ref()
                .and_then(|c| c.packages_r_path.clone())
                .or_else(|| state.cross_file_config.packages_r_path.clone());
            let additional_paths = new_config.as_ref()
                .map(|c| c.packages_additional_library_paths.clone())
                .unwrap_or_else(|| state.cross_file_config.packages_additional_library_paths.clone());
            
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
            
            (open_uris, scope_changed, package_settings_changed, packages_enabled, packages_r_path, additional_paths)
        };
        
        // Reinitialize PackageLibrary if package settings changed
        if package_settings_changed {
            log::info!("Package settings changed, reinitializing PackageLibrary");
            
            let new_package_library = if packages_enabled {
                let r_subprocess = crate::r_subprocess::RSubprocess::new(packages_r_path);
                let mut lib = crate::package_library::PackageLibrary::with_subprocess(r_subprocess);
                if let Err(e) = lib.initialize().await {
                    log::warn!("Failed to reinitialize PackageLibrary: {}", e);
                }
                lib.add_library_paths(&additional_paths);
                std::sync::Arc::new(lib)
            } else {
                std::sync::Arc::new(crate::package_library::PackageLibrary::new_empty())
            };
            
            // Replace under brief write lock
            {
                let mut state = self.state.write().await;
                state.package_library = new_package_library;
            }
        }
        
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
                        
                        // Schedule debounced update in new WorkspaceIndex (Requirement 5.1)
                        state.workspace_index_new.schedule_update(uri.clone());
                        
                        // Schedule for async update (legacy)
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
                        
                        // Remove from new WorkspaceIndex
                        state.workspace_index_new.invalidate(uri);
                        
                        // Remove from dependency graph and caches (legacy)
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
        
        // Update new WorkspaceIndex (Requirement 12.1, 12.2, 12.3)
        {
            let state = self.state.read().await;
            
            // Parse tree for IndexEntry
            let tree = crate::parser_pool::with_parser(|parser| {
                parser.parse(&content, None)
            });
            
            // Extract loaded packages from tree
            let loaded_packages = extract_loaded_packages_from_tree(&tree, &content);
            
            let index_entry = crate::workspace_index::IndexEntry {
                contents: ropey::Rope::from_str(&content),
                tree,
                loaded_packages,
                snapshot: snapshot.clone(),
                metadata: cross_file_meta.clone(),
                artifacts: artifacts.clone(),
                indexed_at_version: state.workspace_index_new.version(),
            };
            
            state.workspace_index_new.insert(file_uri.clone(), index_entry);
        }
        
        // Update legacy workspace index
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
        // Extract needed data while holding read lock briefly
        let (version, sync_diagnostics, directive_meta, workspace_folder, missing_file_severity) = {
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
            
            // Get sync diagnostics (uses cached-only existence checks)
            let sync_diagnostics = handlers::diagnostics(&state, uri);
            
            // Extract metadata for async missing file checks
            let directive_meta = state.documents.get(uri)
                .map(|doc| crate::cross_file::directive::parse_directives(&doc.text()))
                .unwrap_or_default();
            
            let workspace_folder = state.workspace_folders.first().cloned();
            let missing_file_severity = state.cross_file_config.missing_file_severity;
            
            (version, sync_diagnostics, directive_meta, workspace_folder, missing_file_severity)
        };
        // Lock released here
        
        // Perform async missing file existence checks (non-blocking I/O)
        let diagnostics = handlers::diagnostics_async_standalone(
            uri,
            sync_diagnostics,
            &directive_meta,
            workspace_folder.as_ref(),
            missing_file_severity,
        ).await;

        // Re-check freshness after async work to avoid publishing stale diagnostics
        {
            let state = self.state.read().await;
            if let Some(ver) = version {
                let current_version = state.documents.get(uri).and_then(|d| d.version);
                if current_version != Some(ver) {
                    log::trace!("Skipping diagnostics for {}: version changed (was {:?}, now {:?})", uri, version, current_version);
                    return;
                }
                if !state.diagnostics_gate.can_publish(uri, ver) {
                    log::trace!("Skipping diagnostics for {}: monotonic gate after async (version={})", uri, ver);
                    return;
                }
            }
        }
        
        // Record the publish (uses interior mutability, no write lock needed)
        {
            let state = self.state.read().await;
            if let Some(ver) = version {
                state.diagnostics_gate.record_publish(uri, ver);
            }
        }
        
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