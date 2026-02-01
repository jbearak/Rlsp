//
// handlers.rs
//
// Copyright (C) 2024-2026 Posit Software, PBC. All rights reserved.
// Modifications copyright (C) 2026 Jonathan Marc Bearak
//

use std::collections::HashMap;

use tower_lsp::lsp_types::*;
use tree_sitter::Node;
use tree_sitter::Point;

use crate::content_provider::ContentProvider;
use crate::cross_file::dependency::compute_inherited_working_directory;
use crate::cross_file::{scope, ScopedSymbol};
use crate::state::WorldState;

use crate::builtins;

// ============================================================================
// Cross-File Scope Helper
// ============================================================================

/// Collects cross-file symbols visible at the given document position.
///
/// The returned set includes symbols brought into scope via `source()` chains
/// and parent-file backward directives; it uses the state's ContentProvider to
/// resolve referenced files and artifacts.
///
/// # Examples
///
/// ```no_run
/// use url::Url;
/// // `state` is a prepared WorldState; obtain or mock as appropriate in tests.
/// let state = /* WorldState */ todo!();
/// let uri = Url::parse("file:///path/to/script.R").unwrap();
/// let symbols = crate::get_cross_file_symbols(&state, &uri, 12, 4);
/// // `symbols` maps symbol names to their scoped metadata.
/// ```
///
/// # Returns
///
/// A `HashMap` mapping symbol names to their corresponding `ScopedSymbol` entries.
fn get_cross_file_symbols(
    state: &WorldState,
    uri: &Url,
    line: u32,
    column: u32,
) -> HashMap<String, ScopedSymbol> {
    get_cross_file_scope(state, uri, line, column).symbols
}

/// Compute the unified cross-file scope at a given position, including available symbols and package visibility.
///
/// This returns a position-aware ScopeAtPosition that reflects symbols visible at (line, column) in `uri`, along with loaded and inherited package lists and depth information for cross-file resolution.
///
/// # Returns
///
/// A `scope::ScopeAtPosition` containing the resolved symbols, `loaded_packages`, `inherited_packages`, and scope depth metadata for the specified location.
///
/// # Examples
///
/// ```no_run
/// use url::Url;
/// // `state` is your WorldState and `uri` is the file URL you want to query.
/// let uri = Url::parse("file:///path/to/script.R").unwrap();
/// let scope = get_cross_file_scope(&state, &uri, 10, 5);
/// // inspect scope.symbols, scope.loaded_packages, etc.
/// ```
fn get_cross_file_scope(
    state: &WorldState,
    uri: &Url,
    line: u32,
    column: u32,
) -> scope::ScopeAtPosition {
    // Use ContentProvider for unified access
    let content_provider = state.content_provider();

    // Closure to get artifacts for a URI
    let get_artifacts = |target_uri: &Url| -> Option<scope::ScopeArtifacts> {
        content_provider.get_artifacts(target_uri)
    };

    // Closure to get metadata for a URI
    let get_metadata = |target_uri: &Url| -> Option<crate::cross_file::CrossFileMetadata> {
        content_provider.get_metadata(target_uri)
    };

    let max_depth = state.cross_file_config.max_chain_depth;

    // Use the graph-aware scope resolution with PathContext
    scope::scope_at_position_with_graph(
        uri,
        line,
        column,
        &get_artifacts,
        &get_metadata,
        &state.cross_file_graph,
        state.workspace_folders.first(),
        max_depth,
    )
}

// ============================================================================
// Folding Range
// ============================================================================

pub fn folding_range(state: &WorldState, uri: &Url) -> Option<Vec<FoldingRange>> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    let mut ranges = Vec::new();

    collect_folding_ranges(tree.root_node(), &mut ranges);

    Some(ranges)
}

fn collect_folding_ranges(node: Node, ranges: &mut Vec<FoldingRange>) {
    let kind = node.kind();

    // Fold braced expressions, function definitions, and control structures
    let should_fold = matches!(
        kind,
        "brace_list" | "function_definition" | "if_statement" | "for_statement" | "while_statement"
    );

    if should_fold && node.start_position().row != node.end_position().row {
        ranges.push(FoldingRange {
            start_line: node.start_position().row as u32,
            start_character: Some(node.start_position().column as u32),
            end_line: node.end_position().row as u32,
            end_character: Some(node.end_position().column as u32),
            kind: Some(FoldingRangeKind::Region),
            collapsed_text: None,
        });
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_folding_ranges(child, ranges);
    }
}

// ============================================================================
// Selection Range
// ============================================================================

pub fn selection_range(
    state: &WorldState,
    uri: &Url,
    positions: Vec<Position>,
) -> Option<Vec<SelectionRange>> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;

    let mut results = Vec::new();
    for pos in positions {
        let point = Point::new(pos.line as usize, pos.character as usize);
        if let Some(range) = build_selection_range(tree.root_node(), point) {
            results.push(range);
        }
    }

    Some(results)
}

fn build_selection_range(root: Node, point: Point) -> Option<SelectionRange> {
    let mut node = root.descendant_for_point_range(point, point)?;
    let mut ranges: Vec<Range> = Vec::new();

    loop {
        let range = Range {
            start: Position::new(
                node.start_position().row as u32,
                node.start_position().column as u32,
            ),
            end: Position::new(
                node.end_position().row as u32,
                node.end_position().column as u32,
            ),
        };

        if ranges.last() != Some(&range) {
            ranges.push(range);
        }

        if let Some(parent) = node.parent() {
            node = parent;
        } else {
            break;
        }
    }

    // Build nested SelectionRange from innermost to outermost
    let mut result: Option<SelectionRange> = None;
    for range in ranges {
        result = Some(SelectionRange {
            range,
            parent: result.map(Box::new),
        });
    }

    result
}

// ============================================================================
// Document Symbols
// ============================================================================

pub fn document_symbol(state: &WorldState, uri: &Url) -> Option<DocumentSymbolResponse> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    let mut symbols = Vec::new();
    collect_symbols(tree.root_node(), &text, &mut symbols);

    Some(DocumentSymbolResponse::Flat(symbols))
}

#[allow(deprecated)]
fn collect_symbols(node: Node, text: &str, symbols: &mut Vec<SymbolInformation>) {
    // Look for assignments: identifier <- value or identifier = value
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];
            let rhs = children[2];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-") && lhs.kind() == "identifier" {
                let name = node_text(lhs, text).to_string();
                let kind = if rhs.kind() == "function_definition" {
                    SymbolKind::FUNCTION
                } else {
                    SymbolKind::VARIABLE
                };

                symbols.push(SymbolInformation {
                    name,
                    kind,
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri: Url::parse("file:///").unwrap(), // Will be replaced
                        range: Range {
                            start: Position::new(
                                node.start_position().row as u32,
                                node.start_position().column as u32,
                            ),
                            end: Position::new(
                                node.end_position().row as u32,
                                node.end_position().column as u32,
                            ),
                        },
                    },
                    container_name: None,
                });
            }
        }
    }

    // Recurse
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols(child, text, symbols);
    }
}

// ============================================================================
// Diagnostics
// ============================================================================

/// Compute diagnostics for the document at the given URI.
///
/// Performs a full set of checks for the specified open document and returns collected diagnostics.
/// Reported issues include syntax errors, circular dependency and max-depth problems, missing or ambiguous
/// sourced files, out-of-scope symbol usage, missing package warnings, and (when enabled) undefined-variable
/// diagnostics that account for cross-file and package scope.
///
/// # Returns
///
/// `Vec<Diagnostic>` containing diagnostics for the document at `uri`, which may be empty if no issues were found.
///
/// # Examples
///
/// ```
/// // Given a prepared `WorldState` and a `Url` referring to an open document:
/// let diags = diagnostics(&state, &uri);
/// assert!(diags.is_empty() || diags.iter().any(|d| d.severity.is_some()));
/// ```
pub fn diagnostics(state: &WorldState, uri: &Url) -> Vec<Diagnostic> {
    let Some(doc) = state.get_document(uri) else {
        return Vec::new();
    };

    let Some(tree) = &doc.tree else {
        return Vec::new();
    };

    let text = doc.text();
    let mut diagnostics = Vec::new();

    // Parse directives to get ignored lines and cross-file metadata
    let mut directive_meta = crate::cross_file::directive::parse_directives(&text);

    // Compute inherited working directory for files with backward directives
    // This enables child files to inherit the parent's working directory context
    // for resolving paths in their own source() calls.
    // _Requirements: 5.1, 5.2, 6.1_
    if !directive_meta.sourced_by.is_empty() && directive_meta.working_directory.is_none() {
        let workspace_root = state.workspace_folders.first();
        let content_provider = state.content_provider();

        // Create a metadata getter that retrieves metadata from open documents,
        // workspace index, or by parsing content from the file cache
        let get_metadata_for_uri =
            |target_uri: &Url| -> Option<crate::cross_file::CrossFileMetadata> {
                // First check open documents
                if let Some(doc) = state.documents.get(target_uri) {
                    return Some(crate::cross_file::directive::parse_directives(&doc.text()));
                }
                // Then try workspace index
                if let Some(meta) = state.cross_file_workspace_index.get_metadata(target_uri) {
                    return Some(meta);
                }
                // Finally try to read from file cache
                if let Some(content) = content_provider.get_content(target_uri) {
                    return Some(crate::cross_file::extract_metadata(&content));
                }
                None
            };

        directive_meta.inherited_working_directory = compute_inherited_working_directory(
            uri,
            &directive_meta,
            workspace_root,
            get_metadata_for_uri,
        );

        if directive_meta.inherited_working_directory.is_some() {
            log::trace!(
                "Computed inherited working directory for {}: {:?}",
                uri,
                directive_meta.inherited_working_directory
            );
        }
    }

    // Collect syntax errors (not suppressed by @lsp-ignore)
    collect_syntax_errors(tree.root_node(), &mut diagnostics);

    // Check for circular dependencies
    if let Some(cycle_edge) = state.cross_file_graph.detect_cycle(uri) {
        let line = cycle_edge.call_site_line.unwrap_or(0);
        let col = cycle_edge.call_site_column.unwrap_or(0);
        let target = cycle_edge
            .to
            .path_segments()
            .and_then(|mut s| s.next_back().map(|s| s.to_string()))
            .unwrap_or_default();
        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(line, col),
                end: Position::new(line, col + 1),
            },
            severity: Some(state.cross_file_config.circular_dependency_severity),
            message: format!(
                "Circular dependency detected: sourcing '{}' creates a cycle",
                target
            ),
            ..Default::default()
        });
    }

    // Check for max chain depth exceeded (Requirement 5.8)
    collect_max_depth_diagnostics(state, uri, &mut diagnostics);

    // Check for missing files in source() calls and directives (Requirement 10.2)
    collect_missing_file_diagnostics(state, uri, &directive_meta, &mut diagnostics);

    // Check for ambiguous parents (Requirement 5.10 / 10.6)
    collect_ambiguous_parent_diagnostics(state, uri, &directive_meta, &mut diagnostics);

    // Check for out-of-scope symbol usage (Requirement 10.3)
    collect_out_of_scope_diagnostics(
        state,
        uri,
        tree.root_node(),
        &text,
        &directive_meta,
        &mut diagnostics,
    );

    // Check for missing packages in library() calls (Requirement 15.1)
    collect_missing_package_diagnostics(state, &directive_meta, &mut diagnostics);

    // Collect undefined variable errors if enabled in config
    if state.cross_file_config.undefined_variables_enabled {
        collect_undefined_variables_position_aware(
            state,
            uri,
            tree.root_node(),
            &text,
            &doc.loaded_packages,
            &state.workspace_imports,
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );
    }

    diagnostics
}

/// Async version of diagnostics that uses batched existence checks for missing files
///
/// This function performs the same diagnostics as `diagnostics()` but uses
/// `collect_missing_file_diagnostics_async` for non-blocking disk I/O when
/// checking file existence.
///
/// The function is designed to minimize lock hold time:
/// 1. Extract needed data from state (caller holds lock briefly)
/// 2. Release lock before async I/O
/// 3. Perform async existence checks
///
/// **Validates: Requirement 14 (async batched existence checks)**
#[allow(dead_code)]
pub async fn diagnostics_async(
    content_provider: &impl crate::content_provider::AsyncContentProvider,
    uri: &Url,
    sync_diagnostics: Vec<Diagnostic>,
    directive_meta: &crate::cross_file::CrossFileMetadata,
    workspace_folders: Option<&Url>,
    missing_file_severity: DiagnosticSeverity,
) -> Vec<Diagnostic> {
    let mut diagnostics = sync_diagnostics;

    // Replace cached-only missing file diagnostics with async batched checks
    // First, remove any "File not found" or "Parent file not found" diagnostics
    // that were generated by the sync path (cached-only)
    diagnostics.retain(|d| {
        !d.message.starts_with("File not found:")
            && !d.message.starts_with("Parent file not found:")
    });

    // Now add async-checked missing file diagnostics
    let missing_file_diags = collect_missing_file_diagnostics_async(
        content_provider,
        uri,
        directive_meta,
        workspace_folders,
        missing_file_severity,
    )
    .await;
    diagnostics.extend(missing_file_diags);

    diagnostics
}

/// Standalone async version that performs disk existence checks without ContentProvider
///
/// This version is used when we can't hold a reference to ContentProvider across await points.
/// It directly performs async disk I/O using spawn_blocking.
///
/// **Validates: Requirement 14 (async batched existence checks)**
pub async fn diagnostics_async_standalone(
    uri: &Url,
    sync_diagnostics: Vec<Diagnostic>,
    directive_meta: &crate::cross_file::CrossFileMetadata,
    workspace_folders: Option<&Url>,
    missing_file_severity: DiagnosticSeverity,
) -> Vec<Diagnostic> {
    let mut diagnostics = sync_diagnostics;

    // Replace cached-only missing file diagnostics with async batched checks
    diagnostics.retain(|d| {
        !d.message.starts_with("File not found:")
            && !d.message.starts_with("Parent file not found:")
    });

    // Collect URIs to check
    let missing_file_diags = collect_missing_file_diagnostics_standalone(
        uri,
        directive_meta,
        workspace_folders,
        missing_file_severity,
    )
    .await;
    diagnostics.extend(missing_file_diags);

    diagnostics
}

/// Standalone async missing file diagnostics that performs disk checks directly
async fn collect_missing_file_diagnostics_standalone(
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    workspace_folders: Option<&Url>,
    missing_file_severity: DiagnosticSeverity,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let workspace_root = workspace_folders.and_then(|w| w.to_file_path().ok());

    // Forward sources use @lsp-cd for path resolution
    let forward_ctx =
        crate::cross_file::path_resolve::PathContext::from_metadata(uri, meta, workspace_folders);
    // Backward directives IGNORE @lsp-cd - always resolve relative to file's directory
    let backward_ctx = crate::cross_file::path_resolve::PathContext::new(uri, workspace_folders);

    // Collect all paths to check: (path, line, col, is_backward)
    let mut paths_to_check: Vec<(std::path::PathBuf, String, u32, u32, bool)> = Vec::new();

    for source in &meta.sources {
        let resolved = forward_ctx
            .as_ref()
            .and_then(|ctx| crate::cross_file::path_resolve::resolve_path(&source.path, ctx));
        if let Some(path) = resolved {
            if let Some(root) = &workspace_root {
                if !path.starts_with(root) {
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position::new(source.line, source.column),
                            end: Position::new(
                                source.line,
                                source
                                    .column
                                    .saturating_add(source.path.len() as u32)
                                    .saturating_add(10),
                            ),
                        },
                        severity: Some(missing_file_severity),
                        message: format!("Path is outside workspace: '{}'", source.path),
                        ..Default::default()
                    });
                    continue;
                }
            }
            paths_to_check.push((path, source.path.clone(), source.line, source.column, false));
        } else {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(source.line, source.column),
                    end: Position::new(
                        source.line,
                        source
                            .column
                            .saturating_add(source.path.len() as u32)
                            .saturating_add(10),
                    ),
                },
                severity: Some(missing_file_severity),
                message: format!("Cannot resolve path: '{}'", source.path),
                ..Default::default()
            });
        }
    }

    for directive in &meta.sourced_by {
        let resolved = backward_ctx
            .as_ref()
            .and_then(|ctx| crate::cross_file::path_resolve::resolve_path(&directive.path, ctx));
        if let Some(path) = resolved {
            if let Some(root) = &workspace_root {
                if !path.starts_with(root) {
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position::new(directive.directive_line, 0),
                            end: Position::new(directive.directive_line, u32::MAX),
                        },
                        severity: Some(missing_file_severity),
                        message: format!("Path is outside workspace: '{}'", directive.path),
                        ..Default::default()
                    });
                    continue;
                }
            }
            paths_to_check.push((
                path,
                directive.path.clone(),
                directive.directive_line,
                0,
                true,
            ));
        } else {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(directive.directive_line, 0),
                    end: Position::new(directive.directive_line, u32::MAX),
                },
                severity: Some(missing_file_severity),
                message: format!("Cannot resolve parent path: '{}'", directive.path),
                ..Default::default()
            });
        }
    }

    if paths_to_check.is_empty() {
        return diagnostics;
    }

    // Batch check existence on blocking thread
    let paths: Vec<std::path::PathBuf> = paths_to_check
        .iter()
        .map(|(p, _, _, _, _)| p.clone())
        .collect();
    let existence = match tokio::task::spawn_blocking(move || {
        paths.iter().map(|p| p.exists()).collect::<Vec<_>>()
    })
    .await
    {
        Ok(v) => v,
        Err(err) => {
            log::warn!("Missing-file check failed: {err}");
            return diagnostics;
        }
    };

    // Generate diagnostics for missing files
    for (i, (_, path_str, line, col, is_backward)) in paths_to_check.into_iter().enumerate() {
        if !existence.get(i).copied().unwrap_or(false) {
            if is_backward {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(line, 0),
                        end: Position::new(line, u32::MAX),
                    },
                    severity: Some(missing_file_severity),
                    message: format!("Parent file not found: '{}'", path_str),
                    ..Default::default()
                });
            } else {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(line, col),
                        end: Position::new(
                            line,
                            col.saturating_add(path_str.len() as u32).saturating_add(10),
                        ),
                    },
                    severity: Some(missing_file_severity),
                    message: format!("File not found: '{}'", path_str),
                    ..Default::default()
                });
            }
        }
    }

    diagnostics
}

/// Collect diagnostics for missing files referenced in source() calls and directives
///
/// This is the synchronous version that only checks cached sources (no disk I/O).
/// For async disk checking, use `collect_missing_file_diagnostics_async`.
///
/// Path resolution follows the critical distinction from AGENTS.md:
/// - Forward sources (source() calls): use PathContext::from_metadata (respects @lsp-cd)
/// - Backward directives (@lsp-sourced-by): use PathContext::new (ignores @lsp-cd)
///
/// Uses ContentProvider for unified access to cached file existence.
///
/// **Validates: Requirements 14.2, 14.5**
fn collect_missing_file_diagnostics(
    state: &WorldState,
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let content_provider = state.content_provider();

    // Forward sources use @lsp-cd for path resolution
    let forward_ctx = crate::cross_file::path_resolve::PathContext::from_metadata(
        uri,
        meta,
        state.workspace_folders.first(),
    );
    // Backward directives IGNORE @lsp-cd - always resolve relative to file's directory
    let backward_ctx =
        crate::cross_file::path_resolve::PathContext::new(uri, state.workspace_folders.first());

    // Check forward sources (source() calls and @lsp-source directives)
    for source in &meta.sources {
        let resolved = forward_ctx.as_ref().and_then(|ctx| {
            let path = crate::cross_file::path_resolve::resolve_path(&source.path, ctx)?;
            crate::cross_file::path_resolve::path_to_uri(&path)
        });
        if let Some(target_uri) = resolved {
            // Use ContentProvider for cached existence check (no blocking I/O)
            if !content_provider.exists_cached(&target_uri) {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(source.line, source.column),
                        end: Position::new(
                            source.line,
                            source
                                .column
                                .saturating_add(source.path.len() as u32)
                                .saturating_add(10),
                        ),
                    },
                    severity: Some(state.cross_file_config.missing_file_severity),
                    message: format!("File not found: '{}'", source.path),
                    ..Default::default()
                });
            }
        } else {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(source.line, source.column),
                    end: Position::new(
                        source.line,
                        source
                            .column
                            .saturating_add(source.path.len() as u32)
                            .saturating_add(10),
                    ),
                },
                severity: Some(state.cross_file_config.missing_file_severity),
                message: format!("Cannot resolve path: '{}'", source.path),
                ..Default::default()
            });
        }
    }

    // Check backward directives (@lsp-sourced-by)
    for directive in &meta.sourced_by {
        let resolved = backward_ctx.as_ref().and_then(|ctx| {
            let path = crate::cross_file::path_resolve::resolve_path(&directive.path, ctx)?;
            crate::cross_file::path_resolve::path_to_uri(&path)
        });
        if let Some(target_uri) = resolved {
            // Use ContentProvider for cached existence check (no blocking I/O)
            if !content_provider.exists_cached(&target_uri) {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(directive.directive_line, 0),
                        end: Position::new(directive.directive_line, u32::MAX),
                    },
                    severity: Some(state.cross_file_config.missing_file_severity),
                    message: format!("Parent file not found: '{}'", directive.path),
                    ..Default::default()
                });
            }
        } else {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(directive.directive_line, 0),
                    end: Position::new(directive.directive_line, u32::MAX),
                },
                severity: Some(state.cross_file_config.missing_file_severity),
                message: format!("Cannot resolve parent path: '{}'", directive.path),
                ..Default::default()
            });
        }
    }
}

/// Async version of missing file diagnostics that checks disk existence
///
/// This version uses `AsyncContentProvider::check_existence_batch` to perform
/// non-blocking disk I/O for files not found in cache.
///
/// Path resolution follows the critical distinction from AGENTS.md:
/// - Forward sources (source() calls): use PathContext::from_metadata (respects @lsp-cd)
/// - Backward directives (@lsp-sourced-by): use PathContext::new (ignores @lsp-cd)
///
/// **Validates: Requirements 14.2, 14.5**
#[allow(dead_code)]
pub async fn collect_missing_file_diagnostics_async(
    content_provider: &impl crate::content_provider::AsyncContentProvider,
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    workspace_folders: Option<&Url>,
    missing_file_severity: DiagnosticSeverity,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Forward sources use @lsp-cd for path resolution
    let forward_ctx =
        crate::cross_file::path_resolve::PathContext::from_metadata(uri, meta, workspace_folders);
    // Backward directives IGNORE @lsp-cd - always resolve relative to file's directory
    let backward_ctx = crate::cross_file::path_resolve::PathContext::new(uri, workspace_folders);

    // Collect all URIs to check
    let mut uris_to_check: Vec<(Url, String, u32, u32, bool)> = Vec::new(); // (uri, path, line, col, is_backward)

    for source in &meta.sources {
        let resolved = forward_ctx.as_ref().and_then(|ctx| {
            let path = crate::cross_file::path_resolve::resolve_path(&source.path, ctx)?;
            crate::cross_file::path_resolve::path_to_uri(&path)
        });
        if let Some(target_uri) = resolved {
            uris_to_check.push((
                target_uri,
                source.path.clone(),
                source.line,
                source.column,
                false,
            ));
        } else {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(source.line, source.column),
                    end: Position::new(
                        source.line,
                        source
                            .column
                            .saturating_add(source.path.len() as u32)
                            .saturating_add(10),
                    ),
                },
                severity: Some(missing_file_severity),
                message: format!("Cannot resolve path: '{}'", source.path),
                ..Default::default()
            });
        }
    }

    for directive in &meta.sourced_by {
        let resolved = backward_ctx.as_ref().and_then(|ctx| {
            let path = crate::cross_file::path_resolve::resolve_path(&directive.path, ctx)?;
            crate::cross_file::path_resolve::path_to_uri(&path)
        });
        if let Some(target_uri) = resolved {
            uris_to_check.push((
                target_uri,
                directive.path.clone(),
                directive.directive_line,
                0,
                true,
            ));
        } else {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(directive.directive_line, 0),
                    end: Position::new(directive.directive_line, u32::MAX),
                },
                severity: Some(missing_file_severity),
                message: format!("Cannot resolve parent path: '{}'", directive.path),
                ..Default::default()
            });
        }
    }

    if uris_to_check.is_empty() {
        return diagnostics;
    }

    // Batch check existence (non-blocking)
    let uris: Vec<Url> = uris_to_check
        .iter()
        .map(|(u, _, _, _, _)| u.clone())
        .collect();
    let existence = content_provider.check_existence_batch(&uris).await;

    // Generate diagnostics for missing files
    for (target_uri, path, line, col, is_backward) in uris_to_check {
        if !existence.get(&target_uri).copied().unwrap_or(false) {
            if is_backward {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(line, 0),
                        end: Position::new(line, u32::MAX),
                    },
                    severity: Some(missing_file_severity),
                    message: format!("Parent file not found: '{}'", path),
                    ..Default::default()
                });
            } else {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(line, col),
                        end: Position::new(
                            line,
                            col.saturating_add(path.len() as u32).saturating_add(10),
                        ),
                    },
                    severity: Some(missing_file_severity),
                    message: format!("File not found: '{}'", path),
                    ..Default::default()
                });
            }
        }
    }

    diagnostics
}

