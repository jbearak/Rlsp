//! Filesystem watcher for R library paths.
//!
//! Watches one or more `.libPaths()` directories with the `notify` crate,
//! debounces raw filesystem events, diffs the post-debounce directory listing
//! against the previous snapshot, and emits a single `LibpathEvent::Changed`
//! with the delta (added / removed / touched package names).
//!
//! # Recursive vs non-recursive watching
//!
//! Each libpath is attached with `RecursiveMode::Recursive`. Non-recursive
//! watching misses the common in-place upgrade case: `install.packages("pkg")`
//! for an already-installed package overwrites files inside
//! `<libpath>/<pkg>/` without changing the libpath's directory listing, so the
//! `added`/`removed` diff is empty and no directory-level events fire under
//! `NonRecursive`. Recursive watching surfaces those file-level events so
//! `touched_from_events` can mark the package's cached exports as stale.
//!
//! On Linux, `notify`'s recursive inotify implementation attaches one watch
//! per descendant **directory** (not per file). A typical R package has ~10–20
//! subdirectories (`R/`, `man/`, `help/`, `data/`, …), so 500 installed
//! packages is ~5–10k inotify watches. This is comfortably under Debian/Ubuntu's
//! modern default of `fs.inotify.max_user_watches = 524288`, but users on
//! older distros capped at 8192 who install CRAN snapshots may want to raise
//! the limit via `sysctl -w fs.inotify.max_user_watches=524288`.

use std::collections::{HashMap, HashSet};

/// Aggregated notification about changes under one or more libpath directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibpathEvent {
    /// Directory listings changed vs. last snapshot.
    Changed {
        /// Package names whose directories are newly present.
        added: HashSet<String>,
        /// Package names whose directories disappeared.
        removed: HashSet<String>,
        /// Existing package directories whose contents were touched
        /// (e.g. DESCRIPTION/NAMESPACE rewritten in place).
        touched: HashSet<String>,
    },
    /// Watcher attach failed or events were dropped; consumer should fall back
    /// to a full cache clear + re-init.
    Dropped,
    /// A bounded journal overflowed. The active watcher remains valid, but the
    /// consumer must clear and rebuild package cache/routing from current disk.
    Rescan,
}

impl LibpathEvent {
    /// Union of `added ∪ removed ∪ touched` for a `Changed` event; empty otherwise.
    ///
    /// Used by the integration test suite (`crates/raven/tests/libpath_watching.rs`)
    /// to assert post-event package deltas without re-implementing the union locally;
    /// the production consumer destructures the event directly so it does not call
    /// this from within the lib crate.
    pub fn affected_packages(&self) -> HashSet<String> {
        match self {
            LibpathEvent::Changed {
                added,
                removed,
                touched,
            } => {
                let mut out = added.clone();
                out.extend(removed.iter().cloned());
                out.extend(touched.iter().cloned());
                out
            }
            LibpathEvent::Dropped | LibpathEvent::Rescan => HashSet::new(),
        }
    }
}

