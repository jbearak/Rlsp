//
// cross_file/dependency.rs
//
// Dependency graph for cross-file awareness
//

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use tower_lsp::lsp_types::{Diagnostic, Url};

use super::parent_resolve::{infer_call_site_from_parent, resolve_match_pattern};
use super::path_resolve::{
    PathContext, path_to_uri, resolve_path, resolve_path_with_workspace_fallback,
};
use super::types::{CallSiteSpec, CrossFileMetadata, SourceLocality};

/// Resolve the effective working directory of a parent file for inheritance,
/// with depth tracking and cycle detection to prevent infinite chains.
///
/// Returns the parent's inheritable working directory as a string path that
/// can be stored in the child's metadata. The implicit testthat/testit anchor
/// is a soft, file-local default and therefore returns `None` rather than
/// propagating into the child.
///
/// # Arguments
/// * `parent_uri` - The URI of the parent file
/// * `get_metadata` - A closure that retrieves metadata for a given URI
/// * `workspace_root` - Optional workspace root URI for resolving workspace-relative paths
/// * `remaining_depth` - Remaining depth for inheritance chain traversal
/// * `visited` - Set of URIs already visited during this inheritance resolution (for cycle detection)
///
/// # Returns
/// * `Some(String)` - The parent's effective working directory as a string path
/// * `None` - If the parent URI cannot be converted to a file path, or its
///   only working directory is the implicit testthat/testit soft default
///
/// # Fallback Behavior
/// If parent metadata cannot be retrieved via `get_metadata`, or if the depth limit is
/// reached, the function falls back to using the parent file's directory as the effective
/// working directory.
///
/// # Cycle Detection
/// When a URI is encountered that's already in the visited set, the function stops
/// inheritance and returns the *direct parent's directory*. This breaks the loop while still
/// allowing the child to inherit from its parent.
///
/// # Implicit test working directory (issue #638)
/// Every fallback that would return the parent's raw file directory (cycle,
/// depth exhausted, metadata unavailable) instead returns `None` when the
/// parent lies under `tests/testthat|testit/` — see
/// `fallback_parent_directory`. The implicit anchor never propagates into
/// sourced children, and these fallbacks must not reintroduce it as an
/// inherited (hard) working directory while the parent is simply unindexed.
///
/// # Transitive Inheritance
/// When the parent has an inherited_working_directory in its metadata (from its own parent),
/// that value is used through PathContext::from_metadata. This enables transitive inheritance:
/// A → B → C where A has `# raven: cd` (the `@lsp-cd` form is a permanent alias that
/// parses identically), B inherits from A, and C inherits from B (getting A's WD).
///
/// _Requirements: 5.1, 5.3, 9.1, 9.2, 9.3_
pub fn resolve_parent_working_directory_with_visited<F>(
    parent_uri: &Url,
    get_metadata: &F,
    workspace_root: Option<&Url>,
    remaining_depth: usize,
    visited: &mut HashSet<Url>,
) -> Option<String>
where
    F: Fn(&Url) -> Option<std::sync::Arc<CrossFileMetadata>>,
{
    // Check for cycle: if we've already visited this URI, stop and use file's directory
    // (Requirement 9.3)
    if visited.contains(parent_uri) {
        log::trace!(
            "Cycle detected when resolving parent WD for {}, falling back to parent's directory",
            parent_uri
        );
        // In a cycle, still return the direct parent's directory so the child inherits from
        // its parent (not itself), while breaking the loop.
        return fallback_parent_directory(parent_uri, workspace_root);
    }

    // Add current URI to visited set before processing
    visited.insert(parent_uri.clone());

    // Check depth limit
    if remaining_depth == 0 {
        log::trace!(
            "Depth limit reached when resolving parent WD for {}, falling back to parent's directory",
            parent_uri
        );
        // Fall back to parent's directory when depth is exhausted
        return fallback_parent_directory(parent_uri, workspace_root);
    }

    // Try to get parent's metadata
    if let Some(parent_meta) = get_metadata(parent_uri) {
        // Build parent's PathContext from metadata
        // This handles transitive inheritance: if parent has inherited_working_directory,
        // it will be used in effective_working_directory() (Requirement 9.1)
        if let Some(parent_ctx) =
            PathContext::from_metadata(parent_uri, &parent_meta, workspace_root)
        {
            // The implicit testthat/testit anchor is deliberately not
            // inheritable; explicit/inherited cd and the historical ordinary
            // file-directory fallback remain so.
            let inheritable_wd = parent_ctx.inheritable_working_directory()?;
            log::trace!(
                "Resolved parent working directory for {}: {} (depth remaining: {})",
                parent_uri,
                inheritable_wd.display(),
                remaining_depth
            );
            return Some(inheritable_wd.to_string_lossy().to_string());
        }
    }

    // Fallback: use parent's directory when metadata is unavailable
    // This handles the case where the parent file is not indexed or not accessible
    log::trace!(
        "Parent metadata unavailable for {}, falling back to parent's directory",
        parent_uri
    );

    fallback_parent_directory(parent_uri, workspace_root)
}

/// The parent-directory fallback shared by the cycle, depth-exhausted, and
/// metadata-unavailable branches above.
///
/// A parent under an implicit testthat/testit directory (issue #638) yields
/// `None` instead: the anchor is a soft tier that must never propagate into
/// sourced children, and the metadata-available branch already returns `None`
/// for such a parent (no explicit/inherited cd), so returning the raw file
/// directory here would hand the child a *hard* inherited cd — suppressing its
/// workspace-root fallback — purely because the parent was not indexed yet.
fn fallback_parent_directory(parent_uri: &Url, workspace_root: Option<&Url>) -> Option<String> {
    let parent_path = parent_uri.to_file_path().ok()?;
    let parent_dir = parent_path.parent()?;
    if let Some(root) = workspace_root.and_then(|u| u.to_file_path().ok())
        && crate::package_state::is_testthat_or_testit_test(&parent_path, &root)
    {
        log::trace!(
            "Parent {} is under an implicit test working directory; not inheriting its directory",
            parent_uri
        );
        return None;
    }
    Some(parent_dir.to_string_lossy().to_string())
}

/// Default maximum depth for working directory inheritance chains.
/// This prevents infinite loops in circular backward directive chains.
pub const DEFAULT_MAX_INHERITANCE_DEPTH: usize = 10;

/// Compute the inherited working directory for a file based on its backward directives.
///
/// Uses the first backward directive's parent to determine inheritance.
/// Returns None if no backward directives exist, if the child has an explicit working
/// directory, or if the parent URI cannot be resolved to a file path.
///
/// This is a convenience wrapper around `compute_inherited_working_directory_with_depth`
/// that uses the default maximum depth.
///
/// # Arguments
/// * `uri` - The URI of the child file
/// * `meta` - The child file's metadata
/// * `workspace_root` - Optional workspace root URI for resolving workspace-relative paths
/// * `get_metadata` - A closure that retrieves metadata for a given URI
///
/// # Returns
/// * `Some(String)` - The inherited working directory from the parent
/// * `None` - If inheritance should not occur (explicit WD, no backward directives, etc.)
///
/// # Behavior
/// - Skips inheritance if the child file has an explicit `# raven: cd` directive
/// - Uses the first backward directive (document order) to determine the parent
/// - Resolves the parent path using file-relative resolution (not affected by `# raven: cd`)
/// - Calls `resolve_parent_working_directory_with_visited` to get the parent's effective working directory
/// - Uses default max depth of 10 to prevent infinite chains
///
/// _Requirements: 1.1, 2.1, 7.1_
pub fn compute_inherited_working_directory<F>(
    uri: &Url,
    meta: &CrossFileMetadata,
    workspace_root: Option<&Url>,
    get_metadata: F,
) -> Option<String>
where
    F: Fn(&Url) -> Option<std::sync::Arc<CrossFileMetadata>>,
{
    compute_inherited_working_directory_with_depth(
        uri,
        meta,
        workspace_root,
        get_metadata,
        DEFAULT_MAX_INHERITANCE_DEPTH,
    )
}

/// Compute the inherited working directory for a file based on its backward directives,
/// with configurable depth tracking to prevent infinite chains.
///
/// Uses the first backward directive's parent to determine inheritance.
/// Returns None if no backward directives exist, if the child has an explicit working
/// directory, if parent metadata is unavailable, or if max_depth is exceeded.
///
/// # Arguments
/// * `uri` - The URI of the child file
/// * `meta` - The child file's metadata
/// * `workspace_root` - Optional workspace root URI for resolving workspace-relative paths
/// * `get_metadata` - A closure that retrieves metadata for a given URI
/// * `max_depth` - Maximum depth for inheritance chain traversal (prevents infinite loops)
///
/// # Returns
/// * `Some(String)` - The inherited working directory from the parent. When the parent's
///   own metadata is unavailable, this is the parent file's directory (the fallback in
///   `resolve_parent_working_directory_with_visited`), not `None`.
/// * `None` - If inheritance should not occur (explicit WD, no backward directives, max depth exceeded, cycle detected, or the parent path cannot be resolved)
///
/// # Behavior
/// - Skips inheritance if the child file has an explicit `# raven: cd` directive
/// - Uses the first backward directive (document order) to determine the parent
/// - Resolves the parent path using file-relative resolution (not affected by `# raven: cd`)
/// - Calls `resolve_parent_working_directory_with_visited` to get the parent's effective working directory
/// - Stops inheritance if max_depth is 0 (depth limit reached)
/// - Detects cycles and stops inheritance when a cycle is detected
///
/// # Transitive Inheritance
/// When computing B's inherited WD from A, if B's metadata already has an inherited_working_directory,
/// that value is used (which may have come from A). When computing C's inherited WD from B,
/// it gets B's effective WD (which includes A's WD if B inherited from A).
/// This naturally handles transitive inheritance through metadata propagation.
///
/// _Requirements: 1.1, 2.1, 7.1, 9.1, 9.2, 9.3_
pub fn compute_inherited_working_directory_with_depth<F>(
    uri: &Url,
    meta: &CrossFileMetadata,
    workspace_root: Option<&Url>,
    get_metadata: F,
    max_depth: usize,
) -> Option<String>
where
    F: Fn(&Url) -> Option<std::sync::Arc<CrossFileMetadata>>,
{
    let mut visited = HashSet::new();
    compute_inherited_working_directory_with_visited(
        uri,
        meta,
        workspace_root,
        &get_metadata,
        max_depth,
        &mut visited,
    )
}

/// Compute the inherited working directory for a file based on its backward directives,
/// with configurable depth tracking and cycle detection to prevent infinite chains.
///
/// Uses the first backward directive's parent to determine inheritance.
/// Returns None if no backward directives exist, if the child has an explicit working
/// directory, if max_depth is exceeded, if a cycle is detected, or if the parent path
/// cannot be resolved. Note: when the parent's own metadata is unavailable, this does NOT
/// return None — `resolve_parent_working_directory_with_visited` falls back to the parent
/// file's directory (returning `Some`).
///
/// # Arguments
/// * `uri` - The URI of the child file
/// * `meta` - The child file's metadata
/// * `workspace_root` - Optional workspace root URI for resolving workspace-relative paths
/// * `get_metadata` - A closure that retrieves metadata for a given URI
/// * `max_depth` - Maximum depth for inheritance chain traversal (prevents infinite loops)
/// * `visited` - Set of URIs already visited during this inheritance resolution (for cycle detection)
///
/// # Returns
/// * `Some(String)` - The inherited working directory from the parent. When the parent's
///   own metadata is unavailable, this is the parent file's directory (the fallback in
///   `resolve_parent_working_directory_with_visited`), not `None`.
/// * `None` - If inheritance should not occur (explicit WD, no backward directives, max depth exceeded, cycle detected, or the parent path cannot be resolved)
///
/// # Behavior
/// - Skips inheritance if the child file has an explicit `# raven: cd` directive
/// - Uses the first backward directive (document order) to determine the parent
/// - Resolves the parent path using file-relative resolution (not affected by `# raven: cd`)
/// - Calls `resolve_parent_working_directory_with_visited` to get the parent's effective working directory
/// - Stops inheritance if max_depth is 0 (depth limit reached)
/// - Detects cycles and stops inheritance when a cycle is detected (Requirement 9.3)
///
/// # Cycle Detection
/// When a URI is encountered that's already in the visited set, the function stops
/// inheritance and returns None. The caller should then use the file's own directory.
/// This prevents infinite loops in circular backward directive chains (e.g., A → B → A).
///
/// # Transitive Inheritance
/// When computing B's inherited WD from A, if B's metadata already has an inherited_working_directory,
/// that value is used (which may have come from A). When computing C's inherited WD from B,
/// it gets B's effective WD (which includes A's WD if B inherited from A).
/// This naturally handles transitive inheritance through metadata propagation.
///
/// _Requirements: 1.1, 2.1, 7.1, 9.1, 9.2, 9.3_
pub fn compute_inherited_working_directory_with_visited<F>(
    uri: &Url,
    meta: &CrossFileMetadata,
    workspace_root: Option<&Url>,
    get_metadata: &F,
    max_depth: usize,
    visited: &mut HashSet<Url>,
) -> Option<String>
where
    F: Fn(&Url) -> Option<std::sync::Arc<CrossFileMetadata>>,
{
    // Check for cycle: if we've already visited this URI, stop inheritance
    // (Requirement 9.3)
    if visited.contains(uri) {
        log::trace!(
            "Cycle detected when computing inherited WD for {}, stopping inheritance",
            uri
        );
        return None;
    }

    // Add current URI to visited set before processing
    visited.insert(uri.clone());

    // Check depth limit to prevent infinite chains (Requirement 9.2)
    if max_depth == 0 {
        log::trace!("Skipping WD inheritance for {}: max depth exceeded", uri);
        return None;
    }

    // Skip if file has explicit working directory (Requirement 3.1)
    if meta.working_directory.is_some() {
        log::trace!("Skipping WD inheritance for {}: has explicit @lsp-cd", uri);
        return None;
    }

    // Get first backward directive (document order) (Requirement 7.1, 7.2)
    let first_directive = meta.sourced_by.first()?;

    // Log when multiple backward directives exist (Requirement 7.3)
    if meta.sourced_by.len() > 1 {
        log::trace!(
            "File {} has {} backward directives; using first parent '{}' for WD inheritance",
            uri,
            meta.sourced_by.len(),
            first_directive.path
        );
    }

    log::trace!(
        "Computing inherited WD for {} from backward directive: {} (depth remaining: {})",
        uri,
        first_directive.path,
        max_depth
    );

    // Resolve parent URI using file-relative resolution only
    // IMPORTANT: Backward directive paths ignore both explicit `# raven: cd` and inherited
    // working directories - they always resolve relative to the file's directory
    // (Requirements 4.1, 4.2, 4.3)
    let backward_ctx = PathContext::new(uri, workspace_root)?;
    let parent_path = resolve_path(&first_directive.path, &backward_ctx)?;
    let parent_uri = path_to_uri(&parent_path)?;

    // Get parent's effective working directory with depth tracking and cycle detection
    let inherited_wd = resolve_parent_working_directory_with_visited(
        &parent_uri,
        get_metadata,
        workspace_root,
        max_depth,
        visited,
    );

    // If multiple backward directives resolve to different working directories, log which one we used.
    if meta.sourced_by.len() > 1
        && let Some(ref first_wd) = inherited_wd
    {
        let mut differing_parent: Option<(String, String)> = None;
        let backward_ctx = PathContext::new(uri, workspace_root);
        for directive in meta.sourced_by.iter().skip(1) {
            let ctx = match backward_ctx.as_ref() {
                Some(ctx) => ctx,
                None => break,
            };
            let other_parent_path = match resolve_path(&directive.path, ctx) {
                Some(path) => path,
                None => continue,
            };
            let other_parent_uri = match path_to_uri(&other_parent_path) {
                Some(uri) => uri,
                None => continue,
            };
            let mut other_visited = HashSet::new();
            let other_wd = resolve_parent_working_directory_with_visited(
                &other_parent_uri,
                get_metadata,
                workspace_root,
                max_depth,
                &mut other_visited,
            );
            if let Some(other_wd) = other_wd
                && &other_wd != first_wd
            {
                differing_parent = Some((directive.path.clone(), other_wd));
                break;
            }
        }

        if let Some((other_parent, other_wd)) = differing_parent {
            log::trace!(
                "Multiple backward directives for {} resolve to different working directories; using first parent '{}' with WD '{}', ignoring '{}' (WD '{}')",
                uri,
                first_directive.path,
                first_wd,
                other_parent,
                other_wd
            );
        }
    }

    inherited_wd
}

/// A dependency edge from parent (caller) to child (callee)
#[derive(Debug, Clone)]
pub struct DependencyEdge {
    /// Parent file (caller)
    pub from: Url,
    /// Child file (callee)
    pub to: Url,
    /// 0-based line number in parent where call occurs
    pub call_site_line: Option<u32>,
    /// 0-based UTF-16 column in parent where call occurs
    pub call_site_column: Option<u32>,
    /// Precise source destination class. Unlike the legacy metadata `local`
    /// boolean, this distinguishes a proven current frame from an unknown or
    /// external environment that must not inherit ordinary parent bindings.
    pub locality: SourceLocality,
    /// source(..., chdir=TRUE) semantics
    pub chdir: bool,
    /// True for sys.source(), false for source()
    pub is_sys_source: bool,
    /// Whether the `source()`/`sys.source()` call is lexically inside a function
    /// body. A function-scoped call does NOT contribute its child's symbols to
    /// the caller's top-level (EOF) scope, and a function-scoped `local = TRUE`
    /// call binds the child to a non-global frame (declared-only inheritance) —
    /// see the forward gate in `scope_at_position_with_graph_recursive` and the
    /// declared-only decision in `parent_prefix_at`. Carried on the edge and
    /// folded into full-edge equality so a fixed-call-site scope flip bumps
    /// `edge_revision` as well as the source-aware interface hash. Always `false`
    /// for `# raven` directives.
    pub is_function_scoped: bool,
    /// True if the edge was created from any Raven directive — forward-family
    /// (`# raven: source`/`run`/`include`) or backward-family
    /// (`# raven: sourced-by`/`run-by`/`included-by`) — rather than an
    /// AST-detected `source()` call.
    pub is_directive: bool,
    /// True if the edge originates from a backward-family directive
    /// (e.g. `# raven: sourced-by`, `# raven: run-by`, `# raven: included-by`).
    /// False for forward-family directives (e.g. `# raven: source`, `# raven: run`,
    /// `# raven: include`) and AST-detected edges.
    pub is_backward_directive: bool,
    /// True when the parent may consume the child's exports, and the edge
    /// remains visible to full-graph revalidation, but the child must not
    /// inherit symbols or packages from the parent. Used for project-excluded
    /// open buffers: an excluded `E` that sources helper `H` still needs edits
    /// to `H` to reschedule `E`, but `H` must not borrow `E`'s scope.
    ///
    /// Deliberately excluded from `PartialEq`, `Eq`, `Hash`, and `Self::key`:
    /// it is a lending policy marker on an otherwise identical edge, not edge
    /// identity. This keeps `update_file`'s before/after edge snapshots from
    /// reporting a spurious edge change when excluded-root edges are rebuilt
    /// unmarked and then re-marked non-lending.
    pub non_lending: bool,
}

/// Caller/callee relationship used only to find directive-vs-AST conflicts.
///
/// This deliberately ignores call sites and source semantics: finding a
/// relationship merely enters conflict resolution, which then decides whether
/// distinct invocations must both survive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DirectiveConflictIdentity {
    from: Url,
    to: Url,
}

/// Position component of a source invocation identity.
///
/// The paired options preserve the existing three states: an exact position,
/// an explicit end-of-line position (`column == u32::MAX`), or an unknown
/// position. Keeping the interpretation here prevents endpoint or revision
/// projections from reimplementing call-site comparisons ad hoc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SourceCallSiteIdentity {
    line: Option<u32>,
    column: Option<u32>,
}

impl SourceCallSiteIdentity {
    fn is_known_directive_call_site(self) -> bool {
        self.line.is_some() && self.line != Some(u32::MAX)
    }
}

/// Source behavior that can change how the child inherits or lends scope.
///
/// This is intentionally distinct from invocation position and directive
/// provenance. Every field here affects source semantics and therefore belongs
/// in graph deduplication, dependency-interface comparison, and revision
/// invalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SourceInheritanceIdentity {
    locality: SourceLocality,
    chdir: bool,
    is_sys_source: bool,
    is_function_scoped: bool,
}

/// Semantic identity of one source invocation.
///
/// Multiple calls from the same parent to the same child remain distinct by
/// call site. Backward directives remain distinct from forward invocations
/// because their eligibility and removal rules differ.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SourceInvocationIdentity {
    relation: DirectiveConflictIdentity,
    call_site: SourceCallSiteIdentity,
    inheritance: SourceInheritanceIdentity,
    is_backward_directive: bool,
}

/// Purpose-specific key for deduplicating copies inserted into the graph.
///
/// Directive provenance is deliberately absent: after conflict resolution, an
/// AST call and directive describing the same semantic invocation collapse.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GraphEdgeDedupKey(SourceInvocationIdentity);

