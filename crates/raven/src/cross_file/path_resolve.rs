//
// cross_file/path_resolve.rs
//
// Path resolution for cross-file awareness
//
// CRITICAL DESIGN NOTE: Forward vs Backward Directive Path Resolution
// ====================================================================
// Directives are written with the canonical `# raven:` prefix; the `@lsp-`
// forms named below are permanent aliases that parse identically.
// This module provides two PathContext constructors with DIFFERENT behaviors:
//
// 1. PathContext::new() - For BACKWARD directives (`# raven: sourced-by`, `# raven: run-by`, `# raven: included-by`)
//    - IGNORES `# raven: cd` working directory
//    - Always resolves paths relative to the file's own directory
//    - Rationale: Backward directives describe static file relationships from the child's
//      perspective. They declare "this file is sourced by that parent file" - a relationship
//      that should NOT change based on runtime working directory.
//
// 2. PathContext::from_metadata() - For FORWARD directives (`# raven: source`, `# raven: run`, `# raven: include`)
//                                   and source() calls
//    - USES `# raven: cd` working directory when present
//    - Resolves paths relative to the working directory (or file's directory if no `# raven: cd`)
//    - Rationale: Forward directives and source() calls describe runtime execution behavior.
//      They are semantically equivalent to R's source() function, which is affected by
//      the current working directory at runtime.
//
// DO NOT change this behavior without understanding the full implications for cross-file
// awareness. User-facing explanation lives in `docs/cross-file.md`.
//

use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::Url;

use super::types::CrossFileMetadata;

/// Context for path resolution
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathContext {
    /// Path of the current file
    pub file_path: PathBuf,
    /// Explicit working directory from directive
    pub working_directory: Option<PathBuf>,
    /// Shiny application directory used as a runtime working directory.
    pub application_working_directory: Option<PathBuf>,
    /// Working directory inherited from parent
    pub inherited_working_directory: Option<PathBuf>,
    /// Workspace root
    pub workspace_root: Option<PathBuf>,
    /// Implicit testthat/testit working-directory default (issue #638),
    /// layout-only gated: populated only by [`Self::from_metadata`] (never
    /// [`Self::new`] — backward directives are unaffected by construction),
    /// only when the file lies under `<workspace_root>/tests/testthat/` or
    /// `tests/testit/` (any nesting depth; the anchor is always that
    /// two-component directory, matching the runner's `chdir` into it), and
    /// only when no explicit or inherited `# raven: cd` is present. No
    /// DESCRIPTION/package-mode requirement.
    ///
    /// This is a SOFT default, deliberately weaker than a `# raven: cd`:
    /// - consulted only by [`Self::effective_working_directory`], at the
    ///   LOWEST priority (below explicit and inherited cd);
    /// - excluded from [`Self::cd_in_effect`], so the workspace-root fallback
    ///   stays active and [`Self::forward_child_inherited_wd`] never
    ///   propagates it into forward-sourced children;
    /// - complemented by a file-directory compatibility fallback in
    ///   `resolve_path_rich` (see `implicit_wd_file_dir_fallback`) so
    ///   files nested below the anchor keep their file-relative resolution.
    pub implicit_test_working_directory: Option<PathBuf>,
}

impl PathContext {
    /// Create a new context for a file WITHOUT working directory support.
    ///
    /// **USE FOR: Backward directives only** (`# raven: sourced-by`, `# raven: run-by`, `# raven: included-by`)
    ///
    /// This constructor creates a PathContext that resolves paths relative to the file's
    /// own directory, ignoring any `# raven: cd` directive. This is intentional because backward
    /// directives describe static file relationships that should not change based on runtime
    /// working directory.
    ///
    /// **DO NOT USE FOR:** Forward directives (`# raven: source`) or `source()` calls.
    /// Use `PathContext::from_metadata()` instead, which respects `# raven: cd`.
    pub fn new(file_uri: &Url, workspace_root: Option<&Url>) -> Option<Self> {
        let file_path = file_uri.to_file_path().ok()?;
        let workspace_root = workspace_root.and_then(|u| u.to_file_path().ok());
        Some(Self {
            file_path,
            working_directory: None,
            application_working_directory: None,
            inherited_working_directory: None,
            workspace_root,
            implicit_test_working_directory: None,
        })
    }

    /// Create a context for FORWARD resolution (`source()` calls, forward
    /// directives) when no `CrossFileMetadata` is available for the file.
    ///
    /// Equivalent to [`Self::from_metadata`] with empty metadata: no explicit
    /// or inherited `# raven: cd`, but the implicit testthat/testit working
    /// directory (issue #638) is still derived from the file's location, so
    /// metadata-less forward call sites resolve uniformly with metadata-aware
    /// ones. Do NOT use for backward directives — use [`Self::new`].
    pub fn forward_without_metadata(file_uri: &Url, workspace_root: Option<&Url>) -> Option<Self> {
        let mut ctx = Self::new(file_uri, workspace_root)?;
        ctx.implicit_test_working_directory =
            implicit_test_working_directory(&ctx.file_path, ctx.workspace_root.as_deref());
        Some(ctx)
    }

    /// Create a context from a file URI and its metadata WITH working directory support.
    ///
    /// **USE FOR: Forward directives** (`# raven: source`, `# raven: run`, `# raven: include`) **and source() calls**
    ///
    /// This constructor creates a PathContext that respects `# raven: cd` working directory
    /// directives. Paths are resolved relative to the working directory (if set) or the
    /// file's directory (if no working directory). This matches R's runtime behavior where
    /// `source()` calls are affected by the current working directory.
    ///
    /// **DO NOT USE FOR:** Backward directives (`# raven: sourced-by`, `# raven: run-by`, `# raven: included-by`).
    /// Use `PathContext::new()` instead, which ignores `# raven: cd`.
    ///
    /// Priority for path resolution: explicit Raven working directory > Shiny
    /// application directory > inherited working directory > implicit test
    /// directory > file directory.
    pub fn from_metadata(
        file_uri: &Url,
        metadata: &CrossFileMetadata,
        workspace_root: Option<&Url>,
    ) -> Option<Self> {
        let mut ctx = Self::new(file_uri, workspace_root)?;

        // Apply explicit working directory from metadata if present
        if let Some(ref wd_path) = metadata.working_directory {
            ctx.working_directory = resolve_working_directory(wd_path, &ctx);
        }

        // Shiny evaluates convention-loaded files with the application directory
        // as the process working directory. This hard convention is weaker than an
        // explicit Raven override but stronger than unrelated inherited metadata.
        if ctx.working_directory.is_none()
            && let Some(application) = metadata.application_working_directory.as_ref()
        {
            ctx.application_working_directory = Some(PathBuf::from(application));
        }

        // Apply inherited working directory if neither explicit nor Shiny context applies.
        // Inherited working directories are stored as absolute paths, so use directly
        // when absolute. Only resolve if relative (legacy/edge case).
        if ctx.working_directory.is_none()
            && ctx.application_working_directory.is_none()
            && let Some(ref inherited_wd) = metadata.inherited_working_directory
        {
            let inherited_path = PathBuf::from(inherited_wd);
            if inherited_path.is_absolute() {
                ctx.inherited_working_directory = Some(inherited_path);
            } else {
                // Relative inherited paths should not occur in normal operation
                log::trace!(
                    "Inherited WD is relative '{}' for {}, resolving relative to file directory",
                    inherited_wd,
                    file_uri
                );
                ctx.inherited_working_directory = resolve_working_directory(inherited_wd, &ctx);
            }
        }

        // Implicit testthat/testit working directory (issue #638): only when
        // neither an explicit nor an inherited `# raven: cd` applies.
        if ctx.working_directory.is_none()
            && ctx.application_working_directory.is_none()
            && ctx.inherited_working_directory.is_none()
        {
            ctx.implicit_test_working_directory =
                implicit_test_working_directory(&ctx.file_path, ctx.workspace_root.as_deref());
        }

        Some(ctx)
    }

    /// Get the effective working directory for path resolution
    pub fn effective_working_directory(&self) -> PathBuf {
        // Priority: explicit Raven override > Shiny application > inherited >
        // implicit testthat/testit (issue #638) > file directory.
        self.working_directory
            .clone()
            .or_else(|| self.application_working_directory.clone())
            .or_else(|| self.inherited_working_directory.clone())
            .or_else(|| self.implicit_test_working_directory.clone())
            .unwrap_or_else(|| {
                self.file_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| self.file_path.clone())
            })
    }

    /// Ordered base directories whose entries should be offered for an empty
    /// forward path completion (`source("")` or a forward directive).
    ///
    /// This is the completion counterpart of forward path resolution's
    /// working-directory precedence:
    ///
    /// - an explicit or inherited `# raven: cd` contributes only its pinned
    ///   effective working directory;
    /// - the implicit testthat/testit working directory contributes its anchor
    ///   first, followed by the nested file's own directory as the
    ///   compatibility fallback (when those differ), then the workspace-root
    ///   fallback because the implicit tier deliberately does not count as a
    ///   hard cd;
    /// - otherwise the workspace root is the default completion base when it
    ///   is available, preserving the workspace-root completion behavior for
    ///   ordinary unannotated files; without one, the file's directory is used.
    ///
    /// Keeping this policy on `PathContext` prevents completion consumers from
    /// re-deriving the explicit/inherited/implicit tier precedence from public
    /// fields. The returned order is significant: callers must keep the first
    /// entry when two bases contain the same completion name, matching the
    /// resolver's exact-match precedence.
    pub(crate) fn forward_completion_base_directories(&self) -> Vec<PathBuf> {
        if self.cd_in_effect() {
            return vec![self.effective_working_directory()];
        }

        if self.implicit_test_working_directory.is_some() {
            let mut bases = vec![self.effective_working_directory()];
            if let Some(file_dir) = self.implicit_wd_file_dir_fallback() {
                bases.push(file_dir);
            }
            if let Some(workspace_root) = self.workspace_root.clone()
                && !bases.contains(&workspace_root)
            {
                bases.push(workspace_root);
            }
            return bases;
        }

        vec![
            self.workspace_root
                .clone()
                .unwrap_or_else(|| self.effective_working_directory()),
        ]
    }

    /// The file's own directory as a compatibility resolution base, `Some`
    /// only when the implicit testthat/testit working directory (issue #638)
    /// is what `effective_working_directory()` returns AND the file's own
    /// directory differs from that anchor (i.e. the file is nested below it).
    ///
    /// `resolve_path_rich` tries this base after the implicit anchor and
    /// before the workspace-root fallback, so a file in
    /// `tests/testthat/fixtures/` sourcing `"local.R"` file-relatively (which
    /// worked before #638) keeps resolving. For files directly in the anchor
    /// directory the two bases coincide and this returns `None`.
    fn implicit_wd_file_dir_fallback(&self) -> Option<PathBuf> {
        // Explicit/inherited cd wins outright; from_metadata never sets the
        // implicit field alongside them, but guard anyway.
        if self.working_directory.is_some()
            || self.application_working_directory.is_some()
            || self.inherited_working_directory.is_some()
        {
            return None;
        }
        let anchor = self.implicit_test_working_directory.as_ref()?;
        let file_dir = self.file_path.parent()?;
        (file_dir != anchor.as_path()).then(|| file_dir.to_path_buf())
    }

    /// Create a child context for a sourced file with chdir=TRUE
    pub fn child_context_with_chdir(&self, child_path: &Path) -> Self {
        let child_dir = child_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| child_path.to_path_buf());
        Self {
            file_path: child_path.to_path_buf(),
            working_directory: None,
            application_working_directory: None,
            inherited_working_directory: Some(child_dir),
            workspace_root: self.workspace_root.clone(),
            implicit_test_working_directory: None,
        }
    }

    /// Create a child context for a sourced file without chdir
    pub fn child_context(&self, child_path: &Path) -> Self {
        Self {
            file_path: child_path.to_path_buf(),
            working_directory: None,
            application_working_directory: None,
            inherited_working_directory: Some(self.effective_working_directory()),
            workspace_root: self.workspace_root.clone(),
            implicit_test_working_directory: None,
        }
    }

    /// Create a child context for a sourced file, respecting chdir flag.
    ///
    /// NOTE: these `child_context*` methods are unconditional WD-inheritance
    /// primitives — `child_context` always pins the parent's effective directory.
    /// They are NOT the forward-`source()` policy: a forward child inherits a WD
    /// only when a cd is in effect. Decide propagation with
    /// [`Self::forward_child_inherited_wd`]; reach for these primitives only once
    /// that has said a WD applies (or for the explicit `chdir = TRUE` case).
    pub fn child_context_for_source(&self, child_path: &Path, chdir: bool) -> Self {
        if chdir {
            self.child_context_with_chdir(child_path)
        } else {
            self.child_context(child_path)
        }
    }

    /// Whether a `# raven: cd` working directory is in effect for this context —
    /// either declared explicitly on the file or inherited from an ancestor cd.
    /// This is the precondition that keeps a directory pinned across a forward
    /// `source()` hop and that disables the workspace-root fallback in
    /// [`resolve_path_with_workspace_fallback`]; see [`Self::forward_child_inherited_wd`].
    ///
    /// Deliberately EXCLUDES the implicit testthat/testit working directory
    /// (issue #638): that soft default anchors a test file's own relative
    /// paths but must neither suppress the workspace-root fallback nor
    /// propagate into forward-sourced children.
    pub fn cd_in_effect(&self) -> bool {
        self.working_directory.is_some()
            || self.application_working_directory.is_some()
            || self.inherited_working_directory.is_some()
    }

    /// Working directory that may propagate through `sourced-by` metadata.
    ///
    /// The implicit testthat/testit anchor is a soft default for this file
    /// only, so it deliberately returns `None` unless an explicit or inherited
    /// `# raven: cd` is present. Outside that soft-WD regime, the historical
    /// file-directory fallback remains inheritable.
    pub fn inheritable_working_directory(&self) -> Option<PathBuf> {
        if self.implicit_test_working_directory.is_some() && !self.cd_in_effect() {
            None
        } else {
            Some(self.effective_working_directory())
        }
    }

    /// The working directory a non-standalone forward `source()` child inherits
    /// from this (caller) context, or `None` when none is in effect.
    ///
    /// - `chdir == true` → the child's own directory (R changes the working
    ///   directory to the sourced file's directory).
    /// - else a `# raven: cd` in effect here ([`Self::cd_in_effect`]) → this
    ///   context's effective working directory.
    /// - else → `None`, so the child resolves its own `source()` paths relative
    ///   to its own directory WITH the workspace-root fallback — matching how
    ///   the dependency-graph build (`build_dependency_graph_from_workspace`)
    ///   resolves the same edge.
    ///
    /// This is the single source of truth for forward-source working-directory
    /// propagation across the two LIVE-LSP consumers that thread a caller context
    /// into a child: the on-demand index walk (`index_forward_chain`) and scope
    /// resolution (`build_forward_child_path_context`). They share this method so
    /// they cannot disagree. The dependency-graph build
    /// (`build_dependency_graph_from_workspace` → `update_file`) does NOT call
    /// this — it resolves each file from that file's own metadata with no caller
    /// context, and stays consistent by a separate invariant: the workspace scan
    /// only ever enriches *backward*-directive (`sourced_by`) files with an
    /// inherited WD, never forward-source children, so a plain forward hop has no
    /// baked-in WD there either. Keep that invariant if extending scan enrichment.
    ///
    /// Injecting the caller's plain directory for a non-cd hop would re-base a
    /// deep hop and suppress the fallback (see `resolve_path_impl`'s gate, which
    /// reuses [`Self::cd_in_effect`]) — the bug this rule prevents.
    pub fn forward_child_inherited_wd(&self, child_path: &Path, chdir: bool) -> Option<PathBuf> {
        if chdir {
            Some(
                child_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| self.effective_working_directory()),
            )
        } else if self.cd_in_effect() {
            Some(self.effective_working_directory())
        } else {
            None
        }
    }
}

