//
// box_use/path.rs
//
// Resolve a `box::use()` local-module spec to a concrete module file.
//

//! Local box-module path resolution.
//!
//! Resolving a [`BoxSpec::LocalModule`] deliberately does **not** reuse the
//! general cross-file path resolver ([`crate::cross_file::path_resolve`]).
//! box module paths obey a different, stricter contract, and folding them into
//! the general resolver would risk regressing its `# raven: cd` /
//! testthat-working-directory / workspace-root-fallback behaviour. Specifically,
//! a local box module:
//!
//! * resolves **relative to the importing file's own directory** (`./` = that
//!   directory, each leading `../` ascends one level);
//! * **ignores** `# raven: cd`, the implicit testthat/testit working directory,
//!   and the forward workspace-root fallback — none of them apply to box;
//! * omits the file extension in the spec; the resolver appends it;
//! * is **case-sensitive** (box module names are case-sensitive). A path that
//!   exists only under a different case is *not* silently corrected: it is
//!   reported as [`BoxResolveError::CaseMismatch`] so tooling can diagnose it;
//! * preserves the raw, non-canonicalised path/URI (Raven's symlink/case
//!   identity convention — see [`ImportSource`](crate::selective_import::ImportSource)).
//!
//! # Candidate order
//!
//! For a spec resolving to `<dir>/<name>`, candidates are tried in this order,
//! and the **first that exists (case-exactly)** wins:
//!
//! 1. `<dir>/<name>.r`   — a *file module*
//! 2. `<dir>/<name>.R`   — a *file module*
//! 3. `<dir>/<name>/__init__.r` — a *package module*
//! 4. `<dir>/<name>/__init__.R` — a *package module*
//!
//! i.e. a `.r`/`.R` file module beats an `__init__` package module, and a
//! lowercase extension beats an uppercase one, matching box itself.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::Url;

use super::{BoxImport, BoxSpec, LocalModuleResolution};

/// The kind of module a local spec resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    /// A single-file module: `<name>.r` / `<name>.R`.
    File,
    /// A package module directory with an `__init__.r` / `__init__.R`.
    Package,
}

/// A successfully resolved local box module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModule {
    /// The resolved module *file* URI (`<name>.r` or `<name>/__init__.r`),
    /// never a directory. Raw / non-canonicalised.
    pub uri: Url,
    /// Whether the module is a file module or a package (`__init__`) module.
    pub kind: ModuleKind,
}

/// Why a local box module could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoxResolveError {
    /// The spec is not a local module (a package or unsupported spec). Callers
    /// resolve those via the package library / conservative-failure paths, not
    /// this resolver.
    NotLocalModule,
    /// The importing file has no usable parent directory, or `../` ascended past
    /// the filesystem root.
    NoParentDirectory,
    /// No candidate exists (case-exactly or otherwise). `searched` lists the
    /// candidate paths that were tried, in order.
    NotFound {
        /// Candidate paths tried, in candidate order.
        searched: Vec<PathBuf>,
    },
    /// A candidate exists on disk but only under a different case. box is
    /// case-sensitive, so this is an error to diagnose, not a match.
    CaseMismatch {
        /// The path as written (the exact-case candidate that was expected).
        expected: PathBuf,
        /// The differently-cased path that actually exists on disk.
        found: PathBuf,
    },
}

impl fmt::Display for BoxResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoxResolveError::NotLocalModule => write!(f, "not a local module spec"),
            BoxResolveError::NoParentDirectory => {
                write!(f, "importing file has no parent directory")
            }
            BoxResolveError::NotFound { searched } => {
                write!(
                    f,
                    "no module file found (tried {} candidates)",
                    searched.len()
                )
            }
            BoxResolveError::CaseMismatch { expected, found } => write!(
                f,
                "module path case mismatch: expected '{}', found '{}'",
                expected.display(),
                found.display()
            ),
        }
    }
}

impl std::error::Error for BoxResolveError {}

