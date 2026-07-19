//! The pure derivation function from `PackageInputs` to `PackageState`.
//!
//! See spec §6.

use super::*;
use crate::package_namespace::parse_dcf_field_pub;

/// Pure derivation from `PackageInputs` to `PackageState`.
///
/// # Why `_delta` is ignored
///
/// This function always performs a full from-scratch semantic recomputation
/// over `inputs`. Any incremental speedup comes from memoization *inside* the
/// function (per-file facts are reused when the `ContentDigest` matches
/// `prev`), not from delta-driven short-circuiting at the call boundary.
///
/// Operational seed freshness is owned by `WorldState`'s
/// `PackageInputLifecycle`, not by semantic `PackageState`, so index application
/// and derivation cannot reset it. This preserves
/// `proptest_machine::advisory_delta_does_not_affect_correctness`: callers may
/// pass `Initial` without changing derived meaning.
pub fn derive_package_state(
    prev: &PackageState,
    inputs: &PackageInputs,
    _delta: &PackageInputDelta,
) -> PackageState {
    let workspace = effective_workspace(inputs);
    let r_file_facts = derive_r_file_facts(&prev.r_file_facts, &inputs.r_files);
    let namespace_model = if workspace.is_some() {
        Some(merge_namespace_model(
            inputs.namespace.as_ref(),
            &r_file_facts,
        ))
    } else {
        None
    };
    let scope_contribution =
        build_scope_contribution(&workspace, &namespace_model, &r_file_facts, inputs);
    PackageState {
        workspace,
        namespace_model,
        r_file_facts,
        scope_contribution,
    }
}

fn build_scope_contribution(
    workspace: &Option<PackageWorkspace>,
    namespace_model: &Option<PackageNamespaceModel>,
    r_file_facts: &BTreeMap<PathBuf, RFileFacts>,
    inputs: &PackageInputs,
) -> PackageScopeContribution {
    let description = inputs.description.as_ref();
    let dataset_names = &inputs.dataset_names;
    let sysdata_names = &inputs.sysdata_names;
    let rprofile_symbols = &inputs.rprofile_symbols;
    let rprofile_attached_packages = &inputs.rprofile_attached_packages;
    let rprofile_root = inputs.workspace_root.clone();
    let Some(ws) = workspace else {
        return PackageScopeContribution {
            rprofile_symbols: Arc::new(rprofile_symbols.clone()),
            rprofile_attached_packages: Arc::new(rprofile_attached_packages.clone()),
            rprofile_root,
            ..PackageScopeContribution::default()
        };
    };
    // r_internal_symbols: union of top_level_defs from Source files only
    // (exclude tests/testthat/* — those are kind == Test). Partition is
    // driven by the canonical `RFileKind` classification carried in
    // `RFileFacts`, not by a path-prefix check.
    //
    // test_helper_symbols: union of top_level_defs from test-preamble files
    // (`tests/testthat/helper*.R` and `setup*.R` — kind == Test AND filename
    // starts with "helper" or "setup"), keyed by file path so the
    // scope-injection layer can skip a preamble file's own self-injection.
    // Visible to peer test files; never injected into R/.
    //
    // test_helper_attached_packages: same preamble files, but their top-level
    // `library()`/`require()` *attaches* (see `RFileFacts::attached_packages`),
    // also keyed by file path. Sibling tests inherit these packages because
    // testthat sources the preamble before each test runs. Same one-way
    // visibility as the defs: peer test files only, never R/.
    //
    // Known limitation (matches `r_internal_symbols`): both sets are built
    // from `top_level_defs`, which captures every top-level assignment
    // without applying timeline-level `rm()` / `remove()`. A helper that
    // does `x <- 1; rm(x)` still contributes `x` to peer-visible names. The
    // rm-aware extractor (`live_top_level_exports`) operates on parsed
    // `ScopeArtifacts`, not on the cheap `top_level_defs` slice; aligning
    // here would require either re-parsing helper files at derive time or
    // re-keying `RFileFacts` around `ScopeArtifacts`. This edge case is
    // uncommon in real testthat helpers, so it is left untreated for
    // symmetry with `r_internal_symbols` (which has the same limitation
    // for R/ files).
    let mut r_internal_symbols: BTreeSet<String> = BTreeSet::new();
    let mut onload_all: BTreeSet<String> = BTreeSet::new();
    let mut test_helper_symbols: BTreeMap<PathBuf, Arc<BTreeSet<String>>> = BTreeMap::new();
    let mut test_helper_attached_packages: BTreeMap<PathBuf, Arc<BTreeSet<String>>> =
        BTreeMap::new();
    for (path, facts) in r_file_facts {
        match facts.kind {
            RFileKind::Source => {
                for def in facts.top_level_defs.iter() {
                    r_internal_symbols.insert(def.clone());
                }
                for sym in facts.onload_bindings.iter() {
                    onload_all.insert(sym.clone());
                }
            }
            RFileKind::Test => {
                // Preamble files must be direct children of `tests/testthat/`
                // — `testthat::source_test_helpers` / `source_test_setup` use
                // non-recursive `dir(path, pattern = "^helper.*\\.[Rr]$")` /
                // `dir(path, pattern = "^setup.*\\.[Rr]$")`, so files in
                // subdirectories (e.g. `tests/testthat/sub/helper-x.R`)
                // are never auto-sourced. Treating them as preamble here
                // would silently mute legitimate undefined-variable
                // diagnostics for any symbol they define.
                let direct_child = ws.root.join("tests").join("testthat");
                let is_direct_child_of_testthat = path.parent() == Some(direct_child.as_path());
                let is_preamble = is_direct_child_of_testthat
                    && path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(super::is_test_preamble_filename)
                        .unwrap_or(false);
                if is_preamble {
                    // Reuse the preamble file's `top_level_defs` `Arc` rather
                    // than cloning into a fresh set — keeps
                    // `derive_package_state` O(preamble-file count) on the
                    // cached path instead of O(total preamble defs). When the
                    // preamble-source scan harvested defs from the preamble's
                    // transitive `source()` targets (issue #638), union them
                    // in (allocating only for preambles that actually source
                    // something).
                    let defs = union_or_share(
                        &facts.top_level_defs,
                        inputs.preamble_sourced_symbols.get(path),
                    );
                    test_helper_symbols.insert(path.clone(), defs);
                    // Same for top-level `library()`/`require()` attaches — a
                    // helper/setup file's attaches propagate to sibling tests
                    // (testthat sources the preamble before each test),
                    // including attaches made by the preamble's sourced
                    // closure (issue #638). Skip the entry entirely when there
                    // are no attaches so the map stays sparse (most preamble
                    // files attach nothing).
                    let attached = union_or_share(
                        &facts.attached_packages,
                        inputs.preamble_sourced_attached_packages.get(path),
                    );
                    if !attached.is_empty() {
                        test_helper_attached_packages.insert(path.clone(), attached);
                    }
                }
            }
        }
    }
    let (imported_symbols, mut full_imports) = match namespace_model {
        Some(m) => {
            let mut imp: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (pkg, sym) in &m.imports {
                imp.entry(sym.clone()).or_default().insert(pkg.clone());
            }
            let full: BTreeSet<String> = m.full_imports.iter().cloned().collect();
            (imp, full)
        }
        None => (BTreeMap::new(), BTreeSet::new()),
    };
    // Issue #531: `DESCRIPTION` `Depends:` packages are *attached* when the
    // package is loaded (R puts their exports on the search path), so their
    // exports resolve unqualified inside the package's own code — exactly like
    // a NAMESPACE `import(pkg)`. Union them into `full_imports` (runs after the
    // match, so it applies whether or not a NAMESPACE exists; the `BTreeSet`
    // dedupes a package that is both imported and depended-on).
    //
    // `Imports:` is deliberately excluded: it is loaded but not attached, so it
    // stays `::`/`importFrom`-only (matches R). Version constraints and the
    // special `R` entry are stripped by `parse_description_field_pub`. The
    // package's own name is filtered (`pkg != ws.name`): a valid DESCRIPTION
    // never self-depends, but a malformed one mid-edit could, and self-name in
    // `full_imports` would query a possibly-stale installed package of the same
    // name.
    //
    // The same names are also collected into `depends_attached_packages` (a
    // subset of `full_imports`) so the NSE meta-package expansion can treat
    // `Depends:` as an attach WITHOUT meta-expanding NAMESPACE `import()` /
    // `@import` (which share `full_imports` but are not attaches). See the field
    // doc on `PackageScopeContribution::depends_attached_packages`.
    let mut depends_attached_packages: BTreeSet<String> = BTreeSet::new();
    if let Some(desc) = description {
        for pkg in crate::namespace_parser::parse_description_field_pub(&desc.text, "Depends") {
            if pkg != ws.name {
                depends_attached_packages.insert(pkg.clone());
                full_imports.insert(pkg);
            }
        }
    }
    let test_attached_packages = compute_test_attached_packages(description);
    PackageScopeContribution {
        workspace_root: Some(ws.root.clone()),
        package_name: Some(ws.name.clone()),
        r_internal_symbols: Arc::new(r_internal_symbols),
        imported_symbols: Arc::new(imported_symbols),
        full_imports: Arc::new(full_imports),
        depends_attached_packages: Arc::new(depends_attached_packages),
        test_attached_packages: Arc::new(test_attached_packages),
        test_helper_symbols: Arc::new(test_helper_symbols),
        test_helper_attached_packages: Arc::new(test_helper_attached_packages),
        dataset_symbols: Arc::new(dataset_names.clone()),
        sysdata_symbols: Arc::new(sysdata_names.clone()),
        onload_symbols: Arc::new(onload_all),
        rprofile_symbols: Arc::new(rprofile_symbols.clone()),
        rprofile_attached_packages: Arc::new(rprofile_attached_packages.clone()),
        rprofile_root,
    }
}

