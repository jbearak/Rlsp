//
// cross_file/revalidation.rs
//
// Real-time update system for cross-file awareness
//

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use tokio_util::sync::CancellationToken;
use tower_lsp::lsp_types::Url;

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
}