impl GraphEdgeDedupKey {
    fn target(&self) -> &Url {
        &self.0.relation.to
    }
}

/// Edge projection whose changes require dependent revalidation.
///
/// Unlike graph deduplication, directive provenance is interface-visible for
/// diagnostics and revalidation. Lending policy is excluded because it changes
/// snapshot construction without adding or removing a revalidation edge.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DependencyInterfaceEdgeIdentity {
    invocation: SourceInvocationIdentity,
    is_directive: bool,
}

/// Complete edge projection whose changes invalidate revision-gated caches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EdgeRevisionIdentity {
    interface: DependencyInterfaceEdgeIdentity,
    non_lending: bool,
}

impl PartialEq for DependencyEdge {
    fn eq(&self, other: &Self) -> bool {
        self.dependency_interface_identity() == other.dependency_interface_identity()
    }
}

impl Eq for DependencyEdge {}

impl Hash for DependencyEdge {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.dependency_interface_identity().hash(state);
    }
}

impl DependencyEdge {
    /// Whether the child may inherit only declarations, not ordinary bindings,
    /// from this parent edge.
    pub(crate) fn uses_declared_only_parent_inheritance(&self) -> bool {
        matches!(self.locality, SourceLocality::NonInheriting)
    }

    fn directive_conflict_identity(&self) -> DirectiveConflictIdentity {
        DirectiveConflictIdentity {
            from: self.from.clone(),
            to: self.to.clone(),
        }
    }

    fn call_site_identity(&self) -> SourceCallSiteIdentity {
        SourceCallSiteIdentity {
            line: self.call_site_line,
            column: self.call_site_column,
        }
    }

    fn source_invocation_identity(&self) -> SourceInvocationIdentity {
        SourceInvocationIdentity {
            relation: self.directive_conflict_identity(),
            call_site: self.call_site_identity(),
            inheritance: SourceInheritanceIdentity {
                locality: self.locality,
                chdir: self.chdir,
                is_sys_source: self.is_sys_source,
                is_function_scoped: self.is_function_scoped,
            },
            is_backward_directive: self.is_backward_directive,
        }
    }

    fn graph_dedup_key(&self) -> GraphEdgeDedupKey {
        GraphEdgeDedupKey(self.source_invocation_identity())
    }

    fn dependency_interface_identity(&self) -> DependencyInterfaceEdgeIdentity {
        DependencyInterfaceEdgeIdentity {
            invocation: self.source_invocation_identity(),
            is_directive: self.is_directive,
        }
    }

    fn revision_identity(&self) -> EdgeRevisionIdentity {
        EdgeRevisionIdentity {
            interface: self.dependency_interface_identity(),
            non_lending: self.non_lending,
        }
    }
}

/// Result of cycle detection, containing both edges needed for diagnostics.
#[derive(Debug, Clone)]
pub struct CycleDetection {
    /// First outgoing edge FROM the queried URI — use for diagnostic position
    pub outgoing_edge: DependencyEdge,
    /// Edge that closes the cycle BACK to the queried URI — use for message details
    pub closing_edge: DependencyEdge,
}

/// Result of updating a file in the dependency graph
#[derive(Debug, Default)]
pub struct UpdateResult {
    /// Diagnostics to emit (e.g., directive-vs-AST conflict warnings)
    pub diagnostics: Vec<Diagnostic>,
    /// True if forward edges from this file changed (added/removed targets).
    /// Used to trigger revalidation of dependents even when interface hash
    /// doesn't change (e.g., commenting out a source() call breaks a cycle).
    pub edges_changed: bool,
}

/// Which bound, if any, prevented a neighborhood query from visiting every
/// reachable node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NeighborhoodTruncation {
    /// At least one edge led beyond `max_depth`.
    pub depth: bool,
    /// At least one otherwise-unvisited neighbor was omitted at `max_visited`.
    pub visited: bool,
}

impl NeighborhoodTruncation {
    /// Whether either traversal bound truncated the neighborhood.
    pub fn is_truncated(self) -> bool {
        self.depth || self.visited
    }
}

/// Cached `(neighborhood, subgraph, truncation)` payload for a
/// `(root, depth, visited)` query. Wrapped in `Arc` so cache reads are refcount
/// bumps; the inner `subgraph` is also held as `Arc` so consumers (e.g.
/// `DiagnosticsSnapshot`) can keep a refcount-bumped reference instead of
/// cloning the trimmed graph per snapshot.
pub struct NeighborhoodSubgraph {
    pub neighborhood: HashSet<Url>,
    pub subgraph: std::sync::Arc<DependencyGraph>,
    /// Authoritative status from the bounded walk that produced `neighborhood`.
    pub truncation: NeighborhoodTruncation,
}

/// Cap for the per-`DependencyGraph` cycle/subgraph caches.
///
/// Open documents are non-evictable; these derived caches stay independently
/// bounded because misses only require recomputation, not loss of authority.
const CYCLE_CACHE_CAPACITY: usize = 4096;
const SUBGRAPH_CACHE_CAPACITY: usize = 4096;

/// LRU cache of extracted subgraphs keyed by `(root_uri, max_depth, max_visited)`.
type SubgraphCache = std::sync::RwLock<
    lru::LruCache<(Url, usize, usize), (u64, std::sync::Arc<NeighborhoodSubgraph>)>,
>;

/// Dependency graph tracking source relationships between files.
///
/// # Raw URI identity
///
/// Graph nodes are raw file `Url` values, matching the rest of the LSP state.
/// `update_file` stores the caller's `uri` verbatim as the parent node and
/// stores child nodes with `path_to_uri` from the resolved path; neither side is
/// normalized through `std::fs::canonicalize`.
///
/// This is intentional. Symlink aliases and alternate case spellings can refer
/// to the same file on disk, but they remain distinct graph identities unless
/// the path-resolution layer itself rewrites a source-path suffix to the real
/// directory-entry case. Full filesystem canonicalization was rejected because
/// it follows symlinks, which can produce prefixes that no longer match the
/// uncanonicalized workspace-index keys. The graph therefore preserves Raven's
/// supported LSP identity model: graph reachability is a raw-URI property, not
/// an underlying-inode property.
///
/// Open-document aliasing lives outside the graph in
/// [`crate::state::WorldState`]: when a client opens a case or symlink alias of
/// a graph URI, the alias layer makes that open buffer authoritative for
/// revalidation, content, and watched-file vetoes without rewriting graph keys
/// or changing diagnostics publish URIs.
pub struct DependencyGraph {
    /// Forward lookup: parent URI -> edges to children
    forward: HashMap<Url, Vec<DependencyEdge>>,
    /// Reverse lookup: child URI -> edges from parents
    backward: HashMap<Url, Vec<DependencyEdge>>,
    /// Monotonic counter bumped whenever `update_file` reports
    /// `edges_changed`. Cached `detect_cycle` results are keyed on this so
    /// they are invalidated as soon as forward edges change.
    edge_revision: std::sync::atomic::AtomicU64,
    /// Cache of `detect_cycle` results keyed by `uri` (and gated by
    /// `edge_revision` per slot). Bounded LRU so long-lived sessions
    /// don't accumulate entries for files that never get queried again.
    cycle_cache: std::sync::RwLock<lru::LruCache<Url, (u64, Option<CycleDetection>)>>,
    /// Counter of cache hits — exposed for tests; not used in production.
    cycle_cache_hits: std::sync::atomic::AtomicU64,
    /// Cache of `(neighborhood, extract_subgraph)` results keyed by
    /// `(root_uri, max_depth, max_visited)`. Stored as `Arc` so cache reads
    /// in the snapshot path do refcount bumps rather than re-walking the
    /// graph and cloning edges. Bounded LRU for the same reason as
    /// `cycle_cache`.
    subgraph_cache: SubgraphCache,
    /// Counter of subgraph cache hits — exposed for tests.
    subgraph_cache_hits: std::sync::atomic::AtomicU64,
    /// Counter of traversals truncated by the max-visited budget.
    visited_budget_truncations: std::sync::atomic::AtomicU64,
    /// Counter of traversals truncated by max-depth limits.
    depth_truncations: std::sync::atomic::AtomicU64,
    /// Memoized "does this graph contain any source cycle" answer, tagged with
    /// the `edge_revision` it was computed at. Consumed by the forward-child
    /// scope memo (issue #472), which must disable itself on cyclic graphs
    /// because a child's resolved scope is only a pure function of its key when
    /// no cycle is reachable from it. Recomputed on edge-revision change.
    any_cycle_cache: std::sync::RwLock<Option<(u64, bool)>>,
}

impl Default for DependencyGraph {
    fn default() -> Self {
        Self {
            forward: HashMap::new(),
            backward: HashMap::new(),
            edge_revision: std::sync::atomic::AtomicU64::new(0),
            cycle_cache: std::sync::RwLock::new(lru::LruCache::new(super::cache::non_zero_or(
                CYCLE_CACHE_CAPACITY,
                CYCLE_CACHE_CAPACITY,
            ))),
            cycle_cache_hits: std::sync::atomic::AtomicU64::new(0),
            subgraph_cache: std::sync::RwLock::new(lru::LruCache::new(super::cache::non_zero_or(
                SUBGRAPH_CACHE_CAPACITY,
                SUBGRAPH_CACHE_CAPACITY,
            ))),
            subgraph_cache_hits: std::sync::atomic::AtomicU64::new(0),
            visited_budget_truncations: std::sync::atomic::AtomicU64::new(0),
            depth_truncations: std::sync::atomic::AtomicU64::new(0),
            any_cycle_cache: std::sync::RwLock::new(None),
        }
    }
}

impl Clone for DependencyGraph {
    fn clone(&self) -> Self {
        // Caches are intentionally NOT cloned: clones (e.g. via
        // `extract_subgraph`) typically have different edges, so any cached
        // results from the parent graph are not portable. The new graph
        // starts at edge revision 0 with empty caches.
        Self {
            forward: self.forward.clone(),
            backward: self.backward.clone(),
            edge_revision: std::sync::atomic::AtomicU64::new(0),
            cycle_cache: std::sync::RwLock::new(lru::LruCache::new(super::cache::non_zero_or(
                CYCLE_CACHE_CAPACITY,
                CYCLE_CACHE_CAPACITY,
            ))),
            cycle_cache_hits: std::sync::atomic::AtomicU64::new(0),
            subgraph_cache: std::sync::RwLock::new(lru::LruCache::new(super::cache::non_zero_or(
                SUBGRAPH_CACHE_CAPACITY,
                SUBGRAPH_CACHE_CAPACITY,
            ))),
            subgraph_cache_hits: std::sync::atomic::AtomicU64::new(0),
            visited_budget_truncations: std::sync::atomic::AtomicU64::new(0),
            depth_truncations: std::sync::atomic::AtomicU64::new(0),
            any_cycle_cache: std::sync::RwLock::new(None),
        }
    }
}

