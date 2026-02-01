//
// cross_file/revalidation.rs
//
// Real-time update system for cross-file awareness
//

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use tokio_util::sync::CancellationToken;
use tower_lsp::lsp_types::Url;

use super::dependency::DependencyGraph;
use super::types::CrossFileMetadata;

/// Tracks pending revalidation work per file
#[derive(Debug, Default)]
pub struct CrossFileRevalidationState {
    /// Pending revalidation tasks keyed by URI
    pending: RwLock<HashMap<Url, CancellationToken>>,
}

impl CrossFileRevalidationState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedule revalidation for a file, cancelling any pending work.
    /// Returns a cancellation token for the new task.
    pub fn schedule(&self, uri: Url) -> CancellationToken {
        let mut pending = self.pending.write().unwrap();
        // Cancel existing pending work for this URI
        if let Some(old_token) = pending.remove(&uri) {
            old_token.cancel();
        }
        let token = CancellationToken::new();
        pending.insert(uri, token.clone());
        token
    }

    /// Mark revalidation as complete
    pub fn complete(&self, uri: &Url) {
        let mut pending = self.pending.write().unwrap();
        pending.remove(uri);
    }

    /// Cancel pending revalidation for a URI
    pub fn cancel(&self, uri: &Url) {
        let mut pending = self.pending.write().unwrap();
        if let Some(token) = pending.remove(uri) {
            token.cancel();
        }
    }

    /// Cancel all pending revalidations
    pub fn cancel_all(&self) {
        let mut pending = self.pending.write().unwrap();
        for (_, token) in pending.drain() {
            token.cancel();
        }
    }
}

/// Diagnostics publish gating to enforce monotonic publishing
#[derive(Debug, Default)]
pub struct CrossFileDiagnosticsGate {
    /// Last published document version per URI
    last_published_version: RwLock<HashMap<Url, i32>>,
    /// URIs that need forced republish (dependency-triggered, version unchanged)
    force_republish: RwLock<HashSet<Url>>,
}

impl CrossFileDiagnosticsGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if diagnostics can be published for this version.
    ///
    /// Force republish allows same-version republish but NEVER older versions:
    /// - Normal: publish if `version > last_published_version`
    /// - Forced: publish if `version >= last_published_version` (same version allowed)
    /// - Never: publish if `version < last_published_version`
    pub fn can_publish(&self, uri: &Url, version: i32) -> bool {
        let last_published = self.last_published_version.read().unwrap();
        let force = self.force_republish.read().unwrap();

        match last_published.get(uri) {
            Some(&last) => {
                if version < last {
                    return false; // NEVER publish older versions
                }
                if force.contains(uri) {
                    return version >= last; // Force allows same version
                }
                version > last // Normal requires strictly newer
            }
            None => true, // No previous publish, always allowed
        }
    }

    /// Record that diagnostics were published for this version
    pub fn record_publish(&self, uri: &Url, version: i32) {
        let mut last_published = self.last_published_version.write().unwrap();
        let mut force = self.force_republish.write().unwrap();
        last_published.insert(uri.clone(), version);
        force.remove(uri);
    }

    /// Mark a URI for forced republish
    pub fn mark_force_republish(&self, uri: &Url) {
        log::trace!("Marking {} for force republish", uri);
        let mut force = self.force_republish.write().unwrap();
        force.insert(uri.clone());
    }

    /// Clear force republish flag
    pub fn clear_force_republish(&self, uri: &Url) {
        let mut force = self.force_republish.write().unwrap();
        force.remove(uri);
    }

    /// Clear all state for a URI (e.g., when document is closed)
    pub fn clear(&self, uri: &Url) {
        let mut last_published = self.last_published_version.write().unwrap();
        let mut force = self.force_republish.write().unwrap();
        last_published.remove(uri);
        force.remove(uri);
    }
}