const LIBPATH_JOURNAL_NAME_CAPACITY: usize = 1024;
const LIBPATH_RAW_PATH_CAPACITY: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibpathJournalPhase {
    Buffering,
    Active,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibpathPackageChange {
    Added,
    Removed,
    Touched,
}

#[derive(Default)]
struct LibpathJournalPending {
    changes: HashMap<String, LibpathPackageChange>,
    rescan_required: bool,
    dropped: bool,
}

impl LibpathJournalPending {
    fn is_empty(&self) -> bool {
        !self.dropped && !self.rescan_required && self.changes.is_empty()
    }

    fn record(&mut self, event: LibpathEvent) {
        match event {
            LibpathEvent::Dropped => {
                self.changes.clear();
                self.rescan_required = false;
                self.dropped = true;
            }
            LibpathEvent::Rescan => {
                if self.dropped {
                    return;
                }
                self.changes.clear();
                self.rescan_required = true;
            }
            LibpathEvent::Changed {
                added,
                removed,
                touched,
            } => {
                if self.dropped || self.rescan_required {
                    return;
                }
                for (names, change) in [
                    (added, LibpathPackageChange::Added),
                    (removed, LibpathPackageChange::Removed),
                    (touched, LibpathPackageChange::Touched),
                ] {
                    for name in names {
                        if !self.changes.contains_key(&name)
                            && self.changes.len() == LIBPATH_JOURNAL_NAME_CAPACITY
                        {
                            self.changes.clear();
                            self.rescan_required = true;
                            return;
                        }
                        self.changes.insert(name, change);
                    }
                }
            }
        }
    }

    fn take_highest_priority(&mut self) -> Option<LibpathEvent> {
        if self.dropped {
            self.dropped = false;
            return Some(LibpathEvent::Dropped);
        }
        if self.rescan_required {
            self.rescan_required = false;
            return Some(LibpathEvent::Rescan);
        }
        if self.changes.is_empty() {
            return None;
        }
        let mut added = HashSet::new();
        let mut removed = HashSet::new();
        let mut touched = HashSet::new();
        for (name, change) in std::mem::take(&mut self.changes) {
            match change {
                LibpathPackageChange::Added => {
                    added.insert(name);
                }
                LibpathPackageChange::Removed => {
                    removed.insert(name);
                }
                LibpathPackageChange::Touched => {
                    touched.insert(name);
                }
            }
        }
        Some(LibpathEvent::Changed {
            added,
            removed,
            touched,
        })
    }
}

struct LibpathJournalState {
    phase: LibpathJournalPhase,
    pending: LibpathJournalPending,
    claimed_generation: Option<u64>,
    next_claim_generation: u64,
}

/// Bounded state-owned handoff between a prospective watcher and its consumer.
///
/// Watch callbacks and debounce work record while `Buffering`; the central
/// package-library CAS is the sole transition to `Active`. Overflow never
/// relies on another bounded send: it clears the bounded per-package
/// last-write-wins map and sets a sticky `Rescan` bit that the consumer pulls
/// directly.
pub(crate) struct LibpathWatchJournal {
    state: parking_lot::Mutex<LibpathJournalState>,
    wake: tokio::sync::Notify,
    closed: tokio_util::sync::CancellationToken,
    #[cfg(test)]
    prearm_setup_pause: parking_lot::Mutex<Option<PrearmSetupTestPause>>,
}

impl LibpathWatchJournal {
    pub(crate) fn new_buffering() -> Arc<Self> {
        Arc::new(Self {
            state: parking_lot::Mutex::new(LibpathJournalState {
                phase: LibpathJournalPhase::Buffering,
                pending: LibpathJournalPending::default(),
                claimed_generation: None,
                next_claim_generation: 0,
            }),
            wake: tokio::sync::Notify::new(),
            closed: tokio_util::sync::CancellationToken::new(),
            #[cfg(test)]
            prearm_setup_pause: parking_lot::Mutex::new(None),
        })
    }

    pub(crate) fn record(&self, event: LibpathEvent) {
        let mut state = self.state.lock();
        if state.phase == LibpathJournalPhase::Closed {
            return;
        }
        state.pending.record(event);
        let active = state.phase == LibpathJournalPhase::Active
            && state.claimed_generation.is_none()
            && !state.pending.is_empty();
        drop(state);
        if active {
            self.wake.notify_one();
        }
    }

    pub(crate) fn require_rescan(&self) {
        self.record(LibpathEvent::Rescan);
    }

    pub(crate) fn is_buffering(&self) -> bool {
        self.state.lock().phase == LibpathJournalPhase::Buffering
    }

    /// Activate synchronously inside the winning `WorldState` CAS.
    ///
    /// A false result is a stale/invalid prepared watcher, never a benign
    /// no-op: publishing its owner would strand a consumer behind a closed or
    /// already-active journal.
    pub(crate) fn try_activate(&self) -> bool {
        let mut state = self.state.lock();
        if state.phase != LibpathJournalPhase::Buffering {
            return false;
        }
        state.phase = LibpathJournalPhase::Active;
        drop(state);
        self.wake.notify_waiters();
        true
    }

    pub(crate) fn close(&self) {
        let mut state = self.state.lock();
        state.phase = LibpathJournalPhase::Closed;
        state.pending = LibpathJournalPending::default();
        drop(state);
        self.closed.cancel();
        self.wake.notify_waiters();
    }

    /// Pace a durable redelivery while allowing retirement/shutdown to
    /// interrupt immediately.
    pub(crate) async fn wait_retry(
        &self,
        delay: Duration,
        shutdown: &tokio_util::sync::CancellationToken,
    ) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(delay) => true,
            _ = self.closed.cancelled() => false,
            _ = shutdown.cancelled() => false,
        }
    }

    /// Claim the highest-priority durable event.
    ///
    /// Exactly one delivery may be in flight. Dropping the returned claim
    /// without calling [`LibpathJournalDelivery::ack`] re-merges its older
    /// payload ahead of any newer pending input, so cancellation and rejected
    /// routing CASes cannot lose invalidation work.
    pub(crate) async fn claim(self: &Arc<Self>) -> Option<LibpathJournalDelivery> {
        let shutdown = tokio_util::sync::CancellationToken::new();
        self.claim_until_shutdown(&shutdown).await
    }

    /// Claim a durable event, or stop promptly when the routing task owner
    /// closes its shutdown gate.
    pub(crate) async fn claim_until_shutdown(
        self: &Arc<Self>,
        shutdown: &tokio_util::sync::CancellationToken,
    ) -> Option<LibpathJournalDelivery> {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock();
                if shutdown.is_cancelled() {
                    return None;
                }
                match state.phase {
                    LibpathJournalPhase::Closed => return None,
                    LibpathJournalPhase::Buffering => {}
                    LibpathJournalPhase::Active => {
                        if state.claimed_generation.is_none()
                            && let Some(event) = state.pending.take_highest_priority()
                        {
                            let generation = state.next_claim_generation;
                            state.next_claim_generation = state
                                .next_claim_generation
                                .checked_add(1)
                                .expect("libpath journal claim generation exhausted");
                            state.claimed_generation = Some(generation);
                            return Some(LibpathJournalDelivery {
                                journal: Arc::clone(self),
                                generation,
                                event: Some(event),
                            });
                        }
                    }
                }
            }
            tokio::select! {
                _ = notified => {}
                _ = shutdown.cancelled() => return None,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn is_closed_for_test(&self) -> bool {
        self.state.lock().phase == LibpathJournalPhase::Closed
    }

    #[cfg(test)]
    fn arm_prearm_setup_pause_for_test(&self, pause: PrearmSetupTestPause) {
        *self.prearm_setup_pause.lock() = Some(pause);
    }

    #[cfg(test)]
    fn pause_prearm_setup_for_test(&self) -> Option<PrearmSetupTestCompletion> {
        let pause = self.prearm_setup_pause.lock().take();
        if let Some(pause) = pause {
            let _ = pause.arrived.send(pause.owner);
            let _ = pause.release.recv();
            return Some(PrearmSetupTestCompletion {
                owner: pause.owner,
                completed: Some(pause.completed),
            });
        }
        None
    }
}

/// One exact in-flight journal delivery.
///
/// The event is acknowledged only by the central routing winner. Every other
/// drop path restores it with lower temporal precedence than input recorded
/// after the claim.
pub(crate) struct LibpathJournalDelivery {
    journal: Arc<LibpathWatchJournal>,
    generation: u64,
    event: Option<LibpathEvent>,
}

impl LibpathJournalDelivery {
    pub(crate) fn event(&self) -> &LibpathEvent {
        self.event
            .as_ref()
            .expect("an acknowledged libpath delivery has no event")
    }

    pub(crate) fn journal(&self) -> &Arc<LibpathWatchJournal> {
        &self.journal
    }

    pub(crate) fn ack(&mut self) {
        let mut state = self.journal.state.lock();
        assert_eq!(
            state.claimed_generation,
            Some(self.generation),
            "libpath journal ack must consume the exact in-flight generation"
        );
        state.claimed_generation = None;
        self.event = None;
        let wake = state.phase == LibpathJournalPhase::Active && !state.pending.is_empty();
        drop(state);
        if wake {
            self.journal.wake.notify_one();
        }
    }
}

impl Drop for LibpathJournalDelivery {
    fn drop(&mut self) {
        let Some(event) = self.event.take() else {
            return;
        };
        let mut state = self.journal.state.lock();
        if state.claimed_generation != Some(self.generation) {
            return;
        }
        state.claimed_generation = None;
        if state.phase != LibpathJournalPhase::Closed {
            let newer = std::mem::take(&mut state.pending);
            state.pending.record(event);
            if newer.dropped {
                state.pending.record(LibpathEvent::Dropped);
            } else if newer.rescan_required {
                state.pending.record(LibpathEvent::Rescan);
            } else if !newer.changes.is_empty() {
                let mut added = HashSet::new();
                let mut removed = HashSet::new();
                let mut touched = HashSet::new();
                for (name, change) in newer.changes {
                    match change {
                        LibpathPackageChange::Added => {
                            added.insert(name);
                        }
                        LibpathPackageChange::Removed => {
                            removed.insert(name);
                        }
                        LibpathPackageChange::Touched => {
                            touched.insert(name);
                        }
                    }
                }
                state.pending.record(LibpathEvent::Changed {
                    added,
                    removed,
                    touched,
                });
            }
        }
        let wake = state.phase == LibpathJournalPhase::Active && !state.pending.is_empty();
        drop(state);
        if wake {
            self.journal.wake.notify_one();
        }
    }
}

use std::path::{Path, PathBuf};

/// A snapshot of which package subdirectories exist under each libpath.
///
/// `entries` preserves the original libpath order (earlier = higher priority,
/// matching R's `.libPaths()`). Order matters because a package installed into
/// multiple libpaths is resolved from the first one that contains it; if the
/// "winning root" changes between snapshots, consumers must invalidate that
/// package's cached exports even though the name is still present.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LibpathSnapshot {
    entries: Vec<(PathBuf, HashSet<String>)>,
}

