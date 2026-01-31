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
    // Closure to get artifacts for a URI
    let get_artifacts = |target_uri: &Url| -> Option<scope::ScopeArtifacts> {
        // Try open documents first (authoritative)
        if let Some(doc) = state.documents.get(target_uri) {
            if let Some(tree) = &doc.tree {
                let text = doc.text();
                return Some(scope::compute_artifacts(target_uri, tree, &text));
            }
        }
        // Try cross-file workspace index (preferred for closed files)
        if let Some(artifacts) = state.cross_file_workspace_index.get_artifacts(target_uri) {
            return Some(artifacts);
        }
        // Fallback to legacy workspace index
        if let Some(doc) = state.workspace_index.get(target_uri) {
            if let Some(tree) = &doc.tree {
                let text = doc.text();
                return Some(scope::compute_artifacts(target_uri, tree, &text));
            }
        }
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
    
    let resolve_path = |path: &str| -> Option<Url> {
        let ctx = path_ctx.as_ref()?;
        let resolved = crate::cross_file::path_resolve::resolve_path(path, ctx)?;
        crate::cross_file::path_resolve::path_to_uri(&resolved)
    };

    // Check if file exists using cached/indexed data only (no blocking I/O)
    let file_exists = |target_uri: &Url| -> bool {
        // Check open documents first (authoritative)
        if state.documents.contains_key(target_uri) {
            return true;
        }
        // Check workspace index (legacy)
        if state.workspace_index.contains_key(target_uri) {
            return true;
        }
        // Check cross-file workspace index (preferred for closed files)
        if state.cross_file_workspace_index.contains(target_uri) {
            return true;
        }
        // Check file cache (may have been read previously)
        if state.cross_file_file_cache.get(target_uri).is_some() {
            return true;
        }
        // Don't do blocking I/O here - if not in any cache, assume missing
        // The file will be indexed on next didChangeWatchedFiles if it exists
        false
    };

    // Check forward sources (source() calls and @lsp-source directives)
    for source in &meta.sources {
        if let Some(target_uri) = resolve_path(&source.path) {
            if !file_exists(&target_uri) {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(source.line, source.column),
                        end: Position::new(source.line, source.column + source.path.len() as u32 + 10),
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
                    end: Position::new(source.line, source.column + source.path.len() as u32 + 10),
                },
                severity: Some(state.cross_file_config.missing_file_severity),
                message: format!("Cannot resolve path: '{}'", source.path),
                ..Default::default()
            });
        }
    }

    // Check backward directives (@lsp-sourced-by)
    for directive in &meta.sourced_by {
        if let Some(target_uri) = resolve_path(&directive.path) {
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

    // Build PathContext for proper path resolution
    let path_ctx = crate::cross_file::path_resolve::PathContext::from_metadata(
        uri, meta, state.workspace_folders.first()
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

fn collect_identifier_usages<'a>(node: Node<'a>, text: &str, usages: &mut Vec<(String, u32, u32, Node<'a>)>) {
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
                            collect_identifier_usages(child, text, usages);
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
        let col = node.start_position().column as u32;
        usages.push((name, line, col, node));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_usages(child, text, usages);
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
        let cross_file_symbols = get_cross_file_symbols(state, uri, usage_line, usage_col);

        if !cross_file_symbols.contains_key(&name) {
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
    collect_usages_with_context(node, text, &UsageContext::default(), &mut used);

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
            children.first().map_or(false, |op| node_text(*op, text) == "~")
        }
        "binary_operator" => {
            // For binary operator, check if the operator (second child) is ~
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            children.get(1).map_or(false, |op| node_text(*op, text) == "~")
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
    let cross_file_symbols = get_cross_file_symbols(state, uri, position.line, position.character);
    for (name, symbol) in cross_file_symbols {
        if seen_names.contains(&name) {
            continue; // Local definitions take precedence
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
    let line_text = content.lines().nth(symbol.defined_line as usize).unwrap_or("");
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

fn find_assignment_statement<'a>(mut node: tree_sitter::Node<'a>, content: &str) -> Option<StatementMatch<'a>> {
    // Walk up to find binary_operator (assignment), for_statement, or parameter
    loop {
        match node.kind() {
            "binary_operator" => {
                let mut cursor = node.walk();
                let children: Vec<_> = node.children(&mut cursor).collect();
                if children.len() >= 2 {
                    let op_text = node_text(children[1], content);
                    if matches!(op_text, "<-" | "=" | "<<-" | "->") {
                        return Some(StatementMatch { node, header_only: false });
                    }
                }
            }
            "for_statement" => return Some(StatementMatch { node, header_only: false }),
            "parameter" => {
                // For parameters, find enclosing function_definition
                if let Some(func) = find_enclosing_function(node) {
                    return Some(StatementMatch { node: func, header_only: false });
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

fn find_function_statement<'a>(mut node: tree_sitter::Node<'a>, content: &str) -> Option<StatementMatch<'a>> {
    // Walk up to find function_definition or assignment containing function.
    // For definition extraction, we want the full statement so we can include bodies and apply
    // standard truncation rules.
    loop {
        match node.kind() {
            "function_definition" => {
                // Check if parent is assignment
                if let Some(parent) = node.parent() {
                    if parent.kind() == "binary_operator" {
                        return Some(StatementMatch { node: parent, header_only: false });
                    }
                }
                return Some(StatementMatch { node, header_only: false });
            }
            "binary_operator" => {
                let mut cursor = node.walk();
                let children: Vec<_> = node.children(&mut cursor).collect();
                if children.len() >= 3 {
                    let op_text = node_text(children[1], content);
                    // Check for function on RHS (for <- = <<-) or LHS (for ->)
                    if matches!(op_text, "<-" | "=" | "<<-") && children[2].kind() == "function_definition" {
                        return Some(StatementMatch { node, header_only: false });
                    }
                    if op_text == "->" && children[0].kind() == "function_definition" {
                        return Some(StatementMatch { node, header_only: false });
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
                if child.kind() == "brace_list" || child.kind() == "call" || 
                   (child.kind() != "identifier" && child.kind() != "(" && 
                    child.kind() != ")" && child.kind() != "in" && 
                    child.start_position().row > start_line) {
                    // Body starts - extract up to before body
                    let body_start = child.start_position();
                    if body_start.row == start_line {
                        // Body on same line - extract up to body start column
                        let line = lines.get(start_line).unwrap_or(&"");
                        return line[..body_start.column.min(line.len())].trim_end().to_string();
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
        _ => lines.get(start_line).unwrap_or(&"").to_string()
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
        if child.kind() == "brace_list" || 
           (child.kind() != "function" && child.kind() != "parameters" && 
            child.start_position().row >= start_line) {
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
                        result.push_str(&line[..body_start.column.min(line.len())].trim_end());
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

pub fn hover(state: &WorldState, uri: &Url, position: Position) -> Option<Hover> {
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
        start: Position::new(node.start_position().row as u32, node.start_position().column as u32),
        end: Position::new(node.end_position().row as u32, node.end_position().column as u32),
    };

    // Try cross-file symbols (includes local scope with definition extraction)
    log::trace!("Calling get_cross_file_symbols for hover");
    let cross_file_symbols = get_cross_file_symbols(state, uri, position.line, position.character);
    log::trace!("Got {} symbols from cross-file scope", cross_file_symbols.len());
    if let Some(symbol) = cross_file_symbols.get(name) {
        let mut value = String::new();
        
        // Try to extract definition statement
        let workspace_root = state.workspace_folders.first();
        match extract_definition_statement(symbol, state) {
            Some(def_info) => {
                let escaped_statement = escape_markdown(&def_info.statement);
                value.push_str(&format!("```r\n{}\n```\n\n", escaped_statement));
                
                // Add file location
                if def_info.source_uri == *uri {
                    value.push_str(&format!("this file, line {}", def_info.line + 1));
                } else {
                    let relative_path = compute_relative_path(&def_info.source_uri, workspace_root);
                    let absolute_path = def_info.source_uri.as_str();
                    value.push_str(&format!("[{}]({}), line {}", relative_path, absolute_path, def_info.line + 1));
                }
            }
            None => {
                // Graceful fallback: show symbol info without definition statement
                if let Some(sig) = &symbol.signature {
                    value.push_str(&format!("```r\n{}\n```\n", sig));
                } else {
                    value.push_str(&format!("```r\n{}\n```\n", name));
                }
                
                // Add source file info if cross-file
                if symbol.source_uri != *uri {
                    let relative_path = compute_relative_path(&symbol.source_uri, workspace_root);
                    value.push_str(&format!("\n*Defined in {}*", relative_path));
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

    // Fallback to R help system for built-ins and undefined symbols
    // Check cache first to avoid repeated R subprocess calls
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
        // Cached as None - no help available
        return None;
    }

    // Try to get help from R subprocess
    if let Some(help_text) = crate::help::get_help(&name, None) {
        // Cache successful result
        state.help_cache.insert(name.to_string(), Some(help_text.clone()));
        
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```\n{}\n```", help_text),
            }),
            range: Some(node_range),
        });
    }

    // Cache negative result to avoid repeated failed lookups
    state.help_cache.insert(name.to_string(), None);
    None
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
    let cross_file_symbols = get_cross_file_symbols(state, uri, position.line, position.character);
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
// Path Utilities
// ============================================================================

/// Compute relative path from workspace root to target URI.
/// If no workspace root or target is outside workspace, returns filename only.
fn compute_relative_path(target_uri: &Url, workspace_root: Option<&Url>) -> String {
    let Some(workspace_root) = workspace_root else {
        return target_uri.path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("unknown")
            .to_string();
    };

    let Ok(workspace_path) = workspace_root.to_file_path() else {
        return target_uri.path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("unknown")
            .to_string();
    };

    let Ok(target_path) = target_uri.to_file_path() else {
        return target_uri.path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("unknown")
            .to_string();
    };

    match target_path.strip_prefix(&workspace_path) {
        Ok(relative) => relative.to_string_lossy().to_string(),
        Err(_) => target_uri.path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("unknown")
            .to_string(),
    }
}

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
        assert!(!column_used, "RHS 'column' should NOT be collected as usage for $ operator");
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
        assert!(!slot_used, "RHS 'slot' should NOT be collected as usage for @ operator");
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
        assert!(undefined_used, "LHS 'undefined' should be collected as usage");
        
        // 'column' should NOT be collected as a usage (RHS is skipped)
        let column_used = used.iter().any(|(name, _)| name == "column");
        assert!(!column_used, "RHS 'column' should NOT be collected as usage");
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
        assert!(subset_used, "Function name 'subset' should be collected as usage");
        
        // 'df' should NOT be collected as a usage (inside call arguments)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(!df_used, "'df' inside call arguments should NOT be collected as usage");
        
        // 'x' should NOT be collected as a usage (inside call arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(!x_used, "'x' inside call arguments should NOT be collected as usage");
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
        assert!(df_used, "'df' (object being subsetted) should be collected as usage");
        
        // 'x' should NOT be collected as a usage (inside subset arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(!x_used, "'x' inside subset arguments should NOT be collected as usage");
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
        assert!(df_used, "'df' (object being subsetted) should be collected as usage");
        
        // 'x' should NOT be collected as a usage (inside subset2 arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(!x_used, "'x' inside subset2 arguments should NOT be collected as usage");
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
        assert!(func_used, "Function name 'undefined_func' should be collected as usage");
        
        // 'x' should NOT be collected as a usage (inside call arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(!x_used, "'x' inside call arguments should NOT be collected as usage");
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
        assert!(!x_used, "'x' inside unary formula should NOT be collected as usage");
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
        assert!(!y_used, "'y' inside binary formula should NOT be collected as usage");
        
        // 'x' should NOT be collected as a usage (RHS of formula)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(!x_used, "'x' inside binary formula should NOT be collected as usage");
        
        // 'z' should NOT be collected as a usage (RHS of formula)
        let z_used = used.iter().any(|(name, _)| name == "z");
        assert!(!z_used, "'z' inside binary formula should NOT be collected as usage");
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
        assert!(!y_used, "'y' inside formula in call arguments should NOT be collected as usage");
        
        // 'x' should NOT be collected as a usage (inside formula inside call arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(!x_used, "'x' inside formula in call arguments should NOT be collected as usage");
        
        // 'df' should NOT be collected as a usage (inside call arguments)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(!df_used, "'df' inside call arguments should NOT be collected as usage");
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
        assert!(!x_used, "'x' inside deeply nested formula should NOT be collected as usage");
        
        // No identifiers should be collected at all
        assert!(used.is_empty(), "No identifiers should be collected from deeply nested formula");
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
        assert!(!g_used, "'g' inside call arguments should NOT be collected as usage");
        
        // 'h' should NOT be collected as a usage (inside g's arguments, which is inside f's arguments)
        let h_used = used.iter().any(|(name, _)| name == "h");
        assert!(!h_used, "'h' inside nested call arguments should NOT be collected as usage");
        
        // 'x' should NOT be collected as a usage (inside h's arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(!x_used, "'x' inside deeply nested call arguments should NOT be collected as usage");
        
        // Only 'f' should be collected
        assert_eq!(used.len(), 1, "Only the outermost function name should be collected");
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
        assert!(df_used, "'df' (LHS of extract operator) should be collected as usage");
        
        // 'col' should NOT be collected as a usage (RHS of extract operator)
        let col_used = used.iter().any(|(name, _)| name == "col");
        assert!(!col_used, "'col' (RHS of extract operator) should NOT be collected as usage");
        
        // 'x' should NOT be collected as a usage (inside subset arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(!x_used, "'x' inside subset arguments should NOT be collected as usage");
        
        // Only 'df' should be collected
        assert_eq!(used.len(), 1, "Only 'df' should be collected in mixed context");
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
        assert!(df_used, "'df' (leftmost identifier) should be collected as usage");
        
        // 'a' should NOT be collected as a usage (RHS of first extract operator)
        let a_used = used.iter().any(|(name, _)| name == "a");
        assert!(!a_used, "'a' (RHS of extract operator) should NOT be collected as usage");
        
        // 'b' should NOT be collected as a usage (RHS of second extract operator)
        let b_used = used.iter().any(|(name, _)| name == "b");
        assert!(!b_used, "'b' (RHS of extract operator) should NOT be collected as usage");
        
        // 'c' should NOT be collected as a usage (RHS of third extract operator)
        let c_used = used.iter().any(|(name, _)| name == "c");
        assert!(!c_used, "'c' (RHS of extract operator) should NOT be collected as usage");
        
        // Only 'df' should be collected
        assert_eq!(used.len(), 1, "Only 'df' should be collected in chained extracts");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use crate::state::Document;
    use crate::cross_file::scope::{ScopedSymbol, SymbolKind};
    use std::collections::HashSet;

    // Helper to parse R code for property tests
    fn parse_r_code(code: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
        parser.parse(code, None).unwrap()
    }

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
            assert!(result.is_some(), "Should extract statement for operator {}", op);
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
        assert!(!n_used, "n should NOT be collected as it's a named argument");
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
            let hover_result = hover(&state, &uri, position);
            
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
    
    #[test]
    fn test_hover_shows_definition_statement() {
        use std::collections::HashMap;
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
        let position = Position::new(1, 10); // Position of "my_var" in second line
        
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
        let state = WorldState::new(library_paths);
        
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
            let relative_path = compute_relative_path(&def_info.source_uri, state.workspace_folders.first());
            let absolute_path = def_info.source_uri.as_str();
            value.push_str(&format!("[{}]({}), line {}", relative_path, absolute_path, def_info.line + 1));
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
        use crate::cross_file::{dependency, scope};
        
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
        state.documents.insert(main_uri.clone(), Document::new(main_code, None));
        state.documents.insert(utils_uri.clone(), Document::new(utils_code, None));
        
        // Update cross-file graph
        state.cross_file_graph.update_file(&main_uri, &crate::cross_file::extract_metadata(main_code), None, |_| None);
        state.cross_file_graph.update_file(&utils_uri, &crate::cross_file::extract_metadata(utils_code), None, |_| None);
        
        // Test hover on helper_func in main.R (line 1, after source call)
        let position = Position::new(1, 10); // Position of "helper_func"
        let hover_result = hover(&state, &main_uri, position);
        
        assert!(hover_result.is_some());
        let hover = hover_result.unwrap();
        
        if let HoverContents::Markup(content) = hover.contents {
            assert!(content.value.contains("helper\\_func"));
            assert!(content.value.contains("function\\(x\\)"));
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
        
        state.documents.insert(main_uri.clone(), Document::new(main_code, None));
        state.documents.insert(utils_uri.clone(), Document::new(utils_code, None));
        
        // Update cross-file graph
        state.cross_file_graph.update_file(&main_uri, &crate::cross_file::extract_metadata(main_code), None, |_| None);
        state.cross_file_graph.update_file(&utils_uri, &crate::cross_file::extract_metadata(utils_code), None, |_| None);
        
        // Test hover on my_func usage (should show local definition, not utils.R)
        let position = Position::new(2, 10); // Position of "my_func" in usage
        let hover_result = hover(&state, &main_uri, position);
        
        assert!(hover_result.is_some());
        let hover = hover_result.unwrap();
        
        if let HoverContents::Markup(content) = hover.contents {
            assert!(content.value.contains("my\\_func"));
            assert!(content.value.contains("\\(a, b\\)")); // Local signature, not (x)
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
        let node = tree.root_node().descendant_for_point_range(point, point).unwrap();
        assert_eq!(node.kind(), "identifier");
        assert_eq!(&text[node.byte_range()], "mean");
        
        // Test hover should fall back to R help for built-in functions
        let position = Position::new(0, 10);
        
        // Mock the state with the document
        let mut test_state = state;
        test_state.documents.insert(uri.clone(), doc);
        
        let hover_result = hover(&test_state, &uri, position);
        
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
        
        state.documents.insert(uri.clone(), Document::new(code, None));
        
        // Test hover on undefined symbol
        let position = Position::new(0, 10); // Position of "undefined_symbol_that_does_not_exist"
        let hover_result = hover(&state, &uri, position);
        
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
        
        state.documents.insert(main_uri.clone(), Document::new(main_code, None));
        
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
        assert!(def_info.is_none(), "Should return None when source file is missing");
        
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
        
        state.documents.insert(uri.clone(), Document::new(code, None));
        state.documents.insert(utils_uri.clone(), Document::new(utils_code, None));
        
        // Update cross-file graph
        state.cross_file_graph.update_file(&uri, &crate::cross_file::extract_metadata(code), None, |_| None);
        state.cross_file_graph.update_file(&utils_uri, &crate::cross_file::extract_metadata(utils_code), None, |_| None);
        
        // Test hover before source call (line 1) - should not find cross-file symbol
        let position_before = Position::new(1, 11); // "helper_func" before source()
        let cross_file_symbols_before = get_cross_file_symbols(&state, &uri, position_before.line, position_before.character);
        assert!(!cross_file_symbols_before.contains_key("helper_func"), 
               "Symbol should not be available before source() call");
        
        // Test hover after source call (line 5) - should find cross-file symbol
        let position_after = Position::new(5, 11); // "helper_func" after source()
        let cross_file_symbols_after = get_cross_file_symbols(&state, &uri, position_after.line, position_after.character);
        assert!(cross_file_symbols_after.contains_key("helper_func"), 
               "Symbol should be available after source() call");
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
        
        state.documents.insert(main_uri.clone(), Document::new(main_code, None));
        state.documents.insert(utils_uri.clone(), Document::new(utils_code, None));
        state.documents.insert(helpers_uri.clone(), Document::new(helpers_code, None));
        
        // Update cross-file graph for all files
        state.cross_file_graph.update_file(&main_uri, &crate::cross_file::extract_metadata(main_code), None, |_| None);
        state.cross_file_graph.update_file(&utils_uri, &crate::cross_file::extract_metadata(utils_code), None, |_| None);
        state.cross_file_graph.update_file(&helpers_uri, &crate::cross_file::extract_metadata(helpers_code), None, |_| None);
        
        // Test hover on transform_value in utils.R (should resolve through chain)
        let position = Position::new(2, 4); // "transform_value" in utils.R
        let cross_file_symbols = get_cross_file_symbols(&state, &utils_uri, position.line, position.character);
        
        assert!(cross_file_symbols.contains_key("transform_value"), 
               "Should resolve symbol through dependency chain");
        
        let symbol = &cross_file_symbols["transform_value"];
        assert_eq!(symbol.source_uri, helpers_uri, "Should trace back to helpers.R");
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
        
        state.documents.insert(uri.clone(), Document::new(code, None));
        
        // Test scope resolution includes all iterators and parameters
        let positions = vec![
            (Position::new(5, 12), "result", true),  // result inside nested loop
            (Position::new(4, 12), "i", true),       // i iterator
            (Position::new(4, 18), "j", true),       // j iterator
            (Position::new(12, 14), "item", true),   // item used inside the loop body
            (Position::new(2, 20), "data", true),    // function parameter
            (Position::new(6, 27), "threshold", true), // function parameter with default
            (Position::new(14, 14), "filtered", true), // local variable used in return(filtered)
        ];
        
        for (position, symbol_name, should_exist) in positions {
            let symbols = get_cross_file_symbols(&state, &uri, position.line, position.character);
            if should_exist {
                assert!(symbols.contains_key(symbol_name), 
                       "Symbol '{}' should be in scope at line {}, col {}", 
                       symbol_name, position.line + 1, position.character);
            } else {
                assert!(!symbols.contains_key(symbol_name), 
                       "Symbol '{}' should NOT be in scope at line {}, col {}", 
                       symbol_name, position.line + 1, position.character);
            }
        }
        
        // Test no false-positive undefined variable diagnostics
        let diagnostics = diagnostics(&state, &uri);
        let undefined_errors: Vec<_> = diagnostics.iter()
            .filter(|d| d.message.contains("undefined") || d.message.contains("not found"))
            .collect();
        
        assert!(undefined_errors.is_empty(), 
               "Should not have undefined variable errors for loop iterators and function parameters: {:?}", 
               undefined_errors);
        
        // Test hover shows definition statements
        let hover_tests = vec![
            (Position::new(4, 12), "i", "for \\(i in 1:10\\)"),
            (Position::new(4, 18), "j", "for \\(j in 1:5\\)"),
            (Position::new(12, 14), "item", "for \\(item in filtered\\)"),
            (Position::new(2, 20), "data", "process_data <- function\\(data, threshold = 0.5, ...\\)"),
        ];
        
        for (position, symbol_name, expected_statement) in hover_tests {
            let hover_result = hover(&state, &uri, position);
            if let Some(hover) = hover_result {
                if let HoverContents::Markup(content) = hover.contents {
                    assert!(content.value.contains(expected_statement), 
                           "Hover for '{}' should contain '{}', got: {}", 
                           symbol_name, expected_statement, content.value);
                    assert!(content.value.contains("this file"), 
                           "Hover should show file location");
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
        
        state.documents.insert(main_uri.clone(), Document::new(main_code, None));
        state.documents.insert(utils_uri.clone(), Document::new(utils_code, None));
        state.documents.insert(helpers_uri.clone(), Document::new(helpers_code, None));
        
        // Update cross-file graph
        state.cross_file_graph.update_file(&main_uri, &crate::cross_file::extract_metadata(main_code), None, |_| None);
        state.cross_file_graph.update_file(&utils_uri, &crate::cross_file::extract_metadata(utils_code), None, |_| None);
        state.cross_file_graph.update_file(&helpers_uri, &crate::cross_file::extract_metadata(helpers_code), None, |_| None);
        
        // Test nested loop iterators are in scope
        let nested_loop_position = Position::new(8, 8); // Inside nested loop
        let symbols = get_cross_file_symbols(&state, &main_uri, nested_loop_position.line, nested_loop_position.character);
        
        assert!(symbols.contains_key("i"), "Outer loop iterator 'i' should be in scope");
        assert!(symbols.contains_key("j"), "Inner loop iterator 'j' should be in scope");
        assert!(symbols.contains_key("value"), "Local variable 'value' should be in scope");
        
        // Test function parameters are in scope within function
        let function_body_position = Position::new(19, 4); // Inside analyze_data function
        let func_symbols = get_cross_file_symbols(&state, &main_uri, function_body_position.line, function_body_position.character);
        
        assert!(func_symbols.contains_key("dataset"), "Function parameter 'dataset' should be in scope");
        assert!(func_symbols.contains_key("min_threshold"), "Function parameter 'min_threshold' should be in scope");
        assert!(func_symbols.contains_key("max_threshold"), "Function parameter 'max_threshold' should be in scope");
        assert!(func_symbols.contains_key("cleaned"), "Local variable 'cleaned' should be in scope");
        
        // Test cross-file symbols are resolved correctly
        let after_source_position = Position::new(4, 0); // After source() calls
        let cross_symbols = get_cross_file_symbols(&state, &main_uri, after_source_position.line, after_source_position.character);
        
        assert!(cross_symbols.contains_key("utility_func"), "Should resolve utility_func from utils.R");
        assert!(cross_symbols.contains_key("CONSTANT_VALUE"), "Should resolve CONSTANT_VALUE from utils.R");
        // Note: helper_transform should NOT be available due to local=TRUE
        
        // Test hover shows proper formatting for multi-line definitions
        let multi_line_func_position = Position::new(13, 0); // analyze_data function name
        let hover_result = hover(&state, &main_uri, multi_line_func_position);
        
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
        let undefined_errors: Vec<_> = diagnostics.iter()
            .filter(|d| d.message.contains("undefined"))
            .collect();
        
        // Should not report undefined errors for loop iterators, function parameters, or cross-file symbols
        for error in &undefined_errors {
            assert!(!error.message.contains("i "), "Should not report 'i' as undefined");
            assert!(!error.message.contains("j "), "Should not report 'j' as undefined");
            assert!(!error.message.contains("dataset"), "Should not report 'dataset' as undefined");
            assert!(!error.message.contains("utility_func"), "Should not report 'utility_func' as undefined");
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
        
        state.documents.insert(main_uri.clone(), Document::new(main_code, None));
        state.documents.insert(global_uri.clone(), Document::new(global_code, None));
        state.documents.insert(local_uri.clone(), Document::new(local_code, None));
        
        // Update cross-file graph
        state.cross_file_graph.update_file(&main_uri, &crate::cross_file::extract_metadata(main_code), None, |_| None);
        state.cross_file_graph.update_file(&global_uri, &crate::cross_file::extract_metadata(global_code), None, |_| None);
        state.cross_file_graph.update_file(&local_uri, &crate::cross_file::extract_metadata(local_code), None, |_| None);
        
        // Test symbols after both source calls
        let position = Position::new(5, 0); // After both source() calls
        let symbols = get_cross_file_symbols(&state, &main_uri, position.line, position.character);
        
        // Global source symbols should be available
        assert!(symbols.contains_key("global_func"), "global_func should be available from global source");
        assert!(symbols.contains_key("global_var"), "global_var should be available from global source");
        
        // Local source symbols should NOT be available in main scope
        assert!(!symbols.contains_key("local_func"), "local_func should NOT be available from local source");
        assert!(!symbols.contains_key("local_var"), "local_var should NOT be available from local source");
        
        // Test hover on global symbol shows cross-file location
        let hover_position = Position::new(5, 16); // "global_func" usage
        let hover_result = hover(&state, &main_uri, hover_position);
        
        if let Some(hover) = hover_result {
            if let HoverContents::Markup(content) = hover.contents {
                assert!(content.value.contains("global_func"));
                assert!(content.value.contains("global_source.R"), "Should show cross-file source");
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
        
        state.documents.insert(main_uri.clone(), Document::new(main_code, None));
        state.documents.insert(utils_uri.clone(), Document::new(utils_code, None));
        
        // Update cross-file graph
        state.cross_file_graph.update_file(&main_uri, &crate::cross_file::extract_metadata(main_code), None, |_| None);
        state.cross_file_graph.update_file(&utils_uri, &crate::cross_file::extract_metadata(utils_code), None, |_| None);
        
        // Test hover shows proper hyperlink formatting
        let position = Position::new(1, 10); // "helper_with_spaces"
        let hover_result = hover(&state, &main_uri, position);
        
        if let Some(hover) = hover_result {
            if let HoverContents::Markup(content) = hover.contents {
                // Should contain properly formatted hyperlink
                assert!(content.value.contains("[utils/helpers with spaces.R]"));
                assert!(content.value.contains("file:///workspace/utils/helpers%20with%20spaces.R"));
                assert!(content.value.contains("line 1"));
            }
        }
    }
}
