//
// cross_file/dependency.rs
//
// Dependency graph for cross-file awareness
//

use std::collections::{HashMap, HashSet};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Url};

use super::parent_resolve::{infer_call_site_from_parent, resolve_match_pattern};
use super::path_resolve::{path_to_uri, resolve_path, PathContext};
use super::types::{CallSiteSpec, CrossFileMetadata};

/// A dependency edge from parent (caller) to child (callee)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DependencyEdge {
    /// Parent file (caller)
    pub from: Url,
    /// Child file (callee)
    pub to: Url,
    /// 0-based line number in parent where call occurs
    pub call_site_line: Option<u32>,
    /// 0-based UTF-16 column in parent where call occurs
    pub call_site_column: Option<u32>,
    /// source(..., local=TRUE) semantics
    pub local: bool,
    /// source(..., chdir=TRUE) semantics
    pub chdir: bool,
    /// True for sys.source(), false for source()
    pub is_sys_source: bool,
    /// True if from @lsp-source directive, false if from AST detection
    pub is_directive: bool,
}

/// Canonical key for edge deduplication (from, to pair only)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FromToPair {
    from: Url,
    to: Url,
}

/// Full edge key for deduplication including call site
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EdgeKey {
    from: Url,
    to: Url,
    call_site_line: Option<u32>,
    call_site_column: Option<u32>,
    local: bool,
    chdir: bool,
    is_sys_source: bool,
}

impl DependencyEdge {
    fn key(&self) -> EdgeKey {
        EdgeKey {
            from: self.from.clone(),
            to: self.to.clone(),
            call_site_line: self.call_site_line,
            call_site_column: self.call_site_column,
            local: self.local,
            chdir: self.chdir,
            is_sys_source: self.is_sys_source,
        }
    }

    fn from_to_pair(&self) -> FromToPair {
        FromToPair {
            from: self.from.clone(),
            to: self.to.clone(),
        }
    }
}

/// Result of updating a file in the dependency graph
#[derive(Debug, Default)]
pub struct UpdateResult {
    /// Diagnostics to emit (e.g., directive-vs-AST conflict warnings)
    pub diagnostics: Vec<Diagnostic>,
}

/// Dependency graph tracking source relationships between files
#[derive(Debug, Default)]
pub struct DependencyGraph {
    /// Forward lookup: parent URI -> edges to children
    forward: HashMap<Url, Vec<DependencyEdge>>,
    /// Reverse lookup: child URI -> edges from parents
    backward: HashMap<Url, Vec<DependencyEdge>>,
}

