//
// cross_file/path_resolve.rs
//
// Path resolution for cross-file awareness
//
// CRITICAL DESIGN NOTE: Forward vs Backward Directive Path Resolution
// ====================================================================
// This module provides two PathContext constructors with DIFFERENT behaviors:
//
// 1. PathContext::new() - For BACKWARD directives (@lsp-sourced-by, @lsp-run-by, @lsp-included-by)
//    - IGNORES @lsp-cd working directory
//    - Always resolves paths relative to the file's own directory
//    - Rationale: Backward directives describe static file relationships from the child's
//      perspective. They declare "this file is sourced by that parent file" - a relationship
//      that should NOT change based on runtime working directory.
//
// 2. PathContext::from_metadata() - For FORWARD directives (@lsp-source, @lsp-run, @lsp-include)
//                                   and source() calls
//    - USES @lsp-cd working directory when present
//    - Resolves paths relative to the working directory (or file's directory if no @lsp-cd)
//    - Rationale: Forward directives and source() calls describe runtime execution behavior.
//      They are semantically equivalent to R's source() function, which is affected by
//      the current working directory at runtime.
//
// DO NOT change this behavior without understanding the full implications for cross-file
// awareness. See AGENTS.md "Path Resolution: Critical Distinction" for detailed documentation.
//

use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::Url;

use super::types::CrossFileMetadata;

/// Context for path resolution
#[derive(Debug, Clone)]
pub struct PathContext {
    /// Path of the current file
    pub file_path: PathBuf,
    /// Explicit working directory from directive
    pub working_directory: Option<PathBuf>,
    /// Working directory inherited from parent
    pub inherited_working_directory: Option<PathBuf>,
    /// Workspace root
    pub workspace_root: Option<PathBuf>,
}

impl PathContext {
    /// Create a new context for a file WITHOUT working directory support.
    ///
    /// **USE FOR: Backward directives only** (`@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`)
    ///
    /// This constructor creates a PathContext that resolves paths relative to the file's
    /// own directory, ignoring any `@lsp-cd` directive. This is intentional because backward
    /// directives describe static file relationships that should not change based on runtime
    /// working directory.
    ///
    /// **DO NOT USE FOR:** Forward directives (`@lsp-source`) or `source()` calls.
    /// Use `PathContext::from_metadata()` instead, which respects `@lsp-cd`.
    pub fn new(file_uri: &Url, workspace_root: Option<&Url>) -> Option<Self> {
        let file_path = file_uri.to_file_path().ok()?;
        let workspace_root = workspace_root.and_then(|u| u.to_file_path().ok());
        Some(Self {
            file_path,
            working_directory: None,
            inherited_working_directory: None,
            workspace_root,
        })
    }

    /// Create a context from a file URI and its metadata WITH working directory support.
    ///
    /// **USE FOR: Forward directives** (`@lsp-source`, `@lsp-run`, `@lsp-include`) **and source() calls**
    ///
    /// This constructor creates a PathContext that respects `@lsp-cd` working directory
    /// directives. Paths are resolved relative to the working directory (if set) or the
    /// file's directory (if no working directory). This matches R's runtime behavior where
    /// `source()` calls are affected by the current working directory.
    ///
    /// **DO NOT USE FOR:** Backward directives (`@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`).
    /// Use `PathContext::new()` instead, which ignores `@lsp-cd`.
    ///
    /// Priority for path resolution: explicit working_directory > inherited > file's directory
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

        // Apply inherited working directory if no explicit one.
        // Inherited working directories are stored as absolute paths, so use directly
        // when absolute. Only resolve if relative (legacy/edge case).
        if ctx.working_directory.is_none() {
            if let Some(ref inherited_wd) = metadata.inherited_working_directory {
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
        }

        Some(ctx)
    }

    /// Get the effective working directory for path resolution
    pub fn effective_working_directory(&self) -> PathBuf {
        // Priority: explicit > inherited > file's directory
        self.working_directory
            .clone()
            .or_else(|| self.inherited_working_directory.clone())
            .unwrap_or_else(|| {
                self.file_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| self.file_path.clone())
            })
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
            inherited_working_directory: Some(child_dir),
            workspace_root: self.workspace_root.clone(),
        }
    }

    /// Create a child context for a sourced file without chdir
    pub fn child_context(&self, child_path: &Path) -> Self {
        Self {
            file_path: child_path.to_path_buf(),
            working_directory: None,
            inherited_working_directory: Some(self.effective_working_directory()),
            workspace_root: self.workspace_root.clone(),
        }
    }

    /// Create a child context for a sourced file, respecting chdir flag
    pub fn child_context_for_source(&self, child_path: &Path, chdir: bool) -> Self {
        if chdir {
            self.child_context_with_chdir(child_path)
        } else {
            self.child_context(child_path)
        }
    }
}

/// Resolve a path string to an absolute path.
/// Handles file-relative, workspace-relative, and absolute paths with working directory context.
pub fn resolve_path(path: &str, context: &PathContext) -> Option<PathBuf> {
    resolve_path_impl(path, context, false)
}