/// Derive the execution context for one forward-source child.
///
/// A standalone child is caller-independent and re-anchors at its own
/// metadata context. Other children inherit only the working directory that
/// [`PathContext::forward_child_inherited_wd`] permits; an explicit child
/// `# raven: cd` continues to win. Shared by scope resolution and the pure
/// contextual tar-provider traversal so their path semantics cannot drift.
pub(crate) fn forward_child_path_context<G>(
    child_uri: &Url,
    child_is_standalone: bool,
    chdir: bool,
    parent_ctx: Option<&PathContext>,
    workspace_root: Option<&Url>,
    get_metadata: &G,
) -> Option<PathContext>
where
    G: Fn(&Url) -> Option<std::sync::Arc<super::types::CrossFileMetadata>>,
{
    if child_is_standalone {
        get_metadata(child_uri)
            .and_then(|metadata| PathContext::from_metadata(child_uri, &metadata, workspace_root))
            .or_else(|| PathContext::forward_without_metadata(child_uri, workspace_root))
    } else {
        let parent_ctx = parent_ctx?;
        let mut child_ctx = match get_metadata(child_uri) {
            Some(metadata) => PathContext::from_metadata(child_uri, &metadata, workspace_root)?,
            None => PathContext::forward_without_metadata(child_uri, workspace_root)?,
        };
        let inherited = parent_ctx.forward_child_inherited_wd(&child_ctx.file_path, chdir);
        if child_ctx.working_directory.is_none()
            && child_ctx.application_working_directory.is_none()
            && inherited.is_some()
        {
            child_ctx.inherited_working_directory = inherited;
        }
        Some(child_ctx)
    }
}

/// Which filesystem regime produced a case-only path mismatch (issue #530).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseMismatchRegime {
    /// The typed path resolved only because the host filesystem folds case
    /// (macOS, typical Windows). R runs fine here, but the path will not be
    /// found on a case-sensitive filesystem (e.g. Linux CI). Information-level.
    CaseInsensitiveFs,
    /// The typed path did not exist; resolution found exactly one
    /// case-insensitive match on a case-sensitive filesystem. For a forward
    /// `source()` R itself would error at `source()` time; for a backward
    /// directive (issue #535) it is purely a casing/portability mismatch (R never
    /// executes a backward directive). Warning-level either way.
    CaseSensitiveFs,
}

/// Outcome of path resolution: the resolved (case-corrected) path, plus a
/// case-only-mismatch signal set when the path resolved via a case difference
/// rather than an exact match. See issue #530.
///
/// The bare public `resolve_path` (backward) returns just `.path`, discarding the
/// signal; `resolve_backward_path_rich` and `resolve_source_path_rich` carry it
/// for the case-mismatch diagnostics. As of issue #535 BOTH regimes can arise in
/// either direction: `CaseInsensitiveFs` from the direction-independent exact-match
/// (`exists()`) case-correction, and `CaseSensitiveFs` from the
/// single-case-insensitive-match leniency, which is now also direction independent
/// (backward directives gain the leniency but never the forward-only workspace-root
/// fallback).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolveOutcome {
    /// The resolved path, case-corrected to the real on-disk spelling. `Some`
    /// for both a resolved file and an unresolved-but-normalizable path (the
    /// latter so the caller can emit a missing-file diagnostic); `None` only on
    /// normalization failure or a missing workspace root for a `/`-path.
    pub path: Option<PathBuf>,
    /// `Some` iff the path resolved to an existing file via a case-only
    /// difference from the typed spelling.
    pub case_mismatch: Option<CaseMismatchRegime>,
}

impl ResolveOutcome {
    fn resolved(path: PathBuf, case_mismatch: Option<CaseMismatchRegime>) -> Self {
        Self {
            path: Some(path),
            case_mismatch,
        }
    }
    fn unresolved(path: Option<PathBuf>) -> Self {
        Self {
            path,
            case_mismatch: None,
        }
    }
}

/// Return every lexical path tier a forward source may select, in resolution
/// priority order, including tiers that currently lose because another path
/// exists first.
///
/// Filesystem membership watchers use this enumeration so creating a missing
/// higher-priority target retargets an existing fallback. Workspace-root
/// absolute paths have only the workspace tier. Relative paths use the
/// effective working directory, the nested test compatibility directory, and
/// finally the forward-only workspace fallback when no explicit or inherited
/// `# raven: cd` pins resolution.
pub(crate) fn forward_path_candidate_tiers(path: &str, context: &PathContext) -> Vec<PathBuf> {
    if path.is_empty() {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    let mut push_normalized = |candidate: PathBuf| {
        if let Some(candidate) = normalize_path(&candidate)
            && !candidates.contains(&candidate)
        {
            candidates.push(candidate);
        }
    };

    if let Some(stripped) = path.strip_prefix('/') {
        if let Some(root) = context.workspace_root.as_ref() {
            push_normalized(root.join(stripped));
        }
        return candidates;
    }

    push_normalized(context.effective_working_directory().join(path));
    if let Some(file_dir) = context.implicit_wd_file_dir_fallback() {
        push_normalized(file_dir.join(path));
    }
    if !context.cd_in_effect()
        && let Some(root) = context.workspace_root.as_ref()
    {
        push_normalized(root.join(path));
    }
    candidates
}

/// Resolve a path string to an absolute path, **without** the workspace-root
/// fallback (use for backward directives — `# raven: sourced-by`/`run-by`/
/// `included-by` — which resolve relative to the file's own directory and ignore
/// `# raven: cd`). Handles file-relative, workspace-relative, and absolute paths.
///
/// As of issue #535 this folds a single case-only typo to the real on-disk file
/// (the single-case-insensitive-match leniency is direction independent), so a
/// wrong-cased backward directive resolves on a case-sensitive filesystem too —
/// but it still never gains the forward-only workspace-root fallback. The
/// case-mismatch signal is discarded here; use [`resolve_backward_path_rich`] to
/// keep it for the diagnostic.
pub fn resolve_path(path: &str, context: &PathContext) -> Option<PathBuf> {
    resolve_path_rich(path, context, false).path
}

/// Resolve a path with workspace-root fallback for source() statements and forward directives.
///
/// This function first tries normal resolution (relative to file's directory or `# raven: cd`).
/// If that fails AND the file has no explicit working directory directive, it falls back
/// to trying the path relative to workspace root.
///
/// This is useful for codebases that haven't been annotated with directives but where
/// source() calls use paths relative to the project root (a common pattern in R projects).
///
/// Use this for AST-detected `source()` calls AND for forward directives (`# raven: source`,
/// `# raven: run`, `# raven: include`). Forward directives are semantically equivalent to
/// `source()` calls (see `.kiro/specs/lsp-source-directive/`) and must resolve identically.
/// Do NOT use for backward directives (`# raven: sourced-by`, `# raven: run-by`,
/// `# raven: included-by`) which always resolve relative to the file's directory.
pub fn resolve_path_with_workspace_fallback(path: &str, context: &PathContext) -> Option<PathBuf> {
    resolve_path_rich(path, context, true).path
}

/// Forward-source resolution that also reports a case-only mismatch (issue
/// #530). The `path` field is identical to
/// [`resolve_path_with_workspace_fallback`]; `case_mismatch` is `Some` when the
/// file resolved via a case difference. Use for the forward
/// `source-path-case-mismatch` diagnostic. Forward wrapper only — for backward
/// directives use [`resolve_backward_path_rich`] (which also carries the signal,
/// issue #535).
pub fn resolve_source_path_rich(path: &str, context: &PathContext) -> ResolveOutcome {
    resolve_path_rich(path, context, true)
}

/// Backward-directive resolution that also reports a case-only mismatch (issue
/// #535). The `path` field is identical to [`resolve_path`]; `case_mismatch` is
/// `Some` when the file resolved via a case difference. Use for the backward
/// `source-path-case-mismatch` diagnostic.
///
/// Like [`resolve_path`], this passes `try_workspace_fallback = false`, so it
/// gains the single-case-insensitive-match leniency (which is now direction
/// independent, issue #535) but NOT the workspace-root fallback (forward-only;
/// backward directives resolve relative to the file's own directory and ignore
/// `# raven: cd`).
pub fn resolve_backward_path_rich(path: &str, context: &PathContext) -> ResolveOutcome {
    resolve_path_rich(path, context, false)
}

/// The implicit testthat/testit working-directory anchor for `file_path`
/// (issue #638): `<root>/tests/testthat` or `<root>/tests/testit` when the
/// file lies under one of those directories (any nesting depth), else `None`.
///
/// Layout-only gating — no DESCRIPTION or package-mode requirement — because
/// testthat's `test_dir()`/`test_local()` (and testit's `test_dir()`) chdir
/// into that directory before sourcing helpers and running tests regardless
/// of package structure. Reuses `package_state::is_testthat_or_testit_test`
/// as the single source of truth for "is this file testthat/testit-managed"
/// so the two checks cannot drift. O(1) path-component work, no filesystem
/// I/O.
fn implicit_test_working_directory(
    file_path: &Path,
    workspace_root: Option<&Path>,
) -> Option<PathBuf> {
    let root = workspace_root?;
    if !crate::package_state::is_testthat_or_testit_test(file_path, root) {
        return None;
    }
    // is_testthat_or_testit_test guarantees the first two components below
    // root are `tests` then `testthat`|`testit`; the anchor is exactly those.
    let rel = file_path.strip_prefix(root).ok()?;
    let mut comps = rel.components();
    let tests = comps.next()?;
    let runner_dir = comps.next()?;
    Some(root.join(tests).join(runner_dir))
}

/// The trusted directory prefix to case-correct `canonical` beneath: the file's
/// own `base` when the normalized candidate stays under it, otherwise the
/// workspace root for a parent-relative path (`../foo`) that escapes `base` but
/// still lands inside the workspace. Both are registered/trusted prefixes whose
/// casing is taken verbatim (issue #476 keeps the registered workspace-folder
/// spelling intact). Returns `None` only when the candidate escapes the
/// workspace entirely, in which case no case-correction is attempted (the caller
/// falls back to the base prefix, a no-op for an out-of-tree path). Issue #530.
fn case_scan_prefix<'a>(
    canonical: &Path,
    base: &'a Path,
    workspace_root: Option<&'a Path>,
) -> Option<&'a Path> {
    if canonical.starts_with(base) {
        return Some(base);
    }
    workspace_root.filter(|root| canonical.starts_with(root))
}

