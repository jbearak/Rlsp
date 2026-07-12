//! Compiled per-glob lint overrides and per-document `LintConfig` resolution.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};
use serde_json::Value;
use tower_lsp::lsp_types::Url;

use crate::backend::parse_lint_config_from_section;
use crate::linting::LintConfig;

/// A single `[[linting.overrides]]` entry, compiled.
#[derive(Debug, Clone)]
pub struct CompiledLintOverride {
    /// Project root the globs are anchored at.
    pub root: PathBuf,
    /// Compiled glob matchers for `files = [...]`. An override matches when
    /// any of its globs match a document's project-relative path.
    pub matchers: Vec<GlobMatcher>,
    /// The override's body, stored as a partial JSON object that can be
    /// applied as a patch on top of the base `[linting]` JSON.
    pub patch: Value,
}

/// Build compiled overrides from the merged `[linting].overrides` array.
/// `root` is the directory containing `raven.toml`. Returns an empty vec if
/// no overrides are configured.
pub fn compile_lint_overrides(merged: &Value, root: &Path) -> Vec<CompiledLintOverride> {
    let Some(arr) = merged
        .get("linting")
        .and_then(|v| v.get("overrides"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for (idx, entry) in arr.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            log::warn!(
                "raven.toml: [[linting.overrides]] entry #{} is not a table; skipping",
                idx
            );
            continue;
        };
        let Some(files) = obj.get("files").and_then(|v| v.as_array()) else {
            log::warn!(
                "raven.toml: [[linting.overrides]] entry #{} missing `files`; skipping",
                idx
            );
            continue;
        };
        let mut matchers = Vec::new();
        for f in files {
            let Some(s) = f.as_str() else { continue };
            match Glob::new(s) {
                Ok(g) => matchers.push(g.compile_matcher()),
                Err(e) => log::warn!(
                    "raven.toml: [[linting.overrides]] entry #{} has invalid glob {:?}: {}",
                    idx,
                    s,
                    e
                ),
            }
        }
        if matchers.is_empty() {
            continue;
        }
        // Drop `files`; everything else is the patch.
        let mut patch = entry.clone();
        if let Value::Object(map) = &mut patch {
            map.remove("files");
        }
        out.push(CompiledLintOverride {
            root: root.to_path_buf(),
            matchers,
            patch,
        });
    }
    out
}

/// Resolve the effective `LintConfig` for a document. Walks `overrides` in
/// order, applying any whose glob matches `document_uri`'s project-relative
/// path. Returns the base `LintConfig` if no overrides match (or if the URI
/// can't be resolved to a project-relative path).
pub fn resolve_lint_for_document(
    base: &LintConfig,
    base_section: &Value,
    overrides: &[CompiledLintOverride],
    document_uri: &Url,
) -> LintConfig {
    if overrides.is_empty() {
        return base.clone();
    }
    let Some(file_path) = document_uri.to_file_path().ok() else {
        return base.clone();
    };
    let Some(root) = overrides.first().map(|o| o.root.as_path()) else {
        return base.clone();
    };
    let Ok(rel) = file_path.strip_prefix(root) else {
        return base.clone();
    };

    // Start with the base [linting] section JSON and layer matching overrides
    // on top, then re-parse. This keeps semantics identical to what the LSP
    // does at startup.
    let mut effective = base_section.clone();
    // Stamp the base's indentation_unit into the JSON so overrides that don't
    // explicitly set indentationUnit inherit the per-document value, not the
    // stale client placeholder that may be in the raw section.
    if let Some(map) = effective.as_object_mut() {
        map.insert(
            "indentationUnit".to_string(),
            serde_json::json!(base.indentation_unit),
        );
    }
    let mut matched_any = false;
    for ov in overrides {
        if ov.matchers.iter().any(|m| m.is_match(rel)) {
            matched_any = true;
            crate::config_file::merge::merge_into(&mut effective, &ov.patch);
        }
    }
    if !matched_any {
        return base.clone();
    }
    parse_lint_config_from_section(&effective, base.enabled).unwrap_or_else(|| base.clone())
}