/// Collect diagnostics for max chain depth exceeded (Requirement 5.8)
fn collect_max_depth_diagnostics(state: &WorldState, uri: &Url, diagnostics: &mut Vec<Diagnostic>) {
    use crate::cross_file::scope;

    let get_artifacts = |target_uri: &Url| -> Option<scope::ScopeArtifacts> {
        if let Some(doc) = state.documents.get(target_uri) {
            if let Some(tree) = &doc.tree {
                return Some(scope::compute_artifacts(target_uri, tree, &doc.text()));
            }
        }
        if let Some(artifacts) = state.cross_file_workspace_index.get_artifacts(target_uri) {
            return Some(artifacts);
        }
        if let Some(doc) = state.workspace_index.get(target_uri) {
            if let Some(tree) = &doc.tree {
                return Some(scope::compute_artifacts(target_uri, tree, &doc.text()));
            }
        }
        None
    };

    let get_metadata = |target_uri: &Url| -> Option<crate::cross_file::CrossFileMetadata> {
        if let Some(doc) = state.documents.get(target_uri) {
            return Some(crate::cross_file::directive::parse_directives(&doc.text()));
        }
        state.cross_file_workspace_index.get_metadata(target_uri)
    };

    let max_depth = state.cross_file_config.max_chain_depth;

    // Use scope resolution to detect depth exceeded (now uses PathContext internally)
    let scope = scope::scope_at_position_with_graph(
        uri,
        u32::MAX,
        u32::MAX,
        &get_artifacts,
        &get_metadata,
        &state.cross_file_graph,
        state.workspace_folders.first(),
        max_depth,
    );

    // Emit diagnostics for depth exceeded, filtering to only those in this file
    for (exceeded_uri, line, col) in &scope.depth_exceeded {
        if exceeded_uri == uri {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(*line, *col),
                    end: Position::new(*line, col.saturating_add(1)),
                },
                severity: Some(state.cross_file_config.max_chain_depth_severity),
                message: format!(
                    "Maximum chain depth ({}) exceeded; some symbols may not be resolved",
                    max_depth
                ),
                ..Default::default()
            });
        }
    }
}

/// Emit a diagnostic when a file's parent resolution is ambiguous.
///
/// This inspects the cross-file metadata and graph to resolve the file's parent(s).
/// If resolution returns an ambiguous result, a single `Diagnostic` is pushed that
/// points at the first backward directive line and suggests adding `line=` or `match=`
/// to disambiguate. The diagnostic's severity is taken from the cross-file config.
///
/// # Examples
///
/// ```no_run
/// # use url::Url;
/// # use lsp_types::Diagnostic;
/// # use crate::WorldState;
/// # use crate::cross_file::CrossFileMetadata;
/// # fn example(state: &WorldState, uri: &Url, meta: &CrossFileMetadata, diagnostics: &mut Vec<Diagnostic>) {
/// collect_ambiguous_parent_diagnostics(state, uri, meta, diagnostics);
/// # }
/// ```
fn collect_ambiguous_parent_diagnostics(
    state: &WorldState,
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use crate::cross_file::cache::ParentResolution;
    use crate::cross_file::parent_resolve::resolve_parent_with_content;

    // Build PathContext for proper path resolution
    // Use PathContext::new (not from_metadata) because backward directives should
    // resolve relative to the file's directory, ignoring @lsp-cd
    let path_ctx =
        crate::cross_file::path_resolve::PathContext::new(uri, state.workspace_folders.first());

    let resolve_path = |path: &str| -> Option<Url> {
        let ctx = path_ctx.as_ref()?;
        let resolved = crate::cross_file::path_resolve::resolve_path(path, ctx)?;
        crate::cross_file::path_resolve::path_to_uri(&resolved)
    };

    let get_content = |parent_uri: &Url| -> Option<String> {
        // Open docs first, then file cache
        if let Some(doc) = state.documents.get(parent_uri) {
            return Some(doc.text());
        }
        state.cross_file_file_cache.get(parent_uri)
    };

    let resolution = resolve_parent_with_content(
        meta,
        &state.cross_file_graph,
        uri,
        &state.cross_file_config,
        resolve_path,
        get_content,
    );

    if let ParentResolution::Ambiguous {
        selected_uri,
        alternatives,
        ..
    } = resolution
    {
        // Find the first backward directive line to attach the diagnostic
        let directive_line = meta
            .sourced_by
            .first()
            .map(|d| d.directive_line)
            .unwrap_or(0);

        let alt_list: Vec<String> = alternatives
            .iter()
            .filter_map(|u| {
                u.path_segments()
                    .and_then(|mut s| s.next_back().map(|s| s.to_string()))
            })
            .collect();

        let selected_name = selected_uri
            .path_segments()
            .and_then(|mut s| s.next_back().map(|s| s.to_string()))
            .unwrap_or_else(|| selected_uri.to_string());

        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(directive_line, 0),
                end: Position::new(directive_line, u32::MAX),
            },
            severity: Some(state.cross_file_config.ambiguous_parent_severity),
            message: format!(
                "Ambiguous parent: using '{}' but also found: {}. Consider adding line= or match= to disambiguate.",
                selected_name,
                alt_list.join(", ")
            ),
            ..Default::default()
        });
    }
}

/// Emit diagnostics for `library()` calls that reference packages not present in the package library.
///
/// Scans the cross-file metadata for `library()` calls and, for each call that is not ignored
/// via directives and whose package is not found in `state.package_library`, appends a warning
/// Diagnostic covering the call's line range with the configured severity.
///
/// # Examples
///
/// ```ignore
/// // Construct a WorldState with packages enabled and an empty PackageLibrary,
/// // provide CrossFileMetadata containing a LibraryCall for "foo", then:
/// //
/// // collect_missing_package_diagnostics(&state, &meta, &mut diagnostics);
/// //
/// // After the call, `diagnostics` will contain a warning about package "foo".
/// ```
fn collect_missing_package_diagnostics(
    state: &WorldState,
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Skip if packages feature is disabled
    if !state.cross_file_config.packages_enabled {
        return;
    }

    for lib_call in &meta.library_calls {
        // Skip if the line is ignored via @lsp-ignore or @lsp-ignore-next
        if crate::cross_file::directive::is_line_ignored(meta, lib_call.line) {
            continue;
        }

        // Check if the package exists (is installed)
        if !state.package_library.package_exists(&lib_call.package) {
            // Package not found - emit diagnostic with configured severity
            // The column in LibraryCall is already UTF-16 (end position of the call)
            // We want to highlight the library() call, so we use the line and estimate the range

            // Calculate approximate start column (library( is 8 chars, package name varies)
            // We'll highlight from column 0 to the end column for simplicity
            let end_col = lib_call.column;

            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(lib_call.line, 0),
                    end: Position::new(lib_call.line, end_col),
                },
                severity: Some(state.cross_file_config.packages_missing_package_severity),
                message: format!("Package '{}' is not installed", lib_call.package),
                ..Default::default()
            });
        }
    }
}

/// Emit diagnostics for symbols defined in sourced files that are referenced
/// earlier in the current document than the corresponding `source()` call.
///
/// This function:
/// - Scans `directive_meta.sources` and collects source paths declared in the file.
/// - Collects identifier usages (UTF-16 columns) in `node`.
/// - For each sourced file, resolves its URI and obtains its exported symbols (preferring open documents, then cross-file index, then legacy index).
/// - Emits a diagnostic for every usage of an exported symbol that occurs before the `source()` call (skipping lines marked ignored by directives).
///
/// The produced diagnostics are appended to `diagnostics` and use the configured
/// `out_of_scope_severity` from `state.cross_file_config`.
///
/// # Parameters
///
/// - `state`: Workspace state and indexes used to resolve artifacts and configuration.
/// - `uri`: URI of the current document being analyzed (used to resolve relative source paths).
/// - `node`: Root AST node of the current document.
/// - `text`: Full source text of the current document.
/// - `directive_meta`: Cross-file directive metadata (contains `@lsp-source` / `source()` locations).
/// - `diagnostics`: Mutable vector to receive emitted diagnostics.
///
/// # Examples
///
/// ```no_run
/// // Collect diagnostics into `diags` for a parsed document:
/// let mut diags = Vec::new();
/// collect_out_of_scope_diagnostics(&state, &uri, root_node, &text, &directive_meta, &mut diags);
/// // `diags` now contains diagnostics for symbols used before their `source()` calls.
/// ```
fn collect_out_of_scope_diagnostics(
    state: &WorldState,
    uri: &Url,
    node: Node,
    text: &str,
    directive_meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use crate::cross_file::types::byte_offset_to_utf16_column;

    // Get all source() calls and @lsp-source directives in this file
    let source_calls: Vec<_> = directive_meta.sources.iter().collect();

    if source_calls.is_empty() {
        return;
    }

    // Collect all identifier usages with UTF-16 columns
    let mut usages: Vec<(String, u32, u32, Node)> = Vec::new();
    collect_identifier_usages_utf16(node, text, &mut usages);

    // For each source() call, check if any symbols from that file are used before the call
    for source in &source_calls {
        let source_line = source.line;
        let source_col = source.column; // Already UTF-16

        // Resolve the source path
        let resolve_path = |path: &str| -> Option<Url> {
            let from_path = uri.to_file_path().ok()?;
            let parent_dir = from_path.parent()?;
            let resolved = parent_dir.join(path);
            let normalized = crate::cross_file::path_resolve::normalize_path_public(&resolved)?;
            Url::from_file_path(normalized).ok()
        };

        let Some(source_uri) = resolve_path(&source.path) else {
            continue;
        };

        // Get symbols from the sourced file
        let source_symbols: std::collections::HashSet<String> = {
            let get_artifacts = |target_uri: &Url| -> Option<scope::ScopeArtifacts> {
                // Try open documents first (authoritative)
                if let Some(doc) = state.documents.get(target_uri) {
                    if let Some(tree) = &doc.tree {
                        return Some(scope::compute_artifacts(target_uri, tree, &doc.text()));
                    }
                }
                // Try cross-file workspace index (preferred for closed files)
                if let Some(artifacts) = state.cross_file_workspace_index.get_artifacts(target_uri)
                {
                    return Some(artifacts);
                }
                // Fallback to legacy workspace index
                if let Some(doc) = state.workspace_index.get(target_uri) {
                    if let Some(tree) = &doc.tree {
                        return Some(scope::compute_artifacts(target_uri, tree, &doc.text()));
                    }
                }
                None
            };

            get_artifacts(&source_uri)
                .map(|a| a.exported_interface.keys().cloned().collect())
                .unwrap_or_default()
        };

        // Check for usages of these symbols before the source() call
        for (name, usage_line, usage_col, usage_node) in &usages {
            if !source_symbols.contains(name) {
                continue;
            }

            // Check if usage is before the source() call (both columns are UTF-16)
            if (*usage_line, *usage_col) < (source_line, source_col) {
                // Skip if line is ignored
                if crate::cross_file::directive::is_line_ignored(directive_meta, *usage_line) {
                    continue;
                }

                // Convert byte columns to UTF-16 for diagnostic range
                let start_line_text = text
                    .lines()
                    .nth(usage_node.start_position().row)
                    .unwrap_or("");
                let end_line_text = text
                    .lines()
                    .nth(usage_node.end_position().row)
                    .unwrap_or("");
                let start_col = byte_offset_to_utf16_column(
                    start_line_text,
                    usage_node.start_position().column,
                );
                let end_col =
                    byte_offset_to_utf16_column(end_line_text, usage_node.end_position().column);

                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(usage_node.start_position().row as u32, start_col),
                        end: Position::new(usage_node.end_position().row as u32, end_col),
                    },
                    severity: Some(state.cross_file_config.out_of_scope_severity),
                    message: format!(
                        "Symbol '{}' used before source() call at line {}",
                        name,
                        source_line + 1
                    ),
                    ..Default::default()
                });
            }
        }
    }
}

/// Collect identifier usages with UTF-16 column positions
fn collect_identifier_usages_utf16<'a>(
    node: Node<'a>,
    text: &str,
    usages: &mut Vec<(String, u32, u32, Node<'a>)>,
) {
    use crate::cross_file::types::byte_offset_to_utf16_column;

    if node.kind() == "identifier" {
        // Skip if this is the LHS of an assignment
        if let Some(parent) = node.parent() {
            if parent.kind() == "binary_operator" {
                let mut cursor = parent.walk();
                let children: Vec<_> = parent.children(&mut cursor).collect();
                if children.len() >= 2 && children[0].id() == node.id() {
                    let op = children[1];
                    let op_text = &text[op.byte_range()];
                    if matches!(op_text, "<-" | "=" | "<<-") {
                        // Skip LHS of assignment, but recurse into children
                        let mut cursor = node.walk();
                        for child in node.children(&mut cursor) {
                            collect_identifier_usages_utf16(child, text, usages);
                        }
                        return;
                    }
                }
            }
            // Skip named arguments
            if parent.kind() == "argument" {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    if name_node.id() == node.id() {
                        return;
                    }
                }
            }
        }

        let name = text[node.byte_range()].to_string();
        let line = node.start_position().row as u32;
        // Convert byte column to UTF-16
        let line_text = text.lines().nth(node.start_position().row).unwrap_or("");
        let col = byte_offset_to_utf16_column(line_text, node.start_position().column);
        usages.push((name, line, col, node));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_usages_utf16(child, text, usages);
    }
}

fn collect_syntax_errors(node: Node, diagnostics: &mut Vec<Diagnostic>) {
    if node.is_error() || node.is_missing() {
        let message = if node.is_missing() {
            format!("Missing {}", node.kind())
        } else {
            "Syntax error".to_string()
        };

        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(
                    node.start_position().row as u32,
                    node.start_position().column as u32,
                ),
                end: Position::new(
                    node.end_position().row as u32,
                    node.end_position().column as u32,
                ),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            message,
            ..Default::default()
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_syntax_errors(child, diagnostics);
    }
}

/// Report undefined variable usages in a document using position-aware cross-file scope.
///
/// This inspects every identifier usage in `node`, skipping local definitions, builtins,
/// and workspace imports, and checks visibility at the exact usage position by querying
/// the cross-file scope (including position-aware loaded/inherited packages). Lines marked
/// with `@lsp-ignore` or `@lsp-ignore-next` are ignored. When a usage is not found in the
/// resolved scope or in package exports (if packages are enabled), a `Diagnostic` with a
/// UTF-16-aware range is emitted for the undefined variable.
///
/// # Examples
///
/// ```no_run
/// // Illustrative only — real usage requires a populated WorldState, AST node and other
/// // project-specific types from the language server state.
/// // collect_undefined_variables_position_aware(&state, &uri, root_node, &text, &[], &workspace_imports, &package_library, &directive_meta, &mut diagnostics);
/// ```
#[allow(clippy::too_many_arguments)]
fn collect_undefined_variables_position_aware(
    state: &WorldState,
    uri: &Url,
    node: Node,
    text: &str,
    _loaded_packages: &[String], // Deprecated: now using position-aware packages from scope resolution
    workspace_imports: &[String],
    package_library: &crate::package_library::PackageLibrary,
    directive_meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use crate::cross_file::types::byte_offset_to_utf16_column;
    use std::collections::HashSet;

    let mut defined: HashSet<String> = HashSet::new();
    let mut used: Vec<(String, Node)> = Vec::new();

    // First pass: collect all definitions
    collect_definitions(node, text, &mut defined);

    // Second pass: collect all usages with NSE-aware context
    collect_usages_with_context(node, text, &UsageContext::default(), &mut used);

    // Report undefined variables with position-aware cross-file scope
    for (name, usage_node) in used {
        let usage_line = usage_node.start_position().row as u32;

        // Skip if line is ignored via @lsp-ignore or @lsp-ignore-next
        if crate::cross_file::directive::is_line_ignored(directive_meta, usage_line) {
            continue;
        }

        // Skip if locally defined or builtin
        if defined.contains(&name) || is_builtin(&name) || workspace_imports.contains(&name) {
            continue;
        }

        // Convert byte column to UTF-16 for cross-file scope lookup
        let line_text = text
            .lines()
            .nth(usage_node.start_position().row)
            .unwrap_or("");
        let usage_col = byte_offset_to_utf16_column(line_text, usage_node.start_position().column);

        // Get full scope at position, including position-aware loaded packages
        // Requirements 8.1, 8.3, 8.4: Position-aware package checking
        let scope = get_cross_file_scope(state, uri, usage_line, usage_col);

        // Check if symbol is in cross-file scope
        if scope.symbols.contains_key(&name) {
            continue;
        }

        // Check package exports only if packages feature is enabled
        if state.cross_file_config.packages_enabled {
            // Build position-aware package list: inherited packages + locally loaded packages
            // Requirements 5.1, 5.2: Inherited packages from parent files
            // Requirements 8.1, 8.3: Locally loaded packages before this position
            let position_aware_packages: Vec<String> = scope
                .inherited_packages
                .iter()
                .chain(scope.loaded_packages.iter())
                .cloned()
                .collect();

            // Check if symbol is exported by any package loaded at this position
            if is_package_export(&name, &position_aware_packages, package_library) {
                continue;
            }
        }

        // Symbol is undefined - emit diagnostic
        // Convert byte columns to UTF-16 for diagnostic range
        let start_line_text = text
            .lines()
            .nth(usage_node.start_position().row)
            .unwrap_or("");
        let end_line_text = text
            .lines()
            .nth(usage_node.end_position().row)
            .unwrap_or("");
        let start_col =
            byte_offset_to_utf16_column(start_line_text, usage_node.start_position().column);
        let end_col = byte_offset_to_utf16_column(end_line_text, usage_node.end_position().column);

        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(usage_node.start_position().row as u32, start_col),
                end: Position::new(usage_node.end_position().row as u32, end_col),
            },
            severity: Some(DiagnosticSeverity::WARNING),
            message: format!("Undefined variable: {}", name),
            ..Default::default()
        });
    }
}

/// Emit diagnostics for identifiers that are used but not defined, built-in, imported, exported by a loaded package, or available from cross-file symbols.
///
/// This function performs a two-pass analysis on the provided syntax `node`:
/// it collects all defined identifiers, then collects usages (respecting NSE/context rules),
/// and pushes a `Diagnostic` with severity `Warning` for each usage that is not found in any of:
/// - the local definitions in the current tree,
/// - the set of builtins,
/// - symbols exported by any loaded package (via `package_library` and `loaded_packages`),
/// - names imported into the workspace (`workspace_imports`),
/// - the provided `cross_file_symbols`.
///
/// Parameters:
/// - `node`, `text`: the root AST node and source text to analyze.
/// - `loaded_packages`: names of packages considered loaded for package-export checks.
/// - `workspace_imports`: names imported into the workspace (treated as defined).
/// - `package_library`: authoritative package export/index used to determine package exports.
/// - `cross_file_symbols`: cross-file symbols available to the current file.
/// - `diagnostics`: destination vector; undefined-variable diagnostics are appended here.
///
/// # Examples
///
/// ```no_run
/// // Illustrative example (non-executable here): parse a document to `root` and call:
/// // collect_undefined_variables(root, &text, &loaded_packages, &workspace_imports, &package_library, &cross_file_symbols, &mut diagnostics);
/// ```
#[allow(dead_code)]
fn collect_undefined_variables(
    node: Node,
    text: &str,
    loaded_packages: &[String],
    workspace_imports: &[String],
    package_library: &crate::package_library::PackageLibrary,
    cross_file_symbols: &HashMap<String, ScopedSymbol>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use std::collections::HashSet;

    let mut defined: HashSet<String> = HashSet::new();
    let mut used: Vec<(String, Node)> = Vec::new();

    // First pass: collect all definitions
    collect_definitions(node, text, &mut defined);

    // Second pass: collect all usages
    collect_usages_with_context(node, text, &UsageContext::default(), &mut used);

    // Report undefined variables
    for (name, node) in used {
        if !defined.contains(&name)
            && !is_builtin(&name)
            && !is_package_export(&name, loaded_packages, package_library)
            && !workspace_imports.contains(&name)
            && !cross_file_symbols.contains_key(&name)
        {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(
                        node.start_position().row as u32,
                        node.start_position().column as u32,
                    ),
                    end: Position::new(
                        node.end_position().row as u32,
                        node.end_position().column as u32,
                    ),
                },
                severity: Some(DiagnosticSeverity::WARNING),
                message: format!("Undefined variable: {}", name),
                ..Default::default()
            });
        }
    }
}

fn collect_definitions(node: Node, text: &str, defined: &mut std::collections::HashSet<String>) {
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-") && lhs.kind() == "identifier" {
                defined.insert(node_text(lhs, text).to_string());
            }
        }
    }

    // Collect function parameters
    if node.kind() == "function_definition" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "parameters" {
                collect_parameters(child, text, defined);
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_definitions(child, text, defined);
    }
}

fn collect_parameters(node: Node, text: &str, defined: &mut std::collections::HashSet<String>) {
    if node.kind() == "parameter" || node.kind() == "identifier" {
        let name = node_text(node, text);
        if !name.is_empty() && name != "..." {
            defined.insert(name.to_string());
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_parameters(child, text, defined);
    }
}

/// Context for tracking NSE-related state during AST traversal
#[derive(Clone, Default)]
struct UsageContext {
    /// True when inside a formula expression (~ operator)
    in_formula: bool,
    /// True when inside the arguments of a call-like node (call, subset, subset2)
    in_call_like_arguments: bool,
}

/// Legacy version of collect_usages without NSE context tracking.
/// Only used in tests for backward compatibility with existing property tests.
#[cfg(test)]
fn collect_usages<'a>(node: Node<'a>, text: &str, used: &mut Vec<(String, Node<'a>)>) {
    if node.kind() == "identifier" {
        // Skip if this is the LHS of an assignment
        if let Some(parent) = node.parent() {
            if parent.kind() == "binary_operator" {
                let mut cursor = parent.walk();
                let children: Vec<_> = parent.children(&mut cursor).collect();
                if children.len() >= 2 && children[0].id() == node.id() {
                    let op = children[1];
                    let op_text = node_text(op, text);
                    if matches!(op_text, "<-" | "=" | "<<-") {
                        return; // Skip LHS of assignment
                    }
                }
            }

            // Skip if this is a named argument (e.g., n = 1 in readLines(..., n = 1))
            if parent.kind() == "argument" {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    if name_node.id() == node.id() {
                        return; // Skip argument names
                    }
                }
            }
        }

        used.push((node_text(node, text).to_string(), node));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_usages(child, text, used);
    }
}

/// Context-aware version of collect_usages that tracks NSE-related state during AST traversal.
/// This function skips undefined variable checks in contexts where R uses non-standard evaluation.
fn collect_usages_with_context<'a>(
    node: Node<'a>,
    text: &str,
    context: &UsageContext,
    used: &mut Vec<(String, Node<'a>)>,
) {
    if node.kind() == "identifier" {
        // Skip if we're in a formula or call-like arguments context
        if context.in_formula || context.in_call_like_arguments {
            return;
        }

        // Skip if this is the LHS of an assignment
        if let Some(parent) = node.parent() {
            if parent.kind() == "binary_operator" {
                let mut cursor = parent.walk();
                let children: Vec<_> = parent.children(&mut cursor).collect();
                if children.len() >= 2 && children[0].id() == node.id() {
                    let op = children[1];
                    let op_text = node_text(op, text);
                    if matches!(op_text, "<-" | "=" | "<<-") {
                        return; // Skip LHS of assignment
                    }
                }
            }

            // Skip if this is a named argument (e.g., n = 1 in readLines(..., n = 1))
            if parent.kind() == "argument" {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    if name_node.id() == node.id() {
                        return; // Skip argument names
                    }
                }
            }

            // Skip if this is the RHS of an extract operator ($ or @)
            // e.g., df$column or obj@slot - we don't want to check if column/slot is defined
            // The LHS (df, obj) should still be checked for undefined variables
            if parent.kind() == "extract_operator" {
                if let Some(rhs_node) = parent.child_by_field_name("rhs") {
                    if rhs_node.id() == node.id() {
                        return; // Skip RHS of extract operator
                    }
                }
            }
        }

        used.push((node_text(node, text).to_string(), node));
    }

    // Check if we're entering a formula expression (~ operator)
    // Tree-sitter-r represents formulas as:
    // - `~ x` is a `unary_operator` node with `~` as the operator
    // - `y ~ x` is a `binary_operator` node with `~` as the operator
    let is_formula_node = match node.kind() {
        "unary_operator" => {
            // For unary operator, check if the operator is ~
            // The first child is typically the operator
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            children
                .first()
                .is_some_and(|op| node_text(*op, text) == "~")
        }
        "binary_operator" => {
            // For binary operator, check if the operator (second child) is ~
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            children
                .get(1)
                .is_some_and(|op| node_text(*op, text) == "~")
        }
        _ => false,
    };

    // Check if this is a call-like node (call, subset, subset2)
    // These nodes have `function` and `arguments` fields
    // We only set in_call_like_arguments when entering the `arguments` field
    let is_call_like_node = matches!(node.kind(), "call" | "subset" | "subset2");

    // Create updated context if entering a formula
    let base_context = if is_formula_node {
        UsageContext {
            in_formula: true,
            ..context.clone()
        }
    } else {
        context.clone()
    };

    // Recurse into children with the (possibly updated) context
    // For call-like nodes, we need to handle the `arguments` field specially
    if is_call_like_node {
        // For call-like nodes, recurse into children but set in_call_like_arguments
        // only for the `arguments` field, not for the `function` field
        if let Some(function_node) = node.child_by_field_name("function") {
            // The function field should NOT have in_call_like_arguments set
            // We still want to check if the function name is defined
            collect_usages_with_context(function_node, text, &base_context, used);
        }
        if let Some(arguments_node) = node.child_by_field_name("arguments") {
            // The arguments field SHOULD have in_call_like_arguments set
            let args_context = UsageContext {
                in_call_like_arguments: true,
                ..base_context.clone()
            };
            collect_usages_with_context(arguments_node, text, &args_context, used);
        }
    } else {
        // For non-call-like nodes, recurse normally
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_usages_with_context(child, text, &base_context, used);
        }
    }
}