/// `Some(regime)` when `corrected` differs from `canonical` — i.e. the
/// filesystem folded a wrong-case lookup so the on-disk spelling differs from
/// the typed one (issue #530). Used for the exact-match (`exists()`) branches,
/// where any spelling change means a case-insensitive filesystem.
fn case_mismatch_if_corrected(
    canonical: &Path,
    corrected: &Path,
    regime: CaseMismatchRegime,
) -> Option<CaseMismatchRegime> {
    (corrected != canonical).then_some(regime)
}

/// Internal core of path resolution. Returns the resolved/case-corrected path
/// and a case-only-mismatch signal.
///
/// Ordering is exact-before-case-insensitive at every base (issue #530): a path
/// that exists as typed (or via filesystem case-folding) always wins before the
/// single-case-insensitive-match leniency is considered, and the workspace-root
/// fallback retains its priority over the leniency.
///
/// Two independent concepts are decoupled here (issue #535):
/// - **`try_workspace_fallback`** gates ONLY the workspace-root fallback (step 2
///   exact, step 4 single-ci-match, and the `workspace_fallback_root` computation).
///   This is **forward-only**: true for `source()` calls and forward directives,
///   false for backward directives, which resolve relative to the file's own
///   directory and ignore `# raven: cd`.
/// - The **single-case-insensitive-match leniency** (the `/`-path "missing as
///   typed" arm and step 3) is **direction independent**: both forward and
///   backward resolution fold a single case-only typo to the real on-disk file.
///   Backward gained this in #535; it never gains the workspace-root fallback.
///
/// The step-1 exact-match case-correction (`canonicalize_case_below`) was always
/// direction independent. The public `resolve_path` wrapper discards the
/// `case_mismatch` signal; `resolve_backward_path_rich` carries it for the
/// backward diagnostic.
fn resolve_path_rich(
    path: &str,
    context: &PathContext,
    try_workspace_fallback: bool,
) -> ResolveOutcome {
    if path.is_empty() {
        log::trace!("Path resolution: empty path provided");
        return ResolveOutcome::unresolved(None);
    }

    let base = context.effective_working_directory();

    // If path starts with /, it's explicitly workspace-root-relative.
    if let Some(stripped) = path.strip_prefix('/') {
        let Some(workspace_root) = context.workspace_root.as_ref() else {
            log::warn!(
                "Failed to resolve workspace-root-relative path '{}': no workspace root available",
                path
            );
            return ResolveOutcome::unresolved(None);
        };
        let resolved = workspace_root.join(stripped);
        return match normalize_path(&resolved) {
            // Case-correct below the workspace-root prefix when the file exists, so
            // the edge target URI matches the index key on case-insensitive
            // filesystems (issue #476). This also covers a workspace-package
            // `system.file()` target, whose `/inst/...` path resolves here.
            Some(canonical) if canonical.exists() => {
                let corrected = canonicalize_case_below(workspace_root, &canonical);
                let mismatch = case_mismatch_if_corrected(
                    &canonical,
                    &corrected,
                    CaseMismatchRegime::CaseInsensitiveFs,
                );
                ResolveOutcome::resolved(corrected, mismatch)
            }
            // Missing as typed: single-case-insensitive-match leniency. Direction
            // independent (issue #535) — a `/`-path needs a workspace root to reach
            // this code at all, so this is not the (forward-only) workspace-root
            // fallback; it is the case-leniency that now applies to backward
            // directives too.
            Some(canonical) => {
                if let Some(corrected) = resolve_single_ci_match(workspace_root, &canonical) {
                    ResolveOutcome::resolved(corrected, Some(CaseMismatchRegime::CaseSensitiveFs))
                } else {
                    // Return the lexical path for missing-file diagnostics.
                    ResolveOutcome::unresolved(Some(canonical))
                }
            }
            None => {
                log::warn!(
                    "Failed to resolve path '{}': normalization failed, attempted_path='{}'",
                    path,
                    resolved.display()
                );
                ResolveOutcome::unresolved(None)
            }
        };
    }

    // Relative path. `base` (effective working directory) is the trusted prefix
    // for case-correction below.
    let resolved = base.join(path);
    let Some(canonical) = normalize_path(&resolved) else {
        log::warn!(
            "Failed to resolve path '{}': normalization failed, attempted_path='{}'",
            path,
            resolved.display()
        );
        return ResolveOutcome::unresolved(None);
    };

    // The trusted prefix to case-correct beneath: the file's `base` for an
    // ordinary relative path, or the workspace root for a `../`-relative path
    // that normalizes above `base` but still lands inside the workspace. Both
    // are registered/trusted prefixes taken verbatim (issue #476).
    let scan_prefix = case_scan_prefix(&canonical, &base, context.workspace_root.as_deref());

    // Step 1: file-relative exact match (with #476 case-correction).
    if canonical.exists() {
        let corrected = canonicalize_case_below(scan_prefix.unwrap_or(&base), &canonical);
        let mismatch = case_mismatch_if_corrected(
            &canonical,
            &corrected,
            CaseMismatchRegime::CaseInsensitiveFs,
        );
        return ResolveOutcome::resolved(corrected, mismatch);
    }

    // Step 1b: file-directory compatibility fallback for the implicit
    // testthat/testit working directory (issue #638). Only reachable when the
    // implicit anchor is the effective base and the file is nested below it
    // (see `implicit_wd_file_dir_fallback`); keeps pre-#638 file-relative
    // resolution working for files in tests/testthat subdirectories. Retain
    // the normalized candidate for the case-insensitive pass below: every
    // resolution base's exact match must win before any base gets leniency.
    let mut implicit_file_dir_candidate = None;
    if let Some(file_dir) = context.implicit_wd_file_dir_fallback() {
        let fallback_resolved = file_dir.join(path);
        if let Some(fallback_canonical) = normalize_path(&fallback_resolved) {
            if fallback_canonical.exists() {
                let corrected = canonicalize_case_below(&file_dir, &fallback_canonical);
                let mismatch = case_mismatch_if_corrected(
                    &fallback_canonical,
                    &corrected,
                    CaseMismatchRegime::CaseInsensitiveFs,
                );
                return ResolveOutcome::resolved(corrected, mismatch);
            }
            implicit_file_dir_candidate = Some((file_dir, fallback_canonical));
        }
    }

    // The workspace-root fallback applies only to forward source()/directives
    // AND only when no `# raven: cd` is in effect (explicit or inherited) AND a
    // workspace root is available. `cd_in_effect` is the single definition of "a
    // cd pins the working directory"; the callers that decide whether to
    // PROPAGATE a working directory across a hop
    // ([`PathContext::forward_child_inherited_wd`]) use the same predicate.
    let workspace_fallback_root = (try_workspace_fallback && !context.cd_in_effect())
        .then_some(context.workspace_root.as_ref())
        .flatten();

    // Step 2: workspace-root fallback exact match.
    if let Some(workspace_root) = workspace_fallback_root {
        let workspace_resolved = workspace_root.join(path);
        if let Some(workspace_canonical) = normalize_path(&workspace_resolved)
            && workspace_canonical.exists()
        {
            let corrected = canonicalize_case_below(workspace_root, &workspace_canonical);
            let mismatch = case_mismatch_if_corrected(
                &workspace_canonical,
                &corrected,
                CaseMismatchRegime::CaseInsensitiveFs,
            );
            return ResolveOutcome::resolved(corrected, mismatch);
        }
    }

    // Step 3: file-relative single-case-insensitive-match, scanned beneath the
    // trusted prefix that contains the candidate (handles `../`-relative paths
    // that stay inside the workspace). Direction independent (issue #535): both
    // forward source()/directives and backward directives get this leniency. It
    // is NOT the workspace-root fallback — `scan_prefix` is the file's own `base`
    // (or the workspace root only as the case-correction *prefix* for a `../`
    // path), never an alternate resolution base.
    if let Some(prefix) = scan_prefix
        && let Some(corrected) = resolve_single_ci_match(prefix, &canonical)
    {
        return ResolveOutcome::resolved(corrected, Some(CaseMismatchRegime::CaseSensitiveFs));
    }

    // Step 3b: file-directory compatibility single-case-insensitive-match.
    // This comes after every exact candidate, preserving exact-wins ordering,
    // but before the workspace CI fallback to retain the base priority above.
    if let Some((file_dir, fallback_canonical)) = implicit_file_dir_candidate
        && let Some(corrected) = resolve_single_ci_match(&file_dir, &fallback_canonical)
    {
        return ResolveOutcome::resolved(corrected, Some(CaseMismatchRegime::CaseSensitiveFs));
    }

    // Step 4: workspace-root fallback single-case-insensitive-match (forward only).
    if let Some(workspace_root) = workspace_fallback_root {
        let workspace_resolved = workspace_root.join(path);
        if let Some(workspace_canonical) = normalize_path(&workspace_resolved)
            && let Some(corrected) = resolve_single_ci_match(workspace_root, &workspace_canonical)
        {
            return ResolveOutcome::resolved(corrected, Some(CaseMismatchRegime::CaseSensitiveFs));
        }
    }

    // Unresolved: return the lexical path so callers can report a missing file.
    ResolveOutcome::unresolved(Some(canonical))
}

/// Resolve a path that does not exist as typed to its real on-disk spelling,
/// **only** when every component below `base` is either an exact entry or has
/// **exactly one** case-insensitive match, and the resulting path exists. This
/// is the case-sensitive-filesystem leniency for issue #530: it folds a
/// single-case typo (`templates.r` → `templates.R`) but refuses an ambiguous
/// match (both `templates.R` and `templates.r` present) so no arbitrary entry
/// is silently chosen. Returns `None` when ambiguous, genuinely absent, or when
/// nothing below `base` needed case-correction (a real typo).
fn resolve_single_ci_match(base: &Path, full: &Path) -> Option<PathBuf> {
    let corrected = canonicalize_case_below_unique(base, full)?;
    // Only a *case* difference counts; an identical result that doesn't exist is
    // a genuine missing file, not a case mismatch.
    (corrected != full && corrected.exists()).then_some(corrected)
}

