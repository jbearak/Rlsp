//
// cross_file/dependency.rs
//
// Dependency graph for cross-file awareness
//

use std::collections::{HashMap, HashSet};
use tower_lsp::lsp_types::Url;

use super::types::CrossFileMetadata;

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

/// Canonical key for edge deduplication
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

    /// Update edges for a file based on extracted metadata
    pub fn update_file(
        &mut self,
        uri: &Url,
        meta: &CrossFileMetadata,
        resolve_path: impl Fn(&str) -> Option<Url>,
    ) {
        // Remove existing edges where this file is the parent
        self.remove_forward_edges(uri);

        // Collect new edges with deduplication
        let mut seen_keys = HashSet::new();
        let mut new_edges = Vec::new();

        // Process forward sources (this file sources others)
        for source in &meta.sources {
            if let Some(to_uri) = resolve_path(&source.path) {
                let edge = DependencyEdge {
                    from: uri.clone(),
                    to: to_uri,
                    call_site_line: Some(source.line),
                    call_site_column: Some(source.column),
                    local: source.local,
                    chdir: source.chdir,
                    is_sys_source: source.is_sys_source,
                    is_directive: source.is_directive,
                };
                let key = edge.key();
                if !seen_keys.contains(&key) {
                    seen_keys.insert(key);
                    new_edges.push(edge);
                }
            }
        }

        // Add new edges
        for edge in new_edges {
            self.add_edge(edge);
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
            for edge in edges {
                if let Some(backward_edges) = self.backward.get_mut(&edge.to) {
                    backward_edges.retain(|e| &e.from != uri);
                    if backward_edges.is_empty() {
                        self.backward.remove(&edge.to);
                    }
                }
            }
        }
    }

    fn remove_backward_edges(&mut self, uri: &Url) {
        if let Some(edges) = self.backward.remove(uri) {
            for edge in edges {
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
        Url::parse(&format!("file:///{}", s)).unwrap()
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
        graph.update_file(&main, &meta, |p| {
            if p == "utils.R" { Some(utils.clone()) } else { None }
        });

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
        graph.update_file(&main, &meta, |p| {
            if p == "utils.R" { Some(utils.clone()) } else { None }
        });

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
        graph.update_file(&main, &meta, |p| {
            if p == "utils.R" { Some(utils.clone()) } else { None }
        });

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
        graph.update_file(&a, &meta_a, |p| {
            if p == "b.R" { Some(b.clone()) } else { None }
        });

        let meta_b = make_meta_with_source("c.R", 1);
        graph.update_file(&b, &meta_b, |p| {
            if p == "c.R" { Some(c.clone()) } else { None }
        });

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
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true, // Different is_directive, but same key
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                },
            ],
            ..Default::default()
        };

        graph.update_file(&main, &meta, |p| {
            if p == "utils.R" { Some(utils.clone()) } else { None }
        });

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
        graph.update_file(&main, &meta1, |p| {
            if p == "utils.R" { Some(utils.clone()) } else { None }
        });

        // Second update: main sources helpers instead
        let meta2 = make_meta_with_source("helpers.R", 10);
        graph.update_file(&main, &meta2, |p| {
            if p == "helpers.R" { Some(helpers.clone()) } else { None }
        });

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
        graph.update_file(&a, &meta_a, |p| {
            if p == "b.R" { Some(b.clone()) } else { None }
        });

        // b sources a at line 2 (creates cycle)
        let meta_b = make_meta_with_source("a.R", 2);
        graph.update_file(&b, &meta_b, |p| {
            if p == "a.R" { Some(a.clone()) } else { None }
        });

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
        graph.update_file(&a, &meta_a, |p| {
            if p == "b.R" { Some(b.clone()) } else { None }
        });

        assert!(graph.detect_cycle(&a).is_none());
        assert!(graph.detect_cycle(&b).is_none());
    }
}