// package_library.rs - Package library manager for package function awareness
//
// This module provides the PackageLibrary struct which manages installed R packages,
// their exports, and caching. It integrates with the R subprocess interface for
// querying package information and falls back to NAMESPACE file parsing when needed.
//
// Requirement 13.1: THE Package_Cache SHALL store parsed exports per package
// Requirement 13.4: THE Package_Cache SHALL support concurrent read access from multiple LSP handlers

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::namespace_parser::{parse_data_symbols, parse_description_depends, parse_index_exports};
use crate::package_db::PackageMetadataProvider;
use crate::r_subprocess::RSubprocess;

/// Meta-packages that attach multiple other packages when loaded
///
/// Requirement 4.3: WHEN the package is `tidyverse`, THE Package_Resolver SHALL also load
/// exports from: dplyr, readr, forcats, stringr, ggplot2, tibble, lubridate, tidyr, purrr
pub const TIDYVERSE_PACKAGES: &[&str] = &[
    "dplyr",
    "readr",
    "forcats",
    "stringr",
    "ggplot2",
    "tibble",
    "lubridate",
    "tidyr",
    "purrr",
];

/// Requirement 4.4: WHEN the package is `tidymodels`, THE Package_Resolver SHALL also load
/// exports from: broom, dials, dplyr, ggplot2, infer, modeldata, parsnip, purrr, recipes,
/// rsample, tibble, tidyr, tune, workflows, workflowsets, yardstick
pub const TIDYMODELS_PACKAGES: &[&str] = &[
    "broom",
    "dials",
    "dplyr",
    "ggplot2",
    "infer",
    "modeldata",
    "parsnip",
    "purrr",
    "recipes",
    "rsample",
    "tibble",
    "tidyr",
    "tune",
    "workflows",
    "workflowsets",
    "yardstick",
];

/// Return the set of child packages attached by a meta-package name.
/// Empty for non-meta packages.
fn meta_attached_packages(name: &str) -> &'static [&'static str] {
    match name {
        "tidyverse" => TIDYVERSE_PACKAGES,
        "tidymodels" => TIDYMODELS_PACKAGES,
        _ => &[],
    }
}

fn meta_attached_package_names(name: &str) -> Vec<String> {
    meta_attached_packages(name)
        .iter()
        .map(|package| (*package).to_string())
        .collect()
}

/// Meta-package fields for a package name: `(is_meta_package, attached_packages)`.
/// A package is a meta-package exactly when it attaches children, so
/// `meta_attached_packages` is the single source of truth — and deriving both
/// fields here keeps the two `PackageInfo` constructors from drifting apart.
fn meta_package_fields(name: &str) -> (bool, Vec<String>) {
    let attached_packages = meta_attached_package_names(name);
    (!attached_packages.is_empty(), attached_packages)
}

/// Reserved package name used to model `devtools::load_all()` / `pkgload::load_all()`
/// as a synthetic attached package. Contains `_`, which is illegal in real R package
/// names, so it can never collide with an installed/attached package.
pub const LOAD_ALL_SENTINEL: &str = "__raven_load_all__";

/// True iff `name` is the reserved `load_all()` sentinel package name.
///
/// Every consumer that iterates attached package *names* and feeds them to
/// installed-package machinery or the R subprocess MUST skip the sentinel via this
/// predicate (the sentinel resolves only through the local-dev overlay chokepoints).
#[inline]
pub fn is_load_all_sentinel(name: &str) -> bool {
    name == LOAD_ALL_SENTINEL
}

/// User-facing display label for a symbol owned by the `load_all()` sentinel
/// package. The raw [`LOAD_ALL_SENTINEL`] string must NEVER reach the UI or the
/// R subprocess; every owner-consumer (hover, signature help, completion detail)
/// maps a sentinel owner through this helper instead.
///
/// `contrib_package_name` is the dev package's real DESCRIPTION `Package:` name
/// when known (`state.package_state.scope_contribution().package_name`); falls
/// back to a generic label when absent.
pub(crate) fn load_all_owner_display(contrib_package_name: Option<&str>) -> String {
    match contrib_package_name {
        Some(name) if !name.is_empty() && name != "unknown" => {
            format!("package under development ({})", name)
        }
        _ => "package under development".to_string(),
    }
}

/// Workspace-local internal symbol set exposed by a `load_all()` virtual
/// attached package. Built from the active `PackageScopeContribution`;
/// refreshed by the shared derived-package installer. Holds names only —
/// go-to-definition derives locations from the workspace index, never from
/// here.
#[derive(Debug, Clone, Default)]
pub struct LocalDevPackage {
    /// Union of r_internal ∪ sysdata ∪ onload ∪ imported symbol names.
    pub symbols: std::collections::HashSet<String>,
}

/// True when `path` is a regular file whose contents can actually be opened
/// for reading.
///
/// Validates a candidate package directory's `NAMESPACE` / `DESCRIPTION` in
/// [`PackageLibrary::find_package_directory`]. `is_file()` alone already rejects
/// the case where a directory is *named* `NAMESPACE`; the extra `File::open`
/// also skips a metadata file that exists but cannot be read (wrong
/// permissions, a dangling symlink target), so discovery moves on to the next
/// library path instead of treating an unusable directory as the package — the
/// "skip unreadable package directories" behavior. Opening is the only reliable
/// readability probe (a permissions stat is racy and platform-dependent); the
/// cost is one `open`/`close` per *resolved* package, paid once and then cached,
/// so it is negligible on the init / package-resolution path.
fn is_readable_file(path: &Path) -> bool {
    path.is_file() && std::fs::File::open(path).is_ok()
}

/// How complete a package's exported-member set is, for absence diagnostics.
/// `Absent` may be concluded only from a `Complete` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemberCompleteness {
    Complete,
    Partial,
    #[default]
    Unknown,
}

/// Result of a synchronous `pkg::member` membership query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceMemberStatus {
    Present,
    Absent,
    Partial,
    Unknown,
}

/// Cached package information
///
/// Stores all relevant information about an R package including its exports,
/// dependencies, and meta-package status.
#[derive(Debug, Clone)]
pub struct PackageInfo {
    /// Package name
    pub name: String,
    /// Exported symbols (functions, variables, and datasets)
    pub exports: HashSet<String>,
    /// Packages from Depends field
    pub depends: Vec<String>,
    /// Whether this is a meta-package with special handling
    pub is_meta_package: bool,
    /// Packages attached by meta-package (e.g., tidyverse attaches dplyr, ggplot2, etc.)
    pub attached_packages: Vec<String>,
    /// Lazy-loaded dataset names (e.g. `flights`, `diamonds`) — data objects a
    /// package ships under `data/` that appear in neither `export()` nor
    /// `getNamespaceExports()`. Kept distinct from `exports` so hover and
    /// completion can attribute functions vs. data, but `collect_exports_recursive`
    /// folds both into the resolvable set so datasets resolve as symbols
    /// (issue #350).
    ///
    /// For non-base packages this is populated by `package_info_from_dir`,
    /// the single chokepoint every on-disk load path routes through (both
    /// `get_package` and `prefetch_packages`, which inserts straight into the
    /// cache and so bypasses `get_package`). Base-package datasets take a
    /// separate route: `initialize` merges them into `base_exports`, so their
    /// `PackageInfo.lazy_data` is left empty by design.
    pub lazy_data: Vec<String>,
    /// Map from data-file stem to the object names that file binds, from R's
    /// `data(package=)` enumeration (issue #429). Covers multi-object files
    /// (survey: `api` → `apiclus1`, `apistrat`, ...). Populated for any
    /// package with a `data/` dir when the R subprocess is available; empty
    /// otherwise. Consumed only by `data()` call alias expansion in
    /// cross-file scope — never injected after bare `library()` for
    /// non-LazyData packages.
    pub data_aliases: HashMap<String, Vec<String>>,
    /// Completeness of `exports` for member-absence diagnostics. Defaults to
    /// `Unknown` (never absence-authoritative) via every constructor; production
    /// load paths stamp the real value. See `namespace_member_status_sync`.
    pub exports_completeness: MemberCompleteness,
}

impl PackageInfo {
    /// Create a new PackageInfo with the given name and exports
    pub fn new(name: String, exports: HashSet<String>) -> Self {
        let (is_meta_package, attached_packages) = meta_package_fields(&name);

        Self {
            name,
            exports,
            depends: Vec::new(),
            is_meta_package,
            attached_packages,
            lazy_data: Vec::new(),
            data_aliases: HashMap::new(),
            exports_completeness: MemberCompleteness::Unknown,
        }
    }

    /// Create a new PackageInfo with all fields specified
    pub fn with_details(
        name: String,
        exports: HashSet<String>,
        depends: Vec<String>,
        lazy_data: Vec<String>,
    ) -> Self {
        let (is_meta_package, attached_packages) = meta_package_fields(&name);

        Self {
            name,
            exports,
            depends,
            is_meta_package,
            attached_packages,
            lazy_data,
            data_aliases: HashMap::new(),
            exports_completeness: MemberCompleteness::Unknown,
        }
    }
}

/// Assemble a `PackageInfo` for a package found on disk, reading its datasets
/// from `data/` (issue #350). The single chokepoint every non-base on-disk
/// load path routes through, so dataset discovery can't regress when a new
/// path is added — see the `lazy_data` field doc for the full rationale and
/// the base-package exception.
///
/// `parse_data_symbols` is best-effort and runs its filesystem walk in
/// `spawn_blocking`, so this stays off the synchronous diagnostic hot path.
async fn package_info_from_dir(
    name: String,
    pkg_dir: &Path,
    exports: HashSet<String>,
    depends: Vec<String>,
) -> PackageInfo {
    let lazy_data = parse_data_symbols(pkg_dir).await;
    PackageInfo::with_details(name, exports, depends, lazy_data)
}

/// Collect borrowed symbol names into a sorted, de-duplicated owned `Vec`.
fn sorted_unique_symbols<'a>(symbols: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut names: Vec<String> = symbols.cloned().collect();
    names.sort();
    names.dedup();
    names
}

/// Fold R's `data(package=)` enumeration into a freshly-built `PackageInfo`.
///
/// `has_lazy_data_db` is the `data/Rdata.rdb` check: that DB exists iff
/// DESCRIPTION sets LazyData, and only then does `library(pkg)` attach the
/// datasets — so enumerated names replace `lazy_data` only in that case.
/// Non-LazyData packages keep their static file-stem `lazy_data` (deliberately
/// permissive; see issue #429 scope decisions). `data_aliases` is filled for
/// both, feeding `data()` call alias expansion.
fn apply_enumerated_data(
    info: &mut PackageInfo,
    enumerated: &[crate::r_subprocess::DataObject],
    has_lazy_data_db: bool,
) {
    if enumerated.is_empty() {
        return;
    }
    if has_lazy_data_db {
        info.lazy_data = enumerated.iter().map(|d| d.name.clone()).collect();
    }
    for d in enumerated {
        info.data_aliases
            .entry(d.file_stem.clone())
            .or_default()
            .push(d.name.clone());
    }
}

/// Removes the dataset entry for `name` from `map` (if any) and applies it to
/// `info` via [`apply_enumerated_data`].  The `has_db` flag is derived from the
/// package directory here so callers don't repeat that derivation.
fn apply_enumeration_from(
    map: &mut HashMap<String, Vec<crate::r_subprocess::DataObject>>,
    name: &str,
    pkg_dir: &std::path::Path,
    info: &mut PackageInfo,
) {
    if let Some(enumerated) = map.remove(name) {
        let has_db = pkg_dir.join("data").join("Rdata.rdb").is_file();
        apply_enumerated_data(info, &enumerated, has_db);
    }
}

/// De-duplicate `names` keeping first-seen order.
///
/// Used by [`PackageLibrary::prefetch_packages`] on the requested set. A
/// duplicated package name must be loaded exactly once: `prefetch_uncached_level`
/// applies each package's `data()` enumeration via [`apply_enumeration_from`],
/// which *consumes* the enumeration entry, so a second pass over the same name
/// would re-`insert_package` an un-enumerated `PackageInfo` and silently drop
/// the enumerated `data_aliases`/`lazy_data` (issue #429). First-seen order is
/// preserved so the dependency-closure BFS frontier stays deterministic.
fn dedup_preserving_order(names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .filter(|n| seen.insert(n.clone()))
        .collect()
}

/// Result of parsing a package's NAMESPACE and DESCRIPTION files statically.
///
/// This struct separates explicit exports (from `export()` and `S3method()` directives)
/// from packages that require R subprocess due to `exportPattern()` usage.
#[derive(Debug)]
pub struct NamespaceParseResult {
    /// Explicit exports parsed from NAMESPACE: `export()`, `S3method()`,
    /// `exportClasses()` and `exportMethods()` names (issue #474 — S4
    /// classes/generics appear in `getNamespaceExports()` and are captured here
    /// as plain names). `exportPattern()` / `exportClassPattern()` are NOT
    /// included; they set [`Self::has_export_pattern`] instead. So when
    /// `has_export_pattern` is false this set is the package's *complete* export
    /// list, which is what lets `namespace_member_status_sync` conclude `Absent`
    /// from a static parse.
    pub explicit_exports: Vec<String>,
    /// Whether the package uses exportPattern() and needs R subprocess for accuracy
    pub has_export_pattern: bool,
    /// Dependencies from DESCRIPTION Depends field
    pub depends: Vec<String>,
}
/// Cached aggregate package view produced by `get_all_exports`.
///
/// `exports` answers availability: every symbol visible through the aggregate
/// package key, including symbols contributed by `Depends` and meta-package
/// attachments. `owners` answers attribution for the same snapshot:
/// `symbol -> true owner package` (e.g. `mutate -> dplyr` under the
/// `tidyverse` key). Keeping both projections in one entry makes publication,
/// invalidation, and reads atomic at the aggregate-key level.
///
/// Invariant: the sole production writer (`collect_exports_recursive` via
/// `get_all_exports`) fills `exports` and `owners` from the same traversal, so
/// every symbol in `exports` has a matching key in `owners`. The fail-closed
/// guard in `find_package_owner_for_symbol` therefore defends only against a
/// partial snapshot that production never constructs — exercised by tests that
/// seed an entry directly.
#[derive(Debug)]
struct CombinedEntry {
    exports: Arc<HashSet<String>>,
    owners: HashMap<String, String>,
}

impl CombinedEntry {
    fn new(exports: HashSet<String>, owners: HashMap<String, String>) -> Self {
        Self {
            exports: Arc::new(exports),
            owners,
        }
    }
}

type PackageCache = HashMap<String, Arc<PackageInfo>>;
type CombinedCache = HashMap<String, Arc<CombinedEntry>>;
enum CachedCompletionEntry {
    Combined {
        loaded_package: String,
        entry: Arc<CombinedEntry>,
    },
    Package {
        loaded_package: String,
        info: Arc<PackageInfo>,
    },
}

/// Package library manager
///
/// Manages the collection of installed R packages and their cached exports.
/// Uses atomic read-copy snapshots for thread-safe concurrent read access from
/// multiple LSP handlers.
///
/// Requirement 13.1: THE Package_Cache SHALL store parsed exports per package
/// Requirement 13.4: THE Package_Cache SHALL support concurrent read access from multiple LSP handlers
pub struct PackageLibrary {
    /// Library paths (from R or configuration)
    lib_paths: Vec<PathBuf>,
    /// Cached package information (lazy-loaded).
    ///
    /// Readers load immutable snapshots without taking a read lock. Writers
    /// serialize copy-on-write publications through `packages_write`.
    packages: ArcSwap<PackageCache>,
    packages_write: Mutex<()>,
    /// Combined aggregate cache keyed by the loaded package name.
    ///
    /// Each entry stores availability (`exports`) and ownership (`owners`) in
    /// one immutable snapshot. For `library(tidyverse)`, `exports` contains
    /// `mutate` and `owners` records `mutate -> dplyr`. This single source of
    /// truth prevents readers from seeing an aggregate export without its owner
    /// attribution during cache warm-up or invalidation. See issue #407.
    combined_entries: ArcSwap<CombinedCache>,
    combined_entries_write: Mutex<()>,
    /// Base packages (always available)
    base_packages: HashSet<String>,
    /// Base package exports (combined from all base packages).
    ///
    /// Wrapped in `Arc` so consumers (e.g. `DiagnosticsSnapshot::build`) can
    /// share the set across snapshots without deep-cloning every published
    /// diagnostic batch.
    base_exports: Arc<HashSet<String>>,
    /// Durable export-name → owner-package map for the default-attached base
    /// packages (base, stats, utils, ...). Recovers the true owner of a symbol
    /// that the cross-file scope pools under the synthetic `package:base` URI
    /// (see `cross_file::scope::seed_base_exports`), e.g. `ave` → `stats`.
    ///
    /// Deliberately excluded from [`Self::clear_cache`] and rebuilt ONLY by
    /// [`Self::initialize`] (via `recompute_default_attached_owners`), giving it
    /// the same durability contract as `base_exports`: `raven.refreshPackages`
    /// clears the per-package `packages` cache but must NOT drop this map, or
    /// hover would revert to misattributing `ave` to {base}. See issue #592.
    default_attached_owners: Arc<HashMap<String, String>>,
    /// R subprocess interface (None if R is unavailable)
    r_subprocess: Option<RSubprocess>,
    /// Ordered fallback metadata providers (Tier 2 repo DB, then Tier 3 shipped
    /// DB), consulted only when the installed (Tier 1) path does not resolve a
    /// package. Empty by default; populated by `build_package_library`. These
    /// feed export resolution only — never `package_exists()` (install status).
    providers: Vec<Arc<dyn PackageMetadataProvider>>,
    /// Local-dev overlay: sentinel -> workspace-local internal symbols.
    /// Consulted by the three resolution chokepoints before the installed caches.
    /// Refreshed by the shared derived-package installer.
    local_dev_overlay: ArcSwap<Option<Arc<LocalDevPackage>>>,
    /// Operation-level ownership for cache mutations.
    ///
    /// A shared lease spans one complete logical load, including every
    /// per-package and aggregate publication. Library-routing transactions
    /// briefly take the exclusive side to clone a coherent successor and again
    /// at the central commit. `cache_operation_epoch` changes when a logical
    /// mutation starts, so a successor prepared from an older cache is rejected
    /// instead of overwriting an in-flight warm.
    cache_operation_gate: tokio::sync::RwLock<()>,
    cache_operation_epoch: AtomicU64,
    retired: AtomicBool,
}

pub(crate) struct PackageCacheOperationLease<'a> {
    _guard: tokio::sync::RwLockReadGuard<'a, ()>,
}

pub(crate) struct PackageLibraryRoutingLease<'a> {
    _guard: tokio::sync::RwLockWriteGuard<'a, ()>,
}

impl PackageLibrary {
    /// Create a new PackageLibrary with default (empty) state
    ///
    /// This creates an uninitialized PackageLibrary. Call `initialize()` to
    /// populate it with data from R subprocess or fallback values.
    ///
    /// For a fully initialized PackageLibrary, use `PackageLibrary::new()` instead.
    pub fn new_empty() -> Self {
        Self {
            lib_paths: Vec::new(),
            packages: ArcSwap::from_pointee(HashMap::new()),
            packages_write: Mutex::new(()),
            combined_entries: ArcSwap::from_pointee(HashMap::new()),
            combined_entries_write: Mutex::new(()),
            base_packages: HashSet::new(),
            base_exports: Arc::new(HashSet::new()),
            default_attached_owners: Arc::new(HashMap::new()),
            r_subprocess: None,
            providers: Vec::new(),
            local_dev_overlay: ArcSwap::from_pointee(None),
            cache_operation_gate: tokio::sync::RwLock::new(()),
            cache_operation_epoch: AtomicU64::new(0),
            retired: AtomicBool::new(false),
        }
    }

    /// Create a new PackageLibrary with the given R subprocess
    ///
    /// This is a synchronous constructor that sets up the PackageLibrary
    /// with the R subprocess interface. The actual initialization (querying
    /// R for lib_paths and base_packages) should be done via `initialize()`.
    ///
    /// # Arguments
    /// * `r_subprocess` - Optional R subprocess interface for querying package info
    pub fn with_subprocess(r_subprocess: Option<RSubprocess>) -> Self {
        Self {
            lib_paths: Vec::new(),
            packages: ArcSwap::from_pointee(HashMap::new()),
            packages_write: Mutex::new(()),
            combined_entries: ArcSwap::from_pointee(HashMap::new()),
            combined_entries_write: Mutex::new(()),
            base_packages: HashSet::new(),
            base_exports: Arc::new(HashSet::new()),
            default_attached_owners: Arc::new(HashMap::new()),
            r_subprocess,
            providers: Vec::new(),
            local_dev_overlay: ArcSwap::from_pointee(None),
            cache_operation_gate: tokio::sync::RwLock::new(()),
            cache_operation_epoch: AtomicU64::new(0),
            retired: AtomicBool::new(false),
        }
    }

    /// Get the library paths
    pub fn lib_paths(&self) -> &[PathBuf] {
        &self.lib_paths
    }

    /// Get the base packages
    pub fn base_packages(&self) -> &HashSet<String> {
        &self.base_packages
    }

    /// Get the base exports
    pub fn base_exports(&self) -> &Arc<HashSet<String>> {
        &self.base_exports
    }

    /// Check if a symbol is exported by base packages
    ///
    /// Requirement 6.3: THE Base_Packages SHALL be available at all positions
    /// in all files without requiring explicit library() calls
    pub fn is_base_export(&self, symbol: &str) -> bool {
        self.base_exports.contains(symbol)
    }

    /// Check if a package is a base package
    pub fn is_base_package(&self, package: &str) -> bool {
        self.base_packages.contains(package)
    }

    /// Get a reference to the R subprocess interface (if available)
    ///
    /// Returns `None` if R is unavailable or the PackageLibrary was created without
    /// an R subprocess interface.
    pub fn r_subprocess(&self) -> Option<&RSubprocess> {
        self.r_subprocess.as_ref()
    }

    /// Replace the ordered fallback providers (tier order: index 0 first).
    pub fn set_providers(&mut self, providers: Vec<Box<dyn PackageMetadataProvider>>) {
        self.providers = providers.into_iter().map(Arc::from).collect();
    }

    /// Replace the ordered fallback providers with already-shared providers.
    ///
    /// Same-key provider discovery is coalesced across library builds, so its
    /// immutable result must be cheaply reusable by every waiter.
    fn set_shared_providers(&mut self, providers: Vec<Arc<dyn PackageMetadataProvider>>) {
        self.providers = providers;
    }