/// Like [`canonicalize_case_below`], but rejects ambiguity: each component below
/// `base` must resolve to an exact entry or to exactly one case-insensitive
/// match (via [`real_entry_name_unique`]). Returns `None` if any component has
/// no match or 2+ case-insensitive matches. Issue #530.
pub(crate) fn canonicalize_case_below_unique(base: &Path, full: &Path) -> Option<PathBuf> {
    let suffix = full.strip_prefix(base).ok()?;
    let mut result = base.to_path_buf();
    for component in suffix.components() {
        match component {
            std::path::Component::Normal(name) => {
                let real = real_entry_name_unique(&result, name)?;
                result.push(real);
            }
            // Suffix is relative and normalized; non-Normal components are not
            // expected, but pass them through defensively.
            other => result.push(other.as_os_str()),
        }
    }
    Some(result)
}

/// Return the real directory-entry name in `dir` matching `name`: an exact-case
/// match if present, otherwise the unique case-insensitive match. Returns `None`
/// if `dir` is unreadable, nothing matches, or **two or more** entries match
/// case-insensitively (ambiguous — only possible on a case-sensitive
/// filesystem). ASCII-only comparison, matching the rest of this module. Issue
/// #530.
fn real_entry_name_unique(dir: &Path, name: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut ci_match: Option<std::ffi::OsString> = None;
    let mut ci_count = 0usize;
    for entry in entries.flatten() {
        let entry_name = entry.file_name();
        if entry_name == name {
            // Exact match always wins, regardless of any case-insensitive ones.
            return Some(entry_name);
        }
        if entry_name
            .to_str()
            .zip(name.to_str())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            ci_count += 1;
            if ci_match.is_none() {
                ci_match = Some(entry_name);
            }
        }
    }
    (ci_count == 1).then_some(ci_match).flatten()
}

/// Resolve a working directory path.
/// Used for `# raven: cd` directive resolution with workspace-relative and absolute path support.
pub fn resolve_working_directory(path: &str, context: &PathContext) -> Option<PathBuf> {
    if path.is_empty() {
        log::trace!("Working directory resolution: empty path provided");
        return None;
    }

    let file_dir = context.file_path.parent();

    log::trace!(
        "Resolving working directory '{}' with file_path='{}', file_dir={:?}",
        path,
        context.file_path.display(),
        file_dir.as_ref().map(|p| p.display().to_string())
    );

    let resolved = if let Some(stripped) = path.strip_prefix('/') {
        // Workspace-root-relative
        let workspace_root = context.workspace_root.as_ref();
        if workspace_root.is_none() {
            log::warn!(
                "Failed to resolve workspace-root-relative working directory '{}': no workspace root available",
                path
            );
            return None;
        }
        workspace_root.unwrap().join(stripped)
    } else {
        // File-relative
        let file_dir = context.file_path.parent();
        if file_dir.is_none() {
            log::warn!(
                "Failed to resolve working directory '{}': file has no parent directory, file_path='{}'",
                path,
                context.file_path.display()
            );
            return None;
        }
        file_dir.unwrap().join(path)
    };

    match normalize_path(&resolved) {
        Some(canonical) => {
            log::trace!(
                "Resolved working directory '{}' to canonical path: '{}'",
                path,
                canonical.display()
            );
            Some(canonical)
        }
        None => {
            log::warn!(
                "Failed to resolve working directory '{}': normalization failed, attempted_path='{}'",
                path,
                resolved.display()
            );
            None
        }
    }
}

/// Rewrite the casing of `full`'s path components *below* `base` to match the
/// real on-disk directory entries, **without resolving symlinks**. Components of
/// `base` are kept verbatim; only the suffix `full` adds beyond `base` is
/// corrected, component by component, via `read_dir`.
///
/// This fixes the case-insensitive-filesystem mismatch behind issue #476: a
/// `source("scripts/templates.r")` resolves (lexically) to `…/templates.r`,
/// which `Path::exists()` accepts on macOS/Windows even though the directory
/// entry is `templates.R`. The workspace index keys files under their
/// directory-walk spelling (`…/templates.R`), so the verbatim-cased resolver
/// path produced an edge target that never matched the index key, dropping every
/// symbol the sourced file defined. Correcting case here — at the single
/// resolution chokepoint — makes the edge target equal the index key uniformly
/// across graph resolution, scope resolution, missing-file diagnostics,
/// go-to-definition, and path completion.
///
/// Only components below `base` are touched because the index preserves the
/// workspace-folder/file prefix spelling exactly as registered (it never
/// symlink-canonicalizes it); `base` is derived from those same URIs, so it
/// already carries the matching prefix. Rewriting the prefix to its on-disk
/// casing could diverge from a differently-cased registered folder URI. This
/// also bounds `read_dir` to the appended source-path depth (usually 1-3).
///
/// `std::fs::canonicalize` is deliberately avoided: it resolves symlinks, so on
/// macOS a fixture under `$TMPDIR` (`/var/…` → `/private/var/…`) would resolve to
/// a prefix the un-canonicalized index keys never use.
///
/// On a case-sensitive filesystem the exact-match branch always wins, so this is
/// a no-op (and a genuinely absent `templates.r` next to an on-disk `templates.R`
/// is left unresolved, as it should be).
///
/// If `full` does not start with `base` (unexpected), `full` is returned
/// unchanged.
fn canonicalize_case_below(base: &Path, full: &Path) -> PathBuf {
    let Ok(suffix) = full.strip_prefix(base) else {
        return full.to_path_buf();
    };
    let mut result = base.to_path_buf();
    for component in suffix.components() {
        match component {
            std::path::Component::Normal(name) => match real_entry_name(&result, name) {
                Some(real) => result.push(real),
                None => result.push(name),
            },
            // Suffix is relative and normalized; non-Normal components are not
            // expected, but pass them through defensively.
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Return the real directory-entry name in `dir` matching `name`: an exact-case
/// match if present (correct on case-sensitive filesystems where two casings can
/// coexist), otherwise the first case-insensitive match. `None` if `dir` is
/// unreadable or nothing matches.
fn real_entry_name(dir: &Path, name: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut ci_match: Option<std::ffi::OsString> = None;
    for entry in entries.flatten() {
        let entry_name = entry.file_name();
        if entry_name == name {
            return Some(entry_name);
        }
        if ci_match.is_none()
            && entry_name
                .to_str()
                .zip(name.to_str())
                .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            ci_match = Some(entry_name);
        }
    }
    ci_match
}

/// Normalize a path by resolving . and .. components
fn normalize_path(path: &Path) -> Option<PathBuf> {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                // Only pop if the last component is a Normal segment
                // Preserve RootDir and Prefix components
                if let Some(last) = components.last()
                    && matches!(last, std::path::Component::Normal(_))
                {
                    components.pop();
                }
            }
            std::path::Component::CurDir => {}
            c => components.push(c),
        }
    }

    if components.is_empty() {
        return None;
    }

    let mut result = PathBuf::new();
    for c in components {
        result.push(c);
    }
    Some(result)
}

/// Public version of normalize_path for use outside this module.
/// Normalizes path by resolving . and .. components and canonicalizing when possible.
pub fn normalize_path_public(path: &Path) -> Option<PathBuf> {
    normalize_path(path)
}

/// Resolve a `system.file(parts..., package = P)` call to a filesystem path.
///
/// Resolution algorithm (the two layouts differ by an `inst/` prefix):
/// 1. If `P == workspace_package_name` → `<workspace_root>/inst/<rel>`
///    (source layout, WITH `inst/`).
/// 2. Else if `P` is installed → search each `lib_paths` entry for
///    `<lib_path>/P/<rel>` (installed layout, NO `inst/` prefix; first hit wins).
/// 3. Otherwise → `None` (unresolved).
///
/// `rel` is formed by joining `parts` with `/`.
/// Build the relative path from `system.file()` literal components, rejecting
/// any that would escape the intended base (`<workspace>/inst` or
/// `<lib>/<pkg>`). A component that is an absolute path, a drive prefix, or
/// contains a `..` parent segment is refused — otherwise a literal such as
/// `system.file("..", "..", "secret.R", package = "pkg")` would turn
/// `system.file()` analysis into an arbitrary local-file read for an untrusted
/// workspace. Returns `None` for empty input or any escaping component.
fn system_file_relative_path(parts: &[String]) -> Option<PathBuf> {
    use std::path::Component;
    let mut rel = PathBuf::new();
    for part in parts {
        let candidate = Path::new(part);
        if candidate.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return None;
        }
        rel.push(candidate);
    }
    (!rel.as_os_str().is_empty()).then_some(rel)
}

pub fn resolve_system_file(
    parts: &[String],
    package: &str,
    workspace_package_name: Option<&str>,
    workspace_root: Option<&Path>,
    lib_paths: &[PathBuf],
) -> Option<PathBuf> {
    let rel = system_file_relative_path(parts)?;

    // Branch 1: same as workspace package → source layout with inst/ prefix
    if let (Some(ws_pkg), Some(ws_root)) = (workspace_package_name, workspace_root)
        && package == ws_pkg
    {
        let inst_dir = ws_root.join("inst");
        let candidate = inst_dir.join(&rel);
        if candidate.exists() {
            // Case-correct below the `inst/` prefix (issue #476) so the edge
            // target matches the index key on case-insensitive filesystems.
            return Some(canonicalize_case_below(&inst_dir, &candidate));
        }
        // Even if file doesn't exist yet, return the path so diagnostics
        // can report a missing file (consistent with resolve_path behavior).
        return Some(candidate);
    }

    // Branch 2: installed package → search lib_paths without inst/ prefix
    for lib_path in lib_paths {
        let candidate = lib_path.join(package).join(&rel);
        if candidate.exists() {
            return Some(canonicalize_case_below(lib_path, &candidate));
        }
    }

    // Branch 3: unresolved
    None
}

/// Resolve any `system_file`-bearing `ForwardSource` entries in `meta` into
/// concrete paths. Call after `extract_metadata` when workspace and library
/// context is available. Each resolved entry gets `source.path` (and, for
/// cross-package hits, `source.resolved_uri`) populated so the existing
/// dependency/scope machinery handles them transparently.
///
/// `source.system_file` is NEVER cleared and unresolved entries are NEVER
/// dropped: resolution state is recomputed from scratch on every call so that
/// package lifecycle events (install/removal in a watched libpath, a workspace
/// `Package:` rename) can re-resolve without re-extracting metadata from
/// source text. The detached system-file convergence transaction revisits
/// every entry where `system_file.is_some()`. The function is idempotent: calling it
/// again with the same inputs yields the same metadata.
///
/// Resolution states after a call:
/// - branch 1 (workspace self-package): `path = "/inst/<rel>"`, `resolved_uri = None`
/// - branch 2 hit (installed package): `path` absolute, `resolved_uri = Some`
/// - branch 2 miss with non-empty `lib_paths` (not installed): `path` empty,
///   `resolved_uri = None` — any prior resolution is cleared so a removed
///   package's stale edge disappears
/// - `lib_paths` empty and not self-package: left untouched (deferred until
///   the package library is ready)
pub fn resolve_system_file_sources(
    meta: &mut super::types::CrossFileMetadata,
    workspace_package_name: Option<&str>,
    workspace_root: Option<&Path>,
    lib_paths: &[PathBuf],
) {
    resolve_system_file_source_entries(
        &mut meta.sources,
        workspace_package_name,
        workspace_root,
        lib_paths,
    );
}

