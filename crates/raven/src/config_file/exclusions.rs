//! Compiled project-level workspace exclusions.
//!
//! These exclusions are intentionally separate from lint overrides: they remove
//! files from workspace discovery/indexing and default CLI discovery, not just
//! lint diagnostics.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobBuilder, GlobMatcher};
use serde_json::Value;
use tower_lsp::lsp_types::Url;

#[derive(Debug, Clone)]
struct ExclusionRule {
    negated: bool,
    matcher: GlobMatcher,
    prune_matchers: Vec<GlobMatcher>,
}

/// Compiled `[workspace].exclude` matcher.
///
/// Patterns are evaluated in order against paths relative to the containing
/// workspace root; the last matching pattern wins. A leading `!` negates a
/// pattern and re-includes matching paths. Directory pruning is disabled
/// whenever any negated pattern is present, so a re-included descendant is never
/// skipped by an ancestor-directory prune.
#[derive(Debug, Clone, Default)]
pub struct CompiledWorkspaceExclusions {
    roots: Vec<PathBuf>,
    patterns: Vec<String>,
    rules: Vec<ExclusionRule>,
    has_negation: bool,
}

impl CompiledWorkspaceExclusions {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    pub fn has_negation(&self) -> bool {
        self.has_negation
    }

    /// Returns true when `path` is excluded by the last matching rule.
    pub fn is_excluded_path(&self, path: &Path) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let Some(rel) = self.relative_path(path) else {
            return false;
        };
        if rel.as_os_str().is_empty() {
            return false;
        }

        let mut excluded = false;
        for rule in &self.rules {
            if rule.matcher.is_match(rel) {
                excluded = !rule.negated;
            }
        }
        excluded
    }

    pub fn is_excluded_uri(&self, uri: &Url) -> bool {
        uri.to_file_path()
            .ok()
            .is_some_and(|path| self.is_excluded_path(&path))
    }

    /// Returns true when a directory can be pruned before walking descendants.
    ///
    /// This is deliberately stricter than [`Self::is_excluded_path`]: a file
    /// pattern that matches the directory path itself does not prove every child
    /// is excluded. Only directory-glob patterns normalized to `dir/**` (or a
    /// wildcard equivalent such as `**/generated/**`) produce prune matchers.
    /// Any negated rule disables pruning globally.
    pub fn can_prune_directory(&self, dir: &Path) -> bool {
        if self.rules.is_empty() || self.has_negation {
            return false;
        }
        let Some(rel) = self.relative_path(dir) else {
            return false;
        };
        if rel.as_os_str().is_empty() {
            return false;
        }
        self.rules.iter().any(|rule| {
            !rule.negated
                && rule
                    .prune_matchers
                    .iter()
                    .any(|matcher| matcher.is_match(rel))
        })
    }

    fn relative_path<'a>(&'a self, path: &'a Path) -> Option<&'a Path> {
        self.roots
            .iter()
            .filter_map(|root| path.strip_prefix(root).ok())
            .min_by_key(|rel| rel.components().count())
    }
}

/// Build compiled workspace exclusions from `[workspace].exclude`.
///
/// `roots` are the workspace roots against which project-relative patterns are
/// matched. Invalid globs are skipped with a warning.
pub fn compile_workspace_exclusions(
    merged: &Value,
    roots: impl IntoIterator<Item = PathBuf>,
) -> CompiledWorkspaceExclusions {
    let roots: Vec<PathBuf> = roots.into_iter().collect();
    if roots.is_empty() {
        return CompiledWorkspaceExclusions::default();
    }

    let Some(arr) = merged
        .get("workspace")
        .and_then(|v| v.get("exclude"))
        .and_then(|v| v.as_array())
    else {
        return CompiledWorkspaceExclusions::default();
    };

    let mut patterns = Vec::new();
    let mut rules = Vec::new();
    let mut has_negation = false;

    for raw in arr {
        let Some(raw) = raw.as_str() else {
            log::warn!("raven.toml: workspace.exclude entries must be strings; skipping {raw:?}");
            continue;
        };
        let Some((negated, pattern)) = normalize_pattern(raw) else {
            continue;
        };
        let glob = match workspace_glob(&pattern) {
            Ok(glob) => glob,
            Err(err) => {
                log::warn!("raven.toml: invalid workspace.exclude glob {raw:?}: {err}");
                continue;
            }
        };
        let prune_matchers = if negated {
            Vec::new()
        } else {
            compile_prune_matchers(&pattern)
        };
        has_negation |= negated;
        patterns.push(if negated {
            format!("!{pattern}")
        } else {
            pattern.clone()
        });
        rules.push(ExclusionRule {
            negated,
            matcher: glob.compile_matcher(),
            prune_matchers,
        });
    }

    CompiledWorkspaceExclusions {
        roots,
        patterns,
        rules,
        has_negation,
    }
}