    async fn begin_cache_operation(&self) -> Option<PackageCacheOperationLease<'_>> {
        let guard = self.cache_operation_gate.read().await;
        if self.retired.load(Ordering::Acquire) {
            return None;
        }
        self.cache_operation_epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .expect("package-cache operation epoch exhausted");
        Some(PackageCacheOperationLease { _guard: guard })
    }

    pub(crate) async fn routing_lease(&self) -> PackageLibraryRoutingLease<'_> {
        PackageLibraryRoutingLease {
            _guard: self.cache_operation_gate.write().await,
        }
    }

    pub(crate) fn cache_operation_epoch(&self, _lease: &PackageLibraryRoutingLease<'_>) -> u64 {
        self.cache_operation_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn retire(&self, _lease: &PackageLibraryRoutingLease<'_>) {
        self.retired.store(true, Ordering::Release);
    }

    /// Clone the complete cache/runtime view while the caller holds exclusive
    /// routing ownership. The returned library is unpublished and may be
    /// invalidated or warmed without changing the authoritative library.
    pub(crate) fn fork_cache_snapshot(&self, _lease: &PackageLibraryRoutingLease<'_>) -> Self {
        Self {
            lib_paths: self.lib_paths.clone(),
            packages: ArcSwap::new(self.packages.load_full()),
            packages_write: Mutex::new(()),
            combined_entries: ArcSwap::new(self.combined_entries.load_full()),
            combined_entries_write: Mutex::new(()),
            base_packages: self.base_packages.clone(),
            base_exports: Arc::clone(&self.base_exports),
            default_attached_owners: Arc::clone(&self.default_attached_owners),
            r_subprocess: self.r_subprocess.clone(),
            providers: self.providers.clone(),
            local_dev_overlay: ArcSwap::new(self.local_dev_overlay.load_full()),
            cache_operation_gate: tokio::sync::RwLock::new(()),
            cache_operation_epoch: AtomicU64::new(0),
            retired: AtomicBool::new(false),
        }
    }

    /// True when fallback providers are configured (Tier 2/3 present beyond the
    /// common Tier-1-only case).
    pub fn has_providers(&self) -> bool {
        !self.providers.is_empty()
    }

    /// Replace the local-dev overlay.
    ///
    /// Package-state contribution changes call this only from the shared
    /// derived-package installer. Package-library replacement paths separately
    /// reapply the current contribution to the new library object.
    pub fn set_local_dev_overlay(&self, overlay: Option<Arc<LocalDevPackage>>) {
        self.local_dev_overlay.store(Arc::new(overlay));
    }

    /// True iff the load_all sentinel is in `loaded_packages` and the overlay
    /// contains `name`. Consulted at the three resolution chokepoints
    /// (`is_symbol_from_loaded_packages`, `find_package_owner_for_symbol`,
    /// `get_owned_exports_for_completions`).
    fn overlay_has_symbol(&self, name: &str, loaded_packages: &[String]) -> bool {
        // Hot path (per-symbol via `is_symbol_from_loaded_packages`): in the common
        // no-`load_all` case the overlay is `None`, so load it first (an O(1)
        // ArcSwap load) and early-return before scanning `loaded_packages` for the
        // sentinel.
        let overlay = self.local_dev_overlay.load();
        let Some(pkg) = overlay.as_ref().as_ref() else {
            return false;
        };
        if !loaded_packages.iter().any(|p| is_load_all_sentinel(p)) {
            return false;
        }
        pkg.symbols.contains(name)
    }

    /// Publish a new per-package cache snapshot after applying `f` to a clone of
    /// the current map.
    fn update_packages<R>(
        &self,
        _lease: &PackageCacheOperationLease<'_>,
        f: impl FnOnce(&mut PackageCache) -> R,
    ) -> R {
        let _guard = self.packages_write.lock();
        let mut next = self.packages.load_full().as_ref().clone();
        let result = f(&mut next);
        self.packages.store(Arc::new(next));
        result
    }

    /// Publish a new combined-entry cache snapshot after applying `f` to a clone
    /// of the current map.
    fn update_combined_entries<R>(
        &self,
        _lease: &PackageCacheOperationLease<'_>,
        f: impl FnOnce(&mut CombinedCache) -> R,
    ) -> R {
        let _guard = self.combined_entries_write.lock();
        let mut next = self.combined_entries.load_full().as_ref().clone();
        let result = f(&mut next);
        self.combined_entries.store(Arc::new(next));
        result
    }

    /// Test-support hook for benchmarks that need to model an in-progress
    /// package-cache publication. Snapshot readers do not use this gate, so
    /// they continue to read the last published map while the gate is held.
    #[cfg(feature = "test-support")]
    pub fn with_packages_publish_gate_for_test<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self.packages_write.lock();
        f()
    }

    /// Consult the fallback providers (Tier 2 → Tier 3) in order; return the
    /// first source that knows `name`. Pure, synchronous reads.
    fn resolve_from_providers(&self, name: &str) -> Option<PackageInfo> {
        for provider in &self.providers {
            if let Some(info) = provider.lookup(name) {
                log::trace!("Package '{}' resolved from a fallback provider", name);
                return Some(info);
            }
        }
        None
    }

    /// Clone only the cache entry handles needed by completion readers.
    ///
    /// Completion materialization can clone many strings, so snapshot reads
    /// only do the O(loaded_packages) lookup/Arc-clone phase.
    /// Aggregate entries win over direct per-package entries, preserving the
    /// existing "combined first, package fallback" behavior.
    fn cached_completion_entries(&self, loaded_packages: &[String]) -> Vec<CachedCompletionEntry> {
        let combined_cache = self.combined_entries.load();
        let packages_cache = self.packages.load();
        let mut entries = Vec::with_capacity(loaded_packages.len());

        if combined_cache.is_empty() {
            for pkg_name in loaded_packages {
                if let Some(info) = packages_cache.get(pkg_name) {
                    entries.push(CachedCompletionEntry::Package {
                        loaded_package: pkg_name.clone(),
                        info: Arc::clone(info),
                    });
                }
            }
        } else {
            for pkg_name in loaded_packages {
                if let Some(entry) = combined_cache.get(pkg_name) {
                    entries.push(CachedCompletionEntry::Combined {
                        loaded_package: pkg_name.clone(),
                        entry: Arc::clone(entry),
                    });
                } else if let Some(info) = packages_cache.get(pkg_name) {
                    entries.push(CachedCompletionEntry::Package {
                        loaded_package: pkg_name.clone(),
                        info: Arc::clone(info),
                    });
                }
            }
        }

        entries
    }

    /// Get all exports from loaded packages for completions (synchronous, cached-only)
    ///
    /// This method returns a map of symbol name to package name for all exports
    /// from the given loaded packages. It uses cached package information,
    /// preferring combined_entries cache (includes Depends/attached) when available.
    ///
    /// This is a synchronous method suitable for use in completion handlers where
    /// we cannot use async. It reads immutable cache snapshots and clones only
    /// the relevant cache entry handles before materializing completion strings.
    ///
    /// Returns a HashMap where:
    /// - Key: export name (symbol)
    /// - Value: package name that exports it
    ///
    /// If multiple packages export the same symbol, the first package in the list wins.
    /// When multiple packages export the same symbol, all packages are included in the
    /// result vector for that symbol, in the order they were loaded.
    ///
    /// Requirements 9.1, 9.2, 9.3: Get package exports for completions with package attribution
    /// Requirement 9.3: When multiple packages export same symbol, show all with attribution
    pub fn get_exports_for_completions(
        &self,
        loaded_packages: &[String],
    ) -> std::collections::HashMap<String, Vec<String>> {
        let entries = self.cached_completion_entries(loaded_packages);
        let capacity = entries
            .iter()
            .map(|entry| match entry {
                CachedCompletionEntry::Combined { entry, .. } => entry.exports.len(),
                CachedCompletionEntry::Package { info, .. } => info.exports.len(),
            })
            .sum();
        let mut result: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::with_capacity(capacity);

        // Process packages in order (earlier packages appear first in the list)
        // Requirement 9.3: Show all packages that export the same symbol
        for entry in entries {
            match entry {
                CachedCompletionEntry::Combined {
                    loaded_package,
                    entry,
                } => {
                    for export in entry.exports.iter() {
                        result
                            .entry(export.clone())
                            .or_default()
                            .push(loaded_package.clone());
                    }
                }
                CachedCompletionEntry::Package {
                    loaded_package,
                    info,
                } => {
                    for export in &info.exports {
                        result
                            .entry(export.clone())
                            .or_default()
                            .push(loaded_package.clone());
                    }
                }
            }
        }

        result
    }

    /// Like [`Self::get_exports_for_completions`], but attributes each symbol to its
    /// true **owner** package rather than the loaded/aggregate package that made
    /// it visible. For `library(tidyverse)`, `mutate` is attributed to `dplyr`
    /// (the documentation owner) so completion detail (`{dplyr}`) and the
    /// resolve `data.package` open the correct help topic. See issue #407.
    ///
    /// Synchronous and cached-only. It reads immutable cache snapshots and
    /// clones only relevant cache entry handles before materializing completion
    /// strings. A cached aggregate entry is a single availability/ownership
    /// snapshot, so owner-sensitive completion never falls back from a present
    /// aggregate entry to loaded-package attribution. If no aggregate entry
    /// exists yet, it falls back to the per-package cache, preserving the
    /// previous direct-package behavior. Owners are de-duplicated per symbol
    /// (two loaded aggregates can resolve to the same owner).
    pub fn get_owned_exports_for_completions(
        &self,
        loaded_packages: &[String],
    ) -> std::collections::HashMap<String, Vec<String>> {
        let entries = self.cached_completion_entries(loaded_packages);
        let capacity = entries
            .iter()
            .map(|entry| match entry {
                CachedCompletionEntry::Combined { entry, .. } => entry.owners.len(),
                CachedCompletionEntry::Package { info, .. } => info.exports.len(),
            })
            .sum();
        let mut result: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::with_capacity(capacity);

        let mut push_unique = |symbol: &str, owner: &str| {
            let owners = result.entry(symbol.to_string()).or_default();
            if !owners.iter().any(|o| o == owner) {
                owners.push(owner.to_string());
            }
        };
        for entry in entries {
            match entry {
                CachedCompletionEntry::Combined { entry, .. } => {
                    for (symbol, owner) in entry.owners.iter() {
                        push_unique(symbol, owner);
                    }
                }
                CachedCompletionEntry::Package {
                    loaded_package,
                    info,
                } => {
                    for export in &info.exports {
                        push_unique(export, &loaded_package);
                    }
                }
            }
        }

        // Fold in workspace-local internals attached via `devtools::load_all()`,
        // owned by the synthetic sentinel package. Only when the sentinel is in
        // `loaded_packages` and the overlay is present.
        if loaded_packages.iter().any(|p| is_load_all_sentinel(p))
            && let Some(pkg) = self.local_dev_overlay.load().as_ref()
        {
            for symbol in &pkg.symbols {
                push_unique(symbol, LOAD_ALL_SENTINEL);
            }
        }

        result
    }

    /// Synchronously fetch a single package's exported symbol names for `pkg::`
    /// completion, without ever touching the R subprocess.
    ///
    /// Returns the first **non-empty** export set across these tiers, else
    /// `None`. Each tier is consulted only as a fallback when the prior one
    /// yields nothing, so a stale or partial source never shadows a better one:
    /// 1. **Cache** — an already-loaded `PackageInfo` (its `exports` plus
    ///    `lazy_data` datasets, both of which `pkg::name` resolves). An *empty*
    ///    cached entry is NOT trusted: `get_package`/`prefetch_packages` cache an
    ///    empty-export placeholder when a package's namespace fails to load,
    ///    which is exactly why [`Self::package_exists`] ignores the cache. So an empty
    ///    entry falls through to disk rather than suppressing completions.
    /// 2. **Installed on disk** — when the package is in `lib_paths`, its
    ///    `NAMESPACE` is parsed directly for `export()` entries (no DESCRIPTION
    ///    read; `Depends` is irrelevant here). Datasets (under `data/`) and
    ///    `exportPattern()`-only packages (~6%) yield nothing statically and
    ///    fall through; both gaps close once the async `get_package` path has
    ///    run (e.g. via `prefetch_packages` on `did_open`) and Tier 1 hits.
    /// 3. **Metadata providers** — the repo / shipped package DB.
    ///
    /// For the caller, an empty result and `None` are equivalent (both suppress
    /// the popup), so this returns `None` rather than `Some(vec![])` when no
    /// tier yields exports. Names are sorted and de-duplicated.
    ///
    /// Blocking file reads on the request thread are acceptable here for the
    /// same reason file-path completion reads directories synchronously:
    /// NAMESPACE files are small and the completion handler runs off the main
    /// loop. Unlike `get_package`, this does not populate the cache — a package
    /// referenced only through `::` (never attached) is re-parsed each request;
    /// the reads are cheap and the alternative (a sync write path) is not worth
    /// the added locking.
    pub fn get_exports_sync(&self, name: &str) -> Option<Vec<String>> {
        // The load_all() sentinel is a synthetic package name, never a real
        // installed package; its internals are served through the overlay path,
        // never as `sentinel::name` completions.
        if is_load_all_sentinel(name) {
            return None;
        }

        // Tier 1: already-loaded cache snapshot (lock-free read). Trust only a
        // non-empty entry; an empty one may be a failed-load placeholder.
        if let Some(info) = self.packages.load().get(name) {
            let syms = sorted_unique_symbols(info.exports.iter().chain(info.lazy_data.iter()));
            if !syms.is_empty() {
                return Some(syms);
            }
        }

        // Tier 2: installed on disk — parse NAMESPACE for its explicit exports
        // (no DESCRIPTION read; `Depends` is irrelevant here).
        if let Some(pkg_dir) = self.find_package_directory(name) {
            let (exports, _has_pattern) = crate::namespace_parser::parse_namespace_explicit_exports(
                &pkg_dir.join("NAMESPACE"),
            );
            let syms = sorted_unique_symbols(exports.iter());
            if !syms.is_empty() {
                return Some(syms);
            }
        }

        // Tier 3: repo / shipped package DB.
        if let Some(info) = self.resolve_from_providers(name) {
            let syms = sorted_unique_symbols(info.exports.iter().chain(info.lazy_data.iter()));
            if !syms.is_empty() {
                return Some(syms);
            }
        }

        None
    }

    /// Synchronous, cache/provider-only `pkg::member` authority. Never spawns R,
    /// never touches disk, and never mutates the cache. `Absent` only from a
    /// `Complete` exports set. Data objects (`lazy_data`, the per-file
    /// `data_aliases` object names, and base exports) are positive-only: they
    /// confirm-present but never prove absent (issue #505 tightens this).
    ///
    /// Resolution is **cache-or-provider only** by design: a package referenced
    /// via `pkg::` is warmed into the cache in the background, and this authority
    /// reports `Unknown` (silent) until that warm completes and republishes — so
    /// it deliberately does NOT parse the on-disk NAMESPACE here. A synchronous
    /// disk parse would (a) block the keystroke path with file I/O per reference
    /// and (b) be unable to see datasets (which never appear in NAMESPACE
    /// `export()`), risking a false `Absent` on a real `pkg::dataset` before the
    /// warm populates the full metadata.
    pub fn namespace_member_status_sync(
        &self,
        package: &str,
        member: &str,
    ) -> NamespaceMemberStatus {
        if is_load_all_sentinel(package) {
            return NamespaceMemberStatus::Unknown;
        }
        // Base-package datasets/exports are merged into base_exports, not the
        // per-package lazy_data — positive-only present check.
        if self.is_base_package(package) && self.is_base_export(member) {
            return NamespaceMemberStatus::Present;
        }

        // A member is "present" if it is an exported symbol, a lazy-loaded data
        // object, OR an object bound by one of the package's data files (the
        // `data_aliases` VALUES — for a non-LazyData package the real dataset
        // names live only here, while `lazy_data` keeps file stems). All three
        // are positive-only signals.
        let member_present = |info: &PackageInfo| -> bool {
            info.exports.contains(member)
                || info.lazy_data.iter().any(|d| d == member)
                || info
                    .data_aliases
                    .values()
                    .any(|names| names.iter().any(|n| n == member))
        };

        // Tier 1: cache snapshot (already-stamped completeness).
        if let Some(info) = self.packages.load().get(package) {
            if member_present(info) {
                return NamespaceMemberStatus::Present;
            }
            match info.exports_completeness {
                MemberCompleteness::Complete => return NamespaceMemberStatus::Absent,
                MemberCompleteness::Partial => return NamespaceMemberStatus::Partial,
                MemberCompleteness::Unknown => {} // fall through to providers
            }
        }

        // Tier 2: providers (Complete export lists from Tier 2/3 metadata).
        if let Some(info) = self.resolve_from_providers(package) {
            if member_present(&info) {
                return NamespaceMemberStatus::Present;
            }
            // A provider record is authoritative for ABSENCE only when the
            // package is NOT installed locally (the CI / no-`.libPaths()` case).
            // When a local install exists, IT is the source of truth and may be
            // newer than the provider snapshot (e.g. a dev build exporting a
            // symbol the captured release lacked). Background warming resolves
            // the install into Tier 1; until then, defer rather than trust a
            // possibly-stale provider and risk a false `Absent` on a real local
            // export. A bare directory probe (no NAMESPACE parse) keeps this
            // off the keystroke hot path's heavy work.
            if self.find_package_directory(package).is_some() {
                return NamespaceMemberStatus::Unknown;
            }
            return match info.exports_completeness {
                MemberCompleteness::Complete => NamespaceMemberStatus::Absent,
                MemberCompleteness::Partial => NamespaceMemberStatus::Partial,
                MemberCompleteness::Unknown => NamespaceMemberStatus::Unknown,
            };
        }

        NamespaceMemberStatus::Unknown
    }

    /// Check if a symbol is exported by any of the given packages (synchronous, cached-only)
    ///
    /// This method checks if the symbol is exported by any of the loaded packages,
    /// using cached package information. It first checks base exports, then
    /// checks combined_entries cache (includes Depends/attached), then falls back
    /// to per-package exports cache.
    ///
    /// This is a synchronous method suitable for use in diagnostic collection where
    /// we cannot use async. It reads immutable cache snapshots, so an
    /// in-progress writer publication is never interpreted as absence.
    ///
    /// Returns true if:
    /// - The symbol is a base export, OR
    /// - The symbol is exported by any of the loaded packages (from cache)
    ///
    /// Returns false if:
    /// - The symbol is not found in base exports or any cached loaded package
    ///
    /// Requirements 8.1, 8.2: Check if symbol is exported by loaded packages at position
    pub fn is_symbol_from_loaded_packages(&self, symbol: &str, loaded_packages: &[String]) -> bool {
        // First check base exports (always available)
        if self.is_base_export(symbol) {
            return true;
        }

        // Workspace-local internals attached via `devtools::load_all()`. Short-
        // circuits on the sentinel NOT being in `loaded_packages`, so resolution
        // is byte-identical when no `load_all()` is in play.
        if self.overlay_has_symbol(symbol, loaded_packages) {
            return true;
        }

        // Check combined_entries cache first (includes Depends/attached packages)
        {
            let combined_cache = self.combined_entries.load();
            if !combined_cache.is_empty() {
                for pkg_name in loaded_packages {
                    if let Some(entry) = combined_cache.get(pkg_name)
                        && entry.exports.contains(symbol)
                    {
                        return true;
                    }
                }
            }
        }

        // Fall back to per-package exports cache
        let cache = self.packages.load();

        // Check each loaded package
        for pkg_name in loaded_packages {
            if let Some(info) = cache.get(pkg_name)
                && info.exports.contains(symbol)
            {
                return true;
            }
        }

        false
    }

    /// Get cached package info if available
    ///
    /// This is a synchronous method that only checks the cache.
    /// For loading packages that aren't cached, use `get_package()`.
    pub async fn get_cached_package(&self, name: &str) -> Option<Arc<PackageInfo>> {
        let cache = self.packages.load();
        cache.get(name).cloned()
    }

    /// Check if a package is cached
    pub async fn is_cached(&self, name: &str) -> bool {
        let cache = self.packages.load();
        cache.contains_key(name)
    }

    /// Return true when an attached package has no known export metadata source.
    ///
    /// This is intentionally about export-metadata availability, not install
    /// status. Installed packages, base packages, non-empty cached exports, and
    /// Tier 2/3 provider hits all mean Raven has enough information to avoid the
    /// targeted `raven check` metadata-missing warning. Empty cached exports can
    /// be the result of a failed lookup for a non-installed package, so they only
    /// suppress the warning when a provider also knows the package.
    pub async fn export_metadata_missing(&self, package: &str) -> bool {
        if self.package_exists(package) || self.is_base_package(package) {
            return false;
        }
        if let Some(cached) = self.get_cached_package(package).await
            && !cached.exports.is_empty()
        {
            return false;
        }
        self.resolve_from_providers(package).is_none()
    }

    /// Synchronous cache probe for use in hot diagnostic paths.
    ///
    /// Returns true when package metadata is currently cached, false otherwise.
    /// Reads the current immutable snapshot, so writer contention is never
    /// interpreted as absence.
    pub fn is_cached_sync(&self, name: &str) -> bool {
        self.packages.load().contains_key(name)
    }

    /// Synchronous lookup of the object names a `data()` file-stem binds in a
    /// package, for `data()` alias expansion in cross-file scope (issue #429).
    ///
    /// Returns the names in `package`'s `data_aliases[stem]` (e.g. survey's
    /// `api` → `["apiclus1", "apistrat", ...]`), or an empty `Vec` when the
    /// package is not cached, has no `data/` enumeration, or the stem is
    /// unknown. Reads the current immutable `packages` snapshot via ArcSwap
    /// `load()` (no promotion, no locking), matching the hot-path discipline of
    /// [`Self::is_cached_sync`] / [`Self::is_symbol_from_loaded_packages`].
    pub fn data_objects_for_stem_sync(&self, package: &str, stem: &str) -> Vec<String> {
        // The load_all sentinel is not an installed package and has no datasets.
        if is_load_all_sentinel(package) {
            return Vec::new();
        }
        let cache = self.packages.load();
        cache
            .get(package)
            .and_then(|info| info.data_aliases.get(stem))
            .cloned()
            .unwrap_or_default()
    }

    /// Get the number of cached packages
    pub async fn cached_count(&self) -> usize {
        let cache = self.packages.load();
        cache.len()
    }

    /// Insert a package into the cache
    ///
    /// This is primarily used for testing and initialization.
    pub async fn insert_package(&self, info: PackageInfo) {
        let Some(lease) = self.begin_cache_operation().await else {
            return;
        };
        self.insert_package_under_lease(info, &lease);
    }

    fn insert_package_under_lease(
        &self,
        mut info: PackageInfo,
        lease: &PackageCacheOperationLease<'_>,
    ) {
        self.update_packages(lease, |cache| {
            // Monotonic member authority: never let a weaker entry shadow a
            // `Complete` exports set. Upgrades (Unknown/Partial -> Complete) and
            // equal-or-stronger replacements publish as-is; a weaker incoming
            // entry keeps the existing `Complete` exports + completeness, and we
            // fold in (never drop) the existing positive-only data so a
            // re-insert can only add.
            if info.exports_completeness != MemberCompleteness::Complete
                && let Some(existing) = cache.get(&info.name)
                && existing.exports_completeness == MemberCompleteness::Complete
            {
                info.exports = existing.exports.clone();
                info.exports_completeness = existing.exports_completeness;
                for d in &existing.lazy_data {
                    if !info.lazy_data.contains(d) {
                        info.lazy_data.push(d.clone());
                    }
                }
                // Union per stem (not `or_insert_with`, which would keep only the
                // incoming list for a shared stem and drop the existing object
                // names — a silent narrowing that could make a real dataset
                // member falsely Absent). "Can only add" must hold per stem too.
                for (stem, names) in &existing.data_aliases {
                    let merged = info.data_aliases.entry(stem.clone()).or_default();
                    for n in names {
                        if !merged.contains(n) {
                            merged.push(n.clone());
                        }
                    }
                }
            }
            cache.insert(info.name.clone(), Arc::new(info));
        });
    }

    /// Invalidate cache for a package
    ///
    /// Removes the package from the cache, forcing it to be reloaded
    /// on the next access.
    pub async fn invalidate(&self, name: &str) {
        let names = HashSet::from([name.to_string()]);
        let _ = self.invalidate_many(&names).await;
    }

    /// Invalidate a batch of packages, also dropping any `combined_entries`
    /// entries whose aggregate export set depends on `names` — including:
    ///
    /// - direct key matches (invalidating `dplyr` drops combined entry for `dplyr`),
    /// - hardcoded meta-packages (`tidyverse`, `tidymodels`) whose `attached_packages`
    ///   intersect `names` (invalidating `dplyr` drops the cached `tidyverse` aggregate),
    /// - any package `A` whose transitive `depends`/`attached_packages` chain
    ///   reaches any name in `names` (invalidating `C` drops combined entry for
    ///   `A` when `A` Depends: `B` Depends: `C`), since the aggregate rolled up
    ///   a now-stale transitive child.
    ///
    /// Returns the set of `combined_entries` keys that were actually present and
    /// dropped. Callers use this to identify documents whose loaded packages
    /// include meta-aggregates that were invalidated even though the aggregate
    /// name itself is not in `names` (e.g. a document using `library(tidyverse)`
    /// when `dplyr` is installed).
    pub async fn invalidate_many(&self, names: &HashSet<String>) -> HashSet<String> {
        let Some(lease) = self.begin_cache_operation().await else {
            return HashSet::new();
        };
        self.invalidate_many_under_lease(names, &lease)
    }

    fn invalidate_many_under_lease(
        &self,
        names: &HashSet<String>,
        lease: &PackageCacheOperationLease<'_>,
    ) -> HashSet<String> {
        if names.is_empty() {
            return HashSet::new();
        }
        // Compute transitive dependents from the per-package cache BEFORE
        // mutating it so the dependent lookup sees the previous
        // `depends`/`attached_packages` graph.
        let dependent_combined_keys: HashSet<String> = {
            let cache = self.packages.load();
            // Worklist: start with the directly invalidated names, then
            // transitively find every cached package that depends on them.
            let mut frontier: HashSet<String> = names.clone();
            let mut dependents: HashSet<String> = HashSet::new();
            loop {
                let new_dependents: HashSet<String> = cache
                    .iter()
                    .filter(|(k, _)| {
                        !frontier.contains(k.as_str()) && !dependents.contains(k.as_str())
                    })
                    .filter(|(_, info)| {
                        info.depends.iter().any(|dep| frontier.contains(dep))
                            || info
                                .attached_packages
                                .iter()
                                .any(|dep| frontier.contains(dep))
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                if new_dependents.is_empty() {
                    break;
                }
                frontier = new_dependents.clone();
                dependents.extend(new_dependents);
            }
            dependents
        };
        self.update_packages(lease, |cache| {
            for n in names {
                cache.remove(n);
            }
        });
        let mut invalidated_combined: HashSet<String> = HashSet::new();
        self.update_combined_entries(lease, |combined| {
            // Drop direct hits; record only the ones that were actually present.
            for n in names {
                if combined.remove(n).is_some() {
                    invalidated_combined.insert(n.clone());
                }
            }
            // Drop entries for packages whose runtime Depends/attached set intersects `names`.
            for k in &dependent_combined_keys {
                if combined.remove(k).is_some() {
                    invalidated_combined.insert(k.clone());
                }
            }
            // Drop hardcoded meta-package aggregates whose attached set intersects `names`
            // (covers the case where the aggregate was cached but the child PackageInfo
            // was never loaded, so `dependent_combined_keys` above would miss it).
            let meta_hits: Vec<String> = combined
                .keys()
                .filter(|k| {
                    let attached = meta_attached_packages(k.as_str());
                    attached.iter().any(|p| names.contains(*p))
                })
                .cloned()
                .collect();
            for m in meta_hits {
                combined.remove(&m);
                invalidated_combined.insert(m);
            }
        });
        invalidated_combined
    }

    /// Snapshot of the keys currently in the per-package cache.
    pub async fn cached_package_names(&self) -> HashSet<String> {
        let cache = self.packages.load();
        cache.keys().cloned().collect()
    }

    /// Clear all cached packages, including aggregate `combined_entries`.
    pub async fn clear_cache(&self) {
        let Some(lease) = self.begin_cache_operation().await else {
            return;
        };
        self.clear_cache_under_lease(&lease);
    }

    fn clear_cache_under_lease(&self, lease: &PackageCacheOperationLease<'_>) {
        self.update_packages(lease, |cache| {
            cache.clear();
        });
        self.update_combined_entries(lease, |combined| {
            combined.clear();
        });
    }

    /// Load pattern packages via the INDEX + explicit-exports fallback and
    /// cache them. Shared by the two `prefetch_packages` branches that need it
    /// — no R subprocess available, and a batched R query that failed — which
    /// otherwise do byte-identical per-package work.
    ///
    /// `datasets_map` carries any dataset enumeration already fetched up front
    /// (issue #429). When the batched export query fails *after* a successful
    /// `get_multiple_package_datasets`, those results would otherwise be
    /// discarded; applying them here preserves each package's enumerated
    /// `lazy_data`/`data_aliases`. Entries are consumed via
    /// [`apply_enumeration_from`] (which `remove`s the matching key), so the
    /// `&mut` borrow is required. The no-R branch passes an empty map.
    async fn prefetch_pattern_packages_via_index(
        &self,
        pattern_packages: &[String],
        datasets_map: &mut HashMap<String, Vec<crate::r_subprocess::DataObject>>,
        lease: &PackageCacheOperationLease<'_>,
    ) {
        for pkg_name in pattern_packages {
            if let Some(pkg_dir) = self.find_package_directory(pkg_name)
                && let Some(parse_result) = self.parse_package_static(&pkg_dir)
            {
                let exports = self.load_with_index_fallback(&pkg_dir, &parse_result).await;
                let mut info = package_info_from_dir(
                    pkg_name.clone(),
                    &pkg_dir,
                    exports,
                    parse_result.depends,
                )
                .await;
                // INDEX fallback lists only documented topics, not every export.
                info.exports_completeness = MemberCompleteness::Partial;
                // Preserve any pre-fetched dataset enumeration for this package.
                apply_enumeration_from(datasets_map, pkg_name, &pkg_dir, &mut info);
                self.insert_package_under_lease(info, lease);
            }
        }
    }

    /// Prefetch packages by loading their exports into cache
    ///
    /// This method asynchronously loads package exports for the given package names,
    /// populating both the per-package cache and combined_entries cache.
    /// Used for background warm-up after detecting library() calls.
    ///
    /// # Performance - Tiered Prefetch Strategy
    ///
    /// 1. **Static packages (94%)**: Loaded immediately via NAMESPACE parsing (~1-5ms each)
    /// 2. **Pattern packages (6%)**: Batched into single R subprocess call (~100-500ms total)
    ///
    /// This approach eliminates R subprocess calls for most packages while still
    /// providing accurate exports for packages using `exportPattern()`.
    ///
    /// # Arguments
    /// * `packages` - List of package names to prefetch
    pub async fn prefetch_packages(&self, packages: &[String]) -> bool {
        if packages.is_empty() {
            return true;
        }
        let Some(lease) = self.begin_cache_operation().await else {
            return false;
        };
        self.prefetch_packages_under_lease(packages, &lease).await;
        true
    }

    async fn prefetch_packages_under_lease(
        &self,
        packages: &[String],
        lease: &PackageCacheOperationLease<'_>,
    ) {
        // Filter out packages we've already cached, and de-duplicate the
        // request. De-duplication is REQUIRED for correctness, not just
        // efficiency: a package appearing twice (e.g. two `library(dplyr)`
        // lines, which the warm-set builders pass through un-deduped) would be
        // processed twice by `prefetch_uncached_level`, and because
        // `apply_enumeration_from` *consumes* its `data()` enumeration entry
        // (`datasets_map.remove`), the second `insert_package` would overwrite
        // the first with an un-enumerated `PackageInfo` — silently dropping the
        // enumerated `data_aliases`/`lazy_data` (issue #429 regression). Keep
        // first-seen order so the dependency-closure BFS stays deterministic.
        let uncached_packages: Vec<String> = {
            let combined_cache = self.combined_entries.load();
            let packages_cache = self.packages.load();
            let filtered: Vec<String> = packages
                .iter()
                .filter(|p| !combined_cache.contains_key(*p) && !packages_cache.contains_key(*p))
                .cloned()
                .collect();
            dedup_preserving_order(filtered)
        };

        if uncached_packages.is_empty() {
            log::trace!("All {} packages already cached", packages.len());
            return;
        }

        // Issue #429: warm not just the requested packages but the transitive
        // dependency closure that `warm_all_exports` traverses, loading it one
        // dependency level at a time so each level enumerates datasets in a
        // SINGLE batched R call. Loading transitive deps lazily via
        // `get_package` during the recursion would otherwise spawn one `data()`
        // subprocess per data-bearing dependency (75-350ms each, serially) on
        // this synchronous did_open path. Discovery reads the just-cached
        // packages' `depends` (plus meta-package `attached_packages`), mirroring
        // `collect_exports_recursive`'s traversal, and skips anything already
        // scheduled or cached so cycles terminate.
        let mut scheduled: HashSet<String> = uncached_packages.iter().cloned().collect();
        let mut frontier: Vec<String> = uncached_packages.clone();
        while !frontier.is_empty() {
            self.prefetch_uncached_level(&frontier, lease).await;

            let mut next: Vec<String> = Vec::new();
            {
                let cache = self.packages.load();
                for pkg_name in &frontier {
                    if let Some(info) = cache.get(pkg_name) {
                        let mut deps: Vec<String> = info.depends.clone();
                        if info.is_meta_package {
                            deps.extend(info.attached_packages.iter().cloned());
                        }
                        for dep in deps {
                            if !scheduled.contains(&dep) && !cache.contains_key(&dep) {
                                scheduled.insert(dep.clone());
                                next.push(dep);
                            }
                        }
                    }
                }
            }
            frontier = next;
        }

        // Finally, populate the combined_entries cache for the requested
        // packages. Everything the recursive warm touches is already cached by
        // the level loop above, so this performs no further R subprocess spawns.
        for pkg_name in &uncached_packages {
            let _ = self.warm_all_exports_under_lease(pkg_name, lease).await;
        }
    }

    /// Load one level of uncached packages into the per-package cache: parse the
    /// static (no-`exportPattern`) packages, batch the `exportPattern` packages
    /// into a single R export call, and enumerate every data-bearing package's
    /// `data()` objects in one batched R call (issue #429). Does NOT recurse
    /// into dependencies or populate `combined_entries` — [`Self::prefetch_packages`]
    /// drives the transitive closure and the combined warm-up.
    async fn prefetch_uncached_level(
        &self,
        uncached_packages: &[String],
        lease: &PackageCacheOperationLease<'_>,
    ) {
        // Categorize packages: static (no exportPattern) vs pattern (needs R)
        let mut static_packages: Vec<(String, PathBuf, NamespaceParseResult)> = Vec::new();
        let mut pattern_packages: Vec<String> = Vec::new();

        for pkg_name in uncached_packages {
            if let Some(pkg_dir) = self.find_package_directory(pkg_name) {
                if let Some(parse_result) = self.parse_package_static(&pkg_dir) {
                    if parse_result.has_export_pattern {
                        pattern_packages.push(pkg_name.clone());
                    } else {
                        static_packages.push((pkg_name.clone(), pkg_dir, parse_result));
                    }
                }
            } else {
                // Filesystem lookup may miss packages in some environments; probe
                // these directly through the R subprocess batch path.
                pattern_packages.push(pkg_name.clone());
            }
        }

        log::trace!(
            "Prefetching {} packages: {} static, {} pattern",
            uncached_packages.len(),
            static_packages.len(),
            pattern_packages.len()
        );

        // Issue #429: one batched dataset enumeration call for all packages
        // that have a `data/` dir (both static and pattern paths below use it).
        // Each R spawn is 75-350ms, so we batch once up front.
        let mut datasets_map: HashMap<String, Vec<crate::r_subprocess::DataObject>> =
            HashMap::new();
        // Whether the batched R dataset enumeration ran successfully this round.
        // A `data/`-bearing static package is only absence-authoritative when its
        // datasets were enumerated (a binary `data/Rdata.rdb` exposes no object
        // names to the static parse); otherwise it stays `Partial`.
        let mut datasets_batch_ok = false;
        if let Some(ref r_subprocess) = self.r_subprocess {
            // Collect names of packages with a data/ dir from both categories.
            let mut data_pkgs: Vec<String> = Vec::new();
            for (name, pkg_dir, _) in &static_packages {
                if pkg_dir.join("data").is_dir() {
                    data_pkgs.push(name.clone());
                }
            }
            for pkg_name in &pattern_packages {
                if let Some(pkg_dir) = self.find_package_directory(pkg_name)
                    && pkg_dir.join("data").is_dir()
                {
                    data_pkgs.push(pkg_name.clone());
                }
            }
            if !data_pkgs.is_empty() {
                match r_subprocess.get_multiple_package_datasets(&data_pkgs).await {
                    Ok(map) => {
                        log::trace!(
                            "Batched dataset enumeration returned {} entries for {} packages",
                            map.values().map(|v| v.len()).sum::<usize>(),
                            map.len()
                        );
                        datasets_map = map;
                        datasets_batch_ok = true;
                    }
                    Err(e) => {
                        log::trace!(
                            "Batched dataset() enumeration failed: {} (static fallback for all)",
                            e
                        );
                    }
                }
            }
        }

        // Step 1: Load static packages immediately (no R subprocess needed)
        for (name, pkg_dir, parse_result) in static_packages {
            let exports: HashSet<String> = parse_result.explicit_exports.into_iter().collect();
            let mut info =
                package_info_from_dir(name.clone(), &pkg_dir, exports, parse_result.depends).await;
            // `static_packages` holds only non-`exportPattern` packages, so the
            // statically-parsed EXPORT set is exhaustive. But if the package
            // ships a `data/` dir whose datasets were not enumerated via R, its
            // dataset names are unknown to the static parse, so it must not be
            // absence-authoritative — stay `Partial` (datasets are positive-only).
            info.exports_completeness = if pkg_dir.join("data").is_dir() && !datasets_batch_ok {
                MemberCompleteness::Partial
            } else {
                MemberCompleteness::Complete
            };

            // Apply dataset enumeration if available for this package.
            apply_enumeration_from(&mut datasets_map, &name, &pkg_dir, &mut info);

            log::trace!(
                "Loaded {} exports + {} datasets for package '{}' statically",
                info.exports.len(),
                info.lazy_data.len(),
                name
            );

            self.insert_package_under_lease(info, lease);
        }

        // Step 2: Batch R subprocess call only for pattern packages
        if !pattern_packages.is_empty() {
            if let Some(ref r_subprocess) = self.r_subprocess {
                log::trace!(
                    "Batching R subprocess call for {} pattern packages",
                    pattern_packages.len()
                );

                match r_subprocess
                    .get_multiple_package_exports(&pattern_packages)
                    .await
                {
                    Ok(exports_map) => {
                        // Populate the per-package cache with the R subprocess results
                        for (pkg_name, exports) in exports_map {
                            let exports_set: HashSet<String> = exports.into_iter().collect();
                            // Depends and datasets come from the on-disk dir: the
                            // batched R export result carries neither. Datasets
                            // live under `data/`, not in `getNamespaceExports()`.
                            let info = match self.find_package_directory(&pkg_name) {
                                Some(pkg_dir) => {
                                    let depends =
                                        parse_description_depends(&pkg_dir.join("DESCRIPTION"))
                                            .unwrap_or_default();
                                    let mut i = package_info_from_dir(
                                        pkg_name.clone(),
                                        &pkg_dir,
                                        exports_set,
                                        depends,
                                    )
                                    .await;
                                    // Exports came from R's getNamespaceExports —
                                    // an exhaustive enumeration.
                                    i.exports_completeness = MemberCompleteness::Complete;
                                    // Apply dataset enumeration if available.
                                    apply_enumeration_from(
                                        &mut datasets_map,
                                        &pkg_name,
                                        &pkg_dir,
                                        &mut i,
                                    );
                                    i
                                }
                                None => {
                                    // No on-disk directory for this package. When R
                                    // also returned no exports (the tryCatch swallowed
                                    // the asNamespace error and produced an empty
                                    // list), caching an empty PackageInfo would shadow
                                    // any Tier 2/3 provider result via the cache-first
                                    // check in get_package. So on an empty set, consult
                                    // the ordered fallback providers first; only fall
                                    // back to the (empty) R-derived set if none knows it.
                                    let from_provider = if exports_set.is_empty() {
                                        self.resolve_from_providers(&pkg_name)
                                    } else {
                                        None
                                    };
                                    from_provider.unwrap_or_else(|| {
                                        let mut i = PackageInfo::with_details(
                                            pkg_name.clone(),
                                            exports_set,
                                            Vec::new(),
                                            Vec::new(),
                                        );
                                        // A non-empty R `getNamespaceExports()`
                                        // result is authoritative → Complete; an
                                        // empty one is a swallowed error and stays
                                        // Unknown (never absence-authoritative).
                                        if !i.exports.is_empty() {
                                            i.exports_completeness = MemberCompleteness::Complete;
                                        }
                                        i
                                    })
                                }
                            };
                            log::trace!(
                                "Cached {} exports + {} datasets for pattern package '{}' from R",
                                info.exports.len(),
                                info.lazy_data.len(),
                                pkg_name
                            );
                            self.insert_package_under_lease(info, lease);
                        }
                    }
                    Err(e) => {
                        log::trace!(
                            "Batch R call failed: {}, falling back to INDEX for pattern packages",
                            e
                        );

                        // Fall back to INDEX + explicit exports for pattern
                        // packages, preserving any dataset enumeration already
                        // fetched into `datasets_map` (issue #429).
                        self.prefetch_pattern_packages_via_index(
                            &pattern_packages,
                            &mut datasets_map,
                            lease,
                        )
                        .await;
                    }
                }
            } else {
                // No R subprocess - use INDEX fallback for pattern packages
                log::trace!(
                    "No R subprocess, using INDEX fallback for {} pattern packages",
                    pattern_packages.len()
                );

                self.prefetch_pattern_packages_via_index(
                    &pattern_packages,
                    &mut datasets_map,
                    lease,
                )
                .await;
            }
        }
    }

    /// Add additional library paths (deduplicating)
    ///
    /// This method adds paths to the library search paths, avoiding duplicates.
    /// Used to apply user-configured additional library paths.
    pub fn add_library_paths(&mut self, paths: &[PathBuf]) {
        for path in paths {
            if !self.lib_paths.contains(path) {
                self.lib_paths.push(path.clone());
            }
        }
    }

    /// Per-package fallback shared by [`Self::find_package_for_symbol`] and
    /// [`Self::find_package_owner_for_symbol`]: returns the first loaded package whose
    /// own (non-aggregate) cached exports contain `symbol`. Centralizing the loop
    /// keeps availability and owner attribution from diverging on direct-package
    /// lookup. Returns `None` if no loaded package exports the symbol.
    fn find_in_per_package_cache(
        &self,
        symbol: &str,
        loaded_packages: &[String],
    ) -> Option<String> {
        let cache = self.packages.load();
        for pkg_name in loaded_packages {
            if let Some(info) = cache.get(pkg_name)
                && info.exports.contains(symbol)
            {
                return Some(pkg_name.clone());
            }
        }
        None
    }

    /// Find which package exports a symbol (synchronous, cached-only)
    ///
    /// Searches through loaded packages to find which one exports the given symbol.
    /// Returns the first package name that exports the symbol, or None.
    pub fn find_package_for_symbol(
        &self,
        symbol: &str,
        loaded_packages: &[String],
    ) -> Option<String> {
        // Check combined_entries cache first
        {
            let cache = self.combined_entries.load();
            if !cache.is_empty() {
                for pkg_name in loaded_packages {
                    if let Some(entry) = cache.get(pkg_name)
                        && entry.exports.contains(symbol)
                    {
                        return Some(pkg_name.clone());
                    }
                }
            }
        }

        // Fall back to per-package cache
        self.find_in_per_package_cache(symbol, loaded_packages)
    }

    /// Find the true owner package of a symbol made visible by `loaded_packages`
    /// (synchronous, cached-only).
    ///
    /// Distinct from [`Self::find_package_for_symbol`], which answers "which loaded
    /// package made this visible?" and returns the aggregate key (e.g.
    /// `tidyverse`). This answers "which package actually contributed the
    /// symbol?" — the documentation / NSE-policy owner (e.g. `dplyr`) — by
    /// consulting the per-aggregate `CombinedEntry` snapshot. Falls back to
    /// direct per-package exports when no aggregate entry exists yet, preserving
    /// the previous behavior for unwarmed direct package caches. See issue #407.
    pub fn find_package_owner_for_symbol(
        &self,
        symbol: &str,
        loaded_packages: &[String],
    ) -> Option<String> {
        // Workspace-local internals attached via `devtools::load_all()` are owned
        // by the synthetic sentinel package. Short-circuits unless the sentinel is
        // attached, so attribution is unchanged when no `load_all()` is in play.
        if self.overlay_has_symbol(symbol, loaded_packages) {
            return Some(LOAD_ALL_SENTINEL.to_string());
        }

        // Consult the per-aggregate snapshot first. For each loaded package, if
        // the cached aggregate entry records this symbol's owner, that value is
        // the true contributor (e.g. `dplyr` for a `mutate` made visible through
        // `tidyverse`). If the entry says the symbol is available but lacks an
        // owner, fail closed rather than falling back to aggregate attribution.
        {
            let cache = self.combined_entries.load();
            if !cache.is_empty() {
                for pkg_name in loaded_packages {
                    if let Some(entry) = cache.get(pkg_name) {
                        if let Some(owner) = entry.owners.get(symbol) {
                            return Some(owner.clone());
                        }
                        if entry.exports.contains(symbol) {
                            return None;
                        }
                    }
                }
            }
        }

        // Fall back only to direct per-package attribution. Re-reading aggregate
        // availability here can combine an old owner miss with a newer combined
        // hit and return the aggregate package as a false owner.
        self.find_in_per_package_cache(symbol, loaded_packages)
    }

    /// Resolve the true owner package of `symbol` from the durable
    /// default-attached owner map.
    ///
    /// Default-attached base packages are seeded into the cross-file scope under
    /// the synthetic `package:base` URI by `seed_base_exports`, so symbols like
    /// `ave` need this map to recover their real owner (`stats`) for hover help
    /// and `from {pkg}` attribution. The map is built by
    /// `recompute_default_attached_owners` during [`Self::initialize`] and is not
    /// cleared by [`Self::clear_cache`], matching `base_exports` durability. This
    /// keeps `raven.refreshPackages` from reverting symbols like `ave` to {base}
    /// after the clearable per-package cache is wiped. See issue #592.
    pub fn resolve_default_attached_owner(&self, symbol: &str) -> String {
        self.default_attached_owners
            .get(symbol)
            .cloned()
            .unwrap_or_else(|| "base".to_string())
    }

    /// Rebuild [`Self::default_attached_owners`] from the current per-package
    /// `packages` cache, over the default-attached `base_packages`.
    ///
    /// Processes `base` first so it wins name collisions (a symbol base genuinely
    /// exports stays attributed to base), then the other packages in byte/ASCII
    /// `String` order (uppercase sorts before lowercase, so e.g. `grDevices`
    /// precedes `graphics`) for deterministic — not human-alphabetical —
    /// resolution of any cross-package collision. Called at the end of
    /// [`Self::initialize`] once `packages` is fully populated. First-writer-wins
    /// via `entry().or_insert`.
    fn recompute_default_attached_owners(&mut self) {
        let cache = self.packages.load();
        let mut ordered: Vec<String> = self.base_packages.iter().cloned().collect();
        ordered.sort_by(|a, b| match (a.as_str() == "base", b.as_str() == "base") {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        });

        let mut owners: HashMap<String, String> = HashMap::new();
        for pkg in &ordered {
            if let Some(info) = cache.get(pkg) {
                for export in &info.exports {
                    owners.entry(export.clone()).or_insert_with(|| pkg.clone());
                }
            }
        }
        self.default_attached_owners = Arc::new(owners);
    }

    /// Set the library paths
    ///
    /// This is used during initialization to set the library paths
    /// discovered from R or configuration.
    pub fn set_lib_paths(&mut self, paths: Vec<PathBuf>) {
        self.lib_paths = paths;
    }

    /// Set the base packages
    ///
    /// This is used during initialization to set the base packages
    /// discovered from R or fallback values.
    pub fn set_base_packages(&mut self, packages: HashSet<String>) {
        self.base_packages = packages;
    }

    /// Set the base exports
    ///
    /// This is used during initialization to set the combined exports
    /// from all base packages.
    pub fn set_base_exports(&mut self, exports: HashSet<String>) {
        self.base_exports = Arc::new(exports);
    }

    /// Parse a package's NAMESPACE and DESCRIPTION files statically.
    ///
    /// This method extracts exports from the NAMESPACE file and dependencies from
    /// the DESCRIPTION file without calling R. It detects whether the package uses
    /// `exportPattern()` directives that would require R subprocess for accurate results.
    ///
    /// Special case: The `base` package doesn't have a NAMESPACE file. For such packages,
    /// we treat them as having `exportPattern()` (needing R or INDEX fallback).
    ///
    /// # Returns
    /// - `Some(NamespaceParseResult)` with exports, pattern flag, and dependencies
    /// - `None` if the package directory doesn't exist
    pub fn parse_package_static(&self, pkg_dir: &Path) -> Option<NamespaceParseResult> {
        // A non-existent package directory yields nothing to parse (see contract
        // above). Callers normally pass a directory from `find_package_directory`
        // (which guarantees existence), so this guards only direct/foreign calls.
        if !pkg_dir.is_dir() {
            return None;
        }

        // Explicit exports + pattern flag from NAMESPACE (a missing NAMESPACE,
        // e.g. `base`, yields no explicit exports and the pattern flag → INDEX
        // fallback). Shared with the sync `pkg::` completion path.
        let (explicit_exports, has_export_pattern) =
            crate::namespace_parser::parse_namespace_explicit_exports(&pkg_dir.join("NAMESPACE"));

        // Parse DESCRIPTION for Depends
        let depends = parse_description_depends(&pkg_dir.join("DESCRIPTION")).unwrap_or_default();

        Some(NamespaceParseResult {
            explicit_exports,
            has_export_pattern,
            depends,
        })
    }

    /// Load package exports using INDEX file as fallback for pattern packages.
    ///
    /// When a package uses `exportPattern()` and R subprocess is unavailable,
    /// this method combines explicit exports from NAMESPACE with documented
    /// exports from the INDEX file.
    async fn load_with_index_fallback(
        &self,
        pkg_dir: &Path,
        parse_result: &NamespaceParseResult,
    ) -> HashSet<String> {
        let mut exports: HashSet<String> = parse_result.explicit_exports.iter().cloned().collect();

        // Add INDEX exports for pattern packages
        if let Ok(index_exports) = parse_index_exports(pkg_dir).await {
            log::trace!(
                "Loaded {} exports from INDEX file for pattern package",
                index_exports.len()
            );
            exports.extend(index_exports);
        }

        exports
    }

    /// Check if a symbol is exported by a specific package
    ///
    /// Returns true if the package is cached and exports the symbol.
    /// Returns false if the package is not cached or doesn't export the symbol.
    pub async fn is_package_export(&self, symbol: &str, package: &str) -> bool {
        if let Some(info) = self.get_cached_package(package).await {
            info.exports.contains(symbol)
        } else {
            false
        }
    }

    /// Create a new PackageLibrary and initialize it with R subprocess query or fallback
    ///
    /// This is a convenience constructor that creates a PackageLibrary with the
    /// given R subprocess and immediately initializes it.
    ///
    /// # Arguments
    /// * `r_subprocess` - Optional R subprocess interface for querying package info
    ///
    /// # Returns
    /// An initialized PackageLibrary with lib_paths, base_packages, and base_exports populated
    ///
    /// Requirement 6.1, 6.2, 6.3, 7.1
    pub async fn new(r_subprocess: Option<RSubprocess>) -> Self {
        let mut lib = Self::with_subprocess(r_subprocess);
        if let Err(e) = lib.initialize().await {
            log::trace!("Failed to initialize PackageLibrary: {}", e);
            // Continue with empty/fallback state - the library is still usable
        }
        lib
    }

    /// Initialize the PackageLibrary with R subprocess query or fallback
    ///
    /// This method queries R for lib_paths and base_packages, falling back to
    /// hardcoded values if R is unavailable. It also pre-populates base_exports
    /// by querying exports for each base package.
    ///
    /// # Behavior
    /// 1. Query R for library paths using `.libPaths()`, or use platform-specific fallbacks
    /// 2. Query R for base packages using `.packages()`, or use hardcoded fallback list
    /// 3. For each base package, query its exports and combine into base_exports
    /// 4. If any query fails, gracefully fall back to defaults and continue
    ///
    /// Requirement 6.1: THE LSP SHALL query R subprocess at initialization to get
    /// the default search path using `.packages()`
    ///
    /// Requirement 6.2: IF R subprocess is unavailable at initialization, THE LSP
    /// SHALL use a hardcoded list of base packages
    ///
    /// Requirement 6.3: THE Base_Packages SHALL be available at all positions in
    /// all files without requiring explicit library() calls
    ///
    /// Requirement 7.1: THE LSP SHALL query R subprocess to get library paths
    /// using `.libPaths()`
    pub async fn initialize(&mut self) -> anyhow::Result<()> {
        // Strategy: Try static initialization first, then fall back to R subprocess
        //
        // Static initialization is preferred because:
        // 1. Much faster (~50ms vs ~100ms+ for R subprocess)
        // 2. No dependency on R being available
        // 3. Works offline
        //
        // R subprocess is used as fallback for:
        // 1. Getting lib_paths if platform fallbacks fail
        // 2. Pattern packages (base R uses exportPattern)

        // Step 1: Get library paths (try R first for accuracy, then fallback).
        // If a caller pre-populated `lib_paths` via `set_lib_paths` (e.g. for
        // tests, or future explicit configuration), respect that and skip
        // rediscovery — overwriting their choice would be surprising.
        if self.lib_paths.is_empty() {
            self.lib_paths = self.get_lib_paths_with_fallback().await?;
        }

        if self.lib_paths.is_empty() {
            log::trace!("Warning: No library paths found, package loading will fail");
        }

        // Step 2: Use hardcoded base packages list (always reliable). Only
        // the default-attached seven seed `base_exports`; the full
        // base-priority set still needs per-package cache entries so
        // `library(grid)` / `library(tools)` resolve synchronously.
        let attached_base_packages: HashSet<String> =
            crate::r_subprocess::get_fallback_base_packages()
                .into_iter()
                .collect();
        self.base_packages = attached_base_packages.clone();
        let base_priority_packages = crate::r_subprocess::get_base_priority_packages();

        // Step 3: Load base package exports statically with INDEX fallback
        // Base packages use exportPattern, but INDEX file provides documented exports
        // Track per-package exports for completion attribution (e.g., {base}, {utils})
        let mut all_base_exports = HashSet::new();
        let mut per_package_exports: HashMap<String, HashSet<String>> = HashMap::new();
        let mut per_package_depends: HashMap<String, Vec<String>> = HashMap::new();
        let mut pattern_packages: Vec<String> = Vec::new();

        for package in &base_priority_packages {
            if let Some(pkg_dir) = self.find_package_directory(package)
                && let Some(parse_result) = self.parse_package_static(&pkg_dir)
            {
                let is_attached_base = attached_base_packages.contains(package);
                let pkg_exports = per_package_exports.entry(package.clone()).or_default();
                // Preserve depends from DESCRIPTION for transitive dependency resolution
                if !parse_result.depends.is_empty() {
                    per_package_depends.insert(package.clone(), parse_result.depends.clone());
                }
                // Add explicit exports
                for export in &parse_result.explicit_exports {
                    if is_attached_base {
                        all_base_exports.insert(export.clone());
                    }
                    pkg_exports.insert(export.clone());
                }

                if parse_result.has_export_pattern {
                    // Base packages use exportPattern - add INDEX exports and track for R fallback
                    let index_exports =
                        self.load_with_index_fallback(&pkg_dir, &parse_result).await;
                    for export in &index_exports {
                        if is_attached_base {
                            all_base_exports.insert(export.clone());
                        }
                        pkg_exports.insert(export.clone());
                    }
                    pattern_packages.push(package.clone());
                }

                // Step 3b: Pick up data objects auto-attached at startup
                // (issue #276). Lazy-loaded base packages like `datasets`
                // expose `mtcars`/`iris`/... without listing them in
                // NAMESPACE export() or `getNamespaceExports()`. Walk
                // `data/` for individual files and fall back to INDEX
                // topics when the data is bundled into `Rdata.r{db,dx,ds}`.
                //
                // Use `symlink_metadata` (not `is_dir`, which traverses
                // symlinks) for consistency with `parse_data_symbols`'s
                // own rejection of symlinked `data/` trees.
                let has_real_data_dir = std::fs::symlink_metadata(pkg_dir.join("data"))
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
                if has_real_data_dir {
                    for sym in parse_data_symbols(&pkg_dir).await {
                        if is_attached_base {
                            all_base_exports.insert(sym.clone());
                        }
                        pkg_exports.insert(sym);
                    }
                    // INDEX entries are documented topic names — for
                    // lazy-loaded data packages these correspond to
                    // top-level dataset names (mtcars, iris, ...). Only
                    // applied here when has_export_pattern is false so we
                    // don't double-merge with the existing fallback above.
                    if !parse_result.has_export_pattern
                        && let Ok(index_exports) = parse_index_exports(&pkg_dir).await
                    {
                        for export in &index_exports {
                            if is_attached_base {
                                all_base_exports.insert(export.clone());
                            }
                            pkg_exports.insert(export.clone());
                        }
                    }
                }

                // Step 3c: Embedded dataset floor (issue #276). INDEX entries
                // are help *topics*, and a multi-object topic hides the
                // individual objects it documents (`state` covers state.x77 /
                // state.region / ..., `stackloss` covers stack.x / stack.loss).
                // The embedded base table shipped with Raven carries the
                // accurate per-object list, so union it in as a floor; the
                // disk-derived names above still merge on top.
                if let Some(embedded) = crate::package_db::embedded_base::packages()
                    .iter()
                    .find(|p| p.name == *package && !p.datasets.is_empty())
                {
                    for sym in embedded.datasets {
                        if is_attached_base {
                            all_base_exports.insert((*sym).to_string());
                        }
                        pkg_exports.insert((*sym).to_string());
                    }
                }
            }
        }

        // Packages whose NAMESPACE uses `exportPattern` — their disk-derived
        // export set is the INDEX approximation (Partial) UNLESS R's
        // `getNamespaceExports()` enumerates them authoritatively in Step 4.
        let pattern_set: HashSet<String> = pattern_packages.iter().cloned().collect();
        // Pattern packages for which R returned a non-empty authoritative export
        // set this run. Used in Step 5 to stamp `Complete` vs `Partial`.
        let mut r_resolved: HashSet<String> = HashSet::new();

        // Step 4: If R subprocess is available and we have pattern packages,
        // try to get accurate exports for them
        if !pattern_packages.is_empty()
            && let Some(ref r_subprocess) = self.r_subprocess
        {
            log::trace!(
                "Querying R for {} base packages with exportPattern",
                pattern_packages.len()
            );
            match r_subprocess
                .get_multiple_package_exports(&pattern_packages)
                .await
            {
                Ok(exports_map) => {
                    for (pkg_name, exports) in exports_map {
                        // A non-empty R enumeration is authoritative; an empty
                        // one (e.g. a swallowed asNamespace error) is not, so it
                        // leaves the package on the Partial INDEX fallback.
                        if !exports.is_empty() {
                            r_resolved.insert(pkg_name.clone());
                        }
                        let is_attached_base = attached_base_packages.contains(&pkg_name);
                        let pkg_exports = per_package_exports.entry(pkg_name).or_default();
                        for export in exports {
                            if is_attached_base {
                                all_base_exports.insert(export.clone());
                            }
                            pkg_exports.insert(export);
                        }
                    }
                }
                Err(e) => {
                    if e.downcast_ref::<crate::r_subprocess::OuterDeadlineExpired>()
                        .is_some()
                    {
                        return Err(e);
                    }
                    log::trace!(
                        "R batch query for base packages failed: {}, using INDEX fallback",
                        e
                    );
                    // Continue with INDEX-based exports
                }
            }
        }

        // CI/runtime fallback: with no base exports found on disk, load the
        // embedded base-priority table. All 14 packages populate the per-package
        // cache (datasets → lazy_data) so `library(grid)` etc. resolve offline,
        // but only the 7 default-attached packages seed the flat always-in-scope
        // set + base_packages (the others require an explicit library() call).
        // A non-empty disk merge (a real install) always wins and skips this
        // entirely. No sidecar, so initialize() never depends on names.db and
        // the startup ordering problem is gone (ADR 1).
        if all_base_exports.is_empty() {
            let attached: HashSet<String> = crate::r_subprocess::get_fallback_base_packages()
                .into_iter()
                .collect();
            for p in crate::package_db::embedded_base::packages() {
                if attached.contains(p.name) {
                    self.base_packages.insert(p.name.to_string());
                    for s in p.exports.iter().chain(p.datasets.iter()) {
                        all_base_exports.insert(s.to_string());
                    }
                }
                // A base-priority package that WAS found on disk owns its cache
                // entry — Step 5 inserts it with the correct (possibly `Partial`,
                // when an `exportPattern` package fell back to INDEX without R)
                // completeness. The embedded floor must not pre-seed a `Complete`
                // entry that the monotonic `insert_package` guard would then keep,
                // which would make INDEX-only exports falsely absence-authoritative.
                // Disk data wins; the embedded floor only covers absent packages.
                if per_package_exports.contains_key(p.name) {
                    continue;
                }
                let mut info = PackageInfo::with_details(
                    p.name.to_string(),
                    p.exports.iter().map(|s| s.to_string()).collect(),
                    p.depends.iter().map(|s| s.to_string()).collect(),
                    p.datasets.iter().map(|s| s.to_string()).collect(),
                );
                // The embedded base table ships the full export list for each
                // base-priority package.
                info.exports_completeness = MemberCompleteness::Complete;
                self.insert_package(info).await;
            }
        }

        // Step 5: Store per-package exports in the packages cache for completion attribution
        // This allows get_exports_for_completions() to find base packages with correct
        // package names (e.g., {base}, {utils}, {stats}).
        // Preserve depends so get_all_exports() can follow transitive dependencies.
        for (pkg_name, exports) in per_package_exports {
            let depends = per_package_depends.remove(&pkg_name).unwrap_or_default();
            // `lazy_data` is intentionally empty here: these are base packages,
            // whose datasets are merged into `base_exports` above (issue #276) —
            // so this deliberately does NOT route through `package_info_from_dir`.
            // A base package whose NAMESPACE uses `exportPattern` is only
            // `Complete` when R enumerated it this run; otherwise its export set
            // is the INDEX approximation, so it stays `Partial` and can never
            // drive a false `namespace-member-not-found`. Non-pattern base
            // packages parse exhaustively from NAMESPACE → `Complete`. (Member
            // presence is additionally guarded by the positive-only
            // `base_exports` check in `namespace_member_status_sync`.)
            let completeness = if pattern_set.contains(&pkg_name) && !r_resolved.contains(&pkg_name)
            {
                MemberCompleteness::Partial
            } else {
                MemberCompleteness::Complete
            };
            let mut info = PackageInfo::with_details(pkg_name, exports, depends, Vec::new());
            info.exports_completeness = completeness;
            self.insert_package(info).await;
        }

        log::trace!(
            "Initialized PackageLibrary: {} lib_paths, {} base_packages, {} base_exports",
            self.lib_paths.len(),
            self.base_packages.len(),
            all_base_exports.len()
        );

        self.base_exports = Arc::new(all_base_exports);
        self.recompute_default_attached_owners();
        Ok(())
    }

    /// Get library paths with fallback strategy
    async fn get_lib_paths_with_fallback(&self) -> anyhow::Result<Vec<PathBuf>> {
        // Try R subprocess first (most accurate)
        if let Some(ref r_subprocess) = self.r_subprocess {
            match r_subprocess.get_lib_paths().await {
                Ok(paths) if !paths.is_empty() => {
                    log::trace!("Got {} library paths from R", paths.len());
                    return Ok(paths);
                }
                Ok(_) => {
                    log::trace!("R returned empty lib_paths, using fallback");
                }
                Err(e) => {
                    if e.downcast_ref::<crate::r_subprocess::OuterDeadlineExpired>()
                        .is_some()
                    {
                        return Err(e);
                    }
                    log::trace!("Failed to get lib_paths from R: {}, using fallback", e);
                }
            }
        }

        // Use platform-specific fallback
        let fallback = crate::r_subprocess::get_fallback_lib_paths();
        log::trace!("Using fallback library paths: {:?}", fallback);
        Ok(fallback)
    }
    /// Get package info using tiered loading strategy
    ///
    /// # Tiered Loading Strategy
    ///
    /// 1. **Cache check**: Return immediately if package is cached
    /// 2. **Tier 1 - Static parsing**: Parse NAMESPACE/DESCRIPTION files (~1-5ms)
    ///    - If no `exportPattern()` directives: Use static exports directly (94% of packages)
    /// 3. **Tier 2 - R subprocess**: Only for packages with `exportPattern()` (~6%)
    ///    - Call R subprocess for accurate pattern expansion (~100-300ms)
    /// 4. **Tier 3 - INDEX fallback**: If R fails or unavailable
    ///    - Combine explicit exports with INDEX file (~95% accuracy)
    ///
    /// This approach eliminates R subprocess calls for 94% of packages while
    /// maintaining 100% accuracy for packages using explicit exports.
    ///
    /// Requirement 3.1: WHEN a package is loaded, THE Package_Resolver SHALL query R subprocess
    /// to get the package's exported symbols using `getNamespaceExports()` (for pattern packages)
    ///
    /// Requirement 3.2: IF R subprocess is unavailable, THE Package_Resolver SHALL fall back
    /// to parsing the package's NAMESPACE file directly
    ///
    /// Requirement 4.3: WHEN the package is `tidyverse`, THE Package_Resolver SHALL also load
    /// exports from: dplyr, readr, forcats, stringr, ggplot2, tibble, lubridate, tidyr, purrr
    ///
    /// Requirement 4.4: WHEN the package is `tidymodels`, THE Package_Resolver SHALL also load
    /// exports from the tidymodels packages
    pub async fn get_package(&self, name: &str) -> Option<Arc<PackageInfo>> {
        let lease = self.begin_cache_operation().await?;
        self.get_package_under_lease(name, &lease).await
    }

    async fn get_package_under_lease(
        &self,
        name: &str,
        lease: &PackageCacheOperationLease<'_>,
    ) -> Option<Arc<PackageInfo>> {
        // Step 1: Check cache first
        if let Some(cached) = self.get_cached_package(name).await {
            log::trace!("Package '{}' found in cache", name);
            return Some(cached);
        }

        log::trace!("Package '{}' not in cache, attempting to load", name);

        // Step 2: Find package directory
        let pkg_dir = match self.find_package_directory(name) {
            Some(dir) => dir,
            None => {
                // Tier 1 (installed) has no directory for this package. Fall
                // back through the ordered metadata providers (Tier 2 repo DB,
                // then Tier 3 shipped DB). The first provider that knows the
                // package wins and its PackageInfo IS cached. A package that no
                // provider knows is left uncached so `package_exists()` still
                // reports it missing (install status stays Tier-1-only).
                if let Some(info) = self.resolve_from_providers(name) {
                    self.insert_package_under_lease(info, lease);
                    return self.get_cached_package(name).await;
                }
                log::trace!(
                    "Package '{}' not found in any tier (libpaths: {:?})",
                    name,
                    self.lib_paths
                );
                return None;
            }
        };

        // Step 3: Try static parsing (Tier 1)
        let parse_result = match self.parse_package_static(&pkg_dir) {
            Some(result) => result,
            None => {
                log::trace!(
                    "Failed to parse NAMESPACE for package '{}', caching empty",
                    name
                );
                let info = PackageInfo::with_details(
                    name.to_string(),
                    HashSet::new(),
                    Vec::new(),
                    Vec::new(),
                );
                self.insert_package_under_lease(info, lease);
                return self.get_cached_package(name).await;
            }
        };

        // Step 4: Determine export loading strategy based on pattern presence.
        // `completeness` tracks whether the resolved export set is exhaustive:
        // a static parse without `exportPattern` and a successful R enumeration
        // are both `Complete`; an INDEX fallback (R failed/absent) is `Partial`
        // because it lists only documented topics. See `exports_completeness`.
        let (exports, completeness): (HashSet<String>, MemberCompleteness) =
            if parse_result.has_export_pattern {
                // Tier 2: Package uses exportPattern - try R subprocess for accuracy
                log::trace!(
                    "Package '{}' uses exportPattern, trying R subprocess for accuracy",
                    name
                );

                if let Some(ref r_subprocess) = self.r_subprocess {
                    match r_subprocess.get_package_exports(name).await {
                        Ok(r_exports) => {
                            log::trace!(
                                "Got {} exports for package '{}' from R subprocess",
                                r_exports.len(),
                                name
                            );
                            (
                                r_exports.into_iter().collect(),
                                MemberCompleteness::Complete,
                            )
                        }
                        Err(e) => {
                            // Tier 3: R failed, fall back to INDEX + explicit exports
                            log::trace!(
                                "R subprocess failed for package '{}': {}, using INDEX fallback",
                                name,
                                e
                            );
                            (
                                self.load_with_index_fallback(&pkg_dir, &parse_result).await,
                                MemberCompleteness::Partial,
                            )
                        }
                    }
                } else {
                    // No R subprocess, use INDEX fallback
                    log::trace!(
                        "No R subprocess available for package '{}', using INDEX fallback",
                        name
                    );
                    (
                        self.load_with_index_fallback(&pkg_dir, &parse_result).await,
                        MemberCompleteness::Partial,
                    )
                }
            } else {
                // Tier 1: No exportPattern, static parsing is sufficient (94% of packages)
                log::trace!(
                    "Package '{}' uses explicit exports only, loaded {} exports statically",
                    name,
                    parse_result.explicit_exports.len()
                );
                (
                    parse_result.explicit_exports.into_iter().collect(),
                    MemberCompleteness::Complete,
                )
            };

        // Step 5: Create PackageInfo (incl. datasets from `data/`) and cache it.
        let mut info =
            package_info_from_dir(name.to_string(), &pkg_dir, exports, parse_result.depends).await;
        info.exports_completeness = completeness;

        // Issue #429: enumerate lazy-data objects via R when the package ships data/.
        let data_dir = pkg_dir.join("data");
        let mut datasets_enumerated = false;
        if data_dir.is_dir()
            && let Some(ref r_subprocess) = self.r_subprocess
        {
            let pkgs = vec![name.to_string()];
            match r_subprocess.get_multiple_package_datasets(&pkgs).await {
                Ok(mut map) => {
                    apply_enumeration_from(&mut map, name, &pkg_dir, &mut info);
                    datasets_enumerated = true;
                }
                Err(e) => {
                    log::trace!(
                        "data() enumeration failed for '{}': {} (static fallback)",
                        name,
                        e
                    );
                }
            }
        }
        // A package that ships a `data/` directory whose datasets were NOT
        // enumerated via R cannot be absence-authoritative: a binary
        // `data/Rdata.rdb` without a `datalist` exposes no object names to the
        // static parse, so a real `pkg::dataset` would otherwise be wrongly
        // `Absent` (datasets are positive-only). Downgrade an otherwise-`Complete`
        // export set to `Partial` in that case. Datasets enumerated via R, or no
        // `data/` dir at all, keep `Complete`.
        if info.exports_completeness == MemberCompleteness::Complete
            && data_dir.is_dir()
            && !datasets_enumerated
        {
            info.exports_completeness = MemberCompleteness::Partial;
        }

        log::trace!(
            "Created PackageInfo for '{}': {} exports, {} depends, is_meta_package={}",
            name,
            info.exports.len(),
            info.depends.len(),
            info.is_meta_package
        );

        // Insert into cache
        self.insert_package_under_lease(info, lease);

        // Return the cached version
        self.get_cached_package(name).await
    }

    /// Find the package directory in lib_paths
    ///
    /// Searches each library path for a directory with the package name
    /// that contains a NAMESPACE or DESCRIPTION file (indicating a valid R package).
    /// Note: The `base` package doesn't have a NAMESPACE file, only DESCRIPTION.
    /// Returns the first match found.
    ///
    /// Validates package names to prevent path traversal attacks - rejects names
    /// containing path separators or parent directory references.
    pub fn find_package_directory(&self, name: &str) -> Option<PathBuf> {
        // Validate package name to prevent path traversal attacks
        // Valid R package names contain only alphanumeric, '.', and '_' characters
        // and must not contain path separators or '..'
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
            || name.starts_with('.')
        {
            log::trace!(
                "Rejecting invalid package name '{}' (possible path traversal)",
                name
            );
            return None;
        }

        for lib_path in &self.lib_paths {
            let package_dir = lib_path.join(name);
            // Check for NAMESPACE or DESCRIPTION file to ensure it's a valid R package
            // Note: `base` package doesn't have NAMESPACE, only DESCRIPTION
            if is_readable_file(&package_dir.join("NAMESPACE"))
                || is_readable_file(&package_dir.join("DESCRIPTION"))
            {
                return Some(package_dir);
            }
        }
        None
    }

    /// Enumerate the names of all packages present across the library paths.
    /// A "package" is a subdirectory containing a `DESCRIPTION` file. Used by
    /// `raven packages freeze --installed/--all` and the Tier 3 build's
    /// reference-R capture.
    pub fn enumerate_installed_packages(&self) -> Vec<String> {
        let mut names = std::collections::HashSet::new();
        for lib in &self.lib_paths {
            let Ok(entries) = std::fs::read_dir(lib) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir()
                    && path.join("DESCRIPTION").is_file()
                    && let Some(name) = path.file_name().and_then(|n| n.to_str())
                {
                    names.insert(name.to_string());
                }
            }
        }
        names.into_iter().collect()
    }

    /// Read a package's `DESCRIPTION` `Version` field from disk, if the package
    /// is installed in a library path. Used by the Tier 3 build's reference-R
    /// capture and `raven packages freeze` to stamp each record's version.
    pub fn package_version(&self, name: &str) -> Option<String> {
        let dir = self.find_package_directory(name)?;
        let content = std::fs::read_to_string(dir.join("DESCRIPTION")).ok()?;
        crate::namespace_parser::parse_description_field_pub(&content, "Version")
            .into_iter()
            .next()
    }

    /// Check if a package exists (is installed)
    ///
    /// This is a synchronous method that checks installation by:
    /// 1. Checking if it's a base package (always available)
    /// 2. Calling `find_package_directory()` to check for the package on the
    ///    filesystem in any `lib_path`.
    ///
    /// Existence is determined by the filesystem only — this method NEVER
    /// consults the in-memory cache. `prefetch_packages()` inserts an empty-
    /// exports entry for any package whose namespace fails to load (the R
    /// query swallows `asNamespace()` errors via `tryCatch`), so a cached
    /// entry does NOT prove the package is installed. Trusting the cache here
    /// would permanently suppress "Package 'X' is not installed" diagnostics
    /// for any uninstalled package mentioned in a `library()` call after the
    /// first prefetch pass.
    ///
    /// This method does NOT load the package into cache — it only checks
    /// existence. Use `get_package()` to load and cache package information.
    ///
    /// **Validates: Requirement 15.1** - Used to detect non-installed packages for diagnostics
    pub fn package_exists(&self, name: &str) -> bool {
        if self.base_packages.contains(name) {
            return true;
        }
        self.find_package_directory(name).is_some()
    }

    /// Get all exports for a package including Depends and attached packages
    ///
    /// This method loads the package and all its dependencies (from the Depends field
    /// and attached_packages for meta-packages), combining their exports into a single set.
    /// It tracks visited packages to handle circular dependencies.
    ///
    /// Results are cached in combined_entries for efficient repeated lookups.
    ///
    /// # Behavior
    /// 1. Check combined_entries cache first
    /// 2. Load the main package using `get_package()`
    /// 3. Add the package's exports to the result set
    /// 4. Recursively load all packages in `depends` and `attached_packages`
    /// 5. Track visited packages to prevent infinite loops from circular dependencies
    /// 6. Cache and return the combined exports from all packages
    ///
    /// Requirement 4.2: WHEN a package has dependencies in the `Depends` field,
    /// THE Package_Resolver SHALL also load exports from those packages at the same position
    ///
    /// Requirement 4.5: THE Package_Resolver SHALL handle circular dependencies in the
    /// `Depends` chain by tracking visited packages
    pub async fn get_all_exports(&self, name: &str) -> HashSet<String> {
        let Some(lease) = self.begin_cache_operation().await else {
            return HashSet::new();
        };
        self.warm_all_exports_under_lease(name, &lease)
            .await
            .as_ref()
            .clone()
    }

    /// Build and cache the combined export set for `name`, returning the shared
    /// cached snapshot.
    ///
    /// Unlike [`PackageLibrary::get_all_exports`], this does not materialize an
    /// owned `HashSet`. It is the cache-warming path used by
    /// [`PackageLibrary::prefetch_packages`], where callers only need the
    /// aggregate to become available to synchronous cache probes.
    #[cfg(test)]
    async fn warm_all_exports(&self, name: &str) -> Arc<HashSet<String>> {
        let Some(lease) = self.begin_cache_operation().await else {
            return Arc::new(HashSet::new());
        };
        self.warm_all_exports_under_lease(name, &lease).await
    }

    async fn warm_all_exports_under_lease(
        &self,
        name: &str,
        lease: &PackageCacheOperationLease<'_>,
    ) -> Arc<HashSet<String>> {
        // Check cache first
        {
            let cache = self.combined_entries.load();
            if let Some(cached) = cache.get(name) {
                log::trace!("Using cached combined exports for package '{}'", name);
                return Arc::clone(&cached.exports);
            }
        }

        // Compute exports + owner attribution. `owners` records, for each
        // symbol, the package that actually contributed it (the documentation /
        // NSE-policy owner) so consumers can distinguish ownership from mere
        // availability through an aggregate. See issue #407.
        let mut visited = HashSet::new();
        let mut all_exports = HashSet::new();
        let mut owners: HashMap<String, String> = HashMap::new();
        self.collect_exports_recursive(name, &mut visited, &mut all_exports, &mut owners, lease)
            .await;

        // Publish availability and owner attribution as one immutable entry, so
        // hot-path readers can never observe a combined export without the
        // matching owner attribution.
        self.update_combined_entries(lease, |cache| {
            let cached = Arc::new(CombinedEntry::new(all_exports, owners));
            cache.insert(name.to_string(), Arc::clone(&cached));
            log::trace!(
                "Cached {} combined exports for package '{}'",
                cached.exports.len(),
                name
            );
            if cached.exports.is_empty() {
                log::trace!(
                    "No exports collected for package '{}' (may be missing or unreadable)",
                    name
                );
            }
            Arc::clone(&cached.exports)
        })
    }

    /// Helper method to recursively collect exports from a package and its dependencies
    ///
    /// This is a private helper for `get_all_exports()` that handles the recursive
    /// traversal of package dependencies while tracking visited packages to prevent
    /// infinite loops.
    ///
    /// # Arguments
    /// * `name` - The package name to load
    /// * `visited` - Set of already-visited package names (for cycle detection)
    /// * `all_exports` - Accumulator for all collected exports
    /// * `owners` - Accumulator mapping each symbol to its contributing package.
    ///   First contributor wins: because the aggregate root is visited before
    ///   its `depends`/`attached` members, the root owns its own exports and a
    ///   member only owns symbols the root does not export itself (issue #407).
    async fn collect_exports_recursive(
        &self,
        name: &str,
        visited: &mut HashSet<String>,
        all_exports: &mut HashSet<String>,
        owners: &mut HashMap<String, String>,
        lease: &PackageCacheOperationLease<'_>,
    ) {
        // Check if we've already visited this package (circular dependency detection)
        if visited.contains(name) {
            log::trace!(
                "Skipping already-visited package '{}' (circular dependency)",
                name
            );
            return;
        }

        // Mark this package as visited
        visited.insert(name.to_string());

        // Load the package
        let package_info = match self.get_package_under_lease(name, lease).await {
            Some(info) => info,
            None => {
                log::trace!(
                    "Could not load package '{}' for transitive dependency resolution",
                    name
                );
                return;
            }
        };

        // Add this package's exports and datasets to the result. Datasets
        // (lazy_data) resolve as in-scope symbols too; see the `lazy_data`
        // field doc for why they are folded in here.
        all_exports.extend(package_info.exports.iter().cloned());
        all_exports.extend(package_info.lazy_data.iter().cloned());

        // Record owner attribution. `or_insert_with` makes the first contributor
        // win, so the aggregate root (visited first) owns symbols it exports
        // itself, while members own only the symbols the root does not.
        for symbol in package_info
            .exports
            .iter()
            .chain(package_info.lazy_data.iter())
        {
            owners
                .entry(symbol.clone())
                .or_insert_with(|| name.to_string());
        }

        log::trace!(
            "Added {} exports + {} datasets from package '{}' (total: {})",
            package_info.exports.len(),
            package_info.lazy_data.len(),
            name,
            all_exports.len()
        );

        // Collect packages to process: depends + attached_packages (for meta-packages).
        // Keep a companion HashSet so deduplication stays O(1) while preserving
        // the original dependency-first traversal order in the Vec.
        let mut packages_to_process: Vec<String> = package_info.depends.clone();
        let mut packages_seen: HashSet<&str> = package_info
            .depends
            .iter()
            .map(|dep| dep.as_str())
            .collect();

        // For meta-packages (tidyverse, tidymodels), also process attached packages
        if package_info.is_meta_package {
            for attached in &package_info.attached_packages {
                if packages_seen.insert(attached.as_str()) {
                    packages_to_process.push(attached.clone());
                }
            }
        }

        // Recursively process all dependency packages
        // Use Box::pin for recursive async calls
        for dep_name in packages_to_process {
            Box::pin(self.collect_exports_recursive(
                &dep_name,
                visited,
                all_exports,
                owners,
                lease,
            ))
            .await;
        }
    }
}