/// Tracks client activity hints for revalidation prioritization
#[derive(Debug, Clone, Default)]
pub struct CrossFileActivityState {
    /// Currently active document URI (if any)
    pub active_uri: Option<Url>,
    /// Currently visible document URIs
    pub visible_uris: Vec<Url>,
    /// Timestamp of last activity update (for ordering)
    pub timestamp_ms: u64,
    /// Most recently changed/opened URIs (fallback ordering)
    pub recent_uris: Vec<Url>,
}

impl CrossFileActivityState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update activity state from client notification
    pub fn update(&mut self, active_uri: Option<Url>, visible_uris: Vec<Url>, timestamp_ms: u64) {
        self.active_uri = active_uri;
        self.visible_uris = visible_uris;
        self.timestamp_ms = timestamp_ms;
    }

    /// Record a document as recently changed/opened
    pub fn record_recent(&mut self, uri: Url) {
        // Remove if already present, then add to front
        self.recent_uris.retain(|u| u != &uri);
        self.recent_uris.insert(0, uri);
        // Keep bounded
        if self.recent_uris.len() > 100 {
            self.recent_uris.truncate(100);
        }
    }

    /// Remove a URI from activity tracking
    pub fn remove(&mut self, uri: &Url) {
        self.recent_uris.retain(|u| u != uri);
        if self.active_uri.as_ref() == Some(uri) {
            self.active_uri = None;
        }
        self.visible_uris.retain(|u| u != uri);
    }

    /// Get priority score for a URI (lower = higher priority)
    pub fn priority_score(&self, uri: &Url) -> usize {
        if Some(uri) == self.active_uri.as_ref() {
            return 0; // Highest priority: active
        }
        if self.visible_uris.contains(uri) {
            return 1; // Second priority: visible
        }
        // Fallback: position in recent list + 2
        self.recent_uris
            .iter()
            .position(|u| u == uri)
            .map(|p| p + 2)
            .unwrap_or(usize::MAX)
    }
}

/// Detect if a parent file's working directory has changed and find affected children.
///
/// When a parent file's `@lsp-cd` directive is added, changed, or removed, all child files
/// that have backward directives pointing to this parent need to be revalidated so they
/// can re-compute their `inherited_working_directory`.
///
/// # Arguments
/// * `parent_uri` - The URI of the parent file that was changed
/// * `old_meta` - The parent's metadata before the change (None if file was just opened)
/// * `new_meta` - The parent's metadata after the change
/// * `graph` - The dependency graph to find children with backward directives
///
/// # Returns
/// A vector of child URIs that need revalidation due to the parent's WD change.
/// Returns an empty vector if the working directory hasn't changed.
///
/// # Behavior
/// - Compares the parent's effective working directory (explicit `working_directory` or inherited)
/// - If they differ (including None -> Some, Some -> None, or Some(a) -> Some(b)),
///   finds all children that have backward directives to this parent
/// - Only returns children where the edge `is_backward_directive` is true (from backward directives)
///
/// _Requirements: 8.1, 8.2_
pub fn detect_parent_wd_change_affected_children(
    parent_uri: &Url,
    old_meta: Option<&CrossFileMetadata>,
    new_meta: &CrossFileMetadata,
    graph: &DependencyGraph,
) -> Vec<Url> {
    // Get old and new effective working directories (explicit > inherited)
    let old_wd = old_meta.and_then(|m| {
        m.working_directory
            .as_ref()
            .or(m.inherited_working_directory.as_ref())
    });
    let new_wd = new_meta
        .working_directory
        .as_ref()
        .or(new_meta.inherited_working_directory.as_ref());

    // Check if working directory changed
    let wd_changed = old_wd != new_wd;

    if !wd_changed {
        log::trace!("Parent WD unchanged for {}: {:?}", parent_uri, new_wd);
        return Vec::new();
    }

    log::trace!(
        "Parent WD changed for {}: {:?} -> {:?}",
        parent_uri,
        old_wd,
        new_wd
    );

    // Find all children with backward directives to this parent
    // get_dependencies returns edges where parent_uri is the "from" (caller),
    // meaning children that this parent sources
    let children: Vec<Url> = graph
        .get_dependencies(parent_uri)
        .into_iter()
        .filter(|edge| edge.is_backward_directive) // Only edges from backward directives
        .map(|edge| edge.to.clone())
        .collect();

    if !children.is_empty() {
        log::trace!(
            "Parent WD change affects {} children with backward directives: {:?}",
            children.len(),
            children.iter().map(|u| u.path()).collect::<Vec<_>>()
        );
    }

    children
}

