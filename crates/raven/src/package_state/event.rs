//! Translation from LSP events to `PackageInputDelta` + input mutations.
//!
//! Handlers call `translate(&mut inputs, event)` to update inputs and
//! receive a delta. The caller then invokes
//! `WorldState::apply_package_event(delta)` to recompute derived state.

use super::*;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub enum HandlerEvent {
    DidOpen {
        uri: tower_lsp::lsp_types::Url,
        text: Arc<str>,
    },
    DidChange {
        uri: tower_lsp::lsp_types::Url,
        text: Arc<str>,
    },
    DidClose {
        uri: tower_lsp::lsp_types::Url,
        on_disk_text: Option<Arc<str>>,
    },
    WatchedFileChanged {
        uri: tower_lsp::lsp_types::Url,
        on_disk_text: Option<Arc<str>>,
        deleted: bool,
    },
    SettingChanged {
        new_mode: crate::cross_file::config::PackageMode,
    },
}

/// Update package inputs for an LSP/package event and return the matching
/// derive delta.
///
/// `WatchedFileChanged` is the only variant that may perform filesystem I/O
/// (canonicalization, directory checks/scans, or fallback file reads). Open,
/// change, close, and setting events operate only on caller-supplied text or
/// mode data, so backend handlers may apply those variants while holding the
/// `WorldState` write lock without introducing blocking disk work.
pub fn translate(inputs: &mut PackageInputs, event: HandlerEvent) -> Option<PackageInputDelta> {
    let exclusions = crate::config_file::CompiledWorkspaceExclusions::default();
    translate_with_exclusions(inputs, event, &exclusions)
}