/// Load result of the Tier 3 shipped `names.db`, captured during
/// [`build_package_library`]. This is a richer signal than "a names.db file is
/// on disk": it distinguishes *absent* from *present-but-unusable*, which a bare
/// `Path::exists()` check cannot. Consumers that advise the user (e.g. `raven
/// check`'s missing-metadata warning) need that distinction — a corrupt or
/// unsupported DB should steer toward `raven packages update` (re-download a
/// good copy), not `raven packages freeze` (which only helps when the DB loaded
/// fine but genuinely lacks the package).
///
/// Derived from the same [`crate::package_db::binary_db::ShippedDbProvider::from_file`]
/// calls that wire the provider, so it never disagrees with whether a working
/// Tier 3 provider was actually installed. `load_notes` carries the human-readable
/// failure text; this carries the structured verdict so callers don't parse strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShippedDbLoad {
    /// No usable shipped `names.db`: either no candidate exists on disk, or a
    /// candidate opened successfully but is empty (zero packages). An empty DB
    /// is treated as `Absent` rather than `Loaded` because it provides no
    /// coverage — steering the missing-metadata advice toward `raven packages
    /// update` (download a real DB), not `raven packages freeze`.
    Absent,
    /// A non-empty shipped `names.db` was found and loaded successfully (provider
    /// wired). An empty DB does not reach this state — see `Absent`.
    Loaded,
    /// A shipped `names.db` file is present but failed to load — `Corrupt` or
    /// `UnsupportedFormat`. The detail is also recorded in `load_notes`.
    Failed,
}

/// Outcome of [`build_package_library`]: the constructed library plus a single
/// build status that records R/Tier-1 initialization and any degradation reason.
/// Consumer readiness is derived by [`PackageLibraryOutcome::consumer_ready`],
/// because non-`Ready` libraries can still contain useful offline data: Tier 2/3
/// providers or embedded/offline base exports.
pub struct PackageLibraryOutcome {
    /// Always present. `new_empty()` for `Disabled`. A non-`Ready` library may
    /// still carry useful offline data (base symbols, configured additional
    /// paths, fallback providers), so callers should use
    /// [`consumer_ready`](Self::consumer_ready) for normal diagnostic/completion
    /// consumption instead of reading the build status directly.
    pub library: Arc<PackageLibrary>,
    pub status: PackageLibraryStatus,
    /// Non-fatal notes accumulated while opening Tier 2/3 fallback DBs
    /// (e.g. a present-but-unusable DB explained-and-continued). Empty on the
    /// `Disabled` early-return and the Tier-1-only capture path, which wire no
    /// providers.
    pub load_notes: Vec<String>,
    /// Whether the Tier 3 shipped `names.db` was absent, loaded, or
    /// present-but-failed. `Absent` on the `Disabled` early-return and the
    /// Tier-1-only capture path, which never open the shipped DB.
    pub shipped_db_load: ShippedDbLoad,
}

impl PackageLibraryOutcome {
    /// True when the constructed library has enough data for normal consumers.
    ///
    /// Tier-1/R readiness (`Ready`) is sufficient, but not required: offline
    /// Tier 2/3 providers can resolve package exports, and embedded/offline base
    /// exports can satisfy base-symbol diagnostics even when R is unavailable.
    /// `Disabled` naturally stays false because that path returns an empty
    /// library with no providers or base exports.
    pub fn consumer_ready(&self) -> bool {
        self.status.is_ready()
            || self.library.has_providers()
            || !self.library.base_exports().is_empty()
    }
}