impl LibpathSnapshot {
    pub fn capture(paths: &[PathBuf]) -> Self {
        let entries = paths
            .iter()
            .map(|root| (root.clone(), read_package_dir(root)))
            .collect();
        Self { entries }
    }

    /// For each package name present in any watched root, return the first root
    /// (in libpath priority order) that contains it.
    fn winning_roots(&self) -> HashMap<String, PathBuf> {
        let mut winner: HashMap<String, PathBuf> = HashMap::new();
        for (root, names) in &self.entries {
            for name in names {
                winner.entry(name.clone()).or_insert_with(|| root.clone());
            }
        }
        winner
    }

    /// Diff two snapshots by their effective `package -> winning-root` mapping.
    ///
    /// Returns three sets:
    /// - `added`: names that were not present in `self` but are in `other`.
    /// - `removed`: names that were in `self` but are not in `other`.
    /// - `moved`: names present in both but whose winning root changed, so the
    ///   effective on-disk package differs even though the name persists.
    ///
    /// Consumers should treat all three as invalidation triggers.
    #[cfg(test)]
    pub(crate) fn diff(&self, other: &Self) -> (HashSet<String>, HashSet<String>, HashSet<String>) {
        let prev = self.winning_roots();
        let next = other.winning_roots();
        let mut added = HashSet::new();
        let mut removed = HashSet::new();
        let mut moved = HashSet::new();
        for name in prev.keys().chain(next.keys()) {
            match (prev.get(name), next.get(name)) {
                (None, Some(_)) => {
                    added.insert(name.clone());
                }
                (Some(_), None) => {
                    removed.insert(name.clone());
                }
                (Some(p), Some(n)) if p != n => {
                    moved.insert(name.clone());
                }
                _ => {}
            }
        }
        (added, removed, moved)
    }

    fn diff_bounded(
        &self,
        other: &Self,
    ) -> Option<(HashSet<String>, HashSet<String>, HashSet<String>)> {
        let prev = self.winning_roots();
        let next = other.winning_roots();
        let mut added = HashSet::new();
        let mut removed = HashSet::new();
        let mut moved = HashSet::new();
        let mut changed = HashSet::new();
        for name in prev.keys().chain(next.keys()) {
            let target = match (prev.get(name), next.get(name)) {
                (None, Some(_)) => Some(&mut added),
                (Some(_), None) => Some(&mut removed),
                (Some(previous), Some(current)) if previous != current => Some(&mut moved),
                _ => None,
            };
            if let Some(target) = target
                && changed.insert(name.clone())
            {
                if changed.len() > LIBPATH_JOURNAL_NAME_CAPACITY {
                    return None;
                }
                target.insert(name.clone());
            }
        }
        Some((added, removed, moved))
    }

    /// True if any watched root currently contains a package with this name.
    fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|(_, names)| names.contains(name))
    }
}

/// Given raw `notify::Event` paths observed during a debounce window, derive
/// the set of package names that were "touched" — present in both snapshots
/// (so neither added nor removed) but whose contents were rewritten. This
/// covers the common in-place upgrade/reinstall case that produces no
/// directory-listing delta.
fn touched_from_events(
    event_paths: &[PathBuf],
    watched_roots: &[PathBuf],
    prev: &LibpathSnapshot,
    next: &LibpathSnapshot,
) -> HashSet<String> {
    let mut touched = HashSet::new();
    for path in event_paths {
        for root in watched_roots {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            // First component after the root is the package directory name.
            let Some(std::path::Component::Normal(os)) = rel.components().next() else {
                break;
            };
            let Some(name) = os.to_str() else { break };
            if name.starts_with("00LOCK-") {
                break;
            }
            if prev.contains(name) && next.contains(name) {
                touched.insert(name.to_string());
            }
            break;
        }
    }
    touched
}

fn read_package_dir(root: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in read_dir.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // Skip in-progress install staging directories (leading "00LOCK-").
        if name.starts_with("00LOCK-") {
            continue;
        }
        let path = entry.path();
        if path.join("DESCRIPTION").exists() || path.join("NAMESPACE").exists() {
            out.insert(name);
        }
    }
    out
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use tempfile::tempdir;

    fn make_pkg(root: &Path, name: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("DESCRIPTION"), "Package: x\n").unwrap();
    }

    #[test]
    fn capture_lists_packages_with_description() {
        let t = tempdir().unwrap();
        make_pkg(t.path(), "foo");
        make_pkg(t.path(), "bar");
        // Non-package directory (no DESCRIPTION/NAMESPACE) is ignored.
        std::fs::create_dir_all(t.path().join("not-a-pkg")).unwrap();

        let snap = LibpathSnapshot::capture(&[t.path().to_path_buf()]);
        let names: HashSet<String> = snap
            .entries
            .iter()
            .flat_map(|(_, n)| n.iter().cloned())
            .collect();
        assert_eq!(
            names,
            ["foo".to_string(), "bar".to_string()].into_iter().collect()
        );
    }

    #[test]
    fn capture_skips_00lock_staging_dirs() {
        let t = tempdir().unwrap();
        let lock = t.path().join("00LOCK-foo");
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(lock.join("DESCRIPTION"), "").unwrap();

        let snap = LibpathSnapshot::capture(&[t.path().to_path_buf()]);
        assert!(snap.entries.iter().all(|(_, n)| n.is_empty()));
    }

    #[test]
    fn diff_reports_added_and_removed() {
        let t = tempdir().unwrap();
        make_pkg(t.path(), "foo");
        let prev = LibpathSnapshot::capture(&[t.path().to_path_buf()]);

        make_pkg(t.path(), "bar");
        std::fs::remove_dir_all(t.path().join("foo")).unwrap();
        let next = LibpathSnapshot::capture(&[t.path().to_path_buf()]);

        let (added, removed, moved) = prev.diff(&next);
        assert_eq!(added, ["bar".to_string()].into_iter().collect());
        assert_eq!(removed, ["foo".to_string()].into_iter().collect());
        assert!(moved.is_empty());
    }

    #[test]
    fn diff_reports_moved_when_winning_root_changes() {
        // Two libpaths in priority order: high, low. Package `foo` initially
        // lives only in `low`; it is then installed into `high`, shadowing the
        // previous resolution even though the name persists in the union.
        let t_high = tempdir().unwrap();
        let t_low = tempdir().unwrap();
        make_pkg(t_low.path(), "foo");
        let prev =
            LibpathSnapshot::capture(&[t_high.path().to_path_buf(), t_low.path().to_path_buf()]);
        make_pkg(t_high.path(), "foo");
        let next =
            LibpathSnapshot::capture(&[t_high.path().to_path_buf(), t_low.path().to_path_buf()]);

        let (added, removed, moved) = prev.diff(&next);
        assert!(added.is_empty(), "name is in union both times");
        assert!(removed.is_empty());
        assert_eq!(moved, ["foo".to_string()].into_iter().collect());
    }

    #[test]
    fn capture_handles_missing_directory() {
        let snap = LibpathSnapshot::capture(&[PathBuf::from("/does/not/exist/raven")]);
        assert!(snap.entries.iter().all(|(_, n)| n.is_empty()));
    }

    #[test]
    fn touched_from_events_flags_in_place_upgrade() {
        // An in-place `install.packages("foo")` rewrites files *inside*
        // `<libpath>/foo/` without touching the libpath's listing. The diff
        // alone reports added={}, removed={}, moved={} — the only signal is
        // file-level events under `<libpath>/foo/`, which recursive watching
        // surfaces. `touched_from_events` must turn those into `{"foo"}`.
        let t = tempdir().unwrap();
        make_pkg(t.path(), "foo");
        let prev = LibpathSnapshot::capture(&[t.path().to_path_buf()]);
        let next = prev.clone();

        let event_paths = vec![
            t.path().join("foo").join("DESCRIPTION"),
            t.path().join("foo").join("NAMESPACE"),
            // A deep-nested path still resolves to the package name.
            t.path().join("foo").join("help").join("aliases.rds"),
        ];

        let touched = touched_from_events(&event_paths, &[t.path().to_path_buf()], &prev, &next);
        assert_eq!(touched, ["foo".to_string()].into_iter().collect());
    }

    #[test]
    fn touched_from_events_skips_00lock_staging() {
        // Recursive watching fires events under `<libpath>/00LOCK-foo/` during
        // install staging. Those must not be mis-attributed to the eventual
        // real `foo` package.
        let t = tempdir().unwrap();
        make_pkg(t.path(), "foo");
        let snap = LibpathSnapshot::capture(&[t.path().to_path_buf()]);

        let event_paths = vec![
            t.path().join("00LOCK-foo").join("DESCRIPTION"),
            t.path()
                .join("00LOCK-foo")
                .join("foo")
                .join("R")
                .join("foo.R"),
        ];

        let touched = touched_from_events(&event_paths, &[t.path().to_path_buf()], &snap, &snap);
        assert!(touched.is_empty(), "expected no touched, got {:?}", touched);
    }
}

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Handle to a running libpath watcher. Drop the handle to stop watching.
pub struct LibpathWatcherHandle {
    /// Kept alive so the watcher thread keeps running; the task aborts when this drops.
    _watcher: notify::RecommendedWatcher,
    /// Abort handle for the debounce/diff task.
    task: tokio::task::JoinHandle<()>,
    /// Prospective journals are closed explicitly so a blocked consumer exits
    /// even while callback-side Arcs still exist.
    journal: Option<Arc<LibpathWatchJournal>>,
    /// Compatibility bridge for the public mpsc API.
    bridge: Option<tokio::task::JoinHandle<()>>,
    #[cfg(test)]
    drop_probe: Option<Box<dyn Fn() + Send + Sync>>,
}