/// Invalidate metadata cache entries for children affected by a parent's working directory change.
///
/// This function combines `detect_parent_wd_change_affected_children` with cache invalidation.
/// When a parent file's `@lsp-cd` directive is added, changed, or removed, this function:
/// 1. Detects which children have backward directives pointing to the parent
/// 2. Invalidates their metadata cache entries so they will re-compute their
///    `inherited_working_directory` on the next access
///
/// # Arguments
/// * `parent_uri` - The URI of the parent file that was changed
/// * `old_meta` - The parent's metadata before the change (None if file was just opened)
/// * `new_meta` - The parent's metadata after the change
/// * `graph` - The dependency graph to find children with backward directives
/// * `metadata_cache` - The metadata cache to invalidate entries in
///
/// # Returns
/// A vector of child URIs whose metadata cache entries were invalidated.
/// Returns an empty vector if the working directory hasn't changed.
///
/// # Example
/// ```ignore
/// // When parent's @lsp-cd changes, invalidate affected children
/// let affected = invalidate_children_on_parent_wd_change(
///     &parent_uri,
///     Some(&old_meta),
///     &new_meta,
///     &state.cross_file_graph,
///     &state.cross_file_meta,
/// );
/// // Then trigger revalidation for affected children
/// for child_uri in affected {
///     // Schedule revalidation...
/// }
/// ```
///
/// _Requirements: 8.1, 8.2, 8.3_
pub fn invalidate_children_on_parent_wd_change(
    parent_uri: &Url,
    old_meta: Option<&CrossFileMetadata>,
    new_meta: &CrossFileMetadata,
    graph: &DependencyGraph,
    metadata_cache: &super::cache::MetadataCache,
) -> Vec<Url> {
    // Find affected children
    let affected_children =
        detect_parent_wd_change_affected_children(parent_uri, old_meta, new_meta, graph);

    if affected_children.is_empty() {
        return affected_children;
    }

    // Invalidate metadata cache entries for all affected children
    let invalidated_count = metadata_cache.invalidate_many(&affected_children);

    log::trace!(
        "Invalidated {} metadata cache entries for children affected by parent WD change in {}",
        invalidated_count,
        parent_uri
    );

    affected_children
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///{}", name)).unwrap()
    }

    // CrossFileRevalidationState tests

    #[test]
    fn test_revalidation_schedule_returns_token() {
        let state = CrossFileRevalidationState::new();
        let uri = test_uri("test.R");
        let token = state.schedule(uri);
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_revalidation_schedule_cancels_previous() {
        let state = CrossFileRevalidationState::new();
        let uri = test_uri("test.R");

        let token1 = state.schedule(uri.clone());
        let token2 = state.schedule(uri);

        assert!(token1.is_cancelled());
        assert!(!token2.is_cancelled());
    }

    #[test]
    fn test_revalidation_complete_removes_pending() {
        let state = CrossFileRevalidationState::new();
        let uri = test_uri("test.R");

        let _token = state.schedule(uri.clone());
        state.complete(&uri);

        // Scheduling again should not cancel anything (no previous pending)
        let token2 = state.schedule(uri);
        assert!(!token2.is_cancelled());
    }

    #[test]
    fn test_revalidation_cancel() {
        let state = CrossFileRevalidationState::new();
        let uri = test_uri("test.R");

        let token = state.schedule(uri.clone());
        assert!(!token.is_cancelled());

        state.cancel(&uri);
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_revalidation_cancel_all() {
        let state = CrossFileRevalidationState::new();
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");

        let token1 = state.schedule(uri1);
        let token2 = state.schedule(uri2);

        state.cancel_all();

        assert!(token1.is_cancelled());
        assert!(token2.is_cancelled());
    }

    // CrossFileDiagnosticsGate tests

    #[test]
    fn test_gate_allows_first_publish() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");
        assert!(gate.can_publish(&uri, 1));
    }

    #[test]
    fn test_gate_allows_newer_version() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        gate.record_publish(&uri, 1);
        assert!(gate.can_publish(&uri, 2));
    }

    #[test]
    fn test_gate_blocks_older_version() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        gate.record_publish(&uri, 2);
        assert!(!gate.can_publish(&uri, 1));
    }

    #[test]
    fn test_gate_blocks_same_version_without_force() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        gate.record_publish(&uri, 1);
        assert!(!gate.can_publish(&uri, 1));
    }

    #[test]
    fn test_gate_allows_same_version_with_force() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        gate.record_publish(&uri, 1);
        gate.mark_force_republish(&uri);
        assert!(gate.can_publish(&uri, 1));
    }

    #[test]
    fn test_gate_force_still_blocks_older() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        gate.record_publish(&uri, 2);
        gate.mark_force_republish(&uri);
        assert!(!gate.can_publish(&uri, 1)); // Still blocked
    }

    #[test]
    fn test_gate_record_clears_force() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        gate.record_publish(&uri, 1);
        gate.mark_force_republish(&uri);
        gate.record_publish(&uri, 1); // Same version with force

        // Force should be cleared now
        assert!(!gate.can_publish(&uri, 1));
    }

    #[test]
    fn test_gate_clear_resets_state() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        gate.record_publish(&uri, 5);
        gate.mark_force_republish(&uri);
        gate.clear(&uri);

        // After clear, any version should be allowed
        assert!(gate.can_publish(&uri, 1));
    }

    // CrossFileActivityState tests

    #[test]
    fn test_activity_priority_active() {
        let mut state = CrossFileActivityState::new();
        let uri = test_uri("test.R");

        state.update(Some(uri.clone()), vec![], 0);
        assert_eq!(state.priority_score(&uri), 0);
    }

    #[test]
    fn test_activity_priority_visible() {
        let mut state = CrossFileActivityState::new();
        let uri = test_uri("test.R");

        state.update(None, vec![uri.clone()], 0);
        assert_eq!(state.priority_score(&uri), 1);
    }

    #[test]
    fn test_activity_priority_recent() {
        let mut state = CrossFileActivityState::new();
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");

        state.record_recent(uri1.clone());
        state.record_recent(uri2.clone());

        // uri2 was added last, so it's at position 0 -> priority 2
        assert_eq!(state.priority_score(&uri2), 2);
        // uri1 is at position 1 -> priority 3
        assert_eq!(state.priority_score(&uri1), 3);
    }

    #[test]
    fn test_activity_priority_unknown() {
        let state = CrossFileActivityState::new();
        let uri = test_uri("unknown.R");
        assert_eq!(state.priority_score(&uri), usize::MAX);
    }

    #[test]
    fn test_activity_record_recent_moves_to_front() {
        let mut state = CrossFileActivityState::new();
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");

        state.record_recent(uri1.clone());
        state.record_recent(uri2.clone());
        state.record_recent(uri1.clone()); // Move uri1 to front

        assert_eq!(state.priority_score(&uri1), 2); // Now at position 0
        assert_eq!(state.priority_score(&uri2), 3); // Now at position 1
    }

    #[test]
    fn test_activity_record_recent_bounded() {
        let mut state = CrossFileActivityState::new();

        // Add more than 100 URIs
        for i in 0..150 {
            state.record_recent(test_uri(&format!("test{}.R", i)));
        }

        assert_eq!(state.recent_uris.len(), 100);
    }

    #[test]
    fn test_activity_remove() {
        let mut state = CrossFileActivityState::new();
        let uri = test_uri("test.R");

        state.update(Some(uri.clone()), vec![uri.clone()], 0);
        state.record_recent(uri.clone());

        state.remove(&uri);

        assert!(state.active_uri.is_none());
        assert!(state.visible_uris.is_empty());
        assert!(state.recent_uris.is_empty());
    }

    // detect_parent_wd_change_affected_children tests

    #[test]
    fn test_wd_change_no_change_returns_empty() {
        // When working directory hasn't changed, no children should be returned
        let parent_uri = test_uri("parent.R");
        let graph = DependencyGraph::new();

        let old_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };
        let new_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        let affected = detect_parent_wd_change_affected_children(
            &parent_uri,
            Some(&old_meta),
            &new_meta,
            &graph,
        );

        assert!(affected.is_empty());
    }

    #[test]
    fn test_wd_change_none_to_some_detects_change() {
        // When working directory changes from None to Some, detect the change
        // This test verifies the change detection logic, not the graph lookup
        let parent_uri = test_uri("parent.R");
        let graph = DependencyGraph::new(); // Empty graph for this test

        let old_meta = CrossFileMetadata {
            working_directory: None,
            ..Default::default()
        };
        let new_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        // With empty graph, no children are returned (but change was detected internally)
        let affected = detect_parent_wd_change_affected_children(
            &parent_uri,
            Some(&old_meta),
            &new_meta,
            &graph,
        );

        // No children in graph, so empty result
        assert!(affected.is_empty());
    }

    #[test]
    fn test_wd_change_some_to_none_detects_change() {
        // When working directory changes from Some to None, detect the change
        let parent_uri = test_uri("parent.R");
        let graph = DependencyGraph::new();

        let old_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };
        let new_meta = CrossFileMetadata {
            working_directory: None,
            ..Default::default()
        };

        // The function should detect the change (even if no children in graph)
        let affected = detect_parent_wd_change_affected_children(
            &parent_uri,
            Some(&old_meta),
            &new_meta,
            &graph,
        );

        // No children in graph, so empty result
        assert!(affected.is_empty());
    }

    #[test]
    fn test_wd_change_some_to_different_some_detects_change() {
        // When working directory changes from one value to another, detect the change
        let parent_uri = test_uri("parent.R");
        let graph = DependencyGraph::new();

        let old_meta = CrossFileMetadata {
            working_directory: Some("/old/path".to_string()),
            ..Default::default()
        };
        let new_meta = CrossFileMetadata {
            working_directory: Some("/new/path".to_string()),
            ..Default::default()
        };

        let affected = detect_parent_wd_change_affected_children(
            &parent_uri,
            Some(&old_meta),
            &new_meta,
            &graph,
        );

        // No children in graph, so empty result
        assert!(affected.is_empty());
    }

    #[test]
    fn test_wd_change_no_old_meta_detects_change() {
        // When old_meta is None (file just opened), detect change if new has WD
        let parent_uri = test_uri("parent.R");
        let graph = DependencyGraph::new();

        let new_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        let affected = detect_parent_wd_change_affected_children(
            &parent_uri,
            None, // No old metadata
            &new_meta,
            &graph,
        );

        // No children in graph, so empty result
        assert!(affected.is_empty());
    }

    #[test]
    fn test_wd_change_no_old_meta_no_new_wd_no_change() {
        // When old_meta is None and new has no WD, no change detected
        let parent_uri = test_uri("parent.R");
        let graph = DependencyGraph::new();

        let new_meta = CrossFileMetadata {
            working_directory: None,
            ..Default::default()
        };

        let affected = detect_parent_wd_change_affected_children(
            &parent_uri,
            None, // No old metadata
            &new_meta,
            &graph,
        );

        // No change (None == None), so empty result
        assert!(affected.is_empty());
    }

    #[test]
    fn test_wd_change_with_directive_children_returns_children() {
        // When WD changes and there are children with backward directives, return them
        // Use the same URL pattern as dependency.rs tests
        fn url(s: &str) -> Url {
            Url::parse(&format!("file:///project/{}", s)).unwrap()
        }
        fn workspace_root() -> Url {
            Url::parse("file:///project").unwrap()
        }

        let parent_uri = url("parent.R");
        let child_uri = url("subdir/child.R");
        let mut graph = DependencyGraph::new();

        // Add a backward-directive edge from parent to child
        // (simulating what happens when child has @lsp-sourced-by: ../parent.R)
        let child_meta = CrossFileMetadata {
            sourced_by: vec![crate::cross_file::types::BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: crate::cross_file::types::CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };
        graph.update_file(&child_uri, &child_meta, Some(&workspace_root()), |_| None);

        let old_meta = CrossFileMetadata {
            working_directory: None,
            ..Default::default()
        };
        let new_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        let affected = detect_parent_wd_change_affected_children(
            &parent_uri,
            Some(&old_meta),
            &new_meta,
            &graph,
        );

        // Should return the child
        assert_eq!(affected.len(), 1);
        assert_eq!(affected[0], child_uri);
    }

    #[test]
    fn test_wd_change_with_ast_children_not_returned() {
        // When WD changes but children are from AST (not directives), don't return them
        // Use the same URL pattern as dependency.rs tests
        fn url(s: &str) -> Url {
            Url::parse(&format!("file:///project/{}", s)).unwrap()
        }
        fn workspace_root() -> Url {
            Url::parse("file:///project").unwrap()
        }

        let parent_uri = url("parent.R");
        let mut graph = DependencyGraph::new();

        // Add an AST-detected edge (not from directive)
        let parent_meta = CrossFileMetadata {
            sources: vec![crate::cross_file::types::ForwardSource {
                path: "child.R".to_string(),
                line: 5,
                column: 0,
                is_directive: false, // This is from AST detection, not directive
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root()), |_| None);

        let old_meta = CrossFileMetadata {
            working_directory: None,
            ..Default::default()
        };
        let new_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        let affected = detect_parent_wd_change_affected_children(
            &parent_uri,
            Some(&old_meta),
            &new_meta,
            &graph,
        );

        // AST children should NOT be returned (only directive children)
        assert!(affected.is_empty());
    }

    // invalidate_children_on_parent_wd_change tests

    #[test]
    fn test_invalidate_children_no_change_returns_empty() {
        // When working directory hasn't changed, no children should be invalidated
        let parent_uri = test_uri("parent.R");
        let graph = DependencyGraph::new();
        let metadata_cache = super::super::cache::MetadataCache::new();

        let old_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };
        let new_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        let affected = super::invalidate_children_on_parent_wd_change(
            &parent_uri,
            Some(&old_meta),
            &new_meta,
            &graph,
            &metadata_cache,
        );

        assert!(affected.is_empty());
    }

    #[test]
    fn test_invalidate_children_with_directive_children() {
        // When WD changes and there are children with backward directives,
        // their metadata cache entries should be invalidated
        fn url(s: &str) -> Url {
            Url::parse(&format!("file:///project/{}", s)).unwrap()
        }
        fn workspace_root() -> Url {
            Url::parse("file:///project").unwrap()
        }

        let parent_uri = url("parent.R");
        let child_uri = url("subdir/child.R");
        let mut graph = DependencyGraph::new();
        let metadata_cache = super::super::cache::MetadataCache::new();

        // Add a backward-directive edge from parent to child
        let child_meta_for_graph = CrossFileMetadata {
            sourced_by: vec![crate::cross_file::types::BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: crate::cross_file::types::CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };
        graph.update_file(
            &child_uri,
            &child_meta_for_graph,
            Some(&workspace_root()),
            |_| None,
        );

        // Add child's metadata to cache
        let child_meta = CrossFileMetadata {
            inherited_working_directory: Some("/old/path".to_string()),
            ..Default::default()
        };
        metadata_cache.insert(child_uri.clone(), child_meta);

        // Verify child is in cache
        assert!(metadata_cache.get(&child_uri).is_some());

        let old_meta = CrossFileMetadata {
            working_directory: None,
            ..Default::default()
        };
        let new_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        let affected = super::invalidate_children_on_parent_wd_change(
            &parent_uri,
            Some(&old_meta),
            &new_meta,
            &graph,
            &metadata_cache,
        );

        // Should return the child
        assert_eq!(affected.len(), 1);
        assert_eq!(affected[0], child_uri);

        // Child's metadata cache entry should be invalidated
        assert!(metadata_cache.get(&child_uri).is_none());
    }

    #[test]
    fn test_invalidate_children_multiple_children() {
        // When WD changes and there are multiple children with backward directives,
        // all their metadata cache entries should be invalidated
        fn url(s: &str) -> Url {
            Url::parse(&format!("file:///project/{}", s)).unwrap()
        }
        fn workspace_root() -> Url {
            Url::parse("file:///project").unwrap()
        }

        let parent_uri = url("parent.R");
        let child1_uri = url("child1.R");
        let child2_uri = url("child2.R");
        let mut graph = DependencyGraph::new();
        let metadata_cache = super::super::cache::MetadataCache::new();

        // Add backward-directive edges from parent to both children
        let child1_meta_for_graph = CrossFileMetadata {
            sourced_by: vec![crate::cross_file::types::BackwardDirective {
                path: "parent.R".to_string(),
                call_site: crate::cross_file::types::CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };
        let child2_meta_for_graph = CrossFileMetadata {
            sourced_by: vec![crate::cross_file::types::BackwardDirective {
                path: "parent.R".to_string(),
                call_site: crate::cross_file::types::CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };
        graph.update_file(
            &child1_uri,
            &child1_meta_for_graph,
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &child2_uri,
            &child2_meta_for_graph,
            Some(&workspace_root()),
            |_| None,
        );

        // Add children's metadata to cache
        metadata_cache.insert(child1_uri.clone(), CrossFileMetadata::default());
        metadata_cache.insert(child2_uri.clone(), CrossFileMetadata::default());

        // Verify children are in cache
        assert!(metadata_cache.get(&child1_uri).is_some());
        assert!(metadata_cache.get(&child2_uri).is_some());

        let old_meta = CrossFileMetadata {
            working_directory: Some("/old".to_string()),
            ..Default::default()
        };
        let new_meta = CrossFileMetadata {
            working_directory: Some("/new".to_string()),
            ..Default::default()
        };

        let affected = super::invalidate_children_on_parent_wd_change(
            &parent_uri,
            Some(&old_meta),
            &new_meta,
            &graph,
            &metadata_cache,
        );

        // Should return both children
        assert_eq!(affected.len(), 2);
        assert!(affected.contains(&child1_uri));
        assert!(affected.contains(&child2_uri));

        // Both children's metadata cache entries should be invalidated
        assert!(metadata_cache.get(&child1_uri).is_none());
        assert!(metadata_cache.get(&child2_uri).is_none());
    }

    #[test]
    fn test_invalidate_children_ast_children_not_affected() {
        // When WD changes but children are from AST (not directives),
        // their metadata cache entries should NOT be invalidated
        fn url(s: &str) -> Url {
            Url::parse(&format!("file:///project/{}", s)).unwrap()
        }
        fn workspace_root() -> Url {
            Url::parse("file:///project").unwrap()
        }

        let parent_uri = url("parent.R");
        let child_uri = url("child.R");
        let mut graph = DependencyGraph::new();
        let metadata_cache = super::super::cache::MetadataCache::new();

        // Add an AST-detected edge (not from directive)
        let parent_meta = CrossFileMetadata {
            sources: vec![crate::cross_file::types::ForwardSource {
                path: "child.R".to_string(),
                line: 5,
                column: 0,
                is_directive: false, // This is from AST detection, not directive
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root()), |_| None);

        // Add child's metadata to cache
        metadata_cache.insert(child_uri.clone(), CrossFileMetadata::default());

        // Verify child is in cache
        assert!(metadata_cache.get(&child_uri).is_some());

        let old_meta = CrossFileMetadata {
            working_directory: None,
            ..Default::default()
        };
        let new_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        let affected = super::invalidate_children_on_parent_wd_change(
            &parent_uri,
            Some(&old_meta),
            &new_meta,
            &graph,
            &metadata_cache,
        );

        // AST children should NOT be returned
        assert!(affected.is_empty());

        // Child's metadata cache entry should still be present
        assert!(metadata_cache.get(&child_uri).is_some());
    }
}
