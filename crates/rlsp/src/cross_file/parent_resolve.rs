//
// cross_file/parent_resolve.rs
//
// Parent resolution for cross-file awareness
//

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tower_lsp::lsp_types::Url;

use super::cache::{ParentCacheKey, ParentResolution};
use super::config::{CallSiteDefault, CrossFileConfig};
use super::dependency::DependencyGraph;
use super::types::{BackwardDirective, CallSiteSpec, CrossFileMetadata};

/// Resolve the effective call site when a file is sourced multiple times.
/// Returns the earliest call site position using lexicographic ordering.
pub fn resolve_multiple_source_calls(
    call_sites: &[(u32, u32)], // (line, column) pairs
) -> Option<(u32, u32)> {
    call_sites.iter().copied().min()
}

/// Compute hash of CrossFileMetadata for cache key
pub fn compute_metadata_fingerprint(metadata: &CrossFileMetadata) -> u64 {
    let mut hasher = DefaultHasher::new();
    // Hash backward directives
    for directive in &metadata.sourced_by {
        directive.path.hash(&mut hasher);
        directive.directive_line.hash(&mut hasher);
        match &directive.call_site {
            CallSiteSpec::Default => 0u8.hash(&mut hasher),
            CallSiteSpec::Line(n) => {
                1u8.hash(&mut hasher);
                n.hash(&mut hasher);
            }
            CallSiteSpec::Match(s) => {
                2u8.hash(&mut hasher);
                s.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// Compute hash of reverse edges pointing to a child URI
pub fn compute_reverse_edges_hash(graph: &DependencyGraph, child_uri: &Url) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut edges: Vec<_> = graph
        .get_dependents(child_uri)
        .iter()
        .map(|e| (
            e.from.as_str(),
            e.call_site_line,
            e.call_site_column,
            e.local,
            e.chdir,
            e.is_sys_source,
        ))
        .collect();
    edges.sort();
    edges.hash(&mut hasher);
    hasher.finish()
}

/// Resolve parent for a file with backward directives
pub fn resolve_parent(
    metadata: &CrossFileMetadata,
    graph: &DependencyGraph,
    child_uri: &Url,
    config: &CrossFileConfig,
    resolve_path: impl Fn(&str) -> Option<Url>,
) -> ParentResolution {
    #[derive(Debug, Clone)]
    struct Candidate {
        parent: Url,
        call_site_line: Option<u32>,
        call_site_column: Option<u32>,
        precedence: u8,
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    // From backward directives
    for directive in &metadata.sourced_by {
        if let Some(parent_uri) = resolve_path(&directive.path) {
            let (call_site_line, call_site_column, precedence) = match &directive.call_site {
                CallSiteSpec::Line(n) => {
                    // line= is 0-based internally, treat as end-of-line
                    (Some(*n), Some(u32::MAX), 0)
                }
                CallSiteSpec::Match(_pattern) => {
                    // TODO: Implement match pattern lookup in parent file
                    // For now, treat as no call site
                    (None, None, 1)
                }
                CallSiteSpec::Default => {
                    // Use config default
                    match config.assume_call_site {
                        CallSiteDefault::End => (Some(u32::MAX), Some(u32::MAX), 3),
                        CallSiteDefault::Start => (Some(0), Some(0), 3),
                    }
                }
            };
            candidates.push(Candidate {
                parent: parent_uri,
                call_site_line,
                call_site_column,
                precedence,
            });
        }
    }

    // From reverse dependency edges
    for edge in graph.get_dependents(child_uri) {
        let (call_site_line, call_site_column) = match (edge.call_site_line, edge.call_site_column) {
            (Some(line), Some(col)) => (Some(line), Some(col)),
            _ => (None, None),
        };
        let precedence = if call_site_line.is_some() && call_site_column.is_some() { 2 } else { 3 };

        // Avoid duplicates by parent URI
        if let Some(existing) = candidates.iter_mut().find(|c| c.parent == edge.from) {
            if precedence < existing.precedence {
                existing.precedence = precedence;
                existing.call_site_line = call_site_line;
                existing.call_site_column = call_site_column;
            }
        } else {
            candidates.push(Candidate {
                parent: edge.from.clone(),
                call_site_line,
                call_site_column,
                precedence,
            });
        }
    }

    if candidates.is_empty() {
        return ParentResolution::None;
    }

    // Deterministic selection with precedence, then URI tiebreak
    candidates.sort_by(|a, b| {
        (a.precedence, a.parent.as_str()).cmp(&(b.precedence, b.parent.as_str()))
    });

    let selected = candidates.remove(0);
    if candidates.is_empty() {
        return ParentResolution::Single {
            parent_uri: selected.parent,
            call_site_line: selected.call_site_line,
            call_site_column: selected.call_site_column,
        };
    }

    ParentResolution::Ambiguous {
        selected_uri: selected.parent,
        selected_line: selected.call_site_line,
        selected_column: selected.call_site_column,
        alternatives: candidates.into_iter().map(|c| c.parent).collect(),
    }
}

/// Create a cache key for parent resolution
pub fn make_parent_cache_key(
    metadata: &CrossFileMetadata,
    graph: &DependencyGraph,
    child_uri: &Url,
) -> ParentCacheKey {
    ParentCacheKey {
        metadata_fingerprint: compute_metadata_fingerprint(metadata),
        reverse_edges_hash: compute_reverse_edges_hash(graph, child_uri),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(&format!("file:///{}", s)).unwrap()
    }

    #[test]
    fn test_resolve_multiple_source_calls() {
        let calls = vec![(10, 5), (5, 10), (5, 5)];
        let earliest = resolve_multiple_source_calls(&calls);
        assert_eq!(earliest, Some((5, 5)));
    }

    #[test]
    fn test_resolve_multiple_source_calls_empty() {
        let calls: Vec<(u32, u32)> = vec![];
        let earliest = resolve_multiple_source_calls(&calls);
        assert_eq!(earliest, None);
    }

    #[test]
    fn test_compute_metadata_fingerprint_deterministic() {
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../main.R".to_string(),
                call_site: CallSiteSpec::Line(10),
                directive_line: 0,
            }],
            ..Default::default()
        };
        let fp1 = compute_metadata_fingerprint(&meta);
        let fp2 = compute_metadata_fingerprint(&meta);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_resolve_parent_no_directives() {
        let meta = CrossFileMetadata::default();
        let graph = DependencyGraph::new();
        let config = CrossFileConfig::default();
        let child = url("child.R");

        let result = resolve_parent(&meta, &graph, &child, &config, |_| None);
        assert!(matches!(result, ParentResolution::None));
    }

    #[test]
    fn test_resolve_parent_single() {
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../main.R".to_string(),
                call_site: CallSiteSpec::Line(10),
                directive_line: 0,
            }],
            ..Default::default()
        };
        let graph = DependencyGraph::new();
        let config = CrossFileConfig::default();
        let child = url("child.R");
        let parent = url("main.R");

        let result = resolve_parent(&meta, &graph, &child, &config, |p| {
            if p == "../main.R" { Some(parent.clone()) } else { None }
        });

        match result {
            ParentResolution::Single { parent_uri, call_site_line, .. } => {
                assert_eq!(parent_uri, parent);
                assert_eq!(call_site_line, Some(10));
            }
            _ => panic!("Expected Single resolution"),
        }
    }
}