/// Slice-based core of [`resolve_system_file_sources`]: resolves directly on
/// a `ForwardSource` slice so callers that need change detection can resolve
/// into a cloned `Vec` and compare, without deep-cloning the whole
/// `CrossFileMetadata` (`Arc::make_mut`) when nothing changed.
pub fn resolve_system_file_source_entries(
    sources: &mut [super::types::ForwardSource],
    workspace_package_name: Option<&str>,
    workspace_root: Option<&Path>,
    lib_paths: &[PathBuf],
) {
    for source in sources.iter_mut() {
        if let Some(ref sf) = source.system_file {
            // Branch 1: same as workspace package → source layout with inst/ prefix
            if let Some(ws_pkg) = workspace_package_name
                && sf.package == ws_pkg
                && workspace_root.is_some()
            {
                let Some(rel) = system_file_relative_path(&sf.parts) else {
                    continue;
                };
                source.path = format!("/inst/{}", rel.display());
                // Workspace-relative resolution: drop any stale cross-package
                // URI from a previous pass (e.g. before a Package: rename).
                source.resolved_uri = None;
            } else if !lib_paths.is_empty() {
                // Branch 2: cross-package → search lib_paths (only when
                // lib_paths are actually available; otherwise leave intact
                // for a later retry after R initialization).
                let resolved = resolve_system_file(
                    &sf.parts,
                    &sf.package,
                    workspace_package_name,
                    workspace_root,
                    lib_paths,
                );
                if let Some(abs_path) = resolved {
                    source.resolved_uri = Url::from_file_path(&abs_path).ok();
                    source.path = abs_path.display().to_string();
                } else {
                    // Not installed: clear any stale resolution (the package
                    // may have just been removed, or the workspace package
                    // renamed away from a former branch-1 match) but retain
                    // the entry so a later install event can re-resolve it.
                    source.resolved_uri = None;
                    source.path = String::new();
                }
            }
            // When lib_paths is empty AND not same-package, the entry is left
            // intact — including any prior resolution — for a later retry.
        }
    }
}