#[cfg(test)]
impl LibpathWatcherHandle {
    pub(crate) fn set_drop_probe(&mut self, probe: impl Fn() + Send + Sync + 'static) {
        self.drop_probe = Some(Box::new(probe));
    }
}

impl Drop for LibpathWatcherHandle {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(probe) = self.drop_probe.as_ref() {
            probe();
        }
        if let Some(journal) = self.journal.as_ref() {
            journal.close();
        }
        self.task.abort();
        if let Some(bridge) = self.bridge.as_ref() {
            bridge.abort();
        }
    }
}

/// Start watching `paths`. Events are debounced by `debounce` and delivered on `tx`.
///
/// Setup is all-or-nothing. If any requested path cannot be watched, emits a
/// single `LibpathEvent::Dropped`, tears down every attachment, and returns
/// `None`; reporting full coverage while silently omitting a library path
/// would make cache invalidation unsound.
pub fn spawn_watcher(
    paths: Vec<PathBuf>,
    debounce: Duration,
    tx: mpsc::Sender<LibpathEvent>,
) -> Option<LibpathWatcherHandle> {
    let journal = LibpathWatchJournal::new_buffering();
    let mut handle = match spawn_watcher_into_journal(paths, debounce, Arc::clone(&journal)) {
        Some(handle) => handle,
        None => {
            let _ = tx.try_send(LibpathEvent::Dropped);
            return None;
        }
    };
    assert!(
        journal.try_activate(),
        "a newly created compatibility journal must be buffering"
    );
    let bridge_journal = Arc::clone(&journal);
    handle.bridge = Some(tokio::spawn(async move {
        while let Some(mut delivery) = bridge_journal.claim().await {
            if tx.send(delivery.event().clone()).await.is_err() {
                bridge_journal.close();
                return;
            }
            delivery.ack();
        }
    }));
    Some(handle)
}

fn spawn_watcher_into_journal(
    paths: Vec<PathBuf>,
    debounce: Duration,
    journal: Arc<LibpathWatchJournal>,
) -> Option<LibpathWatcherHandle> {
    use notify::{RecursiveMode, Watcher};

    if paths.is_empty() {
        log::info!("LibpathWatcher: no paths to watch, skipping");
        return None;
    }

    // Internal channel: notify -> debounce task. Use a std::sync::mpsc because
    // notify v6 only accepts a synchronous EventHandler closure.
    // Bounded pre-activation journal: a watcher can receive a burst while its
    // baseline snapshot is still scanning a large library. Raw-channel
    // overflow deposits a sticky full-rescan obligation instead of growing
    // memory without bound or silently losing invalidation work.
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<notify::Result<notify::Event>>(1024);
    let raw_tx_cloned = raw_tx.clone();
    let overflow_journal = Arc::clone(&journal);

    let mut watcher =
        match notify::recommended_watcher(move |res| match raw_tx_cloned.try_send(res) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                overflow_journal.require_rescan();
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                log::warn!("LibpathWatcher: failed to construct watcher: {e}");
                return None;
            }
        };

    let mut attached: Vec<PathBuf> = Vec::new();
    for p in &paths {
        // Recursive: needed so in-place package upgrades (which rewrite files
        // inside an existing `<libpath>/<pkg>/` without touching the libpath's
        // listing) fire events we can turn into `touched`. See the module-level
        // docstring for the Linux inotify cost tradeoff.
        match watcher.watch(p, RecursiveMode::Recursive) {
            Ok(()) => {
                // Canonicalize so that `attached` stores the same symlink-resolved
                // form that the OS (FSEvents on macOS, inotify on Linux) uses when
                // reporting event paths. `touched_from_events` strips these roots as
                // a prefix from incoming event paths; if the stored root and the event
                // path use different representations (e.g. `/var/...` vs
                // `/private/var/...` on macOS where `/var -> /private/var`),
                // strip_prefix always fails and in-place package upgrades are silently
                // missed. Removing canonicalize() here breaks that matching invariant.
                attached.push(p.canonicalize().unwrap_or_else(|_| p.clone()));
            }
            Err(e) => {
                // A libpath directory may not exist yet (e.g. empty renv); log and continue.
                log::warn!("LibpathWatcher: cannot watch {}: {e}", p.display());
            }
        }
    }

    if attached.is_empty() {
        log::warn!("LibpathWatcher: no libpath directories could be attached");
        return None;
    }
    if attached.len() != paths.len() {
        log::warn!(
            "LibpathWatcher: attached only {}/{} requested library paths; refusing partial coverage",
            attached.len(),
            paths.len()
        );
        return None;
    }

    // The watcher is pre-armed before snapshot capture. Events that race the
    // detached filesystem scan queue in `raw_rx`, so the debounce task journals
    // them against this baseline instead of leaving a snapshot/attach gap.
    let initial_snap = LibpathSnapshot::capture(&attached);

    let raw_rx = Arc::new(StdMutex::new(raw_rx));
    let task_journal = Arc::clone(&journal);
    let task = tokio::spawn(async move {
        let snapshot = Arc::new(tokio::sync::Mutex::new(initial_snap));
        debounce_loop(raw_rx, snapshot, Arc::new(attached), debounce, task_journal).await;
    });

    Some(LibpathWatcherHandle {
        _watcher: watcher,
        task,
        // Install the close owner before returning from synchronous setup.
        // If the surrounding async `prearm_watcher` future is cancelled after
        // its blocking worker finishes, dropping this handle still closes the
        // consumer's buffering journal.
        journal: Some(journal),
        bridge: None,
        #[cfg(test)]
        drop_probe: None,
    })
}