impl std::fmt::Debug for DependencyGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DependencyGraph")
            .field("forward", &self.forward)
            .field("backward", &self.backward)
            .field(
                "edge_revision",
                &self
                    .edge_revision
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
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
    ///
    /// **Note on Working Directory Inheritance**: If the file has backward directives and should
    /// inherit a working directory from its parent, the caller should compute this inheritance
    /// using `compute_inherited_working_directory()` and set `meta.inherited_working_directory`
    /// BEFORE calling this method. The `PathContext::from_metadata()` will then use the inherited
    /// working directory when resolving forward source paths.
    ///
    /// _Requirements: 5.1, 5.2_
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

        // Build PathContext for forward sources (includes working_directory from `# raven: cd`)
        // IMPORTANT: Forward directives (`# raven: source`, `# raven: run`, `# raven: include`)
        // and AST-detected source() calls should use the working directory from `# raven: cd`
        // for path resolution.
        // This is because forward directives are semantically equivalent to source() calls
        // and describe runtime execution behavior where the working directory matters.
        // Using PathContext::from_metadata() includes both explicit `# raven: cd` and inherited
        // working directories in the path resolution context.
        // (Requirements 3.1, 3.2, 3.4)
        let path_ctx = match PathContext::from_metadata(uri, meta, workspace_root) {
            Some(ctx) => ctx,
            None => return result,
        };

        // Build separate PathContext for backward directives (without any working_directory)
        // IMPORTANT: Backward directive paths (e.g., `# raven: sourced-by ../parent.R`) should
        // ALWAYS resolve relative to the child file's directory, regardless of:
        //   - Explicit `# raven: cd` directives in the child file
        //   - Inherited working directory from parent files
        // This is intentional behavior per Requirements 4.1, 4.2, 4.3.
        // Using PathContext::new() ensures neither working_directory nor
        // inherited_working_directory are set, so paths resolve file-relative.
        let backward_path_ctx = match PathContext::new(uri, workspace_root) {
            Some(ctx) => ctx,
            None => return result,
        };

        // Helper to resolve paths for forward sources (`# raven: source` directives and source() calls)
        // Uses PathContext with working_directory from `# raven: cd`, enabling paths to resolve
        // relative to the configured working directory rather than the file's directory.
        // Also uses workspace-root fallback for AST source() calls AND forward directives
        // in files without `# raven: cd` — forward directives are semantically equivalent to
        // source() calls (see .kiro/specs/lsp-source-directive/) and must resolve identically.
        // Returns Option<Url> - existence is checked later during file read operations.
        let do_resolve = |path: &str| -> Option<Url> {
            let resolved = resolve_path_with_workspace_fallback(path, &path_ctx)?;
            path_to_uri(&resolved)
        };

        // Helper to resolve paths for backward directives (file-relative only)
        // Does NOT use working_directory or inherited_working_directory
        let do_resolve_backward = |path: &str| -> Option<Url> {
            let resolved = resolve_path(path, &backward_path_ctx)?;
            path_to_uri(&resolved)
        };

        // Snapshot both purpose-specific edge projections. Dependency-interface
        // identity drives dependent revalidation; revision identity additionally
        // includes lending policy for cache invalidation. Both include call-site
        // and source semantics, so metadata-only changes still invalidate the
        // appropriate consumers when the target URI set is unchanged.
        let old_forward_interface: HashSet<DependencyInterfaceEdgeIdentity> = self
            .forward
            .get(uri)
            .map(|edges| {
                edges
                    .iter()
                    .map(DependencyEdge::dependency_interface_identity)
                    .collect()
            })
            .unwrap_or_default();
        let old_forward_revision: HashSet<EdgeRevisionIdentity> = self
            .forward
            .get(uri)
            .map(|edges| {
                edges
                    .iter()
                    .map(DependencyEdge::revision_identity)
                    .collect()
            })
            .unwrap_or_default();
        // Snapshot backward edges (incoming `is_backward_directive` edges)
        // before removal: a `# raven: sourced-by` directive change rewires the
        // backward map for `uri` and the forward map for each parent, but
        // leaves `forward[uri]` (this file's outgoing edges) untouched.
        // The same interface/revision projections apply here.
        let old_backward_interface: HashSet<DependencyInterfaceEdgeIdentity> = self
            .backward
            .get(uri)
            .map(|edges| {
                edges
                    .iter()
                    .filter(|e| e.is_backward_directive)
                    .map(DependencyEdge::dependency_interface_identity)
                    .collect()
            })
            .unwrap_or_default();
        let old_backward_revision: HashSet<EdgeRevisionIdentity> = self
            .backward
            .get(uri)
            .map(|edges| {
                edges
                    .iter()
                    .filter(|e| e.is_backward_directive)
                    .map(DependencyEdge::revision_identity)
                    .collect()
            })
            .unwrap_or_default();

        // Remove existing edges where this file is the parent
        // BUT: only remove edges that were created by THIS file's forward sources/directives
        // Do NOT remove edges created by backward directives in other files
        self.remove_forward_edges_from_this_file(uri);

        // Also remove edges where this file is the child (from backward directives)
        // These will be re-created from the current metadata
        self.remove_backward_edges_for_child(uri);

        // Collect directive edges first (they are authoritative)
        let mut directive_edges: Vec<DependencyEdge> = Vec::new();
        // Relationship-only index for directive-vs-AST conflict detection.
        // Edge storage is deliberately call-site-aware and is deduplicated by
        // `GraphEdgeDedupKey` after conflict resolution.
        let mut directive_from_to: HashSet<DirectiveConflictIdentity> = HashSet::new();

        // Process forward directive sources (`# raven: source`, `# raven: run`, `# raven: include`)
        // Uses do_resolve which includes `# raven: cd` working directory in path resolution.
        // This differs from backward directives which ignore `# raven: cd`.
        // Creates edges optimistically; file existence is validated during file operations.
        // (Requirements 3.1, 3.2, 3.4)
        for source in &meta.sources {
            if source.is_directive {
                match do_resolve(&source.path) {
                    Some(to_uri) => {
                        // Path resolved, create edge
                        let edge = DependencyEdge {
                            from: uri.clone(),
                            to: to_uri.clone(),
                            call_site_line: Some(source.line),
                            call_site_column: Some(source.column),
                            locality: source.locality,
                            chdir: source.chdir,
                            is_sys_source: source.is_sys_source,
                            is_function_scoped: source.is_function_scoped,
                            is_directive: true,
                            is_backward_directive: false,
                            non_lending: false,
                        };
                        directive_from_to.insert(edge.directive_conflict_identity());
                        directive_edges.push(edge);
                    }
                    None => {
                        // Path resolution failed - skip edge creation. The
                        // user-facing missing/unresolved diagnostics are
                        // recomputed from the snapshot graph by a separate
                        // collector path, not from this result.
                        log::trace!(
                            "Forward directive @lsp-source '{}' at line {} could not be resolved, skipping edge creation",
                            source.path,
                            source.line
                        );
                    }
                }
            }
        }

        // Process backward directives (`# raven: sourced-by`) - create forward edges from parent to this file
        // Uses do_resolve_backward which resolves paths relative to the file's directory,
        // ignoring both explicit `# raven: cd` and inherited working directories (Requirements 4.1-4.3)
        for directive in &meta.sourced_by {
            if let Some(parent_uri) = do_resolve_backward(&directive.path) {
                // Extract child filename for inference
                let child_filename = uri
                    .path_segments()
                    .and_then(|mut s| s.next_back())
                    .unwrap_or("");

                let (call_site_line, call_site_column) = match &directive.call_site {
                    CallSiteSpec::Line(n) => (Some(*n), Some(u32::MAX)), // end-of-line
                    CallSiteSpec::Match(pattern) => {
                        // Resolve match pattern in parent content
                        if let Some(parent_content) = get_content(&parent_uri) {
                            if let Some((line, col)) =
                                resolve_match_pattern(&parent_content, pattern, child_filename)
                            {
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
                            if let Some((line, col)) =
                                infer_call_site_from_parent(&parent_content, child_filename)
                            {
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
                    locality: SourceLocality::Global,
                    chdir: false,
                    is_sys_source: false,
                    is_function_scoped: false,
                    is_directive: true,
                    is_backward_directive: true,
                    non_lending: false,
                };
                let pair = edge.directive_conflict_identity();
                directive_from_to.insert(pair);
                directive_edges.push(edge);
            }
        }

        // Process AST-detected sources, applying directive-vs-AST conflict resolution
        // Requirements 4.1, 4.2, 4.3, 4.4, 4.5
        let mut ast_edges: Vec<DependencyEdge> = Vec::new();
        for source in &meta.sources {
            if !source.is_directive
                && let Some(to_uri) = source
                    .resolved_uri
                    .clone()
                    .or_else(|| do_resolve(&source.path))
            {
                let edge = DependencyEdge {
                    from: uri.clone(),
                    to: to_uri.clone(),
                    call_site_line: Some(source.line),
                    call_site_column: Some(source.column),
                    locality: source.locality,
                    chdir: source.chdir,
                    is_sys_source: source.is_sys_source,
                    is_function_scoped: source.is_function_scoped,
                    is_directive: false,
                    is_backward_directive: false,
                    non_lending: false,
                };
                let pair = edge.directive_conflict_identity();

                // Check for directive-vs-AST conflict
                if directive_from_to.contains(&pair) {
                    // Find the directive edge for this (from, to) pair
                    let directive_edge = directive_edges
                        .iter()
                        .find(|e| e.directive_conflict_identity() == pair);

                    if let Some(dir_edge) = directive_edge {
                        // Check if directive has a known call site
                        let directive_call_site = dir_edge.call_site_identity();

                        if directive_call_site.is_known_directive_call_site() {
                            // Directive has known call site: only override AST edge at same call site
                            // (Requirement 4.3)
                            if directive_call_site == edge.call_site_identity() {
                                // Case 1: Same call site - directive wins, skip AST edge
                                // (Requirement 4.3)
                                continue;
                            } else {
                                // Case 2: Different call sites - keep both edges
                                // (Requirement 4.4)
                                ast_edges.push(edge);
                                continue;
                            }
                        } else {
                            // Directive has no explicit call site
                            // (Requirement 4.5)

                            // Get the directive's line (where it appears in the file)
                            let directive_line = meta
                                .sources
                                .iter()
                                .find(|s| {
                                    s.is_directive && do_resolve(&s.path) == Some(to_uri.clone())
                                })
                                .map(|s| s.line);

                            // Check if AST edge is at an earlier line than the directive
                            let ast_is_earlier = match directive_line {
                                Some(dir_line) => source.line < dir_line,
                                None => false, // If we can't determine directive line, don't treat AST as earlier
                            };

                            if ast_is_earlier {
                                // Case 3: Directive without line=, AST at earlier line.
                                // Keep AST edge (earliest call site wins). The
                                // redundant-directive diagnostic is recomputed from
                                // the snapshot graph by
                                // `collect_redundant_directive_diagnostics_from_snapshot`,
                                // not stored on this result. (Requirement 4.5, 6.2)
                                ast_edges.push(edge);
                                continue;
                            } else {
                                // AST is at same or later line than directive
                                // Directive wins (it's earlier or at same position)
                                // Skip AST edge
                                continue;
                            }
                        }
                    }
                    // No matching directive edge found (shouldn't happen), skip
                    continue;
                }

                ast_edges.push(edge);
            }
        }

        // Deduplicate and add all edges
        let mut seen_keys = HashSet::new();
        for edge in directive_edges.into_iter().chain(ast_edges) {
            if seen_keys.insert(edge.graph_dedup_key()) {
                self.add_edge(edge);
            }
        }

        // Detect whether forward OR backward edges changed for this file.
        // Backward changes (added/removed `# raven: sourced-by`) don't touch
        // `forward[uri]`, but they DO change the dependency graph that
        // `collect_neighborhood` and `detect_cycle` traverse — so the caches
        // keyed on `edge_revision` must be invalidated for those too. We
        // compare dependency-interface identities (not just URIs), so a
        // metadata-only change (e.g. moved call site) also requests dependent
        // revalidation and refreshes diagnostic positioning.
        let new_forward_interface: HashSet<DependencyInterfaceEdgeIdentity> = self
            .forward
            .get(uri)
            .map(|edges| {
                edges
                    .iter()
                    .map(DependencyEdge::dependency_interface_identity)
                    .collect()
            })
            .unwrap_or_default();
        let new_forward_revision: HashSet<EdgeRevisionIdentity> = self
            .forward
            .get(uri)
            .map(|edges| {
                edges
                    .iter()
                    .map(DependencyEdge::revision_identity)
                    .collect()
            })
            .unwrap_or_default();
        let new_backward_interface: HashSet<DependencyInterfaceEdgeIdentity> = self
            .backward
            .get(uri)
            .map(|edges| {
                edges
                    .iter()
                    .filter(|e| e.is_backward_directive)
                    .map(DependencyEdge::dependency_interface_identity)
                    .collect()
            })
            .unwrap_or_default();
        let new_backward_revision: HashSet<EdgeRevisionIdentity> = self
            .backward
            .get(uri)
            .map(|edges| {
                edges
                    .iter()
                    .filter(|e| e.is_backward_directive)
                    .map(DependencyEdge::revision_identity)
                    .collect()
            })
            .unwrap_or_default();
        result.edges_changed = old_forward_interface != new_forward_interface
            || old_backward_interface != new_backward_interface;
        let revision_identity_changed = old_forward_revision != new_forward_revision
            || old_backward_revision != new_backward_revision;

        // Bump edge_revision so cycle/subgraph caches become stale for every
        // URI. detect_cycle and cached_neighborhood_subgraph then either
        // re-fill their slot or evict via the revision-mismatch check. A
        // non_lending flip invalidates those caches too, but deliberately does
        // not set `edges_changed`: lending policy is not a revalidation edge.
        if result.edges_changed || revision_identity_changed {
            self.edge_revision
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }

        // Log total edge count after update
        let total_edges: usize = self.forward.values().map(|v| v.len()).sum();
        log::trace!(
            "Dependency graph now has {} total edges after updating {}",
            total_edges,
            uri
        );

        result
    }

    /// Extract a subgraph containing only edges involving URIs in the given set.
    ///
    /// This is much cheaper than cloning the entire graph when only a
    /// neighborhood of files is needed (e.g., for diagnostic snapshots).
    ///
    /// `non_lending` edges are retained in the forward index but omitted from
    /// the backward index. That gives an excluded open buffer's own diagnostic
    /// snapshot the helper exports it consumes, while preventing that excluded
    /// buffer from becoming an ancestor whose symbols, packages, or NSE/func
    /// declarations propagate into the helper's trimmed snapshot. The full
    /// live graph keeps the backward copy for revalidation, so this is another
    /// deliberate source of the safe `S_trimmed ⊆ S_full` asymmetry documented
    /// on [`Self::revalidation_consistent_set`].
    pub fn extract_subgraph(&self, uris: &HashSet<Url>) -> Self {
        let mut forward = HashMap::new();
        let mut backward = HashMap::new();

        for uri in uris {
            if let Some(edges) = self.forward.get(uri) {
                let filtered: Vec<_> = edges
                    .iter()
                    .filter(|e| uris.contains(&e.to))
                    .cloned()
                    .collect();
                if !filtered.is_empty() {
                    forward.insert(uri.clone(), filtered);
                }
            }
            if let Some(edges) = self.backward.get(uri) {
                let filtered: Vec<_> = edges
                    .iter()
                    .filter(|e| uris.contains(&e.from) && !e.non_lending)
                    .cloned()
                    .collect();
                if !filtered.is_empty() {
                    backward.insert(uri.clone(), filtered);
                }
            }
        }

        // Subgraphs start with fresh caches; their edges are pruned, so
        // any cached results from the parent graph would be wrong.
        Self {
            forward,
            backward,
            edge_revision: std::sync::atomic::AtomicU64::new(0),
            cycle_cache: std::sync::RwLock::new(lru::LruCache::new(super::cache::non_zero_or(
                CYCLE_CACHE_CAPACITY,
                CYCLE_CACHE_CAPACITY,
            ))),
            cycle_cache_hits: std::sync::atomic::AtomicU64::new(0),
            subgraph_cache: std::sync::RwLock::new(lru::LruCache::new(super::cache::non_zero_or(
                SUBGRAPH_CACHE_CAPACITY,
                SUBGRAPH_CACHE_CAPACITY,
            ))),
            subgraph_cache_hits: std::sync::atomic::AtomicU64::new(0),
            visited_budget_truncations: std::sync::atomic::AtomicU64::new(0),
            depth_truncations: std::sync::atomic::AtomicU64::new(0),
            any_cycle_cache: std::sync::RwLock::new(None),
        }
    }

    /// Remove edges where the given URI is the child that were created from backward directives
    fn remove_backward_edges_for_child(&mut self, child_uri: &Url) {
        // Get edges where this file is the child
        let edges_to_remove: Vec<DependencyEdge> = self
            .backward
            .get(child_uri)
            .map(|edges| {
                edges
                    .iter()
                    .filter(|e| e.is_backward_directive && &e.to == child_uri)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        if !edges_to_remove.is_empty() {
            log::trace!(
                "Removing {} backward directive edges for child {}",
                edges_to_remove.len(),
                child_uri
            );
        }

        // Remove from both forward and backward indices
        for edge in edges_to_remove {
            log::trace!(
                "  Removing backward directive edge: {} -> {}",
                edge.from,
                edge.to
            );
            let dedup_key = edge.graph_dedup_key();
            // Remove the same semantic invocation from the forward index.
            if let Some(forward_edges) = self.forward.get_mut(&edge.from) {
                forward_edges.retain(|candidate| candidate.graph_dedup_key() != dedup_key);
                if forward_edges.is_empty() {
                    self.forward.remove(&edge.from);
                }
            }
            // Remove the same semantic invocation from the backward index.
            if let Some(backward_edges) = self.backward.get_mut(child_uri) {
                backward_edges.retain(|candidate| candidate.graph_dedup_key() != dedup_key);
                if backward_edges.is_empty() {
                    self.backward.remove(child_uri);
                }
            }
        }
    }

    /// Remove all edges involving a file. Bumps `edge_revision` so any
    /// `cycle_cache` / `subgraph_cache` entries that referenced the deleted
    /// file's edges are invalidated on the next lookup.
    pub fn remove_file(&mut self, uri: &Url) {
        let had_forward = self.forward.contains_key(uri);
        let had_backward = self.backward.contains_key(uri);
        // Remove edges where this file is the parent
        self.remove_forward_edges(uri);
        // Remove edges where this file is the child
        self.remove_backward_edges(uri);
        if had_forward || had_backward {
            self.edge_revision
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
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

    fn record_visited_budget_truncation(&self) {
        self.visited_budget_truncations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn record_depth_truncation(&self) {
        self.depth_truncations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn has_unvisited_dependents(&self, uri: &Url, visited: &HashMap<Url, usize>) -> bool {
        self.backward
            .get(uri)
            .is_some_and(|edges| edges.iter().any(|edge| !visited.contains_key(&edge.from)))
    }

    fn has_unvisited_dependencies(&self, uri: &Url, visited: &HashMap<Url, usize>) -> bool {
        self.forward
            .get(uri)
            .is_some_and(|edges| edges.iter().any(|edge| !visited.contains_key(&edge.to)))
    }

    fn has_unvisited_neighbors(&self, uri: &Url, visited: &HashSet<Url>) -> bool {
        self.forward
            .get(uri)
            .is_some_and(|edges| edges.iter().any(|edge| !visited.contains(&edge.to)))
            || self
                .backward
                .get(uri)
                .is_some_and(|edges| edges.iter().any(|edge| !visited.contains(&edge.from)))
    }

    /// Get all transitive dependents (files that depend on uri directly or indirectly).
    ///
    /// `max_visited` caps the total number of nodes explored during DFS to prevent
    /// unbounded traversal in dense graphs (e.g., auto backward dependency mode).
    pub fn get_transitive_dependents(
        &self,
        uri: &Url,
        max_depth: usize,
        max_visited: usize,
    ) -> Vec<Url> {
        let mut result = Vec::new();
        let mut visited: HashMap<Url, usize> = HashMap::new();
        self.collect_dependents(uri, max_depth, 0, &mut visited, &mut result, max_visited);
        result
    }

    /// `visited` tracks the *shallowest* depth at which each URI has been
    /// reached. Same depth-shortest invariant as `collect_dependencies`:
    /// a revisit at a strictly shallower depth must continue recursing so
    /// ancestors reachable within the budget through the shorter path are
    /// not silently dropped.
    fn collect_dependents(
        &self,
        uri: &Url,
        max_depth: usize,
        current_depth: usize,
        visited: &mut HashMap<Url, usize>,
        result: &mut Vec<Url>,
        max_visited: usize,
    ) {
        if current_depth > max_depth {
            self.record_depth_truncation();
            return;
        }
        let is_first_visit = match visited.get(uri) {
            Some(&prev_depth) if prev_depth <= current_depth => return,
            Some(_) => false,
            None => {
                if visited.len() >= max_visited {
                    self.record_visited_budget_truncation();
                    return;
                }
                true
            }
        };
        visited.insert(uri.clone(), current_depth);
        if is_first_visit && current_depth > 0 {
            result.push(uri.clone());
        }
        if current_depth == max_depth {
            if self.has_unvisited_dependents(uri, visited) {
                self.record_depth_truncation();
            }
            return;
        }

        for edge in self.get_dependents(uri) {
            if visited.len() >= max_visited && !visited.contains_key(&edge.from) {
                self.record_visited_budget_truncation();
                break;
            }
            self.collect_dependents(
                &edge.from,
                max_depth,
                current_depth + 1,
                visited,
                result,
                max_visited,
            );
        }
    }

    /// Get all transitive dependencies — files that `uri` sources directly or
    /// transitively (children, grandchildren, etc.).
    ///
    /// Mirror of [`Self::get_transitive_dependents`] but in the *forward*
    /// direction. Used by the cross-file plumbing in `did_change` so that an
    /// edit to a parent file revalidates the entire forward subtree: child
    /// scope inherits from the parent at the `source()` call site, so a
    /// content change in the parent can change every descendant's diagnostics.
    pub fn get_transitive_dependencies(
        &self,
        uri: &Url,
        max_depth: usize,
        max_visited: usize,
    ) -> Vec<Url> {
        let mut result = Vec::new();
        let mut visited: HashMap<Url, usize> = HashMap::new();
        self.collect_dependencies(uri, max_depth, 0, &mut visited, &mut result, max_visited);
        result
    }

    /// Forward-walk descendants from multiple roots with a single shared
    /// `visited` map. Each node/edge is visited at most once across the whole
    /// traversal at the *shallowest* depth found, even if the roots' subtrees
    /// overlap (e.g. the edited file plus all its backward ancestors during
    /// sibling-subtree expansion in `compute_affected_dependents_after_edit`).
    /// Roots themselves are excluded from the result, matching
    /// `get_transitive_dependencies`.
    pub fn get_transitive_dependencies_multi_root<'a, I>(
        &self,
        roots: I,
        max_depth: usize,
        max_visited: usize,
    ) -> Vec<Url>
    where
        I: IntoIterator<Item = &'a Url>,
    {
        let mut result = Vec::new();
        let mut visited: HashMap<Url, usize> = HashMap::new();
        for root in roots {
            if visited.len() >= max_visited {
                self.record_visited_budget_truncation();
                break;
            }
            self.collect_dependencies(root, max_depth, 0, &mut visited, &mut result, max_visited);
        }
        result
    }

    /// The **revalidation-consistent set** of `root`: the union
    /// `ancestors(root) ∪ descendants(ancestors(root) ∪ {root})`, i.e. every
    /// file whose cross-file scope-resolution would visit `root`.
    ///
    /// This is the single source of truth for the *traversal shape* that ties
    /// together NSE/func directive *collection* and dependency *revalidation*
    /// (CLAUDE.md "Cross-file `# raven: nse` / `# raven: func` propagation").
    /// Concretely:
    ///
    /// 1. [`Self::get_transitive_dependents`] — all backward ancestors of `root`.
    /// 2. [`Self::get_transitive_dependencies_multi_root`] over
    ///    `once(root).chain(ancestors)` — forward descendants of `root` AND of
    ///    each ancestor (sibling subtrees), sharing one `visited` set.
    ///
    /// Both `super::revalidation::compute_affected_dependents_after_edit`
    /// (over the FULL graph) and `crate::handlers::collect_cross_file_nse`
    /// (over the TRIMMED snapshot subgraph) build their working set from this
    /// method, so they use the **identical traversal shape** — the same two
    /// graph primitives chained the same way — and can no longer drift in
    /// edge-selection logic. The full directed-inverse equivalence additionally
    /// depends on inputs this helper does not encode: both callers passing
    /// matching `max_depth` / `max_visited` budgets, and the deliberate graph
    /// asymmetry (collection over the trimmed subgraph, revalidation over the
    /// full graph). For an UNTRUNCATED neighborhood that asymmetry is intentional
    /// and **safe-direction**: `S_trimmed ⊆ S_full`, so collection can only ever
    /// *omit* a foreign suppression (leaving a real diagnostic in place — a false
    /// positive at worst), never fabricate one or drop a needed revalidation.
    /// Budget truncation does not preserve the per-member directed inverse:
    /// different roots can spend the same budget in different orders. Therefore
    /// the diagnostics snapshot carries the neighborhood walk's authoritative
    /// truncation status and foreign NSE/func collection fails closed whenever
    /// either limit was hit. `extract_subgraph` also drops `non_lending` edges from its
    /// backward index (while the full graph keeps them for revalidation), which
    /// is the same safe-direction asymmetry: excluded open buffers can be
    /// rescheduled when their helpers change, but cannot lend declarations or
    /// parent-prefix scope into those helpers' trimmed snapshots.
    ///
    /// Returns the union with the backward ancestors FIRST, then the forward
    /// descendants, matching both callers' historical ordering. **`root` itself
    /// is NOT excluded** and the result is **not deduplicated across the two
    /// halves** (each half is internally first-visit-deduplicated, but an
    /// ancestor that is also reachable forward can appear in both halves):
    /// callers apply their own root handling, dedup, and ordering — revalidation
    /// excludes `{root}` and dedups via a shared `seen` set; collection filters
    /// `u != root`, then sorts and dedups. Keeping that post-processing in the
    /// callers preserves each one's existing behavior exactly while sharing the
    /// load-bearing two-traversal construction here.
    pub fn revalidation_consistent_set(
        &self,
        root: &Url,
        max_depth: usize,
        max_visited: usize,
    ) -> impl Iterator<Item = Url> {
        let ancestors = self.get_transitive_dependents(root, max_depth, max_visited);
        // `root` first matches the historical `once(root).chain(ancestors)`
        // convention so the shared `max_visited` budget prioritizes the queried
        // file's own subtree.
        let descendants = self.get_transitive_dependencies_multi_root(
            std::iter::once(root).chain(ancestors.iter()),
            max_depth,
            max_visited,
        );
        // The chained iterator owns both Vecs, so it carries no borrow of `&self`.
        // Callers fold it through their own post-processing (revalidation: a
        // shared `seen` set; collection: filter self, then sort + dedup), so the
        // intermediate `.collect()` neither caller historically had is dropped.
        ancestors.into_iter().chain(descendants)
    }

    /// `visited` tracks the *shallowest* depth at which each URI has been
    /// reached. A revisit at a strictly shallower depth must continue
    /// recursing so descendants reachable within the budget through the
    /// shorter path are not silently dropped — diamond-shaped dep graphs
    /// (a common helper sourced via multiple paths of differing length)
    /// would otherwise lose subtrees beyond `max_depth - prev_depth`.
    fn collect_dependencies(
        &self,
        uri: &Url,
        max_depth: usize,
        current_depth: usize,
        visited: &mut HashMap<Url, usize>,
        result: &mut Vec<Url>,
        max_visited: usize,
    ) {
        if current_depth > max_depth {
            self.record_depth_truncation();
            return;
        }
        let is_first_visit = match visited.get(uri) {
            Some(&prev_depth) if prev_depth <= current_depth => return,
            Some(_) => false,
            None => {
                if visited.len() >= max_visited {
                    self.record_visited_budget_truncation();
                    return;
                }
                true
            }
        };
        visited.insert(uri.clone(), current_depth);
        if is_first_visit && current_depth > 0 {
            result.push(uri.clone());
        }
        if current_depth == max_depth {
            if self.has_unvisited_dependencies(uri, visited) {
                self.record_depth_truncation();
            }
            return;
        }

        for edge in self.get_dependencies(uri) {
            if visited.len() >= max_visited && !visited.contains_key(&edge.to) {
                self.record_visited_budget_truncation();
                break;
            }
            self.collect_dependencies(
                &edge.to,
                max_depth,
                current_depth + 1,
                visited,
                result,
                max_visited,
            );
        }
    }

    fn add_edge(&mut self, edge: DependencyEdge) {
        log::trace!(
            "Adding edge: {} -> {} at line {:?}, column {:?} (directive: {}, locality: {:?}, chdir: {})",
            edge.from,
            edge.to,
            edge.call_site_line,
            edge.call_site_column,
            edge.is_directive,
            edge.locality,
            edge.chdir
        );

        // Add to forward index
        self.forward
            .entry(edge.from.clone())
            .or_default()
            .push(edge.clone());
        // Add to backward index
        self.backward.entry(edge.to.clone()).or_default().push(edge);
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

        log::trace!(
            "Checking {} forward edges from {} for removal",
            edges_to_check.len(),
            uri
        );

        // We'll rebuild the forward edges list, keeping only edges created by backward directives
        let mut edges_to_keep = Vec::new();
        let mut edges_to_remove = Vec::new();

        for edge in edges_to_check {
            // Use the is_backward_directive flag to distinguish between:
            // - Forward directive edges (is_directive=true, is_backward_directive=false):
            //   Created by `# raven: source` in THIS file - should be removed
            // - Backward directive edges (is_directive=true, is_backward_directive=true):
            //   Created by `# raven: sourced-by` in OTHER files - should be kept
            // - AST edges (is_directive=false):
            //   Created by source() calls in THIS file - should be removed

            if edge.is_directive && edge.is_backward_directive {
                // Keep backward directive edges - they were created by other files
                edges_to_keep.push(edge);
            } else {
                // Remove forward directive edges and AST edges - they're from this file
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
            log::trace!(
                "  Removing edge: {} -> {} (is_directive={}, is_backward_directive={})",
                edge.from,
                edge.to,
                edge.is_directive,
                edge.is_backward_directive
            );
            if let Some(backward_edges) = self.backward.get_mut(&edge.to) {
                // Remove edges that match the from/to and are NOT backward directive edges
                // (i.e., remove AST edges and forward directive edges)
                backward_edges.retain(|e| {
                    !(e.from == edge.from
                        && e.to == edge.to
                        && !(e.is_directive && e.is_backward_directive))
                });
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

    /// Preserve `uri`'s outgoing forward edges while marking them non-lending.
    ///
    /// Project-excluded open buffers need their own live diagnostics to follow
    /// `source()` edges into non-excluded helpers, but they must not lend their
    /// own symbols back through the dependency graph. Keeping `forward[uri]`
    /// gives the queried buffer a diagnostic neighborhood and lets it consume
    /// helper exports. Keeping the corresponding `backward[child]` entries
    /// lets full-graph revalidation walk from a helper edit back to the
    /// excluded open consumer. The `non_lending` marker makes those reverse
    /// entries asymmetric: lending consumers (`parent_prefix_at`, the
    /// standalone fingerprint helper, and the trimmed subgraph's backward
    /// index) skip them, so the helper never inherits the excluded buffer's
    /// symbols, packages, or cross-file NSE/func declarations. Incoming edges
    /// to `uri` are still removed wholesale so non-excluded parents cannot
    /// source an excluded child via backward directives.
    ///
    /// Returns `true` when incoming edges were removed or any outgoing edge
    /// copy flips from lending to non-lending.
    pub(crate) fn make_forward_edges_non_lending(&mut self, uri: &Url) -> bool {
        let mut changed = false;

        if self.backward.contains_key(uri) {
            self.remove_backward_edges(uri);
            changed = true;
        }

        let outgoing: Vec<GraphEdgeDedupKey> = self
            .forward
            .get_mut(uri)
            .map(|edges| {
                edges
                    .iter_mut()
                    .map(|edge| {
                        if !edge.non_lending {
                            edge.non_lending = true;
                            changed = true;
                        }
                        edge.graph_dedup_key()
                    })
                    .collect()
            })
            .unwrap_or_default();

        for key in outgoing {
            let to = key.target().clone();
            if let Some(backward_edges) = self.backward.get_mut(&to) {
                for candidate in backward_edges {
                    if candidate.graph_dedup_key() == key && !candidate.non_lending {
                        candidate.non_lending = true;
                        changed = true;
                    }
                }
            }
        }

        if changed {
            self.edge_revision
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }

        changed
    }

    /// Collect all URIs reachable from `uri` within `max_depth` hops,
    /// following both forward and backward edges.
    ///
    /// `max_visited` caps the total number of nodes collected to prevent
    /// unbounded expansion in dense bidirectional graphs.
    ///
    /// Single-seed convenience wrapper around
    /// [`Self::collect_neighborhood_multi`].
    pub fn collect_neighborhood(
        &self,
        uri: &Url,
        max_depth: usize,
        max_visited: usize,
    ) -> HashSet<Url> {
        self.collect_neighborhood_with_truncation(uri, max_depth, max_visited)
            .0
    }

    /// Single-seed neighborhood walk with authoritative per-query truncation
    /// status. Unlike the graph-wide counters, this result can safely govern a
    /// specific diagnostic snapshot.
    fn collect_neighborhood_with_truncation(
        &self,
        uri: &Url,
        max_depth: usize,
        max_visited: usize,
    ) -> (HashSet<Url>, NeighborhoodTruncation) {
        self.collect_neighborhood_multi_with_truncation(
            std::iter::once(uri.clone()),
            max_depth,
            max_visited,
        )
    }

    /// Multi-seed neighborhood walk: a single BFS from all seeds sharing one
    /// visited set and one global `max_visited` budget, avoiding redundant
    /// traversal of shared ancestors. [`Self::collect_neighborhood`] is the
    /// single-seed wrapper.
    pub fn collect_neighborhood_multi(
        &self,
        seeds: impl IntoIterator<Item = Url>,
        max_depth: usize,
        max_visited: usize,
    ) -> HashSet<Url> {
        self.collect_neighborhood_multi_with_truncation(seeds, max_depth, max_visited)
            .0
    }

    fn collect_neighborhood_multi_with_truncation(
        &self,
        seeds: impl IntoIterator<Item = Url>,
        max_depth: usize,
        max_visited: usize,
    ) -> (HashSet<Url>, NeighborhoodTruncation) {
        let mut visited = HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        let mut truncation = NeighborhoodTruncation::default();
        for seed in seeds {
            if visited.contains(&seed) {
                continue;
            }
            if visited.len() >= max_visited {
                truncation.visited = true;
                continue;
            }
            visited.insert(seed.clone());
            queue.push_back((seed, 0usize));
        }

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                truncation.depth |= self.has_unvisited_neighbors(&current, &visited);
                continue;
            }
            if let Some(edges) = self.forward.get(&current) {
                for edge in edges {
                    if visited.contains(&edge.to) {
                        continue;
                    }
                    if visited.len() >= max_visited {
                        truncation.visited = true;
                        continue;
                    }
                    visited.insert(edge.to.clone());
                    queue.push_back((edge.to.clone(), depth + 1));
                }
            }
            if let Some(edges) = self.backward.get(&current) {
                for edge in edges {
                    if visited.contains(&edge.from) {
                        continue;
                    }
                    if visited.len() >= max_visited {
                        truncation.visited = true;
                        continue;
                    }
                    visited.insert(edge.from.clone());
                    queue.push_back((edge.from.clone(), depth + 1));
                }
            }
        }
        if truncation.depth {
            self.record_depth_truncation();
        }
        if truncation.visited {
            self.record_visited_budget_truncation();
        }
        (visited, truncation)
    }

    /// Truncation gate for issue #479's cross-prefix forward-child memo sharing
    /// within a `ScopeStream`. Returns `true` only when no scope resolution
    /// rooted in this neighborhood can hit max-depth truncation.
    ///
    /// A shared forward-child memo is byte-identical to the un-memoized resolver
    /// EXCEPT when an entry's resolved scope depends on a per-query input that
    /// `ForwardChildKey` does not encode (see `ForwardChildMemo`'s "never
    /// shared across queries" discipline). Within one stream there are exactly
    /// two such varying inputs, handled by two independent mechanisms:
    ///
    /// 1. **`query_inside_function` (local scoping).** Current-frame and
    ///    non-inheriting source destinations can apply a
    ///    `query_inside_function`-dependent declared-only inheritance policy
    ///    that the memo key omits. This is handled
    ///    by **slot isolation** in `ScopeStream`: the `query_inside_function =
    ///    true` (hoisted, inside-a-function) prefix slot gets its OWN fresh memo,
    ///    so only the mutually-consistent `false`-context computations (the
    ///    `false` top slot plus every per-child-source call, which all query at
    ///    EOF) ever share. This gate does NOT cover that mode.
    /// 2. **Truncation (visited context).** A memo hit short-circuits a walk that
    ///    would otherwise mark `visited`, perturbing a sibling path's truncation
    ///    (`depth_exceeded`/`chain`). This gate bounds the part of that mode that
    ///    can corrupt the **symbol set**: disable sharing unless the whole
    ///    bidirectional neighborhood sits at BFS depth `< max_depth - 1`. The
    ///    bound is SOUND for symbols regardless of the depth check's precision,
    ///    because a truncating resolution carries a non-empty `depth_exceeded`
    ///    and is therefore never written to the memo (the
    ///    `computed.depth_exceeded.is_empty()` guard in
    ///    `resolve_forward_child_memoized`), so the cache can never feed a
    ///    truncated (wrong) symbol set to another prefix root. The
    ///    `depth_exceeded`/`chain` channel itself has a bounded residual — see
    ///    the NOTE below and issue #484.
    ///
    /// The BFS runs over the bidirectional source neighborhood (global
    /// `visited`, O(V+E)) bounded by `budget`; on exhaustion the method returns
    /// `false` (conservatively disabling sharing — sound, just no speedup).
    /// Realistic global-scope hubs (default `maxChainDepth` 64, shallow chains)
    /// pass and get the full O(N²)→O(N) collapse.
    ///
    /// NOTE on the depth check (residual risk — issue #484): BFS depth is the
    /// *shortest* bidirectional distance, while the resolver can reach a node at
    /// a greater `current_depth` via a *longer* path (forward children resolve
    /// with cloned `visited`, and the backward-then-forward walk can revisit).
    /// So this gate can ADMIT a neighborhood that the resolver then truncates
    /// via a longer path. The consequence is confined to the heuristic
    /// `depth_exceeded`/`chain` channel: under such truncation, the shared
    /// (visited-independent) memo can cause the advisory "Maximum chain depth
    /// exceeded" diagnostic to be emitted on a file where the un-memoized
    /// resolver would not, or vice versa. It can NEVER corrupt the symbol set
    /// (see point 2 — truncated scopes are never cached). It only triggers when
    /// a `source()` chain actually reaches `maxChainDepth`, which does not occur
    /// at the default `maxChainDepth = 64` on realistic workspaces (worldwide
    /// `raven check .` is byte-identical and deterministic). A fully sound depth
    /// check is a longest-simple-path quantity (NP-hard in general, and a naive
    /// search would disable sharing on the very star-hub topology this memo
    /// speeds up), so the residual is accepted and tracked in #484 (proposed
    /// fix: verify the advisory at emit time, off the hot path). The
    /// `memo_equiv_*` suite pins the symbol-equivalence guarantee, including the
    /// gate-admitted-with-truncation case (`memo_equiv_gate_admitted_with_truncation`).
    pub fn prefix_memo_share_safe(&self, from: &Url, max_depth: usize, budget: usize) -> bool {
        // Need at least one hop of headroom below the truncation limit.
        if max_depth < 2 {
            return false;
        }
        let depth_cap = max_depth - 1;
        let mut visited: HashSet<Url> = HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        visited.insert(from.clone());
        queue.push_back((from.clone(), 0usize));
        let mut steps = 0usize;
        while let Some((cur, depth)) = queue.pop_front() {
            steps += 1;
            if steps > budget {
                return false; // conservative: cannot verify the whole neighborhood
            }
            // Any node sitting at/above the depth cap means the neighborhood is
            // deep enough that resolution could plausibly truncate — gate off.
            if depth >= depth_cap {
                return false;
            }
            if let Some(edges) = self.forward.get(&cur) {
                for e in edges {
                    if visited.insert(e.to.clone()) {
                        queue.push_back((e.to.clone(), depth + 1));
                    }
                }
            }
            if let Some(edges) = self.backward.get(&cur) {
                for e in edges {
                    if visited.insert(e.from.clone()) {
                        queue.push_back((e.from.clone(), depth + 1));
                    }
                }
            }
        }
        true
    }

    /// Detect cycles involving a URI.
    ///
    /// Returns a `CycleDetection` containing:
    /// - `outgoing_edge`: the first edge FROM `uri` that leads into the cycle
    ///   (use this for diagnostic positioning in the queried file)
    /// - `closing_edge`: the edge that points BACK to `uri` completing the cycle
    ///   (use this for the diagnostic message details)
    pub fn detect_cycle(&self, uri: &Url) -> Option<CycleDetection> {
        use std::sync::atomic::Ordering;
        let revision = self.edge_revision.load(Ordering::Acquire);

        // Fast path: cache hit at the current edge revision. `peek` to keep
        // the read lock concurrent (no LRU promotion under shared access).
        if let Ok(guard) = self.cycle_cache.read()
            && let Some((cached_rev, cached_result)) = guard.peek(uri)
            && *cached_rev == revision
        {
            self.cycle_cache_hits.fetch_add(1, Ordering::Relaxed);
            return cached_result.clone();
        }

        let mut visited = HashSet::new();
        let mut path = Vec::new();
        let computed = self.detect_cycle_recursive(uri, uri, &mut visited, &mut path);

        if let Ok(mut guard) = self.cycle_cache.write() {
            // `push` promotes/evicts under exclusive access.
            guard.push(uri.clone(), (revision, computed.clone()));
        }
        computed
    }

    /// Number of `detect_cycle` calls served from the cache.
    /// Exposed for tests; not used in production code.
    pub fn cycle_cache_hits(&self) -> u64 {
        self.cycle_cache_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether this graph contains **any** `source()` cycle.
    ///
    /// Used by the forward-child scope memo (issue #472): a child file's
    /// resolved scope is only a pure function of its memo key
    /// `(child_uri, path_fp, pkg_fp)` when no cycle is reachable from it. A
    /// cycle anywhere in the child's neighborhood (including its *backward*
    /// parent walk) makes parts of the scope (e.g. the visited-order `chain`,
    /// and potentially the parent-prefix symbol set) depend on traversal state,
    /// so the memo must disable itself on cyclic graphs. Computing this once
    /// (cached by `edge_revision`) lets the resolver gate cheaply.
    ///
    /// Iterative 3-color DFS over forward edges; `O(V + E)` on a miss, `O(1)`
    /// on a hit.
    pub fn contains_cycle(&self) -> bool {
        use std::sync::atomic::Ordering;
        let revision = self.edge_revision.load(Ordering::Acquire);
        if let Ok(guard) = self.any_cycle_cache.read()
            && let Some((cached_rev, cached)) = *guard
            && cached_rev == revision
        {
            return cached;
        }

        // 3-color DFS: White (unseen), Gray (on stack), Black (done). A forward
        // edge into a Gray node is a back-edge → cycle.
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            Gray,
            Black,
        }
        let mut color: HashMap<&Url, Color> = HashMap::new();
        let mut found = false;
        // Rooting over `self.forward.keys()` is complete: every node on a cycle
        // has an outgoing edge that continues the cycle, so it appears as a
        // `forward` key. `update_file` records both `source()` and
        // backward-directive edges in `forward` (see
        // `test_backward_directive_creates_edge`), so no cycle-capable node is
        // reachable only through the `backward` map.
        // Stack frames: (node, index-of-next-child-to-visit).
        'outer: for root in self.forward.keys() {
            if color.contains_key(root) {
                continue;
            }
            let mut stack: Vec<(&Url, usize)> = vec![(root, 0)];
            color.insert(root, Color::Gray);
            while let Some(&(node, idx)) = stack.last() {
                let next = self.forward.get(node).and_then(|e| e.get(idx));
                match next {
                    Some(edge) => {
                        if let Some(top) = stack.last_mut() {
                            top.1 += 1;
                        }
                        let to = &edge.to;
                        match color.get(to) {
                            Some(Color::Gray) => {
                                found = true;
                                break 'outer;
                            }
                            Some(Color::Black) => {}
                            None => {
                                color.insert(to, Color::Gray);
                                stack.push((to, 0));
                            }
                        }
                    }
                    None => {
                        color.insert(node, Color::Black);
                        stack.pop();
                    }
                }
            }
        }

        if let Ok(mut guard) = self.any_cycle_cache.write() {
            *guard = Some((revision, found));
        }
        found
    }

    /// Return the `(neighborhood, subgraph)` pair for `uri`, sharing the
    /// computation across callers via a graph-edge-revisioned cache.
    ///
    /// Diagnostic snapshots fan out to many dependents; each dependent's
    /// snapshot computes the neighborhood and trims the graph the same way.
    /// Caching that result by `(uri, max_depth, max_visited, edge_revision)`
    /// turns repeat calls (within a stable graph) into refcount bumps.
    pub fn cached_neighborhood_subgraph(
        &self,
        uri: &Url,
        max_depth: usize,
        max_visited: usize,
    ) -> std::sync::Arc<NeighborhoodSubgraph> {
        use std::sync::atomic::Ordering;
        let revision = self.edge_revision.load(Ordering::Acquire);

        if let Ok(guard) = self.subgraph_cache.read()
            && let Some((cached_rev, cached)) = guard.peek(&(uri.clone(), max_depth, max_visited))
            && *cached_rev == revision
        {
            self.subgraph_cache_hits.fetch_add(1, Ordering::Relaxed);
            return std::sync::Arc::clone(cached);
        }

        let (neighborhood, truncation) =
            self.collect_neighborhood_with_truncation(uri, max_depth, max_visited);
        let subgraph = std::sync::Arc::new(self.extract_subgraph(&neighborhood));
        let payload = std::sync::Arc::new(NeighborhoodSubgraph {
            neighborhood,
            subgraph,
            truncation,
        });

        if let Ok(mut guard) = self.subgraph_cache.write() {
            guard.push(
                (uri.clone(), max_depth, max_visited),
                (revision, std::sync::Arc::clone(&payload)),
            );
        }
        payload
    }

    /// Current global edge revision — a monotonic counter bumped on any
    /// structural edge change (`update_file` reporting `edges_changed`,
    /// `remove_file`) or lending-policy transition. Used as the
    /// membership-pinning component of the cross-snapshot `StandaloneScopeCache`
    /// key (issue #483): capture it from the *real* `WorldState` graph (a cloned
    /// per-snapshot graph resets its own counter to 0), so it changes whenever a
    /// `source()` edge is added, retargeted, moved, or switched between lending
    /// and non-lending anywhere in the workspace.
    pub fn edge_revision(&self) -> u64 {
        self.edge_revision
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Number of `cached_neighborhood_subgraph` calls served from the cache.
    /// Exposed for tests; not used in production code.
    pub fn subgraph_cache_hits(&self) -> u64 {
        self.subgraph_cache_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of traversals that hit the max-visited budget.
    pub fn visited_budget_truncations(&self) -> u64 {
        self.visited_budget_truncations
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of traversals that hit a depth limit while more edges remained.
    pub fn depth_truncations(&self) -> u64 {
        self.depth_truncations
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Dump the current state of the dependency graph for debugging.
    /// Returns a human-readable string representation of all edges.
    pub fn dump_state(&self) -> String {
        let total_edges: usize = self.forward.values().map(|v| v.len()).sum();
        let mut output = String::new();
        output.push_str(&format!(
            "Dependency Graph State ({} total edges):\n",
            total_edges
        ));
        output.push_str(&format!(
            "  {} parent files with outgoing edges\n",
            self.forward.len()
        ));
        output.push_str(&format!(
            "  {} child files with incoming edges\n\n",
            self.backward.len()
        ));

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
                        if edge.is_directive {
                            f.push("directive");
                        }
                        if edge.locality != SourceLocality::Global {
                            f.push(match edge.locality {
                                SourceLocality::Global => unreachable!(),
                                SourceLocality::CurrentFrame => "local",
                                SourceLocality::NonInheriting => "non-inheriting",
                            });
                        }
                        if edge.chdir {
                            f.push("chdir");
                        }
                        if edge.is_sys_source {
                            f.push("sys.source");
                        }
                        if f.is_empty() {
                            "".to_string()
                        } else {
                            format!(" [{}]", f.join(", "))
                        }
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
        path: &mut Vec<DependencyEdge>,
    ) -> Option<CycleDetection> {
        if visited.contains(current) {
            return None;
        }
        visited.insert(current.clone());

        for edge in self.get_dependencies(current) {
            path.push(edge.clone());
            if &edge.to == start {
                // path[0] is the outgoing edge from `start`, path[last] closes the cycle
                return Some(CycleDetection {
                    outgoing_edge: path[0].clone(),
                    closing_edge: edge.clone(),
                });
            }
            if let Some(detection) = self.detect_cycle_recursive(start, &edge.to, visited, path) {
                return Some(detection);
            }
            path.pop();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::BackwardDirective;
    use super::*;
    use std::fs::File;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn url(s: &str) -> Url {
        Url::parse(&format!("file:///project/{}", s)).unwrap()
    }

    fn workspace_root() -> Url {
        Url::parse("file:///project").unwrap()
    }

    /// Create a temporary workspace with the given files and return (temp_dir, workspace_root_url)
    fn create_temp_workspace(files: &[&str]) -> (TempDir, Url) {
        let temp_dir = TempDir::new().unwrap();
        for file in files {
            let path = temp_dir.path().join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            File::create(&path).unwrap();
        }
        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        (temp_dir, workspace_url)
    }

    /// Create a URL for a file in the temp workspace
    fn temp_url(temp_dir: &TempDir, file: &str) -> Url {
        Url::from_file_path(temp_dir.path().join(file)).unwrap()
    }

    use crate::test_utils::host_is_case_sensitive;

    fn make_source(path: &str, line: u32) -> super::super::types::ForwardSource {
        super::super::types::ForwardSource {
            path: path.to_string(),
            line,
            column: 0,
            is_directive: false,
            chdir: false,
            is_sys_source: false,
            ..Default::default()
        }
    }

    fn make_meta_with_source(path: &str, line: u32) -> CrossFileMetadata {
        CrossFileMetadata {
            sources: vec![make_source(path, line)],
            ..Default::default()
        }
    }

    /// Regression (WI2b cache soundness, confirmed by Codex): a `source()` /
    /// `sys.source()` call's destination locality and `is_function_scoped` flag
    /// change whether/how it contributes symbols, yet an edit can flip either at
    /// a FIXED `(line, column)` — e.g. `envir = globalenv()` → `new.env()`, or
    /// dropping an enclosing `function(){}` wrapper on a prior line. Both are in
    /// source invocation identity, so such a flip changes the dependency
    /// interface and bumps `edge_revision`; the standalone-scope cache key (#483)
    /// and dependent revalidation both depend on it.
    #[test]
    fn edge_revision_bumps_on_source_semantics_flip_at_fixed_position() {
        use super::super::types::ForwardSource;
        fn meta(sys_global: bool, func_scoped: bool) -> CrossFileMetadata {
            CrossFileMetadata {
                sources: vec![ForwardSource {
                    path: "B.R".to_string(),
                    line: 1,
                    column: 0,
                    is_directive: false,
                    locality: if sys_global {
                        SourceLocality::Global
                    } else {
                        SourceLocality::NonInheriting
                    },
                    chdir: false,
                    is_sys_source: true,
                    is_function_scoped: func_scoped,
                    ..Default::default()
                }],
                ..Default::default()
            }
        }
        let mut graph = DependencyGraph::new();
        graph.update_file(
            &url("A.R"),
            &meta(true, false),
            Some(&workspace_root()),
            |_| None,
        );
        let rev0 = graph.edge_revision();

        // Flip ONLY the sys.source destination locality (same line/col/path/other flags).
        graph.update_file(
            &url("A.R"),
            &meta(false, false),
            Some(&workspace_root()),
            |_| None,
        );
        let rev1 = graph.edge_revision();
        assert!(
            rev1 > rev0,
            "flipping sys.source locality at a fixed call site must bump edge_revision"
        );

        // Flip ONLY is_function_scoped.
        graph.update_file(
            &url("A.R"),
            &meta(false, true),
            Some(&workspace_root()),
            |_| None,
        );
        let rev2 = graph.edge_revision();
        assert!(
            rev2 > rev1,
            "flipping is_function_scoped at a fixed call site must bump edge_revision"
        );
    }

    #[test]
    fn locality_survives_graph_insertion_trim_and_revision_flip() {
        use super::super::types::{ForwardSource, SourceLocality};

        let meta = |locality| CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "B.R".to_string(),
                line: 1,
                column: 0,
                locality,
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut graph = DependencyGraph::new();
        graph.update_file(
            &url("A.R"),
            &meta(SourceLocality::CurrentFrame),
            Some(&workspace_root()),
            |_| None,
        );
        let edge = &graph.get_dependencies(&url("A.R"))[0];
        assert_eq!(edge.locality, SourceLocality::CurrentFrame);
        assert!(!edge.uses_declared_only_parent_inheritance());

        let trimmed = graph.extract_subgraph(&HashSet::from([url("A.R"), url("B.R")]));
        assert_eq!(
            trimmed.get_dependencies(&url("A.R"))[0].locality,
            SourceLocality::CurrentFrame
        );

        let revision = graph.edge_revision();
        let update = graph.update_file(
            &url("A.R"),
            &meta(SourceLocality::NonInheriting),
            Some(&workspace_root()),
            |_| None,
        );
        assert!(update.edges_changed);
        assert!(graph.edge_revision() > revision);
        let edge = &graph.get_dependencies(&url("A.R"))[0];
        assert_eq!(edge.locality, SourceLocality::NonInheriting);
        assert!(edge.uses_declared_only_parent_inheritance());
    }

    #[test]
    fn declared_only_parent_inheritance_depends_only_on_non_inheriting_locality() {
        use super::super::types::{ForwardSource, SourceLocality};

        let meta = |locality| CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "B.R".to_string(),
                line: 1,
                column: 0,
                locality,
                is_function_scoped: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut graph = DependencyGraph::new();

        for (locality, expected) in [
            (SourceLocality::Global, false),
            (SourceLocality::CurrentFrame, false),
            (SourceLocality::NonInheriting, true),
        ] {
            graph.update_file(
                &url("A.R"),
                &meta(locality),
                Some(&workspace_root()),
                |_| None,
            );
            assert_eq!(
                graph.get_dependencies(&url("A.R"))[0].uses_declared_only_parent_inheritance(),
                expected,
                "unexpected declared-only policy for {locality:?}"
            );
        }
    }

    /// Issue #479: the truncation gate admits a shallow acyclic neighborhood.
    #[test]
    fn prefix_memo_share_safe_admits_shallow_acyclic() {
        // A -> B -> C, queried from A. Max BFS depth 2 ≪ max_depth.
        let mut graph = DependencyGraph::new();
        graph.update_file(
            &url("A.R"),
            &make_meta_with_source("B.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &url("B.R"),
            &make_meta_with_source("C.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        assert!(graph.prefix_memo_share_safe(&url("A.R"), 64, 200_000));
    }

    /// Issue #479: a neighborhood deep enough to truncate is gated off, but a
    /// generous `max_depth` over the same graph is admitted.
    #[test]
    fn prefix_memo_share_safe_gates_off_deep_chain() {
        // A -> B -> C -> D -> E
        let mut graph = DependencyGraph::new();
        for (from, to) in [
            ("A.R", "B.R"),
            ("B.R", "C.R"),
            ("C.R", "D.R"),
            ("D.R", "E.R"),
        ] {
            graph.update_file(
                &url(from),
                &make_meta_with_source(to, 1),
                Some(&workspace_root()),
                |_| None,
            );
        }
        // max_depth = 3 -> depth_cap = 2; node C at BFS depth 2 trips the gate.
        assert!(!graph.prefix_memo_share_safe(&url("A.R"), 3, 200_000));
        // A generous limit admits the same (shallow-relative) neighborhood.
        assert!(graph.prefix_memo_share_safe(&url("A.R"), 64, 200_000));
    }

    /// Issue #479: budget exhaustion conservatively disables sharing.
    #[test]
    fn prefix_memo_share_safe_budget_exhaustion_is_false() {
        let mut graph = DependencyGraph::new();
        graph.update_file(
            &url("A.R"),
            &make_meta_with_source("B.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        assert!(!graph.prefix_memo_share_safe(&url("A.R"), 64, 0));
    }

    /// Issue #479: the truncation gate deliberately does NOT consider
    /// `local`/`sys.source` edges — that divergence mode is handled by slot
    /// isolation in `ScopeStream`, not here. A shallow graph with a local edge
    /// is still admitted by this gate.
    #[test]
    fn prefix_memo_share_safe_ignores_local_edges() {
        let mut graph = DependencyGraph::new();
        let mut meta = make_meta_with_source("B.R", 1);
        meta.sources[0].locality = SourceLocality::CurrentFrame;
        graph.update_file(&url("A.R"), &meta, Some(&workspace_root()), |_| None);
        assert!(graph.prefix_memo_share_safe(&url("A.R"), 64, 200_000));
    }

    /// Issue #479: pin the exact admit/reject boundary so an off-by-one in
    /// `depth_cap = max_depth - 1` or the `depth >= depth_cap` comparison is
    /// caught (the deep-chain test uses a generous margin and would not). With
    /// `max_depth = 4`, `depth_cap = 3`: a neighborhood whose deepest node is at
    /// BFS depth 2 (== depth_cap - 1) is the LAST admitted; depth 3 (== depth_cap)
    /// is the FIRST rejected.
    #[test]
    fn prefix_memo_share_safe_pins_depth_boundary() {
        let chain = |links: &[(&str, &str)]| {
            let mut graph = DependencyGraph::new();
            for (from, to) in links {
                graph.update_file(
                    &url(from),
                    &make_meta_with_source(to, 1),
                    Some(&workspace_root()),
                    |_| None,
                );
            }
            graph
        };
        // A->B->C : deepest BFS node C at depth 2 == depth_cap-1 → admitted.
        let admitted = chain(&[("A.R", "B.R"), ("B.R", "C.R")]);
        assert!(admitted.prefix_memo_share_safe(&url("A.R"), 4, 200_000));
        // A->B->C->D : D at depth 3 == depth_cap → rejected.
        let rejected = chain(&[("A.R", "B.R"), ("B.R", "C.R"), ("C.R", "D.R")]);
        assert!(!rejected.prefix_memo_share_safe(&url("A.R"), 4, 200_000));
    }

    /// Issue #479: `max_depth < 2` has no headroom below the truncation limit,
    /// so the gate must refuse outright (the early return).
    #[test]
    fn prefix_memo_share_safe_rejects_tiny_max_depth() {
        let mut graph = DependencyGraph::new();
        graph.update_file(
            &url("A.R"),
            &make_meta_with_source("B.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        assert!(!graph.prefix_memo_share_safe(&url("A.R"), 0, 200_000));
        assert!(!graph.prefix_memo_share_safe(&url("A.R"), 1, 200_000));
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
        let dependents = graph.get_transitive_dependents(&c, 10, 200);
        assert_eq!(dependents.len(), 2);
        assert!(dependents.contains(&b));
        assert!(dependents.contains(&a));
    }

    #[test]
    fn test_transitive_dependencies() {
        // Forward-direction transitive walk: when `a` sources `b` and `b`
        // sources `c`, the *dependencies* of `a` include `b` and `c`.
        // Children inherit symbols from their parent's scope at the
        // `source()` call site, so a content edit in `a` requires
        // revalidating both `b` and `c` (use case for the cross-file
        // diagnostic plumbing fix in `did_change`).
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");
        let c = url("c.R");

        // a sources b, b sources c
        let meta_a = make_meta_with_source("b.R", 1);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);

        let meta_b = make_meta_with_source("c.R", 1);
        graph.update_file(&b, &meta_b, Some(&workspace_root()), |_| None);

        let dependencies = graph.get_transitive_dependencies(&a, 10, 200);
        assert_eq!(dependencies.len(), 2);
        assert!(dependencies.contains(&b));
        assert!(dependencies.contains(&c));

        // Symmetric check: c, the leaf, has no dependencies.
        let leaf = graph.get_transitive_dependencies(&c, 10, 200);
        assert!(leaf.is_empty());
    }

    #[test]
    fn test_transitive_dependencies_respects_max_depth() {
        // Walk depth must be bounded. With max_depth = 1, only direct
        // children are returned, not grandchildren.
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");

        let meta_a = make_meta_with_source("b.R", 1);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);
        let meta_b = make_meta_with_source("c.R", 1);
        graph.update_file(&b, &meta_b, Some(&workspace_root()), |_| None);

        let depth1 = graph.get_transitive_dependencies(&a, 1, 200);
        assert_eq!(depth1, vec![b.clone()]);
    }

    #[test]
    fn test_transitive_dependencies_diamond_short_path_subtree_not_lost() {
        // Diamond at differing depths: descendants reachable through the
        // SHORT path must survive even if the long path was visited first.
        //
        //   root → b → c → d → x → y
        //   root → e → x
        //
        // With max_depth = 5, the long path reaches `x` at depth 4 and
        // recurses into `y` at depth 5 (== max_depth, so `y`'s children
        // would not be visited). The short path reaches `x` at depth 2; if
        // edge order forces the long path first, a `HashSet` `visited`
        // would short-circuit the second visit and lose any descendants
        // reachable from `x` via the short path.
        //
        // We exercise the reachability invariant by adding `y` and `z` so
        // that on the SHORT path z is at depth 4 (≤ max_depth) but on the
        // LONG path z would be at depth 6 (> max_depth). Without
        // depth-shortest tracking, `z` is missing whenever the long path
        // visits `x` first.
        let mut graph = DependencyGraph::new();
        let root = url("root.R");
        let b = url("b.R");
        let c = url("c.R");
        let d = url("d.R");
        let e = url("e.R");
        let x = url("x.R");
        let y = url("y.R");
        let z = url("z.R");

        // Long path: root → b → c → d → x
        use super::super::types::ForwardSource;
        let meta_root = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "b.R".to_string(),
                    line: 1,
                    column: 0,
                    ..Default::default()
                },
                ForwardSource {
                    path: "e.R".to_string(),
                    line: 2,
                    column: 0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        graph.update_file(&root, &meta_root, Some(&workspace_root()), |_| None);
        graph.update_file(
            &b,
            &make_meta_with_source("c.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &c,
            &make_meta_with_source("d.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &d,
            &make_meta_with_source("x.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        // Short path: root → e → x
        graph.update_file(
            &e,
            &make_meta_with_source("x.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        // x → y → z
        graph.update_file(
            &x,
            &make_meta_with_source("y.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &y,
            &make_meta_with_source("z.R", 1),
            Some(&workspace_root()),
            |_| None,
        );

        // max_depth = 5. Short path: root(0)→e(1)→x(2)→y(3)→z(4) — all
        // reachable. Long path: root(0)→b(1)→c(2)→d(3)→x(4)→y(5)→z(6,
        // beyond max). With shortest-depth tracking, z must be in result
        // regardless of edge iteration order.
        let deps = graph.get_transitive_dependencies(&root, 5, 1000);
        assert!(
            deps.contains(&z),
            "z must be reachable via the short path; deps={deps:?}"
        );
        assert!(deps.contains(&y), "y must be reachable; deps={deps:?}");
        assert!(deps.contains(&x), "x must be in deps; deps={deps:?}");
    }

    #[test]
    fn test_transitive_dependents_diamond_short_path_subtree_not_lost() {
        // Symmetric case for the backward walk: ancestors reachable through
        // a short path must survive even if a long path was visited first.
        //
        //   root_anc → mid → x      (long path: ancestor at depth 2)
        //   short_anc → x           (short path: ancestor at depth 1)
        //
        // x's transitive dependents (parents-of-parents) must include
        // root_anc regardless of edge iteration order.
        let mut graph = DependencyGraph::new();
        let root_anc = url("root_anc.R");
        let mid = url("mid.R");
        let short_anc = url("short_anc.R");
        let x = url("x.R");

        // root_anc → mid → x
        graph.update_file(
            &root_anc,
            &make_meta_with_source("mid.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &mid,
            &make_meta_with_source("x.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        // short_anc → x
        graph.update_file(
            &short_anc,
            &make_meta_with_source("x.R", 1),
            Some(&workspace_root()),
            |_| None,
        );

        // max_depth = 2. Querying x's transitive dependents must yield
        // {mid (depth 1), short_anc (depth 1), root_anc (depth 2)}.
        let dependents = graph.get_transitive_dependents(&x, 2, 1000);
        assert!(
            dependents.contains(&mid),
            "mid must be reachable; dependents={dependents:?}"
        );
        assert!(
            dependents.contains(&short_anc),
            "short_anc must be reachable; dependents={dependents:?}"
        );
        assert!(
            dependents.contains(&root_anc),
            "root_anc must be reachable at depth 2; dependents={dependents:?}"
        );
    }

    #[test]
    fn test_transitive_dependencies_handles_cycles() {
        // Cycles must terminate without infinite recursion. a → b → a.
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");

        let meta_a = make_meta_with_source("b.R", 1);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);
        let meta_b = make_meta_with_source("a.R", 1);
        graph.update_file(&b, &meta_b, Some(&workspace_root()), |_| None);

        let deps = graph.get_transitive_dependencies(&a, 10, 200);
        // a's children: b. b's children: a (cycle, skipped). So only b.
        assert_eq!(deps, vec![b]);
    }

    #[test]
    fn test_edge_deduplication() {
        let mut graph = DependencyGraph::new();
        let main = url("main.R");

        // Two sources to same file at same position should deduplicate
        use super::super::types::ForwardSource;
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: false,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true, // Different is_directive, but same key
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
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
    fn test_transitive_dependents_star_graph_respects_max_visited() {
        // Star graph: a, b, c, d, e all source "hub.R"
        let mut graph = DependencyGraph::new();
        let hub = url("hub.R");
        let spokes: Vec<Url> = (0..5).map(|i| url(&format!("spoke_{}.R", i))).collect();

        for spoke in &spokes {
            let meta = make_meta_with_source("hub.R", 1);
            graph.update_file(spoke, &meta, Some(&workspace_root()), |_| None);
        }

        // With max_visited=3, only 2 dependents should be returned (root + 2 = 3 visited)
        let dependents = graph.get_transitive_dependents(&hub, 10, 3);
        assert!(
            dependents.len() <= 2,
            "max_visited=3 should cap at 2 dependents (plus root), got {}",
            dependents.len()
        );

        // No duplicates
        let unique: std::collections::HashSet<_> = dependents.iter().collect();
        assert_eq!(unique.len(), dependents.len(), "should have no duplicates");

        // Full traversal returns all 5
        let all_dependents = graph.get_transitive_dependents(&hub, 10, 200);
        assert_eq!(all_dependents.len(), 5);
    }

    #[test]
    fn visited_budget_counter_increments_when_transitive_walk_truncates() {
        let mut graph = DependencyGraph::new();
        let hub = url("hub.R");
        for i in 0..5 {
            let spoke = url(&format!("spoke_{i}.R"));
            graph.update_file(
                &spoke,
                &make_meta_with_source("hub.R", 1),
                Some(&workspace_root()),
                |_| None,
            );
        }

        let _ = graph.get_transitive_dependents(&hub, 10, 3);

        assert!(
            graph.visited_budget_truncations() > 0,
            "budget-limited traversal should record a truncation"
        );
    }

    #[test]
    fn depth_counter_increments_when_transitive_walk_has_more_edges_at_limit() {
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");
        graph.update_file(
            &a,
            &make_meta_with_source("b.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &b,
            &make_meta_with_source("c.R", 1),
            Some(&workspace_root()),
            |_| None,
        );

        let _ = graph.get_transitive_dependencies(&a, 1, 200);

        assert!(
            graph.depth_truncations() > 0,
            "depth-limited traversal should record a truncation when children remain"
        );
    }

    #[test]
    fn truncation_counters_stay_zero_when_budget_and_depth_are_sufficient() {
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        graph.update_file(
            &a,
            &make_meta_with_source("b.R", 1),
            Some(&workspace_root()),
            |_| None,
        );

        let _ = graph.get_transitive_dependencies(&a, 10, 200);
        let _ = graph.collect_neighborhood(&a, 10, 200);

        assert_eq!(graph.visited_budget_truncations(), 0);
        assert_eq!(graph.depth_truncations(), 0);
    }

    #[test]
    fn test_transitive_dependents_diamond_no_duplicates() {
        // Diamond: a->c, b->c, d->a, d->b (so d depends on c transitively via both a and b)
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");
        let c = url("c.R");
        let d = url("d.R");

        let meta_a = make_meta_with_source("c.R", 1);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);

        let meta_b = make_meta_with_source("c.R", 1);
        graph.update_file(&b, &meta_b, Some(&workspace_root()), |_| None);

        use super::super::types::ForwardSource;
        let meta_d = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "a.R".to_string(),
                    line: 1,
                    column: 0,
                    ..Default::default()
                },
                ForwardSource {
                    path: "b.R".to_string(),
                    line: 2,
                    column: 0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        graph.update_file(&d, &meta_d, Some(&workspace_root()), |_| None);

        let dependents = graph.get_transitive_dependents(&c, 10, 200);
        // Should include a, b, d — no duplicates
        let unique: std::collections::HashSet<_> = dependents.iter().collect();
        assert_eq!(unique.len(), dependents.len(), "should have no duplicates");
        assert!(dependents.contains(&a));
        assert!(dependents.contains(&b));
        assert!(dependents.contains(&d));
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
    fn test_subgraph_cache_invalidates_on_call_site_change() {
        // Codex follow-up: metadata-only edge changes (e.g. moving a
        // `source()` call to a different line) leave the .to URI set
        // unchanged but mutate `call_site_line` on the edge. The cached
        // subgraph holds full DependencyEdge values that diagnostics
        // consume for positioning, so this must invalidate the caches.
        let mut graph = DependencyGraph::new();
        let parent = url("parent.R");
        let child = url("child.R");

        let meta_v1 = make_meta_with_source("child.R", 5);
        graph.update_file(&parent, &meta_v1, Some(&workspace_root()), |_| None);
        graph.update_file(
            &child,
            &CrossFileMetadata::default(),
            Some(&workspace_root()),
            |_| None,
        );

        let _ = graph.cached_neighborhood_subgraph(&parent, 10, 100);
        let hits_before = graph.subgraph_cache_hits();

        // Same target URI (child.R), different call_site line — must bump.
        let meta_v2 = make_meta_with_source("child.R", 12);
        let result = graph.update_file(&parent, &meta_v2, Some(&workspace_root()), |_| None);
        assert!(
            result.edges_changed,
            "moving source() call to a new line must mark edges_changed"
        );

        let _ = graph.cached_neighborhood_subgraph(&parent, 10, 100);
        assert_eq!(
            graph.subgraph_cache_hits(),
            hits_before,
            "call-site change must invalidate the subgraph cache"
        );
    }

    #[test]
    fn test_subgraph_cache_invalidates_on_backward_edge_change() {
        // S1 + S4: changing a `@lsp-sourced-by` directive must bump
        // `edge_revision` so cycle/subgraph caches don't hand out stale
        // results that omit the new backward edge.
        let mut graph = DependencyGraph::new();
        let parent = Url::parse("file:///project/parent.R").unwrap();
        let child = Url::parse("file:///project/child.R").unwrap();

        // Establish baseline (no backward directive).
        graph.update_file(
            &parent,
            &CrossFileMetadata::default(),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &child,
            &CrossFileMetadata::default(),
            Some(&workspace_root()),
            |_| None,
        );

        let _ = graph.cached_neighborhood_subgraph(&parent, 10, 100);
        let _ = graph.detect_cycle(&parent);
        let hits_before = graph.subgraph_cache_hits();

        // Add a backward directive on child: child is sourced by parent.
        // This rewires backward[child] and forward[parent], but leaves
        // forward[child] unchanged. Without the fix the caches stay valid.
        let child_meta_with_backward = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "parent.R".to_string(),
                call_site: CallSiteSpec::Line(1),
                directive_line: 0,
            }],
            ..Default::default()
        };
        let result = graph.update_file(
            &child,
            &child_meta_with_backward,
            Some(&workspace_root()),
            |_| None,
        );
        assert!(
            result.edges_changed,
            "adding @lsp-sourced-by must mark edges_changed"
        );

        let _ = graph.cached_neighborhood_subgraph(&parent, 10, 100);
        // Cache must have missed (revision bumped) → hits unchanged.
        assert_eq!(
            graph.subgraph_cache_hits(),
            hits_before,
            "backward-edge change must invalidate subgraph cache"
        );
    }

    #[test]
    fn test_remove_file_invalidates_caches() {
        // Codex review: remove_file mutates the graph but previously did not
        // bump edge_revision, leaving cycle_cache / subgraph_cache stale.
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");
        let meta_a = make_meta_with_source("b.R", 1);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);
        graph.update_file(
            &b,
            &CrossFileMetadata::default(),
            Some(&workspace_root()),
            |_| None,
        );

        let _ = graph.cached_neighborhood_subgraph(&a, 10, 100);
        let _ = graph.detect_cycle(&a);
        let subgraph_hits_before = graph.subgraph_cache_hits();
        let cycle_hits_before = graph.cycle_cache_hits();

        graph.remove_file(&b);

        let _ = graph.cached_neighborhood_subgraph(&a, 10, 100);
        let _ = graph.detect_cycle(&a);
        assert_eq!(
            graph.subgraph_cache_hits(),
            subgraph_hits_before,
            "remove_file must invalidate subgraph cache"
        );
        assert_eq!(
            graph.cycle_cache_hits(),
            cycle_hits_before,
            "remove_file must invalidate cycle cache"
        );
    }

    #[test]
    fn test_neighborhood_subgraph_subgraph_is_arc_wrapped() {
        // S2: the cached subgraph must be exposed as an Arc so the
        // diagnostic snapshot can hold a refcount-bumped reference instead
        // of cloning the trimmed DependencyGraph per snapshot.
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let meta_a = make_meta_with_source("b.R", 1);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);

        let payload = graph.cached_neighborhood_subgraph(&a, 10, 100);
        let arc1: Arc<DependencyGraph> = payload.subgraph.clone();
        let arc2 = arc1.clone();
        assert!(
            Arc::ptr_eq(&arc1, &arc2),
            "Arc subgraph clones must share storage"
        );
    }

    #[test]
    fn test_neighborhood_subgraph_caches_until_edges_change() {
        // S1: cache (neighborhood, subgraph) by (root, depth budget,
        // edge revision). Repeated calls with no edge change must hit
        // cache; an edge change must invalidate.
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");

        let meta_a = make_meta_with_source("b.R", 1);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);
        let meta_b = CrossFileMetadata::default();
        graph.update_file(&b, &meta_b, Some(&workspace_root()), |_| None);

        let max_depth = 10;
        let max_visited = 100;

        let result1 = graph.cached_neighborhood_subgraph(&a, max_depth, max_visited);
        let hits_after_first = graph.subgraph_cache_hits();

        let result2 = graph.cached_neighborhood_subgraph(&a, max_depth, max_visited);
        assert_eq!(
            graph.subgraph_cache_hits(),
            hits_after_first + 1,
            "second call at same edge revision must hit cache"
        );
        assert!(
            Arc::ptr_eq(&result1, &result2),
            "cache must return the same Arc allocation"
        );

        let meta_a_no_source = CrossFileMetadata::default();
        let result = graph.update_file(&a, &meta_a_no_source, Some(&workspace_root()), |_| None);
        assert!(result.edges_changed);

        let hits_before_third = graph.subgraph_cache_hits();
        let _ = graph.cached_neighborhood_subgraph(&a, max_depth, max_visited);
        assert_eq!(
            graph.subgraph_cache_hits(),
            hits_before_third,
            "edges_changed must invalidate the subgraph cache and force recompute"
        );
    }

    #[test]
    fn test_detect_cycle_caches_until_edges_change() {
        // S4: detect_cycle results must be cached and keyed by graph edge
        // revision. Repeated calls with no edge change should hit cache;
        // an edge mutation should invalidate.
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");

        let meta_a = make_meta_with_source("b.R", 1);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);
        let meta_b = make_meta_with_source("a.R", 2);
        graph.update_file(&b, &meta_b, Some(&workspace_root()), |_| None);

        // First call computes (cache miss).
        let _ = graph.detect_cycle(&a);
        let hits_after_first = graph.cycle_cache_hits();

        // Second call with no edge change: cache hit.
        let _ = graph.detect_cycle(&a);
        assert_eq!(
            graph.cycle_cache_hits(),
            hits_after_first + 1,
            "second detect_cycle for same URI at same edge revision must hit cache"
        );

        // Mutate edges → cache invalidated for this URI.
        let meta_a_no_source = CrossFileMetadata::default();
        let result = graph.update_file(&a, &meta_a_no_source, Some(&workspace_root()), |_| None);
        assert!(
            result.edges_changed,
            "removing source() must mark edges_changed"
        );

        let hits_before_third = graph.cycle_cache_hits();
        // Third call: edge revision bumped, cache invalidated → recomputes.
        let _ = graph.detect_cycle(&a);
        assert_eq!(
            graph.cycle_cache_hits(),
            hits_before_third,
            "edges_changed must invalidate cycle cache and force a recompute"
        );
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
        let detection = cycle.unwrap();
        // outgoing_edge: a -> b (the edge FROM the queried file)
        assert_eq!(detection.outgoing_edge.from, a);
        assert_eq!(detection.outgoing_edge.to, b);
        assert_eq!(detection.outgoing_edge.call_site_line, Some(1));
        // closing_edge: b -> a (the edge that closes the cycle)
        assert_eq!(detection.closing_edge.from, b);
        assert_eq!(detection.closing_edge.to, a);
        assert_eq!(detection.closing_edge.call_site_line, Some(2));

        // Cycle should also be detected from b
        let cycle_b = graph.detect_cycle(&b);
        assert!(cycle_b.is_some());
        let detection_b = cycle_b.unwrap();
        // outgoing_edge: b -> a (the edge FROM the queried file)
        assert_eq!(detection_b.outgoing_edge.from, b);
        assert_eq!(detection_b.outgoing_edge.to, a);
        // closing_edge: a -> b (the edge that closes the cycle)
        assert_eq!(detection_b.closing_edge.from, a);
        assert_eq!(detection_b.closing_edge.to, b);
    }

    #[test]
    fn test_detect_cycle_three_nodes() {
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");
        let c = url("c.R");

        // a -> b at line 5
        let meta_a = make_meta_with_source("b.R", 5);
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);

        // b -> c at line 10
        let meta_b = make_meta_with_source("c.R", 10);
        graph.update_file(&b, &meta_b, Some(&workspace_root()), |_| None);

        // c -> a at line 15 (closes the cycle)
        let meta_c = make_meta_with_source("a.R", 15);
        graph.update_file(&c, &meta_c, Some(&workspace_root()), |_| None);

        // From a: outgoing is a->b, closing is c->a
        let detection = graph.detect_cycle(&a).expect("should detect cycle from a");
        assert_eq!(detection.outgoing_edge.from, a);
        assert_eq!(detection.outgoing_edge.to, b);
        assert_eq!(detection.outgoing_edge.call_site_line, Some(5));
        assert_eq!(detection.closing_edge.from, c);
        assert_eq!(detection.closing_edge.to, a);
        assert_eq!(detection.closing_edge.call_site_line, Some(15));

        // From b: outgoing is b->c, closing is a->b
        let detection_b = graph.detect_cycle(&b).expect("should detect cycle from b");
        assert_eq!(detection_b.outgoing_edge.from, b);
        assert_eq!(detection_b.outgoing_edge.to, c);
        assert_eq!(detection_b.closing_edge.from, a);
        assert_eq!(detection_b.closing_edge.to, b);
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
    fn test_contains_cycle_acyclic_and_cyclic() {
        // Acyclic chain a -> b -> c: no cycle.
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");
        let c = url("c.R");
        graph.update_file(
            &a,
            &make_meta_with_source("b.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &b,
            &make_meta_with_source("c.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        assert!(!graph.contains_cycle(), "a -> b -> c is acyclic");

        // Close the cycle c -> a. The edge_revision bump must invalidate the
        // cached `false` answer.
        graph.update_file(
            &c,
            &make_meta_with_source("a.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        assert!(
            graph.contains_cycle(),
            "adding c -> a must flip the cached contains_cycle answer"
        );
    }

    #[test]
    fn test_contains_cycle_self_loop() {
        // A file that sources itself is a 1-node cycle.
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        graph.update_file(
            &a,
            &make_meta_with_source("a.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        assert!(graph.contains_cycle(), "self-source is a cycle");
    }

    #[test]
    fn test_contains_cycle_diamond_is_acyclic() {
        // Diamond a -> b, a -> c, b -> d, c -> d: shared descendant, no cycle.
        // Guards against a naive visited-set check mistaking a re-reached node
        // for a back-edge.
        let mut graph = DependencyGraph::new();
        let a = url("a.R");
        let b = url("b.R");
        let c = url("c.R");
        let meta_a = CrossFileMetadata {
            sources: vec![make_source("b.R", 1), make_source("c.R", 2)],
            ..Default::default()
        };
        graph.update_file(&a, &meta_a, Some(&workspace_root()), |_| None);
        graph.update_file(
            &b,
            &make_meta_with_source("d.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &c,
            &make_meta_with_source("d.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        assert!(!graph.contains_cycle(), "diamond DAG is acyclic");
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
    fn test_backward_directives_preserve_distinct_call_sites() {
        use super::super::types::{BackwardDirective, CallSiteSpec};

        let mut graph = DependencyGraph::new();
        let parent = Url::parse("file:///project/parent.R").unwrap();
        let child = Url::parse("file:///project/sub/child.R").unwrap();
        let meta = CrossFileMetadata {
            sourced_by: vec![
                BackwardDirective {
                    path: "../parent.R".to_string(),
                    call_site: CallSiteSpec::Line(10),
                    directive_line: 0,
                },
                BackwardDirective {
                    path: "../parent.R".to_string(),
                    call_site: CallSiteSpec::Line(20),
                    directive_line: 1,
                },
            ],
            ..Default::default()
        };

        graph.update_file(&child, &meta, Some(&workspace_root()), |_| None);

        let deps = graph.get_dependencies(&parent);
        assert_eq!(deps.len(), 2);
        assert_eq!(
            deps.iter()
                .map(|edge| edge.call_site_line)
                .collect::<Vec<_>>(),
            vec![Some(10), Some(20)]
        );
        assert!(
            deps.iter()
                .all(|edge| edge.is_directive && edge.is_backward_directive)
        );
    }

    #[test]
    fn test_backward_directive_case_only_mismatch_forms_edge_to_real_file() {
        // Issue #535: a wrong-cased `# raven: sourced-by Parent.r` (real file
        // Parent.R) must still form the backward edge — to the REAL on-disk spelling
        // (Parent.R), so the edge target matches the workspace index key (#476) and
        // the child inherits the parent's scope instead of cascading
        // undefined-variable warnings. Resolves on a case-insensitive FS via the
        // step-1 correction AND on a case-sensitive FS via the new single-ci-match
        // leniency, so the edge target is Parent.R regardless of host.
        use super::super::types::{BackwardDirective, CallSiteSpec};

        let (temp_dir, workspace_url) = create_temp_workspace(&["Parent.R", "child.R"]);
        let child = temp_url(&temp_dir, "child.R");
        let real_parent = temp_url(&temp_dir, "Parent.R");

        let mut graph = DependencyGraph::new();
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "Parent.r".to_string(), // wrong case
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        graph.update_file(&child, &meta, Some(&workspace_url), |_| None);

        let dependents = graph.get_dependents(&child);
        assert_eq!(
            dependents.len(),
            1,
            "the case-only-mismatched backward directive must still form an edge: {dependents:?}"
        );
        assert_eq!(
            dependents[0].from, real_parent,
            "edge must target the REAL on-disk spelling (Parent.R), not the typed 'Parent.r'"
        );
    }

    #[test]
    fn test_backward_directive_ambiguous_case_does_not_pick_a_real_file() {
        // 2+ case-insensitive matches (only constructible on a case-sensitive FS) →
        // ambiguous → the directive must NOT silently resolve to either real file.
        // Like a missing target, the graph still records an edge to the lexical
        // (non-existent) typed path — `do_resolve_backward` checks existence later,
        // not at edge-creation — but that target must be the typed `PARENT.R`,
        // never `Parent.R` or `parent.R`, so no symbols flow from an
        // arbitrarily-picked file.
        if !host_is_case_sensitive() {
            return;
        }
        use super::super::types::{BackwardDirective, CallSiteSpec};

        let (temp_dir, workspace_url) = create_temp_workspace(&["Parent.R", "parent.R", "child.R"]);
        let child = temp_url(&temp_dir, "child.R");

        let mut graph = DependencyGraph::new();
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "PARENT.R".to_string(), // matches both case-insensitively
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        graph.update_file(&child, &meta, Some(&workspace_url), |_| None);

        // Exactly one edge is recorded, and it targets the lexical (non-existent)
        // typed `PARENT.R` — never `Parent.R` or `parent.R`. Asserting the concrete
        // target (rather than only `!= real_file`) keeps this non-vacuous on a
        // case-sensitive host even if edge-creation behavior changes.
        let dependents = graph.get_dependents(&child);
        assert_eq!(
            dependents.len(),
            1,
            "ambiguous directive still records one edge (to the lexical, non-existent target)"
        );
        let lexical = temp_url(&temp_dir, "PARENT.R");
        assert_eq!(
            dependents[0].from, lexical,
            "edge targets the typed (non-existent) PARENT.R, not an arbitrarily-picked real file"
        );
        assert_ne!(dependents[0].from, temp_url(&temp_dir, "Parent.R"));
        assert_ne!(dependents[0].from, temp_url(&temp_dir, "parent.R"));
    }

    #[cfg(unix)]
    #[test]
    fn source_edge_through_symlink_spelling_is_distinct_from_real_open_uri() {
        // Issue #562: raw URI identity is deliberate. A source edge whose parent
        // URI is under a symlink spelling keeps that spelling in the graph, even
        // when the same helper file is open under the symlink target spelling.
        let (temp_dir, workspace_url) = create_temp_workspace(&["real/main.R", "real/helper.R"]);
        let real_dir = temp_dir.path().join("real");
        let link_dir = temp_dir.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

        let link_main = Url::from_file_path(link_dir.join("main.R")).unwrap();
        let link_helper = Url::from_file_path(link_dir.join("helper.R")).unwrap();
        let real_helper = temp_url(&temp_dir, "real/helper.R");

        assert_eq!(
            std::fs::canonicalize(link_helper.to_file_path().unwrap()).unwrap(),
            std::fs::canonicalize(real_helper.to_file_path().unwrap()).unwrap(),
            "fixture must name the same helper file through both spellings"
        );
        assert_ne!(
            link_helper, real_helper,
            "raw LSP URIs must preserve the symlink spelling difference"
        );

        let mut graph = DependencyGraph::new();
        graph.update_file(
            &link_main,
            &make_meta_with_source("helper.R", 1),
            Some(&workspace_url),
            |_| None,
        );

        let deps = graph.get_dependencies(&link_main);
        assert_eq!(deps.len(), 1);
        assert_eq!(
            deps[0].to, link_helper,
            "source() under the symlink parent must create the symlink-spelled graph node"
        );
        assert_ne!(
            deps[0].to, real_helper,
            "the graph must not canonicalize the edge target to the symlink target"
        );

        assert_eq!(graph.get_dependents(&link_helper).len(), 1);
        assert!(
            graph.get_dependents(&real_helper).is_empty(),
            "an open document under the real URI is not authoritative for the symlink URI"
        );
    }

    #[test]
    fn source_edge_case_spelling_is_distinct_from_open_alias_uri() {
        // Issue #562: case spelling is not an LSP identity alias. This fixture
        // uses ASCII-only case (`Helper.R` vs `helper.r`) because Raven's
        // single-case-insensitive-match leniency is intentionally ASCII-only.
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R", "Helper.R"]);
        let main = temp_url(&temp_dir, "main.R");
        let real_helper = temp_url(&temp_dir, "Helper.R");
        let lower_helper_path = temp_dir.path().join("helper.r");
        let lower_helper = Url::from_file_path(&lower_helper_path).unwrap();

        if host_is_case_sensitive() {
            assert!(
                !lower_helper_path.exists(),
                "case-sensitive hosts should exercise the ASCII single-ci-match leniency"
            );
        } else {
            assert!(
                lower_helper_path.exists(),
                "case-insensitive hosts should make helper.r a true alias of Helper.R"
            );
        }
        assert_ne!(
            real_helper, lower_helper,
            "raw LSP URIs must preserve the case spelling difference"
        );

        let mut graph = DependencyGraph::new();
        graph.update_file(
            &main,
            &make_meta_with_source("helper.r", 1),
            Some(&workspace_url),
            |_| None,
        );

        let deps = graph.get_dependencies(&main);
        assert_eq!(deps.len(), 1);
        assert_eq!(
            deps[0].to, real_helper,
            "source() should still resolve to the on-disk/index spelling"
        );
        assert_ne!(
            deps[0].to, lower_helper,
            "the graph target remains distinct from an open document under the typed case alias"
        );

        assert_eq!(graph.get_dependents(&real_helper).len(), 1);
        assert!(
            graph.get_dependents(&lower_helper).is_empty(),
            "an open document under the case alias URI is not authoritative for the resolved URI"
        );
    }

    #[test]
    fn test_directive_with_call_site_preserves_ast_at_different_site() {
        use super::super::types::ForwardSource;

        // Create temp workspace with actual files
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R", "utils.R"]);
        let main = temp_url(&temp_dir, "main.R");

        let mut graph = DependencyGraph::new();

        // Directive at line 5 with known call site, AST at line 10
        // Per spec: directive with known call site only overrides AST at same call site
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true, // Directive at line 5
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 10,
                    column: 0,
                    is_directive: false, // AST at line 10
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let result = graph.update_file(&main, &meta, Some(&workspace_url), |_| None);

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
                chdir: false,
                is_sys_source: false,
                ..Default::default()
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

        // Both directive and AST at same call site
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: false,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
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

        // Create temp workspace with actual files
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R", "utils.R", "helpers.R"]);
        let main = temp_url(&temp_dir, "main.R");

        let mut graph = DependencyGraph::new();

        // Directive to utils, AST to helpers (different targets)
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                ForwardSource {
                    path: "helpers.R".to_string(),
                    line: 10,
                    column: 0,
                    is_directive: false,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let result = graph.update_file(&main, &meta, Some(&workspace_url), |_| None);

        // Should have both edges (different targets)
        let deps = graph.get_dependencies(&main);
        assert_eq!(deps.len(), 2);
        assert!(result.diagnostics.is_empty());
    }

    /// Test that forward directives create edges optimistically (existence checked later)
    #[test]
    fn test_forward_directive_creates_edge_optimistically() {
        use super::super::types::ForwardSource;

        // Create temp workspace with only main.R (utils.R does NOT exist)
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R"]);
        let main = temp_url(&temp_dir, "main.R");

        let mut graph = DependencyGraph::new();

        // Forward directive to non-existent file
        let meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "utils.R".to_string(),
                line: 5,
                column: 0,
                is_directive: true,
                chdir: false,
                is_sys_source: false,
                ..Default::default()
            }],
            ..Default::default()
        };

        graph.update_file(&main, &meta, Some(&workspace_url), |_| None);

        // Edge is created optimistically; existence is validated during file operations
        let deps = graph.get_dependencies(&main);
        assert_eq!(deps.len(), 1, "Edge should be created optimistically");
    }

    /// Test that AST-detected source() calls still create edges even for non-existent files
    /// (only forward directives skip edge creation for non-existent files)
    #[test]
    fn test_ast_source_nonexistent_file_creates_edge() {
        use super::super::types::ForwardSource;

        // Create temp workspace with only main.R (utils.R does NOT exist)
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R"]);
        let main = temp_url(&temp_dir, "main.R");

        let mut graph = DependencyGraph::new();

        // AST-detected source() call to non-existent file
        let meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "utils.R".to_string(),
                line: 5,
                column: 0,
                is_directive: false, // AST-detected, not directive
                chdir: false,
                is_sys_source: false,
                ..Default::default()
            }],
            ..Default::default()
        };

        graph.update_file(&main, &meta, Some(&workspace_url), |_| None);

        // AST edges are still created even for non-existent files
        // (diagnostics are handled separately in handlers.rs)
        let deps = graph.get_dependencies(&main);
        assert_eq!(
            deps.len(),
            1,
            "AST edge should be created even for non-existent file"
        );
    }

    /// Test that forward directive with existing file creates edge normally
    #[test]
    fn test_forward_directive_existing_file_creates_edge() {
        use super::super::types::ForwardSource;

        // Create temp workspace with both files
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R", "utils.R"]);
        let main = temp_url(&temp_dir, "main.R");
        let utils = temp_url(&temp_dir, "utils.R");

        let mut graph = DependencyGraph::new();

        // Forward directive to existing file
        let meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "utils.R".to_string(),
                line: 5,
                column: 0,
                is_directive: true,
                chdir: false,
                is_sys_source: false,
                ..Default::default()
            }],
            ..Default::default()
        };

        graph.update_file(&main, &meta, Some(&workspace_url), |_| None);

        // Should create edge for existing file
        let deps = graph.get_dependencies(&main);
        assert_eq!(deps.len(), 1, "Edge should be created for existing file");
        assert_eq!(deps[0].to, utils);
        assert!(deps[0].is_directive);
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
            if uri == &parent {
                Some(parent_content.to_string())
            } else {
                None
            }
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
            if uri == &parent {
                Some(parent_content.to_string())
            } else {
                None
            }
        });

        // Should create forward edge from parent to child with inferred call site
        let deps = graph.get_dependencies(&parent);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].call_site_line, Some(2)); // 0-based line 2
        assert!(deps[0].call_site_column.is_some());
    }

    #[test]
    fn test_dump_state() {
        // Create temp workspace with actual files
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R", "utils.R", "helpers.R"]);
        let main = temp_url(&temp_dir, "main.R");

        let mut graph = DependencyGraph::new();

        // Add edges: main sources utils and helpers
        let meta = CrossFileMetadata {
            sources: vec![
                super::super::types::ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 10,
                    is_directive: false,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                super::super::types::ForwardSource {
                    path: "helpers.R".to_string(),
                    line: 10,
                    column: 5,
                    is_directive: true,
                    locality: SourceLocality::CurrentFrame,
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        graph.update_file(&main, &meta, Some(&workspace_url), |_| None);

        // Test dump_state
        let state = graph.dump_state();

        // Verify output contains expected information
        assert!(state.contains("2 total edges"));
        assert!(state.contains("main.R"));
        assert!(state.contains("utils.R"));
        assert!(state.contains("helpers.R"));
        assert!(state.contains("line 5, col 10"));
        assert!(state.contains("line 10, col 5"));
        assert!(state.contains("[directive, local]"));
    }

    // Tests for compute_inherited_working_directory

    #[test]
    fn test_compute_inherited_wd_basic() {
        // Validates: Requirements 1.1, 2.1
        // Child with backward directive inherits parent's explicit WD
        let child_uri = Url::parse("file:///project/src/child.R").unwrap();
        let parent_uri = Url::parse("file:///project/src/parent.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let child_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        let parent_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        let result =
            compute_inherited_working_directory(&child_uri, &child_meta, Some(&workspace), |uri| {
                if uri == &parent_uri {
                    Some(std::sync::Arc::new(parent_meta.clone()))
                } else {
                    None
                }
            });

        assert!(result.is_some());
        let wd = result.unwrap();
        // Should inherit parent's explicit WD
        assert_eq!(wd, "/project/data");
    }

    #[test]
    fn test_compute_inherited_wd_skips_when_explicit() {
        // Validates: Requirement 3.1
        // Child with explicit @lsp-cd should not inherit from parent
        let child_uri = Url::parse("file:///project/src/child.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let child_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: Some("/child/explicit".to_string()), // Has explicit WD
            ..Default::default()
        };

        let result =
            compute_inherited_working_directory(&child_uri, &child_meta, Some(&workspace), |_| {
                panic!("Should not call get_metadata when child has explicit WD")
            });

        assert!(result.is_none());
    }

    #[test]
    fn test_compute_inherited_wd_no_backward_directives() {
        // When child has no backward directives, return None
        let child_uri = Url::parse("file:///project/src/child.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let child_meta = CrossFileMetadata {
            sourced_by: vec![], // No backward directives
            working_directory: None,
            ..Default::default()
        };

        let result =
            compute_inherited_working_directory(&child_uri, &child_meta, Some(&workspace), |_| {
                panic!("Should not call get_metadata when no backward directives")
            });

        assert!(result.is_none());
    }

    #[test]
    fn test_compute_inherited_wd_first_directive_wins() {
        // Validates: Requirement 7.1
        // When multiple backward directives exist, use the first one
        let child_uri = Url::parse("file:///project/src/child.R").unwrap();
        let parent1_uri = Url::parse("file:///project/src/parent1.R").unwrap();
        let parent2_uri = Url::parse("file:///project/src/parent2.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let child_meta = CrossFileMetadata {
            sourced_by: vec![
                BackwardDirective {
                    path: "parent1.R".to_string(),
                    call_site: CallSiteSpec::Default,
                    directive_line: 0,
                },
                BackwardDirective {
                    path: "parent2.R".to_string(),
                    call_site: CallSiteSpec::Default,
                    directive_line: 1,
                },
            ],
            working_directory: None,
            ..Default::default()
        };

        let parent1_meta = CrossFileMetadata {
            working_directory: Some("/first".to_string()),
            ..Default::default()
        };

        let parent2_meta = CrossFileMetadata {
            working_directory: Some("/second".to_string()),
            ..Default::default()
        };

        let result =
            compute_inherited_working_directory(&child_uri, &child_meta, Some(&workspace), |uri| {
                if uri == &parent1_uri {
                    Some(std::sync::Arc::new(parent1_meta.clone()))
                } else if uri == &parent2_uri {
                    Some(std::sync::Arc::new(parent2_meta.clone()))
                } else {
                    None
                }
            });

        assert!(result.is_some());
        let wd = result.unwrap();
        // Should use first parent's WD
        assert_eq!(wd, "/project/first");
    }

    #[test]
    fn test_compute_inherited_wd_parent_implicit() {
        // Validates: Requirement 2.1
        // When parent has no explicit WD, inherit parent's directory
        let child_uri = Url::parse("file:///project/src/child.R").unwrap();
        let parent_uri = Url::parse("file:///project/scripts/parent.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let child_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../scripts/parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        let parent_meta = CrossFileMetadata {
            working_directory: None, // No explicit WD
            ..Default::default()
        };

        let result =
            compute_inherited_working_directory(&child_uri, &child_meta, Some(&workspace), |uri| {
                if uri == &parent_uri {
                    Some(std::sync::Arc::new(parent_meta.clone()))
                } else {
                    None
                }
            });

        assert!(result.is_some());
        let wd = result.unwrap();
        // Should inherit parent's directory
        assert_eq!(wd, "/project/scripts");
    }

    #[test]
    fn test_compute_inherited_wd_parent_not_found() {
        // When parent file cannot be resolved, return None
        let child_uri = Url::parse("file:///project/src/child.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let child_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "nonexistent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // The parent path resolves but metadata is unavailable
        // In this case, resolve_parent_working_directory_with_visited falls back to parent's directory
        let result = compute_inherited_working_directory(
            &child_uri,
            &child_meta,
            Some(&workspace),
            |_| None, // Metadata unavailable
        );

        // Should still return something (fallback to parent's directory)
        assert!(result.is_some());
        let wd = result.unwrap();
        // Parent path "nonexistent.R" resolves to /project/src/nonexistent.R
        // Its directory is /project/src
        assert_eq!(wd, "/project/src");
    }

    // Tests for depth tracking in working directory inheritance
    // Validates: Requirements 9.1, 9.2

    #[test]
    fn test_compute_inherited_wd_with_depth_zero() {
        // Validates: Requirement 9.2
        // When max_depth is 0, inheritance should stop
        let child_uri = Url::parse("file:///project/src/child.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let child_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        let result = compute_inherited_working_directory_with_depth(
            &child_uri,
            &child_meta,
            Some(&workspace),
            |_| panic!("Should not call get_metadata when depth is 0"),
            0, // Zero depth
        );

        assert!(result.is_none());
    }

    #[test]
    fn test_compute_inherited_wd_with_depth_one() {
        // Validates: Requirement 9.2
        // With depth 1, should resolve parent's metadata directly (no further recursion needed)
        let child_uri = Url::parse("file:///project/src/child.R").unwrap();
        let parent_uri = Url::parse("file:///project/src/parent.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let child_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // Parent has explicit working directory (workspace-relative)
        let parent_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        // With depth 2, we have enough depth to resolve parent's metadata
        let result = compute_inherited_working_directory_with_depth(
            &child_uri,
            &child_meta,
            Some(&workspace),
            |uri| {
                if uri == &parent_uri {
                    Some(std::sync::Arc::new(parent_meta.clone()))
                } else {
                    None
                }
            },
            1, // Depth of 1: allows direct parent lookup
        );

        assert!(result.is_some());
        let wd = result.unwrap();
        assert_eq!(wd, "/project/data");
    }

    #[test]
    fn test_transitive_inheritance_a_to_b_to_c() {
        // Validates: Requirement 9.1
        // Chain: A (has @lsp-cd) -> B (inherits from A) -> C (inherits from B, gets A's WD)
        let a_uri = Url::parse("file:///project/a.R").unwrap();
        let b_uri = Url::parse("file:///project/b.R").unwrap();
        let c_uri = Url::parse("file:///project/c.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        // A has explicit working directory (workspace-relative, resolves to /project/data)
        let a_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        // B has backward directive to A, and has inherited WD from A
        // Note: inherited_working_directory stores the RESOLVED absolute path
        // (not the original workspace-relative path)
        let b_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "a.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            // This is stored as a file-relative path that will resolve correctly
            // from B's directory (/project) to /project/data
            inherited_working_directory: Some("data".to_string()),
            ..Default::default()
        };

        // C has backward directive to B
        let c_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "b.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // Compute C's inherited WD - should get A's WD through B
        let result =
            compute_inherited_working_directory(&c_uri, &c_meta, Some(&workspace), |uri| {
                if uri == &a_uri {
                    Some(std::sync::Arc::new(a_meta.clone()))
                } else if uri == &b_uri {
                    Some(std::sync::Arc::new(b_meta.clone()))
                } else {
                    None
                }
            });

        assert!(result.is_some());
        let wd = result.unwrap();
        // C should inherit A's working directory through B
        assert_eq!(wd, "/project/data");
    }

    #[test]
    fn test_transitive_inheritance_depth_limit() {
        // Validates: Requirement 9.2
        // When depth limit is reached, should fall back to parent's directory
        let child_uri = Url::parse("file:///project/src/child.R").unwrap();
        let parent_uri = Url::parse("file:///project/src/parent.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let child_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // Parent has backward directive but we'll hit depth limit
        let parent_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "grandparent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // With depth 1, we can resolve parent but parent's inheritance will be limited
        let result = compute_inherited_working_directory_with_depth(
            &child_uri,
            &child_meta,
            Some(&workspace),
            |uri| {
                if uri == &parent_uri {
                    Some(std::sync::Arc::new(parent_meta.clone()))
                } else {
                    None
                }
            },
            1, // Only depth 1
        );

        // Should still get a result (parent's directory as fallback when depth exhausted)
        assert!(result.is_some());
        let wd = result.unwrap();
        // Parent has no explicit WD and depth is exhausted, so falls back to parent's directory
        assert_eq!(wd, "/project/src");
    }

    #[test]
    fn test_default_max_inheritance_depth_constant() {
        // Verify the default constant is reasonable
        assert_eq!(DEFAULT_MAX_INHERITANCE_DEPTH, 10);
    }

    // Tests for cycle detection in working directory inheritance
    // Validates: Requirement 9.3

    #[test]
    fn test_cycle_detection_simple_a_to_b_to_a() {
        // Validates: Requirement 9.3
        // Cycle: A -> B -> A (A sources B, B sources A via backward directives)
        // When computing A's inherited WD, if we follow A -> B -> A, we should detect the cycle
        let a_uri = Url::parse("file:///project/a.R").unwrap();
        let b_uri = Url::parse("file:///project/b.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        // A has backward directive to B
        let a_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "b.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // B has backward directive to A (creates cycle)
        let b_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "a.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // Compute A's inherited WD - should detect cycle and return None or fallback
        let result =
            compute_inherited_working_directory(&a_uri, &a_meta, Some(&workspace), |uri| {
                if uri == &a_uri {
                    Some(std::sync::Arc::new(a_meta.clone()))
                } else if uri == &b_uri {
                    Some(std::sync::Arc::new(b_meta.clone()))
                } else {
                    None
                }
            });

        // Should get B's directory as the result (B is the parent, and when we try to
        // resolve B's inherited WD, we detect the cycle back to A and fall back to B's directory)
        assert!(result.is_some());
        let wd = result.unwrap();
        // B's directory is /project
        assert_eq!(wd, "/project");
    }

    #[test]
    fn test_cycle_detection_self_reference() {
        // Validates: Requirement 9.3
        // Edge case: A has backward directive to itself
        let a_uri = Url::parse("file:///project/a.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        // A has backward directive to itself (self-cycle)
        let a_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "a.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // Compute A's inherited WD - should detect self-cycle
        let result =
            compute_inherited_working_directory(&a_uri, &a_meta, Some(&workspace), |uri| {
                if uri == &a_uri {
                    Some(std::sync::Arc::new(a_meta.clone()))
                } else {
                    None
                }
            });

        // Should get A's directory as fallback when cycle is detected
        assert!(result.is_some());
        let wd = result.unwrap();
        // A's directory is /project
        assert_eq!(wd, "/project");
    }

    #[test]
    fn test_cycle_detection_three_file_cycle() {
        // Validates: Requirement 9.3
        // Cycle: A -> B -> C -> A
        let a_uri = Url::parse("file:///project/a.R").unwrap();
        let b_uri = Url::parse("file:///project/b.R").unwrap();
        let c_uri = Url::parse("file:///project/c.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        // A has backward directive to B
        let a_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "b.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // B has backward directive to C
        let b_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "c.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // C has backward directive to A (creates cycle)
        let c_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "a.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // Compute A's inherited WD - should detect cycle eventually
        let result =
            compute_inherited_working_directory(&a_uri, &a_meta, Some(&workspace), |uri| {
                if uri == &a_uri {
                    Some(std::sync::Arc::new(a_meta.clone()))
                } else if uri == &b_uri {
                    Some(std::sync::Arc::new(b_meta.clone()))
                } else if uri == &c_uri {
                    Some(std::sync::Arc::new(c_meta.clone()))
                } else {
                    None
                }
            });

        // Should get a result (fallback to some directory when cycle is detected)
        assert!(result.is_some());
    }

    #[test]
    fn test_resolve_parent_wd_with_visited_detects_cycle() {
        // Validates: Requirement 9.3
        // Test the lower-level function directly
        let a_uri = Url::parse("file:///project/a.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let a_meta = CrossFileMetadata {
            working_directory: None,
            ..Default::default()
        };

        // Pre-populate visited set with the URI we're about to resolve
        let mut visited = HashSet::new();
        visited.insert(a_uri.clone());

        let result = resolve_parent_working_directory_with_visited(
            &a_uri,
            &|uri| {
                if uri == &a_uri {
                    Some(std::sync::Arc::new(a_meta.clone()))
                } else {
                    None
                }
            },
            Some(&workspace),
            10, // Plenty of depth
            &mut visited,
        );

        // Should detect cycle and fall back to file's directory
        assert!(result.is_some());
        let wd = result.unwrap();
        assert_eq!(wd, "/project");
    }

    #[test]
    fn implicit_testthat_wd_is_not_inherited_through_sourced_by() {
        let parent_uri = Url::parse("file:///project/tests/testthat/helper-project.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();
        let parent_meta = CrossFileMetadata::default();
        let mut visited = HashSet::new();

        let result = resolve_parent_working_directory_with_visited(
            &parent_uri,
            &|uri| (uri == &parent_uri).then(|| std::sync::Arc::new(parent_meta.clone())),
            Some(&workspace),
            10,
            &mut visited,
        );

        assert_eq!(result, None);
    }

    #[test]
    fn test_compute_inherited_wd_with_visited_detects_cycle() {
        // Validates: Requirement 9.3
        // Test the lower-level function directly
        let a_uri = Url::parse("file:///project/a.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let a_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            working_directory: None,
            ..Default::default()
        };

        // Pre-populate visited set with the URI we're about to compute
        let mut visited = HashSet::new();
        visited.insert(a_uri.clone());

        let result = compute_inherited_working_directory_with_visited(
            &a_uri,
            &a_meta,
            Some(&workspace),
            &|_| None,
            10, // Plenty of depth
            &mut visited,
        );

        // Should detect cycle and return None
        assert!(result.is_none());
    }

    // Tests for directive-vs-AST conflict resolution
    // Validates: Requirements 4.1, 4.2, 4.3, 4.4, 4.5

    /// Test Case 1: Same file, same call site - directive wins
    /// Validates: Requirement 4.3
    #[test]
    fn test_directive_vs_ast_same_call_site_directive_wins() {
        use super::super::types::ForwardSource;

        // Create temp workspace with actual files
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R", "utils.R"]);
        let main = temp_url(&temp_dir, "main.R");

        let mut graph = DependencyGraph::new();

        // Both directive and AST at same line (5) and column (0)
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true, // Directive
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: false, // AST
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let result = graph.update_file(&main, &meta, Some(&workspace_url), |_| None);

        // Should have ONE edge (directive wins at same call site)
        let deps = graph.get_dependencies(&main);
        assert_eq!(
            deps.len(),
            1,
            "Should have exactly one edge when directive and AST at same call site"
        );
        assert!(
            deps[0].is_directive,
            "The edge should be the directive edge"
        );

        // No diagnostics for same call site
        assert!(
            result.diagnostics.is_empty(),
            "No diagnostics for same call site"
        );
    }

    /// Test Case 2: Same file, different call sites - keep both edges
    /// Validates: Requirement 4.4
    #[test]
    fn test_directive_vs_ast_different_call_sites_keep_both() {
        use super::super::types::ForwardSource;

        // Create temp workspace with actual files
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R", "utils.R"]);
        let main = temp_url(&temp_dir, "main.R");

        let mut graph = DependencyGraph::new();

        // Directive at line 5, AST at line 10 (different call sites)
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true, // Directive at line 5
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 10,
                    column: 0,
                    is_directive: false, // AST at line 10
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let result = graph.update_file(&main, &meta, Some(&workspace_url), |_| None);

        // Should have TWO edges (different call sites, keep both)
        let deps = graph.get_dependencies(&main);
        assert_eq!(
            deps.len(),
            2,
            "Should have both edges when at different call sites"
        );

        // No diagnostics for different call sites with explicit line
        assert!(
            result.diagnostics.is_empty(),
            "No diagnostics for different call sites"
        );
    }

    /// Test Case 3: Directive without explicit call site, AST at earlier line - keep AST edge
    /// Validates: Requirement 4.5
    /// Note: This case only applies to backward directives where call site couldn't be determined.
    /// Forward directives always have a call site (their own line or explicit line= parameter).
    #[test]
    fn test_directive_no_call_site_ast_earlier_keeps_ast() {
        use super::super::types::ForwardSource;

        // Create temp workspace with actual files
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R", "utils.R"]);
        let main = temp_url(&temp_dir, "main.R");

        let mut graph = DependencyGraph::new();

        // AST at line 5, directive at line 10 (AST is earlier)
        // Since both have explicit call sites (directive at line 10, AST at line 5),
        // this is Case 2 (different call sites) - both edges are kept
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: false, // AST at line 5 (earlier)
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 10,
                    column: 0,
                    is_directive: true, // Directive at line 10 (later)
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let result = graph.update_file(&main, &meta, Some(&workspace_url), |_| None);

        // Should have TWO edges: both directive and AST are kept because they have different call sites
        // (This is Case 2: different call sites, keep both)
        let deps = graph.get_dependencies(&main);
        assert_eq!(
            deps.len(),
            2,
            "Should keep both edges when at different call sites"
        );

        // No diagnostics because both have explicit call sites
        assert!(
            result.diagnostics.is_empty(),
            "No diagnostics when both have explicit call sites"
        );
    }

    /// Test: Directive without explicit call site, AST at later line - directive wins
    /// This is the case where directive is earlier, so AST should be skipped
    #[test]
    fn test_directive_no_call_site_ast_later_directive_wins() {
        use super::super::types::ForwardSource;

        // Create temp workspace with actual files
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R", "utils.R"]);
        let main = temp_url(&temp_dir, "main.R");

        let mut graph = DependencyGraph::new();

        // Directive at line 5, AST at line 10 (directive is earlier)
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true, // Directive at line 5 (earlier)
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 10,
                    column: 0,
                    is_directive: false, // AST at line 10 (later)
                    chdir: false,
                    is_sys_source: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let result = graph.update_file(&main, &meta, Some(&workspace_url), |_| None);

        // Should have TWO edges because directive has explicit call site (line 5)
        // The directive at line 5 has a known call site, so it only overrides AST at same site
        // AST at line 10 is at different site, so it's kept
        let deps = graph.get_dependencies(&main);
        assert_eq!(
            deps.len(),
            2,
            "Should have both edges when directive has explicit call site"
        );

        // No diagnostics
        assert!(result.diagnostics.is_empty());
    }

    /// Test: Directive edge has is_directive=true and is_backward_directive=false
    /// Validates: Requirements 4.1, 4.2
    #[test]
    fn test_forward_directive_edge_flags() {
        use super::super::types::ForwardSource;

        // Create temp workspace with actual files
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R", "utils.R"]);
        let main = temp_url(&temp_dir, "main.R");

        let mut graph = DependencyGraph::new();

        let meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "utils.R".to_string(),
                line: 5,
                column: 0,
                is_directive: true,
                chdir: false,
                is_sys_source: false,
                ..Default::default()
            }],
            ..Default::default()
        };

        graph.update_file(&main, &meta, Some(&workspace_url), |_| None);

        let deps = graph.get_dependencies(&main);
        assert_eq!(deps.len(), 1);
        assert!(
            deps[0].is_directive,
            "Forward directive edge should have is_directive=true"
        );
        assert!(
            !deps[0].is_backward_directive,
            "Forward directive edge should have is_backward_directive=false"
        );
    }

    #[test]
    fn purpose_specific_edge_identities_do_not_collapse_distinct_projections() {
        let base = DependencyEdge {
            from: url("parent.R"),
            to: url("child.R"),
            call_site_line: Some(3),
            call_site_column: Some(7),
            locality: SourceLocality::Global,
            chdir: false,
            is_sys_source: false,
            is_function_scoped: false,
            is_directive: false,
            is_backward_directive: false,
            non_lending: false,
        };

        let mut other_call_site = base.clone();
        other_call_site.call_site_line = Some(4);
        assert_eq!(
            base.directive_conflict_identity(),
            other_call_site.directive_conflict_identity(),
            "relationship conflict detection intentionally ignores call sites"
        );
        assert_ne!(
            base.graph_dedup_key(),
            other_call_site.graph_dedup_key(),
            "multiple same-parent call sites must remain distinct invocations"
        );

        let mut directive = base.clone();
        directive.is_directive = true;
        assert_eq!(
            base.graph_dedup_key(),
            directive.graph_dedup_key(),
            "directive provenance must not defeat graph deduplication"
        );
        assert_ne!(
            base.dependency_interface_identity(),
            directive.dependency_interface_identity(),
            "directive provenance remains dependency-interface-visible"
        );

        let mut non_lending = base.clone();
        non_lending.non_lending = true;
        assert_eq!(
            base.dependency_interface_identity(),
            non_lending.dependency_interface_identity(),
            "lending policy is not a revalidation edge"
        );
        assert_ne!(
            base.revision_identity(),
            non_lending.revision_identity(),
            "lending policy must invalidate revision-gated caches"
        );

        let mut backward = base.clone();
        backward.is_directive = true;
        backward.is_backward_directive = true;
        assert_ne!(
            base.graph_dedup_key(),
            backward.graph_dedup_key(),
            "backward-directive eligibility must remain part of graph identity"
        );

        let mut different_locality = base.clone();
        different_locality.locality = SourceLocality::CurrentFrame;
        assert_ne!(
            base.source_invocation_identity(),
            different_locality.source_invocation_identity(),
            "inheritance semantics must remain part of invocation identity"
        );
    }

    #[test]
    fn non_lending_marker_does_not_participate_in_edge_identity() {
        let mut graph = DependencyGraph::new();
        let excluded = url("excluded.R");
        let helper = url("helper.R");
        graph.update_file(
            &excluded,
            &make_meta_with_source("helper.R", 1),
            Some(&workspace_root()),
            |_| None,
        );

        let before_interface: HashSet<DependencyInterfaceEdgeIdentity> = graph
            .get_dependencies(&excluded)
            .into_iter()
            .map(DependencyEdge::dependency_interface_identity)
            .collect();
        let before_revision: HashSet<EdgeRevisionIdentity> = graph
            .get_dependencies(&excluded)
            .into_iter()
            .map(DependencyEdge::revision_identity)
            .collect();
        assert!(
            graph
                .get_dependencies(&excluded)
                .iter()
                .all(|edge| !edge.non_lending),
            "precondition: freshly built edges are lending"
        );

        assert!(
            graph.make_forward_edges_non_lending(&excluded),
            "first mark must report a graph change"
        );
        let after_interface: HashSet<DependencyInterfaceEdgeIdentity> = graph
            .get_dependencies(&excluded)
            .into_iter()
            .map(DependencyEdge::dependency_interface_identity)
            .collect();
        let after_revision: HashSet<EdgeRevisionIdentity> = graph
            .get_dependencies(&excluded)
            .into_iter()
            .map(DependencyEdge::revision_identity)
            .collect();
        assert!(
            graph
                .get_dependencies(&excluded)
                .iter()
                .all(|edge| edge.non_lending),
            "edge copies should now be marked non-lending"
        );
        assert_eq!(
            before_interface, after_interface,
            "non_lending must not change the dependency interface"
        );
        assert_ne!(
            before_revision, after_revision,
            "non_lending must participate in revision invalidation"
        );
        let revision_after_first_mark = graph.edge_revision();
        assert!(
            !graph.make_forward_edges_non_lending(&excluded),
            "marking an already non-lending edge should not bump the revision"
        );
        assert_eq!(
            graph.edge_revision(),
            revision_after_first_mark,
            "a redundant non-lending mark must not bump edge_revision"
        );
        assert_eq!(
            graph.get_dependencies(&excluded)[0].to,
            helper,
            "the forward edge must remain present"
        );
    }

    #[test]
    fn non_lending_edges_revalidate_in_full_graph_but_not_trimmed_ancestors() {
        let mut graph = DependencyGraph::new();
        let excluded = url("excluded.R");
        let helper = url("helper.R");
        graph.update_file(
            &excluded,
            &make_meta_with_source("helper.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        assert!(graph.make_forward_edges_non_lending(&excluded));

        let full_dependents = graph.get_transitive_dependents(&helper, 10, 200);
        assert!(
            full_dependents.contains(&excluded),
            "the full graph must retain the reverse edge so helper edits revalidate the excluded consumer"
        );
        assert!(
            graph
                .get_dependencies(&excluded)
                .iter()
                .any(|edge| edge.to == helper && edge.non_lending),
            "forward copy should be retained and marked"
        );
        assert!(
            graph
                .get_dependents(&helper)
                .iter()
                .any(|edge| edge.from == excluded && edge.non_lending),
            "backward copy should be retained and marked"
        );

        let uris = HashSet::from([excluded.clone(), helper.clone()]);
        let trimmed = graph.extract_subgraph(&uris);
        assert!(
            trimmed
                .get_dependencies(&excluded)
                .iter()
                .any(|edge| edge.to == helper && edge.non_lending),
            "trimmed forward index keeps the excluded buffer's consumed helper"
        );
        assert!(
            trimmed.get_dependents(&helper).is_empty(),
            "trimmed backward index drops non-lending parents"
        );
        assert!(
            !trimmed
                .get_transitive_dependents(&helper, 10, 200)
                .contains(&excluded),
            "the helper's trimmed ancestors must not include the excluded parent"
        );
    }

    #[test]
    fn non_lending_to_lending_transition_invalidates_cached_trimmed_subgraph() {
        let mut graph = DependencyGraph::new();
        let excluded = url("excluded.R");
        let helper = url("helper.R");
        let meta = make_meta_with_source("helper.R", 1);
        graph.update_file(&excluded, &meta, Some(&workspace_root()), |_| None);
        assert!(graph.make_forward_edges_non_lending(&excluded));
        let revision_while_excluded = graph.edge_revision();

        let cached = graph.cached_neighborhood_subgraph(&helper, 10, 100);
        assert!(
            cached.subgraph.get_dependents(&helper).is_empty(),
            "precondition: a trimmed snapshot built while excluded must drop the non-lending parent"
        );
        let cache_hits_before = graph.subgraph_cache_hits();

        let result = graph.update_file(&excluded, &meta, Some(&workspace_root()), |_| None);
        assert!(
            !result.edges_changed,
            "non_lending is not part of dependent-revalidation edge identity"
        );
        assert!(
            graph.edge_revision() > revision_while_excluded,
            "flipping an existing edge back to lending must invalidate revision-gated caches"
        );

        let refreshed = graph.cached_neighborhood_subgraph(&helper, 10, 100);
        assert_eq!(
            graph.subgraph_cache_hits(),
            cache_hits_before,
            "edge_revision bump must prevent reuse of the stale trimmed snapshot"
        );
        assert!(
            refreshed
                .subgraph
                .get_dependents(&helper)
                .iter()
                .any(|edge| edge.from == excluded && !edge.non_lending),
            "after un-exclusion, the helper's trimmed snapshot must see the parent as lending"
        );
    }

    /// Test: Backward directive without call site (inference failed), AST at earlier line
    /// This tests Requirement 4.5 - when backward directive has no call site and AST is earlier,
    /// keep the AST edge and emit redundancy hint.
    /// Validates: Requirement 4.5
    #[test]
    fn test_backward_directive_no_call_site_ast_earlier() {
        use super::super::types::{BackwardDirective, ForwardSource};

        // Create temp workspace with actual files
        let (temp_dir, workspace_url) = create_temp_workspace(&["main.R", "sub/child.R"]);
        let child = temp_url(&temp_dir, "sub/child.R");
        let main = temp_url(&temp_dir, "main.R");

        let mut graph = DependencyGraph::new();

        // Child has backward directive to main (no call site - inference will fail)
        // and main has AST source() to child at line 5
        // The backward directive creates edge main->child with no call site
        // The AST source in main creates edge main->child at line 5
        // Since backward directive has no call site and AST is at line 5,
        // AST should be kept and redundancy hint emitted
        let child_meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../main.R".to_string(),
                call_site: CallSiteSpec::Default, // No explicit call site
                directive_line: 0,
            }],
            ..Default::default()
        };

        // Update child first - this creates edge from main->child with no call site
        // (inference will fail because we don't provide parent content)
        graph.update_file(&child, &child_meta, Some(&workspace_url), |_| None);

        // Now update main with AST source to child
        let main_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "sub/child.R".to_string(),
                line: 5,
                column: 0,
                is_directive: false, // AST source
                chdir: false,
                is_sys_source: false,
                ..Default::default()
            }],
            ..Default::default()
        };

        let _result = graph.update_file(&main, &main_meta, Some(&workspace_url), |_| None);

        // Should have edges from main to child
        let deps = graph.get_dependencies(&main);

        // The backward directive edge (no call site) and AST edge (line 5) should both exist
        // because they're processed separately (backward directive in child, AST in main)
        // The conflict resolution only applies when both are in the same file's metadata
        assert!(
            !deps.is_empty(),
            "Should have at least one edge from main to child"
        );

        // In this case, since the backward directive was processed when updating child,
        // and the AST was processed when updating main, they don't conflict directly.
        // The AST edge should be created normally.
        let ast_edge = deps.iter().find(|e| !e.is_directive);
        assert!(ast_edge.is_some(), "AST edge should exist");
        assert_eq!(ast_edge.unwrap().call_site_line, Some(5));
    }

    // --- revalidation_consistent_set: shared directed-inverse primitive ---

    fn multi_source_meta(paths: &[&str]) -> CrossFileMetadata {
        use super::super::types::ForwardSource;
        CrossFileMetadata {
            sources: paths
                .iter()
                .enumerate()
                .map(|(i, p)| ForwardSource {
                    path: p.to_string(),
                    line: (i as u32) + 1,
                    column: 0,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    /// Build the sibling-subtree topology: A sources B and C; B sources D.
    /// (Plus a diamond root P that sources A, so A has a backward ancestor.)
    ///
    /// Edges (parent → child):
    ///   P → A,  A → B,  A → C,  B → D
    fn sibling_subtree_graph() -> DependencyGraph {
        let mut graph = DependencyGraph::new();
        graph.update_file(
            &url("P.R"),
            &make_meta_with_source("A.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &url("A.R"),
            &multi_source_meta(&["B.R", "C.R"]),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &url("B.R"),
            &make_meta_with_source("D.R", 1),
            Some(&workspace_root()),
            |_| None,
        );
        graph
    }

    /// The historical *collection* construction, reproduced inline from the
    /// pre-refactor `collect_cross_file_nse` body (ancestors ∪ descendants of
    /// `once(root).chain(ancestors)`, then drop self, sort, dedup).
    fn old_collection_members(
        graph: &DependencyGraph,
        root: &Url,
        max_depth: usize,
        max_visited: usize,
    ) -> Vec<Url> {
        let ancestors = graph.get_transitive_dependents(root, max_depth, max_visited);
        let descendants = graph.get_transitive_dependencies_multi_root(
            std::iter::once(root).chain(ancestors.iter()),
            max_depth,
            max_visited,
        );
        let mut members: Vec<Url> = ancestors
            .into_iter()
            .chain(descendants)
            .filter(|u| u != root)
            .collect();
        members.sort();
        members.dedup();
        members
    }

    /// The historical *revalidation* construction, reproduced inline from the
    /// pre-refactor `compute_affected_dependents_after_edit` body (two loops
    /// folded through a shared `seen` set, excluding `root` and applying an
    /// `is_open` predicate).
    fn old_revalidation_members<F: Fn(&Url) -> bool>(
        graph: &DependencyGraph,
        root: &Url,
        is_open: F,
        max_depth: usize,
        max_visited: usize,
    ) -> Vec<Url> {
        let mut seen: HashSet<Url> = HashSet::new();
        let mut result: Vec<Url> = Vec::new();
        let push_if_new = |dep: Url, seen: &mut HashSet<Url>, result: &mut Vec<Url>| {
            if dep == *root || !is_open(&dep) {
                return;
            }
            if seen.insert(dep.clone()) {
                result.push(dep);
            }
        };
        let backward = graph.get_transitive_dependents(root, max_depth, max_visited);
        for dep in &backward {
            push_if_new(dep.clone(), &mut seen, &mut result);
        }
        let forward_roots = std::iter::once(root).chain(backward.iter());
        for dep in
            graph.get_transitive_dependencies_multi_root(forward_roots, max_depth, max_visited)
        {
            push_if_new(dep, &mut seen, &mut result);
        }
        result
    }

    /// The shared helper, post-processed exactly as `collect_cross_file_nse`
    /// does (drop self, sort, dedup) — must equal the old collection body for
    /// every node in the graph.
    #[test]
    fn test_revalidation_consistent_set_reproduces_old_collection() {
        let graph = sibling_subtree_graph();
        for name in ["P.R", "A.R", "B.R", "C.R", "D.R", "absent.R"] {
            let root = url(name);
            let mut from_helper: Vec<Url> = graph
                .revalidation_consistent_set(&root, 20, 200)
                .filter(|u| *u != root)
                .collect();
            from_helper.sort();
            from_helper.dedup();
            assert_eq!(
                from_helper,
                old_collection_members(&graph, &root, 20, 200),
                "collection construction drifted for root {name}"
            );
        }
    }

    /// The shared helper, folded exactly as `compute_affected_dependents_after_edit`
    /// does (single `seen` set + `is_open` predicate, excluding self) — must
    /// equal the old two-loop revalidation body for every node in the graph,
    /// preserving first-seen ORDER (not just set membership).
    #[test]
    fn test_revalidation_consistent_set_reproduces_old_revalidation() {
        let graph = sibling_subtree_graph();
        // Try both "everything open" and a partial-open predicate to exercise
        // the `is_open` filter alongside the shared traversal.
        let all_open = |_: &Url| true;
        let some_closed = |u: &Url| u != &url("C.R") && u != &url("P.R");
        for name in ["P.R", "A.R", "B.R", "C.R", "D.R"] {
            let root = url(name);

            let mut from_helper: Vec<Url> = Vec::new();
            let mut seen: HashSet<Url> = HashSet::new();
            for dep in graph.revalidation_consistent_set(&root, 20, 200) {
                if dep == root || !all_open(&dep) {
                    continue;
                }
                if seen.insert(dep.clone()) {
                    from_helper.push(dep);
                }
            }
            assert_eq!(
                from_helper,
                old_revalidation_members(&graph, &root, all_open, 20, 200),
                "revalidation construction (all open) drifted for root {name}"
            );

            let mut from_helper_partial: Vec<Url> = Vec::new();
            let mut seen_partial: HashSet<Url> = HashSet::new();
            for dep in graph.revalidation_consistent_set(&root, 20, 200) {
                if dep == root || !some_closed(&dep) {
                    continue;
                }
                if seen_partial.insert(dep.clone()) {
                    from_helper_partial.push(dep);
                }
            }
            assert_eq!(
                from_helper_partial,
                old_revalidation_members(&graph, &root, some_closed, 20, 200),
                "revalidation construction (partial open) drifted for root {name}"
            );
        }
    }

    /// The load-bearing correctness invariant, asserted structurally: for every
    /// ordered pair (member, Q) of distinct nodes,
    ///
    ///   member ∈ consistent_set(Q)  ⟺  Q ∈ affected_dependents(member)
    ///
    /// where the LHS is `collect_cross_file_nse`'s membership test (helper,
    /// drop-self) and the RHS is `compute_affected_dependents_after_edit`'s
    /// result (helper, dedup, drop-self). Both sides now derive from
    /// `revalidation_consistent_set` over the SAME graph with the SAME budgets,
    /// so the shared traversal shape makes this hold here; the test guards
    /// against a future edit that re-splits them. (In production the two run
    /// over different graphs — trimmed vs full — a deliberate, safe-direction
    /// asymmetry; see the helper's doc.) Includes the
    /// sibling-subtree case (A sources B and C; B sources D): D ∈ S(C) ⟺ C
    /// revalidates D — i.e. editing C must republish its sibling-subtree node D.
    #[test]
    fn test_directed_inverse_property_holds_structurally() {
        let graph = sibling_subtree_graph();
        let nodes: Vec<Url> = ["P.R", "A.R", "B.R", "C.R", "D.R"]
            .iter()
            .map(|n| url(n))
            .collect();

        // Collection-side membership: member ∈ S(Q)?
        let in_consistent_set = |q: &Url, member: &Url| -> bool {
            graph
                .revalidation_consistent_set(q, 20, 200)
                .any(|u| &u == member && member != q)
        };
        // Revalidation-side: does editing `member` revalidate `q`? (all open)
        let revalidates = |member: &Url, q: &Url| -> bool {
            let mut seen: HashSet<Url> = HashSet::new();
            graph
                .revalidation_consistent_set(member, 20, 200)
                .filter(|d| d != member)
                .filter(|d| seen.insert(d.clone()))
                .any(|d| &d == q)
        };

        for q in &nodes {
            for member in &nodes {
                if q == member {
                    continue;
                }
                assert_eq!(
                    in_consistent_set(q, member),
                    revalidates(member, q),
                    "directed-inverse violated: member={member} q={q} \
                     (member∈S(q)={}, q∈affected(member)={})",
                    in_consistent_set(q, member),
                    revalidates(member, q),
                );
            }
        }

        // Spot-check the sibling-subtree expectation is actually exercised
        // (guards against a degenerate graph silently passing the loop above):
        // C and D are siblings-subtree-related through their shared ancestor A,
        // so editing C revalidates D and D ∈ S(C).
        assert!(
            revalidates(&url("C.R"), &url("D.R")),
            "sibling-subtree: editing C must revalidate D"
        );
        assert!(
            in_consistent_set(&url("C.R"), &url("D.R")),
            "sibling-subtree: D must be in the consistent set of C"
        );
    }

    /// Exact bounded-traversal counterexample: A→Q, A→D, D→X with budget 3.
    /// Q's trimmed query can reach D, while the full inverse walk rooted at D
    /// spends its budget on X and A before reaching Q.
    #[test]
    fn budget_three_neighborhood_reports_directed_inverse_truncation() {
        let mut graph = DependencyGraph::new();
        graph.update_file(
            &url("A.R"),
            &multi_source_meta(&["Q.R", "D.R"]),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &url("D.R"),
            &make_meta_with_source("X.R", 1),
            Some(&workspace_root()),
            |_| None,
        );

        let payload = graph.cached_neighborhood_subgraph(&url("Q.R"), 20, 3);
        assert_eq!(
            payload.truncation,
            NeighborhoodTruncation {
                depth: false,
                visited: true,
            }
        );
        assert!(payload.neighborhood.contains(&url("D.R")));
        assert!(
            payload
                .subgraph
                .revalidation_consistent_set(&url("Q.R"), 20, 3)
                .any(|member| member == url("D.R")),
            "the truncated query-side set reproduces the unsafe foreign member"
        );
        assert!(
            !graph
                .revalidation_consistent_set(&url("D.R"), 20, 3)
                .any(|dependent| dependent == url("Q.R")),
            "editing D does not revalidate Q under the same budget"
        );
    }

    #[test]
    fn neighborhood_reports_depth_truncation_separately() {
        let mut graph = DependencyGraph::new();
        graph.update_file(
            &url("A.R"),
            &multi_source_meta(&["Q.R", "D.R"]),
            Some(&workspace_root()),
            |_| None,
        );
        graph.update_file(
            &url("D.R"),
            &make_meta_with_source("X.R", 1),
            Some(&workspace_root()),
            |_| None,
        );

        let payload = graph.cached_neighborhood_subgraph(&url("Q.R"), 2, 100);
        assert_eq!(
            payload.truncation,
            NeighborhoodTruncation {
                depth: true,
                visited: false,
            }
        );
        assert!(payload.neighborhood.contains(&url("D.R")));
        assert!(!payload.neighborhood.contains(&url("X.R")));
    }
}