/// Resolve a local box-module [`BoxSpec`] against the importing file.
///
/// Returns [`BoxResolveError::NotLocalModule`] for a package or unsupported
/// spec. See the module docs for the exact contract (relative-to-importer,
/// case-sensitive, candidate order, no `cd`/testthat/workspace fallback).
pub fn resolve_local_module(
    importing_uri: &Url,
    spec: &BoxSpec,
) -> Result<ResolvedModule, BoxResolveError> {
    let (up_levels, components) = match spec {
        BoxSpec::LocalModule {
            up_levels,
            components,
        } => (*up_levels, components.as_slice()),
        _ => return Err(BoxResolveError::NotLocalModule),
    };

    let base = base_directory(importing_uri, up_levels)?;
    let Some((name, dirs)) = components.split_last() else {
        // classify_module never produces an empty component list, but fail
        // conservatively rather than panic if that invariant is ever violated.
        return Err(BoxResolveError::NotLocalModule);
    };
    let candidates = candidate_part_lists(dirs, name);

    // Pass 1: an exact (case-sensitive) match, honouring candidate order.
    for (parts, kind) in &candidates {
        if let SuffixResolution::Exact(path) = resolve_suffix(&base, parts)
            && let Ok(uri) = Url::from_file_path(&path)
        {
            return Ok(ResolvedModule { uri, kind: *kind });
        }
    }

    // Pass 2: a case-only mismatch — a real error to diagnose, not a match. The
    // first candidate that exists case-insensitively wins the report.
    for (parts, _) in &candidates {
        if let SuffixResolution::Folded(found) = resolve_suffix(&base, parts) {
            return Err(BoxResolveError::CaseMismatch {
                expected: join_parts(&base, parts),
                found,
            });
        }
    }

    Err(BoxResolveError::NotFound {
        searched: candidates
            .iter()
            .map(|(parts, _)| join_parts(&base, parts))
            .collect(),
    })
}

/// Resolve every local import in `imports` and persist the outcome for later
/// lock-held consumers. This function performs filesystem I/O and therefore must
/// run only in detached analysis/rebuild work, never while a `WorldState` guard
/// is held and never from an interactive request path.
pub(crate) fn enrich_local_imports(importing_uri: &Url, imports: &mut [BoxImport]) {
    for import in imports {
        if !matches!(import.spec, BoxSpec::LocalModule { .. }) {
            import.local_resolution = None;
            continue;
        }
        import.local_resolution = Some(match resolve_local_module(importing_uri, &import.spec) {
            Ok(resolved) => LocalModuleResolution::Resolved(resolved.uri),
            Err(BoxResolveError::CaseMismatch { expected, found }) => {
                LocalModuleResolution::CaseMismatch { expected, found }
            }
            Err(BoxResolveError::NotFound { .. }) | Err(BoxResolveError::NoParentDirectory) => {
                LocalModuleResolution::Missing
            }
            Err(BoxResolveError::NotLocalModule) => continue,
        });
    }
}

/// The absolute candidate module-file paths a local spec would search, in
/// candidate order. Empty for a non-local spec.
///
/// Exposed for diagnostics (listing what was searched) and tests. Does not touch
/// the filesystem.
pub fn candidate_paths(importing_uri: &Url, spec: &BoxSpec) -> Vec<PathBuf> {
    let (up_levels, components) = match spec {
        BoxSpec::LocalModule {
            up_levels,
            components,
        } => (*up_levels, components.as_slice()),
        _ => return Vec::new(),
    };
    let Ok(base) = base_directory(importing_uri, up_levels) else {
        return Vec::new();
    };
    let Some((name, dirs)) = components.split_last() else {
        return Vec::new();
    };
    candidate_part_lists(dirs, name)
        .iter()
        .map(|(parts, _)| join_parts(&base, parts))
        .collect()
}

/// Whether a watched filesystem path could affect this local import's ordered
/// candidate set, including a case-only spelling. The resolver treats a unique
/// case-folded match as a diagnosable mismatch, so those create/delete/rename
/// events must revalidate the importer just like exact candidate events.
pub(crate) fn candidate_set_matches_path(
    importing_uri: &Url,
    spec: &BoxSpec,
    changed_path: &Path,
) -> bool {
    let changed = changed_path.to_string_lossy();
    candidate_paths(importing_uri, spec)
        .iter()
        .any(|candidate| {
            candidate == changed_path
                || candidate
                    .to_string_lossy()
                    .eq_ignore_ascii_case(changed.as_ref())
        })
}

/// The importing file's directory, ascended `up_levels` parent hops.
fn base_directory(importing_uri: &Url, up_levels: usize) -> Result<PathBuf, BoxResolveError> {
    let file_path = importing_uri
        .to_file_path()
        .map_err(|()| BoxResolveError::NoParentDirectory)?;
    let mut base = file_path
        .parent()
        .ok_or(BoxResolveError::NoParentDirectory)?
        .to_path_buf();
    for _ in 0..up_levels {
        base = base
            .parent()
            .ok_or(BoxResolveError::NoParentDirectory)?
            .to_path_buf();
    }
    Ok(base)
}

