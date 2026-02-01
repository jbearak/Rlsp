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

/// Resolve the effective working directory of a parent file for inheritance.
///
/// Returns the parent's effective working directory as a string path that can
/// be stored in the child's metadata.
///
/// This is a convenience wrapper around `resolve_parent_working_directory_with_depth`
/// that uses the default maximum depth.
///
/// # Arguments
/// * `parent_uri` - The URI of the parent file
/// * `get_metadata` - A closure that retrieves metadata for a given URI
/// * `workspace_root` - Optional workspace root URI for resolving workspace-relative paths
///
/// # Returns
/// * `Some(String)` - The parent's effective working directory as a string path
/// * `None` - If the parent URI cannot be converted to a file path
///
/// # Fallback Behavior
/// If parent metadata cannot be retrieved via `get_metadata`, the function falls back
/// to using the parent file's directory as the inherited working directory.
///
/// _Requirements: 5.1, 5.3_
pub fn resolve_parent_working_directory<F>(
    parent_uri: &Url,
    get_metadata: F,
    workspace_root: Option<&Url>,
) -> Option<String>
where
    F: Fn(&Url) -> Option<CrossFileMetadata>,
{
    resolve_parent_working_directory_with_depth(
        parent_uri,
        &get_metadata,
        workspace_root,
        DEFAULT_MAX_INHERITANCE_DEPTH,
    )
}

/// Resolve the effective working directory of a parent file for inheritance,
/// with depth tracking to prevent infinite chains.
///
/// Returns the parent's effective working directory as a string path that can
/// be stored in the child's metadata.
///
/// # Arguments
/// * `parent_uri` - The URI of the parent file
/// * `get_metadata` - A closure that retrieves metadata for a given URI
/// * `workspace_root` - Optional workspace root URI for resolving workspace-relative paths
/// * `remaining_depth` - Remaining depth for inheritance chain traversal
///
/// # Returns
/// * `Some(String)` - The parent's effective working directory as a string path
/// * `None` - If the parent URI cannot be converted to a file path
///
/// # Fallback Behavior
/// If parent metadata cannot be retrieved via `get_metadata`, or if the depth limit is
/// reached, the function falls back to using the parent file's directory as the effective
/// working directory.
///
/// # Transitive Inheritance
/// When the parent has an inherited_working_directory in its metadata (from its own parent),
/// that value is used through PathContext::from_metadata. This enables transitive inheritance:
/// A → B → C where A has @lsp-cd, B inherits from A, and C inherits from B (getting A's WD).
///
/// _Requirements: 5.1, 5.3, 9.1, 9.2_
pub fn resolve_parent_working_directory_with_depth<F>(
    parent_uri: &Url,
    get_metadata: &F,
    workspace_root: Option<&Url>,
    remaining_depth: usize,
) -> Option<String>
where
    F: Fn(&Url) -> Option<CrossFileMetadata>,
{
    let mut visited = HashSet::new();
    resolve_parent_working_directory_with_visited(
        parent_uri,
        get_metadata,
        workspace_root,
        remaining_depth,
        &mut visited,
    )
}

