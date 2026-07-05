//! R package mode subsystem.
//!
//! See `docs/superpowers/specs/2026-05-10-r-package-mode-architecture-design.md`
//! for the architectural rationale.
//!
//! This module owns all derived state for R package mode. Outside of this
//! module, `PackageState` is read-only — it can only be replaced as a
//! whole, never partially mutated.

pub mod derive;
pub use derive::derive_package_state;
pub mod digest;
pub use digest::ContentDigest;
pub mod event;
pub mod rprofile;
pub mod sysdata;

#[cfg(test)]
mod proptest_machine;

use std::path::PathBuf;
use std::sync::Arc;

use crate::package_namespace::{PackageNamespaceModel, PackageWorkspace};
use crate::roxygen::RoxygenNamespace;

/// Derived state for R package mode. Owned by `WorldState`.
/// Fully derive-based since Phase 5b: all fields are computed by `derive_package_state`.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct PackageState {
    pub(super) workspace: Option<PackageWorkspace>,
    pub(super) namespace_model: Option<PackageNamespaceModel>,

    // Populated by derive_package_state
    pub(super) r_file_facts: BTreeMap<PathBuf, RFileFacts>,
    pub(super) scope_contribution: PackageScopeContribution,
}

impl PackageState {
    pub fn new() -> Self {
        Self {
            workspace: None,
            namespace_model: None,
            r_file_facts: BTreeMap::new(),
            scope_contribution: PackageScopeContribution::default(),
        }
    }

    pub fn workspace(&self) -> Option<&PackageWorkspace> {
        self.workspace.as_ref()
    }

    pub fn namespace_model(&self) -> Option<&PackageNamespaceModel> {
        self.namespace_model.as_ref()
    }

    pub fn r_file_facts(&self) -> &BTreeMap<PathBuf, RFileFacts> {
        &self.r_file_facts
    }

    pub fn scope_contribution(&self) -> &PackageScopeContribution {
        &self.scope_contribution
    }

    /// Replace all derived package-mode state in one step.
    ///
    /// `PackageState` fields stay non-public so consumers cannot update one
    /// derived cache without the others. Event handlers update
    /// `PackageInputs`, call `derive_package_state`, and then install the
    /// complete result through this method.
    pub(super) fn set_from(&mut self, new: PackageState) {
        *self = new;
    }
}

// ============== INPUTS ==============