/// Returns true if the override has `enabled = false` after applying patches;
/// callers (CLI) use this to short-circuit before parsing the file.
pub fn is_skipped_by_overrides(
    base_section: &Value,
    overrides: &[CompiledLintOverride],
    relative_path: &Path,
) -> bool {
    let mut effective = base_section.clone();
    let mut matched = false;
    for ov in overrides {
        if ov.matchers.iter().any(|m| m.is_match(relative_path)) {
            matched = true;
            crate::config_file::merge::merge_into(&mut effective, &ov.patch);
        }
    }
    if !matched {
        return false;
    }
    match effective.get("enabled") {
        Some(serde_json::Value::Bool(false)) => true,
        Some(serde_json::Value::String(s)) if s == "off" || s == "false" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tower_lsp::lsp_types::Url;

    fn make_overrides(root: &Path, patches: Vec<(&str, Value)>) -> Vec<CompiledLintOverride> {
        patches
            .into_iter()
            .map(|(glob, patch)| CompiledLintOverride {
                root: root.to_path_buf(),
                matchers: vec![Glob::new(glob).unwrap().compile_matcher()],
                patch,
            })
            .collect()
    }

    #[test]
    fn no_overrides_returns_base() {
        let base = LintConfig::default();
        let section = json!({});
        let uri = Url::parse("file:///proj/R/foo.R").unwrap();
        let out = resolve_lint_for_document(&base, &section, &[], &uri);
        assert_eq!(out.line_length, base.line_length);
    }

    #[test]
    fn matching_glob_applies_patch() {
        let mut base = LintConfig::default();
        base.line_length = 80;
        let section = json!({ "lineLength": 80, "enabled": true });
        let root = PathBuf::from("/proj");
        let overrides = make_overrides(&root, vec![("tests/**/*.R", json!({ "lineLength": 120 }))]);
        let uri = Url::parse("file:///proj/tests/test-foo.R").unwrap();
        let out = resolve_lint_for_document(&base, &section, &overrides, &uri);
        assert_eq!(out.line_length, 120);
    }

    #[test]
    fn matching_glob_switches_infix_continuation_style() {
        let base = LintConfig::default();
        assert_eq!(
            base.infix_continuation_style,
            crate::linting::InfixContinuationStyle::Either
        );
        let section = json!({ "enabled": true });
        let root = PathBuf::from("/proj");
        let overrides = make_overrides(
            &root,
            vec![("R/**/*.R", json!({ "infixContinuationStyle": "indented" }))],
        );
        let uri = Url::parse("file:///proj/R/foo.R").unwrap();
        let out = resolve_lint_for_document(&base, &section, &overrides, &uri);
        assert_eq!(
            out.infix_continuation_style,
            crate::linting::InfixContinuationStyle::Indented,
            "the override must switch the style away from the Either base"
        );
    }

    #[test]
    fn non_matching_glob_returns_base() {
        let mut base = LintConfig::default();
        base.line_length = 80;
        let section = json!({ "lineLength": 80 });
        let root = PathBuf::from("/proj");
        let overrides = make_overrides(&root, vec![("tests/**/*.R", json!({ "lineLength": 120 }))]);
        let uri = Url::parse("file:///proj/R/foo.R").unwrap();
        let out = resolve_lint_for_document(&base, &section, &overrides, &uri);
        assert_eq!(out.line_length, 80);
    }

    #[test]
    fn later_override_wins_on_same_key() {
        let mut base = LintConfig::default();
        base.line_length = 80;
        let section = json!({ "lineLength": 80, "enabled": true });
        let root = PathBuf::from("/proj");
        let overrides = vec![
            CompiledLintOverride {
                root: root.clone(),
                matchers: vec![Glob::new("R/**/*.R").unwrap().compile_matcher()],
                patch: json!({ "lineLength": 100 }),
            },
            CompiledLintOverride {
                root: root.clone(),
                matchers: vec![Glob::new("R/legacy/**/*.R").unwrap().compile_matcher()],
                patch: json!({ "lineLength": 200 }),
            },
        ];
        let uri = Url::parse("file:///proj/R/legacy/old.R").unwrap();
        let out = resolve_lint_for_document(&base, &section, &overrides, &uri);
        assert_eq!(out.line_length, 200);
    }

    #[test]
    fn untitled_uri_falls_through_to_base() {
        let mut base = LintConfig::default();
        base.line_length = 80;
        let section = json!({ "lineLength": 80 });
        let root = PathBuf::from("/proj");
        let overrides = make_overrides(&root, vec![("**/*.R", json!({ "lineLength": 200 }))]);
        let uri = Url::parse("untitled:Untitled-1").unwrap();
        let out = resolve_lint_for_document(&base, &section, &overrides, &uri);
        assert_eq!(out.line_length, 80);
    }

    #[test]
    fn enabled_false_in_override_is_detected() {
        let section = json!({ "enabled": true });
        let root = PathBuf::from("/proj");
        let overrides = make_overrides(&root, vec![("R/legacy_*.R", json!({ "enabled": false }))]);
        assert!(is_skipped_by_overrides(
            &section,
            &overrides,
            Path::new("R/legacy_old.R")
        ));
        assert!(!is_skipped_by_overrides(
            &section,
            &overrides,
            Path::new("R/main.R")
        ));
    }

    #[test]
    fn enabled_string_off_in_override_is_detected() {
        // String forms ("off", "false") must skip files the same way the
        // boolean form does — tri-state vocabulary is uniform across the
        // master switch and per-glob overrides (#281).
        let section = json!({ "enabled": true });
        let root = PathBuf::from("/proj");
        let overrides = make_overrides(&root, vec![("vendor/**/*.R", json!({ "enabled": "off" }))]);
        assert!(is_skipped_by_overrides(
            &section,
            &overrides,
            Path::new("vendor/foo.R")
        ));

        let overrides = make_overrides(
            &root,
            vec![("vendor/**/*.R", json!({ "enabled": "false" }))],
        );
        assert!(is_skipped_by_overrides(
            &section,
            &overrides,
            Path::new("vendor/foo.R")
        ));
    }

    #[test]
    fn override_no_enabled_key_inherits_base_enabled() {
        // Override that only tweaks lineLength must not flip enabled —
        // the master switch is inherited from the resolved base (#281).
        let mut base = LintConfig::default();
        base.enabled = true;
        let root = PathBuf::from("/proj");
        let overrides = make_overrides(&root, vec![("**/*.R", json!({ "lineLength": 120 }))]);
        let uri = Url::parse("file:///proj/R/foo.R").unwrap();
        let effective = resolve_lint_for_document(&base, &json!({}), &overrides, &uri);
        assert!(
            effective.enabled,
            "override without enabled should inherit base"
        );
        assert_eq!(effective.line_length, 120);
    }

    #[test]
    fn override_auto_inherits_base_enabled() {
        // Per-glob `enabled = "auto"` means "don't override the master
        // switch"; it inherits whatever the base resolved to (#281).
        let mut base = LintConfig::default();
        base.enabled = true;
        let root = PathBuf::from("/proj");
        let overrides = make_overrides(&root, vec![("**/*.R", json!({ "enabled": "auto" }))]);
        let uri = Url::parse("file:///proj/x.R").unwrap();
        let effective = resolve_lint_for_document(&base, &json!({}), &overrides, &uri);
        assert!(effective.enabled, "override 'auto' should inherit base on");

        base.enabled = false;
        let effective = resolve_lint_for_document(&base, &json!({}), &overrides, &uri);
        assert!(
            !effective.enabled,
            "override 'auto' should inherit base off"
        );
    }

    #[test]
    fn override_preserves_per_document_indentation_unit() {
        // When the raw section has indentationUnit: 2 (the client placeholder)
        // but base.indentation_unit = 4 (per-document tab size from the editor),
        // an override that matches but doesn't set indentationUnit must not reset
        // it back to the stale section value.
        let mut base = LintConfig::default();
        base.indentation_unit = 4;
        let section = json!({ "indentationUnit": 2, "lineLength": 80, "enabled": true });
        let root = PathBuf::from("/proj");
        let overrides = make_overrides(&root, vec![("R/**/*.R", json!({ "lineLength": 120 }))]);
        let uri = Url::parse("file:///proj/R/foo.R").unwrap();
        let out = resolve_lint_for_document(&base, &section, &overrides, &uri);
        assert_eq!(
            out.indentation_unit, 4,
            "per-document indent unit must survive override re-parse"
        );
        assert_eq!(
            out.line_length, 120,
            "override line length must still apply"
        );
    }

    #[test]
    fn override_null_inherits_base_enabled() {
        // `enabled = null` is semantically equivalent to absent — it must
        // not flip the master switch (#281).
        let mut base = LintConfig::default();
        base.enabled = true;
        let root = PathBuf::from("/proj");
        let overrides = make_overrides(
            &root,
            vec![("**/*.R", json!({ "enabled": serde_json::Value::Null }))],
        );
        let uri = Url::parse("file:///proj/x.R").unwrap();
        let effective = resolve_lint_for_document(&base, &json!({}), &overrides, &uri);
        assert!(effective.enabled, "override null should inherit base");
    }
}