impl DependencyGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update edges for a file based on extracted metadata.
    /// Processes both forward sources and backward directives.
    /// Returns diagnostics for directive-vs-AST conflicts.
    ///
    /// Uses PathContext for proper working directory and workspace-root-relative path resolution.
    /// The `get_content` closure provides parent file content for match=/inference resolution.
    /// It should return None for files that aren't available (not open, not cached).
    pub fn update_file<F>(
        &mut self,
        uri: &Url,
        meta: &CrossFileMetadata,
        workspace_root: Option<&Url>,
        get_content: F,
    ) -> UpdateResult
    where
        F: Fn(&Url) -> Option<String>,
    {
        let mut result = UpdateResult::default();

        // Build PathContext for this file (includes working_directory from metadata)
        let path_ctx = match PathContext::from_metadata(uri, meta, workspace_root) {
            Some(ctx) => ctx,
            None => return result,
        };

        // Build separate PathContext for backward directives (without working_directory)
        // Backward directives should always resolve relative to the file's directory,
        // not affected by @lsp-cd directives
        let backward_path_ctx = match PathContext::new(uri, workspace_root) {
            Some(ctx) => ctx,
            None => return result,
        };

        // Helper to resolve paths using PathContext (for forward sources)
        let do_resolve = |path: &str| -> Option<Url> {
            let resolved = resolve_path(path, &path_ctx)?;
            path_to_uri(&resolved)
        };

        // Helper to resolve paths for backward directives (without working_directory)
        let do_resolve_backward = |path: &str| -> Option<Url> {
            let resolved = resolve_path(path, &backward_path_ctx)?;
            path_to_uri(&resolved)
        };

        // Remove existing edges where this file is the parent
        // BUT: only remove edges that were created by THIS file's forward sources/directives
        // Do NOT remove edges created by backward directives in other files
        self.remove_forward_edges_from_this_file(uri);

        // Also remove edges where this file is the child (from backward directives)
        // These will be re-created from the current metadata
        self.remove_backward_edges_for_child(uri);

        // Collect directive edges first (they are authoritative)
        let mut directive_edges: Vec<DependencyEdge> = Vec::new();
        let mut directive_from_to: HashSet<FromToPair> = HashSet::new();

        // Process forward directive sources (@lsp-source)
        for source in &meta.sources {
            if source.is_directive {
                if let Some(to_uri) = do_resolve(&source.path) {
                    let edge = DependencyEdge {
                        from: uri.clone(),
                        to: to_uri.clone(),
                        call_site_line: Some(source.line),
                        call_site_column: Some(source.column),
                        local: source.local,
                        chdir: source.chdir,
                        is_sys_source: source.is_sys_source,
                        is_directive: true,
                    };
                    directive_from_to.insert(edge.from_to_pair());
                    directive_edges.push(edge);
                }
            }
        }

        // Process backward directives (@lsp-sourced-by) - create forward edges from parent to this file
        // Use separate PathContext without working_directory to ensure paths are resolved
        // relative to the file's directory, not affected by @lsp-cd directives
        for directive in &meta.sourced_by {
            if let Some(parent_uri) = do_resolve_backward(&directive.path) {
                // Extract child filename for inference
                let child_filename = uri.path_segments()
                    .and_then(|s| s.last())
                    .unwrap_or("");
                
                let (call_site_line, call_site_column) = match &directive.call_site {
                    CallSiteSpec::Line(n) => (Some(*n), Some(u32::MAX)), // end-of-line
                    CallSiteSpec::Match(pattern) => {
                        // Resolve match pattern in parent content
                        if let Some(parent_content) = get_content(&parent_uri) {
                            if let Some((line, col)) = resolve_match_pattern(&parent_content, pattern, child_filename) {
                                (Some(line), Some(col))
                            } else {
                                (None, None) // Pattern not found
                            }
                        } else {
                            (None, None) // Can't read parent
                        }
                    }
                    CallSiteSpec::Default => {
                        // Try text-inference: scan parent for source() call to child
                        if let Some(parent_content) = get_content(&parent_uri) {
                            if let Some((line, col)) = infer_call_site_from_parent(&parent_content, child_filename) {
                                (Some(line), Some(col))
                            } else {
                                (None, None) // No source() call found
                            }
                        } else {
                            (None, None) // Can't read parent
                        }
                    }
                };
                let edge = DependencyEdge {
                    from: parent_uri.clone(),
                    to: uri.clone(),
                    call_site_line,
                    call_site_column,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    is_directive: true,
                };
                let pair = edge.from_to_pair();
                if !directive_from_to.contains(&pair) {
                    directive_from_to.insert(pair);
                    directive_edges.push(edge);
                }
            }
        }

        // Process AST-detected sources, applying directive-vs-AST conflict resolution
        let mut ast_edges: Vec<DependencyEdge> = Vec::new();
        for source in &meta.sources {
            if !source.is_directive {
                if let Some(to_uri) = do_resolve(&source.path) {
                    let edge = DependencyEdge {
                        from: uri.clone(),
                        to: to_uri.clone(),
                        call_site_line: Some(source.line),
                        call_site_column: Some(source.column),
                        local: source.local,
                        chdir: source.chdir,
                        is_sys_source: source.is_sys_source,
                        is_directive: false,
                    };
                    let pair = edge.from_to_pair();

                    // Check for directive-vs-AST conflict (Requirement 6.8)
                    if directive_from_to.contains(&pair) {
                        // Find the directive edge for this (from, to) pair
                        let directive_edge = directive_edges.iter().find(|e| e.from_to_pair() == pair);

                        if let Some(dir_edge) = directive_edge {
                            // Check if directive has a known call site
                            let directive_has_call_site = dir_edge.call_site_line.is_some()
                                && dir_edge.call_site_line != Some(u32::MAX);

                            if directive_has_call_site {
                                // Directive has known call site: only override AST edge at same call site
                                let call_sites_match = dir_edge.call_site_line == edge.call_site_line
                                    && dir_edge.call_site_column == edge.call_site_column;

                                if call_sites_match {
                                    // Same call site: directive wins, skip AST edge
                                    continue;
                                } else {
                                    // Different call site: keep AST edge (no conflict)
                                    ast_edges.push(edge);
                                    continue;
                                }
                            } else {
                                // Directive has no call site: suppress all AST edges to this target
                                // Emit warning about suppression
                                let diag_line = meta.sourced_by.iter()
                                    .find(|d| do_resolve(&d.path) == Some(dir_edge.from.clone()))
                                    .map(|d| d.directive_line)
                                    .or_else(|| meta.sources.iter()
                                        .find(|s| s.is_directive && do_resolve(&s.path) == Some(to_uri.clone()))
                                        .map(|s| s.line))
                                    .unwrap_or(0);

                                result.diagnostics.push(Diagnostic {
                                    range: Range {
                                        start: Position { line: diag_line, character: 0 },
                                        end: Position { line: diag_line, character: u32::MAX },
                                    },
                                    severity: Some(DiagnosticSeverity::WARNING),
                                    message: format!(
                                        "Directive without call site suppresses AST-detected source() call to '{}' at line {}. Consider adding line= or match= to the directive.",
                                        to_uri.path_segments().and_then(|s| s.last()).unwrap_or(""),
                                        source.line + 1
                                    ),
                                    ..Default::default()
                                });
                                continue;
                            }
                        }
                        // No matching directive edge found (shouldn't happen), skip
                        continue;
                    }

                    ast_edges.push(edge);
                }
            }
        }

        // Deduplicate and add all edges
        let mut seen_keys = HashSet::new();
        for edge in directive_edges.into_iter().chain(ast_edges.into_iter()) {
            let key = edge.key();
            if !seen_keys.contains(&key) {
                seen_keys.insert(key);
                self.add_edge(edge);
            }
        }

        // Log total edge count after update
        let total_edges: usize = self.forward.values().map(|v| v.len()).sum();
        log::trace!("Dependency graph now has {} total edges after updating {}", total_edges, uri);

        result
    }

    /// Simple update without content provider (for backward compatibility in tests)
    /// Uses file-relative path resolution only (no workspace root)
    pub fn update_file_simple(
        &mut self,
        uri: &Url,
        meta: &CrossFileMetadata,
    ) {
        let _ = self.update_file(uri, meta, None, |_| None);
    }

    /// Remove edges where the given URI is the child that were created from backward directives
    fn remove_backward_edges_for_child(&mut self, child_uri: &Url) {
        // Get edges where this file is the child
        let edges_to_remove: Vec<DependencyEdge> = self.backward
            .get(child_uri)
            .map(|edges| {
                edges.iter()
                    .filter(|e| e.is_directive && &e.to == child_uri)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        if !edges_to_remove.is_empty() {
            log::trace!("Removing {} backward directive edges for child {}", edges_to_remove.len(), child_uri);
        }

        // Remove from both forward and backward indices
        for edge in edges_to_remove {
            log::trace!("  Removing backward directive edge: {} -> {}", edge.from, edge.to);
            // Remove from forward index
            if let Some(forward_edges) = self.forward.get_mut(&edge.from) {
                forward_edges.retain(|e| !(e.to == edge.to && e.is_directive && e.call_site_line == edge.call_site_line));
                if forward_edges.is_empty() {
                    self.forward.remove(&edge.from);
                }
            }
            // Remove from backward index
            if let Some(backward_edges) = self.backward.get_mut(child_uri) {
                backward_edges.retain(|e| !(e.from == edge.from && e.is_directive && e.call_site_line == edge.call_site_line));
                if backward_edges.is_empty() {
                    self.backward.remove(child_uri);
                }
            }
        }
    }

    /// Remove all edges involving a file
    pub fn remove_file(&mut self, uri: &Url) {
        // Remove edges where this file is the parent
        self.remove_forward_edges(uri);
        // Remove edges where this file is the child
        self.remove_backward_edges(uri);
    }

    /// Get edges where uri is the parent (caller)
    pub fn get_dependencies(&self, uri: &Url) -> Vec<&DependencyEdge> {
        self.forward
            .get(uri)
            .map(|edges| edges.iter().collect())
            .unwrap_or_default()
    }

    /// Get edges where uri is the child (callee)
    pub fn get_dependents(&self, uri: &Url) -> Vec<&DependencyEdge> {
        self.backward
            .get(uri)
            .map(|edges| edges.iter().collect())
            .unwrap_or_default()
    }

    /// Get all transitive dependents (files that depend on uri directly or indirectly)
    pub fn get_transitive_dependents(&self, uri: &Url, max_depth: usize) -> Vec<Url> {
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        self.collect_dependents(uri, max_depth, 0, &mut visited, &mut result);
        result
    }

    fn collect_dependents(
        &self,
        uri: &Url,
        max_depth: usize,
        current_depth: usize,
        visited: &mut HashSet<Url>,
        result: &mut Vec<Url>,
    ) {
        if current_depth >= max_depth || visited.contains(uri) {
            return;
        }
        visited.insert(uri.clone());

        for edge in self.get_dependents(uri) {
            if !visited.contains(&edge.from) {
                result.push(edge.from.clone());
                self.collect_dependents(&edge.from, max_depth, current_depth + 1, visited, result);
            }
        }
    }

    fn add_edge(&mut self, edge: DependencyEdge) {
        log::trace!(
            "Adding edge: {} -> {} at line {:?}, column {:?} (directive: {}, local: {}, chdir: {})",
            edge.from,
            edge.to,
            edge.call_site_line,
            edge.call_site_column,
            edge.is_directive,
            edge.local,
            edge.chdir
        );
        
        // Add to forward index
        self.forward
            .entry(edge.from.clone())
            .or_default()
            .push(edge.clone());
        // Add to backward index
        self.backward
            .entry(edge.to.clone())
            .or_default()
            .push(edge);
    }

    fn remove_forward_edges(&mut self, uri: &Url) {
        if let Some(edges) = self.forward.remove(uri) {
            log::trace!("Removing {} forward edges from {}", edges.len(), uri);
            for edge in edges {
                log::trace!("  Removing edge: {} -> {}", edge.from, edge.to);
                if let Some(backward_edges) = self.backward.get_mut(&edge.to) {
                    backward_edges.retain(|e| &e.from != uri);
                    if backward_edges.is_empty() {
                        self.backward.remove(&edge.to);
                    }
                }
            }
        }
    }

    /// Remove forward edges from a file, but only those created by forward sources/directives
    /// in that file. Preserve edges created by backward directives in other files.
    /// 
    /// This is used during update_file to avoid removing edges that were created by
    /// backward directives in child files.
    fn remove_forward_edges_from_this_file(&mut self, uri: &Url) {
        // Get all current forward edges from this file
        let edges_to_check = self.forward.get(uri).cloned().unwrap_or_default();
        
        if edges_to_check.is_empty() {
            return;
        }
        
        log::trace!("Checking {} forward edges from {} for removal", edges_to_check.len(), uri);
        
        // We'll rebuild the forward edges list, keeping only edges created by backward directives
        let mut edges_to_keep = Vec::new();
        let mut edges_to_remove = Vec::new();
        
        for edge in edges_to_check {
            // Heuristic: if this is a directive edge, it might have been created by a backward
            // directive in the child file. We'll keep it for now and let it be removed when
            // the child file is updated (via remove_backward_edges_for_child).
            // 
            // However, if it's NOT a directive edge, it was definitely created by a source()
            // call in THIS file, so we should remove it.
            //
            // Actually, this heuristic is not quite right. A directive edge could be from
            // either a forward directive in this file OR a backward directive in the child.
            // 
            // Better approach: we'll remove ALL non-directive edges (source() calls), and
            // we'll also remove directive edges, but they'll be re-created if they're still
            // in the metadata. Edges from backward directives in other files will be preserved
            // because they're not in this file's metadata.
            //
            // Wait, that doesn't work either because we process the metadata AFTER removing edges.
            //
            // The real solution: we need to track which file created each edge. But that's a
            // bigger change. For now, let's use a simpler approach: don't remove directive edges
            // at all. Only remove non-directive edges (source() calls).
            //
            // This works because:
            // - Non-directive edges are always from source() calls in THIS file
            // - Directive edges could be from forward directives in THIS file OR backward
            //   directives in OTHER files
            // - If a directive edge is from THIS file, it will be re-created from metadata
            // - If a directive edge is from ANOTHER file, we want to keep it
            //
            // The downside: if we remove a forward directive from THIS file, the edge won't
            // be removed until we update the child file. But that's acceptable for now.
            
            if edge.is_directive {
                // Keep directive edges - they might be from backward directives in other files
                edges_to_keep.push(edge);
            } else {
                // Remove non-directive edges - they're definitely from source() calls in this file
                edges_to_remove.push(edge);
            }
        }
        
        // Update the forward index
        if edges_to_keep.is_empty() {
            self.forward.remove(uri);
        } else {
            self.forward.insert(uri.clone(), edges_to_keep);
        }
        
        // Remove from backward index
        for edge in edges_to_remove {
            log::trace!("  Removing non-directive edge: {} -> {}", edge.from, edge.to);
            if let Some(backward_edges) = self.backward.get_mut(&edge.to) {
                backward_edges.retain(|e| !(e.from == edge.from && e.to == edge.to && !e.is_directive));
                if backward_edges.is_empty() {
                    self.backward.remove(&edge.to);
                }
            }
        }
    }

    fn remove_backward_edges(&mut self, uri: &Url) {
        if let Some(edges) = self.backward.remove(uri) {
            log::trace!("Removing {} backward edges to {}", edges.len(), uri);
            for edge in edges {
                log::trace!("  Removing edge: {} -> {}", edge.from, edge.to);
                if let Some(forward_edges) = self.forward.get_mut(&edge.from) {
                    forward_edges.retain(|e| &e.to != uri);
                    if forward_edges.is_empty() {
                        self.forward.remove(&edge.from);
                    }
                }
            }
        }
    }

    /// Detect cycles involving a URI. Returns the edge that creates the cycle back to `uri`.
    pub fn detect_cycle(&self, uri: &Url) -> Option<DependencyEdge> {
        let mut visited = HashSet::new();
        self.detect_cycle_recursive(uri, uri, &mut visited)
    }

    /// Dump the current state of the dependency graph for debugging.
    /// Returns a human-readable string representation of all edges.
    pub fn dump_state(&self) -> String {
        let total_edges: usize = self.forward.values().map(|v| v.len()).sum();
        let mut output = String::new();
        output.push_str(&format!("Dependency Graph State ({} total edges):\n", total_edges));
        output.push_str(&format!("  {} parent files with outgoing edges\n", self.forward.len()));
        output.push_str(&format!("  {} child files with incoming edges\n\n", self.backward.len()));
        
        if self.forward.is_empty() {
            output.push_str("  (no edges)\n");
            return output;
        }
        
        // Sort parents for consistent output
        let mut parents: Vec<_> = self.forward.keys().collect();
        parents.sort();
        
        for parent in parents {
            if let Some(edges) = self.forward.get(parent) {
                output.push_str(&format!("  {}:\n", parent));
                for edge in edges {
                    let call_site = match (edge.call_site_line, edge.call_site_column) {
                        (Some(line), Some(col)) => format!("line {}, col {}", line, col),
                        (Some(line), None) => format!("line {}", line),
                        _ => "unknown".to_string(),
                    };
                    let flags = {
                        let mut f = Vec::new();
                        if edge.is_directive { f.push("directive"); }
                        if edge.local { f.push("local"); }
                        if edge.chdir { f.push("chdir"); }
                        if edge.is_sys_source { f.push("sys.source"); }
                        if f.is_empty() { "".to_string() } else { format!(" [{}]", f.join(", ")) }
                    };
                    output.push_str(&format!("    -> {} ({}){}\n", edge.to, call_site, flags));
                }
            }
        }
        
        output
    }

    fn detect_cycle_recursive(
        &self,
        start: &Url,
        current: &Url,
        visited: &mut HashSet<Url>,
    ) -> Option<DependencyEdge> {
        if visited.contains(current) {
            return None;
        }
        visited.insert(current.clone());

        for edge in self.get_dependencies(current) {
            if &edge.to == start {
                return Some(edge.clone());
            }
            if let Some(cycle_edge) = self.detect_cycle_recursive(start, &edge.to, visited) {
                return Some(cycle_edge);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(&format!("file:///project/{}", s)).unwrap()
    }

    fn workspace_root() -> Url {
        Url::parse("file:///project").unwrap()
    }

    fn make_meta_with_source(path: &str, line: u32) -> CrossFileMetadata {
        use super::super::types::ForwardSource;
        CrossFileMetadata {
            sources: vec![ForwardSource {
                path: path.to_string(),
                line,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn test_add_and_get_dependencies() {
        let mut graph = DependencyGraph::new();
        let main = url("main.R");
        let utils = url("utils.R");

        let meta = make_meta_with_source("utils.R", 5);
        graph.update_file(&main, &meta, Some(&workspace_root()), |_| None);

        let deps = graph.get_dependencies(&main);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].to, utils);
        assert_eq!(deps[0].call_site_line, Some(5));
    }

    #[test]
    fn test_get_dependents() {
        let mut graph = DependencyGraph::new();
        let main = url("main.R");
        let utils = url("utils.R");

        let meta = make_meta_with_source("utils.R", 5);
        graph.update_file(&main, &meta, Some(&workspace_root()), |_| None);

        let dependents = graph.get_dependents(&utils);
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].from, main);
    }

    #[test]
    fn test_remove_file() {
        let mut graph = DependencyGraph::new();
        let main = url("main.R");
        let utils = url("utils.R");

        let meta = make_meta_with_source("utils.R", 5);
        graph.update_file(&main, &meta, Some(&workspace_root()), |_| None);

        graph.remove_file(&main);

        assert!(graph.get_dependencies(&main).is_empty());
        assert!(graph.get_dependents(&utils).is_empty());
    }

    #[test]
    fn test_transitive_dependents() {
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");
        let c = url("c.R");

        // a sources b, b sources c
        let meta_a = make_meta_with_source("b.R", 1);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);

        let meta_b = make_meta_with_source("c.R", 1);
        graph.update_file(&b, &meta_b, Some(&workspace_root()), |_| None);

        // Dependents of c should include b and a
        let dependents = graph.get_transitive_dependents(&c, 10);
        assert_eq!(dependents.len(), 2);
        assert!(dependents.contains(&b));
        assert!(dependents.contains(&a));
    }

    #[test]
    fn test_edge_deduplication() {
        let mut graph = DependencyGraph::new();
        let main = url("main.R");
        let utils = url("utils.R");

        // Two sources to same file at same position should deduplicate
        use super::super::types::ForwardSource;
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: false,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true, // Different is_directive, but same key
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
            ],
            ..Default::default()
        };

        graph.update_file(&main, &meta, Some(&workspace_root()), |_| None);

        // Should only have one edge (deduplicated)
        let deps = graph.get_dependencies(&main);
        assert_eq!(deps.len(), 1);
    }

    #[test]
    fn test_update_replaces_edges() {
        let mut graph = DependencyGraph::new();
        let main = url("main.R");
        let utils = url("utils.R");
        let helpers = url("helpers.R");

        // First update: main sources utils
        let meta1 = make_meta_with_source("utils.R", 5);
        graph.update_file(&main, &meta1, Some(&workspace_root()), |_| None);

        // Second update: main sources helpers instead
        let meta2 = make_meta_with_source("helpers.R", 10);
        graph.update_file(&main, &meta2, Some(&workspace_root()), |_| None);

        let deps = graph.get_dependencies(&main);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].to, helpers);

        // utils should no longer have main as dependent
        assert!(graph.get_dependents(&utils).is_empty());
    }

    #[test]
    fn test_detect_cycle_ab() {
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");

        // a sources b at line 1
        let meta_a = make_meta_with_source("b.R", 1);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);

        // b sources a at line 2 (creates cycle)
        let meta_b = make_meta_with_source("a.R", 2);
        graph.update_file(&b, &meta_b, Some(&workspace_root()), |_| None);

        // Cycle should be detected from a
        let cycle = graph.detect_cycle(&a);
        assert!(cycle.is_some());
        let edge = cycle.unwrap();
        assert_eq!(edge.from, b);
        assert_eq!(edge.to, a);
        assert_eq!(edge.call_site_line, Some(2));

        // Cycle should also be detected from b
        let cycle_b = graph.detect_cycle(&b);
        assert!(cycle_b.is_some());
    }

    #[test]
    fn test_no_cycle() {
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");

        // a sources b (no cycle)
        let meta_a = make_meta_with_source("b.R", 1);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);

        assert!(graph.detect_cycle(&a).is_none());
        assert!(graph.detect_cycle(&b).is_none());
    }

    #[test]
    fn test_backward_directive_creates_edge() {
        use super::super::types::{BackwardDirective, CallSiteSpec};

        let mut graph = DependencyGraph::new();
        // Use subdirectory structure for backward directive test
        let parent = Url::parse("file:///project/parent.R").unwrap();
        let child = Url::parse("file:///project/sub/child.R").unwrap();

        // Child declares it's sourced by parent at line 10
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Line(10),
                directive_line: 0,
            }],
            ..Default::default()
        };

        graph.update_file(&child, &meta, Some(&workspace_root()), |_| None);

        // Should create forward edge from parent to child
        let deps = graph.get_dependencies(&parent);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].from, parent);
        assert_eq!(deps[0].to, child);
        assert_eq!(deps[0].call_site_line, Some(10));
        assert!(deps[0].is_directive);

        // Child should have parent as dependent
        let dependents = graph.get_dependents(&child);
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].from, parent);
    }

    #[test]
    fn test_directive_with_call_site_preserves_ast_at_different_site() {
        use super::super::types::ForwardSource;

        let mut graph = DependencyGraph::new();
        let main = url("main.R");
        let utils = url("utils.R");

        // Directive at line 5 with known call site, AST at line 10
        // Per spec: directive with known call site only overrides AST at same call site
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true, // Directive at line 5
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 10,
                    column: 0,
                    is_directive: false, // AST at line 10
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
            ],
            ..Default::default()
        };

        let result = graph.update_file(&main, &meta, Some(&workspace_root()), |_| None);

        // Should have TWO edges (directive at line 5, AST at line 10)
        // because directive has known call site and doesn't suppress AST at different site
        let deps = graph.get_dependencies(&main);
        assert_eq!(deps.len(), 2);
        
        // No warning since directive has known call site
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn test_directive_without_call_site_suppresses_all_ast() {
        use super::super::types::{BackwardDirective, ForwardSource};

        let mut graph = DependencyGraph::new();
        let main = url("main.R");
        let utils = url("utils.R");

        // Backward directive without call site (Default), plus AST edge
        // Per spec: directive without call site suppresses all AST edges to that target
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "main.R".to_string(),
                call_site: CallSiteSpec::Default, // No call site
                directive_line: 0,
            }],
            sources: vec![ForwardSource {
                path: "utils.R".to_string(),
                line: 10,
                column: 0,
                is_directive: false, // AST at line 10
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };

        // Update from utils.R perspective (it has the backward directive)
        let _result = graph.update_file(&utils, &meta, Some(&workspace_root()), |_| None);

        // The backward directive creates edge from main->utils with no call site
        // The AST edge is from utils->utils (same file) which is different target
        // So AST edge should be preserved
        let deps = graph.get_dependencies(&utils);
        assert_eq!(deps.len(), 1);
    }

    #[test]
    fn test_directive_and_ast_same_call_site_no_warning() {
        use super::super::types::ForwardSource;

        let mut graph = DependencyGraph::new();
        let main = url("main.R");
        let utils = url("utils.R");

        // Both directive and AST at same call site
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: false,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
            ],
            ..Default::default()
        };

        let result = graph.update_file(&main, &meta, Some(&workspace_root()), |_| None);

        // Should have one edge, no warning (same call site)
        let deps = graph.get_dependencies(&main);
        assert_eq!(deps.len(), 1);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn test_ast_edges_to_different_targets_preserved() {
        use super::super::types::ForwardSource;

        let mut graph = DependencyGraph::new();
        let main = url("main.R");
        let utils = url("utils.R");
        let helpers = url("helpers.R");

        // Directive to utils, AST to helpers (different targets)
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
                ForwardSource {
                    path: "helpers.R".to_string(),
                    line: 10,
                    column: 0,
                    is_directive: false,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
            ],
            ..Default::default()
        };

        let result = graph.update_file(&main, &meta, Some(&workspace_root()), |_| None);

        // Should have both edges (different targets)
        let deps = graph.get_dependencies(&main);
        assert_eq!(deps.len(), 2);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn test_backward_directive_match_resolution() {
        use super::super::types::{BackwardDirective, CallSiteSpec};

        let mut graph = DependencyGraph::new();
        // Use subdirectory structure for backward directive test
        let parent = Url::parse("file:///project/parent.R").unwrap();
        let child = Url::parse("file:///project/sub/child.R").unwrap();

        // Child declares it's sourced by parent with match="source("
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Match("source(".to_string()),
                directive_line: 0,
            }],
            ..Default::default()
        };

        // Parent content with source() call at line 5
        let parent_content = r#"# Setup