/// Build the ordered candidate part-lists (relative to the base directory) with
/// each candidate's [`ModuleKind`]. See the module docs for the order.
fn candidate_part_lists(dirs: &[String], name: &str) -> Vec<(Vec<String>, ModuleKind)> {
    let with_leaf = |leaf: Vec<String>| {
        let mut parts = dirs.to_vec();
        parts.extend(leaf);
        parts
    };
    vec![
        (with_leaf(vec![format!("{name}.r")]), ModuleKind::File),
        (with_leaf(vec![format!("{name}.R")]), ModuleKind::File),
        (
            with_leaf(vec![name.to_string(), "__init__.r".to_string()]),
            ModuleKind::Package,
        ),
        (
            with_leaf(vec![name.to_string(), "__init__.R".to_string()]),
            ModuleKind::Package,
        ),
    ]
}

/// Result of walking a candidate's parts below the base directory.
enum SuffixResolution {
    /// Every component matched case-exactly; the built path exists.
    Exact(PathBuf),
    /// Every component matched, but at least one only case-insensitively; the
    /// held path is the actual, differently-cased path on disk.
    Folded(PathBuf),
    /// Some component had no match (or an ambiguous 2+ case-insensitive match).
    Missing,
}

/// Walk `parts` below `base`, matching each component case-exactly where
/// possible and folding a single case-insensitive match otherwise.
fn resolve_suffix(base: &Path, parts: &[String]) -> SuffixResolution {
    let mut cur = base.to_path_buf();
    let mut folded = false;
    for part in parts {
        match match_entry(&cur, OsStr::new(part.as_str())) {
            ComponentMatch::Exact(real) => cur.push(real),
            ComponentMatch::Folded(real) => {
                folded = true;
                cur.push(real);
            }
            ComponentMatch::None => return SuffixResolution::Missing,
        }
    }
    // The terminal candidate must be a regular file. A directory named like
    // `mod.r` is not a module and must not shadow the later
    // `mod/__init__.r` candidate.
    if !cur.is_file() {
        return SuffixResolution::Missing;
    }
    if folded {
        SuffixResolution::Folded(cur)
    } else {
        SuffixResolution::Exact(cur)
    }
}

/// How a single path component matched a directory entry.
enum ComponentMatch {
    /// An entry with byte-identical name exists.
    Exact(OsString),
    /// No exact entry, but exactly one entry matches case-insensitively.
    Folded(OsString),
    /// No match, or 2+ ambiguous case-insensitive matches (fail closed).
    None,
}