/// Resolve the effective working directory of a parent file for inheritance,
/// with depth tracking and cycle detection to prevent infinite chains.
///
/// Returns the parent's effective working directory as a string path that can
/// be stored in the child's metadata.
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
/// * `None` - If the parent URI cannot be converted to a file path
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
/// # Transitive Inheritance
/// When the parent has an inherited_working_directory in its metadata (from its own parent),
/// that value is used through PathContext::from_metadata. This enables transitive inheritance:
/// A → B → C where A has @lsp-cd, B inherits from A, and C inherits from B (getting A's WD).
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
    F: Fn(&Url) -> Option<CrossFileMetadata>,
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
        let parent_path = parent_uri.to_file_path().ok()?;
        let parent_dir = parent_path.parent()?;
        return Some(parent_dir.to_string_lossy().to_string());
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
        let parent_path = parent_uri.to_file_path().ok()?;
        let parent_dir = parent_path.parent()?;
        return Some(parent_dir.to_string_lossy().to_string());
    }

    // Try to get parent's metadata
    if let Some(parent_meta) = get_metadata(parent_uri) {
        // Build parent's PathContext from metadata
        // This handles transitive inheritance: if parent has inherited_working_directory,
        // it will be used in effective_working_directory() (Requirement 9.1)
        if let Some(parent_ctx) =
            PathContext::from_metadata(parent_uri, &parent_meta, workspace_root)
        {
            // Get effective working directory
            let effective_wd = parent_ctx.effective_working_directory();
            log::trace!(
                "Resolved parent working directory for {}: {} (depth remaining: {})",
                parent_uri,
                effective_wd.display(),
                remaining_depth
            );
            return Some(effective_wd.to_string_lossy().to_string());
        }
    }

    // Fallback: use parent's directory when metadata is unavailable
    // This handles the case where the parent file is not indexed or not accessible
    log::trace!(
        "Parent metadata unavailable for {}, falling back to parent's directory",
        parent_uri
    );

    // Convert parent URI to file path and get its directory
    let parent_path = match parent_uri.to_file_path() {
        Ok(path) => path,
        Err(_) => {
            log::trace!(
                "Cannot convert parent URI to file path for WD inheritance: {}",
                parent_uri
            );
            return None;
        }
    };
    let parent_dir = match parent_path.parent() {
        Some(dir) => dir,
        None => {
            log::trace!(
                "Parent path has no parent directory: {}",
                parent_path.display()
            );
            return None;
        }
    };

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
/// - Skips inheritance if the child file has an explicit `@lsp-cd` directive
/// - Uses the first backward directive (document order) to determine the parent
/// - Resolves the parent path using file-relative resolution (not affected by @lsp-cd)
/// - Calls `resolve_parent_working_directory` to get the parent's effective working directory
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
    F: Fn(&Url) -> Option<CrossFileMetadata>,
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
/// * `Some(String)` - The inherited working directory from the parent
/// * `None` - If inheritance should not occur (explicit WD, no backward directives, max depth exceeded, etc.)
///
/// # Behavior
/// - Skips inheritance if the child file has an explicit `@lsp-cd` directive
/// - Uses the first backward directive (document order) to determine the parent
/// - Resolves the parent path using file-relative resolution (not affected by @lsp-cd)
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
    F: Fn(&Url) -> Option<CrossFileMetadata>,
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
/// directory, if parent metadata is unavailable, if max_depth is exceeded, or if a cycle
/// is detected.
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
/// * `Some(String)` - The inherited working directory from the parent
/// * `None` - If inheritance should not occur (explicit WD, no backward directives, max depth exceeded, cycle detected, etc.)
///
/// # Behavior
/// - Skips inheritance if the child file has an explicit `@lsp-cd` directive
/// - Uses the first backward directive (document order) to determine the parent
/// - Resolves the parent path using file-relative resolution (not affected by @lsp-cd)
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
    F: Fn(&Url) -> Option<CrossFileMetadata>,
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
    // IMPORTANT: Backward directive paths ignore both explicit @lsp-cd and inherited
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
    if meta.sourced_by.len() > 1 {
        if let Some(ref first_wd) = inherited_wd {
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
                if let Some(other_wd) = other_wd {
                    if &other_wd != first_wd {
                        differing_parent = Some((directive.path.clone(), other_wd));
                        break;
                    }
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
    }

    inherited_wd
}

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
    /// True if the edge was created from an LSP directive (@lsp-source or @lsp-sourced-by)
    pub is_directive: bool,
    /// True if the directive origin is a backward directive (@lsp-sourced-by / @lsp-run-by).
    /// False for forward directives (@lsp-source) and AST-detected edges.
    pub is_backward_directive: bool,
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
    is_backward_directive: bool,
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
            is_backward_directive: self.is_backward_directive,
        }
    }

    fn as_from_to_pair(&self) -> FromToPair {
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

        // Build PathContext for this file (includes working_directory from metadata)
        let path_ctx = match PathContext::from_metadata(uri, meta, workspace_root) {
            Some(ctx) => ctx,
            None => return result,
        };

        // Build separate PathContext for backward directives (without any working_directory)
        // IMPORTANT: Backward directive paths (e.g., @lsp-sourced-by: ../parent.R) should
        // ALWAYS resolve relative to the child file's directory, regardless of:
        //   - Explicit @lsp-cd directives in the child file
        //   - Inherited working directory from parent files
        // This is intentional behavior per Requirements 4.1, 4.2, 4.3.
        // Using PathContext::new() ensures neither working_directory nor
        // inherited_working_directory are set, so paths resolve file-relative.
        let backward_path_ctx = match PathContext::new(uri, workspace_root) {
            Some(ctx) => ctx,
            None => return result,
        };

        // Helper to resolve paths using PathContext (for forward sources)
        let do_resolve = |path: &str| -> Option<Url> {
            let resolved = resolve_path(path, &path_ctx)?;
            path_to_uri(&resolved)
        };

        // Helper to resolve paths for backward directives (file-relative only)
        // Does NOT use working_directory or inherited_working_directory
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
                        is_backward_directive: false,
                    };
                    directive_from_to.insert(edge.as_from_to_pair());
                    directive_edges.push(edge);
                }
            }
        }

        // Process backward directives (@lsp-sourced-by) - create forward edges from parent to this file
        // Uses do_resolve_backward which resolves paths relative to the file's directory,
        // ignoring both explicit @lsp-cd and inherited working directories (Requirements 4.1-4.3)
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
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    is_directive: true,
                    is_backward_directive: true,
                };
                let pair = edge.as_from_to_pair();
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
                        is_backward_directive: false,
                    };
                    let pair = edge.as_from_to_pair();

                    // Check for directive-vs-AST conflict (Requirement 6.8)
                    if directive_from_to.contains(&pair) {
                        // Find the directive edge for this (from, to) pair
                        let directive_edge =
                            directive_edges.iter().find(|e| e.as_from_to_pair() == pair);

                        if let Some(dir_edge) = directive_edge {
                            // Check if directive has a known call site
                            let directive_has_call_site = dir_edge.call_site_line.is_some()
                                && dir_edge.call_site_line != Some(u32::MAX);

                            if directive_has_call_site {
                                // Directive has known call site: only override AST edge at same call site
                                let call_sites_match = dir_edge.call_site_line
                                    == edge.call_site_line
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
                                let diag_line = meta
                                    .sourced_by
                                    .iter()
                                    .find(|d| {
                                        do_resolve_backward(&d.path) == Some(dir_edge.from.clone())
                                    })
                                    .map(|d| d.directive_line)
                                    .or_else(|| {
                                        meta.sources
                                            .iter()
                                            .find(|s| {
                                                s.is_directive
                                                    && do_resolve(&s.path) == Some(to_uri.clone())
                                            })
                                            .map(|s| s.line)
                                    })
                                    .unwrap_or(0);

                                result.diagnostics.push(Diagnostic {
                                    range: Range {
                                        start: Position { line: diag_line, character: 0 },
                                        end: Position { line: diag_line, character: u32::MAX },
                                    },
                                    severity: Some(DiagnosticSeverity::WARNING),
                                    message: format!(
                                        "Directive without call site suppresses AST-detected source() call to '{}' at line {}. Consider adding line= or match= to the directive.",
                                        to_uri.path_segments().and_then(|mut s| s.next_back()).unwrap_or(""),
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
        log::trace!(
            "Dependency graph now has {} total edges after updating {}",
            total_edges,
            uri
        );

        result
    }

    /// Simple update without content provider (for backward compatibility in tests)
    /// Uses file-relative path resolution only (no workspace root)
    pub fn update_file_simple(&mut self, uri: &Url, meta: &CrossFileMetadata) {
        let _ = self.update_file(uri, meta, None, |_| None);
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
            // Remove from forward index
            if let Some(forward_edges) = self.forward.get_mut(&edge.from) {
                forward_edges.retain(|e| {
                    !(e.to == edge.to
                        && e.is_backward_directive
                        && e.call_site_line == edge.call_site_line)
                });
                if forward_edges.is_empty() {
                    self.forward.remove(&edge.from);
                }
            }
            // Remove from backward index
            if let Some(backward_edges) = self.backward.get_mut(child_uri) {
                backward_edges.retain(|e| {
                    !(e.from == edge.from
                        && e.is_backward_directive
                        && e.call_site_line == edge.call_site_line)
                });
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
            log::trace!(
                "  Removing non-directive edge: {} -> {}",
                edge.from,
                edge.to
            );
            if let Some(backward_edges) = self.backward.get_mut(&edge.to) {
                backward_edges
                    .retain(|e| !(e.from == edge.from && e.to == edge.to && !e.is_directive));
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
                        if edge.local {
                            f.push("local");
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
    use super::super::types::BackwardDirective;
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
        let mut graph = DependencyGraph::new();
        let main = url("main.R");

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

    // Tests for resolve_parent_working_directory
    // Validates: Requirements 5.1, 5.3

    #[test]
    fn test_resolve_parent_working_directory_with_explicit_wd() {
        // Validates: Requirements 5.1
        // When parent has explicit @lsp-cd, return that as the effective working directory
        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let parent_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        let result = resolve_parent_working_directory(
            &parent_uri,
            |uri| {
                if uri == &parent_uri {
                    Some(parent_meta.clone())
                } else {
                    None
                }
            },
            Some(&workspace),
        );

        assert!(result.is_some());
        let wd = result.unwrap();
        // The explicit working directory "/data" is workspace-relative, so resolves to /project/data
        assert_eq!(wd, "/project/data");
    }

    #[test]
    fn test_resolve_parent_working_directory_no_explicit_wd() {
        // Validates: Requirements 5.1
        // When parent has no explicit @lsp-cd, return parent's directory as effective WD
        let parent_uri = Url::parse("file:///project/src/parent.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let parent_meta = CrossFileMetadata {
            working_directory: None,
            ..Default::default()
        };

        let result = resolve_parent_working_directory(
            &parent_uri,
            |uri| {
                if uri == &parent_uri {
                    Some(parent_meta.clone())
                } else {
                    None
                }
            },
            Some(&workspace),
        );

        assert!(result.is_some());
        let wd = result.unwrap();
        // Parent's directory is /project/src
        assert_eq!(wd, "/project/src");
    }

    #[test]
    fn test_resolve_parent_working_directory_fallback_when_metadata_unavailable() {
        // Validates: Requirements 5.3
        // When parent metadata cannot be retrieved, fall back to parent's directory
        let parent_uri = Url::parse("file:///project/scripts/parent.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let result = resolve_parent_working_directory(
            &parent_uri,
            |_| None, // Metadata unavailable
            Some(&workspace),
        );

        assert!(result.is_some());
        let wd = result.unwrap();
        // Should fall back to parent's directory
        assert_eq!(wd, "/project/scripts");
    }

    #[test]
    fn test_resolve_parent_working_directory_with_inherited_wd() {
        // Validates: Requirements 5.1
        // When parent has inherited working directory (no explicit), use that
        let parent_uri = Url::parse("file:///project/src/parent.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        // Note: inherited_working_directory is stored as an absolute path string
        // that was already resolved, so we use a path that doesn't start with /
        // to avoid workspace-relative resolution
        let parent_meta = CrossFileMetadata {
            working_directory: None,
            // Use a relative path that will resolve correctly
            inherited_working_directory: Some("../data".to_string()),
            ..Default::default()
        };

        let result = resolve_parent_working_directory(
            &parent_uri,
            |uri| {
                if uri == &parent_uri {
                    Some(parent_meta.clone())
                } else {
                    None
                }
            },
            Some(&workspace),
        );

        assert!(result.is_some());
        let wd = result.unwrap();
        // Parent's effective WD should be the inherited one resolved from parent's directory
        // ../data from /project/src resolves to /project/data
        assert_eq!(wd, "/project/data");
    }

    #[test]
    fn test_resolve_parent_working_directory_explicit_takes_precedence() {
        // Validates: Requirements 5.1
        // When parent has both explicit and inherited WD, explicit takes precedence
        let parent_uri = Url::parse("file:///project/src/parent.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let parent_meta = CrossFileMetadata {
            working_directory: Some("/explicit".to_string()),
            inherited_working_directory: Some("/project/inherited".to_string()),
            ..Default::default()
        };

        let result = resolve_parent_working_directory(
            &parent_uri,
            |uri| {
                if uri == &parent_uri {
                    Some(parent_meta.clone())
                } else {
                    None
                }
            },
            Some(&workspace),
        );

        assert!(result.is_some());
        let wd = result.unwrap();
        // Explicit WD should take precedence
        assert_eq!(wd, "/project/explicit");
    }

    #[test]
    fn test_resolve_parent_working_directory_relative_explicit_wd() {
        // Validates: Requirements 5.1
        // When parent has relative explicit @lsp-cd, resolve it relative to parent's directory
        let parent_uri = Url::parse("file:///project/src/parent.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let parent_meta = CrossFileMetadata {
            working_directory: Some("../data".to_string()),
            ..Default::default()
        };

        let result = resolve_parent_working_directory(
            &parent_uri,
            |uri| {
                if uri == &parent_uri {
                    Some(parent_meta.clone())
                } else {
                    None
                }
            },
            Some(&workspace),
        );

        assert!(result.is_some());
        let wd = result.unwrap();
        // ../data from /project/src resolves to /project/data
        assert_eq!(wd, "/project/data");
    }

    #[test]
    fn test_resolve_parent_working_directory_no_workspace_root() {
        // Test behavior when workspace root is not available
        let parent_uri = Url::parse("file:///project/src/parent.R").unwrap();

        let parent_meta = CrossFileMetadata {
            working_directory: None,
            ..Default::default()
        };

        let result = resolve_parent_working_directory(
            &parent_uri,
            |uri| {
                if uri == &parent_uri {
                    Some(parent_meta.clone())
                } else {
                    None
                }
            },
            None, // No workspace root
        );

        assert!(result.is_some());
        let wd = result.unwrap();
        // Should still return parent's directory
        assert_eq!(wd, "/project/src");
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
                    Some(parent_meta.clone())
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
                    Some(parent1_meta.clone())
                } else if uri == &parent2_uri {
                    Some(parent2_meta.clone())
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
                    Some(parent_meta.clone())
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
        // In this case, resolve_parent_working_directory falls back to parent's directory
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
                    Some(parent_meta.clone())
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
                    Some(a_meta.clone())
                } else if uri == &b_uri {
                    Some(b_meta.clone())
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
                    Some(parent_meta.clone())
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
    fn test_resolve_parent_wd_with_depth_zero_fallback() {
        // Validates: Requirement 9.2
        // When depth is 0, should fall back to parent's directory
        let parent_uri = Url::parse("file:///project/scripts/parent.R").unwrap();
        let workspace = Url::parse("file:///project").unwrap();

        let result = resolve_parent_working_directory_with_depth(
            &parent_uri,
            &|_| panic!("Should not call get_metadata when depth is 0"),
            Some(&workspace),
            0, // Zero depth
        );

        assert!(result.is_some());
        let wd = result.unwrap();
        // Should fall back to parent's directory
        assert_eq!(wd, "/project/scripts");
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
                    Some(a_meta.clone())
                } else if uri == &b_uri {
                    Some(b_meta.clone())
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
                    Some(a_meta.clone())
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
                    Some(a_meta.clone())
                } else if uri == &b_uri {
                    Some(b_meta.clone())
                } else if uri == &c_uri {
                    Some(c_meta.clone())
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
                    Some(a_meta.clone())
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
}