/// The single source of truth for package-library **build status** and the
/// reason a build degraded. All four package-library init sites route through
/// [`build_package_library`]: `backend::rebuild_package_library`,
/// `cli::check::maybe_init_r`, `backend::ensure_package_library_initialized`,
/// and the Task B post-scan startup init. Routing them all through one
/// builder is what stops the editor and `raven check` from drifting, and this
/// enum's `classify` is where the status
/// classification and degradation precedence live — not duplicated per site.
///
/// Build status is distinct from the *consumer readiness gate*
/// (`package_library_ready`): runtime diagnostic/completion consumers use
/// [`PackageLibraryOutcome::consumer_ready`] so offline Tier 2/3 providers and
/// embedded/offline base exports are usable even when the Tier-1/R build status
/// is degraded. Tier-1 capture paths such as `raven packages freeze` intentionally
/// continue to inspect this status directly.
#[derive(Debug, Clone, PartialEq)]
pub enum PackageLibraryStatus {
    /// `packages.enabled == false`; no R discovery attempted.
    Disabled,
    /// Initialized with >= 1 library path — the only ready state.
    Ready,
    /// No usable R subprocess located.
    RNotFound,
    /// Library construction failed. Tier-1 `initialize()` currently swallows R
    /// failures, but provider admission, deadline, thread-spawn, or physical
    /// load failures can reach this status through the public builder.
    InitFailed(String),
    /// R found, init ok, but zero lib paths discovered/configured.
    NoLibraryPaths,
}

impl PackageLibraryStatus {
    /// Only `Ready` is ready.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Classify an *enabled* build. `init_error == None` means `initialize()`
    /// succeeded. Precedence is `Ready -> RNotFound -> InitFailed ->
    /// NoLibraryPaths`, mirroring the CLI's pre-refactor order exactly so the
    /// extraction is behavior-identical. `Disabled` is set by the gate in
    /// [`build_package_library`] before this is called.
    fn classify(init_error: Option<String>, r_found: bool, has_lib_paths: bool) -> Self {
        if init_error.is_none() && has_lib_paths {
            Self::Ready
        } else if !r_found {
            Self::RNotFound
        } else if let Some(err) = init_error {
            Self::InitFailed(err)
        } else {
            Self::NoLibraryPaths
        }
    }
}

/// Build a `PackageLibrary` from current configuration — the shared constructor
/// all four package-library init sites route through (`rebuild_package_library`,
/// `maybe_init_r`, `ensure_package_library_initialized`, and the Task B
/// post-scan startup init), so editor and CI can't drift.
///
/// Lock-free by design: takes owned/cloned inputs and no `WorldState`, so it
/// adds no state or performance-accounting dependency to this module. R
/// discovery is async, deadline-aware, and kill-on-drop; Tier 2/3 provider
/// opening and checksum work runs on a capacity-bounded, runtime-independent OS
/// thread. Provider load notes are logged here and returned so a winning caller
/// can also surface them through its UI contract. Configured `additional_paths`
/// are applied *after* R discovery so they augment (never suppress) R-reported
/// paths and count toward readiness.
pub async fn build_package_library(
    r_path: Option<PathBuf>,
    additional_paths: &[PathBuf],
    workspace_root: Option<PathBuf>,
    packages_enabled: bool,
) -> PackageLibraryOutcome {
    match try_build_package_library(
        r_path,
        additional_paths,
        workspace_root,
        packages_enabled,
        ProviderLoadPolicy::Share,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => PackageLibraryOutcome {
            library: Arc::new(PackageLibrary::new_empty()),
            status: PackageLibraryStatus::InitFailed(error.to_string()),
            load_notes: Vec::new(),
            shipped_db_load: ShippedDbLoad::Absent,
        },
    }
}

struct ProviderLoadArtifact {
    providers: Vec<Arc<dyn crate::package_db::PackageMetadataProvider>>,
    notes: Vec<String>,
    shipped_db_load: ShippedDbLoad,
}

#[derive(Debug, Clone)]
struct ProviderLoadFailure(String);

impl std::fmt::Display for ProviderLoadFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProviderLoadFailure {}

type ProviderLoadResult = Result<Arc<ProviderLoadArtifact>, Arc<ProviderLoadFailure>>;

#[derive(Clone)]
enum ProviderLoadState {
    Running,
    Complete(ProviderLoadResult),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderLoadKey {
    repo_db_path: Option<PathBuf>,
    shipped_db_inputs: crate::package_db::ShippedDbCandidateInputs,
}

#[derive(Default)]
struct ProviderLoadRegistry {
    active: std::collections::HashMap<ProviderLoadKey, ActiveProviderLoad>,
}

struct ActiveProviderLoad {
    generation: u64,
    completion: tokio::sync::watch::Receiver<ProviderLoadState>,
    #[cfg(test)]
    followers: usize,
}

/// Authority policy for a fallback-provider build.
///
/// Passive consumers share any same-key physical generation already in
/// progress. An explicit replacement authority supersedes the generation that
/// was active when it arrived and installs the next generation under the
/// registry lock. The old physical job may finish concurrently, but its exact
/// generation cleanup cannot remove the fresh entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderLoadPolicy {
    Share,
    FreshAfterActive,
}

#[cfg(test)]
struct ProviderLoadTestPause {
    arrived: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
fn provider_load_test_pauses()
-> &'static parking_lot::Mutex<std::collections::HashMap<ProviderLoadKey, ProviderLoadTestPause>> {
    static PAUSES: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<ProviderLoadKey, ProviderLoadTestPause>>,
    > = std::sync::OnceLock::new();
    PAUSES.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
fn pause_provider_load_for_test(key: &ProviderLoadKey) {
    if let Some(pause) = provider_load_test_pauses().lock().remove(key) {
        let _ = pause.arrived.send(());
        let _ = pause.release.recv();
    }
}

const PROVIDER_LOAD_HARD_CAP: usize = 4;
static NEXT_PROVIDER_LOAD_GENERATION: AtomicU64 = AtomicU64::new(1);

fn provider_load_registry() -> &'static Arc<parking_lot::Mutex<ProviderLoadRegistry>> {
    static REGISTRY: std::sync::OnceLock<Arc<parking_lot::Mutex<ProviderLoadRegistry>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| Arc::new(parking_lot::Mutex::new(ProviderLoadRegistry::default())))
}

fn provider_load_gate() -> &'static Arc<tokio::sync::Semaphore> {
    static GATE: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(PROVIDER_LOAD_HARD_CAP)))
}

#[derive(Clone)]
struct ProviderLoadCompletion {
    inner: Arc<ProviderLoadCompletionInner>,
}

struct ProviderLoadCompletionInner {
    parts: Mutex<Option<ProviderLoadCompletionParts>>,
}

struct ProviderLoadCompletionParts {
    registry: Arc<parking_lot::Mutex<ProviderLoadRegistry>>,
    key: ProviderLoadKey,
    generation: u64,
    sender: tokio::sync::watch::Sender<ProviderLoadState>,
    permit: tokio::sync::OwnedSemaphorePermit,
}

impl ProviderLoadCompletion {
    fn new(
        registry: Arc<parking_lot::Mutex<ProviderLoadRegistry>>,
        key: ProviderLoadKey,
        generation: u64,
        sender: tokio::sync::watch::Sender<ProviderLoadState>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Self {
        Self {
            inner: Arc::new(ProviderLoadCompletionInner {
                parts: Mutex::new(Some(ProviderLoadCompletionParts {
                    registry,
                    key,
                    generation,
                    sender,
                    permit,
                })),
            }),
        }
    }

    /// Publish and retire this generation exactly once. The permit remains
    /// owned through terminal publication and generation-checked cleanup.
    fn finish(&self, result: ProviderLoadResult) {
        let Some(parts) = self.inner.parts.lock().take() else {
            return;
        };
        complete_provider_load(
            parts.registry,
            parts.key,
            parts.generation,
            parts.sender,
            result,
        );
        drop(parts.permit);
    }
}

struct ProviderThreadCompletionGuard {
    completion: ProviderLoadCompletion,
    result: Option<ProviderLoadResult>,
}

impl Drop for ProviderThreadCompletionGuard {
    fn drop(&mut self) {
        let result = self.result.take().unwrap_or_else(|| {
            Err(Arc::new(ProviderLoadFailure(
                "package metadata provider thread stopped without a result".to_string(),
            )))
        });
        self.completion.finish(result);
    }
}

fn provider_panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

fn spawn_provider_load_thread(
    completion: ProviderLoadCompletion,
    job: impl FnOnce() -> ProviderLoadResult + Send + 'static,
) -> Result<(), ProviderLoadFailure> {
    let thread_completion = completion.clone();
    std::thread::Builder::new()
        .name("raven-package-provider".to_string())
        .spawn(move || {
            let mut guard = ProviderThreadCompletionGuard {
                completion: thread_completion,
                result: None,
            };
            guard.result = Some(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).unwrap_or_else(
                    |payload| {
                        Err(Arc::new(ProviderLoadFailure(format!(
                            "package metadata provider thread panicked: {}",
                            provider_panic_message(payload)
                        ))))
                    },
                ),
            );
        })
        .map(drop)
        .map_err(|error| {
            ProviderLoadFailure(format!(
                "failed to spawn package metadata provider thread: {error}"
            ))
        })
}

async fn subscribe_or_spawn_provider_load_from<F>(
    registry: &Arc<parking_lot::Mutex<ProviderLoadRegistry>>,
    gate: &Arc<tokio::sync::Semaphore>,
    provider_key: &ProviderLoadKey,
    policy: ProviderLoadPolicy,
    spawn: F,
) -> anyhow::Result<tokio::sync::watch::Receiver<ProviderLoadState>>
where
    F: FnOnce(ProviderLoadCompletion) -> Result<(), ProviderLoadFailure>,
{
    let deadline = crate::r_subprocess::current_outer_r_deadline();
    let excluded_generation = {
        let mut registry = registry.lock();
        if let Some(active) = registry.active.get_mut(provider_key) {
            if policy == ProviderLoadPolicy::Share {
                #[cfg(test)]
                {
                    active.followers = active.followers.saturating_add(1);
                }
                return Ok(active.completion.clone());
            }
            Some(active.generation)
        } else {
            None
        }
    };

    // Tokio's semaphore queue is FIFO. The caller's outer deadline is captured
    // once so capacity admission and later completion observe one stable
    // budget. A fresh generation can overlap the excluded physical job and
    // therefore consumes its own permit.
    let acquire = Arc::clone(gate).acquire_owned();
    let permit = if let Some(deadline) = deadline {
        tokio::time::timeout_at(deadline, acquire)
            .await
            .map_err(|_| {
                anyhow::Error::new(crate::r_subprocess::OuterDeadlineExpired {
                    phase: "package metadata provider capacity",
                })
            })?
    } else {
        acquire.await
    }
    .map_err(|_| anyhow::anyhow!("package metadata provider capacity gate closed"))?;

    let mut registry_guard = registry.lock();
    if let Some(active) = registry_guard.active.get_mut(provider_key)
        && Some(active.generation) != excluded_generation
    {
        #[cfg(test)]
        {
            active.followers = active.followers.saturating_add(1);
        }
        let receiver = active.completion.clone();
        drop(registry_guard);
        drop(permit);
        return Ok(receiver);
    }

    let generation = NEXT_PROVIDER_LOAD_GENERATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("provider-load generation exhausted");
    let (sender, receiver) = tokio::sync::watch::channel(ProviderLoadState::Running);
    registry_guard.active.insert(
        provider_key.clone(),
        ActiveProviderLoad {
            generation,
            completion: receiver.clone(),
            #[cfg(test)]
            followers: 0,
        },
    );
    let completion = ProviderLoadCompletion::new(
        Arc::clone(registry),
        provider_key.clone(),
        generation,
        sender,
        permit,
    );
    // Installation and physical detachment share one synchronous critical
    // section. Spawn failure is published immediately after releasing the
    // registry lock so no receiver, entry, or permit can be stranded.
    let spawn_result = spawn(completion.clone());
    drop(registry_guard);
    if let Err(error) = spawn_result {
        completion.finish(Err(Arc::new(error.clone())));
        return Err(anyhow::Error::new(error));
    }
    Ok(receiver)
}

fn complete_provider_load(
    registry: Arc<parking_lot::Mutex<ProviderLoadRegistry>>,
    key: ProviderLoadKey,
    generation: u64,
    sender: tokio::sync::watch::Sender<ProviderLoadState>,
    result: ProviderLoadResult,
) {
    // Publish before generation-checked cleanup. Existing followers retain
    // their receiver even after the registry admits a later generation.
    sender.send_replace(ProviderLoadState::Complete(result));
    let mut registry = registry.lock();
    if registry
        .active
        .get(&key)
        .is_some_and(|active| active.generation == generation)
    {
        registry.active.remove(&key);
    }
}

async fn await_provider_load(
    receiver: tokio::sync::watch::Receiver<ProviderLoadState>,
) -> anyhow::Result<Arc<ProviderLoadArtifact>> {
    match await_provider_load_result(receiver, crate::r_subprocess::current_outer_r_deadline())
        .await?
    {
        Ok(artifact) => Ok(artifact),
        Err(error) => Err(anyhow::anyhow!("{error}")),
    }
}

async fn await_provider_load_result(
    mut receiver: tokio::sync::watch::Receiver<ProviderLoadState>,
    deadline: Option<tokio::time::Instant>,
) -> anyhow::Result<ProviderLoadResult> {
    loop {
        match receiver.borrow_and_update().clone() {
            ProviderLoadState::Complete(result) => return Ok(result),
            ProviderLoadState::Running => {}
        }

        let changed = receiver.changed();
        if let Some(deadline) = deadline {
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| {
                    anyhow::Error::new(crate::r_subprocess::OuterDeadlineExpired {
                        phase: "package metadata provider load",
                    })
                })?
                .map_err(|_| anyhow::anyhow!("package metadata provider thread stopped"))?;
        } else {
            changed
                .await
                .map_err(|_| anyhow::anyhow!("package metadata provider thread stopped"))?;
        }
    }
}

/// Typed package-routing builder. `provider_load_policy` is call-site authority:
/// passive startup/on-demand paths use [`ProviderLoadPolicy::Share`], while the
/// explicit replacement helper used by refresh/config/libpath changes uses
/// [`ProviderLoadPolicy::FreshAfterActive`]. Deadline expiry remains an error so
/// callers cannot accidentally commit an empty/degraded library as a successful
/// replacement.
pub(crate) async fn try_build_package_library(
    r_path: Option<PathBuf>,
    additional_paths: &[PathBuf],
    workspace_root: Option<PathBuf>,
    packages_enabled: bool,
    provider_load_policy: ProviderLoadPolicy,
) -> anyhow::Result<PackageLibraryOutcome> {
    if !packages_enabled {
        return Ok(PackageLibraryOutcome {
            library: Arc::new(PackageLibrary::new_empty()),
            status: PackageLibraryStatus::Disabled,
            load_notes: Vec::new(),
            shipped_db_load: ShippedDbLoad::Absent,
        });
    }

    // Tier 2 (repo DB) path is derived from the workspace root, so clone it
    // before `workspace_root` is moved into the shared core below.
    let repo_db_path = workspace_root
        .as_ref()
        .map(|r| r.join(".raven").join("packages.json"));

    let (mut lib, status) = build_library_inner(r_path, additional_paths, workspace_root).await?;

    // Open the fallback DBs off the async runtime (mmap + ~10-20 ms blake3).
    let shipped_db_inputs = crate::package_db::capture_shipped_db_candidate_inputs();
    let provider_key = ProviderLoadKey {
        repo_db_path: repo_db_path.clone(),
        shipped_db_inputs: shipped_db_inputs.clone(),
    };
    if crate::r_subprocess::current_outer_r_deadline()
        .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
    {
        return Err(anyhow::Error::new(
            crate::r_subprocess::OuterDeadlineExpired {
                phase: "package metadata provider pre-spawn",
            },
        ));
    }
    #[cfg(test)]
    let physical_key = provider_key.clone();
    let physical_deadline =
        crate::r_subprocess::current_outer_r_deadline().map(tokio::time::Instant::into_std);
    let receiver = subscribe_or_spawn_provider_load_from(
        provider_load_registry(),
        provider_load_gate(),
        &provider_key,
        provider_load_policy,
        move |completion| {
            spawn_provider_load_thread(completion, move || {
                let shipped_db_candidates =
                    crate::package_db::locate_shipped_db_candidates_from(&shipped_db_inputs);
                let mut providers: Vec<Arc<dyn crate::package_db::PackageMetadataProvider>> =
                    Vec::new();
                let mut notes: Vec<String> = Vec::new();
                // Tier 2 first (repo DB), then Tier 3 (shipped DB).
                //
                // A valid-but-empty DB (zero packages) resolves nothing, so it is NOT
                // wired as a provider: `has_providers()` must mean "real package
                // coverage exists" (the degraded-environment note in `cli::check` gates
                // on it), and an empty provider would also cost futile lookups. An empty
                // Tier-2 file is thus treated exactly like an absent one.
                if let Some(path) = repo_db_path {
                    match crate::package_db::json_db::RepoDbProvider::from_file_with_deadline(
                        &path,
                        physical_deadline,
                    ) {
                        Ok(Some(p)) if !p.is_empty() => providers.push(Arc::new(p)),
                        Ok(_) => {}                          // Absent or empty -> silent
                        Err(crate::package_db::json_db::RepoDbError::DeadlineExpired) => {
                            return Err(Arc::new(ProviderLoadFailure(
                                "package metadata provider load exceeded the original routing deadline"
                                    .to_string(),
                            )));
                        }
                        Err(e) => notes.push(e.to_string()), // explain-and-continue
                    }
                }
                // Track the Tier 3 verdict alongside the provider: `Absent` until a
                // candidate proves otherwise. A non-empty load wins, is wired, and stops
                // the scan; a failed candidate records `Failed` but keeps looking, so a
                // later working candidate still upgrades the verdict to `Loaded`.
                //
                // An empty DB (zero packages) is treated as no coverage: it is neither
                // wired (see the Tier-2 note above) nor recorded as `Loaded`, and the
                // scan continues to lower-precedence candidates. Leaving the verdict
                // `Absent` is deliberate — the missing-export footer then points at
                // `raven packages update` (download a real DB), the correct remedy,
                // rather than the `Loaded` branch's "the package is likely private / not
                // on CRAN", which would misdescribe an empty database.
                let mut shipped_db_load = ShippedDbLoad::Absent;
                for path in shipped_db_candidates {
                    match crate::package_db::binary_db::ShippedDbProvider::from_file_with_deadline(
                        &path,
                        physical_deadline,
                    ) {
                        Ok(Some(p)) if !p.is_empty() => {
                            providers.push(Arc::new(p));
                            shipped_db_load = ShippedDbLoad::Loaded;
                            break;
                        }
                        Ok(_) => {} // Absent (None) or empty -> not usable coverage; keep scanning
                        Err(crate::package_db::binary_db::ShippedDbError::DeadlineExpired) => {
                            return Err(Arc::new(ProviderLoadFailure(
                                "package metadata provider load exceeded the original routing deadline"
                                    .to_string(),
                            )));
                        }
                        Err(e) => {
                            notes.push(format!("{}: {e}", path.display()));
                            shipped_db_load = ShippedDbLoad::Failed;
                        }
                    }
                }
                let artifact = Arc::new(ProviderLoadArtifact {
                    providers,
                    notes,
                    shipped_db_load,
                });
                #[cfg(test)]
                pause_provider_load_for_test(&physical_key);
                Ok(artifact)
            })
        },
    )
    .await?;
    let artifact = await_provider_load(receiver).await?;

    for note in &artifact.notes {
        log::warn!("{note}");
    }
    lib.set_shared_providers(artifact.providers.clone());

    Ok(PackageLibraryOutcome {
        library: Arc::new(lib),
        status,
        load_notes: artifact.notes.clone(),
        shipped_db_load: artifact.shipped_db_load,
    })
}

/// Shared construction core for both the runtime ([`build_package_library`]) and
/// capture ([`build_package_library_tier1_only`]) paths: R discovery, Tier-1
/// initialization, additional-path augmentation, and status classification. It
/// wires **no** Tier 2/3 providers and does not `Arc`-wrap — callers add
/// providers (or deliberately omit them) and own the wrapping.
async fn build_library_inner(
    r_path: Option<PathBuf>,
    additional_paths: &[PathBuf],
    workspace_root: Option<PathBuf>,
) -> anyhow::Result<(PackageLibrary, PackageLibraryStatus)> {
    let subprocess = match (RSubprocess::new_for_routing(r_path).await?, workspace_root) {
        (Some(subprocess), Some(root)) => Some(subprocess.with_working_dir(root)),
        (subprocess, _) => subprocess,
    };

    let r_found = subprocess.is_some();
    let mut lib = PackageLibrary::with_subprocess(subprocess);
    let init_error = match lib.initialize().await {
        Ok(()) => None,
        Err(error)
            if error
                .downcast_ref::<crate::r_subprocess::OuterDeadlineExpired>()
                .is_some() =>
        {
            return Err(error);
        }
        Err(error) => Some(error.to_string()),
    };
    lib.add_library_paths(additional_paths);
    let has_lib_paths = !lib.lib_paths().is_empty();

    let status = PackageLibraryStatus::classify(init_error, r_found, has_lib_paths);
    Ok((lib, status))
}

