//
// cross_file/revalidation.rs
//
// Real-time update system for cross-file awareness
//

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(any(test, feature = "test-support"))]
use std::sync::atomic::AtomicBool;

use tokio_util::sync::CancellationToken;
use tower_lsp::lsp_types::Url;

use super::dependency::DependencyGraph;
use super::types::CrossFileMetadata;

/// Tracks pending revalidation work per file
#[derive(Debug, Default)]
pub struct CrossFileRevalidationState {
    /// Pending revalidation tasks keyed by URI, tagged with the generation
    /// returned by `schedule` so `complete` can tell its own entry from a
    /// successor's.
    pending: RwLock<HashMap<Url, (u64, CancellationToken)>>,
    /// Monotonic generation counter for `schedule`.
    next_generation: AtomicU64,
}

impl CrossFileRevalidationState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedule revalidation for a file, superseding (cancelling) any pending
    /// work. Returns the new task's generation and cancellation token.
    ///
    /// Callers must gate this on the trigger-matches-current check (see
    /// `run_debounced_diagnostics`): a worker whose trigger no longer equals
    /// the document's current `(version, revision)` — a starved edit worker,
    /// a respawned backstop that captured its trigger before an intervening
    /// edit, or a worker spawned in a previous open epoch — must exit without
    /// scheduling. Spawned workers are not polled in spawn order, so an
    /// unconditional schedule would let such a worker cancel a strictly
    /// fresher pending worker, then die on its own freshness check, leaving
    /// the newest version unpublished until the next trigger.
    pub fn schedule(&self, uri: Url) -> (u64, CancellationToken) {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let mut pending = self.pending.write().unwrap();
        // Cancel existing pending work for this URI
        if let Some((_, old_token)) = pending.remove(&uri) {
            old_token.cancel();
        }
        let token = CancellationToken::new();
        pending.insert(uri, (generation, token.clone()));
        (generation, token)
    }

    /// Mark revalidation as complete.
    ///
    /// Removes the pending entry only if it still belongs to the caller's
    /// `schedule` generation. A task that was superseded (its token cancelled
    /// by a newer `schedule`) but still runs its completion tail must not
    /// evict the successor's token — otherwise `cancel` (e.g. from
    /// `did_close`) finds no entry and the successor becomes uncancellable,
    /// reopening the stale-publish race the publish-lock cancellation
    /// re-check closes.
    pub fn complete(&self, uri: &Url, generation: u64) {
        let mut pending = self.pending.write().unwrap();
        if pending.get(uri).map(|(g, _)| *g) == Some(generation) {
            pending.remove(uri);
        }
    }

    /// Clone the latest pending owner for `uri`.
    ///
    /// Durable single-flight coordinators use this to consume the newest
    /// desired generation after a predecessor's uncancellable blocking work
    /// finishes. Callers must still use generation-checked [`Self::complete`]:
    /// removing a later owner from an older snapshot would lose an event.
    pub(crate) fn latest(&self, uri: &Url) -> Option<(u64, CancellationToken)> {
        self.pending.read().unwrap().get(uri).cloned()
    }

    /// Test-only: whether a pending revalidation entry currently exists for
    /// `uri`. Race tests use this to wait until a spawned worker has
    /// `schedule()`d (and parked in its debounce) before superseding it —
    /// spawning a competing worker earlier would race the first worker's
    /// own `schedule()`, which cancels whichever token is pending.
    #[cfg(any(test, feature = "test-support"))]
    pub fn has_pending_for_test(&self, uri: &Url) -> bool {
        self.pending.read().unwrap().contains_key(uri)
    }

    #[cfg(test)]
    pub fn pending_generation_for_test(&self, uri: &Url) -> Option<u64> {
        self.pending
            .read()
            .unwrap()
            .get(uri)
            .map(|(generation, _)| *generation)
    }

    /// Cancel pending revalidation for a URI
    pub fn cancel(&self, uri: &Url) {
        let mut pending = self.pending.write().unwrap();
        if let Some((_, token)) = pending.remove(uri) {
            token.cancel();
        }
    }

    /// Cancel all pending revalidations
    pub fn cancel_all(&self) {
        let mut pending = self.pending.write().unwrap();
        for (_, (_, token)) in pending.drain() {
            token.cancel();
        }
    }
}

/// Upper bound on outstanding force-republish markers per URI.
///
/// The counter exists so that N concurrent marks each get one matching publish
/// through the gate (see `CrossFileDiagnosticsGate`). In pathological cases a
/// document could accumulate marks faster than they are consumed (e.g. a
/// publish that bails before `record_publish` after the document version
/// changes). Past this cap further marks are coalesced — beyond 64 outstanding
/// republishes the document is being thrashed and "republish at least once
/// more" is enough.
const MAX_FORCE_REPUBLISH: u32 = 64;

/// Globally unique identifier for one diagnostic-eligible lifecycle of a URI
/// (issue #603): from the moment the URI becomes eligible to own push
/// diagnostics (a fresh `didOpen`, or a tab re-addition via
/// `raven/activeDocumentsChanged` while the document stays LSP-open) until it
/// stops being eligible (`didClose`, tab removal, or server shutdown).
///
/// Wrapping the bare `u64` prevents an epoch from being compared against a
/// version or revision at a call site. Neither of those fields can identify a
/// lifecycle: the client may reopen at the same version, and
/// `Document::revision` restarts at 0 on every open. Epochs are minted from a
/// single process-wide counter that is never reset, so a captured epoch can
/// never coincidentally match a later lifecycle of the same URI. (`u64`
/// exhaustion is unreachable in practice; the counter wraps rather than
/// panics, matching the `next_generation` idiom above.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiagnosticsEpoch(u64);

/// Diagnostics publish gating to enforce monotonic publishing
///
/// `force_republish` is a counter, not a set: each `mark_force_republish` adds
/// one to the count (clamped to `MAX_FORCE_REPUBLISH`), and each
/// `record_publish` decrements it (saturating at 0). Force is "active" while
/// count > 0. The counter avoids a race where a single publish can swallow
/// multiple concurrent forced-republish requests, leaving later publishes
/// blocked at the same version. Each marker reliably gets one matching publish
/// through the gate.
///
/// `current_epoch` tracks each URI's live diagnostic lifecycle (issue #603).
/// An entry exists iff the URI is currently diagnostic-eligible;
/// [`Self::begin_epoch`] installs a fresh one and [`Self::clear`] retires it.
/// [`Self::try_consume_publish`] refuses to commit a captured epoch that is no
/// longer current, so work retired by a close or tab removal cannot publish
/// after the URI's lifecycle is reused — even when version and revision
/// coincide with the retired lifecycle's.
///
/// Lock discipline: every method that touches more than one map acquires the
/// guards in the fixed order `current_epoch` → `last_published_version` →
/// `force_republish` and holds all of them until its mutations are complete
/// (nested lifetimes, not just acquisition order). Dropping the
/// `current_epoch` guard before the version/force commit would let a
/// concurrent retire + re-begin interleave between the epoch check and the
/// commit, reintroducing the stale-publish race the epoch exists to close.
#[derive(Debug, Default)]
pub struct CrossFileDiagnosticsGate {
    /// Last published document version per URI
    last_published_version: RwLock<HashMap<Url, i32>>,
    /// Outstanding forced-republish markers per URI (dependency-triggered, version unchanged)
    force_republish: RwLock<HashMap<Url, u32>>,
    /// Each URI's current diagnostic lifecycle epoch. Present iff the URI is
    /// currently diagnostic-eligible and has not been retired via `clear`.
    current_epoch: RwLock<HashMap<Url, DiagnosticsEpoch>>,
    /// Process-wide monotonic source for `begin_epoch`. Never reset, so
    /// retired epochs are never reused.
    next_epoch: AtomicU64,
}

impl CrossFileDiagnosticsGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a fresh diagnostic lifecycle for `uri`: mint a new epoch and
    /// reset ALL gate state for the URI in one critical section.
    ///
    /// Called via `WorldState::begin_diagnostic_lifecycle` on every "URI
    /// becomes diagnostic-eligible" transition (`did_open`, and
    /// `raven/activeDocumentsChanged`'s `added` set). Clearing
    /// `last_published_version` and `force_republish` here — rather than
    /// relying on every caller pairing this with a prior [`Self::clear`] —
    /// guarantees a new lifecycle can neither inherit a stale same-version
    /// high-water mark (which would gate out its first publish) nor surplus
    /// force markers accumulated while the URI had no live epoch (which
    /// would authorize an unrelated same-version publish later).
    pub fn begin_epoch(&self, uri: &Url) -> DiagnosticsEpoch {
        let epoch = DiagnosticsEpoch(self.next_epoch.fetch_add(1, Ordering::Relaxed));
        let mut current = self.current_epoch.write().unwrap();
        let mut last_published = self.last_published_version.write().unwrap();
        let mut force = self.force_republish.write().unwrap();
        last_published.remove(uri);
        force.remove(uri);
        current.insert(uri.clone(), epoch);
        epoch
    }

    /// The URI's live diagnostic lifecycle epoch, or `None` if the URI is
    /// not currently diagnostic-eligible (never began, or retired by
    /// [`Self::clear`]). Captured alongside version/revision when diagnostic
    /// work starts and validated by [`Self::try_consume_publish`] at commit.
    pub fn current_epoch(&self, uri: &Url) -> Option<DiagnosticsEpoch> {
        self.current_epoch.read().unwrap().get(uri).copied()
    }

    /// Check if diagnostics can be published for this version.
    ///
    /// Force republish allows same-version republish but NEVER older versions:
    /// - Normal: publish if `version > last_published_version`
    /// - Forced (count > 0): publish if `version >= last_published_version` (same version allowed)
    /// - Never: publish if `version < last_published_version`
    ///
    /// Production commit paths MUST use [`Self::try_consume_publish`] instead.
    /// Pairing `can_publish` with `record_publish` is racy: two concurrent
    /// same-version callers can both observe `force_active = true` and proceed
    /// off a single marker. This method is retained for cheap advisory
    /// pre-flight checks (e.g. early-skip before computing diagnostics) and
    /// for test fixtures.
    pub fn can_publish(&self, uri: &Url, version: i32) -> bool {
        let last_published = self.last_published_version.read().unwrap();
        let force = self.force_republish.read().unwrap();
        let force_active = force.get(uri).copied().unwrap_or(0) > 0;

        match last_published.get(uri) {
            Some(&last) => {
                if version < last {
                    return false; // NEVER publish older versions
                }
                if force_active {
                    return version >= last; // Force allows same version
                }
                version > last // Normal requires strictly newer
            }
            None => true, // No previous publish, always allowed
        }
    }

    /// Record that diagnostics were published for this version. Consumes one
    /// outstanding force-republish marker (if any) for this URI.
    ///
    /// Production commit paths MUST use [`Self::try_consume_publish`] instead.
    /// Pairing `can_publish` with `record_publish` is racy under contention.
    /// This method is retained for test fixtures.
    pub fn record_publish(&self, uri: &Url, version: i32) {
        let mut last_published = self.last_published_version.write().unwrap();
        let mut force = self.force_republish.write().unwrap();
        last_published.insert(uri.clone(), version);
        if let Some(count) = force.get_mut(uri) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                force.remove(uri);
            }
        }
    }

    /// Atomically check the publish gate and, if it would allow the publish,
    /// commit it: update `last_published_version` to `version` and consume
    /// one outstanding force-republish marker (saturating).
    ///
    /// Returns `true` iff the caller should proceed to publish. Production
    /// commit paths MUST use this method, not the
    /// `can_publish` / `record_publish` pair, to avoid a TOCTOU race where
    /// two concurrent same-version publishes each observe
    /// `force_active = true` and both proceed off a single marker.
    ///
    /// `epoch` is the lifecycle epoch the caller captured when its
    /// diagnostic work started. The commit fails closed — no version update,
    /// no marker consumed — unless it still equals the URI's current epoch,
    /// so work retired by a close/tab-removal can neither publish after the
    /// lifecycle is reused nor steal the new lifecycle's force marker. The
    /// `current_epoch` guard is held across the entire commit (see the
    /// struct-level lock-discipline note).
    ///
    /// Predicate otherwise matches `can_publish`:
    ///   - if `version < last_published`: false (never publish older)
    ///   - if force counter > 0: `version >= last_published` (same OK)
    ///   - else: `version > last_published` (strictly newer)
    pub fn try_consume_publish(&self, uri: &Url, version: i32, epoch: DiagnosticsEpoch) -> bool {
        let current = self.current_epoch.read().unwrap();
        let mut last_published = self.last_published_version.write().unwrap();
        let mut force = self.force_republish.write().unwrap();

        if current.get(uri) != Some(&epoch) {
            return false;
        }

        let allowed = match last_published.get(uri) {
            Some(&last) => {
                if version < last {
                    false
                } else if force.get(uri).copied().unwrap_or(0) > 0 {
                    version >= last
                } else {
                    version > last
                }
            }
            None => true,
        };

        if !allowed {
            return false;
        }

        last_published.insert(uri.clone(), version);
        if let Some(count) = force.get_mut(uri) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                force.remove(uri);
            }
        }
        true
    }

    /// Mark a URI for forced republish (increments the outstanding marker count
    /// up to `MAX_FORCE_REPUBLISH`).
    pub fn mark_force_republish(&self, uri: &Url) {
        let mut force = self.force_republish.write().unwrap();
        Self::mark_force_republish_locked(&mut force, uri);
    }

    /// Mark every URI in the iterator for forced republish under a single
    /// `force_republish` write-lock acquisition. Use this when fanning out
    /// to a batch of dependents so we don't pay the lock pair per URI.
    pub fn mark_force_republish_many<'a, I>(&self, uris: I)
    where
        I: IntoIterator<Item = &'a Url>,
    {
        let mut iter = uris.into_iter().peekable();
        if iter.peek().is_none() {
            return;
        }
        let mut force = self.force_republish.write().unwrap();
        for uri in iter {
            Self::mark_force_republish_locked(&mut force, uri);
        }
    }

    /// Inner increment helper, factored out so single and batch entry points
    /// share the saturation behavior of `MAX_FORCE_REPUBLISH`.
    fn mark_force_republish_locked(force: &mut HashMap<Url, u32>, uri: &Url) {
        let count = force.entry(uri.clone()).or_insert(0);
        if *count >= MAX_FORCE_REPUBLISH {
            log::debug!(
                "force_republish counter saturated at {} for {} — coalescing further marks",
                MAX_FORCE_REPUBLISH,
                uri
            );
            return;
        }
        *count += 1;
        log::trace!("Marking {} for force republish (count={})", uri, count);
    }

    /// Retire one exact lifecycle's reserved force-republish marker without
    /// claiming that diagnostics were published.
    ///
    /// Cancellation paths use this after a multi-file transaction partially
    /// commits but must withhold publication. Epoch validation prevents an
    /// obsolete ticket from consuming a marker minted for a reopened URI.
    pub fn retire_force_republish(&self, uri: &Url, epoch: DiagnosticsEpoch) {
        let current = self.current_epoch.read().unwrap();
        if current.get(uri) != Some(&epoch) {
            return;
        }
        let mut force = self.force_republish.write().unwrap();
        if let Some(count) = force.get_mut(uri) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                force.remove(uri);
            }
        }
    }

    /// Clear all outstanding force-republish markers for this URI
    pub fn clear_force_republish(&self, uri: &Url) {
        let mut force = self.force_republish.write().unwrap();
        force.remove(uri);
    }

    /// Clear all state for a URI, retiring its lifecycle epoch. Called (via
    /// `WorldState::retire_diagnostic_lifecycle`) when the document closes or
    /// its tab is removed from the editor diagnostic set. After this, no
    /// worker holding the retired epoch can commit through
    /// [`Self::try_consume_publish`].
    pub fn clear(&self, uri: &Url) {
        let mut current = self.current_epoch.write().unwrap();
        let mut last_published = self.last_published_version.write().unwrap();
        let mut force = self.force_republish.write().unwrap();
        current.remove(uri);
        last_published.remove(uri);
        force.remove(uri);
    }

    /// Clear all state for every URI, retiring every lifecycle epoch. Called
    /// on server shutdown so no in-flight diagnostic work can publish after
    /// the shutdown response.
    pub fn clear_all(&self) {
        let mut current = self.current_epoch.write().unwrap();
        let mut last_published = self.last_published_version.write().unwrap();
        let mut force = self.force_republish.write().unwrap();
        current.clear();
        last_published.clear();
        force.clear();
    }

    /// Test-only accessor for the outstanding force-republish counter for
    /// a given URI. A non-zero return means the gate is still expecting a
    /// matching publish to consume the marker; zero means every prior
    /// `mark_force_republish*` increment has been matched by a successful
    /// `try_consume_publish`. Used by integration tests that need to
    /// assert "the publish actually ran" without inspecting client-side
    /// notification queues.
    #[cfg(test)]
    pub fn force_republish_count_for_test(&self, uri: &Url) -> u32 {
        let force = self.force_republish.read().unwrap();
        force.get(uri).copied().unwrap_or(0)
    }
}

