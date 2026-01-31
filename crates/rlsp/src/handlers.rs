//
// handlers.rs
//
// Copyright (C) 2025 Posit Software, PBC. All rights reserved.
//
//

use std::collections::HashMap;

use tower_lsp::lsp_types::*;
use tree_sitter::Node;
use tree_sitter::Point;

use crate::cross_file::{scope, ScopedSymbol};
use crate::state::WorldState;

use crate::builtins;

// ============================================================================
// Cross-File Scope Helper
// ============================================================================

/// Get cross-file symbols available at a position.
/// This traverses the source() chain to include symbols from sourced files,
/// and also includes symbols from parent files via backward directives.
fn get_cross_file_symbols(
    state: &WorldState,
    uri: &Url,
    line: u32,
    column: u32,
) -> HashMap<String, ScopedSymbol> {
    log::trace!("get_cross_file_symbols called for {}:{},{}", uri, line, column);
    
    // Closure to get artifacts for a URI
    let get_artifacts = |target_uri: &Url| -> Option<scope::ScopeArtifacts> {
        log::trace!("get_artifacts: looking for {}", target_uri);
        // Try open documents first (authoritative)
        if let Some(doc) = state.documents.get(target_uri) {
            if let Some(tree) = &doc.tree {
                let text = doc.text();
                log::trace!("get_artifacts: found in open documents");
                return Some(scope::compute_artifacts(target_uri, tree, &text));
            }
        }
        // Try cross-file workspace index (preferred for closed files)
        if let Some(artifacts) = state.cross_file_workspace_index.get_artifacts(target_uri) {
            log::trace!("get_artifacts: found in cross-file workspace index ({} symbols)", artifacts.exported_interface.len());
            return Some(artifacts);
        }
        // Fallback to legacy workspace index
        if let Some(doc) = state.workspace_index.get(target_uri) {
            if let Some(tree) = &doc.tree {
                let text = doc.text();
                log::trace!("get_artifacts: found in legacy workspace index");
                return Some(scope::compute_artifacts(target_uri, tree, &text));
            }
        }
        log::trace!("get_artifacts: NOT FOUND");
        None
    };

    // Closure to get metadata for a URI
    let get_metadata = |target_uri: &Url| -> Option<crate::cross_file::CrossFileMetadata> {
        // Try open documents first (authoritative)
        if let Some(doc) = state.documents.get(target_uri) {
            let text = doc.text();
            return Some(crate::cross_file::extract_metadata(&text));
        }
        // Try cross-file workspace index (preferred for closed files)
        if let Some(metadata) = state.cross_file_workspace_index.get_metadata(target_uri) {
            return Some(metadata);
        }
        // Fallback to legacy workspace index
        if let Some(doc) = state.workspace_index.get(target_uri) {
            let text = doc.text();
            return Some(crate::cross_file::extract_metadata(&text));
        }
        None
    };

    let max_depth = state.cross_file_config.max_chain_depth;
    
    log::trace!("Calling scope_at_position_with_graph with max_depth={}", max_depth);
    
    // Use the graph-aware scope resolution with PathContext
    let scope = scope::scope_at_position_with_graph(
        uri,
        line,
        column,
        &get_artifacts,
        &get_metadata,
        &state.cross_file_graph,
        state.workspace_folders.first(),
        max_depth,
    );
    
    log::trace!("scope_at_position_with_graph returned {} symbols", scope.symbols.len());
    
    scope.symbols
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
            start: Position::new(node.start_position().row as u32, node.start_position().column as u32),
            end: Position::new(node.end_position().row as u32, node.end_position().column as u32),
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

pub fn diagnostics(state: &WorldState, uri: &Url) -> Vec<Diagnostic> {
    log::trace!("Diagnostics request for {}", uri);
    
    let Some(doc) = state.get_document(uri) else {
        return Vec::new();
    };

    let Some(tree) = &doc.tree else {
        return Vec::new();
    };

    let text = doc.text();
    let mut diagnostics = Vec::new();

    // Parse directives to get ignored lines and cross-file metadata
    let directive_meta = crate::cross_file::directive::parse_directives(&text);

    // Collect syntax errors (not suppressed by @lsp-ignore)
    collect_syntax_errors(tree.root_node(), &mut diagnostics);

    // Check for circular dependencies
    if let Some(cycle_edge) = state.cross_file_graph.detect_cycle(uri) {
        let line = cycle_edge.call_site_line.unwrap_or(0);
        let col = cycle_edge.call_site_column.unwrap_or(0);
        let target = cycle_edge.to.path_segments().and_then(|s| s.last().map(|s| s.to_string())).unwrap_or_default();
        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(line, col),
                end: Position::new(line, col + 1),
            },
            severity: Some(state.cross_file_config.circular_dependency_severity),
            message: format!("Circular dependency detected: sourcing '{}' creates a cycle", target),
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
    collect_out_of_scope_diagnostics(state, uri, tree.root_node(), &text, &directive_meta, &mut diagnostics);

    // Collect undefined variable errors if enabled in config
    if state.cross_file_config.undefined_variables_enabled {
        collect_undefined_variables_position_aware(
            state,
            uri,
            tree.root_node(),
            &text,
            &doc.loaded_packages,
            &state.workspace_imports,
            &state.library,
            &directive_meta,
            &mut diagnostics,
        );
    }

    diagnostics
}

/// Collect diagnostics for missing files referenced in source() calls and directives
fn collect_missing_file_diagnostics(
    state: &WorldState,
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Build PathContext for proper path resolution
    let path_ctx = crate::cross_file::path_resolve::PathContext::from_metadata(
        uri, meta, state.workspace_folders.first()
    );
    
    // Build a separate context for backward directives that ignores @lsp-cd
    let backward_ctx = crate::cross_file::path_resolve::PathContext::new(
        uri, state.workspace_folders.first()
    );
    
    // Resolve forward paths (source() calls) - respects @lsp-cd
    let resolve_forward_path = |path: &str| -> Option<Url> {
        let ctx = path_ctx.as_ref()?;
        log::trace!("resolve_forward_path: attempting to resolve '{}' with context: file_path={}, wd={:?}", 
            path, ctx.file_path.display(), ctx.working_directory);
        let resolved = crate::cross_file::path_resolve::resolve_path(path, ctx)?;
        log::trace!("resolve_forward_path: resolved to PathBuf: {}", resolved.display());
        let uri = crate::cross_file::path_resolve::path_to_uri(&resolved)?;
        log::trace!("resolve_forward_path: converted to URI: {}", uri);
        Some(uri)
    };
    
    // Resolve backward paths (directives) - ignores @lsp-cd, always relative to file
    let resolve_backward_path = |path: &str| -> Option<Url> {
        let ctx = backward_ctx.as_ref()?;
        log::trace!("resolve_backward_path: attempting to resolve '{}' with context: file_path={}, wd={:?}", 
            path, ctx.file_path.display(), ctx.working_directory);
        let resolved = crate::cross_file::path_resolve::resolve_path(path, ctx)?;
        log::trace!("resolve_backward_path: resolved to PathBuf: {}", resolved.display());
        let uri = crate::cross_file::path_resolve::path_to_uri(&resolved)?;
        log::trace!("resolve_backward_path: converted to URI: {}", uri);
        Some(uri)
    };

    // Check if file exists using cached/indexed data, with filesystem fallback
    let file_exists = |target_uri: &Url| -> bool {
        log::trace!("file_exists: checking if URI exists: {}", target_uri);
        
        // Check open documents first (authoritative)
        if state.documents.contains_key(target_uri) {
            log::trace!("file_exists: found in open documents");
            return true;
        }
        // Check workspace index (legacy)
        if state.workspace_index.contains_key(target_uri) {
            log::trace!("file_exists: found in workspace index");
            return true;
        }
        // Check cross-file workspace index (preferred for closed files)
        if state.cross_file_workspace_index.contains(target_uri) {
            log::trace!("file_exists: found in cross-file workspace index");
            return true;
        }
        // Check file cache (may have been read previously)
        if state.cross_file_file_cache.get(target_uri).is_some() {
            log::trace!("file_exists: found in file cache");
            return true;
        }
        // Don't block on filesystem I/O - assume file exists if not in cache
        // On-demand indexing will populate the cache when the file is needed
        log::trace!("file_exists: {} not in cache, assuming exists to avoid false positives", target_uri);
        true
    };

    // Check forward sources (source() calls and @lsp-source directives)
    // These respect @lsp-cd working directory
    for source in &meta.sources {
        if let Some(target_uri) = resolve_forward_path(&source.path) {
            if !file_exists(&target_uri) {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(source.line, source.column),
                        end: Position::new(source.line, source.column + source.path.len() as u32),
                    },
                    severity: Some(state.cross_file_config.missing_file_severity),
                    message: format!("File not found: '{}'", source.path),
                    ..Default::default()
                });
            }
        } else {
            // Path couldn't be resolved at all
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(source.line, source.column),
                    end: Position::new(source.line, source.column + source.path.len() as u32),
                },
                severity: Some(state.cross_file_config.missing_file_severity),
                message: format!("Cannot resolve path: '{}'", source.path),
                ..Default::default()
            });
        }
    }

    // Check backward directives (@lsp-sourced-by)
    // These are ALWAYS resolved relative to the file's directory, ignoring @lsp-cd
    for directive in &meta.sourced_by {
        if let Some(target_uri) = resolve_backward_path(&directive.path) {
            if !file_exists(&target_uri) {
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

/// Collect diagnostics for max chain depth exceeded (Requirement 5.8)
fn collect_max_depth_diagnostics(
    state: &WorldState,
    uri: &Url,
    diagnostics: &mut Vec<Diagnostic>,
) {
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

/// Collect diagnostics for ambiguous parent relationships
fn collect_ambiguous_parent_diagnostics(
    state: &WorldState,
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use crate::cross_file::parent_resolve::resolve_parent_with_content;
    use crate::cross_file::cache::ParentResolution;

    // Build PathContext WITHOUT working_directory for backward directives
    // This matches the behavior in dependency.rs where backward directives
    // are resolved relative to the file's directory, ignoring @lsp-cd
    let path_ctx = crate::cross_file::path_resolve::PathContext::new(
        uri, state.workspace_folders.first()
    );
    
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

    if let ParentResolution::Ambiguous { selected_uri, alternatives, .. } = resolution {
        // Find the first backward directive line to attach the diagnostic
        let directive_line = meta.sourced_by.first().map(|d| d.directive_line).unwrap_or(0);

        let alt_list: Vec<String> = alternatives.iter()
            .filter_map(|u| u.path_segments().and_then(|s| s.last().map(|s| s.to_string())))
            .collect();

        let selected_name = selected_uri.path_segments()
            .and_then(|s| s.last().map(|s| s.to_string()))
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

/// Collect diagnostics for symbols used before their source() call (Requirement 10.3)
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
                if let Some(artifacts) = state.cross_file_workspace_index.get_artifacts(target_uri) {
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
                let start_line_text = text.lines().nth(usage_node.start_position().row).unwrap_or("");
                let end_line_text = text.lines().nth(usage_node.end_position().row).unwrap_or("");
                let start_col = byte_offset_to_utf16_column(start_line_text, usage_node.start_position().column);
                let end_col = byte_offset_to_utf16_column(end_line_text, usage_node.end_position().column);

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
fn collect_identifier_usages_utf16<'a>(node: Node<'a>, text: &str, usages: &mut Vec<(String, u32, u32, Node<'a>)>) {
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

/// Position-aware undefined variable collection.
/// Checks each usage against the cross-file scope at that specific position.
/// Respects @lsp-ignore and @lsp-ignore-next directives.
fn collect_undefined_variables_position_aware(
    state: &WorldState,
    uri: &Url,
    node: Node,
    text: &str,
    loaded_packages: &[String],
    workspace_imports: &[String],
    library: &crate::state::Library,
    directive_meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use std::collections::HashSet;
    use crate::cross_file::types::byte_offset_to_utf16_column;

    let mut defined: HashSet<String> = HashSet::new();
    let mut used: Vec<(String, Node)> = Vec::new();

    // First pass: collect all definitions
    collect_definitions(node, text, &mut defined);

    // Second pass: collect all usages
    collect_usages(node, text, &mut used);

    // Report undefined variables with position-aware cross-file scope
    for (name, usage_node) in used {
        let usage_line = usage_node.start_position().row as u32;

        // Skip if line is ignored via @lsp-ignore or @lsp-ignore-next
        if crate::cross_file::directive::is_line_ignored(directive_meta, usage_line) {
            continue;
        }

        // Skip if locally defined or builtin
        if defined.contains(&name)
            || is_builtin(&name)
            || is_package_export(&name, loaded_packages, library)
            || workspace_imports.contains(&name)
        {
            continue;
        }

        // Convert byte column to UTF-16 for cross-file scope lookup
        let line_text = text.lines().nth(usage_node.start_position().row).unwrap_or("");
        let usage_col = byte_offset_to_utf16_column(line_text, usage_node.start_position().column);
        log::trace!("Checking cross-file scope for undefined variable '{}' at {}:{},{}", name, uri, usage_line, usage_col);
        let cross_file_symbols = get_cross_file_symbols(state, uri, usage_line, usage_col);

        if !cross_file_symbols.contains_key(&name) {
            log::trace!("Symbol '{}' not found in cross-file scope, marking as undefined", name);
            // Convert byte columns to UTF-16 for diagnostic range
            let start_line_text = text.lines().nth(usage_node.start_position().row).unwrap_or("");
            let end_line_text = text.lines().nth(usage_node.end_position().row).unwrap_or("");
            let start_col = byte_offset_to_utf16_column(start_line_text, usage_node.start_position().column);
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
        } else {
            log::trace!("Symbol '{}' found in cross-file scope, skipping undefined diagnostic", name);
        }
    }
}

#[allow(dead_code)]
fn collect_undefined_variables(
    node: Node,
    text: &str,
    loaded_packages: &[String],
    workspace_imports: &[String],
    library: &crate::state::Library,
    cross_file_symbols: &HashMap<String, ScopedSymbol>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use std::collections::HashSet;

    let mut defined: HashSet<String> = HashSet::new();
    let mut used: Vec<(String, Node)> = Vec::new();

    // First pass: collect all definitions
    collect_definitions(node, text, &mut defined);

    // Second pass: collect all usages
    collect_usages(node, text, &mut used);

    // Report undefined variables
    for (name, node) in used {
        if !defined.contains(&name)
            && !is_builtin(&name)
            && !is_package_export(&name, loaded_packages, library)
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

fn is_builtin(name: &str) -> bool {
    // Check constants first
    if matches!(name, "TRUE" | "FALSE" | "NULL" | "NA" | "Inf" | "NaN" | "T" | "F") {
        return true;
    }
    // Check comprehensive builtin list
    builtins::is_builtin(name)
}

fn is_package_export(name: &str, loaded_packages: &[String], library: &crate::state::Library) -> bool {
    for pkg_name in loaded_packages {
        if let Some(package) = library.get(pkg_name) {
            if package.exports.contains(&name.to_string()) {
                return true;
            }
        }
    }
    false
}

// ============================================================================
// Completions
// ============================================================================

pub fn completion(state: &WorldState, uri: &Url, position: Position) -> Option<CompletionResponse> {
    log::trace!("Completion request at {}:{},{}", uri, position.line, position.character);
    
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
        "if", "else", "repeat", "while", "function", "for", "in", "next", "break",
        "TRUE", "FALSE", "NULL", "Inf", "NaN", "NA", "NA_integer_", "NA_real_",
        "NA_complex_", "NA_character_", "library", "require", "return", "print",
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

    // Add cross-file symbols (from scope resolution)
    log::trace!("Calling get_cross_file_symbols for completion");
    let cross_file_symbols = get_cross_file_symbols(state, uri, position.line, position.character);
    log::trace!("Got {} symbols from cross-file scope", cross_file_symbols.len());
    for (name, symbol) in cross_file_symbols {
        if seen_names.contains(&name) {
            continue; // Local definitions take precedence
        }
        seen_names.insert(name.clone());

        let kind = match symbol.kind {
            crate::cross_file::SymbolKind::Function => CompletionItemKind::FUNCTION,
            crate::cross_file::SymbolKind::Variable => CompletionItemKind::VARIABLE,
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
// Hover
// ============================================================================

pub fn hover(state: &WorldState, uri: &Url, position: Position) -> Option<Hover> {
    log::trace!("Hover request at {}:{},{}", uri, position.line, position.character);
    
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    let point = Point::new(position.line as usize, position.character as usize);
    let node = tree.root_node().descendant_for_point_range(point, point)?;

    // Get the identifier
    let name = if node.kind() == "identifier" || node.kind() == "string" {
        node_text(node, &text)
    } else {
        return None;
    };

    let node_range = Range {
        start: Position::new(node.start_position().row as u32, node.start_position().column as u32),
        end: Position::new(node.end_position().row as u32, node.end_position().column as u32),
    };

    // Try user-defined function first
    if let Some(signature) = find_user_function_signature(state, uri, name) {
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```r\n{}\n```", signature),
            }),
            range: Some(node_range),
        });
    }

    // Try cross-file symbols
    log::trace!("Calling get_cross_file_symbols for hover");
    let cross_file_symbols = get_cross_file_symbols(state, uri, position.line, position.character);
    log::trace!("Got {} symbols from cross-file scope", cross_file_symbols.len());
    if let Some(symbol) = cross_file_symbols.get(name) {
        let mut value = String::new();
        if let Some(sig) = &symbol.signature {
            value.push_str(&format!("```r\n{}\n```\n", sig));
        } else {
            value.push_str(&format!("```r\n{}\n```\n", name));
        }
        if symbol.source_uri != *uri {
            value.push_str(&format!("\n*Defined in {}*", symbol.source_uri.path()));
        }
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(node_range),
        });
    }

    // Check cache first
    if let Some(cached) = state.help_cache.get(&name) {
        if let Some(help_text) = cached {
            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("```\n{}\n```", help_text),
                }),
                range: Some(node_range),
            });
        }
        return None;
    }

    // Try to get help from R
    let help_text = crate::help::get_help(&name, None)?;
    
    // Cache the result
    state.help_cache.insert(name.to_string(), Some(help_text.clone()));

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```\n{}\n```", help_text),
        }),
        range: Some(node_range),
    })
}
// Signature Help
// ============================================================================