/// Resolve a path with workspace-root fallback for source() statements.
///
/// This function first tries normal resolution (relative to file's directory or @lsp-cd).
/// If that fails AND the file has no explicit working directory directive, it falls back
/// to trying the path relative to workspace root.
///
/// This is useful for codebases that haven't been annotated with LSP directives but where
/// source() calls use paths relative to the project root (a common pattern in R projects).
///
/// Use this for source() statements. Do NOT use for backward directives (@lsp-sourced-by)
/// which should always resolve relative to the file's directory.
pub fn resolve_path_with_workspace_fallback(path: &str, context: &PathContext) -> Option<PathBuf> {
    resolve_path_impl(path, context, true)
}

/// Internal implementation of path resolution with optional workspace fallback
fn resolve_path_impl(
    path: &str,
    context: &PathContext,
    try_workspace_fallback: bool,
) -> Option<PathBuf> {
    if path.is_empty() {
        log::trace!("Path resolution: empty path provided");
        return None;
    }

    let base_dir = context.effective_working_directory();
    let working_dir = context.working_directory.as_ref();

    log::trace!(
        "Resolving path '{}' with base_dir='{}', working_dir={:?}, file_path='{}'",
        path,
        base_dir.display(),
        working_dir.as_ref().map(|p| p.display().to_string()),
        context.file_path.display()
    );

    // If path starts with /, it's explicitly workspace-root-relative
    if let Some(stripped) = path.strip_prefix('/') {
        let workspace_root = context.workspace_root.as_ref();
        if workspace_root.is_none() {
            log::warn!(
                "Failed to resolve workspace-root-relative path '{}': no workspace root available, base_dir='{}'",
                path,
                base_dir.display()
            );
            return None;
        }
        let resolved = workspace_root.unwrap().join(stripped);
        return normalize_path(&resolved).or_else(|| {
            log::warn!(
                "Failed to resolve path '{}': normalization failed, attempted_path='{}', base_dir='{}'",
                path,
                resolved.display(),
                base_dir.display()
            );
            None
        });
    }

    // Try file-relative or working-directory-relative path first
    let base = context.effective_working_directory();
    let resolved = base.join(path);

    if let Some(canonical) = normalize_path(&resolved) {
        // Check if the file exists
        if canonical.exists() {
            log::trace!(
                "Resolved path '{}' to canonical path: '{}'",
                path,
                canonical.display()
            );
            return Some(canonical);
        }

        // File doesn't exist at the resolved path
        // Try workspace-root fallback if:
        // 1. Fallback is enabled (for source() statements)
        // 2. No explicit @lsp-cd directive (working_directory is None)
        // 3. No inherited working directory
        // 4. Workspace root is available
        let has_explicit_wd = context.working_directory.is_some();
        let has_inherited_wd = context.inherited_working_directory.is_some();

        if try_workspace_fallback && !has_explicit_wd && !has_inherited_wd {
            if let Some(ref workspace_root) = context.workspace_root {
                let workspace_resolved = workspace_root.join(path);
                if let Some(workspace_canonical) = normalize_path(&workspace_resolved) {
                    if workspace_canonical.exists() {
                        log::trace!(
                            "Resolved path '{}' via workspace-root fallback: '{}' (file-relative '{}' did not exist)",
                            path,
                            workspace_canonical.display(),
                            canonical.display()
                        );
                        return Some(workspace_canonical);
                    }
                }
            }
        }

        // Return the original resolved path even if file doesn't exist
        // (caller may want to report diagnostics about missing file)
        log::trace!(
            "Resolved path '{}' to canonical path: '{}' (file may not exist)",
            path,
            canonical.display()
        );
        return Some(canonical);
    }

    log::warn!(
        "Failed to resolve path '{}': normalization failed, attempted_path='{}', base_dir='{}'",
        path,
        resolved.display(),
        base_dir.display()
    );
    None
}

/// Resolve a working directory path.
/// Used for @lsp-cd directive resolution with workspace-relative and absolute path support.
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

/// Normalize a path by resolving . and .. components
fn normalize_path(path: &Path) -> Option<PathBuf> {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                // Only pop if the last component is a Normal segment
                // Preserve RootDir and Prefix components
                if let Some(last) = components.last() {
                    if matches!(last, std::path::Component::Normal(_)) {
                        components.pop();
                    }
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

/// Convert a resolved path to a file URI.
/// Creates a file:// URI from an absolute filesystem path.
pub fn path_to_uri(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_context(file: &str, workspace: Option<&str>) -> PathContext {
        PathContext {
            file_path: PathBuf::from(file),
            working_directory: None,
            inherited_working_directory: None,
            workspace_root: workspace.map(PathBuf::from),
        }
    }

    #[test]
    fn test_resolve_relative_path() {
        let ctx = make_context("/project/src/main.R", Some("/project"));
        let resolved = resolve_path("utils.R", &ctx).unwrap();
        assert_eq!(resolved, PathBuf::from("/project/src/utils.R"));
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