fn normalize_pattern(raw: &str) -> Option<(bool, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (negated, body) = match trimmed.strip_prefix('!') {
        Some(rest) => (true, rest.trim()),
        None => (false, trimmed),
    };
    if body.is_empty() {
        return None;
    }
    let body = body.strip_prefix("./").unwrap_or(body);
    let pattern = if body.ends_with('/') {
        format!("{body}**")
    } else {
        body.to_string()
    };
    Some((negated, pattern))
}

fn workspace_glob(pattern: &str) -> Result<Glob, globset::Error> {
    GlobBuilder::new(pattern).literal_separator(true).build()
}

fn compile_prune_matchers(pattern: &str) -> Vec<GlobMatcher> {
    let Some(prefix) = pattern.strip_suffix("/**") else {
        return Vec::new();
    };
    if prefix.is_empty() {
        return Vec::new();
    }

    let mut matchers = Vec::new();
    if let Ok(glob) = workspace_glob(prefix) {
        matchers.push(glob.compile_matcher());
    }
    if let Some(stripped) = prefix.strip_prefix("**/")
        && !stripped.is_empty()
        && let Ok(glob) = workspace_glob(stripped)
    {
        matchers.push(glob.compile_matcher());
    }
    matchers
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn last_match_wins_with_negation() {
        let root = PathBuf::from("/workspace");
        let cfg = compile_workspace_exclusions(
            &json!({ "workspace": { "exclude": ["generated/**", "!generated/keep.R"] } }),
            vec![root.clone()],
        );

        assert!(cfg.is_excluded_path(&root.join("generated/drop.R")));
        assert!(!cfg.is_excluded_path(&root.join("generated/keep.R")));
        assert!(cfg.has_negation());
        assert!(
            !cfg.can_prune_directory(&root.join("generated")),
            "negated re-includes disable directory pruning"
        );
    }

    #[test]
    fn directory_glob_prunes_without_negation() {
        let root = PathBuf::from("/workspace");
        let cfg = compile_workspace_exclusions(
            &json!({ "workspace": { "exclude": ["generated/**"] } }),
            vec![root.clone()],
        );

        assert!(cfg.can_prune_directory(&root.join("generated")));
        assert!(cfg.is_excluded_path(&root.join("generated/drop.R")));
        assert!(!cfg.is_excluded_path(&root.join("other/generated/drop.R")));
    }

    #[test]
    fn single_star_does_not_cross_directory_separator() {
        let root = PathBuf::from("/workspace");
        let cfg = compile_workspace_exclusions(
            &json!({ "workspace": { "exclude": ["generated/*"] } }),
            vec![root.clone()],
        );

        assert!(cfg.is_excluded_path(&root.join("generated/file.R")));
        assert!(
            !cfg.is_excluded_path(&root.join("generated/nested/file.R")),
            "single-star globs must not match through '/'"
        );
    }

    #[test]
    fn double_star_crosses_directory_separator() {
        let root = PathBuf::from("/workspace");
        let cfg = compile_workspace_exclusions(
            &json!({ "workspace": { "exclude": ["generated/**"] } }),
            vec![root.clone()],
        );

        assert!(cfg.is_excluded_path(&root.join("generated/file.R")));
        assert!(cfg.is_excluded_path(&root.join("generated/nested/file.R")));
    }

    #[test]
    fn recursive_directory_glob_prunes_any_matching_directory() {
        let root = PathBuf::from("/workspace");
        let cfg = compile_workspace_exclusions(
            &json!({ "workspace": { "exclude": ["**/generated/**"] } }),
            vec![root.clone()],
        );

        assert!(cfg.can_prune_directory(&root.join("generated")));
        assert!(cfg.can_prune_directory(&root.join("pkg/generated")));
        assert!(cfg.is_excluded_path(&root.join("pkg/generated/file.R")));
    }
}