/// Return the cached immutable set when adding `harvested` cannot change it.
fn union_or_share(
    cached: &Arc<BTreeSet<String>>,
    harvested: Option<&BTreeSet<String>>,
) -> Arc<BTreeSet<String>> {
    match harvested {
        None => Arc::clone(cached),
        Some(harvested) if harvested.is_empty() || harvested.is_subset(cached) => {
            Arc::clone(cached)
        }
        Some(harvested) => Arc::new(cached.union(harvested).cloned().collect()),
    }
}

/// Packages that should be implicitly attached when resolving scope under
/// `tests/testthat/`. Currently this is the singleton `{"testthat"}` whenever
/// the package's `DESCRIPTION` declares testthat in `Suggests:`, `Imports:`,
/// or `Depends:` — matching `R CMD check`'s "declared dependency" gate.
/// Returns an empty set otherwise.
///
/// The set is intentionally narrow: implicitly attaching every `Suggests:`
/// dependency in tests would over-approximate and mute legitimate undefined-
/// variable diagnostics. Other test-only packages should be brought in with
/// an explicit `library()` call in a `helper-*.R` file or the test file
/// itself.
fn compute_test_attached_packages(description: Option<&DescriptionInput>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(desc) = description else {
        return out;
    };
    if description_declares_dependency(&desc.text, "testthat") {
        out.insert("testthat".to_string());
    }
    out
}

/// Returns `true` if `pkg` appears in DESCRIPTION's `Suggests:`, `Imports:`,
/// or `Depends:` field. Stripping of version constraints and the special
/// `R` entry is handled by `parse_description_field_pub`.
fn description_declares_dependency(description_text: &str, pkg: &str) -> bool {
    for field in ["Suggests", "Imports", "Depends"] {
        let listed = crate::namespace_parser::parse_description_field_pub(description_text, field);
        if listed.iter().any(|p| p == pkg) {
            return true;
        }
    }
    false
}

fn merge_namespace_model(
    namespace: Option<&NamespaceInput>,
    r_file_facts: &BTreeMap<PathBuf, RFileFacts>,
) -> PackageNamespaceModel {
    let mut model = match namespace {
        Some(ns) => crate::package_namespace::namespace_model_from_content(&ns.text),
        None => PackageNamespaceModel::default(),
    };
    // Union roxygen-derived imports/exports from Source files only.
    // Test files (tests/testthat/*) must never contribute @export/@importFrom
    // tags to the package-wide namespace model — matching the `RFileKind::Source`
    // filter used by `build_scope_contribution`.

    // Dedup against a HashSet rather than scanning the accumulating Vec —
    // previously each insert did a linear scan of the growing `imports`
    // Vec (O(files × imports²) overall). We seed each set with the entries
    // already parsed out of NAMESPACE so roxygen tags that duplicate
    // NAMESPACE entries aren't re-appended.
    let mut seen_imports: std::collections::HashSet<(String, String)> =
        model.imports.iter().cloned().collect();
    let mut seen_full: std::collections::HashSet<String> =
        model.full_imports.iter().cloned().collect();

    for facts in r_file_facts.values() {
        if facts.kind != RFileKind::Source {
            continue;
        }
        for sym in &facts.roxygen_namespace.exports {
            // `exports` is a HashSet, so `insert` is idempotent and O(1)
            // — no need for a prior `contains` check.
            model.exports.insert(sym.clone());
        }
        // RoxygenNamespace::import_from → PackageNamespaceModel::imports (Vec<(pkg, sym)>)
        for (pkg, sym) in &facts.roxygen_namespace.import_from {
            if seen_imports.insert((pkg.clone(), sym.clone())) {
                model.imports.push((pkg.clone(), sym.clone()));
            }
        }
        // RoxygenNamespace::imports (full package imports) → PackageNamespaceModel::full_imports
        for pkg in &facts.roxygen_namespace.imports {
            if seen_full.insert(pkg.clone()) {
                model.full_imports.push(pkg.clone());
            }
        }
    }
    model
}

fn derive_r_file_facts(
    prev: &BTreeMap<PathBuf, RFileFacts>,
    inputs: &BTreeMap<PathBuf, RFileInput>,
) -> BTreeMap<PathBuf, RFileFacts> {
    let mut out = BTreeMap::new();
    for (path, file) in inputs {
        // Reuse only when both digest AND kind match. `kind` is derived
        // from path shape, so in practice it never flips for a given path,
        // but we still compare it so cached facts can never drift from
        // the classification carried in `RFileInput`.
        let reuse = prev.get(path).filter(|cached| {
            cached.content_digest == file.content_digest && cached.kind == file.kind
        });
        let facts = match reuse {
            Some(cached) => cached.clone(),
            None => RFileFacts {
                kind: file.kind,
                roxygen_namespace: crate::roxygen::extract_roxygen_namespace_tags(&file.text),
                top_level_defs: Arc::new(crate::roxygen::extract_top_level_defs(&file.text)),
                onload_bindings: Arc::new(crate::package_state::sysdata::extract_onload_bindings(
                    &file.text,
                )),
                // Only Test-kind files can contribute testthat-preamble attaches;
                // skip the extra parse for Source files (their `library()` calls
                // are handled by the standard position-aware scope path).
                attached_packages: match file.kind {
                    RFileKind::Test => Arc::new(
                        crate::cross_file::source_detect::extract_attached_packages(&file.text),
                    ),
                    RFileKind::Source => Arc::new(BTreeSet::new()),
                },
                content_digest: file.content_digest,
            },
        };
        out.insert(path.clone(), facts);
    }
    out
}