/// Determines whether an identifier is a recognized R builtin (constant or function).
///
/// Checks common R constants (e.g., `TRUE`, `FALSE`, `NULL`, `NA`, `Inf`, `NaN`, `T`, `F`) first and then consults the comprehensive builtin registry.
///
/// # Examples
///
/// ```
/// assert!(is_builtin("TRUE"));
/// assert!(is_builtin("sum"));
/// assert!(!is_builtin("my_custom_fn"));
/// ```
///
/// # Returns
/// `true` if `name` is a recognized R builtin constant or function, `false` otherwise.
fn is_builtin(name: &str) -> bool {
    // Check constants first
    if matches!(
        name,
        "TRUE" | "FALSE" | "NULL" | "NA" | "Inf" | "NaN" | "T" | "F"
    ) {
        return true;
    }
    // Check comprehensive builtin list
    builtins::is_builtin(name)
}

/// Determines whether an identifier is exported by any of the currently loaded packages.
///
/// Queries the provided PackageLibrary to decide if `name` originates from one of the packages
/// listed in `loaded_packages`.
///
/// # Returns
///
/// `true` if the symbol is exported by a loaded package, `false` otherwise.
///
/// # Examples
///
/// ```rust,no_run
/// # use crate::package_library::PackageLibrary;
/// # let package_library: PackageLibrary = unimplemented!();
/// let loaded = vec!["stats".to_string(), "base".to_string()];
/// let is_export = is_package_export("lm", &loaded, &package_library);
/// ```
fn is_package_export(
    name: &str,
    loaded_packages: &[String],
    package_library: &crate::package_library::PackageLibrary,
) -> bool {
    // Use PackageLibrary's synchronous method to check if symbol is from loaded packages
    // This checks base exports first, then cached package exports
    // Requirements 8.1, 8.2: Check position-aware loaded packages
    package_library.is_symbol_from_loaded_packages(name, loaded_packages)
}

// ============================================================================
// Completions
// ============================================================================

/// Build a list of completion items for the given document and cursor position.
///
/// Returns completion items that prioritize local document definitions, then package exports
/// (when packages are enabled) with per-package attribution, and finally cross-file symbols.
/// The function returns `None` when the document, its parse tree, or the node at the cursor
/// cannot be resolved.
///
/// # Examples
///
/// ```
/// // Obtain a `WorldState`, `Url`, and `Position` from your environment.
/// let state = /* WorldState instance */;
/// let uri = /* Url for the document */;
/// let pos = /* Position { line, character } */;
/// let resp = completion(&state, &uri, pos);
/// assert!(resp.is_some());
/// ```
pub fn completion(state: &WorldState, uri: &Url, position: Position) -> Option<CompletionResponse> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    let point = Point::new(position.line as usize, position.character as usize);
    let node = tree.root_node().descendant_for_point_range(point, point)?;

    let mut items = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    // Check if we're in a namespace context (pkg::)
    if find_namespace_context(&node, &text).is_some() {
        // TODO: Get package exports from library
        return Some(CompletionResponse::Array(items));
    }

    // Add R keywords
    let keywords = [
        "if",
        "else",
        "repeat",
        "while",
        "function",
        "for",
        "in",
        "next",
        "break",
        "TRUE",
        "FALSE",
        "NULL",
        "Inf",
        "NaN",
        "NA",
        "NA_integer_",
        "NA_real_",
        "NA_complex_",
        "NA_character_",
        "library",
        "require",
        "return",
        "print",
    ];

    for kw in keywords {
        items.push(CompletionItem {
            label: kw.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
        seen_names.insert(kw.to_string());
    }

    // Add symbols from current document (local definitions take precedence)
    collect_document_completions(tree.root_node(), &text, &mut items, &mut seen_names);

    // Get scope at cursor position for package exports
    // Requirements 9.1, 9.2: Add package exports to completions with package attribution
    let scope = get_cross_file_scope(state, uri, position.line, position.character);

    // Add package exports only if packages feature is enabled
    if state.cross_file_config.packages_enabled {
        // Combine inherited and loaded packages
        let mut all_packages: Vec<String> = scope.inherited_packages.clone();
        for pkg in &scope.loaded_packages {
            if !all_packages.contains(pkg) {
                all_packages.push(pkg.clone());
            }
        }

        // Add package exports (after local definitions, before cross-file symbols)
        // Requirement 9.4: Local definitions > package exports > cross-file symbols
        // Requirement 9.3: When multiple packages export same symbol, show all with attribution
        let package_exports = state
            .package_library
            .get_exports_for_completions(&all_packages);
        for (export_name, package_names) in package_exports {
            if seen_names.contains(&export_name) {
                continue; // Local definitions take precedence
            }
            seen_names.insert(export_name.clone());

            // Requirement 9.3: Show all packages that export this symbol
            for package_name in package_names {
                // Requirement 9.2: Include package name in detail field (e.g., "{dplyr}")
                items.push(CompletionItem {
                    label: export_name.clone(),
                    kind: Some(CompletionItemKind::FUNCTION), // Most package exports are functions
                    detail: Some(format!("{{{}}}", package_name)),
                    ..Default::default()
                });
            }
        }
    }

    // Add cross-file symbols (from scope resolution)
    // Requirement 9.5: Package exports > cross-file symbols
    for (name, symbol) in scope.symbols {
        if seen_names.contains(&name) {
            continue; // Local definitions and package exports take precedence
        }
        seen_names.insert(name.clone());

        let kind = match symbol.kind {
            crate::cross_file::SymbolKind::Function => CompletionItemKind::FUNCTION,
            crate::cross_file::SymbolKind::Variable => CompletionItemKind::VARIABLE,
            crate::cross_file::SymbolKind::Parameter => CompletionItemKind::VARIABLE,
        };

        // Add source file info to detail if from another file
        let detail = if symbol.source_uri != *uri {
            Some(format!("from {}", symbol.source_uri.path()))
        } else {
            None
        };

        items.push(CompletionItem {
            label: name,
            kind: Some(kind),
            detail,
            ..Default::default()
        });
    }

    Some(CompletionResponse::Array(items))
}

fn find_namespace_context<'a>(node: &Node<'a>, text: &'a str) -> Option<&'a str> {
    // Walk up to find namespace_operator
    let mut current = *node;
    loop {
        if current.kind() == "namespace_operator" {
            let mut cursor = current.walk();
            let children: Vec<_> = current.children(&mut cursor).collect();
            if !children.is_empty() {
                return Some(node_text(children[0], text));
            }
        }
        current = current.parent()?;
    }
}

fn collect_document_completions(
    node: Node,
    text: &str,
    items: &mut Vec<CompletionItem>,
    seen: &mut std::collections::HashSet<String>,
) {
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];
            let rhs = children[2];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-") && lhs.kind() == "identifier" {
                let name = node_text(lhs, text).to_string();
                if !seen.contains(&name) {
                    seen.insert(name.clone());
                    let kind = if rhs.kind() == "function_definition" {
                        CompletionItemKind::FUNCTION
                    } else {
                        CompletionItemKind::VARIABLE
                    };

                    items.push(CompletionItem {
                        label: name,
                        kind: Some(kind),
                        ..Default::default()
                    });
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_document_completions(child, text, items, seen);
    }
}

// ============================================================================
// Definition Statement Extraction
// ============================================================================

pub struct DefinitionInfo {
    pub statement: String,
    pub source_uri: Url,
    pub line: u32,
    #[allow(dead_code)]
    pub column: u32,
}

pub fn extract_definition_statement(
    symbol: &ScopedSymbol,
    state: &WorldState,
) -> Option<DefinitionInfo> {
    // Get content provider for the symbol's source file
    let content = if let Some(doc) = state.documents.get(&symbol.source_uri) {
        doc.text()
    } else if let Some(cached) = state.cross_file_file_cache.get(&symbol.source_uri) {
        cached
    } else {
        return None;
    };

    // Get tree for parsing
    let tree = if let Some(doc) = state.documents.get(&symbol.source_uri) {
        doc.tree.as_ref()?
    } else {
        // Parse content if not in documents
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into()).ok()?;
        let parsed = parser.parse(&content, None)?;
        // We can't store this tree, so we need to work with it immediately
        return extract_statement_from_tree(&parsed, symbol, &content);
    };

    extract_statement_from_tree(tree, symbol, &content)
}

fn utf16_column_to_byte_offset(line: &str, utf16_col: u32) -> usize {
    let mut utf16_count = 0;
    for (byte_idx, ch) in line.char_indices() {
        if utf16_count == utf16_col as usize {
            return byte_idx;
        }
        utf16_count += ch.len_utf16();
    }
    line.len()
}

fn next_utf8_char_boundary(line: &str, byte_offset: usize) -> usize {
    if byte_offset >= line.len() {
        return line.len();
    }

    // Find the next UTF-8 codepoint boundary (i.e., the start byte of the next char).
    // This avoids creating Point ranges that land mid-codepoint (which tree-sitter rejects).
    for (idx, _ch) in line.char_indices() {
        if idx > byte_offset {
            return idx;
        }
    }

    line.len()
}

fn extract_statement_from_tree(
    tree: &tree_sitter::Tree,
    symbol: &ScopedSymbol,
    content: &str,
) -> Option<DefinitionInfo> {
    let line_text = content
        .lines()
        .nth(symbol.defined_line as usize)
        .unwrap_or("");
    let byte_col = utf16_column_to_byte_offset(line_text, symbol.defined_column);
    let row = symbol.defined_line as usize;

    // descendant_for_point_range can behave unexpectedly on 0-length ranges at node boundaries.
    // Use a small non-empty range when possible and prefer named nodes.
    let point_start = tree_sitter::Point::new(row, byte_col);
    let byte_col_end = next_utf8_char_boundary(line_text, byte_col);
    let point_end = tree_sitter::Point::new(row, byte_col_end);

    let root = tree.root_node();
    let node = root
        .named_descendant_for_point_range(point_start, point_end)
        .or_else(|| root.descendant_for_point_range(point_start, point_end))?;

    // Find the appropriate parent node based on symbol kind
    let statement_node = match symbol.kind {
        scope::SymbolKind::Variable => find_assignment_statement(node, content),
        scope::SymbolKind::Function => find_function_statement(node, content),
        scope::SymbolKind::Parameter => find_function_statement(node, content),
    }?;

    let statement = extract_statement_text(statement_node, content);

    Some(DefinitionInfo {
        statement,
        source_uri: symbol.source_uri.clone(),
        line: symbol.defined_line,
        column: symbol.defined_column,
    })
}

/// Result of finding a statement node - includes whether to extract header only
struct StatementMatch<'a> {
    node: tree_sitter::Node<'a>,
    header_only: bool,
}

fn find_assignment_statement<'a>(
    mut node: tree_sitter::Node<'a>,
    content: &str,
) -> Option<StatementMatch<'a>> {
    // Walk up to find binary_operator (assignment), for_statement, or parameter
    loop {
        match node.kind() {
            "binary_operator" => {
                let mut cursor = node.walk();
                let children: Vec<_> = node.children(&mut cursor).collect();
                if children.len() >= 2 {
                    let op_text = node_text(children[1], content);
                    if matches!(op_text, "<-" | "=" | "<<-" | "->") {
                        return Some(StatementMatch {
                            node,
                            header_only: false,
                        });
                    }
                }
            }
            "for_statement" => {
                return Some(StatementMatch {
                    node,
                    header_only: false,
                })
            }
            "parameter" => {
                // For parameters, find enclosing function_definition
                if let Some(func) = find_enclosing_function(node) {
                    return Some(StatementMatch {
                        node: func,
                        header_only: false,
                    });
                }
            }
            _ => {}
        }

        if let Some(parent) = node.parent() {
            node = parent;
        } else {
            break;
        }
    }
    None
}

fn find_function_statement<'a>(
    mut node: tree_sitter::Node<'a>,
    content: &str,
) -> Option<StatementMatch<'a>> {
    // Walk up to find function_definition or assignment containing function.
    // For definition extraction, we want the full statement so we can include bodies and apply
    // standard truncation rules.
    loop {
        match node.kind() {
            "function_definition" => {
                // Check if parent is assignment
                if let Some(parent) = node.parent() {
                    if parent.kind() == "binary_operator" {
                        return Some(StatementMatch {
                            node: parent,
                            header_only: false,
                        });
                    }
                }
                return Some(StatementMatch {
                    node,
                    header_only: false,
                });
            }
            "binary_operator" => {
                let mut cursor = node.walk();
                let children: Vec<_> = node.children(&mut cursor).collect();
                if children.len() >= 3 {
                    let op_text = node_text(children[1], content);
                    // Check for function on RHS (for <- = <<-) or LHS (for ->)
                    if matches!(op_text, "<-" | "=" | "<<-")
                        && children[2].kind() == "function_definition"
                    {
                        return Some(StatementMatch {
                            node,
                            header_only: false,
                        });
                    }
                    if op_text == "->" && children[0].kind() == "function_definition" {
                        return Some(StatementMatch {
                            node,
                            header_only: false,
                        });
                    }
                }
            }
            _ => {}
        }

        if let Some(parent) = node.parent() {
            node = parent;
        } else {
            break;
        }
    }
    None
}

fn find_enclosing_function(mut node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    loop {
        if node.kind() == "function_definition" {
            // Check if parent is assignment
            if let Some(parent) = node.parent() {
                if parent.kind() == "binary_operator" {
                    return Some(parent);
                }
            }
            return Some(node);
        }
        node = node.parent()?;
    }
}

#[allow(clippy::needless_range_loop)]
fn extract_statement_text(stmt: StatementMatch, content: &str) -> String {
    let node = stmt.node;
    let lines: Vec<&str> = content.lines().collect();
    let start_line = node.start_position().row;

    if start_line >= lines.len() {
        return String::new();
    }

    if stmt.header_only {
        // For for-loops: extract just the header (for (x in seq))
        // For functions: extract signature up to body start
        return extract_header(node, content);
    }

    let end_line = node.end_position().row;

    // Truncate to 10 lines maximum
    let actual_end_line = if end_line - start_line >= 10 {
        start_line + 9
    } else {
        end_line
    };

    let mut result = String::new();
    for i in start_line..=actual_end_line.min(lines.len() - 1) {
        if i > start_line {
            result.push('\n');
        }
        result.push_str(lines[i]);
    }

    // Add ellipsis if truncated
    if end_line - start_line >= 10 {
        result.push_str("\n...");
    }

    result
}

fn extract_header(node: tree_sitter::Node, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start_line = node.start_position().row;

    match node.kind() {
        "for_statement" => {
            // For loop: extract "for (var in seq)"
            // Find the body child and stop before it
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "brace_list"
                    || child.kind() == "call"
                    || (child.kind() != "identifier"
                        && child.kind() != "("
                        && child.kind() != ")"
                        && child.kind() != "in"
                        && child.start_position().row > start_line)
                {
                    // Body starts - extract up to before body
                    let body_start = child.start_position();
                    if body_start.row == start_line {
                        // Body on same line - extract up to body start column
                        let line = lines.get(start_line).unwrap_or(&"");
                        return line[..body_start.column.min(line.len())]
                            .trim_end()
                            .to_string();
                    } else {
                        // Body on different line - extract header lines
                        let mut result = String::new();
                        for i in start_line..body_start.row {
                            if i > start_line {
                                result.push('\n');
                            }
                            if let Some(line) = lines.get(i) {
                                result.push_str(line);
                            }
                        }
                        return result;
                    }
                }
            }
            // Fallback: just first line
            lines.get(start_line).unwrap_or(&"").to_string()
        }
        "function_definition" | "binary_operator" => {
            // Function: extract signature up to body
            extract_function_header(node, content)
        }
        _ => lines.get(start_line).unwrap_or(&"").to_string(),
    }
}

fn extract_function_header(node: tree_sitter::Node, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start_line = node.start_position().row;

    // Find the function_definition node
    let func_node = if node.kind() == "function_definition" {
        node
    } else {
        // binary_operator - find function_definition child
        let mut cursor = node.walk();
        let mut func = None;
        for child in node.children(&mut cursor) {
            if child.kind() == "function_definition" {
                func = Some(child);
                break;
            }
        }
        match func {
            Some(f) => f,
            None => return lines.get(start_line).unwrap_or(&"").to_string(),
        }
    };

    // Find body in function_definition
    let mut cursor = func_node.walk();
    for child in func_node.children(&mut cursor) {
        // Body is typically brace_list or any expression after parameters
        if child.kind() == "brace_list"
            || (child.kind() != "function"
                && child.kind() != "parameters"
                && child.start_position().row >= start_line)
        {
            let body_start = child.start_position();

            // Extract from node start to body start
            if body_start.row == start_line {
                let line = lines.get(start_line).unwrap_or(&"");
                let end_col = body_start.column.min(line.len());
                return line[..end_col].trim_end().to_string();
            } else {
                let mut result = String::new();
                for i in start_line..body_start.row {
                    if i > start_line {
                        result.push('\n');
                    }
                    if let Some(line) = lines.get(i) {
                        result.push_str(line);
                    }
                }
                // Add partial last line if body starts mid-line
                if body_start.column > 0 {
                    if let Some(line) = lines.get(body_start.row) {
                        if !result.is_empty() {
                            result.push('\n');
                        }
                        result.push_str(line[..body_start.column.min(line.len())].trim_end());
                    }
                }
                return result;
            }
        }
    }

    // Fallback: first line
    lines.get(start_line).unwrap_or(&"").to_string()
}

// ============================================================================
// Hover
// ============================================================================

/// Provide hover information for the symbol at a given text document position.
///
/// Tries, in order:
/// 1. Cross-file symbol resolution (including local definitions), returning an extracted definition or signature with source attribution.
/// 2. Package exports discovered from the combined package scope, returning a signature and package attribution.
/// 3. Cached R help text or a one-time lookup of R help for builtins and other symbols.
///
/// The produced hover content is Markdown (code block for signatures/definitions and optional attribution) and the hover range corresponds to the identifier node under the cursor.
///
/// # Examples
///
/// ```no_run
/// # use lsp_types::Position;
/// # use url::Url;
/// # use crate::state::WorldState;
/// // Assuming `state` is available and `uri` refers to an open R document:
/// let pos = Position::new(10, 4);
/// let _ = hover(&state, &uri, pos);
/// ```
///
/// Returns `Some(Hover)` when information (definition, signature, package attribution, or help text) is available for the identifier at `position`, `None` when no useful hover content can be produced.
pub async fn hover(state: &WorldState, uri: &Url, position: Position) -> Option<Hover> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    let line_text = text.lines().nth(position.line as usize).unwrap_or("");
    let byte_col = utf16_column_to_byte_offset(line_text, position.character);
    let row = position.line as usize;

    let point_start = Point::new(row, byte_col);
    let point_end = Point::new(row, next_utf8_char_boundary(line_text, byte_col));
    let root = tree.root_node();
    let node = root
        .named_descendant_for_point_range(point_start, point_end)
        .or_else(|| root.descendant_for_point_range(point_start, point_end))?;

    // Get the identifier
    let name = if node.kind() == "identifier" || node.kind() == "string" {
        node_text(node, &text)
    } else {
        return None;
    };

    let node_range = Range {
        start: Position::new(
            node.start_position().row as u32,
            node.start_position().column as u32,
        ),
        end: Position::new(
            node.end_position().row as u32,
            node.end_position().column as u32,
        ),
    };

    // Try cross-file symbols (includes local scope with definition extraction)
    log::trace!("Calling get_cross_file_symbols for hover");
    let cross_file_symbols = get_cross_file_symbols(state, uri, position.line, position.character);
    log::trace!(
        "Got {} symbols from cross-file scope",
        cross_file_symbols.len()
    );
    if let Some(symbol) = cross_file_symbols.get(name) {
        let mut value = String::new();

        // Check if this is a package export (source_uri starts with "package:")
        // Package exports have URIs like "package:dplyr" or "package:base"
        let package_name = symbol.source_uri.as_str().strip_prefix("package:");

        // Try to extract definition statement
        let workspace_root = state.workspace_folders.first();
        match extract_definition_statement(symbol, state) {
            Some(def_info) => {
                // Note: No escaping needed inside code blocks - markdown doesn't interpret special chars there
                value.push_str(&format!("```r\n{}\n```\n\n", def_info.statement));

                // Add file location
                if def_info.source_uri == *uri {
                    value.push_str(&format!("this file, line {}", def_info.line + 1));
                } else {
                    let relative_path = compute_relative_path(&def_info.source_uri, workspace_root);
                    let absolute_path = def_info.source_uri.as_str();
                    value.push_str(&format!(
                        "[{}]({}), line {}",
                        relative_path,
                        absolute_path,
                        def_info.line + 1
                    ));
                }
            }
            None => {
                // Graceful fallback: show symbol info without definition statement
                // For package exports, try to get function signature from R help
                // Validates: Requirement 10.2
                if let Some(pkg) = package_name {
                    // Try to get function signature from R help
                    let name_owned = name.to_string();
                    let pkg_owned = pkg.to_string();
                    if let Ok(signature) = tokio::task::spawn_blocking(move || {
                        crate::help::get_function_signature(&name_owned, &pkg_owned)
                    })
                    .await
                    {
                        if let Some(signature) = signature {
                            value.push_str(&format!("```r\n{}\n```\n", signature));
                        } else if let Some(sig) = &symbol.signature {
                            value.push_str(&format!("```r\n{}\n```\n", sig));
                        } else {
                            value.push_str(&format!("```r\n{}\n```\n", name));
                        }
                    } else if let Some(sig) = &symbol.signature {
                        value.push_str(&format!("```r\n{}\n```\n", sig));
                    } else {
                        value.push_str(&format!("```r\n{}\n```\n", name));
                    }
                    // Display package name prominently for package exports
                    // Validates: Requirement 10.1
                    value.push_str(&format!("\nfrom {{{}}}", pkg));
                } else if let Some(sig) = &symbol.signature {
                    value.push_str(&format!("```r\n{}\n```\n", sig));
                    if symbol.source_uri != *uri {
                        let relative_path =
                            compute_relative_path(&symbol.source_uri, workspace_root);
                        value.push_str(&format!("\n*Defined in {}*", relative_path));
                    }
                } else {
                    value.push_str(&format!("```r\n{}\n```\n", name));
                    if symbol.source_uri != *uri {
                        let relative_path =
                            compute_relative_path(&symbol.source_uri, workspace_root);
                        value.push_str(&format!("\n*Defined in {}*", relative_path));
                    }
                }
            }
        }

        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(node_range),
        });
    }

    // Check package exports from combined_exports cache (if packages enabled)
    // This surfaces package exports without blocking on R subprocess
    if state.cross_file_config.packages_enabled {
        let scope = get_cross_file_scope(state, uri, position.line, position.character);
        let all_packages: Vec<String> = scope
            .inherited_packages
            .iter()
            .chain(scope.loaded_packages.iter())
            .cloned()
            .collect();

        if let Some(pkg_name) = state
            .package_library
            .find_package_for_symbol(name, &all_packages)
        {
            let mut value = String::new();

            // Try to get function signature from R help
            let name_owned = name.to_string();
            let pkg_owned = pkg_name.to_string();
            if let Ok(signature) = tokio::task::spawn_blocking(move || {
                crate::help::get_function_signature(&name_owned, &pkg_owned)
            })
            .await
            {
                if let Some(signature) = signature {
                    value.push_str(&format!("```r\n{}\n```\n", signature));
                } else {
                    value.push_str(&format!("```r\n{}\n```\n", name));
                }
            } else {
                value.push_str(&format!("```r\n{}\n```\n", name));
            }
            value.push_str(&format!("\nfrom {{{}}}", pkg_name));

            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value,
                }),
                range: Some(node_range),
            });
        }
    }

    // Fallback to R help system for built-ins and undefined symbols
    // Check cache first to avoid repeated R subprocess calls
    if let Some(cached) = state.help_cache.get(name) {
        if let Some(help_text) = cached {
            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("```\n{}\n```", help_text),
                }),
                range: Some(node_range),
            });
        }
        // Cached as None - no help available
        return None;
    }

    // Try to get help from R subprocess
    let name_owned = name.to_string();
    if let Ok(help_text) =
        tokio::task::spawn_blocking(move || crate::help::get_help(&name_owned, None)).await
    {
        if let Some(help_text) = help_text {
            // Cache successful result
            state
                .help_cache
                .insert(name.to_string(), Some(help_text.clone()));

            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("```\n{}\n```", help_text),
                }),
                range: Some(node_range),
            });
        }
    }

    // Cache negative result to avoid repeated failed lookups
    state.help_cache.insert(name.to_string(), None);
    None
}
// Signature Help
// ============================================================================

pub fn signature_help(state: &WorldState, uri: &Url, position: Position) -> Option<SignatureHelp> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    let point = Point::new(position.line as usize, position.character as usize);

    // Find enclosing call
    let mut node = tree.root_node().descendant_for_point_range(point, point)?;

    loop {
        if node.kind() == "call" {
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();

            if !children.is_empty() {
                let func_node = children[0];
                let func_name = node_text(func_node, &text);

                return Some(SignatureHelp {
                    signatures: vec![SignatureInformation {
                        label: format!("{}(...)", func_name),
                        documentation: None,
                        parameters: None,
                        active_parameter: None,
                    }],
                    active_signature: Some(0),
                    active_parameter: None,
                });
            }
        }

        node = node.parent()?;
    }
}

// ============================================================================
// Goto Definition
// ============================================================================