/// Test-only pause points for the diagnostics publish pipelines (issue
/// #603): lets a test park a worker in the post-compute, pre-commit window
/// so it can race a lifecycle transition (close+reopen, tab removal +
/// re-addition, shutdown) against the pending commit deterministically.
///
/// Lives as a field on `WorldState` — not a global static — so an armed
/// pause's lifetime is tied to the backend under test and cannot leak into
/// unrelated tests sharing the process.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
pub struct DiagnosticsPublishPause {
    gates: RwLock<HashMap<Url, std::sync::Arc<PauseGate>>>,
    identity: std::sync::Arc<()>,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for DiagnosticsPublishPause {
    fn default() -> Self {
        Self {
            gates: RwLock::default(),
            identity: std::sync::Arc::new(()),
        }
    }
}

/// One armed pause point. `arrived`/`release` are `Notify`s, so the
/// signal is retained even if the notifier runs before the awaiter.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default)]
pub struct PauseGate {
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
    has_arrived: AtomicBool,
}

#[cfg(any(test, feature = "test-support"))]
impl DiagnosticsPublishPause {
    /// Arm a one-shot pause for `uri`. The returned handle lets the test
    /// wait for a worker to arrive at the pause point and later release it.
    pub fn arm(&self, uri: Url) -> PauseHandle {
        let gate = std::sync::Arc::new(PauseGate::default());
        self.gates
            .write()
            .unwrap()
            .insert(uri.clone(), gate.clone());
        PauseHandle {
            gate,
            registry_identity: self.identity.clone(),
            uri,
        }
    }

    /// Arm the next one-shot pause before releasing an arrived predecessor.
    ///
    /// Repeated interceptions at the same seam must use this handoff instead
    /// of releasing and then calling [`Self::arm`]. The latter leaves a
    /// scheduler-visible gap in which the worker can cross the seam before the
    /// successor exists.
    ///
    /// # Panics
    ///
    /// Panics if `predecessor` has not arrived, belongs to another pause
    /// registry or URI, or if another pause is already armed for `uri`.
    pub fn rearm_before_release(&self, uri: Url, predecessor: PauseHandle) -> PauseHandle {
        assert!(
            predecessor.gate.has_arrived.load(Ordering::Acquire),
            "a pause successor can only replace an arrived predecessor"
        );
        assert!(
            std::sync::Arc::ptr_eq(&self.identity, &predecessor.registry_identity),
            "a pause successor must use its predecessor's registry"
        );
        assert_eq!(
            uri, predecessor.uri,
            "a pause successor must use its predecessor's URI"
        );
        let gate = std::sync::Arc::new(PauseGate::default());
        let mut gates = self.gates.write().unwrap();
        assert!(
            !gates.contains_key(&uri),
            "the arrived predecessor must already be consumed before rearming"
        );
        gates.insert(uri.clone(), gate.clone());
        drop(gates);
        predecessor.release();
        PauseHandle {
            gate,
            registry_identity: self.identity.clone(),
            uri,
        }
    }