/// Convert a resolved path to a file URI.
/// Creates a file:// URI from an absolute filesystem path.
pub fn path_to_uri(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Issue #476: case-correction of resolved source paths so the edge target URI
    // matches the workspace-index key on case-insensitive filesystems.
    #[test]
    fn canonicalize_case_below_prefers_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Templates.R"), "").unwrap();
        // An exact-case query must be returned verbatim, never folded to another
        // entry — critical on case-sensitive filesystems where two casings coexist.
        let full = dir.path().join("Templates.R");
        let got = canonicalize_case_below(dir.path(), &full);
        assert_eq!(got, full);
    }

    #[test]
    fn canonicalize_case_below_folds_to_on_disk_case() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("templates.R"), "").unwrap();
        // Query with the wrong case for the final component; the real entry name
        // (`templates.R`) must be substituted so the URI matches the index key.
        // (On a case-insensitive FS `templates.r` opens the same inode; on a
        // case-sensitive FS there is no `templates.r`, so the case-insensitive
        // fallback still resolves to the real entry — the function's job.)
        let queried = dir.path().join("templates.r");
        let got = canonicalize_case_below(dir.path(), &queried);
        assert_eq!(got, dir.path().join("templates.R"));
    }

    #[test]
    fn canonicalize_case_below_corrects_only_below_base() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("Child.R"), "").unwrap();
        // base is the workspace/file prefix and is trusted verbatim; only the
        // appended components are case-corrected.
        let queried = dir.path().join("sub").join("child.r");
        let got = canonicalize_case_below(dir.path(), &queried);
        assert_eq!(got, dir.path().join("sub").join("Child.R"));
    }

    #[test]
    fn canonicalize_case_below_keeps_missing_component_as_typed() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing on disk matches; the path is returned unchanged (it will be
        // reported as a missing file downstream).
        let queried = dir.path().join("nope.R");
        let got = canonicalize_case_below(dir.path(), &queried);
        assert_eq!(got, queried);
    }

    #[test]
    fn canonicalize_case_below_passthrough_when_not_under_base() {
        let base = PathBuf::from("/some/base");
        let unrelated = PathBuf::from("/other/place/file.R");
        assert_eq!(canonicalize_case_below(&base, &unrelated), unrelated);
    }

    // ---- Issue #530: case-only source() path mismatch ----

    use crate::test_utils::host_is_case_sensitive;

    #[test]
    fn real_entry_name_unique_returns_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Templates.R"), "").unwrap();
        let got = real_entry_name_unique(dir.path(), std::ffi::OsStr::new("Templates.R"));
        assert_eq!(got.as_deref(), Some(std::ffi::OsStr::new("Templates.R")));
    }

    #[test]
    fn real_entry_name_unique_folds_single_ci_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("templates.R"), "").unwrap();
        let got = real_entry_name_unique(dir.path(), std::ffi::OsStr::new("templates.r"));
        assert_eq!(got.as_deref(), Some(std::ffi::OsStr::new("templates.R")));
    }

    #[test]
    fn real_entry_name_unique_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("other.R"), "").unwrap();
        let got = real_entry_name_unique(dir.path(), std::ffi::OsStr::new("templates.r"));
        assert_eq!(got, None);
    }

    #[test]
    fn real_entry_name_unique_none_when_ambiguous() {
        // Two entries differing only by case can exist only on a case-sensitive
        // FS; an inexact query that matches both must be rejected as ambiguous.
        if !host_is_case_sensitive() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("templates.R"), "").unwrap();
        std::fs::write(dir.path().join("templates.r"), "").unwrap();
        // An exact query still wins.
        assert_eq!(
            real_entry_name_unique(dir.path(), std::ffi::OsStr::new("templates.R")).as_deref(),
            Some(std::ffi::OsStr::new("templates.R"))
        );
        // A third casing matches both case-insensitively → ambiguous → None.
        assert_eq!(
            real_entry_name_unique(dir.path(), std::ffi::OsStr::new("Templates.R")),
            None
        );
    }

    #[test]
    fn resolve_source_path_rich_no_mismatch_on_exact() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join("scripts").join("templates.R"), "").unwrap();
        let ctx = make_context(
            dir.path().join("main.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_source_path_rich("scripts/templates.R", &ctx);
        assert_eq!(outcome.path, Some(dir.path().join("scripts/templates.R")));
        assert_eq!(outcome.case_mismatch, None);
    }

    #[test]
    fn resolve_source_path_rich_flags_case_only_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join("scripts").join("templates.R"), "").unwrap();
        let ctx = make_context(
            dir.path().join("main.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_source_path_rich("scripts/templates.r", &ctx);
        // Either FS resolves the file into the graph at its real on-disk spelling.
        assert_eq!(outcome.path, Some(dir.path().join("scripts/templates.R")));
        // The regime is host-derived: case-insensitive FS folded the lookup
        // (INFO), case-sensitive FS resolved via the single-ci-match branch
        // (WARNING).
        let expected = if host_is_case_sensitive() {
            CaseMismatchRegime::CaseSensitiveFs
        } else {
            CaseMismatchRegime::CaseInsensitiveFs
        };
        assert_eq!(outcome.case_mismatch, Some(expected));
    }

    #[test]
    fn resolve_source_path_rich_missing_file_no_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("scripts")).unwrap();
        let ctx = make_context(
            dir.path().join("main.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_source_path_rich("scripts/ghost.R", &ctx);
        // A genuinely-missing file still returns a lexical path (for the
        // unresolved-source-path diagnostic) but no case mismatch.
        assert_eq!(outcome.path, Some(dir.path().join("scripts/ghost.R")));
        assert_eq!(outcome.case_mismatch, None);
    }

    #[test]
    fn resolve_source_path_rich_handles_parent_relative_case_mismatch() {
        // `source("../child.r")` from <ws>/R/main.R where the real file is
        // <ws>/child.R: the normalized candidate escapes `base` (<ws>/R) but
        // stays inside the workspace, so case-correction must fall back to the
        // workspace root as its trusted prefix. The cascade fix (and the
        // diagnostic) must reach `../` paths too, in both FS regimes.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("child.R"), "").unwrap();
        std::fs::create_dir(dir.path().join("R")).unwrap();
        let ctx = make_context(
            dir.path().join("R").join("main.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_source_path_rich("../child.r", &ctx);
        assert_eq!(outcome.path, Some(dir.path().join("child.R")));
        let expected = if host_is_case_sensitive() {
            CaseMismatchRegime::CaseSensitiveFs
        } else {
            CaseMismatchRegime::CaseInsensitiveFs
        };
        assert_eq!(outcome.case_mismatch, Some(expected));
    }

    #[test]
    fn backward_resolution_resolves_single_ci_match_on_case_sensitive_fs() {
        // Issue #535: backward directives (try_workspace_fallback = false) now gain
        // the single-case-insensitive-match leniency on a case-sensitive FS — a
        // wrong-cased `parent.r` resolves to the real on-disk `Parent.R` and carries
        // the CaseSensitiveFs signal — WITHOUT gaining the workspace-root fallback.
        if !host_is_case_sensitive() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Parent.R"), "").unwrap();
        let ctx = make_context(
            dir.path().join("child.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_path_rich("parent.r", &ctx, false);
        assert_eq!(outcome.path, Some(dir.path().join("Parent.R")));
        assert_eq!(
            outcome.case_mismatch,
            Some(CaseMismatchRegime::CaseSensitiveFs)
        );
    }

    #[test]
    fn backward_resolution_does_not_gain_workspace_root_fallback() {
        // Issue #535 decouples case-leniency from the workspace-root fallback. The
        // fallback must stay forward-only: a backward path that exists ONLY at the
        // workspace root (not relative to the file's directory) must NOT resolve.
        let dir = tempfile::tempdir().unwrap();
        // Real file lives at <ws>/parent.R; the child is in <ws>/sub, so the path
        // "parent.R" resolves (file-relative) to <ws>/sub/parent.R, which is absent.
        std::fs::write(dir.path().join("parent.R"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let ctx = make_context(
            dir.path().join("sub").join("child.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        // Backward (false): no workspace-root fallback → stays unresolved (lexical).
        let backward = resolve_path_rich("parent.R", &ctx, false);
        assert_eq!(backward.path, Some(dir.path().join("sub").join("parent.R")));
        assert_eq!(backward.case_mismatch, None);
        // Forward (true): workspace-root fallback resolves it at <ws>/parent.R.
        let forward = resolve_path_rich("parent.R", &ctx, true);
        assert_eq!(forward.path, Some(dir.path().join("parent.R")));
    }

    #[test]
    fn backward_resolution_leaves_ambiguous_match_unresolved() {
        // 2+ case-insensitive matches and no exact → stays lexical/unresolved for
        // backward too (no silent pick), mirroring the forward rule.
        if !host_is_case_sensitive() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("parent.R"), "").unwrap();
        std::fs::write(dir.path().join("parent.r"), "").unwrap();
        let ctx = make_context(
            dir.path().join("child.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_path_rich("Parent.R", &ctx, false);
        assert_eq!(outcome.path, Some(dir.path().join("Parent.R")));
        assert_eq!(outcome.case_mismatch, None);
    }

    #[test]
    fn resolve_backward_path_rich_flags_case_only_mismatch() {
        // The public backward rich resolver carries the case-mismatch signal (the
        // bare `resolve_path` still discards it). Host-derived regime, mirroring
        // `resolve_source_path_rich_flags_case_only_mismatch`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Parent.R"), "").unwrap();
        let ctx = make_context(
            dir.path().join("child.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_backward_path_rich("parent.r", &ctx);
        assert_eq!(outcome.path, Some(dir.path().join("Parent.R")));
        let expected = if host_is_case_sensitive() {
            CaseMismatchRegime::CaseSensitiveFs
        } else {
            CaseMismatchRegime::CaseInsensitiveFs
        };
        assert_eq!(outcome.case_mismatch, Some(expected));
    }

    #[test]
    fn resolve_backward_path_rich_no_mismatch_on_exact() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Parent.R"), "").unwrap();
        let ctx = make_context(
            dir.path().join("child.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_backward_path_rich("Parent.R", &ctx);
        assert_eq!(outcome.path, Some(dir.path().join("Parent.R")));
        assert_eq!(outcome.case_mismatch, None);
    }

    #[test]
    fn resolve_backward_path_rich_missing_file_no_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_context(
            dir.path().join("child.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_backward_path_rich("ghost.R", &ctx);
        assert_eq!(outcome.path, Some(dir.path().join("ghost.R")));
        assert_eq!(outcome.case_mismatch, None);
    }

    #[test]
    fn resolve_backward_path_rich_handles_parent_relative_case_mismatch() {
        // `# raven: sourced-by ../main.r` from <ws>/R/child.R where the real file is
        // <ws>/main.R: the candidate escapes `base` (<ws>/R) but stays in the
        // workspace, so case-correction uses the workspace-root *prefix*
        // (`case_scan_prefix`) — which is NOT the workspace-root *fallback*. Must
        // resolve for backward too, in both FS regimes.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.R"), "").unwrap();
        std::fs::create_dir(dir.path().join("R")).unwrap();
        let ctx = make_context(
            dir.path().join("R").join("child.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_backward_path_rich("../main.r", &ctx);
        assert_eq!(outcome.path, Some(dir.path().join("main.R")));
        let expected = if host_is_case_sensitive() {
            CaseMismatchRegime::CaseSensitiveFs
        } else {
            CaseMismatchRegime::CaseInsensitiveFs
        };
        assert_eq!(outcome.case_mismatch, Some(expected));
    }

    #[test]
    fn forward_resolution_resolves_single_ci_match_on_case_sensitive_fs() {
        if !host_is_case_sensitive() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Parent.R"), "").unwrap();
        let ctx = make_context(
            dir.path().join("child.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_path_rich("parent.r", &ctx, true);
        assert_eq!(outcome.path, Some(dir.path().join("Parent.R")));
        assert_eq!(
            outcome.case_mismatch,
            Some(CaseMismatchRegime::CaseSensitiveFs)
        );
    }

    #[test]
    fn forward_resolution_leaves_ambiguous_match_unresolved() {
        // 2+ case-insensitive matches and no exact → stays lexical/unresolved
        // (no silent pick of an arbitrary one).
        if !host_is_case_sensitive() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("parent.R"), "").unwrap();
        std::fs::write(dir.path().join("parent.r"), "").unwrap();
        let ctx = make_context(
            dir.path().join("child.R").to_str().unwrap(),
            Some(dir.path().to_str().unwrap()),
        );
        let outcome = resolve_path_rich("Parent.R", &ctx, true);
        assert_eq!(outcome.path, Some(dir.path().join("Parent.R")));
        assert_eq!(outcome.case_mismatch, None);
    }

    fn make_context(file: &str, workspace: Option<&str>) -> PathContext {
        PathContext {
            file_path: PathBuf::from(file),
            working_directory: None,
            application_working_directory: None,
            inherited_working_directory: None,
            workspace_root: workspace.map(PathBuf::from),
            implicit_test_working_directory: None,
        }
    }

    #[test]
    fn test_resolve_relative_path() {
        let ctx = make_context("/project/src/main.R", Some("/project"));
        let resolved = resolve_path("utils.R", &ctx).unwrap();
        assert_eq!(resolved, PathBuf::from("/project/src/utils.R"));
    }

    #[test]
    fn system_file_rejects_escaping_components() {
        // Parent-dir, absolute, and empty parts must not resolve — otherwise a
        // crafted `system.file("..", ...)` could read files outside the package.
        let lib = vec![PathBuf::from("/lib")];
        assert_eq!(
            resolve_system_file(
                &["..".into(), "..".into(), "secret.R".into()],
                "pkg",
                None,
                None,
                &lib,
            ),
            None
        );
        assert_eq!(
            resolve_system_file(&["/etc/passwd".into()], "pkg", None, None, &lib),
            None
        );
        assert_eq!(resolve_system_file(&[], "pkg", None, None, &lib), None);
        // A normal relative component still resolves via the workspace branch
        // (which returns the candidate path even when the file doesn't exist).
        assert_eq!(
            resolve_system_file(
                &["extdata".into(), "x.R".into()],
                "pkg",
                Some("pkg"),
                Some(Path::new("/ws")),
                &lib,
            ),
            Some(PathBuf::from("/ws/inst/extdata/x.R"))
        );
    }

    #[test]
    fn shiny_application_wd_anchors_forward_paths_and_explicit_cd_wins() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("R")).unwrap();
        std::fs::create_dir(root.join("override")).unwrap();
        std::fs::write(root.join("from-app.R"), "").unwrap();
        std::fs::write(root.join("override/from-override.R"), "").unwrap();
        let helper_uri = Url::from_file_path(root.join("R/helper.R")).unwrap();
        let workspace = Url::from_directory_path(root).unwrap();

        let mut metadata = CrossFileMetadata {
            application_working_directory: Some(root.to_string_lossy().into_owned()),
            ..CrossFileMetadata::default()
        };
        let app_context =
            PathContext::from_metadata(&helper_uri, &metadata, Some(&workspace)).unwrap();
        assert!(app_context.cd_in_effect());
        assert_eq!(
            resolve_path_with_workspace_fallback("from-app.R", &app_context),
            Some(root.join("from-app.R"))
        );
        assert_eq!(
            app_context.forward_completion_base_directories(),
            [root.to_path_buf()]
        );

        metadata.working_directory = Some("/override".to_string());
        let explicit =
            PathContext::from_metadata(&helper_uri, &metadata, Some(&workspace)).unwrap();
        assert_eq!(explicit.application_working_directory, None);
        assert_eq!(
            resolve_path_with_workspace_fallback("from-override.R", &explicit),
            Some(root.join("override/from-override.R"))
        );

        let backward = PathContext::new(&helper_uri, Some(&workspace)).unwrap();
        assert_eq!(backward.application_working_directory, None);
        assert_eq!(backward.effective_working_directory(), root.join("R"));
    }

    #[test]
    fn shiny_application_wd_propagates_until_source_chdir_true() {
        let parent = make_context("/app/R/helper.R", Some("/app"));
        let parent = PathContext {
            application_working_directory: Some(PathBuf::from("/app")),
            ..parent
        };
        assert_eq!(
            parent.forward_child_inherited_wd(Path::new("/app/nested/child.R"), false),
            Some(PathBuf::from("/app"))
        );
        assert_eq!(
            parent.forward_child_inherited_wd(Path::new("/app/nested/child.R"), true),
            Some(PathBuf::from("/app/nested"))
        );
    }

    /// from_metadata for a file under tests/testthat with no cd: implicit
    /// anchor set, effective WD is the anchor, and cd_in_effect stays false
    /// (issue #638 soft-default semantics).
    #[test]
    fn implicit_testthat_wd_set_for_testthat_file() {
        let uri = Url::from_file_path("/proj/tests/testthat/helper-x.R").unwrap();
        let root = Url::from_file_path("/proj").unwrap();
        let ctx =
            PathContext::from_metadata(&uri, &CrossFileMetadata::default(), Some(&root)).unwrap();
        assert_eq!(
            ctx.implicit_test_working_directory,
            Some(PathBuf::from("/proj/tests/testthat"))
        );
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/proj/tests/testthat")
        );
        assert!(!ctx.cd_in_effect());
    }

    /// A file nested below the anchor still anchors at tests/testthat (not
    /// its own subdirectory), and exposes the file-dir compat fallback.
    #[test]
    fn implicit_testthat_wd_nested_file_anchors_at_testthat() {
        let uri = Url::from_file_path("/proj/tests/testthat/fixtures/helper.R").unwrap();
        let root = Url::from_file_path("/proj").unwrap();
        let ctx =
            PathContext::from_metadata(&uri, &CrossFileMetadata::default(), Some(&root)).unwrap();
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/proj/tests/testthat")
        );
        assert_eq!(
            ctx.implicit_wd_file_dir_fallback(),
            Some(PathBuf::from("/proj/tests/testthat/fixtures"))
        );
        assert_eq!(
            ctx.forward_completion_base_directories(),
            vec![
                PathBuf::from("/proj/tests/testthat"),
                PathBuf::from("/proj/tests/testthat/fixtures"),
                PathBuf::from("/proj"),
            ]
        );
    }

    /// For a file directly in the anchor directory the compat fallback is
    /// None (bases coincide).
    #[test]
    fn implicit_testthat_wd_no_fallback_when_file_dir_is_anchor() {
        let uri = Url::from_file_path("/proj/tests/testthat/test-a.R").unwrap();
        let root = Url::from_file_path("/proj").unwrap();
        let ctx =
            PathContext::from_metadata(&uri, &CrossFileMetadata::default(), Some(&root)).unwrap();
        assert_eq!(ctx.implicit_wd_file_dir_fallback(), None);
    }

    /// An explicit `# raven: cd` suppresses the implicit anchor entirely.
    #[test]
    fn explicit_cd_overrides_implicit_testthat_wd() {
        let uri = Url::from_file_path("/proj/tests/testthat/helper-x.R").unwrap();
        let root = Url::from_file_path("/proj").unwrap();
        let meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };
        let ctx = PathContext::from_metadata(&uri, &meta, Some(&root)).unwrap();
        assert_eq!(ctx.implicit_test_working_directory, None);
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/proj/data")
        );
        assert!(ctx.cd_in_effect());
        assert_eq!(
            ctx.forward_completion_base_directories(),
            vec![PathBuf::from("/proj/data")]
        );
    }

    /// An inherited cd also suppresses the implicit anchor.
    #[test]
    fn inherited_cd_overrides_implicit_testthat_wd() {
        let uri = Url::from_file_path("/proj/tests/testthat/helper-x.R").unwrap();
        let root = Url::from_file_path("/proj").unwrap();
        let meta = CrossFileMetadata {
            inherited_working_directory: Some("/proj/scripts".to_string()),
            ..Default::default()
        };
        let ctx = PathContext::from_metadata(&uri, &meta, Some(&root)).unwrap();
        assert_eq!(ctx.implicit_test_working_directory, None);
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/proj/scripts")
        );
        assert_eq!(
            ctx.forward_completion_base_directories(),
            vec![PathBuf::from("/proj/scripts")]
        );
    }

    /// testit files anchor at tests/testit; plain tests/ scripts and files
    /// outside the workspace get no implicit anchor.
    #[test]
    fn implicit_wd_layout_gating() {
        let root = Url::from_file_path("/proj").unwrap();
        let testit = Url::from_file_path("/proj/tests/testit/test-a.R").unwrap();
        let ctx = PathContext::from_metadata(&testit, &CrossFileMetadata::default(), Some(&root))
            .unwrap();
        assert_eq!(
            ctx.implicit_test_working_directory,
            Some(PathBuf::from("/proj/tests/testit"))
        );

        let plain = Url::from_file_path("/proj/tests/smoke.R").unwrap();
        let ctx =
            PathContext::from_metadata(&plain, &CrossFileMetadata::default(), Some(&root)).unwrap();
        assert_eq!(ctx.implicit_test_working_directory, None);

        let outside = Url::from_file_path("/other/tests/testthat/test-a.R").unwrap();
        let ctx = PathContext::from_metadata(&outside, &CrossFileMetadata::default(), Some(&root))
            .unwrap();
        assert_eq!(ctx.implicit_test_working_directory, None);

        // No workspace root → no anchor.
        let uri = Url::from_file_path("/proj/tests/testthat/test-a.R").unwrap();
        let ctx = PathContext::from_metadata(&uri, &CrossFileMetadata::default(), None).unwrap();
        assert_eq!(ctx.implicit_test_working_directory, None);
    }

    /// Backward-directive contexts (PathContext::new) never carry the
    /// implicit anchor; forward_without_metadata does.
    #[test]
    fn implicit_wd_forward_only_constructors() {
        let uri = Url::from_file_path("/proj/tests/testthat/helper-x.R").unwrap();
        let root = Url::from_file_path("/proj").unwrap();
        let backward = PathContext::new(&uri, Some(&root)).unwrap();
        assert_eq!(backward.implicit_test_working_directory, None);
        assert_eq!(
            backward.effective_working_directory(),
            PathBuf::from("/proj/tests/testthat")
        );

        let forward = PathContext::forward_without_metadata(&uri, Some(&root)).unwrap();
        assert_eq!(
            forward.implicit_test_working_directory,
            Some(PathBuf::from("/proj/tests/testthat"))
        );
    }

    /// The implicit anchor never propagates into a forward-sourced child
    /// (soft default: forward_child_inherited_wd keys off cd_in_effect).
    #[test]
    fn implicit_wd_never_propagates_to_children() {
        let uri = Url::from_file_path("/proj/tests/testthat/helper-x.R").unwrap();
        let root = Url::from_file_path("/proj").unwrap();
        let ctx =
            PathContext::from_metadata(&uri, &CrossFileMetadata::default(), Some(&root)).unwrap();
        assert_eq!(
            ctx.forward_child_inherited_wd(Path::new("/proj/scripts/helpers.R"), false),
            None
        );
    }

    /// End-to-end resolution chain on a real directory tree: implicit anchor
    /// first, file-dir compat fallback second, workspace-root fallback third
    /// (the anchor never suppresses the workspace fallback).
    #[test]
    fn implicit_wd_resolution_chain_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("tests/testthat/fixtures")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("tests/testthat/helper-common.R"), "x <- 1\n").unwrap();
        std::fs::write(root.join("tests/testthat/fixtures/local.R"), "y <- 1\n").unwrap();
        std::fs::write(
            root.join("tests/testthat/fixtures/CaseOnly.R"),
            "case_only <- 1\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tests/testthat/fixtures/Priority.R"),
            "compatibility_ci <- 1\n",
        )
        .unwrap();
        std::fs::write(root.join("priority.r"), "workspace_exact <- 1\n").unwrap();
        std::fs::write(root.join("scripts/helpers.R"), "z <- 1\n").unwrap();
        let nested = root.join("tests/testthat/fixtures/helper.R");
        std::fs::write(&nested, "").unwrap();

        let uri = Url::from_file_path(&nested).unwrap();
        let root_url = Url::from_file_path(root).unwrap();
        let ctx = PathContext::from_metadata(&uri, &CrossFileMetadata::default(), Some(&root_url))
            .unwrap();

        // Anchor-relative (testthat runtime behavior).
        assert_eq!(
            resolve_path_with_workspace_fallback("helper-common.R", &ctx),
            Some(root.join("tests/testthat/helper-common.R"))
        );
        // File-dir compat fallback for a path that only exists next to the
        // nested file (pre-#638 behavior preserved).
        assert_eq!(
            resolve_path_with_workspace_fallback("local.R", &ctx),
            Some(root.join("tests/testthat/fixtures/local.R"))
        );
        // The compatibility candidate gets the same unique case-insensitive
        // pass as the anchor and workspace-root candidates.
        assert_eq!(
            resolve_path_with_workspace_fallback("caseonly.r", &ctx),
            Some(root.join("tests/testthat/fixtures/CaseOnly.R"))
        );
        // Exact matches across every candidate precede case-insensitive
        // leniency: a workspace exact match beats a compatibility CI match.
        if !root.join("tests/testthat/fixtures/priority.r").exists() {
            // Meaningful on case-sensitive filesystems. On case-insensitive
            // filesystems `exists()` intentionally treats Priority.R as the
            // compatibility base's exact-phase match.
            assert_eq!(
                resolve_path_with_workspace_fallback("priority.r", &ctx),
                Some(root.join("priority.r"))
            );
        }
        // Workspace-root fallback still active despite the implicit anchor.
        assert_eq!(
            resolve_path_with_workspace_fallback("scripts/helpers.R", &ctx),
            Some(root.join("scripts/helpers.R"))
        );
        // The issue's folded "../../scripts/helpers.R" from a directly-placed
        // helper resolves via the anchor.
        let helper = root.join("tests/testthat/helper-project.R");
        std::fs::write(&helper, "").unwrap();
        let helper_uri = Url::from_file_path(&helper).unwrap();
        let helper_ctx =
            PathContext::from_metadata(&helper_uri, &CrossFileMetadata::default(), Some(&root_url))
                .unwrap();
        assert_eq!(
            resolve_path_with_workspace_fallback("../../scripts/helpers.R", &helper_ctx),
            Some(root.join("scripts/helpers.R"))
        );
    }

    #[test]
    fn test_resolve_parent_directory() {
        let ctx = make_context("/project/src/main.R", Some("/project"));
        let resolved = resolve_path("../data/input.R", &ctx).unwrap();
        assert_eq!(resolved, PathBuf::from("/project/data/input.R"));
    }

    #[test]
    fn test_resolve_workspace_root_relative() {
        let ctx = make_context("/project/src/main.R", Some("/project"));
        let resolved = resolve_path("/data/input.R", &ctx).unwrap();
        assert_eq!(resolved, PathBuf::from("/project/data/input.R"));
    }

    #[test]
    fn test_resolve_workspace_root_relative_no_workspace() {
        let ctx = make_context("/project/src/main.R", None);
        let resolved = resolve_path("/data/input.R", &ctx);
        assert!(resolved.is_none());
    }

    #[test]
    fn test_effective_working_directory_default() {
        let ctx = make_context("/project/src/main.R", Some("/project"));
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/project/src")
        );
    }

    #[test]
    fn test_effective_working_directory_explicit() {
        let mut ctx = make_context("/project/src/main.R", Some("/project"));
        ctx.working_directory = Some(PathBuf::from("/project/data"));
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/project/data")
        );
    }

    #[test]
    fn test_effective_working_directory_inherited() {
        let mut ctx = make_context("/project/src/main.R", Some("/project"));
        ctx.inherited_working_directory = Some(PathBuf::from("/project/scripts"));
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/project/scripts")
        );
    }

    #[test]
    fn test_child_context_with_chdir() {
        let ctx = make_context("/project/src/main.R", Some("/project"));
        let child = ctx.child_context_with_chdir(Path::new("/project/data/utils.R"));
        assert_eq!(
            child.effective_working_directory(),
            PathBuf::from("/project/data")
        );
    }

    #[test]
    fn test_child_context_without_chdir() {
        let ctx = make_context("/project/src/main.R", Some("/project"));
        let child = ctx.child_context(Path::new("/project/data/utils.R"));
        // Inherits parent's effective working directory
        assert_eq!(
            child.effective_working_directory(),
            PathBuf::from("/project/src")
        );
    }

    #[test]
    fn test_resolve_working_directory_relative() {
        let ctx = make_context("/project/src/main.R", Some("/project"));
        let resolved = resolve_working_directory("../data", &ctx).unwrap();
        assert_eq!(resolved, PathBuf::from("/project/data"));
    }

    #[test]
    fn test_resolve_working_directory_workspace_relative() {
        let ctx = make_context("/project/src/main.R", Some("/project"));
        let resolved = resolve_working_directory("/data/scripts", &ctx).unwrap();
        assert_eq!(resolved, PathBuf::from("/project/data/scripts"));
    }

    #[test]
    fn test_from_metadata_with_working_directory() {
        use super::super::types::CrossFileMetadata;

        let file_uri = Url::parse("file:///project/src/main.R").unwrap();
        let workspace_uri = Url::parse("file:///project").unwrap();

        let meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()),
            ..Default::default()
        };

        let ctx = PathContext::from_metadata(&file_uri, &meta, Some(&workspace_uri)).unwrap();
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/project/data")
        );
    }

    #[test]
    fn test_from_metadata_relative_working_directory() {
        use super::super::types::CrossFileMetadata;

        let file_uri = Url::parse("file:///project/src/main.R").unwrap();
        let workspace_uri = Url::parse("file:///project").unwrap();

        let meta = CrossFileMetadata {
            working_directory: Some("../data".to_string()),
            ..Default::default()
        };

        let ctx = PathContext::from_metadata(&file_uri, &meta, Some(&workspace_uri)).unwrap();
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/project/data")
        );
    }

    // Tests for inherited_working_directory in from_metadata
    // Validates: Requirements 6.2, 6.3, 3.1

    #[test]
    fn test_from_metadata_with_inherited_working_directory() {
        // Validates: Requirements 6.2, 6.3
        // When metadata has inherited_working_directory and no explicit working_directory,
        // the PathContext should use the inherited working directory.
        // Inherited working directories are stored as absolute paths.
        use super::super::types::CrossFileMetadata;

        let file_uri = Url::parse("file:///project/src/child.R").unwrap();
        let workspace_uri = Url::parse("file:///project").unwrap();

        let meta = CrossFileMetadata {
            working_directory: None,
            application_working_directory: None,
            inherited_working_directory: Some("/project/data".to_string()),
            ..Default::default()
        };

        let ctx = PathContext::from_metadata(&file_uri, &meta, Some(&workspace_uri)).unwrap();

        // Absolute inherited paths are used directly
        assert!(ctx.working_directory.is_none());
        assert_eq!(
            ctx.inherited_working_directory,
            Some(PathBuf::from("/project/data"))
        );
        // Effective working directory should use inherited
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/project/data")
        );
    }

    #[test]
    fn test_from_metadata_inherited_working_directory_relative_path() {
        // Validates: Requirements 6.2, 6.3
        // Inherited working directory with relative path should resolve relative to file's directory
        use super::super::types::CrossFileMetadata;

        let file_uri = Url::parse("file:///project/src/child.R").unwrap();
        let workspace_uri = Url::parse("file:///project").unwrap();

        let meta = CrossFileMetadata {
            working_directory: None,
            application_working_directory: None,
            inherited_working_directory: Some("../data".to_string()),
            ..Default::default()
        };

        let ctx = PathContext::from_metadata(&file_uri, &meta, Some(&workspace_uri)).unwrap();

        assert!(ctx.working_directory.is_none());
        assert_eq!(
            ctx.inherited_working_directory,
            Some(PathBuf::from("/project/data"))
        );
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/project/data")
        );
    }

    #[test]
    fn test_from_metadata_explicit_working_directory_takes_precedence() {
        // Validates: Requirements 3.1
        // When both explicit and inherited working directories are present,
        // explicit should take precedence
        use super::super::types::CrossFileMetadata;

        let file_uri = Url::parse("file:///project/src/child.R").unwrap();
        let workspace_uri = Url::parse("file:///project").unwrap();

        let meta = CrossFileMetadata {
            working_directory: Some("/explicit".to_string()),
            inherited_working_directory: Some("/inherited".to_string()),
            ..Default::default()
        };

        let ctx = PathContext::from_metadata(&file_uri, &meta, Some(&workspace_uri)).unwrap();

        // Explicit working directory should be set
        assert_eq!(
            ctx.working_directory,
            Some(PathBuf::from("/project/explicit"))
        );
        // Inherited should NOT be set when explicit is present
        assert!(ctx.inherited_working_directory.is_none());
        // Effective should use explicit
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/project/explicit")
        );
    }

    #[test]
    fn test_from_metadata_no_working_directories() {
        // When neither explicit nor inherited working directory is set,
        // effective working directory should be file's directory
        use super::super::types::CrossFileMetadata;

        let file_uri = Url::parse("file:///project/src/child.R").unwrap();
        let workspace_uri = Url::parse("file:///project").unwrap();

        let meta = CrossFileMetadata {
            working_directory: None,
            application_working_directory: None,
            inherited_working_directory: None,
            ..Default::default()
        };

        let ctx = PathContext::from_metadata(&file_uri, &meta, Some(&workspace_uri)).unwrap();

        assert!(ctx.working_directory.is_none());
        assert!(ctx.inherited_working_directory.is_none());
        // Effective should fall back to file's directory
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/project/src")
        );
    }

    #[test]
    fn test_from_metadata_inherited_working_directory_absolute_path() {
        // Validates: Requirements 6.2, 6.3
        // Inherited working directories are stored as absolute paths, so absolute
        // paths should be used directly without re-resolution
        use super::super::types::CrossFileMetadata;

        let file_uri = Url::parse("file:///project/src/child.R").unwrap();
        let workspace_uri = Url::parse("file:///project").unwrap();

        let meta = CrossFileMetadata {
            working_directory: None,
            application_working_directory: None,
            inherited_working_directory: Some("/absolute/path".to_string()),
            ..Default::default()
        };

        let ctx = PathContext::from_metadata(&file_uri, &meta, Some(&workspace_uri)).unwrap();

        // Absolute inherited paths are used directly (not re-resolved as workspace-relative)
        assert_eq!(
            ctx.inherited_working_directory,
            Some(PathBuf::from("/absolute/path"))
        );
        assert_eq!(
            ctx.effective_working_directory(),
            PathBuf::from("/absolute/path")
        );
    }

    #[test]
    fn test_child_context_for_source_with_chdir() {
        let ctx = make_context("/project/src/main.R", Some("/project"));
        let child = ctx.child_context_for_source(Path::new("/project/data/utils.R"), true);
        assert_eq!(
            child.effective_working_directory(),
            PathBuf::from("/project/data")
        );
    }

    #[test]
    fn test_child_context_for_source_without_chdir() {
        let ctx = make_context("/project/src/main.R", Some("/project"));
        let child = ctx.child_context_for_source(Path::new("/project/data/utils.R"), false);
        assert_eq!(
            child.effective_working_directory(),
            PathBuf::from("/project/src")
        );
    }

    // Tests for normalize_path ParentDir handling (Requirements 4.1-4.4)
    #[test]
    fn test_normalize_path_preserves_root_with_parent_dir() {
        // "/../a" should produce "/a", not "a"
        let path = Path::new("/../a");
        let result = normalize_path(path).unwrap();
        assert_eq!(result, PathBuf::from("/a"));
    }

    #[test]
    fn test_normalize_path_normal_parent_dir() {
        // "/a/../b" should produce "/b"
        let path = Path::new("/a/../b");
        let result = normalize_path(path).unwrap();
        assert_eq!(result, PathBuf::from("/b"));
    }

    #[test]
    fn test_normalize_path_relative_parent_dir() {
        // "a/../b" should produce "b"
        let path = Path::new("a/../b");
        let result = normalize_path(path).unwrap();
        assert_eq!(result, PathBuf::from("b"));
    }

    #[test]
    fn test_normalize_path_multiple_parent_dirs() {
        // "/a/b/../../c" should produce "/c"
        let path = Path::new("/a/b/../../c");
        let result = normalize_path(path).unwrap();
        assert_eq!(result, PathBuf::from("/c"));
    }

    #[test]
    fn test_normalize_path_leading_parent_dirs() {
        // "/../../../a" should produce "/a" (can't go above root)
        let path = Path::new("/../../../a");
        let result = normalize_path(path).unwrap();
        assert_eq!(result, PathBuf::from("/a"));
    }

    // ==================== system.file resolution tests ====================

    #[test]
    fn test_resolve_system_file_same_package_inst_prefix() {
        // Same-package: workspace "Matrix" with inst/test-tools.R → resolved via inst/
        let tmp = tempfile::tempdir().unwrap();
        let ws_root = tmp.path();
        std::fs::create_dir_all(ws_root.join("inst")).unwrap();
        std::fs::write(ws_root.join("inst/test-tools.R"), "f <- 1").unwrap();

        let result = resolve_system_file(
            &["test-tools.R".to_string()],
            "Matrix",
            Some("Matrix"),
            Some(ws_root),
            &[],
        );
        assert_eq!(result, Some(ws_root.join("inst/test-tools.R")));
    }

    #[test]
    fn test_resolve_system_file_installed_cross_package_no_inst() {
        // Installed cross-package: lib_path contains P/helper.R (no inst/ prefix)
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path();
        std::fs::create_dir_all(lib.join("P")).unwrap();
        std::fs::write(lib.join("P/helper.R"), "g <- 2").unwrap();

        let result = resolve_system_file(
            &["helper.R".to_string()],
            "P",
            Some("MyPkg"),
            Some(Path::new("/fake/ws")),
            &[lib.to_path_buf()],
        );
        assert_eq!(result, Some(lib.join("P/helper.R")));
    }

    #[test]
    fn test_resolve_system_file_multi_part_join() {
        // Multi-part: system.file("a", "b.R", package = "P") → P/a/b.R
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path();
        std::fs::create_dir_all(lib.join("P/a")).unwrap();
        std::fs::write(lib.join("P/a/b.R"), "h <- 3").unwrap();

        let result = resolve_system_file(
            &["a".to_string(), "b.R".to_string()],
            "P",
            None,
            None,
            &[lib.to_path_buf()],
        );
        assert_eq!(result, Some(lib.join("P/a/b.R")));
    }

    #[test]
    fn test_resolve_system_file_unresolved_fallback() {
        // Package neither self nor installed → None, no panic
        let result = resolve_system_file(
            &["helper.R".to_string()],
            "NonExistent",
            Some("MyPkg"),
            Some(Path::new("/fake/ws")),
            &[PathBuf::from("/no/such/lib")],
        );
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_system_file_sources_integration() {
        // resolve_system_file_sources sets the workspace-root-relative path and
        // retains system_file so later events can re-resolve.
        use super::super::source_detect::SystemFileCall;
        use super::super::types::{CrossFileMetadata, ForwardSource};

        let tmp = tempfile::tempdir().unwrap();
        let ws_root = tmp.path();
        std::fs::create_dir_all(ws_root.join("inst")).unwrap();
        std::fs::write(ws_root.join("inst/helper.R"), "x <- 1").unwrap();

        let mut meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                system_file: Some(SystemFileCall {
                    parts: vec!["helper.R".to_string()],
                    package: "mypkg".to_string(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        resolve_system_file_sources(&mut meta, Some("mypkg"), Some(ws_root), &[]);

        assert_eq!(meta.sources[0].path, "/inst/helper.R");
        assert!(
            meta.sources[0].system_file.is_some(),
            "system_file must be retained so a Package: rename can re-resolve"
        );
    }

    #[test]
    fn test_resolve_system_file_sources_unresolved_retained() {
        // Unresolved (cross-package, not installed) entries are RETAINED even
        // when lib_paths is non-empty (resolution attempted and failed), so a
        // later package-install event can re-resolve them. They stay inert:
        // empty path, no resolved_uri.
        use super::super::source_detect::SystemFileCall;
        use super::super::types::{CrossFileMetadata, ForwardSource};

        let tmp = tempfile::tempdir().unwrap();
        let ws_root = tmp.path();
        // A lib_path that exists but doesn't contain the package
        let lib_dir = tempfile::tempdir().unwrap();

        let mut meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                system_file: Some(SystemFileCall {
                    parts: vec!["setup.R".to_string()],
                    package: "otherpkg".to_string(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        resolve_system_file_sources(
            &mut meta,
            Some("mypkg"),
            Some(ws_root),
            &[lib_dir.path().to_path_buf()],
        );

        assert_eq!(
            meta.sources.len(),
            1,
            "Unresolved entries must be retained for later re-resolution"
        );
        assert!(meta.sources[0].system_file.is_some());
        assert!(meta.sources[0].path.is_empty());
        assert!(meta.sources[0].resolved_uri.is_none());
    }

    // ---- P7: system.file edge re-resolution after a library swap ----
    //
    // Simulates the scenario in system-file convergence: a
    // `ForwardSource` that was previously left with `system_file.is_some()`
    // (lib_paths was empty at index time) is re-resolved once a new
    // `package_library` with non-empty lib_paths is available.
    //
    // The test directly exercises `resolve_system_file_sources` — the same
    // function called by system-file convergence — with the "before
    // swap" (empty lib_paths, entry stays) and "after swap" (lib_paths now
    // point at the installed package, entry resolves) states.

    /// After a library swap that populates lib_paths, an unresolved
    /// `system_file` source is resolved to the installed path, `resolved_uri`
    /// is set, and `system_file` is retained for future re-resolution.
    #[test]
    fn system_file_re_resolved_after_library_swap() {
        use super::super::source_detect::SystemFileCall;
        use super::super::types::{CrossFileMetadata, ForwardSource};

        let libdir = tempfile::tempdir().unwrap();
        // "otherpkg" installed at libdir/otherpkg/helper.R (installed layout:
        // no inst/ prefix).
        let pkg_dir = libdir.path().join("otherpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();

        // --- Step 1: initial call with empty lib_paths (before swap) ---
        // Entry must survive (deferred): system_file stays Some, source not
        // dropped.
        let mut meta_before = CrossFileMetadata {
            sources: vec![ForwardSource {
                system_file: Some(SystemFileCall {
                    parts: vec!["helper.R".to_string()],
                    package: "otherpkg".to_string(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        resolve_system_file_sources(&mut meta_before, Some("mypkg"), None, &[]);
        assert_eq!(
            meta_before.sources.len(),
            1,
            "With empty lib_paths the entry must be kept (deferred) for a later retry"
        );
        assert!(
            meta_before.sources[0].system_file.is_some(),
            "system_file must remain Some when lib_paths is empty (deferred)"
        );
        assert!(
            meta_before.sources[0].resolved_uri.is_none(),
            "resolved_uri must remain None before lib_paths are available"
        );

        // --- Step 2: retry after the library swap (lib_paths now populated) ---
        // Entry must resolve: resolved_uri points into the new lib path while
        // system_file stays Some for future lifecycle events.
        let mut meta_after = meta_before.clone();
        resolve_system_file_sources(
            &mut meta_after,
            Some("mypkg"),
            None,
            &[libdir.path().to_path_buf()],
        );
        assert_eq!(
            meta_after.sources.len(),
            1,
            "Entry must survive resolution (it resolved successfully)"
        );
        assert!(
            meta_after.sources[0].system_file.is_some(),
            "system_file must be retained after successful resolution so a \
             package-removal event can re-resolve"
        );
        let resolved_uri = meta_after.sources[0]
            .resolved_uri
            .as_ref()
            .expect("resolved_uri must be set after cross-package system.file() resolution");
        let resolved_path = resolved_uri.to_file_path().unwrap();
        assert!(
            resolved_path.starts_with(libdir.path()),
            "resolved path must be inside the new lib_path. Got: {resolved_path:?}"
        );
        assert!(
            resolved_path.ends_with("otherpkg/helper.R"),
            "resolved path must point at otherpkg/helper.R. Got: {resolved_path:?}"
        );
    }

    /// Positive control for the library-swap test: a same-package `system.file()`
    /// resolves to `inst/` immediately, regardless of lib_paths.
    #[test]
    fn system_file_same_package_resolves_immediately_without_lib_paths() {
        use super::super::source_detect::SystemFileCall;
        use super::super::types::{CrossFileMetadata, ForwardSource};

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("inst")).unwrap();
        std::fs::write(tmp.path().join("inst").join("helper.R"), "x <- 1\n").unwrap();

        let mut meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                system_file: Some(SystemFileCall {
                    parts: vec!["helper.R".to_string()],
                    package: "selfpkg".to_string(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        resolve_system_file_sources(&mut meta, Some("selfpkg"), Some(tmp.path()), &[]);

        assert_eq!(meta.sources.len(), 1);
        assert!(
            meta.sources[0].path.contains("/inst/helper.R"),
            "same-package system.file() must resolve immediately (no lib_paths \
             needed); path must be set to the inst/ location, got: {:?}",
            meta.sources[0].path
        );
        assert!(
            meta.sources[0].system_file.is_some(),
            "system_file must be retained so a Package: rename can re-resolve"
        );
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    /// Strategy for generating path segments
    fn segment_strategy() -> impl Strategy<Value = String> {
        prop::string::string_regex("[a-z][a-z0-9_]{0,10}")
            .unwrap()
            .prop_filter("non-empty", |s| !s.is_empty())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 3: Path normalization preserves root
        /// For any absolute path with leading ParentDir, the root should be preserved.
        #[test]
        fn prop_normalize_preserves_root(
            num_parent_dirs in 1_usize..5,
            segments in prop::collection::vec(segment_strategy(), 1..5)
        ) {
            // Build path like "/../../../a/b/c"
            let mut path_str = String::from("/");
            for _ in 0..num_parent_dirs {
                path_str.push_str("../");
            }
            path_str.push_str(&segments.join("/"));

            let path = Path::new(&path_str);
            let result = normalize_path(path);

            // Result should exist and start with root
            prop_assert!(result.is_some());
            let normalized = result.unwrap();
            prop_assert!(normalized.is_absolute(), "Normalized path should be absolute");
        }

        /// Property 3: Normal parent dir resolution works correctly
        #[test]
        fn prop_normal_parent_dir_resolution(
            prefix_segments in prop::collection::vec(segment_strategy(), 1..3),
            suffix_segments in prop::collection::vec(segment_strategy(), 1..3)
        ) {
            // Build path like "/a/b/../c/d" where .. should cancel one segment
            let mut path_str = String::from("/");
            path_str.push_str(&prefix_segments.join("/"));
            path_str.push_str("/../");
            path_str.push_str(&suffix_segments.join("/"));

            let path = Path::new(&path_str);
            let result = normalize_path(path);

            prop_assert!(result.is_some());
            let normalized = result.unwrap();

            // The result should have one fewer segment than prefix + suffix
            let expected_segments = prefix_segments.len() - 1 + suffix_segments.len();
            let actual_segments = normalized.components()
                .filter(|c| matches!(c, std::path::Component::Normal(_)))
                .count();
            prop_assert_eq!(actual_segments, expected_segments);
        }
    }
}