/// Build a Tier-1-only library with **no** fallback providers wired. This is the
/// capture path used by `freeze` and `build-shipped-db`, which must observe only
/// the live R installation and never resolve names from a Tier 2/3 DB (otherwise
/// captured data would be contaminated by the very DBs it produces).
pub async fn build_package_library_tier1_only(
    r_path: Option<PathBuf>,
    additional_paths: &[PathBuf],
    workspace_root: Option<PathBuf>,
) -> PackageLibraryOutcome {
    let (lib, status) = build_library_inner(r_path, additional_paths, workspace_root)
        .await
        .unwrap_or_else(|_| (PackageLibrary::new_empty(), PackageLibraryStatus::RNotFound));
    PackageLibraryOutcome {
        library: Arc::new(lib),
        status,
        load_notes: Vec::new(),
        shipped_db_load: ShippedDbLoad::Absent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared `RAVEN_NAMES_DB` env-var serialization lock and its RAII guard
    /// (defined in `package_db` so every lib-test that touches the var shares one
    /// instance / one audited `unsafe` mutation site).
    use crate::package_db::{NamesDbEnvGuard, RAVEN_NAMES_DB_ENV_LOCK};

    fn provider_load_key_for_test(name: &str) -> ProviderLoadKey {
        ProviderLoadKey {
            repo_db_path: Some(PathBuf::from(name)),
            shipped_db_inputs: crate::package_db::capture_shipped_db_candidate_inputs(),
        }
    }

    fn empty_provider_load_artifact() -> Arc<ProviderLoadArtifact> {
        Arc::new(ProviderLoadArtifact {
            providers: Vec::new(),
            notes: Vec::new(),
            shipped_db_load: ShippedDbLoad::Absent,
        })
    }

    fn replace_provider_file_for_test(path: &Path, contents: &str) {
        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, contents).unwrap();
        #[cfg(windows)]
        std::fs::remove_file(path).unwrap();
        std::fs::rename(replacement, path).unwrap();
    }

    struct CapturedProviderSpawn {
        completion: ProviderLoadCompletion,
    }

    async fn test_subscribe(
        registry: &Arc<parking_lot::Mutex<ProviderLoadRegistry>>,
        gate: &Arc<tokio::sync::Semaphore>,
        key: &ProviderLoadKey,
    ) -> anyhow::Result<(
        tokio::sync::watch::Receiver<ProviderLoadState>,
        Option<CapturedProviderSpawn>,
    )> {
        test_subscribe_with_policy(registry, gate, key, ProviderLoadPolicy::Share).await
    }

    async fn test_subscribe_with_policy(
        registry: &Arc<parking_lot::Mutex<ProviderLoadRegistry>>,
        gate: &Arc<tokio::sync::Semaphore>,
        key: &ProviderLoadKey,
        policy: ProviderLoadPolicy,
    ) -> anyhow::Result<(
        tokio::sync::watch::Receiver<ProviderLoadState>,
        Option<CapturedProviderSpawn>,
    )> {
        let captured = Arc::new(parking_lot::Mutex::new(None));
        let spawn_capture = Arc::clone(&captured);
        let receiver =
            subscribe_or_spawn_provider_load_from(registry, gate, key, policy, move |completion| {
                *spawn_capture.lock() = Some(CapturedProviderSpawn { completion });
                Ok(())
            })
            .await?;
        let spawn = captured.lock().take();
        Ok((receiver, spawn))
    }

    fn finish_test_spawn(
        registry: &Arc<parking_lot::Mutex<ProviderLoadRegistry>>,
        key: &ProviderLoadKey,
        spawn: CapturedProviderSpawn,
        result: ProviderLoadResult,
    ) {
        let _ = (registry, key);
        spawn.completion.finish(result);
    }

    fn wait_for_provider_cleanup(
        registry: &Arc<parking_lot::Mutex<ProviderLoadRegistry>>,
        gate: &Arc<tokio::sync::Semaphore>,
        key: &ProviderLoadKey,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while registry.lock().active.contains_key(key)
            || gate.available_permits() != PROVIDER_LOAD_HARD_CAP
        {
            assert!(
                std::time::Instant::now() < deadline,
                "provider generation did not clean up"
            );
            std::thread::yield_now();
        }
    }

    #[tokio::test]
    async fn provider_load_capacity_queues_fifth_then_succeeds() {
        let registry = Arc::new(parking_lot::Mutex::new(ProviderLoadRegistry::default()));
        let gate = Arc::new(tokio::sync::Semaphore::new(PROVIDER_LOAD_HARD_CAP));
        let keys = (0..=PROVIDER_LOAD_HARD_CAP)
            .map(|index| provider_load_key_for_test(&format!("provider-{index}")))
            .collect::<Vec<_>>();

        let mut spawns = Vec::new();
        for key in &keys[..PROVIDER_LOAD_HARD_CAP] {
            let (_, spawn) = test_subscribe(&registry, &gate, key).await.unwrap();
            spawns.push(spawn.expect("distinct key starts one physical supervisor"));
        }
        assert_eq!(registry.lock().active.len(), PROVIDER_LOAD_HARD_CAP);
        assert_eq!(gate.available_permits(), 0);
        let (_, duplicate) = test_subscribe(&registry, &gate, &keys[1]).await.unwrap();
        assert!(duplicate.is_none());
        assert_eq!(
            gate.available_permits(),
            0,
            "same-key fast path bypasses a full physical cap"
        );

        let fifth = tokio::spawn({
            let registry = Arc::clone(&registry);
            let gate = Arc::clone(&gate);
            let key = keys[PROVIDER_LOAD_HARD_CAP].clone();
            async move { test_subscribe(&registry, &gate, &key).await }
        });
        tokio::task::yield_now().await;
        assert!(
            !fifth.is_finished(),
            "the fifth distinct key waits for capacity"
        );

        let first = spawns.remove(0);
        finish_test_spawn(
            &registry,
            &keys[0],
            first,
            Ok(empty_provider_load_artifact()),
        );
        let (_, fifth_spawn) = tokio::time::timeout(std::time::Duration::from_secs(1), fifth)
            .await
            .expect("released capacity wakes the FIFO waiter")
            .unwrap()
            .unwrap();
        assert!(fifth_spawn.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn queued_provider_deadline_leaves_no_entry_or_permit_leak() {
        let registry = Arc::new(parking_lot::Mutex::new(ProviderLoadRegistry::default()));
        let gate = Arc::new(tokio::sync::Semaphore::new(PROVIDER_LOAD_HARD_CAP));
        let full_keys = (0..PROVIDER_LOAD_HARD_CAP)
            .map(|index| provider_load_key_for_test(&format!("timeout-full-{index}")))
            .collect::<Vec<_>>();
        let mut full_spawns = Vec::new();
        for key in &full_keys {
            let (_, spawn) = test_subscribe(&registry, &gate, key).await.unwrap();
            full_spawns.push(spawn.unwrap());
        }
        let queued_key = provider_load_key_for_test("timeout-queued");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let queued = tokio::spawn({
            let registry = Arc::clone(&registry);
            let gate = Arc::clone(&gate);
            let key = queued_key.clone();
            crate::r_subprocess::with_outer_r_deadline(deadline, async move {
                test_subscribe(&registry, &gate, &key).await
            })
        });
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        let result = queued.await.unwrap();
        assert!(matches!(
            result,
            Err(ref error)
                if error.is::<crate::r_subprocess::OuterDeadlineExpired>()
        ));
        assert!(!registry.lock().active.contains_key(&queued_key));
        assert_eq!(gate.available_permits(), 0);

        for (key, spawn) in full_keys.iter().zip(full_spawns) {
            finish_test_spawn(&registry, key, spawn, Ok(empty_provider_load_artifact()));
        }
        assert_eq!(gate.available_permits(), PROVIDER_LOAD_HARD_CAP);
    }

    #[tokio::test]
    async fn racing_same_key_admission_spawns_one_physical_generation() {
        let registry = Arc::new(parking_lot::Mutex::new(ProviderLoadRegistry::default()));
        let gate = Arc::new(tokio::sync::Semaphore::new(PROVIDER_LOAD_HARD_CAP));
        let key = provider_load_key_for_test("racing-same-key");
        let (left, right) = tokio::join!(
            test_subscribe(&registry, &gate, &key),
            test_subscribe(&registry, &gate, &key),
        );
        let (left_receiver, left_spawn) = left.unwrap();
        let (right_receiver, right_spawn) = right.unwrap();
        assert_eq!(
            usize::from(left_spawn.is_some()) + usize::from(right_spawn.is_some()),
            1
        );
        let spawn = left_spawn.or(right_spawn).unwrap();
        let artifact = empty_provider_load_artifact();
        finish_test_spawn(&registry, &key, spawn, Ok(Arc::clone(&artifact)));
        assert!(Arc::ptr_eq(
            &await_provider_load(left_receiver).await.unwrap(),
            &artifact
        ));
        assert!(Arc::ptr_eq(
            &await_provider_load(right_receiver).await.unwrap(),
            &artifact
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_policy_supersedes_paused_old_generation_and_share_follows_new() {
        let registry = Arc::new(parking_lot::Mutex::new(ProviderLoadRegistry::default()));
        let gate = Arc::new(tokio::sync::Semaphore::new(PROVIDER_LOAD_HARD_CAP));
        let key = provider_load_key_for_test("paused-old-new");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("provider-version");
        std::fs::write(&path, "old").unwrap();
        let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let old_path = path.clone();
        let old_receiver = subscribe_or_spawn_provider_load_from(
            &registry,
            &gate,
            &key,
            ProviderLoadPolicy::Share,
            move |completion| {
                spawn_provider_load_thread(completion, move || {
                    let version = std::fs::read_to_string(old_path).unwrap();
                    arrived_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(Arc::new(ProviderLoadArtifact {
                        providers: Vec::new(),
                        notes: vec![version],
                        shipped_db_load: ShippedDbLoad::Absent,
                    }))
                })
            },
        )
        .await
        .unwrap();
        arrived_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        // Same-path atomic replacement on Unix exercises the opened-old-handle
        // versus newly-addressable-path case. Windows' std API requires the
        // destination removal in the helper, but still changes file identity.
        replace_provider_file_for_test(&path, "new");

        let fresh_path = path.clone();
        let (fresh_arrived_tx, fresh_arrived_rx) = std::sync::mpsc::channel();
        let (fresh_release_tx, fresh_release_rx) = std::sync::mpsc::channel();
        let fresh_receiver = subscribe_or_spawn_provider_load_from(
            &registry,
            &gate,
            &key,
            ProviderLoadPolicy::FreshAfterActive,
            move |completion| {
                spawn_provider_load_thread(completion, move || {
                    let version = std::fs::read_to_string(fresh_path).unwrap();
                    fresh_arrived_tx.send(()).unwrap();
                    fresh_release_rx.recv().unwrap();
                    Ok(Arc::new(ProviderLoadArtifact {
                        providers: Vec::new(),
                        notes: vec![version],
                        shipped_db_load: ShippedDbLoad::Absent,
                    }))
                })
            },
        )
        .await
        .unwrap();
        fresh_arrived_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            gate.available_permits(),
            PROVIDER_LOAD_HARD_CAP - 2,
            "old and fresh physical generations overlap"
        );
        let (new_follower, duplicate) = test_subscribe(&registry, &gate, &key).await.unwrap();
        assert!(
            duplicate.is_none(),
            "a later Share caller follows the fresh generation"
        );

        let fresh_generation = registry.lock().active.get(&key).unwrap().generation;
        release_tx.send(()).unwrap();
        let old = await_provider_load(old_receiver).await.unwrap();
        assert_eq!(old.notes, ["old"]);
        let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while gate.available_permits() != PROVIDER_LOAD_HARD_CAP - 1 {
            assert!(
                std::time::Instant::now() < cleanup_deadline,
                "superseded old generation did not release its permit"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            registry.lock().active.get(&key).unwrap().generation,
            fresh_generation,
            "old exact cleanup cannot remove the fresh registry entry"
        );

        fresh_release_tx.send(()).unwrap();
        let fresh = await_provider_load(fresh_receiver).await.unwrap();
        let followed = await_provider_load(new_follower).await.unwrap();
        assert_eq!(fresh.notes, ["new"]);
        assert!(Arc::ptr_eq(&fresh, &followed));
        wait_for_provider_cleanup(&registry, &gate, &key);
    }

    #[test]
    fn physical_provider_completion_crosses_tokio_runtime_shutdown() {
        let registry = Arc::new(parking_lot::Mutex::new(ProviderLoadRegistry::default()));
        let gate = Arc::new(tokio::sync::Semaphore::new(PROVIDER_LOAD_HARD_CAP));
        let key = provider_load_key_for_test("cross-runtime");
        let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let receiver = first_runtime
            .block_on(subscribe_or_spawn_provider_load_from(
                &registry,
                &gate,
                &key,
                ProviderLoadPolicy::Share,
                move |completion| {
                    spawn_provider_load_thread(completion, move || {
                        arrived_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(empty_provider_load_artifact())
                    })
                },
            ))
            .unwrap();
        arrived_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        drop(first_runtime);

        release_tx.send(()).unwrap();
        let second_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        second_runtime.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                await_provider_load(receiver),
            )
            .await
            .expect("a new runtime observes the std-thread completion")
            .unwrap()
        });
        wait_for_provider_cleanup(&registry, &gate, &key);
    }

    #[tokio::test]
    async fn panicking_physical_provider_load_publishes_failure_and_cleans_capacity() {
        let registry = Arc::new(parking_lot::Mutex::new(ProviderLoadRegistry::default()));
        let gate = Arc::new(tokio::sync::Semaphore::new(PROVIDER_LOAD_HARD_CAP));
        let key = provider_load_key_for_test("panicking-provider");
        let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let leader = subscribe_or_spawn_provider_load_from(
            &registry,
            &gate,
            &key,
            ProviderLoadPolicy::Share,
            move |completion| {
                spawn_provider_load_thread(completion, move || -> ProviderLoadResult {
                    arrived_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    panic!("synthetic provider panic");
                })
            },
        )
        .await
        .unwrap();
        arrived_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (follower, duplicate) = test_subscribe(&registry, &gate, &key).await.unwrap();
        assert!(duplicate.is_none());
        release_tx.send(()).unwrap();
        assert!(await_provider_load(leader).await.is_err());
        assert!(await_provider_load(follower).await.is_err());
        wait_for_provider_cleanup(&registry, &gate, &key);
    }

    #[tokio::test]
    async fn provider_thread_spawn_failure_publishes_and_cleans_capacity() {
        let registry = Arc::new(parking_lot::Mutex::new(ProviderLoadRegistry::default()));
        let gate = Arc::new(tokio::sync::Semaphore::new(PROVIDER_LOAD_HARD_CAP));
        let key = provider_load_key_for_test("spawn-failure-provider");
        let result = subscribe_or_spawn_provider_load_from(
            &registry,
            &gate,
            &key,
            ProviderLoadPolicy::Share,
            |_completion| Err(ProviderLoadFailure("synthetic spawn failure".to_string())),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("synthetic spawn failure must reject admission"),
            Err(error) => error,
        };
        assert!(error.is::<ProviderLoadFailure>());
        assert!(error.to_string().contains("synthetic spawn failure"));
        assert!(!registry.lock().active.contains_key(&key));
        assert_eq!(gate.available_permits(), PROVIDER_LOAD_HARD_CAP);
    }

    #[tokio::test]
    async fn provider_load_completion_wakes_all_same_key_followers_and_self_cleans() {
        let registry = Arc::new(parking_lot::Mutex::new(ProviderLoadRegistry::default()));
        let gate = Arc::new(tokio::sync::Semaphore::new(PROVIDER_LOAD_HARD_CAP));
        let key = provider_load_key_for_test("coalesced-provider");
        let (leader, spawn) = test_subscribe(&registry, &gate, &key).await.unwrap();
        let spawn = spawn.expect("first claimant starts the physical load");
        let (follower, duplicate) = test_subscribe(&registry, &gate, &key).await.unwrap();
        assert!(
            duplicate.is_none(),
            "same-key claimant follows without capacity"
        );
        drop(leader);
        assert!(registry.lock().active.contains_key(&key));
        assert_eq!(gate.available_permits(), PROVIDER_LOAD_HARD_CAP - 1);

        let completion_registry = Arc::clone(&registry);
        let completion_key = key.clone();
        let artifact = empty_provider_load_artifact();
        finish_test_spawn(
            &completion_registry,
            &completion_key,
            spawn,
            Ok(Arc::clone(&artifact)),
        );
        assert!(Arc::ptr_eq(
            &await_provider_load(follower).await.unwrap(),
            &artifact
        ));
        assert!(!registry.lock().active.contains_key(&key));
        assert_eq!(gate.available_permits(), PROVIDER_LOAD_HARD_CAP);
    }

    #[tokio::test(start_paused = true)]
    async fn provider_follower_deadline_does_not_cancel_physical_generation() {
        let registry = Arc::new(parking_lot::Mutex::new(ProviderLoadRegistry::default()));
        let gate = Arc::new(tokio::sync::Semaphore::new(PROVIDER_LOAD_HARD_CAP));
        let key = provider_load_key_for_test("deadline-provider");
        let (leader, spawn) = test_subscribe(&registry, &gate, &key).await.unwrap();
        let spawn = spawn.expect("first claimant starts the physical load");
        let (follower, duplicate) = test_subscribe(&registry, &gate, &key).await.unwrap();
        assert!(duplicate.is_none());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let timed_out = tokio::spawn(crate::r_subprocess::with_outer_r_deadline(
            deadline,
            await_provider_load(follower),
        ));
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        let result = timed_out.await.unwrap();
        assert!(matches!(
            result,
            Err(ref error)
                if error.is::<crate::r_subprocess::OuterDeadlineExpired>()
        ));
        assert!(registry.lock().active.contains_key(&key));

        let artifact = empty_provider_load_artifact();
        finish_test_spawn(&registry, &key, spawn, Ok(Arc::clone(&artifact)));
        assert!(Arc::ptr_eq(
            &await_provider_load(leader).await.unwrap(),
            &artifact
        ));
        assert!(!registry.lock().active.contains_key(&key));
        assert_eq!(gate.available_permits(), PROVIDER_LOAD_HARD_CAP);
    }

    #[tokio::test]
    async fn stale_provider_completion_cannot_remove_new_generation() {
        let registry = Arc::new(parking_lot::Mutex::new(ProviderLoadRegistry::default()));
        let key = provider_load_key_for_test("stale-completion");
        let (_new_sender, new_receiver) = tokio::sync::watch::channel(ProviderLoadState::Running);
        registry.lock().active.insert(
            key.clone(),
            ActiveProviderLoad {
                generation: 22,
                completion: new_receiver,
                followers: 0,
            },
        );
        let (stale_sender, _stale_receiver) =
            tokio::sync::watch::channel(ProviderLoadState::Running);
        complete_provider_load(
            Arc::clone(&registry),
            key.clone(),
            11,
            stale_sender,
            Ok(empty_provider_load_artifact()),
        );
        assert_eq!(registry.lock().active.get(&key).unwrap().generation, 22);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_real_builds_share_one_global_provider_load() {
        let _env_guard = RAVEN_NAMES_DB_ENV_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        let workspace_root = root.path().to_path_buf();
        let r_path = workspace_root.join("definitely-not-an-R-executable");
        let key = ProviderLoadKey {
            repo_db_path: Some(workspace_root.join(".raven").join("packages.json")),
            shipped_db_inputs: crate::package_db::capture_shipped_db_candidate_inputs(),
        };
        let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        provider_load_test_pauses().lock().insert(
            key.clone(),
            ProviderLoadTestPause {
                arrived: arrived_tx,
                release: release_rx,
            },
        );

        let first = tokio::spawn(try_build_package_library(
            Some(r_path.clone()),
            &[],
            Some(workspace_root.clone()),
            true,
            ProviderLoadPolicy::Share,
        ));
        tokio::task::spawn_blocking(move || {
            arrived_rx.recv_timeout(std::time::Duration::from_secs(5))
        })
        .await
        .unwrap()
        .unwrap();
        let second = tokio::spawn(try_build_package_library(
            Some(r_path),
            &[],
            Some(workspace_root),
            true,
            ProviderLoadPolicy::Share,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if provider_load_registry()
                    .lock()
                    .active
                    .get(&key)
                    .is_some_and(|active| active.followers == 1)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the second real build follows the first physical load");

        first.abort();
        let leader = first.await;
        assert!(
            matches!(leader, Err(ref error) if error.is_cancelled()),
            "dropping the leader cannot own the physical generation"
        );
        release_tx.send(()).unwrap();
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), second)
            .await
            .expect("the follower survives leader cancellation");
        assert!(second.unwrap().is_ok());
        assert!(!provider_load_registry().lock().active.contains_key(&key));
    }

    /// Seed a `combined_entries` cache entry directly, bypassing `get_all_exports`.
    /// Lets tests construct specific aggregate snapshots — including the
    /// partial/ownerless states the production warm path never builds — without
    /// repeating the copy-on-write publication boilerplate.
    async fn seed_combined_entry(
        lib: &PackageLibrary,
        name: &str,
        exports: HashSet<String>,
        owners: HashMap<String, String>,
    ) {
        let lease = lib.begin_cache_operation().await.unwrap();
        lib.update_combined_entries(&lease, |combined| {
            combined.insert(
                name.to_string(),
                Arc::new(CombinedEntry::new(exports, owners)),
            );
        });
    }

    #[tokio::test]
    async fn build_library_wires_shipped_db_provider_from_env() {
        use crate::package_db::binary_db::{ShippedDbProvenance, write_shipped_db};
        use crate::package_db::model::PackageRecord;

        // Use a synthetic name that cannot exist in any real R install, so the
        // ONLY way it can resolve is via the wired Tier 3 provider. (A real
        // package like `dplyr` would resolve from Tier 1 on a developer machine
        // that has it installed, masking whether providers were wired.)
        let _env_guard = RAVEN_NAMES_DB_ENV_LOCK.lock().await;
        let pkg = "ravenfaketier3pkg";
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("names.db");
        write_shipped_db(
            &db_path,
            &[PackageRecord {
                name: pkg.into(),
                version: "1.1.4".into(),
                exports: vec!["mutate".into()],
                depends: vec![],
                lazy_data: vec![],
            }],
            ShippedDbProvenance {
                source: "test".into(),
                snapshot_date: "2026-05-30".into(),
                package_count: 1,
                raven_version: "9.9.9".into(),
            },
        )
        .unwrap();

        let _db_env = NamesDbEnvGuard::set(&db_path);
        let outcome = build_package_library(None, &[], None, true).await; // runtime path -> wires providers
        let outcome_t1 = build_package_library_tier1_only(None, &[], None).await; // capture path -> no providers

        assert!(
            outcome
                .library
                .get_package(pkg)
                .await
                .expect("Tier 3 resolves synthetic pkg")
                .exports
                .contains("mutate")
        );
        // Provider-less capture must NOT resolve the synthetic pkg from Tier 3.
        assert!(outcome_t1.library.get_package(pkg).await.is_none());
    }

    #[tokio::test]
    async fn build_library_reports_unreadable_shipped_db_in_load_notes() {
        let _env_guard = RAVEN_NAMES_DB_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("names.db");
        // Too short / bad magic => ShippedDb::open returns Corrupt => a load note.
        std::fs::write(&bad, b"NOT A RAVEN DB").unwrap();

        // Point the user-data candidate at an empty dir so the bad env DB is the
        // only shipped-DB candidate that exists — otherwise a real machine-level
        // names.db would load after it and flip the verdict to `Loaded`.
        let empty_user_data = dir.path().join("user-data");
        std::fs::create_dir_all(&empty_user_data).unwrap();
        let _user_data_guard = crate::package_db::test_user_data_dir_guard(empty_user_data);
        let _db_env = NamesDbEnvGuard::set(&bad);
        let outcome = build_package_library(None, &[], None, true).await;

        // The build degrades (does not panic) AND explains the unreadable DB.
        assert!(
            !outcome.load_notes.is_empty(),
            "a corrupt names.db must produce a load note"
        );
        assert!(
            outcome.load_notes.iter().any(|n| n.contains("names.db")),
            "the note should mention names.db; got {:?}",
            outcome.load_notes
        );
        // ...and the structured verdict reports present-but-failed, not absent.
        assert_eq!(outcome.shipped_db_load, ShippedDbLoad::Failed);
    }

    #[tokio::test]
    async fn build_library_falls_back_from_bad_user_db_to_lower_candidate() {
        use crate::package_db::binary_db::{ShippedDbProvenance, write_shipped_db};
        use crate::package_db::model::PackageRecord;

        let _env_guard = RAVEN_NAMES_DB_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let user_data = dir.path().join("data");
        let bad_env_db = dir.path().join("bad.db");
        let good_user_db = user_data.join("names.db");
        std::fs::create_dir_all(&user_data).unwrap();
        std::fs::write(&bad_env_db, b"not a raven db").unwrap();

        let pkg = "ravenlowercandidatepkg";
        write_shipped_db(
            &good_user_db,
            &[PackageRecord {
                name: pkg.into(),
                version: "1.0.0".into(),
                exports: vec!["lower_export".into()],
                depends: vec![],
                lazy_data: vec![],
            }],
            ShippedDbProvenance {
                source: "test".into(),
                snapshot_date: "2026-05-31".into(),
                package_count: 1,
                raven_version: "9.9.9".into(),
            },
        )
        .unwrap();

        let _user_data_guard = crate::package_db::test_user_data_dir_guard(user_data);
        let _db_env = NamesDbEnvGuard::set(&bad_env_db);
        let outcome = build_package_library(None, &[], None, true).await;

        assert!(outcome.load_notes.iter().any(|n| n.contains("bad.db")));
        assert!(
            outcome
                .library
                .get_package(pkg)
                .await
                .expect("lower candidate provider resolves")
                .exports
                .contains("lower_export")
        );
        // A failed candidate followed by a working one resolves to `Loaded`:
        // the working DB was searched, so `freeze` would be the right advice.
        assert_eq!(outcome.shipped_db_load, ShippedDbLoad::Loaded);
    }

    /// Pins the readiness predicate and degradation precedence
    /// platform-independently. The `Some(_)` rows exercise Tier-1
    /// classification directly; the public builder can also produce
    /// `InitFailed` for provider admission, deadline, spawn, or load failures.
    #[test]
    fn classify_truth_table() {
        use PackageLibraryStatus::*;
        let none: Option<String> = None;
        let err = || Some("boom".to_string());
        // (init_error, r_found, has_lib_paths) -> status
        assert_eq!(
            PackageLibraryStatus::classify(none.clone(), true, true),
            Ready
        );
        assert_eq!(
            PackageLibraryStatus::classify(none.clone(), true, false),
            NoLibraryPaths
        );
        assert_eq!(
            PackageLibraryStatus::classify(none.clone(), false, true),
            Ready
        );
        assert_eq!(
            PackageLibraryStatus::classify(none, false, false),
            RNotFound
        );
        assert_eq!(
            PackageLibraryStatus::classify(err(), true, true),
            InitFailed("boom".to_string())
        );
        assert_eq!(
            PackageLibraryStatus::classify(err(), true, false),
            InitFailed("boom".to_string())
        );
        assert_eq!(
            PackageLibraryStatus::classify(err(), false, true),
            RNotFound
        );
        assert_eq!(
            PackageLibraryStatus::classify(err(), false, false),
            RNotFound
        );
    }

    #[test]
    fn is_ready_only_for_ready() {
        use PackageLibraryStatus::*;
        assert!(Ready.is_ready());
        for s in [Disabled, RNotFound, InitFailed("x".into()), NoLibraryPaths] {
            assert!(!s.is_ready(), "{s:?} must not be ready");
        }
    }

    #[test]
    fn consumer_ready_false_for_r_not_found_with_empty_library() {
        let outcome = PackageLibraryOutcome {
            library: Arc::new(PackageLibrary::new_empty()),
            status: PackageLibraryStatus::RNotFound,
            load_notes: Vec::new(),
            shipped_db_load: ShippedDbLoad::Absent,
        };

        assert!(!outcome.consumer_ready());
    }

    #[test]
    fn consumer_ready_true_for_r_not_found_with_base_exports() {
        let mut lib = PackageLibrary::new_empty();
        lib.set_base_exports(HashSet::from(["print".to_string()]));
        let outcome = PackageLibraryOutcome {
            library: Arc::new(lib),
            status: PackageLibraryStatus::RNotFound,
            load_notes: Vec::new(),
            shipped_db_load: ShippedDbLoad::Absent,
        };

        assert!(outcome.consumer_ready());
    }

    #[test]
    fn consumer_ready_true_for_r_not_found_with_providers() {
        use crate::package_db::PackageMetadataProvider;

        struct Fake;
        impl PackageMetadataProvider for Fake {
            fn lookup(&self, _: &str) -> Option<PackageInfo> {
                None
            }
        }

        let mut lib = PackageLibrary::new_empty();
        lib.set_providers(vec![Box::new(Fake)]);
        let outcome = PackageLibraryOutcome {
            library: Arc::new(lib),
            status: PackageLibraryStatus::RNotFound,
            load_notes: Vec::new(),
            shipped_db_load: ShippedDbLoad::Absent,
        };

        assert!(outcome.consumer_ready());
    }

    #[test]
    fn consumer_ready_true_for_ready() {
        let outcome = PackageLibraryOutcome {
            library: Arc::new(PackageLibrary::new_empty()),
            status: PackageLibraryStatus::Ready,
            load_notes: Vec::new(),
            shipped_db_load: ShippedDbLoad::Absent,
        };

        assert!(outcome.consumer_ready());
    }

    #[tokio::test]
    async fn build_package_library_disabled_is_empty_and_not_ready() {
        let outcome = build_package_library(None, &[], None, false).await;
        assert_eq!(outcome.status, PackageLibraryStatus::Disabled);
        assert!(!outcome.status.is_ready());
        assert!(outcome.library.lib_paths().is_empty());
    }

    /// A real temp dir is used because `add_library_paths` appends without an
    /// existence check, so this reflects the "valid additional path" intent.
    /// R-independent: additional paths are applied after R discovery.
    #[tokio::test]
    async fn build_package_library_honors_additional_paths() {
        let extra = tempfile::TempDir::new().unwrap();
        let extra_path = extra.path().to_path_buf();
        let outcome =
            build_package_library(None, std::slice::from_ref(&extra_path), None, true).await;
        assert!(
            outcome.library.lib_paths().iter().any(|p| p == &extra_path),
            "additional path must appear in lib_paths; got {:?}",
            outcome.library.lib_paths()
        );
    }

    #[test]
    fn test_package_info_new() {
        let mut exports = HashSet::new();
        exports.insert("func1".to_string());
        exports.insert("func2".to_string());

        let info = PackageInfo::new("testpkg".to_string(), exports.clone());

        assert_eq!(info.name, "testpkg");
        assert_eq!(info.exports, exports);
        assert!(info.depends.is_empty());
        assert!(!info.is_meta_package);
        assert!(info.attached_packages.is_empty());
        assert!(info.lazy_data.is_empty());
    }

    #[test]
    fn test_package_info_tidyverse_is_meta_package() {
        let info = PackageInfo::new("tidyverse".to_string(), HashSet::new());

        assert!(info.is_meta_package);
        assert!(!info.attached_packages.is_empty());
        assert!(info.attached_packages.contains(&"dplyr".to_string()));
        assert!(info.attached_packages.contains(&"ggplot2".to_string()));
        assert!(info.attached_packages.contains(&"tidyr".to_string()));
    }

    #[test]
    fn test_package_info_tidymodels_is_meta_package() {
        let info = PackageInfo::new("tidymodels".to_string(), HashSet::new());

        assert!(info.is_meta_package);
        assert!(!info.attached_packages.is_empty());
        assert!(info.attached_packages.contains(&"parsnip".to_string()));
        assert!(info.attached_packages.contains(&"recipes".to_string()));
        assert!(info.attached_packages.contains(&"yardstick".to_string()));
    }

    #[test]
    fn test_package_info_with_details() {
        let mut exports = HashSet::new();
        exports.insert("func1".to_string());

        let depends = vec!["dep1".to_string(), "dep2".to_string()];
        let lazy_data = vec!["dataset1".to_string()];

        let info = PackageInfo::with_details(
            "testpkg".to_string(),
            exports.clone(),
            depends.clone(),
            lazy_data.clone(),
        );

        assert_eq!(info.name, "testpkg");
        assert_eq!(info.exports, exports);
        assert_eq!(info.depends, depends);
        assert_eq!(info.lazy_data, lazy_data);
        assert!(!info.is_meta_package);
    }

    #[test]
    fn test_package_info_with_details_meta_matches_new() {
        // Pins that `with_details` derives the meta-package fields identically
        // to `new` — the invariant the two constructors must share. Guards the
        // single derivation helper against the two constructors drifting apart.
        let via_details =
            PackageInfo::with_details("tidyverse".to_string(), HashSet::new(), vec![], vec![]);
        let via_new = PackageInfo::new("tidyverse".to_string(), HashSet::new());

        assert!(via_details.is_meta_package);
        assert_eq!(via_details.is_meta_package, via_new.is_meta_package);
        assert_eq!(via_details.attached_packages, via_new.attached_packages);
        assert!(via_details.attached_packages.contains(&"dplyr".to_string()));
    }

    #[tokio::test]
    async fn test_package_library_new_empty() {
        let lib = PackageLibrary::new_empty();

        assert!(lib.lib_paths().is_empty());
        assert!(lib.base_packages().is_empty());
        assert!(lib.base_exports().is_empty());
        assert_eq!(lib.cached_count().await, 0);
    }

    #[tokio::test]
    async fn test_package_library_insert_and_get() {
        let lib = PackageLibrary::new_empty();

        let mut exports = HashSet::new();
        exports.insert("mutate".to_string());
        exports.insert("filter".to_string());

        let info = PackageInfo::new("dplyr".to_string(), exports);
        lib.insert_package(info).await;

        assert!(lib.is_cached("dplyr").await);
        assert!(!lib.is_cached("ggplot2").await);

        let cached = lib.get_cached_package("dplyr").await;
        assert!(cached.is_some());
        let cached = cached.unwrap();
        assert_eq!(cached.name, "dplyr");
        assert!(cached.exports.contains("mutate"));
        assert!(cached.exports.contains("filter"));
    }

    #[tokio::test]
    async fn test_package_library_invalidate() {
        let lib = PackageLibrary::new_empty();

        let info = PackageInfo::new("testpkg".to_string(), HashSet::new());
        lib.insert_package(info).await;
        lib.insert_package(PackageInfo::with_details(
            "dependent".to_string(),
            HashSet::new(),
            vec!["testpkg".to_string()],
            Vec::new(),
        ))
        .await;
        seed_combined_entry(
            &lib,
            "testpkg",
            ["stale_export".to_string()].into_iter().collect(),
            HashMap::new(),
        )
        .await;
        seed_combined_entry(
            &lib,
            "dependent",
            ["dependent_export".to_string()].into_iter().collect(),
            HashMap::new(),
        )
        .await;

        assert!(lib.is_cached("testpkg").await);
        assert!(lib.combined_entries.load().contains_key("testpkg"));
        assert!(lib.combined_entries.load().contains_key("dependent"));

        lib.invalidate("testpkg").await;

        assert!(!lib.is_cached("testpkg").await);
        assert!(!lib.combined_entries.load().contains_key("testpkg"));
        assert!(!lib.combined_entries.load().contains_key("dependent"));
    }

    #[tokio::test]
    async fn test_package_exists_ignores_cache() {
        // prefetch_packages() inserts a cache entry even for packages that R
        // can't find: the R query wraps `asNamespace()` in `tryCatch(...,
        // error = function(e) {})`, so a not-installed package emits its
        // `__PKG:name__` marker followed by zero export lines, and the
        // resulting empty-exports entry gets inserted into the cache. If
        // `package_exists()` trusts that cache, it returns true for a package
        // that isn't actually installed — and "Package not installed"
        // diagnostics get permanently suppressed. Existence must be determined
        // from base_packages and the filesystem only.
        let lib = PackageLibrary::new_empty();

        // Empty lib_paths means find_package_directory() returns None for any
        // name. Cache-only "existence" is the bug we're guarding against.
        assert!(
            lib.lib_paths().is_empty(),
            "test precondition: lib_paths must be empty"
        );

        lib.insert_package(PackageInfo::new(
            "__raven_not_installed__".to_string(),
            HashSet::new(),
        ))
        .await;

        assert!(
            lib.is_cached("__raven_not_installed__").await,
            "test precondition: package must be cached"
        );
        assert!(
            !lib.package_exists("__raven_not_installed__"),
            "package_exists must return false for cached-but-not-on-disk packages"
        );
    }

    #[tokio::test]
    async fn missing_export_metadata_reports_provider_miss() {
        let lib = PackageLibrary::new_empty();
        assert!(lib.export_metadata_missing("ravenmissingpkg").await);
    }

    #[tokio::test]
    async fn missing_export_metadata_reports_empty_cached_provider_miss() {
        let lib = PackageLibrary::new_empty();
        let pkg = "ravenemptycachedpkg";
        lib.insert_package(PackageInfo::new(pkg.into(), HashSet::new()))
            .await;

        assert!(lib.export_metadata_missing(pkg).await);
    }

    #[tokio::test]
    async fn missing_export_metadata_ignores_provider_hit() {
        use crate::package_db::PackageMetadataProvider;

        struct Fake;
        impl PackageMetadataProvider for Fake {
            fn lookup(&self, name: &str) -> Option<PackageInfo> {
                (name == "ravenproviderpkg")
                    .then(|| PackageInfo::new("ravenproviderpkg".into(), HashSet::new()))
            }
        }

        let mut lib = PackageLibrary::new_empty();
        lib.set_providers(vec![Box::new(Fake)]);

        assert!(!lib.export_metadata_missing("ravenproviderpkg").await);
    }

    #[tokio::test]
    async fn test_package_library_clear_cache() {
        let lib = PackageLibrary::new_empty();

        lib.insert_package(PackageInfo::new("pkg1".to_string(), HashSet::new()))
            .await;
        lib.insert_package(PackageInfo::new("pkg2".to_string(), HashSet::new()))
            .await;
        // Seed a combined_entries entry to verify it is also cleared.
        seed_combined_entry(
            &lib,
            "pkg1",
            ["foo".to_string()].into_iter().collect(),
            HashMap::new(),
        )
        .await;

        assert_eq!(lib.cached_count().await, 2);
        assert!(lib.combined_entries.load().contains_key("pkg1"));

        lib.clear_cache().await;

        assert_eq!(lib.cached_count().await, 0);
        assert!(lib.combined_entries.load().is_empty());
    }

    #[tokio::test]
    async fn test_package_library_is_package_export() {
        let lib = PackageLibrary::new_empty();

        let mut exports = HashSet::new();
        exports.insert("mutate".to_string());
        exports.insert("filter".to_string());

        let info = PackageInfo::new("dplyr".to_string(), exports);
        lib.insert_package(info).await;

        assert!(lib.is_package_export("mutate", "dplyr").await);
        assert!(lib.is_package_export("filter", "dplyr").await);
        assert!(!lib.is_package_export("ggplot", "dplyr").await);
        assert!(!lib.is_package_export("mutate", "ggplot2").await);
    }

    #[test]
    fn test_package_library_is_base_export() {
        let mut lib = PackageLibrary::new_empty();

        let mut base_exports = HashSet::new();
        base_exports.insert("print".to_string());
        base_exports.insert("cat".to_string());
        base_exports.insert("sum".to_string());

        lib.set_base_exports(base_exports);

        assert!(lib.is_base_export("print"));
        assert!(lib.is_base_export("cat"));
        assert!(lib.is_base_export("sum"));
        assert!(!lib.is_base_export("mutate"));
    }

    #[test]
    fn test_package_library_is_base_package() {
        let mut lib = PackageLibrary::new_empty();

        let mut base_packages = HashSet::new();
        base_packages.insert("base".to_string());
        base_packages.insert("stats".to_string());
        base_packages.insert("utils".to_string());

        lib.set_base_packages(base_packages);

        assert!(lib.is_base_package("base"));
        assert!(lib.is_base_package("stats"));
        assert!(lib.is_base_package("utils"));
        assert!(!lib.is_base_package("dplyr"));
    }

    #[test]
    fn test_tidyverse_packages_constant() {
        // Verify the tidyverse packages list matches the requirement
        assert!(TIDYVERSE_PACKAGES.contains(&"dplyr"));
        assert!(TIDYVERSE_PACKAGES.contains(&"readr"));
        assert!(TIDYVERSE_PACKAGES.contains(&"forcats"));
        assert!(TIDYVERSE_PACKAGES.contains(&"stringr"));
        assert!(TIDYVERSE_PACKAGES.contains(&"ggplot2"));
        assert!(TIDYVERSE_PACKAGES.contains(&"tibble"));
        assert!(TIDYVERSE_PACKAGES.contains(&"lubridate"));
        assert!(TIDYVERSE_PACKAGES.contains(&"tidyr"));
        assert!(TIDYVERSE_PACKAGES.contains(&"purrr"));
        assert_eq!(TIDYVERSE_PACKAGES.len(), 9);
    }

    #[test]
    fn test_tidymodels_packages_constant() {
        // Verify the tidymodels packages list matches the requirement
        assert!(TIDYMODELS_PACKAGES.contains(&"broom"));
        assert!(TIDYMODELS_PACKAGES.contains(&"dials"));
        assert!(TIDYMODELS_PACKAGES.contains(&"dplyr"));
        assert!(TIDYMODELS_PACKAGES.contains(&"ggplot2"));
        assert!(TIDYMODELS_PACKAGES.contains(&"infer"));
        assert!(TIDYMODELS_PACKAGES.contains(&"modeldata"));
        assert!(TIDYMODELS_PACKAGES.contains(&"parsnip"));
        assert!(TIDYMODELS_PACKAGES.contains(&"purrr"));
        assert!(TIDYMODELS_PACKAGES.contains(&"recipes"));
        assert!(TIDYMODELS_PACKAGES.contains(&"rsample"));
        assert!(TIDYMODELS_PACKAGES.contains(&"tibble"));
        assert!(TIDYMODELS_PACKAGES.contains(&"tidyr"));
        assert!(TIDYMODELS_PACKAGES.contains(&"tune"));
        assert!(TIDYMODELS_PACKAGES.contains(&"workflows"));
        assert!(TIDYMODELS_PACKAGES.contains(&"workflowsets"));
        assert!(TIDYMODELS_PACKAGES.contains(&"yardstick"));
        assert_eq!(TIDYMODELS_PACKAGES.len(), 16);
    }

    #[tokio::test]
    async fn test_concurrent_read_access() {
        // Test that multiple readers can access the cache concurrently
        // This validates Requirement 13.4
        let lib = Arc::new(PackageLibrary::new_empty());

        // Insert some packages
        let mut exports = HashSet::new();
        exports.insert("func1".to_string());
        lib.insert_package(PackageInfo::new("pkg1".to_string(), exports.clone()))
            .await;
        lib.insert_package(PackageInfo::new("pkg2".to_string(), exports))
            .await;

        // Spawn multiple concurrent readers
        let mut handles = Vec::new();
        for i in 0..10 {
            let lib_clone = Arc::clone(&lib);
            let handle = tokio::spawn(async move {
                let pkg_name = if i % 2 == 0 { "pkg1" } else { "pkg2" };
                let cached = lib_clone.get_cached_package(pkg_name).await;
                assert!(cached.is_some());
                cached.unwrap().name.clone()
            });
            handles.push(handle);
        }

        // Wait for all readers to complete
        for handle in handles {
            let result = handle.await;
            assert!(result.is_ok());
        }
    }

    #[tokio::test]
    async fn sync_reader_uses_published_snapshot_while_package_writer_gate_is_held() {
        let lib = Arc::new(PackageLibrary::new_empty());
        let package = "ravencontentionpkg".to_string();
        let symbol = "raven_contention_symbol".to_string();
        {
            let mut exports = HashSet::new();
            exports.insert(symbol.clone());
            lib.insert_package(PackageInfo::new(package.clone(), exports))
                .await;
        }

        let publish_gate = lib.packages_write.lock();
        let (tx, rx) = std::sync::mpsc::channel();
        let lib_for_reader = Arc::clone(&lib);
        let package_for_reader = package.clone();
        let symbol_for_reader = symbol.clone();
        let reader = std::thread::spawn(move || {
            let loaded = vec![package_for_reader];
            tx.send(lib_for_reader.is_symbol_from_loaded_packages(&symbol_for_reader, &loaded))
                .expect("reader result receiver should be alive");
        });

        let result = match rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(value) => value,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("snapshot reader blocked while package publication gate was held")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("reader thread disconnected before sending a result")
            }
        };

        drop(publish_gate);
        reader.join().expect("reader thread should not panic");
        assert!(
            result,
            "sync reader must observe the last published package snapshot"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sync_readers_stay_correct_during_unrelated_package_churn() {
        let lib = Arc::new(PackageLibrary::new_empty());
        let stable_package = "ravenstablepkg".to_string();
        let stable_symbol = "raven_stable_symbol".to_string();
        {
            let mut exports = HashSet::new();
            exports.insert(stable_symbol.clone());
            lib.insert_package(PackageInfo::new(stable_package.clone(), exports))
                .await;
        }

        let mut tasks = Vec::new();
        for writer_id in 0..4 {
            let lib = Arc::clone(&lib);
            tasks.push(tokio::spawn(async move {
                for i in 0..100 {
                    let package = format!("ravenchurnpkg{writer_id}_{i}");
                    let mut exports = HashSet::new();
                    exports.insert(format!("raven_churn_symbol_{writer_id}_{i}"));
                    lib.insert_package(PackageInfo::new(package.clone(), exports))
                        .await;
                    let to_invalidate: HashSet<String> = [package].into_iter().collect();
                    lib.invalidate_many(&to_invalidate).await;
                    tokio::task::yield_now().await;
                }
            }));
        }

        for _ in 0..4 {
            let lib = Arc::clone(&lib);
            let loaded = vec![stable_package.clone()];
            let stable_package = stable_package.clone();
            let stable_symbol = stable_symbol.clone();
            tasks.push(tokio::spawn(async move {
                for _ in 0..200 {
                    assert!(
                        lib.is_symbol_from_loaded_packages(&stable_symbol, &loaded),
                        "stable symbol must remain available during unrelated cache churn"
                    );
                    assert_eq!(
                        lib.find_package_owner_for_symbol(&stable_symbol, &loaded),
                        Some(stable_package.clone())
                    );
                    let completions = lib.get_owned_exports_for_completions(&loaded);
                    assert_eq!(
                        completions.get(&stable_symbol),
                        Some(&vec![stable_package.clone()])
                    );
                    tokio::task::yield_now().await;
                }
            }));
        }

        for task in tasks {
            task.await.expect("contention stress task should not panic");
        }
    }

    // Tests for initialize() - Task 3.2

    #[tokio::test]
    async fn test_initialize_without_r_subprocess_uses_fallback() {
        // Test that initialize() uses fallback values when R subprocess is None
        // Requirement 6.2: IF R subprocess is unavailable, use hardcoded base packages
        let mut lib = PackageLibrary::with_subprocess(None);
        let result = lib.initialize().await;

        assert!(result.is_ok(), "initialize() should succeed even without R");

        // Should have fallback base packages
        let fallback_packages = crate::r_subprocess::get_fallback_base_packages();
        for pkg in &fallback_packages {
            assert!(
                lib.is_base_package(pkg),
                "Should have fallback base package '{}'",
                pkg
            );
        }

        // base_exports will be empty without R subprocess (can't query exports)
        // This is expected behavior - we can't get exports without R
    }

    #[tokio::test]
    async fn test_initialize_with_r_subprocess() {
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let mut lib = PackageLibrary::with_subprocess(Some(r_subprocess));
        let result = lib.initialize().await;

        assert!(result.is_ok(), "initialize() should succeed with R");

        // Should have library paths
        assert!(
            !lib.lib_paths().is_empty(),
            "Should have at least one library path"
        );

        // Should have base packages
        assert!(!lib.base_packages().is_empty(), "Should have base packages");
        assert!(lib.is_base_package("base"), "Should have 'base' package");
        assert!(lib.is_base_package("stats"), "Should have 'stats' package");
        assert!(lib.is_base_package("utils"), "Should have 'utils' package");

        // Should have base exports (from querying base packages)
        assert!(!lib.base_exports().is_empty(), "Should have base exports");
        // Common base functions should be in exports
        assert!(
            lib.is_base_export("print"),
            "Should have 'print' in base exports"
        );
        assert!(
            lib.is_base_export("cat"),
            "Should have 'cat' in base exports"
        );
    }

    #[tokio::test]
    async fn test_new_constructor_initializes() {
        // Test the convenience new() constructor
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Should be initialized
        assert!(
            !lib.lib_paths().is_empty(),
            "new() should initialize lib_paths"
        );
        assert!(
            !lib.base_packages().is_empty(),
            "new() should initialize base_packages"
        );
        assert!(
            !lib.base_exports().is_empty(),
            "new() should initialize base_exports"
        );
    }

    #[tokio::test]
    async fn test_new_constructor_without_r() {
        // Test new() constructor without R subprocess
        let lib = PackageLibrary::new(None).await;

        // Should have fallback base packages
        let fallback_packages = crate::r_subprocess::get_fallback_base_packages();
        for pkg in &fallback_packages {
            assert!(
                lib.is_base_package(pkg),
                "new(None) should have fallback base package '{}'",
                pkg
            );
        }
    }

    #[tokio::test]
    async fn test_initialize_base_exports_contain_common_functions() {
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Requirement 6.3: Base packages shall be available at all positions
        // These are common functions that should be in base exports
        let common_base_functions = [
            "print", "cat", "c", "list", "length", "sum", "mean", "paste", "paste0", "sprintf",
            "format",
        ];

        for func in &common_base_functions {
            assert!(
                lib.is_base_export(func),
                "Base exports should contain common function '{}'",
                func
            );
        }
    }

    /// Regression test for issue #276: base-package data objects must be in scope.
    ///
    /// `datasets` is auto-attached at R startup, so its data objects (`mtcars`,
    /// `iris`, `airquality`, `ChickWeight`, etc.) should be treated as defined
    /// at every position in every R file. These items are lazy-loaded and don't
    /// appear in `getNamespaceExports()` or NAMESPACE `export(...)` lines, so
    /// they require dedicated discovery from the package's INDEX / data/ layout.
    #[tokio::test]
    async fn test_initialize_base_exports_contain_dataset_symbols() {
        // Initialize without R subprocess - the fix must work statically via
        // INDEX file and data/ directory enumeration.
        let mut lib = PackageLibrary::with_subprocess(None);
        lib.initialize().await.expect("initialize() should succeed");

        // Skip the assertion if no library was discovered on the test host
        // (CI without R installed); the fix can't be exercised then.
        let datasets_dir_found = lib
            .lib_paths()
            .iter()
            .any(|p| p.join("datasets").join("INDEX").exists());
        if !datasets_dir_found {
            return;
        }

        // The data objects from `datasets` that should be in base_exports.
        let dataset_objects = ["mtcars", "iris", "airquality", "ChickWeight"];
        for name in &dataset_objects {
            assert!(
                lib.is_base_export(name),
                "Base exports should contain `datasets` object '{}' (issue #276)",
                name
            );
        }
    }

    /// Regression test for issue #276 that does NOT depend on a real R install.
    ///
    /// Builds a fake `datasets`-style package on disk and runs the real
    /// `initialize()` against it (via a pre-set `lib_paths`). This proves the
    /// production wiring — not just the helper functions — picks up the
    /// dataset symbols. Runs on CI hosts without R, so a future regression
    /// that drops the dataset path from `initialize()` will fail loudly here.
    #[tokio::test]
    async fn test_initialize_picks_up_datasets_from_fake_library() {
        let lib_root = tempfile::tempdir().unwrap();

        // All seven base packages need a minimal directory so
        // `find_package_directory` returns Some for each — otherwise
        // `initialize()` skips them silently and we lose coverage.
        for pkg in [
            "base",
            "methods",
            "utils",
            "grDevices",
            "graphics",
            "stats",
            "datasets",
        ] {
            let dir = lib_root.path().join(pkg);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("DESCRIPTION"),
                format!("Package: {}\nVersion: 4.6.0\nPriority: base\n", pkg),
            )
            .unwrap();
            // `base` has no NAMESPACE in real installs — leave it absent so
            // it hits the existing INDEX-fallback branch.
            if pkg != "base" {
                std::fs::write(
                    dir.join("NAMESPACE"),
                    "# minimal NAMESPACE for fake-library test\n",
                )
                .unwrap();
            }
        }

        // Datasets-specific contents: empty NAMESPACE, INDEX listing topic
        // names, and the lazy-load sentinel `Rdata.rdx` (so the data items
        // come from INDEX rather than individual `.rda` files).
        let datasets_dir = lib_root.path().join("datasets");
        let data_dir = datasets_dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(
            datasets_dir.join("NAMESPACE"),
            "# This package exports nothing (it uses lazydata)\n# exportPattern(\".\")\n",
        )
        .unwrap();
        std::fs::write(
            datasets_dir.join("INDEX"),
            "mtcars                  Motor Trend Car Road Tests\n\
             iris                    Edgar Anderson's Iris Data\n\
             airquality              New York Air Quality Measurements\n\
             ChickWeight             Weight Versus Age of Chicks on Different Diets\n",
        )
        .unwrap();
        std::fs::write(data_dir.join("Rdata.rdx"), b"").unwrap();
        std::fs::write(data_dir.join("Rdata.rdb"), b"").unwrap();
        std::fs::write(data_dir.join("Rdata.rds"), b"").unwrap();
        // Also a non-lazy data file to confirm enumeration works alongside INDEX.
        std::fs::write(data_dir.join("morley.tab"), b"").unwrap();

        let mut lib = PackageLibrary::with_subprocess(None);
        // Pre-populate lib_paths; `initialize()` respects an existing value
        // and only calls the platform fallback when empty.
        lib.set_lib_paths(vec![lib_root.path().to_path_buf()]);
        lib.initialize().await.expect("initialize() succeeds");

        for name in [
            "mtcars",
            "iris",
            "airquality",
            "ChickWeight",
            // `morley` comes from `data/morley.tab` enumeration.
            "morley",
        ] {
            assert!(
                lib.is_base_export(name),
                "Expected `{}` in base_exports after initialize() (issue #276); \
                 lib_paths={:?}, base_exports_len={}",
                name,
                lib.lib_paths(),
                lib.base_exports().len(),
            );
        }
    }

    /// Regression test for issue #276: INDEX *topics* under-enumerate
    /// multi-object datasets.
    ///
    /// When `datasets` bundles its data into `Rdata.rdb`, `initialize()` falls
    /// back to INDEX help topics — but a topic like `state` covers several
    /// objects (`state.x77`, `state.region`, `state.abb`, ...) and `stackloss`
    /// covers `stack.x` / `stack.loss`. The embedded base table shipped with
    /// Raven carries the accurate per-object list, so the installed path must
    /// union it in as a floor under the disk-derived names.
    #[tokio::test]
    async fn test_initialize_unions_embedded_datasets_under_index_topics() {
        let lib_root = tempfile::tempdir().unwrap();

        for pkg in [
            "base",
            "methods",
            "utils",
            "grDevices",
            "graphics",
            "stats",
            "datasets",
        ] {
            let dir = lib_root.path().join(pkg);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("DESCRIPTION"),
                format!("Package: {}\nVersion: 4.6.0\nPriority: base\n", pkg),
            )
            .unwrap();
            if pkg != "base" {
                std::fs::write(
                    dir.join("NAMESPACE"),
                    "# minimal NAMESPACE for fake-library test\n",
                )
                .unwrap();
            }
        }

        // Like a real install: data bundled into Rdata.rdb, so enumeration
        // falls back to INDEX topics — which list `state` and `stackloss`
        // rather than the individual objects they document.
        let datasets_dir = lib_root.path().join("datasets");
        let data_dir = datasets_dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(
            datasets_dir.join("INDEX"),
            "mtcars                  Motor Trend Car Road Tests\n\
             state                   US State Facts and Figures\n\
             stackloss               Brownlee's Stack Loss Plant Data\n",
        )
        .unwrap();
        std::fs::write(data_dir.join("Rdata.rdx"), b"").unwrap();
        std::fs::write(data_dir.join("Rdata.rdb"), b"").unwrap();
        std::fs::write(data_dir.join("Rdata.rds"), b"").unwrap();

        let mut lib = PackageLibrary::with_subprocess(None);
        lib.set_lib_paths(vec![lib_root.path().to_path_buf()]);
        lib.initialize().await.expect("initialize() succeeds");

        // Disk-derived INDEX topics still merge on top...
        assert!(
            lib.is_base_export("mtcars"),
            "single-object INDEX topic should still be enumerated"
        );
        // ...and the embedded floor supplies the per-object names that the
        // multi-object INDEX topics hide.
        for name in [
            "state.x77",
            "state.region",
            "state.abb",
            "stack.x",
            "stack.loss",
        ] {
            assert!(
                lib.is_base_export(name),
                "Expected `{}` in base_exports via the embedded datasets floor \
                 (issue #276: INDEX topic `state`/`stackloss` hides it)",
                name
            );
        }
    }

    #[tokio::test]
    async fn test_initialize_skips_unusable_package_dir_for_later_valid_dir() {
        let unusable_root = tempfile::tempdir().unwrap();
        let valid_root = tempfile::tempdir().unwrap();

        let unusable_datasets = unusable_root.path().join("datasets");
        std::fs::create_dir_all(unusable_datasets.join("NAMESPACE")).unwrap();

        let valid_datasets = valid_root.path().join("datasets");
        let valid_data = valid_datasets.join("data");
        std::fs::create_dir_all(&valid_data).unwrap();
        std::fs::write(
            valid_datasets.join("DESCRIPTION"),
            "Package: datasets\nVersion: 4.6.0\nPriority: base\n",
        )
        .unwrap();
        std::fs::write(
            valid_datasets.join("NAMESPACE"),
            "# This package exports nothing (it uses lazydata)\n",
        )
        .unwrap();
        std::fs::write(
            valid_datasets.join("INDEX"),
            "mtcars                  Motor Trend Car Road Tests\n",
        )
        .unwrap();
        std::fs::write(valid_data.join("Rdata.rdx"), b"").unwrap();

        let mut lib = PackageLibrary::with_subprocess(None);
        lib.set_lib_paths(vec![
            unusable_root.path().to_path_buf(),
            valid_root.path().to_path_buf(),
        ]);
        lib.initialize().await.expect("initialize() succeeds");

        assert!(
            lib.is_base_export("mtcars"),
            "initialize() should keep searching after an unusable package directory"
        );
    }

    #[tokio::test]
    async fn test_initialize_lib_paths_exist() {
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // All returned lib_paths should exist
        for path in lib.lib_paths() {
            assert!(path.exists(), "Library path {:?} should exist", path);
        }
    }

    #[tokio::test]
    async fn test_initialize_idempotent() {
        // Test that calling initialize() multiple times is safe
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let mut lib = PackageLibrary::with_subprocess(Some(r_subprocess));

        // Initialize twice
        let result1 = lib.initialize().await;
        let base_packages_count = lib.base_packages().len();
        let base_exports_count = lib.base_exports().len();

        let result2 = lib.initialize().await;

        assert!(result1.is_ok());
        assert!(result2.is_ok());

        // Should have same results after second initialization
        assert_eq!(
            lib.base_packages().len(),
            base_packages_count,
            "Base packages count should be same after re-initialization"
        );
        assert_eq!(
            lib.base_exports().len(),
            base_exports_count,
            "Base exports count should be same after re-initialization"
        );
    }

    // ============================================================================
    // Tests for get_package() - Task 3.3
    // ============================================================================

    #[tokio::test]
    async fn test_get_package_returns_cached() {
        // Test that get_package returns cached package if available
        let lib = PackageLibrary::new_empty();

        // Pre-populate cache
        let mut exports = HashSet::new();
        exports.insert("mutate".to_string());
        exports.insert("filter".to_string());
        let info = PackageInfo::new("dplyr".to_string(), exports);
        lib.insert_package(info).await;

        // get_package should return the cached version
        let result = lib.get_package("dplyr").await;
        assert!(result.is_some());
        let pkg = result.unwrap();
        assert_eq!(pkg.name, "dplyr");
        assert!(pkg.exports.contains("mutate"));
        assert!(pkg.exports.contains("filter"));
    }

    #[tokio::test]
    async fn test_get_package_with_r_subprocess() {
        // Test that get_package queries R subprocess for non-cached packages
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Query a base package that should be installed
        let result = lib.get_package("base").await;
        assert!(result.is_some(), "Should be able to load 'base' package");
        let pkg = result.unwrap();
        assert_eq!(pkg.name, "base");
        assert!(!pkg.exports.is_empty(), "base package should have exports");

        // Common base functions should be in exports
        assert!(pkg.exports.contains("print"), "base should export 'print'");
        assert!(pkg.exports.contains("cat"), "base should export 'cat'");
        assert!(pkg.exports.contains("c"), "base should export 'c'");
    }

    #[tokio::test]
    async fn test_get_package_caches_result() {
        // Test that get_package caches the result after loading
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Base packages (like stats) are now pre-cached during initialization.
        // Verify stats is cached and that repeated calls return the same Arc.
        assert!(lib.is_cached("stats").await);
        let result1 = lib.get_package("stats").await;
        assert!(result1.is_some());

        // Second call should return cached version
        let result2 = lib.get_package("stats").await;
        assert!(result2.is_some());

        // Both should be the same Arc
        assert!(Arc::ptr_eq(&result1.unwrap(), &result2.unwrap()));
    }

    #[tokio::test]
    async fn test_get_package_tidyverse_is_meta_package() {
        // Test that tidyverse is recognized as a meta-package
        // Skip if R is not available or tidyverse is not installed
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        let result = lib.get_package("tidyverse").await;
        // tidyverse may or may not be installed, so we check if it was loaded
        if let Some(pkg) = result {
            assert!(pkg.is_meta_package, "tidyverse should be a meta-package");
            assert!(
                !pkg.attached_packages.is_empty(),
                "tidyverse should have attached packages"
            );
            assert!(
                pkg.attached_packages.contains(&"dplyr".to_string()),
                "tidyverse should attach dplyr"
            );
            assert!(
                pkg.attached_packages.contains(&"ggplot2".to_string()),
                "tidyverse should attach ggplot2"
            );
        }
    }

    #[tokio::test]
    async fn test_get_package_tidymodels_is_meta_package() {
        // Test that tidymodels is recognized as a meta-package
        // Skip if R is not available or tidymodels is not installed
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        let result = lib.get_package("tidymodels").await;
        // tidymodels may or may not be installed, so we check if it was loaded
        if let Some(pkg) = result {
            assert!(pkg.is_meta_package, "tidymodels should be a meta-package");
            assert!(
                !pkg.attached_packages.is_empty(),
                "tidymodels should have attached packages"
            );
            assert!(
                pkg.attached_packages.contains(&"parsnip".to_string()),
                "tidymodels should attach parsnip"
            );
            assert!(
                pkg.attached_packages.contains(&"recipes".to_string()),
                "tidymodels should attach recipes"
            );
        }
    }

    #[tokio::test]
    async fn test_get_package_nonexistent_returns_none() {
        // Test that get_package handles non-existent packages gracefully
        // by returning None (not caching missing packages, so package_exists()
        // correctly reports them as missing)
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Query a package that definitely doesn't exist
        let result = lib.get_package("__nonexistent_package_xyz__").await;
        // Should return None for missing packages (not cached)
        assert!(result.is_none());
        // package_exists should also return false
        assert!(!lib.package_exists("__nonexistent_package_xyz__"));
    }

    #[tokio::test]
    async fn test_get_package_without_r_subprocess() {
        // Test that get_package works without R subprocess (filesystem fallback)
        let lib = PackageLibrary::new(None).await;

        // Without R subprocess, we rely on fallback lib_paths
        // The method should not panic regardless of whether packages are found
        let result = lib.get_package("dplyr").await;
        // Should return Some if dplyr is installed (common in most R environments)
        // Returns None if package not found (not cached to allow package_exists to work)
        if result.is_some() {
            // Package is installed, test that it's cached for subsequent calls
            let result2 = lib.get_package("dplyr").await;
            assert!(result2.is_some());
        }
    }

    #[tokio::test]
    async fn test_get_package_regular_package_not_meta() {
        // Test that regular packages are not marked as meta-packages
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        let result = lib.get_package("stats").await;
        assert!(result.is_some());
        let pkg = result.unwrap();
        assert!(!pkg.is_meta_package, "stats should not be a meta-package");
        assert!(
            pkg.attached_packages.is_empty(),
            "stats should not have attached packages"
        );
    }

    #[tokio::test]
    async fn test_get_package_loads_depends() {
        // Test that get_package loads the Depends field
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // stats package typically depends on base packages
        let result = lib.get_package("stats").await;
        assert!(result.is_some());
        // Note: depends may be empty for base packages, that's OK
    }

    #[tokio::test]
    async fn test_find_package_directory() {
        // Test the find_package_directory helper
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // stats package should be findable and has standard structure
        let stats_dir = lib.find_package_directory("stats");
        assert!(stats_dir.is_some(), "Should find 'stats' package directory");
        let stats_dir = stats_dir.unwrap();
        assert!(stats_dir.is_dir());
        // stats package should have DESCRIPTION (all packages have this)
        assert!(stats_dir.join("DESCRIPTION").exists());
    }

    #[tokio::test]
    async fn test_find_package_directory_nonexistent() {
        // Test that find_package_directory returns None for non-existent packages
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        let result = lib.find_package_directory("__nonexistent_package_xyz__");
        assert!(result.is_none());
    }

    // ============================================================================
    // Tests for get_all_exports() - Task 3.4
    // ============================================================================

    #[tokio::test]
    async fn test_get_all_exports_single_package() {
        // Test that get_all_exports returns exports for a single package with no deps
        let lib = PackageLibrary::new_empty();

        // Create a package with no dependencies
        let mut exports = HashSet::new();
        exports.insert("func1".to_string());
        exports.insert("func2".to_string());
        let info = PackageInfo::new("testpkg".to_string(), exports.clone());
        lib.insert_package(info).await;

        let all_exports = lib.get_all_exports("testpkg").await;

        assert_eq!(all_exports.len(), 2);
        assert!(all_exports.contains("func1"));
        assert!(all_exports.contains("func2"));
    }

    #[tokio::test]
    async fn test_get_all_exports_with_depends() {
        // Test that get_all_exports includes exports from Depends packages
        // Requirement 4.2
        let lib = PackageLibrary::new_empty();

        // Create dependency package
        let mut dep_exports = HashSet::new();
        dep_exports.insert("dep_func".to_string());
        let dep_info = PackageInfo::new("deppkg".to_string(), dep_exports);
        lib.insert_package(dep_info).await;

        // Create main package that depends on deppkg
        let mut main_exports = HashSet::new();
        main_exports.insert("main_func".to_string());
        let main_info = PackageInfo::with_details(
            "mainpkg".to_string(),
            main_exports,
            vec!["deppkg".to_string()],
            Vec::new(),
        );
        lib.insert_package(main_info).await;

        let all_exports = lib.get_all_exports("mainpkg").await;

        // Should have exports from both packages
        assert!(
            all_exports.contains("main_func"),
            "Should have main package export"
        );
        assert!(
            all_exports.contains("dep_func"),
            "Should have dependency export"
        );
        assert_eq!(all_exports.len(), 2);
    }

    #[tokio::test]
    async fn test_get_all_exports_transitive_depends() {
        // Test that get_all_exports handles transitive dependencies (A -> B -> C)
        let lib = PackageLibrary::new_empty();

        // Create package C (no deps)
        let mut c_exports = HashSet::new();
        c_exports.insert("c_func".to_string());
        let c_info = PackageInfo::new("pkgC".to_string(), c_exports);
        lib.insert_package(c_info).await;

        // Create package B (depends on C)
        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        // Create package A (depends on B)
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        let all_exports = lib.get_all_exports("pkgA").await;

        // Should have exports from all three packages
        assert!(all_exports.contains("a_func"), "Should have A's export");
        assert!(all_exports.contains("b_func"), "Should have B's export");
        assert!(all_exports.contains("c_func"), "Should have C's export");
        assert_eq!(all_exports.len(), 3);
    }

    #[tokio::test]
    async fn test_get_all_exports_circular_dependency() {
        // Test that get_all_exports handles circular dependencies
        // Requirement 4.5
        let lib = PackageLibrary::new_empty();

        // Create package A (depends on B)
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        // Create package B (depends on A - circular!)
        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgA".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        // Should not hang or panic - should terminate and return exports
        let all_exports = lib.get_all_exports("pkgA").await;

        // Should have exports from both packages
        assert!(all_exports.contains("a_func"), "Should have A's export");
        assert!(all_exports.contains("b_func"), "Should have B's export");
        assert_eq!(all_exports.len(), 2);
    }

    #[tokio::test]
    async fn test_get_all_exports_self_dependency() {
        // Test that get_all_exports handles self-referential dependencies
        let lib = PackageLibrary::new_empty();

        // Create package that depends on itself (pathological case)
        let mut exports = HashSet::new();
        exports.insert("func".to_string());
        let info = PackageInfo::with_details(
            "selfpkg".to_string(),
            exports,
            vec!["selfpkg".to_string()],
            Vec::new(),
        );
        lib.insert_package(info).await;

        // Should not hang or panic
        let all_exports = lib.get_all_exports("selfpkg").await;

        assert!(all_exports.contains("func"));
        assert_eq!(all_exports.len(), 1);
    }

    #[tokio::test]
    async fn test_get_all_exports_meta_package_attached() {
        // Test that get_all_exports includes attached packages for meta-packages
        let lib = PackageLibrary::new_empty();

        // Create some packages that tidyverse would attach
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        dplyr_exports.insert("filter".to_string());
        let dplyr_info = PackageInfo::new("dplyr".to_string(), dplyr_exports);
        lib.insert_package(dplyr_info).await;

        let mut ggplot2_exports = HashSet::new();
        ggplot2_exports.insert("ggplot".to_string());
        ggplot2_exports.insert("aes".to_string());
        let ggplot2_info = PackageInfo::new("ggplot2".to_string(), ggplot2_exports);
        lib.insert_package(ggplot2_info).await;

        // Create tidyverse (meta-package)
        let mut tidyverse_exports = HashSet::new();
        tidyverse_exports.insert("tidyverse_conflicts".to_string());
        let tidyverse_info = PackageInfo::new("tidyverse".to_string(), tidyverse_exports);
        lib.insert_package(tidyverse_info).await;

        let all_exports = lib.get_all_exports("tidyverse").await;

        // Should have tidyverse's own exports
        assert!(all_exports.contains("tidyverse_conflicts"));
        // Should have dplyr exports (attached package)
        assert!(all_exports.contains("mutate"));
        assert!(all_exports.contains("filter"));
        // Should have ggplot2 exports (attached package)
        assert!(all_exports.contains("ggplot"));
        assert!(all_exports.contains("aes"));
    }

    #[tokio::test]
    async fn test_get_all_exports_missing_dependency() {
        // Test that get_all_exports handles missing dependencies gracefully
        let lib = PackageLibrary::new_empty();

        // Create package that depends on a non-existent package
        let mut exports = HashSet::new();
        exports.insert("func".to_string());
        let info = PackageInfo::with_details(
            "mainpkg".to_string(),
            exports,
            vec!["nonexistent".to_string()],
            Vec::new(),
        );
        lib.insert_package(info).await;

        // Should not panic, should return main package's exports
        let all_exports = lib.get_all_exports("mainpkg").await;

        assert!(all_exports.contains("func"));
        // nonexistent package contributes nothing (empty exports)
    }

    // ============================================================================
    // Tests for package-dataset (lazy_data) resolution - issue #350
    // ============================================================================

    #[test]
    fn dedup_preserving_order_keeps_first_occurrence() {
        // Guards the issue #429 fix: prefetch_packages must collapse a
        // duplicated package name (e.g. two `library(dplyr)` lines) to a single
        // entry in first-seen order, so its consumed `data()` enumeration is
        // applied exactly once rather than overwritten by an un-enumerated
        // second insert. The input's first-seen order ("tibble", "dplyr",
        // "survey") is deliberately NOT lexicographically sorted, so this
        // assertion fails for a no-op (keeps all 5 entries) AND for a sort-based
        // dedup (would yield ["dplyr","survey","tibble"]).
        let out = dedup_preserving_order(vec![
            "tibble".into(),
            "dplyr".into(),
            "tibble".into(),
            "survey".into(),
            "dplyr".into(),
        ]);
        assert_eq!(
            out,
            vec![
                "tibble".to_string(),
                "dplyr".to_string(),
                "survey".to_string(),
            ]
        );
    }

    /// Write a static (no-`exportPattern`) package on disk under `lib_root`:
    /// one `export(<export>)` in NAMESPACE and an optional `Depends:` line in
    /// DESCRIPTION. No `data/` dir, so it loads with no R subprocess.
    fn write_static_pkg(lib_root: &std::path::Path, name: &str, export: &str, depends: &[&str]) {
        let pkg_dir = lib_root.join(name);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let depends_line = if depends.is_empty() {
            String::new()
        } else {
            format!("Depends: {}\n", depends.join(", "))
        };
        std::fs::write(
            pkg_dir.join("DESCRIPTION"),
            format!("Package: {name}\nVersion: 1.0.0\n{depends_line}"),
        )
        .unwrap();
        std::fs::write(pkg_dir.join("NAMESPACE"), format!("export({export})\n")).unwrap();
    }

    #[tokio::test]
    async fn prefetch_loads_transitive_closure_without_r_and_terminates_on_cycle() {
        // Covers the BFS added by the issue #429 N+1 fix
        // (`prefetch_packages` → `prefetch_uncached_level`):
        //   1. Termination on dependency cycles — the `scheduled` + cache guards
        //      are what stop the level loop from re-queuing `pkgA`/`pkgB`
        //      forever; remove them and this test hangs (verified by mutation).
        //   2. The full transitive `Depends` closure is loaded and its combined
        //      exports resolve through every level.
        // R-free: static (no-`exportPattern`) packages load via
        // NAMESPACE/DESCRIPTION parsing with no subprocess. (The batched-vs-
        // per-package enumeration distinction is a subprocess-count property,
        // not observable without R, so it is not asserted here.)
        let lib_root = tempfile::tempdir().unwrap();
        let root = lib_root.path();
        write_static_pkg(root, "pkgA", "funcA", &["pkgB"]);
        write_static_pkg(root, "pkgB", "funcB", &["pkgC", "pkgA"]); // pkgA back-edge → cycle
        write_static_pkg(root, "pkgC", "funcC", &[]);

        let mut lib = PackageLibrary::with_subprocess(None);
        lib.set_lib_paths(vec![root.to_path_buf()]);

        // Must return (terminate) despite the pkgA<->pkgB dependency cycle.
        lib.prefetch_packages(&["pkgA".to_string()]).await;

        // The entire transitive closure is in the per-package cache.
        assert!(
            lib.get_cached_package("pkgA").await.is_some(),
            "requested package pkgA must be cached"
        );
        assert!(
            lib.get_cached_package("pkgB").await.is_some(),
            "first-level transitive dependency pkgB must be cached"
        );
        assert!(
            lib.get_cached_package("pkgC").await.is_some(),
            "second-level transitive dependency pkgC must be cached"
        );

        // The combined (transitive) export set resolves through the closure.
        let exports = lib.get_all_exports("pkgA").await;
        assert!(exports.contains("funcA"), "own export");
        assert!(exports.contains("funcB"), "Depends export (level 1)");
        assert!(exports.contains("funcC"), "transitive export (level 2)");
    }

    /// Write a package whose NAMESPACE exports symbols ONLY via the S4
    /// directives `exportMethods`/`exportClasses` plus one plain `export`, with
    /// NO `exportPattern` — the sp/maptools shape from issue #474. Because there
    /// is no `exportPattern`, the loader takes the static path and never calls
    /// R, so these names must come from the static parse.
    fn write_s4_pkg(lib_root: &std::path::Path, name: &str) {
        let pkg_dir = lib_root.join(name);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("DESCRIPTION"),
            format!("Package: {name}\nVersion: 1.0.0\n"),
        )
        .unwrap();
        std::fs::write(
            pkg_dir.join("NAMESPACE"),
            "export(plainFn)\nexportClasses(Spatial)\nexportMethods(spTransform)\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn get_package_resolves_s4_exports_without_r() {
        // Issue #474: a no-`exportPattern` package exporting via
        // exportMethods/exportClasses must resolve through `get_package` with no
        // R subprocess.
        let lib_root = tempfile::tempdir().unwrap();
        write_s4_pkg(lib_root.path(), "sppkg");
        let mut lib = PackageLibrary::with_subprocess(None);
        lib.set_lib_paths(vec![lib_root.path().to_path_buf()]);

        let info = lib.get_package("sppkg").await.expect("package loads");
        assert!(info.exports.contains("plainFn"), "plain export");
        assert!(
            info.exports.contains("spTransform"),
            "exportMethods S4 generic must resolve (was the #474 bug): {:?}",
            info.exports
        );
        assert!(
            info.exports.contains("Spatial"),
            "exportClasses S4 class must resolve: {:?}",
            info.exports
        );
    }

    #[tokio::test]
    async fn prefetch_resolves_s4_exports_without_r() {
        // Issue #474: the prefetch path makes the same static-vs-pattern
        // decision as get_package; S4 exports must resolve from cache after a
        // warm with no R subprocess.
        let lib_root = tempfile::tempdir().unwrap();
        write_s4_pkg(lib_root.path(), "sppkg");
        let mut lib = PackageLibrary::with_subprocess(None);
        lib.set_lib_paths(vec![lib_root.path().to_path_buf()]);

        lib.prefetch_packages(&["sppkg".to_string()]).await;

        let info = lib
            .get_cached_package("sppkg")
            .await
            .expect("prefetched package is cached");
        assert!(
            info.exports.contains("spTransform"),
            "exportMethods S4 generic must resolve via prefetch: {:?}",
            info.exports
        );
        assert!(
            info.exports.contains("Spatial"),
            "exportClasses S4 class must resolve via prefetch: {:?}",
            info.exports
        );
    }

    /// Build a fake installed package on disk and return `(lib_root, lib)` with
    /// `lib`'s search path pointing at it. `namespace` is the NAMESPACE body and
    /// `datasets` are written one-per-line to `data/datalist`. The returned
    /// `TempDir` must stay alive for the duration of the test.
    fn fake_installed_pkg(
        pkg_name: &str,
        namespace: &str,
        datasets: &[&str],
    ) -> (tempfile::TempDir, PackageLibrary) {
        let lib_root = tempfile::tempdir().unwrap();
        let pkg_dir = lib_root.path().join(pkg_name);
        let data_dir = pkg_dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(
            pkg_dir.join("DESCRIPTION"),
            format!("Package: {pkg_name}\nVersion: 1.0.0\n"),
        )
        .unwrap();
        std::fs::write(pkg_dir.join("NAMESPACE"), namespace).unwrap();
        std::fs::write(data_dir.join("datalist"), datasets.join("\n") + "\n").unwrap();

        let mut lib = PackageLibrary::with_subprocess(None);
        lib.set_lib_paths(vec![lib_root.path().to_path_buf()]);
        (lib_root, lib)
    }

    #[tokio::test]
    async fn test_get_all_exports_includes_lazy_data() {
        // A package's datasets (lazy_data) must be resolvable as symbols, not
        // just its function exports. `nycflights13` exports no functions but
        // ships the `flights`/`airports` datasets.
        let lib = PackageLibrary::new_empty();

        let info = PackageInfo::with_details(
            "nycflights13".to_string(),
            HashSet::new(),
            Vec::new(),
            vec!["flights".to_string(), "airports".to_string()],
        );
        lib.insert_package(info).await;

        let all_exports = lib.get_all_exports("nycflights13").await;

        assert!(
            all_exports.contains("flights"),
            "dataset `flights` should resolve as a symbol"
        );
        assert!(
            all_exports.contains("airports"),
            "dataset `airports` should resolve as a symbol"
        );
    }

    #[tokio::test]
    async fn test_get_all_exports_includes_transitive_lazy_data() {
        // Datasets must resolve transitively through `Depends`, the same as
        // function exports do.
        let lib = PackageLibrary::new_empty();

        let dep = PackageInfo::with_details(
            "datapkg".to_string(),
            HashSet::new(),
            Vec::new(),
            vec!["bundled_dataset".to_string()],
        );
        lib.insert_package(dep).await;

        let mut main_exports = HashSet::new();
        main_exports.insert("main_func".to_string());
        let main = PackageInfo::with_details(
            "mainpkg".to_string(),
            main_exports,
            vec!["datapkg".to_string()],
            Vec::new(),
        );
        lib.insert_package(main).await;

        let all_exports = lib.get_all_exports("mainpkg").await;

        assert!(all_exports.contains("main_func"), "Should have main export");
        assert!(
            all_exports.contains("bundled_dataset"),
            "dataset from a Depends package should resolve transitively"
        );
    }

    #[tokio::test]
    async fn test_get_all_exports_includes_meta_attached_lazy_data() {
        // Datasets must resolve through meta-package `attached_packages` —
        // e.g. `library(tidyverse); diamonds` where `diamonds` ships with the
        // attached `ggplot2`.
        let lib = PackageLibrary::new_empty();

        let ggplot2 = PackageInfo::with_details(
            "ggplot2".to_string(),
            HashSet::new(),
            Vec::new(),
            vec!["diamonds".to_string()],
        );
        lib.insert_package(ggplot2).await;

        // tidyverse is a meta-package that attaches ggplot2.
        lib.insert_package(PackageInfo::new("tidyverse".to_string(), HashSet::new()))
            .await;

        let all_exports = lib.get_all_exports("tidyverse").await;

        assert!(
            all_exports.contains("diamonds"),
            "dataset from an attached meta-package member should resolve"
        );
    }

    #[tokio::test]
    async fn test_owner_for_meta_attached_export_is_member() {
        // library(tidyverse); mutate -> documentation / NSE owner is dplyr, not
        // tidyverse. tidyverse makes `mutate` visible only by attaching dplyr;
        // its own namespace does not export the verb (issue #407).
        let lib = PackageLibrary::new_empty();

        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        lib.insert_package(PackageInfo::new("dplyr".to_string(), dplyr_exports))
            .await;
        // tidyverse is a meta-package that attaches dplyr.
        lib.insert_package(PackageInfo::new("tidyverse".to_string(), HashSet::new()))
            .await;

        // Warm the combined caches the way the diagnostic prefetch does.
        lib.get_all_exports("tidyverse").await;

        let loaded = vec!["tidyverse".to_string()];
        // Availability is unchanged: the symbol is still visible.
        assert!(lib.is_symbol_from_loaded_packages("mutate", &loaded));
        // But ownership resolves to the true contributor.
        assert_eq!(
            lib.find_package_owner_for_symbol("mutate", &loaded),
            Some("dplyr".to_string()),
            "owner of mutate under library(tidyverse) should be dplyr"
        );
    }

    #[tokio::test]
    async fn test_owner_for_direct_package_is_the_package() {
        // library(dplyr); mutate -> owner is dplyr (root owns its own export).
        let lib = PackageLibrary::new_empty();
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        lib.insert_package(PackageInfo::new("dplyr".to_string(), dplyr_exports))
            .await;
        lib.get_all_exports("dplyr").await;

        let loaded = vec!["dplyr".to_string()];
        assert_eq!(
            lib.find_package_owner_for_symbol("mutate", &loaded),
            Some("dplyr".to_string())
        );
    }

    #[tokio::test]
    async fn test_resolve_default_attached_owner_recovers_true_owner() {
        let mut lib = PackageLibrary::new_empty();
        lib.set_base_packages(HashSet::from(["base".to_string(), "stats".to_string()]));
        lib.set_base_exports(HashSet::from(["ave".to_string(), "sum".to_string()]));

        lib.insert_package(PackageInfo::new(
            "base".to_string(),
            HashSet::from(["sum".to_string()]),
        ))
        .await;
        lib.insert_package(PackageInfo::new(
            "stats".to_string(),
            HashSet::from(["ave".to_string()]),
        ))
        .await;
        lib.recompute_default_attached_owners();

        assert_eq!(lib.resolve_default_attached_owner("ave"), "stats");
        assert_eq!(lib.resolve_default_attached_owner("sum"), "base");
        assert_eq!(lib.resolve_default_attached_owner("not_exported"), "base");
    }

    #[tokio::test]
    async fn test_resolve_default_attached_owner_survives_clear_cache() {
        let mut lib = PackageLibrary::new_empty();
        lib.set_base_packages(HashSet::from(["base".to_string(), "stats".to_string()]));
        lib.set_base_exports(HashSet::from(["ave".to_string(), "sum".to_string()]));

        lib.insert_package(PackageInfo::new(
            "base".to_string(),
            HashSet::from(["sum".to_string()]),
        ))
        .await;
        lib.insert_package(PackageInfo::new(
            "stats".to_string(),
            HashSet::from(["ave".to_string()]),
        ))
        .await;
        lib.recompute_default_attached_owners();

        // Simulate `raven.refreshPackages`'s post-swap clear_cache: the per-package
        // cache is wiped but the durable owner map must survive so hover still
        // attributes `ave` to {stats}. Regression guard for issue #592.
        lib.clear_cache().await;

        assert_eq!(lib.resolve_default_attached_owner("ave"), "stats");
        assert_eq!(lib.resolve_default_attached_owner("sum"), "base");
    }

    #[tokio::test]
    async fn test_resolve_default_attached_owner_uses_deterministic_overlap_winner() {
        let mut lib = PackageLibrary::new_empty();
        lib.set_base_packages(HashSet::from([
            "base".to_string(),
            "graphics".to_string(),
            "grDevices".to_string(),
        ]));
        lib.set_base_exports(HashSet::from(["shared_sym".to_string()]));

        lib.insert_package(PackageInfo::new(
            "graphics".to_string(),
            HashSet::from(["shared_sym".to_string()]),
        ))
        .await;
        lib.insert_package(PackageInfo::new(
            "grDevices".to_string(),
            HashSet::from(["shared_sym".to_string()]),
        ))
        .await;
        lib.recompute_default_attached_owners();

        assert_eq!(
            lib.resolve_default_attached_owner("shared_sym"),
            "grDevices"
        );
    }

    #[tokio::test]
    async fn test_owner_root_export_wins_over_dependency() {
        // A Depends on B, and BOTH export `foo`. A genuine root export wins over
        // a dependency that also happens to export the name (it is not a
        // re-export). The aggregate root is visited first, so it owns `foo`.
        let lib = PackageLibrary::new_empty();

        let mut b_exports = HashSet::new();
        b_exports.insert("foo".to_string());
        b_exports.insert("only_b".to_string());
        lib.insert_package(PackageInfo::new("pkgB".to_string(), b_exports))
            .await;

        let mut a_exports = HashSet::new();
        a_exports.insert("foo".to_string());
        lib.insert_package(PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string()],
            Vec::new(),
        ))
        .await;

        lib.get_all_exports("pkgA").await;

        let loaded = vec!["pkgA".to_string()];
        assert_eq!(
            lib.find_package_owner_for_symbol("foo", &loaded),
            Some("pkgA".to_string()),
            "a genuine root export should be owned by the root"
        );
        // A symbol that only the dependency exports is owned by the dependency.
        assert_eq!(
            lib.find_package_owner_for_symbol("only_b", &loaded),
            Some("pkgB".to_string()),
            "a dependency-only export should be owned by the dependency"
        );
    }

    #[tokio::test]
    async fn test_owner_falls_back_when_owner_map_absent() {
        // When no aggregate entry has been warmed, the lookup falls back to the
        // per-package availability cache (find_in_per_package_cache), attributing
        // the symbol to the directly-loaded package that exports it.
        let lib = PackageLibrary::new_empty();
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        lib.insert_package(PackageInfo::new("dplyr".to_string(), dplyr_exports))
            .await;
        // Note: no get_all_exports() call, so combined_entries is empty.
        let loaded = vec!["dplyr".to_string()];
        assert_eq!(
            lib.find_package_owner_for_symbol("mutate", &loaded),
            Some("dplyr".to_string())
        );
    }

    #[tokio::test]
    async fn test_owner_lookup_ignores_ownerless_combined_entry() {
        // A combined availability hit without matching owner attribution is a
        // partial snapshot. Owner-sensitive lookup must fail closed instead of
        // attributing a transitive export to the aggregate package.
        let lib = PackageLibrary::new_empty();
        lib.insert_package(PackageInfo::new("tidyverse".to_string(), HashSet::new()))
            .await;
        seed_combined_entry(
            &lib,
            "tidyverse",
            ["mutate".to_string()].into_iter().collect(),
            HashMap::new(),
        )
        .await;

        let loaded = vec!["tidyverse".to_string()];
        assert_eq!(
            lib.find_package_owner_for_symbol("mutate", &loaded),
            None,
            "owner lookup must not fall back to aggregate attribution from ownerless combined availability"
        );
    }

    #[tokio::test]
    async fn test_owned_exports_for_completions_attributes_owner() {
        // Completion attribution must use the true owner: under library(tidyverse)
        // the `mutate` completion is detailed/resolved as {dplyr}, not {tidyverse}.
        let lib = PackageLibrary::new_empty();
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        lib.insert_package(PackageInfo::new("dplyr".to_string(), dplyr_exports))
            .await;
        lib.insert_package(PackageInfo::new("tidyverse".to_string(), HashSet::new()))
            .await;
        lib.get_all_exports("tidyverse").await;

        let owned = lib.get_owned_exports_for_completions(&["tidyverse".to_string()]);
        assert_eq!(
            owned.get("mutate"),
            Some(&vec!["dplyr".to_string()]),
            "completion owner of mutate under tidyverse should be dplyr"
        );
    }

    #[tokio::test]
    async fn test_owned_exports_for_completions_falls_back_to_loaded_package() {
        // When no aggregate entry is warmed, attribution falls back to the loaded
        // package (existing get_exports_for_completions behavior).
        let lib = PackageLibrary::new_empty();
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        lib.insert_package(PackageInfo::new("dplyr".to_string(), dplyr_exports))
            .await;
        // No get_all_exports(): aggregate entry absent, per-package cache present.
        let owned = lib.get_owned_exports_for_completions(&["dplyr".to_string()]);
        assert_eq!(owned.get("mutate"), Some(&vec!["dplyr".to_string()]));
    }

    #[tokio::test]
    async fn test_clear_cache_drops_combined_entry() {
        // clear_cache() must drop the unified aggregate entry containing both
        // availability and ownership.
        let lib = PackageLibrary::new_empty();
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        lib.insert_package(PackageInfo::new("dplyr".to_string(), dplyr_exports))
            .await;
        lib.get_all_exports("dplyr").await;
        assert!(lib.combined_entries.load().contains_key("dplyr"));

        lib.clear_cache().await;

        assert!(
            lib.combined_entries.load().is_empty(),
            "clear_cache must clear aggregate entries"
        );
    }

    #[tokio::test]
    async fn test_invalidate_many_drops_combined_entry_for_dependent_aggregate() {
        // Invalidating an attached member (dplyr) must drop the cached unified
        // aggregate entry (tidyverse) that rolled it up.
        let lib = PackageLibrary::new_empty();
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        lib.insert_package(PackageInfo::new("dplyr".to_string(), dplyr_exports))
            .await;
        lib.insert_package(PackageInfo::new("tidyverse".to_string(), HashSet::new()))
            .await;
        lib.get_all_exports("tidyverse").await;
        assert!(lib.combined_entries.load().contains_key("tidyverse"));

        let mut names = HashSet::new();
        names.insert("dplyr".to_string());
        lib.invalidate_many(&names).await;

        assert!(
            !lib.combined_entries.load().contains_key("tidyverse"),
            "invalidating dplyr must drop the tidyverse aggregate entry"
        );
    }

    #[tokio::test]
    async fn test_get_package_populates_lazy_data_from_disk() {
        // `get_package` must walk the installed package's `data/` directory and
        // record dataset names in `lazy_data` (issue #350).
        let (_lib_root, lib) = fake_installed_pkg(
            "nycflights13",
            "# exports nothing\n",
            &["flights", "airports", "planes"],
        );

        let info = lib
            .get_package("nycflights13")
            .await
            .expect("package should load");

        assert!(
            info.lazy_data.contains(&"flights".to_string()),
            "lazy_data should contain `flights`, got {:?}",
            info.lazy_data
        );
        assert!(
            info.lazy_data.contains(&"airports".to_string()),
            "lazy_data should contain `airports`, got {:?}",
            info.lazy_data
        );
        assert!(
            info.lazy_data.contains(&"planes".to_string()),
            "lazy_data should contain `planes`, got {:?}",
            info.lazy_data
        );
    }

    // ============================================================================
    // Tests for apply_enumerated_data and data_aliases (issue #429)
    // ============================================================================

    #[test]
    fn test_package_info_data_aliases_default_empty() {
        let info = PackageInfo::new("pkg".to_string(), HashSet::new());
        assert!(info.data_aliases.is_empty());
    }

    #[test]
    fn package_info_defaults_exports_completeness_unknown() {
        let info = PackageInfo::new("p".to_string(), HashSet::new());
        assert_eq!(info.exports_completeness, MemberCompleteness::Unknown);
    }

    #[tokio::test]
    async fn static_namespace_exports_are_complete() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("mypkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("DESCRIPTION"), "Package: mypkg\nVersion: 1.0\n").unwrap();
        // No exportPattern() → static parse is exhaustive → Complete.
        std::fs::write(pkg.join("NAMESPACE"), "export(foo)\nexport(bar)\n").unwrap();
        let lib = build_package_library_tier1_only(None, &[dir.path().to_path_buf()], None).await;
        let info = lib.library.get_package("mypkg").await.expect("loads");
        assert_eq!(info.exports_completeness, MemberCompleteness::Complete);
    }

    #[tokio::test]
    async fn member_status_present_absent_partial() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("mypkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("DESCRIPTION"), "Package: mypkg\nVersion: 1.0\n").unwrap();
        std::fs::write(pkg.join("NAMESPACE"), "export(foo)\n").unwrap();
        let lib = build_package_library_tier1_only(None, &[dir.path().to_path_buf()], None)
            .await
            .library;
        lib.get_package("mypkg").await; // warm

        assert_eq!(
            lib.namespace_member_status_sync("mypkg", "foo"),
            NamespaceMemberStatus::Present
        );
        assert_eq!(
            lib.namespace_member_status_sync("mypkg", "nope"),
            NamespaceMemberStatus::Absent
        );
        // Never-loaded package with no on-disk dir and no provider: Unknown,
        // never Absent.
        assert_eq!(
            lib.namespace_member_status_sync("ghostpkg", "x"),
            NamespaceMemberStatus::Unknown
        );
    }

    #[tokio::test]
    async fn complete_exports_not_downgraded_by_partial() {
        let lib = PackageLibrary::new_empty();
        // Seed a Complete entry: exports {foo}, so `bar` is authoritatively Absent.
        let mut complete = PackageInfo::with_details(
            "pkg".to_string(),
            HashSet::from(["foo".to_string()]),
            vec![],
            vec![],
        );
        complete.exports_completeness = MemberCompleteness::Complete;
        lib.insert_package(complete).await;
        assert_eq!(
            lib.namespace_member_status_sync("pkg", "bar"),
            NamespaceMemberStatus::Absent
        );

        // A weaker (Partial) re-insert with a different export set must not
        // downgrade the cached Complete authority.
        let mut partial = PackageInfo::with_details(
            "pkg".to_string(),
            HashSet::from(["baz".to_string()]),
            vec![],
            vec![],
        );
        partial.exports_completeness = MemberCompleteness::Partial;
        lib.insert_package(partial).await;

        let cached = lib.packages.load();
        let info = cached.get("pkg").expect("cached");
        assert_eq!(info.exports_completeness, MemberCompleteness::Complete);
        assert!(info.exports.contains("foo"));
        assert_eq!(
            lib.namespace_member_status_sync("pkg", "bar"),
            NamespaceMemberStatus::Absent
        );
    }

    #[tokio::test]
    async fn member_status_defers_to_local_install_over_stale_provider() {
        use crate::package_db::PackageMetadataProvider;
        // A provider snapshot that is STALE: it knows `mypkg` but only an old
        // export, missing `new_fn`.
        struct StaleProvider;
        impl PackageMetadataProvider for StaleProvider {
            fn lookup(&self, name: &str) -> Option<PackageInfo> {
                (name == "mypkg").then(|| {
                    let mut info = PackageInfo::with_details(
                        "mypkg".to_string(),
                        HashSet::from(["old_fn".to_string()]),
                        vec![],
                        vec![],
                    );
                    info.exports_completeness = MemberCompleteness::Complete;
                    info
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("mypkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("DESCRIPTION"), "Package: mypkg\nVersion: 2.0\n").unwrap();
        // The LOCAL install is newer: it exports `new_fn`.
        std::fs::write(pkg.join("NAMESPACE"), "export(new_fn)\n").unwrap();

        let mut lib = PackageLibrary::new_empty();
        lib.set_lib_paths(vec![dir.path().to_path_buf()]);
        lib.set_providers(vec![Box::new(StaleProvider)]);

        // Cache miss + stale provider lacking `new_fn`, but mypkg is installed
        // locally — must DEFER (Unknown), never a false Absent on a real local
        // export. (The provider's `old_fn` is also not asserted Absent for the
        // same reason.)
        assert_eq!(
            lib.namespace_member_status_sync("mypkg", "new_fn"),
            NamespaceMemberStatus::Unknown
        );

        // A package the provider knows but that is NOT installed locally: the
        // provider IS authoritative, so a genuine typo is Absent.
        assert_eq!(
            lib.namespace_member_status_sync("ghostpkg", "anything"),
            NamespaceMemberStatus::Unknown // provider doesn't know ghostpkg either
        );

        // After warming from the local install, the authoritative export set
        // resolves `new_fn` as Present.
        lib.get_package("mypkg").await;
        assert_eq!(
            lib.namespace_member_status_sync("mypkg", "new_fn"),
            NamespaceMemberStatus::Present
        );
    }

    #[tokio::test]
    async fn data_package_without_r_enumeration_is_partial() {
        // Regression: a non-exportPattern package that ships a `data/` directory
        // whose datasets cannot be enumerated without R (a binary Rdata.rdb
        // exposes no object names to the static parse) must NOT be Complete —
        // otherwise a real `pkg::dataset` would be falsely Absent. new_empty()
        // has no R subprocess, so enumeration deterministically cannot run.
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("datapkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("DESCRIPTION"), "Package: datapkg\nVersion: 1.0\n").unwrap();
        std::fs::write(pkg.join("NAMESPACE"), "export(some_fn)\n").unwrap();
        std::fs::create_dir_all(pkg.join("data")).unwrap();
        // Binary lazy-load DB: opaque to the static dataset parse.
        std::fs::write(pkg.join("data").join("Rdata.rdb"), b"\x00binary").unwrap();

        let mut lib = PackageLibrary::new_empty();
        lib.set_lib_paths(vec![dir.path().to_path_buf()]);
        let info = lib.get_package("datapkg").await.expect("loads");
        assert_eq!(
            info.exports_completeness,
            MemberCompleteness::Partial,
            "data/-bearing package without R dataset enumeration must be Partial"
        );
        // A dataset member is therefore Partial (silent), never a false Absent.
        assert_eq!(
            lib.namespace_member_status_sync("datapkg", "some_dataset"),
            NamespaceMemberStatus::Partial
        );
        // The exported function is still confirmed Present.
        assert_eq!(
            lib.namespace_member_status_sync("datapkg", "some_fn"),
            NamespaceMemberStatus::Present
        );
    }

    #[tokio::test]
    async fn member_status_resolves_data_alias_objects() {
        // Regression: a non-LazyData data package (e.g. survey) keeps file STEMS
        // in lazy_data but the real dataset object NAMES in data_aliases values.
        // The member authority must treat those as Present, not Absent, even when
        // the package is Complete.
        let lib = PackageLibrary::new_empty();
        let mut info = PackageInfo::with_details(
            "survey".to_string(),
            HashSet::from(["svymean".to_string()]),
            vec![],
            // lazy_data carries only the file stem for a non-LazyData package.
            vec!["api".to_string()],
        );
        info.data_aliases.insert(
            "api".to_string(),
            vec!["apiclus1".to_string(), "apistrat".to_string()],
        );
        info.exports_completeness = MemberCompleteness::Complete;
        lib.insert_package(info).await;

        // Exported function: Present.
        assert_eq!(
            lib.namespace_member_status_sync("survey", "svymean"),
            NamespaceMemberStatus::Present
        );
        // Real dataset object bound by a data file (data_aliases value): Present,
        // NOT a false Absent.
        assert_eq!(
            lib.namespace_member_status_sync("survey", "apiclus1"),
            NamespaceMemberStatus::Present
        );
        // A genuine typo against a Complete package: Absent.
        assert_eq!(
            lib.namespace_member_status_sync("survey", "apiclusX"),
            NamespaceMemberStatus::Absent
        );
    }

    #[tokio::test]
    async fn test_apply_enumerated_data_lazydata_package() {
        // Rdata.rdb present → lazy_data replaced by enumerated names; aliases mapped.
        let enumerated = vec![
            crate::r_subprocess::DataObject {
                name: "apiclus1".into(),
                file_stem: "api".into(),
            },
            crate::r_subprocess::DataObject {
                name: "apistrat".into(),
                file_stem: "api".into(),
            },
            crate::r_subprocess::DataObject {
                name: "lung".into(),
                file_stem: "lung".into(),
            },
        ];
        let mut info = PackageInfo::new("survey".to_string(), HashSet::new());
        info.lazy_data = vec!["api".to_string()]; // static stem discovery result
        apply_enumerated_data(&mut info, &enumerated, /* has_lazy_data_db */ true);
        assert_eq!(info.lazy_data, vec!["apiclus1", "apistrat", "lung"]);
        assert_eq!(info.data_aliases["api"], vec!["apiclus1", "apistrat"]);
        assert_eq!(info.data_aliases["lung"], vec!["lung"]);
    }

    #[tokio::test]
    async fn test_apply_enumerated_data_non_lazydata_package() {
        // No Rdata.rdb → lazy_data untouched (permissive static stems stay); aliases still mapped.
        let enumerated = vec![crate::r_subprocess::DataObject {
            name: "apiclus1".into(),
            file_stem: "api".into(),
        }];
        let mut info = PackageInfo::new("survey".to_string(), HashSet::new());
        info.lazy_data = vec!["api".to_string()];
        apply_enumerated_data(&mut info, &enumerated, false);
        assert_eq!(info.lazy_data, vec!["api"]);
        assert_eq!(info.data_aliases["api"], vec!["apiclus1"]);
    }

    #[tokio::test]
    async fn test_get_package_enumerates_datasets_with_r() {
        // survival ships an Rdata.rdb with no datalist; its `lung` dataset is
        // invisible to static stem discovery, so this exercises enumeration.
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        let Some(info) = lib.get_package("survival").await else {
            eprintln!("survival not installed, skipping");
            return;
        };
        assert!(
            info.lazy_data.contains(&"lung".to_string()),
            "lazy_data should contain `lung`, got {:?}",
            info.lazy_data
        );
    }

    #[tokio::test]
    async fn test_disk_package_dataset_resolves_as_loaded_symbol() {
        // End-to-end: a dataset from an installed package resolves through the
        // synchronous diagnostic check `is_symbol_from_loaded_packages`, closing
        // the false-positive undefined-variable gap (issue #350).
        let (_lib_root, lib) =
            fake_installed_pkg("nycflights13", "# exports nothing\n", &["flights"]);

        // Warm the combined-exports cache the way the diagnostic prefetch does.
        lib.get_all_exports("nycflights13").await;

        let loaded = vec!["nycflights13".to_string()];
        assert!(
            lib.is_symbol_from_loaded_packages("flights", &loaded),
            "dataset `flights` should resolve via is_symbol_from_loaded_packages"
        );
        assert_eq!(
            lib.find_package_for_symbol("flights", &loaded),
            Some("nycflights13".to_string()),
            "find_package_for_symbol should attribute the dataset to its package"
        );
    }

    #[tokio::test]
    async fn test_prefetch_static_package_resolves_dataset() {
        // The production LSP warm-up path is `prefetch_packages`, which inserts
        // PackageInfo and combined_entries directly — bypassing `get_package`.
        // A statically-loaded package's datasets must resolve through it too,
        // or `library(nycflights13); flights` stays a false positive in the
        // editor (issue #350).
        //
        // Explicit export only (no exportPattern) -> static prefetch path.
        let (_lib_root, lib) = fake_installed_pkg(
            "nycflights13",
            "export(nycflights13)\n",
            &["flights", "airports"],
        );

        lib.prefetch_packages(&["nycflights13".to_string()]).await;

        let loaded = vec!["nycflights13".to_string()];
        assert!(
            lib.is_symbol_from_loaded_packages("flights", &loaded),
            "dataset `flights` should resolve after prefetch_packages"
        );
        assert!(
            lib.is_symbol_from_loaded_packages("airports", &loaded),
            "dataset `airports` should resolve after prefetch_packages"
        );
        // Function exports must still resolve (no regression).
        assert!(
            lib.is_symbol_from_loaded_packages("nycflights13", &loaded),
            "function export should still resolve after prefetch_packages"
        );
    }

    #[tokio::test]
    async fn test_warm_all_exports_returns_cached_arc_without_owned_clone() {
        let lib = PackageLibrary::new_empty();
        let exports = HashSet::from(["foo".to_string(), "bar".to_string()]);
        lib.insert_package(PackageInfo::new("pkg".to_string(), exports))
            .await;

        let warmed = lib.warm_all_exports("pkg").await;
        assert!(warmed.contains("foo"));
        assert!(warmed.contains("bar"));

        let cached = {
            let cache = lib.combined_entries.load();
            cache
                .get("pkg")
                .map(|entry| Arc::clone(&entry.exports))
                .expect("warm_all_exports should populate combined_entries")
        };
        assert!(
            Arc::ptr_eq(&warmed, &cached),
            "warm_all_exports should return the cached Arc instead of materializing an owned set"
        );

        let owned = lib.get_all_exports("pkg").await;
        assert_eq!(owned, HashSet::from(["foo".to_string(), "bar".to_string()]));
    }

    #[tokio::test]
    async fn test_get_all_exports_nonexistent_package() {
        // Test that get_all_exports returns empty set for non-existent package
        let lib = PackageLibrary::new_empty();

        let all_exports = lib.get_all_exports("nonexistent").await;

        assert!(all_exports.is_empty());
    }

    #[tokio::test]
    async fn test_get_all_exports_diamond_dependency() {
        // Test diamond dependency pattern: A -> B, A -> C, B -> D, C -> D
        let lib = PackageLibrary::new_empty();

        // Create package D (shared dependency)
        let mut d_exports = HashSet::new();
        d_exports.insert("d_func".to_string());
        let d_info = PackageInfo::new("pkgD".to_string(), d_exports);
        lib.insert_package(d_info).await;

        // Create package B (depends on D)
        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgD".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        // Create package C (depends on D)
        let mut c_exports = HashSet::new();
        c_exports.insert("c_func".to_string());
        let c_info = PackageInfo::with_details(
            "pkgC".to_string(),
            c_exports,
            vec!["pkgD".to_string()],
            Vec::new(),
        );
        lib.insert_package(c_info).await;

        // Create package A (depends on B and C)
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string(), "pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        let all_exports = lib.get_all_exports("pkgA").await;

        // Should have exports from all four packages (D only counted once)
        assert!(all_exports.contains("a_func"));
        assert!(all_exports.contains("b_func"));
        assert!(all_exports.contains("c_func"));
        assert!(all_exports.contains("d_func"));
        assert_eq!(all_exports.len(), 4);
    }

    #[tokio::test]
    async fn test_get_all_exports_with_r_subprocess() {
        // Integration test with real R subprocess
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // stats package typically depends on base packages
        let all_exports = lib.get_all_exports("stats").await;

        // Should have stats exports
        assert!(!all_exports.is_empty(), "stats should have exports");
        // Common stats functions
        assert!(
            all_exports.contains("lm")
                || all_exports.contains("t.test")
                || all_exports.contains("sd"),
            "stats should have common statistical functions"
        );
    }

    // ============================================================================
    // Property-Based Tests for Cache Idempotence - Task 3.6
    // ============================================================================

    use proptest::prelude::*;

    /// Strategy to generate valid R package names
    ///
    /// R package names must:
    /// - Start with a letter
    /// - Contain only letters, digits, and dots
    /// - Not end with a dot
    /// - Be at least 2 characters long
    fn package_name_strategy() -> impl Strategy<Value = String> {
        // Generate package names that look like real R packages
        prop::string::string_regex("[a-z][a-z0-9]{1,9}")
            .unwrap()
            .prop_filter("Package name must be at least 2 chars", |s| s.len() >= 2)
    }

    /// Strategy to generate a set of export names
    fn exports_strategy() -> impl Strategy<Value = HashSet<String>> {
        prop::collection::hash_set(
            prop::string::string_regex("[a-z][a-zA-Z0-9_.]{0,15}").unwrap(),
            0..=20,
        )
    }

    /// Strategy to generate a list of dependency package names
    fn depends_strategy() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(package_name_strategy(), 0..=5)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: package-function-awareness, Property 15: Cache Idempotence
        // **Validates: Requirements 3.7, 13.1, 13.2**
        //
        // Property 15a: For any package P, repeated calls to get_package(P) SHALL
        // return identical results (same Arc pointer after caching).
        #[test]
        fn prop_cache_idempotence_same_arc(
            pkg_name in package_name_strategy(),
            exports in exports_strategy()
        ) {
            // Use tokio runtime for async test
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Pre-populate cache with a package
                let info = PackageInfo::new(pkg_name.clone(), exports.clone());
                lib.insert_package(info).await;

                // Call get_package multiple times
                let result1 = lib.get_package(&pkg_name).await;
                let result2 = lib.get_package(&pkg_name).await;
                let result3 = lib.get_package(&pkg_name).await;

                // All results should be Some
                prop_assert!(result1.is_some(), "First call should return Some");
                prop_assert!(result2.is_some(), "Second call should return Some");
                prop_assert!(result3.is_some(), "Third call should return Some");

                let arc1 = result1.unwrap();
                let arc2 = result2.unwrap();
                let arc3 = result3.unwrap();

                // All should point to the same Arc (cache consistency)
                prop_assert!(
                    Arc::ptr_eq(&arc1, &arc2),
                    "First and second calls should return same Arc"
                );
                prop_assert!(
                    Arc::ptr_eq(&arc2, &arc3),
                    "Second and third calls should return same Arc"
                );

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 15: Cache Idempotence
        // **Validates: Requirements 3.7, 13.1, 13.2**
        //
        // Property 15b: For any package P, the exports returned by repeated calls
        // to get_package(P) SHALL be identical sets.
        #[test]
        fn prop_cache_idempotence_same_exports(
            pkg_name in package_name_strategy(),
            exports in exports_strategy()
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Pre-populate cache
                let info = PackageInfo::new(pkg_name.clone(), exports.clone());
                lib.insert_package(info).await;

                // Call get_package multiple times
                let result1 = lib.get_package(&pkg_name).await;
                let result2 = lib.get_package(&pkg_name).await;

                let pkg1 = result1.unwrap();
                let pkg2 = result2.unwrap();

                // Exports should be identical
                prop_assert_eq!(
                    &pkg1.exports,
                    &pkg2.exports,
                    "Exports should be identical across calls"
                );

                // Exports should match what we inserted
                prop_assert_eq!(
                    &pkg1.exports,
                    &exports,
                    "Exports should match original inserted exports"
                );

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 15: Cache Idempotence
        // **Validates: Requirements 3.7, 13.1, 13.2**
        //
        // Property 15c: For any package P with dependencies, repeated calls to
        // get_all_exports(P) SHALL return identical export sets.
        #[test]
        fn prop_cache_idempotence_all_exports(
            pkg_name in package_name_strategy(),
            exports in exports_strategy(),
            depends in depends_strategy()
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Pre-populate cache with main package and its dependencies
                let info = PackageInfo::with_details(
                    pkg_name.clone(),
                    exports.clone(),
                    depends.clone(),
                    Vec::new(),
                );
                lib.insert_package(info).await;

                // Also insert dependency packages (with empty exports for simplicity)
                for dep in &depends {
                    if !lib.is_cached(dep).await {
                        let dep_info = PackageInfo::new(dep.clone(), HashSet::new());
                        lib.insert_package(dep_info).await;
                    }
                }

                // Call get_all_exports multiple times
                let result1 = lib.get_all_exports(&pkg_name).await;
                let result2 = lib.get_all_exports(&pkg_name).await;
                let result3 = lib.get_all_exports(&pkg_name).await;

                // All results should be identical
                prop_assert_eq!(
                    &result1,
                    &result2,
                    "First and second get_all_exports should return identical sets"
                );
                prop_assert_eq!(
                    &result2,
                    &result3,
                    "Second and third get_all_exports should return identical sets"
                );

                // Results should contain the original exports
                for export in &exports {
                    prop_assert!(
                        result1.contains(export),
                        "Result should contain original export '{}'",
                        export
                    );
                }

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 15: Cache Idempotence
        // **Validates: Requirements 3.7, 13.1, 13.2**
        //
        // Property 15d: Cache SHALL be populated after first get_package call,
        // and is_cached SHALL return true for subsequent checks.
        #[test]
        fn prop_cache_populated_after_first_call(
            pkg_name in package_name_strategy(),
            exports in exports_strategy()
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Pre-populate cache (simulating what get_package does internally)
                let info = PackageInfo::new(pkg_name.clone(), exports);
                lib.insert_package(info).await;

                // Verify cache is populated
                prop_assert!(
                    lib.is_cached(&pkg_name).await,
                    "Package should be cached after insertion"
                );

                // Call get_package
                let result = lib.get_package(&pkg_name).await;
                prop_assert!(result.is_some(), "get_package should return Some");

                // Cache should still be populated
                prop_assert!(
                    lib.is_cached(&pkg_name).await,
                    "Package should still be cached after get_package"
                );

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 15: Cache Idempotence
        // **Validates: Requirements 3.7, 13.1, 13.2**
        //
        // Property 15e: Multiple packages can be cached independently, and
        // accessing one SHALL NOT affect the cached state of others.
        #[test]
        fn prop_cache_independence(
            pkg1_name in package_name_strategy(),
            pkg2_name in package_name_strategy(),
            exports1 in exports_strategy(),
            exports2 in exports_strategy()
        ) {
            // Skip if package names are the same
            prop_assume!(pkg1_name != pkg2_name);

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Insert both packages
                let info1 = PackageInfo::new(pkg1_name.clone(), exports1.clone());
                let info2 = PackageInfo::new(pkg2_name.clone(), exports2.clone());
                lib.insert_package(info1).await;
                lib.insert_package(info2).await;

                // Access pkg1 multiple times
                let result1a = lib.get_package(&pkg1_name).await;
                let result1b = lib.get_package(&pkg1_name).await;

                // Access pkg2
                let result2 = lib.get_package(&pkg2_name).await;

                // Access pkg1 again
                let result1c = lib.get_package(&pkg1_name).await;

                // All pkg1 results should be the same Arc
                let arc1a = result1a.unwrap();
                let arc1b = result1b.unwrap();
                let arc1c = result1c.unwrap();
                prop_assert!(Arc::ptr_eq(&arc1a, &arc1b));
                prop_assert!(Arc::ptr_eq(&arc1b, &arc1c));

                // pkg2 should have its own Arc
                let arc2 = result2.unwrap();
                prop_assert!(!Arc::ptr_eq(&arc1a, &arc2), "Different packages should have different Arcs");

                Ok(())
            })?;
        }
    }

    // ============================================================================
    // Property-Based Tests for Circular Dependency Handling - Task 3.7
    // ============================================================================

    /// Strategy to generate a dependency graph with guaranteed cycles
    ///
    /// Generates a set of packages where all packages are connected in a single cycle.
    /// Returns a Vec of (name, exports, depends) tuples.
    fn circular_deps_strategy() -> impl Strategy<Value = Vec<(String, HashSet<String>, Vec<String>)>>
    {
        // Generate 2-6 packages with circular dependencies
        (2..=6usize).prop_flat_map(|num_packages| {
            // Generate unique package names
            let names = prop::collection::vec(
                prop::string::string_regex("[a-z][a-z0-9]{1,5}").unwrap(),
                num_packages,
            );

            // Generate exports for each package
            let exports = prop::collection::vec(
                prop::collection::hash_set(
                    prop::string::string_regex("[a-z][a-zA-Z0-9_]{0,10}").unwrap(),
                    1..=5,
                ),
                num_packages,
            );

            (names, exports).prop_map(|(mut names, exports)| {
                // Ensure unique names by appending index if needed
                let mut seen = std::collections::HashSet::new();
                for (i, name) in names.iter_mut().enumerate() {
                    while seen.contains(name) {
                        name.push_str(&format!("{}", i));
                    }
                    seen.insert(name.clone());
                }

                let mut packages: Vec<(String, HashSet<String>, Vec<String>)> = Vec::new();

                // Create all packages first with empty dependencies
                for (name, exp) in names.into_iter().zip(exports) {
                    packages.push((name, exp, Vec::new()));
                }

                // Create a single cycle connecting all packages: A -> B -> C -> ... -> A
                // This ensures all packages are in the same cycle
                for i in 0..packages.len() {
                    let next_idx = (i + 1) % packages.len();
                    packages[i].2 = vec![packages[next_idx].0.clone()];
                }

                packages
            })
        })
    }

    /// Strategy to generate self-referential dependency (package depends on itself)
    fn self_dependency_strategy() -> impl Strategy<Value = (String, HashSet<String>)> {
        (
            prop::string::string_regex("[a-z][a-z0-9]{1,5}").unwrap(),
            prop::collection::hash_set(
                prop::string::string_regex("[a-z][a-zA-Z0-9_]{0,10}").unwrap(),
                1..=5,
            ),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: package-function-awareness, Property 7: Circular Dependency Handling
        // **Validates: Requirement 4.5**
        //
        // Property 7a: For any set of packages with circular dependencies in their
        // Depends fields, the Package_Resolver SHALL terminate and return exports
        // without infinite loops.
        #[test]
        fn prop_circular_dependency_terminates(packages in circular_deps_strategy()) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Insert all packages into the cache
                for (name, exports, depends) in &packages {
                    let info = PackageInfo::with_details(
                        name.clone(),
                        exports.clone(),
                        depends.clone(),
                        Vec::new(),
                    );
                    lib.insert_package(info).await;
                }

                // Call get_all_exports on the first package
                // This should terminate without hanging (the test itself acts as a timeout)
                if let Some((first_name, _, _)) = packages.first() {
                    let result = lib.get_all_exports(first_name).await;

                    // The function should return a non-empty result (at least the first package's exports)
                    prop_assert!(
                        !result.is_empty() || packages.iter().all(|(_, e, _)| e.is_empty()),
                        "get_all_exports should return exports from the dependency graph"
                    );
                }

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 7: Circular Dependency Handling
        // **Validates: Requirement 4.5**
        //
        // Property 7b: For any set of packages with circular dependencies,
        // get_all_exports SHALL return exports from ALL packages in the cycle.
        #[test]
        fn prop_circular_dependency_returns_all_exports(packages in circular_deps_strategy()) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Collect all expected exports
                let mut all_expected_exports = HashSet::new();
                for (_, exports, _) in &packages {
                    for export in exports {
                        all_expected_exports.insert(export.clone());
                    }
                }

                // Insert all packages into the cache
                for (name, exports, depends) in &packages {
                    let info = PackageInfo::with_details(
                        name.clone(),
                        exports.clone(),
                        depends.clone(),
                        Vec::new(),
                    );
                    lib.insert_package(info).await;
                }

                // Call get_all_exports on the first package
                if let Some((first_name, _, _)) = packages.first() {
                    let result = lib.get_all_exports(first_name).await;

                    // All exports from all packages should be in the result
                    // (since they're all connected via circular dependencies)
                    for export in &all_expected_exports {
                        prop_assert!(
                            result.contains(export),
                            "Result should contain export '{}' from the circular dependency graph",
                            export
                        );
                    }
                }

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 7: Circular Dependency Handling
        // **Validates: Requirement 4.5**
        //
        // Property 7c: Self-dependencies (A depends on A) SHALL be handled without
        // infinite loops and return the package's exports.
        #[test]
        fn prop_self_dependency_terminates((pkg_name, exports) in self_dependency_strategy()) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Create a package that depends on itself
                let info = PackageInfo::with_details(
                    pkg_name.clone(),
                    exports.clone(),
                    vec![pkg_name.clone()], // Self-dependency
                    Vec::new(),
                );
                lib.insert_package(info).await;

                // Call get_all_exports - should terminate without hanging
                let result = lib.get_all_exports(&pkg_name).await;

                // Should return the package's exports
                prop_assert_eq!(
                    result,
                    exports,
                    "Self-dependent package should return its own exports"
                );

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 7: Circular Dependency Handling
        // **Validates: Requirement 4.5**
        //
        // Property 7d: For circular dependencies, calling get_all_exports from ANY
        // package in the cycle SHALL return the same set of exports.
        #[test]
        fn prop_circular_dependency_consistent_from_any_start(packages in circular_deps_strategy()) {
            // Only test if we have at least 2 packages
            prop_assume!(packages.len() >= 2);

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Insert all packages into the cache
                for (name, exports, depends) in &packages {
                    let info = PackageInfo::with_details(
                        name.clone(),
                        exports.clone(),
                        depends.clone(),
                        Vec::new(),
                    );
                    lib.insert_package(info).await;
                }

                // Get exports starting from first package
                let first_name = &packages[0].0;
                let result_from_first = lib.get_all_exports(first_name).await;

                // Get exports starting from second package
                let second_name = &packages[1].0;
                let result_from_second = lib.get_all_exports(second_name).await;

                // Both should return the same set of exports
                // (since all packages are connected via circular dependencies)
                prop_assert_eq!(
                    result_from_first,
                    result_from_second,
                    "get_all_exports should return same exports regardless of starting package in cycle"
                );

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 7: Circular Dependency Handling
        // **Validates: Requirement 4.5**
        //
        // Property 7e: Repeated calls to get_all_exports on circular dependencies
        // SHALL return identical results (idempotence with cycles).
        #[test]
        fn prop_circular_dependency_idempotent(packages in circular_deps_strategy()) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Insert all packages into the cache
                for (name, exports, depends) in &packages {
                    let info = PackageInfo::with_details(
                        name.clone(),
                        exports.clone(),
                        depends.clone(),
                        Vec::new(),
                    );
                    lib.insert_package(info).await;
                }

                if let Some((first_name, _, _)) = packages.first() {
                    // Call get_all_exports multiple times
                    let result1 = lib.get_all_exports(first_name).await;
                    let result2 = lib.get_all_exports(first_name).await;
                    let result3 = lib.get_all_exports(first_name).await;

                    // All results should be identical
                    prop_assert_eq!(
                        &result1,
                        &result2,
                        "First and second calls should return identical results"
                    );
                    prop_assert_eq!(
                        &result2,
                        &result3,
                        "Second and third calls should return identical results"
                    );
                }

                Ok(())
            })?;
        }
    }

    // ============================================================================
    // Additional unit tests for edge cases in circular dependency handling
    // ============================================================================

    #[tokio::test]
    async fn test_complex_cycle_a_b_c_a() {
        // Test complex cycle: A -> B -> C -> A
        let lib = PackageLibrary::new_empty();

        // Create package A (depends on B)
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        // Create package B (depends on C)
        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        // Create package C (depends on A - completing the cycle)
        let mut c_exports = HashSet::new();
        c_exports.insert("c_func".to_string());
        let c_info = PackageInfo::with_details(
            "pkgC".to_string(),
            c_exports,
            vec!["pkgA".to_string()],
            Vec::new(),
        );
        lib.insert_package(c_info).await;

        // Should terminate and return all exports
        let all_exports = lib.get_all_exports("pkgA").await;

        assert!(all_exports.contains("a_func"), "Should have A's export");
        assert!(all_exports.contains("b_func"), "Should have B's export");
        assert!(all_exports.contains("c_func"), "Should have C's export");
        assert_eq!(all_exports.len(), 3);
    }

    #[tokio::test]
    async fn test_multiple_cycles_in_graph() {
        // Test graph with multiple cycles: A <-> B, C <-> D, A -> C
        let lib = PackageLibrary::new_empty();

        // Cycle 1: A <-> B
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string(), "pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgA".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        // Cycle 2: C <-> D
        let mut c_exports = HashSet::new();
        c_exports.insert("c_func".to_string());
        let c_info = PackageInfo::with_details(
            "pkgC".to_string(),
            c_exports,
            vec!["pkgD".to_string()],
            Vec::new(),
        );
        lib.insert_package(c_info).await;

        let mut d_exports = HashSet::new();
        d_exports.insert("d_func".to_string());
        let d_info = PackageInfo::with_details(
            "pkgD".to_string(),
            d_exports,
            vec!["pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(d_info).await;

        // Should terminate and return all exports from both cycles
        let all_exports = lib.get_all_exports("pkgA").await;

        assert!(all_exports.contains("a_func"));
        assert!(all_exports.contains("b_func"));
        assert!(all_exports.contains("c_func"));
        assert!(all_exports.contains("d_func"));
        assert_eq!(all_exports.len(), 4);
    }

    #[tokio::test]
    async fn test_cycle_with_external_dependency() {
        // Test cycle with an external non-cyclic dependency
        // A <-> B, both depend on C (no cycle)
        let lib = PackageLibrary::new_empty();

        // External package C (no cycle)
        let mut c_exports = HashSet::new();
        c_exports.insert("c_func".to_string());
        let c_info = PackageInfo::new("pkgC".to_string(), c_exports);
        lib.insert_package(c_info).await;

        // Cycle: A <-> B, both also depend on C
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string(), "pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgA".to_string(), "pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        let all_exports = lib.get_all_exports("pkgA").await;

        assert!(all_exports.contains("a_func"));
        assert!(all_exports.contains("b_func"));
        assert!(all_exports.contains("c_func"));
        assert_eq!(all_exports.len(), 3);
    }

    // ============================================================================
    // Tests for package_exists() - Task 10.3
    // ============================================================================

    #[test]
    fn test_package_exists_base_package() {
        // Test that base packages are always reported as existing
        // Validates: Requirement 15.1 (base packages should not trigger missing package diagnostic)
        let mut lib = PackageLibrary::new_empty();

        let mut base_packages = HashSet::new();
        base_packages.insert("base".to_string());
        base_packages.insert("stats".to_string());
        base_packages.insert("utils".to_string());
        lib.set_base_packages(base_packages);

        assert!(lib.package_exists("base"), "base should exist");
        assert!(lib.package_exists("stats"), "stats should exist");
        assert!(lib.package_exists("utils"), "utils should exist");
    }

    #[tokio::test]
    async fn test_package_exists_cached_package_not_on_disk() {
        // Cache presence does NOT prove installation. prefetch_packages()
        // inserts an empty-exports cache entry for any package the R query
        // fails to load (asNamespace error swallowed by tryCatch), so trusting
        // the cache here would suppress "Package not installed" diagnostics
        // for never-installed packages that happen to be cached.
        let lib = PackageLibrary::new_empty();

        let info = PackageInfo::new("dplyr".to_string(), HashSet::new());
        lib.insert_package(info).await;

        assert!(
            !lib.package_exists("dplyr"),
            "cached-but-not-on-disk package must not be reported as installed"
        );
    }

    #[test]
    fn test_package_exists_nonexistent_package() {
        // Test that non-existent packages are reported as not existing
        // Validates: Requirement 15.1 (non-installed packages should trigger diagnostic)
        let lib = PackageLibrary::new_empty();

        assert!(
            !lib.package_exists("__nonexistent_package_xyz__"),
            "non-existent package should not exist"
        );
    }

    #[tokio::test]
    async fn test_package_exists_with_r_subprocess() {
        // Test package_exists with actual R subprocess
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Base packages should exist
        assert!(lib.package_exists("base"), "base should exist with R");
        assert!(lib.package_exists("stats"), "stats should exist with R");

        // Non-existent package should not exist
        assert!(
            !lib.package_exists("__nonexistent_package_xyz__"),
            "non-existent package should not exist"
        );
    }

    // ============================================================================
    // Tests for get_exports_for_completions() - Task 11.1
    // ============================================================================

    #[tokio::test]
    async fn test_get_exports_for_completions_empty_packages() {
        // Test that empty package list returns empty map
        // Validates: Requirement 9.1 (completions from loaded packages)
        let lib = PackageLibrary::new_empty();

        let result = lib.get_exports_for_completions(&[]);
        assert!(
            result.is_empty(),
            "Empty package list should return empty map"
        );
    }

    #[tokio::test]
    async fn test_get_exports_for_completions_single_package() {
        // Test that exports from a single package are returned with package attribution
        // Validates: Requirements 9.1, 9.2
        let lib = PackageLibrary::new_empty();

        let mut exports = HashSet::new();
        exports.insert("mutate".to_string());
        exports.insert("filter".to_string());
        exports.insert("select".to_string());
        let info = PackageInfo::new("dplyr".to_string(), exports);
        lib.insert_package(info).await;

        let result = lib.get_exports_for_completions(&["dplyr".to_string()]);

        assert_eq!(result.len(), 3, "Should have 3 exports");
        assert_eq!(
            result.get("mutate"),
            Some(&vec!["dplyr".to_string()]),
            "mutate should be from dplyr"
        );
        assert_eq!(
            result.get("filter"),
            Some(&vec!["dplyr".to_string()]),
            "filter should be from dplyr"
        );
        assert_eq!(
            result.get("select"),
            Some(&vec!["dplyr".to_string()]),
            "select should be from dplyr"
        );
    }

    #[tokio::test]
    async fn test_get_exports_for_completions_multiple_packages() {
        // Test that exports from multiple packages are returned with correct attribution
        // Validates: Requirements 9.1, 9.2
        let lib = PackageLibrary::new_empty();

        // Add dplyr
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        dplyr_exports.insert("filter".to_string());
        let dplyr_info = PackageInfo::new("dplyr".to_string(), dplyr_exports);
        lib.insert_package(dplyr_info).await;

        // Add ggplot2
        let mut ggplot2_exports = HashSet::new();
        ggplot2_exports.insert("ggplot".to_string());
        ggplot2_exports.insert("aes".to_string());
        let ggplot2_info = PackageInfo::new("ggplot2".to_string(), ggplot2_exports);
        lib.insert_package(ggplot2_info).await;

        let result = lib.get_exports_for_completions(&["dplyr".to_string(), "ggplot2".to_string()]);

        assert_eq!(result.len(), 4, "Should have 4 exports total");
        assert_eq!(result.get("mutate"), Some(&vec!["dplyr".to_string()]));
        assert_eq!(result.get("filter"), Some(&vec!["dplyr".to_string()]));
        assert_eq!(result.get("ggplot"), Some(&vec!["ggplot2".to_string()]));
        assert_eq!(result.get("aes"), Some(&vec!["ggplot2".to_string()]));
    }

    #[tokio::test]
    async fn test_get_exports_for_completions_duplicate_exports_shows_all() {
        // Test that when multiple packages export the same symbol, all packages are shown
        // Validates: Requirement 9.3 (multiple packages export same symbol, show all)
        let lib = PackageLibrary::new_empty();

        // Add dplyr with filter
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("filter".to_string());
        let dplyr_info = PackageInfo::new("dplyr".to_string(), dplyr_exports);
        lib.insert_package(dplyr_info).await;

        // Add stats with filter (same name)
        let mut stats_exports = HashSet::new();
        stats_exports.insert("filter".to_string());
        let stats_info = PackageInfo::new("stats".to_string(), stats_exports);
        lib.insert_package(stats_info).await;

        // Both packages should be shown for filter, in load order
        let result = lib.get_exports_for_completions(&["dplyr".to_string(), "stats".to_string()]);
        assert_eq!(
            result.get("filter"),
            Some(&vec!["dplyr".to_string(), "stats".to_string()]),
            "Both dplyr and stats should be shown for filter, in load order"
        );

        // Reverse order: stats first, then dplyr
        let result2 = lib.get_exports_for_completions(&["stats".to_string(), "dplyr".to_string()]);
        assert_eq!(
            result2.get("filter"),
            Some(&vec!["stats".to_string(), "dplyr".to_string()]),
            "Both stats and dplyr should be shown for filter, in load order"
        );
    }

    #[tokio::test]
    async fn test_get_exports_for_completions_uncached_package() {
        // Test that uncached packages are skipped (no exports returned)
        // Validates: Requirement 9.1 (only cached packages contribute)
        let lib = PackageLibrary::new_empty();

        // Add dplyr to cache
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        let dplyr_info = PackageInfo::new("dplyr".to_string(), dplyr_exports);
        lib.insert_package(dplyr_info).await;

        // Request exports from dplyr (cached) and ggplot2 (not cached)
        let result = lib.get_exports_for_completions(&["dplyr".to_string(), "ggplot2".to_string()]);

        assert_eq!(
            result.len(),
            1,
            "Should only have exports from cached package"
        );
        assert_eq!(result.get("mutate"), Some(&vec!["dplyr".to_string()]));
        assert!(
            !result.contains_key("ggplot"),
            "ggplot should not be in results"
        );
    }

    #[test]
    fn test_get_exports_for_completions_synchronous() {
        // Test that the method works synchronously (no async required)
        // This is important for use in completion handlers
        let lib = PackageLibrary::new_empty();

        // The method should work even without async runtime
        let result = lib.get_exports_for_completions(&[]);
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn invalidate_many_removes_all_listed_packages() {
        use std::collections::HashSet;

        let lib = PackageLibrary::new_empty();
        lib.insert_package(PackageInfo::new("dplyr".into(), HashSet::new()))
            .await;
        lib.insert_package(PackageInfo::new("ggplot2".into(), HashSet::new()))
            .await;
        lib.insert_package(PackageInfo::new("readr".into(), HashSet::new()))
            .await;
        assert_eq!(lib.cached_count().await, 3);

        let to_invalidate: HashSet<String> = ["dplyr".into(), "readr".into()].into_iter().collect();
        lib.invalidate_many(&to_invalidate).await;

        assert_eq!(lib.cached_count().await, 1);
        assert!(lib.is_cached("ggplot2").await);
        assert!(!lib.is_cached("dplyr").await);
        assert!(!lib.is_cached("readr").await);
    }

    #[tokio::test]
    async fn invalidate_many_clears_combined_entries_for_meta_packages() {
        use std::collections::HashSet;

        let lib = PackageLibrary::new_empty();
        // Seed packages cache too so invalidate_many can discover dependent
        // combined keys from cached PackageInfo.attached_packages.
        let tidyverse_info =
            PackageInfo::with_details("tidyverse".into(), HashSet::new(), vec![], vec![]);
        lib.insert_package(tidyverse_info).await;
        // Seed combined_entries as though tidyverse had been loaded.
        seed_combined_entry(
            &lib,
            "tidyverse",
            ["mutate".to_string(), "ggplot".to_string()]
                .into_iter()
                .collect(),
            HashMap::new(),
        )
        .await;
        seed_combined_entry(
            &lib,
            "dplyr",
            ["mutate".to_string()].into_iter().collect(),
            HashMap::new(),
        )
        .await;

        // Invalidate a child (dplyr) — the meta-package combined entry must be dropped too.
        let set: HashSet<String> = ["dplyr".to_string()].into_iter().collect();
        let invalidated = lib.invalidate_many(&set).await;

        let combined = lib.combined_entries.load();
        assert!(!combined.contains_key("tidyverse"));
        assert!(!combined.contains_key("dplyr"));

        // Returned set surfaces which combined_entries keys were actually
        // dropped so callers can revalidate documents that loaded tidyverse
        // (not dplyr) directly.
        assert!(invalidated.contains("tidyverse"));
        assert!(invalidated.contains("dplyr"));

        // invalidate_many must drop combined_entries entries but preserve the
        // per-package PackageInfo cache: the meta-package itself still exists
        // on disk, so its individual entry should remain available for
        // subsequent re-aggregation.
        drop(combined);
        let packages = lib.packages.load();
        assert!(
            packages.contains_key("tidyverse"),
            "PackageInfo for tidyverse must survive invalidate_many"
        );
    }

    #[tokio::test]
    async fn invalidate_many_returns_empty_for_names_not_in_combined_entries() {
        use std::collections::HashSet;
        let lib = PackageLibrary::new_empty();
        lib.insert_package(PackageInfo::new(
            "uncached_meta_child".into(),
            HashSet::new(),
        ))
        .await;

        // combined_entries is empty for this package name — invalidate_many
        // must not claim it was dropped.
        let set: HashSet<String> = ["uncached_meta_child".to_string()].into_iter().collect();
        let invalidated = lib.invalidate_many(&set).await;
        assert!(invalidated.is_empty());
    }

    #[tokio::test]
    async fn invalidate_many_clears_combined_entries_for_transitive_depends() {
        use std::collections::HashSet;

        let lib = PackageLibrary::new_empty();
        // Package `A` Depends on `B`. Its PackageInfo is cached, and its
        // combined_entries aggregate has been built.
        let a_info = PackageInfo::with_details(
            "A".into(),
            ["a_fn".to_string()].into_iter().collect(),
            vec!["B".to_string()],
            vec![],
        );
        lib.insert_package(a_info).await;
        seed_combined_entry(
            &lib,
            "A",
            ["a_fn".to_string(), "b_fn".to_string()]
                .into_iter()
                .collect(),
            HashMap::new(),
        )
        .await;

        // Invalidate B — A's combined aggregate rolled up B's exports, so it
        // must be dropped even though B is not a direct hit and A is not a
        // hardcoded meta-package.
        let set: HashSet<String> = ["B".to_string()].into_iter().collect();
        let invalidated = lib.invalidate_many(&set).await;

        let combined = lib.combined_entries.load();
        assert!(
            !combined.contains_key("A"),
            "A's combined_entries aggregate should be cleared when its dependency B is invalidated"
        );
        // Returned set surfaces A so consumers know documents using A need revalidation.
        assert!(invalidated.contains("A"));
    }

    #[tokio::test]
    async fn cached_package_names_returns_current_keys() {
        use std::collections::HashSet;
        let lib = PackageLibrary::new_empty();
        lib.insert_package(PackageInfo::new("a".into(), HashSet::new()))
            .await;
        lib.insert_package(PackageInfo::new("b".into(), HashSet::new()))
            .await;

        let names = lib.cached_package_names().await;
        assert_eq!(
            names,
            ["a".to_string(), "b".to_string()].into_iter().collect()
        );
    }

    #[tokio::test]
    async fn providers_default_empty_and_settable() {
        use crate::package_db::PackageMetadataProvider;
        use crate::package_library::PackageInfo;
        use std::collections::HashSet;

        struct Fake;
        impl PackageMetadataProvider for Fake {
            fn lookup(&self, name: &str) -> Option<PackageInfo> {
                if name == "fakepkg" {
                    Some(PackageInfo::new(
                        "fakepkg".into(),
                        HashSet::from(["zzz".into()]),
                    ))
                } else {
                    None
                }
            }
        }

        let mut lib = PackageLibrary::new_empty();
        assert!(!lib.has_providers());
        lib.set_providers(vec![Box::new(Fake)]);
        assert!(lib.has_providers());
    }

    #[tokio::test]
    async fn get_package_falls_back_to_provider_when_not_installed() {
        use crate::package_db::PackageMetadataProvider;
        use std::collections::HashSet;

        struct Fake;
        impl PackageMetadataProvider for Fake {
            fn lookup(&self, name: &str) -> Option<PackageInfo> {
                (name == "fakepkg")
                    .then(|| PackageInfo::new("fakepkg".into(), HashSet::from(["zzz".into()])))
            }
        }

        // new_empty has no lib paths, so find_package_directory always misses.
        let mut lib = PackageLibrary::new_empty();
        lib.set_providers(vec![Box::new(Fake)]);

        let info = lib
            .get_package("fakepkg")
            .await
            .expect("resolved via provider");
        assert!(info.exports.contains("zzz"));
        // Provider hits ARE cached.
        assert!(lib.is_cached("fakepkg").await);

        // A package no provider knows stays unresolved and uncached.
        assert!(lib.get_package("unknownpkg").await.is_none());
        assert!(!lib.is_cached("unknownpkg").await);
    }

    #[tokio::test]
    async fn prefetch_warms_provider_packages_via_existing_step3() {
        use crate::package_db::PackageMetadataProvider;
        use std::collections::HashSet;

        struct Fake;
        impl PackageMetadataProvider for Fake {
            fn lookup(&self, name: &str) -> Option<PackageInfo> {
                (name == "dplyr")
                    .then(|| PackageInfo::new("dplyr".into(), HashSet::from(["mutate".into()])))
            }
        }

        let mut lib = PackageLibrary::new_empty();
        lib.set_providers(vec![Box::new(Fake)]);

        lib.prefetch_packages(&["dplyr".to_string(), "ggplot2".to_string()])
            .await;

        assert!(lib.is_cached("dplyr").await);
        assert!(lib.is_symbol_from_loaded_packages("mutate", &["dplyr".to_string()]));
        assert!(!lib.is_cached("ggplot2").await);
    }

    #[tokio::test]
    async fn package_exists_never_consults_providers() {
        use crate::package_db::PackageMetadataProvider;
        use std::collections::HashSet;

        struct Fake;
        impl PackageMetadataProvider for Fake {
            fn lookup(&self, name: &str) -> Option<PackageInfo> {
                (name == "dplyr")
                    .then(|| PackageInfo::new("dplyr".into(), HashSet::from(["mutate".into()])))
            }
        }

        let mut lib = PackageLibrary::new_empty();
        lib.set_providers(vec![Box::new(Fake)]);

        // The provider KNOWS dplyr's exports, but it is not installed on disk.
        // package_exists answers "is it installed?", which stays Tier-1-only.
        assert!(!lib.package_exists("dplyr"));
    }

    #[tokio::test]
    async fn tier2_outranks_tier3() {
        use crate::package_db::PackageMetadataProvider;
        use std::collections::HashSet;

        struct Tier2;
        impl PackageMetadataProvider for Tier2 {
            fn lookup(&self, name: &str) -> Option<PackageInfo> {
                (name == "dplyr")
                    .then(|| PackageInfo::new("dplyr".into(), HashSet::from(["from_tier2".into()])))
            }
        }
        struct Tier3;
        impl PackageMetadataProvider for Tier3 {
            fn lookup(&self, name: &str) -> Option<PackageInfo> {
                match name {
                    "dplyr" => Some(PackageInfo::new(
                        "dplyr".into(),
                        HashSet::from(["from_tier3".into()]),
                    )),
                    "tidyr" => Some(PackageInfo::new(
                        "tidyr".into(),
                        HashSet::from(["pivot_longer".into()]),
                    )),
                    _ => None,
                }
            }
        }

        let mut lib = PackageLibrary::new_empty();
        lib.set_providers(vec![Box::new(Tier2), Box::new(Tier3)]); // Tier 2 first

        let dplyr = lib.get_package("dplyr").await.unwrap();
        assert!(
            dplyr.exports.contains("from_tier2"),
            "Tier 2 wins when both know dplyr"
        );
        assert!(!dplyr.exports.contains("from_tier3"));

        let tidyr = lib.get_package("tidyr").await.unwrap();
        assert!(
            tidyr.exports.contains("pivot_longer"),
            "Tier-3-only package still resolves"
        );
    }

    #[tokio::test]
    async fn initialize_caches_library_only_base_priority_packages_from_disk() {
        let _env_guard = crate::package_db::RAVEN_NAMES_DB_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let lib_root = dir.path().join("lib");
        std::fs::create_dir_all(&lib_root).unwrap();
        let _user_data_guard =
            crate::package_db::test_user_data_dir_guard(dir.path().join("missing-data"));

        let base_dir = lib_root.join("base");
        std::fs::create_dir_all(&base_dir).unwrap();
        std::fs::write(
            base_dir.join("DESCRIPTION"),
            "Package: base\nVersion: 4.6.0\nPriority: base\n",
        )
        .unwrap();
        std::fs::write(
            base_dir.join("INDEX"),
            "print                   Print Values\n",
        )
        .unwrap();

        let grid_dir = lib_root.join("grid");
        std::fs::create_dir_all(&grid_dir).unwrap();
        std::fs::write(
            grid_dir.join("DESCRIPTION"),
            "Package: grid\nVersion: 4.6.0\nPriority: base\n",
        )
        .unwrap();
        std::fs::write(grid_dir.join("NAMESPACE"), "export(grid.ls)\n").unwrap();

        let mut lib = PackageLibrary::new_empty();
        lib.set_lib_paths(vec![lib_root]);
        lib.initialize().await.unwrap();

        assert!(lib.is_base_export("print"));
        assert!(
            !lib.is_base_export("grid.ls"),
            "grid exports require library(grid); they must not be globally in scope"
        );
        assert!(
            lib.is_symbol_from_loaded_packages("grid.ls", &["grid".to_string()]),
            "library(grid) should resolve grid.ls from the initialized cache"
        );
    }

    #[tokio::test]
    async fn initialize_uses_embedded_base_exports_when_disk_and_sidecars_absent() {
        let _env_guard = crate::package_db::RAVEN_NAMES_DB_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let empty_lib = dir.path().join("empty-lib");
        std::fs::create_dir_all(&empty_lib).unwrap();
        let _user_data_guard =
            crate::package_db::test_user_data_dir_guard(dir.path().join("missing-data"));

        let mut lib = PackageLibrary::new_empty();
        lib.set_lib_paths(vec![empty_lib]);
        lib.initialize().await.unwrap();

        assert!(lib.base_exports().contains("print"));
        assert!(lib.base_exports().contains("mtcars"));
        assert!(lib.is_base_package("base"));
        assert!(lib.is_base_package("datasets"));

        // The non-attached base-priority 7 are cached for offline library()
        // resolution but MUST NOT be always-in-scope.
        assert!(
            !lib.base_exports().contains("gpar"),
            "grid::gpar must not be in the always-in-scope set"
        );
        assert!(
            !lib.is_base_package("grid"),
            "grid is base-priority but not default-attached"
        );
        let grid = lib
            .get_package("grid")
            .await
            .expect("grid resolvable offline");
        assert!(
            grid.exports.contains("gpar"),
            "library(grid) resolves gpar with no R and no names.db"
        );
    }

    #[tokio::test]
    async fn base_pattern_package_without_r_stays_partial() {
        // Regression (issue #503 review blocker): a base-priority package whose
        // NAMESPACE uses exportPattern, initialized with NO R subprocess, must
        // NOT be stamped Complete — its exports came only from the INDEX
        // approximation, so member absence is not authoritative.
        let _env_guard = crate::package_db::RAVEN_NAMES_DB_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let lib_root = dir.path().join("lib");
        std::fs::create_dir_all(&lib_root).unwrap();
        let _user_data_guard =
            crate::package_db::test_user_data_dir_guard(dir.path().join("missing-data"));

        // A real base package on disk so the embedded fallback path is skipped.
        let base_dir = lib_root.join("base");
        std::fs::create_dir_all(&base_dir).unwrap();
        std::fs::write(
            base_dir.join("DESCRIPTION"),
            "Package: base\nVersion: 4.6.0\nPriority: base\n",
        )
        .unwrap();
        std::fs::write(
            base_dir.join("INDEX"),
            "print                   Print Values\n",
        )
        .unwrap();

        // grid: base-priority, exportPattern → needs R/INDEX. INDEX lists only
        // `grid.ls`, so the cached export set is the partial approximation.
        let grid_dir = lib_root.join("grid");
        std::fs::create_dir_all(&grid_dir).unwrap();
        std::fs::write(
            grid_dir.join("DESCRIPTION"),
            "Package: grid\nVersion: 4.6.0\nPriority: base\n",
        )
        .unwrap();
        std::fs::write(grid_dir.join("NAMESPACE"), "exportPattern(\"^grid\")\n").unwrap();
        std::fs::write(
            grid_dir.join("INDEX"),
            "grid.ls                 List Grobs\n",
        )
        .unwrap();

        let mut lib = PackageLibrary::new_empty();
        lib.set_lib_paths(vec![lib_root]);
        // No r_subprocess: pattern packages can only use the INDEX fallback.
        lib.initialize().await.unwrap();

        let grid = lib.get_package("grid").await.expect("grid cached");
        assert_eq!(
            grid.exports_completeness,
            MemberCompleteness::Partial,
            "exportPattern base package without R must be Partial, not Complete"
        );
        // Therefore an unknown member is Partial (silent), never a false Absent.
        assert_eq!(
            lib.namespace_member_status_sync("grid", "definitely_not_exported"),
            NamespaceMemberStatus::Partial
        );
    }

    #[tokio::test]
    async fn embedded_fallback_does_not_override_disk_partial_base_package() {
        // Regression (issue #503 re-review blocker): a lib path with a
        // non-attached base-priority `exportPattern` package (grid) but NO
        // attached base package leaves `all_base_exports` empty, so the embedded
        // fallback fires. It must NOT pre-seed a Complete grid entry that the
        // monotonic insert guard would keep over the disk-derived Partial.
        let _env_guard = crate::package_db::RAVEN_NAMES_DB_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let lib_root = dir.path().join("lib");
        std::fs::create_dir_all(&lib_root).unwrap();
        let _user_data_guard =
            crate::package_db::test_user_data_dir_guard(dir.path().join("missing-data"));

        // Only grid on disk — no attached base package contributes exports, so
        // the embedded fallback path runs.
        let grid_dir = lib_root.join("grid");
        std::fs::create_dir_all(&grid_dir).unwrap();
        std::fs::write(
            grid_dir.join("DESCRIPTION"),
            "Package: grid\nVersion: 4.6.0\nPriority: base\n",
        )
        .unwrap();
        std::fs::write(grid_dir.join("NAMESPACE"), "exportPattern(\"^grid\")\n").unwrap();
        std::fs::write(
            grid_dir.join("INDEX"),
            "grid.ls                 List Grobs\n",
        )
        .unwrap();

        let mut lib = PackageLibrary::new_empty();
        lib.set_lib_paths(vec![lib_root]);
        lib.initialize().await.unwrap();

        let grid = lib.get_package("grid").await.expect("grid cached");
        assert_eq!(
            grid.exports_completeness,
            MemberCompleteness::Partial,
            "on-disk exportPattern grid (no R) must stay Partial even when the \
             embedded fallback runs"
        );
        assert_eq!(
            lib.namespace_member_status_sync("grid", "definitely_not_exported"),
            NamespaceMemberStatus::Partial
        );
    }

    #[test]
    fn enumerate_installed_lists_package_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(lib.join("dplyr")).unwrap();
        std::fs::create_dir_all(lib.join("ggplot2")).unwrap();
        // A package directory is identified by a DESCRIPTION file.
        std::fs::write(lib.join("dplyr").join("DESCRIPTION"), "Package: dplyr\n").unwrap();
        std::fs::write(
            lib.join("ggplot2").join("DESCRIPTION"),
            "Package: ggplot2\n",
        )
        .unwrap();
        // a non-package file should be ignored
        std::fs::write(lib.join("README"), "x").unwrap();

        let mut pl = PackageLibrary::new_empty();
        pl.set_lib_paths(vec![lib]);
        let mut found = pl.enumerate_installed_packages();
        found.sort();
        assert_eq!(found, vec!["dplyr".to_string(), "ggplot2".to_string()]);
    }

    #[test]
    fn package_version_reads_description_version() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(lib.join("dplyr")).unwrap();
        std::fs::write(
            lib.join("dplyr").join("DESCRIPTION"),
            "Package: dplyr\nVersion: 1.2.3\n",
        )
        .unwrap();
        let mut pl = PackageLibrary::new_empty();
        pl.set_lib_paths(vec![lib]);
        assert_eq!(pl.package_version("dplyr"), Some("1.2.3".to_string()));
        assert_eq!(pl.package_version("nonexistent"), None);
    }

    #[tokio::test]
    async fn initialize_without_disk_base_loads_embedded_records_into_cache() {
        // No lib paths → no disk base → embedded fallback.
        let mut lib = PackageLibrary::with_subprocess(None);
        lib.set_lib_paths(vec![std::path::PathBuf::from("/nonexistent-xyz")]);
        lib.initialize().await.unwrap();

        // Flat always-in-scope set includes a base export and a base dataset.
        assert!(lib.base_exports().contains("print"));
        assert!(lib.base_exports().contains("mtcars"));
        // Per-package cache populated from the embedded table, datasets in lazy_data.
        let datasets = lib
            .get_cached_package("datasets")
            .await
            .expect("datasets cached");
        assert!(datasets.lazy_data.contains(&"mtcars".to_string()));
    }

    #[test]
    fn load_all_owner_display_never_leaks_sentinel() {
        // With a known dev-package name, show it; otherwise a generic label.
        // Never the raw sentinel string, in any case.
        let with_name = load_all_owner_display(Some("mypkg"));
        assert!(with_name.contains("mypkg"));
        assert!(with_name.contains("package under development"));
        assert!(!with_name.contains(LOAD_ALL_SENTINEL));

        for fallback in [None, Some(""), Some("unknown")] {
            let label = load_all_owner_display(fallback);
            assert_eq!(label, "package under development");
            assert!(!label.contains(LOAD_ALL_SENTINEL));
        }
    }

    #[test]
    fn load_all_sentinel_is_recognized_and_non_colliding() {
        assert!(is_load_all_sentinel(LOAD_ALL_SENTINEL));
        assert!(!is_load_all_sentinel("dplyr"));
        assert!(!is_load_all_sentinel("load_all"));
        assert!(LOAD_ALL_SENTINEL.contains('_'));
    }

    #[test]
    fn empty_overlay_resolution_is_unchanged() {
        let lib = PackageLibrary::new_empty();
        assert!(!lib.is_symbol_from_loaded_packages("anything", &[LOAD_ALL_SENTINEL.to_string()]));
        assert_eq!(
            lib.find_package_owner_for_symbol("anything", &[LOAD_ALL_SENTINEL.to_string()]),
            None
        );
    }

    #[test]
    fn overlay_resolves_sentinel_symbols_only_when_sentinel_attached() {
        let lib = PackageLibrary::new_empty();
        let mut syms = std::collections::HashSet::new();
        syms.insert("my_func".to_string());
        lib.set_local_dev_overlay(Some(std::sync::Arc::new(LocalDevPackage { symbols: syms })));

        // Sentinel attached => resolves at all three chokepoints.
        assert!(lib.is_symbol_from_loaded_packages("my_func", &[LOAD_ALL_SENTINEL.to_string()]));
        assert_eq!(
            lib.find_package_owner_for_symbol("my_func", &[LOAD_ALL_SENTINEL.to_string()]),
            Some(LOAD_ALL_SENTINEL.to_string())
        );
        assert!(
            lib.get_owned_exports_for_completions(&[LOAD_ALL_SENTINEL.to_string()])
                .contains_key("my_func")
        );

        // Sentinel NOT attached => overlay contributes nothing (isolation guard).
        assert!(!lib.is_symbol_from_loaded_packages("my_func", &["dplyr".to_string()]));
        assert_eq!(
            lib.find_package_owner_for_symbol("my_func", &["dplyr".to_string()]),
            None
        );
        assert!(
            !lib.get_owned_exports_for_completions(&["dplyr".to_string()])
                .contains_key("my_func")
        );

        // Unknown symbol not resolved even with sentinel attached.
        assert!(
            !lib.is_symbol_from_loaded_packages("not_a_symbol", &[LOAD_ALL_SENTINEL.to_string()])
        );
    }

    // ========================================================================
    // get_exports_sync: on-demand synchronous export fetch for `pkg::`
    // completions (cache -> on-disk NAMESPACE -> providers).
    // ========================================================================

    #[tokio::test]
    async fn get_exports_sync_returns_cached_exports_and_data_sorted() {
        let lib = PackageLibrary::new_empty();
        let info = PackageInfo::with_details(
            "dplyr".to_string(),
            HashSet::from(["mutate".to_string(), "filter".to_string()]),
            Vec::new(),
            vec!["starwars".to_string()],
        );
        lib.insert_package(info).await;

        let exports = lib
            .get_exports_sync("dplyr")
            .expect("cached package resolves");
        // Functions and datasets are folded together, sorted, deduped.
        assert_eq!(exports, vec!["filter", "mutate", "starwars"]);
    }

    #[test]
    fn get_exports_sync_unknown_package_returns_none() {
        let lib = PackageLibrary::new_empty();
        assert_eq!(lib.get_exports_sync("nopkg"), None);
    }

    #[test]
    fn get_exports_sync_skips_load_all_sentinel() {
        let lib = PackageLibrary::new_empty();
        assert_eq!(lib.get_exports_sync(LOAD_ALL_SENTINEL), None);
    }

    #[test]
    fn get_exports_sync_parses_namespace_from_disk() {
        let lib_root = tempfile::tempdir().unwrap();
        let pkg_dir = lib_root.path().join("mypkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("DESCRIPTION"),
            "Package: mypkg\nVersion: 1.0\n",
        )
        .unwrap();
        std::fs::write(pkg_dir.join("NAMESPACE"), "export(foo)\nexport(bar)\n").unwrap();

        let mut lib = PackageLibrary::new_empty();
        lib.set_lib_paths(vec![lib_root.path().to_path_buf()]);

        // Never library()-loaded, never prefetched, not cached — still resolves
        // by parsing the installed NAMESPACE synchronously.
        let exports = lib
            .get_exports_sync("mypkg")
            .expect("installed package resolves from disk");
        assert_eq!(exports, vec!["bar", "foo"]);
    }

    #[test]
    fn get_exports_sync_resolves_from_providers_when_not_on_disk() {
        use crate::package_db::PackageMetadataProvider;

        struct Fake;
        impl PackageMetadataProvider for Fake {
            fn lookup(&self, name: &str) -> Option<PackageInfo> {
                (name == "repopkg").then(|| {
                    PackageInfo::with_details(
                        "repopkg".to_string(),
                        HashSet::from(["alpha".to_string(), "beta".to_string()]),
                        Vec::new(),
                        Vec::new(),
                    )
                })
            }
        }

        let mut lib = PackageLibrary::new_empty();
        lib.set_providers(vec![Box::new(Fake)]);

        let exports = lib
            .get_exports_sync("repopkg")
            .expect("provider-known package resolves");
        assert_eq!(exports, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn get_exports_sync_empty_cache_entry_falls_through_to_disk() {
        // A failed-load placeholder (empty exports) was cached for an installed
        // package — `get_package`/`prefetch` do this when a namespace fails to
        // load. The empty entry must NOT shadow the real on-disk NAMESPACE.
        let lib_root = tempfile::tempdir().unwrap();
        let pkg_dir = lib_root.path().join("fallpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("NAMESPACE"), "export(real_fn)\n").unwrap();

        let mut lib = PackageLibrary::new_empty();
        lib.set_lib_paths(vec![lib_root.path().to_path_buf()]);
        lib.insert_package(PackageInfo::new("fallpkg".to_string(), HashSet::new()))
            .await;

        let exports = lib
            .get_exports_sync("fallpkg")
            .expect("empty cache entry falls through to the on-disk NAMESPACE");
        assert_eq!(exports, vec!["real_fn"]);
    }
}