    /// Worker-side lookup, consuming the armed entry (one-shot so a respawn
    /// for the same URI does not park again with nobody left to release
    /// it). Returns an owned handle: callers MUST drop any outer lock guard
    /// before awaiting [`PauseGate::pause`] on it.
    pub fn take_armed(&self, uri: &Url) -> Option<std::sync::Arc<PauseGate>> {
        self.gates.write().unwrap().remove(uri)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl PauseGate {
    /// Worker side: signal arrival, then park until the test releases.
    pub async fn pause(&self) {
        self.has_arrived.store(true, Ordering::Release);
        self.arrived.notify_one();
        self.release.notified().await;
    }
}

/// Test-side handle to an armed [`DiagnosticsPublishPause`] entry.
#[cfg(any(test, feature = "test-support"))]
pub struct PauseHandle {
    gate: std::sync::Arc<PauseGate>,
    registry_identity: std::sync::Arc<()>,
    uri: Url,
}

#[cfg(any(test, feature = "test-support"))]
impl PauseHandle {
    /// Wait until a worker has arrived at the pause point.
    pub async fn wait_arrived(&self) {
        self.gate.arrived.notified().await;
    }

    /// Release the parked worker.
    pub fn release(self) {
        self.gate.release.notify_one();
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
/// When a parent file's effective working directory changes — whether set
/// directly by `# raven: cd` (the `@lsp-cd` alias parses identically) or
/// inherited from its own parent — all child files that have backward
/// directives pointing to this parent need to be revalidated so they can
/// re-compute their `inherited_working_directory`.
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
/// When a parent file's effective working directory changes — whether set
/// directly by `# raven: cd` or inherited from its own parent — this function:
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
/// ```text
/// // When parent's # raven: cd changes, invalidate affected children
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

/// Compute the URIs whose diagnostics need force-republish in response to an
/// edit of `edited_uri`.
///
/// The cross-file scope of any document depends on its parents, its
/// children, AND its siblings under shared ancestors. The working set is the
/// **revalidation-consistent set** of `edited_uri`
/// ([`DependencyGraph::revalidation_consistent_set`]), which performs the two
/// traversals that together cover every file whose scope-resolution would
/// visit `edited_uri`:
///
/// 1. **Backward** (`get_transitive_dependents`): every parent that sources
///    `edited_uri` directly or transitively consumes its exported
///    interface, so their cycle/symbol diagnostics may change.
/// 2. **Forward** (`get_transitive_dependencies_multi_root` over `edited_uri`
///    plus its backward ancestors): every child sourced by `edited_uri`
///    inherits the parent's scope at the `source()` call site, so a change to
///    `edited_uri` flips descendants' undefined-variable diagnostics; and for
///    every backward ancestor `A`, the same forward walk captures `A`'s OTHER
///    descendants — siblings of `edited_uri` under a shared parent (and their
///    subtrees). Example: `parent.R` sources `child.R` and then sources
///    `grandchild.R`; the grandchild's scope at its source() call site
///    includes the child's exports because `parent.R`'s scope at that point
///    already consumed them. Editing the child must republish the grandchild
///    even though they are not directly connected in the graph.
///
/// Sharing [`DependencyGraph::revalidation_consistent_set`] with
/// `crate::handlers::collect_cross_file_nse` gives this function and NSE/func
/// collection the **identical traversal shape**, so they can no longer drift in
/// edge-selection logic. The full directed-inverse equivalence additionally
/// relies on both callers passing matching `max_depth` / `max_visited` budgets
/// and on the deliberate graph asymmetry — revalidation here runs over the FULL
/// graph, collection over the TRIMMED subgraph. For an untruncated diagnostic
/// neighborhood that asymmetry is safe-direction (`S_trimmed ⊆ S_full`). If the
/// neighborhood walk hits either bound, collection instead fails closed and uses
/// no foreign declarations, because root-dependent budget spending can otherwise
/// break the per-member inverse guarantee. See the helper's doc and CLAUDE.md
/// "Cross-file `# raven: nse` / `# raven: func` propagation".
///
/// Returns deduplicated URIs filtered through `is_open`; never includes
/// `edited_uri` itself. Returns an empty vec if neither `interface_changed`
/// nor `edges_changed`. The two halves of the consistent set are folded
/// through a single `seen` set here so each URI is emitted at most once even
/// when multiple paths reach it (e.g. diamond topologies).
///
/// `is_open` is a predicate, not a `&HashSet<Url>`, so callers can reuse
/// their existing `HashMap<Url, Document>` directly (`|u| state.documents.contains_key(u)`)
/// without cloning every URI on every edit.
pub(crate) fn compute_affected_dependents_after_edit<F>(
    edited_uri: &Url,
    interface_changed: bool,
    edges_changed: bool,
    graph: &DependencyGraph,
    is_open: F,
    max_depth: usize,
    max_visited: usize,
) -> Vec<Url>
where
    F: Fn(&Url) -> bool,
{
    if !(interface_changed || edges_changed) {
        return Vec::new();
    }

    let mut seen: std::collections::HashSet<Url> = std::collections::HashSet::new();
    let mut result: Vec<Url> = Vec::new();
    let push_if_new =
        |dep: Url, seen: &mut std::collections::HashSet<Url>, result: &mut Vec<Url>| {
            if dep == *edited_uri || !is_open(&dep) {
                return;
            }
            if seen.insert(dep.clone()) {
                result.push(dep);
            }
        };

    // (1) Backward ancestors of edited_uri, then (2 + 3) forward descendants of
    //     edited_uri AND of each backward ancestor (sibling subtrees). This is
    //     the revalidation-consistent set — the directed inverse of the
    //     NSE/func collection set in `collect_cross_file_nse`. Both build their
    //     working set from `DependencyGraph::revalidation_consistent_set`, so the
    //     two share the identical traversal shape (full equivalence also needs
    //     matching budgets + the safe trimmed-vs-full graph asymmetry — see the
    //     helper's doc). The helper returns ancestors
    //     first then descendants and does NOT exclude `edited_uri`; `push_if_new`
    //     dedups via the shared `seen` set and drops `edited_uri` / unopened
    //     files, matching the historical two-loop behavior exactly.
    for dep in graph.revalidation_consistent_set(edited_uri, max_depth, max_visited) {
        push_if_new(dep, &mut seen, &mut result);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///{}", name)).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_rearm_registers_successor_before_releasing_predecessor() {
        let pauses = std::sync::Arc::new(DiagnosticsPublishPause::default());
        let uri = test_uri("pause");
        let first = pauses.arm(uri.clone());
        let first_gate = pauses.take_armed(&uri).unwrap();
        let worker_pauses = pauses.clone();
        let worker_uri = uri.clone();
        let first_worker = tokio::spawn(async move {
            first_gate.pause().await;
            let second_gate = worker_pauses
                .take_armed(&worker_uri)
                .expect("successor is visible before the predecessor resumes");
            second_gate.pause().await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), first.wait_arrived())
            .await
            .expect("predecessor arrives");

        let second = pauses.rearm_before_release(uri.clone(), first);
        tokio::time::timeout(std::time::Duration::from_secs(1), second.wait_arrived())
            .await
            .expect("successor arrives");
        second.release();
        tokio::time::timeout(std::time::Duration::from_secs(1), first_worker)
            .await
            .expect("both pause generations finish")
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "a pause successor must use its predecessor's registry")]
    fn pause_rearm_rejects_a_predecessor_from_another_registry() {
        let pauses = DiagnosticsPublishPause::default();
        let other = DiagnosticsPublishPause::default();
        let uri = test_uri("pause");
        let predecessor = other.arm(uri.clone());
        predecessor.gate.has_arrived.store(true, Ordering::Release);

        pauses.rearm_before_release(uri, predecessor);
    }

    #[test]
    #[should_panic(expected = "a pause successor must use its predecessor's URI")]
    fn pause_rearm_rejects_a_predecessor_from_another_uri() {
        let pauses = DiagnosticsPublishPause::default();
        let predecessor = pauses.arm(test_uri("first"));
        predecessor.gate.has_arrived.store(true, Ordering::Release);

        pauses.rearm_before_release(test_uri("second"), predecessor);
    }

    #[test]
    #[should_panic(expected = "a pause successor can only replace an arrived predecessor")]
    fn pause_rearm_rejects_a_predecessor_that_has_not_arrived() {
        let pauses = DiagnosticsPublishPause::default();
        let uri = test_uri("pause");
        let predecessor = pauses.arm(uri.clone());

        pauses.rearm_before_release(uri, predecessor);
    }

    #[test]
    #[should_panic(expected = "the arrived predecessor must already be consumed before rearming")]
    fn pause_rearm_rejects_an_occupied_seam() {
        let pauses = DiagnosticsPublishPause::default();
        let uri = test_uri("pause");
        let predecessor = pauses.arm(uri.clone());
        pauses.take_armed(&uri).unwrap();
        predecessor.gate.has_arrived.store(true, Ordering::Release);
        let _occupied = pauses.arm(uri.clone());

        pauses.rearm_before_release(uri, predecessor);
    }

    // CrossFileRevalidationState tests

    #[test]
    fn test_revalidation_schedule_returns_token() {
        let state = CrossFileRevalidationState::new();
        let uri = test_uri("test.R");
        let (_, token) = state.schedule(uri);
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_revalidation_schedule_cancels_previous() {
        let state = CrossFileRevalidationState::new();
        let uri = test_uri("test.R");

        let (_, token1) = state.schedule(uri.clone());
        let (_, token2) = state.schedule(uri);

        assert!(token1.is_cancelled());
        assert!(!token2.is_cancelled());
    }

    #[test]
    fn test_revalidation_complete_removes_pending() {
        let state = CrossFileRevalidationState::new();
        let uri = test_uri("test.R");

        let (generation, _token) = state.schedule(uri.clone());
        state.complete(&uri, generation);

        // Scheduling again should not cancel anything (no previous pending)
        let (_, token2) = state.schedule(uri);
        assert!(!token2.is_cancelled());
    }

    #[test]
    fn test_revalidation_complete_ignores_superseded_generation() {
        let state = CrossFileRevalidationState::new();
        let uri = test_uri("test.R");

        let (gen1, token1) = state.schedule(uri.clone());
        let (_, token2) = state.schedule(uri.clone());
        assert!(token1.is_cancelled());

        // A superseded task's complete must not evict the successor's entry:
        // cancel() must still reach the successor's token afterwards.
        state.complete(&uri, gen1);
        state.cancel(&uri);
        assert!(token2.is_cancelled());
    }

    #[test]
    fn test_revalidation_cancel() {
        let state = CrossFileRevalidationState::new();
        let uri = test_uri("test.R");

        let (_, token) = state.schedule(uri.clone());
        assert!(!token.is_cancelled());

        state.cancel(&uri);
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_revalidation_cancel_all() {
        let state = CrossFileRevalidationState::new();
        let uri1 = test_uri("test1.R");
        let uri2 = test_uri("test2.R");

        let (_, token1) = state.schedule(uri1);
        let (_, token2) = state.schedule(uri2);

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
    fn test_gate_multi_mark_each_consumed_independently() {
        // Regression: with a HashSet-based force_republish, two concurrent
        // mark_force_republish calls were collapsed into one — so the FIRST
        // matching publish cleared the flag and the SECOND publish was blocked
        // at the same version. The counter-based gate gives each marker its
        // own publish through the gate.
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        gate.record_publish(&uri, 1);
        gate.mark_force_republish(&uri);
        gate.mark_force_republish(&uri);

        // First forced publish at the same version: allowed.
        assert!(gate.can_publish(&uri, 1));
        gate.record_publish(&uri, 1);

        // Second forced publish at the same version: still allowed (count was 2).
        assert!(gate.can_publish(&uri, 1));
        gate.record_publish(&uri, 1);

        // Both markers consumed: same-version publish blocked again.
        assert!(!gate.can_publish(&uri, 1));
    }

    #[test]
    fn test_mark_force_republish_many_marks_all_uris() {
        // S3: a bulk mark must mark every URI in one go (single critical
        // section). Behavioral assertion: after the bulk mark, each URI's
        // gate allows a same-version republish.
        let gate = CrossFileDiagnosticsGate::new();
        let uris: Vec<Url> = (0..5).map(|i| test_uri(&format!("file_{i}.R"))).collect();

        for u in &uris {
            gate.record_publish(u, 1);
        }

        gate.mark_force_republish_many(uris.iter());

        for u in &uris {
            assert!(
                gate.can_publish(u, 1),
                "{} must be force-marked after mark_force_republish_many",
                u
            );
        }
    }

    #[test]
    fn test_gate_force_republish_counter_capped() {
        // Marks beyond MAX_FORCE_REPUBLISH must be coalesced so a thrashing
        // document cannot accumulate an unbounded counter.
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        for _ in 0..(MAX_FORCE_REPUBLISH + 50) {
            gate.mark_force_republish(&uri);
        }

        // Drain MAX_FORCE_REPUBLISH publishes through the gate at the same version.
        gate.record_publish(&uri, 1);
        for _ in 0..(MAX_FORCE_REPUBLISH - 1) {
            assert!(gate.can_publish(&uri, 1));
            gate.record_publish(&uri, 1);
        }
        // Counter is saturated, not unbounded — same-version publish blocked again.
        assert!(!gate.can_publish(&uri, 1));
    }

    #[test]
    fn test_gate_try_consume_publish_no_excess_with_pre_marked_state() {
        // Race reproducer: with one outstanding force marker and N concurrent
        // try_consume_publish callers (no further marks), exactly ONE publish
        // must succeed. With the legacy can_publish + record_publish pair, two
        // racing callers both observe force_active = true and both proceed off
        // a single marker — this assertion would fail on that buggy code path.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};
        use std::thread;

        let gate = Arc::new(CrossFileDiagnosticsGate::new());
        let uri = test_uri("test.R");

        let epoch = gate.begin_epoch(&uri);
        gate.record_publish(&uri, 1);
        gate.mark_force_republish(&uri);

        const N_THREADS: usize = 32;
        let barrier = Arc::new(Barrier::new(N_THREADS));
        let successes = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(N_THREADS);

        for _ in 0..N_THREADS {
            let gate = gate.clone();
            let uri = uri.clone();
            let barrier = barrier.clone();
            let successes = successes.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                if gate.try_consume_publish(&uri, 1, epoch) {
                    successes.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            successes.load(Ordering::Relaxed),
            1,
            "One marker must permit exactly one publish, even under N racing consumers"
        );
    }

    #[test]
    fn test_gate_try_consume_publish_atomic_under_concurrency() {
        // Contract test: each thread marks once via mark_force_republish, then
        // races on try_consume_publish at the same version. Asserts
        // successes == N (one publish per mark). Documents the per-mark
        // contract under contention.
        //
        // Contrast with test_gate_try_consume_publish_no_excess_with_pre_marked_state,
        // which asserts the inverse: with one pre-set marker and N racing
        // try_consume_publish callers, exactly one publish must succeed
        // (no excess). Together they pin both directions of the per-marker
        // invariant: marks and successful consumes are 1:1.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};
        use std::thread;

        let gate = Arc::new(CrossFileDiagnosticsGate::new());
        let uri = test_uri("test.R");

        let epoch = gate.begin_epoch(&uri);
        gate.record_publish(&uri, 1);

        const N_THREADS: usize = 32;
        let barrier = Arc::new(Barrier::new(N_THREADS));
        let successes = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(N_THREADS);

        for _ in 0..N_THREADS {
            let gate = gate.clone();
            let uri = uri.clone();
            let barrier = barrier.clone();
            let successes = successes.clone();
            handles.push(thread::spawn(move || {
                gate.mark_force_republish(&uri);
                barrier.wait();
                if gate.try_consume_publish(&uri, 1, epoch) {
                    successes.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            successes.load(Ordering::Relaxed),
            N_THREADS,
            "Each of N marks should permit exactly one publish"
        );
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

    // Lifecycle-epoch tests (issue #603)

    #[test]
    fn test_begin_epoch_mints_unique_epochs() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri1 = test_uri("a.R");
        let uri2 = test_uri("b.R");

        let e1 = gate.begin_epoch(&uri1);
        let e2 = gate.begin_epoch(&uri2);
        let e3 = gate.begin_epoch(&uri1); // same URI, new lifecycle

        assert_ne!(e1, e2);
        assert_ne!(e1, e3);
        assert_ne!(e2, e3);
        assert_eq!(gate.current_epoch(&uri1), Some(e3));
        assert_eq!(gate.current_epoch(&uri2), Some(e2));
    }

    #[test]
    fn test_current_epoch_absent_before_begin_and_after_clear() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        assert_eq!(gate.current_epoch(&uri), None);
        let e = gate.begin_epoch(&uri);
        assert_eq!(gate.current_epoch(&uri), Some(e));
        gate.clear(&uri);
        assert_eq!(gate.current_epoch(&uri), None);
    }

    #[test]
    fn test_try_consume_publish_rejects_stale_epoch_and_preserves_marker() {
        // The core #603 regression: a retired lifecycle's epoch must be
        // refused at commit even when the force marker + same-version
        // predicate would otherwise allow the publish — and the refusal must
        // NOT consume the marker meant for the live lifecycle.
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        let old = gate.begin_epoch(&uri);
        assert!(gate.try_consume_publish(&uri, 1, old));

        let new = gate.begin_epoch(&uri); // reopen: retires `old`
        assert!(gate.try_consume_publish(&uri, 1, new));
        gate.mark_force_republish(&uri);

        assert!(
            !gate.try_consume_publish(&uri, 1, old),
            "stale epoch must be refused despite an active force marker"
        );
        assert_eq!(
            gate.force_republish_count_for_test(&uri),
            1,
            "a refused stale-epoch commit must not consume the marker"
        );
        assert!(
            gate.try_consume_publish(&uri, 1, new),
            "the live lifecycle must still get the marker's publish"
        );
    }

    #[test]
    fn retire_force_republish_is_exactly_lifecycle_scoped() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");
        let retired = gate.begin_epoch(&uri);
        let current = gate.begin_epoch(&uri);
        gate.mark_force_republish(&uri);

        gate.retire_force_republish(&uri, retired);
        assert_eq!(gate.force_republish_count_for_test(&uri), 1);

        gate.retire_force_republish(&uri, current);
        assert_eq!(gate.force_republish_count_for_test(&uri), 0);
    }

    #[test]
    fn test_try_consume_publish_rejects_retired_epoch() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        let e = gate.begin_epoch(&uri);
        gate.clear(&uri); // didClose / tab removal
        assert!(
            !gate.try_consume_publish(&uri, 1, e),
            "no live epoch: commit must fail closed"
        );
        assert!(
            gate.can_publish(&uri, 1),
            "the refused commit must not have recorded a version"
        );
    }

    #[test]
    fn test_begin_epoch_resets_version_state() {
        // A duplicate lifecycle start (no intervening clear) must not
        // inherit the previous lifecycle's same-version high-water mark,
        // which would gate out the new lifecycle's first publish.
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        let e1 = gate.begin_epoch(&uri);
        assert!(gate.try_consume_publish(&uri, 1, e1));

        let e2 = gate.begin_epoch(&uri);
        assert!(
            gate.try_consume_publish(&uri, 1, e2),
            "fresh lifecycle must publish at the same version without a force marker"
        );
    }

    #[test]
    fn test_begin_epoch_drops_orphaned_force_markers() {
        // Markers accumulated while the URI had no live epoch (e.g. a hidden
        // tab-ineligible document swept up in bulk mark_force_republish_many
        // calls whose workers bail at the eligibility check) must not
        // survive into the new lifecycle, where a surplus marker would
        // authorize an unrelated same-version publish.
        let gate = CrossFileDiagnosticsGate::new();
        let uri = test_uri("test.R");

        gate.mark_force_republish(&uri);
        gate.mark_force_republish(&uri);

        let e = gate.begin_epoch(&uri);
        assert_eq!(
            gate.force_republish_count_for_test(&uri),
            0,
            "begin_epoch must start with clean force state"
        );
        assert!(gate.try_consume_publish(&uri, 1, e));
        assert!(
            !gate.try_consume_publish(&uri, 1, e),
            "no marker may leak into the new lifecycle"
        );
    }

    #[test]
    fn test_clear_all_retires_every_epoch() {
        let gate = CrossFileDiagnosticsGate::new();
        let uri1 = test_uri("a.R");
        let uri2 = test_uri("b.R");

        let e1 = gate.begin_epoch(&uri1);
        let e2 = gate.begin_epoch(&uri2);
        gate.clear_all();

        assert_eq!(gate.current_epoch(&uri1), None);
        assert_eq!(gate.current_epoch(&uri2), None);
        assert!(!gate.try_consume_publish(&uri1, 1, e1));
        assert!(!gate.try_consume_publish(&uri2, 1, e2));
    }

    #[test]
    fn test_gate_clear_races_try_consume_publish_without_wedging() {
        // The gate is presented as the lifecycle authority, so its internal
        // lock discipline must hold even when clear() runs concurrently with
        // try_consume_publish() (nothing at the gate level enforces the
        // outer WorldState lock choreography). Assert two things under real
        // contention: no deadlock, and the nested-guard-lifetime invariant —
        // try_consume_publish must hold the `current_epoch` guard across the
        // entire version/force commit.
        //
        // Detector: the lifecycle thread retires the epoch with a single
        // clear() and never re-mints it. clear() takes all three write
        // guards, so in a correct implementation every commit either fully
        // precedes it (its `last_published_version` entry is then removed by
        // the clear) or fully follows it (the epoch check fails; no entry is
        // written). Only an implementation that releases the epoch guard
        // between the check and the commit can interleave with the clear and
        // write an entry AFTER the clear removed everything — so any
        // surviving entry after all threads join proves the guard-lifetime
        // bug. `can_publish(uri, i32::MIN)` is true iff no entry survived
        // (consumers only commit versions >= 0).
        //
        // This is a STRESS test: the postcondition is sound (a failure is
        // always a real bug; no false positives), but detection is
        // opportunistic — nothing can force clear() into a hypothetical
        // broken implementation's dropped-guard window from outside the
        // gate's public API. The deterministic epoch-guard coverage lives in
        // test_try_consume_publish_rejects_stale_epoch_and_preserves_marker
        // and test_try_consume_publish_rejects_retired_epoch, which pin the
        // check-under-guard behavior directly. What this test guarantees on
        // every run is the no-deadlock property under real contention.
        use std::sync::{Arc, Barrier};
        use std::thread;

        let gate = Arc::new(CrossFileDiagnosticsGate::new());
        let uri = test_uri("test.R");
        let initial = gate.begin_epoch(&uri);

        const N_CONSUMERS: usize = 16;
        const N_CYCLES: usize = 200;
        let barrier = Arc::new(Barrier::new(N_CONSUMERS + 1));
        let mut handles = Vec::new();

        for _ in 0..N_CONSUMERS {
            let gate = gate.clone();
            let uri = uri.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                for v in 0..N_CYCLES as i32 {
                    gate.try_consume_publish(&uri, v, initial);
                }
            }));
        }

        let lifecycle_gate = gate.clone();
        let lifecycle_uri = uri.clone();
        let lifecycle_barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            lifecycle_barrier.wait();
            lifecycle_gate.clear(&lifecycle_uri);
        }));

        for h in handles {
            h.join().unwrap();
        }

        assert!(
            gate.can_publish(&uri, i32::MIN),
            "a last_published_version entry survived the retiring clear: a \
             commit interleaved between the epoch check and the version/force \
             write, so the current_epoch guard was not held across the commit"
        );
        assert_eq!(gate.current_epoch(&uri), None);
    }

    #[test]
    fn test_force_marker_persists_until_consumed_by_publish() {
        // Documents the gate semantics that motivate `did_open`'s
        // deferred-marking design (commits 9f4bc45 + 01e3411).
        //
        // `mark_force_republish` increments a counter; `record_publish`
        // decrements it. A marker is *not tied* to the work item that
        // produced it — once incremented, it persists until any publish
        // consumes it, including a publish triggered by an unrelated
        // later edit.
        //
        // Consequence for `did_open`: if the initial cap pass marks a URI
        // and re-enrichment then evicts that URI from `work_items`, the
        // marker is left behind and the next unrelated same-version
        // publish for that URI slips through the gate. The fix is to
        // defer marking until after re-enrichment has settled
        // `work_items`, so URIs that won't actually be republished by
        // this trigger never receive a marker.
        //
        // This test exercises the gate-side leak directly: a URI that has
        // an outstanding marker passes the same-version gate even though
        // nothing in this test "owns" the planned republish.
        let gate = CrossFileDiagnosticsGate::new();
        let evicted = test_uri("evicted.R");

        // Baseline: a URI that has been published at v=1 and has no
        // marker is blocked from same-version republish.
        gate.record_publish(&evicted, 1);
        assert!(
            !gate.can_publish(&evicted, 1),
            "no marker → same-version publish blocked"
        );

        // Pre-fix `did_open`: the initial cap pass would mark this URI
        // before re-enrichment had a chance to evict it.
        gate.mark_force_republish_many([&evicted].iter().copied());

        // Pre-fix bug: even though re-enrichment "evicts" `evicted` from
        // `work_items` (modeled here as: nothing further consumes the
        // marker on its behalf), the marker persists and lets an
        // unrelated same-version publish pass the gate.
        assert!(
            gate.can_publish(&evicted, 1),
            "outstanding marker leaks: orphan passes same-version gate"
        );

        // The post-fix `did_open` avoids ever creating that marker for
        // an evicted URI: marking is deferred to a single end-of-flow
        // site that iterates the *final* work_items only. No regression
        // hook exists at the gate level for the post-fix path because
        // the deferred-marking flow simply does not invoke
        // `mark_force_republish_many` for evicted URIs — the contract is
        // structural, not algorithmic. The helper-level `cap_truncates_*`
        // and `higher_priority_*` tests in
        // `backend::tests::reenrichment_revalidation_cap` cover the
        // eviction logic that determines which URIs reach the
        // end-of-flow mark.
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
                chdir: false,
                is_sys_source: false,
                ..Default::default()
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
                chdir: false,
                is_sys_source: false,
                ..Default::default()
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

    // compute_affected_dependents_after_edit tests

    fn affected_url(s: &str) -> Url {
        Url::parse(&format!("file:///project/{}", s)).unwrap()
    }

    fn affected_workspace_root() -> Url {
        Url::parse("file:///project").unwrap()
    }

    fn make_meta_with_source(path: &str, line: u32) -> CrossFileMetadata {
        CrossFileMetadata {
            sources: vec![crate::cross_file::types::ForwardSource {
                path: path.to_string(),
                line,
                column: 0,
                is_directive: false,
                chdir: false,
                is_sys_source: false,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn test_compute_affected_skips_when_nothing_changed() {
        // No interface change, no edges change → no force-republishes.
        let mut graph = DependencyGraph::new();
        let parent = affected_url("parent.R");
        let child = affected_url("child.R");
        let meta_parent = make_meta_with_source("child.R", 1);
        graph.update_file(
            &parent,
            &meta_parent,
            Some(&affected_workspace_root()),
            |_| None,
        );

        let mut open: std::collections::HashSet<Url> = std::collections::HashSet::new();
        open.insert(parent.clone());
        open.insert(child.clone());

        let affected = compute_affected_dependents_after_edit(
            &parent,
            false,
            false,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        assert!(affected.is_empty());
    }

    #[test]
    fn locality_only_edge_change_revalidates_child() {
        use crate::cross_file::types::SourceLocality;

        let mut graph = DependencyGraph::new();
        let parent = affected_url("parent.R");
        let child = affected_url("child.R");
        let mut meta = make_meta_with_source("child.R", 1);
        meta.sources[0].locality = SourceLocality::CurrentFrame;
        graph.update_file(&parent, &meta, Some(&affected_workspace_root()), |_| None);

        meta.sources[0].locality = SourceLocality::NonInheriting;
        let update = graph.update_file(&parent, &meta, Some(&affected_workspace_root()), |_| None);
        assert!(update.edges_changed);

        let open = std::collections::HashSet::from([parent.clone(), child.clone()]);
        let affected = compute_affected_dependents_after_edit(
            &parent,
            false,
            update.edges_changed,
            &graph,
            |uri| open.contains(uri),
            10,
            200,
        );
        assert!(
            affected.contains(&child),
            "a CurrentFrame-to-NonInheriting edit must refresh the child's inherited scope"
        );
    }

    #[test]
    fn finalized_source_orderability_revalidates_dependent_without_edge_change() {
        use crate::cross_file::scope::compute_artifacts_with_metadata;

        let orderable = r#"bquote(expr = .(source("child.R")), where = parent.frame())"#;
        let inverted = r#"bquote(expr = .(source("child.R")), where = { x <- 1; parent.frame() })"#;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        let orderable_tree = parser.parse(orderable, None).unwrap();
        let inverted_tree = parser.parse(inverted, None).unwrap();
        let orderable_metadata =
            crate::cross_file::extract_metadata_with_tree(orderable, Some(&orderable_tree));
        let inverted_metadata =
            crate::cross_file::extract_metadata_with_tree(inverted, Some(&inverted_tree));
        assert_eq!(orderable_metadata.sources, inverted_metadata.sources);

        let parent = affected_url("parent.R");
        let child = affected_url("child.R");
        let mut graph = DependencyGraph::new();
        graph.update_file(
            &parent,
            &orderable_metadata,
            Some(&affected_workspace_root()),
            |_| None,
        );
        let update = graph.update_file(
            &parent,
            &inverted_metadata,
            Some(&affected_workspace_root()),
            |_| None,
        );
        assert!(
            !update.edges_changed,
            "the unchanged ForwardSource must preserve graph edge identity"
        );

        let old_hash = compute_artifacts_with_metadata(
            &parent,
            &orderable_tree,
            orderable,
            Some(&orderable_metadata),
        )
        .interface_hash;
        let new_hash = compute_artifacts_with_metadata(
            &parent,
            &inverted_tree,
            inverted,
            Some(&inverted_metadata),
        )
        .interface_hash;
        assert_ne!(old_hash, new_hash);

        let open = std::collections::HashSet::from([parent.clone(), child.clone()]);
        let affected = compute_affected_dependents_after_edit(
            &parent,
            old_hash != new_hash,
            update.edges_changed,
            &graph,
            |uri| open.contains(uri),
            10,
            200,
        );
        assert_eq!(affected, vec![child]);
    }

    #[test]
    fn test_compute_affected_includes_backward_dependents() {
        // child is edited; parent (which sources child) must be revalidated.
        let mut graph = DependencyGraph::new();
        let parent = affected_url("parent.R");
        let child = affected_url("child.R");
        let meta_parent = make_meta_with_source("child.R", 1);
        graph.update_file(
            &parent,
            &meta_parent,
            Some(&affected_workspace_root()),
            |_| None,
        );

        let mut open: std::collections::HashSet<Url> = std::collections::HashSet::new();
        open.insert(parent.clone());
        open.insert(child.clone());

        let affected = compute_affected_dependents_after_edit(
            &child,
            true,
            false,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        assert_eq!(affected.len(), 1);
        assert!(affected.contains(&parent));
    }

    #[test]
    fn excluded_non_lending_parent_is_revalidated_when_helper_changes() {
        // Issue #578: an excluded open buffer still consumes the helper it
        // sources, so helper edits must reschedule that open buffer. The
        // non-lending marker only prevents the helper from inheriting symbols
        // from the excluded parent.
        let mut graph = DependencyGraph::new();
        let excluded = affected_url("excluded.R");
        let helper = affected_url("helper.R");
        let meta_excluded = make_meta_with_source("helper.R", 1);
        graph.update_file(
            &excluded,
            &meta_excluded,
            Some(&affected_workspace_root()),
            |_| None,
        );
        graph.make_forward_edges_non_lending(&excluded);

        let mut open: std::collections::HashSet<Url> = std::collections::HashSet::new();
        open.insert(excluded.clone());
        open.insert(helper.clone());

        let affected = compute_affected_dependents_after_edit(
            &helper,
            true,
            false,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        assert_eq!(
            affected,
            vec![excluded],
            "editing the helper must reschedule the excluded open consumer"
        );
    }

    #[test]
    fn test_compute_affected_includes_forward_dependencies() {
        // parent is edited; child (sourced by parent) must be revalidated.
        // This is the bug: the previous implementation only walked backward
        // dependents, so a parent edit never triggered a child republish.
        // User-visible symptom: edits removing `y <- 1` from parent.R never
        // produced "y is not defined" in child.R until the user
        // manually edited child.R.
        let mut graph = DependencyGraph::new();
        let parent = affected_url("parent.R");
        let child = affected_url("child.R");
        let meta_parent = make_meta_with_source("child.R", 1);
        graph.update_file(
            &parent,
            &meta_parent,
            Some(&affected_workspace_root()),
            |_| None,
        );

        let mut open: std::collections::HashSet<Url> = std::collections::HashSet::new();
        open.insert(parent.clone());
        open.insert(child.clone());

        let affected = compute_affected_dependents_after_edit(
            &parent,
            true,
            false,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        assert_eq!(affected.len(), 1);
        assert!(
            affected.contains(&child),
            "child must be force-republished when its parent's interface changes; got {affected:?}"
        );
    }

    #[test]
    fn test_compute_affected_propagates_through_grandchildren() {
        // parent → child → grandchild. Editing parent must revalidate both
        // child and grandchild — matches the user's grandchild observation.
        let mut graph = DependencyGraph::new();
        let parent = affected_url("parent.R");
        let child = affected_url("child.R");
        let grandchild = affected_url("grandchild.R");

        graph.update_file(
            &parent,
            &make_meta_with_source("child.R", 1),
            Some(&affected_workspace_root()),
            |_| None,
        );
        graph.update_file(
            &child,
            &make_meta_with_source("grandchild.R", 1),
            Some(&affected_workspace_root()),
            |_| None,
        );

        let mut open: std::collections::HashSet<Url> = std::collections::HashSet::new();
        open.insert(parent.clone());
        open.insert(child.clone());
        open.insert(grandchild.clone());

        let affected = compute_affected_dependents_after_edit(
            &parent,
            true,
            false,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        assert!(affected.contains(&child));
        assert!(affected.contains(&grandchild));
        assert_eq!(affected.len(), 2);
    }

    #[test]
    fn test_compute_affected_filters_unopen_documents() {
        // Files not in `open_documents` must not be returned (we only
        // republish for files the editor has open).
        let mut graph = DependencyGraph::new();
        let parent = affected_url("parent.R");
        let child = affected_url("child.R");
        let grandchild = affected_url("grandchild.R");
        let _ = &grandchild; // referenced only for graph topology below

        graph.update_file(
            &parent,
            &make_meta_with_source("child.R", 1),
            Some(&affected_workspace_root()),
            |_| None,
        );
        graph.update_file(
            &child,
            &make_meta_with_source("grandchild.R", 1),
            Some(&affected_workspace_root()),
            |_| None,
        );

        // grandchild is NOT open
        let mut open: std::collections::HashSet<Url> = std::collections::HashSet::new();
        open.insert(parent.clone());
        open.insert(child.clone());

        let affected = compute_affected_dependents_after_edit(
            &parent,
            true,
            false,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        assert_eq!(affected, vec![child]);
    }

    #[test]
    fn test_compute_affected_excludes_edited_uri_from_result() {
        // The edited URI itself must not appear in the affected set —
        // callers handle the edited URI's republish separately.
        let mut graph = DependencyGraph::new();
        let parent = affected_url("parent.R");
        let child = affected_url("child.R");
        graph.update_file(
            &parent,
            &make_meta_with_source("child.R", 1),
            Some(&affected_workspace_root()),
            |_| None,
        );

        let mut open: std::collections::HashSet<Url> = std::collections::HashSet::new();
        open.insert(parent.clone());
        open.insert(child.clone());

        let affected = compute_affected_dependents_after_edit(
            &parent,
            true,
            false,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        assert!(!affected.contains(&parent));
    }

    #[test]
    fn test_compute_affected_includes_siblings_under_shared_parent() {
        // parent.R: source("child.R"); source("grandchild.R")
        // child.R: x <- 1
        // grandchild.R: x       (uses x from parent's pre-source(grandchild) scope)
        //
        // When child is edited, grandchild's INHERITED scope changes because
        // parent's scope at source("grandchild.R") includes child's exports
        // (from a prior source("child.R") call). The previous fix walks
        // Backward(child) = [parent] and Forward(child) = [], missing the
        // sibling. To catch this, the walk must also include forward
        // descendants of every backward ancestor.
        let mut graph = DependencyGraph::new();
        let parent = affected_url("parent.R");
        let child = affected_url("child.R");
        let grandchild = affected_url("grandchild.R");

        let parent_meta = CrossFileMetadata {
            sources: vec![
                crate::cross_file::types::ForwardSource {
                    path: "child.R".to_string(),
                    line: 0,
                    column: 0,
                    is_directive: false,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                crate::cross_file::types::ForwardSource {
                    path: "grandchild.R".to_string(),
                    line: 1,
                    column: 0,
                    is_directive: false,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        graph.update_file(
            &parent,
            &parent_meta,
            Some(&affected_workspace_root()),
            |_| None,
        );

        let mut open: std::collections::HashSet<Url> = std::collections::HashSet::new();
        open.insert(parent.clone());
        open.insert(child.clone());
        open.insert(grandchild.clone());

        let affected = compute_affected_dependents_after_edit(
            &child,
            true,
            false,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        assert!(affected.contains(&parent), "parent must be revalidated");
        assert!(
            affected.contains(&grandchild),
            "grandchild must be revalidated when its sibling child is edited; got {affected:?}"
        );
    }

    #[test]
    fn test_compute_affected_includes_transitive_siblings() {
        // grandparent.R sources parent.R, then sources auntie.R.
        // parent.R sources child.R.
        // Editing child should affect auntie (via shared grandparent).
        // child's backward ancestors: [parent, grandparent]
        // grandparent's forward descendants: [parent, child, auntie]
        // → auntie is captured.
        let mut graph = DependencyGraph::new();
        let grandparent = affected_url("grandparent.R");
        let parent = affected_url("parent.R");
        let child = affected_url("child.R");
        let auntie = affected_url("auntie.R");

        let grandparent_meta = CrossFileMetadata {
            sources: vec![
                crate::cross_file::types::ForwardSource {
                    path: "parent.R".to_string(),
                    line: 0,
                    column: 0,
                    is_directive: false,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                crate::cross_file::types::ForwardSource {
                    path: "auntie.R".to_string(),
                    line: 1,
                    column: 0,
                    is_directive: false,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        graph.update_file(
            &grandparent,
            &grandparent_meta,
            Some(&affected_workspace_root()),
            |_| None,
        );
        graph.update_file(
            &parent,
            &make_meta_with_source("child.R", 0),
            Some(&affected_workspace_root()),
            |_| None,
        );

        let mut open: std::collections::HashSet<Url> = std::collections::HashSet::new();
        open.insert(grandparent.clone());
        open.insert(parent.clone());
        open.insert(child.clone());
        open.insert(auntie.clone());

        let affected = compute_affected_dependents_after_edit(
            &child,
            true,
            false,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        assert!(affected.contains(&grandparent));
        assert!(affected.contains(&parent));
        assert!(
            affected.contains(&auntie),
            "auntie (transitive sibling via grandparent) must be revalidated; got {affected:?}"
        );
    }

    #[test]
    fn test_compute_affected_dedups_diamond() {
        // a → b, a → c, b → d, c → d. Editing a returns b, c, d each once.
        let mut graph = DependencyGraph::new();
        let a = affected_url("a.R");
        let b = affected_url("b.R");
        let c = affected_url("c.R");
        let d = affected_url("d.R");

        let meta_a = CrossFileMetadata {
            sources: vec![
                crate::cross_file::types::ForwardSource {
                    path: "b.R".to_string(),
                    line: 1,
                    column: 0,
                    is_directive: false,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                crate::cross_file::types::ForwardSource {
                    path: "c.R".to_string(),
                    line: 2,
                    column: 0,
                    is_directive: false,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        graph.update_file(&a, &meta_a, Some(&affected_workspace_root()), |_| None);
        graph.update_file(
            &b,
            &make_meta_with_source("d.R", 1),
            Some(&affected_workspace_root()),
            |_| None,
        );
        graph.update_file(
            &c,
            &make_meta_with_source("d.R", 1),
            Some(&affected_workspace_root()),
            |_| None,
        );

        let mut open: std::collections::HashSet<Url> = std::collections::HashSet::new();
        open.insert(a.clone());
        open.insert(b.clone());
        open.insert(c.clone());
        open.insert(d.clone());

        let affected = compute_affected_dependents_after_edit(
            &a,
            true,
            false,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        let mut sorted = affected.clone();
        sorted.sort_by_key(|u| u.path().to_string());
        assert_eq!(sorted, vec![b, c, d], "diamond must yield deduped URIs");
    }

    #[test]
    fn test_compute_affected_edges_only_revalidates_dependents() {
        // edges_changed=true with interface_changed=false must still walk
        // dependents — e.g. when a file's `source()` topology changes but
        // its declared symbols hash to the same value, the cycle/sibling
        // diagnostics in dependents can still flip.
        let mut graph = DependencyGraph::new();
        let parent = affected_url("parent.R");
        let child = affected_url("child.R");
        graph.update_file(
            &parent,
            &make_meta_with_source("child.R", 1),
            Some(&affected_workspace_root()),
            |_| None,
        );

        let mut open: std::collections::HashSet<Url> = std::collections::HashSet::new();
        open.insert(parent.clone());
        open.insert(child.clone());

        // child edited; only edges changed (e.g. it added a new source()
        // line), but its exported interface is unchanged.
        let affected_from_child = compute_affected_dependents_after_edit(
            &child,
            false,
            true,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        assert_eq!(affected_from_child.len(), 1);
        assert!(
            affected_from_child.contains(&parent),
            "edges_only edit on child must still revalidate its parent"
        );

        // parent edited; only edges changed.
        let affected_from_parent = compute_affected_dependents_after_edit(
            &parent,
            false,
            true,
            &graph,
            |u| open.contains(u),
            10,
            200,
        );
        assert_eq!(affected_from_parent.len(), 1);
        assert!(
            affected_from_parent.contains(&child),
            "edges_only edit on parent must still revalidate its forward subtree"
        );
    }
}
