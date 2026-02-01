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
#[cfg(test)]
use super::types::BackwardDirective;
use super::types::{byte_offset_to_utf16_column, CallSiteSpec, CrossFileMetadata};

/// Resolve the effective call site when a file is sourced multiple times.
/// Returns the earliest call site position using lexicographic ordering.
pub fn resolve_multiple_source_calls(
    call_sites: &[(u32, u32)], // (line, column) pairs
) -> Option<(u32, u32)> {
    call_sites.iter().copied().min()
}

/// Compute hash of CrossFileMetadata for cache key.
/// Used to detect when metadata changes require cache invalidation.
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

/// Resolve a match= pattern in parent content to find the call site.
/// Returns (line, utf16_column) of the first match on a line containing source()/sys.source() to child.
/// Falls back to first match on any line if no source() call found.
pub fn resolve_match_pattern(
    parent_content: &str,
    pattern: &str,
    child_path: &str,
) -> Option<(u32, u32)> {
    // Extract just the filename from child_path for matching
    let child_filename = std::path::Path::new(child_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(child_path);

    let mut first_match: Option<(u32, u32)> = None;

    for (line_num, line) in parent_content.lines().enumerate() {
        if let Some(byte_offset) = line.find(pattern) {
            let utf16_col = byte_offset_to_utf16_column(line, byte_offset);
            let pos = (line_num as u32, utf16_col);

            // Check if this line contains a source() or sys.source() call to the child
            let has_source_call = (line.contains("source(") || line.contains("sys.source("))
                && (line.contains(child_path) || line.contains(child_filename));

            if has_source_call {
                return Some(pos);
            }

            // Remember first match as fallback
            if first_match.is_none() {
                first_match = Some(pos);
            }
        }
    }

    first_match
}

/// Infer call site by scanning parent content for source()/sys.source() calls to child.
/// Used when call_site is Default and no reverse edge exists.
/// Returns (line, utf16_column) of the first matching source() call.
pub fn infer_call_site_from_parent(parent_content: &str, child_path: &str) -> Option<(u32, u32)> {
    let child_filename = std::path::Path::new(child_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(child_path);

    // Helper to check if a path in source() matches our child path
    // Accepts exact matches or paths that end with our filename/path
    let path_matches = |quoted_path: &str| -> bool {
        quoted_path == child_path
            || quoted_path == child_filename
            || quoted_path.ends_with(&format!("/{}", child_filename))
            || quoted_path.ends_with(&format!("/{}", child_path))
    };

    for (line_num, line) in parent_content.lines().enumerate() {
        // Look for sys.source() first (more specific), then source()
        let call_start = if let Some(pos) = line.find("sys.source(") {
            Some(pos)
        } else {
            line.find("source(")
        };

        if let Some(start) = call_start {
            // Check if this call references the child path (string literal)
            let after_call = &line[start..];

            // Extract paths from quoted strings and check for matches
            let mut matched = false;

            // Check double-quoted paths
            for part in after_call.split('"') {
                // Every other part (1st, 3rd, etc.) is the content between quotes
                if path_matches(part) {
                    matched = true;
                    break;
                }
            }

            // Check single-quoted paths if not yet matched
            if !matched {
                for part in after_call.split('\'') {
                    if path_matches(part) {
                        matched = true;
                        break;
                    }
                }
            }

            if matched {
                let utf16_col = byte_offset_to_utf16_column(line, start);
                return Some((line_num as u32, utf16_col));
            }
        }
    }

    None
}

/// Compute hash of reverse edges pointing to a child URI.
/// Used to detect when dependency graph changes require cache invalidation.
pub fn compute_reverse_edges_hash(graph: &DependencyGraph, child_uri: &Url) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut edges: Vec<_> = graph
        .get_dependents(child_uri)
        .iter()
        .map(|e| {
            (
                e.from.as_str(),
                e.call_site_line,
                e.call_site_column,
                e.local,
                e.chdir,
                e.is_sys_source,
            )
        })
        .collect();
    edges.sort();
    edges.hash(&mut hasher);
    hasher.finish()
}

/// Resolve parent for a file with backward directives.
/// This version accepts a content provider for match= resolution and call-site inference.
pub fn resolve_parent_with_content<F>(
    metadata: &CrossFileMetadata,
    graph: &DependencyGraph,
    child_uri: &Url,
    config: &CrossFileConfig,
    resolve_path: impl Fn(&str) -> Option<Url>,
    get_content: F,
) -> ParentResolution
where
    F: Fn(&Url) -> Option<String>,
{
    #[derive(Debug, Clone)]
    struct Candidate {
        parent: Url,
        call_site_line: Option<u32>,
        call_site_column: Option<u32>,
        precedence: u8,
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    // Derive child_path from child_uri for match pattern and call-site inference
    // Try to produce a relative path when possible (e.g., "subdir/child.R"), falling back to just filename
    let child_path = child_uri
        .to_file_path()
        .ok()
        .map(|full_path| {
            // Try to extract a meaningful relative path by finding the filename and potentially one parent dir
            // This helps match source("subdir/child.R") calls
            let filename = full_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();

            // Try to get parent directory name for relative path matching
            if let Some(parent) = full_path.parent() {
                if let Some(parent_name) = parent.file_name() {
                    let parent_str = parent_name.to_string_lossy();
                    // Return "parent/filename" format for better matching
                    return format!("{}/{}", parent_str, filename);
                }
            }
            filename
        })
        .unwrap_or_default();

    // From backward directives
    for directive in &metadata.sourced_by {
        if let Some(parent_uri) = resolve_path(&directive.path) {
            let (call_site_line, call_site_column, precedence) = match &directive.call_site {
                CallSiteSpec::Line(n) => {
                    // line= is 0-based internally, treat as end-of-line
                    (Some(*n), Some(u32::MAX), 0)
                }
                CallSiteSpec::Match(pattern) => {
                    // Resolve match pattern in parent content
                    if let Some(parent_content) = get_content(&parent_uri) {
                        if let Some((line, col)) =
                            resolve_match_pattern(&parent_content, pattern, &child_path)
                        {
                            (Some(line), Some(col), 0) // Same precedence as line=
                        } else {
                            // Pattern not found, fall back to config default
                            match config.assume_call_site {
                                CallSiteDefault::End => (Some(u32::MAX), Some(u32::MAX), 3),
                                CallSiteDefault::Start => (Some(0), Some(0), 3),
                            }
                        }
                    } else {
                        // Can't read parent, fall back to config default
                        match config.assume_call_site {
                            CallSiteDefault::End => (Some(u32::MAX), Some(u32::MAX), 3),
                            CallSiteDefault::Start => (Some(0), Some(0), 3),
                        }
                    }
                }
                CallSiteSpec::Default => {
                    // Check if there's a reverse edge with known call site
                    let has_reverse_edge = graph
                        .get_dependents(child_uri)
                        .iter()
                        .any(|e| e.from == parent_uri && e.call_site_line.is_some());

                    if has_reverse_edge {
                        // Will be handled by reverse edge processing below
                        // Don't add a candidate here - let the reverse edge add it
                        continue;
                    } else {
                        // Try text-inference: scan parent for source() call to child
                        if let Some(parent_content) = get_content(&parent_uri) {
                            if let Some((line, col)) =
                                infer_call_site_from_parent(&parent_content, &child_path)
                            {
                                (Some(line), Some(col), 1) // Precedence 1: inferred
                            } else {
                                // Fall back to config default
                                match config.assume_call_site {
                                    CallSiteDefault::End => (Some(u32::MAX), Some(u32::MAX), 3),
                                    CallSiteDefault::Start => (Some(0), Some(0), 3),
                                }
                            }
                        } else {
                            match config.assume_call_site {
                                CallSiteDefault::End => (Some(u32::MAX), Some(u32::MAX), 3),
                                CallSiteDefault::Start => (Some(0), Some(0), 3),
                            }
                        }
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
        let (call_site_line, call_site_column) = match (edge.call_site_line, edge.call_site_column)
        {
            (Some(line), Some(col)) => (Some(line), Some(col)),
            _ => (None, None),
        };
        let precedence = if call_site_line.is_some() && call_site_column.is_some() {
            2
        } else {
            3
        };

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
    candidates
        .sort_by(|a, b| (a.precedence, a.parent.as_str()).cmp(&(b.precedence, b.parent.as_str())));

    let selected = candidates.remove(0);

    // Filter out alternatives that point to the same parent as selected
    // This prevents false ambiguity when the same parent appears from multiple sources
    let unique_alternatives: Vec<Url> = candidates
        .into_iter()
        .filter(|c| c.parent != selected.parent)
        .map(|c| c.parent)
        .collect();

    if unique_alternatives.is_empty() {
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
        alternatives: unique_alternatives,
    }
}

/// Resolve parent for a file with backward directives (legacy version without content provider)
pub fn resolve_parent(
    metadata: &CrossFileMetadata,
    graph: &DependencyGraph,
    child_uri: &Url,
    config: &CrossFileConfig,
    resolve_path: impl Fn(&str) -> Option<Url>,
) -> ParentResolution {
    // Delegate to resolve_parent_with_content with a no-content provider
    // This means match= patterns will fall back to config default
    resolve_parent_with_content(metadata, graph, child_uri, config, resolve_path, |_| None)
}

/// Create a cache key for parent resolution.
/// Combines metadata fingerprint and reverse edges hash for efficient cache lookups.
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
            if p == "../main.R" {
                Some(parent.clone())
            } else {
                None
            }
        });

        match result {
            ParentResolution::Single {
                parent_uri,
                call_site_line,
                ..
            } => {
                assert_eq!(parent_uri, parent);
                assert_eq!(call_site_line, Some(10));
            }
            _ => panic!("Expected Single resolution"),
        }
    }

    #[test]
    fn test_resolve_parent_ambiguous() {
        let meta = CrossFileMetadata {
            sourced_by: vec![
                BackwardDirective {
                    path: "../main.R".to_string(),
                    call_site: CallSiteSpec::Default,
                    directive_line: 0,
                },
                BackwardDirective {
                    path: "../other.R".to_string(),
                    call_site: CallSiteSpec::Default,
                    directive_line: 1,
                },
            ],
            ..Default::default()
        };
        let graph = DependencyGraph::new();
        let config = CrossFileConfig::default();
        let child = url("child.R");
        let main = url("main.R");
        let other = url("other.R");

        let result = resolve_parent(&meta, &graph, &child, &config, |p| match p {
            "../main.R" => Some(main.clone()),
            "../other.R" => Some(other.clone()),
            _ => None,
        });

        match result {
            ParentResolution::Ambiguous {
                selected_uri,
                alternatives,
                ..
            } => {
                // Deterministic: main.R comes before other.R alphabetically
                assert_eq!(selected_uri, main);
                assert_eq!(alternatives.len(), 1);
                assert_eq!(alternatives[0], other);
            }
            _ => panic!("Expected Ambiguous resolution"),
        }
    }

    #[test]
    fn test_resolve_match_pattern_basic() {
        let parent_content = r#"x <- 1
source("child.R")
y <- 2"#;
        let result = resolve_match_pattern(parent_content, "source(", "child.R");
        assert_eq!(result, Some((1, 0)));
    }

    #[test]
    fn test_resolve_match_pattern_with_source_call() {
        let parent_content = r#"# source( comment
x <- 1
source("child.R")
y <- 2"#;
        // Should prefer line with actual source() call to child
        let result = resolve_match_pattern(parent_content, "source(", "child.R");
        assert_eq!(result, Some((2, 0)));
    }

    #[test]
    fn test_resolve_match_pattern_fallback() {
        let parent_content = r#"# source( comment
x <- 1
y <- 2"#;
        // No source() call to child, falls back to first match
        let result = resolve_match_pattern(parent_content, "source(", "other.R");
        assert_eq!(result, Some((0, 2))); // "# source(" at column 2
    }

    #[test]
    fn test_resolve_match_pattern_not_found() {
        let parent_content = "x <- 1\ny <- 2";
        let result = resolve_match_pattern(parent_content, "source(", "child.R");
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_match_pattern_utf16_column() {
        // Test with Unicode: 🎉 is 4 bytes UTF-8, 2 UTF-16 code units
        let parent_content = "🎉source(\"child.R\")";
        let result = resolve_match_pattern(parent_content, "source(", "child.R");
        // "🎉" is 2 UTF-16 units, so source( starts at column 2
        assert_eq!(result, Some((0, 2)));
    }

    #[test]
    fn test_infer_call_site_basic() {
        let parent_content = r#"x <- 1
source("child.R")
y <- 2"#;
        let result = infer_call_site_from_parent(parent_content, "child.R");
        assert_eq!(result, Some((1, 0)));
    }

    #[test]
    fn test_infer_call_site_sys_source() {
        let parent_content = r#"x <- 1
sys.source("child.R", envir = globalenv())
y <- 2"#;
        let result = infer_call_site_from_parent(parent_content, "child.R");
        // sys.source( starts at column 0
        assert_eq!(result, Some((1, 0)));
    }

    #[test]
    fn test_infer_call_site_named_arg() {
        let parent_content = r#"source(file = "child.R")"#;
        let result = infer_call_site_from_parent(parent_content, "child.R");
        assert_eq!(result, Some((0, 0)));
    }

    #[test]
    fn test_infer_call_site_single_quotes() {
        let parent_content = "source('child.R')";
        let result = infer_call_site_from_parent(parent_content, "child.R");
        assert_eq!(result, Some((0, 0)));
    }

    #[test]
    fn test_infer_call_site_not_found() {
        let parent_content = "source(\"other.R\")";
        let result = infer_call_site_from_parent(parent_content, "child.R");
        assert_eq!(result, None);
    }

    #[test]
    fn test_infer_call_site_filename_only() {
        // Should match by filename even if directive has relative path
        let parent_content = "source(\"child.R\")";
        let result = infer_call_site_from_parent(parent_content, "../subdir/child.R");
        assert_eq!(result, Some((0, 0)));
    }

    #[test]
    fn test_infer_call_site_subdir_path() {
        // Should match source("subdir/child.R") when child_path is "subdir/child.R"
        let parent_content = "x <- 1\nsource(\"subdir/child.R\")\ny <- 2";
        let result = infer_call_site_from_parent(parent_content, "subdir/child.R");
        assert_eq!(result, Some((1, 0)));
    }

    #[test]
    fn test_infer_call_site_nested_subdir_path() {
        // Should match when source path ends with our child path
        let parent_content = "source(\"R/subdir/child.R\")";
        let result = infer_call_site_from_parent(parent_content, "subdir/child.R");
        assert_eq!(result, Some((0, 0)));
    }

    #[test]
    fn test_infer_call_site_relative_path_match() {
        // Should match relative path in source call
        let parent_content = "source(\"./subdir/child.R\")";
        // This should match because the path ends with "subdir/child.R" after removing "./"
        let result = infer_call_site_from_parent(parent_content, "subdir/child.R");
        assert_eq!(result, Some((0, 0)));
    }

    #[test]
    fn test_resolve_parent_with_content_match() {
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../main.R".to_string(),
                call_site: CallSiteSpec::Match("source(".to_string()),
                directive_line: 0,
            }],
            ..Default::default()
        };
        let graph = DependencyGraph::new();
        let config = CrossFileConfig::default();
        let child = url("child.R");
        let parent = url("main.R");

        // Parent content should have source() call to child.R (the child file)
        let parent_content = "x <- 1\nsource(\"child.R\")\ny <- 2";

        let result = resolve_parent_with_content(
            &meta,
            &graph,
            &child,
            &config,
            |p| {
                if p == "../main.R" {
                    Some(parent.clone())
                } else {
                    None
                }
            },
            |_| Some(parent_content.to_string()),
        );

        match result {
            ParentResolution::Single {
                parent_uri,
                call_site_line,
                call_site_column,
            } => {
                assert_eq!(parent_uri, parent);
                assert_eq!(call_site_line, Some(1));
                assert_eq!(call_site_column, Some(0));
            }
            _ => panic!("Expected Single resolution"),
        }
    }

    #[test]
    fn test_resolve_parent_with_content_infer() {
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../main.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };
        let graph = DependencyGraph::new();
        let config = CrossFileConfig::default();
        let child = url("child.R");
        let parent = url("main.R");

        // Parent content should have source() call to child.R (the child file)
        let parent_content = "x <- 1\nsource(\"child.R\")\ny <- 2";

        let result = resolve_parent_with_content(
            &meta,
            &graph,
            &child,
            &config,
            |p| {
                if p == "../main.R" {
                    Some(parent.clone())
                } else {
                    None
                }
            },
            |_| Some(parent_content.to_string()),
        );

        match result {
            ParentResolution::Single {
                parent_uri,
                call_site_line,
                call_site_column,
            } => {
                assert_eq!(parent_uri, parent);
                // Should infer call site from source("child.R") call
                assert_eq!(call_site_line, Some(1));
                assert_eq!(call_site_column, Some(0));
            }
            _ => panic!("Expected Single resolution"),
        }
    }

    #[test]
    fn test_resolve_parent_no_false_ambiguity() {
        // Test that when the same parent appears from both directive and reverse edge,
        // it's not treated as ambiguous
        use super::super::dependency::DependencyGraph;
        use super::super::types::ForwardSource;

        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../oos.r".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        // Create a graph with a reverse edge from the same parent
        let mut graph = DependencyGraph::new();
        let child = url("subdir/collate.r");
        let parent = url("oos.r");

        // Simulate that the parent has a forward edge to the child
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "subdir/collate.r".to_string(),
                line: 5,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file_simple(&parent, &parent_meta);

        let config = CrossFileConfig::default();

        let result = resolve_parent_with_content(
            &meta,
            &graph,
            &child,
            &config,
            |p| {
                if p == "../oos.r" {
                    Some(parent.clone())
                } else {
                    None
                }
            },
            |_| None,
        );

        // Should be Single, not Ambiguous, because both sources point to the same parent
        match result {
            ParentResolution::Single { parent_uri, .. } => {
                assert_eq!(parent_uri, parent);
            }
            ParentResolution::Ambiguous {
                selected_uri,
                alternatives,
                ..
            } => {
                panic!(
                    "Expected Single resolution, got Ambiguous with selected={} and alternatives={:?}",
                    selected_uri, alternatives
                );
            }
            _ => panic!("Expected Single resolution, got {:?}", result),
        }
    }

    #[test]
    fn test_resolve_parent_uses_child_path_for_match() {
        // Test that match= pattern resolution uses child_path derived from child_uri,
        // not directive.path (which is the parent path)
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../main.R".to_string(),
                call_site: CallSiteSpec::Match("source(".to_string()),
                directive_line: 0,
            }],
            ..Default::default()
        };
        let graph = DependencyGraph::new();
        let config = CrossFileConfig::default();
        // Child URI with a specific filename
        let child = Url::parse("file:///project/subdir/child.R").unwrap();
        let parent = url("main.R");

        // Parent content has source() call to child.R (not main.R)
        let parent_content = "x <- 1\nsource(\"subdir/child.R\")\ny <- 2";

        let result = resolve_parent_with_content(
            &meta,
            &graph,
            &child,
            &config,
            |p| {
                if p == "../main.R" {
                    Some(parent.clone())
                } else {
                    None
                }
            },
            |_| Some(parent_content.to_string()),
        );

        match result {
            ParentResolution::Single {
                parent_uri,
                call_site_line,
                call_site_column,
            } => {
                assert_eq!(parent_uri, parent);
                // Should find the source() call to child.R at line 1
                assert_eq!(call_site_line, Some(1));
                assert_eq!(call_site_column, Some(0));
            }
            _ => panic!("Expected Single resolution"),
        }
    }

    #[test]
    fn test_resolve_parent_uses_child_path_for_inference() {
        // Test that call-site inference uses child_path derived from child_uri
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../main.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };
        let graph = DependencyGraph::new();
        let config = CrossFileConfig::default();
        // Child URI with a specific filename
        let child = Url::parse("file:///project/subdir/child.R").unwrap();
        let parent = url("main.R");

        // Parent content has source() call to child.R (not main.R)
        let parent_content = "x <- 1\nsource(\"child.R\")\ny <- 2";

        let result = resolve_parent_with_content(
            &meta,
            &graph,
            &child,
            &config,
            |p| {
                if p == "../main.R" {
                    Some(parent.clone())
                } else {
                    None
                }
            },
            |_| Some(parent_content.to_string()),
        );

        match result {
            ParentResolution::Single {
                parent_uri,
                call_site_line,
                call_site_column,
            } => {
                assert_eq!(parent_uri, parent);
                // Should infer call site from source("child.R") at line 1
                assert_eq!(call_site_line, Some(1));
                assert_eq!(call_site_column, Some(0));
            }
            _ => panic!("Expected Single resolution"),
        }
    }
}