fn effective_workspace(inputs: &PackageInputs) -> Option<PackageWorkspace> {
    let root = inputs.workspace_root.as_ref()?;
    let parsed_name = inputs
        .description
        .as_ref()
        .and_then(|d| parse_dcf_field_pub(&d.text, "Package"))
        .filter(|s| !s.is_empty());
    // `Auto` activates on a single authoritative signal: a workspace-root
    // DESCRIPTION with a non-empty `Package:` field — exactly R's own definition
    // of a package. NAMESPACE/roxygen/`R/` presence is deliberately NOT an
    // activation heuristic (a NAMESPACE without a DESCRIPTION is not an
    // installable package). Non-standard layouts opt in via `Enabled`.
    //
    // This is deliberate, not an oversight: do NOT re-add a NAMESPACE-based
    // `Auto` branch (it was tried, drifted from the design, and was reverted).
    // See docs/superpowers/specs/2026-05-10-r-package-mode-architecture-design.md
    // §6.1 (the 2026-06-17 decision note) for the rationale and history.
    match (inputs.package_mode, parsed_name.as_deref()) {
        (PackageMode::Disabled, _) => None,
        (PackageMode::Auto, Some(name)) => Some(PackageWorkspace {
            name: name.to_string(),
            root: root.clone(),
        }),
        (PackageMode::Auto, None) => None,
        (PackageMode::Enabled, Some(name)) => Some(PackageWorkspace {
            name: name.to_string(),
            root: root.clone(),
        }),
        (PackageMode::Enabled, None) => Some(PackageWorkspace {
            name: "unknown".to_string(),
            root: root.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_inputs(mode: PackageMode) -> PackageInputs {
        PackageInputs {
            workspace_root: Some("/work/pkg".into()),
            package_mode: mode,
            description: None,
            namespace: None,
            r_files: BTreeMap::new(),
            dataset_names: BTreeSet::new(),
            sysdata_names: BTreeSet::new(),
            model_rprofile: false,
            rprofile_symbols: BTreeSet::new(),
            rprofile_attached_packages: BTreeSet::new(),
            rprofile_sourced_files: BTreeSet::new(),
            preamble_sourced_symbols: BTreeMap::new(),
            preamble_sourced_attached_packages: BTreeMap::new(),
            preamble_sourced_files: BTreeSet::new(),
            preamble_sourced_files_by_preamble: BTreeMap::new(),
        }
    }

    fn with_description(mode: PackageMode, text: &str) -> PackageInputs {
        let mut i = empty_inputs(mode);
        i.description = Some(DescriptionInput { text: text.into() });
        i
    }

    #[test]
    fn union_or_share_reuses_cached_set_without_added_values() {
        let cached = Arc::new(BTreeSet::from(["cached".to_string()]));
        let empty = BTreeSet::new();

        assert!(Arc::ptr_eq(&union_or_share(&cached, None), &cached));
        assert!(Arc::ptr_eq(&union_or_share(&cached, Some(&empty)), &cached));
    }

    #[test]
    fn union_or_share_allocates_complete_union_for_new_values() {
        let cached = Arc::new(BTreeSet::from(["cached".to_string()]));
        let harvested = BTreeSet::from(["added".to_string()]);
        let merged = union_or_share(&cached, Some(&harvested));

        assert!(!Arc::ptr_eq(&merged, &cached));
        assert_eq!(
            merged.as_ref(),
            &BTreeSet::from(["added".to_string(), "cached".to_string()])
        );
    }

    #[test]
    fn contribution_reuses_cached_helper_defs_for_subset_closure() {
        let helper_path: PathBuf = "/work/pkg/tests/testthat/helper-shared.R".into();
        let helper_text: Arc<str> = "shared <- 1\n".into();
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            helper_path.clone(),
            RFileInput {
                kind: RFileKind::Test,
                text: helper_text.clone(),
                content_digest: ContentDigest::of(&helper_text),
            },
        );
        inputs
            .preamble_sourced_symbols
            .insert(helper_path.clone(), BTreeSet::from(["shared".to_string()]));

        let state = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let cached = &state.r_file_facts[&helper_path].top_level_defs;
        let contributed = &state.scope_contribution.test_helper_symbols[&helper_path];

        assert!(Arc::ptr_eq(contributed, cached));
        assert_eq!(
            contributed.as_ref(),
            &BTreeSet::from(["shared".to_string()])
        );
    }

    #[test]
    fn contribution_reuses_cached_helper_attaches_for_subset_closure() {
        let helper_path: PathBuf = "/work/pkg/tests/testthat/helper-shared.R".into();
        let helper_text: Arc<str> = "library(tidyr)\n".into();
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            helper_path.clone(),
            RFileInput {
                kind: RFileKind::Test,
                text: helper_text.clone(),
                content_digest: ContentDigest::of(&helper_text),
            },
        );
        inputs
            .preamble_sourced_attached_packages
            .insert(helper_path.clone(), BTreeSet::from(["tidyr".to_string()]));

        let state = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let cached = &state.r_file_facts[&helper_path].attached_packages;
        let contributed = &state.scope_contribution.test_helper_attached_packages[&helper_path];

        assert!(Arc::ptr_eq(contributed, cached));
        assert_eq!(contributed.as_ref(), &BTreeSet::from(["tidyr".to_string()]));
    }

    #[test]
    fn disabled_mode_yields_no_workspace() {
        let s = derive_package_state(
            &PackageState::default(),
            &with_description(PackageMode::Disabled, "Package: foo\n"),
            &PackageInputDelta::Initial,
        );
        assert!(s.workspace.is_none());
    }

    #[test]
    fn auto_with_no_description_yields_no_workspace() {
        let s = derive_package_state(
            &PackageState::default(),
            &empty_inputs(PackageMode::Auto),
            &PackageInputDelta::Initial,
        );
        assert!(s.workspace.is_none());
    }
    /// Single-signal activation: NAMESPACE + R sources without a DESCRIPTION
    /// `Package:` field does NOT activate package mode in `Auto`. A NAMESPACE
    /// is not an activation heuristic; non-package workspaces stay in script
    /// mode regardless of NAMESPACE presence (see `docs/r-package-dev.md`).
    /// Use `Enabled` to force package mode on such layouts.
    #[test]
    fn auto_with_namespace_and_source_no_description_yields_no_workspace() {
        let mut inputs = empty_inputs(PackageMode::Auto);
        inputs.namespace = Some(NamespaceInput {
            text: "export(foo)\n".into(),
        });
        let text: Arc<str> = "foo <- function() 1\n".into();
        inputs.r_files.insert(
            "/work/pkg/R/foo.R".into(),
            RFileInput {
                kind: RFileKind::Source,
                content_digest: ContentDigest::of(&text),
                text,
            },
        );

        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        assert!(
            s.workspace.is_none(),
            "Auto mode with NAMESPACE + sources but no DESCRIPTION Package: must not activate"
        );
        assert!(s.scope_contribution.workspace_root.is_none());
        assert!(s.scope_contribution.r_internal_symbols.is_empty());
    }

    #[test]
    fn auto_with_description_yields_workspace() {
        let s = derive_package_state(
            &PackageState::default(),
            &with_description(PackageMode::Auto, "Package: foo\n"),
            &PackageInputDelta::Initial,
        );
        assert_eq!(s.workspace.as_ref().unwrap().name, "foo");
    }

    #[test]
    fn enabled_with_no_description_synthesizes_unknown() {
        let s = derive_package_state(
            &PackageState::default(),
            &empty_inputs(PackageMode::Enabled),
            &PackageInputDelta::Initial,
        );
        assert_eq!(s.workspace.as_ref().unwrap().name, "unknown");
    }

    #[test]
    fn enabled_with_invalid_description_synthesizes_unknown() {
        let s = derive_package_state(
            &PackageState::default(),
            &with_description(PackageMode::Enabled, "no Package field here\n"),
            &PackageInputDelta::Initial,
        );
        assert_eq!(s.workspace.as_ref().unwrap().name, "unknown");
    }

    /// Issue #531: a package in `DESCRIPTION` `Depends:` is attached at load
    /// time, so its exports resolve unqualified in the package's own code —
    /// equivalent to a NAMESPACE `import(pkg)`. It must land in `full_imports`
    /// even with no NAMESPACE present (the issue's repro has no NAMESPACE).
    #[test]
    fn depends_packages_added_to_full_imports() {
        let s = derive_package_state(
            &PackageState::default(),
            &with_description(PackageMode::Auto, "Package: tp\nDepends: ggplot2\n"),
            &PackageInputDelta::Initial,
        );
        assert!(
            s.scope_contribution.full_imports.contains("ggplot2"),
            "Depends: ggplot2 should contribute to full_imports"
        );
    }

    /// `Imports:` keeps the stricter R semantics (loaded, not attached → needs
    /// `::`/`importFrom`), so it must NOT contribute to the unqualified set.
    #[test]
    fn imports_not_added_to_full_imports() {
        let s = derive_package_state(
            &PackageState::default(),
            &with_description(PackageMode::Auto, "Package: tp\nImports: dplyr\n"),
            &PackageInputDelta::Initial,
        );
        assert!(
            !s.scope_contribution.full_imports.contains("dplyr"),
            "Imports: dplyr must not contribute to full_imports"
        );
    }

    /// `Depends:` unions with NAMESPACE `import(pkg)` — both populate the same
    /// unqualified-export set.
    #[test]
    fn depends_unions_with_namespace_full_imports() {
        let mut inputs = with_description(PackageMode::Auto, "Package: tp\nDepends: ggplot2\n");
        inputs.namespace = Some(NamespaceInput {
            text: "import(rlang)\n".into(),
        });
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let full = &s.scope_contribution.full_imports;
        assert!(full.contains("ggplot2"), "Depends entry missing");
        assert!(full.contains("rlang"), "NAMESPACE import entry missing");
    }

    /// Version constraints are stripped and the special `R` entry is dropped.
    #[test]
    fn depends_strips_version_and_drops_r() {
        let s = derive_package_state(
            &PackageState::default(),
            &with_description(
                PackageMode::Auto,
                "Package: tp\nDepends: R (>= 3.5), ggplot2 (>= 3.0)\n",
            ),
            &PackageInputDelta::Initial,
        );
        let full = &s.scope_contribution.full_imports;
        assert!(full.contains("ggplot2"), "ggplot2 should be present");
        assert!(
            !full.contains("R"),
            "R version requirement is not a package"
        );
        assert!(
            !full.iter().any(|p| p.contains('(')),
            "version constraints must be stripped"
        );
    }

    /// A package both depended-on and NAMESPACE-imported collapses to one entry
    /// (the `BTreeSet` dedupes).
    #[test]
    fn depends_dedupes_with_namespace() {
        let mut inputs = with_description(PackageMode::Auto, "Package: tp\nDepends: rlang\n");
        inputs.namespace = Some(NamespaceInput {
            text: "import(rlang)\n".into(),
        });
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let count = s
            .scope_contribution
            .full_imports
            .iter()
            .filter(|p| p.as_str() == "rlang")
            .count();
        assert_eq!(count, 1, "rlang should appear exactly once");
    }

    /// `Depends:` packages populate `depends_attached_packages` (the
    /// meta-expansion subset), while NAMESPACE `import(pkg)` does NOT — only
    /// `Depends:` is a true attach. Both still land in `full_imports`.
    #[test]
    fn depends_populates_attached_subset_but_namespace_import_does_not() {
        let mut inputs = with_description(PackageMode::Auto, "Package: tp\nDepends: tidyverse\n");
        inputs.namespace = Some(NamespaceInput {
            text: "import(rlang)\n".into(),
        });
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let attached = &s.scope_contribution.depends_attached_packages;
        assert!(
            attached.contains("tidyverse"),
            "Depends entry must be attached"
        );
        assert!(
            !attached.contains("rlang"),
            "NAMESPACE import(rlang) must NOT be in depends_attached_packages"
        );
        // Both still resolve unqualified via full_imports.
        let full = &s.scope_contribution.full_imports;
        assert!(full.contains("tidyverse") && full.contains("rlang"));
    }

    /// The self-name filter applies to `depends_attached_packages` too.
    #[test]
    fn depends_attached_excludes_own_package_name() {
        let s = derive_package_state(
            &PackageState::default(),
            &with_description(PackageMode::Auto, "Package: tp\nDepends: tp, ggplot2\n"),
            &PackageInputDelta::Initial,
        );
        let attached = &s.scope_contribution.depends_attached_packages;
        assert!(attached.contains("ggplot2"));
        assert!(!attached.contains("tp"), "own name must be excluded");
    }

    /// A malformed self-dependency (`Depends:` listing the package's own name)
    /// is filtered out so we never query a stale installed package of the same
    /// name.
    #[test]
    fn depends_filters_own_package_name() {
        let s = derive_package_state(
            &PackageState::default(),
            &with_description(PackageMode::Auto, "Package: tp\nDepends: tp, ggplot2\n"),
            &PackageInputDelta::Initial,
        );
        let full = &s.scope_contribution.full_imports;
        assert!(full.contains("ggplot2"), "ggplot2 should be present");
        assert!(
            !full.contains("tp"),
            "own package name must be filtered from full_imports"
        );
    }

    #[test]
    fn memoization_reuses_cached_facts_on_unchanged_content() {
        let path: PathBuf = "/work/pkg/R/foo.R".into();
        let text: Arc<str> = "foo <- function() 1\n".into();
        let digest = ContentDigest::of(&text);
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            path.clone(),
            RFileInput {
                kind: RFileKind::Source,
                text: text.clone(),
                content_digest: digest,
            },
        );
        let s1 = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let s2 = derive_package_state(&s1, &inputs, &PackageInputDelta::Initial);
        let f1 = s1.r_file_facts.get(&path).unwrap();
        let f2 = s2.r_file_facts.get(&path).unwrap();
        assert!(std::sync::Arc::ptr_eq(
            &f1.top_level_defs,
            &f2.top_level_defs
        ));
    }

    #[test]
    fn memoization_recomputes_on_content_change() {
        let path: PathBuf = "/work/pkg/R/foo.R".into();
        let text1: Arc<str> = "foo <- function() 1\n".into();
        let text2: Arc<str> = "foo <- function() 2\n".into();
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            path.clone(),
            RFileInput {
                kind: RFileKind::Source,
                text: text1.clone(),
                content_digest: ContentDigest::of(&text1),
            },
        );
        let s1 = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let entry = inputs.r_files.get_mut(&path).unwrap();
        entry.text = text2.clone();
        entry.content_digest = ContentDigest::of(&text2);
        let s2 = derive_package_state(&s1, &inputs, &PackageInputDelta::Initial);
        let f1 = s1.r_file_facts.get(&path).unwrap();
        let f2 = s2.r_file_facts.get(&path).unwrap();
        assert!(!std::sync::Arc::ptr_eq(
            &f1.top_level_defs,
            &f2.top_level_defs
        ));
    }

    #[test]
    fn merge_unions_namespace_and_roxygen() {
        let path: PathBuf = "/work/pkg/R/foo.R".into();
        let text: Arc<str> = "#' @importFrom dplyr filter\nfoo <- function() 1\n".into();
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.namespace = Some(NamespaceInput {
            text: "importFrom(dplyr, mutate)\nimport(magrittr)\n".into(),
        });
        inputs.r_files.insert(
            path,
            RFileInput {
                kind: RFileKind::Source,
                text: text.clone(),
                content_digest: ContentDigest::of(&text),
            },
        );
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let m = s.namespace_model.unwrap();
        assert!(
            m.imports.iter().any(|(p, n)| p == "dplyr" && n == "filter"),
            "missing roxygen filter: {:?}",
            m
        );
        assert!(
            m.imports.iter().any(|(p, n)| p == "dplyr" && n == "mutate"),
            "missing NAMESPACE mutate: {:?}",
            m
        );
        assert!(
            m.full_imports.iter().any(|p| p == "magrittr"),
            "missing magrittr: {:?}",
            m
        );
    }

    #[test]
    fn scope_contribution_includes_internal_symbols_from_r_dir() {
        let path: PathBuf = "/work/pkg/R/utils.R".into();
        let text: Arc<str> = "helper <- function() 1\n".into();
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            path,
            RFileInput {
                kind: RFileKind::Source,
                text: text.clone(),
                content_digest: ContentDigest::of(&text),
            },
        );
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        assert!(s.scope_contribution.r_internal_symbols.contains("helper"));
    }

    #[test]
    fn scope_contribution_excludes_test_file_symbols() {
        let test_path: PathBuf = "/work/pkg/tests/testthat/test-utils.R".into();
        let text: Arc<str> = "test_helper <- function() 1\n".into();
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            test_path,
            RFileInput {
                kind: RFileKind::Test,
                text: text.clone(),
                content_digest: ContentDigest::of(&text),
            },
        );
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        assert!(
            !s.scope_contribution
                .r_internal_symbols
                .contains("test_helper")
        );
    }

    #[test]
    fn namespace_model_excludes_roxygen_from_test_files() {
        // A test file carrying roxygen tags must NOT contribute to the
        // package-wide namespace model — mirrors the path filter in
        // `build_scope_contribution`.
        let test_path: PathBuf = "/work/pkg/tests/testthat/test-utils.R".into();
        let text: Arc<str> = concat!(
            "#' @export\n",
            "#' @importFrom dplyr filter\n",
            "#' @import ggplot2\n",
            "leaked <- function() 1\n",
        )
        .into();
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            test_path,
            RFileInput {
                kind: RFileKind::Test,
                text: text.clone(),
                content_digest: ContentDigest::of(&text),
            },
        );
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let m = s.namespace_model.as_ref().unwrap();
        assert!(
            !m.exports.contains("leaked"),
            "test-file @export leaked: {:?}",
            m.exports
        );
        assert!(
            !m.imports.iter().any(|(p, s)| p == "dplyr" && s == "filter"),
            "test-file @importFrom leaked: {:?}",
            m.imports
        );
        assert!(
            !m.full_imports.iter().any(|p| p == "ggplot2"),
            "test-file @import leaked: {:?}",
            m.full_imports
        );
    }

    #[test]
    fn scope_contribution_carries_imports_from_namespace() {
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.namespace = Some(NamespaceInput {
            text: "importFrom(dplyr, filter)\nimportFrom(stats, filter)\n".into(),
        });
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let pkgs = s.scope_contribution.imported_symbols.get("filter").unwrap();
        assert!(pkgs.contains("dplyr"));
        assert!(pkgs.contains("stats"));
    }

    /// Issue #431: the scope contribution must carry the package's own name
    /// (from DESCRIPTION `Package:`) so the diagnostics collector can consult
    /// the package's own NSE policies for its own files. This name feeds the
    /// policy lookup only — it is NOT added to the NSE in-play package set used
    /// for standard-eval export resolution.
    #[test]
    fn scope_contribution_carries_package_name() {
        let inputs = with_description(PackageMode::Auto, "Package: dplyr\n");
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        assert_eq!(
            s.scope_contribution.package_name.as_deref(),
            Some("dplyr"),
            "scope_contribution must carry the DESCRIPTION Package name"
        );
    }

    #[test]
    fn test_files_get_rfile_facts_but_dont_contribute_internal_symbols() {
        let test_path: std::path::PathBuf = "/work/pkg/tests/testthat/test-utils.R".into();
        let text: Arc<str> = "test_helper <- function() 99\n".into();
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            test_path.clone(),
            RFileInput {
                kind: RFileKind::Test,
                text: text.clone(),
                content_digest: ContentDigest::of(&text),
            },
        );
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        // The test file gets per-file facts (its top_level_defs ARE extracted).
        assert!(s.r_file_facts.contains_key(&test_path));
        let facts = s.r_file_facts.get(&test_path).unwrap();
        assert!(
            facts.top_level_defs.contains("test_helper"),
            "expected test_helper to be in top_level_defs of the test file, got: {:?}",
            facts.top_level_defs
        );

        // But it does NOT contribute to r_internal_symbols — test-file symbols
        // are invisible to R/ files (one-way visibility: tests→R/, not R/→tests).
        assert!(
            !s.scope_contribution
                .r_internal_symbols
                .contains("test_helper"),
            "test-file symbol should not appear in r_internal_symbols: {:?}",
            s.scope_contribution.r_internal_symbols
        );
    }

    #[test]
    fn r_dir_files_contribute_internal_symbols_visible_to_test_files() {
        // R/utils.R defines helper() — contributes to r_internal_symbols.
        let r_path: std::path::PathBuf = "/work/pkg/R/utils.R".into();
        let r_text: Arc<str> = "helper <- function() 1\n".into();
        // tests/testthat/test-utils.R calls it (scope injection verified in scope.rs tests).
        let test_path: std::path::PathBuf = "/work/pkg/tests/testthat/test-utils.R".into();
        let test_text: Arc<str> = "result <- helper()\n".into();

        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            r_path,
            RFileInput {
                kind: RFileKind::Source,
                text: r_text.clone(),
                content_digest: ContentDigest::of(&r_text),
            },
        );
        inputs.r_files.insert(
            test_path,
            RFileInput {
                kind: RFileKind::Test,
                text: test_text.clone(),
                content_digest: ContentDigest::of(&test_text),
            },
        );
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        // helper appears in r_internal_symbols (contributed by R/utils.R).
        assert!(
            s.scope_contribution.r_internal_symbols.contains("helper"),
            "R/ symbol must appear in r_internal_symbols: {:?}",
            s.scope_contribution.r_internal_symbols
        );
        // test_helper is absent — the test file's top-level defs never pollute
        // r_internal_symbols (confirmed by test_files_get_rfile_facts_but_dont_contribute_internal_symbols).
    }

    /// A workspace with a NAMESPACE but no DESCRIPTION `Package:` field does NOT
    /// activate package mode in `Auto` — with or without R source files (see
    /// `auto_with_namespace_and_source_no_description_yields_no_workspace`).
    #[test]
    fn namespace_alone_without_sources_does_not_produce_scope_contribution() {
        // Auto mode with NAMESPACE but no DESCRIPTION and no R source files.
        let mut inputs = empty_inputs(PackageMode::Auto);
        inputs.namespace = Some(NamespaceInput {
            text: "importFrom(dplyr, filter)\nimport(ggplot2)\n".into(),
        });

        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        // No source files → no package mode → empty scope contribution.
        assert!(
            s.workspace.is_none(),
            "Auto mode with NAMESPACE but no source files must not produce a workspace"
        );
        assert!(s.scope_contribution.workspace_root.is_none());
        assert!(s.scope_contribution.imported_symbols.is_empty());
        assert!(s.scope_contribution.full_imports.is_empty());
    }

    // ------------------------------------------------------------------
    // merge_namespace_model correctness & perf regression
    // ------------------------------------------------------------------

    /// Roxygen tags duplicated across R files must dedupe in the merged
    /// namespace model, and the NAMESPACE-seeded entries must also be
    /// deduped against (no double-counting).
    #[test]
    fn merge_namespace_model_dedupes_duplicate_import_from_and_full_imports() {
        use std::path::PathBuf;

        let mut inputs = with_description(PackageMode::Auto, "Package: pkg\n");
        inputs.namespace = Some(NamespaceInput {
            text: "importFrom(dplyr, filter)\nimport(ggplot2)\n".into(),
        });
        // Three R files each repeating the same @importFrom and @import
        // tags that NAMESPACE already provides.
        for n in 0..3 {
            let path: PathBuf = format!("/work/pkg/R/file{}.R", n).into();
            let text: Arc<str> = "\
#' @importFrom dplyr filter
#' @import ggplot2
foo <- function() 1
"
            .to_string()
            .into();
            inputs.r_files.insert(
                path,
                RFileInput {
                    kind: RFileKind::Source,
                    text: text.clone(),
                    content_digest: ContentDigest::of(&text),
                },
            );
        }

        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let ns = s.namespace_model.as_ref().expect("namespace model built");

        let filter_count = ns
            .imports
            .iter()
            .filter(|(p, sym)| p == "dplyr" && sym == "filter")
            .count();
        assert_eq!(
            filter_count, 1,
            "(dplyr, filter) must appear exactly once after dedupe; got: {:?}",
            ns.imports,
        );

        let ggplot_count = ns
            .full_imports
            .iter()
            .filter(|p| p.as_str() == "ggplot2")
            .count();
        assert_eq!(
            ggplot_count, 1,
            "ggplot2 must appear exactly once in full_imports after dedupe; got: {:?}",
            ns.full_imports,
        );
    }

    /// Perf regression: `merge_namespace_model` must not be quadratic in
    /// the number of `@importFrom` entries contributed across R files.
    ///
    /// Before the HashSet dedup fix, a 500-file package with ~5 imports
    /// each (2500 total) would linear-scan the growing Vec on every
    /// insertion: ~2500 × ~1250 average Vec length ≈ 3M pair comparisons
    /// per derive — observed at multiple ms under the write lock.
    /// 5 seconds is an extremely generous ceiling chosen to be robust on
    /// the slowest CI hardware; a quadratic regression would blow past it.
    #[test]
    fn merge_namespace_model_is_linear_in_total_imports() {
        use std::path::PathBuf;

        let n_files = 500usize;
        let imports_per_file = 10usize;

        let mut inputs = with_description(PackageMode::Auto, "Package: pkg\n");
        for f in 0..n_files {
            // Each file gets a unique set of `@importFrom` pairs (no dedupe
            // shortcut across files) plus a few shared pairs (exercise the
            // dedupe path on the already-seen set).
            let mut body = String::new();
            for i in 0..imports_per_file {
                body.push_str(&format!("#' @importFrom pkg{}_of_{} sym{}\n", i % 20, f, i,));
            }
            body.push_str("foo <- function() 1\n");
            let path: PathBuf = format!("/work/pkg/R/file{}.R", f).into();
            let text: Arc<str> = body.into();
            inputs.r_files.insert(
                path,
                RFileInput {
                    kind: RFileKind::Source,
                    text: text.clone(),
                    content_digest: ContentDigest::of(&text),
                },
            );
        }

        let start = std::time::Instant::now();
        let _ = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "derive_package_state took {:?} for {} files × {} imports — \
             suspected quadratic regression in merge_namespace_model",
            elapsed,
            n_files,
            imports_per_file,
        );
    }

    // ------------------------------------------------------------------
    // testthat implicit-attachment and helper file visibility (#275)
    // ------------------------------------------------------------------

    /// Suggests-declared testthat should populate `test_attached_packages`,
    /// which the scope-injection layer uses to model `testthat::test_check`'s
    /// implicit `library(testthat)` in `tests/testthat/` files.
    #[test]
    fn scope_contribution_attaches_testthat_when_in_suggests() {
        let s = derive_package_state(
            &PackageState::default(),
            &with_description(PackageMode::Auto, "Package: foo\nSuggests: testthat\n"),
            &PackageInputDelta::Initial,
        );
        assert!(
            s.scope_contribution
                .test_attached_packages
                .contains("testthat"),
            "testthat in Suggests must populate test_attached_packages, got: {:?}",
            s.scope_contribution.test_attached_packages,
        );
    }

    /// Imports- and Depends-declared testthat also count: matches `R CMD
    /// check`'s declared-dependency semantics.
    #[test]
    fn scope_contribution_attaches_testthat_when_in_imports_or_depends() {
        let s_imp = derive_package_state(
            &PackageState::default(),
            &with_description(PackageMode::Auto, "Package: foo\nImports: testthat\n"),
            &PackageInputDelta::Initial,
        );
        assert!(
            s_imp
                .scope_contribution
                .test_attached_packages
                .contains("testthat")
        );

        let s_dep = derive_package_state(
            &PackageState::default(),
            &with_description(PackageMode::Auto, "Package: foo\nDepends: testthat\n"),
            &PackageInputDelta::Initial,
        );
        assert!(
            s_dep
                .scope_contribution
                .test_attached_packages
                .contains("testthat")
        );
    }

    /// Without testthat declared anywhere, do not implicitly attach it.
    /// Hard gate: an undeclared testthat would silently mute legitimate
    /// undefined-variable diagnostics for any name that happens to be a
    /// testthat export.
    #[test]
    fn scope_contribution_skips_testthat_when_undeclared() {
        let s = derive_package_state(
            &PackageState::default(),
            &with_description(PackageMode::Auto, "Package: foo\nSuggests: knitr\n"),
            &PackageInputDelta::Initial,
        );
        assert!(
            !s.scope_contribution
                .test_attached_packages
                .contains("testthat"),
            "testthat must NOT be attached when only `knitr` is suggested: {:?}",
            s.scope_contribution.test_attached_packages,
        );
    }

    /// Without package mode active (no DESCRIPTION → no workspace), the
    /// scope contribution is empty regardless of declared dependencies.
    #[test]
    fn scope_contribution_is_empty_outside_package_mode() {
        let mut inputs = empty_inputs(PackageMode::Auto);
        inputs.description = Some(DescriptionInput {
            text: "Suggests: testthat\n".into(), // No Package: field.
        });
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        assert!(s.workspace.is_none());
        assert!(s.scope_contribution.test_attached_packages.is_empty());
        assert!(s.scope_contribution.test_helper_symbols.is_empty());
    }

    /// `tests/testthat/helper-*.R` top-level defs go into `test_helper_symbols`
    /// (visible to peer tests) but NOT into `r_internal_symbols` (the one-way
    /// visibility into R/ stays asymmetric).
    #[test]
    fn scope_contribution_collects_helper_top_level_defs() {
        let helper_path: PathBuf = "/work/pkg/tests/testthat/helper-fixtures.R".into();
        let test_path: PathBuf = "/work/pkg/tests/testthat/test-foo.R".into();

        let helper_text: Arc<str> = "fixture <- function() 1\n".into();
        let test_text: Arc<str> = "test_local <- function() 2\n".into();

        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            helper_path,
            RFileInput {
                kind: RFileKind::Test,
                text: helper_text.clone(),
                content_digest: ContentDigest::of(&helper_text),
            },
        );
        inputs.r_files.insert(
            test_path,
            RFileInput {
                kind: RFileKind::Test,
                text: test_text.clone(),
                content_digest: ContentDigest::of(&test_text),
            },
        );

        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        // Helper defs go into test_helper_symbols.
        assert!(
            s.scope_contribution
                .test_helper_symbols
                .values()
                .any(|syms| syms.contains("fixture")),
            "helper top-level defs must populate test_helper_symbols: {:?}",
            s.scope_contribution.test_helper_symbols,
        );
        // Non-helper test files do NOT pollute test_helper_symbols.
        assert!(
            !s.scope_contribution
                .test_helper_symbols
                .values()
                .any(|syms| syms.contains("test_local")),
            "test-*.R defs must NOT appear in test_helper_symbols: {:?}",
            s.scope_contribution.test_helper_symbols,
        );
        // Neither leaks into r_internal_symbols (R/ files remain isolated
        // from tests/testthat/ contributions).
        assert!(!s.scope_contribution.r_internal_symbols.contains("fixture"));
        assert!(
            !s.scope_contribution
                .r_internal_symbols
                .contains("test_local")
        );
    }

    /// testthat sources `setup*.R` files (`^setup.*\.[rR]$`) before any test
    /// runs, so their top-level bindings are visible to test files exactly
    /// like helper defs. Real-world FP this guards against: googledrive's
    /// `tests/testthat/setup-testing.R` line 1 is `CLEAN <- SETUP <- FALSE`,
    /// referenced by 17 `test-*.R` files — without setup recognition Raven
    /// emitted 34 "CLEAN/SETUP is not defined" diagnostics.
    #[test]
    fn scope_contribution_collects_setup_top_level_defs() {
        let setup_path: PathBuf = "/work/pkg/tests/testthat/setup-testing.R".into();
        let setup_text: Arc<str> = "CLEAN <- SETUP <- FALSE\n".into();

        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            setup_path,
            RFileInput {
                kind: RFileKind::Test,
                text: setup_text.clone(),
                content_digest: ContentDigest::of(&setup_text),
            },
        );

        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        for sym in ["CLEAN", "SETUP"] {
            assert!(
                s.scope_contribution
                    .test_helper_symbols
                    .values()
                    .any(|syms| syms.contains(sym)),
                "setup-file top-level def `{sym}` must populate test_helper_symbols: {:?}",
                s.scope_contribution.test_helper_symbols,
            );
            // Setup defs never leak into R/ (one-way visibility stays
            // asymmetric, same as helpers).
            assert!(
                !s.scope_contribution.r_internal_symbols.contains(sym),
                "setup-file def `{sym}` must NOT enter r_internal_symbols",
            );
        }
    }

    /// Multiple helper files contribute their union — peer helpers see each
    /// other since `testthat::test_check` sources them in order before each
    /// test.
    #[test]
    fn scope_contribution_unions_helpers_across_files() {
        let helper_a: PathBuf = "/work/pkg/tests/testthat/helper-a.R".into();
        let helper_b: PathBuf = "/work/pkg/tests/testthat/helper-b.R".into();
        let text_a: Arc<str> = "helper_a_fn <- function() 1\n".into();
        let text_b: Arc<str> = "helper_b_fn <- function() 2\n".into();

        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            helper_a,
            RFileInput {
                kind: RFileKind::Test,
                text: text_a.clone(),
                content_digest: ContentDigest::of(&text_a),
            },
        );
        inputs.r_files.insert(
            helper_b,
            RFileInput {
                kind: RFileKind::Test,
                text: text_b.clone(),
                content_digest: ContentDigest::of(&text_b),
            },
        );
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        assert!(
            s.scope_contribution
                .test_helper_symbols
                .values()
                .any(|syms| syms.contains("helper_a_fn"))
        );
        assert!(
            s.scope_contribution
                .test_helper_symbols
                .values()
                .any(|syms| syms.contains("helper_b_fn"))
        );
    }

    /// Codex follow-up: `tests/testthat/sub/helper-x.R` must NOT be
    /// treated as a testthat helper — `testthat::source_test_helpers`
    /// uses non-recursive `dir(path, pattern = "^helper.*\\.[Rr]$")`, so
    /// subdirectory helpers are never auto-sourced. Including them in
    /// `test_helper_symbols` would silently mute legitimate
    /// undefined-variable diagnostics for any symbol they define.
    #[test]
    fn helpers_in_subdirectories_are_not_collected() {
        let nested: PathBuf = "/work/pkg/tests/testthat/sub/helper-deep.R".into();
        let direct: PathBuf = "/work/pkg/tests/testthat/helper-top.R".into();
        let nested_text: Arc<str> = "deep_fixture <- function() 1\n".into();
        let direct_text: Arc<str> = "top_fixture <- function() 2\n".into();

        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            nested,
            RFileInput {
                kind: RFileKind::Test,
                text: nested_text.clone(),
                content_digest: ContentDigest::of(&nested_text),
            },
        );
        inputs.r_files.insert(
            direct,
            RFileInput {
                kind: RFileKind::Test,
                text: direct_text.clone(),
                content_digest: ContentDigest::of(&direct_text),
            },
        );
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        assert!(
            s.scope_contribution
                .test_helper_symbols
                .values()
                .any(|syms| syms.contains("top_fixture")),
            "direct child helper-top.R must contribute its def",
        );
        assert!(
            !s.scope_contribution
                .test_helper_symbols
                .values()
                .any(|syms| syms.contains("deep_fixture")),
            "subdir helper-deep.R must NOT contribute (testthat does not recurse): {:?}",
            s.scope_contribution.test_helper_symbols,
        );
    }

    /// Issue #275 / Codex review: a helper file's own top-level defs must
    /// NOT be visible to the helper itself via the contribution. They're
    /// keyed by file path so the scope-injection layer can skip
    /// `helper_path == queried_path` entries — that preserves
    /// forward-reference diagnostics inside a helper.
    #[test]
    fn test_helper_symbols_keyed_by_helper_path() {
        let helper_a: PathBuf = "/work/pkg/tests/testthat/helper-a.R".into();
        let helper_b: PathBuf = "/work/pkg/tests/testthat/helper-b.R".into();
        let text_a: Arc<str> = "fixture_a <- function() 1\n".into();
        let text_b: Arc<str> = "fixture_b <- function() 2\n".into();

        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            helper_a.clone(),
            RFileInput {
                kind: RFileKind::Test,
                text: text_a.clone(),
                content_digest: ContentDigest::of(&text_a),
            },
        );
        inputs.r_files.insert(
            helper_b.clone(),
            RFileInput {
                kind: RFileKind::Test,
                text: text_b.clone(),
                content_digest: ContentDigest::of(&text_b),
            },
        );

        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        let map = &s.scope_contribution.test_helper_symbols;
        // Each helper keeps its own defs under its own path.
        let a_syms = map.get(&helper_a).expect("helper-a entry");
        assert!(a_syms.contains("fixture_a"));
        assert!(!a_syms.contains("fixture_b"));
        let b_syms = map.get(&helper_b).expect("helper-b entry");
        assert!(b_syms.contains("fixture_b"));
        assert!(!b_syms.contains("fixture_a"));
    }

    /// Issue #432: a testthat preamble file's top-level `library()` attach
    /// must populate `test_helper_attached_packages` (keyed by the preamble
    /// path) so sibling test files inherit the attach. `loadNamespace()` and
    /// attaches nested inside a function body must NOT count.
    #[test]
    fn scope_contribution_collects_helper_attached_packages() {
        let helper_path: PathBuf = "/work/pkg/tests/testthat/helper-lib.R".into();
        let helper_text: Arc<str> = concat!(
            "library(tidyr)\n",
            "require(dplyr)\n",
            "loadNamespace(\"data.table\")\n",
            "f <- function() library(stringr)\n",
        )
        .into();

        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            helper_path.clone(),
            RFileInput {
                kind: RFileKind::Test,
                text: helper_text.clone(),
                content_digest: ContentDigest::of(&helper_text),
            },
        );

        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        let attached = s
            .scope_contribution
            .test_helper_attached_packages
            .get(&helper_path)
            .expect("helper-lib.R must have an attached-packages entry");
        assert!(attached.contains("tidyr"), "got: {attached:?}");
        assert!(attached.contains("dplyr"), "got: {attached:?}");
        // loadNamespace does not attach.
        assert!(!attached.contains("data.table"), "got: {attached:?}");
        // Nested (function-body) library() does not attach at source time.
        assert!(!attached.contains("stringr"), "got: {attached:?}");
    }

    /// Setup files attach packages for sibling tests too (testthat sources
    /// `^setup.*\.[rR]$` before tests run).
    #[test]
    fn scope_contribution_collects_setup_attached_packages() {
        let setup_path: PathBuf = "/work/pkg/tests/testthat/setup-pkgs.R".into();
        let setup_text: Arc<str> = "library(tidyr)\n".into();

        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            setup_path.clone(),
            RFileInput {
                kind: RFileKind::Test,
                text: setup_text.clone(),
                content_digest: ContentDigest::of(&setup_text),
            },
        );

        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        assert!(
            s.scope_contribution
                .test_helper_attached_packages
                .get(&setup_path)
                .is_some_and(|p| p.contains("tidyr")),
            "setup file attach must be collected: {:?}",
            s.scope_contribution.test_helper_attached_packages,
        );
    }

    /// A `library()` attach in a NON-preamble test file (`test-*.R`) must NOT
    /// populate `test_helper_attached_packages` — only preamble files are
    /// sourced before sibling tests. (The test file's own attach is handled
    /// by the standard position-aware `library()` path.) Likewise, attaches in
    /// `R/*.R` never enter the contribution (one-way visibility).
    #[test]
    fn scope_contribution_excludes_non_preamble_and_source_attaches() {
        let test_path: PathBuf = "/work/pkg/tests/testthat/test-a.R".into();
        let r_path: PathBuf = "/work/pkg/R/f.R".into();
        let test_text: Arc<str> = "library(tidyr)\n".into();
        let r_text: Arc<str> = "library(dplyr)\nf <- function() 1\n".into();

        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            test_path,
            RFileInput {
                kind: RFileKind::Test,
                text: test_text.clone(),
                content_digest: ContentDigest::of(&test_text),
            },
        );
        inputs.r_files.insert(
            r_path,
            RFileInput {
                kind: RFileKind::Source,
                text: r_text.clone(),
                content_digest: ContentDigest::of(&r_text),
            },
        );

        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        assert!(
            s.scope_contribution
                .test_helper_attached_packages
                .is_empty(),
            "neither test-*.R nor R/*.R attaches should enter the contribution: {:?}",
            s.scope_contribution.test_helper_attached_packages,
        );
    }

    /// Subdirectory helpers are not auto-sourced by testthat, so their
    /// attaches must not be collected either (mirrors the def behavior).
    #[test]
    fn helper_attaches_in_subdirectories_are_not_collected() {
        let nested: PathBuf = "/work/pkg/tests/testthat/sub/helper-deep.R".into();
        let nested_text: Arc<str> = "library(tidyr)\n".into();

        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.r_files.insert(
            nested,
            RFileInput {
                kind: RFileKind::Test,
                text: nested_text.clone(),
                content_digest: ContentDigest::of(&nested_text),
            },
        );
        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        assert!(
            s.scope_contribution
                .test_helper_attached_packages
                .is_empty(),
            "subdir helper attaches must NOT be collected: {:?}",
            s.scope_contribution.test_helper_attached_packages,
        );
    }

    // ------------------------------------------------------------------
    // .Rprofile prelude carry-through (Task 6)
    // ------------------------------------------------------------------

    #[test]
    fn contribution_carries_rprofile_symbols_in_package_mode() {
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.rprofile_symbols.insert("r_bind".to_string());
        inputs
            .rprofile_attached_packages
            .insert("stringr".to_string());
        let s = derive_package_state(&PackageState::new(), &inputs, &PackageInputDelta::Initial);
        let c = s.scope_contribution();
        assert!(c.rprofile_symbols.contains("r_bind"));
        assert!(c.rprofile_attached_packages.contains("stringr"));
        assert_eq!(c.rprofile_root, inputs.workspace_root);
        // package mode active → workspace_root is Some
        assert!(c.workspace_root.is_some());
    }

    #[test]
    fn contribution_carries_rprofile_symbols_in_script_mode() {
        // No DESCRIPTION + Auto → no package workspace, but the prelude must
        // still be carried (script-mode R/ inclusion depends on this).
        let mut inputs = empty_inputs(PackageMode::Auto);
        inputs.workspace_root = Some(std::path::PathBuf::from("/work"));
        inputs.rprofile_symbols.insert("zz".to_string());
        let s = derive_package_state(&PackageState::new(), &inputs, &PackageInputDelta::Initial);
        let c = s.scope_contribution();
        assert!(
            c.rprofile_symbols.contains("zz"),
            "prelude must survive in script mode"
        );
        assert_eq!(
            c.rprofile_root.as_deref(),
            Some(std::path::Path::new("/work"))
        );
        // script mode → no package workspace
        assert!(c.workspace_root.is_none());
    }

    // ------------------------------------------------------------------
    // Dataset symbols from data/ directory (Workstream D)
    // ------------------------------------------------------------------

    #[test]
    fn scope_contribution_carries_dataset_names_from_inputs() {
        let mut inputs = with_description(PackageMode::Auto, "Package: foo\n");
        inputs.dataset_names.insert("mpg".to_string());
        inputs.dataset_names.insert("diamonds".to_string());

        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        assert!(
            s.scope_contribution.dataset_symbols.contains("mpg"),
            "dataset_symbols must contain mpg: {:?}",
            s.scope_contribution.dataset_symbols,
        );
        assert!(
            s.scope_contribution.dataset_symbols.contains("diamonds"),
            "dataset_symbols must contain diamonds: {:?}",
            s.scope_contribution.dataset_symbols,
        );
    }

    #[test]
    fn scope_contribution_empty_datasets_when_no_workspace() {
        let mut inputs = empty_inputs(PackageMode::Auto);
        inputs.dataset_names.insert("mpg".to_string());

        let s = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );

        // No workspace → no scope contribution → datasets not injected.
        assert!(s.scope_contribution.dataset_symbols.is_empty());
    }
}