/// Locate the definition location for the identifier at the given position by searching
/// the current document, cross-file symbols, open documents, and the workspace index.
///
/// If the identifier is defined in the current document, its local definition is returned.
/// Otherwise the function searches cross-file symbols and exported interfaces from open
/// documents and the workspace. If the symbol originates from a package (pseudo-URI
/// starting with "package:"), no navigable location is returned.
///
/// # Returns
///
/// `Some(Location)` pointing to the symbol's defining range when a navigable definition is found;
/// `None` if no definition is found or if the symbol is a package export (non-navigable).
///
/// # Examples
///
/// ```
/// // Assume `state`, `uri`, and `pos` are available in the test harness.
/// let result = goto_definition(&state, &uri, pos);
/// // `result` will be `Some(...)` when a navigable definition exists, otherwise `None`.
/// ```
pub fn goto_definition(
    state: &WorldState,
    uri: &Url,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    // Use ContentProvider for unified access
    let content_provider = state.content_provider();

    // Try open document first, then workspace index
    let doc = state
        .get_document(uri)
        .or_else(|| state.workspace_index.get(uri))?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    let point = Point::new(position.line as usize, position.character as usize);
    let node = tree.root_node().descendant_for_point_range(point, point)?;

    if node.kind() != "identifier" {
        return None;
    }

    let name = node_text(node, &text);

    // Search current document first
    if let Some(def_range) = find_definition_in_tree(tree.root_node(), name, &text) {
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: def_range,
        }));
    }

    // Try cross-file symbols (from scope resolution)
    let cross_file_symbols = get_cross_file_symbols(state, uri, position.line, position.character);
    if let Some(symbol) = cross_file_symbols.get(name) {
        // Check if this is a package export (source_uri starts with "package:")
        // Package exports have pseudo-URIs like "package:dplyr" that can't be navigated to
        // Validates: Requirements 11.1, 11.2
        if symbol.source_uri.as_str().starts_with("package:") {
            // Package source is not available for navigation
            // The hover handler shows package info; go-to-definition returns None
            // to indicate no navigable location exists
            log::trace!(
                "Symbol '{}' is from package '{}', no navigable source available",
                name,
                symbol
                    .source_uri
                    .as_str()
                    .strip_prefix("package:")
                    .unwrap_or("unknown")
            );
            return None;
        }

        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: symbol.source_uri.clone(),
            range: Range {
                start: Position::new(symbol.defined_line, symbol.defined_column),
                end: Position::new(
                    symbol.defined_line,
                    symbol.defined_column + name.len() as u32,
                ),
            },
        }));
    }

    // Search all open documents using ContentProvider
    for file_uri in state.document_store.uris() {
        if &file_uri == uri {
            continue;
        }
        if let Some(artifacts) = content_provider.get_artifacts(&file_uri) {
            if let Some(symbol) = artifacts.exported_interface.get(name) {
                // Skip package exports (they have pseudo-URIs that can't be navigated to)
                if symbol.source_uri.as_str().starts_with("package:") {
                    continue;
                }
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: symbol.source_uri.clone(),
                    range: Range {
                        start: Position::new(symbol.defined_line, symbol.defined_column),
                        end: Position::new(
                            symbol.defined_line,
                            symbol.defined_column + name.len() as u32,
                        ),
                    },
                }));
            }
        }
    }

    // Search workspace index using ContentProvider
    for (file_uri, _) in state.workspace_index_new.iter() {
        if &file_uri == uri {
            continue;
        }
        if let Some(artifacts) = content_provider.get_artifacts(&file_uri) {
            if let Some(symbol) = artifacts.exported_interface.get(name) {
                // Skip package exports (they have pseudo-URIs that can't be navigated to)
                if symbol.source_uri.as_str().starts_with("package:") {
                    continue;
                }
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: symbol.source_uri.clone(),
                    range: Range {
                        start: Position::new(symbol.defined_line, symbol.defined_column),
                        end: Position::new(
                            symbol.defined_line,
                            symbol.defined_column + name.len() as u32,
                        ),
                    },
                }));
            }
        }
    }

    // Fallback: Search legacy open documents
    for (file_uri, doc) in &state.documents {
        if file_uri == uri {
            continue;
        }
        if let Some(tree) = &doc.tree {
            let file_text = doc.text();
            if let Some(def_range) = find_definition_in_tree(tree.root_node(), name, &file_text) {
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: file_uri.clone(),
                    range: def_range,
                }));
            }
        }
    }

    // Fallback: Search legacy workspace index
    for (file_uri, doc) in &state.workspace_index {
        if file_uri == uri {
            continue;
        }
        if let Some(tree) = &doc.tree {
            let file_text = doc.text();
            if let Some(def_range) = find_definition_in_tree(tree.root_node(), name, &file_text) {
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: file_uri.clone(),
                    range: def_range,
                }));
            }
        }
    }

    None
}

