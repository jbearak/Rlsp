//
// backend.rs
//
// Copyright (C) 2025 Posit Software, PBC. All rights reserved.
//
//

use std::sync::Arc;

use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::Client;
use tower_lsp::LanguageServer;
use tower_lsp::LspService;
use tower_lsp::Server;

use crate::handlers;
use crate::r_env;
use crate::state::WorldState;

pub struct Backend {
    client: Client,
    state: Arc<RwLock<WorldState>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let library_paths = r_env::find_library_paths();
        log::info!("Discovered R library paths: {:?}", library_paths);

        Self {
            client,
            state: Arc::new(RwLock::new(WorldState::new(library_paths))),
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
        
        let mut state = self.state.write().await;
        state.index_workspace();
    }

    async fn shutdown(&self) -> Result<()> {
        log::info!("ark-lsp shutting down");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        let version = params.text_document.version;

        let mut state = self.state.write().await;
        state.open_document(uri.clone(), &text, Some(version));
        // Record as recently opened for activity prioritization
        state.cross_file_activity.record_recent(uri.clone());
        drop(state);

        self.publish_diagnostics(&uri).await;
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
            
            // Compute affected files from dependency graph
            let mut affected: Vec<Url> = vec![uri.clone()];
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
                    affected.push(dep);
                }
            }
            
            // Prioritize by activity (trigger first, then active, then visible, then recent)
            let activity = &state.cross_file_activity;
            affected.sort_by_key(|u| {
                if *u == uri { 0 }
                else { activity.priority_score(u) + 1 }
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

    async fn did_change_configuration(&self, _params: DidChangeConfigurationParams) {
        // Requirement 11.11: When configuration changes, re-resolve scope chains for open documents
        log::trace!("Configuration changed, invalidating caches and scheduling revalidation");
        
        let open_uris: Vec<Url> = {
            let mut state = self.state.write().await;
            
            // Invalidate all scope caches since config affects resolution
            state.cross_file_cache.invalidate_all();
            
            // Get list of open documents
            state.documents.keys().cloned().collect()
        };
        
        // Schedule diagnostics for all open documents
        for uri in open_uris {
            self.publish_diagnostics(&uri).await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        log::trace!("Received watched files change: {} changes", params.changes.len());
        
        let mut state = self.state.write().await;
        for change in params.changes {
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
                    log::trace!("Invalidated caches for changed file: {}", uri);
                }
                FileChangeType::DELETED => {
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
    async fn publish_diagnostics(&self, uri: &Url) {
        let state = self.state.read().await;
        let version = state.documents.get(uri).and_then(|d| d.version);
        
        // Check if we can publish (monotonic gate)
        if let Some(ver) = version {
            if !state.diagnostics_gate.can_publish(uri, ver) {
                log::trace!("Skipping diagnostics for {}: monotonic gate", uri);
                return;
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
}

pub async fn start_lsp() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;

    Ok(())
}