/// Match `name` against the entries of `dir`: exact wins; otherwise a *unique*
/// case-insensitive match folds. ASCII-only comparison, matching the general
/// resolver's [`real_entry_name_unique`](crate::cross_file::path_resolve).
fn match_entry(dir: &Path, name: &OsStr) -> ComponentMatch {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return ComponentMatch::None;
    };
    let mut ci_match: Option<OsString> = None;
    let mut ci_count = 0usize;
    for entry in entries.flatten() {
        let entry_name = entry.file_name();
        if entry_name == name {
            return ComponentMatch::Exact(entry_name);
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
    match (ci_count, ci_match) {
        (1, Some(real)) => ComponentMatch::Folded(real),
        _ => ComponentMatch::None,
    }
}

/// Join a base directory with `parts` (the literal, as-written candidate path).
fn join_parts(base: &Path, parts: &[String]) -> PathBuf {
    let mut p = base.to_path_buf();
    for part in parts {
        p.push(part);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn uri(p: &Path) -> Url {
        Url::from_file_path(p).unwrap()
    }

    fn local(up: usize, components: &[&str]) -> BoxSpec {
        BoxSpec::LocalModule {
            up_levels: up,
            components: components.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn resolves_sibling_file_module() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("helpers.r"), "x <- 1\n").unwrap();
        let importer = uri(&dir.path().join("main.R"));

        let resolved = resolve_local_module(&importer, &local(0, &["helpers"])).unwrap();
        assert_eq!(resolved.kind, ModuleKind::File);
        assert_eq!(resolved.uri, uri(&dir.path().join("helpers.r")));
    }

    #[test]
    fn file_module_beats_package_module() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("mod.r"), "x <- 1\n").unwrap();
        fs::create_dir(dir.path().join("mod")).unwrap();
        fs::write(dir.path().join("mod/__init__.r"), "y <- 2\n").unwrap();
        let importer = uri(&dir.path().join("main.R"));

        let resolved = resolve_local_module(&importer, &local(0, &["mod"])).unwrap();
        assert_eq!(resolved.kind, ModuleKind::File);
        assert_eq!(resolved.uri, uri(&dir.path().join("mod.r")));
    }

    #[test]
    fn directory_named_like_file_does_not_shadow_package_module() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("mod.r")).unwrap();
        fs::create_dir(dir.path().join("mod")).unwrap();
        fs::write(dir.path().join("mod/__init__.r"), "y <- 2\n").unwrap();
        let importer = uri(&dir.path().join("main.R"));

        let resolved = resolve_local_module(&importer, &local(0, &["mod"])).unwrap();
        assert_eq!(resolved.kind, ModuleKind::Package);
        assert_eq!(resolved.uri, uri(&dir.path().join("mod/__init__.r")));
    }

    #[test]
    fn resolves_package_module_init() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("pkg")).unwrap();
        fs::write(dir.path().join("pkg/__init__.R"), "y <- 2\n").unwrap();
        let importer = uri(&dir.path().join("main.R"));

        let resolved = resolve_local_module(&importer, &local(0, &["pkg"])).unwrap();
        assert_eq!(resolved.kind, ModuleKind::Package);
        assert_eq!(resolved.uri, uri(&dir.path().join("pkg/__init__.R")));
    }

    #[test]
    fn lowercase_r_beats_uppercase_r() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("m.r"), "1\n").unwrap();
        fs::write(dir.path().join("m.R"), "2\n").unwrap();
        // On a case-insensitive FS these collapse to one file; only assert when
        // both truly coexist (case-sensitive FS).
        if dir.path().join("m.r").exists() && dir.path().join("m.R").exists() {
            let importer = uri(&dir.path().join("main.R"));
            let resolved = resolve_local_module(&importer, &local(0, &["m"])).unwrap();
            assert_eq!(resolved.uri, uri(&dir.path().join("m.r")));
        }
    }

    #[test]
    fn resolves_nested_dirs_and_parent_hops() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("lib/sub")).unwrap();
        fs::write(dir.path().join("lib/sub/util.r"), "1\n").unwrap();
        fs::create_dir(dir.path().join("here")).unwrap();
        let importer = uri(&dir.path().join("here/main.R"));

        // `../lib/sub/util` from here/main.R
        let resolved = resolve_local_module(&importer, &local(1, &["lib", "sub", "util"])).unwrap();
        assert_eq!(resolved.uri, uri(&dir.path().join("lib/sub/util.r")));
    }

    #[test]
    fn missing_module_reports_searched_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let importer = uri(&dir.path().join("main.R"));
        let err = resolve_local_module(&importer, &local(0, &["nope"])).unwrap_err();
        match err {
            BoxResolveError::NotFound { searched } => {
                assert_eq!(searched.len(), 4);
                assert!(searched.contains(&dir.path().join("nope.r")));
                assert!(searched.contains(&dir.path().join("nope/__init__.R")));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn case_only_mismatch_is_diagnosed_not_corrected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Helpers.r"), "1\n").unwrap();
        let importer = uri(&dir.path().join("main.R"));

        // Spec `./helpers` (lowercase) must NOT silently resolve to `Helpers.r`.
        let result = resolve_local_module(&importer, &local(0, &["helpers"]));
        // On a case-insensitive FS `helpers.r` would exact-match `Helpers.r`'s
        // inode; only assert the mismatch path when the FS is case-sensitive.
        if !dir.path().join("helpers.r").exists() {
            match result {
                Err(BoxResolveError::CaseMismatch { expected, found }) => {
                    assert_eq!(expected, dir.path().join("helpers.r"));
                    assert_eq!(found, dir.path().join("Helpers.r"));
                }
                other => panic!("expected CaseMismatch, got {other:?}"),
            }
        }
    }

    #[test]
    fn package_and_unsupported_specs_are_not_local() {
        let importer = Url::parse("file:///proj/main.R").unwrap();
        assert_eq!(
            resolve_local_module(&importer, &BoxSpec::Package("dplyr".into())),
            Err(BoxResolveError::NotLocalModule)
        );
        assert_eq!(
            resolve_local_module(&importer, &BoxSpec::Unsupported("foo/bar".into())),
            Err(BoxResolveError::NotLocalModule)
        );
        assert!(candidate_paths(&importer, &BoxSpec::Package("dplyr".into())).is_empty());
    }

    #[test]
    fn candidate_paths_lists_all_four_in_order() {
        let importer = Url::parse("file:///proj/main.R").unwrap();
        let paths = candidate_paths(&importer, &local(0, &["mod"]));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/proj/mod.r"),
                PathBuf::from("/proj/mod.R"),
                PathBuf::from("/proj/mod/__init__.r"),
                PathBuf::from("/proj/mod/__init__.R"),
            ]
        );
    }

    #[test]
    fn watched_candidate_matching_includes_case_only_spelling() {
        let importer = Url::parse("file:///proj/main.R").unwrap();
        let spec = local(0, &["mod"]);
        assert!(candidate_set_matches_path(
            &importer,
            &spec,
            Path::new("/proj/Mod.r")
        ));
        assert!(!candidate_set_matches_path(
            &importer,
            &spec,
            Path::new("/proj/other.r")
        ));
    }
}
