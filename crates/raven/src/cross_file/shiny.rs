//! Filesystem enrichment for Shiny's implicit application loading conventions.
//!
//! Discovery is deliberately bounded to conventional candidate files and the
//! candidate application's direct children. Scope queries consume the finalized
//! topology from metadata and never inspect the filesystem.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::Url;

use crate::config_file::exclusions::CompiledWorkspaceExclusions;

use super::types::{ShinyApplicationMetadata, ShinyApplicationMode, ShinyFileRole};

/// Bound implicit helper topology to the same limit as explicit list-files
/// source batches. An oversized directory fails closed as an empty helper batch.
const MAX_SHINY_HELPERS: usize = 256;

#[derive(Debug, Clone, Default)]
pub(crate) struct ShinyExpansion {
    pub metadata: Option<ShinyApplicationMetadata>,
    /// Lexical application directory used for runtime relative-path semantics.
    pub application_working_directory: Option<PathBuf>,
    /// Exact selected `server.R` or `app.R` path used to bootstrap direct member opens.
    pub selected_entry: Option<PathBuf>,
    pub global: Option<PathBuf>,
    pub helpers: Vec<PathBuf>,
    pub watch_paths: Vec<PathBuf>,
}

/// Discover the implicit Shiny topology relevant to `uri`.
///
/// Only conventional entry/global names and direct `R/*.[Rr]` candidates enter
/// this path. This keeps ordinary files filesystem-free during enrichment.
pub(crate) fn discover_shiny_application(
    uri: &Url,
    exclusions: &CompiledWorkspaceExclusions,
) -> ShinyExpansion {
    if exclusions.is_excluded_uri(uri) {
        return ShinyExpansion::default();
    }
    let Some(path) = uri.to_file_path().ok() else {
        return ShinyExpansion::default();
    };
    let Some(application_root) = candidate_application_root(&path) else {
        return ShinyExpansion::default();
    };

    let application_identity = canonical_identity_path(&application_root);
    let mut result = ShinyExpansion {
        watch_paths: vec![
            application_root.clone(),
            application_root.join("R"),
            application_identity.clone(),
            application_identity.join("R"),
        ],
        ..ShinyExpansion::default()
    };

    let root_entries = read_direct_entries(&application_root)
        .into_iter()
        .filter(|entry| !exclusions.is_excluded_path(entry))
        .collect::<Vec<_>>();
    let server = select_case_insensitive_file(&root_entries, "server.R");
    let app = select_case_insensitive_file(&root_entries, "app.R");
    let mode = if server.is_some() {
        Some(ShinyApplicationMode::Legacy)
    } else if app.is_some() {
        Some(ShinyApplicationMode::SingleFile)
    } else {
        None
    };

    let ui = select_case_insensitive_file(&root_entries, "ui.R");
    let global = select_case_insensitive_file(&root_entries, "global.R");
    let helpers_dir = application_root.join("R");
    let helper_entries = if exclusions.can_prune_directory(&helpers_dir) {
        Vec::new()
    } else {
        read_direct_entries(&helpers_dir)
            .into_iter()
            .filter(|entry| !exclusions.is_excluded_path(entry))
            .collect()
    };
    result.watch_paths.extend(root_entries.iter().cloned());
    result.watch_paths.extend(helper_entries.iter().cloned());

    let helpers_disabled = helper_entries.iter().any(|entry| {
        entry.file_name().is_some_and(|name| {
            os_str_eq_ignore_ascii_case(name, OsStr::new("_disable_autoload.R"))
        })
    });
    let mut helpers = if helpers_disabled {
        Vec::new()
    } else {
        helper_entries
            .iter()
            .filter(|entry| {
                entry.is_file() && !has_hidden_file_name(entry) && has_r_extension(entry)
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    helpers.sort_by_cached_key(|path| c_locale_file_name_key(path));
    if helpers.len() > MAX_SHINY_HELPERS {
        log::warn!(
            "Shiny helper batch at {} has {} members (limit {}); ignoring the batch",
            helpers_dir.display(),
            helpers.len(),
            MAX_SHINY_HELPERS
        );
        helpers.clear();
    }

    let role = match mode {
        Some(ShinyApplicationMode::Legacy) => {
            if server
                .as_ref()
                .is_some_and(|selected| paths_refer_to_same_file(selected, &path))
            {
                ShinyFileRole::ServerEntry
            } else if ui
                .as_ref()
                .is_some_and(|selected| paths_refer_to_same_file(selected, &path))
            {
                ShinyFileRole::UiEntry
            } else if global
                .as_ref()
                .is_some_and(|selected| paths_refer_to_same_file(selected, &path))
            {
                ShinyFileRole::LegacyGlobal
            } else if let Some(ordinal) = helpers
                .iter()
                .position(|helper| paths_refer_to_same_file(helper, &path))
            {
                ShinyFileRole::Helper {
                    ordinal: ordinal as u32,
                }
            } else {
                ShinyFileRole::Candidate
            }
        }
        Some(ShinyApplicationMode::SingleFile) => {
            if app
                .as_ref()
                .is_some_and(|selected| paths_refer_to_same_file(selected, &path))
            {
                ShinyFileRole::AppEntry
            } else if let Some(ordinal) = helpers
                .iter()
                .position(|helper| paths_refer_to_same_file(helper, &path))
            {
                ShinyFileRole::Helper {
                    ordinal: ordinal as u32,
                }
            } else {
                ShinyFileRole::Candidate
            }
        }
        None => ShinyFileRole::Candidate,
    };

    let active_entry = role.is_entry();
    result.metadata = Some(ShinyApplicationMetadata {
        application_root: application_root.to_string_lossy().into_owned(),
        application_identity: Some(application_identity.to_string_lossy().into_owned()),
        mode,
        role,
    });
    if mode.is_some() {
        result.application_working_directory = Some(application_root);
        result.selected_entry = match mode {
            Some(ShinyApplicationMode::Legacy) => server.map(|path| canonical_identity_path(&path)),
            Some(ShinyApplicationMode::SingleFile) => {
                app.map(|path| canonical_identity_path(&path))
            }
            None => None,
        };
    }
    if active_entry {
        if mode == Some(ShinyApplicationMode::Legacy) {
            result.global = global.map(|path| canonical_identity_path(&path));
        }
        result.helpers = helpers
            .into_iter()
            .map(|path| canonical_identity_path(&path))
            .collect();
    }
    result.watch_paths.sort();
    result.watch_paths.dedup();
    result
}

fn canonical_identity_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn candidate_application_root(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    if ["app.R", "server.R", "ui.R", "global.R"]
        .iter()
        .any(|candidate| os_str_eq_ignore_ascii_case(name, OsStr::new(candidate)))
    {
        return path.parent().map(Path::to_path_buf);
    }

    let parent = path.parent()?;
    if parent.file_name() == Some(OsStr::new("R")) && has_r_extension(path) {
        return parent.parent().map(Path::to_path_buf);
    }
    None
}

fn read_direct_entries(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return Vec::new();
        };
        paths.push(entry.path());
    }
    paths.sort_by_cached_key(|path| c_locale_file_name_key(path));
    paths
}

/// Exact conventional spelling wins; otherwise one unique ASCII-case-insensitive
/// match is accepted. Ambiguous inexact matches fail closed.
fn select_case_insensitive_file(entries: &[PathBuf], expected: &str) -> Option<PathBuf> {
    if let Some(exact) = entries
        .iter()
        .find(|entry| entry.is_file() && entry.file_name() == Some(OsStr::new(expected)))
    {
        return Some(exact.clone());
    }
    let mut matches = entries.iter().filter(|entry| {
        entry.is_file()
            && entry
                .file_name()
                .is_some_and(|name| os_str_eq_ignore_ascii_case(name, OsStr::new(expected)))
    });
    let first = matches.next()?.clone();
    matches.next().is_none().then_some(first)
}

fn has_hidden_file_name(path: &Path) -> bool {
    path.file_name().is_some_and(os_str_starts_with_ascii_dot)
}

fn has_r_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| os_str_eq_ignore_ascii_case(extension, OsStr::new("R")))
}

fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

#[cfg(unix)]
fn os_str_starts_with_ascii_dot(value: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().first() == Some(&b'.')
}

#[cfg(windows)]
fn os_str_starts_with_ascii_dot(value: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().next() == Some(u16::from(b'.'))
}

#[cfg(unix)]
fn os_str_eq_ignore_ascii_case(left: &OsStr, right: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    left.as_bytes().eq_ignore_ascii_case(right.as_bytes())
}

#[cfg(windows)]
fn os_str_eq_ignore_ascii_case(left: &OsStr, right: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let left: Vec<_> = left.encode_wide().collect();
    let right: Vec<_> = right.encode_wide().collect();
    left.len() == right.len()
        && left.iter().zip(right.iter()).all(|(left, right)| {
            left == right
                || u8::try_from(*left)
                    .ok()
                    .zip(u8::try_from(*right).ok())
                    .is_some_and(|(left, right)| left.eq_ignore_ascii_case(&right))
        })
}

#[cfg(unix)]
fn c_locale_file_name_key(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.file_name()
        .map(OsStr::as_bytes)
        .unwrap_or_default()
        .to_vec()
}

#[cfg(windows)]
fn c_locale_file_name_key(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.file_name()
        .map(|name| name.encode_wide().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    fn uri(path: &Path) -> Url {
        Url::from_file_path(path).unwrap()
    }

    fn discover(uri: &Url) -> ShinyExpansion {
        discover_shiny_application(uri, &CompiledWorkspaceExclusions::default())
    }

    fn exclusions(root: &Path, patterns: &[&str]) -> CompiledWorkspaceExclusions {
        crate::config_file::compile_workspace_exclusions(
            &serde_json::json!({ "workspace": { "exclude": patterns } }),
            vec![root.to_path_buf()],
        )
    }

    #[test]
    fn legacy_mode_wins_and_orders_helpers() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("app.R"));
        write(&root.path().join("server.r"));
        write(&root.path().join("global.R"));
        write(&root.path().join("R/b.r"));
        write(&root.path().join("R/A.R"));

        let expansion = discover(&uri(&root.path().join("server.r")));
        assert_eq!(
            expansion.metadata.as_ref().unwrap().mode,
            Some(ShinyApplicationMode::Legacy)
        );
        assert!(expansion.global.is_some());
        let names: Vec<_> = expansion
            .helpers
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["A.R", "b.r"]);
    }

    #[test]
    fn disable_marker_keeps_legacy_global_only() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("server.R"));
        write(&root.path().join("global.R"));
        write(&root.path().join("R/helper.R"));
        write(&root.path().join("R/_DISABLE_AUTOLOAD.r"));

        let expansion = discover(&uri(&root.path().join("server.R")));
        assert!(expansion.global.is_some());
        assert!(expansion.helpers.is_empty());
    }

    #[test]
    fn single_file_ignores_adjacent_global() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("APP.r"));
        write(&root.path().join("global.R"));

        let expansion = discover(&uri(&root.path().join("APP.r")));
        assert_eq!(
            expansion.metadata.as_ref().unwrap().mode,
            Some(ShinyApplicationMode::SingleFile)
        );
        assert!(expansion.global.is_none());
    }

    #[test]
    fn hidden_r_files_are_not_helpers() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("app.R"));
        write(&root.path().join("R/.hidden.R"));
        write(&root.path().join("R/visible.R"));

        let expansion = discover(&uri(&root.path().join("app.R")));
        assert_eq!(
            expansion.helpers,
            [canonical_identity_path(&root.path().join("R/visible.R"))]
        );
    }

    #[test]
    fn oversized_helper_batch_fails_closed() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("app.R"));
        for index in 0..=MAX_SHINY_HELPERS {
            write(&root.path().join("R").join(format!("helper-{index:03}.R")));
        }

        let expansion = discover(&uri(&root.path().join("app.R")));
        assert!(expansion.helpers.is_empty());
    }

    #[test]
    fn only_direct_r_children_are_helpers() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("app.R"));
        write(&root.path().join("R/top.R"));
        write(&root.path().join("R/nested/hidden.R"));

        let expansion = discover(&uri(&root.path().join("app.R")));
        assert_eq!(
            expansion.helpers,
            [canonical_identity_path(&root.path().join("R/top.R"))]
        );
    }

    #[test]
    fn exact_entry_spelling_wins_over_other_case_insensitive_matches() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("server.R"));
        write(&root.path().join("SERVER.r"));
        write(&root.path().join("app.R"));

        let expansion = discover(&uri(&root.path().join("server.R")));
        let metadata = expansion.metadata.unwrap();
        assert_eq!(metadata.mode, Some(ShinyApplicationMode::Legacy));
        assert_eq!(metadata.role, ShinyFileRole::ServerEntry);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ambiguous_inexact_server_candidates_fail_closed() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("SERVER.r"));
        write(&root.path().join("SeRvEr.R"));
        write(&root.path().join("app.R"));

        let expansion = discover(&uri(&root.path().join("app.R")));
        assert_eq!(
            expansion.metadata.unwrap().mode,
            Some(ShinyApplicationMode::SingleFile)
        );
    }

    #[test]
    fn nested_application_selects_its_own_root() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("app.R"));
        write(&root.path().join("R/outer.R"));
        write(&root.path().join("nested/server.R"));
        write(&root.path().join("nested/R/inner.R"));

        let expansion = discover(&uri(&root.path().join("nested/server.R")));
        let metadata = expansion.metadata.unwrap();
        assert_eq!(
            PathBuf::from(metadata.application_root),
            root.path().join("nested")
        );
        assert_eq!(metadata.mode, Some(ShinyApplicationMode::Legacy));
        assert_eq!(
            expansion.helpers,
            [canonical_identity_path(
                &root.path().join("nested/R/inner.R")
            )]
        );
    }

    #[test]
    fn incomplete_candidate_retains_watch_roots() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("ui.R"));

        let expansion = discover(&uri(&root.path().join("ui.R")));
        let metadata = expansion.metadata.unwrap();
        assert_eq!(metadata.mode, None);
        assert_eq!(metadata.role, ShinyFileRole::Candidate);
        assert!(expansion.watch_paths.contains(&root.path().to_path_buf()));
        assert!(expansion.watch_paths.contains(&root.path().join("R")));
    }

    #[test]
    fn exclusions_apply_before_mode_marker_and_helper_selection() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("server.R"));
        write(&root.path().join("app.R"));
        write(&root.path().join("R/_disable_autoload.R"));
        write(&root.path().join("R/a.R"));
        write(&root.path().join("R/b.R"));
        let exclusions = exclusions(root.path(), &["server.R", "R/_disable_autoload.R", "R/a.R"]);

        let expansion = discover_shiny_application(&uri(&root.path().join("app.R")), &exclusions);
        let metadata = expansion.metadata.unwrap();
        assert_eq!(metadata.mode, Some(ShinyApplicationMode::SingleFile));
        assert_eq!(metadata.role, ShinyFileRole::AppEntry);
        assert_eq!(
            expansion.helpers,
            [canonical_identity_path(&root.path().join("R/b.R"))]
        );
        assert_eq!(
            expansion.selected_entry,
            Some(canonical_identity_path(&root.path().join("app.R")))
        );
    }

    #[test]
    fn excluded_open_candidate_has_no_shiny_topology() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("app.R"));
        let exclusions = exclusions(root.path(), &["app.R"]);

        let expansion = discover_shiny_application(&uri(&root.path().join("app.R")), &exclusions);
        assert!(expansion.metadata.is_none());
        assert!(expansion.watch_paths.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_shares_identity_but_preserves_lexical_working_directory() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let real = root.path().join("real");
        let alias = root.path().join("alias");
        write(&real.join("app.R"));
        write(&real.join("R/helper.R"));
        symlink(&real, &alias).unwrap();

        let expansion = discover(&uri(&alias.join("app.R")));
        let metadata = expansion.metadata.unwrap();
        let canonical_real = canonical_identity_path(&real);
        assert_eq!(
            metadata.application_identity.as_deref(),
            Some(canonical_real.to_string_lossy().as_ref())
        );
        assert_eq!(expansion.application_working_directory, Some(alias.clone()));
        assert_eq!(expansion.selected_entry, Some(canonical_real.join("app.R")));
        assert_eq!(expansion.helpers, [canonical_real.join("R/helper.R")]);
        assert!(expansion.watch_paths.contains(&alias));
        assert!(expansion.watch_paths.contains(&canonical_real));
        assert!(expansion.watch_paths.contains(&root.path().join("alias/R")));
        assert!(expansion.watch_paths.contains(&canonical_real.join("R")));
    }
}