/// Like [`translate`], but applies `[workspace].exclude` to package-input
/// watcher rescans.
pub fn translate_with_exclusions(
    inputs: &mut PackageInputs,
    event: HandlerEvent,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> Option<PackageInputDelta> {
    // Events that can fire before a workspace root is known (or that don't
    // require one) are handled up front. Previously, the early
    // `let Some(root) = ... else { return None }` dropped these silently.
    if let HandlerEvent::SettingChanged { new_mode } = event {
        inputs.package_mode = new_mode;
        return Some(PackageInputDelta::SettingChanged);
    }

    let root = inputs.workspace_root.clone()?;
    match event {
        HandlerEvent::DidOpen { uri, text } | HandlerEvent::DidChange { uri, text } => {
            let path = uri.to_file_path().ok()?;
            if path == root.join("DESCRIPTION") {
                inputs.description = Some(DescriptionInput { text });
                return Some(PackageInputDelta::DescriptionChanged);
            }
            if path == root.join("NAMESPACE") {
                inputs.namespace = Some(NamespaceInput { text });
                return Some(PackageInputDelta::NamespaceChanged);
            }
            let kind = is_r_source_path(&path, &root)?;
            let digest = ContentDigest::of(&text);
            inputs.r_files.insert(
                path.clone(),
                RFileInput {
                    kind,
                    text,
                    content_digest: digest,
                },
            );
            Some(PackageInputDelta::RFileChanged { path, kind })
        }
        HandlerEvent::DidClose { uri, on_disk_text } => {
            let path = uri.to_file_path().ok()?;
            if path == root.join("DESCRIPTION") {
                inputs.description = on_disk_text.map(|text| DescriptionInput { text });
                return Some(PackageInputDelta::DescriptionChanged);
            }
            if path == root.join("NAMESPACE") {
                inputs.namespace = on_disk_text.map(|text| NamespaceInput { text });
                return Some(PackageInputDelta::NamespaceChanged);
            }
            let kind = is_r_source_path(&path, &root)?;
            match on_disk_text {
                Some(text) => {
                    let digest = ContentDigest::of(&text);
                    inputs.r_files.insert(
                        path.clone(),
                        RFileInput {
                            kind,
                            text,
                            content_digest: digest,
                        },
                    );
                    Some(PackageInputDelta::RFileChanged { path, kind })
                }
                None => {
                    inputs.r_files.remove(&path);
                    Some(PackageInputDelta::RFileDeleted { path, kind })
                }
            }
        }
        HandlerEvent::WatchedFileChanged {
            uri,
            on_disk_text,
            deleted,
        } => translate_watched(inputs, &root, uri, on_disk_text, deleted, exclusions),
        // SettingChanged handled above.
        HandlerEvent::SettingChanged { .. } => None,
    }
}

/// Apply a (possibly precomputed) `.Rprofile` scan to the prelude inputs.
///
/// Pure in-memory — does **no** disk I/O — so it is safe to call while holding
/// the `WorldState` write lock. This is the seam that lets the live-buffer and
/// startup/rebuild paths scan OFF-lock (it follows transitive `source()`) and
/// then apply only the prebuilt result here under the lock.
///
/// `scan` is `Some(..)` to install a fresh scan, or `None` to clear the prelude
/// (a deletion, or when modeling is disabled). When `inputs.model_rprofile` is
/// false the prelude is cleared regardless of `scan`. Returns the
/// `RProfileChanged` delta, or `None` only when modeling is disabled AND nothing
/// needed clearing (so callers don't fire a no-op re-derive).
pub(crate) fn apply_rprofile_scan(
    inputs: &mut PackageInputs,
    scan: Option<super::rprofile::RprofileScan>,
) -> Option<PackageInputDelta> {
    if !inputs.model_rprofile {
        let had = !inputs.rprofile_symbols.is_empty()
            || !inputs.rprofile_attached_packages.is_empty()
            || !inputs.rprofile_sourced_files.is_empty();
        inputs.rprofile_symbols.clear();
        inputs.rprofile_attached_packages.clear();
        inputs.rprofile_sourced_files.clear();
        return had.then_some(PackageInputDelta::RProfileChanged);
    }
    match scan {
        Some(scan) => {
            inputs.rprofile_symbols = scan.symbols;
            inputs.rprofile_attached_packages = scan.attached_packages;
            inputs.rprofile_sourced_files = scan.sourced_files;
        }
        None => {
            inputs.rprofile_symbols.clear();
            inputs.rprofile_attached_packages.clear();
            inputs.rprofile_sourced_files.clear();
        }
    }
    Some(PackageInputDelta::RProfileChanged)
}

/// Apply a (possibly precomputed) testthat preamble-source scan (issue #638).
///
/// Pure in-memory (no disk I/O), mirroring [`apply_rprofile_scan`]: `Some` to
/// install a fresh scan, `None` to clear. Gated on package mode — when
/// disabled, the inputs are cleared regardless. Returns
/// `PreambleSourcesChanged`, or `None` when disabled AND nothing needed
/// clearing.
pub(crate) fn apply_preamble_scan(
    inputs: &mut PackageInputs,
    scan: Option<super::preamble::PreambleScan>,
) -> Option<PackageInputDelta> {
    if inputs.package_mode == super::PackageMode::Disabled {
        let had = !inputs.preamble_sourced_symbols.is_empty()
            || !inputs.preamble_sourced_attached_packages.is_empty()
            || !inputs.preamble_sourced_files.is_empty()
            || !inputs.preamble_sourced_files_by_preamble.is_empty();
        inputs.preamble_sourced_symbols.clear();
        inputs.preamble_sourced_attached_packages.clear();
        inputs.preamble_sourced_files.clear();
        inputs.preamble_sourced_files_by_preamble.clear();
        return had.then_some(PackageInputDelta::PreambleSourcesChanged);
    }
    match scan {
        Some(scan) => {
            inputs.preamble_sourced_symbols = scan.symbols;
            inputs.preamble_sourced_attached_packages = scan.attached_packages;
            inputs.preamble_sourced_files = scan.sourced_files;
            inputs.preamble_sourced_files_by_preamble = scan.sourced_files_by_preamble;
        }
        None => {
            inputs.preamble_sourced_symbols.clear();
            inputs.preamble_sourced_attached_packages.clear();
            inputs.preamble_sourced_files.clear();
            inputs.preamble_sourced_files_by_preamble.clear();
        }
    }
    Some(PackageInputDelta::PreambleSourcesChanged)
}

/// `true` when a watched change to `path` must refresh the testthat
/// preamble-source scan (issue #638): the file IS a preamble scan root
/// (direct `tests/testthat/` child named `helper*`/`setup*` — its `source()`
/// set may have changed), or it is one of the files a preamble transitively
/// `source()`s (`preamble_sourced_files`, canonical when possible).
fn preamble_rescan_needed(
    inputs: &PackageInputs,
    root: &Path,
    path: &Path,
    canonical_path: &Path,
) -> bool {
    if inputs.package_mode == super::PackageMode::Disabled {
        return false;
    }
    if inputs.preamble_sourced_files.contains(canonical_path)
        || inputs.preamble_sourced_files.contains(path)
    {
        return true;
    }
    if super::preamble::is_testthat_preamble_path(path, root) {
        return true;
    }
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    super::preamble::is_testthat_preamble_path(canonical_path, &canonical_root)
}

fn current_preamble_scan(inputs: &PackageInputs) -> super::preamble::PreambleScan {
    super::preamble::PreambleScan {
        symbols: inputs.preamble_sourced_symbols.clone(),
        attached_packages: inputs.preamble_sourced_attached_packages.clone(),
        sourced_files: inputs.preamble_sourced_files.clone(),
        sourced_files_by_preamble: inputs.preamble_sourced_files_by_preamble.clone(),
    }
}

fn rescan_preamble_for_path(
    inputs: &PackageInputs,
    root: &Path,
    path: &Path,
    canonical_path: &Path,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> super::preamble::PreambleScan {
    let mut affected_paths = vec![path.to_path_buf()];
    if canonical_path != path {
        affected_paths.push(canonical_path.to_path_buf());
    }
    // Synchronous caller: `current_preamble_scan(inputs)` is read under the
    // same lock the result is applied under, so no concurrent update can slip
    // between snapshot and apply — installing the whole scan is safe and the
    // rescanned-preamble set is not needed.
    super::preamble::rescan_testthat_preambles_for_paths_with_overrides_and_exclusions(
        root,
        &current_preamble_scan(inputs),
        &affected_paths,
        &super::preamble::PreambleTextOverrides::new(),
        exclusions,
    )
    .0
}

/// If `canonical_path` is a file that `.Rprofile` transitively `source()`s (and
/// modeling is on), rescan the prelude; if it is a testthat preamble file or a
/// file a preamble transitively `source()`s (issue #638), rescan the preamble
/// closure. Any triggered rescans are combined with `base` as a `Batch`;
/// otherwise `base` is returned unchanged. Used by the terminal
/// `translate_watched` arms (R-source, `data/`, `data-raw/`) so a watched file
/// that is BOTH a package input AND a sourced helper refreshes both concerns —
/// never one at the expense of the other. (Like the surrounding arms, the
/// rescans are bounded disk reads in the `WatchedFileChanged` path, which
/// `translate` sanctions.)
fn fold_prelude_rescan(
    inputs: &mut PackageInputs,
    root: &Path,
    path: &Path,
    canonical_path: &Path,
    base: PackageInputDelta,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> Option<PackageInputDelta> {
    let mut deltas = vec![base];
    if inputs.model_rprofile && inputs.rprofile_sourced_files.contains(canonical_path) {
        let scan = super::rprofile::scan_workspace_rprofile_with_exclusions(root, exclusions);
        apply_rprofile_scan(inputs, Some(scan));
        deltas.push(PackageInputDelta::RProfileChanged);
    }
    if preamble_rescan_needed(inputs, root, path, canonical_path) {
        let scan = rescan_preamble_for_path(inputs, root, path, canonical_path, exclusions);
        if let Some(delta) = apply_preamble_scan(inputs, Some(scan)) {
            deltas.push(delta);
        }
    }
    if deltas.len() == 1 {
        deltas.pop()
    } else {
        Some(PackageInputDelta::Batch(deltas))
    }
}

fn translate_watched(
    inputs: &mut PackageInputs,
    root: &Path,
    uri: tower_lsp::lsp_types::Url,
    on_disk_text: Option<Arc<str>>,
    deleted: bool,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> Option<PackageInputDelta> {
    let path = uri.to_file_path().ok()?;

    // Normalize root and path once so comparisons against `<root>/DESCRIPTION`
    // and `<root>/NAMESPACE` aren't foiled by symlinks, casing, or trailing
    // separators. `canonicalize` requires the target to exist; on deletion it
    // fails (the file is gone), so fall back to canonicalizing the PARENT (which
    // still exists) and rejoining the file name. Without that, a DELETED
    // `.Rprofile`/testthat-preamble sourced helper under a SYMLINKED workspace
    // root would miss its canonical source-set membership check and leave stale
    // symbols in scope.
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let canonical_path =
        path.canonicalize()
            .unwrap_or_else(|_| match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => parent
                    .canonicalize()
                    .map(|cp| cp.join(name))
                    .unwrap_or_else(|_| path.clone()),
                _ => path.clone(),
            });
    let canonical_desc = canonical_root.join("DESCRIPTION");
    let canonical_ns = canonical_root.join("NAMESPACE");
    // Treat a newly-excluded path the same as a deletion: every branch below
    // (DESCRIPTION/NAMESPACE/.Rprofile/r-source/data(-raw)/generic-dir) already
    // has correct "file gone" cleanup logic, so folding exclusion into
    // `deleted` reuses it instead of duplicating removal logic per branch.
    let deleted = deleted || (!exclusions.is_empty() && exclusions.is_excluded_path(&path));

    if canonical_path == canonical_desc || path == root.join("DESCRIPTION") {
        if deleted {
            inputs.description = None;
            return Some(PackageInputDelta::DescriptionChanged);
        }
        // Treat a failed read as "no signal" rather than a deletion: leave
        // the existing input untouched and emit no delta. Otherwise a
        // transient I/O error would wipe DESCRIPTION state and force
        // downstream re-derives as if the file were gone.
        let text = on_disk_text.or_else(|| fs::read_to_string(&path).ok().map(Arc::from))?;
        inputs.description = Some(DescriptionInput { text });
        return Some(PackageInputDelta::DescriptionChanged);
    }

    if canonical_path == canonical_ns || path == root.join("NAMESPACE") {
        if deleted {
            inputs.namespace = None;
            return Some(PackageInputDelta::NamespaceChanged);
        }
        let text = on_disk_text.or_else(|| fs::read_to_string(&path).ok().map(Arc::from))?;
        inputs.namespace = Some(NamespaceInput { text });
        return Some(PackageInputDelta::NamespaceChanged);
    }

    let canonical_rprofile = canonical_root.join(".Rprofile");
    if canonical_path == canonical_rprofile || path == root.join(".Rprofile") {
        // `WatchedFileChanged` MAY do disk I/O (see `translate`'s doc comment):
        // scan `.Rprofile` from disk unless it was deleted or modeling is off,
        // then apply via the shared lock-safe seam (which also clears on
        // delete/disabled and reports whether anything changed).
        let scan = if deleted || !inputs.model_rprofile {
            None
        } else {
            Some(super::rprofile::scan_workspace_rprofile_with_exclusions(
                root, exclusions,
            ))
        };
        return apply_rprofile_scan(inputs, scan);
    }

    if let Some(kind) = is_r_source_path(&path, root) {
        // Compute the base R-source delta first. A package source file edit can
        // ALSO be a helper that `.Rprofile` follows via `source()`; in that case
        // the prelude must be re-scanned so its harvested symbols/packages stay
        // fresh (Task 12 transitive freshness). The base delta is always emitted;
        // the prelude rescan is folded in as a Batch when applicable.
        let base = if deleted {
            inputs.r_files.remove(&path);
            PackageInputDelta::RFileDeleted {
                path: path.clone(),
                kind,
            }
        } else {
            // Decode the disk fallback through the shared BOM-aware seam so this
            // incremental path matches the bulk scan
            // (collect_package_r_file_inputs_from_disk); an undecodable file yields
            // no text and is treated as "no signal" (leaves the prior input).
            let text =
                on_disk_text.or_else(|| crate::state::read_source(&path).ok().map(Arc::from))?;
            let digest = ContentDigest::of(&text);
            inputs.r_files.insert(
                path.clone(),
                RFileInput {
                    kind,
                    text,
                    content_digest: digest,
                },
            );
            PackageInputDelta::RFileChanged {
                path: path.clone(),
                kind,
            }
        };

        // Membership uses `canonical_path`: `rprofile_sourced_files` stores the
        // canonicalized paths the scanner followed (see `scan_workspace_rprofile`),
        // and `canonical_path` is canonicalized the same way at the top of this fn.
        // A package source file can ALSO be a `.Rprofile` helper — fold the
        // prelude rescan in as a Batch when so.
        return fold_prelude_rescan(inputs, root, &path, &canonical_path, base, exclusions);
    }

    // data/ directory file changes: rescan dataset names. A `data/` file can
    // also be a `.Rprofile`-sourced helper, so fold the prelude rescan in (a
    // helper here must refresh BOTH dataset names AND the prelude — neither at
    // the other's expense).
    let data_dir = root.join("data");
    if path.starts_with(&data_dir) && path != data_dir {
        inputs.dataset_names = super::scan_own_package_data_dir_with_exclusions(root, exclusions);
        return fold_prelude_rescan(
            inputs,
            root,
            &path,
            &canonical_path,
            PackageInputDelta::DataDirChanged,
            exclusions,
        );
    }

    // data-raw/ directory file changes: rescan sysdata generating scripts (same
    // dual-concern fold as data/ above).
    let data_raw_dir = root.join("data-raw");
    if path.starts_with(&data_raw_dir) && path != data_raw_dir {
        inputs.sysdata_names =
            super::sysdata::scan_sysdata_generating_scripts_with_exclusions(root, exclusions);
        return fold_prelude_rescan(
            inputs,
            root,
            &path,
            &canonical_path,
            PackageInputDelta::DataDirChanged,
            exclusions,
        );
    }

    // A `.Rprofile` OR a testthat preamble may `source()` a helper that lives
    // OUTSIDE every tracked package input dir (e.g. `scripts/setup.R`, plain
    // `inst/foo.R`). Such a helper is not an `is_r_source_path`, not a
    // `data*/` file, and not a tracked package dir, so none of the arms above
    // fire — but editing it must still re-scan the prelude (Task 12) and/or
    // the preamble closure (issue #638). This arm is LAST among the file arms
    // so the dual-concern folds above take precedence. Membership uses
    // `canonical_path` to match the canonical paths the scanners record.
    {
        let mut deltas = Vec::new();
        if inputs.model_rprofile && inputs.rprofile_sourced_files.contains(&canonical_path) {
            let scan = super::rprofile::scan_workspace_rprofile_with_exclusions(root, exclusions);
            if let Some(delta) = apply_rprofile_scan(inputs, Some(scan)) {
                deltas.push(delta);
            }
        }
        if preamble_rescan_needed(inputs, root, &path, &canonical_path) {
            let scan = rescan_preamble_for_path(inputs, root, &path, &canonical_path, exclusions);
            if let Some(delta) = apply_preamble_scan(inputs, Some(scan)) {
                deltas.push(delta);
            }
        }
        match deltas.len() {
            0 => {}
            1 => return deltas.pop(),
            _ => return Some(PackageInputDelta::Batch(deltas)),
        }
    }

    translate_watched_directory(inputs, root, &path, deleted, exclusions)
}

fn translate_watched_directory(
    inputs: &mut PackageInputs,
    root: &Path,
    path: &Path,
    deleted: bool,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> Option<PackageInputDelta> {
    if !deleted && !path.is_dir() {
        return None;
    }
    if !is_tracked_package_dir(path, root) {
        return None;
    }

    // data/ directory: rescan dataset names rather than collecting R source files.
    let data_dir = root.join("data");
    if path == data_dir || path.starts_with(&data_dir) {
        inputs.dataset_names = super::scan_own_package_data_dir_with_exclusions(root, exclusions);
        return Some(PackageInputDelta::DataDirChanged);
    }

    // data-raw/ directory: rescan sysdata generating scripts.
    let data_raw_dir = root.join("data-raw");
    if path == data_raw_dir || path.starts_with(&data_raw_dir) {
        inputs.sysdata_names =
            super::sysdata::scan_sysdata_generating_scripts_with_exclusions(root, exclusions);
        return Some(PackageInputDelta::DataDirChanged);
    }

    let mut deltas = Vec::new();
    let existing_under_path: Vec<_> = inputs
        .r_files
        .keys()
        .filter(|candidate| candidate.starts_with(path))
        .cloned()
        .collect();
    let mut seen = BTreeSet::new();

    let complete_scan = if deleted {
        true
    } else if exclusions.is_empty() {
        collect_r_file_inputs_from_dir(inputs, root, path, &mut seen, &mut deltas)
    } else {
        collect_r_file_inputs_from_dir_with_exclusions(
            inputs,
            root,
            path,
            exclusions,
            &mut seen,
            &mut deltas,
        )
    };

    if complete_scan {
        for existing in existing_under_path {
            if seen.contains(&existing) {
                continue;
            }
            if let Some(kind) = inputs.r_files.remove(&existing).map(|entry| entry.kind) {
                deltas.push(PackageInputDelta::RFileDeleted {
                    path: existing,
                    kind,
                });
            }
        }
    }

    if deltas.is_empty() {
        None
    } else {
        Some(PackageInputDelta::Batch(deltas))
    }
}

fn is_tracked_package_dir(path: &Path, root: &Path) -> bool {
    let r_dir = root.join("R");
    let testthat_dir = root.join("tests").join("testthat");
    let testit_dir = root.join("tests").join("testit");
    let data_dir = root.join("data");
    let data_raw_dir = root.join("data-raw");
    path == r_dir
        || path.starts_with(&r_dir)
        || path == testthat_dir
        || path.starts_with(&testthat_dir)
        || path == testit_dir
        || path.starts_with(&testit_dir)
        || path == data_dir
        || path.starts_with(&data_dir)
        || path == data_raw_dir
        || path.starts_with(&data_raw_dir)
}

fn collect_r_file_inputs_from_dir(
    inputs: &mut PackageInputs,
    root: &Path,
    dir: &Path,
    seen: &mut BTreeSet<std::path::PathBuf>,
    deltas: &mut Vec<PackageInputDelta>,
) -> bool {
    let mut complete_scan = true;
    for entry in walkdir::WalkDir::new(dir).follow_links(false).into_iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                complete_scan = false;
                log::trace!(
                    "Package R directory scan skipped entry in {}: {}",
                    dir.display(),
                    err
                );
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        let Some(kind) = is_r_source_path(&path, root) else {
            continue;
        };
        // BOM-aware decode, matching the bulk scan; an undecodable file is
        // skipped (and marks the scan incomplete so prior inputs are preserved).
        let text = match crate::state::read_source(&path) {
            Ok(text) => text,
            Err(err) => {
                complete_scan = false;
                log::trace!(
                    "Package R directory scan could not read {}: {}",
                    path.display(),
                    err
                );
                continue;
            }
        };
        let text: Arc<str> = text.into();
        let digest = ContentDigest::of(&text);
        seen.insert(path.clone());
        inputs.r_files.insert(
            path.clone(),
            RFileInput {
                kind,
                text,
                content_digest: digest,
            },
        );
        deltas.push(PackageInputDelta::RFileChanged { path, kind });
    }
    complete_scan
}

fn collect_r_file_inputs_from_dir_with_exclusions(
    inputs: &mut PackageInputs,
    root: &Path,
    dir: &Path,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
    seen: &mut BTreeSet<std::path::PathBuf>,
    deltas: &mut Vec<PackageInputDelta>,
) -> bool {
    if exclusions.can_prune_directory(dir) {
        return true;
    }

    let mut complete_scan = true;
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            !entry.file_type().is_dir() || !exclusions.can_prune_directory(entry.path())
        })
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                complete_scan = false;
                log::trace!(
                    "Package R directory scan skipped entry in {}: {}",
                    dir.display(),
                    err
                );
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        if exclusions.is_excluded_path(&path) {
            continue;
        }
        let Some(kind) = is_r_source_path(&path, root) else {
            continue;
        };
        let text = match crate::state::read_source(&path) {
            Ok(text) => text,
            Err(err) => {
                complete_scan = false;
                log::trace!(
                    "Package R directory scan could not read {}: {}",
                    path.display(),
                    err
                );
                continue;
            }
        };
        let text: Arc<str> = text.into();
        let digest = ContentDigest::of(&text);
        seen.insert(path.clone());
        inputs.r_files.insert(
            path.clone(),
            RFileInput {
                kind,
                text,
                content_digest: digest,
            },
        );
        deltas.push(PackageInputDelta::RFileChanged { path, kind });
    }
    complete_scan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_file::config::PackageMode;

    fn root_inputs() -> PackageInputs {
        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some("/work/pkg".into());
        inputs.package_mode = PackageMode::Auto;
        inputs
    }

    #[test]
    fn did_change_for_r_file_emits_rfile_changed() {
        let mut inputs = root_inputs();
        let uri = tower_lsp::lsp_types::Url::from_file_path("/work/pkg/R/foo.R").unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::DidChange {
                uri,
                text: "x <- 1\n".into(),
            },
        );
        assert!(matches!(
            delta,
            Some(PackageInputDelta::RFileChanged {
                kind: RFileKind::Source,
                ..
            })
        ));
        assert_eq!(inputs.r_files.len(), 1);
    }

    #[test]
    fn did_change_for_non_r_file_emits_no_delta() {
        let mut inputs = root_inputs();
        let uri = tower_lsp::lsp_types::Url::from_file_path("/work/pkg/inst/data.R").unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::DidChange {
                uri,
                text: "x <- 1\n".into(),
            },
        );
        assert!(delta.is_none());
    }

    #[test]
    fn did_change_for_description_updates_input() {
        let mut inputs = root_inputs();
        let uri = tower_lsp::lsp_types::Url::from_file_path("/work/pkg/DESCRIPTION").unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::DidChange {
                uri,
                text: "Package: foo\nImports: stats\n".into(),
            },
        );
        assert!(matches!(delta, Some(PackageInputDelta::DescriptionChanged)));
        assert_eq!(
            inputs.description.as_ref().map(|d| &*d.text),
            Some("Package: foo\nImports: stats\n")
        );
    }

    #[test]
    fn did_change_for_namespace_updates_input() {
        let mut inputs = root_inputs();
        let uri = tower_lsp::lsp_types::Url::from_file_path("/work/pkg/NAMESPACE").unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::DidChange {
                uri,
                text: "importFrom(stats, median)\n".into(),
            },
        );
        assert!(matches!(delta, Some(PackageInputDelta::NamespaceChanged)));
        assert_eq!(
            inputs.namespace.as_ref().map(|d| &*d.text),
            Some("importFrom(stats, median)\n")
        );
    }

    #[test]
    fn description_change_updates_inputs_and_emits_delta() {
        let mut inputs = root_inputs();
        let uri = tower_lsp::lsp_types::Url::from_file_path("/work/pkg/DESCRIPTION").unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: Some("Package: foo\n".into()),
                deleted: false,
            },
        );
        assert!(matches!(delta, Some(PackageInputDelta::DescriptionChanged)));
        assert!(inputs.description.is_some());
    }

    #[test]
    fn description_deletion_clears_input() {
        let mut inputs = root_inputs();
        inputs.description = Some(DescriptionInput {
            text: "Package: foo\n".into(),
        });
        let uri = tower_lsp::lsp_types::Url::from_file_path("/work/pkg/DESCRIPTION").unwrap();
        let _ = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: None,
                deleted: true,
            },
        );
        assert!(inputs.description.is_none());
    }

    #[test]
    fn watched_manifest_read_failure_preserves_inputs() {
        // Use a tempdir root so canonicalize succeeds for the root but the
        // DESCRIPTION/NAMESPACE files never exist on disk. With `deleted=false`
        // and no `on_disk_text`, the fallback read fails and the prior inputs
        // must be left untouched (no spurious delta wiping the state).
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;
        inputs.description = Some(DescriptionInput {
            text: "Package: foo\n".into(),
        });
        inputs.namespace = Some(NamespaceInput {
            text: "export(foo)\n".into(),
        });

        let desc_uri = tower_lsp::lsp_types::Url::from_file_path(root.join("DESCRIPTION")).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: desc_uri,
                on_disk_text: None,
                deleted: false,
            },
        );
        assert!(delta.is_none());
        assert_eq!(
            inputs.description.as_ref().map(|d| &*d.text),
            Some("Package: foo\n")
        );

        let ns_uri = tower_lsp::lsp_types::Url::from_file_path(root.join("NAMESPACE")).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: ns_uri,
                on_disk_text: None,
                deleted: false,
            },
        );
        assert!(delta.is_none());
        assert_eq!(
            inputs.namespace.as_ref().map(|n| &*n.text),
            Some("export(foo)\n")
        );
    }

    #[test]
    fn did_close_with_disk_text_keeps_file_in_inputs() {
        let mut inputs = root_inputs();
        let uri = tower_lsp::lsp_types::Url::from_file_path("/work/pkg/R/foo.R").unwrap();
        let _ = translate(
            &mut inputs,
            HandlerEvent::DidOpen {
                uri: uri.clone(),
                text: "open\n".into(),
            },
        );
        let _ = translate(
            &mut inputs,
            HandlerEvent::DidClose {
                uri: uri.clone(),
                on_disk_text: Some("disk\n".into()),
            },
        );
        let entry = inputs.r_files.get(&uri.to_file_path().unwrap()).unwrap();
        assert_eq!(&*entry.text, "disk\n");
    }

    #[test]
    fn did_close_without_disk_removes_file() {
        let mut inputs = root_inputs();
        let uri = tower_lsp::lsp_types::Url::from_file_path("/work/pkg/R/foo.R").unwrap();
        let _ = translate(
            &mut inputs,
            HandlerEvent::DidOpen {
                uri: uri.clone(),
                text: "open\n".into(),
            },
        );
        let _ = translate(
            &mut inputs,
            HandlerEvent::DidClose {
                uri: uri.clone(),
                on_disk_text: None,
            },
        );
        assert!(inputs.r_files.is_empty());
    }

    #[test]
    fn watched_directory_change_hydrates_disk_r_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let r_dir = root.join("R");
        let test_dir = root.join("tests").join("testthat");
        std::fs::create_dir_all(&r_dir).unwrap();
        std::fs::create_dir_all(&test_dir).unwrap();
        std::fs::write(r_dir.join("foo.R"), "foo <- 1\n").unwrap();
        std::fs::write(test_dir.join("test-foo.R"), "test_that('foo', foo)\n").unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;

        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: tower_lsp::lsp_types::Url::from_file_path(&r_dir).unwrap(),
                on_disk_text: None,
                deleted: false,
            },
        );

        assert!(matches!(delta, Some(PackageInputDelta::Batch(_))));
        assert!(inputs.r_files.contains_key(&r_dir.join("foo.R")));

        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: tower_lsp::lsp_types::Url::from_file_path(&test_dir).unwrap(),
                on_disk_text: None,
                deleted: false,
            },
        );

        assert!(matches!(delta, Some(PackageInputDelta::Batch(_))));
        assert!(inputs.r_files.contains_key(&test_dir.join("test-foo.R")));
    }

    #[test]
    fn watched_directory_partial_read_failure_preserves_existing_r_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let r_dir = root.join("R");
        std::fs::create_dir_all(&r_dir).unwrap();
        let good_path = r_dir.join("good.R");
        let unreadable_path = r_dir.join("unreadable.R");
        std::fs::write(&good_path, "good <- 1\n").unwrap();
        std::fs::write(&unreadable_path, [0xff, 0xfe, b'\n']).unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;
        let old_text: Arc<str> = "old <- 1\n".into();
        inputs.r_files.insert(
            unreadable_path.clone(),
            RFileInput {
                kind: RFileKind::Source,
                content_digest: ContentDigest::of(&old_text),
                text: old_text,
            },
        );

        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: tower_lsp::lsp_types::Url::from_file_path(&r_dir).unwrap(),
                on_disk_text: None,
                deleted: false,
            },
        );

        let Some(PackageInputDelta::Batch(deltas)) = delta else {
            panic!("expected batch delta for readable sibling");
        };
        assert!(
            deltas.iter().all(|delta| !matches!(
                delta,
                PackageInputDelta::RFileDeleted { path, .. } if path == &unreadable_path
            )),
            "partial scan must not emit deletion for an existing unreadable R file"
        );
        assert!(
            inputs.r_files.contains_key(&unreadable_path),
            "partial scan must preserve prior input for unreadable existing files"
        );
        assert!(inputs.r_files.contains_key(&good_path));
    }

    #[test]
    fn watched_directory_delete_removes_existing_subtree_inputs() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let r_dir = root.join("R");
        let nested = r_dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;
        for path in [r_dir.join("foo.R"), nested.join("bar.R")] {
            let text: std::sync::Arc<str> = "x <- 1\n".into();
            inputs.r_files.insert(
                path,
                RFileInput {
                    kind: RFileKind::Source,
                    content_digest: ContentDigest::of(&text),
                    text,
                },
            );
        }

        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: tower_lsp::lsp_types::Url::from_file_path(&r_dir).unwrap(),
                on_disk_text: None,
                deleted: true,
            },
        );

        assert!(matches!(delta, Some(PackageInputDelta::Batch(_))));
        assert!(inputs.r_files.is_empty());
    }

    #[test]
    fn watched_r_file_change_strips_bom_when_reading_from_disk() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let r_dir = root.join("R");
        std::fs::create_dir_all(&r_dir).unwrap();
        let path = r_dir.join("foo.R");
        // A UTF-8-BOM package R file. The watched-file path falls back to a disk
        // read here (on_disk_text is None when the cross-file cache is cold), and
        // must strip the BOM so the incremental path agrees with the bulk scan
        // (collect_package_r_file_inputs_from_disk) on RFileInput.text and its
        // ContentDigest — otherwise the same file yields two digests/parses
        // depending on ingestion path.
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"foo <- 2\n");
        std::fs::write(&path, bytes).unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;

        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: tower_lsp::lsp_types::Url::from_file_path(&path).unwrap(),
                on_disk_text: None,
                deleted: false,
            },
        );

        assert!(matches!(
            delta,
            Some(PackageInputDelta::RFileChanged { .. })
        ));
        let entry = inputs.r_files.get(&path).expect("file input");
        assert_eq!(
            &*entry.text, "foo <- 2\n",
            "BOM must be stripped on the watched-file disk read"
        );
    }

    #[test]
    fn watched_directory_scan_strips_bom() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let r_dir = root.join("R");
        std::fs::create_dir_all(&r_dir).unwrap();
        let path = r_dir.join("foo.R");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"foo <- 1\n");
        std::fs::write(&path, bytes).unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;

        // A directory-level watched event triggers the recursive rescan, which
        // must decode through the same BOM-aware seam as the bulk scan.
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: tower_lsp::lsp_types::Url::from_file_path(&r_dir).unwrap(),
                on_disk_text: None,
                deleted: false,
            },
        );

        assert!(matches!(delta, Some(PackageInputDelta::Batch(_))));
        let entry = inputs.r_files.get(&path).expect("file input");
        assert_eq!(
            &*entry.text, "foo <- 1\n",
            "BOM must be stripped in the directory rescan"
        );
    }

    #[test]
    fn watched_file_change_without_text_reads_from_disk() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let r_dir = root.join("R");
        std::fs::create_dir_all(&r_dir).unwrap();
        let path = r_dir.join("foo.R");
        std::fs::write(&path, "foo <- 2\n").unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;

        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: tower_lsp::lsp_types::Url::from_file_path(&path).unwrap(),
                on_disk_text: None,
                deleted: false,
            },
        );

        assert!(matches!(
            delta,
            Some(PackageInputDelta::RFileChanged { .. })
        ));
        let entry = inputs.r_files.get(&path).expect("file input");
        assert_eq!(&*entry.text, "foo <- 2\n");
    }

    #[test]
    fn setting_changed_without_workspace_root_is_applied() {
        let mut inputs = PackageInputs::default();
        assert!(inputs.workspace_root.is_none());
        // Start in the default Auto mode.
        assert_eq!(inputs.package_mode, PackageMode::Auto);
        let delta = translate(
            &mut inputs,
            HandlerEvent::SettingChanged {
                new_mode: PackageMode::Disabled,
            },
        );
        assert!(matches!(delta, Some(PackageInputDelta::SettingChanged)));
        assert_eq!(inputs.package_mode, PackageMode::Disabled);
    }

    #[test]
    fn watched_rprofile_change_rescans_and_emits_delta() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.model_rprofile = true;
        std::fs::write(root.join(".Rprofile"), "my_helper <- function() 1\n").unwrap();
        let uri = tower_lsp::lsp_types::Url::from_file_path(root.join(".Rprofile")).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: None,
                deleted: false,
            },
        );
        assert_eq!(delta, Some(PackageInputDelta::RProfileChanged));
        assert!(
            inputs.rprofile_symbols.contains("my_helper"),
            "got {:?}",
            inputs.rprofile_symbols
        );
    }

    #[test]
    fn watched_rprofile_delete_clears_symbols() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.model_rprofile = true;
        inputs.rprofile_symbols.insert("old".to_string());
        let uri = tower_lsp::lsp_types::Url::from_file_path(root.join(".Rprofile")).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: None,
                deleted: true,
            },
        );
        assert_eq!(delta, Some(PackageInputDelta::RProfileChanged));
        assert!(inputs.rprofile_symbols.is_empty());
    }

    #[test]
    fn watched_rprofile_change_is_noop_when_disabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.model_rprofile = false;
        std::fs::write(root.join(".Rprofile"), "my_helper <- function() 1\n").unwrap();
        let uri = tower_lsp::lsp_types::Url::from_file_path(root.join(".Rprofile")).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: None,
                deleted: false,
            },
        );
        // Disabled: no rescan, no symbols. (Delta may be None.)
        assert!(inputs.rprofile_symbols.is_empty());
        let _ = delta;
    }

    #[test]
    fn acceptance_9_live_update_redrives_contribution() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.model_rprofile = true;

        // Initial: helper_a defined.
        std::fs::write(root.join(".Rprofile"), "helper_a <- function() 1\n").unwrap();
        let uri = tower_lsp::lsp_types::Url::from_file_path(root.join(".Rprofile")).unwrap();
        let _ = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: uri.clone(),
                on_disk_text: None,
                deleted: false,
            },
        );
        let s1 = crate::package_state::derive_package_state(
            &crate::package_state::PackageState::new(),
            &inputs,
            &PackageInputDelta::RProfileChanged,
        );
        assert!(
            s1.scope_contribution()
                .rprofile_symbols
                .contains("helper_a")
        );

        // Edit: helper_b instead.
        std::fs::write(root.join(".Rprofile"), "helper_b <- function() 1\n").unwrap();
        let _ = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: None,
                deleted: false,
            },
        );
        let s2 = crate::package_state::derive_package_state(
            &s1,
            &inputs,
            &PackageInputDelta::RProfileChanged,
        );
        assert!(
            s2.scope_contribution()
                .rprofile_symbols
                .contains("helper_b")
        );
        assert!(
            !s2.scope_contribution()
                .rprofile_symbols
                .contains("helper_a"),
            "live edit must drop the old symbol"
        );
    }

    #[test]
    fn editing_a_sourced_helper_rescans_the_prelude() {
        // `.Rprofile` sources `R/functions.r`; that helper defines a symbol.
        // Editing the helper (a package R-source file) must (a) emit the normal
        // RFileChanged delta for the package source file AND (b) re-scan the
        // prelude so the new helper symbol is reflected in rprofile_symbols.
        // The combined effect is delivered as a Batch.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let r_dir = root.join("R");
        std::fs::create_dir_all(&r_dir).unwrap();
        let helper = r_dir.join("functions.r");

        std::fs::write(root.join(".Rprofile"), "source(\"R/functions.r\")\n").unwrap();
        std::fs::write(&helper, "helper_a <- function() 1\n").unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;
        inputs.model_rprofile = true;

        // Seed the prelude by scanning `.Rprofile` once (records the sourced
        // helper in rprofile_sourced_files and harvests helper_a).
        let scan = super::rprofile::scan_workspace_rprofile(root);
        inputs.rprofile_symbols = scan.symbols;
        inputs.rprofile_attached_packages = scan.attached_packages;
        inputs.rprofile_sourced_files = scan.sourced_files;
        assert!(
            inputs.rprofile_symbols.contains("helper_a"),
            "precondition: prelude harvests helper_a from the sourced file"
        );

        // Edit the helper to define a different symbol.
        std::fs::write(&helper, "helper_b <- function() 1\n").unwrap();
        let uri = tower_lsp::lsp_types::Url::from_file_path(&helper).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: None,
                deleted: false,
            },
        );

        // The watched-file edit yields a Batch carrying the package RFileChanged
        // delta plus an RProfileChanged delta from the forced prelude rescan.
        let Some(PackageInputDelta::Batch(deltas)) = delta else {
            panic!("expected Batch delta, got {:?}", delta);
        };
        assert!(
            deltas
                .iter()
                .any(|d| matches!(d, PackageInputDelta::RFileChanged { .. })),
            "batch must include the base RFileChanged delta: {:?}",
            deltas
        );
        assert!(
            deltas
                .iter()
                .any(|d| matches!(d, PackageInputDelta::RProfileChanged)),
            "batch must include an RProfileChanged delta from the rescan: {:?}",
            deltas
        );

        // The prelude rescan picked up the helper edit.
        assert!(
            inputs.rprofile_symbols.contains("helper_b"),
            "rescan must harvest the new helper symbol, got {:?}",
            inputs.rprofile_symbols
        );
        assert!(
            !inputs.rprofile_symbols.contains("helper_a"),
            "rescan must drop the old helper symbol, got {:?}",
            inputs.rprofile_symbols
        );
    }

    #[test]
    fn editing_a_non_source_sourced_helper_rescans_the_prelude() {
        // `.Rprofile` sources `scripts/setup.R`, a helper that is NOT a tracked
        // package R-source file (`is_r_source_path` → None), so the
        // is_r_source_path rescan branch never fires for it. The dedicated
        // sourced-helper arm must still re-scan the prelude (Task 12) so a
        // `scripts/` helper edit is reflected in rprofile_symbols.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let scripts = root.join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let helper = scripts.join("setup.R");

        std::fs::write(root.join(".Rprofile"), "source(\"scripts/setup.R\")\n").unwrap();
        std::fs::write(&helper, "helper_a <- function() 1\n").unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;
        inputs.model_rprofile = true;

        // Seed the prelude (records the sourced helper + harvests helper_a).
        let scan = super::rprofile::scan_workspace_rprofile(root);
        inputs.rprofile_symbols = scan.symbols;
        inputs.rprofile_attached_packages = scan.attached_packages;
        inputs.rprofile_sourced_files = scan.sourced_files;
        assert!(
            inputs.rprofile_symbols.contains("helper_a"),
            "precondition: prelude harvests helper_a from scripts/setup.R"
        );
        // Sanity: the helper is genuinely not a package R-source path, so this
        // test exercises the dedicated arm rather than the is_r_source_path one.
        assert!(is_r_source_path(&helper, root).is_none());

        // Edit the helper to define a different symbol.
        std::fs::write(&helper, "helper_b <- function() 1\n").unwrap();
        let uri = tower_lsp::lsp_types::Url::from_file_path(&helper).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: None,
                deleted: false,
            },
        );

        // A non-source helper yields RProfileChanged alone (no base RFileChanged,
        // since it is not tracked package state).
        assert_eq!(
            delta,
            Some(PackageInputDelta::RProfileChanged),
            "scripts/ helper edit must emit RProfileChanged, got {:?}",
            delta
        );
        assert!(
            inputs.rprofile_symbols.contains("helper_b"),
            "rescan must harvest the new helper symbol, got {:?}",
            inputs.rprofile_symbols
        );
        assert!(
            !inputs.rprofile_symbols.contains("helper_a"),
            "rescan must drop the old helper symbol, got {:?}",
            inputs.rprofile_symbols
        );
    }

    #[test]
    fn editing_a_preamble_sourced_helper_rescans_the_preamble_closure() {
        // tests/testthat/helper-project.R sources scripts/helpers.R via the
        // issue #638 computed-path idiom. scripts/helpers.R is NOT a tracked
        // package R-source file, so only the dedicated preamble-sourced arm
        // can refresh the harvested symbols when it changes on disk.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("tests/testthat")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        let helper = root.join("scripts/helpers.R");
        let preamble = root.join("tests/testthat/helper-project.R");
        std::fs::write(
            &preamble,
            "repo_root <- normalizePath(file.path(\"..\", \"..\"))\nsource(file.path(repo_root, \"scripts/helpers.R\"))\n",
        )
        .unwrap();
        std::fs::write(&helper, "old_def <- function() 1\n").unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;

        // Seed the preamble scan (records the sourced helper + harvests old_def).
        let exclusions = crate::config_file::CompiledWorkspaceExclusions::default();
        let scan = super::preamble::scan_testthat_preambles_with_exclusions(root, &exclusions);
        inputs.preamble_sourced_symbols = scan.symbols;
        inputs.preamble_sourced_attached_packages = scan.attached_packages;
        inputs.preamble_sourced_files = scan.sourced_files;
        inputs.preamble_sourced_files_by_preamble = scan.sourced_files_by_preamble;
        assert!(
            inputs
                .preamble_sourced_symbols
                .get(&preamble)
                .is_some_and(|s| s.contains("old_def")),
            "precondition: scan harvests old_def, got {:?}",
            inputs.preamble_sourced_symbols
        );
        assert!(is_r_source_path(&helper, root).is_none());

        // Edit the sourced helper to define a different symbol.
        std::fs::write(&helper, "new_def <- function() 1\n").unwrap();
        let uri = tower_lsp::lsp_types::Url::from_file_path(&helper).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: None,
                deleted: false,
            },
        );

        assert_eq!(
            delta,
            Some(PackageInputDelta::PreambleSourcesChanged),
            "scripts/ helper edit must emit PreambleSourcesChanged, got {delta:?}"
        );
        let symbols = inputs.preamble_sourced_symbols.get(&preamble).unwrap();
        assert!(symbols.contains("new_def"), "got {symbols:?}");
        assert!(!symbols.contains("old_def"), "got {symbols:?}");
    }

    #[cfg(unix)]
    #[test]
    fn deleting_preamble_helper_through_symlink_rescans_canonical_closure() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let real_root = tmp.path().join("real-pkg");
        let linked_root = tmp.path().join("linked-pkg");
        std::fs::create_dir_all(real_root.join("tests/testthat")).unwrap();
        std::fs::create_dir_all(real_root.join("scripts")).unwrap();
        symlink(&real_root, &linked_root).unwrap();

        let preamble = linked_root.join("tests/testthat/helper-project.R");
        let helper = linked_root.join("scripts/helpers.R");
        std::fs::write(&preamble, "source(\"../../scripts/helpers.R\")\n").unwrap();
        std::fs::write(&helper, "stale_def <- 1\n").unwrap();

        let exclusions = crate::config_file::CompiledWorkspaceExclusions::default();
        let scan =
            super::preamble::scan_testthat_preambles_with_exclusions(&linked_root, &exclusions);
        let mut inputs = PackageInputs {
            workspace_root: Some(linked_root.clone()),
            package_mode: PackageMode::Auto,
            preamble_sourced_symbols: scan.symbols,
            preamble_sourced_attached_packages: scan.attached_packages,
            preamble_sourced_files: scan.sourced_files,
            preamble_sourced_files_by_preamble: scan.sourced_files_by_preamble,
            ..PackageInputs::default()
        };
        assert!(
            inputs
                .preamble_sourced_symbols
                .get(&preamble)
                .is_some_and(|symbols| symbols.contains("stale_def"))
        );

        std::fs::remove_file(&helper).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: tower_lsp::lsp_types::Url::from_file_path(&helper).unwrap(),
                on_disk_text: None,
                deleted: true,
            },
        );

        assert_eq!(delta, Some(PackageInputDelta::PreambleSourcesChanged));
        assert!(!inputs.preamble_sourced_symbols.contains_key(&preamble));
        assert!(
            inputs
                .preamble_sourced_files
                .contains(&real_root.canonicalize().unwrap().join("scripts/helpers.R")),
            "missing target must remain routed for a later creation event, got {:?}",
            inputs.preamble_sourced_files
        );

        std::fs::write(&helper, "recreated_def <- 1\n").unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: tower_lsp::lsp_types::Url::from_file_path(&helper).unwrap(),
                on_disk_text: None,
                deleted: false,
            },
        );

        assert_eq!(delta, Some(PackageInputDelta::PreambleSourcesChanged));
        assert!(
            inputs
                .preamble_sourced_symbols
                .get(&preamble)
                .is_some_and(|symbols| symbols.contains("recreated_def"))
        );
    }

    #[test]
    fn editing_a_preamble_file_rescans_its_source_closure() {
        // Changing which files the preamble sources (a watched on-disk edit to
        // the preamble itself, which IS an is_r_source_path Test file) must
        // fold a preamble rescan into the RFileChanged delta.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("tests/testthat")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts/a.R"), "a_def <- 1\n").unwrap();
        std::fs::write(root.join("scripts/b.R"), "b_def <- 1\n").unwrap();
        let preamble = root.join("tests/testthat/helper-project.R");
        std::fs::write(&preamble, "source(\"../../scripts/a.R\")\n").unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;
        let exclusions = crate::config_file::CompiledWorkspaceExclusions::default();
        let scan = super::preamble::scan_testthat_preambles_with_exclusions(root, &exclusions);
        inputs.preamble_sourced_symbols = scan.symbols;
        inputs.preamble_sourced_files = scan.sourced_files;
        inputs.preamble_sourced_files_by_preamble = scan.sourced_files_by_preamble;

        // Repoint the preamble at scripts/b.R and translate the watched edit.
        std::fs::write(&preamble, "source(\"../../scripts/b.R\")\n").unwrap();
        let uri = tower_lsp::lsp_types::Url::from_file_path(&preamble).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: None,
                deleted: false,
            },
        );

        // Batch of the base RFileChanged plus the preamble rescan.
        match delta {
            Some(PackageInputDelta::Batch(ref deltas)) => {
                assert!(
                    deltas.contains(&PackageInputDelta::PreambleSourcesChanged),
                    "batch must carry PreambleSourcesChanged, got {deltas:?}"
                );
            }
            other => panic!("expected Batch with PreambleSourcesChanged, got {other:?}"),
        }
        let symbols = inputs.preamble_sourced_symbols.get(&preamble).unwrap();
        assert!(symbols.contains("b_def"), "got {symbols:?}");
        assert!(!symbols.contains("a_def"), "got {symbols:?}");
    }

    #[test]
    fn apply_preamble_scan_clears_when_package_mode_disabled() {
        let mut inputs = PackageInputs::default();
        inputs.package_mode = PackageMode::Disabled;
        inputs.preamble_sourced_symbols.insert(
            "x".into(),
            std::collections::BTreeSet::from(["s".to_string()]),
        );
        inputs
            .preamble_sourced_files
            .insert(std::path::PathBuf::from("/w/scripts/h.R"));

        let delta = apply_preamble_scan(&mut inputs, None);
        assert_eq!(delta, Some(PackageInputDelta::PreambleSourcesChanged));
        assert!(inputs.preamble_sourced_symbols.is_empty());
        assert!(inputs.preamble_sourced_attached_packages.is_empty());
        assert!(inputs.preamble_sourced_files.is_empty());
        assert!(inputs.preamble_sourced_files_by_preamble.is_empty());

        // Second application: nothing to clear → no delta.
        assert_eq!(apply_preamble_scan(&mut inputs, None), None);
    }

    #[test]
    fn watched_rprofile_delete_emits_delta_even_when_already_cleared() {
        // The watched-files handler may translate a `.Rprofile` deletion twice:
        // the early DELETED pre-pass clears the prelude, then the manifest block
        // translates it again and relies on the *second* translate still
        // returning RProfileChanged so its `scripts/` fanout fires. Guard that
        // the delete arm is unconditional, not gated on "had symbols".
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.model_rprofile = true;
        // Prelude already empty, as if a prior pre-pass cleared it.
        assert!(inputs.rprofile_symbols.is_empty());
        let uri = tower_lsp::lsp_types::Url::from_file_path(root.join(".Rprofile")).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: None,
                deleted: true,
            },
        );
        assert_eq!(
            delta,
            Some(PackageInputDelta::RProfileChanged),
            "delete must emit RProfileChanged unconditionally so the manifest \
             block's script fanout fires even after the pre-pass already cleared"
        );
    }

    #[test]
    fn sourced_helper_under_data_raw_refreshes_both_sysdata_and_prelude() {
        // A file can be BOTH a `data-raw/` sysdata script AND a `.Rprofile`
        // sourced helper. A watched change must refresh both concerns — the
        // prelude rescan must NOT preempt the sysdata rescan (or vice versa).
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let data_raw = root.join("data-raw");
        std::fs::create_dir_all(&data_raw).unwrap();
        let helper = data_raw.join("setup.R");
        std::fs::write(
            &helper,
            "helper_a <- function() 1\nusethis::use_data(my_internal, internal = TRUE)\n",
        )
        .unwrap();
        std::fs::write(root.join(".Rprofile"), "source(\"data-raw/setup.R\")\n").unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;
        inputs.model_rprofile = true;
        // Seed the prelude (records data-raw/setup.R as a sourced helper).
        let scan = super::rprofile::scan_workspace_rprofile(root);
        inputs.rprofile_symbols = scan.symbols;
        inputs.rprofile_attached_packages = scan.attached_packages;
        inputs.rprofile_sourced_files = scan.sourced_files;
        assert!(inputs.rprofile_symbols.contains("helper_a"));

        // Edit the helper: change BOTH the prelude symbol and the sysdata symbol.
        std::fs::write(
            &helper,
            "helper_b <- function() 1\nusethis::use_data(other_internal, internal = TRUE)\n",
        )
        .unwrap();
        let uri = tower_lsp::lsp_types::Url::from_file_path(&helper).unwrap();
        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri,
                on_disk_text: None,
                deleted: false,
            },
        );

        let Some(PackageInputDelta::Batch(deltas)) = delta else {
            panic!("expected a Batch carrying both refreshes, got {:?}", delta);
        };
        assert!(
            deltas
                .iter()
                .any(|d| matches!(d, PackageInputDelta::DataDirChanged)),
            "sysdata rescan must still fire: {:?}",
            deltas
        );
        assert!(
            deltas
                .iter()
                .any(|d| matches!(d, PackageInputDelta::RProfileChanged)),
            "prelude rescan must also fire: {:?}",
            deltas
        );
        assert!(
            inputs.sysdata_names.contains("other_internal"),
            "sysdata names must be refreshed: {:?}",
            inputs.sysdata_names
        );
        assert!(
            inputs.rprofile_symbols.contains("helper_b"),
            "prelude must be refreshed: {:?}",
            inputs.rprofile_symbols
        );
    }

    #[test]
    fn watched_data_raw_directory_event_refreshes_sysdata_names() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let data_raw = root.join("data-raw");
        std::fs::create_dir_all(&data_raw).unwrap();
        std::fs::write(
            data_raw.join("generate.R"),
            "usethis::use_data(my_internal, internal = TRUE)\n",
        )
        .unwrap();

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;

        let delta = translate(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: tower_lsp::lsp_types::Url::from_file_path(&data_raw).unwrap(),
                on_disk_text: None,
                deleted: false,
            },
        );

        assert!(
            matches!(delta, Some(PackageInputDelta::DataDirChanged)),
            "expected DataDirChanged, got: {:?}",
            delta
        );
        assert!(
            inputs.sysdata_names.contains("my_internal"),
            "expected sysdata_names to contain 'my_internal', got: {:?}",
            inputs.sysdata_names
        );
    }

    #[test]
    fn watched_excluded_data_file_change_does_not_seed_dataset_name() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let data_dir = root.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let excluded = data_dir.join("excluded.R");
        std::fs::write(&excluded, "excluded_dataset <- 1\n").unwrap();
        let exclusions = crate::config_file::compile_workspace_exclusions(
            &serde_json::json!({ "workspace": { "exclude": ["data/**"] } }),
            vec![root.to_path_buf()],
        );

        let mut inputs = PackageInputs::default();
        inputs.workspace_root = Some(root.to_path_buf());
        inputs.package_mode = PackageMode::Auto;
        inputs.dataset_names.insert("excluded_dataset".to_string());

        let delta = translate_with_exclusions(
            &mut inputs,
            HandlerEvent::WatchedFileChanged {
                uri: tower_lsp::lsp_types::Url::from_file_path(&excluded).unwrap(),
                on_disk_text: None,
                deleted: false,
            },
            &exclusions,
        );

        assert!(
            matches!(delta, Some(PackageInputDelta::DataDirChanged)),
            "expected DataDirChanged, got: {:?}",
            delta
        );
        assert!(
            !inputs.dataset_names.contains("excluded_dataset"),
            "excluded data/*.R change must not (re)seed dataset names: {:?}",
            inputs.dataset_names
        );
    }
}