/// Prepare a watcher into a caller-owned buffering journal while keeping
/// synchronous attach/baseline work off the async executor.
pub(crate) async fn prearm_watcher(
    paths: Vec<PathBuf>,
    debounce: Duration,
    journal: Arc<LibpathWatchJournal>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Option<LibpathWatcherHandle> {
    // This async-side guard covers cancellation while `spawn_blocking` is
    // still running. The blocking worker has its own guard for panic/failure.
    let mut close_guard = LibpathJournalSetupCloseGuard::new(Arc::clone(&journal));
    let setup_journal = Arc::clone(&journal);
    let mut worker = tokio::task::spawn_blocking(move || {
        let mut close_guard = LibpathJournalSetupCloseGuard::new(Arc::clone(&setup_journal));
        #[cfg(test)]
        let _setup_completion = setup_journal.pause_prearm_setup_for_test();
        if !setup_journal.is_buffering() {
            return None;
        }
        let handle = spawn_watcher_into_journal(paths, debounce, setup_journal);
        if handle.as_ref().is_some_and(|handle| {
            handle
                .journal
                .as_ref()
                .is_some_and(|journal| journal.is_buffering())
        }) {
            close_guard.disarm();
        }
        handle
    });
    let result = tokio::select! {
        result = &mut worker => result,
        _ = shutdown.cancelled() => return None,
    };
    match result {
        Ok(Some(handle)) if journal.is_buffering() => {
            close_guard.disarm();
            Some(handle)
        }
        Ok(Some(handle)) => {
            drop(handle);
            None
        }
        Ok(None) => None,
        Err(error) => {
            log::warn!("LibpathWatcher: prearmed setup worker failed: {error}");
            None
        }
    }
}

#[cfg(test)]
struct PrearmSetupTestPause {
    owner: &'static str,
    arrived: tokio::sync::oneshot::Sender<&'static str>,
    release: std::sync::mpsc::Receiver<()>,
    completed: tokio::sync::oneshot::Sender<&'static str>,
}

#[cfg(test)]
struct PrearmSetupTestCompletion {
    owner: &'static str,
    completed: Option<tokio::sync::oneshot::Sender<&'static str>>,
}

#[cfg(test)]
impl Drop for PrearmSetupTestCompletion {
    fn drop(&mut self) {
        if let Some(completed) = self.completed.take() {
            let _ = completed.send(self.owner);
        }
    }
}

struct LibpathJournalSetupCloseGuard {
    journal: Arc<LibpathWatchJournal>,
    armed: bool,
}

impl LibpathJournalSetupCloseGuard {
    fn new(journal: Arc<LibpathWatchJournal>) -> Self {
        Self {
            journal,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LibpathJournalSetupCloseGuard {
    fn drop(&mut self) {
        if self.armed {
            self.journal.close();
        }
    }
}

async fn debounce_loop(
    raw_rx: Arc<StdMutex<std::sync::mpsc::Receiver<notify::Result<notify::Event>>>>,
    snapshot: Arc<tokio::sync::Mutex<LibpathSnapshot>>,
    paths: Arc<Vec<PathBuf>>,
    debounce: Duration,
    journal: Arc<LibpathWatchJournal>,
) {
    loop {
        // Block on the next raw event. We move raw_rx across an await using
        // spawn_blocking because std::sync::mpsc::Receiver::recv blocks.
        let rx_arc = Arc::clone(&raw_rx);
        let first = tokio::task::spawn_blocking(move || {
            // Unwrap: StdMutex never poisons in normal operation.
            let guard = rx_arc.lock().unwrap();
            guard.recv()
        })
        .await;

        match first {
            Ok(Ok(notify_result)) => {
                // Capture paths from the initial event and everything drained
                // during the debounce window. We need these to reconstruct the
                // `touched` set (in-place upgrades that produce no listing delta).
                // An `Err` notify result at the head of the stream means notify
                // surfaced an error for this callback — log and proceed with an
                // empty starting path list so we still run the diff.
                let mut raw_paths_overflowed = false;
                let mut event_paths: Vec<PathBuf> = match notify_result {
                    Ok(mut evt) => {
                        if evt.paths.len() > LIBPATH_RAW_PATH_CAPACITY {
                            evt.paths.truncate(LIBPATH_RAW_PATH_CAPACITY);
                            raw_paths_overflowed = true;
                        }
                        evt.paths
                    }
                    Err(e) => {
                        log::warn!("LibpathWatcher: notify error event: {e}");
                        journal.require_rescan();
                        Vec::new()
                    }
                };
                tokio::time::sleep(debounce).await;
                let rx_arc = Arc::clone(&raw_rx);
                let (drained_paths, drained_error, drained_overflow): (Vec<PathBuf>, bool, bool) =
                    match tokio::task::spawn_blocking(move || {
                        let mut paths = Vec::new();
                        let mut saw_error = false;
                        let mut overflowed = false;
                        let guard = rx_arc.lock().unwrap();
                        while let Ok(res) = guard.try_recv() {
                            match res {
                                Ok(evt) => {
                                    let remaining =
                                        LIBPATH_RAW_PATH_CAPACITY.saturating_sub(paths.len());
                                    if evt.paths.len() > remaining {
                                        overflowed = true;
                                    }
                                    paths.extend(evt.paths.into_iter().take(remaining));
                                }
                                Err(e) => {
                                    log::warn!("LibpathWatcher: notify error during drain: {e}");
                                    saw_error = true;
                                }
                            }
                        }
                        (paths, saw_error, overflowed)
                    })
                    .await
                    {
                        Ok(result) => result,
                        Err(e) => {
                            log::warn!("LibpathWatcher: drain task failed: {e}");
                            (Vec::new(), true, false)
                        }
                    };
                if drained_error {
                    journal.require_rescan();
                }
                let remaining = LIBPATH_RAW_PATH_CAPACITY.saturating_sub(event_paths.len());
                if drained_paths.len() > remaining {
                    raw_paths_overflowed = true;
                }
                event_paths.extend(drained_paths.into_iter().take(remaining));
                if raw_paths_overflowed || drained_overflow {
                    journal.require_rescan();
                }

                // Diff and derive touched under a single snapshot-lock acquisition.
                let paths_for_capture = paths.clone();
                let next_snap = match tokio::task::spawn_blocking(move || {
                    LibpathSnapshot::capture(&paths_for_capture)
                })
                .await
                {
                    Ok(snap) => snap,
                    Err(e) => {
                        log::warn!("LibpathWatcher: capture task failed: {e}");
                        journal.record(LibpathEvent::Dropped);
                        return;
                    }
                };
                let (added, removed, touched) = {
                    let mut snap_guard = snapshot.lock().await;
                    let Some((added, removed, moved)) = snap_guard.diff_bounded(&next_snap) else {
                        *snap_guard = next_snap;
                        journal.require_rescan();
                        continue;
                    };
                    let mut touched =
                        touched_from_events(&event_paths, &paths, &snap_guard, &next_snap);
                    // Packages whose winning libpath changed are also "touched"
                    // from the consumer's perspective — the effective on-disk
                    // version differs even though the name persists.
                    touched.extend(moved);
                    *snap_guard = next_snap;
                    (added, removed, touched)
                };

                if !added.is_empty() || !removed.is_empty() || !touched.is_empty() {
                    if added
                        .iter()
                        .chain(removed.iter())
                        .chain(touched.iter())
                        .collect::<HashSet<_>>()
                        .len()
                        > LIBPATH_JOURNAL_NAME_CAPACITY
                    {
                        journal.require_rescan();
                        continue;
                    }
                    journal.record(LibpathEvent::Changed {
                        added,
                        removed,
                        touched,
                    });
                }
            }
            Ok(Err(_disconnect)) => {
                log::warn!("LibpathWatcher: raw channel disconnected, exiting");
                // Notify consumer so the fallback (full cache clear) path runs;
                // otherwise package invalidation silently stops for this session.
                journal.record(LibpathEvent::Dropped);
                return;
            }
            Err(join_err) => {
                log::warn!("LibpathWatcher: blocking task failed: {join_err}");
                journal.record(LibpathEvent::Dropped);
                return;
            }
        }
    }
}

#[cfg(test)]
mod watcher_tests {
    use super::*;
    use tempfile::tempdir;

    fn make_pkg(root: &Path, name: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("DESCRIPTION"), "Package: x\n").unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires reliable macOS FSEvents delivery; run with `cargo test -- --ignored`"]
    async fn watcher_emits_added_on_new_package() {
        let t = tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel::<LibpathEvent>(16);

        let _handle = spawn_watcher(vec![t.path().to_path_buf()], Duration::from_millis(300), tx)
            .expect("watcher attached");

        // Give the watcher a moment to register.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Simulate install.
        make_pkg(t.path(), "foo");

        let evt = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("event arrived in time")
            .expect("channel not closed");

        match evt {
            LibpathEvent::Changed { added, removed, .. } => {
                assert_eq!(added, ["foo".to_string()].into_iter().collect());
                assert!(removed.is_empty());
            }
            LibpathEvent::Dropped | LibpathEvent::Rescan => {
                panic!("expected Changed, got terminal/rescan")
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires reliable FS notifications; run with `cargo test -- --ignored`"]
    async fn watcher_emits_touched_on_in_place_upgrade() {
        // Regression for the NonRecursive → Recursive switch: rewriting files
        // inside an existing package directory must report it as `touched`.
        //
        // Start the watcher on an empty libpath, then install the package and
        // drain the resulting `added` event. Once that event has arrived we
        // know FSEvents is fully settled and delivering recursive events for
        // `foo/` — only then do we trigger the in-place overwrite. This
        // avoids the flakiness that came from using a fixed sleep as the
        // only readiness barrier.
        let t = tempdir().unwrap();

        let (tx, mut rx) = mpsc::channel::<LibpathEvent>(16);
        let _handle = spawn_watcher(vec![t.path().to_path_buf()], Duration::from_millis(300), tx)
            .expect("watcher attached");

        // Create the package and wait for the watcher to confirm it via an
        // `added` event. This is the readiness signal: once the debounce loop
        // has delivered a Changed event, recursive watching for `foo/` is live.
        make_pkg(t.path(), "foo");
        let added_evt = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("added event arrived in time")
            .expect("channel not closed");
        match added_evt {
            LibpathEvent::Changed { added, .. } => {
                assert!(added.contains("foo"), "expected foo in added: {:?}", added);
            }
            LibpathEvent::Dropped | LibpathEvent::Rescan => {
                panic!("expected Changed for add, got terminal/rescan")
            }
        }

        // Rewrite files inside the existing package directory — no listing
        // delta, so only recursive watching surfaces this as a signal.
        std::fs::write(
            t.path().join("foo").join("DESCRIPTION"),
            "Package: foo\nVersion: 2.0\n",
        )
        .unwrap();
        std::fs::write(t.path().join("foo").join("NAMESPACE"), "export(new_fn)\n").unwrap();

        let evt = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("touched event arrived in time")
            .expect("channel not closed");

        match evt {
            LibpathEvent::Changed {
                added,
                removed,
                touched,
            } => {
                assert!(added.is_empty(), "no dir was added: {:?}", added);
                assert!(removed.is_empty(), "no dir was removed: {:?}", removed);
                assert!(
                    touched.contains("foo"),
                    "expected 'foo' in touched, got {:?}",
                    touched
                );
            }
            LibpathEvent::Dropped | LibpathEvent::Rescan => {
                panic!("expected Changed, got terminal/rescan")
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires reliable macOS FSEvents delivery; run with `cargo test -- --ignored`"]
    async fn watcher_emits_removed_on_package_deletion() {
        let t = tempdir().unwrap();
        make_pkg(t.path(), "foo");

        let (tx, mut rx) = mpsc::channel::<LibpathEvent>(16);
        let _handle = spawn_watcher(vec![t.path().to_path_buf()], Duration::from_millis(300), tx)
            .expect("watcher attached");

        tokio::time::sleep(Duration::from_millis(200)).await;

        std::fs::remove_dir_all(t.path().join("foo")).unwrap();

        let evt = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("event arrived in time")
            .expect("channel not closed");

        match evt {
            LibpathEvent::Changed { added, removed, .. } => {
                assert_eq!(removed, ["foo".to_string()].into_iter().collect());
                assert!(added.is_empty());
            }
            LibpathEvent::Dropped | LibpathEvent::Rescan => {
                panic!("expected Changed, got terminal/rescan")
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn watcher_returns_none_when_no_paths_attach() {
        let (tx, mut rx) = mpsc::channel::<LibpathEvent>(16);
        // Non-existent path should fail to attach on all platforms.
        let handle = spawn_watcher(
            vec![PathBuf::from("/raven/nonexistent/xyz-abc")],
            Duration::from_millis(50),
            tx,
        );
        assert!(handle.is_none());
        // Contract: when no paths attach, spawn_watcher must emit Dropped on the
        // provided sender so the backend's consumer can run its recovery path
        // (clear cache, force-republish diagnostics) instead of silently going
        // dark.
        let evt = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("Dropped delivered before timeout")
            .expect("channel still open");
        assert!(matches!(evt, LibpathEvent::Dropped));
    }

    #[tokio::test]
    async fn prospective_watcher_refuses_partial_coverage_and_closes_journal() {
        let valid = tempdir().unwrap();
        let journal = LibpathWatchJournal::new_buffering();
        let handle = prearm_watcher(
            vec![
                valid.path().to_path_buf(),
                valid.path().join("missing-library"),
            ],
            Duration::from_millis(50),
            Arc::clone(&journal),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(handle.is_none());
        assert!(journal.is_closed_for_test());
        assert!(journal.claim().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_prearm_after_setup_starts_closes_buffering_journal() {
        let valid = tempdir().unwrap();
        let journal = LibpathWatchJournal::new_buffering();
        let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        journal.arm_prearm_setup_pause_for_test(PrearmSetupTestPause {
            owner: "cancelled-owner",
            arrived: arrived_tx,
            release: release_rx,
            completed: completed_tx,
        });

        let mut task = tokio::spawn(prearm_watcher(
            vec![valid.path().to_path_buf()],
            Duration::from_millis(50),
            Arc::clone(&journal),
            tokio_util::sync::CancellationToken::new(),
        ));
        assert_eq!(arrived_rx.await.unwrap(), "cancelled-owner");

        // The pause belongs only to this journal. An unrelated prearm must
        // reach its own invocation-owned barrier instead of stealing or
        // waiting on this setup hook. No wall-clock timeout is involved.
        let unrelated = tempdir().unwrap();
        let unrelated_journal = LibpathWatchJournal::new_buffering();
        let (unrelated_arrived_tx, unrelated_arrived_rx) = tokio::sync::oneshot::channel();
        let (unrelated_release_tx, unrelated_release_rx) = std::sync::mpsc::channel();
        let (unrelated_completed_tx, unrelated_completed_rx) = tokio::sync::oneshot::channel();
        unrelated_journal.arm_prearm_setup_pause_for_test(PrearmSetupTestPause {
            owner: "unrelated-owner",
            arrived: unrelated_arrived_tx,
            release: unrelated_release_rx,
            completed: unrelated_completed_tx,
        });
        let unrelated_task = tokio::spawn(prearm_watcher(
            vec![unrelated.path().to_path_buf()],
            Duration::from_millis(50),
            Arc::clone(&unrelated_journal),
            tokio_util::sync::CancellationToken::new(),
        ));
        tokio::pin!(unrelated_arrived_rx);
        tokio::select! {
            owner = &mut unrelated_arrived_rx => {
                assert_eq!(owner.unwrap(), "unrelated-owner");
            }
            _ = &mut task => {
                panic!(
                    "cancelled-owner crossed its held pause while unrelated-owner was starting"
                );
            }
        }
        unrelated_release_tx.send(()).unwrap();
        assert_eq!(unrelated_completed_rx.await.unwrap(), "unrelated-owner");
        let unrelated_handle = unrelated_task
            .await
            .unwrap()
            .expect("unrelated path attaches");
        drop(unrelated_handle);

        task.abort();
        match task.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("prearm task must observe cancellation"),
        }
        assert!(
            journal.is_closed_for_test(),
            "async-side guard closes before detached setup is released"
        );
        release_tx.send(()).unwrap();
        assert_eq!(completed_rx.await.unwrap(), "cancelled-owner");
        assert!(journal.claim().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_cancels_paused_prospective_prearm_and_closes_journal() {
        let valid = tempdir().unwrap();
        let journal = LibpathWatchJournal::new_buffering();
        let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        journal.arm_prearm_setup_pause_for_test(PrearmSetupTestPause {
            owner: "shutdown-owner",
            arrived: arrived_tx,
            release: release_rx,
            completed: completed_tx,
        });
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(prearm_watcher(
            vec![valid.path().to_path_buf()],
            Duration::from_millis(50),
            Arc::clone(&journal),
            shutdown.clone(),
        ));
        assert_eq!(arrived_rx.await.unwrap(), "shutdown-owner");

        shutdown.cancel();
        assert!(task.await.unwrap().is_none());
        assert!(journal.is_closed_for_test());
        release_tx.send(()).unwrap();
        assert_eq!(completed_rx.await.unwrap(), "shutdown-owner");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(added: &[&str], removed: &[&str], touched: &[&str]) -> LibpathEvent {
        LibpathEvent::Changed {
            added: added.iter().map(|name| (*name).to_string()).collect(),
            removed: removed.iter().map(|name| (*name).to_string()).collect(),
            touched: touched.iter().map(|name| (*name).to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn journal_buffers_until_one_checked_activation() {
        let journal = LibpathWatchJournal::new_buffering();
        journal.record(changed(&["alpha"], &[], &[]));
        let waiter = tokio::spawn({
            let journal = Arc::clone(&journal);
            async move { journal.claim().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        assert!(journal.is_buffering());
        assert!(journal.try_activate());
        assert!(!journal.try_activate());
        let mut delivery = waiter.await.unwrap().unwrap();
        assert_eq!(delivery.event(), &changed(&["alpha"], &[], &[]));
        delivery.ack();
    }

    #[tokio::test]
    async fn cancelled_shutdown_cannot_claim_ready_journal_currency() {
        let journal = LibpathWatchJournal::new_buffering();
        journal.record(changed(&["alpha"], &[], &[]));
        assert!(journal.try_activate());
        let shutdown = tokio_util::sync::CancellationToken::new();
        shutdown.cancel();
        assert!(
            journal.claim_until_shutdown(&shutdown).await.is_none(),
            "already-cancelled shutdown wins before ready currency is removed"
        );
        let mut delivery = journal.claim().await.unwrap();
        assert_eq!(delivery.event(), &changed(&["alpha"], &[], &[]));
        delivery.ack();
    }

    #[tokio::test(start_paused = true)]
    async fn journal_retry_delay_is_not_shortened_by_event_storm() {
        let journal = LibpathWatchJournal::new_buffering();
        assert!(journal.try_activate());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let retry = tokio::spawn({
            let journal = Arc::clone(&journal);
            let shutdown = shutdown.clone();
            async move { journal.wait_retry(Duration::from_secs(10), &shutdown).await }
        });
        tokio::task::yield_now().await;
        for index in 0..100 {
            journal.record(changed(&[&format!("package-{index}")], &[], &[]));
        }
        tokio::task::yield_now().await;
        assert!(!retry.is_finished());
        tokio::time::advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;
        assert!(!retry.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(retry.await.unwrap());
    }

    #[test]
    fn journal_activation_rejects_closed_phase() {
        let journal = LibpathWatchJournal::new_buffering();
        journal.close();
        assert!(journal.is_closed_for_test());
        assert!(!journal.try_activate());
    }

    #[tokio::test]
    async fn journal_coalesces_package_changes_last_write_wins() {
        let journal = LibpathWatchJournal::new_buffering();
        journal.record(changed(&["add_remove", "add_touch"], &[], &[]));
        journal.record(changed(&["remove_add"], &["add_remove"], &["add_touch"]));
        journal.record(changed(&[], &["remove_add"], &[]));
        journal.record(changed(&["remove_add"], &[], &[]));
        assert!(journal.try_activate());

        let mut delivery = journal.claim().await.unwrap();
        assert_eq!(
            delivery.event(),
            &changed(&["remove_add"], &["add_remove"], &["add_touch"])
        );
        delivery.ack();
    }

    #[tokio::test]
    async fn journal_huge_change_promotes_to_rescan_and_dropped_dominates() {
        let journal = LibpathWatchJournal::new_buffering();
        let touched = (0..=LIBPATH_JOURNAL_NAME_CAPACITY)
            .map(|index| format!("pkg_{index}"))
            .collect();
        journal.record(LibpathEvent::Changed {
            added: HashSet::new(),
            removed: HashSet::new(),
            touched,
        });
        journal.record(changed(&["subsumed"], &[], &[]));
        journal.record(LibpathEvent::Dropped);
        journal.record(LibpathEvent::Rescan);
        assert!(journal.try_activate());

        let mut delivery = journal.claim().await.unwrap();
        assert_eq!(delivery.event(), &LibpathEvent::Dropped);
        delivery.ack();
    }

    #[tokio::test]
    async fn journal_capacity_counts_distinct_names_and_repeated_keys_do_not_overflow() {
        let journal = LibpathWatchJournal::new_buffering();
        for index in 0..LIBPATH_JOURNAL_NAME_CAPACITY {
            journal.record(changed(&[], &[], &[&format!("pkg_{index}")]));
        }
        for _ in 0..LIBPATH_JOURNAL_NAME_CAPACITY {
            journal.record(changed(&["pkg_0"], &[], &[]));
        }
        assert!(journal.try_activate());
        let mut delivery = journal.claim().await.unwrap();
        let LibpathEvent::Changed {
            added,
            removed,
            touched,
        } = delivery.event()
        else {
            panic!("exact capacity remains targeted")
        };
        assert_eq!(added, &HashSet::from(["pkg_0".to_string()]));
        assert!(removed.is_empty());
        assert_eq!(added.len() + touched.len(), LIBPATH_JOURNAL_NAME_CAPACITY);
        delivery.ack();

        let overflow = LibpathWatchJournal::new_buffering();
        for index in 0..=LIBPATH_JOURNAL_NAME_CAPACITY {
            overflow.record(changed(&[], &[], &[&format!("pkg_{index}")]));
        }
        assert!(overflow.try_activate());
        let delivery = overflow.claim().await.unwrap();
        assert_eq!(delivery.event(), &LibpathEvent::Rescan);
    }

    #[tokio::test]
    async fn journal_rescan_ack_preserves_newer_changed_and_nack_subsumes_it() {
        let journal = LibpathWatchJournal::new_buffering();
        journal.record(LibpathEvent::Rescan);
        assert!(journal.try_activate());
        let mut rescan = journal.claim().await.unwrap();
        journal.record(changed(&["newer"], &[], &[]));
        rescan.ack();
        let mut changed_delivery = journal.claim().await.unwrap();
        assert_eq!(changed_delivery.event(), &changed(&["newer"], &[], &[]));
        changed_delivery.ack();

        journal.record(LibpathEvent::Rescan);
        let rescan = journal.claim().await.unwrap();
        journal.record(changed(&["subsumed"], &[], &[]));
        drop(rescan);
        let mut restored = journal.claim().await.unwrap();
        assert_eq!(restored.event(), &LibpathEvent::Rescan);
        restored.ack();
    }

    #[tokio::test]
    async fn journal_close_during_claim_prevents_requeue_and_wakes_waiter() {
        let journal = LibpathWatchJournal::new_buffering();
        journal.record(changed(&["claimed"], &[], &[]));
        assert!(journal.try_activate());
        let delivery = journal.claim().await.unwrap();
        let blocked = tokio::spawn({
            let journal = Arc::clone(&journal);
            async move { journal.claim().await }
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        journal.close();
        drop(delivery);
        assert!(blocked.await.unwrap().is_none());
        assert!(journal.is_closed_for_test());
    }

    #[tokio::test]
    async fn journal_nack_remerges_older_claim_before_newer_changes() {
        let journal = LibpathWatchJournal::new_buffering();
        journal.record(changed(&["alpha"], &[], &[]));
        assert!(journal.try_activate());
        let delivery = journal.claim().await.unwrap();
        journal.record(changed(&["beta"], &["alpha"], &[]));
        drop(delivery);

        let mut redelivery = journal.claim().await.unwrap();
        assert_eq!(redelivery.event(), &changed(&["beta"], &["alpha"], &[]));
        redelivery.ack();
    }

    #[tokio::test]
    async fn journal_ack_preserves_newer_dropped() {
        let journal = LibpathWatchJournal::new_buffering();
        journal.record(LibpathEvent::Rescan);
        assert!(journal.try_activate());
        let mut delivery = journal.claim().await.unwrap();
        journal.record(LibpathEvent::Dropped);
        delivery.ack();

        let mut terminal = journal.claim().await.unwrap();
        assert_eq!(terminal.event(), &LibpathEvent::Dropped);
        terminal.ack();
    }

    #[tokio::test]
    #[should_panic(expected = "exact in-flight generation")]
    async fn journal_ack_rejects_an_already_consumed_generation() {
        let journal = LibpathWatchJournal::new_buffering();
        journal.record(LibpathEvent::Rescan);
        assert!(journal.try_activate());
        let mut delivery = journal.claim().await.unwrap();
        delivery.ack();
        delivery.ack();
    }

    #[test]
    fn affected_packages_unions_all_three_sets() {
        let ev = LibpathEvent::Changed {
            added: ["a".to_string()].into_iter().collect(),
            removed: ["b".to_string()].into_iter().collect(),
            touched: ["c".to_string(), "a".to_string()].into_iter().collect(),
        };
        let aff = ev.affected_packages();
        assert!(aff.contains("a"));
        assert!(aff.contains("b"));
        assert!(aff.contains("c"));
        assert_eq!(aff.len(), 3);
    }

    #[test]
    fn affected_packages_empty_for_dropped() {
        assert!(LibpathEvent::Dropped.affected_packages().is_empty());
    }
}