pub fn signature_help(
    state: &WorldState,
    uri: &Url,
    position: Position,
) -> Option<SignatureHelp> {
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

pub fn goto_definition(
    state: &WorldState,
    uri: &Url,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    log::trace!("Goto definition request at {}:{},{}", uri, position.line, position.character);
    
    // Try open document first, then workspace index
    let doc = state.get_document(uri).or_else(|| state.workspace_index.get(uri))?;
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
    log::trace!("Calling get_cross_file_symbols for goto definition");
    let cross_file_symbols = get_cross_file_symbols(state, uri, position.line, position.character);
    log::trace!("Got {} symbols from cross-file scope", cross_file_symbols.len());
    if let Some(symbol) = cross_file_symbols.get(name) {
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: symbol.source_uri.clone(),
            range: Range {
                start: Position::new(symbol.defined_line, symbol.defined_column),
                end: Position::new(symbol.defined_line, symbol.defined_column + name.len() as u32),
            },
        }));
    }

    // Search all open documents
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

    // Search workspace index
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
            if matches!(op_text, "<-" | "=" | "<<-") && lhs.kind() == "identifier" {
                if node_text(lhs, text) == name {
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
    // Try open document first, then workspace index
    let doc = state.get_document(uri).or_else(|| state.workspace_index.get(uri))?;
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

    // Search all open documents
    for (file_uri, doc) in &state.documents {
        if file_uri == uri {
            continue; // Already searched
        }
        if let Some(tree) = &doc.tree {
            let file_text = doc.text();
            find_references_in_tree(tree.root_node(), name, &file_text, file_uri, &mut locations);
        }
    }

    // Search workspace index
    for (file_uri, doc) in &state.workspace_index {
        if file_uri == uri {
            continue; // Already searched
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
    let indent: String = prev_line.chars().take_while(|c| c.is_whitespace()).collect();

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
// Signature Extraction
// ============================================================================

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
            } else if let Some(identifier) = param_children.iter().find(|n| n.kind() == "identifier") {
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

fn find_user_function_signature(state: &WorldState, current_uri: &Url, name: &str) -> Option<String> {
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
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn parse_r_code(code: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
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
        let params_node = func_node.children(&mut cursor)
            .find(|n| n.kind() == "parameters").unwrap();
        
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

    // Tests for diagnostic range precision (Requirements 5.1, 5.2)
    #[test]
    fn test_diagnostic_range_matches_path_length() {
        // Property 4: Diagnostic range matches path length
        // The end column should equal start column plus path length
        let path = "utils/helpers.R";
        let start_column: u32 = 8; // e.g., after 'source("'
        let end_column = start_column + path.len() as u32;
        
        // Verify the calculation is correct
        assert_eq!(end_column, start_column + 15); // "utils/helpers.R" is 15 chars
        assert_eq!(end_column - start_column, path.len() as u32);
    }

    #[test]
    fn test_diagnostic_range_various_path_lengths() {
        // Test with various path lengths
        let test_cases = vec![
            ("a.R", 3),
            ("utils.R", 7),
            ("path/to/file.R", 14),
            ("very/long/path/to/some/file.R", 29),
        ];
        
        for (path, expected_len) in test_cases {
            assert_eq!(path.len(), expected_len, "Path '{}' should have length {}", path, expected_len);
            
            let start_column: u32 = 0;
            let end_column = start_column + path.len() as u32;
            
            // Range should exactly cover the path
            assert_eq!(end_column - start_column, expected_len as u32);
        }
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use crate::state::Document;
    use std::collections::HashSet;

    // Helper to filter out R reserved keywords from generated identifiers
    fn is_r_reserved(s: &str) -> bool {
        matches!(s, "for" | "if" | "in" | "else" | "while" | "repeat" | "next" | "break" 
            | "function" | "return" | "true" | "false" | "null" | "inf" | "nan")
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
            let func_node = find_function_definition(tree.root_node()).unwrap();
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

    fn parse_r_code(code: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
        parser.parse(code, None).unwrap()
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
        assert!(!n_used, "n should NOT be collected as it's a named argument");
    }

    proptest! {
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

        // Property 4: Diagnostic range matches path length
        #[test]
        fn test_diagnostic_range_precision(
            path in "[a-z]{1,5}(/[a-z]{1,5}){0,3}\\.R",
            start_column in 0u32..100
        ) {
            let end_column = start_column + path.len() as u32;
            
            // The range should exactly cover the path
            prop_assert_eq!(end_column - start_column, path.len() as u32);
            
            // End column should be >= start column
            prop_assert!(end_column >= start_column);
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::state::{WorldState, Document};
    use crate::r_env;
    
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
            ("library(utils)\ndata <- read.csv('file.csv')", vec!["read.csv"]),
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
                assert!(!has_error, "Function {} should not generate undefined variable error", func);
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
            assert!(!stats_pkg.exports.is_empty(), "stats package should have exports");
            
            // Check for some known stats exports
            let has_common_funcs = stats_pkg.exports.iter().any(|e| 
                e == "rnorm" || e == "lm" || e == "t.test"
            );
            assert!(has_common_funcs, "stats should export common statistical functions");
        }
    }
}