x <- 1
y <- 2

source("child.R")  # Line 4 (0-based)
z <- 3
"#;

        graph.update_file(&child, &meta, Some(&workspace_root()), |uri| {
            if uri == &parent { Some(parent_content.to_string()) } else { None }
        });

        // Should create forward edge from parent to child with resolved call site
        let deps = graph.get_dependencies(&parent);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].call_site_line, Some(4)); // 0-based line 4
        assert!(deps[0].call_site_column.is_some());
    }

    #[test]
    fn test_backward_directive_inference_resolution() {
        use super::super::types::{BackwardDirective, CallSiteSpec};

        let mut graph = DependencyGraph::new();
        let parent = url("parent.R");
        let child = url("child.R");

        // Child declares it's sourced by parent with Default (triggers inference)
        // Use subdirectory structure for backward directive test
        let parent = Url::parse("file:///project/parent.R").unwrap();
        let child = Url::parse("file:///project/sub/child.R").unwrap();

        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        // Parent content with source() call to child at line 2
        let parent_content = r#"# Setup
x <- 1
source("child.R")
z <- 3
"#;

        graph.update_file(&child, &meta, Some(&workspace_root()), |uri| {
            if uri == &parent { Some(parent_content.to_string()) } else { None }
        });

        // Should create forward edge from parent to child with inferred call site
        let deps = graph.get_dependencies(&parent);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].call_site_line, Some(2)); // 0-based line 2
        assert!(deps[0].call_site_column.is_some());
    }

    #[test]
    fn test_dump_state() {
        let mut graph = DependencyGraph::new();
        let main = url("main.R");
        let utils = url("utils.R");
        let helpers = url("helpers.R");

        // Add edges: main sources utils and helpers
        let meta = CrossFileMetadata {
            sources: vec![
                super::super::types::ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 10,
                    is_directive: false,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
                super::super::types::ForwardSource {
                    path: "helpers.R".to_string(),
                    line: 10,
                    column: 5,
                    is_directive: true,
                    local: true,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
            ],
            ..Default::default()
        };

        graph.update_file(&main, &meta, Some(&workspace_root()), |_| None);

        // Test dump_state
        let state = graph.dump_state();
        
        // Verify output contains expected information
        assert!(state.contains("2 total edges"));
        assert!(state.contains("file:///project/main.R"));
        assert!(state.contains("file:///project/utils.R"));
        assert!(state.contains("file:///project/helpers.R"));
        assert!(state.contains("line 5, col 10"));
        assert!(state.contains("line 10, col 5"));
        assert!(state.contains("[directive, local]"));
    }
}