use crate::cross_file::config::PackageMode;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct PackageInputs {
    pub workspace_root: Option<PathBuf>,
    pub package_mode: PackageMode,
    pub description: Option<DescriptionInput>,
    pub namespace: Option<NamespaceInput>,
    pub r_files: BTreeMap<PathBuf, RFileInput>,
    /// Dataset names discovered from `<root>/data/`. Populated by startup
    /// scan and updated on watched-file changes. Includes file stems of
    /// `data/*.{rda,RData,rds,tab,txt,csv}` and top-level assignments from
    /// `data/*.R` scripts.
    pub dataset_names: BTreeSet<String>,
    /// Symbol names from `R/sysdata.rda`. Populated by AST-scanning
    /// `data-raw/**/*.R` for `use_data(..., internal=TRUE)` and
    /// `save(..., file="...sysdata.rda")` calls, with an R-subprocess
    /// fallback when AST finds nothing and an R executable is available.
    pub sysdata_names: BTreeSet<String>,
    /// Whether `.Rprofile` prelude modeling is enabled (mirrors
    /// `CrossFileConfig.model_rprofile`). Carried here so the watched-file
    /// `translate` path can gate the scan without reaching for config.
    /// Set by `initialize_package_inputs_from_state_with_exclusions`; `Default`
    /// is `false`
    /// (seeders set the real value from config, which defaults `true`).
    pub model_rprofile: bool,
    /// Top-level symbol names introduced by the workspace-root `.Rprofile`
    /// (and its transitive literal `source()` targets). Populated by
    /// `rprofile::scan_workspace_rprofile`. Empty when modeling is off or the
    /// file is absent.
    pub rprofile_symbols: BTreeSet<String>,
    /// Packages attached (top-level `library()`/`require()`) by the
    /// workspace-root `.Rprofile` and its transitive `source()` targets.
    pub rprofile_attached_packages: BTreeSet<String>,
    /// Canonical paths of helper files the prelude followed via `source()`
    /// (from `RprofileScan::sourced_files`). Used by the optional
    /// transitive-freshness wiring (Task 12) to rescan when a sourced helper is
    /// edited. Not carried onto the contribution (watch-routing only).
    pub rprofile_sourced_files: BTreeSet<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct DescriptionInput {
    pub text: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct NamespaceInput {
    pub text: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct RFileInput {
    pub kind: RFileKind,
    pub text: Arc<str>,
    pub content_digest: ContentDigest,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum RFileKind {
    Source,
    Test,
}

#[cfg(test)]
mod input_tests {
    use super::*;

    #[test]
    fn default_inputs_are_empty() {
        let inputs = PackageInputs::default();
        assert!(inputs.workspace_root.is_none());
        assert!(inputs.r_files.is_empty());
    }
}

// ============== DELTA ==============

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageInputDelta {
    Initial,
    RFileChanged { path: PathBuf, kind: RFileKind },
    RFileDeleted { path: PathBuf, kind: RFileKind },
    NamespaceChanged,
    DescriptionChanged,
    SettingChanged,
    DataDirChanged,
    RProfileChanged,
    Batch(Vec<PackageInputDelta>),
}

// ============== PATH HELPERS ==============

use std::path::Path;

/// Returns `Some(kind)` if `path` is a package source/test file we track,
/// based on the workspace root. Returns `None` otherwise.
///
/// Rules:
/// - `<root>/R/**/*.R` (or `*.r`) → `Source`
/// - `<root>/tests/testthat/**/*.R` (or `*.r`) → `Test`
/// - `<root>/tests/testit/**/*.R` (or `*.r`) → `Test`
/// - `<root>/tests/*.R` (direct children only, or `*.r`) → `Test`
/// - `<root>/inst/tinytest/**/*.R` (or `*.r`) → `Test`
/// - `<root>/inst/unitTests/**/*.R` (or `*.r`) → `Test`
/// - everything else → `None`
///
/// `inst/tinytest/` and `inst/unitTests/` are installed test suites that run
/// with the package loaded, so they are `Test`-kind (one-way package R/
/// visibility) like `tests/testthat/`. They are NOT testthat-managed, so
/// [`is_testthat_or_testit_test`] still excludes them from testthat-specific
/// helper/attached-package injection.
pub fn is_r_source_path(path: &Path, workspace_root: &Path) -> Option<RFileKind> {
    let rel = path.strip_prefix(workspace_root).ok()?;
    let mut comps = rel.components();
    let first = comps.next()?.as_os_str().to_str()?;

    let is_r_extension = matches!(path.extension().and_then(|e| e.to_str()), Some("R" | "r"),);
    if !is_r_extension {
        return None;
    }

    match first {
        "R" => Some(RFileKind::Source),
        "inst" => {
            // Installed test suites run with the package loaded.
            let second = comps.next()?.as_os_str().to_str()?;
            if second == "tinytest" || second == "unitTests" {
                Some(RFileKind::Test)
            } else {
                None
            }
        }
        "tests" => {
            let second = comps.next()?.as_os_str().to_str()?;
            if second == "testthat" || second == "testit" {
                Some(RFileKind::Test)
            } else if comps.next().is_none() {
                // Direct child of tests/ (no further path components) —
                // plain R CMD check test script.
                Some(RFileKind::Test)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Returns `true` when `path` is under `<root>/tests/testthat/` or
/// `<root>/tests/testit/` — i.e. a testthat/testit-managed test file,
/// NOT a plain `tests/*.R` script. Used to gate testthat-specific
/// injections (helper symbols, test_attached_packages).
pub fn is_testthat_or_testit_test(path: &Path, workspace_root: &Path) -> bool {
    let Some(rel) = path.strip_prefix(workspace_root).ok() else {
        return false;
    };
    let mut comps = rel.components();
    let Some(first) = comps.next().and_then(|c| c.as_os_str().to_str()) else {
        return false;
    };
    if first != "tests" {
        return false;
    }
    let Some(second) = comps.next().and_then(|c| c.as_os_str().to_str()) else {
        return false;
    };
    second == "testthat" || second == "testit"
}

/// Returns `true` when `path` is an R file under one of the package's
/// "dev-context" directories: `demo/`, `vignettes/`, `data-raw/`, `man/`.
/// These directories see the package's own R/ top-level symbols and NAMESPACE
/// imports (one-way: their defs never leak into R/, and they don't see each
/// other) because the package is loaded when their code runs. Package mode
/// only.
///
/// `inst/` and `revdep/` are deliberately excluded: plain `inst/` scripts
/// (examples, shiny apps, rmarkdown templates) and reverse-dependency checks
/// are not run with the package implicitly loaded, so they rely on explicit
/// `library()`/directives like any other script. (Installed test suites under
/// `inst/tinytest/` and `inst/unitTests/` are handled separately as `Test`-kind
/// files by [`is_r_source_path`].)
pub fn is_dev_context_path(path: &Path, workspace_root: &Path) -> bool {
    let Some(rel) = path.strip_prefix(workspace_root).ok() else {
        return false;
    };
    let is_r_extension = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("R" | "r" | "Rmd" | "rmd" | "qmd")
    );
    if !is_r_extension {
        return false;
    }
    let Some(first) = rel.components().next().and_then(|c| c.as_os_str().to_str()) else {
        return false;
    };
    matches!(first, "demo" | "data-raw" | "vignettes" | "man")
}

/// True when `path` is an R file under the package's built/checked doc dirs
/// (`vignettes/`, `man/`, `demo/`) — rebuilt by `R CMD build` / run by
/// `R CMD check` with the user profile suppressed. Used (in package mode only)
/// to withhold the `.Rprofile` prelude. DELIBERATELY NARROWER than
/// [`is_dev_context_path`]: `data-raw/` is dev-only, `.Rbuildignore`d, and run
/// interactively from the root, so the prelude APPLIES there.
pub fn is_built_doc_dir_path(path: &Path, workspace_root: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(workspace_root) else {
        return false;
    };
    let is_r_extension = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("R" | "r" | "Rmd" | "rmd" | "qmd")
    );
    if !is_r_extension {
        return false;
    }
    let Some(first) = rel.components().next().and_then(|c| c.as_os_str().to_str()) else {
        return false;
    };
    matches!(first, "vignettes" | "man" | "demo")
}

/// In package mode, the `.Rprofile` prelude is withheld from files whose
/// canonical run context is a profile-suppressed `R CMD check` / `build`
/// session: namespace `R/` (Source) and all test files (via
/// [`is_r_source_path`]), plus built doc dirs (via [`is_built_doc_dir_path`]).
/// Callers apply this ONLY when a package workspace is active — in script mode
/// the prelude applies everywhere, including `R/`.
pub fn rprofile_withheld_in_package_mode(path: &Path, workspace_root: &Path) -> bool {
    is_r_source_path(path, workspace_root).is_some() || is_built_doc_dir_path(path, workspace_root)
}

/// Returns `true` when `path` is an R file anywhere under the workspace root
/// that should see the package's own dataset symbols. This is broader than
/// `is_r_source_path`: datasets are visible in R/, tests/, vignettes/, inst/,
/// demo/, and data-raw/ — essentially any `.R` file in the package tree.
pub fn is_package_workspace_r_file(path: &Path, workspace_root: &Path) -> bool {
    if path.strip_prefix(workspace_root).is_err() {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("R" | "r" | "Rmd" | "rmd" | "qmd")
    )
}

/// Synchronously scan `<workspace_root>/data/` for dataset names.
///
/// Returns file stems of recognized data file extensions plus top-level
/// assignment names from `data/*.R` scripts. This mirrors
/// [`crate::namespace_parser::parse_data_symbols`] but operates synchronously
/// and additionally extracts top-level defs from `.R` scripts (which create
/// dataset objects at load time via side-effects).
pub fn scan_own_package_data_dir(workspace_root: &Path) -> BTreeSet<String> {
    scan_own_package_data_dir_impl::<false>(workspace_root, None)
}

/// Like [`scan_own_package_data_dir`], but skips files matched by
/// `[workspace].exclude`.
pub fn scan_own_package_data_dir_with_exclusions(
    workspace_root: &Path,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> BTreeSet<String> {
    if exclusions.is_empty() {
        return scan_own_package_data_dir(workspace_root);
    }
    scan_own_package_data_dir_impl::<true>(workspace_root, Some(exclusions))
}

fn scan_own_package_data_dir_impl<const USE_EXCLUSIONS: bool>(
    workspace_root: &Path,
    exclusions: Option<&crate::config_file::CompiledWorkspaceExclusions>,
) -> BTreeSet<String> {
    use std::fs;

    let data_dir = workspace_root.join("data");
    let mut symbols = BTreeSet::new();

    let data_meta = match fs::symlink_metadata(&data_dir) {
        Ok(m) => m,
        Err(_) => return symbols,
    };
    if !data_meta.is_dir() {
        return symbols;
    }

    // datalist file (same format as installed packages)
    let datalist_path = data_dir.join("datalist");
    if (!USE_EXCLUSIONS || !exclusions.is_some_and(|e| e.is_excluded_path(&datalist_path)))
        && let Ok(content) = fs::read_to_string(&datalist_path)
    {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((primary, rest)) = line.split_once(':') {
                let primary = primary.trim();
                if !primary.is_empty() {
                    symbols.insert(primary.to_string());
                }
                for sub in rest.split_whitespace() {
                    if !sub.is_empty() {
                        symbols.insert(sub.to_string());
                    }
                }
            } else if !line.is_empty() {
                symbols.insert(line.to_string());
            }
        }
    }

    // Recognized data-file extensions (matches namespace_parser::data_file_stem)
    const SERIALIZED_EXTS: &[&str] = &["rda", "rdata", "rds"];
    const TABULAR_EXTS: &[&str] = &["csv", "tab", "txt"];
    const COMPRESSION_EXTS: &[&str] = &["gz", "bz2", "xz"];
    const SKIP_FILES: &[&str] = &["Rdata.rdb", "Rdata.rdx", "Rdata.rds", "datalist"];

    let entries = match fs::read_dir(&data_dir) {
        Ok(e) => e,
        Err(_) => return symbols,
    };

    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if USE_EXCLUSIONS && exclusions.is_some_and(|e| e.is_excluded_path(&path)) {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if SKIP_FILES.contains(&file_name) {
            continue;
        }

        // Check for .R scripts — parse for top-level defs
        let ext_lc = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext_lc == "r" {
            if let Ok(content) = fs::read_to_string(&path) {
                let defs = crate::roxygen::extract_top_level_defs(&content);
                symbols.extend(defs);
            }
            continue;
        }

        // Serialized data files: stem is dataset name
        if SERIALIZED_EXTS.contains(&ext_lc.as_str()) || TABULAR_EXTS.contains(&ext_lc.as_str()) {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                symbols.insert(stem.to_string());
            }
            continue;
        }

        // Compressed tabular: strip compression ext, check inner ext
        if COMPRESSION_EXTS.contains(&ext_lc.as_str()) {
            let stem_outer = file_name.rsplit_once('.').map(|(s, _)| s).unwrap_or("");
            if let Some((inner_stem, inner_ext)) = stem_outer.rsplit_once('.')
                && TABULAR_EXTS.contains(&inner_ext.to_ascii_lowercase().as_str())
            {
                symbols.insert(inner_stem.to_string());
            }
        }
    }

    symbols
}

/// Returns `true` for testthat-recognized test-preamble files: files under
/// `tests/testthat/` whose basename starts with `"helper"` or `"setup"`
/// (case-sensitive match against testthat's own loaders —
/// `source_test_helpers` sources `^helper.*\\.[rR]$` and
/// `source_test_setup` sources `^setup.*\\.[rR]$`, in that order, before any
/// test file runs). Preamble top-level definitions are visible to peer files
/// under `tests/testthat/`, but never propagate to `R/`.
///
/// Teardown files (`teardown*.R`) are deliberately NOT matched: testthat
/// sources them only AFTER all tests finish, so their bindings are never
/// visible to test code.
///
/// The caller is responsible for first confirming the file is a **direct
/// child of `tests/testthat/`** (e.g. `path.parent() == <root>/tests/testthat`,
/// as `derive.rs` does). `is_r_source_path` returning `RFileKind::Test` is NOT
/// a sufficient gate — it also matches `tests/testit/` and plain `tests/*.R`
/// files, where testthat's helper/setup sourcing semantics do not apply. This
/// function only inspects the basename.
pub fn is_test_preamble_filename(file_name: &str) -> bool {
    // Prefix is case-sensitive to match testthat's regexes
    // `^helper.*\.[rR]$` / `^setup.*\.[rR]$`; only the extension accepts
    // either `R` or `r`.
    if !file_name.starts_with("helper") && !file_name.starts_with("setup") {
        return false;
    }
    matches!(
        Path::new(file_name).extension().and_then(|e| e.to_str()),
        Some("R" | "r")
    )
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    #[allow(non_snake_case)]
    fn r_source_path_recognizes_R_dir() {
        assert_eq!(
            is_r_source_path(Path::new("/work/pkg/R/utils.R"), Path::new("/work/pkg")),
            Some(RFileKind::Source),
        );
    }

    #[test]
    fn r_source_path_recognizes_testthat() {
        assert_eq!(
            is_r_source_path(
                Path::new("/work/pkg/tests/testthat/test-utils.R"),
                Path::new("/work/pkg")
            ),
            Some(RFileKind::Test),
        );
    }

    #[test]
    fn r_source_path_recognizes_testit() {
        assert_eq!(
            is_r_source_path(
                Path::new("/work/pkg/tests/testit/test-utils.R"),
                Path::new("/work/pkg")
            ),
            Some(RFileKind::Test),
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn r_source_path_rejects_non_R_files() {
        let root = Path::new("/work/pkg");
        assert_eq!(
            is_r_source_path(Path::new("/work/pkg/R/utils.txt"), root),
            None
        );
        assert_eq!(
            is_r_source_path(Path::new("/work/pkg/inst/data.R"), root),
            None
        );
        assert_eq!(
            is_r_source_path(Path::new("/elsewhere/utils.R"), root),
            None
        );
    }

    #[test]
    fn r_source_path_handles_lowercase_extension() {
        assert_eq!(
            is_r_source_path(Path::new("/work/pkg/R/utils.r"), Path::new("/work/pkg")),
            Some(RFileKind::Source),
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn r_source_path_recognizes_subdirs_in_R() {
        assert_eq!(
            is_r_source_path(
                Path::new("/work/pkg/R/unix/utils.R"),
                Path::new("/work/pkg")
            ),
            Some(RFileKind::Source),
        );
    }

    #[test]
    fn test_helper_filename_recognizes_helper_prefix() {
        assert!(is_test_preamble_filename("helper.R"));
        assert!(is_test_preamble_filename("helper-utils.R"));
        assert!(is_test_preamble_filename("helper_utils.R"));
        assert!(is_test_preamble_filename("helper.r"));
    }

    /// testthat also sources `setup*.R` files (`^setup.*\.[rR]$`) before any
    /// test runs, so their top-level bindings are visible to test files
    /// exactly like helper defs. Real-world FP this guards against:
    /// googledrive's `tests/testthat/setup-testing.R` defines
    /// `CLEAN <- SETUP <- FALSE`, referenced by 17 `test-*.R` files.
    #[test]
    fn test_preamble_filename_recognizes_setup_prefix() {
        assert!(is_test_preamble_filename("setup.R"));
        assert!(is_test_preamble_filename("setup-testing.R"));
        assert!(is_test_preamble_filename("setup_db.R"));
        assert!(is_test_preamble_filename("setup.r"));
        // testthat's pattern is `^setup.*` — any "setup" prefix matches,
        // even without a separator.
        assert!(is_test_preamble_filename("setupx.R"));
    }

    #[test]
    fn test_helper_filename_rejects_non_helpers() {
        assert!(!is_test_preamble_filename("test-utils.R"));
        // testthat's loader regex is case-sensitive for the prefix:
        // `^helper.*\.[Rr]$` / `^setup.*\.[Rr]$`.
        assert!(!is_test_preamble_filename("Helper-mixedCase.R"));
        assert!(!is_test_preamble_filename("HELPER-shouty.R"));
        assert!(!is_test_preamble_filename("Setup-mixedCase.R"));
        assert!(!is_test_preamble_filename("SETUP-x.r"));
        // Teardown files run AFTER the tests — their bindings are never
        // visible to test code, so they must NOT be treated as preamble.
        assert!(!is_test_preamble_filename("teardown.R"));
        assert!(!is_test_preamble_filename("teardown-db.R"));
        // Prefix matches but extension is not R.
        assert!(!is_test_preamble_filename("helper-data.csv"));
        assert!(!is_test_preamble_filename("helper.txt"));
        assert!(!is_test_preamble_filename("setup-data.csv"));
        assert!(!is_test_preamble_filename("setup.txt"));
        // Too short to start with "helper" / "setup".
        assert!(!is_test_preamble_filename("help.R"));
        assert!(!is_test_preamble_filename("setu.R"));
        // Doesn't start with either prefix.
        assert!(!is_test_preamble_filename("my-helper.R"));
        assert!(!is_test_preamble_filename("my-setup.R"));
    }

    /// Regression: byte-indexed slicing of a multi-byte UTF-8 filename
    /// must not panic. The original implementation evaluated
    /// `file_name[..6].eq_ignore_ascii_case("helper")`, which panics when
    /// byte index 6 falls inside a non-ASCII character.
    #[test]
    fn test_helper_filename_multibyte_safe() {
        // "hel😀.R" — 3 ASCII bytes followed by the 4-byte UTF-8 sequence
        // for U+1F600. Byte index 6 sits in the MIDDLE of the 4-byte
        // emoji (bytes 3..7), so the old `file_name[..6]` slice would
        // panic with "byte index 6 is not a char boundary". The byte-iter
        // implementation must not panic and must not match (prefix bytes
        // 0..6 are "hel" + 3 bytes of emoji, which do not equal "helper").
        let name = "hel\u{1F600}.R";
        assert!(!is_test_preamble_filename(name));
        // A purely non-ASCII prefix must not match (and must not panic).
        assert!(!is_test_preamble_filename("βλέπω-utils.R"));
        // A non-ASCII-leading name that happens to share a tail must not match either.
        assert!(!is_test_preamble_filename("éhelper.R"));
        // Same guarantees for the "setup" prefix: byte index 5 falls inside
        // the emoji, and a non-ASCII-leading tail match must not count.
        assert!(!is_test_preamble_filename("set\u{1F600}.R"));
        assert!(!is_test_preamble_filename("ésetup.R"));
    }

    #[test]
    fn r_source_path_recognizes_plain_tests() {
        let root = Path::new("/work/pkg");
        assert_eq!(
            is_r_source_path(Path::new("/work/pkg/tests/Simple.R"), root),
            Some(RFileKind::Test),
        );
        assert_eq!(
            is_r_source_path(Path::new("/work/pkg/tests/indexing.R"), root),
            Some(RFileKind::Test),
        );
        assert_eq!(
            is_r_source_path(Path::new("/work/pkg/tests/foo.r"), root),
            Some(RFileKind::Test),
        );
    }

    #[test]
    fn r_source_path_rejects_tests_subdirs_other_than_testthat_testit() {
        let root = Path::new("/work/pkg");
        // files in unrecognized subdirs of tests/ should NOT be tracked
        assert_eq!(
            is_r_source_path(Path::new("/work/pkg/tests/other/foo.R"), root),
            None
        );
    }

    #[test]
    fn is_testthat_or_testit_test_distinguishes_correctly() {
        let root = Path::new("/work/pkg");
        assert!(is_testthat_or_testit_test(
            Path::new("/work/pkg/tests/testthat/test-x.R"),
            root
        ));
        assert!(is_testthat_or_testit_test(
            Path::new("/work/pkg/tests/testit/test-x.R"),
            root
        ));
        assert!(!is_testthat_or_testit_test(
            Path::new("/work/pkg/tests/Simple.R"),
            root
        ));
        assert!(!is_testthat_or_testit_test(
            Path::new("/work/pkg/R/utils.R"),
            root
        ));
    }

    #[test]
    fn dev_context_path_recognizes_all_dirs() {
        let root = Path::new("/work/pkg");
        assert!(is_dev_context_path(
            Path::new("/work/pkg/demo/example.R"),
            root
        ));
        assert!(is_dev_context_path(
            Path::new("/work/pkg/data-raw/prepare.R"),
            root
        ));
        assert!(is_dev_context_path(
            Path::new("/work/pkg/vignettes/intro.Rmd"),
            root
        ));
        assert!(is_dev_context_path(
            Path::new("/work/pkg/man/rmd/topic.Rmd"),
            root
        ));
    }

    /// F4: `inst/` and `revdep/` are no longer blanket dev-context — plain
    /// `inst/` scripts and revdep checks rely on explicit `library()`.
    #[test]
    fn dev_context_path_excludes_inst_and_revdep() {
        let root = Path::new("/work/pkg");
        assert!(!is_dev_context_path(
            Path::new("/work/pkg/inst/script.R"),
            root
        ));
        assert!(!is_dev_context_path(
            Path::new("/work/pkg/inst/extdata/helper.R"),
            root
        ));
        assert!(!is_dev_context_path(
            Path::new("/work/pkg/revdep/check.R"),
            root
        ));
        // A bare reference inside an installed rmarkdown template skeleton is
        // NOT silenced: the file sees no implicit package symbols.
        assert!(!is_dev_context_path(
            Path::new("/work/pkg/inst/rmarkdown/templates/report/skeleton/skeleton.Rmd"),
            root
        ));
    }

    /// F4: installed test suites under `inst/tinytest/` and `inst/unitTests/`
    /// are `Test`-kind (one-way package R/ visibility) — they run with the
    /// package loaded.
    #[test]
    fn r_source_path_recognizes_inst_test_suites() {
        let root = Path::new("/work/pkg");
        assert_eq!(
            is_r_source_path(Path::new("/work/pkg/inst/tinytest/test_a.R"), root),
            Some(RFileKind::Test),
        );
        assert_eq!(
            is_r_source_path(Path::new("/work/pkg/inst/unitTests/runit.foo.R"), root),
            Some(RFileKind::Test),
        );
        // Other inst/ R files remain untracked.
        assert_eq!(
            is_r_source_path(Path::new("/work/pkg/inst/script.R"), root),
            None,
        );
        // tinytest/unitTests are not testthat-managed, so testthat-specific
        // injection still excludes them.
        assert!(!is_testthat_or_testit_test(
            Path::new("/work/pkg/inst/tinytest/test_a.R"),
            root
        ));
    }

    #[test]
    fn dev_context_path_rejects_non_dev_dirs() {
        let root = Path::new("/work/pkg");
        // R/ is not dev-context (it's Source)
        assert!(!is_dev_context_path(Path::new("/work/pkg/R/utils.R"), root));
        // tests/ is not dev-context (it's Test)
        assert!(!is_dev_context_path(
            Path::new("/work/pkg/tests/testthat/test-x.R"),
            root
        ));
        // Outside workspace
        assert!(!is_dev_context_path(
            Path::new("/other/inst/script.R"),
            root
        ));
        // Non-R extension
        assert!(!is_dev_context_path(
            Path::new("/work/pkg/inst/data.csv"),
            root
        ));
        // Random dir
        assert!(!is_dev_context_path(
            Path::new("/work/pkg/src/code.R"),
            root
        ));
    }

    #[test]
    fn built_doc_dir_path_matches_vignettes_man_demo_only() {
        let root = Path::new("/work/pkg");
        assert!(is_built_doc_dir_path(
            Path::new("/work/pkg/vignettes/v.R"),
            root
        ));
        assert!(is_built_doc_dir_path(Path::new("/work/pkg/man/ex.R"), root));
        assert!(is_built_doc_dir_path(Path::new("/work/pkg/demo/d.R"), root));
        // data-raw is APPLIED to (not a built doc dir) — narrower than is_dev_context_path.
        assert!(!is_built_doc_dir_path(
            Path::new("/work/pkg/data-raw/prep.R"),
            root
        ));
        assert!(!is_built_doc_dir_path(
            Path::new("/work/pkg/scripts/a.R"),
            root
        ));
    }

    #[test]
    fn rprofile_withheld_covers_namespace_tests_built_dirs() {
        let root = Path::new("/work/pkg");
        assert!(rprofile_withheld_in_package_mode(
            Path::new("/work/pkg/R/f.R"),
            root
        ));
        assert!(rprofile_withheld_in_package_mode(
            Path::new("/work/pkg/tests/testthat/test-x.R"),
            root
        ));
        assert!(rprofile_withheld_in_package_mode(
            Path::new("/work/pkg/tests/foo.R"),
            root
        ));
        assert!(rprofile_withheld_in_package_mode(
            Path::new("/work/pkg/vignettes/v.R"),
            root
        ));
        // applied-to dirs are NOT withheld
        assert!(!rprofile_withheld_in_package_mode(
            Path::new("/work/pkg/scripts/a.R"),
            root
        ));
        assert!(!rprofile_withheld_in_package_mode(
            Path::new("/work/pkg/data-raw/prep.R"),
            root
        ));
    }
}

// ============== OUTPUTS (continued) ==============

use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RFileFacts {
    /// Canonical `Source` vs `Test` classification for this file,
    /// carried through from the corresponding `RFileInput`. Consumers
    /// that need to partition facts by location (e.g. `build_scope_contribution`,
    /// `merge_namespace_model`) MUST filter on `kind` rather than re-deriving
    /// the classification from the path, so there is a single source of truth.
    pub kind: RFileKind,
    pub roxygen_namespace: RoxygenNamespace,
    pub top_level_defs: Arc<BTreeSet<String>>,
    /// Symbols bound inside `.onLoad`/`.onAttach` hooks in this file.
    pub onload_bindings: Arc<BTreeSet<String>>,
    /// Packages this file *attaches* via a top-level `library()`/`require()`
    /// call (see [`crate::cross_file::source_detect::extract_attached_packages`]).
    /// Only populated for `Test`-kind files — the sole consumer is
    /// `build_scope_contribution`, which collects the attaches of testthat
    /// preamble files (`helper*.R`/`setup*.R`) so sibling test files inherit
    /// them. Always empty for `Source` files (their `library()` calls are
    /// handled by the standard position-aware scope path, not the package
    /// contribution).
    pub attached_packages: Arc<BTreeSet<String>>,
    pub content_digest: ContentDigest,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageScopeContribution {
    /// The workspace root for this package, if known. Carried here so that
    /// scope-injection logic (Phase 5) can check whether the queried file is
    /// under `R/` or `tests/testthat/` without requiring a separate parameter.
    pub workspace_root: Option<PathBuf>,
    /// The package's own name (DESCRIPTION `Package:` field), if known.
    ///
    /// Threaded here (issue #431) so the undefined-variable collector can
    /// consult the package's own NSE argument policies when analyzing its OWN
    /// files (R/, tests/, vignettes/, man/rmd). Without this, a verb the package
    /// itself exports (e.g. dplyr's `filter`) loses its data-masking policy
    /// inside the package's own test suite / vignettes, so mask arguments like
    /// `x` in `filter(df, x > 1)` are analyzed as ordinary code and falsely
    /// flagged. This feeds ONLY the policy lookup (`NseAnalysis.self_nse_package`,
    /// resolver step 2.5) and is deliberately never added to the in-play package
    /// set used for standard-eval export resolution — so a self-package verb
    /// with no known policy stays conservatively arg-suppressed rather than
    /// being newly checked. `None` when no package workspace is detected, and
    /// `"unknown"` for an `Enabled`-mode workspace with no DESCRIPTION
    /// `Package:` field (harmless — no policy is keyed on `"unknown"`). In
    /// `Auto` mode a missing/empty `Package:` yields no workspace at all.
    pub package_name: Option<String>,
    pub r_internal_symbols: Arc<BTreeSet<String>>,
    pub imported_symbols: Arc<BTreeMap<String, BTreeSet<String>>>,
    pub full_imports: Arc<BTreeSet<String>>,

    /// Packages from `DESCRIPTION` `Depends:` — a *subset* of `full_imports`
    /// (issue #531). `Depends:` is a true attach (R puts the package's exports
    /// on the bare search path when this package loads), so these are also fed
    /// to the NSE meta-package expansion in `collect_in_play_packages`: a
    /// meta-package here (e.g. `tidyverse`) expands to its members so a bare
    /// data-masking verb resolves to the member's policy.
    ///
    /// Tracked separately from `full_imports` precisely so this hardcoded
    /// meta-expansion does NOT also apply to NAMESPACE `import(pkg)` / roxygen
    /// `@import` entries (which also live in `full_imports` but are selective
    /// namespace imports, not attaches — `import(tidyverse)` does not put
    /// dplyr's `filter` on the search path, so expanding it would falsely
    /// suppress a masked column). The package's own name is excluded.
    pub depends_attached_packages: Arc<BTreeSet<String>>,

    /// Packages that should be treated as if attached (via `library(...)`)
    /// when resolving scope for any file under `<root>/tests/testthat/`.
    ///
    /// Populated for testthat when the package's `DESCRIPTION` declares
    /// `testthat` in `Suggests:`, `Imports:`, or `Depends:`. The standard
    /// `tests/testthat.R` runner attaches testthat before sourcing each test
    /// file (matching `testthat::test_check`'s semantics), so test files
    /// transitively see testthat exports without an explicit `library(testthat)`.
    /// These packages are NOT visible to files under `R/` — they are scoped
    /// to `tests/testthat/` only.
    pub test_attached_packages: Arc<BTreeSet<String>>,

    /// Top-level definitions contributed by testthat preamble files —
    /// `tests/testthat/helper*.R` and `setup*.R` (see
    /// [`is_test_preamble_filename`]) — keyed by the preamble file's path so
    /// the scope-injection layer can skip a preamble file's own definitions
    /// when querying that file (otherwise a `use_x()` line earlier in the
    /// file would falsely see `x <- ...` defined later in the same file).
    ///
    /// Visible from any file under `<root>/tests/testthat/` — peer preamble
    /// files see earlier-sourced ones and `test-*.R` files see them all.
    /// Never injected into files under `R/`. Mirrors `r_internal_symbols`
    /// but with the opposite visibility direction.
    ///
    /// `BTreeMap` ordering is intentional — derive iteration is
    /// deterministic so cached `PackageState` equality (used by the
    /// proptest machine) is stable across runs.
    pub test_helper_symbols: Arc<BTreeMap<PathBuf, Arc<BTreeSet<String>>>>,

    /// Packages *attached* (via top-level `library()`/`require()`) by testthat
    /// preamble files — `tests/testthat/helper*.R` and `setup*.R` (see
    /// [`is_test_preamble_filename`]) — keyed by the preamble file's path.
    ///
    /// testthat sources preamble files at the top level before any test runs,
    /// so a `library(tidyr)` in `helper-lib.R` attaches tidyr for every sibling
    /// test file. The scope-injection layer adds these packages to the queried
    /// file's `inherited_packages` (NOT to the symbol set — their exports are
    /// resolved by the package library like any other attached package).
    ///
    /// Keyed by path — and consumed with the same source-order gate as
    /// `test_helper_symbols` — so a preamble file only inherits attaches from
    /// preamble files testthat sources strictly before it, and a preamble
    /// file's own attach is left to the standard position-aware `library()`
    /// path (never re-injected). Visible from any file under
    /// `<root>/tests/testthat/`; never injected into `R/`. This is the
    /// explicit-`library()` analogue of `test_attached_packages` (which models
    /// testthat's own implicit attach).
    pub test_helper_attached_packages: Arc<BTreeMap<PathBuf, Arc<BTreeSet<String>>>>,

    /// Dataset names from the package's own `data/` directory. These are
    /// visible to any `.R` file under the workspace root — R/, tests/,
    /// vignettes/, inst/, demo/, data-raw/ — matching `data()` semantics
    /// for the package's own lazy-data objects.
    ///
    /// Populated from `PackageInputs::dataset_names` which is computed by
    /// scanning `<root>/data/` for file stems of recognized data extensions
    /// plus top-level assignments in `data/*.R` scripts.
    pub dataset_symbols: Arc<BTreeSet<String>>,

    /// Symbols from `R/sysdata.rda` — internal data objects available to
    /// all code within the package namespace at runtime. Visible in R/,
    /// tests/testthat/, and dev-context files. Populated via AST scanning
    /// of `data-raw/**/*.R` for generating calls, with an R-subprocess
    /// fallback.
    pub sysdata_symbols: Arc<BTreeSet<String>>,

    /// Symbols bound inside `.onLoad`/`.onAttach` hooks via
    /// `assign("x", ..., envir=ns)` or `ns$x <- ...`. Visible alongside
    /// `r_internal_symbols`.
    pub onload_symbols: Arc<BTreeSet<String>>,

    /// Symbol names contributed by a workspace-root `.Rprofile` prelude
    /// (assignments + transitive `source()` defs). Injected by
    /// `append_rprofile_prelude` into files where R would source `.Rprofile`
    /// (gated by `rprofile_withheld_in_package_mode` in package mode).
    /// Suppressive-only.
    pub rprofile_symbols: Arc<BTreeSet<String>>,
    /// Packages attached by the `.Rprofile` prelude. Added to a file's
    /// `inherited_packages` under the same applicability rule.
    pub rprofile_attached_packages: Arc<BTreeSet<String>>,
    /// Workspace root used for the `.Rprofile` prelude's path-containment and
    /// applicability checks. Set whenever a workspace root is known (BOTH
    /// package and script mode) — deliberately distinct from `workspace_root`,
    /// which is `Some` only in package mode. `None` when no root is known.
    pub rprofile_root: Option<PathBuf>,
}

impl PackageScopeContribution {
    /// Every symbol this package exposes as a `load_all()` internal — the exact
    /// contents of the local-dev overlay: R/ internals, sysdata objects,
    /// `.onLoad`/`.onAttach` bindings, and NAMESPACE-imported names.
    ///
    /// This is the SINGLE enumeration of those four sources. Both consumers go
    /// through it so they cannot drift: `WorldState::refresh_local_dev_overlay`
    /// (state.rs) collects it into the `LocalDevPackage` symbol set, and
    /// [`Self::is_local_dev_internal`] (the goto contributed-internal gate in
    /// handlers.rs) tests membership against it. Add a new internal-symbol source
    /// here and both pick it up automatically.
    pub fn local_dev_internal_symbols(&self) -> impl Iterator<Item = &str> {
        self.r_internal_symbols
            .iter()
            .map(String::as_str)
            .chain(self.sysdata_symbols.iter().map(String::as_str))
            .chain(self.onload_symbols.iter().map(String::as_str))
            .chain(self.imported_symbols.keys().map(String::as_str))
    }

    /// True iff `name` is exposed by this package as a `load_all()` internal.
    /// Delegates to [`Self::local_dev_internal_symbols`] so it can never disagree
    /// with the overlay build about which sources count. Called O(1) times per
    /// go-to-definition (not per diagnostics symbol), so the linear scan over the
    /// internal set is not on a hot path.
    pub fn is_local_dev_internal(&self, name: &str) -> bool {
        self.local_dev_internal_symbols().any(|s| s == name)
    }
}

#[cfg(test)]
mod scan_data_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn scan_finds_rda_file_stems() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        fs::write(data_dir.join("mpg.rda"), b"fake").unwrap();
        fs::write(data_dir.join("diamonds.RData"), b"fake").unwrap();
        fs::write(data_dir.join("storms.rds"), b"fake").unwrap();

        let syms = scan_own_package_data_dir(tmp.path());
        assert!(syms.contains("mpg"), "got: {:?}", syms);
        assert!(syms.contains("diamonds"), "got: {:?}", syms);
        assert!(syms.contains("storms"), "got: {:?}", syms);
    }

    #[test]
    fn scan_finds_tabular_file_stems() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        fs::write(data_dir.join("relig_income.csv"), b"fake").unwrap();
        fs::write(data_dir.join("table1.tab"), b"fake").unwrap();
        fs::write(data_dir.join("words.txt"), b"fake").unwrap();

        let syms = scan_own_package_data_dir(tmp.path());
        assert!(syms.contains("relig_income"), "got: {:?}", syms);
        assert!(syms.contains("table1"), "got: {:?}", syms);
        assert!(syms.contains("words"), "got: {:?}", syms);
    }

    #[test]
    fn scan_extracts_top_level_defs_from_r_scripts() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        fs::write(
            data_dir.join("starwars.R"),
            "starwars <- data.frame(name = 'Luke')\nstarwars_films <- list()\n",
        )
        .unwrap();

        let syms = scan_own_package_data_dir(tmp.path());
        assert!(syms.contains("starwars"), "got: {:?}", syms);
        assert!(syms.contains("starwars_films"), "got: {:?}", syms);
    }

    #[test]
    fn scan_with_exclusions_skips_excluded_r_script_defs() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        fs::write(data_dir.join("included.R"), "included_dataset <- 1\n").unwrap();
        fs::write(data_dir.join("excluded.R"), "excluded_dataset <- 1\n").unwrap();
        let exclusions = crate::config_file::compile_workspace_exclusions(
            &serde_json::json!({ "workspace": { "exclude": ["data/excluded.R"] } }),
            vec![tmp.path().to_path_buf()],
        );

        let syms = scan_own_package_data_dir_with_exclusions(tmp.path(), &exclusions);

        assert!(syms.contains("included_dataset"), "got: {:?}", syms);
        assert!(
            !syms.contains("excluded_dataset"),
            "excluded data/*.R must not seed dataset names: {:?}",
            syms
        );
    }

    #[test]
    fn scan_handles_compressed_tabular() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        fs::write(data_dir.join("big_data.csv.gz"), b"fake").unwrap();
        fs::write(data_dir.join("compressed.tab.bz2"), b"fake").unwrap();

        let syms = scan_own_package_data_dir(tmp.path());
        assert!(syms.contains("big_data"), "got: {:?}", syms);
        assert!(syms.contains("compressed"), "got: {:?}", syms);
    }

    #[test]
    fn scan_reads_datalist() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        fs::write(
            data_dir.join("datalist"),
            "flights\nairlines: name carrier\n",
        )
        .unwrap();

        let syms = scan_own_package_data_dir(tmp.path());
        assert!(syms.contains("flights"), "got: {:?}", syms);
        assert!(syms.contains("airlines"), "got: {:?}", syms);
        assert!(syms.contains("name"), "got: {:?}", syms);
        assert!(syms.contains("carrier"), "got: {:?}", syms);
    }

    #[test]
    fn scan_returns_empty_when_no_data_dir() {
        let tmp = TempDir::new().unwrap();
        let syms = scan_own_package_data_dir(tmp.path());
        assert!(syms.is_empty());
    }

    #[test]
    fn is_package_workspace_r_file_detects_vignettes() {
        let root = Path::new("/work/pkg");
        assert!(is_package_workspace_r_file(
            Path::new("/work/pkg/vignettes/intro.R"),
            root
        ));
        assert!(is_package_workspace_r_file(
            Path::new("/work/pkg/vignettes/intro.Rmd"),
            root
        ));
        assert!(is_package_workspace_r_file(
            Path::new("/work/pkg/inst/script.R"),
            root
        ));
        assert!(is_package_workspace_r_file(
            Path::new("/work/pkg/demo/demo.R"),
            root
        ));
        assert!(is_package_workspace_r_file(
            Path::new("/work/pkg/data-raw/prep.R"),
            root
        ));
    }

    #[test]
    fn is_package_workspace_r_file_rejects_outside() {
        let root = Path::new("/work/pkg");
        assert!(!is_package_workspace_r_file(
            Path::new("/other/script.R"),
            root
        ));
        assert!(!is_package_workspace_r_file(
            Path::new("/work/pkg/data/foo.csv"),
            root
        ));
    }

    #[test]
    fn scripts_file_reached_only_by_broadened_rprofile_fanout() {
        // The Task-12 backend fanout (`backend.rs`, the `if ns_changed` block in
        // the watched-files handler) adds open files to the revalidation set when
        // a sourced helper edit rescans the prelude. Its predicate is
        // `is_r_source_path(..).is_some() || (rprofile_changed && is_package_workspace_r_file(..))`.
        // The prelude reaches `scripts/` files, which are NOT package source
        // paths — so this invariant must hold or the broadening would be a no-op:
        // a `scripts/*.R` file is matched ONLY by the broadened arm.
        let root = Path::new("/work/pkg");
        let script = Path::new("/work/pkg/scripts/analysis.R");
        assert!(
            is_r_source_path(script, root).is_none(),
            "scripts/ is not a package source path; the existing R/+tests arm must miss it"
        );
        assert!(
            is_package_workspace_r_file(script, root),
            "scripts/ IS a workspace R file; the broadened arm must reach it"
        );
    }
}