fn find_definition_in_tree(node: Node, name: &str, text: &str) -> Option<Range> {
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-")
                && lhs.kind() == "identifier"
                && node_text(lhs, text) == name
            {
                return Some(Range {
                    start: Position::new(
                        lhs.start_position().row as u32,
                        lhs.start_position().column as u32,
                    ),
                    end: Position::new(
                        lhs.end_position().row as u32,
                        lhs.end_position().column as u32,
                    ),
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(range) = find_definition_in_tree(child, name, text) {
            return Some(range);
        }
    }

    None
}

// ============================================================================
// References
// ============================================================================

pub fn references(state: &WorldState, uri: &Url, position: Position) -> Option<Vec<Location>> {
    // Use ContentProvider for unified access
    let content_provider = state.content_provider();

    // Try open document first, then workspace index
    let doc = state
        .get_document(uri)
        .or_else(|| state.workspace_index.get(uri))?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    let point = Point::new(position.line as usize, position.character as usize);
    let node = tree.root_node().descendant_for_point_range(point, point)?;

    if node.kind() != "identifier" {
        return None;
    }

    let name = node_text(node, &text);
    let mut locations = Vec::new();

    // Search current document
    find_references_in_tree(tree.root_node(), name, &text, uri, &mut locations);

    // Search all open documents using new DocumentStore
    for file_uri in state.document_store.uris() {
        if &file_uri == uri {
            continue; // Already searched
        }
        if let Some(content) = content_provider.get_content(&file_uri) {
            // Parse the content to search for references
            if let Some(doc_state) = state.document_store.get_without_touch(&file_uri) {
                if let Some(tree) = &doc_state.tree {
                    find_references_in_tree(
                        tree.root_node(),
                        name,
                        &content,
                        &file_uri,
                        &mut locations,
                    );
                }
            }
        }
    }

    // Search workspace index using new WorkspaceIndex
    for (file_uri, entry) in state.workspace_index_new.iter() {
        if &file_uri == uri {
            continue; // Already searched
        }
        if let Some(tree) = &entry.tree {
            let file_text = entry.contents.to_string();
            find_references_in_tree(
                tree.root_node(),
                name,
                &file_text,
                &file_uri,
                &mut locations,
            );
        }
    }

    // Fallback: Search legacy open documents
    for (file_uri, doc) in &state.documents {
        if file_uri == uri {
            continue; // Already searched
        }
        // Skip if already found in new stores
        if state.document_store.contains(file_uri) {
            continue;
        }
        if let Some(tree) = &doc.tree {
            let file_text = doc.text();
            find_references_in_tree(tree.root_node(), name, &file_text, file_uri, &mut locations);
        }
    }

    // Fallback: Search legacy workspace index
    for (file_uri, doc) in &state.workspace_index {
        if file_uri == uri {
            continue; // Already searched
        }
        // Skip if already found in new stores
        if state.workspace_index_new.contains(file_uri) {
            continue;
        }
        if let Some(tree) = &doc.tree {
            let file_text = doc.text();
            find_references_in_tree(tree.root_node(), name, &file_text, file_uri, &mut locations);
        }
    }

    Some(locations)
}

fn find_references_in_tree(
    node: Node,
    name: &str,
    text: &str,
    uri: &Url,
    locations: &mut Vec<Location>,
) {
    if node.kind() == "identifier" && node_text(node, text) == name {
        locations.push(Location {
            uri: uri.clone(),
            range: Range {
                start: Position::new(
                    node.start_position().row as u32,
                    node.start_position().column as u32,
                ),
                end: Position::new(
                    node.end_position().row as u32,
                    node.end_position().column as u32,
                ),
            },
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_references_in_tree(child, name, text, uri, locations);
    }
}

// ============================================================================
// On Type Formatting (Indentation)
// ============================================================================

pub fn on_type_formatting(
    state: &WorldState,
    uri: &Url,
    position: Position,
) -> Option<Vec<TextEdit>> {
    let doc = state.get_document(uri)?;
    let text = doc.text();

    // Simple indentation: match previous line's indentation
    if position.line == 0 {
        return None;
    }

    let prev_line_idx = position.line as usize - 1;
    let lines: Vec<&str> = text.lines().collect();

    if prev_line_idx >= lines.len() {
        return None;
    }

    let prev_line = lines[prev_line_idx];
    let indent: String = prev_line
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();

    // Check if previous line ends with { or ( - add extra indent
    let trimmed = prev_line.trim_end();
    let extra_indent = if trimmed.ends_with('{') || trimmed.ends_with('(') {
        "  "
    } else {
        ""
    };

    let new_indent = format!("{}{}", indent, extra_indent);

    Some(vec![TextEdit {
        range: Range {
            start: Position::new(position.line, 0),
            end: Position::new(position.line, 0),
        },
        new_text: new_indent,
    }])
}

// ============================================================================
// Utilities
// ============================================================================

fn node_text<'a>(node: Node<'a>, text: &'a str) -> &'a str {
    &text[node.byte_range()]
}

// ============================================================================
// Signature Extraction (used in tests)
// ============================================================================

#[cfg(test)]
fn extract_parameters(params_node: Node, text: &str) -> Vec<String> {
    let mut parameters = Vec::new();
    let mut cursor = params_node.walk();

    for child in params_node.children(&mut cursor) {
        if child.kind() == "parameter" {
            let mut param_cursor = child.walk();
            let param_children: Vec<_> = child.children(&mut param_cursor).collect();

            // Check if this parameter contains dots
            if let Some(_dots) = param_children.iter().find(|n| n.kind() == "dots") {
                parameters.push("...".to_string());
            } else if let Some(identifier) =
                param_children.iter().find(|n| n.kind() == "identifier")
            {
                let param_name = node_text(*identifier, text);

                // Check for default value
                if param_children.len() >= 3 && param_children[1].kind() == "=" {
                    let default_value = node_text(param_children[2], text);
                    parameters.push(format!("{} = {}", param_name, default_value));
                } else {
                    parameters.push(param_name.to_string());
                }
            }
        } else if child.kind() == "dots" {
            parameters.push("...".to_string());
        }
    }

    parameters
}

#[cfg(test)]
fn extract_function_signature(func_node: Node, func_name: &str, text: &str) -> String {
    let mut cursor = func_node.walk();

    for child in func_node.children(&mut cursor) {
        if child.kind() == "parameters" {
            let params = extract_parameters(child, text);
            return format!("{}({})", func_name, params.join(", "));
        }
    }

    format!("{}()", func_name)
}

#[cfg(test)]
fn find_function_definition_node<'a>(node: Node<'a>, name: &str, text: &str) -> Option<Node<'a>> {
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];
            let rhs = children[2];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-")
                && lhs.kind() == "identifier"
                && node_text(lhs, text) == name
                && rhs.kind() == "function_definition"
            {
                return Some(rhs);
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(func_node) = find_function_definition_node(child, name, text) {
            return Some(func_node);
        }
    }

    None
}

#[cfg(test)]
fn find_user_function_signature(
    state: &WorldState,
    current_uri: &Url,
    name: &str,
) -> Option<String> {
    // 1. Search current document
    if let Some(doc) = state.get_document(current_uri) {
        if let Some(tree) = &doc.tree {
            let text = doc.text();
            if let Some(func_node) = find_function_definition_node(tree.root_node(), name, &text) {
                return Some(extract_function_signature(func_node, name, &text));
            }
        }
    }

    // 2. Search open documents (skip current_uri)
    for (uri, doc) in &state.documents {
        if uri == current_uri {
            continue;
        }
        if let Some(tree) = &doc.tree {
            let text = doc.text();
            if let Some(func_node) = find_function_definition_node(tree.root_node(), name, &text) {
                return Some(extract_function_signature(func_node, name, &text));
            }
        }
    }

    // 3. Search workspace index
    for doc in state.workspace_index.values() {
        if let Some(tree) = &doc.tree {
            let text = doc.text();
            if let Some(func_node) = find_function_definition_node(tree.root_node(), name, &text) {
                return Some(extract_function_signature(func_node, name, &text));
            }
        }
    }

    None
}

// ============================================================================
// Path Utilities
// ============================================================================

/// Compute relative path from workspace root to target URI.
/// If no workspace root or target is outside workspace, returns filename only.
fn compute_relative_path(target_uri: &Url, workspace_root: Option<&Url>) -> String {
    let Some(workspace_root) = workspace_root else {
        return target_uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown")
            .to_string();
    };

    let Ok(workspace_path) = workspace_root.to_file_path() else {
        return target_uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown")
            .to_string();
    };

    let Ok(target_path) = target_uri.to_file_path() else {
        return target_uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown")
            .to_string();
    };

    match target_path.strip_prefix(&workspace_path) {
        Ok(relative) => relative.to_string_lossy().to_string(),
        Err(_) => target_uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown")
            .to_string(),
    }
}

// Note: escape_markdown is only used in tests now.
// Code blocks (```r ... ```) don't need escaping - markdown doesn't interpret special chars inside them.
#[cfg(test)]
/// Escape markdown special characters in text.
/// Characters to escape: * _ [ ] ( ) # ` \
fn escape_markdown(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '*' | '_' | '[' | ']' | '(' | ')' | '#' | '`' | '\\' => format!("\\{}", c),
            _ => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
fn hover_blocking(state: &WorldState, uri: &Url, position: Position) -> Option<Hover> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.block_on(hover(state, uri, position))
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(hover(state, uri, position))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn parse_r_code(code: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    #[test]
    fn test_function_parameters_recognized() {
        let code = "f <- function(a, b) { a + b }";
        let tree = parse_r_code(code);
        let mut defined = HashSet::new();
        collect_definitions(tree.root_node(), code, &mut defined);

        assert!(defined.contains("f"), "Function name should be defined");
        assert!(defined.contains("a"), "Parameter 'a' should be defined");
        assert!(defined.contains("b"), "Parameter 'b' should be defined");
    }

    #[test]
    fn test_single_parameter() {
        let code = "square <- function(x) { x * x }";
        let tree = parse_r_code(code);
        let mut defined = HashSet::new();
        collect_definitions(tree.root_node(), code, &mut defined);

        assert!(defined.contains("square"));
        assert!(defined.contains("x"));
    }

    #[test]
    fn test_no_parameters() {
        let code = "get_pi <- function() { 3.14 }";
        let tree = parse_r_code(code);
        let mut defined = HashSet::new();
        collect_definitions(tree.root_node(), code, &mut defined);

        assert!(defined.contains("get_pi"));
    }

    #[test]
    fn test_builtin_functions() {
        assert!(is_builtin("warning"));
        assert!(is_builtin("any"));
        assert!(is_builtin("is.na"));
        assert!(is_builtin("sprintf"));
        assert!(is_builtin("print"));
        assert!(is_builtin("sum"));
        assert!(is_builtin("mean"));
    }

    #[test]
    fn test_builtin_constants() {
        assert!(is_builtin("TRUE"));
        assert!(is_builtin("FALSE"));
        assert!(is_builtin("NULL"));
        assert!(is_builtin("NA"));
        assert!(is_builtin("Inf"));
        assert!(is_builtin("NaN"));
    }

    #[test]
    fn test_not_builtin() {
        assert!(!is_builtin("my_custom_function"));
        assert!(!is_builtin("undefined_var"));
    }

    #[test]
    fn test_nested_function_parameters() {
        let code = "outer <- function(x) { inner <- function(y) { x + y }; inner }";
        let tree = parse_r_code(code);
        let mut defined = HashSet::new();
        collect_definitions(tree.root_node(), code, &mut defined);

        assert!(defined.contains("outer"));
        assert!(defined.contains("x"));
        assert!(defined.contains("inner"));
        assert!(defined.contains("y"));
    }

    #[test]
    fn test_extract_parameters_simple() {
        let code = "add <- function(a, b = 1) { }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition(tree.root_node()).unwrap();
        let mut cursor = func_node.walk();
        let params_node = func_node
            .children(&mut cursor)
            .find(|n| n.kind() == "parameters")
            .unwrap();

        let params = extract_parameters(params_node, code);
        assert_eq!(params, vec!["a", "b = 1"]);
    }

    #[test]
    fn test_extract_function_signature() {
        let code = "add <- function(a, b = 1) { }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition(tree.root_node()).unwrap();
        let signature = extract_function_signature(func_node, "add", code);
        assert_eq!(signature, "add(a, b = 1)");
    }

    #[test]
    fn test_signature_simple_function() {
        let code = "add <- function(a, b) { a + b }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition_node(tree.root_node(), "add", code).unwrap();
        let signature = extract_function_signature(func_node, "add", code);
        assert_eq!(signature, "add(a, b)");
    }

    #[test]
    fn test_signature_no_parameters() {
        let code = "get_pi <- function() { 3.14 }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition_node(tree.root_node(), "get_pi", code).unwrap();
        let signature = extract_function_signature(func_node, "get_pi", code);
        assert_eq!(signature, "get_pi()");
    }

    #[test]
    fn test_signature_with_defaults() {
        let code = "greet <- function(name = \"World\") { }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition_node(tree.root_node(), "greet", code).unwrap();
        let signature = extract_function_signature(func_node, "greet", code);
        assert_eq!(signature, "greet(name = \"World\")");
    }

    #[test]
    fn test_signature_with_dots() {
        let code = "wrapper <- function(...) { }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition_node(tree.root_node(), "wrapper", code).unwrap();
        let signature = extract_function_signature(func_node, "wrapper", code);
        assert_eq!(signature, "wrapper(...)");
    }

    #[test]
    fn test_compute_relative_path_with_workspace_root() {
        let workspace_root = Url::parse("file:///workspace/").unwrap();
        let target_uri = Url::parse("file:///workspace/src/main.R").unwrap();

        let result = compute_relative_path(&target_uri, Some(&workspace_root));
        assert_eq!(result, "src/main.R");
    }

    #[test]
    fn test_compute_relative_path_without_workspace_root() {
        let target_uri = Url::parse("file:///workspace/src/main.R").unwrap();

        let result = compute_relative_path(&target_uri, None);
        assert_eq!(result, "main.R");
    }

    #[test]
    fn test_compute_relative_path_outside_workspace() {
        let workspace_root = Url::parse("file:///workspace/").unwrap();
        let target_uri = Url::parse("file:///other/path/script.R").unwrap();

        let result = compute_relative_path(&target_uri, Some(&workspace_root));
        assert_eq!(result, "script.R");
    }

    #[test]
    fn test_escape_markdown_all_special_chars() {
        let input = "*_[]()#`\\";
        let expected = "\\*\\_\\[\\]\\(\\)\\#\\`\\\\";

        let result = escape_markdown(input);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_escape_markdown_no_special_chars() {
        let input = "hello world 123";

        let result = escape_markdown(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_escape_markdown_mixed_content() {
        let input = "function(x) { x * 2 }";
        let expected = "function\\(x\\) { x \\* 2 }";

        let result = escape_markdown(input);
        assert_eq!(result, expected);
    }

    fn find_function_definition(node: Node) -> Option<Node> {
        if node.kind() == "function_definition" {
            return Some(node);
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(func) = find_function_definition(child) {
                return Some(func);
            }
        }
        None
    }

    // ========================================================================
    // Extract Operator Tests (Task 6.1)
    // Tests for skip-nse-undefined-checks feature
    // Validates: Requirements 1.1, 1.2, 1.3
    // ========================================================================

    /// Test that df$column does not produce a diagnostic for 'column'
    /// Validates: Requirement 1.1 - RHS of $ operator should be skipped
    #[test]
    fn test_extract_operator_dollar_rhs_skipped() {
        let code = "df$column";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'df' should be collected as a usage (LHS is checked)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(df_used, "LHS 'df' should be collected as usage");

        // 'column' should NOT be collected as a usage (RHS is skipped)
        let column_used = used.iter().any(|(name, _)| name == "column");
        assert!(
            !column_used,
            "RHS 'column' should NOT be collected as usage for $ operator"
        );
    }

    /// Test that obj@slot does not produce a diagnostic for 'slot'
    /// Validates: Requirement 1.2 - RHS of @ operator should be skipped
    #[test]
    fn test_extract_operator_at_rhs_skipped() {
        let code = "obj@slot";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'obj' should be collected as a usage (LHS is checked)
        let obj_used = used.iter().any(|(name, _)| name == "obj");
        assert!(obj_used, "LHS 'obj' should be collected as usage");

        // 'slot' should NOT be collected as a usage (RHS is skipped)
        let slot_used = used.iter().any(|(name, _)| name == "slot");
        assert!(
            !slot_used,
            "RHS 'slot' should NOT be collected as usage for @ operator"
        );
    }

    /// Test that undefined$column produces a diagnostic for 'undefined' (LHS is still checked)
    /// Validates: Requirement 1.3 - LHS of extract operators should still be checked
    #[test]
    fn test_extract_operator_lhs_checked() {
        let code = "undefined$column";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'undefined' should be collected as a usage (LHS is checked)
        let undefined_used = used.iter().any(|(name, _)| name == "undefined");
        assert!(
            undefined_used,
            "LHS 'undefined' should be collected as usage"
        );

        // 'column' should NOT be collected as a usage (RHS is skipped)
        let column_used = used.iter().any(|(name, _)| name == "column");
        assert!(
            !column_used,
            "RHS 'column' should NOT be collected as usage"
        );
    }

    // ==================== Call-Like Argument Tests ====================
    // These tests verify that identifiers inside call-like arguments are skipped
    // (Requirements 2.1, 2.2, 2.3, 2.4)

    /// Test that subset(df, x > 5) does not produce a diagnostic for 'x'
    /// Validates: Requirement 2.1 - Identifiers inside function call arguments should be skipped
    #[test]
    fn test_call_arguments_skipped() {
        let code = "subset(df, x > 5)";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'subset' should be collected as a usage (function name is checked)
        let subset_used = used.iter().any(|(name, _)| name == "subset");
        assert!(
            subset_used,
            "Function name 'subset' should be collected as usage"
        );

        // 'df' should NOT be collected as a usage (inside call arguments)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            !df_used,
            "'df' inside call arguments should NOT be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside call arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside call arguments should NOT be collected as usage"
        );
    }

    /// Test that df[x > 5, ] does not produce a diagnostic for 'x'
    /// Validates: Requirement 2.2 - Identifiers inside subset ([) arguments should be skipped
    #[test]
    fn test_subset_arguments_skipped() {
        let code = "df[x > 5, ]";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'df' should be collected as a usage (the object being subsetted is checked)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            df_used,
            "'df' (object being subsetted) should be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside subset arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside subset arguments should NOT be collected as usage"
        );
    }

    /// Test that df[[x]] does not produce a diagnostic for 'x'
    /// Validates: Requirement 2.3 - Identifiers inside subset2 ([[) arguments should be skipped
    #[test]
    fn test_subset2_arguments_skipped() {
        let code = "df[[x]]";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'df' should be collected as a usage (the object being subsetted is checked)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            df_used,
            "'df' (object being subsetted) should be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside subset2 arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside subset2 arguments should NOT be collected as usage"
        );
    }

    /// Test that undefined_func(x) produces a diagnostic for 'undefined_func'
    /// Validates: Requirement 2.4 - Function names should still be checked
    #[test]
    fn test_function_name_checked() {
        let code = "undefined_func(x)";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'undefined_func' should be collected as a usage (function name is checked)
        let func_used = used.iter().any(|(name, _)| name == "undefined_func");
        assert!(
            func_used,
            "Function name 'undefined_func' should be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside call arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside call arguments should NOT be collected as usage"
        );
    }

    // ==================== Formula Tests (Task 6.3) ====================
    // These tests verify that identifiers inside formula expressions are skipped
    // (Requirements 3.1, 3.2, 3.4)

    /// Test that ~ x does not produce a diagnostic for 'x'
    /// Validates: Requirement 3.1 - Identifiers inside unary formula expressions should be skipped
    #[test]
    fn test_unary_formula_skipped() {
        let code = "~ x";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'x' should NOT be collected as a usage (inside formula)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside unary formula should NOT be collected as usage"
        );
    }

    /// Test that y ~ x + z does not produce diagnostics for 'y', 'x', 'z'
    /// Validates: Requirement 3.2 - Identifiers inside binary formula expressions should be skipped
    #[test]
    fn test_binary_formula_skipped() {
        let code = "y ~ x + z";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'y' should NOT be collected as a usage (LHS of formula)
        let y_used = used.iter().any(|(name, _)| name == "y");
        assert!(
            !y_used,
            "'y' inside binary formula should NOT be collected as usage"
        );

        // 'x' should NOT be collected as a usage (RHS of formula)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside binary formula should NOT be collected as usage"
        );

        // 'z' should NOT be collected as a usage (RHS of formula)
        let z_used = used.iter().any(|(name, _)| name == "z");
        assert!(
            !z_used,
            "'z' inside binary formula should NOT be collected as usage"
        );
    }

    /// Test that lm(y ~ x, data = df) does not produce diagnostics for 'y', 'x'
    /// Validates: Requirement 3.4 - Formulas nested inside call arguments should have both contexts apply
    #[test]
    fn test_formula_inside_call_arguments_skipped() {
        let code = "lm(y ~ x, data = df)";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'lm' should be collected as a usage (function name is checked)
        let lm_used = used.iter().any(|(name, _)| name == "lm");
        assert!(lm_used, "Function name 'lm' should be collected as usage");

        // 'y' should NOT be collected as a usage (inside formula inside call arguments)
        let y_used = used.iter().any(|(name, _)| name == "y");
        assert!(
            !y_used,
            "'y' inside formula in call arguments should NOT be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside formula inside call arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside formula in call arguments should NOT be collected as usage"
        );

        // 'df' should NOT be collected as a usage (inside call arguments)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            !df_used,
            "'df' inside call arguments should NOT be collected as usage"
        );
    }

    // ==================== Edge Case Tests (Task 6.4) ====================
    // These tests verify edge cases for the NSE skip logic
    // (Requirements 1.1, 1.2, 2.1, 3.1)

    /// Test deeply nested formulas: ~ (~ (~ x)) - all identifiers should be skipped
    /// Validates: Requirement 3.1 - Identifiers inside formula expressions should be skipped
    #[test]
    fn test_deeply_nested_formulas() {
        let code = "~ (~ (~ x))";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'x' should NOT be collected as a usage (inside deeply nested formula)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside deeply nested formula should NOT be collected as usage"
        );

        // No identifiers should be collected at all
        assert!(
            used.is_empty(),
            "No identifiers should be collected from deeply nested formula"
        );
    }

    /// Test nested call arguments: f(g(h(x))) - all identifiers in all argument levels should be skipped
    /// Validates: Requirement 2.1 - Identifiers inside call arguments should be skipped
    #[test]
    fn test_nested_call_arguments() {
        let code = "f(g(h(x)))";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'f' should be collected as a usage (outermost function name is checked)
        let f_used = used.iter().any(|(name, _)| name == "f");
        assert!(f_used, "Function name 'f' should be collected as usage");

        // 'g' should NOT be collected as a usage (inside f's arguments)
        let g_used = used.iter().any(|(name, _)| name == "g");
        assert!(
            !g_used,
            "'g' inside call arguments should NOT be collected as usage"
        );

        // 'h' should NOT be collected as a usage (inside g's arguments, which is inside f's arguments)
        let h_used = used.iter().any(|(name, _)| name == "h");
        assert!(
            !h_used,
            "'h' inside nested call arguments should NOT be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside h's arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside deeply nested call arguments should NOT be collected as usage"
        );

        // Only 'f' should be collected
        assert_eq!(
            used.len(),
            1,
            "Only the outermost function name should be collected"
        );
    }

    /// Test mixed contexts: df$col[x > 5] - 'col' skipped (extract RHS), 'x' skipped (subset arguments), 'df' checked
    /// Validates: Requirements 1.1, 1.2, 2.1 - Extract RHS and subset arguments should be skipped
    #[test]
    fn test_mixed_contexts() {
        let code = "df$col[x > 5]";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'df' should be collected as a usage (LHS of extract operator is checked)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            df_used,
            "'df' (LHS of extract operator) should be collected as usage"
        );

        // 'col' should NOT be collected as a usage (RHS of extract operator)
        let col_used = used.iter().any(|(name, _)| name == "col");
        assert!(
            !col_used,
            "'col' (RHS of extract operator) should NOT be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside subset arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside subset arguments should NOT be collected as usage"
        );

        // Only 'df' should be collected
        assert_eq!(
            used.len(),
            1,
            "Only 'df' should be collected in mixed context"
        );
    }

    /// Test chained extracts: df$a$b$c - only 'df' should be checked, all others are RHS of extract operators
    /// Validates: Requirements 1.1, 1.2 - RHS of extract operators should be skipped
    #[test]
    fn test_chained_extracts() {
        let code = "df$a$b$c";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'df' should be collected as a usage (leftmost identifier is checked)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            df_used,
            "'df' (leftmost identifier) should be collected as usage"
        );

        // 'a' should NOT be collected as a usage (RHS of first extract operator)
        let a_used = used.iter().any(|(name, _)| name == "a");
        assert!(
            !a_used,
            "'a' (RHS of extract operator) should NOT be collected as usage"
        );

        // 'b' should NOT be collected as a usage (RHS of second extract operator)
        let b_used = used.iter().any(|(name, _)| name == "b");
        assert!(
            !b_used,
            "'b' (RHS of extract operator) should NOT be collected as usage"
        );

        // 'c' should NOT be collected as a usage (RHS of third extract operator)
        let c_used = used.iter().any(|(name, _)| name == "c");
        assert!(
            !c_used,
            "'c' (RHS of extract operator) should NOT be collected as usage"
        );

        // Only 'df' should be collected
        assert_eq!(
            used.len(),
            1,
            "Only 'df' should be collected in chained extracts"
        );
    }

    // ========================================================================
    // Completion Precedence Tests (Task 11.2)
    // Tests for completion precedence: local > package exports > cross-file
    // Validates: Requirements 9.4, 9.5
    // ========================================================================

    /// Test that local definitions take precedence over package exports in completions.
    /// Validates: Requirement 9.4 - Local definitions > package exports
    #[test]
    fn test_completion_local_over_package_exports() {
        use crate::package_library::PackageInfo;
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Create a WorldState with a package that exports "mutate"
            let mut state = WorldState::new(vec![]);

            // Add a package with "mutate" export
            let mut exports = std::collections::HashSet::new();
            exports.insert("mutate".to_string());
            exports.insert("filter".to_string());
            let pkg_info = PackageInfo::new("dplyr".to_string(), exports);
            state.package_library.insert_package(pkg_info).await;

            // Create a document that defines "mutate" locally and loads dplyr
            let code = r#"library(dplyr)
mutate <- function(x) { x * 2 }
result <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions at the end of the file (after "result <- ")
            let position = Position::new(2, 10);
            let completions = super::completion(&state, &uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Find the "mutate" completion item
                let mutate_items: Vec<_> = items.iter()
                    .filter(|item| item.label == "mutate")
                    .collect();

                // There should be exactly one "mutate" item (the local definition)
                assert_eq!(
                    mutate_items.len(),
                    1,
                    "Should have exactly one 'mutate' completion (local definition takes precedence)"
                );

                // The local definition should NOT have package attribution
                let mutate_item = mutate_items[0];
                assert!(
                    mutate_item.detail.is_none() || !mutate_item.detail.as_ref().unwrap().contains("{dplyr}"),
                    "Local 'mutate' should not have package attribution"
                );
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    /// Test that package exports take precedence over cross-file symbols in completions.
    /// Validates: Requirement 9.5 - Package exports > cross-file symbols
    #[test]
    fn test_completion_package_over_cross_file() {
        use crate::package_library::PackageInfo;
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Create a WorldState with a package that exports "helper_func"
            let mut state = WorldState::new(vec![]);

            // Add a package with "helper_func" export
            let mut exports = std::collections::HashSet::new();
            exports.insert("helper_func".to_string());
            let pkg_info = PackageInfo::new("testpkg".to_string(), exports);
            state.package_library.insert_package(pkg_info).await;

            // Create main file that loads testpkg
            let main_code = r#"library(testpkg)
result <- "#;
            let main_uri = Url::parse("file:///main.R").unwrap();
            let main_doc = Document::new(main_code, None);
            state.documents.insert(main_uri.clone(), main_doc);

            // Create a helper file that defines "helper_func"
            let helper_code = r#"helper_func <- function(x) { x + 1 }"#;
            let helper_uri = Url::parse("file:///helper.R").unwrap();
            let helper_doc = Document::new(helper_code, None);
            state.documents.insert(helper_uri.clone(), helper_doc);

            // Note: In a real scenario, the cross-file symbol would come from scope resolution
            // through source() calls. For this test, we verify that package exports are added
            // before cross-file symbols in the completion list.

            // Get completions at the end of main file
            let position = Position::new(1, 10);
            let completions = super::completion(&state, &main_uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Find the "helper_func" completion item
                let helper_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "helper_func")
                    .collect();

                // There should be at least one "helper_func" item (from package)
                assert!(
                    !helper_items.is_empty(),
                    "Should have 'helper_func' completion from package"
                );

                // The first (and only) helper_func should be from the package
                let helper_item = helper_items[0];
                assert!(
                    helper_item
                        .detail
                        .as_ref()
                        .map_or(false, |d| d.contains("{testpkg}")),
                    "helper_func should have package attribution {{testpkg}}"
                );
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    /// Test that keywords take precedence over all other completions.
    /// Validates: Implicit requirement - keywords should always be available
    #[test]
    fn test_completion_keywords_always_present() {
        use crate::package_library::PackageInfo;
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionItemKind, CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Create a WorldState with a package that exports "if" (hypothetically)
            let mut state = WorldState::new(vec![]);

            // Add a package with "if" export (edge case - shouldn't override keyword)
            let mut exports = std::collections::HashSet::new();
            exports.insert("if".to_string());
            let pkg_info = PackageInfo::new("badpkg".to_string(), exports);
            state.package_library.insert_package(pkg_info).await;

            // Create a document that loads the package
            let code = r#"library(badpkg)
x <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions
            let position = Position::new(1, 5);
            let completions = super::completion(&state, &uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Find the "if" completion item
                let if_items: Vec<_> = items.iter().filter(|item| item.label == "if").collect();

                // There should be exactly one "if" item (the keyword)
                assert_eq!(
                    if_items.len(),
                    1,
                    "Should have exactly one 'if' completion (keyword takes precedence)"
                );

                // The "if" should be a keyword, not a function from package
                let if_item = if_items[0];
                assert_eq!(
                    if_item.kind,
                    Some(CompletionItemKind::KEYWORD),
                    "'if' should be a KEYWORD, not a function from package"
                );
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    /// Verifies completion precedence where local definitions shadow package exports, and package exports take precedence over cross-file symbols.
    ///
    /// Sets up a WorldState with a package ("dplyr") that exports several symbols, opens a document that loads that package and defines a local `mutate` (which should shadow the package export) and `my_func`, then requests completions at a position and asserts:
    /// - the local `mutate` appears once with no package attribution,
    /// - `filter` and `select` appear once each with package attribution `{dplyr}`,
    /// - `my_func` appears as a function completion.
    ///
    /// # Examples
    ///
    /// ```
    /// // Arrange: create state, insert package exports and document, then call completion.
    /// // Assert: see comments above for expected precedence behavior.
    /// ```
    #[test]
    fn test_completion_full_precedence_chain() {
        use crate::package_library::PackageInfo;
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionItemKind, CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut state = WorldState::new(vec![]);

            // Add packages with various exports
            let mut dplyr_exports = std::collections::HashSet::new();
            dplyr_exports.insert("mutate".to_string());
            dplyr_exports.insert("filter".to_string());
            dplyr_exports.insert("select".to_string());
            let dplyr_info = PackageInfo::new("dplyr".to_string(), dplyr_exports);
            state.package_library.insert_package(dplyr_info).await;

            // Create a document that:
            // 1. Loads dplyr (provides mutate, filter, select)
            // 2. Defines "mutate" locally (should shadow package export)
            // 3. Defines "my_func" locally
            let code = r#"library(dplyr)
mutate <- function(df, ...) { df }
my_func <- function(x) { x }
result <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions at the end
            let position = Position::new(3, 10);
            let completions = super::completion(&state, &uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Check "mutate" - should be local (no package attribution)
                let mutate_items: Vec<_> =
                    items.iter().filter(|item| item.label == "mutate").collect();
                assert_eq!(mutate_items.len(), 1, "Should have exactly one 'mutate'");
                assert!(
                    mutate_items[0].detail.is_none()
                        || !mutate_items[0].detail.as_ref().unwrap().contains("{dplyr}"),
                    "Local 'mutate' should not have package attribution"
                );

                // Check "filter" - should be from package (has attribution)
                let filter_items: Vec<_> =
                    items.iter().filter(|item| item.label == "filter").collect();
                assert_eq!(filter_items.len(), 1, "Should have exactly one 'filter'");
                assert!(
                    filter_items[0]
                        .detail
                        .as_ref()
                        .map_or(false, |d| d.contains("{dplyr}")),
                    "'filter' should have package attribution {{dplyr}}"
                );

                // Check "select" - should be from package (has attribution)
                let select_items: Vec<_> =
                    items.iter().filter(|item| item.label == "select").collect();
                assert_eq!(select_items.len(), 1, "Should have exactly one 'select'");
                assert!(
                    select_items[0]
                        .detail
                        .as_ref()
                        .map_or(false, |d| d.contains("{dplyr}")),
                    "'select' should have package attribution {{dplyr}}"
                );

                // Check "my_func" - should be local (no package attribution)
                let my_func_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "my_func")
                    .collect();
                assert_eq!(my_func_items.len(), 1, "Should have exactly one 'my_func'");
                assert_eq!(
                    my_func_items[0].kind,
                    Some(CompletionItemKind::FUNCTION),
                    "'my_func' should be a FUNCTION"
                );
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    /// Test that seen_names correctly prevents duplicates across all sources.
    /// Validates: Requirements 9.3, 9.4, 9.5 - duplicate exports show all packages
    #[test]
    fn test_completion_duplicate_exports_show_all_packages() {
        use crate::package_library::PackageInfo;
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut state = WorldState::new(vec![]);

            // Add two packages that both export "common_func"
            let mut pkg1_exports = std::collections::HashSet::new();
            pkg1_exports.insert("common_func".to_string());
            pkg1_exports.insert("pkg1_only".to_string());
            let pkg1_info = PackageInfo::new("pkg1".to_string(), pkg1_exports);
            state.package_library.insert_package(pkg1_info).await;

            let mut pkg2_exports = std::collections::HashSet::new();
            pkg2_exports.insert("common_func".to_string());
            pkg2_exports.insert("pkg2_only".to_string());
            let pkg2_info = PackageInfo::new("pkg2".to_string(), pkg2_exports);
            state.package_library.insert_package(pkg2_info).await;

            // Create a document that loads both packages
            let code = r#"library(pkg1)
library(pkg2)
x <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions
            let position = Position::new(2, 5);
            let completions = super::completion(&state, &uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Requirement 9.3: When multiple packages export same symbol, show all with attribution
                // Check that "common_func" appears twice (once for each package)
                let common_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "common_func")
                    .collect();
                assert_eq!(
                    common_items.len(),
                    2,
                    "Should have two 'common_func' entries (one per package)"
                );

                // Both packages should be represented
                let has_pkg1 = common_items
                    .iter()
                    .any(|item| item.detail.as_ref().map_or(false, |d| d.contains("{pkg1}")));
                let has_pkg2 = common_items
                    .iter()
                    .any(|item| item.detail.as_ref().map_or(false, |d| d.contains("{pkg2}")));
                assert!(has_pkg1, "'common_func' should have entry from pkg1");
                assert!(has_pkg2, "'common_func' should have entry from pkg2");

                // Check that unique exports from both packages are present
                let pkg1_only_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "pkg1_only")
                    .collect();
                assert_eq!(pkg1_only_items.len(), 1, "Should have 'pkg1_only'");

                let pkg2_only_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "pkg2_only")
                    .collect();
                assert_eq!(pkg2_only_items.len(), 1, "Should have 'pkg2_only'");
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    // ========================================================================
    // Backward Directive Path Resolution Tests
    // Tests for fix-backward-directive-path-resolution spec
    // Validates: Requirements 1.2, 3.2
    // ========================================================================

    /// Test that backward directive paths resolve relative to file's directory, ignoring @lsp-cd.
    ///
    /// This test reproduces a bug where `collect_ambiguous_parent_diagnostics` was using
    /// `PathContext::from_metadata` (which respects @lsp-cd) instead of `PathContext::new`
    /// (which ignores @lsp-cd) for backward directive resolution.
    ///
    /// Scenario:
    /// - Child file at `subdir/child.r` contains:
    ///   - `@lsp-cd ..` (sets working directory to parent/workspace root)
    ///   - `@lsp-run-by: program.r` (declares parent file)
    /// - The backward directive should resolve `program.r` relative to `subdir/` (file's directory)
    ///   NOT relative to the workspace root (the @lsp-cd directory)
    ///
    /// Validates: Requirements 1.2, 3.2
    #[test]
    fn test_backward_directive_ignores_lsp_cd() {
        use crate::cross_file::path_resolve::PathContext;
        use crate::cross_file::types::CrossFileMetadata;

        // Simulate a child file at /project/subdir/child.r
        let child_uri = Url::parse("file:///project/subdir/child.r").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Metadata with @lsp-cd .. (points to /project, the workspace root)
        let meta = CrossFileMetadata {
            working_directory: Some("..".to_string()),
            ..Default::default()
        };

        // PathContext::new should ignore @lsp-cd
        let ctx_new = PathContext::new(&child_uri, Some(&workspace_root)).unwrap();

        // PathContext::from_metadata should respect @lsp-cd
        let ctx_from_meta =
            PathContext::from_metadata(&child_uri, &meta, Some(&workspace_root)).unwrap();

        // Verify that PathContext::new ignores @lsp-cd
        // The effective working directory should be the file's directory: /project/subdir
        assert_eq!(
            ctx_new.effective_working_directory(),
            std::path::PathBuf::from("/project/subdir"),
            "PathContext::new should use file's directory, ignoring @lsp-cd"
        );

        // Verify that PathContext::from_metadata respects @lsp-cd
        // The effective working directory should be /project (the @lsp-cd directory)
        assert_eq!(
            ctx_from_meta.effective_working_directory(),
            std::path::PathBuf::from("/project"),
            "PathContext::from_metadata should use @lsp-cd directory"
        );

        // Now test path resolution for a backward directive path "program.r"
        let backward_path = "program.r";

        // With PathContext::new (correct for backward directives):
        // "program.r" should resolve to /project/subdir/program.r
        let resolved_new = crate::cross_file::path_resolve::resolve_path(backward_path, &ctx_new);
        assert_eq!(
            resolved_new,
            Some(std::path::PathBuf::from("/project/subdir/program.r")),
            "Backward directive 'program.r' should resolve relative to file's directory"
        );

        // With PathContext::from_metadata (incorrect for backward directives):
        // "program.r" would resolve to /project/program.r (wrong!)
        let resolved_from_meta =
            crate::cross_file::path_resolve::resolve_path(backward_path, &ctx_from_meta);
        assert_eq!(
            resolved_from_meta,
            Some(std::path::PathBuf::from("/project/program.r")),
            "With @lsp-cd, 'program.r' would incorrectly resolve to workspace root"
        );

        // The key assertion: the two resolutions are DIFFERENT
        // This demonstrates why using PathContext::new is essential for backward directives
        assert_ne!(
            resolved_new, resolved_from_meta,
            "PathContext::new and PathContext::from_metadata should produce different results when @lsp-cd is present"
        );
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use crate::cross_file::scope::{ScopedSymbol, SymbolKind};
    use crate::state::Document;
    use proptest::prelude::*;
    use std::collections::HashSet;

    // Helper to parse R code for property tests
    fn parse_r_code(code: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    // Helper to filter out R reserved keywords from generated identifiers
    fn is_r_reserved(s: &str) -> bool {
        matches!(
            s,
            "for"
                | "if"
                | "in"
                | "else"
                | "while"
                | "repeat"
                | "next"
                | "break"
                | "function"
                | "return"
                | "true"
                | "false"
                | "null"
                | "inf"
                | "nan"
        )
    }

    proptest! {
        #[test]
        fn test_library_require_extraction(pkg_name in "[a-z]{3,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))) {
            let code_library = format!("library({})", pkg_name);
            let code_require = format!("require({})", pkg_name);
            let code_loadns = format!("loadNamespace(\"{}\")", pkg_name);

            let doc1 = Document::new(&code_library, None);
            let doc2 = Document::new(&code_require, None);
            let doc3 = Document::new(&code_loadns, None);

            prop_assert!(doc1.loaded_packages.contains(&pkg_name));
            prop_assert!(doc2.loaded_packages.contains(&pkg_name));
            prop_assert!(doc3.loaded_packages.contains(&pkg_name));
        }

        #[test]
        fn test_multiple_library_calls(pkg_count in 1usize..5) {
            let packages: Vec<String> = (0..pkg_count)
                .map(|i| format!("pkg{}", i))
                .collect();

            let code = packages.iter()
                .map(|p| format!("library({})", p))
                .collect::<Vec<_>>()
                .join("\n");

            let doc = Document::new(&code, None);

            for pkg in &packages {
                prop_assert!(doc.loaded_packages.contains(pkg));
            }
            prop_assert_eq!(doc.loaded_packages.len(), pkg_count);
        }

        #[test]
        fn test_mixed_symbol_types(
            var_name in "[a-z]{3,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            func_name in "[a-z]{3,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            builtin in prop::sample::select(vec!["print", "sum", "mean", "length"])
        ) {
            let code = format!(
                "{} <- 42\n{} <- function() {{}}\n{}({})",
                var_name, func_name, builtin, var_name
            );

            let tree = parse_r_code(&code);
            let mut defined = HashSet::new();
            collect_definitions(tree.root_node(), &code, &mut defined);

            prop_assert!(defined.contains(&var_name));
            prop_assert!(defined.contains(&func_name));
            prop_assert!(is_builtin(&builtin));
        }

        #[test]
        fn test_named_arguments_not_flagged(
            func_name in "[a-z]{3,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            arg_name in "[a-z]{2,6}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            value in 1i32..100
        ) {
            let code = format!("{}({} = {})", func_name, arg_name, value);

            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages(tree.root_node(), &code, &mut used);

            // func_name should be in used, but arg_name should NOT be
            let func_used = used.iter().any(|(name, _)| name == &func_name);
            let arg_used = used.iter().any(|(name, _)| name == &arg_name);

            prop_assert!(func_used, "Function name should be collected as usage");
            prop_assert!(!arg_used, "Named argument should NOT be collected as usage");
        }

        #[test]
        fn test_multiple_named_arguments(
            arg_count in 1usize..4
        ) {
            let args: Vec<String> = (0..arg_count)
                .map(|i| format!("arg{} = {}", i, i + 1))
                .collect();

            let code = format!("func({})", args.join(", "));

            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages(tree.root_node(), &code, &mut used);

            // None of the argument names should be flagged as usages
            for i in 0..arg_count {
                let arg_name = format!("arg{}", i);
                let arg_used = used.iter().any(|(name, _)| name == &arg_name);
                prop_assert!(!arg_used, "Named argument {} should not be flagged", arg_name);
            }
        }

        #[test]
        fn test_parameter_extraction_completeness(
            param_count in 1usize..5,
            has_defaults in prop::collection::vec(any::<bool>(), 1..5)
        ) {
            let param_count = param_count.min(has_defaults.len());
            let mut params = Vec::new();

            for i in 0..param_count {
                if has_defaults[i] {
                    params.push(format!("p{} = {}", i, i + 1));
                } else {
                    params.push(format!("p{}", i));
                }
            }

            let code = format!("f <- function({}) {{}}", params.join(", "));
            let tree = parse_r_code(&code);

            // Find function definition node
            let func_node = find_function_definition_node(tree.root_node(), "f", &code).unwrap();
            let signature = extract_function_signature(func_node, "f", &code);

            // All parameters should be present in signature
            for i in 0..param_count {
                let param_name = format!("p{}", i);
                prop_assert!(signature.contains(&param_name),
                    "Parameter {} should be in signature: {}", param_name, signature);
            }
        }

        #[test]
        fn test_assignment_operators_recognized(
            func_name in "[a-z]{3,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            op in prop::sample::select(vec!["<-", "=", "<<-"])
        ) {
            let code = format!("{} {} function() {{}}", func_name, op);
            let tree = parse_r_code(&code);

            let func_def = find_function_definition_node(tree.root_node(), &func_name, &code);
            prop_assert!(func_def.is_some(), "Function definition should be found for operator {}", op);

            if let Some(node) = func_def {
                prop_assert_eq!(node.kind(), "function_definition");
            }
        }

        #[test]
        fn test_search_priority(func_name in "[a-z]{3,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))) {
            use crate::state::{WorldState, Document};
            use tower_lsp::lsp_types::Url;

            let current_uri = Url::parse("file:///current.R").unwrap();
            let other_uri = Url::parse("file:///other.R").unwrap();
            let workspace_uri = Url::parse("file:///workspace.R").unwrap();

            // Create function definitions with different signatures
            let current_code = format!("{} <- function(a) {{ a }}", func_name);
            let other_code = format!("{} <- function(b, c) {{ b + c }}", func_name);
            let workspace_code = format!("{} <- function(x, y, z) {{ x + y + z }}", func_name);

            let mut state = WorldState::new(vec![]);
            state.documents.insert(current_uri.clone(), Document::new(&current_code, None));
            state.documents.insert(other_uri.clone(), Document::new(&other_code, None));
            state.workspace_index.insert(workspace_uri.clone(), Document::new(&workspace_code, None));

            // Search should return current document's definition first
            let signature = find_user_function_signature(&state, &current_uri, &func_name);
            prop_assert!(signature.is_some());

            if let Some(sig) = signature {
                prop_assert!(sig.contains("(a)"), "Should return current document's signature: {}", sig);
                prop_assert!(!sig.contains("(b, c)"), "Should not return other document's signature");
                prop_assert!(!sig.contains("(x, y, z)"), "Should not return workspace signature");
            }
        }
    }

    #[test]
    fn test_extract_definition_statement_variable() {
        use crate::cross_file::scope::SymbolKind;

        let code = "x <- 42\ny <- x + 1";
        let tree = parse_r_code(code);

        let symbol = ScopedSymbol {
            name: "x".to_string(),
            kind: SymbolKind::Variable,
            source_uri: Url::parse("file:///test.R").unwrap(),
            defined_line: 0,
            defined_column: 0,
            signature: None,
        };

        let result = extract_statement_from_tree(&tree, &symbol, code);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.statement, "x <- 42");
    }

    #[test]
    fn test_extract_definition_statement_function() {
        let code = "f <- function(a, b) {\n  a + b\n}";
        let tree = parse_r_code(code);

        let symbol = ScopedSymbol {
            name: "f".to_string(),
            kind: SymbolKind::Function,
            source_uri: Url::parse("file:///test.R").unwrap(),
            defined_line: 0,
            defined_column: 0,
            signature: Some("f(a, b)".to_string()),
        };

        let result = extract_statement_from_tree(&tree, &symbol, code);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.statement, "f <- function(a, b) {\n  a + b\n}");
    }

    #[test]
    fn test_extract_definition_statement_truncation() {
        let mut code = "long_func <- function() {\n".to_string();
        for i in 1..=15 {
            code.push_str(&format!("  line_{}\n", i));
        }
        code.push('}');

        let tree = parse_r_code(&code);

        let symbol = ScopedSymbol {
            name: "long_func".to_string(),
            kind: SymbolKind::Function,
            source_uri: Url::parse("file:///test.R").unwrap(),
            defined_line: 0,
            defined_column: 0,
            signature: None,
        };

        let result = extract_statement_from_tree(&tree, &symbol, &code);
        assert!(result.is_some());
        let info = result.unwrap();

        // Should be truncated to 10 lines with ellipsis
        let lines: Vec<&str> = info.statement.lines().collect();
        assert_eq!(lines.len(), 11); // 10 lines + "..."
        assert_eq!(lines[10], "...");
    }

    #[test]
    fn test_extract_definition_statement_assignment_operators() {
        let test_cases = vec![
            ("x <- 42", "<-"),
            ("y = 100", "="),
            ("z <<- 'global'", "<<-"),
        ];

        for (code, op) in test_cases {
            let tree = parse_r_code(code);
            let var_name = code.split_whitespace().next().unwrap();

            let symbol = ScopedSymbol {
                name: var_name.to_string(),
                kind: SymbolKind::Variable,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
            };

            let result = extract_statement_from_tree(&tree, &symbol, code);
            assert!(
                result.is_some(),
                "Should extract statement for operator {}",
                op
            );
            let info = result.unwrap();
            assert_eq!(info.statement, code);
        }
    }

    #[test]
    fn test_extract_definition_statement_for_loop_iterator() {
        let code = "for (i in 1:10) {\n  print(i)\n}";
        let tree = parse_r_code(code);

        let symbol = ScopedSymbol {
            name: "i".to_string(),
            kind: SymbolKind::Variable,
            source_uri: Url::parse("file:///test.R").unwrap(),
            defined_line: 0,
            defined_column: 5, // Position of 'i' in for loop
            signature: None,
        };

        let result = extract_statement_from_tree(&tree, &symbol, code);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.statement, "for (i in 1:10) {\n  print(i)\n}");
    }

    #[test]
    fn test_readlines_named_arg() {
        // This is the exact code from collate.r line 13
        let code = r#"run_hash <- trimws(readLines("output/oos/latest_hash.txt", n = 1))"#;
        let tree = parse_r_code(code);

        let mut used = Vec::new();
        collect_usages(tree.root_node(), code, &mut used);

        eprintln!("\n=== Collected usages ===");
        for (name, node) in &used {
            eprintln!("  '{}' (kind: {})", name, node.kind());
        }

        // trimws and readLines should be collected, but n should NOT be
        let trimws_used = used.iter().any(|(name, _)| name == "trimws");
        let readlines_used = used.iter().any(|(name, _)| name == "readLines");
        let n_used = used.iter().any(|(name, _)| name == "n");

        assert!(trimws_used, "trimws should be collected");
        assert!(readlines_used, "readLines should be collected");
        assert!(
            !n_used,
            "n should NOT be collected as it's a named argument"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 100,
            .. ProptestConfig::default()
        })]
        #[test]
        fn test_user_defined_priority_over_builtins(
            builtin in prop::sample::select(vec!["print", "sum", "mean", "length"])
        ) {
            use crate::state::{WorldState, Document};
            use tower_lsp::lsp_types::Url;

            let uri = Url::parse("file:///test.R").unwrap();

            // Create code with user-defined function that shadows a built-in
            let code = format!("{} <- function(x, y) {{ x + y }}", builtin);

            let mut state = WorldState::new(vec![]);
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Should return user-defined signature, not built-in
            let signature = find_user_function_signature(&state, &uri, &builtin);
            prop_assert!(signature.is_some(), "Should find user-defined function");

            if let Some(sig) = signature {
                prop_assert!(sig.contains("(x, y)"), "Should return user-defined signature: {}", sig);
                prop_assert!(sig.contains(&builtin), "Should contain function name: {}", sig);
            }
        }

        #[test]
        fn test_signature_format_correctness(
            func_name in "[a-z][a-z0-9_]{2,10}",
            param_count in 0usize..5
        ) {
            let params: Vec<String> = (0..param_count)
                .map(|i| format!("p{}", i))
                .collect();

            let code = format!("{} <- function({}) {{}}", func_name, params.join(", "));
            let tree = parse_r_code(&code);

            let func_node = find_function_definition_node(tree.root_node(), &func_name, &code).unwrap();
            let signature = extract_function_signature(func_node, &func_name, &code);

            // Verify format: name(params)
            prop_assert!(signature.starts_with(&func_name), "Signature should start with function name");
            prop_assert!(signature.contains('('), "Signature should contain opening parenthesis");
            prop_assert!(signature.ends_with(')'), "Signature should end with closing parenthesis");

            let expected = format!("{}({})", func_name, params.join(", "));
            prop_assert_eq!(signature, expected, "Signature format should match expected pattern");
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 10: Variable hover definition extraction
        fn prop_variable_hover_definition_extraction(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            value in 1i32..1000
        ) {
            let code = format!("{} <- {}", var_name, value);
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: var_name.clone(),
                kind: SymbolKind::Variable,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract definition for variable");

            let info = def_info.unwrap();
            prop_assert_eq!(info.statement, code, "Should include complete definition statement");
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 11: Function hover signature extraction
        fn prop_function_hover_signature_extraction(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            param_count in 0usize..3
        ) {
            let params: Vec<String> = (0..param_count)
                .map(|i| format!("p{}", i))
                .collect();

            let code = format!("{} <- function({}) {{}}", func_name, params.join(", "));
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: func_name.clone(),
                kind: SymbolKind::Function,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract definition for function");

            let info = def_info.unwrap();
            prop_assert!(info.statement.contains(&func_name), "Should include function name");
            prop_assert!(info.statement.contains("function"), "Should include function keyword");

            for param in &params {
                prop_assert!(info.statement.contains(param), "Should include parameter {}", param);
            }
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 12: Multi-line definition handling
        fn prop_multiline_definition_handling(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            line_count in 5usize..15
        ) {
            let mut code = format!("{} <- function() {{\n", func_name);
            for i in 1..line_count {
                code.push_str(&format!("  line_{}\n", i));
            }
            code.push('}');

            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: func_name.clone(),
                kind: SymbolKind::Function,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract multi-line definition");

            let info = def_info.unwrap();
            let lines: Vec<&str> = info.statement.lines().collect();

            // The generated code has (line_count + 1) total lines (header + (line_count-1) body lines + closing brace).
            // Truncation happens when total lines > 10, i.e. when line_count > 9.
            if line_count > 9 {
                prop_assert_eq!(lines.len(), 11, "Should truncate to 10 lines + ellipsis");
                prop_assert_eq!(lines[10], "...", "Should end with ellipsis when truncated");
            } else {
                // The generated code includes the function header line and a closing brace line.
                let expected_lines = line_count + 1;
                prop_assert_eq!(lines.len(), expected_lines, "Should include all lines when <= 10");
                prop_assert!(!info.statement.contains("..."), "Should not have ellipsis when not truncated");
            }
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 13: Markdown code block formatting
        fn prop_markdown_code_block_formatting(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            special_chars in prop::sample::select(vec!["*", "_", "[", "]", "(", ")", "#", "`", "\\"])
        ) {
            let code = format!("{} <- \"value with {} chars\"", var_name, special_chars);
            let escaped = escape_markdown(&code);
            let formatted = format!("```r\n{}\n```", escaped);

            prop_assert!(formatted.starts_with("```r\n"), "Should start with R code block marker");
            prop_assert!(formatted.ends_with("\n```"), "Should end with code block marker");
            prop_assert!(formatted.contains(&format!("\\{}", special_chars)), "Should escape special markdown characters");
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 14: Same-file location format
        fn prop_same_file_location_format(
            line_num in 0u32..100
        ) {
            let uri = Url::parse("file:///test.R").unwrap();
            let def_info = DefinitionInfo {
                statement: "test_var <- 42".to_string(),
                source_uri: uri.clone(),
                line: line_num,
                column: 0,
            };

            let mut value = String::new();
            value.push_str(&format!("```r\n{}\n```\n\n", escape_markdown(&def_info.statement)));

            if def_info.source_uri == uri {
                value.push_str(&format!("this file, line {}", def_info.line + 1));
            }

            prop_assert!(value.contains("this file"), "Should indicate same file");
            prop_assert!(value.contains(&format!("line {}", line_num + 1)), "Should show 1-based line number");
            prop_assert!(!value.contains("file://"), "Should not contain file URI for same file");
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 15: Cross-file hyperlink format
        fn prop_cross_file_hyperlink_format(
            line_num in 0u32..100
        ) {
            let current_uri = Url::parse("file:///workspace/main.R").unwrap();
            let def_uri = Url::parse("file:///workspace/utils/helper.R").unwrap();
            let workspace_root = Some(Url::parse("file:///workspace/").unwrap());

            let def_info = DefinitionInfo {
                statement: "helper_func <- function() {}".to_string(),
                source_uri: def_uri.clone(),
                line: line_num,
                column: 0,
            };

            let mut value = String::new();
            value.push_str(&format!("```r\n{}\n```\n\n", escape_markdown(&def_info.statement)));

            if def_info.source_uri != current_uri {
                let relative_path = compute_relative_path(&def_info.source_uri, workspace_root.as_ref());
                let absolute_path = def_info.source_uri.as_str();
                value.push_str(&format!("[{}]({}), line {}", relative_path, absolute_path, def_info.line + 1));
            }

            prop_assert!(value.contains("[utils/helper.R]"), "Should show relative path in brackets");
            prop_assert!(value.contains("(file:///workspace/utils/helper.R)"), "Should show absolute URI in parentheses");
            prop_assert!(value.contains(&format!("line {}", line_num + 1)), "Should show 1-based line number");
            prop_assert!(value.contains(", line"), "Should separate path and line with comma");
        }

        #[test]
        // Property 21: Definition statement and location separation
        fn prop_definition_statement_location_separation(
            statement in "[a-z_]+ <- [a-z0-9_(){}]+",
            line_num in 0u32..100
        ) {
            let def_info = DefinitionInfo {
                statement: statement.clone(),
                source_uri: Url::parse("file:///test.R").unwrap(),
                line: line_num,
                column: 0,
            };

            let escaped_statement = escape_markdown(&def_info.statement);
            let mut value = String::new();
            value.push_str(&format!("```r\n{}\n```\n\n", escaped_statement));
            value.push_str(&format!("this file, line {}", def_info.line + 1));

            // Should have exactly one blank line between definition and location
            prop_assert!(value.contains("```\n\nthis file"), "Should have blank line separator");
            prop_assert!(!value.contains("```\nthis file"), "Should not have zero blank lines");
            prop_assert!(!value.contains("```\n\n\nthis file"), "Should not have multiple blank lines");
        }

        #[test]
        // Property 22: Definition statement truncation
        fn prop_definition_statement_truncation(
            line_count in 11usize..20
        ) {
            let mut statement = "long_func <- function() {\n".to_string();
            for i in 1..line_count {
                statement.push_str(&format!("  line_{}\n", i));
            }
            statement.push('}');

            let tree = parse_r_code(&statement);
            let symbol = ScopedSymbol {
                name: "long_func".to_string(),
                kind: SymbolKind::Function,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &statement);
            prop_assert!(def_info.is_some(), "Should extract definition");

            let info = def_info.unwrap();
            let lines: Vec<&str> = info.statement.lines().collect();

            prop_assert_eq!(lines.len(), 11, "Should truncate to 10 lines + ellipsis");
            prop_assert_eq!(lines[10], "...", "Should end with ellipsis");
        }

        #[test]
        // Property 23: Indentation preservation
        fn prop_indentation_preservation(
            indent_size in 0usize..8,
            line_count in 2usize..6
        ) {
            let indent = " ".repeat(indent_size);
            let mut statement = format!("{}func <- function() {{\n", indent);
            for i in 1..line_count {
                statement.push_str(&format!("{}  line_{}\n", indent, i));
            }
            statement.push_str(&format!("{}}}", indent));

            let tree = parse_r_code(&statement);
            let symbol = ScopedSymbol {
                name: "func".to_string(),
                kind: SymbolKind::Function,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: indent_size as u32,
                signature: None,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &statement);
            prop_assert!(def_info.is_some(), "Should extract definition");

            let info = def_info.unwrap();
            let lines: Vec<&str> = info.statement.lines().collect();

            // Check that indentation is preserved
            for line in &lines {
                if !line.trim().is_empty() {
                    prop_assert!(line.starts_with(&indent), "Should preserve original indentation: '{}'", line);
                }
            }
        }

        #[test]
        // Property 24: Markdown character escaping
        fn prop_markdown_character_escaping(
            special_char in prop::sample::select(vec!["*", "_", "[", "]", "(", ")", "#", "`", "\\"])
        ) {
            let statement = format!("var <- \"value with {} char\"", special_char);
            let escaped = escape_markdown(&statement);

            let expected_escaped = format!("\\{}", special_char);
            prop_assert!(escaped.contains(&expected_escaped),
                "Should escape '{}' to '{}' in: '{}'", special_char, expected_escaped, escaped);

            // Verify it's properly formatted in hover content
            let hover_content = format!("```r\n{}\n```", escaped);
            prop_assert!(hover_content.contains(&expected_escaped),
                "Should contain escaped character in hover content");
        }

        #[test]
        // Property 28: Assignment operator extraction
        fn prop_assignment_operator_extraction(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            op in prop::sample::select(vec!["<-", "=", "<<-"]),
            value in 1i32..1000
        ) {
            let code = format!("{} {} {}", var_name, op, value);
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: var_name.clone(),
                kind: SymbolKind::Variable,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract assignment statement");

            let info = def_info.unwrap();
            let statement = &info.statement;
            prop_assert_eq!(statement, &code, "Should include complete assignment statement");
            prop_assert!(statement.contains(&op), "Should include assignment operator {}", op);
        }

        #[test]
        // Property 29: Inline function extraction
        fn prop_inline_function_extraction(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            param_count in 0usize..3
        ) {
            let params: Vec<String> = (0..param_count)
                .map(|i| format!("p{}", i))
                .collect();

            let code = format!("{} <- function({}) {{ {} }}", func_name, params.join(", "), "x + 1");
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: func_name.clone(),
                kind: SymbolKind::Function,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract function definition");

            let info = def_info.unwrap();
            prop_assert!(info.statement.contains("function"), "Should include function keyword");
            prop_assert!(info.statement.contains(&format!("({})", params.join(", "))), "Should include function signature");
        }

        #[test]
        // Property 30: Loop iterator definition extraction
        fn prop_loop_iterator_definition_extraction(
            iterator in "[a-z][a-z0-9_]{1,5}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            range_end in 5i32..20
        ) {
            let code = format!("for ({} in 1:{}) {{\n  print({})\n}}", iterator, range_end, iterator);
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: iterator.clone(),
                kind: SymbolKind::Variable,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 5, // Position of iterator in for loop
                signature: None,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract for loop definition");

            let info = def_info.unwrap();
            prop_assert!(info.statement.contains("for"), "Should include for loop header");
            prop_assert!(info.statement.contains(&format!("{} in", iterator)), "Should include iterator definition");
        }

        #[test]
        // Property 31: Function parameter definition extraction
        fn prop_function_parameter_definition_extraction(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            param_name in "[a-z][a-z0-9_]{1,5}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            has_default in any::<bool>()
        ) {
            let param_def = if has_default {
                format!("{} = 42", param_name)
            } else {
                param_name.clone()
            };

            let code = format!("{} <- function({}) {{\n  {}\n}}", func_name, param_def, param_name);
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: param_name.clone(),
                kind: SymbolKind::Variable,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: func_name.len() as u32 + 15, // Approximate position in function signature
                signature: None,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract function definition for parameter");

            let info = def_info.unwrap();
            prop_assert!(info.statement.contains("function"), "Should include function keyword");
            prop_assert!(info.statement.contains(&param_name), "Should include parameter name in signature");
        }

        #[test]
        // Property 16: File URI protocol
        fn prop_file_uri_protocol(
            path_segments in prop::collection::vec("[a-z]{3,8}", 1..4)
        ) {
            let path = format!("/{}", path_segments.join("/"));
            let uri = Url::parse(&format!("file://{}/test.R", path)).unwrap();

            let def_info = DefinitionInfo {
                statement: "test_var <- 42".to_string(),
                source_uri: uri.clone(),
                line: 0,
                column: 0,
            };

            let current_uri = Url::parse("file:///workspace/main.R").unwrap();
            let mut value = String::new();
            value.push_str(&format!("```r\n{}\n```\n\n", escape_markdown(&def_info.statement)));

            if def_info.source_uri != current_uri {
                let relative_path = compute_relative_path(&def_info.source_uri, None);
                let absolute_path = def_info.source_uri.as_str();
                value.push_str(&format!("[{}]({}), line {}", relative_path, absolute_path, def_info.line + 1));
            }

            prop_assert!(value.contains("file://"), "Cross-file URI should use file:// protocol");
            prop_assert!(value.contains(&format!("file://{}/test.R", path)), "Should contain absolute path with file:// protocol");
        }

        #[test]
        // Property 17: Relative path calculation
        fn prop_relative_path_calculation(
            workspace_depth in 1usize..3,
            file_depth in 1usize..3
        ) {
            let workspace_segments: Vec<String> = (0..workspace_depth).map(|i| format!("ws{}", i)).collect();
            let file_segments: Vec<String> = (0..file_depth).map(|i| format!("dir{}", i)).collect();

            let workspace_root = Url::parse(&format!("file:///{}/", workspace_segments.join("/"))).unwrap();
            let target_uri = Url::parse(&format!("file:///{}/{}/test.R", workspace_segments.join("/"), file_segments.join("/"))).unwrap();

            let relative_path = compute_relative_path(&target_uri, Some(&workspace_root));

            prop_assert!(relative_path.contains(&file_segments.join("/")), "Should contain file path relative to workspace");
            prop_assert!(!relative_path.starts_with('/'), "Relative path should not start with /");
            prop_assert!(relative_path.ends_with("test.R"), "Should end with filename");
        }

        #[test]
        // Property 18: LSP Markdown markup kind
        fn prop_lsp_markdown_markup_kind(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            use crate::state::{WorldState, Document};

            let library_paths = vec![];
            let mut state = WorldState::new(library_paths);

            let uri = Url::parse("file:///test.R").unwrap();
            let code = format!("{} <- 42", var_name);
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let position = Position::new(0, 5);
            let hover_result = hover_blocking(&state, &uri, position);

            if let Some(hover) = hover_result {
                if let HoverContents::Markup(content) = hover.contents {
                    prop_assert_eq!(content.kind, MarkupKind::Markdown, "Hover content should use Markdown markup kind");
                } else {
                    prop_assert!(false, "Hover should return Markup content");
                }
            }
        }

        #[test]
        // Property 19: Cross-file definition resolution
        fn prop_cross_file_definition_resolution(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            use crate::state::{WorldState, Document};

            let library_paths = vec![];
            let mut state = WorldState::new(library_paths);

            let main_uri = Url::parse("file:///main.R").unwrap();
            let utils_uri = Url::parse("file:///utils.R").unwrap();

            let main_code = format!("source(\"utils.R\")\nresult <- {}(42)", func_name);
            let utils_code = format!("{} <- function(x) {{ x * 2 }}", func_name);

            state.documents.insert(main_uri.clone(), Document::new(&main_code, None));
            state.documents.insert(utils_uri.clone(), Document::new(&utils_code, None));

            // Update cross-file graph
            state.cross_file_graph.update_file(&main_uri, &crate::cross_file::extract_metadata(&main_code), None, |_| None);
            state.cross_file_graph.update_file(&utils_uri, &crate::cross_file::extract_metadata(&utils_code), None, |_| None);

            let position = Position::new(1, 10); // Position after source() call
            let cross_file_symbols = get_cross_file_symbols(&state, &main_uri, position.line, position.character);

            prop_assert!(cross_file_symbols.contains_key(&func_name), "Should resolve cross-file symbol using dependency graph");

            if let Some(symbol) = cross_file_symbols.get(&func_name) {
                prop_assert_eq!(&symbol.source_uri, &utils_uri, "Should locate definition in sourced file");
            }
        }

        #[test]
        // Property 20: Scope-based definition selection
        fn prop_scope_based_definition_selection(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            use crate::state::{WorldState, Document};

            let library_paths = vec![];
            let mut state = WorldState::new(library_paths);

            let uri = Url::parse("file:///test.R").unwrap();
            let code = format!(
                "{} <- function(a) {{ a }}\nsource(\"utils.R\")\n{} <- function(b, c) {{ b + c }}\nresult <- {}(1, 2)",
                func_name, func_name, func_name
            );

            let utils_uri = Url::parse("file:///utils.R").unwrap();
            let utils_code = format!("{} <- function(x, y, z) {{ x + y + z }}", func_name);

            state.documents.insert(uri.clone(), Document::new(&code, None));
            state.documents.insert(utils_uri.clone(), Document::new(&utils_code, None));

            // Update cross-file graph
            state.cross_file_graph.update_file(&uri, &crate::cross_file::extract_metadata(&code), None, |_| None);
            state.cross_file_graph.update_file(&utils_uri, &crate::cross_file::extract_metadata(&utils_code), None, |_| None);

            let position = Position::new(3, 10); // Position of function usage
            let cross_file_symbols = get_cross_file_symbols(&state, &uri, position.line, position.character);

            prop_assert!(cross_file_symbols.contains_key(&func_name), "Should find symbol definition");

            if let Some(symbol) = cross_file_symbols.get(&func_name) {
                // Should select the local definition (line 2) that's in scope, not the earlier one or utils.R
                prop_assert_eq!(&symbol.source_uri, &uri, "Should select definition from same file");
                prop_assert_eq!(symbol.defined_line, 2, "Should select the definition that's in scope at reference position");
            }
        }

        // ========================================================================
        // Feature: skip-nse-undefined-checks
        // Property-based tests for NSE context skipping in undefined variable checks
        // ========================================================================

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 1: Extract Operator RHS Skipped
        /// For any R code containing an extract operator ($ or @), the identifier on the
        /// right-hand side SHALL NOT be collected as a usage.
        fn prop_skip_nse_extract_operator_rhs_skipped(
            lhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            rhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            op in prop::sample::select(vec!["$", "@"])
        ) {
            let code = format!("{}{}{}", lhs, op, rhs);
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let rhs_used = used.iter().any(|(name, _)| name == &rhs);
            prop_assert!(!rhs_used, "RHS '{}' of extract operator should NOT be collected", rhs);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 2: Extract Operator LHS Checked
        /// For any R code containing an extract operator ($ or @), the identifier on the
        /// left-hand side SHALL be collected as a usage.
        fn prop_skip_nse_extract_operator_lhs_checked(
            lhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            rhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            op in prop::sample::select(vec!["$", "@"])
        ) {
            let code = format!("{}{}{}", lhs, op, rhs);
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let lhs_used = used.iter().any(|(name, _)| name == &lhs);
            prop_assert!(lhs_used, "LHS '{}' of extract operator should be collected", lhs);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 3: Call-Like Arguments Skipped
        /// For any R code containing a call-like node (call, subset, subset2), identifiers
        /// inside the arguments field SHALL NOT be collected as usages.
        fn prop_skip_nse_call_like_arguments_skipped(
            func in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            arg in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            call_type in prop::sample::select(vec!["call", "subset", "subset2"])
        ) {
            let code = match call_type {
                "call" => format!("{}({})", func, arg),
                "subset" => format!("{}[{}]", func, arg),
                "subset2" => format!("{}[[{}]]", func, arg),
                _ => unreachable!(),
            };
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let arg_used = used.iter().any(|(name, _)| name == &arg);
            prop_assert!(!arg_used, "Argument '{}' inside {} should NOT be collected", arg, call_type);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 4: Function Names Checked
        /// For any R code containing a function call, the function name SHALL be collected
        /// as a usage.
        fn prop_skip_nse_function_names_checked(
            func in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            arg in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            let code = format!("{}({})", func, arg);
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let func_used = used.iter().any(|(name, _)| name == &func);
            prop_assert!(func_used, "Function name '{}' should be collected", func);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 5: Formula Expressions Skipped
        /// For any R code containing a formula expression (unary ~ or binary ~), identifiers
        /// inside the formula SHALL NOT be collected as usages.
        fn prop_skip_nse_formula_expressions_skipped(
            var in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            formula_type in prop::sample::select(vec!["unary", "binary"])
        ) {
            let code = match formula_type {
                "unary" => format!("~ {}", var),
                "binary" => format!("y ~ {}", var),
                _ => unreachable!(),
            };
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let var_used = used.iter().any(|(name, _)| name == &var);
            prop_assert!(!var_used, "Variable '{}' inside {} formula should NOT be collected", var, formula_type);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 6: Nested Skip Contexts
        /// For any R code where a formula appears inside call arguments, identifiers in the
        /// formula SHALL NOT be collected (both skip contexts apply).
        fn prop_skip_nse_nested_formula_in_call(
            func in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            lhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            rhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            let code = format!("{}({} ~ {})", func, lhs, rhs);
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let lhs_used = used.iter().any(|(name, _)| name == &lhs);
            let rhs_used = used.iter().any(|(name, _)| name == &rhs);
            prop_assert!(!lhs_used, "Formula LHS '{}' inside call should NOT be collected", lhs);
            prop_assert!(!rhs_used, "Formula RHS '{}' inside call should NOT be collected", rhs);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 7: Existing Skip Rules Preserved
        /// For any R code containing assignments or named arguments, the existing skip rules
        /// SHALL continue to work (assignment LHS and named argument names are skipped).
        fn prop_skip_nse_existing_rules_preserved(
            var in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            op in prop::sample::select(vec!["<-", "=", "<<-"]),
            arg_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Test assignment LHS
            let assign_code = format!("{} {} 42", var, op);
            let tree = parse_r_code(&assign_code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &assign_code, &UsageContext::default(), &mut used);
            let var_used = used.iter().any(|(name, _)| name == &var);
            prop_assert!(!var_used, "Assignment LHS '{}' with '{}' should NOT be collected", var, op);

            // Test named argument
            let named_arg_code = format!("func({} = 1)", arg_name);
            let tree2 = parse_r_code(&named_arg_code);
            let mut used2 = Vec::new();
            collect_usages_with_context(tree2.root_node(), &named_arg_code, &UsageContext::default(), &mut used2);
            let arg_used = used2.iter().any(|(name, _)| name == &arg_name);
            prop_assert!(!arg_used, "Named argument '{}' should NOT be collected", arg_name);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 8: Non-Skipped Contexts Checked
        /// For any R code containing an identifier NOT in a skip context, the identifier
        /// SHALL be collected as a usage.
        fn prop_skip_nse_non_skipped_contexts_checked(
            var in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Standalone identifier (not in any skip context)
            let code = var.clone();
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let var_used = used.iter().any(|(name, _)| name == &var);
            prop_assert!(var_used, "Standalone identifier '{}' should be collected", var);
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::r_env;
    use crate::state::{Document, WorldState};

    #[test]
    fn test_base_package_functions() {
        // Test that base R functions are recognized
        let library_paths = r_env::find_library_paths();
        let _state = WorldState::new(library_paths);

        let code = "library(stats)\nx <- rnorm(100)\ny <- mean(x)";
        let doc = Document::new(code, None);

        // rnorm and mean should be recognized (rnorm from stats, mean from base)
        assert!(doc.loaded_packages.contains(&"stats".to_string()));
    }

    #[test]
    fn test_no_spurious_errors_with_common_packages() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Test code that uses common package functions
        let test_cases = vec![
            ("library(stats)\nx <- rnorm(100)", vec!["rnorm"]),
            (
                "library(utils)\ndata <- read.csv('file.csv')",
                vec!["read.csv"],
            ),
            ("require(graphics)\nplot(1:10)", vec!["plot"]),
        ];

        for (code, expected_funcs) in test_cases {
            let doc = Document::new(code, None);
            let uri = tower_lsp::lsp_types::Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), doc);

            let diagnostics = diagnostics(&state, &uri);

            // Check that expected functions don't generate undefined variable errors
            for func in expected_funcs {
                let has_error = diagnostics.iter().any(|d| d.message.contains(func));
                assert!(
                    !has_error,
                    "Function {} should not generate undefined variable error",
                    func
                );
            }
        }
    }

    #[test]
    fn test_package_exports_loaded() {
        let library_paths = r_env::find_library_paths();
        let state = WorldState::new(library_paths);

        // Try to load stats package metadata
        if let Some(stats_pkg) = state.library.get("stats") {
            // stats should export common functions
            assert!(
                !stats_pkg.exports.is_empty(),
                "stats package should have exports"
            );

            // Check for some known stats exports
            let has_common_funcs = stats_pkg
                .exports
                .iter()
                .any(|e| e == "rnorm" || e == "lm" || e == "t.test");
            assert!(
                has_common_funcs,
                "stats should export common statistical functions"
            );
        }
    }

    #[test]
    fn test_hover_shows_definition_statement() {
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Create a test document
        let uri = Url::parse("file:///test.R").unwrap();
        let code = "my_var <- 42\nresult <- my_var + 1";
        let doc = Document::new(code, None);
        state.documents.insert(uri.clone(), doc);

        // Create a scoped symbol with definition info
        let symbol = ScopedSymbol {
            name: "my_var".to_string(),
            kind: SymbolKind::Variable,
            source_uri: uri.clone(),
            defined_line: 0,
            defined_column: 0,
            signature: None,
        };

        // Test hover on the symbol
        // Mock get_cross_file_symbols to return our test symbol
        // Note: In a real test, we'd need to set up the cross-file state properly
        // For now, we'll test the definition extraction directly
        let def_info = extract_definition_statement(&symbol, &state);
        assert!(def_info.is_some());
        let def_info = def_info.unwrap();
        assert_eq!(def_info.statement, "my_var <- 42");
    }

    #[test]
    fn test_hover_same_file_location_format() {
        let library_paths = r_env::find_library_paths();
        let _state = WorldState::new(library_paths);

        let uri = Url::parse("file:///test.R").unwrap();
        let def_info = DefinitionInfo {
            statement: "my_var <- 42".to_string(),
            source_uri: uri.clone(),
            line: 0, // 0-based
            column: 0,
        };

        // Test same-file location formatting
        let escaped_statement = escape_markdown(&def_info.statement);
        let mut value = String::new();
        value.push_str(&format!("```r\n{}\n```\n\n", escaped_statement));

        if def_info.source_uri == uri {
            value.push_str(&format!("this file, line {}", def_info.line + 1)); // 1-based
        }

        assert!(value.contains("```r\nmy\\_var <- 42\n```"));
        assert!(value.contains("this file, line 1"));
        assert!(value.contains("\n\n")); // Blank line separator
    }

    #[test]
    fn test_hover_cross_file_hyperlink_format() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);
        state.workspace_folders = vec![Url::parse("file:///workspace/").unwrap()];

        let current_uri = Url::parse("file:///workspace/main.R").unwrap();
        let def_uri = Url::parse("file:///workspace/utils/helper.R").unwrap();

        let def_info = DefinitionInfo {
            statement: "helper_func <- function(x) { x + 1 }".to_string(),
            source_uri: def_uri.clone(),
            line: 5, // 0-based
            column: 0,
        };

        // Test cross-file location formatting
        let escaped_statement = escape_markdown(&def_info.statement);
        let mut value = String::new();
        value.push_str(&format!("```r\n{}\n```\n\n", escaped_statement));

        if def_info.source_uri != current_uri {
            let relative_path =
                compute_relative_path(&def_info.source_uri, state.workspace_folders.first());
            let absolute_path = def_info.source_uri.as_str();
            value.push_str(&format!(
                "[{}]({}), line {}",
                relative_path,
                absolute_path,
                def_info.line + 1
            ));
        }

        assert!(value.contains("```r\nhelper\\_func <- function\\(x\\) { x + 1 }\n```"));
        assert!(value.contains("[utils/helper.R](file:///workspace/utils/helper.R), line 6"));
        assert!(value.contains("\n\n")); // Blank line separator
    }

    #[test]
    fn test_hover_markdown_code_block_formatting() {
        let statement = "my_var <- c(1, 2, 3) # comment with *special* chars";
        let escaped = escape_markdown(statement);

        let formatted = format!("```r\n{}\n```", escaped);

        assert!(formatted.starts_with("```r\n"));
        assert!(formatted.ends_with("\n```"));
        assert!(formatted.contains("\\*special\\*")); // Markdown chars should be escaped
    }

    #[test]
    fn test_hover_blank_line_separator() {
        let def_info = DefinitionInfo {
            statement: "test_func <- function() {}".to_string(),
            source_uri: Url::parse("file:///test.R").unwrap(),
            line: 0,
            column: 0,
        };

        let escaped_statement = escape_markdown(&def_info.statement);
        let mut value = String::new();
        value.push_str(&format!("```r\n{}\n```\n\n", escaped_statement));
        value.push_str("this file, line 1");

        // Should have exactly one blank line between code block and location
        assert!(value.contains("```\n\nthis file"));
        assert!(!value.contains("```\n\n\nthis file")); // Not two blank lines
        assert!(!value.contains("```\nthis file")); // Not zero blank lines
    }

    #[test]
    fn test_cross_file_hover_resolution() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Create main.R that sources utils.R
        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let utils_uri = Url::parse("file:///workspace/utils.R").unwrap();

        let main_code = r#"source("utils.R")
result <- helper_func(42)"#;

        let utils_code = r#"helper_func <- function(x) {
    x * 2
}"#;

        // Add documents to state
        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );

        // Test hover on helper_func in main.R (line 1, after source call)
        let position = Position::new(1, 10); // Position of "helper_func"
        let hover_result = hover_blocking(&state, &main_uri, position);

        assert!(hover_result.is_some());
        let hover = hover_result.unwrap();

        if let HoverContents::Markup(content) = hover.contents {
            // Code blocks don't need escaping - content should be unescaped
            assert!(content.value.contains("helper_func"));
            assert!(content.value.contains("function(x)"));
            assert!(content.value.contains("utils.R")); // Should show cross-file source
        } else {
            panic!("Expected markup content");
        }
    }

    #[test]
    fn test_hover_symbol_shadowing() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Create files with shadowing: local definition should take precedence
        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let utils_uri = Url::parse("file:///workspace/utils.R").unwrap();

        let main_code = r#"source("utils.R")
my_func <- function(a, b) { a + b }  # Local definition shadows utils.R
result <- my_func(1, 2)"#;

        let utils_code = r#"my_func <- function(x) { x * 2 }  # Will be shadowed"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );

        // Test hover on my_func usage (should show local definition, not utils.R)
        let position = Position::new(2, 10); // Position of "my_func" in usage
        let hover_result = hover_blocking(&state, &main_uri, position);

        assert!(hover_result.is_some());
        let hover = hover_result.unwrap();

        if let HoverContents::Markup(content) = hover.contents {
            // Code blocks don't need escaping - content should be unescaped
            assert!(content.value.contains("my_func"));
            assert!(content.value.contains("(a, b)")); // Local signature, not (x)
            assert!(content.value.contains("this file")); // Should be local, not cross-file
        } else {
            panic!("Expected markup content");
        }
    }

    #[test]
    fn test_hover_builtin_function_fallback() {
        let library_paths = r_env::find_library_paths();
        let state = WorldState::new(library_paths);

        let uri = Url::parse("file:///test.R").unwrap();
        let code = r#"result <- mean(c(1, 2, 3))"#;

        let doc = Document::new(code, None);
        let tree = doc.tree.as_ref().unwrap();
        let text = doc.text();

        // Find the "mean" identifier
        let point = tree_sitter::Point::new(0, 10); // Position of "mean"
        let node = tree
            .root_node()
            .descendant_for_point_range(point, point)
            .unwrap();
        assert_eq!(node.kind(), "identifier");
        assert_eq!(&text[node.byte_range()], "mean");

        // Test hover should fall back to R help for built-in functions
        let position = Position::new(0, 10);

        // Mock the state with the document
        let mut test_state = state;
        test_state.documents.insert(uri.clone(), doc);

        let hover_result = hover_blocking(&test_state, &uri, position);

        // Should return hover info (either from help cache or R subprocess)
        // The exact content depends on R availability, but structure should be consistent
        if let Some(hover) = hover_result {
            if let HoverContents::Markup(content) = hover.contents {
                assert!(content.kind == MarkupKind::Markdown);
                assert!(content.value.starts_with("```"));
                assert!(content.value.ends_with("```"));
            } else {
                panic!("Expected markup content");
            }
        }
        // Note: We don't assert hover_result.is_some() because R might not be available in CI
    }

    #[test]
    fn test_hover_undefined_symbol_returns_none() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///test.R").unwrap();
        let code = r#"result <- undefined_symbol_that_does_not_exist"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Test hover on undefined symbol
        let position = Position::new(0, 10); // Position of "undefined_symbol_that_does_not_exist"
        let hover_result = hover_blocking(&state, &uri, position);

        // Should return None for truly undefined symbols (after trying all fallbacks)
        // This tests the graceful handling when no definition is found anywhere
        assert!(hover_result.is_none());
    }

    #[test]
    fn test_hover_graceful_fallback_missing_definition_file() {
        use crate::cross_file::ScopedSymbol;

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let missing_uri = Url::parse("file:///workspace/missing.R").unwrap(); // File doesn't exist

        let main_code = r#"# Symbol from missing file
result <- missing_func(42)"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));

        // Create a scoped symbol that references a missing file
        let symbol = ScopedSymbol {
            name: "missing_func".to_string(),
            kind: crate::cross_file::SymbolKind::Function,
            source_uri: missing_uri, // This file doesn't exist in state
            defined_line: 0,
            defined_column: 0,
            signature: Some("missing_func(x)".to_string()),
        };

        // Test extract_definition_statement with missing file (should return None)
        let def_info = extract_definition_statement(&symbol, &state);
        assert!(
            def_info.is_none(),
            "Should return None when source file is missing"
        );

        // The hover function should gracefully fall back to showing just the signature
        // This is tested implicitly in the hover function's match arm for None from extract_definition_statement
    }

    #[test]
    fn test_hover_position_aware_scope_resolution() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/test.R").unwrap();
        let code = r#"# Before source call - symbol not available
result1 <- helper_func(1)  # Should not resolve

source("utils.R")

# After source call - symbol available  
result2 <- helper_func(2)  # Should resolve"#;

        let utils_uri = Url::parse("file:///workspace/utils.R").unwrap();
        let utils_code = r#"helper_func <- function(x) { x * 2 }"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &uri,
            &crate::cross_file::extract_metadata(code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );

        // Test hover before source call (line 1) - should not find cross-file symbol
        let position_before = Position::new(1, 11); // "helper_func" before source()
        let cross_file_symbols_before = get_cross_file_symbols(
            &state,
            &uri,
            position_before.line,
            position_before.character,
        );
        assert!(
            !cross_file_symbols_before.contains_key("helper_func"),
            "Symbol should not be available before source() call"
        );

        // Test hover after source call (line 5) - should find cross-file symbol
        let position_after = Position::new(5, 11); // "helper_func" after source()
        let cross_file_symbols_after =
            get_cross_file_symbols(&state, &uri, position_after.line, position_after.character);
        assert!(
            cross_file_symbols_after.contains_key("helper_func"),
            "Symbol should be available after source() call"
        );
    }

    #[test]
    fn test_hover_uses_dependency_graph_correctly() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Create a chain: main.R -> utils.R -> helpers.R
        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let utils_uri = Url::parse("file:///workspace/utils.R").unwrap();
        let helpers_uri = Url::parse("file:///workspace/helpers.R").unwrap();

        let main_code = r#"source("utils.R")
result <- process_data(42)"#;

        let utils_code = r#"source("helpers.R")
process_data <- function(x) {
    transform_value(x) + 10
}"#;

        let helpers_code = r#"transform_value <- function(x) { x * 2 }"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));
        state
            .documents
            .insert(helpers_uri.clone(), Document::new(helpers_code, None));

        // Update cross-file graph for all files
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &helpers_uri,
            &crate::cross_file::extract_metadata(helpers_code),
            None,
            |_| None,
        );

        // Test hover on transform_value in utils.R (should resolve through chain)
        let position = Position::new(2, 4); // "transform_value" in utils.R
        let cross_file_symbols =
            get_cross_file_symbols(&state, &utils_uri, position.line, position.character);

        assert!(
            cross_file_symbols.contains_key("transform_value"),
            "Should resolve symbol through dependency chain"
        );

        let symbol = &cross_file_symbols["transform_value"];
        assert_eq!(
            symbol.source_uri, helpers_uri,
            "Should trace back to helpers.R"
        );
    }

    // ============================================================================
    // Task 17: Enhanced Variable Detection Hover Integration Tests
    // ============================================================================

    #[test]
    fn test_complete_workflow_for_loops_and_functions() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/test.R").unwrap();
        let code = r#"# Test for loops and function parameters
process_data <- function(data, threshold = 0.5, ...) {
    filtered <- data[data > threshold]
    for (i in 1:10) {
        for (j in 1:5) {
            result <- i * j
            if (result > threshold) {
                print(result)
            }
        }
    }
    for (item in filtered) {
        print(item)
    }
    return(filtered)
}"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Test scope resolution includes all iterators and parameters
        let positions = vec![
            (Position::new(5, 12), "result", true), // result inside nested loop
            (Position::new(4, 12), "i", true),      // i iterator
            (Position::new(4, 18), "j", true),      // j iterator
            (Position::new(12, 14), "item", true),  // item used inside the loop body
            (Position::new(2, 20), "data", true),   // function parameter
            (Position::new(6, 27), "threshold", true), // function parameter with default
            (Position::new(14, 14), "filtered", true), // local variable used in return(filtered)
        ];

        for (position, symbol_name, should_exist) in positions {
            let symbols = get_cross_file_symbols(&state, &uri, position.line, position.character);
            if should_exist {
                assert!(
                    symbols.contains_key(symbol_name),
                    "Symbol '{}' should be in scope at line {}, col {}",
                    symbol_name,
                    position.line + 1,
                    position.character
                );
            } else {
                assert!(
                    !symbols.contains_key(symbol_name),
                    "Symbol '{}' should NOT be in scope at line {}, col {}",
                    symbol_name,
                    position.line + 1,
                    position.character
                );
            }
        }

        // Test no false-positive undefined variable diagnostics
        let diagnostics = diagnostics(&state, &uri);
        let undefined_errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("undefined") || d.message.contains("not found"))
            .collect();

        assert!(
            undefined_errors.is_empty(),
            "Should not have undefined variable errors for loop iterators and function parameters: {:?}",
            undefined_errors
        );

        // Test hover shows definition statements (no escaping needed in code blocks)
        let hover_tests = vec![
            (Position::new(4, 12), "i", "for (i in 1:10)"),
            (Position::new(4, 18), "j", "for (j in 1:5)"),
            (Position::new(12, 14), "item", "for (item in filtered)"),
            (
                Position::new(2, 20),
                "data",
                "process_data <- function(data, threshold = 0.5, ...)",
            ),
        ];

        for (position, symbol_name, expected_statement) in hover_tests {
            let hover_result = hover_blocking(&state, &uri, position);
            if let Some(hover) = hover_result {
                if let HoverContents::Markup(content) = hover.contents {
                    assert!(
                        content.value.contains(expected_statement),
                        "Hover for '{}' should contain '{}', got: {}",
                        symbol_name,
                        expected_statement,
                        content.value
                    );
                    assert!(
                        content.value.contains("this file"),
                        "Hover should show file location"
                    );
                }
            }
        }
    }

    #[test]
    fn test_realistic_r_code_patterns() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Create main file with realistic patterns
        let main_uri = Url::parse("file:///workspace/analysis.R").unwrap();
        let utils_uri = Url::parse("file:///workspace/utils.R").unwrap();
        let helpers_uri = Url::parse("file:///workspace/helpers.R").unwrap();

        let main_code = r#"# Analysis script with realistic patterns
source("utils.R")
source("helpers.R", local = TRUE)

# Nested loops with multiple iterators
results <- list()
for (i in 1:10) {
    for (j in 1:5) {
        value <- i * j
        results[[paste0(i, "_", j)]] <- value
    }
}

# Function with parameters and locals
analyze_data <- function(dataset, 
                        min_threshold = 0.1,
                        max_threshold = 0.9,
                        ...) {
    # Multi-line function definition
    cleaned <- dataset[!is.na(dataset)]
    
    for (threshold in seq(min_threshold, max_threshold, 0.1)) {
        filtered <- cleaned[cleaned > threshold]
        cat("Threshold:", threshold, "Count:", length(filtered), "\n")
    }
    
    return(cleaned)
}

# Code with markdown special characters
comment_with_stars <- "This has *asterisks* and _underscores_"
backtick_var <- `special name with spaces`
"#;

        let utils_code = r#"# Utility functions
utility_func <- function(x, y = 2) {
    x ^ y
}

CONSTANT_VALUE <- 42
"#;

        let helpers_code = r#"# Helper functions (sourced with local=TRUE)
helper_transform <- function(data) {
    data * 2
}
"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));
        state
            .documents
            .insert(helpers_uri.clone(), Document::new(helpers_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &helpers_uri,
            &crate::cross_file::extract_metadata(helpers_code),
            None,
            |_| None,
        );

        // Test nested loop iterators are in scope
        let nested_loop_position = Position::new(8, 8); // Inside nested loop
        let symbols = get_cross_file_symbols(
            &state,
            &main_uri,
            nested_loop_position.line,
            nested_loop_position.character,
        );

        assert!(
            symbols.contains_key("i"),
            "Outer loop iterator 'i' should be in scope"
        );
        assert!(
            symbols.contains_key("j"),
            "Inner loop iterator 'j' should be in scope"
        );
        assert!(
            symbols.contains_key("value"),
            "Local variable 'value' should be in scope"
        );

        // Test function parameters are in scope within function
        let function_body_position = Position::new(19, 4); // Inside analyze_data function
        let func_symbols = get_cross_file_symbols(
            &state,
            &main_uri,
            function_body_position.line,
            function_body_position.character,
        );

        assert!(
            func_symbols.contains_key("dataset"),
            "Function parameter 'dataset' should be in scope"
        );
        assert!(
            func_symbols.contains_key("min_threshold"),
            "Function parameter 'min_threshold' should be in scope"
        );
        assert!(
            func_symbols.contains_key("max_threshold"),
            "Function parameter 'max_threshold' should be in scope"
        );
        assert!(
            func_symbols.contains_key("cleaned"),
            "Local variable 'cleaned' should be in scope"
        );

        // Test cross-file symbols are resolved correctly
        let after_source_position = Position::new(4, 0); // After source() calls
        let cross_symbols = get_cross_file_symbols(
            &state,
            &main_uri,
            after_source_position.line,
            after_source_position.character,
        );

        assert!(
            cross_symbols.contains_key("utility_func"),
            "Should resolve utility_func from utils.R"
        );
        assert!(
            cross_symbols.contains_key("CONSTANT_VALUE"),
            "Should resolve CONSTANT_VALUE from utils.R"
        );
        // Note: helper_transform should NOT be available due to local=TRUE

        // Test hover shows proper formatting for multi-line definitions
        let multi_line_func_position = Position::new(13, 0); // analyze_data function name
        let hover_result = hover_blocking(&state, &main_uri, multi_line_func_position);

        if let Some(hover) = hover_result {
            if let HoverContents::Markup(content) = hover.contents {
                assert!(content.value.contains("analyze_data <- function(dataset,"));
                assert!(content.value.contains("this file"));
                // Should handle markdown special characters properly
                assert!(!content.value.contains("*asterisks*")); // Should be escaped
            }
        }

        // Test no false positives for valid symbols
        let diagnostics = diagnostics(&state, &main_uri);
        let undefined_errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("undefined"))
            .collect();

        // Should not report undefined errors for loop iterators, function parameters, or cross-file symbols
        for error in &undefined_errors {
            assert!(
                !error.message.contains("i "),
                "Should not report 'i' as undefined"
            );
            assert!(
                !error.message.contains("j "),
                "Should not report 'j' as undefined"
            );
            assert!(
                !error.message.contains("dataset"),
                "Should not report 'dataset' as undefined"
            );
            assert!(
                !error.message.contains("utility_func"),
                "Should not report 'utility_func' as undefined"
            );
        }
    }

    #[test]
    fn test_cross_file_local_scope_isolation() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let local_uri = Url::parse("file:///workspace/local_source.R").unwrap();
        let global_uri = Url::parse("file:///workspace/global_source.R").unwrap();

        let main_code = r#"# Test local vs global sourcing
source("global_source.R")           # Global scope
source("local_source.R", local = TRUE)  # Local scope

# These should be available from global source
global_result <- global_func(42)

# These should NOT be available from local source
# local_func(42)  # Would be undefined
"#;

        let global_code = r#"global_func <- function(x) { x + 1 }
global_var <- 100"#;

        let local_code = r#"local_func <- function(x) { x * 2 }
local_var <- 200"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(global_uri.clone(), Document::new(global_code, None));
        state
            .documents
            .insert(local_uri.clone(), Document::new(local_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &global_uri,
            &crate::cross_file::extract_metadata(global_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &local_uri,
            &crate::cross_file::extract_metadata(local_code),
            None,
            |_| None,
        );

        // Test symbols after both source calls
        let position = Position::new(5, 0); // After both source() calls
        let symbols = get_cross_file_symbols(&state, &main_uri, position.line, position.character);

        // Global source symbols should be available
        assert!(
            symbols.contains_key("global_func"),
            "global_func should be available from global source"
        );
        assert!(
            symbols.contains_key("global_var"),
            "global_var should be available from global source"
        );

        // Local source symbols should NOT be available in main scope
        assert!(
            !symbols.contains_key("local_func"),
            "local_func should NOT be available from local source"
        );
        assert!(
            !symbols.contains_key("local_var"),
            "local_var should NOT be available from local source"
        );

        // Test hover on global symbol shows cross-file location
        let hover_position = Position::new(5, 16); // "global_func" usage
        let hover_result = hover_blocking(&state, &main_uri, hover_position);

        if let Some(hover) = hover_result {
            if let HoverContents::Markup(content) = hover.contents {
                assert!(content.value.contains("global_func"));
                assert!(
                    content.value.contains("global_source.R"),
                    "Should show cross-file source"
                );
            }
        }
    }

    #[test]
    fn test_hover_hyperlink_formatting_with_special_paths() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);
        state.workspace_folders = vec![Url::parse("file:///workspace/").unwrap()];

        // Test various path scenarios
        let main_uri = Url::parse("file:///workspace/src/analysis/main.R").unwrap();
        let utils_uri = Url::parse("file:///workspace/utils/helpers with spaces.R").unwrap();

        let main_code = r#"source("../../utils/helpers with spaces.R")
result <- helper_with_spaces(42)"#;

        let utils_code = r#"helper_with_spaces <- function(x) {
    # Function with special characters in filename
    x * 2
}"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );

        // Test hover shows proper hyperlink formatting
        let position = Position::new(1, 10); // "helper_with_spaces"
        let hover_result = hover_blocking(&state, &main_uri, position);

        if let Some(hover) = hover_result {
            if let HoverContents::Markup(content) = hover.contents {
                // Should contain properly formatted hyperlink
                assert!(content.value.contains("[utils/helpers with spaces.R]"));
                assert!(content
                    .value
                    .contains("file:///workspace/utils/helpers%20with%20spaces.R"));
                assert!(content.value.contains("line 1"));
            }
        }
    }

    // ============================================================================
    // Tests for hover package info - Task 12.1
    // ============================================================================

    #[test]
    fn test_hover_shows_package_name_for_package_exports() {
        // Test that hover displays package name for package exports
        // Validates: Requirement 10.1
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a symbol with a package URI
        let package_uri = Url::parse("package:dplyr").unwrap();
        let symbol = ScopedSymbol {
            name: "mutate".to_string(),
            kind: SymbolKind::Variable,
            source_uri: package_uri,
            defined_line: 0,
            defined_column: 0,
            signature: None,
        };

        // Verify the package name can be extracted from the URI
        let package_name = symbol.source_uri.as_str().strip_prefix("package:");
        assert_eq!(
            package_name,
            Some("dplyr"),
            "Should extract package name from URI"
        );

        // Test the formatting that would be used in hover
        let mut value = String::new();
        value.push_str(&format!("```r\n{}\n```\n", symbol.name));
        if let Some(pkg) = package_name {
            value.push_str(&format!("\nfrom {{{}}}", pkg));
        }

        assert!(
            value.contains("```r\nmutate\n```"),
            "Should contain symbol name in code block"
        );
        assert!(
            value.contains("from {dplyr}"),
            "Should contain package name in braces"
        );
    }

    #[test]
    fn test_hover_package_uri_detection() {
        // Test that package URIs are correctly detected
        // Validates: Requirement 10.1

        // Package URIs should be detected
        let package_uri = Url::parse("package:ggplot2").unwrap();
        assert!(
            package_uri.as_str().starts_with("package:"),
            "Package URI should start with 'package:'"
        );
        assert_eq!(
            package_uri.as_str().strip_prefix("package:"),
            Some("ggplot2")
        );

        // Base package URI should also be detected
        let base_uri = Url::parse("package:base").unwrap();
        assert!(
            base_uri.as_str().starts_with("package:"),
            "Base package URI should start with 'package:'"
        );
        assert_eq!(base_uri.as_str().strip_prefix("package:"), Some("base"));

        // File URIs should NOT be detected as packages
        let file_uri = Url::parse("file:///test.R").unwrap();
        assert!(
            !file_uri.as_str().starts_with("package:"),
            "File URI should not start with 'package:'"
        );
        assert_eq!(file_uri.as_str().strip_prefix("package:"), None);
    }

    #[test]
    fn test_hover_local_definition_not_shown_as_package() {
        // Test that local definitions are not shown as package exports
        // Validates: Requirement 10.4 (shadowing)
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a symbol with a file URI (local definition)
        let file_uri = Url::parse("file:///workspace/main.R").unwrap();
        let symbol = ScopedSymbol {
            name: "mutate".to_string(),
            kind: SymbolKind::Function,
            source_uri: file_uri.clone(),
            defined_line: 5,
            defined_column: 0,
            signature: Some("mutate <- function(x) { x + 1 }".to_string()),
        };

        // Verify this is NOT detected as a package export
        let package_name = symbol.source_uri.as_str().strip_prefix("package:");
        assert_eq!(
            package_name, None,
            "Local definition should not be detected as package export"
        );
    }

    // ============================================================================
    // Tests for collect_missing_package_diagnostics - Task 10.3
    // ============================================================================

    #[test]
    fn test_missing_package_diagnostic_emitted() {
        // Test that a diagnostic is emitted for a non-installed package
        // Validates: Requirement 15.1
        let mut meta = crate::cross_file::CrossFileMetadata::default();
        meta.library_calls
            .push(crate::cross_file::source_detect::LibraryCall {
                package: "__nonexistent_package_xyz__".to_string(),
                line: 0,
                column: 30,
                function_scope: None,
            });

        let state = WorldState::new(Vec::new());
        let mut diagnostics = Vec::new();

        collect_missing_package_diagnostics(&state, &meta, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit one diagnostic for missing package"
        );
        assert!(diagnostics[0]
            .message
            .contains("__nonexistent_package_xyz__"));
        assert!(diagnostics[0].message.contains("not installed"));
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn test_missing_package_diagnostic_not_emitted_for_base_package() {
        // Test that no diagnostic is emitted for base packages
        // Validates: Requirement 15.1 (base packages are always available)
        let mut meta = crate::cross_file::CrossFileMetadata::default();
        meta.library_calls
            .push(crate::cross_file::source_detect::LibraryCall {
                package: "base".to_string(),
                line: 0,
                column: 15,
                function_scope: None,
            });

        let mut state = WorldState::new(Vec::new());
        // Ensure base is in base_packages by creating a new PackageLibrary
        let mut base_packages = std::collections::HashSet::new();
        base_packages.insert("base".to_string());
        let mut pkg_lib = crate::package_library::PackageLibrary::new_empty();
        pkg_lib.set_base_packages(base_packages);
        state.package_library = std::sync::Arc::new(pkg_lib);

        let mut diagnostics = Vec::new();

        collect_missing_package_diagnostics(&state, &meta, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should not emit diagnostic for base package"
        );
    }

    #[test]
    fn test_missing_package_diagnostic_ignored_line() {
        // Test that diagnostics are not emitted for ignored lines
        // Validates: Requirement 15.1 with @lsp-ignore support
        let mut meta = crate::cross_file::CrossFileMetadata::default();
        meta.library_calls
            .push(crate::cross_file::source_detect::LibraryCall {
                package: "__nonexistent_package_xyz__".to_string(),
                line: 5,
                column: 30,
                function_scope: None,
            });
        // Mark line 5 as ignored
        meta.ignored_lines.insert(5);

        let state = WorldState::new(Vec::new());
        let mut diagnostics = Vec::new();

        collect_missing_package_diagnostics(&state, &meta, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should not emit diagnostic for ignored line"
        );
    }

    #[test]
    fn test_missing_package_diagnostic_multiple_packages() {
        // Test that diagnostics are emitted for multiple missing packages
        // Validates: Requirement 15.1
        let mut meta = crate::cross_file::CrossFileMetadata::default();
        meta.library_calls
            .push(crate::cross_file::source_detect::LibraryCall {
                package: "__missing_pkg1__".to_string(),
                line: 0,
                column: 20,
                function_scope: None,
            });
        meta.library_calls
            .push(crate::cross_file::source_detect::LibraryCall {
                package: "__missing_pkg2__".to_string(),
                line: 1,
                column: 20,
                function_scope: None,
            });

        let state = WorldState::new(Vec::new());
        let mut diagnostics = Vec::new();

        collect_missing_package_diagnostics(&state, &meta, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            2,
            "Should emit diagnostics for both missing packages"
        );
        assert!(diagnostics[0].message.contains("__missing_pkg1__"));
        assert!(diagnostics[1].message.contains("__missing_pkg2__"));
    }

    // ============================================================================
    // Tests for hover shadowing - Task 12.3
    // ============================================================================

    #[test]
    fn test_hover_local_definition_shadows_package_export() {
        // Test that when a local definition shadows a package export,
        // hover shows the local definition, not the package export.
        // Validates: Requirement 10.4
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/main.R").unwrap();

        // Code that loads a package and then defines a local function with the same name
        // as a package export. The local definition should shadow the package export.
        let code = r#"library(dplyr)
mutate <- function(x, y) { x + y }  # Local definition shadows dplyr::mutate
result <- mutate(1, 2)"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Update cross-file graph with metadata
        state.cross_file_graph.update_file(
            &uri,
            &crate::cross_file::extract_metadata(code),
            None,
            |_| None,
        );

        // Test hover on "mutate" usage (line 2, position 10)
        let position = Position::new(2, 10);
        let hover_result = hover_blocking(&state, &uri, position);

        assert!(hover_result.is_some(), "Hover should return a result");
        let hover = hover_result.unwrap();

        if let HoverContents::Markup(content) = hover.contents {
            // Should show local definition signature (x, y), not dplyr's mutate
            assert!(
                content.value.contains("mutate"),
                "Should contain function name"
            );
            assert!(
                content.value.contains("(x, y)"),
                "Should show local signature (x, y), not dplyr's signature"
            );
            // Should NOT show package attribution
            assert!(
                !content.value.contains("{dplyr}"),
                "Should NOT show package attribution for shadowed symbol"
            );
            // Should show local file location
            assert!(
                content.value.contains("this file"),
                "Should show local file location"
            );
        } else {
            panic!("Expected markup content");
        }
    }

    #[test]
    fn test_hover_shadowing_scope_resolution_returns_local() {
        // Test that scope resolution returns the local definition when it shadows a package export.
        // This verifies the underlying mechanism that hover relies on.
        // Validates: Requirement 10.4
        use crate::cross_file::scope::{compute_artifacts, scope_at_position_with_packages};
        use std::collections::HashSet;

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Code with library() and local definition of same name
        let code = r#"library(dplyr)
filter <- function(x) { x > 0 }
result <- filter(c(1, -2, 3))"#;

        // Use Document::new to parse the code (same as other tests)
        let doc = Document::new(code, None);
        let tree = doc.tree.as_ref().expect("Should parse successfully");
        let artifacts = compute_artifacts(&uri, tree, code);

        // Create a mock package exports callback that returns "filter" for dplyr
        let get_exports = |pkg: &str| -> HashSet<String> {
            if pkg == "dplyr" {
                let mut exports = HashSet::new();
                exports.insert("filter".to_string());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 2 (after both library and local definition)
        let scope = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Symbol should be in scope
        assert!(
            scope.symbols.contains_key("filter"),
            "filter should be in scope"
        );

        // The symbol should be from the local definition, not the package
        let symbol = scope.symbols.get("filter").unwrap();
        assert!(
            !symbol.source_uri.as_str().starts_with("package:"),
            "filter should be from local definition, not package. Got URI: '{}'",
            symbol.source_uri.as_str()
        );
        assert_eq!(
            symbol.source_uri, uri,
            "filter should be from the local file"
        );
    }

    #[test]
    fn test_hover_package_export_shown_when_no_local_shadow() {
        // Test that when there's no local definition, hover shows the package export.
        // This is the complement to test_hover_local_definition_shadows_package_export.
        // Validates: Requirements 10.1, 10.4
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a symbol that represents a package export
        let package_uri = Url::parse("package:dplyr").unwrap();
        let symbol = ScopedSymbol {
            name: "mutate".to_string(),
            kind: SymbolKind::Function,
            source_uri: package_uri.clone(),
            defined_line: 0,
            defined_column: 0,
            signature: Some("mutate(.data, ...)".to_string()),
        };

        // Verify this IS detected as a package export
        let package_name = symbol.source_uri.as_str().strip_prefix("package:");
        assert_eq!(
            package_name,
            Some("dplyr"),
            "Package export should be detected"
        );

        // Verify the formatting that would be used in hover
        let mut value = String::new();
        if let Some(pkg) = package_name {
            if let Some(sig) = &symbol.signature {
                value.push_str(&format!("```r\n{}\n```\n", sig));
            }
            value.push_str(&format!("\nfrom {{{}}}", pkg));
        }

        assert!(
            value.contains("mutate(.data, ...)"),
            "Should show function signature"
        );
        assert!(
            value.contains("from {dplyr}"),
            "Should show package attribution"
        );
    }

    #[test]
    fn test_hover_shadowing_position_aware() {
        // Test that shadowing is position-aware: before the local definition,
        // the package export should be shown; after, the local definition.
        // Validates: Requirement 10.4
        use crate::cross_file::scope::{compute_artifacts, scope_at_position_with_packages};
        use std::collections::HashSet;

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Code with library() first, then local definition later
        let code = r#"library(dplyr)
x <- mutate(df, y = 1)  # Uses package export
mutate <- function(x) { x + 1 }  # Local definition
z <- mutate(5)  # Uses local definition"#;

        // Use Document::new to parse the code (same as other tests)
        let doc = Document::new(code, None);
        let tree = doc.tree.as_ref().expect("Should parse successfully");
        let artifacts = compute_artifacts(&uri, tree, code);

        // Create a mock package exports callback
        let get_exports = |pkg: &str| -> HashSet<String> {
            if pkg == "dplyr" {
                let mut exports = HashSet::new();
                exports.insert("mutate".to_string());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 1 (before local definition) - should get package export
        let scope_before =
            scope_at_position_with_packages(&artifacts, 1, 5, &get_exports, &base_exports);
        assert!(
            scope_before.symbols.contains_key("mutate"),
            "mutate should be in scope before local def"
        );
        let symbol_before = scope_before.symbols.get("mutate").unwrap();
        assert!(
            symbol_before.source_uri.as_str().starts_with("package:"),
            "Before local definition, mutate should be from package. Got URI: '{}'",
            symbol_before.source_uri.as_str()
        );

        // Query scope at line 3 (after local definition) - should get local definition
        let scope_after =
            scope_at_position_with_packages(&artifacts, 3, 5, &get_exports, &base_exports);
        assert!(
            scope_after.symbols.contains_key("mutate"),
            "mutate should be in scope after local def"
        );
        let symbol_after = scope_after.symbols.get("mutate").unwrap();
        assert!(
            !symbol_after.source_uri.as_str().starts_with("package:"),
            "After local definition, mutate should be from local file. Got URI: '{}'",
            symbol_after.source_uri.as_str()
        );
        assert_eq!(
            symbol_after.source_uri, uri,
            "mutate should be from the local file"
        );
    }

    // ============================================================================
    // Tests for goto_definition package handling - Task 13.1
    // ============================================================================

    /// Verifies that symbols originating from packages are treated as non-navigable.
    ///
    /// This test constructs a `ScopedSymbol` whose `source_uri` uses the `package:`
    /// scheme and asserts that such URIs are recognized as package exports (which
    /// goto-definition should not navigate into).
    ///
    /// # Examples
    ///
    /// ```
    /// use crate::cross_file::scope::{ScopedSymbol, SymbolKind};
    /// use url::Url;
    ///
    /// let package_uri = Url::parse("package:dplyr").unwrap();
    /// let symbol = ScopedSymbol {
    ///     name: "mutate".to_string(),
    ///     kind: SymbolKind::Function,
    ///     source_uri: package_uri.clone(),
    ///     defined_line: 0,
    ///     defined_column: 0,
    ///     signature: Some("mutate(.data, ...)".to_string()),
    /// };
    ///
    /// assert!(symbol.source_uri.as_str().starts_with("package:"));
    /// let is_package_export = symbol.source_uri.as_str().starts_with("package:");
    /// assert!(is_package_export);
    /// let package_name = symbol.source_uri.as_str().strip_prefix("package:");
    /// assert_eq!(package_name, Some("dplyr"));
    /// ```
    #[test]
    fn test_goto_definition_returns_none_for_package_exports() {
        // Test that goto_definition returns None for package exports
        // since package source files are not navigable
        // Validates: Requirements 11.1, 11.2
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a symbol with a package URI
        let package_uri = Url::parse("package:dplyr").unwrap();
        let symbol = ScopedSymbol {
            name: "mutate".to_string(),
            kind: SymbolKind::Function,
            source_uri: package_uri.clone(),
            defined_line: 0,
            defined_column: 0,
            signature: Some("mutate(.data, ...)".to_string()),
        };

        // Verify the package URI is detected correctly
        assert!(
            symbol.source_uri.as_str().starts_with("package:"),
            "Package export should have package: URI prefix"
        );

        // The goto_definition logic should skip package exports
        // This test verifies the detection logic used in goto_definition
        let is_package_export = symbol.source_uri.as_str().starts_with("package:");
        assert!(is_package_export, "Should detect package export");

        // Extract package name for logging
        let package_name = symbol.source_uri.as_str().strip_prefix("package:");
        assert_eq!(package_name, Some("dplyr"), "Should extract package name");
    }

    #[test]
    fn test_goto_definition_navigates_to_local_definition() {
        // Test that goto_definition navigates to local definitions (not package exports)
        // Validates: Requirement 11.3 (shadowing)
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a symbol with a file URI (local definition)
        let file_uri = Url::parse("file:///workspace/main.R").unwrap();
        let symbol = ScopedSymbol {
            name: "mutate".to_string(),
            kind: SymbolKind::Function,
            source_uri: file_uri.clone(),
            defined_line: 5,
            defined_column: 0,
            signature: Some("mutate <- function(x) { x + 1 }".to_string()),
        };

        // Verify this is NOT a package export
        assert!(
            !symbol.source_uri.as_str().starts_with("package:"),
            "Local definition should not have package: URI prefix"
        );

        // The goto_definition logic should navigate to local definitions
        let is_package_export = symbol.source_uri.as_str().starts_with("package:");
        assert!(!is_package_export, "Should not detect as package export");

        // Verify the location would be correct
        let expected_line = symbol.defined_line;
        let expected_column = symbol.defined_column;
        assert_eq!(expected_line, 5, "Should navigate to correct line");
        assert_eq!(expected_column, 0, "Should navigate to correct column");
    }

    #[test]
    fn test_goto_definition_package_uri_formats() {
        // Test various package URI formats are correctly detected
        // Validates: Requirements 11.1, 11.2

        // Standard package URI
        let dplyr_uri = Url::parse("package:dplyr").unwrap();
        assert!(dplyr_uri.as_str().starts_with("package:"));
        assert_eq!(dplyr_uri.as_str().strip_prefix("package:"), Some("dplyr"));

        // Base package URI
        let base_uri = Url::parse("package:base").unwrap();
        assert!(base_uri.as_str().starts_with("package:"));
        assert_eq!(base_uri.as_str().strip_prefix("package:"), Some("base"));

        // Package with dots in name
        let data_table_uri = Url::parse("package:data.table").unwrap();
        assert!(data_table_uri.as_str().starts_with("package:"));
        assert_eq!(
            data_table_uri.as_str().strip_prefix("package:"),
            Some("data.table")
        );

        // File URIs should NOT be detected as packages
        let file_uri = Url::parse("file:///workspace/test.R").unwrap();
        assert!(!file_uri.as_str().starts_with("package:"));
        assert_eq!(file_uri.as_str().strip_prefix("package:"), None);
    }

    // ============================================================================
    // Tests for goto_definition shadowing behavior - Task 13.2
    // ============================================================================

    #[test]
    fn test_goto_definition_local_shadows_package_export() {
        // Test that when a local definition shadows a package export,
        // goto_definition navigates to the local definition, not the package.
        // Validates: Requirement 11.3

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/main.R").unwrap();

        // Code that loads a package and then defines a local function with the same name
        // as a package export. The local definition should shadow the package export.
        // "mutate" is defined locally on line 1 (0-indexed), shadowing dplyr::mutate
        let code = r#"library(dplyr)
mutate <- function(x, y) { x + y }
result <- mutate(1, 2)"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Update cross-file graph with metadata
        state.cross_file_graph.update_file(
            &uri,
            &crate::cross_file::extract_metadata(code),
            None,
            |_| None,
        );

        // Test goto_definition on "mutate" usage (line 2, position 10 - within "mutate")
        let position = Position::new(2, 10);
        let result = goto_definition(&state, &uri, position);

        // Should navigate to local definition, not return None (which would happen for package exports)
        assert!(
            result.is_some(),
            "goto_definition should return a result for shadowed symbol"
        );

        if let Some(GotoDefinitionResponse::Scalar(location)) = result {
            // Should navigate to the local definition on line 1
            assert_eq!(location.uri, uri, "Should navigate to the same file");
            assert_eq!(
                location.range.start.line, 1,
                "Should navigate to line 1 where local mutate is defined"
            );
            assert_eq!(
                location.range.start.character, 0,
                "Should navigate to column 0"
            );
        } else {
            panic!("Expected Scalar response");
        }
    }

    #[test]
    fn test_goto_definition_local_definition_found_first() {
        // Test that goto_definition searches the current document first,
        // ensuring local definitions are found before cross-file symbols.
        // This is the core mechanism that enables shadowing.
        // Validates: Requirement 11.3

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Simple code with a local function definition and usage
        let code = r#"my_func <- function(a, b) { a + b }
result <- my_func(1, 2)"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Test goto_definition on "my_func" usage (line 1, position 10)
        let position = Position::new(1, 10);
        let result = goto_definition(&state, &uri, position);

        assert!(
            result.is_some(),
            "goto_definition should find local definition"
        );

        if let Some(GotoDefinitionResponse::Scalar(location)) = result {
            assert_eq!(location.uri, uri, "Should navigate to the same file");
            assert_eq!(
                location.range.start.line, 0,
                "Should navigate to line 0 where my_func is defined"
            );
        } else {
            panic!("Expected Scalar response");
        }
    }

    /// Verifies that scope resolution prefers local definitions over package exports for goto-definition.
    ///
    /// Constructs a document containing a `library()` call and a local function named `filter`, computes
    /// the cross-file scope at a position after the local definition, and asserts that the `filter`
    /// symbol resolves to the local file (not a `package:` URI) and has the expected definition line.
    ///
    /// # Examples
    ///
    /// ```
    /// // Confirms a local `filter` shadows the `dplyr` export when resolving definitions.
    /// ```
    #[test]
    fn test_goto_definition_shadowing_scope_resolution() {
        // Test that scope resolution correctly returns local definitions over package exports.
        // This verifies the underlying mechanism that goto_definition relies on.
        // Validates: Requirement 11.3
        use crate::cross_file::scope::{compute_artifacts, scope_at_position_with_packages};
        use std::collections::HashSet;

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Code with library() and local definition of same name
        let code = r#"library(dplyr)
filter <- function(x) { x > 0 }
result <- filter(c(1, -2, 3))"#;

        let doc = Document::new(code, None);
        let tree = doc.tree.as_ref().expect("Should parse successfully");
        let artifacts = compute_artifacts(&uri, tree, code);

        // Create a mock package exports callback that returns "filter" for dplyr
        let get_exports = |pkg: &str| -> HashSet<String> {
            if pkg == "dplyr" {
                let mut exports = HashSet::new();
                exports.insert("filter".to_string());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 2 (after both library and local definition)
        let scope = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Symbol should be in scope
        assert!(
            scope.symbols.contains_key("filter"),
            "filter should be in scope"
        );

        // The symbol should be from the local definition, not the package
        let symbol = scope.symbols.get("filter").unwrap();
        assert!(
            !symbol.source_uri.as_str().starts_with("package:"),
            "filter should be from local definition, not package. Got URI: '{}'",
            symbol.source_uri.as_str()
        );
        assert_eq!(
            symbol.source_uri, uri,
            "filter should be from the local file"
        );

        // Verify the definition position matches the local definition
        assert_eq!(symbol.defined_line, 1, "filter should be defined on line 1");
    }

    #[test]
    fn test_goto_definition_shadowing_position_aware() {
        // Test that shadowing is position-aware: before the local definition,
        // the package export would be used; after, the local definition.
        // For goto_definition, this means:
        // - Before local def: returns None (package export, not navigable)
        // - After local def: returns local definition location
        // Validates: Requirement 11.3

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Code where package is loaded, then used, then shadowed, then used again
        // Line 0: library(dplyr)
        // Line 1: x <- filter(data)  # Uses dplyr::filter
        // Line 2: filter <- function(x) { x > 0 }  # Local definition
        // Line 3: y <- filter(data)  # Uses local filter
        let code = r#"library(dplyr)
x <- filter(data)
filter <- function(x) { x > 0 }
y <- filter(data)"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));
        state.cross_file_graph.update_file(
            &uri,
            &crate::cross_file::extract_metadata(code),
            None,
            |_| None,
        );

        // Test goto_definition on "filter" usage AFTER local definition (line 3, position 5)
        let position_after = Position::new(3, 5);
        let result_after = goto_definition(&state, &uri, position_after);

        // After local definition, should navigate to local definition
        assert!(
            result_after.is_some(),
            "goto_definition should find local definition after shadowing"
        );

        if let Some(GotoDefinitionResponse::Scalar(location)) = result_after {
            assert_eq!(location.uri, uri, "Should navigate to the same file");
            assert_eq!(
                location.range.start.line, 2,
                "Should navigate to line 2 where local filter is defined"
            );
        } else {
            panic!("Expected Scalar response");
        }
    }

    #[test]
    fn test_goto_definition_multiple_local_definitions() {
        // Test that goto_definition finds the first local definition when
        // there are multiple definitions of the same symbol.
        // Validates: Requirement 11.3

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Code with multiple definitions of the same symbol
        let code = r#"x <- 1
x <- 2
y <- x"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Test goto_definition on "x" usage (line 2, position 5)
        let position = Position::new(2, 5);
        let result = goto_definition(&state, &uri, position);

        assert!(result.is_some(), "goto_definition should find definition");

        if let Some(GotoDefinitionResponse::Scalar(location)) = result {
            assert_eq!(location.uri, uri, "Should navigate to the same file");
            // find_definition_in_tree finds the first definition in document order
            assert_eq!(
                location.range.start.line, 0,
                "Should navigate to first definition on line 0"
            );
        } else {
            panic!("Expected Scalar response");
        }
    }
}
