//
// backend.rs
//
// Copyright (C) 2022-2026 Posit Software, PBC. All rights reserved.
// Modifications copyright (C) 2026 Jonathan Marc Bearak
//

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};
use std::task::{Context, Poll};

use serde::Deserialize;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tower::Service;
use tower_lsp::Client;
use tower_lsp::LanguageServer;
use tower_lsp::LspService;
use tower_lsp::Server;
use tower_lsp::jsonrpc::{
    Error as JsonRpcError, Id as JsonRpcId, Request as JsonRpcRequest, Response as JsonRpcResponse,
    Result,
};
use tower_lsp::lsp_types::*;

use crate::handlers;
use crate::indentation;
use crate::state::{IndentationSettings, SymbolConfig, WorldState, scan_workspace};
use crate::utf16::utf16_column_to_byte_offset;
tokio::task_local! {
    static CURRENT_LSP_REQUEST_ID: JsonRpcId;
}

/// Category of files for on-demand indexing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexCategory {
    /// Files directly sourced by open documents
    Sourced,
    /// Files referenced by backward directives (`# raven: run-by`, `# raven: sourced-by`)
    BackwardDirective,
}
const DIAGNOSTIC_FANOUT_CONCURRENCY: usize = 8;

use crate::r_subprocess::is_valid_package_name;

/// Extract loaded packages from metadata-derived library calls.
fn extract_loaded_packages_from_library_calls(
    library_calls: &[crate::cross_file::LibraryCall],
) -> Vec<String> {
    let mut packages = Vec::new();
    for lib_call in library_calls {
        if is_valid_package_name(&lib_call.package) {
            packages.push(lib_call.package.clone());
        } else {
            log::warn!("Skipping suspicious package name: {}", lib_call.package);
        }
    }
    packages
}

/// Direct package warm set for a single edited document: its validated
/// `library()`/`require()` loads plus the packages named in its
/// `data(..., package = "pkg")` calls (issue #429). Warming the latter lets the
/// diagnostics-time `data()` alias expansion resolve those packages' dataset
/// objects on live edits — parity with did_open and `raven check`, which both
/// warm `doc.data_packages`. `data_packages` names are validated downstream by
/// the `is_valid_package_name` filter the caller applies to the merged
/// prefetch set (mirroring the did_open path). Extracted for unit testing.
fn edited_document_warm_packages(
    library_calls: &[crate::cross_file::LibraryCall],
    doc: &crate::state::Document,
) -> Vec<String> {
    let mut packages = extract_loaded_packages_from_library_calls(library_calls);
    packages.extend(doc.data_packages.iter().cloned());
    packages
}

/// Namespace-reference package names that pass the warm filter: a valid package
/// name that is not the load-all sentinel.
///
/// Single source of truth for "which `pkg::` packages may be warmed", shared by
/// every warm path (`namespace_warm_packages`, `collect_open_document_packages`)
/// because `prefetch_packages()` trusts its callers. Namespace references warm
/// metadata only — they never attach the package to bare-name scope.
fn valid_namespace_ref_packages(
    meta: &crate::cross_file::CrossFileMetadata,
) -> impl Iterator<Item = &str> {
    meta.namespace_references
        .iter()
        .map(|r| r.package.as_str())
        .filter(|p| is_valid_package_name(p) && !crate::package_library::is_load_all_sentinel(p))
}

/// Deduplicated package names worth warming from a document's namespace
/// references, in first-seen order.
fn namespace_warm_packages(meta: &crate::cross_file::CrossFileMetadata) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut packages = Vec::new();
    for p in valid_namespace_ref_packages(meta) {
        if seen.insert(p) {
            packages.push(p.to_string());
        }
    }
    packages
}

/// Parameters for the raven/activeDocumentsChanged notification
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveDocumentsChangedParams {
    active_uri: Option<String>,
    visible_uris: Vec<String>,
    timestamp_ms: u64,
}

/// One entry in the raven/documentIndentUnitsChanged notification.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentIndentUnit {
    uri: String,
    indent_unit: u32,
}

/// Parameters for the raven/documentIndentUnitsChanged notification.
///
/// The extension sends this whenever `raven.linting.indentationUnit` is
/// `"auto"` and any open R document's resolved `editor.tabSize` changes.
/// The map replaces the server's previous per-document overrides wholesale.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentIndentUnitsChangedParams {
    units: Vec<DocumentIndentUnit>,
}

/// Parameters for the raven/semanticTokensForRString custom request.
///
/// Lets the Knit Output webview pipeline (`editors/vscode/src/knit/...`)
/// fetch Raven's function-token classification for an arbitrary R code-block
/// body, without requiring the source Rmd to be open as an LSP document. The
/// returned tokens are encoded in the same LSP delta format as the
/// standard `textDocument/semanticTokens/full` response, and use the same
/// single-entry legend (`function`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SemanticTokensForRStringParams {
    /// The raw R source text to tokenize. Treated as a single document for
    /// parsing; multi-line input is supported. Line / column positions in
    /// the response are relative to this string.
    text: String,
}

fn normalize_document_indent_unit(unit: u32) -> u32 {
    unit.clamp(1, 8)
}

/// Parse cross-file configuration from LSP settings.
///
/// Reads the top-level `crossFile`, `diagnostics`, and `packages` sections from a
/// serde_json::Value and constructs a populated `CrossFileConfig`. Only fields
/// present in the provided JSON are applied; absent fields retain their defaults
/// from `CrossFileConfig::default()`.
///
/// Supported top-level keys read:
/// - `crossFile`: core cross-file behavior and diagnostic severities.
/// - `diagnostics.enabled` and `diagnostics.undefinedVariableSeverity`: diagnostics master switch and undefined variable diagnostic severity.
/// - `packages`: package-related settings (`enabled`, `additionalLibraryPaths`, `rPath`,
///   `missingPackageSeverity`, `watchLibraryPaths`, `watchDebounceMs`).
///
/// # Returns
///
/// `Ok(Some(CrossFileConfig))` populated from `settings` when at least one of
/// `crossFile`, `diagnostics`, or `packages` is present; `Ok(None)` if all are missing.
/// Returns `Err(...)` when a top-level section (`crossFile`, `diagnostics`,
/// or `packages`) is present but is not a JSON object.
///
/// # Examples
///
/// ```text
/// use serde_json::json;
/// let settings = json!({
///     "crossFile": {
///         "maxBackwardDepth": 5,
///         "indexWorkspace": true,
///         "missingFileSeverity": "warning",
///         "onDemandIndexing": { "enabled": true }
///     },
///     "packages": {
///         "enabled": true,
///         "additionalLibraryPaths": ["/usr/local/lib/R/site-library"],
///         "rPath": "/usr/bin/R",
///         "missingPackageSeverity": "information"
///     },
///     "diagnostics": { "enabled": true, "undefinedVariableSeverity": "warning" }
/// });
///
/// let cfg = raven::backend::parse_cross_file_config(&settings).unwrap();
/// assert!(cfg.is_some());
/// let cfg = cfg.unwrap();
/// assert_eq!(cfg.max_backward_depth, 5);
/// assert!(cfg.index_workspace);
/// assert!(cfg.packages_enabled);
/// assert!(cfg.diagnostics_enabled);
/// ```
pub(crate) fn parse_cross_file_config(
    settings: &serde_json::Value,
) -> std::result::Result<Option<crate::cross_file::CrossFileConfig>, String> {
    use crate::cross_file::{CallSiteDefault, CrossFileConfig};

    // crossFile section is optional - we can still parse diagnostics and packages without it
    let cross_file = settings.get("crossFile");
    let diagnostics = settings.get("diagnostics");
    let packages = settings.get("packages");
    // Return None only if no relevant settings are present at all
    if cross_file.is_none() && diagnostics.is_none() && packages.is_none() {
        return Ok(None);
    }

    // Validate that present sections are objects (not scalars/arrays)
    fn ensure_object_section(
        value: Option<&serde_json::Value>,
        name: &str,
    ) -> std::result::Result<(), String> {
        if let Some(v) = value
            && !v.is_object()
        {
            return Err(format!("{name} must be an object."));
        }
        Ok(())
    }
    ensure_object_section(cross_file, "crossFile")?;
    ensure_object_section(diagnostics, "diagnostics")?;
    ensure_object_section(packages, "packages")?;

    let mut config = CrossFileConfig::default();

    // Parse crossFile settings if present
    if let Some(cross_file) = cross_file {
        if let Some(v) = cross_file.get("maxBackwardDepth").and_then(|v| v.as_u64()) {
            config.max_backward_depth = v as usize;
        }
        if let Some(v) = cross_file.get("maxForwardDepth").and_then(|v| v.as_u64()) {
            config.max_forward_depth = v as usize;
        }
        if let Some(v) = cross_file.get("maxChainDepth").and_then(|v| v.as_u64()) {
            config.max_chain_depth = v as usize;
        }
        if let Some(v) = cross_file
            .get("maxTransitiveDependentsVisited")
            .and_then(|v| v.as_u64())
        {
            config.max_transitive_dependents_visited = v as usize;
        }
        if let Some(v) = cross_file.get("assumeCallSite").and_then(|v| v.as_str()) {
            config.assume_call_site = match v {
                "start" => CallSiteDefault::Start,
                _ => CallSiteDefault::End,
            };
        }
        if let Some(v) = cross_file.get("indexWorkspace").and_then(|v| v.as_bool()) {
            config.index_workspace = v;
        }
        if let Some(v) = cross_file
            .get("maxRevalidationsPerTrigger")
            .and_then(|v| v.as_u64())
        {
            config.max_revalidations_per_trigger = v as usize;
        }
        if let Some(v) = cross_file
            .get("revalidationDebounceMs")
            .and_then(|v| v.as_u64())
        {
            config.revalidation_debounce_ms = v;
        }
        if let Some(v) = cross_file
            .get("editedFileDebounceMs")
            .and_then(|v| v.as_u64())
        {
            config.edited_file_debounce_ms = v;
        }

        // Parse diagnostic severities
        if let Some(sev) = cross_file
            .get("missingFileSeverity")
            .and_then(|v| v.as_str())
        {
            config.missing_file_severity = parse_severity(sev);
        }
        if let Some(sev) = cross_file
            .get("caseMismatchSeverity")
            .and_then(|v| v.as_str())
        {
            config.case_mismatch_severity = parse_case_mismatch_severity(sev);
        }
        if let Some(sev) = cross_file
            .get("circularDependencySeverity")
            .and_then(|v| v.as_str())
        {
            config.circular_dependency_severity = parse_severity(sev);
        }
        if let Some(sev) = cross_file
            .get("outOfScopeSeverity")
            .and_then(|v| v.as_str())
        {
            config.out_of_scope_severity = parse_severity(sev);
        }
        if let Some(sev) = cross_file
            .get("maxChainDepthSeverity")
            .and_then(|v| v.as_str())
        {
            config.max_chain_depth_severity = parse_severity(sev);
        }
        // Parse redundant directive severity (Requirement 6.2)
        // Supports "off" to disable the diagnostic entirely
        if let Some(sev) = cross_file
            .get("redundantDirectiveSeverity")
            .and_then(|v| v.as_str())
        {
            config.redundant_directive_severity = parse_severity(sev);
        }

        if let Some(v) = cross_file
            .get("hoistGlobalsInFunctions")
            .and_then(|v| v.as_bool())
        {
            config.hoist_globals_in_functions = v;
        }

        if let Some(raw) = cross_file.get("backwardDependencies") {
            match raw.as_str() {
                Some("auto") => {
                    config.backward_dependencies = crate::cross_file::BackwardDependencyMode::Auto;
                }
                Some("explicit") => {
                    config.backward_dependencies =
                        crate::cross_file::BackwardDependencyMode::Explicit;
                }
                Some(other) => {
                    log::warn!(
                        "Unrecognized crossFile.backwardDependencies value '{other}', defaulting to 'auto'."
                    );
                    config.backward_dependencies = crate::cross_file::BackwardDependencyMode::Auto;
                }
                None => {
                    log::warn!(
                        "crossFile.backwardDependencies must be a string, defaulting to 'auto'."
                    );
                    config.backward_dependencies = crate::cross_file::BackwardDependencyMode::Auto;
                }
            }
        }

        // Parse on-demand indexing settings
        if let Some(on_demand) = cross_file.get("onDemandIndexing")
            && let Some(v) = on_demand.get("enabled").and_then(|v| v.as_bool())
        {
            config.on_demand_indexing_enabled = v;
        }

        // Parse cache settings
        if let Some(cache) = cross_file.get("cache") {
            if let Some(v) = cache.get("metadataMaxEntries").and_then(|v| v.as_u64()) {
                config.cache_metadata_max_entries = (v as usize).max(1);
            }
            if let Some(v) = cache.get("fileContentMaxEntries").and_then(|v| v.as_u64()) {
                config.cache_file_content_max_entries = (v as usize).max(1);
            }
            if let Some(v) = cache.get("existenceMaxEntries").and_then(|v| v.as_u64()) {
                config.cache_existence_max_entries = (v as usize).max(1);
            }
            if let Some(v) = cache
                .get("workspaceIndexMaxEntries")
                .and_then(|v| v.as_u64())
            {
                config.cache_workspace_index_max_entries = (v as usize).max(1);
            }
        }
    }

    // Parse diagnostics settings
    if let Some(diag) = diagnostics {
        // Parse diagnostics.enabled (master switch)
        if let Some(v) = diag.get("enabled").and_then(|v| v.as_bool()) {
            config.diagnostics_enabled = v;
        }
        // Parse diagnostics.undefinedVariableSeverity
        if let Some(sev) = diag
            .get("undefinedVariableSeverity")
            .and_then(|v| v.as_str())
        {
            config.undefined_variable_severity = parse_severity(sev);
        }
        // Parse diagnostics.undefinedVariableInCallArguments (issue #398)
        if let Some(v) = diag
            .get("undefinedVariableInCallArguments")
            .and_then(|v| v.as_bool())
        {
            config.undefined_variable_in_call_arguments = v;
        }
        // Parse diagnostics.undefinedVariableInBracketIndices (issue #398)
        if let Some(v) = diag
            .get("undefinedVariableInBracketIndices")
            .and_then(|v| v.as_bool())
        {
            config.undefined_variable_in_bracket_indices = v;
        }
        // Parse diagnostics.mixedLogicalSeverity
        if let Some(sev) = diag.get("mixedLogicalSeverity").and_then(|v| v.as_str()) {
            config.mixed_logical_severity = parse_severity(sev);
        }
        // Parse diagnostics.conditionAssignmentSeverity
        if let Some(sev) = diag
            .get("conditionAssignmentSeverity")
            .and_then(|v| v.as_str())
        {
            config.condition_assignment_severity = parse_severity(sev);
        }
        // Parse diagnostics.reportUnusedSuppressions (F2 Step 3)
        if let Some(v) = diag
            .get("reportUnusedSuppressions")
            .and_then(|v| v.as_bool())
        {
            config.report_unused_suppressions = v;
        }
    }

    // Parse package settings (Requirement 12, Task 14.2)
    if let Some(packages) = packages {
        if let Some(v) = packages.get("enabled").and_then(|v| v.as_bool()) {
            config.packages_enabled = v;
        }
        if let Some(paths) = packages
            .get("additionalLibraryPaths")
            .and_then(|v| v.as_array())
        {
            config.packages_additional_library_paths = paths
                .iter()
                .filter_map(|p| p.as_str())
                .filter(|s| !s.is_empty() && !s.contains('\0'))
                .map(std::path::PathBuf::from)
                .collect();
        }
        if let Some(v) = packages.get("rPath").and_then(|v| v.as_str())
            && !v.is_empty()
            && !v.contains('\0')
        {
            config.packages_r_path = Some(std::path::PathBuf::from(v));
        }
        if let Some(sev) = packages
            .get("missingPackageSeverity")
            .and_then(|v| v.as_str())
        {
            config.packages_missing_package_severity = parse_severity(sev);
        }
        if let Some(sev) = packages
            .get("namespaceMemberSeverity")
            .and_then(|v| v.as_str())
        {
            config.packages_namespace_member_severity = parse_severity(sev);
        }
        if let Some(v) = packages.get("watchLibraryPaths").and_then(|v| v.as_bool()) {
            config.packages_watch_library_paths = v;
        }
        if let Some(v) = packages.get("watchDebounceMs").and_then(|v| v.as_u64()) {
            config.packages_watch_debounce_ms = v.clamp(100, 5000);
        }
        if let Some(v) = packages.get("packageMode").and_then(|v| v.as_str()) {
            config.package_mode = match v {
                "enabled" => crate::cross_file::config::PackageMode::Enabled,
                "disabled" => crate::cross_file::config::PackageMode::Disabled,
                _ => crate::cross_file::config::PackageMode::Auto,
            };
        }
        if let Some(v) = packages.get("rprofilePrelude").and_then(|v| v.as_bool()) {
            config.model_rprofile = v;
        }
    }

    log::info!("Cross-file configuration loaded from LSP settings:");
    log::info!("  max_backward_depth: {}", config.max_backward_depth);
    log::info!("  max_forward_depth: {}", config.max_forward_depth);
    log::info!("  max_chain_depth: {}", config.max_chain_depth);
    log::info!("  assume_call_site: {:?}", config.assume_call_site);
    log::info!("  index_workspace: {}", config.index_workspace);
    log::info!(
        "  max_revalidations_per_trigger: {}",
        config.max_revalidations_per_trigger
    );
    log::info!(
        "  revalidation_debounce_ms: {}",
        config.revalidation_debounce_ms
    );
    log::info!("  diagnostics_enabled: {}", config.diagnostics_enabled);
    log::info!(
        "  hoist_globals_in_functions: {}",
        config.hoist_globals_in_functions
    );
    log::info!(
        "  backward_dependencies: {:?}",
        config.backward_dependencies
    );
    log::info!("  On-demand indexing:");
    log::info!("    enabled: {}", config.on_demand_indexing_enabled);
    log::info!("  Diagnostic severities:");
    log::info!(
        "    undefined_variable: {:?}",
        config.undefined_variable_severity
    );
    log::info!("    missing_file: {:?}", config.missing_file_severity);
    log::info!("    case_mismatch: {:?}", config.case_mismatch_severity);
    log::info!(
        "    circular_dependency: {:?}",
        config.circular_dependency_severity
    );
    log::info!("    out_of_scope: {:?}", config.out_of_scope_severity);
    log::info!("    max_chain_depth: {:?}", config.max_chain_depth_severity);
    log::info!(
        "    redundant_directive: {:?}",
        config.redundant_directive_severity
    );
    log::info!("  Package settings:");
    log::info!("    enabled: {}", config.packages_enabled);
    log::info!(
        "    additional_library_paths: {:?}",
        config.packages_additional_library_paths
    );
    log::info!("    r_path: {:?}", config.packages_r_path);
    log::info!(
        "    missing_package_severity: {:?}",
        config.packages_missing_package_severity
    );
    log::info!("  Cache settings (LRU):");
    log::info!(
        "    metadata_max_entries: {}",
        config.cache_metadata_max_entries
    );
    log::info!(
        "    file_content_max_entries: {}",
        config.cache_file_content_max_entries
    );
    log::info!(
        "    existence_max_entries: {}",
        config.cache_existence_max_entries
    );
    log::info!(
        "    workspace_index_max_entries: {}",
        config.cache_workspace_index_max_entries
    );
    Ok(Some(config))
}

/// Variant of [`parse_lint_config`] that takes the `[linting]` section
/// directly (not wrapped in a top-level object). Used by per-document
/// override resolution where the section has already been extracted; the
/// override inherits `base_enabled` instead of re-resolving `Auto` from
/// discovery state. See spec section 6.
pub(crate) fn parse_lint_config_from_section(
    section: &serde_json::Value,
    base_enabled: bool,
) -> Option<crate::linting::LintConfig> {
    let wrapped = serde_json::json!({ "linting": section });
    parse_lint_config(&wrapped, base_enabled)
}

fn parse_lint_enabled(raw: Option<&serde_json::Value>) -> crate::linting::LintEnabled {
    use crate::linting::LintEnabled;
    use serde_json::Value;
    match raw {
        // Absent and explicit JSON null are semantically equivalent
        // ("no preference") and remain silent.
        None | Some(Value::Null) => LintEnabled::Auto,
        Some(Value::Bool(true)) => LintEnabled::On,
        Some(Value::Bool(false)) => LintEnabled::Off,
        Some(Value::String(s)) => match s.as_str() {
            "auto" => LintEnabled::Auto,
            "on" | "true" => LintEnabled::On,
            "off" | "false" => LintEnabled::Off,
            other => {
                log::warn!("Unrecognised linting.enabled value '{other}'; defaulting to 'auto'.");
                LintEnabled::Auto
            }
        },
        Some(other) => {
            let kind = match other {
                Value::Number(_) => "number",
                Value::Array(_) => "array",
                Value::Object(_) => "object",
                _ => "value",
            };
            log::warn!(
                "linting.enabled must be boolean or string \"auto|on|off\"; got {kind}. Defaulting to 'auto'."
            );
            LintEnabled::Auto
        }
    }
}

async fn run_bounded_fanout<T, MakeFuture, FutureOutput>(
    items: Vec<T>,
    limit: usize,
    make_future: MakeFuture,
) where
    T: Send + 'static,
    MakeFuture: Fn(T) -> FutureOutput + Send + Sync + 'static,
    FutureOutput: Future<Output = ()> + Send + 'static,
{
    if items.is_empty() {
        return;
    }

    let semaphore = Arc::new(tokio::sync::Semaphore::new(limit.max(1)));
    let make_future = Arc::new(make_future);
    let mut join_set = tokio::task::JoinSet::new();

    for item in items {
        let permit = match Arc::clone(&semaphore).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        let make_future = Arc::clone(&make_future);
        join_set.spawn(async move {
            let _permit = permit;
            make_future(item).await;
        });
    }

    while let Some(result) = join_set.join_next().await {
        if let Err(err) = result {
            log::trace!("bounded diagnostic fan-out task failed: {err}");
        }
    }
}

#[cfg(feature = "test-support")]
pub async fn run_bounded_fanout_for_test<T, MakeFuture, FutureOutput>(
    items: Vec<T>,
    limit: usize,
    make_future: MakeFuture,
) where
    T: Send + 'static,
    MakeFuture: Fn(T) -> FutureOutput + Send + Sync + 'static,
    FutureOutput: Future<Output = ()> + Send + 'static,
{
    run_bounded_fanout(items, limit, make_future).await;
}
/// Parse linting configuration from merged client + project settings.
///
/// Reads the `linting` section and constructs a [`crate::linting::config::LintConfig`]. The
/// `enabled` field is tri-state (see [`crate::linting::LintEnabled`]):
/// `Auto` (the default) resolves to `lintr_discovered`; `On` and `Off`
/// always win. When `linting` is absent or non-object, returns
/// `Some(LintConfig::default())` with `enabled = lintr_discovered` if a
/// `.lintr` was the discovered project config, and `None` otherwise (so
/// callers can fall back to defaults without losing the "section never seen"
/// signal).
///
/// This function does not decide *whether* a discovered `.lintr` opts in —
/// callers compute `lintr_discovered`, and the rule "a content-free `.lintr`
/// does not auto-enable" is enforced upstream by
/// [`crate::config_file::lintr_expresses_linting`] (and the `raven lint`
/// equivalent). A configured `.lintr`, including a bare `linters_with_defaults()`,
/// always carries a `linting` object by the time it reaches here.
///
/// Recognised keys:
/// * `enabled` (`"auto"` / `"on"` / `"off"` / `true` / `false`) — master switch.
/// * `lineLength` (number) — max line length; clamped to `[20, 10_000]`.
/// * `objectLength` (number) — max identifier length; clamped to `[5, 1_000]`.
/// * `indentationUnit` (number) — spaces per indent level for the
///   indentation lint; clamped to `[1, 8]`.
/// * `assignmentOperator` (`"<-"` or `"="`) — preferred operator.
/// * `stringDelimiter` (`"\""` or `"'"`) — preferred string delimiter.
/// * `objectNameStyleFunction`, `objectNameStyleVariable`,
///   `objectNameStyleArgument` (string, one of `"snake_case" | "camelCase" |
///   "dotted.case" | "UPPER_CASE" | "lowercase" | "any"`) — naming scheme
///   for each symbol kind. `"any"` disables that kind without disabling the
///   rule entirely.
/// * Per-rule severities (string, `"error" | "warning" | "information" |
///   "hint" | "off"`):
///   - `lineLengthSeverity`
///   - `trailingWhitespaceSeverity`
///   - `noTabSeverity`
///   - `trailingBlankLinesSeverity`
///   - `assignmentOperatorSeverity`
///   - `objectNameSeverity`
///   - `infixSpacesSeverity`
///   - `commentedCodeSeverity`
///   - `quotesSeverity`
///   - `commasSeverity`
///   - `tAndFSymbolSeverity`
///   - `semicolonSeverity`
///   - `equalsNaSeverity`
///   - `objectLengthSeverity`
///   - `vectorLogicSeverity`
///   - `functionLeftParenthesesSeverity`
///   - `spacesInsideSeverity`
///   - `indentationSeverity`
pub(crate) fn parse_lint_config(
    settings: &serde_json::Value,
    lintr_discovered: bool,
) -> Option<crate::linting::LintConfig> {
    use crate::linting::LintEnabled;

    let linting = settings.get("linting");
    let linting_obj = match linting {
        Some(v) if v.is_object() => Some(v),
        Some(_) => {
            log::warn!("linting settings must be an object; ignoring.");
            // When `.lintr` was discovered, still return a defaulted config so
            // Auto resolves to on; otherwise behave as before and return None.
            if !lintr_discovered {
                return None;
            }
            None
        }
        None => {
            if !lintr_discovered {
                return None;
            }
            None
        }
    };

    let mut config = crate::linting::LintConfig::default();

    let raw_enabled = linting_obj.and_then(|l| l.get("enabled"));
    config.enabled = match parse_lint_enabled(raw_enabled) {
        LintEnabled::On => true,
        LintEnabled::Off => false,
        LintEnabled::Auto => lintr_discovered,
    };

    // Re-bind `linting` for the rest of the function below to keep the
    // existing field-parsing code unchanged. If we have no linting object
    // (synthesised default for .lintr), use an empty object so `.get(...)`
    // calls below uniformly return None.
    let empty = serde_json::Value::Object(serde_json::Map::new());
    let linting = linting_obj.unwrap_or(&empty);

    if let Some(v) = linting.get("lineLength").and_then(|v| v.as_u64()) {
        // Clamp on u64 first; casting to u32 before clamping would wrap values
        // above u32::MAX (e.g. u32::MAX + 5 becomes 4) into a small value and
        // then clamp to the floor of 20 — silently bogus. The clamp ceiling
        // is well below u32::MAX so the post-clamp cast is lossless.
        config.line_length = v.clamp(20, 10_000) as u32;
    }
    if let Some(v) = linting.get("objectLength").and_then(|v| v.as_u64()) {
        // Same clamp-first pattern as `lineLength`. The floor of 5 keeps
        // single-letter conventions usable; the ceiling is well below u32::MAX.
        config.object_length = v.clamp(5, 1_000) as u32;
    }
    if let Some(v) = linting.get("indentationUnit").and_then(|v| v.as_u64()) {
        // Same clamp-first pattern. Floors at 1 because zero-space indents
        // wouldn't be visually distinguishable; ceiling matches the `tab_size`
        // bound used by the on-type indentation provider.
        config.indentation_unit = v.clamp(1, 8) as u32;
    }
    if let Some(op) = linting.get("assignmentOperator").and_then(|v| v.as_str()) {
        config.assignment_operator_style = match op {
            "=" => crate::linting::AssignmentOperatorStyle::Equals,
            "<-" => crate::linting::AssignmentOperatorStyle::LeftArrow,
            other => {
                log::warn!(
                    "Unrecognised linting.assignmentOperator '{other}', defaulting to '<-'."
                );
                crate::linting::AssignmentOperatorStyle::LeftArrow
            }
        };
    }
    if let Some(d) = linting.get("stringDelimiter").and_then(|v| v.as_str()) {
        config.string_delimiter = match d {
            "\"" => crate::linting::StringDelimiter::Double,
            "'" => crate::linting::StringDelimiter::Single,
            other => {
                log::warn!("Unrecognised linting.stringDelimiter '{other}', defaulting to '\"'.");
                crate::linting::StringDelimiter::Double
            }
        };
    }
    if let Some(sev) = linting.get("lineLengthSeverity").and_then(|v| v.as_str()) {
        config.line_length_severity = parse_severity(sev);
    }
    if let Some(sev) = linting
        .get("trailingWhitespaceSeverity")
        .and_then(|v| v.as_str())
    {
        config.trailing_whitespace_severity = parse_severity(sev);
    }
    if let Some(sev) = linting.get("noTabSeverity").and_then(|v| v.as_str()) {
        config.no_tab_severity = parse_severity(sev);
    }
    if let Some(sev) = linting
        .get("trailingBlankLinesSeverity")
        .and_then(|v| v.as_str())
    {
        config.trailing_blank_lines_severity = parse_severity(sev);
    }
    if let Some(sev) = linting
        .get("assignmentOperatorSeverity")
        .and_then(|v| v.as_str())
    {
        config.assignment_operator_severity = parse_severity(sev);
    }
    if let Some(style) = linting
        .get("objectNameStyleFunction")
        .and_then(|v| v.as_str())
    {
        config.object_name_style_function =
            parse_object_name_style(style, "objectNameStyleFunction");
    }
    if let Some(style) = linting
        .get("objectNameStyleVariable")
        .and_then(|v| v.as_str())
    {
        config.object_name_style_variable =
            parse_object_name_style(style, "objectNameStyleVariable");
    }
    if let Some(style) = linting
        .get("objectNameStyleArgument")
        .and_then(|v| v.as_str())
    {
        config.object_name_style_argument =
            parse_object_name_style(style, "objectNameStyleArgument");
    }
    if let Some(sev) = linting.get("objectNameSeverity").and_then(|v| v.as_str()) {
        config.object_name_severity = parse_severity(sev);
    }
    if let Some(sev) = linting.get("infixSpacesSeverity").and_then(|v| v.as_str()) {
        config.infix_spaces_severity = parse_severity(sev);
    }
    if let Some(sev) = linting
        .get("commentedCodeSeverity")
        .and_then(|v| v.as_str())
    {
        config.commented_code_severity = parse_severity(sev);
    }
    if let Some(sev) = linting.get("quotesSeverity").and_then(|v| v.as_str()) {
        config.quotes_severity = parse_severity(sev);
    }
    if let Some(sev) = linting.get("commasSeverity").and_then(|v| v.as_str()) {
        config.commas_severity = parse_severity(sev);
    }
    if let Some(sev) = linting.get("tAndFSymbolSeverity").and_then(|v| v.as_str()) {
        config.t_and_f_symbol_severity = parse_severity(sev);
    }
    if let Some(sev) = linting.get("semicolonSeverity").and_then(|v| v.as_str()) {
        config.semicolon_severity = parse_severity(sev);
    }
    if let Some(sev) = linting.get("equalsNaSeverity").and_then(|v| v.as_str()) {
        config.equals_na_severity = parse_severity(sev);
    }
    if let Some(sev) = linting.get("objectLengthSeverity").and_then(|v| v.as_str()) {
        config.object_length_severity = parse_severity(sev);
    }
    if let Some(sev) = linting.get("vectorLogicSeverity").and_then(|v| v.as_str()) {
        config.vector_logic_severity = parse_severity(sev);
    }
    if let Some(sev) = linting
        .get("functionLeftParenthesesSeverity")
        .and_then(|v| v.as_str())
    {
        config.function_left_parentheses_severity = parse_severity(sev);
    }
    if let Some(sev) = linting.get("spacesInsideSeverity").and_then(|v| v.as_str()) {
        config.spaces_inside_severity = parse_severity(sev);
    }
    if let Some(sev) = linting.get("indentationSeverity").and_then(|v| v.as_str()) {
        config.indentation_severity = parse_severity(sev);
    }

    log::info!("Linting configuration loaded from LSP settings:");
    log::info!("  enabled: {}", config.enabled);
    log::info!("  line_length: {}", config.line_length);
    log::info!("  object_length: {}", config.object_length);
    log::info!(
        "  assignment_operator_style: {:?}",
        config.assignment_operator_style
    );
    log::info!("  string_delimiter: {:?}", config.string_delimiter);
    log::info!(
        "  severities: line={:?} ws={:?} tab={:?} blank={:?} assign={:?} obj_name={:?} infix_spaces={:?} commented_code={:?}",
        config.line_length_severity,
        config.trailing_whitespace_severity,
        config.no_tab_severity,
        config.trailing_blank_lines_severity,
        config.assignment_operator_severity,
        config.object_name_severity,
        config.infix_spaces_severity,
        config.commented_code_severity
    );
    log::info!(
        "  severities: quotes={:?} commas={:?} t_and_f={:?} semicolon={:?} equals_na={:?} object_length={:?} vector_logic={:?} function_left_paren={:?} spaces_inside={:?} indentation={:?}",
        config.quotes_severity,
        config.commas_severity,
        config.t_and_f_symbol_severity,
        config.semicolon_severity,
        config.equals_na_severity,
        config.object_length_severity,
        config.vector_logic_severity,
        config.function_left_parentheses_severity,
        config.spaces_inside_severity,
        config.indentation_severity,
    );
    log::info!("  indentation_unit: {}", config.indentation_unit);
    log::info!(
        "  object_name styles: fn={:?} var={:?} arg={:?}",
        config.object_name_style_function,
        config.object_name_style_variable,
        config.object_name_style_argument,
    );

    Some(config)
}

/// Parse indentation configuration from LSP settings.
///
/// Reads the `indentation` section from a serde_json::Value and constructs a
/// populated `IndentationSettings`. Only fields present in the provided JSON
/// are applied; absent fields retain their defaults.
pub(crate) fn parse_indentation_config(
    settings: &serde_json::Value,
) -> Option<IndentationSettings> {
    let indentation = settings.get("indentation")?;

    let mut config = IndentationSettings::default();

    if let Some(style_str) = indentation.get("style").and_then(|v| v.as_str()) {
        config.style = match style_str.to_lowercase().as_str() {
            "rstudio" => crate::indentation::IndentationStyle::RStudio,
            "rstudio-minus" => crate::indentation::IndentationStyle::RStudioMinus,
            "off" => crate::indentation::IndentationStyle::Off,
            _ => {
                log::warn!(
                    "Invalid indentation.style: {}, defaulting to rstudio",
                    style_str
                );
                crate::indentation::IndentationStyle::RStudio
            }
        };
    }

    Some(config)
}

/// Parse a severity string into an optional `DiagnosticSeverity`.
///
/// Returns `None` for "off", which disables the diagnostic entirely.
/// Returns the corresponding severity for "error", "warning", "information"/"info", "hint".
/// Unrecognised values default to `Some(WARNING)`.
pub(crate) fn parse_severity(s: &str) -> Option<DiagnosticSeverity> {
    match s.to_lowercase().as_str() {
        "off" => None,
        "error" => Some(DiagnosticSeverity::ERROR),
        "warning" => Some(DiagnosticSeverity::WARNING),
        "information" | "info" => Some(DiagnosticSeverity::INFORMATION),
        "hint" => Some(DiagnosticSeverity::HINT),
        _ => Some(DiagnosticSeverity::WARNING),
    }
}

/// Parse the `crossFile.caseMismatchSeverity` setting (issue #530). Unlike the
/// plain severities, this carries an `"auto"` sentinel (the host-derived
/// default) distinct from a pinned level and from `"off"`. An unrecognised
/// value falls back to `Auto`.
pub(crate) fn parse_case_mismatch_severity(s: &str) -> crate::cross_file::CaseMismatchSeverity {
    use crate::cross_file::CaseMismatchSeverity;
    match s.to_lowercase().as_str() {
        "auto" => CaseMismatchSeverity::Auto,
        "off" => CaseMismatchSeverity::Off,
        "error" => CaseMismatchSeverity::Fixed(DiagnosticSeverity::ERROR),
        "warning" => CaseMismatchSeverity::Fixed(DiagnosticSeverity::WARNING),
        "information" | "info" => CaseMismatchSeverity::Fixed(DiagnosticSeverity::INFORMATION),
        "hint" => CaseMismatchSeverity::Fixed(DiagnosticSeverity::HINT),
        _ => CaseMismatchSeverity::Auto,
    }
}

#[cfg(test)]
mod case_mismatch_severity_parse_tests {
    use super::parse_case_mismatch_severity;
    use crate::cross_file::CaseMismatchSeverity;
    use tower_lsp::lsp_types::DiagnosticSeverity;

    #[test]
    fn parses_auto_off_and_pinned_levels() {
        assert_eq!(
            parse_case_mismatch_severity("auto"),
            CaseMismatchSeverity::Auto
        );
        assert_eq!(
            parse_case_mismatch_severity("off"),
            CaseMismatchSeverity::Off
        );
        assert_eq!(
            parse_case_mismatch_severity("warning"),
            CaseMismatchSeverity::Fixed(DiagnosticSeverity::WARNING)
        );
        assert_eq!(
            parse_case_mismatch_severity("ERROR"),
            CaseMismatchSeverity::Fixed(DiagnosticSeverity::ERROR)
        );
        assert_eq!(
            parse_case_mismatch_severity("info"),
            CaseMismatchSeverity::Fixed(DiagnosticSeverity::INFORMATION)
        );
        // Unrecognised → Auto (host-derived default), not a surprising level.
        assert_eq!(
            parse_case_mismatch_severity("bogus"),
            CaseMismatchSeverity::Auto
        );
    }
}

/// Parse an [`ObjectNameStyle`](crate::linting::ObjectNameStyle) string.
///
/// Returns [`crate::linting::config::ObjectNameStyle::Any`] (the "disabled" sentinel for an individual
/// kind) on unrecognised values so a misconfigured setting falls back to "no
/// check" rather than a surprising default. `setting_name` is included in the
/// warning so the user can find the offending setting in their config.
fn parse_object_name_style(value: &str, setting_name: &str) -> crate::linting::ObjectNameStyle {
    use crate::linting::ObjectNameStyle;
    ObjectNameStyle::from_config_name(value).unwrap_or_else(|| {
        log::warn!(
            "Unrecognised linting.{setting_name} '{value}', disabling this kind (treating as 'any')."
        );
        ObjectNameStyle::Any
    })
}

/// Parse symbol provider configuration from LSP settings.
///
/// Reads the `symbols` section from a serde_json::Value and constructs a
/// populated `SymbolConfig`. Only fields present in the provided JSON are
/// applied; absent fields retain their defaults from `SymbolConfig::default()`.
///
/// Supported settings:
/// - `symbols.workspaceMaxResults`: Maximum number of workspace symbol results (100-10000)
///
/// # Returns
///
/// `Some(SymbolConfig)` populated from `settings` when the `symbols` section is present;
/// `None` if the section is missing.
///
/// # Examples
///
/// ```text
/// use serde_json::json;
/// let settings = json!({
///     "symbols": {
///         "workspaceMaxResults": 500
///     }
/// });
///
/// let cfg = crate::backend::parse_symbol_config(&settings);
/// assert!(cfg.is_some());
/// let cfg = cfg.unwrap();
/// assert_eq!(cfg.workspace_max_results, 500);
/// ```
///
/// # Requirements
///
/// - **11.1**: Default value of 1000 for workspace_max_results
/// - **11.2**: Configurable via `symbols.workspaceMaxResults` initialization option
/// - **11.3**: Valid range 100-10000 with clamping
pub(crate) fn parse_symbol_config(settings: &serde_json::Value) -> Option<SymbolConfig> {
    let symbols = settings.get("symbols")?;

    let mut config = SymbolConfig::default();

    // Parse symbols.workspaceMaxResults
    // Requirement 11.2: Configurable via symbols.workspaceMaxResults
    // Requirement 11.3: Valid range 100-10000 with clamping
    if let Some(v) = symbols.get("workspaceMaxResults") {
        if let Some(n) = v.as_u64() {
            config = SymbolConfig::with_max_results(n as usize);
        } else {
            log::warn!(
                "Invalid type for symbols.workspaceMaxResults: expected number, got {}; using default",
                v
            );
        }
    }

    log::info!("Symbol configuration loaded from LSP settings:");
    log::info!("  workspace_max_results: {}", config.workspace_max_results);

    Some(config)
}

/// Parse completion provider configuration from LSP settings.
///
/// Reads the `completion` section from a serde_json::Value and constructs a
/// populated `CompletionConfig`. Only fields present in the provided JSON are
/// applied; absent fields retain their defaults from `CompletionConfig::default()`.
///
/// # Returns
///
/// `Some(CompletionConfig)` populated from `settings` when the `completion` section
/// is present; `None` if the section is missing.
pub(crate) fn parse_completion_config(
    settings: &serde_json::Value,
) -> Option<crate::state::CompletionConfig> {
    let completion = settings.get("completion")?;

    let mut config = crate::state::CompletionConfig::default();

    if let Some(v) = completion
        .get("triggerOnOpenParen")
        .and_then(|v| v.as_bool())
    {
        config.trigger_on_open_paren = v;
    }

    log::info!("Completion configuration loaded from LSP settings:");
    log::info!("  trigger_on_open_paren: {}", config.trigger_on_open_paren);

    Some(config)
}

/// Build the list of completion trigger characters, conditionally including `(`.
fn build_completion_trigger_chars(trigger_on_open_paren: bool) -> Vec<String> {
    let mut chars = vec![
        String::from(":"),
        String::from("$"),
        String::from("@"),
        String::from("/"),
        String::from("\""),
        String::from("'"),
    ];
    if trigger_on_open_paren {
        chars.push(String::from("("));
    }
    chars
}

fn semantic_tokens_capability() -> SemanticTokensServerCapabilities {
    SemanticTokensOptions {
        work_done_progress_options: WorkDoneProgressOptions::default(),
        legend: SemanticTokensLegend {
            token_types: vec![SemanticTokenType::FUNCTION],
            token_modifiers: vec![],
        },
        range: None,
        full: Some(SemanticTokensFullOptions::Bool(true)),
    }
    .into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancellableRequestKind {
    GotoDefinition,
}

impl CancellableRequestKind {
    fn from_method(method: &str) -> Option<Self> {
        match method {
            "textDocument/definition" => Some(Self::GotoDefinition),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct ActiveCancellableRequest {
    kind: CancellableRequestKind,
    token: CancellationToken,
}

/// Request-scoped cooperative cancellation registry for interactive LSP work.
///
/// `tower-lsp` only runs the built-in `$/cancelRequest` handler when that
/// notification's future is polled. Raven keeps `Server::concurrency_level(1)`
/// to preserve ordered text-sync handling, so cancellation notifications can sit
/// behind an in-flight request. `RequestCancellationService` therefore updates
/// this registry synchronously in `Service::call`, before tower-lsp queues the
/// notification future, and request handlers poll the matching token while doing
/// CPU-bound work under the `WorldState` read guard.
#[derive(Debug, Default)]
struct RequestCancellationRegistry {
    active: StdRwLock<HashMap<JsonRpcId, ActiveCancellableRequest>>,
}

impl RequestCancellationRegistry {
    fn new() -> Self {
        Self::default()
    }

    fn register(&self, id: JsonRpcId, kind: CancellableRequestKind) -> CancellationToken {
        let token = CancellationToken::new();
        let mut active = self.active.write().unwrap();

        // Treat a newer request of the same interactive kind as superseding
        // older ones. Editors usually send `$/cancelRequest`, but this keeps
        // cursor-move storms responsive even when they do not.
        for (active_id, request) in active.iter() {
            if *active_id != id && request.kind == kind {
                request.token.cancel();
            }
        }

        if let Some(previous) = active.insert(
            id,
            ActiveCancellableRequest {
                kind,
                token: token.clone(),
            },
        ) {
            previous.token.cancel();
        }

        token
    }

    fn cancel(&self, id: &JsonRpcId) {
        let active = self.active.read().unwrap();
        if let Some(request) = active.get(id) {
            request.token.cancel();
        }
    }

    fn complete(&self, id: &JsonRpcId) {
        let mut active = self.active.write().unwrap();
        active.remove(id);
    }

    fn token_for_current_request(
        &self,
        kind: CancellableRequestKind,
    ) -> Option<handlers::DiagCancelToken> {
        CURRENT_LSP_REQUEST_ID
            .try_with(|id| self.token_for_id(id, kind))
            .ok()
            .flatten()
            .map(handlers::DiagCancelToken::from_token)
    }

    fn token_for_id(
        &self,
        id: &JsonRpcId,
        kind: CancellableRequestKind,
    ) -> Option<CancellationToken> {
        let active = self.active.read().unwrap();
        let request = active.get(id)?;
        (request.kind == kind).then(|| request.token.clone())
    }
}

type RequestCancellationFuture<E> =
    Pin<Box<dyn Future<Output = std::result::Result<Option<JsonRpcResponse>, E>> + Send + 'static>>;

struct RequestCancellationService<S> {
    inner: S,
    registry: Arc<RequestCancellationRegistry>,
}

impl<S> RequestCancellationService<S> {
    fn new(inner: S, registry: Arc<RequestCancellationRegistry>) -> Self {
        Self { inner, registry }
    }

    fn cancel_from_params(&self, params: Option<&serde_json::Value>) {
        let Some(params) = params else {
            log::trace!("Received $/cancelRequest without params");
            return;
        };
        match serde_json::from_value::<CancelParams>(params.clone()) {
            Ok(params) => self.registry.cancel(&JsonRpcId::from(params.id)),
            Err(err) => log::trace!("Ignoring malformed $/cancelRequest params: {err}"),
        }
    }
}

impl<S> Service<JsonRpcRequest> for RequestCancellationService<S>
where
    S: Service<JsonRpcRequest, Response = Option<JsonRpcResponse>> + Send + 'static,
    S::Error: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Option<JsonRpcResponse>;
    type Error = S::Error;
    type Future = RequestCancellationFuture<S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: JsonRpcRequest) -> Self::Future {
        if req.method() == "$/cancelRequest" {
            self.cancel_from_params(req.params());
        }

        let kind = CancellableRequestKind::from_method(req.method());
        let id = req.id().cloned();

        if let (Some(kind), Some(id)) = (kind, id) {
            self.registry.register(id.clone(), kind);
            let fut = self.inner.call(req);
            let registry = Arc::clone(&self.registry);
            let scope_id = id.clone();

            Box::pin(CURRENT_LSP_REQUEST_ID.scope(scope_id, async move {
                let result = fut.await;
                registry.complete(&id);
                result
            }))
        } else {
            Box::pin(self.inner.call(req))
        }
    }
}

pub struct Backend {
    client: Client,
    state: Arc<RwLock<WorldState>>,
    request_cancellation: Arc<RequestCancellationRegistry>,
    traversal_truncation: Arc<TraversalTruncationState>,
    #[cfg(test)]
    test_home_dir: std::sync::Mutex<Option<std::path::PathBuf>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TraversalTruncationDelta {
    visited_budget: u64,
    depth: u64,
}

impl TraversalTruncationDelta {
    fn total(&self) -> u64 {
        self.visited_budget.saturating_add(self.depth)
    }
}

#[derive(Debug, Default)]
struct TraversalTruncationState {
    visited_budget_baseline: AtomicU64,
    depth_baseline: AtomicU64,
    visited_budget_notified: AtomicBool,
}

impl TraversalTruncationState {
    fn consume_delta(
        &self,
        visited_budget_total: u64,
        depth_total: u64,
    ) -> TraversalTruncationDelta {
        let visited_budget_previous = self
            .visited_budget_baseline
            .swap(visited_budget_total, Ordering::AcqRel);
        let depth_previous = self.depth_baseline.swap(depth_total, Ordering::AcqRel);

        TraversalTruncationDelta {
            visited_budget: visited_budget_total.saturating_sub(visited_budget_previous),
            depth: depth_total.saturating_sub(depth_previous),
        }
    }

    fn should_show_visited_budget_notice(&self) -> bool {
        !self.visited_budget_notified.swap(true, Ordering::AcqRel)
    }

    fn reset_notice_throttle(&self) {
        self.visited_budget_notified.store(false, Ordering::Release);
    }
}

impl Backend {
    /// Surface present-but-unusable package-DB load notes (e.g. a
    /// `.raven/packages.json` from a newer Raven, or a corrupt/incompatible
    /// `names.db`) to the editor as warnings. Single source for how these
    /// build-time notes reach the user, called from both package-library
    /// init paths.
    async fn surface_load_notes(&self, notes: &[String]) {
        for note in notes {
            self.client.show_message(MessageType::WARNING, note).await;
        }
    }

    async fn ensure_package_library_initialized(&self) -> bool {
        let (enabled, already_ready) = {
            let state = self.state.read().await;
            (
                state.cross_file_config.packages_enabled,
                state.package_library_ready,
            )
        };

        // Disabled-preserve gate (load-bearing — do NOT collapse into a
        // `build_package_library(..., enabled)` call). When packages are
        // disabled this returns without touching `state.package_library`, so a
        // library the user built before disabling is left intact. Routing
        // through the helper instead would build `new_empty()` and, under the
        // race re-check below, could swap it in — clobbering that library. This
        // is the same hazard the `raven.refreshPackages` early-return guards
        // against; the two disabled-preserve gates are intentionally kept out
        // of the shared helper for this reason. (The helper still owns the
        // *install-empty-when-disabled* policy for the rebuild/startup sites.)
        if !enabled {
            return false;
        }
        if already_ready {
            return true;
        }

        let (packages_r_path, additional_paths, workspace_root) = {
            let state = self.state.read().await;
            (
                state.cross_file_config.packages_r_path.clone(),
                state
                    .cross_file_config
                    .packages_additional_library_paths
                    .clone(),
                state
                    .workspace_folders
                    .first()
                    .and_then(|url| url.to_file_path().ok()),
            )
        };

        log::trace!("Initializing PackageLibrary on demand (not yet ready)");
        // We only get here with packages enabled (early-returned above), so pass
        // `true`. The shared helper runs R discovery in spawn_blocking and
        // computes readiness after applying additional paths.
        let outcome = crate::package_library::build_package_library(
            packages_r_path,
            &additional_paths,
            workspace_root,
            true,
        )
        .await;
        if let crate::package_library::PackageLibraryStatus::InitFailed(e) = &outcome.status {
            log::warn!("Failed to initialize PackageLibrary: {}", e);
        }
        let ready = outcome.consumer_ready();
        let load_notes = outcome.load_notes;
        let library = outcome.library;
        // Re-check under the write lock: `initialized()` may have raced ahead
        // and already written a library with prefetched caches.
        let (committed, final_ready) = {
            let mut state = self.state.write().await;
            if !state.package_library_ready {
                state.package_library = library;
                // The fresh library starts with no local-dev overlay; re-install
                // it so load_all sentinel resolution survives the swap.
                state.refresh_local_dev_overlay();
                state.package_library_ready = ready;
                state.bump_package_config_generation();
                (true, ready)
            } else {
                log::trace!("PackageLibrary already initialized (race), keeping existing");
                (false, state.package_library_ready)
            }
        };
        // Surface Tier 2/3 load notes only on the path that actually committed
        // the library, so a startup race doesn't toast the same notes twice.
        // Done after dropping the write lock — `surface_load_notes` awaits.
        if committed {
            self.surface_load_notes(&load_notes).await;
        }
        final_ready
    }

    async fn check_and_warn_traversal_truncation(&self) {
        check_and_warn_traversal_truncation(
            &self.state,
            &self.client,
            self.traversal_truncation.as_ref(),
        )
        .await;
    }

    pub fn new(client: Client) -> Self {
        Self::new_with_request_cancellation(client, Arc::new(RequestCancellationRegistry::new()))
    }

    fn new_with_request_cancellation(
        client: Client,
        request_cancellation: Arc<RequestCancellationRegistry>,
    ) -> Self {
        let state = Arc::new(RwLock::new(WorldState::new()));

        Self {
            client,
            state,
            request_cancellation,
            traversal_truncation: Arc::new(TraversalTruncationState::default()),
            #[cfg(test)]
            test_home_dir: std::sync::Mutex::new(None),
        }
    }

    fn discovery_options_from_client_settings(
        &self,
        raw_client: &serde_json::Value,
    ) -> crate::config_file::DiscoveryOptions {
        let options = crate::config_file::DiscoveryOptions::from_client_settings(raw_client);
        #[cfg(test)]
        {
            if let Some(home_dir) = self.test_home_dir.lock().unwrap().clone() {
                return options.with_home_dir(&home_dir);
            }
        }
        options
    }

    #[cfg(test)]
    fn set_test_home_dir(&self, home_dir: &std::path::Path) {
        *self.test_home_dir.lock().unwrap() = Some(home_dir.to_path_buf());
    }
}

/// Collect open R/*.R and tests/testthat/ siblings of `closing_uri` whose
/// diagnostics must refresh after a package-mode visibility change triggered
/// by `did_close`. Filters out the closing URI and non-package files,
/// enforces `max_revalidations_per_trigger` (priority-ordered via
/// `cross_file_activity`), then snapshots each survivor's `(version, revision)`
/// for the `run_debounced_diagnostics` freshness guard.
///
/// Returned as a `Vec` (not a `HashSet`) because the caller iterates in the
/// post-cap, post-priority order to spawn one debounced diagnostic task per
/// sibling.
fn collect_close_fanout_siblings(
    state: &WorldState,
    closing_uri: &Url,
    workspace_root: &std::path::Path,
) -> Vec<(Url, Option<i32>, Option<u64>)> {
    let mut candidates: Vec<Url> = state
        .documents
        .keys()
        .filter(|open_uri| *open_uri != closing_uri)
        .filter(|open_uri| {
            open_uri.to_file_path().ok().is_some_and(|p| {
                crate::package_state::is_r_source_path(&p, workspace_root).is_some()
            })
        })
        .cloned()
        .collect();
    cap_watched_file_revalidations(
        &mut candidates,
        &state.cross_file_activity,
        state.cross_file_config.max_revalidations_per_trigger,
    );
    candidates
        .into_iter()
        .filter_map(|sibling_uri| {
            state
                .documents
                .get(&sibling_uri)
                .map(|doc| (sibling_uri, doc.version, Some(doc.revision)))
        })
        .collect()
}

fn extend_with_open_package_docs(
    affected: &mut Vec<Url>,
    affected_set: &mut std::collections::HashSet<Url>,
    state: &WorldState,
    workspace_root: &std::path::Path,
) {
    for open_uri in state.documents.keys() {
        if open_uri
            .to_file_path()
            .ok()
            .is_some_and(|p| crate::package_state::is_r_source_path(&p, workspace_root).is_some())
            && affected_set.insert(open_uri.clone())
        {
            affected.push(open_uri.clone());
        }
    }
}

/// Widen `affected` with every open doc whose scope may include the
/// `load_all()` sentinel package, so an `R/`-change-driven recompute of the
/// [`crate::package_state::PackageScopeContribution`] re-diagnoses them.
///
/// The package-visibility fanout in `did_change_watched_files`
/// ([`extend_with_open_package_docs`] / its async-path twin) only force-marks
/// open files that are `is_r_source_path` (under `R/` or `tests/testthat/`).
/// A `devtools::load_all()` *carrier* — e.g. a root-level `analysis.R` or a
/// `scripts/` file — is not `is_r_source_path`, so a change to the loaded
/// package's `R/` would silently leave the carrier (and its source-graph
/// neighborhood) stale. This helper closes that gap.
///
/// The affected set is widened with, restricted to OPEN docs that exist in
/// `open_docs`:
/// 1. **Carriers**: every open doc whose artifacts have
///    `calls_dev_load_all == true` AND whose path is under the package root
///    (`path.strip_prefix(pkg_root).is_ok()`) — matching the query-file gate
///    that activates the sentinel overlay. An out-of-root `load_all()` caller
///    is NOT a carrier (its sentinel would not resolve).
/// 2. For each carrier, its source-graph ancestors (callers, via the
///    child→parent scope hoist) and descendants (callees), via the shared
///    [`crate::cross_file::dependency::DependencyGraph::revalidation_consistent_set`]
///    primitive.
/// 3. When the workspace `.Rprofile` attaches the sentinel
///    (`rprofile_attaches_sentinel`), every open doc for which the rprofile
///    prelude applies (`rprofile_applies`), plus that doc's graph
///    neighborhood.
///
/// Conservative superset: position-aware suppress/unsuppress correctness and
/// version monotonicity are enforced later at diagnosis-time scope resolution
/// and by the force-republish gate — this closure's only contract is "the
/// right docs get force-marked". Lock-safe: it touches only graph reachability
/// and per-doc booleans, never cross-file scope resolution.
///
/// `open_docs` is `(uri, calls_dev_load_all, path)` per open document; the
/// caller reads `calls_dev_load_all` via the artifact cache peek
/// (`document_store.get_without_touch`). `max_depth` / `max_visited` must match
/// the budgets the surrounding handler uses for its other graph walks
/// (`max_chain_depth` / `max_transitive_dependents_visited`).
// Deliberate: this mirrors the inputs `DependencyGraph::revalidation_consistent_set`
// needs (graph + per-doc carrier facts + matching budgets); bundling them into a
// struct would only relocate the argument list without improving clarity.
#[allow(clippy::too_many_arguments)]
fn extend_affected_for_load_all_revalidation(
    affected: &mut Vec<Url>,
    affected_set: &mut std::collections::HashSet<Url>,
    open_docs: &[(Url, bool, std::path::PathBuf)],
    pkg_root: &std::path::Path,
    rprofile_attaches_sentinel: bool,
    rprofile_applies: impl Fn(&std::path::Path) -> bool,
    graph: &crate::cross_file::dependency::DependencyGraph,
    max_depth: usize,
    max_visited: usize,
) {
    let open_set: std::collections::HashSet<&Url> = open_docs.iter().map(|(u, _, _)| u).collect();

    let push =
        |uri: &Url, affected: &mut Vec<Url>, affected_set: &mut std::collections::HashSet<Url>| {
            if affected_set.insert(uri.clone()) {
                affected.push(uri.clone());
            }
        };

    // Seed roots: load_all carriers (open + calls_dev_load_all + under root)
    // and — when the workspace .Rprofile attaches the sentinel — every open
    // doc the prelude applies to.
    let mut roots: Vec<Url> = Vec::new();
    for (uri, calls_load_all, path) in open_docs {
        let is_carrier = *calls_load_all && path.strip_prefix(pkg_root).is_ok();
        let via_rprofile = rprofile_attaches_sentinel && rprofile_applies(path);
        if is_carrier || via_rprofile {
            push(uri, affected, affected_set);
            roots.push(uri.clone());
        }
    }

    // Each seed's source-graph neighborhood: ancestors (callers, child→parent
    // hoist) and descendants (callees). Only OPEN docs are added.
    for root in &roots {
        for neighbor in graph.revalidation_consistent_set(root, max_depth, max_visited) {
            if open_set.contains(&neighbor) {
                push(&neighbor, affected, affected_set);
            }
        }
    }
}

/// Lock-held adapter over [`extend_affected_for_load_all_revalidation`]:
/// gathers the open-doc carrier flags (via the artifact cache peek
/// `document_store.get_without_touch`, NOT scope resolution), the workspace
/// `.Rprofile`'s sentinel-attachment bool, and the source graph from `state`,
/// then widens `affected` / `affected_set`. Lock-safe — touches only cache
/// peeks, graph reachability, and per-doc booleans. Called from the
/// package-visibility-change blocks of `did_change_watched_files` alongside
/// the existing `R/`+`tests/` fanout.
fn extend_affected_for_load_all_revalidation_from_state(
    affected: &mut Vec<Url>,
    affected_set: &mut std::collections::HashSet<Url>,
    state: &WorldState,
    pkg_root: &std::path::Path,
) {
    let contrib = state.package_state.scope_contribution();
    let rprofile_attaches_sentinel = contrib
        .rprofile_attached_packages
        .contains(crate::package_library::LOAD_ALL_SENTINEL);
    // Fast path: in a workspace with no `.Rprofile` sentinel attach and no open
    // load_all carrier — the overwhelmingly common case — there is nothing to
    // seed. Detecting a carrier needs the cache peek anyway, but skipping the
    // per-doc `to_file_path()` allocation and the `open_docs` Vec build below is
    // free on every R/-change. (When we DO proceed, the carrier peek is repeated
    // in the Vec build; that path only runs when load_all is actually in play.)
    if !rprofile_attaches_sentinel
        && !state.documents.keys().any(|u| {
            state
                .document_store
                .get_without_touch(u)
                .is_some_and(|doc| doc.artifacts.calls_dev_load_all)
        })
    {
        return;
    }
    // Snapshot open docs with their load_all-carrier flag (cache peek, no scope
    // resolution) and on-disk path.
    let open_docs: Vec<(Url, bool, std::path::PathBuf)> = state
        .documents
        .keys()
        .filter_map(|u| {
            let path = u.to_file_path().ok()?;
            let calls_load_all = state
                .document_store
                .get_without_touch(u)
                .is_some_and(|doc| doc.artifacts.calls_dev_load_all);
            Some((u.clone(), calls_load_all, path))
        })
        .collect();
    extend_affected_for_load_all_revalidation(
        affected,
        affected_set,
        &open_docs,
        pkg_root,
        rprofile_attaches_sentinel,
        |p| crate::cross_file::scope::rprofile_prelude_applies(p, contrib),
        &state.cross_file_graph,
        state.cross_file_config.max_chain_depth,
        state.cross_file_config.max_transitive_dependents_visited,
    );
}

/// Whether the DELETED-only branch of `did_change_watched_files` should
/// run `apply_package_event(Initial)` to re-derive package state.
///
/// `derive_package_state` walks every R file's roxygen tags and rebuilds
/// the namespace model from inputs. Skip when no package-relevant
/// deletions occurred in this batch — non-R deletions (e.g.
/// `data/foo.csv`) leave `package_inputs` untouched, and re-deriving
/// from unchanged inputs cannot change `package_state`.
///
/// Matches the gate in the parallel mixed-batch branch:
/// `else if had_pkg_deletion && state.package_inputs.workspace_root.is_some()`.
fn should_rederive_after_deletion_batch(
    workspace_is_package: bool,
    had_pkg_deletion: bool,
) -> bool {
    workspace_is_package && had_pkg_deletion
}

/// Extend `affected_open_docs` with URIs from `open_keys` not already
/// present, conditionally marking the new URIs for force-republish.
///
/// Called from the DESCRIPTION/NAMESPACE manifest event block in
/// `did_change_watched_files`. Marking is gated on `sync_publish_path`
/// because the manifest block runs before either the synchronous
/// publish loop or the async cap+mark+publish block:
///
/// - **Sync path** (`uris_to_update.is_empty()`): every URI in
///   `affected_open_docs` is published without an additional cap, so the
///   newly added URIs must be marked here to allow the same-version
///   forced republish through the gate.
/// - **Async path** (`uris_to_update` non-empty): the spawned task caps
///   `affected_for_async` to `max_revalidations_per_trigger` and only
///   marks the post-cap survivors. Marking here would create orphan
///   force-republish markers for URIs the cap drops, which would
///   incorrectly allow a future same-version publish to succeed without
///   an explicit mark.
fn extend_affected_for_manifest_change(
    affected_open_docs: &mut Vec<Url>,
    open_keys: Vec<Url>,
    sync_publish_path: bool,
    gate: &crate::cross_file::revalidation::CrossFileDiagnosticsGate,
) {
    let existing: std::collections::HashSet<Url> = affected_open_docs.iter().cloned().collect();
    let new_uris: Vec<Url> = open_keys
        .into_iter()
        .filter(|u| !existing.contains(u))
        .collect();
    if sync_publish_path {
        gate.mark_force_republish_many(new_uris.iter());
    }
    affected_open_docs.extend(new_uris);
}

#[cfg(test)]
mod deletion_rederive_decision_tests {
    use super::*;

    #[test]
    fn rederive_after_pkg_deletion_in_package_workspace() {
        assert!(should_rederive_after_deletion_batch(true, true));
    }

    #[test]
    fn no_rederive_for_non_pkg_deletion_in_package_workspace() {
        // Regression: previously the DELETED-only branch unconditionally
        // re-derived whenever workspace_root.is_some(), even if no package
        // source files were deleted. `derive_package_state` walks every R
        // file's roxygen tags — wasted work per non-package deletion in a
        // package workspace (e.g. deleting `data/foo.csv`).
        assert!(!should_rederive_after_deletion_batch(true, false));
    }

    #[test]
    fn no_rederive_outside_package_workspace() {
        assert!(!should_rederive_after_deletion_batch(false, true));
        assert!(!should_rederive_after_deletion_batch(false, false));
    }
}

#[cfg(test)]
mod manifest_extend_tests {
    use super::*;
    use crate::cross_file::revalidation::CrossFileDiagnosticsGate;

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///{name}")).unwrap()
    }

    #[test]
    fn sync_path_marks_new_uris_only() {
        let gate = CrossFileDiagnosticsGate::new();
        let existing = test_uri("R/existing.R");
        let new1 = test_uri("R/new1.R");
        let new2 = test_uri("R/new2.R");
        for u in &[&existing, &new1, &new2] {
            gate.record_publish(u, 1);
        }
        let mut affected: Vec<Url> = vec![existing.clone()];
        let open_keys = vec![existing.clone(), new1.clone(), new2.clone()];

        extend_affected_for_manifest_change(&mut affected, open_keys, true, &gate);

        assert_eq!(affected.len(), 3, "all unique open keys appended");
        assert!(
            gate.can_publish(&new1, 1),
            "new URI {new1} must be force-marked in sync path"
        );
        assert!(
            gate.can_publish(&new2, 1),
            "new URI {new2} must be force-marked in sync path"
        );
        assert!(
            !gate.can_publish(&existing, 1),
            "pre-existing URI {existing} (already in affected) must not be re-marked by this helper"
        );
    }

    #[test]
    fn async_path_extends_without_marking() {
        let gate = CrossFileDiagnosticsGate::new();
        let new1 = test_uri("R/new1.R");
        let new2 = test_uri("R/new2.R");
        for u in &[&new1, &new2] {
            gate.record_publish(u, 1);
        }
        let mut affected: Vec<Url> = vec![];
        let open_keys = vec![new1.clone(), new2.clone()];

        extend_affected_for_manifest_change(&mut affected, open_keys.clone(), false, &gate);

        assert_eq!(affected, open_keys, "affected extended with open_keys");
        for u in &open_keys {
            assert!(
                !gate.can_publish(u, 1),
                "URI {u} must NOT be force-marked in async path (post-cap mark owns marking)"
            );
        }
    }

    #[test]
    fn async_path_followed_by_cap_publish_leaves_no_orphan_markers() {
        // Regression: prior to this fix, the manifest event block in
        // did_change_watched_files called mark_force_republish_many on
        // ALL open documents unconditionally. The async block then
        // applied max_revalidations_per_trigger and force-marked only
        // the post-cap survivors, leaving URIs dropped by the cap with
        // orphan force-republish markers. Those orphan markers would
        // incorrectly allow a future same-version publish to succeed
        // without an explicit mark for that publish.
        let gate = CrossFileDiagnosticsGate::new();
        let max_revalidations = 10usize;
        let open_uris: Vec<Url> = (0..20).map(|i| test_uri(&format!("R/f{i}.R"))).collect();
        for u in &open_uris {
            gate.record_publish(u, 1);
        }

        // Manifest block (fixed pattern): extend, do NOT mark.
        let mut affected: Vec<Url> = vec![];
        extend_affected_for_manifest_change(&mut affected, open_uris.clone(), false, &gate);

        // Async block: cap, mark, publish.
        let mut affected_for_async = affected.clone();
        affected_for_async.truncate(max_revalidations);
        gate.mark_force_republish_many(affected_for_async.iter());
        for u in &affected_for_async {
            assert!(
                gate.try_consume_publish(u, 1),
                "force-marked URI must publish at v=1"
            );
        }

        // URIs dropped by the cap must NOT have orphan force markers.
        for u in &open_uris[max_revalidations..] {
            assert!(
                !gate.can_publish(u, 1),
                "URI {u} dropped by cap retained an orphan force-republish marker"
            );
        }
    }
}

#[cfg(test)]
mod load_all_revalidation_tests {
    use super::*;
    use crate::cross_file::dependency::DependencyGraph;
    use crate::cross_file::revalidation::CrossFileDiagnosticsGate;
    use crate::cross_file::{CrossFileMetadata, ForwardSource};

    fn uri(rel: &str) -> Url {
        Url::parse(&format!("file:///project/{rel}")).unwrap()
    }

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from("/project")
    }

    fn ws_url() -> Url {
        Url::parse("file:///project").unwrap()
    }

    fn meta_sourcing(rel: &str) -> CrossFileMetadata {
        CrossFileMetadata {
            sources: vec![ForwardSource {
                path: rel.to_string(),
                line: 1,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// `caller.R --source--> L.R --source--> callee.R`, with `L.R` the
    /// load_all carrier. All three files live at the workspace root.
    fn chain_graph() -> DependencyGraph {
        let mut g = DependencyGraph::new();
        g.update_file(
            &uri("caller.R"),
            &meta_sourcing("L.R"),
            Some(&ws_url()),
            |_| None,
        );
        g.update_file(
            &uri("L.R"),
            &meta_sourcing("callee.R"),
            Some(&ws_url()),
            |_| None,
        );
        g.update_file(
            &uri("callee.R"),
            &CrossFileMetadata::default(),
            Some(&ws_url()),
            |_| None,
        );
        g
    }

    fn path_of(rel: &str) -> std::path::PathBuf {
        root().join(rel)
    }

    // Section B: all three roles (carrier, caller, callee) get re-diagnosed.
    #[test]
    fn load_all_revalidation_marks_carrier_callers_and_callees() {
        let graph = chain_graph();
        let open_docs = vec![
            (uri("caller.R"), false, path_of("caller.R")),
            (uri("L.R"), true, path_of("L.R")), // carrier
            (uri("callee.R"), false, path_of("callee.R")),
        ];
        let mut affected: Vec<Url> = Vec::new();
        let mut affected_set: std::collections::HashSet<Url> = std::collections::HashSet::new();

        extend_affected_for_load_all_revalidation(
            &mut affected,
            &mut affected_set,
            &open_docs,
            &root(),
            false,
            |_| false,
            &graph,
            64,
            50_000,
        );

        assert!(affected_set.contains(&uri("L.R")), "carrier must be marked");
        assert!(
            affected_set.contains(&uri("caller.R")),
            "ancestor (caller) must be marked via child->parent hoist"
        );
        assert!(
            affected_set.contains(&uri("callee.R")),
            "descendant (callee) must be marked"
        );
        // No duplicates between Vec and set.
        assert_eq!(affected.len(), affected_set.len());
    }

    // The gap the spec calls out: a root-level carrier (NOT under R/).
    #[test]
    fn load_all_revalidation_closes_root_level_carrier_gap() {
        let graph = DependencyGraph::new();
        let open_docs = vec![(uri("analysis.R"), true, path_of("analysis.R"))];
        let mut affected: Vec<Url> = Vec::new();
        let mut affected_set: std::collections::HashSet<Url> = std::collections::HashSet::new();

        extend_affected_for_load_all_revalidation(
            &mut affected,
            &mut affected_set,
            &open_docs,
            &root(),
            false,
            |_| false,
            &graph,
            64,
            50_000,
        );

        assert!(
            affected_set.contains(&uri("analysis.R")),
            "root-level (non-R/) load_all carrier must be seeded"
        );
    }

    // Matches the query-file gate: a carrier OUTSIDE the package root is not seeded.
    #[test]
    fn load_all_revalidation_skips_out_of_root_carrier() {
        let graph = DependencyGraph::new();
        let outside = Url::parse("file:///elsewhere/script.R").unwrap();
        let open_docs = vec![(
            outside.clone(),
            true,
            std::path::PathBuf::from("/elsewhere/script.R"),
        )];
        let mut affected: Vec<Url> = Vec::new();
        let mut affected_set: std::collections::HashSet<Url> = std::collections::HashSet::new();

        extend_affected_for_load_all_revalidation(
            &mut affected,
            &mut affected_set,
            &open_docs,
            &root(),
            false,
            |_| false,
            &graph,
            64,
            50_000,
        );

        assert!(
            !affected_set.contains(&outside),
            "out-of-root load_all caller must NOT be seeded as a carrier"
        );
        assert!(affected.is_empty());
    }

    // When .Rprofile attaches the sentinel, an open doc the prelude applies to
    // is added; one for which it does not is withheld via this route.
    #[test]
    fn load_all_revalidation_rprofile_reach() {
        let graph = DependencyGraph::new();
        let reachable = uri("scripts/a.R");
        let withheld = uri("R/internal.R");
        let open_docs = vec![
            (reachable.clone(), false, path_of("scripts/a.R")),
            (withheld.clone(), false, path_of("R/internal.R")),
        ];
        let mut affected: Vec<Url> = Vec::new();
        let mut affected_set: std::collections::HashSet<Url> = std::collections::HashSet::new();

        // Prelude applies to scripts/a.R but not to the R/ file (modeling the
        // package-mode withholding of the prelude from R/).
        let applies = |p: &std::path::Path| p.ends_with("scripts/a.R");

        extend_affected_for_load_all_revalidation(
            &mut affected,
            &mut affected_set,
            &open_docs,
            &root(),
            true, // .Rprofile attaches the sentinel
            applies,
            &graph,
            64,
            50_000,
        );

        assert!(
            affected_set.contains(&reachable),
            "prelude-reachable open doc must be added via the rprofile route"
        );
        assert!(
            !affected_set.contains(&withheld),
            "prelude-withheld open doc must NOT be added via the rprofile route"
        );
    }

    // Handler-level: after running the marking on the affected set, the gate
    // reports force-republish for carrier + caller + callee. Mirrors
    // `sync_path_marks_new_uris_only`'s gate usage.
    #[test]
    fn load_all_revalidation_marks_force_republish_gate() {
        let graph = chain_graph();
        let open_docs = vec![
            (uri("caller.R"), false, path_of("caller.R")),
            (uri("L.R"), true, path_of("L.R")),
            (uri("callee.R"), false, path_of("callee.R")),
        ];
        let mut affected: Vec<Url> = Vec::new();
        let mut affected_set: std::collections::HashSet<Url> = std::collections::HashSet::new();

        extend_affected_for_load_all_revalidation(
            &mut affected,
            &mut affected_set,
            &open_docs,
            &root(),
            false,
            |_| false,
            &graph,
            64,
            50_000,
        );

        // Handler step (mirrors the sync path): cap then bulk-mark.
        let gate = CrossFileDiagnosticsGate::new();
        for u in &affected {
            gate.record_publish(u, 1);
        }
        gate.mark_force_republish_many(affected.iter());

        for u in [uri("L.R"), uri("caller.R"), uri("callee.R")] {
            assert!(
                gate.force_republish_count_for_test(&u) > 0,
                "force-republish must fire for {u}"
            );
        }
    }
}

fn is_package_source_dir(path: &std::path::Path, root: &std::path::Path) -> bool {
    let r_dir = root.join("R");
    let testthat_dir = root.join("tests").join("testthat");
    path == r_dir
        || path.starts_with(&r_dir)
        || path == testthat_dir
        || path.starts_with(testthat_dir)
}

/// Whether `path` lives under the package's `data/` or `data-raw/` directories.
///
/// CREATED/CHANGED watched-file events for these directories must reach
/// `package_state::event::translate`, which has dedicated handlers that rescan
/// `dataset_names` (from `data/`) and `sysdata_names` (from `data-raw/`).
/// The R-source gate (`is_r_source_path` / [`is_package_source_dir`]) does not
/// cover them, so without this predicate adding or editing a `data/*.rda` or
/// `data-raw/*.R` file would leave those symbol sets stale until an unrelated
/// `R/` edit. (DELETE events already reach `translate` unconditionally.)
///
/// The directory boundaries mirror `event.rs`'s own detection: a path strictly
/// *under* `data/` or `data-raw/` (not the directory node itself). `Path::starts_with`
/// is component-wise, so `data-raw/` does not match the `data/` prefix.
fn is_package_data_path(path: &std::path::Path, root: &std::path::Path) -> bool {
    let data_dir = root.join("data");
    let data_raw_dir = root.join("data-raw");
    (path.starts_with(&data_dir) && path != data_dir)
        || (path.starts_with(&data_raw_dir) && path != data_raw_dir)
}

fn is_package_manifest_path(path: &std::path::Path, root: &std::path::Path) -> bool {
    path == root.join("DESCRIPTION") || path == root.join("NAMESPACE")
}

fn is_package_relevant_open_uri(uri: &Url, root: &std::path::Path) -> bool {
    uri.to_file_path().ok().is_some_and(|path| {
        crate::package_state::is_r_source_path(&path, root).is_some()
            || is_package_manifest_path(&path, root)
    })
}

/// True if `p` (or its canonical form) is one of the files `.Rprofile`
/// transitively `source()`s. The static scanner records *canonical* paths in
/// `rprofile_sourced_files`, but watcher URIs are not necessarily canonical
/// (symlinked roots like macOS `/tmp` → `/private/tmp`), so fall back to
/// canonicalizing `p` before giving up. Used to route an edit to a sourced
/// helper that lives outside the package's tracked R dirs (e.g. `scripts/`,
/// `inst/`) into the package-state update so the prelude is re-scanned (Task 12).
fn path_in_rprofile_sourced_set(
    p: &std::path::Path,
    sourced: &std::collections::BTreeSet<std::path::PathBuf>,
) -> bool {
    sourced.contains(p) || p.canonicalize().ok().is_some_and(|c| sourced.contains(&c))
}

/// Returns `true` when any delta in `deltas` is `RProfileChanged`, or when any
/// nested [`crate::package_state::PackageInputDelta::Batch`] recursively contains one.
fn batch_contains_rprofile_changed(deltas: &[crate::package_state::PackageInputDelta]) -> bool {
    deltas.iter().any(|d| match d {
        crate::package_state::PackageInputDelta::RProfileChanged => true,
        crate::package_state::PackageInputDelta::Batch(inner) => {
            batch_contains_rprofile_changed(inner)
        }
        _ => false,
    })
}

fn collect_package_r_file_inputs_from_disk(
    root: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, crate::package_state::RFileInput> {
    let mut r_files = std::collections::BTreeMap::new();
    for base in [root.join("R"), root.join("tests").join("testthat")] {
        if !base.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(base)
            .follow_links(false)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
        {
            let path = entry.into_path();
            let Some(kind) = crate::package_state::is_r_source_path(&path, root) else {
                continue;
            };
            // Decode through the shared BOM-aware seam so package R-file inputs
            // match the workspace scan (which uses read_source); an undecodable
            // file is skipped, like the scan.
            let Ok(text) = crate::state::read_source(&path) else {
                continue;
            };
            let text: std::sync::Arc<str> = text.into();
            let digest = crate::package_state::ContentDigest::of(&text);
            r_files.insert(
                path,
                crate::package_state::RFileInput {
                    kind,
                    text,
                    content_digest: digest,
                },
            );
        }
    }
    r_files
}

fn hydrate_package_r_files_from_state(
    state: &WorldState,
    root: &std::path::Path,
    mut r_files: std::collections::BTreeMap<std::path::PathBuf, crate::package_state::RFileInput>,
) -> std::collections::BTreeMap<std::path::PathBuf, crate::package_state::RFileInput> {
    let open_uris: std::collections::HashSet<Url> = state.documents.keys().cloned().collect();

    for uri in state.workspace_index_new.uris() {
        if open_uris.contains(&uri) {
            continue;
        }
        if let Ok(path) = uri.to_file_path()
            && let Some(kind) = crate::package_state::is_r_source_path(&path, root)
            && let Some(entry) = state.workspace_index_new.get(&uri)
        {
            let text: std::sync::Arc<str> = entry.contents.to_string().into();
            let digest = crate::package_state::ContentDigest::of(&text);
            r_files.insert(
                path,
                crate::package_state::RFileInput {
                    kind,
                    text,
                    content_digest: digest,
                },
            );
        }
    }

    for uri in &open_uris {
        if let Ok(path) = uri.to_file_path()
            && let Some(kind) = crate::package_state::is_r_source_path(&path, root)
        {
            let text: std::sync::Arc<str> = state
                .documents
                .get(uri)
                .map(|d| d.text())
                .unwrap_or_default()
                .into();
            let digest = crate::package_state::ContentDigest::of(&text);
            r_files.insert(
                path,
                crate::package_state::RFileInput {
                    kind,
                    text,
                    content_digest: digest,
                },
            );
        }
    }

    r_files
}

pub(crate) fn initialize_package_inputs_from_state(
    state: &mut WorldState,
    root: std::path::PathBuf,
    desc_text: Option<Arc<str>>,
    ns_text: Option<Arc<str>>,
    disk_r_files: std::collections::BTreeMap<std::path::PathBuf, crate::package_state::RFileInput>,
    precomputed_rprofile_scan: Option<crate::package_state::rprofile::RprofileScan>,
) {
    state.package_inputs.workspace_root = Some(root.clone());
    state.package_inputs.package_mode = state.cross_file_config.package_mode;
    state.package_inputs.model_rprofile = state.cross_file_config.model_rprofile;

    state.package_inputs.description =
        desc_text.map(|text| crate::package_state::DescriptionInput { text });
    state.package_inputs.namespace =
        ns_text.map(|text| crate::package_state::NamespaceInput { text });

    let new_r_files = hydrate_package_r_files_from_state(state, &root, disk_r_files);
    state.package_inputs.r_files = new_r_files;
    state.package_inputs.dataset_names = crate::package_state::scan_own_package_data_dir(&root);
    state.package_inputs.sysdata_names =
        crate::package_state::sysdata::scan_sysdata_generating_scripts(&root);

    // If `.Rprofile` is currently OPEN as a live document, its prelude is owned
    // by the did_open/did_change path (sourced from the in-memory buffer). A
    // disk-based (re)seed here — startup background scan completing late, or a
    // `packageMode`/`rprofilePrelude` rebuild — must not clobber that buffer
    // prelude with the on-disk scan and lose unsaved edits. Leave the prelude
    // inputs untouched in that case (modeling stays enabled). When modeling is
    // disabled the prelude is cleared regardless (an open buffer can't keep a
    // prelude that the feature is turned off for).
    let rprofile_open = state
        .documents
        .keys()
        .any(|u| u.to_file_path().ok().as_deref() == Some(root.join(".Rprofile").as_path()));
    if state.package_inputs.model_rprofile && rprofile_open {
        // Buffer-authoritative: keep the live prelude inputs as-is.
    } else {
        // The `.Rprofile` prelude scan follows transitive `source()` chains, so
        // it must NOT run while the `WorldState` write lock is held
        // (locking-discipline invariant). Callers that hold the lock under
        // contention (LSP startup / `packageMode` rebuild) precompute the scan
        // OFF-lock and pass it in via `precomputed_rprofile_scan`; the
        // non-contended callers (`raven check`, unit tests) pass `None` and let
        // it scan inline. When modeling is disabled the prelude is cleared.
        let rprofile_scan = if state.package_inputs.model_rprofile {
            precomputed_rprofile_scan
                .unwrap_or_else(|| crate::package_state::rprofile::scan_workspace_rprofile(&root))
        } else {
            crate::package_state::rprofile::RprofileScan::default()
        };
        state.package_inputs.rprofile_symbols = rprofile_scan.symbols;
        state.package_inputs.rprofile_attached_packages = rprofile_scan.attached_packages;
        state.package_inputs.rprofile_sourced_files = rprofile_scan.sourced_files;
    }

    state.apply_package_event(&crate::package_state::PackageInputDelta::Initial);

    // Resolve system.file() sources in workspace index now that package state
    // (workspace name, lib paths) is available.
    state.resolve_system_file_in_workspace();
}

/// Shared trigger predicate for the sysdata R fallback: the AST scan of
/// `data-raw/` found nothing AND a binary `R/sysdata.rda` exists on disk
/// (e.g. sources that commit the `.rda` with no generating script, like
/// r-lib/cli). Single-sourced here so the LSP startup path and `raven check`
/// cannot drift on when the fallback fires.
pub(crate) fn sysdata_r_fallback_needed(state: &WorldState) -> bool {
    let has_sysdata_rda = state
        .package_inputs
        .workspace_root
        .as_ref()
        .map(|r| r.join("R").join("sysdata.rda").is_file())
        .unwrap_or(false);
    state.package_inputs.sysdata_names.is_empty() && has_sysdata_rda
}

/// Remove a file that no longer exists on disk from every cross-file
/// structure: the dependency graph (all edges in both directions), the
/// disk-content cache, both workspace indexes, and the metadata cache.
///
/// Returns the affected open dependents, computed from the **pre-removal**
/// graph via `compute_affected_dependents_after_edit` — the removal itself
/// destroys the edges needed to find them. Walks both backward (parents that
/// source the removed file lose its symbols) and forward (children that were
/// sourced by it lose their inherited parent scope).
///
/// Deliberately caller-owned, because batching differs per caller:
/// - `package_inputs` translation (`HandlerEvent::WatchedFileChanged { deleted: true }`);
/// - `recompute_open_neighborhood_pins` — the watched-files DELETED loop
///   recomputes pins once per batch, not once per file;
/// - force-republish marking and diagnostics publishing.
///
/// Shared by the watched-files `FileChangeType::DELETED` branch and the
/// `did_close` disk-resync path (issue #558), so removal semantics cannot
/// drift between them.
fn remove_file_from_cross_file_state(state: &mut WorldState, uri: &Url) -> Vec<Url> {
    let neighbors = crate::cross_file::revalidation::compute_affected_dependents_after_edit(
        uri,
        true,
        false,
        &state.cross_file_graph,
        |u| state.documents.contains_key(u),
        state.cross_file_config.max_chain_depth,
        state.cross_file_config.max_transitive_dependents_visited,
    );

    state.workspace_index_new.invalidate(uri);
    state.cross_file_graph.remove_file(uri);
    state.cross_file_file_cache.invalidate(uri);
    state.cross_file_workspace_index.invalidate(uri);
    state.cross_file_meta.remove(uri);
    // The legacy startup-scan document map is a content-provider tier of its
    // own (`DefaultContentProvider` consults it before the file cache) and an
    // `exists_cached` source; leaving the entry would keep reporting a
    // deleted file as present.
    state.workspace_index.remove(uri);
    neighbors
}

/// Resolve a file's `# raven: sourced-by` targets and pre-collect their
/// content (open buffer first, then the disk-content cache) into a map, so
/// `DependencyGraph::update_file`'s `get_content` closure can run while the
/// graph is mutably borrowed. Backward directives always resolve relative to
/// the file's own directory — `PathContext::new`, never `# raven: cd` (the
/// CLAUDE.md path-resolution invariant). Shared by `did_open`, `did_change`,
/// and `resync_file_from_disk` so backward parent lookup cannot drift.
fn collect_backward_parent_content(
    state: &WorldState,
    uri: &Url,
    workspace_root: Option<&Url>,
    meta: &crate::cross_file::CrossFileMetadata,
) -> std::collections::HashMap<Url, String> {
    let backward_path_ctx = crate::cross_file::path_resolve::PathContext::new(uri, workspace_root);
    meta.sourced_by
        .iter()
        .filter_map(|d| {
            let ctx = backward_path_ctx.as_ref()?;
            let resolved = crate::cross_file::path_resolve::resolve_path(&d.path, ctx)?;
            let parent_uri = Url::from_file_path(resolved).ok()?;
            let content = state
                .documents
                .get(&parent_uri)
                .map(|doc| doc.text())
                .or_else(|| state.cross_file_file_cache.get(&parent_uri))?;
            Some((parent_uri, content))
        })
        .collect()
}

/// What a single-file disk resync did, so the caller can fold the affected
/// open dependents into its own fanout batch (capping, force-republish
/// marking, and publishing stay caller-owned — callers batch differently).
enum ResyncOutcome {
    /// Disk content was committed. `affected_after_commit` is the deduped
    /// union of open working-directory-invalidated children and — when the
    /// commit changed edges — the post-update
    /// `compute_affected_dependents_after_edit` walk. `new_interface_hash`
    /// is the committed disk artifacts' `interface_hash`, so callers that
    /// know the pre-resync hash (the close path captures the buffer's) can
    /// widen their fanout on interface-only changes.
    Updated {
        affected_after_commit: Vec<Url>,
        new_interface_hash: u64,
    },
    /// The file no longer exists on disk; its cross-file state was removed
    /// via `remove_file_from_cross_file_state`. `affected_dependents` comes
    /// from the pre-removal graph.
    Removed { affected_dependents: Vec<Url> },
    /// The URI was an open document at commit time, so the disk state was
    /// discarded untouched: the open buffer is authoritative, and its own
    /// `did_open`/`did_change` pipeline has already installed fresher state.
    Vetoed,
    /// Nothing to resync: non-`file:` URI, the file exists but could not be
    /// read/decoded, or a concurrent resync already committed a NEWER disk
    /// snapshot for this file (staleness veto). No state was mutated.
    Skipped,
}

/// The "file is gone" tail of [`resync_file_from_disk`]: veto if the URI was
/// (re)opened, otherwise remove the file's cross-file state and refresh the
/// pin set.
async fn resync_missing_file(state_arc: &Arc<RwLock<WorldState>>, uri: &Url) -> ResyncOutcome {
    let mut state = state_arc.write().await;
    if state.documents.contains_key(uri) {
        return ResyncOutcome::Vetoed;
    }
    let affected_dependents = remove_file_from_cross_file_state(&mut state, uri);
    state.recompute_open_neighborhood_pins();
    ResyncOutcome::Removed {
        affected_dependents,
    }
}

/// Re-sync one file's cross-file state (disk-content cache, workspace index,
/// dependency graph) from its on-disk content. The shared single-file
/// primitive behind the watched-files CREATED/CHANGED async pass and the
/// `did_close` disk resync (issue #558) — the two paths must not drift.
///
/// `old_meta` is caller-supplied because the correct capture point differs:
/// the watched-files path reads it just before the disk read (the file was
/// never open, so every metadata tier already reflects disk), while the
/// close path must capture it **before** `did_close` wipes the document
/// stores — the unsaved buffer's metadata (its `# raven: cd` state in
/// particular) lives only there, and working-directory change detection
/// compares against what dependents last saw, i.e. the buffer.
///
/// Locking: all disk I/O and content-derived computation (masking, parsing,
/// metadata/artifact extraction) run off-lock; `resolve_system_file_sources`
/// inputs are snapshotted under a brief read lock; every mutation happens
/// inside **one** write-lock critical section whose first statement is the
/// reopen veto — if the URI is an open document at commit time the resync
/// returns [`ResyncOutcome::Vetoed`] having mutated nothing. Because the
/// buffer-derived edges are only ever replaced atomically by
/// `DependencyGraph::update_file` inside that section, there is no transient
/// window where the file has an empty edge set. (The veto is deliberately
/// unconditional: the pre-#558 watched-files pass split this across separate
/// read/write acquisitions with no open-at-commit check on the graph update,
/// so a reopen during the async gap could clobber fresh buffer state.)
///
/// Rmd/Quarto disk content is masked before metadata/artifact extraction
/// (chunk bodies only, never prose); the file cache stores the RAW content —
/// its readers mask on read. `chunk_kind` preserves the closing document's
/// editor-language classification: a file-backed document without an
/// `.Rmd`/`.qmd` extension opened with `languageId: rmd` was chunk-masked
/// while open, and path-only reclassification here would parse the same disk
/// text as plain R and mint prose `source()` edges. The close path passes
/// the tracked `Document`'s `chunk_kind`; the watched-files path passes
/// `None` (no editor signal exists for an external disk event) and falls
/// back to path classification.
/// `undecodable_as_missing` splits `SourceReadError::InvalidEncoding`
/// handling by caller: the close path passes `true` (the user closed a file
/// whose disk content is not R-readable — converge like a deletion, matching
/// the package `DidClose` which read the same bytes), while the watched path
/// passes `false` (a non-atomic external write can transiently present
/// invalid UTF-8, and removal would strip closed parents' incoming edges,
/// which only OPEN parents can recover — skip instead and let the completing
/// write's next CHANGED event converge).
async fn resync_file_from_disk(
    state_arc: &Arc<RwLock<WorldState>>,
    uri: &Url,
    chunk_kind: Option<crate::chunks::ChunkKind>,
    old_meta: Option<std::sync::Arc<crate::cross_file::CrossFileMetadata>>,
    undecodable_as_missing: bool,
) -> ResyncOutcome {
    let Ok(path) = uri.to_file_path() else {
        return ResyncOutcome::Skipped;
    };

    // Cheap preflight veto: if the URI is (already again) an open document,
    // skip the disk read and parse entirely. Purely an optimization for
    // close-then-reopen and watched-event-races-open bursts — the commit-time
    // veto below remains the correctness gate.
    if state_arc.read().await.documents.contains_key(uri) {
        log::trace!("Disk resync preflight-vetoed by open document: {}", uri);
        return ResyncOutcome::Vetoed;
    }

    let content = match crate::state::read_source_async(&path).await {
        Ok(c) => c,
        Err(crate::state::SourceReadError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return resync_missing_file(state_arc, uri).await;
        }
        Err(e @ crate::state::SourceReadError::InvalidEncoding { .. }) => {
            // See the doc comment: removal for the close path (converge with
            // the package DidClose that read the same bytes), skip for the
            // watched path (transient non-atomic writes).
            if undecodable_as_missing {
                log::trace!("Disk resync removing undecodable file state {}: {}", uri, e);
                return resync_missing_file(state_arc, uri).await;
            }
            log::trace!("Disk resync skipping undecodable file {}: {}", uri, e);
            return ResyncOutcome::Skipped;
        }
        Err(e) => {
            // Transient IO errors (permissions, interrupted reads): mutate
            // nothing; a later watcher event or reopen converges.
            log::trace!("Failed to read file {}: {}", uri, e);
            return ResyncOutcome::Skipped;
        }
    };

    let metadata = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return resync_missing_file(state_arc, uri).await;
        }
        Err(_) => return ResyncOutcome::Skipped,
    };

    // Mask Rmd/Quarto prose so directives/source()/library() come from chunk
    // bodies only (#343), then parse ONCE and derive both metadata and
    // artifacts from the same tree.
    let analysis = match chunk_kind {
        Some(kind) => crate::cross_file::analysis_text_for_kind(kind, &content),
        None => crate::cross_file::analysis_text_for_path(uri.path(), &content),
    };
    let tree = crate::parser_pool::with_parser(|parser| parser.parse(analysis.as_ref(), None));
    let mut cross_file_meta =
        crate::cross_file::extract_metadata_with_tree(&analysis, tree.as_ref());

    // Resolve system.file() source entries into concrete paths BEFORE
    // computing artifacts (artifact computation consumes the resolved meta).
    {
        let state = state_arc.read().await;
        let ws = state.package_state.workspace();
        let ws_name = ws.map(|w| w.name.as_str());
        let ws_root = ws.map(|w| w.root.as_path());
        let lib_paths = state.package_library.lib_paths();
        crate::cross_file::resolve_system_file_sources(
            &mut cross_file_meta,
            ws_name,
            ws_root,
            lib_paths,
        );
    }

    // **Validates: Requirements 5.1, 5.2, 5.3, 5.4** (Diagnostic suppression
    // for declared symbols)
    let artifacts = std::sync::Arc::new(match tree.as_ref() {
        Some(tree) => crate::cross_file::scope::compute_artifacts_with_metadata(
            uri,
            tree,
            &analysis,
            Some(&cross_file_meta),
        ),
        None => crate::cross_file::scope::ScopeArtifacts::default(),
    });
    let new_interface_hash = artifacts.interface_hash;

    let snapshot =
        crate::cross_file::file_cache::FileSnapshot::with_content_hash(&metadata, &content);

    // Single write-lock commit. The reopen veto MUST stay the first statement
    // under the lock: nothing above mutated shared state, so a veto is total.
    let mut state = state_arc.write().await;
    if state.documents.contains_key(uri) {
        log::trace!("Disk resync vetoed by reopen: {}", uri);
        return ResyncOutcome::Vetoed;
    }

    // Staleness veto: a concurrent resync of the SAME file (a close resync
    // racing a watched CHANGED event, or two watched batches) may have
    // committed a NEWER disk snapshot between our off-lock read and this
    // commit. The open-document vetoes are blind to that — they check
    // openness, not freshness — so compare against the snapshot the earlier
    // commit stored and abort rather than overwrite fresher state with an
    // older read. (Same-mtime writes within the filesystem's timestamp
    // granularity are not distinguished; the next watcher event converges
    // them.)
    if let Some(existing) = state.cross_file_file_cache.get_snapshot(uri)
        && existing.mtime > snapshot.mtime
    {
        log::trace!(
            "Disk resync staleness-vetoed (newer snapshot already committed): {}",
            uri
        );
        return ResyncOutcome::Skipped;
    }

    // Inherit the parent's working directory for `# raven: sourced-by` files
    // with no own `# raven: cd`, matching `did_open`/`did_change` and the
    // startup scan's WD fixpoint — without it, the commit below would resolve
    // this file's forward `source()` paths file-relative and drop edges the
    // open buffer (or the scan) had correctly formed through the parent's WD.
    {
        let workspace_root = state.workspace_folders.first().cloned();
        let max_chain_depth = state.cross_file_config.max_chain_depth;
        crate::cross_file::enrich_metadata_with_inherited_wd(
            &mut cross_file_meta,
            uri,
            workspace_root.as_ref(),
            |parent_uri| state.get_enriched_metadata(parent_uri),
            max_chain_depth,
        );
    }

    // Replace the unified-index entry IN PLACE with fresh disk state.
    // `workspace_index_new` outranks `cross_file_workspace_index` in
    // `get_enriched_metadata` and the content provider, so a stale scan-time
    // entry would shadow the commit below — but merely invalidating it would
    // drop the file from reference search, which walks this index for closed
    // files and nothing repopulates it outside the scan / on-demand
    // indexing. Mirrors the on-demand insert (including its empty
    // `loaded_packages` precedent); raw contents + analysis-tree pair is the
    // scan's own convention (masking is geometry-preserving). Known residual
    // (issue #563): the entry does not persist `chunk_kind`, so for an
    // extension-mismatched Rmd document (languageId-classified) tree
    // consumers that reconstruct analysis text by path pair this masked
    // tree with raw text.
    let fresh_entry = crate::workspace_index::IndexEntry {
        contents: ropey::Rope::from_str(&content),
        tree,
        loaded_packages: Vec::new(),
        snapshot: snapshot.clone(),
        metadata: std::sync::Arc::new(cross_file_meta.clone()),
        artifacts: artifacts.clone(),
        indexed_at_version: 0,
    };
    state.workspace_index_new.insert(uri.clone(), fresh_entry);

    // Cache RAW content for future match/inference resolution (readers mask).
    state
        .cross_file_file_cache
        .insert(uri.clone(), snapshot.clone(), content);

    // Drop the legacy startup-scan document map entry:
    // `DefaultContentProvider` consults it above the file cache, so a stale
    // scan-time entry would shadow the fresh content committed above. The
    // fresh unified-index entry keeps reference search fed.
    state.workspace_index.remove(uri);

    // `update_from_disk`'s open-document guard only ever tests THIS uri,
    // which the veto above just proved closed — pass an empty set rather
    // than cloning every open-document URI under the write lock.
    let open_docs = std::collections::HashSet::new();
    state.cross_file_workspace_index.update_from_disk(
        uri,
        &open_docs,
        snapshot,
        cross_file_meta.clone(),
        artifacts,
    );

    let workspace_root = state.workspace_folders.first().cloned();

    let parent_content =
        collect_backward_parent_content(&state, uri, workspace_root.as_ref(), &cross_file_meta);

    let graph_result = state.cross_file_graph.update_file(
        uri,
        &cross_file_meta,
        workspace_root.as_ref(),
        |parent_uri| parent_content.get(parent_uri).cloned(),
    );

    // Restore incoming edges from OPEN parents after an exists -> missing ->
    // exists cycle: `remove_file` strips parent -> child edges (they are
    // owned by PARENT metadata), and updating the recreated child cannot
    // re-derive them. Open parents still carry the resolved reference in
    // their metadata, so when this file has no incoming forward edges,
    // re-run `update_file` for each open parent that references it. (Closed
    // parents converge on their own watched events or reopen — their stale
    // edges were removed with the file, matching pre-existing behavior.)
    let mut parent_edges_restored = false;
    let has_incoming_forward = state
        .cross_file_graph
        .get_dependents(uri)
        .iter()
        .any(|e| !e.is_backward_directive);
    if !has_incoming_forward {
        // Candidate filter: ordinary `source()` entries carry only their raw
        // path string (`resolved_uri` is populated for `system.file()`
        // resolution only), so pre-match on the target's file name and let
        // `update_file` do the real path resolution — a false positive is a
        // no-op re-derivation of edges the parent already has.
        let target_name = path.file_name();
        let candidate_parents: Vec<Url> = state
            .documents
            .keys()
            .filter(|p| *p != uri)
            .filter(|p| {
                state.document_store.get_without_touch(p).is_some_and(|d| {
                    d.metadata.sources.iter().any(|s| {
                        s.resolved_uri.as_ref() == Some(uri)
                            || std::path::Path::new(&s.path).file_name() == target_name
                    })
                })
            })
            .cloned()
            .collect();
        for parent in candidate_parents {
            let Some(parent_meta) = state
                .document_store
                .get_without_touch(&parent)
                .map(|d| d.metadata.clone())
            else {
                continue;
            };
            let pc = collect_backward_parent_content(
                &state,
                &parent,
                workspace_root.as_ref(),
                &parent_meta,
            );
            let res = state.cross_file_graph.update_file(
                &parent,
                &parent_meta,
                workspace_root.as_ref(),
                |u| pc.get(u).cloned(),
            );
            parent_edges_restored |= res.edges_changed;
        }
    }

    // Invalidate children affected by working directory change (Requirement 8).
    let wd_children = crate::cross_file::revalidation::invalidate_children_on_parent_wd_change(
        uri,
        old_meta.as_deref(),
        &cross_file_meta,
        &state.cross_file_graph,
        &state.cross_file_meta,
    );

    let mut affected: Vec<Url> = Vec::new();
    let mut affected_set: std::collections::HashSet<Url> = std::collections::HashSet::new();
    for child in wd_children {
        if state.documents.contains_key(&child) && affected_set.insert(child.clone()) {
            affected.push(child);
        }
    }

    // Recompute neighbors against the POST-update graph: any forward/backward
    // edge the new disk content introduces (e.g. a freshly-added `source()`
    // call) — or a restored open-parent edge — is invisible to pre-update
    // walks the caller may have done.
    if graph_result.edges_changed || parent_edges_restored {
        let post_neighbors =
            crate::cross_file::revalidation::compute_affected_dependents_after_edit(
                uri,
                true,
                true,
                &state.cross_file_graph,
                |u| state.documents.contains_key(u),
                state.cross_file_config.max_chain_depth,
                state.cross_file_config.max_transitive_dependents_visited,
            );
        for dep in post_neighbors {
            if affected_set.insert(dep.clone()) {
                affected.push(dep);
            }
        }
        // Edges moved: URIs reachable (or no longer reachable) from open
        // documents changed, so refresh the LRU pin set.
        state.recompute_open_neighborhood_pins();
    }

    ResyncOutcome::Updated {
        affected_after_commit: affected,
        new_interface_hash,
    }
}

/// `did_close`'s disk-resync driver (issue #558): converge `uri`'s cross-file
/// state back to disk truth via [`resync_file_from_disk`], union a
/// pre-commit dependent walk (over the buffer-derived edges, which stay in
/// the graph until this resync's commit replaces them) with the post-commit
/// walks, then cap, force-mark, and publish through the bounded fanout — the
/// same tail the watched-files async pass uses.
///
/// Global bound on concurrent close-resync tasks. Each `did_close` of an R
/// document spawns one `run_close_resync` (disk read, parse, write-lock
/// commit, bounded diagnostics fanout); without a server-wide cap, a "close
/// all" of N files runs N of those concurrently — N write-lock commits
/// contending with keystrokes and up to N x `DIAGNOSTIC_FANOUT_CONCURRENCY`
/// diagnostic workers. Queued resyncs stay correct: a file reopened while
/// its resync waits is caught by the preflight/commit vetoes.
static CLOSE_RESYNC_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// `package_sibling_fanout` carries `did_close`'s package-visibility fanout
/// (open siblings whose diagnostics depend on the closed buffer's package
/// symbols). Folding it in here — instead of `did_close` marking and
/// scheduling it separately — guarantees each URI is force-marked at most
/// once per close (two independent marks would race: the second
/// `run_debounced_diagnostics` schedule cancels the first pending task and
/// strands its marker) and publishes the siblings AFTER the disk commit.
/// The sibling set publishes even on [`ResyncOutcome::Vetoed`] /
/// [`ResyncOutcome::Skipped`]: the package `DidClose` event already committed
/// synchronously inside `did_close`, independent of the cross-file commit.
/// Graph-derived sets (`pre_affected` and the post-commit walks) publish only
/// after a successful commit — on a veto the reopened buffer's own `did_open`
/// pipeline owns them, and on a skip nothing changed. The closed file itself
/// is never in the affected set — `compute_affected_dependents_after_edit`
/// excludes its root — so a close never publishes for the closed document.
///
/// Known counter slack (shared with every mark-then-bounded-publish path,
/// e.g. the watched-files async tail): the gate's force-republish counter is
/// per-URI, not per-trigger, so overlapping closes that share a dependent can
/// strand one marker when a later `run_debounced_diagnostics` schedule
/// cancels an earlier pending task, and a pre-existing pending publish can
/// consume a marker meant for this trigger. The worst case either way is one
/// extra — or one gate-skipped — same-version republish of already-current
/// diagnostics, bounded by `MAX_FORCE_REPUBLISH`; monotonicity is never
/// violated. Trigger-scoped accounting would be a gate redesign, out of
/// scope here.
#[allow(clippy::too_many_arguments)]
async fn run_close_resync(
    state_arc: Arc<RwLock<WorldState>>,
    client: Client,
    traversal_truncation: Arc<TraversalTruncationState>,
    uri: Url,
    chunk_kind: Option<crate::chunks::ChunkKind>,
    old_meta: Option<std::sync::Arc<crate::cross_file::CrossFileMetadata>>,
    old_interface_hash: Option<u64>,
    package_sibling_fanout: Vec<Url>,
) {
    // Bound close-burst concurrency server-wide (see CLOSE_RESYNC_PERMITS).
    // The permit covers the dependent walk, the disk read, the commit, AND
    // the publish fanout.
    let _permit = CLOSE_RESYNC_PERMITS
        .acquire()
        .await
        .expect("static close-resync semaphore is never closed");

    // Pre-commit dependent walk (bounded, up to
    // `max_transitive_dependents_visited` nodes), off the handler path so a
    // close never delays the next queued LSP message with it. The graph
    // still holds the closed buffer's edges: this resync is the only thing
    // that replaces them, and it runs strictly after this walk.
    let pre_affected: Vec<Url> = {
        let state = state_arc.read().await;
        crate::cross_file::revalidation::compute_affected_dependents_after_edit(
            &uri,
            true,
            false,
            &state.cross_file_graph,
            |u| state.documents.contains_key(u),
            state.cross_file_config.max_chain_depth,
            state.cross_file_config.max_transitive_dependents_visited,
        )
    };

    let (post, interface_changed, committed) =
        match resync_file_from_disk(&state_arc, &uri, chunk_kind, old_meta, true).await {
            ResyncOutcome::Updated {
                affected_after_commit,
                new_interface_hash,
            } => {
                // `None` (no buffer artifacts survived to the capture) is
                // treated as changed — the safe direction.
                let changed = old_interface_hash != Some(new_interface_hash);
                (affected_after_commit, changed, true)
            }
            ResyncOutcome::Removed {
                affected_dependents,
            } => (affected_dependents, false, true),
            ResyncOutcome::Vetoed | ResyncOutcome::Skipped => (Vec::new(), false, false),
        };

    let mut affected: Vec<Url> = Vec::new();
    let mut seen: std::collections::HashSet<Url> = std::collections::HashSet::new();
    for dep in package_sibling_fanout {
        if seen.insert(dep.clone()) {
            affected.push(dep);
        }
    }
    if committed {
        for dep in pre_affected.into_iter().chain(post) {
            if seen.insert(dep.clone()) {
                affected.push(dep);
            }
        }
    }

    {
        let state = state_arc.read().await;
        // The pre-close walk saw the graph as it stood at close time; when
        // the committed disk interface differs from the buffer's, re-walk
        // over the POST-commit graph too (mirroring did_change's
        // interface_changed fanout). This catches dependents reachable only
        // through edges another concurrent resync restored between the
        // capture and this commit — e.g. closing a parent and child together
        // where the parent's disk content re-adds the parent -> child edge.
        if committed && interface_changed {
            let post_commit_deps =
                crate::cross_file::revalidation::compute_affected_dependents_after_edit(
                    &uri,
                    true,
                    false,
                    &state.cross_file_graph,
                    |u| state.documents.contains_key(u),
                    state.cross_file_config.max_chain_depth,
                    state.cross_file_config.max_transitive_dependents_visited,
                );
            for dep in post_commit_deps {
                if seen.insert(dep.clone()) {
                    affected.push(dep);
                }
            }
        }
        // Drop dependents that closed during the async gap: marking a closed
        // URI would leave an orphan force-republish counter for a later
        // reopen to inherit (`did_close` clears the gate only at ITS close).
        affected.retain(|u| state.documents.contains_key(u));
        if affected.is_empty() {
            return;
        }
        cap_watched_file_revalidations(
            &mut affected,
            &state.cross_file_activity,
            state.cross_file_config.max_revalidations_per_trigger,
        );
        state
            .diagnostics_gate
            .mark_force_republish_many(affected.iter());
    }
    Backend::publish_diagnostics_for_uris_bounded(
        state_arc,
        client,
        affected,
        Some(traversal_truncation),
    )
    .await;
}

/// Sort watched-file diagnostic fanout by activity and enforce the configured
/// per-trigger cap before any force-republish markers are created.
fn cap_watched_file_revalidations(
    affected: &mut Vec<Url>,
    activity: &crate::cross_file::revalidation::CrossFileActivityState,
    max_revalidations: usize,
) {
    affected.sort_by_cached_key(|u| activity.priority_score(u).saturating_add(1));
    if affected.len() > max_revalidations {
        log::trace!(
            "Watched-files revalidation cap exceeded: {} affected, scheduling {}",
            affected.len(),
            max_revalidations
        );
        affected.truncate(max_revalidations);
    }
}

/// Merge post-update neighbors into the existing scheduled `prev_uris` set,
/// re-sort by activity priority (with `pinned_uri` always first), and
/// truncate to `max_revalidations`. Returns the deduped, priority-sorted,
/// capped union.
///
/// Used by both `did_open` re-enrichment paths (on-demand-indexing and
/// non-on-demand branches). The original implementation appended new
/// neighbors only when `prev_uris.len() < max_revalidations`. Since the
/// initial scheduling pass already truncates to the cap, the loop body
/// never executed when the cap was full, silently dropping newly reachable
/// neighbors regardless of priority.
fn merge_and_cap_reenrichment_revalidations(
    pinned_uri: &Url,
    prev_uris: std::collections::HashSet<Url>,
    new_neighbors: Vec<Url>,
    max_revalidations: usize,
    activity: &crate::cross_file::revalidation::CrossFileActivityState,
) -> Vec<Url> {
    let mut union: Vec<Url> = prev_uris.iter().cloned().collect();
    let mut seen = prev_uris;
    for dep in new_neighbors {
        if seen.insert(dep.clone()) {
            union.push(dep);
        }
    }
    union.sort_by_cached_key(|u| {
        if u == pinned_uri {
            0
        } else {
            activity.priority_score(u).saturating_add(1)
        }
    });
    union.truncate(max_revalidations);
    union
}

/// Rebuild `work_items` for the `did_open` re-enrichment paths after the
/// dependency graph has changed: derive `prev_uris` from the current
/// `work_items`, merge in the new neighbors via
/// [`merge_and_cap_reenrichment_revalidations`], and snapshot each surviving
/// URI's current `(version, revision)` from `state.documents` for the
/// freshness guard in `run_debounced_diagnostics`.
///
/// Both `did_open` re-enrichment branches (on-demand-indexing and
/// non-on-demand) share this exact shape. Force-republish marking is NOT
/// done here — it is deferred to a single end-of-flow site so URIs evicted
/// by the cap don't carry orphaned force counters.
fn rebuild_work_items_after_reenrichment(
    pinned_uri: &Url,
    prev_work_items: &[(Url, Option<i32>, Option<u64>)],
    new_neighbors: Vec<Url>,
    state: &WorldState,
) -> Vec<(Url, Option<i32>, Option<u64>)> {
    let max_revalidations = state.cross_file_config.max_revalidations_per_trigger;
    let prev_uris: std::collections::HashSet<Url> =
        prev_work_items.iter().map(|(u, _, _)| u.clone()).collect();
    let final_uris = merge_and_cap_reenrichment_revalidations(
        pinned_uri,
        prev_uris,
        new_neighbors,
        max_revalidations,
        &state.cross_file_activity,
    );
    final_uris
        .into_iter()
        .map(|u| {
            let doc = state.documents.get(&u);
            let trigger_version = doc.and_then(|d| d.version);
            let trigger_revision = doc.map(|d| d.revision);
            (u, trigger_version, trigger_revision)
        })
        .collect()
}

async fn check_and_warn_traversal_truncation(
    state_arc: &Arc<RwLock<WorldState>>,
    client: &Client,
    traversal_truncation: &TraversalTruncationState,
) {
    let (visited_budget_total, depth_total, max_visited, max_chain_depth) = {
        let state = state_arc.read().await;
        (
            state.cross_file_graph.visited_budget_truncations(),
            state.cross_file_graph.depth_truncations(),
            state.cross_file_config.max_transitive_dependents_visited,
            state.cross_file_config.max_chain_depth,
        )
    };

    let delta = traversal_truncation.consume_delta(visited_budget_total, depth_total);
    if delta.total() == 0 {
        return;
    }

    if delta.visited_budget > 0 {
        log::warn!(
            "Cross-file traversal visited-budget limit hit {} time(s); max_transitive_dependents_visited={}",
            delta.visited_budget,
            max_visited
        );
        if traversal_truncation.should_show_visited_budget_notice() {
            client
                .show_message(
                    MessageType::WARNING,
                    format!(
                        "Raven: this workspace's R dependency graph exceeded the cross-file analysis budget (visited the maximum of {max_visited} files), so some cross-file diagnostics may be incomplete. Increase `raven.crossFile.maxTransitiveDependentsVisited` (current: {max_visited}) to analyze more files."
                    ),
                )
                .await;
        }
    }

    if delta.depth > 0 {
        log::debug!(
            "Cross-file traversal depth limit hit {} time(s); max_chain_depth={}",
            delta.depth,
            max_chain_depth
        );
    }
}

/// Run debounced diagnostics for a single URI.
///
/// This is the shared diagnostics pipeline used by both `did_open`/`did_change`
/// work-item loops and post-prefetch revalidation callbacks. It handles:
/// scheduling → debounce/cancel → freshness check → compute → async file checks → publish.
async fn run_debounced_diagnostics(
    state_arc: Arc<RwLock<WorldState>>,
    client: Client,
    affected_uri: Url,
    debounce_ms: u64,
    trigger_version: Option<i32>,
    trigger_revision: Option<u64>,
    traversal_truncation: Option<Arc<TraversalTruncationState>>,
) {
    // Schedule with cancellation token
    let token = {
        let state = state_arc.read().await;
        state.cross_file_revalidation.schedule(affected_uri.clone())
    };

    // Clone token before select so we can pass it into diagnostic computation
    let cancel = handlers::DiagCancelToken::from_token(token.clone());

    // Debounce / cancellation
    tokio::select! {
        _ = token.cancelled() => { return; }
        _ = tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)) => {}
    }

    // Build snapshot under the read lock (brief hold), then compute diagnostics outside
    let snapshot_data = {
        let state = state_arc.read().await;

        let doc = state.documents.get(&affected_uri);
        let current_version = doc.and_then(|d| d.version);
        let current_revision = doc.map(|d| d.revision);

        if current_version != trigger_version || current_revision != trigger_revision {
            log::trace!(
                "Skipping stale diagnostics for {}: revision changed",
                affected_uri
            );
            return;
        }

        if let Some(ver) = current_version
            && !state.diagnostics_gate.can_publish(&affected_uri, ver)
        {
            log::trace!("Skipping diagnostics for {}: monotonic gate", affected_uri);
            return;
        }

        // Build the snapshot (captures all state needed for diagnostics)
        let snapshot = handlers::DiagnosticsSnapshot::build(&state, &affected_uri);
        let workspace_folder = state.workspace_folders.first().cloned();
        let missing_file_severity = state.cross_file_config.missing_file_severity;

        snapshot.map(|s| (s, workspace_folder, missing_file_severity))
    }; // Read lock released here

    let Some((snapshot, workspace_folder, missing_file_severity)) = snapshot_data else {
        return;
    };

    if let Some(traversal_truncation) = traversal_truncation.as_deref() {
        check_and_warn_traversal_truncation(&state_arc, &client, traversal_truncation).await;
    }

    // Compute diagnostics WITHOUT holding any lock
    let sync_diagnostics =
        match handlers::diagnostics_from_snapshot(&snapshot, &affected_uri, &cancel) {
            Some(diags) => diags,
            None => {
                log::trace!("Diagnostics cancelled for {}", affected_uri);
                return;
            }
        };

    if cancel.is_cancelled() {
        log::trace!(
            "Diagnostics cancelled before async phase for {}",
            affected_uri
        );
        return;
    }

    // Perform async missing file existence checks (non-blocking I/O)
    let diagnostics = handlers::diagnostics_async_standalone(
        &affected_uri,
        sync_diagnostics,
        &snapshot.directive_meta,
        workspace_folder.as_ref(),
        missing_file_severity,
        snapshot.cross_file_config.case_mismatch_severity,
    )
    .await;

    if cancel.is_cancelled() {
        log::trace!(
            "Diagnostics cancelled after async phase for {}",
            affected_uri
        );
        return;
    }

    // Second freshness check + atomic gate commit before publishing.
    // try_consume_publish takes write locks on the gate's maps, evaluates the
    // same predicate as can_publish, and on success updates last_published
    // and consumes one force-republish marker — closing the race where
    // two same-version publishes could share one marker.
    let can_publish = {
        let state = state_arc.read().await;
        let doc = state.documents.get(&affected_uri);
        let current_version = doc.and_then(|d| d.version);
        let current_revision = doc.map(|d| d.revision);

        if current_version != trigger_version || current_revision != trigger_revision {
            false
        } else if let Some(ver) = current_version {
            state
                .diagnostics_gate
                .try_consume_publish(&affected_uri, ver)
        } else {
            true
        }
    };

    if can_publish {
        client
            .publish_diagnostics(affected_uri.clone(), diagnostics, None)
            .await;

        let state = state_arc.read().await;
        state.cross_file_revalidation.complete(&affected_uri);
    }
}

pub(crate) enum RavenProjectConfigLoaded {}

impl tower_lsp::lsp_types::notification::Notification for RavenProjectConfigLoaded {
    type Params = serde_json::Value;
    const METHOD: &'static str = "raven/projectConfigLoaded";
}

/// Build the `raven/projectConfigLoaded` payload. Pure function so it can
/// be unit-tested without spinning up an LSP service. The notification
/// payload schema is:
///   - `path: string | null` — absolute path of the active project
///     config, or `null` when no config is in effect.
///   - `source: "raven.toml" | ".lintr" | null` — discriminator derived
///     from the file name; `null` when `path` is `null`.
fn build_project_config_loaded_payload(path: Option<&std::path::Path>) -> serde_json::Value {
    match path {
        Some(p) => {
            let source = crate::config_file::ConfigFileKind::from_path(p)
                .unwrap_or(crate::config_file::ConfigFileKind::RavenToml)
                .source_name();
            serde_json::json!({
                "path": p.display().to_string(),
                "source": source,
            })
        }
        None => serde_json::json!({
            "path": serde_json::Value::Null,
            "source": serde_json::Value::Null,
        }),
    }
}

fn log_project_config_warnings(layer: &crate::config_file::DiscoveredLoad) {
    if let crate::config_file::DiscoveredLoad::Loaded { warnings, .. } = layer {
        for warning in warnings {
            log::warn!("{warning}");
        }
    }
}

fn apply_project_config_layer(state: &mut WorldState, layer: crate::config_file::DiscoveredLoad) {
    let loaded_project = match layer {
        crate::config_file::DiscoveredLoad::Loaded { path, settings, .. } => Some((path, settings)),
        crate::config_file::DiscoveredLoad::LoadFailed { .. }
        | crate::config_file::DiscoveredLoad::None => None,
    };

    state.raw_project_settings = None;
    state.project_config_path = None;
    if let Some((path, settings)) = loaded_project {
        state.raw_project_settings = Some(settings);
        state.project_config_path = Some(path);
    }
}

fn is_project_config_file(path: &std::path::Path) -> bool {
    crate::config_file::ConfigFileKind::from_path(path).is_some()
}

enum ProjectConfigWatcherPolicy<'a> {
    WorkspaceRoot,
    Discovery(&'a crate::config_file::DiscoveryOptions),
}

fn project_config_watchers_for_dir(
    dir: &std::path::Path,
    policy: ProjectConfigWatcherPolicy<'_>,
) -> Vec<FileSystemWatcher> {
    let Ok(parent_uri) = Url::from_file_path(dir) else {
        return Vec::new();
    };

    crate::config_file::ConfigFileKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let file_name = kind.file_name();
            let candidate = dir.join(file_name);
            match policy {
                ProjectConfigWatcherPolicy::WorkspaceRoot => {}
                ProjectConfigWatcherPolicy::Discovery(options)
                    if !options.allows_config_path(&candidate) =>
                {
                    return None;
                }
                ProjectConfigWatcherPolicy::Discovery(_) => {}
            }
            Some(FileSystemWatcher {
                glob_pattern: GlobPattern::Relative(RelativePattern {
                    base_uri: OneOf::Right(parent_uri.clone()),
                    pattern: file_name.into(),
                }),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            })
        })
        .collect()
}

fn build_workspace_project_config_watchers(workspace_folders: &[Url]) -> Vec<FileSystemWatcher> {
    let mut dirs = std::collections::BTreeSet::new();

    for workspace in workspace_folders.iter().take(1) {
        let Ok(root) = workspace.to_file_path() else {
            continue;
        };
        dirs.insert(root);
    }

    dirs.into_iter()
        .flat_map(|dir| {
            project_config_watchers_for_dir(&dir, ProjectConfigWatcherPolicy::WorkspaceRoot)
        })
        .collect()
}

fn is_inside_active_project_workspace(path: &std::path::Path, workspace_folders: &[Url]) -> bool {
    workspace_folders.iter().take(1).any(|workspace| {
        workspace
            .to_file_path()
            .map(|root| path.starts_with(root))
            .unwrap_or(false)
    })
}

fn external_project_config_watch_dirs(workspace_folders: &[Url]) -> Vec<std::path::PathBuf> {
    let mut dirs = std::collections::BTreeSet::new();

    for workspace in workspace_folders.iter().take(1) {
        let Ok(root) = workspace.to_file_path() else {
            continue;
        };
        for dir in crate::config_file::discovery::ancestor_dirs(&root) {
            if !is_inside_active_project_workspace(dir, workspace_folders) {
                dirs.insert(dir.to_path_buf());
            }
        }
    }

    dirs.into_iter().collect()
}

fn build_external_project_config_watchers(
    workspace_folders: &[Url],
    discovery_options: &crate::config_file::DiscoveryOptions,
) -> Vec<FileSystemWatcher> {
    external_project_config_watch_dirs(workspace_folders)
        .into_iter()
        .flat_map(|dir| {
            project_config_watchers_for_dir(
                &dir,
                ProjectConfigWatcherPolicy::Discovery(discovery_options),
            )
        })
        .collect()
}

fn config_file_event_affects_project_config(
    path: &std::path::Path,
    project_root: Option<&std::path::Path>,
    discovery_options: &crate::config_file::DiscoveryOptions,
) -> bool {
    if !is_project_config_file(path) || !discovery_options.allows_config_path(path) {
        return false;
    }

    let Some(project_root) = project_root else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };

    crate::config_file::discovery::ancestor_dirs(project_root).any(|dir| dir == parent)
}

fn external_project_config_watch_changed(
    workspace_folders: &[Url],
    old_options: &crate::config_file::DiscoveryOptions,
    new_options: &crate::config_file::DiscoveryOptions,
) -> bool {
    build_external_project_config_watchers(workspace_folders, old_options)
        != build_external_project_config_watchers(workspace_folders, new_options)
}

/// Pre-recompute snapshot of the parsed configs that drive change-detection
/// inside [`Backend::reconcile_after_config_recompute`].
///
/// Captured by the caller under a write lock, **before** the caller mutates
/// `raw_client_settings` / `raw_project_settings` and calls
/// [`crate::config_file::recompute_parsed_configs`]. The helper diffs the
/// post-recompute `state.*_config` values against this snapshot to decide
/// which downstream rebuilds to run.
#[derive(Debug, Clone)]
struct ConfigChangeSnapshot {
    prev_cross_file: crate::cross_file::CrossFileConfig,
    prev_lint: crate::linting::LintConfig,
    prev_completion: crate::state::CompletionConfig,
    /// `recompute_parsed_configs` resets `symbol_config` to defaults. The
    /// helper restores `hierarchical_document_symbol_support` from this
    /// value (set from client capabilities at initialize time).
    prev_hier_support: bool,
}

/// Flags + carried state derived under the write lock inside
/// [`Backend::reconcile_after_config_recompute`], consumed by the
/// out-of-lock work below.
///
/// Replaces what was a 12-element tuple. Don't make it `pub` — every
/// field is internal to one helper.
#[derive(Debug)]
struct ReconciliationDecisions {
    scope_changed: bool,
    package_settings_changed: bool,
    watch_settings_changed: bool,
    only_watch_changed: bool,
    diagnostics_enabled_changed: bool,
    old_diagnostics_enabled: bool,
    new_diagnostics_enabled: bool,
    packages_enabled: bool,
    max_transitive_dependents_visited_changed: bool,
    trigger_on_open_paren_changed: bool,
    new_trigger_on_open_paren: bool,
    /// `Some(root)` when `packageMode` flipped to a non-Disabled mode and
    /// the helper still needs to read `DESCRIPTION` / `NAMESPACE` from
    /// disk before re-applying. `None` otherwise (no mode change, or the
    /// mode change was already applied in-lock for Disabled).
    pkg_mode_io_needed: Option<std::path::PathBuf>,
    open_uris: Vec<Url>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    /// Initializes the server state from the client's InitializeParams and returns the LSP
    /// InitializeResult advertising the server's capabilities (text sync, folding, symbols,
    /// completion triggers, hover, signature help, definition/references, formatting, workspace symbols, etc.).
    ///
    /// The method records workspace folders from the params into shared state before constructing
    /// the capabilities and server information.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use tower_lsp::lsp_types::InitializeParams;
    /// # use tower_lsp::LanguageServer;
    /// # async fn example(backend: &raven::backend::Backend) -> tower_lsp::lsp_types::InitializeResult {
    /// let params = InitializeParams::default();
    /// // run inside an async context (tokio/runtime)
    /// backend.initialize(params).await.unwrap()
    /// # }
    /// ```
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        log::info!("Initializing ark-lsp");

        // First lock window: register workspace folders, snapshot the root for
        // off-lock discovery + I/O.
        let project_root: Option<std::path::PathBuf> = {
            let mut state = self.state.write().await;
            if let Some(folders) = params.workspace_folders.clone() {
                for folder in folders {
                    log::info!("Adding workspace folder: {}", folder.uri);
                    state.workspace_folders.push(folder.uri);
                }
            } else if let Some(root_uri) = params.root_uri.clone() {
                log::info!("Adding root URI as workspace folder: {}", root_uri);
                state.workspace_folders.push(root_uri);
            }
            state
                .workspace_folders
                .first()
                .and_then(|u| u.to_file_path().ok())
        };

        // OFF-LOCK: filesystem walk + TOML read. Holding the write lock across
        // file I/O violates the locking-discipline invariant in CLAUDE.md.
        let raw_client = params
            .initialization_options
            .clone()
            .unwrap_or(serde_json::Value::Null);
        let discovery_options = self.discovery_options_from_client_settings(&raw_client);
        let project_layer = project_root
            .as_deref()
            .map(|root| {
                crate::config_file::discover_and_load_with_options(root, &discovery_options)
            })
            .unwrap_or(crate::config_file::DiscoveredLoad::None);
        log_project_config_warnings(&project_layer);

        // Second lock window: store raw layers, recompute parsed configs,
        // compile overrides. No I/O in this scope. The discovered path
        // (if any) is now read back from `state.project_config_path` in
        // `initialized()` — sending the notification from here would
        // violate the LSP spec (see note above).
        {
            let mut state = self.state.write().await;
            state.raw_client_settings = raw_client;
            apply_project_config_layer(&mut state, project_layer);
            // `recompute_parsed_configs` now also recompiles
            // `state.lint_overrides` — callers no longer need a
            // separate `compile_lint_overrides` step.
            crate::config_file::recompute_parsed_configs(&mut state);
        }

        // NOTE: the `raven/projectConfigLoaded` notification is NOT sent
        // from here. Per the LSP spec, the server MUST NOT send any
        // requests or notifications to the client before responding to
        // `initialize` (only `window/showMessage`, `window/logMessage`,
        // `telemetry/event`, `window/showMessageRequest`, and
        // `$/progress` are allowed during the initialization phase).
        // The matching emit lives in `initialized()` below — by then
        // the handshake is guaranteed to be complete and the client
        // will reliably route the custom notification.

        // Detect client capability for hierarchical document symbols
        // Requirements 1.1, 1.2: Response type selection based on client capability
        // Path: params.capabilities.text_document.document_symbol.hierarchical_document_symbol_support
        let hierarchical_support = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|td| td.document_symbol.as_ref())
            .and_then(|ds| ds.hierarchical_document_symbol_support)
            .unwrap_or(false);

        log::info!(
            "Client hierarchicalDocumentSymbolSupport: {}",
            hierarchical_support
        );

        // Third lock window: store the hierarchical-support flag and read the
        // completion trigger setting. Short and lock-only — no I/O.
        let trigger_on_open_paren = {
            let mut state = self.state.write().await;
            state.symbol_config.hierarchical_document_symbol_support = hierarchical_support;
            state.completion_config.trigger_on_open_paren
        };

        let completion_trigger_chars = build_completion_trigger_chars(trigger_on_open_paren);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(completion_trigger_chars),
                    resolve_provider: Some(true),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec![String::from("("), String::from(",")]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                document_on_type_formatting_provider: Some(
                    indentation::on_type_formatting_capability(),
                ),
                semantic_tokens_provider: Some(semantic_tokens_capability()),
                execute_command_provider: Some(tower_lsp::lsp_types::ExecuteCommandOptions {
                    commands: vec![],
                    ..Default::default()
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: String::from("ark-lsp"),
                version: Some(String::from(env!("CARGO_PKG_VERSION"))),
            }),
        })
    }

    /// Performs post-initialization work for the language server.
    ///
    /// This method performs workspace startup tasks: it scans configured workspace folders and applies the discovered workspace index to shared state, and it initializes or replaces the in-memory PackageLibrary according to the current cross-file configuration (enabling package-aware features when configured). All long-running or blocking operations are performed without holding the main WorldState write lock so startup does not block other LSP activity.
    ///
    /// # Examples
    ///
    /// ```text
    /// // Assuming `backend` is an initialized `Backend` and a Tokio runtime is available:
    /// // tokio::spawn(async move {
    /// //     backend.initialized(lsp_types::InitializedParams {}).await;
    /// // });
    /// ```
    async fn initialized(&self, _: InitializedParams) {
        log::info!("ark-lsp initialized");
        let init_start = std::time::Instant::now();

        // Emit the project-config-loaded notification deferred from
        // `initialize()` (LSP requires the handshake to complete before
        // any custom notifications). Skipped here when no config was
        // discovered; the watched-files reload path emits the
        // cleared-config form (`path: null`) once the user actually
        // removes the file.
        let loaded_path = self.state.read().await.project_config_path.clone();
        if let Some(path) = &loaded_path {
            self.notify_project_config_loaded(Some(path.as_path()));
        }

        // Get workspace folders and config under brief lock
        let (
            folders,
            max_chain_depth,
            packages_enabled,
            packages_r_path,
            additional_paths,
            index_workspace,
        ) = {
            let state = self.state.read().await;
            (
                state.workspace_folders.clone(),
                state.cross_file_config.max_chain_depth,
                state.cross_file_config.packages_enabled,
                state.cross_file_config.packages_r_path.clone(),
                state
                    .cross_file_config
                    .packages_additional_library_paths
                    .clone(),
                state.cross_file_config.index_workspace,
            )
        };

        // Task A: Spawn background workspace scan (don't await - runs in background)
        // This allows LSP to respond to requests immediately while scan completes.
        // Files the user opens will be indexed on-demand via did_open, which takes
        // priority over the background scan.
        let state_clone = self.state.clone();
        let folders_clone = folders.clone();
        // Capture the workspace root before spawning the blocking scan so we can
        // read DESCRIPTION/NAMESPACE outside the WorldState write lock below.
        let root_for_pkg_inputs: Option<std::path::PathBuf> =
            folders.first().and_then(|u| u.to_file_path().ok());
        let client_clone = self.client.clone();
        let traversal_truncation = self.traversal_truncation.clone();
        if index_workspace {
            tokio::task::spawn(async move {
                // Run the blocking scan in a blocking task
                let scan_result = tokio::task::spawn_blocking(move || {
                    let scan_start = std::time::Instant::now();
                    let result = scan_workspace(&folders_clone, max_chain_depth);
                    let scan_duration = scan_start.elapsed();
                    let file_count = result.0.len();
                    crate::perf::record_workspace_scan(scan_duration, file_count);
                    log::info!(
                        "[Background] Workspace scan complete: {} files in {:?}",
                        file_count,
                        scan_duration
                    );
                    result
                })
                .await;

                // Apply results when scan completes
                match scan_result {
                    Ok((index, cross_file_entries, new_index_entries)) => {
                        // Read DESCRIPTION and NAMESPACE BEFORE acquiring the
                        // WorldState write lock — `std::fs::read_to_string` is
                        // blocking and would stall any concurrent writer (e.g.
                        // an in-flight `did_change`) for the duration of disk
                        // I/O. Files were already touched during the blocking
                        // scan above, so the read is typically a warm cache hit.
                        // Also precompute the `.Rprofile` prelude scan here,
                        // OFF the write lock — it follows transitive source()
                        // chains and must not run under the lock (see
                        // `initialize_package_inputs_from_state`).
                        let (desc_text, ns_text, rprofile_scan) =
                            if let Some(ref root) = root_for_pkg_inputs {
                                let desc = std::fs::read_to_string(root.join("DESCRIPTION"))
                                    .ok()
                                    .map(|t| std::sync::Arc::from(t.as_str()));
                                let ns = std::fs::read_to_string(root.join("NAMESPACE"))
                                    .ok()
                                    .map(|t| std::sync::Arc::from(t.as_str()));
                                let scan =
                                    crate::package_state::rprofile::scan_workspace_rprofile(root);
                                (desc, ns, Some(scan))
                            } else {
                                (None, None, None)
                            };

                        // Apply index and snapshot trigger versions under a single write lock
                        let (work_items, debounce_ms, pkg_lib, packages_enabled) = {
                            let mut state = state_clone.write().await;
                            state.apply_workspace_index(
                                index,
                                cross_file_entries,
                                new_index_entries,
                            );
                            if let Some(root) = root_for_pkg_inputs.clone() {
                                // Populate package_inputs from the now-applied
                                // workspace index and overlay open documents
                                // (authoritative unsaved edits).
                                initialize_package_inputs_from_state(
                                    &mut state,
                                    root,
                                    desc_text,
                                    ns_text,
                                    Default::default(),
                                    rprofile_scan,
                                );
                            } else {
                                state.apply_package_event(
                                    &crate::package_state::PackageInputDelta::Initial,
                                );
                            }
                            // Mark force republish for all open documents so they pick up
                            // newly discovered backward edges from the dependency graph
                            // and snapshot trigger versions while we hold the lock.
                            // Bulk-mark first to avoid per-URI lock churn on large
                            // workspaces.
                            let open_keys: Vec<Url> = state.documents.keys().cloned().collect();
                            state
                                .diagnostics_gate
                                .mark_force_republish_many(open_keys.iter());
                            let items: Vec<(Url, Option<i32>, Option<u64>)> = open_keys
                                .into_iter()
                                .map(|uri| {
                                    let doc = state.documents.get(&uri);
                                    let v = doc.and_then(|d| d.version);
                                    let r = doc.map(|d| d.revision);
                                    (uri, v, r)
                                })
                                .collect();
                            let debounce = state.cross_file_config.revalidation_debounce_ms;
                            let pkg_lib = state.package_library.clone();
                            let pkgs_enabled = state.cross_file_config.packages_enabled
                                && state.package_library_ready;
                            (items, debounce, pkg_lib, pkgs_enabled)
                        };
                        log::info!("[Background] Workspace index applied");

                        // Warm the package cache for inherited packages newly visible
                        // via backward edges discovered by the workspace scan. Without
                        // this, open documents whose scope inherits packages from
                        // now-indexed parent files see those packages as
                        // installed-but-uncached, and `package_cache_pending` suppresses
                        // undefined-variable diagnostics for genuinely-uninstalled
                        // packages — e.g. `library(lme4); lmer()` is silenced if the
                        // parent chain loads other (installed but uncached) packages.
                        //
                        // If `packages_enabled` was false at capture time the scan
                        // raced ahead of Task B (PackageLibrary init); the captured
                        // `pkg_lib` is the empty default Arc and Task B has since
                        // swapped `state.package_library` with a fresh Arc. Re-read
                        // the live library so prefetch warms the right cache instead
                        // of an orphaned one.
                        let (effective_pkg_lib, effective_packages_enabled) = if packages_enabled {
                            (pkg_lib, true)
                        } else {
                            let state = state_clone.read().await;
                            let enabled = state.cross_file_config.packages_enabled
                                && state.package_library_ready;
                            let live = state.package_library.clone();
                            if !std::sync::Arc::ptr_eq(&pkg_lib, &live) {
                                log::trace!(
                                    "Post-scan prefetch: package_library swapped between scan capture and prefetch (packages_enabled now {})",
                                    enabled
                                );
                            }
                            (live, enabled)
                        };
                        if effective_packages_enabled {
                            prefetch_packages_for_open_documents(&state_clone, &effective_pkg_lib)
                                .await;
                        }

                        // Revalidate all open documents to pick up auto-detected backward edges
                        for (uri, trigger_version, trigger_revision) in work_items {
                            let state_arc = state_clone.clone();
                            let client = client_clone.clone();
                            let traversal_truncation = traversal_truncation.clone();
                            tokio::spawn(run_debounced_diagnostics(
                                state_arc,
                                client,
                                uri,
                                debounce_ms,
                                trigger_version,
                                trigger_revision,
                                Some(traversal_truncation),
                            ));
                        }
                    }
                    Err(e) => {
                        log::error!("Background workspace scan task failed: {}", e);
                    }
                }
            });
        } else {
            let package_seed = if let Some(root) = root_for_pkg_inputs.clone() {
                let root_clone = root.clone();
                Some((
                    root,
                    tokio::task::spawn_blocking(move || {
                        let desc = std::fs::read_to_string(root_clone.join("DESCRIPTION"))
                            .ok()
                            .map(|s| Arc::from(s.as_str()));
                        let ns = std::fs::read_to_string(root_clone.join("NAMESPACE"))
                            .ok()
                            .map(|s| Arc::from(s.as_str()));
                        let disk_r_files = collect_package_r_file_inputs_from_disk(&root_clone);
                        // Precompute the `.Rprofile` prelude scan OFF the write
                        // lock (it follows transitive source(); see
                        // `initialize_package_inputs_from_state`).
                        let rprofile_scan =
                            crate::package_state::rprofile::scan_workspace_rprofile(&root_clone);
                        (desc, ns, disk_r_files, rprofile_scan)
                    })
                    .await
                    .unwrap_or((
                        None,
                        None,
                        Default::default(),
                        Default::default(),
                    )),
                ))
            } else {
                None
            };

            // No workspace scan — mark complete immediately and still seed
            // package inputs from disk/open documents so package mode starts
            // with a derived state.
            let mut state = self.state.write().await;
            state.workspace_scan_complete = true;
            if let Some((root, (desc_text, ns_text, disk_r_files, rprofile_scan))) = package_seed {
                initialize_package_inputs_from_state(
                    &mut state,
                    root,
                    desc_text,
                    ns_text,
                    disk_r_files,
                    Some(rprofile_scan),
                );
            }
        }

        // Task B: Initialize PackageLibrary (await this - diagnostics need it)
        // This is fast (~100ms) due to batched R subprocess calls.
        let (new_package_library, package_library_ready, load_notes) = {
            let pkg_start = std::time::Instant::now();
            let r_calls_before = crate::perf::get_r_subprocess_calls();

            // Get workspace root from folders (if available) for R working
            // directory (e.g. for renv).
            let workspace_root = folders.first().and_then(|url| url.to_file_path().ok());

            // The shared helper self-gates on `packages_enabled` (returning
            // `Disabled` with an empty library and no R discovery), moves R
            // discovery into spawn_blocking, and computes readiness after
            // applying additional paths. Perf metrics, the count log, and the
            // "disabled" log stay here — the helper never logs.
            let outcome = crate::package_library::build_package_library(
                packages_r_path,
                &additional_paths,
                workspace_root,
                packages_enabled,
            )
            .await;

            // Captured here before `outcome` is destructured; surfaced after the
            // commit below, and only if this path wins the race.
            let package_library_ready = outcome.consumer_ready();
            let load_notes = outcome.load_notes;

            use crate::package_library::PackageLibraryStatus;
            let status = outcome.status;
            let library = outcome.library;
            if status == PackageLibraryStatus::Disabled {
                log::info!("Package function awareness disabled");
                (library, false, load_notes)
            } else {
                if let PackageLibraryStatus::InitFailed(e) = &status {
                    log::warn!("Failed to initialize PackageLibrary: {}", e);
                }
                let pkg_duration = pkg_start.elapsed();
                let r_calls = crate::perf::get_r_subprocess_calls() - r_calls_before;
                crate::perf::record_package_init(pkg_duration, r_calls);

                log::info!(
                    "PackageLibrary initialized: {} lib_paths, {} base_packages, {} base_exports",
                    library.lib_paths().len(),
                    library.base_packages().len(),
                    library.base_exports().len()
                );
                (library, package_library_ready, load_notes)
            }
        };

        // Apply PackageLibrary only if not already initialized.
        // `ensure_package_library_initialized` (called from `did_open`) may have raced
        // ahead and already written a library with prefetched package caches.
        // Overwriting it would discard those caches and cause false-positive
        // "Package is not installed" diagnostics until the next prefetch.
        let committed = {
            let mut state = self.state.write().await;
            if !state.package_library_ready {
                state.package_library = new_package_library;
                // The fresh library starts with no local-dev overlay; re-install
                // it so load_all sentinel resolution survives the swap.
                state.refresh_local_dev_overlay();
                state.package_library_ready = package_library_ready;
                state.bump_package_config_generation();
                true
            } else {
                log::info!(
                    "PackageLibrary already initialized (from did_open), skipping overwrite"
                );
                false
            }
        };
        // Surface Tier 2/3 load notes only if this path committed the library,
        // so the did_open race (`ensure_package_library_initialized`) doesn't
        // double-toast the same notes.
        if committed {
            self.surface_load_notes(&load_notes).await;
        }

        // Re-resolve system.file() sources now that lib_paths are available.
        // The workspace scan (Task A) may have called resolve_system_file_in_workspace()
        // before the library was ready, leaving branch-2 (installed-package)
        // targets unresolved. This second pass picks them up.
        if package_library_ready {
            let mut state = self.state.write().await;
            state.resolve_system_file_in_workspace();
        }

        // Start the libpath watcher if enabled and we have a real package
        // library. `lib_paths` is captured here as a one-time snapshot — if the
        // user later changes `.libPaths()` mid-session (e.g. `renv` switches
        // projects, `.Rprofile` is edited, or `.libPaths(new=...)` is called),
        // the watcher keeps watching the originally-captured directories. The
        // documented workaround is the `raven.refreshPackages` command, which
        // rebuilds `PackageLibrary` (re-running `.libPaths()`) and restarts the
        // watcher over the newly-discovered paths. See `docs/packages.md`.
        restart_libpath_watcher(&self.state, &self.client, true).await;

        // R fallback for sysdata: when the AST scan found nothing AND
        // R/sysdata.rda exists, try loading via an R subprocess.
        {
            let needs_fallback = {
                let state = self.state.read().await;
                sysdata_r_fallback_needed(&state)
            };
            if needs_fallback {
                let state_arc = self.state.clone();
                let client = self.client.clone();
                let traversal_truncation = self.traversal_truncation.clone();
                tokio::spawn(async move {
                    let (r_path, workspace_root) = {
                        let state = state_arc.read().await;
                        let r = state
                            .package_library
                            .r_subprocess()
                            .map(|r| r.r_path().clone());
                        let root = state.package_inputs.workspace_root.clone();
                        (r, root)
                    };
                    if let (Some(r_path), Some(root)) = (r_path, workspace_root)
                        && let Some(r) = crate::r_subprocess::RSubprocess::new(Some(r_path))
                    {
                        let names =
                            crate::package_state::sysdata::load_sysdata_via_r(&r, &root).await;
                        if !names.is_empty() {
                            // Apply the fallback names under the write lock, then
                            // snapshot the open URIs to republish. Without this,
                            // open package buffers keep their pre-fallback
                            // diagnostics (e.g. undefined-variable on sysdata
                            // symbols) until the next edit.
                            let open_uris: Vec<tower_lsp::lsp_types::Url> = {
                                let mut state = state_arc.write().await;
                                state.package_inputs.sysdata_names = names;
                                state.apply_package_event(
                                    &crate::package_state::PackageInputDelta::DataDirChanged,
                                );
                                let uris: Vec<_> = state.documents.keys().cloned().collect();
                                state
                                    .diagnostics_gate
                                    .mark_force_republish_many(uris.iter());
                                uris
                            };
                            // Route through the same debounced, bounded pipeline
                            // the watcher consumer and `raven.refreshPackages`
                            // use, so the republish cooperates with revalidation
                            // cancellation/freshness gates.
                            Backend::publish_diagnostics_for_uris_bounded(
                                state_arc.clone(),
                                client,
                                open_uris,
                                Some(traversal_truncation),
                            )
                            .await;
                        }
                    }
                });
            }
        }

        // Register dynamic file watches for raven.toml / .lintr along the
        // same workspace-root/ancestor paths that project-config discovery
        // may consult. VS Code also sends broad synchronize.fileEvents for
        // these names; the handler below filters those events back to the
        // same discovery walk before reloading.
        {
            use tower_lsp::lsp_types::Registration;
            let (workspace_folders, raw_client) = {
                let state = self.state.read().await;
                (
                    state.workspace_folders.clone(),
                    state.raw_client_settings.clone(),
                )
            };
            let discovery_options = self.discovery_options_from_client_settings(&raw_client);
            let watchers = build_workspace_project_config_watchers(&workspace_folders);
            if !watchers.is_empty() {
                let reg = Registration {
                    id: "raven-config-files".into(),
                    method: "workspace/didChangeWatchedFiles".into(),
                    register_options: Some(
                        serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers })
                            .unwrap(),
                    ),
                };
                if let Err(e) = self.client.register_capability(vec![reg]).await {
                    log::warn!("dynamic watch registration failed: {e}");
                }
            }
            self.register_external_project_config_watch(workspace_folders, discovery_options)
                .await;
        }

        let init_duration = init_start.elapsed();
        if crate::perf::is_enabled() {
            log::info!("[PERF] Total initialization: {:?}", init_duration);
            if let Ok(m) = crate::perf::startup_metrics().lock() {
                m.log_summary()
            }
        }
        log::info!("Initialization complete (workspace scan running in background)");
    }

    async fn shutdown(&self) -> Result<()> {
        log::info!("ark-lsp shutting down");
        Ok(())
    }

    async fn execute_command(
        &self,
        params: tower_lsp::lsp_types::ExecuteCommandParams,
    ) -> tower_lsp::jsonrpc::Result<Option<serde_json::Value>> {
        match params.command.as_str() {
            "raven.refreshPackages" => {
                // refreshPackages is meaningless when package awareness is
                // disabled, and must leave the library the user built before
                // disabling completely untouched — not just its `Arc`, but its
                // cache, readiness flag, libpath watcher, and prefetch state.
                // Bail out before any of that work runs. (This is also why the
                // command does NOT route through the self-gating
                // `rebuild_package_library`: that would swap in `new_empty()`
                // and clobber the existing library.)
                let packages_enabled =
                    { self.state.read().await.cross_file_config.packages_enabled };
                if !packages_enabled {
                    log::info!(
                        "raven.refreshPackages ignored: package function awareness is disabled"
                    );
                    return Ok(Some(serde_json::json!({ "cleared": 0 })));
                }

                // Collect open URIs for force-republish (needed before rebuild).
                let open_uris: Vec<tower_lsp::lsp_types::Url> = {
                    let state = self.state.read().await;
                    state.documents.keys().cloned().collect()
                };

                // Capture the cache size of the *current* library before any
                // rebuild/refresh so the user-visible "cleared N entries"
                // count is meaningful even when the rebuild swaps in a fresh
                // empty library whose `cached_count()` starts at 0. We compute
                // the user-visible delta against the pre-rebuild count instead.
                let before_count = self.state.read().await.package_library.cached_count().await;

                // Rebuild the PackageLibrary first — this re-runs `.libPaths()`
                // so mid-session libpath changes (renv switched projects,
                // `.Rprofile` edited, `.libPaths(new, ...)` called) are picked
                // up. `clear_cache` alone would leave the stale libpath
                // snapshot in place and the refresh would re-populate with the
                // same wrong paths.
                let (new_lib, ready) = rebuild_package_library(&self.state).await;
                {
                    let mut state = self.state.write().await;
                    state.package_library = new_lib;
                    // The fresh library starts with no local-dev overlay; re-install
                    // it so load_all sentinel resolution survives the libpath rebuild.
                    state.refresh_local_dev_overlay();
                    state.package_library_ready = ready;
                    state.bump_package_config_generation();
                    // Swapping the library may have changed `lib_paths`
                    // (renv switch, `.Rprofile` edit). Re-resolve
                    // `source(system.file(...))` edges against the new paths
                    // BEFORE the force-republish below, so a file whose target
                    // lives in a newly-discovered libpath stops being
                    // stale/unresolved. See `rebuild_package_library`'s invariant.
                    if ready {
                        state.resolve_system_file_in_workspace();
                    }
                }

                // Restart the watcher over the freshly-discovered libpaths so
                // subsequent installs in the new libpaths are observed. A
                // no-op if watching is disabled or the library is not ready.
                restart_libpath_watcher(&self.state, &self.client, true).await;

                // Clear the cache then compute the eviction count before
                // warm-prefetching, so prefetched entries don't reduce the
                // reported cleared count.
                let pkg_lib = self.state.read().await.package_library.clone();
                pkg_lib.clear_cache().await;
                // Help and HTML help caches reference package-export content
                // that just got invalidated; flush them too so subsequent
                // hover/help-panel requests re-fetch from R.
                self.state.read().await.clear_help_caches();
                let after_count = pkg_lib.cached_count().await;
                let cleared = before_count.saturating_sub(after_count);
                prefetch_packages_for_open_documents(&self.state, &pkg_lib).await;
                log::info!("raven.refreshPackages: cleared {cleared} cache entries");

                // Force-republish diagnostics for all open documents.
                {
                    let state = self.state.read().await;
                    state
                        .diagnostics_gate
                        .mark_force_republish_many(open_uris.iter());
                }
                // Route through the same debounced pipeline the watcher consumer
                // uses, so refresh runs in parallel (not serial) and cooperates
                // with revalidation cancellation/freshness gates.
                let state_arc = Arc::clone(&self.state);
                let client = self.client.clone();
                let traversal_truncation = self.traversal_truncation.clone();
                tokio::spawn(async move {
                    Backend::publish_diagnostics_for_uris_bounded(
                        state_arc,
                        client,
                        open_uris,
                        Some(traversal_truncation),
                    )
                    .await;
                });
                Ok(Some(serde_json::json!({ "cleared": cleared })))
            }
            "raven.getHelpHtml" => {
                let topic = params
                    .arguments
                    .first()
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                // Distinguish missing (absent or JSON null) from wrong-type
                // (e.g. a number) so a malformed second argument doesn't
                // silently fall through to an unqualified lookup.
                let package = match params.arguments.get(1) {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(s)) => Some(s.as_str()),
                    Some(_) => {
                        return Ok(Some(serde_json::json!({
                            "ok": false,
                            "reason": "invalid-topic",
                            "message": "package argument must be a string or null",
                        })));
                    }
                };

                // Validation runs first — defense-in-depth before any cache or R access.
                if !crate::help::is_valid_help_topic(topic) {
                    return Ok(Some(serde_json::json!({
                        "ok": false,
                        "reason": "invalid-topic",
                        "message": "topic failed validation",
                    })));
                }
                if let Some(p) = package
                    && !crate::r_subprocess::is_valid_package_name(p)
                {
                    return Ok(Some(serde_json::json!({
                        "ok": false,
                        "reason": "invalid-topic",
                        "message": "package failed validation",
                    })));
                }

                let (cache, r_path) = {
                    let state = self.state.read().await;
                    // Mirror the hover handler: fall back to "R" (PATH lookup) when
                    // raven.packages.rPath is unset. The setting is opt-in for users
                    // with non-default R installs; the default expectation is that
                    // `R` resolves through PATH like every other R-spawning code path.
                    let r_path = state
                        .cross_file_config
                        .packages_r_path
                        .clone()
                        .unwrap_or_else(|| std::path::PathBuf::from("R"));
                    let cache = state.html_help_cache.clone();
                    (cache, r_path)
                };

                // Probe the cache before spawning R: a cached positive entry
                // is returned without re-running the subprocess.
                if let Some(cached) = cache.get(topic, package) {
                    return Ok(Some(help_html_to_json(cached)));
                }

                // Belt-and-suspenders timeout: bound the R subprocess await
                // with `HELP_LOOKUP_TIMEOUT` so an unforeseen lock or kill
                // failure inside `get_help_html`'s own watchdog can't freeze
                // the execute_command dispatcher.
                //
                // The timeout MUST live inside the fetch closure rather than
                // around the outer `cache.get_or_fetch(...).await`. Wrapping
                // the outer call would, on timeout, drop the `get_or_fetch`
                // future mid-flight — and if this caller happens to be the
                // single-flight Owner, its post-fetch cleanup (remove the
                // `in_flight` entry, broadcast to subscribers) never runs.
                // The map's surviving `Sender` clone keeps the broadcast
                // channel open with no remaining sender to ever publish,
                // poisoning the key until `drain()` clears it. Putting the
                // timeout inside guarantees the Owner always reaches its
                // cleanup branch and broadcasts `Err(Timeout)` to any
                // waiting subscribers. `Timeout` is non-cacheable, so the
                // next caller starts a fresh fetch.
                let result = cache
                    .get_or_fetch(topic, package, move |t, p| async move {
                        let task = tokio::task::spawn_blocking(move || {
                            crate::help::get_help_html(&t, p.as_deref(), &r_path)
                        });
                        match tokio::time::timeout(crate::handlers::HELP_LOOKUP_TIMEOUT, task).await
                        {
                            Ok(Ok(r)) => r,
                            Ok(Err(_)) => Err(crate::help::HelpHtmlError::RenderFailed {
                                message: "spawn_blocking joined with error".into(),
                            }),
                            Err(_) => Err(crate::help::HelpHtmlError::Timeout),
                        }
                    })
                    .await;

                Ok(Some(help_html_to_json(result)))
            }
            other => {
                log::warn!("execute_command: unknown command '{other}'");
                Ok(None)
            }
        }
    }

    /// Handle textDocument/didOpen notification.
    ///
    /// ## Lock Acquisition Pattern (Deadlock Analysis)
    ///
    /// This handler follows a safe lock acquisition pattern to avoid deadlocks:
    ///
    /// 1. **Write lock phase**: Acquires write lock to update document state, dependency graph,
    ///    and collect work items. All state mutations happen in this phase.
    ///
    /// 2. **Lock release**: Write lock is released BEFORE any synchronous indexing or
    ///    async operations that might need state access.
    ///
    /// 3. **Synchronous indexing**: Directly sourced files, the forward source chain, and
    ///    backward-directive targets are indexed synchronously AFTER releasing the write lock.
    ///    Each indexing operation acquires its own locks as needed.
    ///
    /// 4. **Async diagnostics**: Diagnostics are scheduled as separate async tasks that
    ///    acquire their own read locks independently.
    ///
    /// This pattern ensures:
    /// - No nested lock acquisition (write lock is never held while acquiring another lock)
    /// - Background tasks can safely acquire locks without blocking on this handler
    /// - Concurrent read operations can proceed during diagnostics computation
    ///
    /// **Note for maintainers**: If adding new operations that need state access,
    /// ensure they happen AFTER the write lock is released, or use interior mutability
    /// patterns (like the diagnostics_gate) that don't require exclusive access.
    ///
    /// **BOM handling**: editor-supplied text is stored verbatim — a leading
    /// U+FEFF is **not** stripped here, unlike disk reads (`state::decode_source`).
    /// Stripping server-side would desync LSP positions on line 0 for any client
    /// whose text model includes the BOM (tsserver's `ScriptInfo.open()` stores
    /// open-document text verbatim for the same reason). Analysis stays correct
    /// because tree-sitter-r treats U+FEFF as whitespace, and the raw-text
    /// column-0 scanners skip it at their scan anchor via
    /// [`crate::utf16::strip_leading_bom_for_scan`]. Issues #345, #346.
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let language_id = params.text_document.language_id;
        let text = params.text_document.text;
        let version = params.text_document.version;

        // Captured inside the lock block when the opened doc is the
        // workspace-root `.Rprofile`; drives an off-lock prelude refresh after.
        let mut rprofile_live_edit: Option<(std::path::PathBuf, Arc<str>)> = None;

        // Compute affected files while holding write lock
        let (
            mut work_items,
            debounce_ms,
            files_to_index,
            on_demand_enabled,
            packages_to_prefetch,
            packages_enabled,
        ) = {
            let mut state = self.state.write().await;

            // Capture old metadata before recomputing (for WD change detection)
            let old_meta = state.get_enriched_metadata(&uri);

            // Capture old interface_hash from workspace index (for selective invalidation)
            // This optimization avoids invalidating dependents when file content hasn't changed
            let old_interface_hash = state
                .cross_file_workspace_index
                .get_artifacts(&uri)
                .map(|a| a.interface_hash)
                .or_else(|| {
                    // Also check the new workspace index
                    state
                        .workspace_index_new
                        .get_artifacts(&uri)
                        .map(|a| a.interface_hash)
                });

            // Extract and enrich metadata with inherited working directory.
            // For Rmd/Quarto docs this masks the prose first so the graph,
            // DocumentStore, and on-demand indexing see only chunk-body
            // source()/library() calls and directives (#343). Classify by
            // languageId-then-URI so untitled `.Rmd`/`.qmd` buffers (no file
            // extension) are masked too — and reuse the same `chunk_kind` for
            // the DocumentStore open below so its tree/artifacts agree.
            let (chunk_kind, analysis_text) =
                crate::cross_file::classify_and_mask(Some(language_id.as_str()), &uri, &text);
            let mut meta = crate::cross_file::extract_metadata(&analysis_text);
            let uri_clone = uri.clone();
            let workspace_root = state.workspace_folders.first().cloned();
            let max_chain_depth = state.cross_file_config.max_chain_depth;

            // Enrich metadata with inherited working directory before any use
            // Use get_enriched_metadata to prefer already-enriched sources for transitive inheritance
            crate::cross_file::enrich_metadata_with_inherited_wd(
                &mut meta,
                &uri_clone,
                workspace_root.as_ref(),
                |parent_uri| state.get_enriched_metadata(parent_uri),
                max_chain_depth,
            );

            // Resolve system.file() source entries into concrete paths
            {
                let ws = state.package_state.workspace();
                let ws_name = ws.map(|w| w.name.as_str());
                let ws_root = ws.map(|w| w.root.as_path());
                let lib_paths = state.package_library.lib_paths();
                crate::cross_file::resolve_system_file_sources(
                    &mut meta, ws_name, ws_root, lib_paths,
                );
            }

            // Update new DocumentStore with enriched metadata (Requirement 1.3).
            // `chunk_kind` was classified above by languageId-then-URI so
            // untitled `.Rmd`/`.qmd` buffers mask their tree/artifacts (#343).
            state
                .document_store
                .open_with_metadata(uri.clone(), &text, version, chunk_kind, meta.clone())
                .await;

            // Update legacy documents HashMap (for migration compatibility)
            state.open_document_with_language_id(
                uri.clone(),
                &text,
                Some(version),
                Some(language_id.as_str()),
            );
            // Record as recently opened for activity prioritization
            state.cross_file_activity.record_recent(uri.clone());

            // Update package state via event-driven path when a package input opens.
            // This uses the `DidOpen` variant, which consumes caller-supplied
            // buffer text only; the disk-reading `WatchedFileChanged` variant
            // is handled by watched-file code paths instead.
            let mut package_visibility_changed = false;
            {
                let arc_text: std::sync::Arc<str> = text.as_str().into();
                let old_ns_model = state.package_state.namespace_model().cloned();
                let old_contribution = state.package_state.scope_contribution().clone();
                let event = crate::package_state::event::HandlerEvent::DidOpen {
                    uri: uri.clone(),
                    text: arc_text,
                };
                if let Some(delta) =
                    crate::package_state::event::translate(&mut state.package_inputs, event)
                {
                    state.apply_package_event(&delta);
                    package_visibility_changed = state.package_state.namespace_model()
                        != old_ns_model.as_ref()
                        || state.package_state.scope_contribution() != &old_contribution;
                }
            }

            // If the opened document is the workspace-root `.Rprofile`, capture
            // its buffer text for an off-lock prelude refresh after this block.
            // `did_open` can carry UNSAVED text (e.g. VS Code hot-exit restoring
            // a dirty buffer), so the prelude must reflect the buffer, not just
            // the disk seed — without this, open scripts would see a stale
            // prelude until the first keystroke.
            if let Some(root) = state.package_inputs.workspace_root.clone()
                && uri.to_file_path().ok().as_deref() == Some(root.join(".Rprofile").as_path())
            {
                rprofile_live_edit = Some((root, text.as_str().into()));
            }

            let on_demand_enabled = state.cross_file_config.on_demand_indexing_enabled;
            let packages_enabled = state.cross_file_config.packages_enabled;

            // Collect package names from library calls for background prefetch
            let packages_to_prefetch: Vec<String> = if packages_enabled {
                let mut p = extract_loaded_packages_from_library_calls(&meta.library_calls);
                p.extend(namespace_warm_packages(&meta));
                p
            } else {
                Vec::new()
            };

            // On-demand indexing: Collect sourced files that need indexing
            // Synchronous indexing: Files directly sourced by this open document and backward directive targets
            let mut files_to_index = Vec::new();

            if on_demand_enabled {
                let path_ctx = crate::cross_file::path_resolve::PathContext::from_metadata(
                    &uri_clone,
                    &meta,
                    workspace_root.as_ref(),
                );

                for source in &meta.sources {
                    let source_uri_opt = source.resolved_uri.clone().or_else(|| {
                        path_ctx.as_ref().and_then(|ctx| {
                            let resolved =
                                crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(
                                    &source.path,
                                    ctx,
                                );
                            resolved.and_then(|p| Url::from_file_path(p).ok())
                        })
                    });
                    if let Some(source_uri) = source_uri_opt {
                        // Check if file needs indexing (not open, not in workspace index)
                        if !state.documents.contains_key(&source_uri)
                            && !state.cross_file_workspace_index.contains(&source_uri)
                        {
                            log::trace!(
                                "Scheduling on-demand indexing for sourced file: {}",
                                source_uri
                            );
                            files_to_index.push((source_uri, IndexCategory::Sourced));
                        }
                    }
                }

                // Files referenced by backward directives
                let backward_ctx = crate::cross_file::path_resolve::PathContext::new(
                    &uri_clone,
                    workspace_root.as_ref(),
                );

                for directive in &meta.sourced_by {
                    if let Some(ctx) = backward_ctx.as_ref()
                        && let Some(resolved) =
                            crate::cross_file::path_resolve::resolve_path(&directive.path, ctx)
                        && let Ok(parent_uri) = Url::from_file_path(resolved)
                        && !state.documents.contains_key(&parent_uri)
                        && !state.cross_file_workspace_index.contains(&parent_uri)
                    {
                        log::trace!(
                            "Scheduling on-demand indexing for parent file: {}",
                            parent_uri
                        );
                        files_to_index.push((parent_uri, IndexCategory::BackwardDirective));
                    }
                }
            }

            let parent_content =
                collect_backward_parent_content(&state, &uri_clone, workspace_root.as_ref(), &meta);

            let result = state.cross_file_graph.update_file(
                &uri,
                &meta,
                workspace_root.as_ref(),
                |parent_uri| parent_content.get(parent_uri).cloned(),
            );

            // Invalidate children affected by working directory change (Requirement 8)
            let wd_affected =
                crate::cross_file::revalidation::invalidate_children_on_parent_wd_change(
                    &uri,
                    old_meta.as_deref(),
                    &meta,
                    &state.cross_file_graph,
                    &state.cross_file_meta,
                );

            // Emit any directive-vs-AST conflict diagnostics
            if !result.diagnostics.is_empty() {
                log::trace!(
                    "Directive-vs-AST conflicts detected: {} diagnostics",
                    result.diagnostics.len()
                );
            }

            // Compute new interface_hash after opening the document
            // Use the document_store which has artifacts computed with metadata (including declared symbols)
            // **Validates: Requirements 5.1, 5.2, 5.3, 5.4** (Diagnostic suppression for declared symbols)
            let new_interface_hash = state
                .document_store
                .get_without_touch(&uri)
                .map(|doc| doc.artifacts.interface_hash);

            // Determine if interface changed (selective invalidation optimization)
            // Only invalidate dependents if the exported interface actually changed
            let interface_changed = match (old_interface_hash, new_interface_hash) {
                (Some(old), Some(new)) => old != new,
                (None, Some(_)) => true, // File wasn't indexed before, now has interface
                (Some(_), None) => true, // File lost its interface (parse error?)
                (None, None) => false,   // No interface before or after
            };

            if interface_changed {
                log::trace!(
                    "Interface changed on open for {}: {:?} -> {:?}",
                    uri,
                    old_interface_hash,
                    new_interface_hash
                );
                // Package-internal symbols are now updated by apply_package_event
                // above (via scope_contribution → internal_symbols_cache). No
                // separate rebuild is needed here.
            }

            // Compute affected files from dependency graph using HashSet for O(1) deduplication
            let mut affected: std::collections::HashSet<Url> =
                std::collections::HashSet::from([uri.clone()]);

            // In package mode, when an R/*.R file's interface changed on open
            // (e.g., external edit), revalidate siblings so they pick up
            // added/removed symbols via mutual visibility. Sibling files
            // include BOTH `R/` (two-way visible to each other) and
            // `tests/testthat/` (one-way: sees R/ but R/ doesn't see it).
            // `append_package_contribution` injects package-internal symbols
            // into both kinds — so when an R/ interface changes, both kinds
            // of open files have potentially stale diagnostics.
            if (interface_changed || package_visibility_changed)
                && let Some(pkg) = state.package_workspace()
                && let Ok(fp) = uri.to_file_path()
            {
                let r_dir = pkg.root.join("R");
                if package_visibility_changed || fp.starts_with(&r_dir) {
                    for open_uri in state.documents.keys() {
                        if let Ok(p) = open_uri.to_file_path()
                            && crate::package_state::is_r_source_path(&p, &pkg.root).is_some()
                        {
                            affected.insert(open_uri.clone());
                        }
                    }
                }
            }

            // Invalidate cross-file scope neighbors if interface changed OR
            // dependency edges changed. See `compute_affected_dependents_after_edit`
            // for the symmetric backward+forward walk — children of `uri` also
            // need revalidation because their inherited scope is taken from
            // `uri`'s symbols at the source() call site.
            if interface_changed || result.edges_changed {
                let neighbors =
                    crate::cross_file::revalidation::compute_affected_dependents_after_edit(
                        &uri,
                        interface_changed,
                        result.edges_changed,
                        &state.cross_file_graph,
                        |u| state.documents.contains_key(u),
                        state.cross_file_config.max_chain_depth,
                        state.cross_file_config.max_transitive_dependents_visited,
                    );
                for dep in neighbors {
                    affected.insert(dep);
                }
            }
            // Include children affected by WD change (Requirement 8)
            for child in wd_affected {
                if state.documents.contains_key(&child) {
                    affected.insert(child);
                }
            }
            // Convert to Vec for sorting
            let mut affected: Vec<Url> = affected.into_iter().collect();

            // Prioritize by activity
            // Use saturating_add to prevent integer overflow at usize::MAX.
            // sort_by_cached_key memoizes priority_score per URI so the
            // O(N) recent_uris position scan runs once per element rather
            // than once per sort comparison.
            let activity = &state.cross_file_activity;
            affected.sort_by_cached_key(|u| {
                if *u == uri {
                    0
                } else {
                    activity.priority_score(u).saturating_add(1)
                }
            });

            // Apply revalidation cap
            let max_revalidations = state.cross_file_config.max_revalidations_per_trigger;
            if affected.len() > max_revalidations {
                log::trace!(
                    "Cross-file revalidation cap exceeded: {} affected, scheduling {}",
                    affected.len(),
                    max_revalidations
                );
                affected.truncate(max_revalidations);
            }

            // Build work items with trigger revision snapshot
            // NOTE: force-republish markers are NOT set here — they must be
            // deferred until after re-enrichment completes, since re-enrichment
            // can evict entries from work_items. Marking here would create
            // orphaned force counters on evicted URIs.
            let work_items: Vec<_> = affected
                .into_iter()
                .map(|affected_uri| {
                    let doc = state.documents.get(&affected_uri);
                    let trigger_version = doc.and_then(|d| d.version);
                    let trigger_revision = doc.map(|d| d.revision);
                    (affected_uri, trigger_version, trigger_revision)
                })
                .collect();

            // Refresh the document_store pin set: the open set just changed
            // and the dependency graph may have new edges from the new file.
            state.recompute_open_neighborhood_pins();

            let debounce_ms = state.cross_file_config.revalidation_debounce_ms;
            (
                work_items,
                debounce_ms,
                files_to_index,
                on_demand_enabled,
                packages_to_prefetch,
                packages_enabled,
            )
        };

        // Live `.Rprofile` open: establish the prelude from the (possibly
        // unsaved) buffer text, off-lock, and fan out to open consumers.
        if let Some((root, text)) = rprofile_live_edit {
            self.refresh_rprofile_prelude_from_buffer(root, text).await;
        }

        let needs_open_stabilization =
            !files_to_index.is_empty() || !packages_to_prefetch.is_empty();

        // Prefetch package exports synchronously before scheduling diagnostics so
        // did_open diagnostics don't race package cache warm-up.
        if packages_enabled && !packages_to_prefetch.is_empty() {
            let _ = self.ensure_package_library_initialized().await;
            let package_library = self.state.read().await.package_library.clone();
            log::trace!(
                "Synchronously prefetching {} packages before did_open diagnostics",
                packages_to_prefetch.len()
            );
            package_library
                .prefetch_packages(&packages_to_prefetch)
                .await;
        }

        // Only perform on-demand indexing if enabled
        if on_demand_enabled {
            // Perform SYNCHRONOUS on-demand indexing for sourced files
            // This ensures symbols are available BEFORE diagnostics run
            let sourced_files: Vec<Url> = files_to_index
                .iter()
                .filter(|(_, category)| *category == IndexCategory::Sourced)
                .map(|(uri, _)| uri.clone())
                .collect();

            if !sourced_files.is_empty() {
                log::info!(
                    "Synchronously indexing {} directly sourced files before diagnostics",
                    sourced_files.len()
                );
                for file_uri in sourced_files {
                    self.index_file_on_demand(&file_uri).await;
                }
            }

            // Synchronously index the forward source chain so transitive
            // dependencies are available before diagnostics run.
            // index_forward_chain handles cycle detection, depth limits, and
            // skips files already in documents or cross_file_workspace_index.
            let (workspace_root, max_backward_depth, max_forward_depth) = {
                let state = self.state.read().await;
                (
                    state.workspace_folders.first().cloned(),
                    state.cross_file_config.max_backward_depth,
                    state.cross_file_config.max_forward_depth,
                )
            };

            if max_forward_depth > 0 {
                self.index_forward_chain(&uri, max_forward_depth, workspace_root.as_ref())
                    .await;
            }

            // Backward directive targets are indexed synchronously
            // so parent scopes are available before diagnostics.

            let backward_directive_files: Vec<Url> = files_to_index
                .iter()
                .filter(|(_, category)| *category == IndexCategory::BackwardDirective)
                .map(|(uri, _)| uri.clone())
                .collect();

            if !backward_directive_files.is_empty() {
                log::info!(
                    "Synchronously indexing {} backward directive targets before diagnostics",
                    backward_directive_files.len()
                );
                self.index_backward_chain(
                    backward_directive_files,
                    max_backward_depth,
                    max_forward_depth,
                )
                .await;
            }

            // Re-enrich metadata now that backward/forward chains are indexed.
            // This ensures working-directory inheritance is accurate before diagnostics.
            // Ensure the package library is ready so resolve_system_file_sources
            // sees populated lib_paths (Finding 3: don't hold the write lock
            // across the await).
            if packages_enabled {
                let _ = self.ensure_package_library_initialized().await;
            }
            {
                let mut state = self.state.write().await;
                let workspace_root = state.workspace_folders.first().cloned();
                let max_chain_depth = state.cross_file_config.max_chain_depth;

                // Masked for Rmd/Quarto (chunk bodies only); raw otherwise.
                // Classify by languageId-then-URI so untitled buffers mask, and
                // extract metadata from that same masked view (#343).
                let (chunk_kind, analysis_text) =
                    crate::cross_file::classify_and_mask(Some(language_id.as_str()), &uri, &text);
                let mut meta = crate::cross_file::extract_metadata(&analysis_text);
                log::trace!(
                    "did_open re-enrich: uri={}, sources={}, sourced_by={}",
                    uri,
                    meta.sources.len(),
                    meta.sourced_by.len()
                );
                crate::cross_file::enrich_metadata_with_inherited_wd(
                    &mut meta,
                    &uri,
                    workspace_root.as_ref(),
                    |parent_uri| state.get_enriched_metadata(parent_uri),
                    max_chain_depth,
                );
                log::trace!(
                    "did_open re-enrich: uri={} working_directory={:?} inherited_working_directory={:?}",
                    uri,
                    meta.working_directory,
                    meta.inherited_working_directory
                );

                // Resolve system.file() source entries into concrete paths
                {
                    let ws = state.package_state.workspace();
                    let ws_name = ws.map(|w| w.name.as_str());
                    let ws_root = ws.map(|w| w.root.as_path());
                    let lib_paths = state.package_library.lib_paths();
                    crate::cross_file::resolve_system_file_sources(
                        &mut meta, ws_name, ws_root, lib_paths,
                    );
                }

                state
                    .document_store
                    .open_with_metadata(uri.clone(), &text, version, chunk_kind, meta.clone())
                    .await;
                state.open_document_with_language_id(
                    uri.clone(),
                    &text,
                    Some(version),
                    Some(language_id.as_str()),
                );

                let backward_path_ctx = crate::cross_file::path_resolve::PathContext::new(
                    &uri,
                    workspace_root.as_ref(),
                );
                let parent_content: std::collections::HashMap<Url, String> = meta
                    .sourced_by
                    .iter()
                    .filter_map(|d| {
                        let ctx = backward_path_ctx.as_ref()?;
                        let resolved = crate::cross_file::path_resolve::resolve_path(&d.path, ctx)?;
                        log::trace!(
                            "did_open re-enrich: backward directive {} -> {}",
                            d.path,
                            resolved.display()
                        );
                        let parent_uri = Url::from_file_path(resolved).ok()?;
                        let content = state
                            .documents
                            .get(&parent_uri)
                            .map(|doc| doc.text())
                            .or_else(|| state.cross_file_file_cache.get(&parent_uri))?;
                        Some((parent_uri, content))
                    })
                    .collect();

                if let Some(forward_ctx) =
                    crate::cross_file::path_resolve::PathContext::from_metadata(
                        &uri,
                        &meta,
                        workspace_root.as_ref(),
                    )
                {
                    for source in &meta.sources {
                        if let Some(resolved) =
                            crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(
                                &source.path,
                                &forward_ctx,
                            )
                        {
                            log::trace!(
                                "did_open re-enrich: source() {} -> {}",
                                source.path,
                                resolved.display()
                            );
                        } else {
                            log::trace!(
                                "did_open re-enrich: source() {} -> <unresolved>",
                                source.path
                            );
                        }
                    }
                }

                let second_result = state.cross_file_graph.update_file(
                    &uri,
                    &meta,
                    workspace_root.as_ref(),
                    |parent_uri| parent_content.get(parent_uri).cloned(),
                );

                // If re-enrichment changed dependency edges (e.g., inherited WD
                // altered path resolution), schedule newly affected open
                // neighbors — both backward dependents and forward children
                // (their inherited scope is taken from this file's symbols at
                // the source() call site).
                if second_result.edges_changed {
                    let neighbors =
                        crate::cross_file::revalidation::compute_affected_dependents_after_edit(
                            &uri,
                            false,
                            true,
                            &state.cross_file_graph,
                            |u| state.documents.contains_key(u),
                            state.cross_file_config.max_chain_depth,
                            state.cross_file_config.max_transitive_dependents_visited,
                        );
                    // Re-enrichment changed work_items; force-republish marking
                    // is deferred until after both re-enrichment paths complete
                    // so evicted URIs don't carry orphaned force counters.
                    work_items =
                        rebuild_work_items_after_reenrichment(&uri, &work_items, neighbors, &state);
                    // Re-enrichment moved edges; refresh pins so the open-doc
                    // neighborhood matches the post-update graph.
                    state.recompute_open_neighborhood_pins();
                }

                // Ensure direct sources for this document are indexed using the re-enriched metadata.
                let forward_ctx = crate::cross_file::path_resolve::PathContext::from_metadata(
                    &uri,
                    &meta,
                    workspace_root.as_ref(),
                );
                for source in &meta.sources {
                    let child_uri_opt = source.resolved_uri.clone().or_else(|| {
                        forward_ctx.as_ref().and_then(|ctx| {
                            let resolved =
                                crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(
                                    &source.path,
                                    ctx,
                                );
                            resolved.and_then(|p| Url::from_file_path(p).ok())
                        })
                    });
                    if let Some(child_uri) = child_uri_opt {
                        let needs_indexing = {
                            !state.documents.contains_key(&child_uri)
                                && !state.cross_file_workspace_index.contains(&child_uri)
                        };
                        if needs_indexing {
                            log::trace!("did_open re-enrich: indexing direct source {}", child_uri);
                            drop(state);
                            let _ = self.index_file_on_demand(&child_uri).await;
                            state = self.state.write().await;
                        }
                    }
                }
            }
        }
        // Even when on-demand indexing is disabled, re-enrich metadata so inherited
        // working-directory context and graph edges match did_change behavior.
        else {
            // Ensure library is ready so resolve_system_file_sources sees
            // populated lib_paths (Finding 3: await outside write lock).
            if packages_enabled {
                let _ = self.ensure_package_library_initialized().await;
            }
            let mut state = self.state.write().await;
            let workspace_root = state.workspace_folders.first().cloned();
            let max_chain_depth = state.cross_file_config.max_chain_depth;

            // languageId-then-URI classification masks untitled buffers (#343);
            // extract metadata from the same masked view so the graph and the
            // DocumentStore tree/artifacts agree (chunk bodies only for Rmd).
            let (chunk_kind, analysis_text) =
                crate::cross_file::classify_and_mask(Some(language_id.as_str()), &uri, &text);
            let mut meta = crate::cross_file::extract_metadata(&analysis_text);
            crate::cross_file::enrich_metadata_with_inherited_wd(
                &mut meta,
                &uri,
                workspace_root.as_ref(),
                |parent_uri| state.get_enriched_metadata(parent_uri),
                max_chain_depth,
            );

            // Resolve system.file() source entries into concrete paths
            {
                let ws = state.package_state.workspace();
                let ws_name = ws.map(|w| w.name.as_str());
                let ws_root = ws.map(|w| w.root.as_path());
                let lib_paths = state.package_library.lib_paths();
                crate::cross_file::resolve_system_file_sources(
                    &mut meta, ws_name, ws_root, lib_paths,
                );
            }

            state
                .document_store
                .open_with_metadata(uri.clone(), &text, version, chunk_kind, meta.clone())
                .await;
            state.open_document_with_language_id(
                uri.clone(),
                &text,
                Some(version),
                Some(language_id.as_str()),
            );

            let backward_path_ctx =
                crate::cross_file::path_resolve::PathContext::new(&uri, workspace_root.as_ref());
            let parent_content: std::collections::HashMap<Url, String> = meta
                .sourced_by
                .iter()
                .filter_map(|d| {
                    let ctx = backward_path_ctx.as_ref()?;
                    let resolved = crate::cross_file::path_resolve::resolve_path(&d.path, ctx)?;
                    let parent_uri = Url::from_file_path(resolved).ok()?;
                    let content = state
                        .documents
                        .get(&parent_uri)
                        .map(|doc| doc.text())
                        .or_else(|| state.cross_file_file_cache.get(&parent_uri))?;
                    Some((parent_uri, content))
                })
                .collect();

            let second_result = state.cross_file_graph.update_file(
                &uri,
                &meta,
                workspace_root.as_ref(),
                |parent_uri| parent_content.get(parent_uri).cloned(),
            );

            if second_result.edges_changed {
                let neighbors =
                    crate::cross_file::revalidation::compute_affected_dependents_after_edit(
                        &uri,
                        false,
                        true,
                        &state.cross_file_graph,
                        |u| state.documents.contains_key(u),
                        state.cross_file_config.max_chain_depth,
                        state.cross_file_config.max_transitive_dependents_visited,
                    );
                // Re-enrichment changed work_items; force-republish marking
                // is deferred until after both re-enrichment paths complete
                // so evicted URIs don't carry orphaned force counters.
                work_items =
                    rebuild_work_items_after_reenrichment(&uri, &work_items, neighbors, &state);
                // Re-enrichment moved edges; refresh pins so the open-doc
                // neighborhood matches the post-update graph.
                state.recompute_open_neighborhood_pins();
            }
        }

        // Prefetch packages for inherited scope to avoid transient undefined diagnostics.
        // Snapshot under the lock, release it, then run scope resolution outside the lock
        // (AGENTS.md: must not hold RwLock read lock during expensive scope resolution).
        if packages_enabled {
            let _ = self.ensure_package_library_initialized().await;

            let (package_library, package_library_ready, scope_packages) = {
                let (probe, pkg_lib, ready, data_packages) = {
                    let state = self.state.read().await;
                    let doc = state.documents.get(&uri);
                    let last_line = doc
                        .map(|d| d.text().lines().count().saturating_sub(1) as u32)
                        .unwrap_or(0);
                    // Issue #429: capture `data(..., package = "pkg")` packages
                    // to warm so the diagnostics-time `data()` alias expansion
                    // can resolve their dataset objects (parity with `raven
                    // check`). Validated by the `is_valid_package_name` filter
                    // applied to the merged set below.
                    let data_packages: Vec<String> =
                        doc.map(|d| d.data_packages.clone()).unwrap_or_default();
                    let snapshot = state.build_package_scope_snapshot(&[(uri.clone(), last_line)]);
                    (
                        snapshot,
                        state.package_library.clone(),
                        state.package_library_ready,
                        data_packages,
                    )
                }; // read lock released

                let empty_base_exports = std::collections::HashSet::new();
                let get_artifacts = |u: &Url| probe.artifacts_map.get(u).cloned();
                let get_metadata = |u: &Url| probe.metadata_map.get(u).cloned();
                let probe_line = probe.docs.first().map(|(_, l)| *l).unwrap_or(0);
                let scope = crate::cross_file::scope::scope_at_position_with_graph(
                    &uri,
                    probe_line,
                    u32::MAX,
                    &get_artifacts,
                    &get_metadata,
                    &probe.graph,
                    probe.workspace_folder.as_ref(),
                    probe.max_chain_depth,
                    &empty_base_exports,
                    false,
                    probe.backward_dependencies,
                    &|| false,
                    Some(&probe.scope_contribution),
                    // Package-scope probe reads only `inherited_packages` /
                    // `loaded_packages`, never `scope.symbols`, so `data()`
                    // alias expansion (issue #429) is unnecessary here.
                    None,
                );

                let mut pkgs = scope.inherited_packages;
                pkgs.extend(scope.loaded_packages);
                // Issue #429: warm `data(..., package = "pkg")` packages too, so
                // the diagnostics-time `data()` alias expansion resolves their
                // dataset object names in the editor (parity with `raven check`).
                pkgs.extend(data_packages);
                // Filter the merged set so suspicious names from inherited
                // packages (which originate in parents' library_calls and
                // are not pre-validated) cannot reach the R subprocess /
                // filesystem path. Mirrors the validation applied to direct
                // library_calls via extract_loaded_packages_from_library_calls.
                // `is_valid_package_name` already rejects the underscore-prefixed
                // load_all() sentinel; the explicit guard is defense-in-depth so
                // the sentinel can never reach prefetch / the R subprocess.
                let pkgs: Vec<String> = pkgs
                    .into_iter()
                    .filter(|p| {
                        is_valid_package_name(p) && !crate::package_library::is_load_all_sentinel(p)
                    })
                    .collect();

                (pkg_lib, ready, pkgs)
            };

            if !scope_packages.is_empty() {
                log::trace!(
                    "did_open prefetch: uri={} packages={:?}",
                    uri,
                    scope_packages
                );
                if package_library_ready {
                    package_library.prefetch_packages(&scope_packages).await;
                } else {
                    log::trace!("did_open prefetch: package library not ready, skipping");
                }
            }
        }

        // Mark force-republish on the final work_items set, excluding the
        // edited URI itself (its publish is driven by its own version bump,
        // not by a cross-file marker). Marking after re-enrichment ensures
        // that evicted URIs don't carry orphaned force counters.
        {
            let state = self.state.read().await;
            state.diagnostics_gate.mark_force_republish_many(
                work_items.iter().map(|(u, _, _)| u).filter(|u| **u != uri),
            );
        }

        // Schedule debounced diagnostics for all affected files via revalidation system
        for (affected_uri, trigger_version, trigger_revision) in work_items {
            let state_arc = self.state.clone();
            let client = self.client.clone();
            let traversal_truncation = self.traversal_truncation.clone();

            tokio::spawn(run_debounced_diagnostics(
                state_arc,
                client,
                affected_uri,
                debounce_ms,
                trigger_version,
                trigger_revision,
                Some(traversal_truncation),
            ));
        }

        if needs_open_stabilization {
            let state_arc = self.state.clone();
            let client = self.client.clone();
            let stabilization_uri = uri.clone();
            let traversal_truncation = self.traversal_truncation.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                let (trigger_version, trigger_revision) = {
                    let state = state_arc.read().await;
                    if !state.documents.contains_key(&stabilization_uri) {
                        return;
                    }
                    state
                        .diagnostics_gate
                        .mark_force_republish(&stabilization_uri);
                    let doc = state.documents.get(&stabilization_uri);
                    let ver = doc.and_then(|d| d.version);
                    let rev = doc.map(|d| d.revision);
                    (ver, rev)
                };

                run_debounced_diagnostics(
                    state_arc,
                    client,
                    stabilization_uri,
                    0,
                    trigger_version,
                    trigger_revision,
                    Some(traversal_truncation),
                )
                .await;
            });
        }
    }

    /// Handle a text-document change: update in-memory state, compute affected documents, and schedule debounced diagnostics and optional package prefetching.
    ///
    /// This method processes an LSP `textDocument/didChange` notification by updating the server's document store and dependency graph, determining which open documents are affected by the change, and scheduling debounced asynchronous diagnostics for those documents. If package indexing is enabled and package references are found, it also triggers background prefetching of package exports.
    ///
    /// Key observable behaviors:
    /// - Updates the server's document store and legacy open-document map with the incoming changes.
    /// - Recomputes cross-file dependencies and selects affected open documents to revalidate (subject to configured caps and activity-based prioritization).
    /// - Schedules debounced diagnostic runs for each affected document; each scheduled run performs freshness checks before and after asynchronous work and respects the server's monotonic publishing gate.
    /// - If packages are enabled and package names were discovered, initiates background prefetch of referenced package exports without holding the main state lock.
    ///
    /// # Examples
    ///
    /// ```text
    /// // Typical invocation occurs inside the LSP runtime:
    /// // backend.did_change(params).await;
    /// ```
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        let changes = params.content_changes;

        // When the edited document is the workspace-root `.Rprofile`, capture
        // its (post-change) in-memory text so we can refresh the prelude from
        // the live buffer AFTER releasing the lock (see
        // `refresh_rprofile_prelude_from_buffer`). `.Rprofile` is a live R
        // document, but its prelude is package-state rather than a graph edge,
        // so the normal dependent fanout below does not cover it.
        let mut rprofile_live_edit: Option<(std::path::PathBuf, Arc<str>)> = None;

        // Compute affected files and debounce config while holding write lock
        let (
            work_items,
            edited_file_debounce_ms,
            dependent_debounce_ms,
            packages_to_prefetch,
            packages_enabled,
            package_library,
        ) = {
            let mut state = self.state.write().await;

            // Capture old metadata before recomputing (for WD change detection)
            let old_meta = state.get_enriched_metadata(&uri);

            // Capture old interface_hash before applying changes (for selective invalidation)
            // This optimization avoids invalidating dependents when only comments/local vars change
            // Use document_store which has artifacts computed with metadata (including declared symbols)
            // **Validates: Requirements 5.1, 5.2, 5.3, 5.4** (Diagnostic suppression for declared symbols)
            let old_interface_hash = state
                .document_store
                .get_without_touch(&uri)
                .map(|doc| doc.artifacts.interface_hash);

            // Update legacy documents HashMap first (for migration compatibility)
            if let Some(doc) = state.documents.get_mut(&uri) {
                doc.version = Some(version);
            }
            for change in changes.clone() {
                state.apply_change(&uri, change);
            }
            // Record as recently changed for activity prioritization
            state.cross_file_activity.record_recent(uri.clone());

            // Snapshot the post-change buffer text if this is the workspace-root
            // `.Rprofile`, for the off-lock prelude refresh after this block.
            if let Some(root) = state.package_inputs.workspace_root.clone()
                && uri.to_file_path().ok().as_deref() == Some(root.join(".Rprofile").as_path())
                && let Some(text) = state
                    .documents
                    .get(&uri)
                    .map(|d| Arc::<str>::from(d.text()))
            {
                rprofile_live_edit = Some((root, text));
            }

            // Capture package settings for background prefetch
            let packages_enabled = state.cross_file_config.packages_enabled;
            let package_library = state.package_library.clone();
            let max_chain_depth = state.cross_file_config.max_chain_depth;

            // Extract and enrich metadata with inherited working directory
            let (
                packages_to_prefetch,
                enriched_meta,
                precomputed_masked,
                wd_affected,
                edges_changed,
            ) = if let Some(doc) = state.documents.get(&uri) {
                // Analysis text: masked for Rmd/Quarto (chunk bodies only),
                // raw otherwise. Feeds the graph + DocumentStore (#343).
                // `apply_change` above already masked this exact raw content
                // for this doc's fixed chunk kind, so we hand the Rmd mask to
                // `update_with_metadata` below to skip a second `mask_to_r`
                // pass per keystroke. Only Rmd carries a mask; plain R's
                // analysis text equals raw text, so pass `None` there.
                let text = doc.analysis_text();
                let mut meta = crate::cross_file::extract_metadata(&text);
                // Extract first, then move `text` into the mask slot — avoids
                // cloning the masked String a second time per keystroke.
                let precomputed_masked = doc.is_rmd_document().then_some(text);
                let uri_clone = uri.clone();
                let workspace_root = state.workspace_folders.first().cloned();

                // Enrich metadata with inherited working directory before any use
                // Use get_enriched_metadata to prefer already-enriched sources for transitive inheritance
                crate::cross_file::enrich_metadata_with_inherited_wd(
                    &mut meta,
                    &uri_clone,
                    workspace_root.as_ref(),
                    |parent_uri| state.get_enriched_metadata(parent_uri),
                    max_chain_depth,
                );

                // Resolve system.file() source entries into concrete paths
                {
                    let ws = state.package_state.workspace();
                    let ws_name = ws.map(|w| w.name.as_str());
                    let ws_root = ws.map(|w| w.root.as_path());
                    let lib_paths = state.package_library.lib_paths();
                    crate::cross_file::resolve_system_file_sources(
                        &mut meta, ws_name, ws_root, lib_paths,
                    );
                }

                // Collect package names for prefetch (validate names to
                // reject suspicious inputs before R subprocess calls)
                let pkgs: Vec<String> = if packages_enabled {
                    let mut p = match state.documents.get(&uri_clone) {
                        Some(doc) => edited_document_warm_packages(&meta.library_calls, doc),
                        None => extract_loaded_packages_from_library_calls(&meta.library_calls),
                    };
                    p.extend(namespace_warm_packages(&meta));
                    p
                } else {
                    Vec::new()
                };

                let parent_content = collect_backward_parent_content(
                    &state,
                    &uri_clone,
                    workspace_root.as_ref(),
                    &meta,
                );

                let graph_result = state.cross_file_graph.update_file(
                    &uri,
                    &meta,
                    workspace_root.as_ref(),
                    |parent_uri| parent_content.get(parent_uri).cloned(),
                );

                // Invalidate children affected by working directory change (Requirement 8)
                let wd_children =
                    crate::cross_file::revalidation::invalidate_children_on_parent_wd_change(
                        &uri,
                        old_meta.as_deref(),
                        &meta,
                        &state.cross_file_graph,
                        &state.cross_file_meta,
                    );

                (
                    pkgs,
                    Some(meta),
                    precomputed_masked,
                    wd_children,
                    graph_result.edges_changed,
                )
            } else {
                (Vec::new(), None, None, Vec::new(), false)
            };

            // Update new DocumentStore with enriched metadata (Requirement 1.4)
            if let Some(meta) = enriched_meta {
                state
                    .document_store
                    .update_with_metadata(&uri, changes, version, meta, precomputed_masked)
                    .await;
            } else {
                state.document_store.update(&uri, changes, version).await;
            }

            // Update package state via event-driven path when an R/*.R file
            // changes in a package workspace.
            //
            // Guard the potentially expensive full-document `Rope -> String ->
            // Arc<str>` materialization behind a cheap path check. Without
            // the guard, every keystroke on every open R file in every
            // workspace (including non-package and scratch-file workflows)
            // paid two full-document heap allocations just to have
            // `translate()` short-circuit on `is_r_source_path == None`.
            // The `DidChange` variant uses only this in-memory buffer text;
            // watched-file events are the only package events that may read
            // from disk and are handled on their own code paths.
            let mut package_visibility_changed = false;
            let in_package_input_path = state
                .package_inputs
                .workspace_root
                .as_ref()
                .zip(uri.to_file_path().ok())
                .is_some_and(|(root, path)| {
                    crate::package_state::is_r_source_path(&path, root).is_some()
                        || path == root.join("DESCRIPTION")
                        || path == root.join("NAMESPACE")
                });
            if in_package_input_path {
                let text: std::sync::Arc<str> = state
                    .documents
                    .get(&uri)
                    .map(|d| d.text())
                    .unwrap_or_default()
                    .into();
                let old_ns_model = state.package_state.namespace_model().cloned();
                let old_contribution = state.package_state.scope_contribution().clone();
                let event = crate::package_state::event::HandlerEvent::DidChange {
                    uri: uri.clone(),
                    text,
                };
                if let Some(delta) =
                    crate::package_state::event::translate(&mut state.package_inputs, event)
                {
                    state.apply_package_event(&delta);
                    package_visibility_changed = state.package_state.namespace_model()
                        != old_ns_model.as_ref()
                        || state.package_state.scope_contribution() != &old_contribution;
                }
            }

            // Compute new interface_hash after applying changes
            // Use document_store which has artifacts computed with metadata (including declared symbols)
            // **Validates: Requirements 5.1, 5.2, 5.3, 5.4** (Diagnostic suppression for declared symbols)
            let new_interface_hash = state
                .document_store
                .get_without_touch(&uri)
                .map(|doc| doc.artifacts.interface_hash);

            // Determine if interface changed (selective invalidation optimization)
            // Only invalidate dependents if the exported interface actually changed
            let interface_changed = match (old_interface_hash, new_interface_hash) {
                (Some(old), Some(new)) => old != new,
                (None, Some(_)) => true, // New file with interface
                (Some(_), None) => true, // File lost its interface (parse error?)
                (None, None) => false,   // No interface before or after
            };

            if interface_changed {
                log::trace!(
                    "Interface changed for {}: {:?} -> {:?}",
                    uri,
                    old_interface_hash,
                    new_interface_hash
                );
                // Package-internal symbols are now updated by apply_package_event
                // above (via scope_contribution → internal_symbols_cache). No
                // separate rebuild is needed here.
            }

            // Compute affected files from dependency graph using HashSet for O(1) deduplication
            let mut affected: std::collections::HashSet<Url> =
                std::collections::HashSet::from([uri.clone()]);

            // Invalidate cross-file scope neighbors if interface changed OR
            // dependency edges changed. Walks both directions:
            //   - backward (parents who source `uri`): they consume `uri`'s
            //     exported interface, so their cycle/symbol diagnostics may
            //     change.
            //   - forward (files `uri` sources): their inherited scope is
            //     taken from `uri` at the source() call site, so changing
            //     `uri`'s top-level symbols or source() topology can flip
            //     undefined-variable diagnostics in descendants. Without
            //     this, an edit to `parent.R` that drops `y <- 1` never
            //     triggers a republish of `child.R` (which uses `y`) until
            //     the user manually edits `child.R`.
            // Bulk-mark all dependents/children under a single write-lock to
            // skip per-URI lock churn on large fan-outs (Requirement 0.8).
            if interface_changed || edges_changed {
                let neighbors =
                    crate::cross_file::revalidation::compute_affected_dependents_after_edit(
                        &uri,
                        interface_changed,
                        edges_changed,
                        &state.cross_file_graph,
                        |u| state.documents.contains_key(u),
                        state.cross_file_config.max_chain_depth,
                        state.cross_file_config.max_transitive_dependents_visited,
                    );
                for dep in neighbors {
                    affected.insert(dep);
                }
            }
            // Include children affected by WD change (Requirement 8)
            for child in wd_affected {
                if state.documents.contains_key(&child) {
                    affected.insert(child);
                }
            }
            // When package-mode visibility changed, all open package files
            // need revalidation so imports and internal symbols propagate.
            // Both `R/` (two-way) and `tests/testthat/` (one-way) receive the
            // contribution via `append_package_contribution`, so both need
            // the affected-set.
            if package_visibility_changed && let Some(pkg) = state.package_workspace() {
                for open_uri in state.documents.keys() {
                    if let Ok(p) = open_uri.to_file_path()
                        && crate::package_state::is_r_source_path(&p, &pkg.root).is_some()
                    {
                        affected.insert(open_uri.clone());
                    }
                }
            }
            // In package mode, when the exported interface of an R/*.R file
            // changes, other open package files depend on it via mutual
            // visibility (not the source() graph). Revalidate them so
            // added/removed symbols propagate to undefined-variable
            // diagnostics. `tests/testthat/` sees R/ symbols (one-way), so
            // include test files in the affected set too.
            if interface_changed
                && !package_visibility_changed
                && let Some(pkg) = state.package_workspace()
                && let Ok(fp) = uri.to_file_path()
            {
                let r_dir = pkg.root.join("R");
                if fp.starts_with(&r_dir) {
                    for open_uri in state.documents.keys() {
                        if let Ok(p) = open_uri.to_file_path()
                            && crate::package_state::is_r_source_path(&p, &pkg.root).is_some()
                        {
                            affected.insert(open_uri.clone());
                        }
                    }
                }
            }
            // Convert to Vec for sorting
            let mut affected: Vec<Url> = affected.into_iter().collect();

            // Prioritize by activity (trigger first, then active, then visible, then recent)
            // Use saturating_add to prevent integer overflow at usize::MAX.
            // sort_by_cached_key memoizes priority_score per URI so the
            // O(N) recent_uris position scan runs once per element rather
            // than once per sort comparison.
            let activity = &state.cross_file_activity;
            affected.sort_by_cached_key(|u| {
                if *u == uri {
                    0
                } else {
                    activity.priority_score(u).saturating_add(1)
                }
            });

            // Apply revalidation cap (Requirement 0.9, 0.10)
            let max_revalidations = state.cross_file_config.max_revalidations_per_trigger;
            if affected.len() > max_revalidations {
                log::trace!(
                    "Cross-file revalidation cap exceeded: {} affected, scheduling {}",
                    affected.len(),
                    max_revalidations
                );
                affected.truncate(max_revalidations);
            }

            // Bulk-mark dependents under a single write-lock, AFTER the cap
            // is applied. Iterating the deduped, truncated set prevents two
            // bugs: (1) a URI present in both the dependency-graph walk and
            // `wd_affected` would otherwise get its force-republish counter
            // incremented twice (the counter decrements only on a consumed
            // publish, so duplicate marks leak as phantom markers); (2) URIs
            // dropped by the cap would otherwise carry orphaned force
            // counters that leak into a future unrelated same-version
            // publish. The edited URI itself is excluded — its publish is
            // driven by its own version bump.
            state
                .diagnostics_gate
                .mark_force_republish_many(affected.iter().filter(|u| **u != uri));

            // Build work items with trigger revision snapshot for freshness guard
            let work_items: Vec<_> = affected
                .into_iter()
                .map(|affected_uri| {
                    let doc = state.documents.get(&affected_uri);
                    let trigger_version = doc.and_then(|d| d.version);
                    let trigger_revision = doc.map(|d| d.revision);
                    (affected_uri, trigger_version, trigger_revision)
                })
                .collect();

            // Refresh pins when graph edges shift: a removed `source()` may
            // pull a closed file out of the open neighborhood (let it become
            // LRU-evictable), and a newly added one may pull one in.
            if edges_changed {
                state.recompute_open_neighborhood_pins();
            }

            let edited_file_debounce_ms = state.cross_file_config.edited_file_debounce_ms;
            let dependent_debounce_ms = state.cross_file_config.revalidation_debounce_ms;
            (
                work_items,
                edited_file_debounce_ms,
                dependent_debounce_ms,
                packages_to_prefetch,
                packages_enabled,
                package_library,
            )
        };

        // Live `.Rprofile` edit: refresh the prelude from the in-memory buffer
        // (off-lock scan + apply + fanout) so open scripts/ update as you type.
        if let Some((root, text)) = rprofile_live_edit {
            self.refresh_rprofile_prelude_from_buffer(root, text).await;
        }

        // Background prefetch package exports (without holding WorldState lock)
        // After prefetch completes, schedule diagnostic revalidation so newly
        // cached exports clear false-positive "undefined variable" diagnostics.
        if packages_enabled {
            let pkg_lib = package_library;
            let state_arc = self.state.clone();
            let client = self.client.clone();
            let revalidation_uri = uri.clone();
            let direct_packages = packages_to_prefetch;
            let traversal_truncation = self.traversal_truncation.clone();
            // URIs already force-marked + scheduled by the synchronous
            // dependency-graph path above. The sibling republish below must
            // exclude them: double-marking the force-republish counter would
            // leak a phantom marker (a later same-URI job cancels an earlier
            // one before it consumes its mark).
            let scheduled_uris: std::collections::HashSet<Url> =
                work_items.iter().map(|(u, _, _)| u.clone()).collect();
            tokio::spawn(async move {
                // Extend direct library_calls with inherited packages from
                // parent source() chains. Snapshot under the lock, release it,
                // then run scope resolution outside the lock.
                let mut all_packages: std::collections::HashSet<String> =
                    direct_packages.into_iter().collect();

                {
                    let probe = {
                        let state = state_arc.read().await;
                        if !state.documents.contains_key(&revalidation_uri) {
                            return; // Document was closed
                        }
                        let last_line = state
                            .documents
                            .get(&revalidation_uri)
                            .map(|d| d.text().lines().count().saturating_sub(1) as u32)
                            .unwrap_or(0);
                        state.build_package_scope_snapshot(&[(revalidation_uri.clone(), last_line)])
                    }; // read lock released

                    let empty_base = std::collections::HashSet::new();
                    let get_artifacts = |u: &Url| probe.artifacts_map.get(u).cloned();
                    let get_metadata = |u: &Url| probe.metadata_map.get(u).cloned();
                    let probe_line = probe.docs.first().map(|(_, l)| *l).unwrap_or(0);
                    let scope = crate::cross_file::scope::scope_at_position_with_graph(
                        &revalidation_uri,
                        probe_line,
                        u32::MAX,
                        &get_artifacts,
                        &get_metadata,
                        &probe.graph,
                        probe.workspace_folder.as_ref(),
                        probe.max_chain_depth,
                        &empty_base,
                        false,
                        probe.backward_dependencies,
                        &|| false,
                        Some(&probe.scope_contribution),
                        // Probe reads only packages, not `scope.symbols`;
                        // no `data()` alias expansion needed (issue #429).
                        None,
                    );
                    all_packages.extend(scope.inherited_packages);
                    all_packages.extend(scope.loaded_packages);
                }

                if all_packages.is_empty() {
                    return;
                }

                // Filter the merged set to drop suspicious names from
                // inherited/loaded packages (parents may carry unvalidated
                // entries through scope resolution). Mirrors the
                // `prefetch_packages_for_open_documents` filter so every
                // prefetch call site applies the same validation.
                // `is_valid_package_name` already rejects the underscore-prefixed
                // load_all() sentinel; the explicit guard is defense-in-depth.
                let packages_vec: Vec<String> = all_packages
                    .into_iter()
                    .filter(|p| {
                        is_valid_package_name(p) && !crate::package_library::is_load_all_sentinel(p)
                    })
                    .collect();
                if packages_vec.is_empty() {
                    return;
                }
                log::trace!("Background prefetching {} packages", packages_vec.len());
                pkg_lib.prefetch_packages(&packages_vec).await;

                // Warming a package can change `pkg::member` / missing-package
                // diagnostics in OTHER open documents that reference it but do
                // not `source()` this file (issue #503), so collect those
                // siblings and force-republish them alongside the edited file.
                let warmed: std::collections::HashSet<String> = packages_vec.into_iter().collect();

                // After prefetch completes, trigger diagnostic revalidation
                let (debounce_ms, trigger_version, trigger_revision, sibling_jobs) = {
                    let state = state_arc.read().await;
                    if !state.documents.contains_key(&revalidation_uri) {
                        return; // Document was closed during prefetch
                    }
                    state
                        .diagnostics_gate
                        .mark_force_republish(&revalidation_uri);
                    let doc = state.documents.get(&revalidation_uri);
                    let ver = doc.and_then(|d| d.version);
                    let rev = doc.map(|d| d.revision);

                    // Cross-document sibling republish (issue #503), excluding the
                    // edited file (revalidated below by its own version bump).
                    let mut sibling_jobs: Vec<(Url, Option<i32>, Option<u64>)> = Vec::new();
                    for sib in open_docs_referencing_packages(&state, &warmed) {
                        // Skip the edited file (revalidated by its own version
                        // bump) and any URI the synchronous path already marked +
                        // scheduled — re-marking would leak a phantom force
                        // counter.
                        if sib == revalidation_uri || scheduled_uris.contains(&sib) {
                            continue;
                        }
                        state.diagnostics_gate.mark_force_republish(&sib);
                        let sdoc = state.documents.get(&sib);
                        let sver = sdoc.and_then(|d| d.version);
                        let srev = sdoc.map(|d| d.revision);
                        sibling_jobs.push((sib, sver, srev));
                    }
                    (
                        state.cross_file_config.revalidation_debounce_ms,
                        ver,
                        rev,
                        sibling_jobs,
                    )
                };

                for (sib, sver, srev) in sibling_jobs {
                    tokio::spawn(run_debounced_diagnostics(
                        state_arc.clone(),
                        client.clone(),
                        sib,
                        debounce_ms,
                        sver,
                        srev,
                        Some(traversal_truncation.clone()),
                    ));
                }

                run_debounced_diagnostics(
                    state_arc,
                    client,
                    revalidation_uri,
                    debounce_ms,
                    trigger_version,
                    trigger_revision,
                    Some(traversal_truncation),
                )
                .await;
            });
        }

        // Schedule debounced diagnostics for all affected files (Requirement 0.5)
        // The edited file uses a shorter debounce for near-instant feedback;
        // dependent files use the longer revalidation debounce.
        for (affected_uri, trigger_version, trigger_revision) in work_items {
            let state_arc = self.state.clone();
            let client = self.client.clone();
            let traversal_truncation = self.traversal_truncation.clone();
            let debounce = if affected_uri == uri {
                edited_file_debounce_ms
            } else {
                dependent_debounce_ms
            };

            tokio::spawn(run_debounced_diagnostics(
                state_arc,
                client,
                affected_uri,
                debounce,
                trigger_version,
                trigger_revision,
                Some(traversal_truncation),
            ));
        }

        self.check_and_warn_traversal_truncation().await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = &params.text_document.uri;

        let package_close_path: bool = {
            let state = self.state.read().await;
            state
                .package_inputs
                .workspace_root
                .as_ref()
                .zip(uri.to_file_path().ok())
                .is_some_and(|(root, path)| {
                    crate::package_state::is_r_source_path(&path, root).is_some()
                        || is_package_manifest_path(&path, root)
                })
        };

        // Closing reverts package inputs to DISK truth — never the discarded
        // buffer (the same convergence rule the cross-file resync below
        // follows, and what `refresh_rprofile_prelude_from_disk` documents
        // for `.Rprofile`). A missing or undecodable file yields `None`,
        // which `HandlerEvent::DidClose` translates into removing the file
        // from `package_inputs` — so a file deleted from disk while its
        // buffer was open drops its package-internal symbols on close
        // instead of retaining them until an (never-coming) watcher event.
        //
        // Deliberate double read: the spawned cross-file resync re-reads the
        // same file. Handing this snapshot to the resync would trade
        // commit-time freshness (the resync's read happens right before its
        // veto+commit) for one saved small-file read on a warm page cache —
        // not worth the coupling. If a disk write lands between the two
        // reads, package and cross-file state briefly reflect different
        // snapshots; the write also fires a watcher CHANGED event whose
        // pipeline re-syncs both, so the straddle self-heals.
        let close_text: Option<Arc<str>> = if package_close_path {
            match uri.to_file_path() {
                Ok(path) => {
                    tokio::task::spawn_blocking(move || {
                        // BOM-aware decode (read off the async runtime).
                        crate::state::read_source(&path)
                            .ok()
                            .map(|text| Arc::from(text.as_str()))
                    })
                    .await
                    .unwrap_or(None)
                }
                Err(_) => None,
            }
        } else {
            None
        };

        // If the closed document is the workspace-root `.Rprofile`, its prelude
        // must be re-scanned from disk after this block: closing discards any
        // unsaved buffer edits the live did_change refresh folded in.
        let mut rprofile_close_root: Option<std::path::PathBuf> = None;

        // Issue #558: closing a graph-relevant file schedules a disk resync
        // (spawned after this block) so cross-file state derived from a
        // discarded unsaved buffer converges back to disk truth. Gated on
        // the tracked document's own `file_type` (set inside the write-lock
        // block below), not the URI extension: that is what fed the buffer's
        // edges into the graph in the first place, and it covers
        // extension-less R documents like `.Rprofile` as well as `.R` and
        // `.Rmd`/`.qmd`. Non-`file:` URIs (untitled buffers) have no disk
        // truth to converge to.
        let has_disk_path = uri.to_file_path().is_ok();
        let close_resync;
        let mut resync_old_meta: Option<std::sync::Arc<crate::cross_file::CrossFileMetadata>> =
            None;
        let mut resync_old_interface_hash: Option<u64> = None;
        let mut resync_chunk_kind: Option<crate::chunks::ChunkKind> = None;

        let (sibling_fanout, debounce_ms): (Vec<(Url, Option<i32>, Option<u64>)>, u64) = {
            let mut state = self.state.write().await;

            // Capture the pre-close view the disk resync needs, BEFORE the
            // document stores are wiped below. `old_meta` must come from the
            // buffer tier: working-directory change detection compares the
            // buffer's `# raven: cd` (what dependents last resolved against)
            // against the disk state the resync installs. The dependent walk
            // runs over the buffer-derived graph edges, which deliberately
            // stay in place until the resync commits — there is no transient
            // empty-edge window for dependents to observe.
            close_resync = has_disk_path
                && state
                    .documents
                    .get(uri)
                    .is_some_and(|doc| doc.file_type == crate::file_type::FileType::R);
            if close_resync {
                resync_old_meta = state.get_enriched_metadata(uri);
                resync_old_interface_hash = state
                    .document_store
                    .get_without_touch(uri)
                    .map(|doc| doc.artifacts.interface_hash);
                // The buffer's chunk classification came from the editor's
                // languageId (#343); the resync must mask disk content the
                // same way — path reclassification would parse an
                // extensionless Rmd/Quarto document as plain R.
                // (The pre-commit dependent WALK deliberately lives inside
                // `run_close_resync`, off the handler path: it needs only
                // the pre-commit graph — the buffer-derived edges stay in
                // place until that resync's own commit replaces them.)
                resync_chunk_kind = state.documents.get(uri).map(|doc| doc.chunk_kind);
            }

            // Close in new DocumentStore (Requirement 1.5)
            state.document_store.close(uri);

            // Clear diagnostics gate state
            state.diagnostics_gate.clear(uri);

            // Cancel pending revalidation
            state.cross_file_revalidation.cancel(uri);

            // Remove from activity tracking
            state.cross_file_activity.remove(uri);

            // Close the document (legacy)
            state.close_document(uri);

            // Snapshot package visibility before applying the close event, so we
            // can detect symbol/import changes caused by switching the closed
            // file's content from Open (unsaved buffer) to Disk. Open siblings
            // resolved against the unsaved buffer must refresh their diagnostics
            // when the buffer-only symbols disappear; otherwise stale "no error"
            // diagnostics linger until the user touches each sibling.
            let old_ns_model = state.package_state.namespace_model().cloned();
            let old_contribution = state.package_state.scope_contribution().clone();

            // In package mode, update package inputs on close from the disk
            // snapshot captured above (buffer edits are discarded on close).
            if package_close_path {
                let event = crate::package_state::event::HandlerEvent::DidClose {
                    uri: uri.clone(),
                    on_disk_text: close_text,
                };
                if let Some(delta) =
                    crate::package_state::event::translate(&mut state.package_inputs, event)
                {
                    state.apply_package_event(&delta);
                }
            }

            // Workspace-root `.Rprofile` is not a `package_close_path` (it is
            // neither an R-source path nor a manifest), so the close above does
            // not touch the prelude. Schedule a disk re-scan after the lock to
            // revert any unsaved buffer edits (off-lock; the scan follows
            // transitive source()).
            if let Some(root) = state.package_inputs.workspace_root.clone()
                && uri.to_file_path().ok().as_deref() == Some(root.join(".Rprofile").as_path())
            {
                rprofile_close_root = Some(root);
            }

            // Closing a helper the `.Rprofile` transitively source()s also
            // reverts the prelude to disk: its symbols reached the prelude
            // through the live buffer, and the watcher path — which folds
            // the same rescan into its R-file deltas (Task 12) — skipped
            // every event while this document was open.
            if rprofile_close_root.is_none()
                && state.package_inputs.model_rprofile
                && let Some(root) = state.package_inputs.workspace_root.clone()
                && uri.to_file_path().ok().is_some_and(|p| {
                    path_in_rprofile_sourced_set(&p, &state.package_inputs.rprofile_sourced_files)
                })
            {
                rprofile_close_root = Some(root);
            }

            // Detect package-mode visibility changes triggered by the close.
            // When an unsaved buffer with buffer-only symbols (e.g. a freshly
            // added function) closes, those symbols leave `r_internal_symbols`
            // and any open sibling that referenced them needs its diagnostics
            // recomputed. Mirrors the watched-file fanout above (including
            // `max_revalidations_per_trigger`).
            let mut sibling_fanout: Vec<(Url, Option<i32>, Option<u64>)> = Vec::new();
            let pkg_visibility_changed = state.package_state.namespace_model()
                != old_ns_model.as_ref()
                || state.package_state.scope_contribution() != &old_contribution;
            if pkg_visibility_changed {
                if let Some(root) = state.package_inputs.workspace_root.clone() {
                    sibling_fanout = collect_close_fanout_siblings(&state, uri, &root);
                    // Close-time visibility changes must also reach
                    // devtools::load_all() carriers (root-level analysis.R,
                    // scripts/) and their source-graph neighborhoods —
                    // `is_r_source_path` misses those, and every
                    // watcher-driven visibility fanout already widens this
                    // way. Re-cap the union so the extension cannot bypass
                    // `max_revalidations_per_trigger`.
                    let mut uris: Vec<Url> =
                        sibling_fanout.iter().map(|(u, _, _)| u.clone()).collect();
                    let mut seen: std::collections::HashSet<Url> = uris.iter().cloned().collect();
                    seen.insert(uri.clone());
                    extend_affected_for_load_all_revalidation_from_state(
                        &mut uris, &mut seen, &state, &root,
                    );
                    cap_watched_file_revalidations(
                        &mut uris,
                        &state.cross_file_activity,
                        state.cross_file_config.max_revalidations_per_trigger,
                    );
                    sibling_fanout = uris
                        .into_iter()
                        .filter_map(|u| {
                            let doc = state.documents.get(&u)?;
                            Some((u, doc.version, Some(doc.revision)))
                        })
                        .collect();
                }
                // When the disk resync runs, it owns the siblings' marking and
                // publishing (folded into run_close_resync so an open file
                // that is both a package sibling and a graph dependent is
                // force-marked exactly once, and publishes post-commit).
                // Marking here as well would strand a counter: the resync's
                // debounced schedule cancels the pending sibling task.
                if !sibling_fanout.is_empty() && !close_resync {
                    state
                        .diagnostics_gate
                        .mark_force_republish_many(sibling_fanout.iter().map(|(u, _, _)| u));
                }
            }

            // Invalidate the workspace index entry so the next scan re-reads from disk.
            if let Ok(p) = uri.to_file_path()
                && state
                    .package_workspace()
                    .is_some_and(|pkg| is_package_source_dir(&p, &pkg.root))
            {
                state.cross_file_workspace_index.invalidate(uri);
                state.workspace_index_new.invalidate(uri);
            }

            // Refresh the pin set now that the open set has shrunk; URIs reachable
            // only from the closed file are no longer protected from eviction.
            state.recompute_open_neighborhood_pins();

            let debounce_ms = state.cross_file_config.revalidation_debounce_ms;
            (sibling_fanout, debounce_ms)
        };

        // Revert the prelude to on-disk `.Rprofile` after a close (off-lock).
        if let Some(root) = rprofile_close_root {
            self.refresh_rprofile_prelude_from_disk(root).await;
        }

        if close_resync {
            // Issue #558: converge cross-file state back to disk truth
            // off-lock. The buffer-derived graph edges were deliberately left
            // untouched in the write-lock block above; the spawned resync
            // replaces them atomically from disk (or removes the file's entry
            // if it is gone), then republishes affected open dependents —
            // including the package-visibility siblings, which ride the
            // resync so each URI is marked exactly once and published after
            // the commit — through the normal gate. A close-then-reopen race
            // is handled by the resync's commit-time veto. Never calls
            // `scan_workspace` — this re-reads exactly one file.
            let sibling_uris: Vec<Url> = sibling_fanout.into_iter().map(|(u, _, _)| u).collect();
            tokio::spawn(run_close_resync(
                self.state.clone(),
                self.client.clone(),
                self.traversal_truncation.clone(),
                uri.clone(),
                resync_chunk_kind,
                resync_old_meta,
                resync_old_interface_hash,
                sibling_uris,
            ));
        } else {
            // No disk resync for this close (non-file URI or non-R document):
            // schedule the package-sibling revalidation directly, outside the
            // write lock. The debounce window collapses bursts (e.g. "close
            // all" closing many files in quick succession) into a single
            // republish per sibling.
            for (sibling_uri, trigger_version, trigger_revision) in sibling_fanout {
                let state_arc = self.state.clone();
                let client = self.client.clone();
                let traversal_truncation = self.traversal_truncation.clone();
                tokio::spawn(run_debounced_diagnostics(
                    state_arc,
                    client,
                    sibling_uri,
                    debounce_ms,
                    trigger_version,
                    trigger_revision,
                    Some(traversal_truncation),
                ));
            }
        }
    }

    /// Apply updated workspace configuration, invalidate caches that affect name-resolution scope, and re-run diagnostics for all open documents.
    ///
    /// This handles parsing the new cross-file configuration from the provided LSP settings, applies it to shared state if valid, invalidates cross-file resolution caches, marks open documents for force republish, optionally reinitializes the PackageLibrary when package-related settings change, and schedules diagnostics publication for every open document.
    ///
    /// # Parameters
    ///
    /// - `params`: LSP DidChangeConfigurationParams containing the new settings to parse and apply.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Called from an async context when the client sends updated configuration.
    /// # use tower_lsp::LanguageServer;
    /// # async fn example(backend: &raven::backend::Backend, params: tower_lsp::lsp_types::DidChangeConfigurationParams) {
    /// backend.did_change_configuration(params).await;
    /// # }
    /// ```
    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // Requirement 11.11: When configuration changes, re-resolve scope chains for open documents
        log::trace!("Configuration changed, parsing new config and scheduling revalidation");

        // Surface cross-file validation errors as a VS Code toast (matches
        // pre-Task-7 behavior). Recompute below also logs the same error via
        // log::warn!, so we don't need the value — only the side effect.
        if let Err(err) = parse_cross_file_config(&params.settings) {
            self.client.show_message(MessageType::WARNING, err).await;
        }

        let new_raw_client = params.settings.clone();
        let (project_root, previous_project_config_path, workspace_folders, old_raw_client): (
            Option<std::path::PathBuf>,
            Option<std::path::PathBuf>,
            Vec<Url>,
            serde_json::Value,
        ) = {
            let state = self.state.read().await;
            (
                state
                    .workspace_folders
                    .first()
                    .and_then(|u| u.to_file_path().ok()),
                state.project_config_path.clone(),
                state.workspace_folders.clone(),
                state.raw_client_settings.clone(),
            )
        };
        let old_discovery_options = self.discovery_options_from_client_settings(&old_raw_client);
        let new_discovery_options = self.discovery_options_from_client_settings(&new_raw_client);
        // Any discovery-affecting setting change triggers rediscovery. Today
        // that's only `read_home_lintr`, but comparing the whole struct keeps
        // this correct if `DiscoveryOptions` grows another field.
        let rediscover_project_config = old_discovery_options != new_discovery_options;
        let project_layer = if rediscover_project_config {
            let layer = project_root
                .as_deref()
                .map(|root| {
                    crate::config_file::discover_and_load_with_options(root, &new_discovery_options)
                })
                .unwrap_or(crate::config_file::DiscoveredLoad::None);
            log_project_config_warnings(&layer);
            Some(layer)
        } else {
            None
        };

        // Lock-only window: snapshot pre-change configs, store the new raw
        // client settings, recompute parsed configs, recompile lint
        // overrides. NO I/O — the helper handles every blocking action
        // outside the lock.
        let (snapshot, project_config_path) = {
            let mut state = self.state.write().await;

            let prev = ConfigChangeSnapshot {
                prev_cross_file: state.cross_file_config.clone(),
                prev_lint: state.lint_config.clone(),
                prev_completion: state.completion_config.clone(),
                prev_hier_support: state.symbol_config.hierarchical_document_symbol_support,
            };

            // Store the new raw client settings and re-merge with the project
            // file (if any). recompute_parsed_configs() overwrites every
            // parsed config; absent sections reset to defaults.
            state.raw_client_settings = new_raw_client;
            if let Some(layer) = project_layer {
                apply_project_config_layer(&mut state, layer);
            }
            // `recompute_parsed_configs` now also recompiles
            // `state.lint_overrides` from the merged settings.
            crate::config_file::recompute_parsed_configs(&mut state);

            (prev, state.project_config_path.clone())
        };

        // Helper drives change detection, package rebuilds, watcher
        // restart, completion re-registration, and force-republish
        // marking. Returns the URIs to publish (empty when only the
        // watcher-lifecycle settings flipped).
        let to_publish = self.reconcile_after_config_recompute(snapshot).await;

        if rediscover_project_config {
            if external_project_config_watch_changed(
                &workspace_folders,
                &old_discovery_options,
                &new_discovery_options,
            ) {
                self.refresh_external_project_config_watch(
                    workspace_folders,
                    new_discovery_options,
                )
                .await;
            }
            if previous_project_config_path != project_config_path {
                self.notify_project_config_loaded(project_config_path.as_deref());
            }
        }

        for uri in to_publish {
            self.publish_diagnostics(&uri).await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        log::trace!(
            "Received watched files change: {} changes",
            params.changes.len()
        );

        let (project_root, raw_client): (Option<std::path::PathBuf>, serde_json::Value) = {
            let state = self.state.read().await;
            (
                state
                    .workspace_folders
                    .first()
                    .and_then(|u| u.to_file_path().ok()),
                state.raw_client_settings.clone(),
            )
        };
        let discovery_options = self.discovery_options_from_client_settings(&raw_client);

        // Detect raven.toml / .lintr events. These files are never part of
        // the source-file flow; only the subset that lies on the active
        // discovery walk triggers a config-layer reload.
        let config_file_change_uris: std::collections::HashSet<Url> = params
            .changes
            .iter()
            .filter(|c| {
                let Ok(p) = c.uri.to_file_path() else {
                    return false;
                };
                is_project_config_file(&p)
            })
            .map(|c| c.uri.clone())
            .collect();
        let project_config_changed = params.changes.iter().any(|c| {
            if !config_file_change_uris.contains(&c.uri) {
                return false;
            }
            let Ok(p) = c.uri.to_file_path() else {
                return false;
            };
            config_file_event_affects_project_config(
                &p,
                project_root.as_deref(),
                &discovery_options,
            )
        });

        if project_config_changed {
            // Step 1 (lock-free I/O): snapshot the workspace root, then run
            // discovery + config-file loading off-lock. Holding the write lock across
            // disk I/O violates the locking-discipline invariant in
            // CLAUDE.md.
            let project_layer = project_root
                .as_deref()
                .map(|root| {
                    crate::config_file::discover_and_load_with_options(root, &discovery_options)
                })
                .unwrap_or(crate::config_file::DiscoveredLoad::None);
            log_project_config_warnings(&project_layer);

            // Step 2 (under write lock, no I/O): apply the reload —
            // snapshot prev configs for downstream rebuild gating, swap
            // raw project settings, recompute parsed configs, recompile
            // overrides. The shared reconciliation helper drives every
            // downstream action outside the lock.
            let snapshot = {
                let mut state = self.state.write().await;
                let prev = ConfigChangeSnapshot {
                    prev_cross_file: state.cross_file_config.clone(),
                    prev_lint: state.lint_config.clone(),
                    prev_completion: state.completion_config.clone(),
                    prev_hier_support: state.symbol_config.hierarchical_document_symbol_support,
                };

                // Re-run discovery from the workspace root. Order matters:
                // raven.toml beats .lintr (DiscoveredConfig embodies that).
                apply_project_config_layer(&mut state, project_layer);

                // `recompute_parsed_configs` now also recompiles
                // `state.lint_overrides` from the merged settings.
                crate::config_file::recompute_parsed_configs(&mut state);

                prev
            };
            // Helper drives change detection, package rebuilds, watcher
            // restart, completion re-registration, and force-republish
            // marking. Returns the URIs to publish (empty when only the
            // watcher-lifecycle settings flipped — nothing
            // diagnostic-affecting moved).
            let to_publish = self.reconcile_after_config_recompute(snapshot).await;

            // Re-emit `raven/projectConfigLoaded` so clients (the VS Code
            // status bar in particular) see the new state without a
            // window reload. The notification fires unconditionally for
            // every reload event — even when the new path matches the
            // old — so consumers can treat it as the authoritative
            // "what's in effect now" signal. `path: null` means the
            // config file was removed.
            let new_path = {
                let state = self.state.read().await;
                state.project_config_path.clone()
            };
            self.notify_project_config_loaded(new_path.as_deref());

            // Re-publish diagnostics for every open document. The
            // force-republish markers set by the helper ensure
            // `publish_diagnostics` actually emits (rather than being
            // short-circuited by an unchanged-version check).
            //
            // Per-URI publishes are independent: each takes brief read
            // locks on `state` and the `diagnostics_gate`'s per-URI
            // `HashMap` entries, then releases before doing the async
            // missing-file checks and the `client.publish_diagnostics`
            // send. Running them in parallel — bounded — avoids N×
            // latency on workspaces with many open files. The
            // monotonic-publish invariant still holds: distinct URIs
            // commit to distinct gate entries, and every spawned task is
            // joined before this handler returns so a later `did_change`
            // can't race a still-running reload publish.
            const MAX_CONCURRENT_PUBLISHES: usize = 8;
            let mut join_set = tokio::task::JoinSet::new();
            for uri in to_publish {
                while join_set.len() >= MAX_CONCURRENT_PUBLISHES {
                    join_set.join_next().await;
                }
                let state_arc = self.state.clone();
                let client = self.client.clone();
                join_set.spawn(async move {
                    publish_diagnostics_inner(&state_arc, &client, &uri).await;
                });
            }
            while join_set.join_next().await.is_some() {}
        }

        // If every change was a config file, the source-file flow below has
        // nothing to do. Otherwise, build a filtered `params` containing
        // only the non-config events and continue.
        let remaining_changes: Vec<FileEvent> = params
            .changes
            .iter()
            .filter(|c| !config_file_change_uris.contains(&c.uri))
            .cloned()
            .collect();
        if remaining_changes.is_empty() {
            return;
        }
        let params = DidChangeWatchedFilesParams {
            changes: remaining_changes,
        };

        // Collect URIs to update and affected open documents
        let (uris_to_update, mut affected_open_docs, pkg_manifest_changes): (
            Vec<Url>,
            Vec<Url>,
            Vec<(Url, bool)>,
        ) = {
            let mut state = self.state.write().await;
            let mut to_update = Vec::new();
            let mut affected: Vec<Url> = Vec::new();
            // O(1) dedupe set companion for `affected` — large bursts of
            // watched-file changes can otherwise spend the lock-hold time
            // doing Vec::contains scans per dependent.
            let mut affected_set: std::collections::HashSet<Url> = std::collections::HashSet::new();
            // Track whether any deletion touched `package_inputs`, so that
            // mixed batches (deletions + non-R creates/changes) still trigger
            // a re-derive. Without this, a batch like
            // `DELETE R/foo.R + CREATE inst/data.csv` skipped the sync
            // `apply_package_event` (because `!to_update.is_empty()`) AND
            // skipped the async path's `has_pkg_files` check (because
            // `inst/data.csv` is not a package source file) — leaving
            // symbols from the deleted R file stale in
            // `scope_contribution.r_internal_symbols` until the next R/
            // edit happened to trigger a derive.
            let mut had_pkg_deletion = false;

            for change in &params.changes {
                let uri = &change.uri;

                // Skip if document is open (open docs are authoritative)
                if state.documents.contains_key(uri) {
                    log::trace!("Skipping watched file change for open document: {}", uri);
                    continue;
                }

                // Rmd/Quarto files flow through like plain R since #558:
                // `resync_file_from_disk` masks path-classified `.Rmd`/`.qmd`
                // content (chunk bodies only, never prose — the pollution the
                // old skip guarded against), and the close resync now
                // installs graph/index state for closed Rmd documents that a
                // later external edit or deletion must be able to update or
                // remove.

                match change.typ {
                    FileChangeType::CREATED | FileChangeType::CHANGED => {
                        // Invalidate disk-backed caches
                        state.cross_file_file_cache.invalidate(uri);
                        state.cross_file_workspace_index.invalidate(uri);

                        // Schedule debounced update in new WorkspaceIndex (Requirement 5.1)
                        state.workspace_index_new.schedule_update(uri.clone());

                        // Schedule for async update (legacy)
                        to_update.push(uri.clone());

                        // Find open neighbors (both backward dependents and
                        // forward children) of this file. Forward children
                        // need revalidation because their inherited scope is
                        // taken from this file's symbols at the source() call.
                        let neighbors =
                            crate::cross_file::revalidation::compute_affected_dependents_after_edit(
                                uri,
                                true,
                                false,
                                &state.cross_file_graph,
                                |u| state.documents.contains_key(u),
                                state.cross_file_config.max_chain_depth,
                                state.cross_file_config.max_transitive_dependents_visited,
                            );
                        for dep in neighbors {
                            if affected_set.insert(dep.clone()) {
                                affected.push(dep);
                            }
                        }
                        log::trace!("Invalidated caches for changed file: {}", uri);
                    }
                    FileChangeType::DELETED => {
                        let neighbors = remove_file_from_cross_file_state(&mut state, uri);
                        for dep in neighbors {
                            if affected_set.insert(dep.clone()) {
                                affected.push(dep);
                            }
                        }

                        // Update package inputs for deleted R/*.R or DESCRIPTION/NAMESPACE files.
                        // The event-driven path in the post-loop block handles the derive.
                        // (Accumulated here; apply_package_event called once after the loop.)
                        {
                            let event =
                                crate::package_state::event::HandlerEvent::WatchedFileChanged {
                                    uri: uri.clone(),
                                    on_disk_text: None,
                                    deleted: true,
                                };
                            if crate::package_state::event::translate(
                                &mut state.package_inputs,
                                event,
                            )
                            .is_some()
                            {
                                had_pkg_deletion = true;
                            }
                        }

                        log::trace!("Removed deleted file from cross-file state: {}", uri);
                    }
                    _ => {}
                }
            }

            // CREATED/CHANGED graph mutations happen in the async disk-read
            // pass below. When that pass is needed, defer capping and
            // force-republish marking until after the post-update graph walk
            // has added newly reachable open documents. For DELETED-only
            // batches, the graph is already updated here, so cap and mark now.
            if to_update.is_empty() {
                // Derive package state after processing all deletions.
                // The translate calls above accumulated input mutations; now
                // derive the final state from the updated inputs. Skip the
                // re-derive when no package source files were deleted —
                // non-package deletions (e.g. `data/foo.csv`) leave
                // `package_inputs` untouched, and a re-derive from
                // unchanged inputs cannot change `package_state`.
                if should_rederive_after_deletion_batch(
                    state.package_inputs.workspace_root.is_some(),
                    had_pkg_deletion,
                ) {
                    let old_ns_model = state.package_state.namespace_model().cloned();
                    let old_contribution = state.package_state.scope_contribution().clone();
                    state.apply_package_event(&crate::package_state::PackageInputDelta::Initial);
                    let pkg_visibility_changed = state.package_state.namespace_model()
                        != old_ns_model.as_ref()
                        || state.package_state.scope_contribution() != &old_contribution;
                    if pkg_visibility_changed
                        && let Some(root) = state.package_inputs.workspace_root.clone()
                    {
                        extend_with_open_package_docs(
                            &mut affected,
                            &mut affected_set,
                            &state,
                            &root,
                        );
                        // R/+tests/ fanout above misses arbitrary load_all
                        // carriers (e.g. root-level analysis.R / scripts/) and
                        // their source-graph neighborhood; widen for them too.
                        extend_affected_for_load_all_revalidation_from_state(
                            &mut affected,
                            &mut affected_set,
                            &state,
                            &root,
                        );
                    }
                }

                cap_watched_file_revalidations(
                    &mut affected,
                    &state.cross_file_activity,
                    state.cross_file_config.max_revalidations_per_trigger,
                );
                // Bulk-mark force-republish on the post-truncation set under a
                // single write-lock acquisition.
                state
                    .diagnostics_gate
                    .mark_force_republish_many(affected.iter());
            } else if had_pkg_deletion && state.package_inputs.workspace_root.is_some() {
                // Mixed batch: deletions of package source files were applied
                // above but `to_update` is non-empty, so the DELETED-only
                // branch doesn't run. The async block below gates its derive
                // on `uris_to_update` containing a package source path, which
                // doesn't cover deletion-only mutations to `package_inputs`.
                // Run the derive eagerly here so sibling diagnostics don't
                // see stale `r_internal_symbols` entries for the deleted
                // files until some later edit happens to trigger a derive.
                let old_ns_model = state.package_state.namespace_model().cloned();
                let old_contribution = state.package_state.scope_contribution().clone();
                state.apply_package_event(&crate::package_state::PackageInputDelta::Initial);
                let pkg_visibility_changed = state.package_state.namespace_model()
                    != old_ns_model.as_ref()
                    || state.package_state.scope_contribution() != &old_contribution;
                if pkg_visibility_changed
                    && let Some(root) = state.package_inputs.workspace_root.clone()
                {
                    extend_with_open_package_docs(&mut affected, &mut affected_set, &state, &root);
                    extend_affected_for_load_all_revalidation_from_state(
                        &mut affected,
                        &mut affected_set,
                        &state,
                        &root,
                    );
                }
            }
            // Watched-file deletions can drop edges that put a closed neighbor
            // outside the open-document neighborhood; refresh the pin set so
            // newly unreachable URIs become LRU-evictable again.
            state.recompute_open_neighborhood_pins();

            // Identify DESCRIPTION/NAMESPACE changes for the event-driven path.
            // Content is read outside the lock (spawn_blocking) below.
            let pkg_manifest_changes: Vec<(Url, bool)> = {
                let workspace_root = state
                    .workspace_folders
                    .first()
                    .and_then(|u| u.to_file_path().ok());
                workspace_root
                    .map(|root| {
                        params
                            .changes
                            .iter()
                            .filter_map(|c| {
                                let p = c.uri.to_file_path().ok()?;
                                // Open documents are authoritative: a watcher
                                // event for an OPEN buffer must not overwrite it
                                // from disk. The main watcher loop skips open
                                // docs; mirror that here. This matters for
                                // `.Rprofile` (now a live R document) — without
                                // it, an external disk touch (or even the
                                // editor's own save) would clobber the
                                // live-buffer prelude with the on-disk scan. The
                                // live did_open/did_change/did_close paths own
                                // the prelude while `.Rprofile` is open.
                                if state.documents.contains_key(&c.uri) {
                                    return None;
                                }
                                if p == root.join("DESCRIPTION")
                                    || p == root.join("NAMESPACE")
                                    || p == root.join(".Rprofile")
                                {
                                    Some((c.uri.clone(), c.typ == FileChangeType::DELETED))
                                } else {
                                    None
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };

            (to_update, affected, pkg_manifest_changes)
        };

        // --- Package manifest (DESCRIPTION/NAMESPACE) event-driven update ---
        // For each changed manifest file, read content outside the lock then
        // translate into package input mutations and re-derive state.
        if !pkg_manifest_changes.is_empty() {
            // Snapshot the root + modeling flag so the `.Rprofile` prelude scan
            // can run OFF-lock below (it follows transitive source(); the
            // locking-discipline invariant forbids that scan under the write
            // lock — see `initialize_package_inputs_from_state`).
            let (ws_root, model_rprofile) = {
                let state = self.state.read().await;
                (
                    state.package_inputs.workspace_root.clone(),
                    state.package_inputs.model_rprofile,
                )
            };
            let rprofile_path = ws_root.as_ref().map(|r| r.join(".Rprofile"));

            // DESCRIPTION/NAMESPACE carry on-disk text for `translate`; the
            // `.Rprofile` payload is a prelude scan precomputed off-lock (None
            // when deleted or modeling disabled — both clear the prelude).
            enum ManifestPayload {
                Text(Option<std::sync::Arc<str>>),
                Rprofile(Option<crate::package_state::rprofile::RprofileScan>),
            }

            let mut manifest_contents: Vec<(Url, ManifestPayload, bool)> = Vec::new();
            for (uri, deleted) in &pkg_manifest_changes {
                let is_rprofile = uri.to_file_path().ok().as_deref() == rprofile_path.as_deref();
                let payload = if is_rprofile {
                    let scan = if *deleted || !model_rprofile {
                        None
                    } else if let Some(root) = ws_root.clone() {
                        tokio::task::spawn_blocking(move || {
                            crate::package_state::rprofile::scan_workspace_rprofile(&root)
                        })
                        .await
                        .ok()
                    } else {
                        None
                    };
                    ManifestPayload::Rprofile(scan)
                } else if *deleted {
                    ManifestPayload::Text(None)
                } else if let Ok(path) = uri.to_file_path() {
                    let on_disk_text = tokio::task::spawn_blocking(move || {
                        std::fs::read_to_string(path)
                            .ok()
                            .map(|s| std::sync::Arc::from(s.as_str()))
                    })
                    .await
                    .unwrap_or(None);
                    ManifestPayload::Text(on_disk_text)
                } else {
                    ManifestPayload::Text(None)
                };
                manifest_contents.push((uri.clone(), payload, *deleted));
            }

            // Apply manifest events under write lock
            let mut state = self.state.write().await;
            let mut deltas = Vec::new();
            for (uri, payload, deleted) in manifest_contents {
                let delta = match payload {
                    ManifestPayload::Text(on_disk_text) => {
                        let event = crate::package_state::event::HandlerEvent::WatchedFileChanged {
                            uri,
                            on_disk_text,
                            deleted,
                        };
                        crate::package_state::event::translate(&mut state.package_inputs, event)
                    }
                    // Apply the off-lock prelude scan via the lock-safe seam.
                    // `None` (delete/disabled) clears; the deletion delta is
                    // still emitted when enabled so the script fanout fires.
                    ManifestPayload::Rprofile(scan) => {
                        crate::package_state::event::apply_rprofile_scan(
                            &mut state.package_inputs,
                            scan,
                        )
                    }
                };
                if let Some(delta) = delta {
                    deltas.push(delta);
                }
            }
            if !deltas.is_empty() {
                let rprofile_changed = batch_contains_rprofile_changed(&deltas);
                let batch = crate::package_state::PackageInputDelta::Batch(deltas);
                state.apply_package_event(&batch);
                // A `Package:` rename changes which `system.file()` references
                // are self-package (branch 1), so re-resolve retained entries
                // against the new workspace name.
                let system_file_changed = state.resolve_system_file_in_workspace();
                log::info!("Updated package state after DESCRIPTION/NAMESPACE/.Rprofile change");
                // Force republish for all open R files so namespace model
                // changes propagate (they're not dependency-graph neighbors of
                // DESCRIPTION/NAMESPACE so the sync pass didn't add them).
                // Marking is gated on the publish path: see
                // `extend_affected_for_manifest_change`. The async block below
                // applies `max_revalidations_per_trigger` and force-marks only
                // the post-cap survivors, so marking everything here would
                // leave orphan markers on cap-dropped URIs.
                let open_keys_filtered: Vec<Url> = state
                    .package_inputs
                    .workspace_root
                    .as_ref()
                    .map(|root| {
                        state
                            .documents
                            .keys()
                            .filter(|uri| {
                                // DESCRIPTION/NAMESPACE: package-relevant files.
                                // .Rprofile prelude: any workspace R-language
                                // file (the prelude reaches scripts/, which are
                                // not "package-relevant").
                                is_package_relevant_open_uri(uri, root)
                                    || (rprofile_changed
                                        && uri.to_file_path().ok().is_some_and(|p| {
                                            crate::package_state::is_package_workspace_r_file(
                                                &p, root,
                                            )
                                        }))
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                let sync_publish_path = uris_to_update.is_empty();
                extend_affected_for_manifest_change(
                    &mut affected_open_docs,
                    open_keys_filtered,
                    sync_publish_path,
                    &state.diagnostics_gate,
                );
                // Files whose system.file resolution changed — and their open
                // transitive dependents — can fall outside the
                // package-relevant set (e.g. open scripts elsewhere in the
                // workspace); extend them with the same gated marking.
                let system_file_republish = state.system_file_republish_set(&system_file_changed);
                extend_affected_for_manifest_change(
                    &mut affected_open_docs,
                    system_file_republish,
                    sync_publish_path,
                    &state.diagnostics_gate,
                );
                drop(state);
            }
        }

        // Schedule async disk reads to update workspace index for changed files.
        // CREATED/CHANGED graph mutations happen inside the spawned task, so
        // diagnostic publishes for `affected_open_docs` must be deferred to
        // run *after* the graph is up to date — otherwise the debounced
        // diagnostic pass can race the async update and emit results from
        // the pre-update graph (Codex review #2).
        if !uris_to_update.is_empty() {
            let state_arc = self.state.clone();
            let client = self.client.clone();
            let traversal_truncation = self.traversal_truncation.clone();
            let mut affected_for_async = affected_open_docs.clone();
            // Track URIs already in `affected_for_async` so the post-update
            // recomputation can union new neighbors without rescans.
            let mut affected_for_async_set: std::collections::HashSet<Url> =
                affected_for_async.iter().cloned().collect();
            tokio::spawn(async move {
                for uri in &uris_to_update {
                    // Capture old metadata before the disk read (for WD change
                    // detection). The file was never open — the sync pass
                    // skips open documents — so every metadata tier already
                    // reflects the last-known disk state.
                    let old_meta = {
                        let state = state_arc.read().await;
                        state.get_enriched_metadata(uri)
                    };

                    // Affected dependents are collected but NOT force-marked
                    // yet: marking is deferred until after the full async
                    // union is capped, avoiding both duplicate markers and
                    // cap bypasses from post-update fanout.
                    match resync_file_from_disk(&state_arc, uri, None, old_meta, false).await {
                        ResyncOutcome::Updated {
                            affected_after_commit: deps,
                            ..
                        }
                        | ResyncOutcome::Removed {
                            affected_dependents: deps,
                        } => {
                            for dep in deps {
                                if affected_for_async_set.insert(dep.clone()) {
                                    affected_for_async.push(dep);
                                }
                            }
                            log::trace!("Updated workspace index for: {}", uri);
                        }
                        ResyncOutcome::Vetoed | ResyncOutcome::Skipped => {}
                    }
                }

                // Update package inputs and derive state for CREATED/CHANGED R/*.R
                // files so roxygen tag changes from external edits (e.g. git
                // checkout) propagate to the namespace model and internal symbols.
                {
                    let mut state = state_arc.write().await;
                    // Gate on any package source file — `is_r_source_path`
                    // matches both `R/` and `tests/testthat/`. Without the
                    // `tests/` branch, external edits that touch only test
                    // files (e.g. `git checkout` on a topic branch) would
                    // leave their RFileFacts stale in `package_state`.
                    let root_for_check = state.package_inputs.workspace_root.clone();
                    let model_rprofile = state.package_inputs.model_rprofile;
                    let rprofile_sourced = &state.package_inputs.rprofile_sourced_files;
                    let has_pkg_files = root_for_check.as_ref().is_some_and(|root| {
                        uris_to_update.iter().any(|u| {
                            u.to_file_path().ok().is_some_and(|p| {
                                crate::package_state::is_r_source_path(&p, root).is_some()
                                    || is_package_source_dir(&p, root)
                                    // data/ and data-raw/ CREATED/CHANGED events
                                    // also have dedicated translate() handlers
                                    // (dataset_names / sysdata_names rescans).
                                    || is_package_data_path(&p, root)
                                    // A helper that `.Rprofile` transitively
                                    // source()s can live outside the package's
                                    // tracked R dirs (e.g. scripts/, inst/).
                                    // Editing it isn't an RFileChanged, but it
                                    // must still reach translate() so the prelude
                                    // is re-scanned and its symbols/packages reach
                                    // open `scripts/` files (Task 12).
                                    || (model_rprofile
                                        && path_in_rprofile_sourced_set(&p, rprofile_sourced))
                            })
                        })
                    });
                    if has_pkg_files {
                        let mut deltas = Vec::new();
                        let mut ns_changed = false;
                        let old_ns_model = state.package_state.namespace_model().cloned();
                        // Snapshot the package's visibility/contribution state
                        // (Arc-backed clones are cheap) so visibility-only
                        // changes — e.g. internal symbol or NAMESPACE-import
                        // edits that don't alter exports — also trigger the
                        // open-file fanout below.
                        let old_contribution = state.package_state.scope_contribution().clone();
                        for uri in &uris_to_update {
                            if state.documents.contains_key(uri) {
                                continue; // open docs are authoritative; skip
                            }
                            // Use the file cache content (already inserted above).
                            let on_disk_text: Option<std::sync::Arc<str>> = state
                                .cross_file_file_cache
                                .get(uri)
                                .map(|s| std::sync::Arc::from(s.as_str()));
                            let event =
                                crate::package_state::event::HandlerEvent::WatchedFileChanged {
                                    uri: uri.clone(),
                                    on_disk_text,
                                    deleted: false,
                                };
                            if let Some(delta) = crate::package_state::event::translate(
                                &mut state.package_inputs,
                                event,
                            ) {
                                deltas.push(delta);
                            }
                        }
                        // Detect whether this batch carries a prelude rescan
                        // (Task 12): editing a helper that `.Rprofile` sources
                        // re-scans the prelude, whose symbols/packages reach
                        // `scripts/` files — files that are NOT `is_r_source_path`
                        // and so are missed by the R/+tests fanout below. The
                        // RProfileChanged delta may appear top-level or nested in
                        // the per-file Batch built by `translate_watched`, so check
                        // both. Computed BEFORE `deltas` is moved into `Batch`.
                        let rprofile_changed = batch_contains_rprofile_changed(&deltas);
                        if !deltas.is_empty() {
                            let batch = crate::package_state::PackageInputDelta::Batch(deltas);
                            state.apply_package_event(&batch);
                            ns_changed = state.package_state.namespace_model()
                                != old_ns_model.as_ref()
                                || state.package_state.scope_contribution() != &old_contribution;
                        }
                        if ns_changed {
                            // Namespace model changed (e.g. roxygen tags changed in an
                            // external edit). Add all open package files (R/ and
                            // tests/testthat/) to affected set so their @import
                            // diagnostics are refreshed. When the prelude rescanned,
                            // also add every open workspace R-language file (incl.
                            // `scripts/`), since the prelude contributes to their
                            // script scope.
                            if let Some(ref root) = state.package_inputs.workspace_root.clone() {
                                for open_uri in state.documents.keys() {
                                    if let Ok(p) = open_uri.to_file_path()
                                        && (crate::package_state::is_r_source_path(&p, root)
                                            .is_some()
                                            || (rprofile_changed
                                                && crate::package_state::is_package_workspace_r_file(
                                                    &p, root,
                                                )))
                                        && affected_for_async_set.insert(open_uri.clone())
                                    {
                                        affected_for_async.push(open_uri.clone());
                                    }
                                }
                                // The fanout above misses arbitrary load_all
                                // carriers (root-level analysis.R / scripts/)
                                // and their source-graph neighborhood; widen
                                // for them on the async path too.
                                extend_affected_for_load_all_revalidation_from_state(
                                    &mut affected_for_async,
                                    &mut affected_for_async_set,
                                    &state,
                                    root,
                                );
                            }
                        }
                    }
                }

                // Now that the graph reflects every CREATED/CHANGED file in
                // this batch, cap and force-mark the full union of affected
                // open documents before scheduling diagnostics. Running this
                // here (not before the spawn) guarantees the debounced
                // diagnostic pass builds its snapshot from the post-update
                // graph, and prevents post-update edge fanout from bypassing
                // `max_revalidations_per_trigger`.
                {
                    let state = state_arc.read().await;
                    cap_watched_file_revalidations(
                        &mut affected_for_async,
                        &state.cross_file_activity,
                        state.cross_file_config.max_revalidations_per_trigger,
                    );
                    state
                        .diagnostics_gate
                        .mark_force_republish_many(affected_for_async.iter());
                }
                Backend::publish_diagnostics_for_uris_bounded(
                    state_arc.clone(),
                    client.clone(),
                    affected_for_async,
                    Some(traversal_truncation.clone()),
                )
                .await;
            });
        } else {
            // DELETED-only path: the sync block already mutated the graph
            // (`remove_file` + `recompute_open_neighborhood_pins`), so
            // publishing synchronously sees the post-update state.
            for uri in affected_open_docs {
                self.publish_diagnostics(&uri).await;
            }
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.publish_diagnostics(&params.text_document.uri).await;
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let state = self.state.read().await;
        Ok(handlers::folding_range(&state, &params.text_document.uri))
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let state = self.state.read().await;
        Ok(handlers::selection_range(
            &state,
            &params.text_document.uri,
            params.positions,
        ))
    }

    /// Provides the document symbols for the specified text document URI.
    ///
    /// The returned value contains the symbol information or hierarchical document symbols
    /// for the document, or `None` when no symbols are available.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use tower_lsp::lsp_types::DocumentSymbolParams;
    /// # use tower_lsp::LanguageServer;
    /// # async fn example(backend: &raven::backend::Backend, params: DocumentSymbolParams) {
    /// let result = backend.document_symbol(params).await.unwrap();
    /// if let Some(symbols) = result {
    ///     // process symbols
    /// }
    /// # }
    /// ```
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let state = self.state.clone();
        let uri = params.text_document.uri;
        match tokio::task::spawn_blocking(move || {
            let handle = tokio::runtime::Handle::current();
            let state = handle.block_on(state.read());
            handlers::document_symbol(&state, &uri)
        })
        .await
        {
            Ok(result) => Ok(result),
            Err(e) => {
                log::trace!("document_symbol: spawn_blocking failed: {e}");
                Err(tower_lsp::jsonrpc::Error::internal_error())
            }
        }
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        enum Mode {
            // Live document, whole-document R tree already parsed by the
            // document store. Used for plain `.R` / `.r` files.
            FullR(tree_sitter::Tree, String),
            // R Markdown / Quarto: the whole-document R tree spans prose +
            // YAML + non-R chunks and is full of errors, so we route through
            // a chunk-aware path that re-parses each R chunk body in
            // isolation.
            Rmd(String),
        }
        let mode = {
            let state = self.state.read().await;
            let doc = match state.get_document(&params.text_document.uri) {
                Some(doc) => doc,
                None => return Ok(None),
            };
            if doc.file_type != crate::file_type::FileType::R {
                return Ok(None);
            }
            if doc.is_rmd_document() {
                Mode::Rmd(doc.text())
            } else {
                let tree = match &doc.tree {
                    Some(tree) => tree.clone(),
                    None => return Ok(None),
                };
                Mode::FullR(tree, doc.text())
            }
        };
        let join = tokio::task::spawn_blocking(move || match mode {
            Mode::FullR(tree, text) => handlers::semantic_tokens_full(&tree, &text),
            Mode::Rmd(text) => handlers::semantic_tokens_for_rmd_document(&text),
        })
        .await;
        match join {
            Ok(tokens) => Ok(Some(SemanticTokensResult::Tokens(tokens))),
            Err(e) => {
                log::trace!("semantic_tokens_full: spawn_blocking failed: {e}");
                Err(tower_lsp::jsonrpc::Error::internal_error())
            }
        }
    }

    /// Searches workspace symbols that match the provided query string.
    ///
    /// Returns an `Option<Vec<SymbolInformation>>` containing matching symbols when available,
    /// or `None` if the server has no symbol results for the query.
    ///
    /// # Examples
    ///
    /// ```text
    /// // Call from an async context with a prepared `backend` and params:
    /// let params = lsp_types::WorkspaceSymbolParams { query: "my_fn".into(), ..Default::default() };
    /// let symbols_opt = backend.symbol(params).await?;
    /// if let Some(symbols) = symbols_opt {
    ///     // inspect or assert on `symbols`
    /// }
    /// ```
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let state = self.state.read().await;
        Ok(handlers::workspace_symbol(&state, &params.query))
    }

    /// Compute code completions for a text document position.
    ///
    /// Returns `Some(CompletionResponse)` with completion items when completions are available for the
    /// given document position, `None` otherwise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tower_lsp::lsp_types::CompletionParams;
    /// # use tower_lsp::LanguageServer;
    ///
    /// // `backend` is an initialized `Backend` instance and `params` is a prepared `CompletionParams`.
    /// // This example shows the call site; actual construction of `Backend` and `params` is omitted.
    /// # async fn example(backend: &raven::backend::Backend, params: CompletionParams) {
    /// let result = backend.completion(params).await.unwrap();
    /// if let Some(response) = result {
    ///     // Inspect completion items in `response`
    /// }
    /// # }
    /// ```
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        // Clone Arc<RwLock<WorldState>> and params for the blocking closure.
        // Run in spawn_blocking since parameter resolution may call R subprocess
        // (blocking I/O via get_function_formals). This follows the same pattern
        // used by completion_resolve() to avoid blocking the async runtime.
        let state = self.state.clone();
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let context = params.context;
        match tokio::task::spawn_blocking(move || {
            let handle = tokio::runtime::Handle::current();
            let state = handle.block_on(state.read());
            handlers::completion(&state, &uri, position, context)
        })
        .await
        {
            Ok(result) => Ok(result),
            Err(e) => {
                log::trace!("completion: spawn_blocking failed: {e}");
                Err(tower_lsp::jsonrpc::Error::internal_error())
            }
        }
    }

    async fn completion_resolve(&self, item: CompletionItem) -> Result<CompletionItem> {
        log::trace!("completion_resolve: label={}", item.label);
        let state = self.state.read().await;
        // Clone the help cache Arc before moving into spawn_blocking
        let help_cache = state.help_cache.clone();
        // Snapshot open document contents for user-defined function resolve
        let document_contents: std::collections::HashMap<tower_lsp::lsp_types::Url, String> = state
            .documents
            .iter()
            .map(|(uri, doc)| (uri.clone(), doc.text()))
            .collect();
        // Resolve R executable path; falls back to "R" (PATH lookup) when not configured.
        let r_path: std::path::PathBuf = state
            .cross_file_config
            .packages_r_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("R"));
        drop(state);
        // Run in spawn_blocking since get_help() calls R subprocess (blocking I/O)
        match tokio::task::spawn_blocking(move || {
            handlers::completion_item_resolve(item, &help_cache, &document_contents, &r_path)
        })
        .await
        {
            Ok(resolved) => Ok(resolved),
            Err(e) => {
                log::trace!("completion_resolve: spawn_blocking failed: {e}");
                Err(tower_lsp::jsonrpc::Error::internal_error())
            }
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let state = self.state.read().await;
        Ok(handlers::hover(
            &state,
            &params.text_document_position_params.text_document.uri,
            params.text_document_position_params.position,
        )
        .await)
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        // Phase 1: sync work under read lock
        let ctx = {
            let state = self.state.read().await;
            handlers::prepare_signature_help(&state, &uri, position)
        }; // read lock released

        // Phase 2: async work (package help fetch) without holding the lock
        Ok(match ctx {
            Some(ctx) => handlers::resolve_signature_help(ctx).await,
            None => None,
        })
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let cancel = self
            .request_cancellation
            .token_for_current_request(CancellableRequestKind::GotoDefinition)
            .unwrap_or_else(handlers::DiagCancelToken::never);
        if cancel.is_cancelled() {
            return Err(JsonRpcError::request_cancelled());
        }
        let state = tokio::select! {
            state = self.state.read() => state,
            _ = cancel.cancelled() => return Err(JsonRpcError::request_cancelled()),
        };
        if cancel.is_cancelled() {
            return Err(JsonRpcError::request_cancelled());
        }

        let result = handlers::goto_definition_with_cancel(
            &state,
            &params.text_document_position_params.text_document.uri,
            params.text_document_position_params.position,
            &cancel,
        );
        if cancel.is_cancelled() {
            return Err(JsonRpcError::request_cancelled());
        }

        Ok(result)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let state = self.state.read().await;
        Ok(handlers::references(
            &state,
            &params.text_document_position.text_document.uri,
            params.text_document_position.position,
        ))
    }

    /// Handles on-type formatting requests triggered by newline characters.
    ///
    /// This provides AST-aware indentation for R code, computing the correct
    /// indentation based on syntactic context (pipe chains, function arguments,
    /// brace blocks, etc.).
    ///
    /// # Requirements
    ///
    /// Validates: Requirements 3.1, 6.1, 6.2, 8.3, 8.4
    /// - 3.1: Chain start detection with iteration limit
    /// - 6.1: Read FormattingOptions.tab_size from LSP request parameters
    /// - 6.2: Read FormattingOptions.insert_spaces from LSP request parameters
    /// - 8.3: Compute indentation using tree-sitter AST
    /// - 8.4: Return TextEdit array that overrides VS Code's declarative indentation
    ///
    /// # Error Handling
    ///
    /// - Invalid AST states: Falls back to regex-based detection
    /// - UTF-16 position validation: Validates position is within document bounds
    /// - Malformed FormattingOptions: Clamps tab_size to 1-8, defaults insert_spaces to true
    /// - Chain start infinite loop: Iteration limit of 1000 lines
    /// - Missing/unmatched delimiters: Handled gracefully with heuristics
    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        // Only handle registered trigger characters
        if params.ch != "\n" && !matches!(params.ch.as_str(), ")" | "]" | "}") {
            return Ok(None);
        }

        let state = self.state.read().await;

        // Get document from state
        let uri = &params.text_document_position.text_document.uri;
        let doc = match state.get_document(uri) {
            Some(d) => d,
            None => {
                log::trace!("on_type_formatting: document not found: {}", uri);
                return Ok(None);
            }
        };

        let position = params.text_document_position.position;

        // Rmd/Quarto: indentation is first-class inside R chunk bodies, but a
        // prose/YAML line maps to a blank masked line that the R indentation
        // engine would treat as top-level R — applying R rules to markdown.
        // Short-circuit unless the position is inside an R chunk body. The
        // chunk-position check uses the RAW text (fences are blanked in the
        // masked analysis text).
        if doc.is_rmd_document()
            && !crate::chunks::position_in_r_chunk_body(&doc.text(), position.line)
        {
            log::trace!(
                "on_type_formatting: skipping prose position in Rmd/Quarto document {}",
                uri
            );
            return Ok(None);
        }

        // Get tree-sitter AST
        let tree = match doc.tree.as_ref() {
            Some(t) => t,
            None => {
                log::trace!("on_type_formatting: no parse tree for: {}", uri);
                return Ok(None);
            }
        };

        // The tree is parsed from the analysis text (masked for Rmd); the
        // indentation engine must receive the same text so its byte offsets
        // and tree-sitter points line up. Behavior-neutral for plain R.
        let source = doc.analysis_text();

        // Get indentation style from server configuration
        let style = state.indentation_config.style;

        // If style is Off, disable all formatting — return no edits
        // so only Tier 1 declarative rules apply
        if style == indentation::IndentationStyle::Off {
            log::trace!("on_type_formatting: style is Off, returning no edits");
            return Ok(None);
        }

        // Handle closing delimiter triggers: detect and remove auto-close duplicates.
        // When VS Code auto-closes `(` → `()` and the user later types `)` after
        // Enter pushed the auto-closed `)` to a new line, the over-type mechanism
        // fails and a duplicate `)` is inserted. We detect this via tree-sitter:
        // if the character at the cursor position is the same delimiter and it
        // sits inside an ERROR node (unmatched bracket), we delete the duplicate.
        if matches!(params.ch.as_str(), ")" | "]" | "}") {
            if let Some(line_text) = source.lines().nth(position.line as usize) {
                // Convert UTF-16 column to byte offset for indexing and tree-sitter
                let byte_col = utf16_column_to_byte_offset(line_text, position.character);
                if byte_col < line_text.len() {
                    let next_byte = line_text.as_bytes()[byte_col];
                    let typed_byte = params.ch.as_bytes()[0];
                    if next_byte == typed_byte {
                        // Potential duplicate — check if the next delimiter is
                        // inside an ERROR node (unmatched bracket).
                        let point = tree_sitter::Point::new(position.line as usize, byte_col);
                        if let Some(node) =
                            tree.root_node().descendant_for_point_range(point, point)
                        {
                            let is_error =
                                node.is_error() || node.parent().is_some_and(|p| p.is_error());
                            if is_error {
                                log::trace!(
                                    "on_type_formatting: removing duplicate auto-closed '{}' at ({},{})",
                                    params.ch,
                                    position.line,
                                    byte_col
                                );
                                // TextEdit range uses UTF-16 offsets (LSP protocol)
                                return Ok(Some(vec![TextEdit {
                                    range: tower_lsp::lsp_types::Range {
                                        start: Position {
                                            line: position.line,
                                            character: position.character,
                                        },
                                        end: Position {
                                            line: position.line,
                                            character: position.character.saturating_add(1),
                                        },
                                    },
                                    new_text: String::new(),
                                }]));
                            }
                        }
                    }
                }
            }
            // No duplicate detected — no edits needed for delimiter triggers
            return Ok(None);
        }

        // Extract FormattingOptions (Requirements 6.1, 6.2)
        let raw_tab_size = params.options.tab_size;
        let tab_size = raw_tab_size.clamp(1, 8);
        if raw_tab_size == 0 {
            log::warn!("on_type_formatting: tab_size 0 is invalid, clamped to 1");
        } else if raw_tab_size > 8 {
            log::warn!(
                "on_type_formatting: tab_size {} is out of range, clamped to 8",
                raw_tab_size
            );
        }
        let insert_spaces = params.options.insert_spaces;

        // Build IndentationConfig
        let config = indentation::IndentationConfig {
            tab_size,
            insert_spaces,
            style,
        };

        // Detect syntactic context using AST (Requirement 8.3)
        // This handles invalid AST states with fallback to regex-based detection
        let context = indentation::detect_context(tree, &source, position, tab_size);

        if log::log_enabled!(log::Level::Trace) {
            let source_lines = source.lines().count();
            log::trace!(
                "on_type_formatting: pos=({},{}), context={:?}, style={:?}, tab_size={}, source_lines={}",
                position.line,
                position.character,
                context,
                style,
                tab_size,
                source_lines
            );
        }

        // Calculate target indentation
        let target_column = indentation::calculate_indentation(context, config.clone(), &source);

        // Generate TextEdit (Requirement 8.4)
        let edit = indentation::format_indentation(position.line, target_column, config, &source);

        log::trace!(
            "on_type_formatting: line={}, target_column={}, edit=({},{})-({},{}) new_text={:?}",
            position.line,
            target_column,
            edit.range.start.line,
            edit.range.start.character,
            edit.range.end.line,
            edit.range.end.character,
            edit.new_text
        );

        Ok(Some(vec![edit]))
    }
}

/// Convert a `get_help_html` result into the LSP JSON response for `raven.getHelpHtml`.
///
/// Used by the `execute_command` dispatcher both for cache hits and for the
/// `get_or_fetch` path, keeping the JSON shape in exactly one place.
fn help_html_to_json(
    result: std::result::Result<crate::help::HelpHtml, crate::help::HelpHtmlError>,
) -> serde_json::Value {
    match result {
        Ok(h) => serde_json::json!({
            "ok": true,
            "topic": h.topic,
            "package": h.package,
            "title": h.title,
            "html": h.html,
            "helpDir": h.help_dir,
            "libPaths": h.lib_paths,
        }),
        Err(e) => serde_json::json!({
            "ok": false,
            "reason": e.reason(),
            "message": format!("{e}"),
        }),
    }
}

impl Backend {
    async fn register_external_project_config_watch(
        &self,
        workspace_folders: Vec<Url>,
        discovery_options: crate::config_file::DiscoveryOptions,
    ) {
        use tower_lsp::lsp_types::Registration;

        let watchers =
            build_external_project_config_watchers(&workspace_folders, &discovery_options);
        let client = self.client.clone();
        if watchers.is_empty() {
            return;
        }
        let registration = Registration {
            id: "raven-active-project-config-file".into(),
            method: "workspace/didChangeWatchedFiles".into(),
            register_options: Some(
                serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers })
                    .unwrap(),
            ),
        };
        if let Err(e) = client.register_capability(vec![registration]).await {
            log::warn!("external project-config watch registration failed: {e}");
        }
    }

    async fn refresh_external_project_config_watch(
        &self,
        workspace_folders: Vec<Url>,
        discovery_options: crate::config_file::DiscoveryOptions,
    ) {
        use tower_lsp::lsp_types::Unregistration;

        let client = self.client.clone();
        let unregistration = Unregistration {
            id: "raven-active-project-config-file".into(),
            method: "workspace/didChangeWatchedFiles".into(),
        };
        let _ = client.unregister_capability(vec![unregistration]).await;
        self.register_external_project_config_watch(workspace_folders, discovery_options)
            .await;
    }

    /// Send the `raven/projectConfigLoaded` notification reflecting the
    /// current project-config state.
    ///
    /// Fires from `initialize`, the `did_change_watched_files`
    /// `raven.toml` / `.lintr` reload branch, and `did_change_configuration`
    /// rediscovery after client settings alter config discovery. Clients see a
    /// fresh payload when the active project config changes, not just at
    /// session start.
    ///
    /// Spawns the send on a detached tokio task so callers never hold a
    /// state lock across `client.send_notification`.
    fn notify_project_config_loaded(&self, path: Option<&std::path::Path>) {
        let client = self.client.clone();
        let payload = build_project_config_loaded_payload(path);
        tokio::spawn(async move {
            let _ = client
                .send_notification::<RavenProjectConfigLoaded>(payload)
                .await;
        });
    }

    /// Drive every downstream action that depends on which parsed-config
    /// field moved during a configuration reload.
    ///
    /// Caller contract — the helper assumes the caller has already, under a
    /// write lock on `self.state`:
    ///   1. Captured the pre-change parsed configs into a
    ///      [`ConfigChangeSnapshot`].
    ///   2. Mutated whichever raw settings layer the caller owns
    ///      (`raw_client_settings` for `did_change_configuration`;
    ///      `raw_project_settings` + `project_config_path` for
    ///      `did_change_watched_files`).
    ///   3. Called [`crate::config_file::recompute_parsed_configs`],
    ///      which now also recompiles `state.lint_overrides`.
    ///   4. Released the write lock before calling this helper.
    ///
    /// The helper acquires its own brief write lock to:
    ///   - restore `symbol_config.hierarchical_document_symbol_support`
    ///     (which `recompute_parsed_configs` resets to default),
    ///   - compute `*_changed` flags against the snapshot,
    ///   - apply the `packageMode` `SettingChanged` translate (and capture
    ///     the workspace root for off-lock disk I/O when the new mode is
    ///     non-`Disabled`),
    ///   - mark every open document for force-republish (unless only the
    ///     `packagesWatch*` lifecycle settings flipped).
    ///
    /// It releases that lock before doing any blocking work. Outside the
    /// lock, in order, it:
    ///   - re-reads `DESCRIPTION` / `NAMESPACE` on a `packageMode` mode
    ///     switch, then re-applies under a brief write lock,
    ///   - re-registers the completion capability if `triggerOnOpenParen`
    ///     changed,
    ///   - rebuilds `PackageLibrary` (R subprocess) if a package-rebuild
    ///     setting changed,
    ///   - restarts the libpath watcher if any package or
    ///     `packagesWatch*` setting changed,
    ///   - prefetches package exports for open documents if a
    ///     package-rebuild setting changed and packages remain enabled.
    ///
    /// Returns the open URIs the caller should republish. Empty when only
    /// `packagesWatch*` flipped — nothing about diagnostic content moved,
    /// so the workspace-wide republish is a waste.
    async fn reconcile_after_config_recompute(&self, prev: ConfigChangeSnapshot) -> Vec<Url> {
        // Brief write lock: change detection, package_mode translate,
        // hier_support restore, force-republish marking. NO blocking I/O
        // happens inside this scope.
        let decisions = {
            let mut state = self.state.write().await;

            // `recompute_parsed_configs` reset `symbol_config` to defaults
            // when the caller ran it; restore the hierarchical-support flag
            // set from client capabilities at initialize() time.
            state.symbol_config.hierarchical_document_symbol_support = prev.prev_hier_support;

            let scope_changed = prev
                .prev_cross_file
                .scope_settings_changed(&state.cross_file_config);

            let old_diagnostics_enabled = prev.prev_cross_file.diagnostics_enabled;
            let new_diagnostics_enabled = state.cross_file_config.diagnostics_enabled;
            let diagnostics_enabled_changed = old_diagnostics_enabled != new_diagnostics_enabled;

            // Settings that require reinitializing `PackageLibrary` via an R
            // subprocess call (~100ms). Keep this narrow.
            let package_settings_changed = state.cross_file_config.packages_enabled
                != prev.prev_cross_file.packages_enabled
                || state.cross_file_config.packages_r_path != prev.prev_cross_file.packages_r_path
                || state.cross_file_config.packages_additional_library_paths
                    != prev.prev_cross_file.packages_additional_library_paths;

            // Watcher-only settings. Changing just these should restart the
            // filesystem watcher but MUST NOT trigger an R subprocess
            // roundtrip or wipe the package cache.
            let watch_settings_changed = state.cross_file_config.packages_watch_library_paths
                != prev.prev_cross_file.packages_watch_library_paths
                || state.cross_file_config.packages_watch_debounce_ms
                    != prev.prev_cross_file.packages_watch_debounce_ms;

            // If `watch_settings_changed` is the only thing that flipped,
            // the change is purely watcher-lifecycle; diagnostic content is
            // unaffected, so don't force every open document to revalidate.
            //
            // Coverage is automatic for every field on `CrossFileConfig`:
            // we compare the entire struct with the watch fields reverted,
            // so any new diagnostic-affecting field added to
            // `CrossFileConfig` is picked up without touching this site.
            // Config structs that live OUTSIDE `CrossFileConfig` (e.g.
            // `LintConfig`) still need an explicit `*_changed` guard below —
            // extend the chain when adding another such struct.
            let lint_config_changed = state.lint_config != prev.prev_lint;

            let only_watch_changed = watch_settings_changed && !lint_config_changed && {
                let mut probe = state.cross_file_config.clone();
                probe.packages_watch_library_paths =
                    prev.prev_cross_file.packages_watch_library_paths;
                probe.packages_watch_debounce_ms = prev.prev_cross_file.packages_watch_debounce_ms;
                probe == prev.prev_cross_file
            };

            let packages_enabled = state.cross_file_config.packages_enabled;
            let max_transitive_dependents_visited_changed =
                state.cross_file_config.max_transitive_dependents_visited
                    != prev.prev_cross_file.max_transitive_dependents_visited;

            // If `package_mode` changed, apply the setting change via the
            // event-driven path. For Disabled: translate immediately
            // (derive yields no workspace). For Auto/Enabled: capture root
            // for disk I/O outside the lock.
            let package_mode_changed =
                state.cross_file_config.package_mode != prev.prev_cross_file.package_mode;
            let pkg_mode_io_needed: Option<std::path::PathBuf> = if package_mode_changed {
                use crate::cross_file::config::PackageMode;
                let mode = state.cross_file_config.package_mode;
                let event =
                    crate::package_state::event::HandlerEvent::SettingChanged { new_mode: mode };
                if let Some(delta) =
                    crate::package_state::event::translate(&mut state.package_inputs, event)
                {
                    if mode == PackageMode::Disabled {
                        state.apply_package_event(&delta);
                        None
                    } else {
                        state
                            .workspace_folders
                            .first()
                            .and_then(|u| u.to_file_path().ok())
                    }
                } else {
                    None
                }
            } else {
                None
            };

            let model_rprofile_changed =
                state.cross_file_config.model_rprofile != prev.prev_cross_file.model_rprofile;
            let pkg_mode_io_needed = pkg_mode_io_needed.or_else(|| {
                if model_rprofile_changed {
                    state
                        .workspace_folders
                        .first()
                        .and_then(|u| u.to_file_path().ok())
                } else {
                    None
                }
            });

            let new_trigger_on_open_paren = state.completion_config.trigger_on_open_paren;
            let trigger_on_open_paren_changed =
                prev.prev_completion.trigger_on_open_paren != new_trigger_on_open_paren;

            // Mark all open documents for force republish (unless only the
            // watcher-lifecycle settings changed). Bulk-mark to avoid
            // per-URI lock churn on workspaces with many open files.
            let open_uris: Vec<Url> = state.documents.keys().cloned().collect();
            if !only_watch_changed {
                state
                    .diagnostics_gate
                    .mark_force_republish_many(open_uris.iter());
            }

            ReconciliationDecisions {
                scope_changed,
                package_settings_changed,
                watch_settings_changed,
                only_watch_changed,
                diagnostics_enabled_changed,
                old_diagnostics_enabled,
                new_diagnostics_enabled,
                packages_enabled,
                max_transitive_dependents_visited_changed,
                trigger_on_open_paren_changed,
                new_trigger_on_open_paren,
                pkg_mode_io_needed,
                open_uris,
            }
        };

        let ReconciliationDecisions {
            scope_changed,
            package_settings_changed,
            watch_settings_changed,
            only_watch_changed,
            diagnostics_enabled_changed,
            old_diagnostics_enabled,
            new_diagnostics_enabled,
            packages_enabled,
            max_transitive_dependents_visited_changed,
            trigger_on_open_paren_changed,
            new_trigger_on_open_paren,
            pkg_mode_io_needed,
            open_uris,
        } = decisions;

        if max_transitive_dependents_visited_changed {
            self.traversal_truncation.reset_notice_throttle();
        }

        // --- Package mode rebuild: repopulate inputs after mode switch ---
        // For non-Disabled mode switches, re-read DESCRIPTION and NAMESPACE
        // from disk (R files are already in `package_inputs` from prior
        // did_open/did_change/scan events). Then derive package state.
        if let Some(root) = pkg_mode_io_needed {
            let root_clone = root.clone();
            let (desc_text, ns_text, disk_r_files, rprofile_scan) =
                tokio::task::spawn_blocking(move || {
                    let desc = std::fs::read_to_string(root_clone.join("DESCRIPTION"))
                        .ok()
                        .map(|s| std::sync::Arc::from(s.as_str()));
                    let ns = std::fs::read_to_string(root_clone.join("NAMESPACE"))
                        .ok()
                        .map(|s| std::sync::Arc::from(s.as_str()));
                    let disk_r_files = collect_package_r_file_inputs_from_disk(&root_clone);
                    // Precompute the `.Rprofile` prelude scan OFF the write lock
                    // (it follows transitive source(); see
                    // `initialize_package_inputs_from_state`).
                    let rprofile_scan =
                        crate::package_state::rprofile::scan_workspace_rprofile(&root_clone);
                    (desc, ns, disk_r_files, rprofile_scan)
                })
                .await
                .unwrap_or((None, None, Default::default(), Default::default()));

            // Re-acquire write lock to apply results. Route through the same
            // seeding helper the startup paths use, so config-reload and startup
            // share one implementation: it sets workspace_root, package_mode,
            // description, and namespace, hydrates R files from disk + index +
            // open buffers, then applies the `Initial` delta. Seeding from disk
            // matters here so a packageMode switch still sees closed R files when
            // the background workspace index has not populated yet. (Re-setting
            // package_mode is a no-op: `translate(SettingChanged)` above already
            // set it to this same mode.)
            let mut state = self.state.write().await;
            initialize_package_inputs_from_state(
                &mut state,
                root,
                desc_text,
                ns_text,
                disk_r_files,
                Some(rprofile_scan),
            );
            log::info!("Rebuilt package state after packageMode change (event-driven)");
        }

        if diagnostics_enabled_changed {
            log::info!(
                "Diagnostics master switch changed: {} -> {}",
                old_diagnostics_enabled,
                new_diagnostics_enabled
            );
        }

        // Dynamically re-register completion capability if trigger
        // characters changed.
        if trigger_on_open_paren_changed {
            log::info!(
                "trigger_on_open_paren changed to {}, re-registering completion capability",
                new_trigger_on_open_paren
            );

            let trigger_chars = build_completion_trigger_chars(new_trigger_on_open_paren);
            let registration_options = CompletionRegistrationOptions {
                text_document_registration_options: TextDocumentRegistrationOptions {
                    document_selector: Some(vec![
                        DocumentFilter {
                            language: Some(String::from("r")),
                            scheme: None,
                            pattern: None,
                        },
                        DocumentFilter {
                            language: Some(String::from("jags")),
                            scheme: None,
                            pattern: None,
                        },
                        DocumentFilter {
                            language: Some(String::from("stan")),
                            scheme: None,
                            pattern: None,
                        },
                    ]),
                },
                completion_options: CompletionOptions {
                    trigger_characters: Some(trigger_chars),
                    resolve_provider: Some(true),
                    ..Default::default()
                },
            };

            let registration_id = String::from("completion");
            let method = String::from("textDocument/completion");

            if let Err(e) = self
                .client
                .unregister_capability(vec![Unregistration {
                    id: registration_id.clone(),
                    method: method.clone(),
                }])
                .await
            {
                log::warn!("Failed to unregister completion capability: {}", e);
            }

            if let Err(e) = self
                .client
                .register_capability(vec![Registration {
                    id: registration_id,
                    method,
                    register_options: serde_json::to_value(registration_options).ok(),
                }])
                .await
            {
                log::warn!("Failed to re-register completion capability: {}", e);
            }
        }

        // Reinitialize PackageLibrary only if R-subprocess-affecting
        // settings changed (enabled flag, R path, or additional library
        // paths).
        if package_settings_changed {
            log::info!("Package settings changed, reinitializing PackageLibrary");

            // No `packages_enabled` gate here: `rebuild_package_library`
            // self-gates and returns `(new_empty(), false)` when packages are
            // disabled, which is exactly what the old `else` branch produced.
            // Unconditionally swapping the result in is therefore behavior-
            // neutral and keeps the disabled→empty decision in one place.
            let (new_package_library, package_library_ready) =
                rebuild_package_library(&self.state).await;

            // Replace under brief write lock.
            {
                let mut state = self.state.write().await;
                state.package_library = new_package_library;
                // The fresh library starts with no local-dev overlay; re-install
                // it so load_all sentinel resolution survives the swap.
                state.refresh_local_dev_overlay();
                state.package_library_ready = package_library_ready;
                state.bump_package_config_generation();
                // The new library may carry different `lib_paths` (the user
                // changed `raven.packages.additionalLibraryPaths`, or a
                // raven.toml reload re-ran `.libPaths()`). Re-resolve
                // `source(system.file(...))` edges against the new paths BEFORE
                // the force-republish (the helper's returned `open_uris` are
                // marked and republished by the caller), so targets in a
                // newly-discovered libpath stop being stale/unresolved. See
                // `rebuild_package_library`'s invariant.
                if package_library_ready {
                    state.resolve_system_file_in_workspace();
                }
            }

            // Help/HTML help caches index by (topic, package); the package
            // set just changed, so flush them to match watcher and refresh
            // paths.
            self.state.read().await.clear_help_caches();
        }

        // Restart the libpath watcher if any setting that affects it
        // changed. Covers both the reinit path above (the `lib_paths`
        // vector may have changed) and the pure watch-settings path (e.g.
        // user flipped `watchLibraryPaths` or adjusted the debounce
        // slider) — the helper handles the teardown + respawn atomically
        // and does NOT re-run the R subprocess.
        if package_settings_changed || watch_settings_changed {
            restart_libpath_watcher(&self.state, &self.client, true).await;
        }

        // Warm the package export cache before republishing diagnostics so
        // the fresh (empty) library doesn't cause transient false-positive
        // "unknown function" diagnostics for package exports.
        if package_settings_changed && packages_enabled {
            let pkg_lib = self.state.read().await.package_library.clone();
            prefetch_packages_for_open_documents(&self.state, &pkg_lib).await;
        }

        if scope_changed {
            log::trace!(
                "Scope-affecting settings changed, revalidating {} open documents",
                open_uris.len()
            );
        }

        if only_watch_changed {
            Vec::new()
        } else {
            open_uris
        }
    }

    /// Synchronously index a file on-demand (blocking operation).
    /// Returns the cross-file metadata if indexing succeeded, None otherwise.
    ///
    /// ## Sequential File I/O Rationale
    ///
    /// This function processes files sequentially rather than concurrently for several reasons:
    ///
    /// 1. **Dependency graph serialization**: Each file's metadata updates the dependency graph,
    ///    which requires exclusive write access. Concurrent updates would require complex
    ///    synchronization and could lead to inconsistent graph state.
    ///
    /// 2. **Cache coherence**: The workspace index and file cache are updated after each file.
    ///    Sequential processing ensures later files can see earlier files' cached content
    ///    for parent resolution.
    ///
    /// 3. **I/O is fast relative to parsing**: File reads are typically fast (< 1ms for typical
    ///    R files). The parsing and analysis phase dominates execution time, and that already
    ///    uses efficient thread-local parsers.
    ///
    /// 4. **Simpler error handling**: Sequential processing allows early termination on errors
    ///    without needing to coordinate cancellation of parallel tasks.
    ///
    /// **When concurrent execution might be beneficial**:
    /// - If profiling shows I/O wait time dominates (e.g., network filesystems)
    /// - If files are independent (no cross-references between them)
    /// - Consider batching: read all files concurrently, then process sequentially
    async fn index_file_on_demand(
        &self,
        file_uri: &Url,
    ) -> Option<std::sync::Arc<crate::cross_file::CrossFileMetadata>> {
        log::trace!("On-demand indexing: {}", file_uri);

        // Read file content
        let path = match file_uri.to_file_path() {
            Ok(p) => p,
            Err(_) => {
                log::trace!("Failed to convert URI to path: {}", file_uri);
                return None;
            }
        };

        let content = match crate::state::read_source_async(&path).await {
            Ok(c) => c,
            Err(e) => {
                log::trace!("Failed to read/decode file {}: {}", file_uri, e);
                return None;
            }
        };

        let metadata = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => {
                log::trace!("Failed to get metadata for: {}", file_uri);
                return None;
            }
        };

        // Analysis text: masked for Rmd/Quarto (chunk bodies only), raw
        // otherwise. The tree, metadata, and artifacts derive from this so a
        // `.Rmd` reached via a backward directive contributes chunk-defined
        // symbols and chunk library()/source() calls — not prose. The cached
        // content and `IndexEntry.contents` below stay RAW (see those sites).
        let analysis_text_cow =
            crate::cross_file::analysis_text_for_path(file_uri.path(), &content);
        let analysis_text: &str = &analysis_text_cow;
        let tree = crate::parser_pool::with_parser(|parser| parser.parse(analysis_text, None));
        let mut cross_file_meta =
            crate::cross_file::extract_metadata_with_tree(analysis_text, tree.as_ref());
        let artifacts = std::sync::Arc::new(match tree.as_ref() {
            Some(tree) => crate::cross_file::scope::compute_artifacts_with_metadata(
                file_uri,
                tree,
                analysis_text,
                Some(&cross_file_meta),
            ),
            None => crate::cross_file::scope::ScopeArtifacts::default(),
        });

        let (
            workspace_root,
            packages_enabled,
            open_docs,
            workspace_index_version,
            parent_content,
            sys_file_ws_name,
            sys_file_ws_root,
            sys_file_lib_paths,
        ) = {
            let state = self.state.read().await;
            let workspace_root = state.workspace_folders.first().cloned();
            let max_chain_depth = state.cross_file_config.max_chain_depth;
            let packages_enabled = state.cross_file_config.packages_enabled;

            crate::cross_file::enrich_metadata_with_inherited_wd(
                &mut cross_file_meta,
                file_uri,
                workspace_root.as_ref(),
                |parent_uri| state.get_enriched_metadata(parent_uri),
                max_chain_depth,
            );

            let backward_path_ctx = crate::cross_file::path_resolve::PathContext::new(
                file_uri,
                workspace_root.as_ref(),
            );
            let parent_content: std::collections::HashMap<Url, String> = cross_file_meta
                .sourced_by
                .iter()
                .filter_map(|d| {
                    let ctx = backward_path_ctx.as_ref()?;
                    let resolved = crate::cross_file::path_resolve::resolve_path(&d.path, ctx)?;
                    let parent_uri = Url::from_file_path(resolved).ok()?;
                    let content = state
                        .documents
                        .get(&parent_uri)
                        .map(|doc| doc.text())
                        .or_else(|| state.cross_file_file_cache.get(&parent_uri))?;
                    Some((parent_uri, content))
                })
                .collect();

            let open_docs: std::collections::HashSet<_> = state.documents.keys().cloned().collect();
            let workspace_index_version = state.workspace_index_new.version();

            // Capture system.file() resolution inputs for post-enrich resolve
            let (ws_name, ws_root, lib_paths) = state.snapshot_system_file_inputs();

            (
                workspace_root,
                packages_enabled,
                open_docs,
                workspace_index_version,
                parent_content,
                ws_name,
                ws_root,
                lib_paths,
            )
        };

        // Resolve system.file() source entries into concrete paths so
        // transitive on-demand walks don't stop at this hop.
        crate::cross_file::resolve_system_file_sources(
            &mut cross_file_meta,
            sys_file_ws_name.as_deref(),
            sys_file_ws_root.as_deref(),
            &sys_file_lib_paths,
        );

        let snapshot =
            crate::cross_file::file_cache::FileSnapshot::with_content_hash(&metadata, &content);

        let loaded_packages =
            extract_loaded_packages_from_library_calls(&cross_file_meta.library_calls);
        let packages_to_prefetch = if packages_enabled {
            loaded_packages.clone()
        } else {
            Vec::new()
        };

        let cross_file_meta = std::sync::Arc::new(cross_file_meta);
        // Raw-content / masked-analysis split (#343): `contents` holds the
        // verbatim RAW source (serves snippets / get_content), while `tree`,
        // `metadata`, and `artifacts` were derived above from the masked
        // analysis text (chunk bodies only for Rmd/Quarto).
        let index_entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str(&content),
            tree,
            loaded_packages,
            snapshot: snapshot.clone(),
            metadata: cross_file_meta.clone(),
            artifacts: artifacts.clone(),
            indexed_at_version: workspace_index_version,
        };

        {
            let mut state = self.state.write().await;
            // RAW content cache (serves snippets / get_content), not masked.
            state
                .cross_file_file_cache
                .insert(file_uri.clone(), snapshot.clone(), content.clone());
            state
                .workspace_index_new
                .insert(file_uri.clone(), index_entry);
            state.cross_file_workspace_index.update_from_disk(
                file_uri,
                &open_docs,
                snapshot,
                (*cross_file_meta).clone(),
                artifacts.clone(),
            );
            state.cross_file_graph.update_file(
                file_uri,
                &cross_file_meta,
                workspace_root.as_ref(),
                |parent_uri| parent_content.get(parent_uri).cloned(),
            );
            // The graph just got new edges for `file_uri`; refresh pins so
            // callers reached via index_backward_chain / index_forward_chain
            // (which don't run their own pin recompute) see the updated
            // open-doc neighborhood.
            state.recompute_open_neighborhood_pins();
        }

        if !packages_to_prefetch.is_empty() {
            let ready = self.ensure_package_library_initialized().await;
            if !ready {
                log::trace!(
                    "On-demand indexing: package library not ready, skipping prefetch for {}",
                    file_uri
                );
            } else {
                log::trace!(
                    "On-demand indexing: prefetching packages for {}: {:?}",
                    file_uri,
                    packages_to_prefetch
                );
                let pkg_lib = self.state.read().await.package_library.clone();
                pkg_lib.prefetch_packages(&packages_to_prefetch).await;
            }
        }

        log::info!(
            "On-demand indexed: {} (exported {} symbols)",
            file_uri,
            artifacts.exported_interface.len()
        );

        Some(cross_file_meta)
    }

    async fn index_backward_chain(
        &self,
        start_uris: Vec<Url>,
        max_backward_depth: usize,
        max_forward_depth: usize,
    ) {
        use std::collections::{HashSet, VecDeque};

        if start_uris.is_empty() || max_backward_depth == 0 {
            return;
        }

        let workspace_root = self.state.read().await.workspace_folders.first().cloned();
        let mut visited: HashSet<Url> = HashSet::new();
        let mut queue: VecDeque<(Url, usize)> = start_uris.into_iter().map(|u| (u, 0)).collect();

        while let Some((uri, depth)) = queue.pop_front() {
            if depth >= max_backward_depth || visited.contains(&uri) {
                continue;
            }
            visited.insert(uri.clone());
            log::trace!("index_backward_chain: visiting {} depth={}", uri, depth);

            let needs_indexing = {
                let state = self.state.read().await;
                !state.documents.contains_key(&uri)
                    && !state.cross_file_workspace_index.contains(&uri)
            };

            let meta = if needs_indexing {
                self.index_file_on_demand(&uri).await
            } else {
                let state = self.state.read().await;
                state.get_enriched_metadata(&uri)
            };

            let Some(meta) = meta else { continue };

            if max_forward_depth > 0 {
                self.index_forward_chain(&uri, max_forward_depth, workspace_root.as_ref())
                    .await;
            }

            let ctx =
                crate::cross_file::path_resolve::PathContext::new(&uri, workspace_root.as_ref());
            let Some(ctx) = ctx.as_ref() else { continue };

            for directive in &meta.sourced_by {
                if let Some(resolved) =
                    crate::cross_file::path_resolve::resolve_path(&directive.path, ctx)
                {
                    log::trace!(
                        "index_backward_chain: {} -> parent {}",
                        directive.path,
                        resolved.display()
                    );
                    if let Ok(parent_uri) = Url::from_file_path(resolved) {
                        queue.push_back((parent_uri, depth.saturating_add(1)));
                    }
                }
            }
        }
    }

    async fn index_forward_chain(
        &self,
        start_uri: &Url,
        max_depth: usize,
        workspace_root: Option<&Url>,
    ) {
        use std::collections::{HashSet, VecDeque};

        if max_depth == 0 {
            return;
        }

        let mut visited: HashSet<Url> = HashSet::new();
        let mut queue: VecDeque<(Url, usize)> = VecDeque::new();
        queue.push_back((start_uri.clone(), 0));

        while let Some((uri, depth)) = queue.pop_front() {
            if depth >= max_depth || visited.contains(&uri) {
                continue;
            }
            visited.insert(uri.clone());
            log::trace!("index_forward_chain: visiting {} depth={}", uri, depth);

            let needs_indexing = {
                let state = self.state.read().await;
                !state.documents.contains_key(&uri)
                    && !state.cross_file_workspace_index.contains(&uri)
            };

            let meta = if needs_indexing {
                self.index_file_on_demand(&uri).await
            } else {
                let state = self.state.read().await;
                state.get_enriched_metadata(&uri)
            };

            let Some(meta) = meta else { continue };

            let Some(forward_ctx) = crate::cross_file::path_resolve::PathContext::from_metadata(
                &uri,
                &meta,
                workspace_root,
            ) else {
                continue;
            };

            for source in &meta.sources {
                if let Some(resolved) =
                    crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(
                        &source.path,
                        &forward_ctx,
                    )
                {
                    log::trace!(
                        "index_forward_chain: {} -> child {}",
                        source.path,
                        resolved.display()
                    );
                    // `resolved` is the child's filesystem path; borrow it for both
                    // the child URI and the inherited-WD decision (no round-trip).
                    if let Ok(child_uri) = Url::from_file_path(&resolved) {
                        // Propagate a working directory to the child only when one
                        // is in effect (`chdir`, or a `# raven: cd` on this file —
                        // explicit or inherited). With no cd, leave it unset so the
                        // child resolves its own source() paths relative to its own
                        // directory WITH the workspace-root fallback — matching the
                        // workspace scan (`build_dependency_graph_from_workspace`).
                        // The decision lives in `forward_child_inherited_wd`, the
                        // same seam scope resolution uses, so the on-demand edge
                        // and the scope path cannot drift.
                        let inherited_wd =
                            forward_ctx.forward_child_inherited_wd(&resolved, source.chdir);
                        let should_index = {
                            let state = self.state.read().await;
                            if state.documents.contains_key(&child_uri) {
                                false
                            } else if !state.cross_file_workspace_index.contains(&child_uri) {
                                // Not indexed yet → index it. (Same "indexed?"
                                // predicate the pop-time `needs_indexing` check uses.)
                                true
                            } else {
                                // Already indexed. A child that declares its own
                                // `# raven: cd` keeps its own working directory
                                // regardless of the caller, so no inherited WD is ever
                                // stored for it (see `index_file_on_demand_with_inherited_wd`)
                                // — never re-index it for WD reasons. Otherwise re-index
                                // only when the WD now in effect differs from the stored
                                // one: converges a plain (no-cd) child to the scan's
                                // no-WD baseline, and lets a later no-cd path clear a
                                // stale WD left by an earlier cd path to a shared child
                                // (and vice versa), instead of keying on mere WD presence.
                                match state.get_enriched_metadata(&child_uri) {
                                    Some(child_meta) if child_meta.working_directory.is_some() => {
                                        false
                                    }
                                    Some(child_meta) => {
                                        child_meta
                                            .inherited_working_directory
                                            .as_deref()
                                            .map(std::path::Path::new)
                                            != inherited_wd.as_deref()
                                    }
                                    // Indexed but metadata unavailable (shouldn't
                                    // happen) → re-index defensively.
                                    None => true,
                                }
                            }
                        };
                        if should_index {
                            match inherited_wd {
                                Some(ref wd) => {
                                    let _ = self
                                        .index_file_on_demand_with_inherited_wd(&child_uri, wd)
                                        .await;
                                }
                                None => {
                                    let _ = self.index_file_on_demand(&child_uri).await;
                                }
                            }
                        }
                        queue.push_back((child_uri, depth.saturating_add(1)));
                    }
                }
            }
        }
    }

    async fn index_file_on_demand_with_inherited_wd(
        &self,
        file_uri: &Url,
        inherited_wd: &std::path::Path,
    ) -> Option<std::sync::Arc<crate::cross_file::CrossFileMetadata>> {
        log::trace!(
            "On-demand indexing (inherited wd={}): {}",
            inherited_wd.display(),
            file_uri
        );

        let path = match file_uri.to_file_path() {
            Ok(p) => p,
            Err(_) => {
                log::trace!("Failed to convert URI to path: {}", file_uri);
                return None;
            }
        };

        let content = match crate::state::read_source_async(&path).await {
            Ok(c) => c,
            Err(e) => {
                log::trace!("Failed to read/decode file {}: {}", file_uri, e);
                return None;
            }
        };

        let metadata = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => {
                log::trace!("Failed to get metadata for: {}", file_uri);
                return None;
            }
        };

        // Analysis text: masked for Rmd/Quarto (chunk bodies only), raw
        // otherwise (#343). Cached content / `IndexEntry.contents` stay RAW.
        let analysis_text_cow =
            crate::cross_file::analysis_text_for_path(file_uri.path(), &content);
        let analysis_text: &str = &analysis_text_cow;
        let tree = crate::parser_pool::with_parser(|parser| parser.parse(analysis_text, None));
        let mut cross_file_meta =
            crate::cross_file::extract_metadata_with_tree(analysis_text, tree.as_ref());
        if cross_file_meta.working_directory.is_none()
            && cross_file_meta.inherited_working_directory.is_none()
        {
            cross_file_meta.inherited_working_directory =
                Some(inherited_wd.to_string_lossy().to_string());
        }
        let artifacts = std::sync::Arc::new(match tree.as_ref() {
            Some(tree) => crate::cross_file::scope::compute_artifacts_with_metadata(
                file_uri,
                tree,
                analysis_text,
                Some(&cross_file_meta),
            ),
            None => crate::cross_file::scope::ScopeArtifacts::default(),
        });

        let snapshot =
            crate::cross_file::file_cache::FileSnapshot::with_content_hash(&metadata, &content);

        let (
            workspace_root,
            packages_enabled,
            open_docs,
            workspace_index_version,
            parent_content,
            sys_file_ws_name,
            sys_file_ws_root,
            sys_file_lib_paths,
        ) = {
            let state = self.state.read().await;
            let workspace_root = state.workspace_folders.first().cloned();
            let packages_enabled = state.cross_file_config.packages_enabled;

            let backward_path_ctx = crate::cross_file::path_resolve::PathContext::new(
                file_uri,
                workspace_root.as_ref(),
            );
            let parent_content: std::collections::HashMap<Url, String> = cross_file_meta
                .sourced_by
                .iter()
                .filter_map(|d| {
                    let ctx = backward_path_ctx.as_ref()?;
                    let resolved = crate::cross_file::path_resolve::resolve_path(&d.path, ctx)?;
                    let parent_uri = Url::from_file_path(resolved).ok()?;
                    let content = state
                        .documents
                        .get(&parent_uri)
                        .map(|doc| doc.text())
                        .or_else(|| state.cross_file_file_cache.get(&parent_uri))?;
                    Some((parent_uri, content))
                })
                .collect();

            let open_docs: std::collections::HashSet<_> = state.documents.keys().cloned().collect();
            let workspace_index_version = state.workspace_index_new.version();

            // Capture system.file() resolution inputs for post-enrich resolve
            let (ws_name, ws_root, lib_paths) = state.snapshot_system_file_inputs();

            (
                workspace_root,
                packages_enabled,
                open_docs,
                workspace_index_version,
                parent_content,
                ws_name,
                ws_root,
                lib_paths,
            )
        };

        // Resolve system.file() source entries into concrete paths so
        // transitive on-demand walks don't stop at this hop.
        crate::cross_file::resolve_system_file_sources(
            &mut cross_file_meta,
            sys_file_ws_name.as_deref(),
            sys_file_ws_root.as_deref(),
            &sys_file_lib_paths,
        );

        let loaded_packages =
            extract_loaded_packages_from_library_calls(&cross_file_meta.library_calls);
        let packages_to_prefetch = if packages_enabled {
            loaded_packages.clone()
        } else {
            Vec::new()
        };

        let cross_file_meta = std::sync::Arc::new(cross_file_meta);
        // Raw-content / masked-analysis split (#343): `contents` holds the
        // verbatim RAW source (serves snippets / get_content), while `tree`,
        // `metadata`, and `artifacts` were derived above from the masked
        // analysis text (chunk bodies only for Rmd/Quarto).
        let index_entry = crate::workspace_index::IndexEntry {
            contents: ropey::Rope::from_str(&content),
            tree,
            loaded_packages,
            snapshot: snapshot.clone(),
            metadata: cross_file_meta.clone(),
            artifacts: artifacts.clone(),
            indexed_at_version: workspace_index_version,
        };

        {
            let mut state = self.state.write().await;
            // RAW content cache (serves snippets / get_content), not masked.
            state
                .cross_file_file_cache
                .insert(file_uri.clone(), snapshot.clone(), content.clone());
            state
                .workspace_index_new
                .insert(file_uri.clone(), index_entry);
            state.cross_file_workspace_index.update_from_disk(
                file_uri,
                &open_docs,
                snapshot,
                (*cross_file_meta).clone(),
                artifacts.clone(),
            );
            state.cross_file_graph.update_file(
                file_uri,
                &cross_file_meta,
                workspace_root.as_ref(),
                |parent_uri| parent_content.get(parent_uri).cloned(),
            );
            // Same rationale as `index_file_on_demand`: refresh pins after
            // graph mutation so callers without their own recompute see the
            // post-update open-doc neighborhood.
            state.recompute_open_neighborhood_pins();
        }

        if !packages_to_prefetch.is_empty() {
            let ready = self.ensure_package_library_initialized().await;
            if !ready {
                log::trace!(
                    "On-demand indexing (inherited wd): package library not ready, skipping prefetch for {}",
                    file_uri
                );
            } else {
                log::trace!(
                    "On-demand indexing (inherited wd): prefetching packages for {}: {:?}",
                    file_uri,
                    packages_to_prefetch
                );
                let pkg_lib = self.state.read().await.package_library.clone();
                pkg_lib.prefetch_packages(&packages_to_prefetch).await;
            }
        }

        Some(cross_file_meta)
    }

    /// Free-standing variant of `publish_diagnostics` usable from background tasks.
    /// Runs the same debounced pipeline to completion.
    async fn publish_diagnostics_via_arc(
        state_arc: Arc<RwLock<WorldState>>,
        client: Client,
        uri: &Url,
        traversal_truncation: Option<Arc<TraversalTruncationState>>,
    ) {
        let (debounce_ms, trigger_version, trigger_revision) = {
            let state = state_arc.read().await;
            let doc = state.documents.get(uri);
            let v = doc.and_then(|d| d.version);
            let r = doc.map(|d| d.revision);
            (state.cross_file_config.revalidation_debounce_ms, v, r)
        };
        run_debounced_diagnostics(
            state_arc,
            client,
            uri.clone(),
            debounce_ms,
            trigger_version,
            trigger_revision,
            traversal_truncation,
        )
        .await;
    }

    /// Publish diagnostics for an already-computed affected-URI set with bounded
    /// parallelism. Each worker runs the normal debounced pipeline, which
    /// rebuilds its own snapshot and commits through the monotonic gate.
    async fn publish_diagnostics_for_uris_bounded(
        state_arc: Arc<RwLock<WorldState>>,
        client: Client,
        uris: Vec<Url>,
        traversal_truncation: Option<Arc<TraversalTruncationState>>,
    ) {
        run_bounded_fanout(uris, DIAGNOSTIC_FANOUT_CONCURRENCY, move |uri| {
            let state_arc = Arc::clone(&state_arc);
            let client = client.clone();
            let traversal_truncation = traversal_truncation.clone();
            async move {
                Backend::publish_diagnostics_via_arc(state_arc, client, &uri, traversal_truncation)
                    .await;
            }
        })
        .await;
    }

    async fn publish_diagnostics(&self, uri: &Url) {
        publish_diagnostics_inner(&self.state, &self.client, uri).await;
        self.check_and_warn_traversal_truncation().await;
    }

    /// Live `.Rprofile` prelude refresh from the in-memory buffer
    /// (`did_change`). `.Rprofile` is a live R document, but the prelude it
    /// feeds into open `scripts/` and other non-namespace workspace R files is
    /// package-state, not a cross-file graph edge — so an unsaved `.Rprofile`
    /// edit is not picked up by the normal dependent fanout. Scans the prelude
    /// from the buffer `text` OFF the write lock (the scan follows transitive
    /// `source()` from disk), then applies + fans out. Mirrors the watched-file
    /// (`did_change_watched_files`) `.Rprofile` fanout, but sourced from the
    /// buffer instead of disk so the prelude tracks edits as you type.
    async fn refresh_rprofile_prelude_from_buffer(&self, root: std::path::PathBuf, text: Arc<str>) {
        let root_for_scan = root.clone();
        let Ok(scan) = tokio::task::spawn_blocking(move || {
            crate::package_state::rprofile::scan_workspace_rprofile_with_root_text(
                &root_for_scan,
                &text,
            )
        })
        .await
        else {
            return;
        };
        self.apply_rprofile_prelude_rescan(scan).await;
    }

    /// Revert the `.Rprofile` prelude to the on-disk content (`did_close`). When
    /// an open `.Rprofile` is closed WITHOUT saving, the live buffer edits that
    /// `refresh_rprofile_prelude_from_buffer` folded into the prelude must be
    /// discarded — the disk file is authoritative again. Re-scans from disk
    /// OFF-lock and applies + fans out, mirroring how `did_close` reverts open
    /// package R files from buffer to disk.
    async fn refresh_rprofile_prelude_from_disk(&self, root: std::path::PathBuf) {
        let root_for_scan = root.clone();
        let Ok(scan) = tokio::task::spawn_blocking(move || {
            crate::package_state::rprofile::scan_workspace_rprofile(&root_for_scan)
        })
        .await
        else {
            return;
        };
        self.apply_rprofile_prelude_rescan(scan).await;
    }

    /// Shared tail of the live `.Rprofile` refresh: apply the already-scanned
    /// (off-lock) prelude under the write lock via the lock-safe seam
    /// ([`apply_rprofile_scan`]), then republish every open workspace R file
    /// that consumes the prelude.
    ///
    /// [`apply_rprofile_scan`]: crate::package_state::event::apply_rprofile_scan
    async fn apply_rprofile_prelude_rescan(
        &self,
        scan: crate::package_state::rprofile::RprofileScan,
    ) {
        let affected: Vec<Url> = {
            let mut state = self.state.write().await;
            // Pure in-memory apply — safe under the lock. `None` means modeling
            // is disabled and nothing was cleared, so there's no re-derive or
            // fanout to do.
            let Some(delta) = crate::package_state::event::apply_rprofile_scan(
                &mut state.package_inputs,
                Some(scan),
            ) else {
                return;
            };
            state.apply_package_event(&delta);
            // Fan out to every open workspace R file that consumes the prelude
            // (scripts/, data-raw/, non-package R/, …). `.Rprofile` itself has
            // no `.R` extension, so `is_package_workspace_r_file` excludes it,
            // and the edited buffer's own diagnostics are republished by the
            // normal did_change/did_close flow.
            let Some(ws_root) = state.package_inputs.workspace_root.clone() else {
                return;
            };
            let affected: Vec<Url> = state
                .documents
                .keys()
                .filter(|u| {
                    u.to_file_path().ok().is_some_and(|p| {
                        crate::package_state::is_package_workspace_r_file(&p, &ws_root)
                    })
                })
                .cloned()
                .collect();
            // Same-version republish requires force-marking (diagnostics publish
            // monotonically by version; this is the documented force-republish
            // escape hatch).
            state
                .diagnostics_gate
                .mark_force_republish_many(affected.iter());
            affected
        };

        if affected.is_empty() {
            return;
        }
        // Publish via the bounded, concurrency-capped fanout — and SPAWN it
        // rather than await, so this live-edit refresh does not block the
        // sequentially-processed LSP message queue (a serial per-file publish
        // here would stall subsequent edits on large workspaces).
        let state_arc = self.state.clone();
        let client = self.client.clone();
        let traversal_truncation = self.traversal_truncation.clone();
        tokio::spawn(Backend::publish_diagnostics_for_uris_bounded(
            state_arc,
            client,
            affected,
            Some(traversal_truncation),
        ));
    }

    /// Handle the raven/activeDocumentsChanged notification (Requirement 15)
    async fn handle_active_documents_changed(&self, params: ActiveDocumentsChangedParams) {
        log::trace!(
            "Received activeDocumentsChanged: active={:?}, visible={}, timestamp={}",
            params.active_uri,
            params.visible_uris.len(),
            params.timestamp_ms
        );

        let active_uri = params.active_uri.and_then(|s| Url::parse(&s).ok());
        let visible_uris: Vec<Url> = params
            .visible_uris
            .iter()
            .filter_map(|s| Url::parse(s).ok())
            .collect();

        let mut state = self.state.write().await;
        state
            .cross_file_activity
            .update(active_uri, visible_uris, params.timestamp_ms);
    }

    /// Handle the raven/semanticTokensForRString custom request.
    ///
    /// Tokenizes a raw R source string using Raven's tree-sitter based
    /// function-token detector and returns the result in the same LSP delta
    /// format as `textDocument/semanticTokens/full`. The single-entry legend
    /// (`function`) is identical to the live-document path, so the client
    /// can reuse the same decoder.
    ///
    /// Used by the Knit Output webview pipeline: when rendering an R code
    /// block in the Rmd preview, the extension calls this with the block's
    /// raw text and overlays the resulting function spans on top of the
    /// vscode-textmate grammar tokens. This sidesteps the source-to-HTML
    /// position-mapping problem (pandoc may hide / reformat / reorder
    /// chunks) by tokenizing each rendered block independently.
    async fn handle_semantic_tokens_for_r_string(
        &self,
        params: SemanticTokensForRStringParams,
    ) -> tower_lsp::jsonrpc::Result<SemanticTokens> {
        let text = params.text;
        match tokio::task::spawn_blocking(move || handlers::semantic_tokens_for_r_string(&text))
            .await
        {
            Ok(tokens) => Ok(tokens),
            Err(e) => {
                log::trace!("semantic_tokens_for_r_string: spawn_blocking failed: {e}");
                Err(tower_lsp::jsonrpc::Error::internal_error())
            }
        }
    }

    /// Handle the raven/documentIndentUnitsChanged notification.
    ///
    /// Replaces the per-document indent unit map wholesale and triggers a
    /// force-republish for every document whose effective indent unit changed.
    async fn handle_document_indent_units_changed(&self, params: DocumentIndentUnitsChangedParams) {
        log::trace!(
            "Received documentIndentUnitsChanged: {} entries",
            params.units.len()
        );

        let new_map: std::collections::HashMap<String, u32> = params
            .units
            .into_iter()
            .map(|e| (e.uri, normalize_document_indent_unit(e.indent_unit)))
            .collect();

        let affected_uris: Vec<Url> = {
            let mut state = self.state.write().await;

            // Collect URIs whose effective indent unit changed.
            let open_uris: Vec<Url> = state.documents.keys().cloned().collect();
            let affected: Vec<Url> = open_uris
                .into_iter()
                .filter(|uri| {
                    let key = uri.as_str();
                    let old = state
                        .per_document_indent_unit
                        .get(key)
                        .copied()
                        .unwrap_or(state.lint_config.indentation_unit);
                    let new = new_map
                        .get(key)
                        .copied()
                        .unwrap_or(state.lint_config.indentation_unit);
                    old != new
                })
                .collect();

            state.per_document_indent_unit = new_map;

            if !affected.is_empty() {
                state
                    .diagnostics_gate
                    .mark_force_republish_many(affected.iter());
            }

            affected
        };

        let state_arc = self.state.clone();
        let client = self.client.clone();
        let traversal_truncation = self.traversal_truncation.clone();
        tokio::spawn(async move {
            Backend::publish_diagnostics_for_uris_bounded(
                state_arc,
                client,
                affected_uris,
                Some(traversal_truncation),
            )
            .await;
        });
    }
}

/// Body of [`Backend::publish_diagnostics`], factored out so that
/// parallel publish drivers (e.g. the `did_change_watched_files` reload
/// path) can spawn tasks that own clones of `state_arc` + `client`
/// rather than borrowing `&Backend`.
///
/// Concurrency: safe to invoke from multiple tasks for distinct URIs.
/// The monotonic publish predicate is atomically committed via
/// [`crate::cross_file::CrossFileDiagnosticsGate::try_consume_publish`],
/// which writes to per-URI `HashMap` entries — concurrent commits for
/// different URIs touch disjoint state, contending only on the briefly
/// held global `RwLock<HashMap<Url, _>>` write lock around the
/// `HashMap::get`/`HashMap::insert` call.
///
/// Lock discipline: the read lock on `WorldState` is only held while
/// building [`handlers::DiagnosticsSnapshot`] (which captures all
/// inputs the scope engine needs) and capturing the directive metadata
/// / workspace folder / severity. The lock is then released BEFORE the
/// heavy `diagnostics_from_snapshot` call (which runs scope resolution)
/// and the async missing-file checks. This matches the snapshot
/// pattern's design intent — "Built under the read lock, then used to
/// compute diagnostics without holding any lock" — and is required for
/// the parallel reload driver: with up to 8 concurrent
/// `publish_diagnostics_inner` tasks, holding the read lock across
/// scope resolution would block `did_change` writers for the full
/// duration of every parallel diagnostic computation.
pub(crate) async fn publish_diagnostics_inner(
    state_arc: &Arc<RwLock<WorldState>>,
    client: &Client,
    uri: &Url,
) {
    // Phase 1: brief read lock — gate check, build the diagnostics
    // snapshot, capture inputs for the off-lock work. NO scope
    // resolution or other heavy work happens inside this scope.
    let (
        version,
        diagnostics_enabled,
        snapshot,
        directive_meta,
        workspace_folder,
        missing_file_severity,
        case_mismatch_severity,
    ) = {
        let state = state_arc.read().await;
        let version = state.documents.get(uri).and_then(|d| d.version);

        // Check if we can publish (monotonic gate)
        if let Some(ver) = version {
            if !state.diagnostics_gate.can_publish(uri, ver) {
                log::trace!(
                    "Skipping diagnostics for {}: monotonic gate (version={})",
                    uri,
                    ver
                );
                return;
            } else {
                log::trace!(
                    "Publishing diagnostics for {}: monotonic gate allows (version={})",
                    uri,
                    ver
                );
            }
        }

        // Capture the diagnostics master switch under the read lock so
        // it gates BOTH the sync snapshot build AND the async
        // missing-file checks below — without this, a config reload
        // that flips `diagnostics.enabled` to `false` could still
        // publish missing-file diagnostics from the async phase.
        let diagnostics_enabled = state.cross_file_config.diagnostics_enabled;

        // Skip the snapshot build entirely when the master switch is
        // off — saves the snapshot's metadata clone + neighborhood walk.
        // Mirrors the early-exit in `handlers::diagnostics`.
        let snapshot = if diagnostics_enabled {
            handlers::DiagnosticsSnapshot::build(&state, uri)
        } else {
            None
        };

        // Metadata for async missing-file checks. Reuse the snapshot's
        // `directive_meta`, which is `extract_metadata` (not just directives):
        // it carries AST-detected `source()` calls AND is masked-correct for
        // Rmd/Quarto (chunk bodies only). The old `parse_directives(doc.text())`
        // here was raw and directives-only, so an AST `source("missing.R")`
        // inside a chunk was stripped by `diagnostics_async_standalone`'s
        // retain() and never re-added — losing the missing-file diagnostic
        // (#343). Mirrors the dependent-revalidation path, which already passes
        // `snapshot.directive_meta`. When the snapshot is absent (diagnostics
        // disabled) the async phase below is skipped, so the fallback is inert.
        let directive_meta = snapshot
            .as_ref()
            .map(|s| s.directive_meta.clone())
            .unwrap_or_default();

        let workspace_folder = state.workspace_folders.first().cloned();
        let missing_file_severity = state.cross_file_config.missing_file_severity;
        let case_mismatch_severity = state.cross_file_config.case_mismatch_severity;

        (
            version,
            diagnostics_enabled,
            snapshot,
            directive_meta,
            workspace_folder,
            missing_file_severity,
            case_mismatch_severity,
        )
    };
    // Read lock released — scope resolution and async I/O run unlocked.

    // Phase 2: run `diagnostics_from_snapshot` outside the lock. This is
    // the heavy phase the snapshot pattern was designed to keep
    // lock-free (see DiagnosticsSnapshot doc comment in handlers.rs).
    // Replicate the remaining `handlers::diagnostics` early-exits via
    // snapshot fields, which already mirror `doc.file_type` from build time.
    let sync_diagnostics = match snapshot {
        // Rmd/Quarto documents flow through too (issue #343): the snapshot
        // carries the masked analysis text + tree, so `diagnostics_from_snapshot`
        // sees only real R chunk-body content. JAGS/Stan stay suppressed via the
        // `file_type == R` condition.
        Some(snap) if snap.file_type == crate::file_type::FileType::R => {
            handlers::diagnostics_from_snapshot(&snap, uri, &handlers::DiagCancelToken::never())
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };

    // Phase 3: async missing-file existence checks. Gated on
    // `diagnostics_enabled` so a master-switch-off reload doesn't keep
    // publishing missing-file diagnostics while the sync phase was
    // empty. When disabled, publish an explicit empty `Vec` so the
    // client clears any prior diagnostics for the URI.
    let diagnostics = if diagnostics_enabled {
        handlers::diagnostics_async_standalone(
            uri,
            sync_diagnostics,
            &directive_meta,
            workspace_folder.as_ref(),
            missing_file_severity,
            case_mismatch_severity,
        )
        .await
    } else {
        Vec::new()
    };

    // Re-check freshness after async work, atomically commit gate state, before publishing.
    // try_consume_publish replaces the racy can_publish + record_publish pair.
    {
        let state = state_arc.read().await;
        if let Some(ver) = version {
            let current_version = state.documents.get(uri).and_then(|d| d.version);
            if current_version != Some(ver) {
                log::trace!(
                    "Skipping diagnostics for {}: version changed (was {:?}, now {:?})",
                    uri,
                    version,
                    current_version
                );
                return;
            }
            if !state.diagnostics_gate.try_consume_publish(uri, ver) {
                log::trace!(
                    "Skipping diagnostics for {}: monotonic gate after async (version={})",
                    uri,
                    ver
                );
                return;
            }
        }
    }

    client
        .publish_diagnostics(uri.clone(), diagnostics, None)
        .await;
}

pub async fn start_lsp() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let request_cancellation = Arc::new(RequestCancellationRegistry::new());
    let backend_request_cancellation = Arc::clone(&request_cancellation);
    let (service, socket) = LspService::build(move |client| {
        Backend::new_with_request_cancellation(client, Arc::clone(&backend_request_cancellation))
    })
    .custom_method(
        "raven/activeDocumentsChanged",
        Backend::handle_active_documents_changed,
    )
    .custom_method(
        "raven/documentIndentUnitsChanged",
        Backend::handle_document_indent_units_changed,
    )
    .custom_method(
        "raven/semanticTokensForRString",
        Backend::handle_semantic_tokens_for_r_string,
    )
    .finish();
    let service = RequestCancellationService::new(service, request_cancellation);
    // Force sequential message processing to prevent out-of-order did_change
    // notifications from corrupting incremental text sync.
    // tower-lsp 0.20 defaults to buffer_unordered(4) which can reorder messages.
    Server::new(stdin, stdout, socket)
        .concurrency_level(1)
        .serve(service)
        .await;

    Ok(())
}

/// Per-open-document package warm set plus scope-probe seeds.
///
/// For every open document this unions its `library()`/`require()` loads with
/// its `data(..., package = "pkg")` arguments (issue #429) — the latter so the
/// diagnostics-time `data()` alias expansion can resolve those packages'
/// dataset object names in the editor, matching what `raven check` already
/// warms via `doc.data_packages`. Also returns each document's `(uri,
/// last_line)` for scope-probe seeding. Extracted from
/// [`prefetch_packages_for_open_documents`] so the warm-set composition is
/// unit-testable without the async/scope-resolution pipeline. Names are
/// validated by the caller's `is_valid_package_name` filter before reaching the
/// R subprocess / filesystem.
fn collect_open_document_packages(
    state: &WorldState,
) -> (std::collections::HashSet<String>, Vec<(Url, u32)>) {
    let mut doc_pkgs = std::collections::HashSet::new();
    let mut docs = Vec::new();
    for (uri, doc) in &state.documents {
        for p in &doc.loaded_packages {
            doc_pkgs.insert(p.clone());
        }
        for p in &doc.data_packages {
            doc_pkgs.insert(p.clone());
        }
        // Namespace references warm metadata but never attach scope; pull them
        // from the document's enriched metadata (the shared source of truth),
        // through the same warm filter every other path uses.
        if let Some(meta) = state.get_enriched_metadata(uri) {
            for p in valid_namespace_ref_packages(&meta) {
                doc_pkgs.insert(p.to_string());
            }
        }
        let line = doc.text().lines().count().saturating_sub(1) as u32;
        docs.push((uri.clone(), line));
    }
    (doc_pkgs, docs)
}

/// Open documents whose namespace references (`pkg::member`) name any package in
/// `warmed`. After a background prefetch warms a package, these documents may
/// gain/lose `namespace-member-not-found` / `package-not-installed` diagnostics
/// even though they don't `source()` the edited file, so they must be marked
/// for force-republish (issue #503). Reads each doc's enriched metadata — the
/// shared source of truth for namespace references.
fn open_docs_referencing_packages(
    state: &WorldState,
    warmed: &std::collections::HashSet<String>,
) -> Vec<Url> {
    let mut out = Vec::new();
    for uri in state.documents.keys() {
        if let Some(meta) = state.get_enriched_metadata(uri)
            && meta
                .namespace_references
                .iter()
                .any(|r| warmed.contains(&r.package))
        {
            out.push(uri.clone());
        }
    }
    out
}

/// Collect effective packages for all open documents (direct `library()` calls
/// plus inherited packages from parent `source()` chains, plus — in package
/// mode — NAMESPACE whole-package `import(pkg)` directives from
/// `scope_contribution.full_imports`, whose exports the undefined-variable
/// check resolves via `is_package_export` in both call and value position)
/// and prefetch their exports into `pkg_lib`. Snapshots state under the read
/// lock, releases it before running scope resolution (per AGENTS.md lock-hold
/// invariant).
pub(crate) async fn prefetch_packages_for_open_documents(
    state_arc: &Arc<RwLock<WorldState>>,
    pkg_lib: &Arc<crate::package_library::PackageLibrary>,
) {
    let (doc_packages, probe) = {
        let state = state_arc.read().await;
        let (doc_pkgs, docs) = collect_open_document_packages(&state);
        let snapshot = state.build_package_scope_snapshot(&docs);
        (doc_pkgs, snapshot)
    }; // read lock released

    let mut all_pkgs = doc_packages;
    // Package mode: NAMESPACE `import(pkg)` puts every export of `pkg` in
    // scope for the package's own R files, so warm those exports too (parity
    // with `raven check`'s `prefetch_reported_packages`). `full_imports` is
    // empty outside package mode, making this a no-op there.
    all_pkgs.extend(probe.scope_contribution.full_imports.iter().cloned());
    let empty_base_exports = std::collections::HashSet::new();
    let get_artifacts = |target_uri: &Url| probe.artifacts_map.get(target_uri).cloned();
    let get_metadata = |target_uri: &Url| probe.metadata_map.get(target_uri).cloned();
    for (uri, line) in &probe.docs {
        let scope = crate::cross_file::scope::scope_at_position_with_graph(
            uri,
            *line,
            u32::MAX,
            &get_artifacts,
            &get_metadata,
            &probe.graph,
            probe.workspace_folder.as_ref(),
            probe.max_chain_depth,
            &empty_base_exports,
            false,
            probe.backward_dependencies,
            &|| false,
            Some(&probe.scope_contribution),
            // Probe reads only packages, not `scope.symbols`; no `data()`
            // alias expansion needed (issue #429).
            None,
        );
        for p in scope.inherited_packages {
            all_pkgs.insert(p);
        }
        for p in scope.loaded_packages {
            all_pkgs.insert(p);
        }
    }

    // `is_valid_package_name` already rejects the underscore-prefixed load_all()
    // sentinel; the explicit guard is defense-in-depth.
    let packages: Vec<String> = all_pkgs
        .into_iter()
        .filter(|p| is_valid_package_name(p) && !crate::package_library::is_load_all_sentinel(p))
        .collect();
    if !packages.is_empty() {
        log::trace!(
            "Prefetching {} packages for {} open documents",
            packages.len(),
            probe.docs.len()
        );
        pkg_lib.prefetch_packages(&packages).await;
    }
}

/// Core logic of the old `raven.refreshPackages` inline path. Retained for
/// tests that verify the cleared-count accounting.
#[cfg(test)]
pub(crate) async fn refresh_packages_command_body(
    pkg_lib: &std::sync::Arc<crate::package_library::PackageLibrary>,
    loaded_packages_to_prefetch: &[String],
) -> usize {
    let before = pkg_lib.cached_count().await;
    pkg_lib.clear_cache().await;
    if !loaded_packages_to_prefetch.is_empty() {
        pkg_lib.prefetch_packages(loaded_packages_to_prefetch).await;
    }
    let after = pkg_lib.cached_count().await;
    before.saturating_sub(after)
}

/// Build a fresh `PackageLibrary` from current configuration, re-querying R for
/// `.libPaths()`. Returns the new library and whether it is considered ready
/// (non-empty lib_paths). The caller decides when to swap it into `state`.
///
/// Used by `raven.refreshPackages` and by the settings-change / init paths so
/// mid-session changes to `.libPaths()` (e.g. renv switching projects, the user
/// editing `.Rprofile`, or a `.libPaths(new, ...)` call) are picked up without
/// requiring an LSP restart.
///
/// INVARIANT: any path that swaps the result into `state.package_library` (and
/// thereby changes `lib_paths`) MUST call
/// `WorldState::resolve_system_file_in_workspace()` under the write lock BEFORE
/// force-republishing open documents. Otherwise a file whose
/// `source(system.file(...))` resolves against a package in a newly-discovered
/// libpath keeps its stale/unresolved `system_file` index entry until restart.
/// Call sites: `raven.refreshPackages`, `reconcile_after_config_recompute`, and
/// the post-Task-B startup retry in `initialized`.
pub(crate) async fn rebuild_package_library(
    state_arc: &Arc<RwLock<WorldState>>,
) -> (Arc<crate::package_library::PackageLibrary>, bool) {
    let (packages_enabled, packages_r_path, additional_paths, workspace_root) = {
        let state = state_arc.read().await;
        (
            state.cross_file_config.packages_enabled,
            state.cross_file_config.packages_r_path.clone(),
            state
                .cross_file_config
                .packages_additional_library_paths
                .clone(),
            state
                .workspace_folders
                .first()
                .and_then(|url| url.to_file_path().ok()),
        )
    };

    let outcome = crate::package_library::build_package_library(
        packages_r_path,
        &additional_paths,
        workspace_root,
        packages_enabled,
    )
    .await;
    if let crate::package_library::PackageLibraryStatus::InitFailed(e) = &outcome.status {
        log::warn!("rebuild_package_library: initialize failed: {e}");
    }
    let ready = outcome.consumer_ready();
    (outcome.library, ready)
}

/// Tear down any running libpath watcher and, if watching is enabled and the
/// package library is ready with at least one library path, spawn a fresh one over the current
/// `state.package_library.lib_paths()`. Returns whether a new watcher was
/// attached.
///
/// `allow_recovery` is propagated to the spawned consumer — primary spawns pass
/// `true` so a later `Dropped` can attempt one recovery; the recovery spawn
/// itself passes `false` to prevent a tight restart loop if the underlying
/// failure is persistent (all libpaths unwatchable, inotify quota exhausted,
/// etc.).
///
/// This returns a boxed future because `run_libpath_consumer`'s Dropped branch
/// can call back into `restart_libpath_watcher`; boxing one side of that cycle
/// lets rustc size and `Send`-check both futures.
pub(crate) fn restart_libpath_watcher<'a>(
    state_arc: &'a Arc<RwLock<WorldState>>,
    client: &'a Client,
    allow_recovery: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
    Box::pin(async move {
        {
            let mut state = state_arc.write().await;
            state.libpath_watcher_handle = None;
        }

        let (should_start, lib_paths, debounce) = {
            let state = state_arc.read().await;
            let lib_paths = state.package_library.lib_paths().to_vec();
            let should = state.cross_file_config.packages_enabled
                && state.cross_file_config.packages_watch_library_paths
                && state.package_library_ready
                && !lib_paths.is_empty();
            (
                should,
                lib_paths,
                std::time::Duration::from_millis(
                    state.cross_file_config.packages_watch_debounce_ms,
                ),
            )
        };
        if !should_start {
            return false;
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<crate::libpath_watcher::LibpathEvent>(64);

        // Spawn the consumer BEFORE the watcher so that if spawn_watcher
        // sends LibpathEvent::Dropped (all watches failed) and returns None,
        // the consumer is already listening and will run the recovery/cleanup
        // path. When spawn_watcher returns None the tx is dropped, so the
        // consumer will process the Dropped event then exit naturally.
        let state_for_consumer = Arc::clone(state_arc);
        let client_for_consumer = client.clone();
        tokio::spawn(run_libpath_consumer(
            state_for_consumer,
            client_for_consumer,
            rx,
            allow_recovery,
        ));

        let Some(handle) = crate::libpath_watcher::spawn_watcher(lib_paths, debounce, tx) else {
            return false;
        };

        {
            let mut state = state_arc.write().await;
            state.libpath_watcher_handle = Some(Arc::new(handle));
        }
        true
    })
}

/// Pre-collected snapshot of the inputs `scope_at_position_with_graph` needs
/// for filtering open documents by package scope. Captured under the read
/// lock so the (potentially expensive) per-document scope traversal can run
/// outside the lock — see the libpath consumer's `Changed` branch.
///
/// Built via `WorldState::build_package_scope_snapshot` to ensure the full
/// dependency neighborhood (including closed parent files) is included.
pub(crate) struct ScopeProbeSnapshot {
    pub(crate) docs: Vec<(Url, u32)>,
    pub(crate) artifacts_map:
        std::collections::HashMap<Url, Arc<crate::cross_file::scope::ScopeArtifacts>>,
    pub(crate) metadata_map:
        std::collections::HashMap<Url, Arc<crate::cross_file::CrossFileMetadata>>,
    /// Per-document `loaded_packages` from the document store (includes
    /// function-local `library()` calls that the EOF scope probe misses).
    pub(crate) doc_loaded_packages: std::collections::HashMap<Url, Vec<String>>,
    pub(crate) graph: crate::cross_file::dependency::DependencyGraph,
    pub(crate) workspace_folder: Option<Url>,
    pub(crate) max_chain_depth: usize,
    pub(crate) backward_dependencies: crate::cross_file::config::BackwardDependencyMode,
    pub(crate) scope_contribution: crate::package_state::PackageScopeContribution,
}

/// State-side preparation for a `LibpathEvent::Dropped`: clears the package
/// cache so stale entries can't leak into subsequent lookups, and marks every
/// open document for force-republish so the scheduled diagnostic pass can
/// overwrite the last publish at the same document version. Returns the list
/// of open URIs so the caller can schedule the actual publishing.
///
/// Split out of the consumer body to keep the state invariants unit-testable
/// without needing a tower-lsp `Client`.
pub(crate) async fn prepare_dropped_recovery(state_arc: &Arc<RwLock<WorldState>>) -> Vec<Url> {
    let pkg_lib = { state_arc.read().await.package_library.clone() };
    pkg_lib.clear_cache().await;

    let open_uris: Vec<Url> = {
        let state = state_arc.read().await;
        state.documents.keys().cloned().collect()
    };
    {
        let state = state_arc.read().await;
        state
            .diagnostics_gate
            .mark_force_republish_many(open_uris.iter());
    }
    open_uris
}

/// Consumer for libpath change events. Invalidates the package cache for
/// affected packages and schedules diagnostic revalidation for open documents
/// whose loaded packages intersect the change set.
///
/// `allow_recovery` controls what happens on `LibpathEvent::Dropped`: if `true`
/// (primary watcher), the consumer clears the cache, republishes diagnostics
/// for all open docs, and spawns a replacement watcher with
/// `allow_recovery = false`; if `false` (recovery watcher), it still clears +
/// republishes but does not attempt another restart, so a persistent failure
/// cannot loop.
async fn run_libpath_consumer(
    state_arc: Arc<RwLock<WorldState>>,
    client: Client,
    mut rx: tokio::sync::mpsc::Receiver<crate::libpath_watcher::LibpathEvent>,
    allow_recovery: bool,
) {
    use crate::libpath_watcher::LibpathEvent;

    while let Some(evt) = rx.recv().await {
        match evt {
            LibpathEvent::Changed {
                added,
                removed,
                touched,
            } => {
                let affected: std::collections::HashSet<String> = added
                    .iter()
                    .chain(removed.iter())
                    .chain(touched.iter())
                    .cloned()
                    .collect();
                if affected.is_empty() {
                    continue;
                }
                log::info!(
                    "LibpathWatcher: +{} -{} ~{} packages",
                    added.len(),
                    removed.len(),
                    touched.len()
                );

                // Bust help caches before invalidating the package library so
                // any concurrent hover/completion requests see a clean slate and
                // re-fetch from R rather than returning stale HTML or text help.
                state_arc.read().await.clear_help_caches();

                // Invalidate the package cache and learn which combined
                // aggregate entries were dropped as a side effect. A document loading
                // a meta-package like `tidyverse` needs revalidation when
                // `dplyr` changes — `tidyverse` is not in `affected` but its
                // aggregate was invalidated, so it's in `invalidated_combined`.
                let pkg_lib = { state_arc.read().await.package_library.clone() };
                let invalidated_combined = pkg_lib.invalidate_many(&affected).await;

                // Union of names that mark a document as affected: either the
                // direct disk-level change (`affected`) or a dropped aggregate
                // (`invalidated_combined`). Documents whose `loaded_packages`
                // intersect this union need diagnostic revalidation.
                let trigger_set: std::collections::HashSet<String> = affected
                    .iter()
                    .chain(invalidated_combined.iter())
                    .cloned()
                    .collect();

                // Await the warmup prefetch BEFORE republishing diagnostics so
                // the next diagnostic pass sees a warmed cache; otherwise a
                // raced prefetch can land just after diagnostics compute and
                // leave the user looking at stale results.
                let non_removed_trigger_set: std::collections::HashSet<String> = touched
                    .iter()
                    .chain(added.iter())
                    .chain(invalidated_combined.iter())
                    .filter(|name| !removed.contains(*name))
                    .cloned()
                    .collect();
                if !non_removed_trigger_set.is_empty() {
                    let prefetch_vec: Vec<String> = non_removed_trigger_set.into_iter().collect();
                    pkg_lib.prefetch_packages(&prefetch_vec).await;
                }

                // A package install/removal under a watched libpath can create
                // or destroy `source(system.file(...))` resolution targets even
                // though the set of lib_paths is unchanged. Pre-check under the
                // read lock so workspaces without a matching system.file source
                // (the common case) never pay for the write lock. Otherwise
                // re-resolve just the entries referencing changed packages so
                // edges form for a newly-installed package and stale edges drop
                // for a removed one. The write-lock section does bounded disk
                // work — stat probes per selected entry plus reading/parsing
                // newly resolved external files in
                // `index_cross_package_resolved_files` — the same work every
                // library-swap site performs under this lock.
                let touches_system_file = {
                    let state = state_arc.read().await;
                    let selected = |s: &crate::cross_file::ForwardSource| {
                        s.system_file
                            .as_ref()
                            .is_some_and(|sf| affected.contains(&sf.package))
                    };
                    state
                        .workspace_index_new
                        .any_entry(|entry| entry.metadata.sources.iter().any(selected))
                        || state.document_store.uris().iter().any(|uri| {
                            state
                                .document_store
                                .get_without_touch(uri)
                                .is_some_and(|doc| doc.metadata.sources.iter().any(selected))
                        })
                };
                let system_file_republish: Vec<Url> = if touches_system_file {
                    let mut state = state_arc.write().await;
                    let changed =
                        state.resolve_system_file_in_workspace_for_packages(Some(&affected));
                    state.system_file_republish_set(&changed)
                } else {
                    Vec::new()
                };

                // Collect URIs whose effective package scope intersects
                // `trigger_set`.
                //
                // Snapshot the inputs scope resolution needs under the read
                // lock, then release it before doing the per-document
                // traversal. AGENTS.md learning: "Diagnostic computation must
                // not hold the RwLock read lock during expensive scope
                // resolution. Build a [...] snapshot (captures artifacts,
                // metadata, graph clone, config), release the lock, then
                // compute [...] from the snapshot." Holding the lock across N
                // cross-file scope resolutions can starve `did_change`
                // writers in workspaces with many open files.
                use std::collections::HashSet;
                let probe = {
                    let state = state_arc.read().await;

                    let docs: Vec<(Url, u32)> = state
                        .documents
                        .iter()
                        .map(|(uri, doc)| {
                            let line = doc.text().lines().count().saturating_sub(1) as u32;
                            (uri.clone(), line)
                        })
                        .collect();

                    state.build_package_scope_snapshot(&docs)
                };

                let get_artifacts =
                    |target_uri: &Url| -> Option<Arc<crate::cross_file::scope::ScopeArtifacts>> {
                        probe.artifacts_map.get(target_uri).cloned()
                    };
                let get_metadata = |target_uri: &Url| -> Option<
                    std::sync::Arc<crate::cross_file::CrossFileMetadata>,
                > {
                    probe.metadata_map.get(target_uri).cloned()
                };
                let empty_base_exports: HashSet<String> = HashSet::new();
                let mut affected_uris: Vec<Url> = probe
                    .docs
                    .iter()
                    .filter_map(|(uri, line)| {
                        let scope = crate::cross_file::scope::scope_at_position_with_graph(
                            uri,
                            *line,
                            u32::MAX,
                            &get_artifacts,
                            &get_metadata,
                            &probe.graph,
                            probe.workspace_folder.as_ref(),
                            probe.max_chain_depth,
                            &empty_base_exports,
                            false,
                            probe.backward_dependencies,
                            &|| false,
                            Some(&probe.scope_contribution),
                            // Probe reads only packages, not `scope.symbols`;
                            // no `data()` alias expansion needed (issue #429).
                            None,
                        );
                        // Scope probe captures inherited + global-scope packages.
                        // Also check the document's full loaded_packages which
                        // includes function-local library() calls the EOF scope
                        // probe misses.
                        let scope_hit = scope
                            .inherited_packages
                            .iter()
                            .chain(scope.loaded_packages.iter())
                            .any(|p| trigger_set.contains(p));
                        let doc_hit = probe
                            .doc_loaded_packages
                            .get(uri)
                            .map(|pkgs| pkgs.iter().any(|p| trigger_set.contains(p)))
                            .unwrap_or(false);
                        if scope_hit || doc_hit {
                            Some(uri.clone())
                        } else {
                            None
                        }
                    })
                    .collect();

                // Union in the system.file republish set: open documents whose
                // resolution changed plus their open transitive dependents —
                // an edge formed or dropped on a child changes a parent's
                // cross-file scope even when the parent loads no affected
                // package. Closed files need no publish; the set is already
                // filtered to open docs by `system_file_republish_set`.
                for uri in system_file_republish {
                    if !affected_uris.contains(&uri) {
                        affected_uris.push(uri);
                    }
                }

                // Force republish is safe: the text hasn't changed but the
                // underlying package set has, so we want to overwrite the last
                // publish at the same version.
                {
                    let state = state_arc.read().await;
                    state
                        .diagnostics_gate
                        .mark_force_republish_many(affected_uris.iter());
                }
                Backend::publish_diagnostics_for_uris_bounded(
                    Arc::clone(&state_arc),
                    client.clone(),
                    affected_uris,
                    None,
                )
                .await;
            }
            LibpathEvent::Dropped => {
                log::warn!(
                    "LibpathWatcher: dropped (allow_recovery={}); clearing package cache and \
                     force-republishing diagnostics",
                    allow_recovery
                );

                // The watcher is gone so we can no longer track installs or
                // upgrades; any cached help content may now be stale.
                state_arc.read().await.clear_help_caches();

                let open_uris = prepare_dropped_recovery(&state_arc).await;
                Backend::publish_diagnostics_for_uris_bounded(
                    Arc::clone(&state_arc),
                    client.clone(),
                    open_uris,
                    None,
                )
                .await;

                // Attempt a one-shot recovery. The replacement consumer runs
                // with `allow_recovery = false` so a persistent failure cannot
                // loop. Users can still force a full re-discovery via
                // `raven.refreshPackages`, which additionally rebuilds the
                // PackageLibrary (picking up `.libPaths()` changes).
                if allow_recovery {
                    let attached = restart_libpath_watcher(&state_arc, &client, false).await;
                    log::info!(
                        "LibpathWatcher: recovery attempted, new watcher attached = {}",
                        attached
                    );
                }
                return;
            }
        }
    }
    log::info!("LibpathWatcher consumer channel closed; exiting");
}

#[cfg(test)]
mod tests {
    mod document_indent_units {
        #[test]
        fn normalizes_client_supplied_indent_units() {
            assert_eq!(super::super::normalize_document_indent_unit(0), 1);
            assert_eq!(super::super::normalize_document_indent_unit(4), 4);
            assert_eq!(super::super::normalize_document_indent_unit(99), 8);
        }
    }

    mod config_file_watches {
        use tempfile::TempDir;
        use tower_lsp::lsp_types::{GlobPattern, OneOf, Url, WatchKind};

        #[test]
        fn adds_exact_watches_for_external_config_directory() {
            let home = TempDir::new().unwrap();
            let workspace = home.path().join("project");
            std::fs::create_dir(&workspace).unwrap();

            let workspace_roots = vec![Url::from_file_path(&workspace).unwrap()];
            let watchers = super::super::build_external_project_config_watchers(
                &workspace_roots,
                &crate::config_file::DiscoveryOptions::default(),
            );
            let parent_uri = Url::from_file_path(home.path()).unwrap();

            for watcher in &watchers {
                assert_eq!(
                    watcher.kind,
                    Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete)
                );
            }
            assert!(
                watchers.iter().any(|watcher| matches!(
                    &watcher.glob_pattern,
                    GlobPattern::Relative(pattern)
                        if pattern.base_uri == OneOf::Right(parent_uri.clone())
                            && pattern.pattern == "raven.toml"
                )),
                "expected exact watched-file pattern for external raven.toml, got {watchers:?}"
            );
            assert!(
                watchers.iter().any(|watcher| matches!(
                    &watcher.glob_pattern,
                    GlobPattern::Relative(pattern)
                        if pattern.base_uri == OneOf::Right(parent_uri.clone())
                            && pattern.pattern == ".lintr"
                )),
                "expected exact watched-file pattern for external .lintr, got {watchers:?}"
            );
        }
    }

    mod bounded_fanout {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        fn record_max(max_seen: &AtomicUsize, current: usize) {
            let mut observed = max_seen.load(Ordering::Acquire);
            while current > observed {
                match max_seen.compare_exchange_weak(
                    observed,
                    current,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(next) => observed = next,
                }
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn run_bounded_fanout_runs_all_items_without_exceeding_limit() {
            let active = Arc::new(AtomicUsize::new(0));
            let max_seen = Arc::new(AtomicUsize::new(0));
            let completed = Arc::new(AtomicUsize::new(0));
            let limit = 3;
            let items: Vec<usize> = (0..24).collect();

            super::super::run_bounded_fanout(items, limit, {
                let active = Arc::clone(&active);
                let max_seen = Arc::clone(&max_seen);
                let completed = Arc::clone(&completed);
                move |_| {
                    let active = Arc::clone(&active);
                    let max_seen = Arc::clone(&max_seen);
                    let completed = Arc::clone(&completed);
                    async move {
                        let now = active.fetch_add(1, Ordering::AcqRel) + 1;
                        record_max(&max_seen, now);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        active.fetch_sub(1, Ordering::AcqRel);
                        completed.fetch_add(1, Ordering::AcqRel);
                    }
                }
            })
            .await;

            assert_eq!(completed.load(Ordering::Acquire), 24);
            assert!(
                max_seen.load(Ordering::Acquire) <= limit,
                "bounded fan-out exceeded concurrency limit"
            );
            assert!(
                max_seen.load(Ordering::Acquire) > 1,
                "test should exercise actual parallelism"
            );
        }
    }

    mod request_cancellation {
        use super::super::{
            Backend, CURRENT_LSP_REQUEST_ID, CancellableRequestKind, RequestCancellationRegistry,
            RequestCancellationService,
        };
        use std::convert::Infallible;
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Arc;
        use std::task::{Context, Poll};
        use std::time::Duration;
        use tokio::sync::oneshot;
        use tower::Service;
        use tower_lsp::jsonrpc::{
            Id as JsonRpcId, Request as JsonRpcRequest, Response as JsonRpcResponse,
        };
        use tower_lsp::lsp_types::{GotoDefinitionParams, InitializeParams, InitializeResult};
        use tower_lsp::{LanguageServer, LspService};

        #[derive(Debug)]
        struct DummyLanguageServer;

        #[tower_lsp::async_trait]
        impl LanguageServer for DummyLanguageServer {
            async fn initialize(
                &self,
                _: InitializeParams,
            ) -> tower_lsp::jsonrpc::Result<InitializeResult> {
                Ok(InitializeResult::default())
            }

            async fn shutdown(&self) -> tower_lsp::jsonrpc::Result<()> {
                Ok(())
            }
        }

        struct ObservingService {
            registry: Arc<RequestCancellationRegistry>,
            observed: Option<oneshot::Sender<bool>>,
            release: Option<oneshot::Receiver<()>>,
        }

        impl Service<JsonRpcRequest> for ObservingService {
            type Response = Option<JsonRpcResponse>;
            type Error = Infallible;
            type Future =
                Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: JsonRpcRequest) -> Self::Future {
                let method = req.method().to_string();
                let id = req.id().cloned();
                if method != "textDocument/definition" {
                    return Box::pin(async { Ok(None) });
                }

                let registry = Arc::clone(&self.registry);
                let observed = self.observed.take().expect("single observed request");
                let release = self.release.take().expect("single observed request");
                Box::pin(async move {
                    let token_visible = registry
                        .token_for_current_request(CancellableRequestKind::GotoDefinition)
                        .is_some_and(|token| !token.is_cancelled());
                    observed.send(token_visible).ok();
                    release.await.ok();
                    Ok(id.map(|id| JsonRpcResponse::from_ok(id, serde_json::Value::Null)))
                })
            }
        }

        struct BackendGotoDefinitionService {
            backend: Arc<Backend>,
            registry: Arc<RequestCancellationRegistry>,
            observed: Option<oneshot::Sender<bool>>,
        }

        impl Service<JsonRpcRequest> for BackendGotoDefinitionService {
            type Response = Option<JsonRpcResponse>;
            type Error = Infallible;
            type Future =
                Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: JsonRpcRequest) -> Self::Future {
                if req.method() != "textDocument/definition" {
                    return Box::pin(async { Ok(None) });
                }

                let id = req.id().cloned().expect("definition request has id");
                let params: GotoDefinitionParams = serde_json::from_value(
                    req.params()
                        .cloned()
                        .expect("definition request has params"),
                )
                .expect("valid definition params");
                let backend = Arc::clone(&self.backend);
                let registry = Arc::clone(&self.registry);
                let observed = self.observed.take().expect("single observed request");

                Box::pin(async move {
                    let token_visible = registry
                        .token_for_current_request(CancellableRequestKind::GotoDefinition)
                        .is_some_and(|token| !token.is_cancelled());
                    observed.send(token_visible).ok();

                    let body = Backend::goto_definition(&backend, params)
                        .await
                        .map(|result| serde_json::to_value(result).expect("serializable result"));
                    Ok(Some(JsonRpcResponse::from_parts(id, body)))
                })
            }
        }

        #[test]
        fn superseding_same_kind_cancels_older_request() {
            let registry = RequestCancellationRegistry::new();
            let first = registry.register(
                JsonRpcId::from(1_i64),
                CancellableRequestKind::GotoDefinition,
            );
            let second = registry.register(
                JsonRpcId::from(2_i64),
                CancellableRequestKind::GotoDefinition,
            );

            assert!(first.is_cancelled());
            assert!(!second.is_cancelled());
        }

        #[tokio::test]
        async fn current_request_token_tracks_registered_id() {
            let registry = RequestCancellationRegistry::new();
            let id = JsonRpcId::from("definition-1");
            let token = registry.register(id.clone(), CancellableRequestKind::GotoDefinition);

            let cancel = CURRENT_LSP_REQUEST_ID
                .scope(id.clone(), async {
                    registry
                        .token_for_current_request(CancellableRequestKind::GotoDefinition)
                        .expect("current request has token")
                })
                .await;

            assert!(!cancel.is_cancelled());
            registry.cancel(&id);
            assert!(token.is_cancelled());
            assert!(cancel.is_cancelled());

            registry.complete(&id);
            let missing = CURRENT_LSP_REQUEST_ID
                .scope(id, async {
                    registry.token_for_current_request(CancellableRequestKind::GotoDefinition)
                })
                .await;
            assert!(missing.is_none());
        }

        #[test]
        fn completed_request_has_no_token() {
            let registry = RequestCancellationRegistry::new();
            let id = JsonRpcId::from(7_i64);
            registry.register(id.clone(), CancellableRequestKind::GotoDefinition);
            registry.complete(&id);

            assert!(
                registry
                    .token_for_id(&id, CancellableRequestKind::GotoDefinition)
                    .is_none()
            );
        }

        #[tokio::test]
        async fn unregistered_current_request_has_no_token() {
            let registry = RequestCancellationRegistry::new();
            let id = JsonRpcId::from("definition-1");

            let missing = CURRENT_LSP_REQUEST_ID
                .scope(id, async {
                    registry.token_for_current_request(CancellableRequestKind::GotoDefinition)
                })
                .await;

            assert!(missing.is_none());
        }

        #[tokio::test]
        async fn service_registers_scopes_cancels_and_completes_request_tokens() {
            let registry = Arc::new(RequestCancellationRegistry::new());
            let request_id = JsonRpcId::from(99_i64);
            let (observed_tx, observed_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            let inner = ObservingService {
                registry: Arc::clone(&registry),
                observed: Some(observed_tx),
                release: Some(release_rx),
            };
            let mut service = RequestCancellationService::new(inner, Arc::clone(&registry));

            let request = JsonRpcRequest::build("textDocument/definition")
                .id(request_id.clone())
                .finish();
            let request_fut = service.call(request);

            let token = registry
                .token_for_id(&request_id, CancellableRequestKind::GotoDefinition)
                .expect("request token registered on service call");
            assert!(!token.is_cancelled());

            let request_task = tokio::spawn(request_fut);
            assert!(
                observed_rx.await.expect("inner future observed request"),
                "inner future should see the task-local request token"
            );

            let cancel = JsonRpcRequest::build("$/cancelRequest")
                .params(serde_json::json!({ "id": 99 }))
                .finish();
            service.call(cancel).await.expect("cancel request handled");

            assert!(token.is_cancelled());
            assert!(
                registry
                    .token_for_id(&request_id, CancellableRequestKind::GotoDefinition)
                    .is_some_and(|token| token.is_cancelled())
            );

            release_tx.send(()).expect("release observed request");
            let response = request_task
                .await
                .expect("request task completes")
                .expect("inner service succeeds")
                .expect("request has a response");
            assert!(response.is_ok());
            assert!(
                registry
                    .token_for_id(&request_id, CancellableRequestKind::GotoDefinition)
                    .is_none()
            );
        }

        #[tokio::test]
        async fn goto_definition_cancels_while_waiting_for_state_read_lock() {
            let registry = Arc::new(RequestCancellationRegistry::new());
            let (client_tx, client_rx) = std::sync::mpsc::channel();
            let (_service, _socket) = LspService::new(|client| {
                client_tx.send(client).expect("capture client");
                DummyLanguageServer
            });
            let client = client_rx.recv().expect("client captured");
            let backend = Arc::new(Backend::new_with_request_cancellation(
                client,
                Arc::clone(&registry),
            ));

            let _state_write_guard = backend.state.write().await;
            let request_id = JsonRpcId::from(321_i64);
            let (observed_tx, observed_rx) = oneshot::channel();
            let inner = BackendGotoDefinitionService {
                backend: Arc::clone(&backend),
                registry: Arc::clone(&registry),
                observed: Some(observed_tx),
            };
            let mut service = RequestCancellationService::new(inner, Arc::clone(&registry));

            let request = JsonRpcRequest::build("textDocument/definition")
                .id(request_id.clone())
                .params(serde_json::json!({
                    "textDocument": { "uri": "file:///tmp/goto-definition-cancel.R" },
                    "position": { "line": 0, "character": 0 }
                }))
                .finish();
            let request_fut = service.call(request);

            let token = registry
                .token_for_id(&request_id, CancellableRequestKind::GotoDefinition)
                .expect("request token registered on service call");
            assert!(!token.is_cancelled());

            let request_task = tokio::spawn(request_fut);
            assert!(
                observed_rx.await.expect("backend future observed request"),
                "Backend::goto_definition should see the CURRENT_LSP_REQUEST_ID token"
            );

            let cancel = JsonRpcRequest::build("$/cancelRequest")
                .params(serde_json::json!({ "id": 321 }))
                .finish();
            service.call(cancel).await.expect("cancel request handled");

            assert!(token.is_cancelled());
            let response = tokio::time::timeout(Duration::from_secs(1), request_task)
                .await
                .expect("Backend::goto_definition should cancel without acquiring state read lock")
                .expect("request task completes")
                .expect("inner service succeeds")
                .expect("request has a response");
            assert_eq!(
                response.error(),
                Some(&tower_lsp::jsonrpc::Error::request_cancelled())
            );
            assert!(
                registry
                    .token_for_id(&request_id, CancellableRequestKind::GotoDefinition)
                    .is_none()
            );
        }
    }
    /// Tests for saturating arithmetic used in priority scoring
    /// Validates Requirements 1.1, 1.2
    mod saturating_arithmetic {
        #[test]
        fn test_saturating_add_at_max() {
            // usize::MAX + 1 should saturate to usize::MAX
            assert_eq!(usize::MAX.saturating_add(1), usize::MAX);
        }

        #[test]
        fn test_saturating_add_near_max() {
            // (usize::MAX - 1) + 1 should equal usize::MAX
            assert_eq!((usize::MAX - 1).saturating_add(1), usize::MAX);
        }

        #[test]
        fn test_saturating_add_normal_values() {
            // Normal values should work correctly
            assert_eq!(0_usize.saturating_add(1), 1);
            assert_eq!(100_usize.saturating_add(1), 101);
            assert_eq!(1000_usize.saturating_add(1), 1001);
        }
    }

    /// Tests for HashSet behavior in affected files collection
    /// Validates Requirements 3.3
    mod hashset_behavior {
        use std::collections::HashSet;

        #[test]
        fn test_first_insert_returns_true() {
            let mut set: HashSet<String> = HashSet::new();
            assert!(set.insert("file1.R".to_string()));
        }

        #[test]
        fn test_duplicate_insert_returns_false() {
            let mut set: HashSet<String> = HashSet::new();
            set.insert("file1.R".to_string());
            assert!(!set.insert("file1.R".to_string()));
        }

        #[test]
        fn test_no_duplicates_in_collection() {
            let mut set: HashSet<String> = HashSet::new();
            set.insert("file1.R".to_string());
            set.insert("file2.R".to_string());
            set.insert("file1.R".to_string()); // duplicate
            set.insert("file3.R".to_string());
            set.insert("file2.R".to_string()); // duplicate

            assert_eq!(set.len(), 3);
            assert!(set.contains("file1.R"));
            assert!(set.contains("file2.R"));
            assert!(set.contains("file3.R"));
        }
    }

    /// Tests for watched-file revalidation cap handling.
    mod watched_file_revalidation_cap {
        use super::super::cap_watched_file_revalidations;
        use crate::cross_file::revalidation::CrossFileActivityState;
        use tower_lsp::lsp_types::Url;

        fn uri(name: &str) -> Url {
            Url::parse(&format!("file:///workspace/{name}")).unwrap()
        }

        #[test]
        fn post_update_union_is_capped_after_activity_prioritization() {
            let stale_pre_update_a = uri("pre_a.R");
            let stale_pre_update_b = uri("pre_b.R");
            let active_post_update = uri("post_active.R");

            let mut activity = CrossFileActivityState::new();
            activity.update(Some(active_post_update.clone()), vec![], 1);

            // Mirrors the watched-files async path: pre-update neighbors are
            // collected first, then post-update edge fanout appends a newly
            // reachable URI. The cap must apply to the final union so the
            // post-update URI cannot increase the scheduled count past max.
            let mut affected = vec![
                stale_pre_update_a.clone(),
                stale_pre_update_b,
                active_post_update.clone(),
            ];

            cap_watched_file_revalidations(&mut affected, &activity, 2);

            assert_eq!(affected.len(), 2);
            assert!(affected.contains(&active_post_update));
            assert!(affected.contains(&stale_pre_update_a));
        }

        #[test]
        fn zero_cap_schedules_no_watched_file_revalidations() {
            let mut affected = vec![uri("a.R"), uri("b.R")];
            let activity = CrossFileActivityState::new();

            cap_watched_file_revalidations(&mut affected, &activity, 0);

            assert!(affected.is_empty());
        }
    }

    /// Regression tests for `collect_close_fanout_siblings`, which decides
    /// which open package files get force-republished when a sibling closes
    /// and the close changes package-mode visibility.
    mod close_fanout_siblings {
        use super::super::{
            collect_close_fanout_siblings, collect_package_r_file_inputs_from_disk,
            extend_with_open_package_docs, hydrate_package_r_files_from_state,
            initialize_package_inputs_from_state, is_package_data_path,
            is_package_relevant_open_uri, is_package_source_dir, path_in_rprofile_sourced_set,
        };
        use crate::state::{Document, WorldState};
        use std::path::PathBuf;
        use tower_lsp::lsp_types::Url;

        fn pkg_root() -> PathBuf {
            PathBuf::from("/work/pkg")
        }

        fn r_uri(name: &str) -> Url {
            Url::from_file_path(pkg_root().join("R").join(name)).unwrap()
        }

        fn make_state_with_open_pkg_docs(names: &[&str]) -> WorldState {
            let mut state = WorldState::new();
            state
                .workspace_folders
                .push(Url::from_file_path(pkg_root()).unwrap());
            // Deterministic cap for tests; explicit overrides come per-test.
            state.cross_file_config.max_revalidations_per_trigger = 10;
            for name in names {
                state
                    .documents
                    .insert(r_uri(name), Document::new("x <- 1\n", Some(1)));
            }
            state
        }

        #[test]
        fn fanout_excludes_the_closing_uri() {
            let state = make_state_with_open_pkg_docs(&["a.R", "b.R", "c.R"]);
            let closing = r_uri("a.R");

            let fanout = collect_close_fanout_siblings(&state, &closing, &pkg_root());

            assert_eq!(
                fanout.len(),
                2,
                "should fan out to the two non-closing siblings"
            );
            assert!(
                fanout.iter().all(|(u, _, _)| u != &closing),
                "fanout must not include the closing URI"
            );
            assert!(fanout.iter().any(|(u, _, _)| u == &r_uri("b.R")));
            assert!(fanout.iter().any(|(u, _, _)| u == &r_uri("c.R")));
        }

        #[test]
        fn fanout_filters_out_files_outside_the_package_layout() {
            let mut state = make_state_with_open_pkg_docs(&["a.R"]);
            // Same workspace root, but not under R/ or tests/testthat/.
            let scratch = Url::from_file_path(pkg_root().join("scratch.R")).unwrap();
            state
                .documents
                .insert(scratch.clone(), Document::new("y <- 2\n", Some(1)));
            // Different workspace entirely.
            let outside = Url::from_file_path("/other/proj/foo.R").unwrap();
            state
                .documents
                .insert(outside.clone(), Document::new("z <- 3\n", Some(1)));

            let closing = r_uri("a.R");
            let fanout = collect_close_fanout_siblings(&state, &closing, &pkg_root());

            assert!(
                fanout
                    .iter()
                    .all(|(u, _, _)| u != &scratch && u != &outside),
                "files outside R/ and tests/testthat/ must not appear in fanout"
            );
        }

        #[test]
        fn fanout_includes_testthat_siblings() {
            let mut state = make_state_with_open_pkg_docs(&["a.R"]);
            let test_uri =
                Url::from_file_path(pkg_root().join("tests").join("testthat").join("test-a.R"))
                    .unwrap();
            state.documents.insert(
                test_uri.clone(),
                Document::new("expect_equal(1, 1)\n", Some(1)),
            );

            let closing = r_uri("a.R");
            let fanout = collect_close_fanout_siblings(&state, &closing, &pkg_root());

            assert!(
                fanout.iter().any(|(u, _, _)| u == &test_uri),
                "tests/testthat/ siblings should refresh on R/ close"
            );
        }

        #[test]
        fn fanout_respects_max_revalidations_per_trigger() {
            let names: Vec<String> = (0..20).map(|i| format!("f{i}.R")).collect();
            let names_refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let mut state = make_state_with_open_pkg_docs(&names_refs);
            state.cross_file_config.max_revalidations_per_trigger = 3;

            let closing = r_uri("f0.R");
            let fanout = collect_close_fanout_siblings(&state, &closing, &pkg_root());

            assert_eq!(
                fanout.len(),
                3,
                "fanout must be capped at max_revalidations_per_trigger \
                 to bound diagnostic bursts on close-all"
            );
        }

        #[test]
        fn fanout_snapshots_each_siblings_version_and_revision() {
            let mut state = make_state_with_open_pkg_docs(&["a.R"]);
            // Insert a sibling with a specific version/revision pair.
            let b = r_uri("b.R");
            let mut doc_b = Document::new("y <- 2\n", Some(7));
            doc_b.revision = 42;
            state.documents.insert(b.clone(), doc_b);

            let closing = r_uri("a.R");
            let fanout = collect_close_fanout_siblings(&state, &closing, &pkg_root());

            let (got_uri, got_version, got_revision) = fanout
                .into_iter()
                .find(|(u, _, _)| u == &b)
                .expect("b.R in fanout");
            assert_eq!(got_uri, b);
            assert_eq!(got_version, Some(7));
            assert_eq!(got_revision, Some(42));
        }

        #[test]
        fn open_package_doc_extension_dedupes_and_includes_tests() {
            let mut state = make_state_with_open_pkg_docs(&["a.R", "b.R"]);
            let test_uri =
                Url::from_file_path(pkg_root().join("tests").join("testthat").join("test-a.R"))
                    .unwrap();
            state
                .documents
                .insert(test_uri.clone(), Document::new("helper()\n", Some(1)));
            let scratch = Url::from_file_path(pkg_root().join("scratch.R")).unwrap();
            state
                .documents
                .insert(scratch.clone(), Document::new("scratch <- 1\n", Some(1)));

            let existing = r_uri("a.R");
            let mut affected = vec![existing.clone()];
            let mut affected_set = std::collections::HashSet::from([existing.clone()]);

            extend_with_open_package_docs(&mut affected, &mut affected_set, &state, &pkg_root());

            assert_eq!(affected.iter().filter(|u| *u == &existing).count(), 1);
            assert!(affected.contains(&r_uri("b.R")));
            assert!(affected.contains(&test_uri));
            assert!(!affected.contains(&scratch));
        }

        #[test]
        fn package_hydration_uses_disk_fallback_with_open_overlay() {
            let temp = tempfile::TempDir::new().unwrap();
            let root = temp.path();
            let r_dir = root.join("R");
            std::fs::create_dir_all(&r_dir).unwrap();
            let disk_only = r_dir.join("disk-only.R");
            let open_path = r_dir.join("open.R");
            std::fs::write(&disk_only, "disk_only <- 1\n").unwrap();
            std::fs::write(&open_path, "open_value <- 'disk'\n").unwrap();

            let mut state = WorldState::new();
            let open_uri = Url::from_file_path(&open_path).unwrap();
            state
                .documents
                .insert(open_uri, Document::new("open_value <- 'buffer'\n", Some(3)));

            let disk_seed = collect_package_r_file_inputs_from_disk(root);
            let hydrated = hydrate_package_r_files_from_state(&state, root, disk_seed);

            assert!(hydrated.contains_key(&disk_only));
            let open_entry = hydrated.get(&open_path).expect("open file hydrated");
            assert_eq!(&*open_entry.text, "open_value <- 'buffer'\n");
        }

        #[test]
        fn package_initialization_helper_populates_inputs_and_derives_state() {
            let temp = tempfile::TempDir::new().unwrap();
            let root = temp.path();
            let r_dir = root.join("R");
            std::fs::create_dir_all(&r_dir).unwrap();
            let helper_path = r_dir.join("helper.R");
            std::fs::write(&helper_path, "helper <- function() 1\n").unwrap();

            let mut state = WorldState::new();
            let disk_seed = collect_package_r_file_inputs_from_disk(root);
            initialize_package_inputs_from_state(
                &mut state,
                root.to_path_buf(),
                Some("Package: pkg\n".into()),
                None,
                disk_seed,
                None,
            );

            assert_eq!(state.package_inputs.workspace_root.as_deref(), Some(root));
            assert!(state.package_inputs.r_files.contains_key(&helper_path));
            assert!(
                state
                    .package_state
                    .scope_contribution()
                    .r_internal_symbols
                    .contains("helper"),
                "initial package input seeding must derive package state"
            );
        }

        #[tokio::test]
        async fn initialize_package_inputs_scans_rprofile() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            std::fs::create_dir_all(root.join("R")).unwrap();
            std::fs::write(
                root.join("R").join("functions.r"),
                "r_bind <- function() 1\n",
            )
            .unwrap();
            std::fs::write(
                root.join(".Rprofile"),
                "source(\"R/functions.r\")\nmy_helper <- function() 1\n",
            )
            .unwrap();

            let mut state = WorldState::new();
            // model_rprofile defaults true in CrossFileConfig.
            let disk_seed = collect_package_r_file_inputs_from_disk(root);
            // `None` → the helper scans `.Rprofile` inline (the non-contended
            // path); this asserts that inline scan still harvests the prelude.
            initialize_package_inputs_from_state(
                &mut state,
                root.to_path_buf(),
                Some("Package: pkg\n".into()),
                None,
                disk_seed,
                None,
            );

            assert!(state.package_inputs.model_rprofile);
            assert!(
                state.package_inputs.rprofile_symbols.contains("my_helper"),
                "got {:?}",
                state.package_inputs.rprofile_symbols
            );
            assert!(
                state.package_inputs.rprofile_symbols.contains("r_bind"),
                "got {:?}",
                state.package_inputs.rprofile_symbols
            );
        }

        #[tokio::test]
        async fn initialize_package_inputs_skips_rprofile_when_disabled() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            std::fs::write(root.join(".Rprofile"), "my_helper <- function() 1\n").unwrap();
            let mut state = WorldState::new();
            state.cross_file_config.model_rprofile = false;
            // Pass a non-empty precomputed scan: when modeling is disabled the
            // prelude must be cleared regardless of what the caller supplies.
            let mut precomputed = crate::package_state::rprofile::RprofileScan::default();
            precomputed.symbols.insert("my_helper".to_string());
            initialize_package_inputs_from_state(
                &mut state,
                root.to_path_buf(),
                None,
                None,
                Default::default(),
                Some(precomputed),
            );
            assert!(
                state.package_inputs.rprofile_symbols.is_empty(),
                "disabled → prelude cleared even when a scan is passed"
            );
        }

        #[test]
        fn package_source_dir_matches_r_and_testthat_files() {
            let root = pkg_root();
            assert!(is_package_source_dir(&root.join("R").join("foo.R"), &root));
            assert!(is_package_source_dir(
                &root.join("tests").join("testthat").join("test-foo.R"),
                &root
            ));
            assert!(!is_package_source_dir(
                &root.join("tests").join("helper.R"),
                &root
            ));
            assert!(!is_package_source_dir(&root.join("scratch.R"), &root));
        }

        #[test]
        fn package_data_path_matches_data_and_data_raw_files() {
            // FIX 2: CREATED/CHANGED events under data/ and data-raw/ must route
            // through the package-state gate (they have dedicated translate()
            // handlers that rescan dataset_names / sysdata_names).
            let root = pkg_root();
            assert!(is_package_data_path(
                &root.join("data").join("mtcars.rda"),
                &root
            ));
            assert!(is_package_data_path(
                &root.join("data-raw").join("prep.R"),
                &root
            ));
            assert!(is_package_data_path(
                &root.join("data").join("sub").join("x.rda"),
                &root
            ));
            // The directory nodes themselves are not data *files*.
            assert!(!is_package_data_path(&root.join("data"), &root));
            assert!(!is_package_data_path(&root.join("data-raw"), &root));
            // Unrelated paths and sibling dirs with the `data` prefix must not match.
            assert!(!is_package_data_path(&root.join("R").join("foo.R"), &root));
            assert!(!is_package_data_path(
                &root.join("database").join("x.R"),
                &root
            ));
            // The gate must accept either source files or data files together,
            // mirroring the `has_pkg_files` predicate in the watched-file handler.
            let data_path = root.join("data-raw").join("prep.R");
            assert!(
                crate::package_state::is_r_source_path(&data_path, &root).is_none()
                    && !is_package_source_dir(&data_path, &root)
                    && is_package_data_path(&data_path, &root),
                "data-raw/*.R is reached only via the data-path branch"
            );
        }

        #[test]
        fn rprofile_sourced_set_membership_is_canonical_aware() {
            use std::collections::BTreeSet;
            // The watched-files gate routes a `.Rprofile` sourced helper into
            // translate() even when it lives outside the tracked R dirs. The set
            // stores canonical paths; a watcher path that already matches must
            // hit directly. Use a real tempdir so canonicalize() succeeds.
            let tmp = tempfile::TempDir::new().unwrap();
            let helper = tmp.path().join("scripts").join("setup.R");
            std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
            std::fs::write(&helper, "x <- 1\n").unwrap();
            let canonical = helper.canonicalize().unwrap();

            let mut sourced: BTreeSet<PathBuf> = BTreeSet::new();
            sourced.insert(canonical.clone());

            // Canonical path is a member.
            assert!(path_in_rprofile_sourced_set(&canonical, &sourced));
            // A non-canonical sibling path (`scripts/../scripts/setup.R`)
            // resolves via the canonicalize fallback.
            let non_canonical = tmp
                .path()
                .join("scripts")
                .join("..")
                .join("scripts")
                .join("setup.R");
            assert!(path_in_rprofile_sourced_set(&non_canonical, &sourced));
            // An unrelated file is not a member.
            let other = tmp.path().join("scripts").join("other.R");
            assert!(!path_in_rprofile_sourced_set(&other, &sourced));
        }

        #[test]
        fn package_relevant_open_uri_filters_to_package_inputs() {
            let root = pkg_root();
            let source = Url::from_file_path(root.join("R").join("foo.R")).unwrap();
            let test = Url::from_file_path(root.join("tests").join("testthat").join("test-foo.R"))
                .unwrap();
            let desc = Url::from_file_path(root.join("DESCRIPTION")).unwrap();
            let namespace = Url::from_file_path(root.join("NAMESPACE")).unwrap();
            let scratch = Url::from_file_path(root.join("scratch.R")).unwrap();
            let data = Url::from_file_path(root.join("data").join("foo.R")).unwrap();

            assert!(is_package_relevant_open_uri(&source, &root));
            assert!(is_package_relevant_open_uri(&test, &root));
            assert!(is_package_relevant_open_uri(&desc, &root));
            assert!(is_package_relevant_open_uri(&namespace, &root));
            assert!(!is_package_relevant_open_uri(&scratch, &root));
            assert!(!is_package_relevant_open_uri(&data, &root));
        }
    }

    /// Tests for `did_open` re-enrichment cap merge-and-resort behavior.
    ///
    /// Regression coverage for commit 9f4bc45: when the initial scheduling
    /// pass already filled `work_items` to `max_revalidations_per_trigger`,
    /// the prior implementation appended new neighbors only while
    /// `work_items.len() < max_revalidations`, so it silently dropped every
    /// post-update neighbor — including higher-priority URIs that should
    /// have displaced lower-priority initial-pass entries.
    mod reenrichment_revalidation_cap {
        use super::super::merge_and_cap_reenrichment_revalidations;
        use crate::cross_file::revalidation::CrossFileActivityState;
        use std::collections::HashSet;
        use tower_lsp::lsp_types::Url;

        fn uri(name: &str) -> Url {
            Url::parse(&format!("file:///workspace/{name}")).unwrap()
        }

        #[test]
        fn higher_priority_post_update_neighbor_displaces_lower_priority_initial_pass_entry() {
            // Pinned trigger.
            let edited = uri("edited.R");
            // Initial pass filled the cap with two low-priority URIs.
            let stale_a = uri("stale_a.R");
            let stale_b = uri("stale_b.R");
            // Re-enrichment surfaces a high-priority URI (the user's active
            // document) that wasn't reachable before the graph update.
            let active_post_update = uri("active.R");

            let mut activity = CrossFileActivityState::new();
            activity.update(Some(active_post_update.clone()), vec![], 1);

            let prev_uris: HashSet<Url> = [stale_a.clone(), stale_b.clone()].into_iter().collect();
            let new_neighbors = vec![active_post_update.clone()];

            let final_uris = merge_and_cap_reenrichment_revalidations(
                &edited,
                prev_uris,
                new_neighbors,
                2,
                &activity,
            );

            // Cap honored: still two URIs.
            assert_eq!(final_uris.len(), 2);
            // The high-priority post-update URI MUST be scheduled — this is
            // the regression: pre-fix, it was silently dropped because the
            // initial pass had already filled the cap.
            assert!(
                final_uris.contains(&active_post_update),
                "active post-update URI was dropped: {final_uris:?}"
            );
            // Sorted by priority: active comes first.
            assert_eq!(final_uris[0], active_post_update);
        }

        #[test]
        fn pinned_uri_sorts_first_even_when_low_activity_priority() {
            // Pinned trigger: not in activity state at all (priority MAX
            // via fallback) — but must still sort first because it's the
            // edited file.
            let edited = uri("edited.R");
            let active = uri("active.R");

            let mut activity = CrossFileActivityState::new();
            activity.update(Some(active.clone()), vec![], 1);

            let prev_uris: HashSet<Url> = [edited.clone(), active.clone()].into_iter().collect();

            let final_uris =
                merge_and_cap_reenrichment_revalidations(&edited, prev_uris, vec![], 10, &activity);

            assert_eq!(final_uris[0], edited);
            assert_eq!(final_uris[1], active);
        }

        #[test]
        fn deduplicates_new_neighbors_against_prev_uris_and_each_other() {
            let edited = uri("edited.R");
            let a = uri("a.R");
            let b = uri("b.R");

            let activity = CrossFileActivityState::new();
            let prev_uris: HashSet<Url> = [a.clone()].into_iter().collect();
            // Duplicates within new_neighbors AND against prev_uris.
            let new_neighbors = vec![a.clone(), b.clone(), b.clone(), a.clone()];

            let final_uris = merge_and_cap_reenrichment_revalidations(
                &edited,
                prev_uris,
                new_neighbors,
                10,
                &activity,
            );

            assert_eq!(final_uris.len(), 2);
            assert!(final_uris.contains(&a));
            assert!(final_uris.contains(&b));
        }

        #[test]
        fn cap_truncates_lowest_priority_first() {
            let edited = uri("edited.R");
            let active = uri("active.R");
            let visible = uri("visible.R");
            let other = uri("other.R");

            let mut activity = CrossFileActivityState::new();
            activity.update(Some(active.clone()), vec![visible.clone()], 1);

            let prev_uris: HashSet<Url> = [other.clone()].into_iter().collect();
            let new_neighbors = vec![active.clone(), visible.clone()];

            let final_uris = merge_and_cap_reenrichment_revalidations(
                &edited,
                prev_uris,
                new_neighbors,
                2,
                &activity,
            );

            // Cap = 2: active and visible win, `other` is truncated.
            assert_eq!(final_uris.len(), 2);
            assert_eq!(final_uris[0], active);
            assert_eq!(final_uris[1], visible);
            assert!(!final_uris.contains(&other));
        }

        #[test]
        fn zero_cap_schedules_nothing() {
            let edited = uri("edited.R");
            let activity = CrossFileActivityState::new();
            let prev_uris: HashSet<Url> = [uri("a.R")].into_iter().collect();
            let new_neighbors = vec![uri("b.R")];

            let final_uris = merge_and_cap_reenrichment_revalidations(
                &edited,
                prev_uris,
                new_neighbors,
                0,
                &activity,
            );

            assert!(final_uris.is_empty());
        }
    }

    mod traversal_truncation_notice {
        use super::super::TraversalTruncationState;

        #[test]
        fn delta_detects_new_truncations_and_advances_baselines() {
            let state = TraversalTruncationState::default();

            let delta = state.consume_delta(3, 2);

            assert_eq!(delta.visited_budget, 3);
            assert_eq!(delta.depth, 2);
            assert_eq!(state.consume_delta(3, 2).total(), 0);

            let delta = state.consume_delta(5, 4);
            assert_eq!(delta.visited_budget, 2);
            assert_eq!(delta.depth, 2);
        }

        #[test]
        fn visited_budget_notice_is_throttled_until_reset() {
            let state = TraversalTruncationState::default();

            assert!(state.should_show_visited_budget_notice());
            assert!(!state.should_show_visited_budget_notice());

            state.reset_notice_throttle();

            assert!(state.should_show_visited_budget_notice());
        }
    }

    // ============================================================================
    // Unit Tests for Configuration Parsing Defaults
    // Property 4: Configuration parsing defaults to enabled when absent
    // **Validates: Requirements 2.4**
    // ============================================================================
    mod config_parsing_defaults {
        use serde_json::json;

        /// Test that missing `diagnostics.enabled` results in `true` (default)
        /// when the JSON has only the required `crossFile` section.
        ///
        /// **Property 4: Configuration parsing defaults to enabled when absent**
        /// **Validates: Requirements 2.4**
        #[test]
        fn test_minimal_json_defaults_diagnostics_enabled_to_true() {
            // JSON with only required crossFile section, no diagnostics section
            let settings = json!({
                "crossFile": {}
            });

            let config = crate::backend::parse_cross_file_config(&settings).unwrap();

            // Should successfully parse
            assert!(config.is_some(), "Configuration parsing should succeed");
            let config = config.unwrap();

            // diagnostics_enabled should default to true
            assert!(
                config.diagnostics_enabled,
                "diagnostics_enabled should default to true when diagnostics section is absent"
            );
        }

        /// Test that missing `diagnostics.enabled` results in `true` (default)
        /// when the `diagnostics` section exists but has no `enabled` key.
        ///
        /// **Property 4: Configuration parsing defaults to enabled when absent**
        /// **Validates: Requirements 2.4**
        #[test]
        fn test_diagnostics_section_without_enabled_defaults_to_true() {
            // JSON with diagnostics section but no enabled key
            let settings = json!({
                "diagnostics": {
                    "undefinedVariableSeverity": "off"
                }
            });

            let config = crate::backend::parse_cross_file_config(&settings).unwrap();

            // Should successfully parse
            assert!(config.is_some(), "Configuration parsing should succeed");
            let config = config.unwrap();

            // diagnostics_enabled should default to true
            assert!(
                config.diagnostics_enabled,
                "diagnostics_enabled should default to true when enabled key is absent"
            );
        }

        #[test]
        fn parse_cross_file_config_rprofile_prelude_defaults_on_and_reads_explicit() {
            // Default-on: a client that omits `packages.rprofilePrelude` still
            // gets the `.Rprofile` prelude — `CrossFileConfig::default()` has
            // `model_rprofile = true` and the parser only overrides when the
            // key is present. This is the value the package seed (in
            // `initialized()`) copies into `package_inputs.model_rprofile`, so
            // the feature is on by default even when the client sends nothing
            // for it. (The earlier "off by default?" worry was a test artifact:
            // the integration test never ran `initialized()`, so its seed never
            // executed — not a default-propagation gap.)
            let absent = json!({ "crossFile": {}, "packages": { "enabled": true } });
            let config = crate::backend::parse_cross_file_config(&absent)
                .unwrap()
                .expect("packages section present → Some");
            assert!(
                config.model_rprofile,
                "rprofilePrelude must default to true when the client omits it"
            );

            // Explicit values are honored.
            let off = json!({ "packages": { "rprofilePrelude": false } });
            let config = crate::backend::parse_cross_file_config(&off)
                .unwrap()
                .expect("packages section present → Some");
            assert!(
                !config.model_rprofile,
                "explicit rprofilePrelude=false must be honored"
            );

            let on = json!({ "packages": { "rprofilePrelude": true } });
            let config = crate::backend::parse_cross_file_config(&on)
                .unwrap()
                .expect("packages section present → Some");
            assert!(config.model_rprofile);
        }

        #[test]
        fn parse_cross_file_config_reads_undefined_variable_severity_off() {
            use tower_lsp::lsp_types::DiagnosticSeverity;

            let settings = json!({
                "diagnostics": { "undefinedVariableSeverity": "off" }
            });
            let config = crate::backend::parse_cross_file_config(&settings)
                .unwrap()
                .unwrap();
            assert_eq!(
                config.undefined_variable_severity, None,
                "'off' should disable the diagnostic"
            );

            // Sanity: other severities are unaffected by this setting.
            assert_eq!(
                config.missing_file_severity,
                Some(DiagnosticSeverity::WARNING)
            );
        }

        #[test]
        fn parse_cross_file_config_reads_undefined_variable_severity_explicit() {
            use tower_lsp::lsp_types::DiagnosticSeverity;

            for (input, expected) in [
                ("error", Some(DiagnosticSeverity::ERROR)),
                ("warning", Some(DiagnosticSeverity::WARNING)),
                ("information", Some(DiagnosticSeverity::INFORMATION)),
                ("hint", Some(DiagnosticSeverity::HINT)),
            ] {
                let settings = json!({
                    "diagnostics": { "undefinedVariableSeverity": input }
                });
                let config = crate::backend::parse_cross_file_config(&settings)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    config.undefined_variable_severity, expected,
                    "{input:?} should parse to {expected:?}"
                );
            }
        }

        #[test]
        fn parse_cross_file_config_undefined_variable_severity_defaults_to_warning() {
            use tower_lsp::lsp_types::DiagnosticSeverity;

            // Diagnostics section present but no undefinedVariableSeverity key
            let settings = json!({
                "diagnostics": { "enabled": true }
            });
            let config = crate::backend::parse_cross_file_config(&settings)
                .unwrap()
                .unwrap();
            assert_eq!(
                config.undefined_variable_severity,
                Some(DiagnosticSeverity::WARNING),
                "absent key should retain the default warning severity"
            );
        }

        #[test]
        fn parse_cross_file_config_reads_undefined_variable_scope_booleans() {
            // Issue #398: both default on; explicit false disables collection.
            let default_config = crate::backend::parse_cross_file_config(&json!({
                "diagnostics": { "enabled": true }
            }))
            .unwrap()
            .unwrap();
            assert!(default_config.undefined_variable_in_call_arguments);
            assert!(default_config.undefined_variable_in_bracket_indices);

            let off_config = crate::backend::parse_cross_file_config(&json!({
                "diagnostics": {
                    "undefinedVariableInCallArguments": false,
                    "undefinedVariableInBracketIndices": false,
                }
            }))
            .unwrap()
            .unwrap();
            assert!(!off_config.undefined_variable_in_call_arguments);
            assert!(!off_config.undefined_variable_in_bracket_indices);
        }

        /// Test that missing `diagnostics.enabled` results in `true` (default)
        /// when other settings are present but no `diagnostics` section exists.
        ///
        /// **Property 4: Configuration parsing defaults to enabled when absent**
        /// **Validates: Requirements 2.4**
        #[test]
        fn test_other_settings_without_diagnostics_section_defaults_to_true() {
            // JSON with other settings but no diagnostics section
            let settings = json!({
                "crossFile": {
                    "enabled": true
                },
                "packages": {
                    "enabled": true
                }
            });

            let config = crate::backend::parse_cross_file_config(&settings).unwrap();

            // Should successfully parse
            assert!(config.is_some(), "Configuration parsing should succeed");
            let config = config.unwrap();

            // diagnostics_enabled should default to true
            assert!(
                config.diagnostics_enabled,
                "diagnostics_enabled should default to true when diagnostics section is absent"
            );
        }

        #[test]
        fn test_non_string_backward_dependencies_defaults_to_auto() {
            let settings = json!({
                "crossFile": {
                    "backwardDependencies": false
                }
            });

            let config = crate::backend::parse_cross_file_config(&settings)
                .expect("non-string value should not cause an error")
                .expect("should return Some config");
            assert_eq!(
                config.backward_dependencies,
                crate::cross_file::BackwardDependencyMode::Auto,
                "non-string value should default to Auto"
            );
        }

        #[test]
        fn test_invalid_backward_dependencies_defaults_to_auto() {
            let settings = json!({
                "crossFile": {
                    "backwardDependencies": "invalid"
                }
            });

            let config = crate::backend::parse_cross_file_config(&settings)
                .expect("invalid string value should not cause an error")
                .expect("should return Some config");
            assert_eq!(
                config.backward_dependencies,
                crate::cross_file::BackwardDependencyMode::Auto,
                "unrecognized string value should default to Auto"
            );
        }

        #[test]
        fn test_non_object_cross_file_section_returns_error() {
            let settings = json!({ "crossFile": true });
            let err = crate::backend::parse_cross_file_config(&settings).unwrap_err();
            assert!(
                err.contains("crossFile must be an object"),
                "expected object validation error, got: {}",
                err
            );
        }

        #[test]
        fn test_non_object_diagnostics_section_returns_error() {
            let settings = json!({ "diagnostics": "yes" });
            let err = crate::backend::parse_cross_file_config(&settings).unwrap_err();
            assert!(
                err.contains("diagnostics must be an object"),
                "expected object validation error, got: {}",
                err
            );
        }

        #[test]
        fn test_non_object_packages_section_returns_error() {
            let settings = json!({ "packages": 42 });
            let err = crate::backend::parse_cross_file_config(&settings).unwrap_err();
            assert!(
                err.contains("packages must be an object"),
                "expected object validation error, got: {}",
                err
            );
        }

        #[test]
        fn parse_cross_file_config_reads_watch_fields() {
            let settings = json!({
                "packages": {
                    "watchLibraryPaths": false,
                    "watchDebounceMs": 250
                }
            });
            let cfg = crate::backend::parse_cross_file_config(&settings)
                .unwrap()
                .unwrap();
            assert!(!cfg.packages_watch_library_paths);
            assert_eq!(cfg.packages_watch_debounce_ms, 250);
        }

        #[test]
        fn parse_cross_file_config_reads_max_transitive_dependents_visited() {
            let settings = json!({
                "crossFile": {
                    "maxTransitiveDependentsVisited": 1234
                }
            });

            let cfg = crate::backend::parse_cross_file_config(&settings)
                .unwrap()
                .unwrap();
            assert_eq!(cfg.max_transitive_dependents_visited, 1234);
        }

        #[test]
        fn parse_cross_file_config_reads_rprofile_prelude() {
            let settings = json!({
                "packages": { "rprofilePrelude": false }
            });
            let config = crate::backend::parse_cross_file_config(&settings)
                .unwrap()
                .unwrap();
            assert!(!config.model_rprofile, "rprofilePrelude=false must parse");
        }

        #[test]
        fn parse_cross_file_config_rprofile_prelude_defaults_true() {
            let settings = json!({ "packages": {} });
            let config = crate::backend::parse_cross_file_config(&settings)
                .unwrap()
                .unwrap();
            assert!(
                config.model_rprofile,
                "rprofilePrelude must default to true"
            );
        }

        #[test]
        fn parse_cross_file_config_reads_namespace_member_severity() {
            use tower_lsp::lsp_types::DiagnosticSeverity;
            let settings = json!({
                "packages": { "namespaceMemberSeverity": "error" }
            });
            let config = crate::backend::parse_cross_file_config(&settings)
                .unwrap()
                .unwrap();
            assert_eq!(
                config.packages_namespace_member_severity,
                Some(DiagnosticSeverity::ERROR)
            );
        }

        #[test]
        fn parse_cross_file_config_namespace_member_severity_defaults_to_warning() {
            use tower_lsp::lsp_types::DiagnosticSeverity;
            let settings = json!({ "packages": {} });
            let config = crate::backend::parse_cross_file_config(&settings)
                .unwrap()
                .unwrap();
            assert_eq!(
                config.packages_namespace_member_severity,
                Some(DiagnosticSeverity::WARNING)
            );
        }

        #[test]
        fn parse_cross_file_config_clamps_watch_debounce_ms() {
            let settings = json!({
                "packages": { "watchDebounceMs": 50 }  // below floor
            });
            let cfg = crate::backend::parse_cross_file_config(&settings)
                .unwrap()
                .unwrap();
            assert_eq!(cfg.packages_watch_debounce_ms, 100);

            let settings = json!({
                "packages": { "watchDebounceMs": 99999 } // above ceiling
            });
            let cfg = crate::backend::parse_cross_file_config(&settings)
                .unwrap()
                .unwrap();
            assert_eq!(cfg.packages_watch_debounce_ms, 5000);
        }
    }

    // ============================================================================
    // LintConfig Parsing Tests
    // ============================================================================
    mod lint_config_parsing {
        use serde_json::json;

        #[test]
        fn parse_lint_config_returns_none_when_section_absent() {
            let settings = json!({});
            assert!(crate::backend::parse_lint_config(&settings, false).is_none());
        }

        #[test]
        fn parse_lint_config_reads_master_switch_and_line_length() {
            let settings = json!({
                "linting": {
                    "enabled": true,
                    "lineLength": 120
                }
            });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert!(cfg.enabled);
            assert_eq!(cfg.line_length, 120);
        }

        #[test]
        fn parse_lint_config_clamps_line_length() {
            let settings = json!({ "linting": { "lineLength": 1 } });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert_eq!(cfg.line_length, 20);

            let settings = json!({ "linting": { "lineLength": 999_999 } });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert_eq!(cfg.line_length, 10_000);
        }

        #[test]
        fn parse_lint_config_clamps_before_u32_truncation() {
            // Regression: casting `u64 as u32` before clamping wraps values
            // above u32::MAX into a small number (u32::MAX + 5 -> 4), which
            // then clamps to the floor of 20 instead of the ceiling of
            // 10_000.
            let oversized = (u32::MAX as u64) + 5;
            let settings = json!({ "linting": { "lineLength": oversized } });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert_eq!(cfg.line_length, 10_000);
        }

        #[test]
        fn parse_lint_config_reads_assignment_operator_styles() {
            let settings = json!({ "linting": { "assignmentOperator": "=" } });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert_eq!(
                cfg.assignment_operator_style,
                crate::linting::AssignmentOperatorStyle::Equals
            );

            let settings = json!({ "linting": { "assignmentOperator": "<-" } });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert_eq!(
                cfg.assignment_operator_style,
                crate::linting::AssignmentOperatorStyle::LeftArrow
            );
        }

        #[test]
        fn parse_lint_config_severities_off_disables_each_rule() {
            let settings = json!({
                "linting": {
                    "lineLengthSeverity": "off",
                    "trailingWhitespaceSeverity": "off",
                    "noTabSeverity": "off",
                    "trailingBlankLinesSeverity": "off",
                    "assignmentOperatorSeverity": "off",
                    "objectNameSeverity": "off",
                    "infixSpacesSeverity": "off",
                    "commentedCodeSeverity": "off",
                    "indentationSeverity": "off"
                }
            });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert_eq!(cfg.line_length_severity, None);
            assert_eq!(cfg.trailing_whitespace_severity, None);
            assert_eq!(cfg.no_tab_severity, None);
            assert_eq!(cfg.trailing_blank_lines_severity, None);
            assert_eq!(cfg.assignment_operator_severity, None);
            assert_eq!(cfg.object_name_severity, None);
            assert_eq!(cfg.infix_spaces_severity, None);
            assert_eq!(cfg.commented_code_severity, None);
            assert_eq!(cfg.indentation_severity, None);
        }

        #[test]
        fn parse_lint_config_reads_indentation_unit_and_clamps() {
            let settings = json!({ "linting": { "indentationUnit": 4 } });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert_eq!(cfg.indentation_unit, 4);

            // Above the ceiling is clamped down to 8.
            let settings = json!({ "linting": { "indentationUnit": 99 } });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert_eq!(cfg.indentation_unit, 8);

            // Zero is clamped up to 1 (the floor).
            let settings = json!({ "linting": { "indentationUnit": 0 } });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert_eq!(cfg.indentation_unit, 1);
        }

        #[test]
        fn parse_lint_config_reads_object_name_styles() {
            use crate::linting::ObjectNameStyle;
            let settings = json!({
                "linting": {
                    "objectNameStyleFunction": "camelCase",
                    "objectNameStyleVariable": "UPPER_CASE",
                    "objectNameStyleArgument": "any"
                }
            });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert_eq!(cfg.object_name_style_function, ObjectNameStyle::CamelCase);
            assert_eq!(cfg.object_name_style_variable, ObjectNameStyle::UpperCase);
            assert_eq!(cfg.object_name_style_argument, ObjectNameStyle::Any);
        }

        #[test]
        fn parse_lint_config_unrecognized_object_name_style_falls_back_to_any() {
            use crate::linting::ObjectNameStyle;
            let settings = json!({
                "linting": { "objectNameStyleFunction": "kebab-case" }
            });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert_eq!(cfg.object_name_style_function, ObjectNameStyle::Any);
        }

        #[test]
        fn parse_lint_config_non_object_returns_none() {
            let settings = json!({ "linting": 42 });
            assert!(crate::backend::parse_lint_config(&settings, false).is_none());
        }

        // ============================================================================
        // Tri-state `enabled` resolution (#281)
        // ============================================================================

        #[test]
        fn parse_lint_config_client_false_overrides_lintr_discovery() {
            // Regression for #281: an explicit client `enabled = false` must
            // remain false even when a .lintr is the discovered project config.
            let settings = json!({ "linting": { "enabled": false } });
            let cfg = crate::backend::parse_lint_config(&settings, true).unwrap();
            assert!(!cfg.enabled, "client false must win over .lintr discovery");
        }

        #[test]
        fn parse_lint_config_auto_default_no_project_off() {
            let settings = json!({ "linting": { "enabled": "auto" } });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert!(!cfg.enabled);
        }

        #[test]
        fn parse_lint_config_auto_with_lintr_on() {
            let settings = json!({ "linting": { "enabled": "auto" } });
            let cfg = crate::backend::parse_lint_config(&settings, true).unwrap();
            assert!(cfg.enabled);
        }

        #[test]
        fn parse_lint_config_auto_no_section_with_discovered_true_is_on() {
            // Pure-function contract: given lintr_discovered = true and no
            // linting section, return Some(default config) with enabled = true.
            // (End-to-end, a content-free `.lintr` no longer reaches here with
            // `true` — `config_file::lintr_expresses_linting` gates that — but
            // the function must remain defensively correct for any caller.)
            let settings = json!({});
            let cfg = crate::backend::parse_lint_config(&settings, true).unwrap();
            assert!(cfg.enabled);
            assert_eq!(
                cfg.line_length,
                crate::linting::LintConfig::default().line_length
            );
        }

        #[test]
        fn parse_lint_config_no_section_no_lintr_returns_none() {
            // No linting section AND no .lintr discovered → keep the old
            // behavior of returning None so the caller can decide a default.
            let settings = json!({});
            assert!(crate::backend::parse_lint_config(&settings, false).is_none());
        }

        #[test]
        fn parse_lint_config_string_on_off() {
            let on = json!({ "linting": { "enabled": "on" } });
            let off = json!({ "linting": { "enabled": "off" } });
            assert!(
                crate::backend::parse_lint_config(&on, false)
                    .unwrap()
                    .enabled
            );
            assert!(
                !crate::backend::parse_lint_config(&off, true)
                    .unwrap()
                    .enabled
            );
        }

        #[test]
        fn parse_lint_config_string_true_false_backcompat() {
            let t = json!({ "linting": { "enabled": "true" } });
            let f = json!({ "linting": { "enabled": "false" } });
            assert!(
                crate::backend::parse_lint_config(&t, false)
                    .unwrap()
                    .enabled
            );
            assert!(!crate::backend::parse_lint_config(&f, true).unwrap().enabled);
        }

        #[test]
        fn parse_lint_config_bool_backcompat() {
            let t = json!({ "linting": { "enabled": true } });
            let f = json!({ "linting": { "enabled": false } });
            assert!(
                crate::backend::parse_lint_config(&t, false)
                    .unwrap()
                    .enabled
            );
            assert!(!crate::backend::parse_lint_config(&f, true).unwrap().enabled);
        }

        #[test]
        fn parse_lint_config_invalid_string_warns_falls_back_to_auto() {
            let settings = json!({ "linting": { "enabled": "yes" } });
            // With lintr_discovered = false, Auto resolves to off.
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert!(!cfg.enabled);
            // With lintr_discovered = true, Auto resolves to on.
            let cfg = crate::backend::parse_lint_config(&settings, true).unwrap();
            assert!(cfg.enabled);
        }

        #[test]
        fn parse_lint_config_invalid_json_types_fall_back_to_auto() {
            for bad in [json!(42), json!([]), json!({})] {
                let settings = json!({ "linting": { "enabled": bad } });
                // Auto with no lintr_discovered → off.
                let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
                assert!(!cfg.enabled, "expected off for invalid enabled value {bad}");
            }
        }

        #[test]
        fn parse_lint_config_null_silent_falls_back_to_auto() {
            let settings = json!({ "linting": { "enabled": null } });
            let cfg = crate::backend::parse_lint_config(&settings, false).unwrap();
            assert!(!cfg.enabled);
            let cfg = crate::backend::parse_lint_config(&settings, true).unwrap();
            assert!(cfg.enabled);
        }
    }

    // ============================================================================
    // CompletionConfig Parsing Tests
    // ============================================================================
    mod completion_config_parsing {
        use serde_json::json;

        /// Test that `parse_completion_config` returns None when completion section is absent.
        #[test]
        fn test_trigger_on_open_paren_defaults_to_true() {
            let settings = json!({
                "crossFile": {}
            });

            let config = crate::backend::parse_completion_config(&settings);
            assert!(
                config.is_none(),
                "Should return None when completion section is absent"
            );
            // When None, caller uses CompletionConfig::default() which has trigger_on_open_paren: true
            let default = crate::state::CompletionConfig::default();
            assert!(default.trigger_on_open_paren);
        }

        /// Test that `completion.triggerOnOpenParen` can be set to false.
        #[test]
        fn test_trigger_on_open_paren_explicit_false() {
            let settings = json!({
                "completion": {
                    "triggerOnOpenParen": false
                }
            });

            let config = crate::backend::parse_completion_config(&settings);
            assert!(config.is_some());
            let config = config.unwrap();
            assert!(
                !config.trigger_on_open_paren,
                "trigger_on_open_paren should be false when explicitly set to false"
            );
        }

        /// Test that `completion.triggerOnOpenParen` can be set to true explicitly.
        #[test]
        fn test_trigger_on_open_paren_explicit_true() {
            let settings = json!({
                "completion": {
                    "triggerOnOpenParen": true
                }
            });

            let config = crate::backend::parse_completion_config(&settings);
            assert!(config.is_some());
            let config = config.unwrap();
            assert!(
                config.trigger_on_open_paren,
                "trigger_on_open_paren should be true when explicitly set to true"
            );
        }

        /// Test that empty completion section returns default config.
        #[test]
        fn test_empty_completion_section_returns_default() {
            let settings = json!({
                "completion": {}
            });

            let config = crate::backend::parse_completion_config(&settings);
            assert!(
                config.is_some(),
                "Should return Some when completion section exists"
            );
            let config = config.unwrap();
            assert!(
                config.trigger_on_open_paren,
                "trigger_on_open_paren should default to true"
            );
        }

        #[test]
        fn test_completion_trigger_chars_include_file_path_triggers() {
            let trigger_chars = crate::backend::build_completion_trigger_chars(true);

            assert!(trigger_chars.contains(&"\"".to_string()));
            assert!(trigger_chars.contains(&"'".to_string()));
            assert!(trigger_chars.contains(&"/".to_string()));
        }

        #[test]
        fn test_completion_trigger_chars_optionally_include_open_paren() {
            let with_paren = crate::backend::build_completion_trigger_chars(true);
            let without_paren = crate::backend::build_completion_trigger_chars(false);

            assert!(with_paren.contains(&"(".to_string()));
            assert!(!without_paren.contains(&"(".to_string()));
        }
    }

    // ============================================================================
    // Property Tests for Saturating Arithmetic
    // Property 1: Saturating Arithmetic Prevents Overflow - validates Requirements 1.1, 1.2
    // ============================================================================
    mod property_tests {
        use proptest::prelude::*;
        use std::collections::HashSet;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            // ============================================================================
            // Property: completion.triggerOnOpenParen round-trip
            // For any boolean value `b`, if initialization options JSON contains
            // `completion.triggerOnOpenParen` set to `b`, parsing SHALL produce a
            // `CompletionConfig` with `trigger_on_open_paren` equal to `b`.
            // ============================================================================

            /// Property: trigger_on_open_paren round-trip parsing
            #[test]
            fn prop_config_parsing_trigger_on_open_paren_roundtrip(trigger: bool) {
                use serde_json::json;

                let settings = json!({
                    "completion": {
                        "triggerOnOpenParen": trigger
                    }
                });

                let config = crate::backend::parse_completion_config(&settings);
                prop_assert!(config.is_some(), "Configuration parsing should succeed");
                let config = config.unwrap();

                prop_assert_eq!(
                    config.trigger_on_open_paren,
                    trigger,
                    "trigger_on_open_paren should match input: expected {}, got {}",
                    trigger,
                    config.trigger_on_open_paren
                );
            }

            // ============================================================================
            // Property 3: Configuration parsing round-trip for explicit boolean
            // **Validates: Requirements 2.3**
            // For any boolean value `b`, if initialization options JSON contains
            // `diagnostics.enabled` set to `b`, parsing SHALL produce a `CrossFileConfig`
            // with `diagnostics_enabled` equal to `b`.
            // ============================================================================

            /// Property 3: Configuration parsing round-trip for explicit boolean
            #[test]
            fn prop_config_parsing_diagnostics_enabled_roundtrip(enabled: bool) {
                use serde_json::json;

                // Create JSON with diagnostics.enabled set to the generated boolean
                let settings = json!({
                    "crossFile": {},
                    "diagnostics": {
                        "enabled": enabled
                    }
                });

                // Parse the configuration
                let config = crate::backend::parse_cross_file_config(&settings).unwrap();

                // Should successfully parse
                prop_assert!(config.is_some(), "Configuration parsing should succeed");
                let config = config.unwrap();

                // The parsed diagnostics_enabled should equal the input boolean
                prop_assert_eq!(
                    config.diagnostics_enabled,
                    enabled,
                    "diagnostics_enabled should match input: expected {}, got {}",
                    enabled,
                    config.diagnostics_enabled
                );
            }

            /// Property 1: For any usize value, saturating_add(1) should never overflow
            /// and should return a value >= the original value.
            #[test]
            fn prop_saturating_add_never_overflows(value: usize) {
                let result = value.saturating_add(1);
                // Result should be >= original (no wrap-around)
                prop_assert!(result >= value);
            }

            /// Property 1 extended: saturating_add should be monotonic up to MAX
            #[test]
            fn prop_saturating_add_monotonic(value in 0..usize::MAX) {
                let result = value.saturating_add(1);
                // For values < MAX, result should be exactly value + 1
                prop_assert_eq!(result, value + 1);
            }

            /// Property 1 boundary: values at or near MAX should saturate
            #[test]
            fn prop_saturating_add_boundary(offset in 0_usize..10) {
                let value = usize::MAX - offset;
                let result = value.saturating_add(offset + 1);
                // Should saturate at MAX
                prop_assert_eq!(result, usize::MAX);
            }

            // ============================================================================
            // Property 2: System Stability at Boundary Conditions - validates Requirements 1.4
            // ============================================================================

            /// Property 2: System should remain stable when counters are at boundary values.
            /// Operations involving priority scores and depth counters should not panic or
            /// produce incorrect results at maximum values.
            #[test]
            fn prop_system_stability_at_boundaries(
                priority_score in 0_usize..=usize::MAX,
                depth in 0_usize..=usize::MAX,
                num_operations in 1_usize..100
            ) {
                // Simulate priority score adjustments
                let mut score = priority_score;
                for _ in 0..num_operations {
                    score = score.saturating_add(1);
                    // Should never panic or overflow
                    prop_assert!(score >= priority_score);
                }

                // Simulate depth increments
                let mut d = depth;
                for _ in 0..num_operations {
                    d = d.saturating_add(1);
                    // Should never panic or overflow
                    prop_assert!(d >= depth);
                }
            }

            // ============================================================================
            // Property 4: HashSet Insert Deduplication - validates Requirements 3.3
            // ============================================================================

            /// Property 4: For any sequence of strings with duplicates, HashSet should
            /// deduplicate and insert should return correct boolean.
            #[test]
            fn prop_hashset_insert_deduplication(
                items in prop::collection::vec("[a-z]{1,10}\\.R", 1..20)
            ) {
                let mut set: HashSet<String> = HashSet::new();
                let mut seen: HashSet<String> = HashSet::new();

                for item in &items {
                    let is_new = !seen.contains(item);
                    let insert_result = set.insert(item.clone());

                    // insert should return true iff item was not seen before
                    prop_assert_eq!(insert_result, is_new);
                    seen.insert(item.clone());
                }

                // Final set should have no duplicates
                let unique_count = items.iter().collect::<HashSet<_>>().len();
                prop_assert_eq!(set.len(), unique_count);
            }
        }
    }

    /// Integration tests for cross-file features
    /// Validates Requirements 1.4 (system stability at boundary conditions)
    mod integration_tests {
        use std::collections::HashSet;

        /// Test that affected files collection handles large dependency graphs
        #[test]
        fn test_large_dependency_graph_deduplication() {
            // Simulate a large dependency graph with many duplicates
            let mut affected: HashSet<String> = HashSet::new();

            // Add 1000 files with many duplicates
            for i in 0..1000 {
                let file = format!("file{}.R", i % 100); // Only 100 unique files
                affected.insert(file);
            }

            // Should have exactly 100 unique files
            assert_eq!(affected.len(), 100);

            // Convert to Vec for sorting (as done in actual code)
            let mut affected_vec: Vec<String> = affected.into_iter().collect();
            affected_vec.sort();

            assert_eq!(affected_vec.len(), 100);
        }

        /// Test that saturating arithmetic handles deep transitive dependencies
        #[test]
        fn test_deep_transitive_dependencies() {
            // Simulate depth tracking with saturating arithmetic
            let mut depth: usize = 0;

            // Simulate very deep dependency chain
            for _ in 0..1000 {
                depth = depth.saturating_add(1);
            }

            assert_eq!(depth, 1000);

            // Test at boundary
            depth = usize::MAX - 5;
            for _ in 0..10 {
                depth = depth.saturating_add(1);
            }

            // Should saturate at MAX
            assert_eq!(depth, usize::MAX);
        }

        /// Test that priority scoring handles maximum values
        #[test]
        fn test_priority_scoring_at_max() {
            // Simulate priority scoring with saturating arithmetic
            let scores = vec![0_usize, 1, 100, usize::MAX - 1, usize::MAX];

            for score in scores {
                let adjusted = score.saturating_add(1);
                // Should never overflow
                assert!(adjusted >= score);
            }
        }
    }

    /// Tests for on-demand indexing global flag
    /// Validates Requirements 1.1, 1.2, 1.3, 1.4
    mod on_demand_indexing_flag {
        /// Category of files for on-demand indexing (test-local copy)
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum IndexCategory {
            Sourced,
            BackwardDirective,
        }

        /// Property 1: On-demand indexing respects global flag
        /// When on_demand_indexing_enabled is false, no indexing operations should occur.
        #[test]
        fn test_global_flag_disables_all_indexing() {
            // Simulate the flag check logic from did_open
            let on_demand_enabled = false;
            let mut files_to_index: Vec<(String, IndexCategory)> = Vec::new();
            let mut sourced_indexed = false;
            let mut backward_indexed = false;

            // Simulate file collection (only if enabled)
            if on_demand_enabled {
                files_to_index.push(("sourced.R".to_string(), IndexCategory::Sourced));
                files_to_index.push(("parent.R".to_string(), IndexCategory::BackwardDirective));
            }

            // Simulate indexing (only if enabled)
            if on_demand_enabled {
                // Sourced files synchronous indexing
                for (_, category) in &files_to_index {
                    if *category == IndexCategory::Sourced {
                        sourced_indexed = true;
                    }
                }
                // Backward directive files synchronous indexing
                for (_, category) in &files_to_index {
                    if *category == IndexCategory::BackwardDirective {
                        backward_indexed = true;
                    }
                }
            }

            // Verify no indexing occurred
            assert!(
                files_to_index.is_empty(),
                "No files should be collected when disabled"
            );
            assert!(!sourced_indexed, "Sourced file indexing should be skipped");
            assert!(
                !backward_indexed,
                "Backward directive indexing should be skipped"
            );
        }

        #[test]
        fn test_global_flag_enables_indexing() {
            // Simulate the flag check logic from did_open
            let on_demand_enabled = true;
            let mut files_to_index: Vec<(String, IndexCategory)> = Vec::new();
            let mut sourced_indexed = false;
            let mut backward_indexed = false;

            // Simulate file collection (only if enabled)
            if on_demand_enabled {
                files_to_index.push(("sourced.R".to_string(), IndexCategory::Sourced));
                files_to_index.push(("parent.R".to_string(), IndexCategory::BackwardDirective));
            }

            // Simulate indexing (only if enabled)
            if on_demand_enabled {
                // Sourced files synchronous indexing
                for (_, category) in &files_to_index {
                    if *category == IndexCategory::Sourced {
                        sourced_indexed = true;
                    }
                }
                // Backward directive files synchronous indexing
                for (_, category) in &files_to_index {
                    if *category == IndexCategory::BackwardDirective {
                        backward_indexed = true;
                    }
                }
            }

            // Verify indexing occurred
            assert_eq!(
                files_to_index.len(),
                2,
                "Files should be collected when enabled"
            );
            assert!(sourced_indexed, "Sourced file indexing should occur");
            assert!(backward_indexed, "Backward directive indexing should occur");
        }
    }

    // ============================================================================
    // Unit Tests for Symbol Configuration Parsing
    // **Validates: Requirements 11.1, 11.2, 11.3**
    // ============================================================================
    mod symbol_config_parsing {
        use crate::state::SymbolConfig;
        use serde_json::json;

        /// Test that parse_symbol_config returns None when symbols section is absent
        /// **Validates: Requirements 11.1**
        #[test]
        fn test_missing_symbols_section_returns_none() {
            let settings = json!({
                "crossFile": {}
            });

            let config = crate::backend::parse_symbol_config(&settings);
            assert!(
                config.is_none(),
                "Should return None when symbols section is absent"
            );
        }

        /// Test that parse_symbol_config returns default when symbols section is empty
        /// **Validates: Requirements 11.1**
        #[test]
        fn test_empty_symbols_section_returns_default() {
            let settings = json!({
                "symbols": {}
            });

            let config = crate::backend::parse_symbol_config(&settings);
            assert!(
                config.is_some(),
                "Should return Some when symbols section exists"
            );
            let config = config.unwrap();
            assert_eq!(
                config.workspace_max_results,
                SymbolConfig::DEFAULT_WORKSPACE_MAX_RESULTS,
                "Should use default value when workspaceMaxResults is absent"
            );
        }

        /// Test that parse_symbol_config parses workspaceMaxResults correctly
        /// **Validates: Requirements 11.2**
        #[test]
        fn test_parse_workspace_max_results() {
            let settings = json!({
                "symbols": {
                    "workspaceMaxResults": 500
                }
            });

            let config = crate::backend::parse_symbol_config(&settings);
            assert!(
                config.is_some(),
                "Should return Some when symbols section exists"
            );
            let config = config.unwrap();
            assert_eq!(
                config.workspace_max_results, 500,
                "Should parse workspaceMaxResults value"
            );
        }

        /// Test that values below minimum are clamped to 100
        /// **Validates: Requirements 11.3**
        #[test]
        fn test_clamp_below_minimum() {
            let settings = json!({
                "symbols": {
                    "workspaceMaxResults": 50
                }
            });

            let config = crate::backend::parse_symbol_config(&settings);
            assert!(
                config.is_some(),
                "Should return Some when symbols section exists"
            );
            let config = config.unwrap();
            assert_eq!(
                config.workspace_max_results,
                SymbolConfig::MIN_WORKSPACE_MAX_RESULTS,
                "Values below minimum should be clamped to 100"
            );
        }

        /// Test that values above maximum are clamped to 10000
        /// **Validates: Requirements 11.3**
        #[test]
        fn test_clamp_above_maximum() {
            let settings = json!({
                "symbols": {
                    "workspaceMaxResults": 20000
                }
            });

            let config = crate::backend::parse_symbol_config(&settings);
            assert!(
                config.is_some(),
                "Should return Some when symbols section exists"
            );
            let config = config.unwrap();
            assert_eq!(
                config.workspace_max_results,
                SymbolConfig::MAX_WORKSPACE_MAX_RESULTS,
                "Values above maximum should be clamped to 10000"
            );
        }

        /// Test that boundary values are accepted
        /// **Validates: Requirements 11.3**
        #[test]
        fn test_boundary_values_accepted() {
            // Test minimum boundary
            let settings = json!({
                "symbols": {
                    "workspaceMaxResults": 100
                }
            });
            let config = crate::backend::parse_symbol_config(&settings).unwrap();
            assert_eq!(
                config.workspace_max_results, 100,
                "Minimum boundary should be accepted"
            );

            // Test maximum boundary
            let settings = json!({
                "symbols": {
                    "workspaceMaxResults": 10000
                }
            });
            let config = crate::backend::parse_symbol_config(&settings).unwrap();
            assert_eq!(
                config.workspace_max_results, 10000,
                "Maximum boundary should be accepted"
            );
        }

        /// Test that default value is 1000
        /// **Validates: Requirements 11.1**
        #[test]
        fn test_default_value_is_1000() {
            assert_eq!(
                SymbolConfig::DEFAULT_WORKSPACE_MAX_RESULTS,
                1000,
                "Default value should be 1000"
            );
            assert_eq!(
                SymbolConfig::default().workspace_max_results,
                1000,
                "Default config should have workspace_max_results = 1000"
            );
        }
    }

    // ============================================================================
    // Unit Tests for Indentation Style Configuration Parsing
    // **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    // ============================================================================
    mod indentation_config_parsing {
        use crate::indentation::IndentationStyle;
        use crate::state::IndentationSettings;
        use serde_json::json;

        /// Test that missing indentation section defaults to RStudio style
        /// **Validates: Requirements 7.4**
        #[test]
        fn test_missing_indentation_section_defaults_to_rstudio() {
            let settings = json!({
                "crossFile": {}
            });
            let config = crate::backend::parse_indentation_config(&settings);
            assert!(
                config.is_none(),
                "Should return None when indentation section is absent"
            );
            assert_eq!(
                IndentationSettings::default().style,
                IndentationStyle::RStudio,
                "Should default to RStudio style when indentation section is absent"
            );
        }

        /// Test that empty indentation section defaults to RStudio style
        /// **Validates: Requirements 7.4**
        #[test]
        fn test_empty_indentation_section_defaults_to_rstudio() {
            let settings = json!({
                "indentation": {}
            });
            let config = crate::backend::parse_indentation_config(&settings);
            assert!(config.is_some(), "Configuration parsing should succeed");
            let config = config.unwrap();
            assert_eq!(
                config.style,
                IndentationStyle::RStudio,
                "Should default to RStudio style when style key is absent"
            );
        }

        /// Test that "rstudio" value is parsed correctly
        /// **Validates: Requirements 7.1, 7.2**
        #[test]
        fn test_parse_rstudio_style() {
            let settings = json!({
                "indentation": {
                    "style": "rstudio"
                }
            });
            let config = crate::backend::parse_indentation_config(&settings);
            assert!(config.is_some(), "Configuration parsing should succeed");
            let config = config.unwrap();
            assert_eq!(
                config.style,
                IndentationStyle::RStudio,
                "Should parse 'rstudio' as RStudio style"
            );
        }

        /// Test that "rstudio-minus" value is parsed correctly
        /// **Validates: Requirements 7.1, 7.3**
        #[test]
        fn test_parse_rstudio_minus_style() {
            let settings = json!({
                "indentation": {
                    "style": "rstudio-minus"
                }
            });
            let config = crate::backend::parse_indentation_config(&settings);
            assert!(config.is_some(), "Configuration parsing should succeed");
            let config = config.unwrap();
            assert_eq!(
                config.style,
                IndentationStyle::RStudioMinus,
                "Should parse 'rstudio-minus' as RStudioMinus style"
            );
        }

        /// Test that invalid style value defaults to RStudio
        /// **Validates: Requirements 7.4**
        #[test]
        fn test_invalid_style_defaults_to_rstudio() {
            let settings = json!({
                "indentation": {
                    "style": "invalid-style"
                }
            });
            let config = crate::backend::parse_indentation_config(&settings);
            assert!(config.is_some(), "Configuration parsing should succeed");
            let config = config.unwrap();
            assert_eq!(
                config.style,
                IndentationStyle::RStudio,
                "Should default to RStudio style for invalid values"
            );
        }

        /// Test that style parsing is case-insensitive
        /// **Validates: Requirements 7.1**
        #[test]
        fn test_style_parsing_case_insensitive() {
            // Test uppercase
            let settings = json!({
                "crossFile": {},
                "indentation": {
                    "style": "RSTUDIO"
                }
            });
            let config = crate::backend::parse_indentation_config(&settings).unwrap();
            assert_eq!(
                config.style,
                IndentationStyle::RStudio,
                "Should parse 'RSTUDIO' as RStudio style"
            );

            // Test mixed case
            let settings = json!({
                "crossFile": {},
                "indentation": {
                    "style": "RStudio-Minus"
                }
            });
            let config = crate::backend::parse_indentation_config(&settings).unwrap();
            assert_eq!(
                config.style,
                IndentationStyle::RStudioMinus,
                "Should parse 'RStudio-Minus' as RStudioMinus style"
            );
        }
    }
}

#[cfg(test)]
mod refresh_packages_tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;

    /// Minimal stub `LanguageServer` and a no-R `Backend`, shared by the
    /// submodules below that drive `Backend` methods directly. Fully qualified so
    /// it needs no extra `use` imports at this module level.
    struct StubServer;
    #[tower_lsp::async_trait]
    impl tower_lsp::LanguageServer for StubServer {
        async fn initialize(
            &self,
            _: tower_lsp::lsp_types::InitializeParams,
        ) -> tower_lsp::jsonrpc::Result<tower_lsp::lsp_types::InitializeResult> {
            Ok(tower_lsp::lsp_types::InitializeResult::default())
        }
        async fn shutdown(&self) -> tower_lsp::jsonrpc::Result<()> {
            Ok(())
        }
    }

    fn make_test_backend() -> Arc<Backend> {
        let (client_tx, client_rx) = std::sync::mpsc::channel();
        let (_service, _socket) = tower_lsp::LspService::new(|client| {
            client_tx.send(client).expect("capture client");
            StubServer
        });
        let client = client_rx.recv().expect("client captured");
        Arc::new(Backend::new_with_request_cancellation(
            client,
            Arc::new(RequestCancellationRegistry::new()),
        ))
    }

    #[tokio::test]
    async fn refresh_clears_package_cache_and_returns_count() {
        use crate::package_library::{PackageInfo, PackageLibrary};
        let lib = Arc::new(PackageLibrary::new_empty());
        lib.insert_package(PackageInfo::new("foo".into(), HashSet::new()))
            .await;
        lib.insert_package(PackageInfo::new("bar".into(), HashSet::new()))
            .await;
        assert_eq!(lib.cached_count().await, 2);

        let cleared = refresh_packages_command_body(&lib, &[]).await;
        assert_eq!(cleared, 2);
        assert_eq!(lib.cached_count().await, 0);
    }

    /// Regression for the issue #429 editor/CLI parity gap: the open-document
    /// warm set must include packages named in `data(..., package = "pkg")` —
    /// not just `library()`/`require()` loads — so the diagnostics-time `data()`
    /// alias expansion can resolve those packages' dataset objects in the editor
    /// (matching what `raven check` warms via `doc.data_packages`). Without the
    /// fix, `survey` is absent from the warm set and `apiclus1`-style objects
    /// flag as undefined. Deterministic: pure set composition, no fs or R.
    #[test]
    fn open_document_warm_set_includes_data_package_args() {
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::Url;

        let mut state = WorldState::new();
        state.documents.insert(
            Url::from_file_path("/work/a.R").unwrap(),
            Document::new("library(dplyr)\ndata(api, package = \"survey\")\n", Some(1)),
        );

        let (pkgs, docs) = collect_open_document_packages(&state);

        assert!(
            pkgs.contains("dplyr"),
            "library() package must be in the warm set"
        );
        assert!(
            pkgs.contains("survey"),
            "data(package = \"survey\") package must be in the warm set (issue #429 editor/CLI parity); got {:?}",
            pkgs
        );
        assert_eq!(docs.len(), 1, "one open document → one scope-probe seed");
    }

    /// Regression for the issue #429 did_change parity gap: the live-editing
    /// warm path must include packages named in `data(..., package = "pkg")`,
    /// not just `library()`/`require()` loads, so the diagnostics-time `data()`
    /// alias expansion resolves their dataset objects as the user types
    /// (parity with did_open and `raven check`). Without the fix, a freshly
    /// typed `data(x, package = "pkg")` leaves its dataset objects flagged
    /// undefined until the file is reopened. Deterministic: pure composition,
    /// no `LibraryCall` construction needed (empty library_calls isolates the
    /// data_packages contribution).
    #[test]
    fn edited_document_warm_set_includes_data_package_args() {
        use crate::state::Document;

        let doc = Document::new("data(api, package = \"survey\")\n", Some(1));
        let pkgs = edited_document_warm_packages(&[], &doc);

        assert!(
            pkgs.contains(&"survey".to_string()),
            "did_change warm set must include data(package=) packages (issue #429); got {:?}",
            pkgs
        );
    }

    #[test]
    fn namespace_warm_packages_validates_and_dedups() {
        let mut meta = crate::cross_file::CrossFileMetadata::default();
        let mk = |pkg: &str| crate::cross_file::source_detect::NamespaceReference {
            package: pkg.to_string(),
            package_range: crate::cross_file::source_detect::SourceRange {
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            },
            member: None,
            internal: false,
            range: crate::cross_file::source_detect::SourceRange {
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            },
        };
        meta.namespace_references = vec![
            mk("dplyr"),
            mk("dplyr"),                                   // dup
            mk(crate::package_library::LOAD_ALL_SENTINEL), // sentinel filtered
            mk("not a package"),                           // invalid filtered
        ];
        let pkgs = namespace_warm_packages(&meta);
        assert_eq!(pkgs, vec!["dplyr".to_string()]);
    }

    #[test]
    fn open_docs_referencing_packages_finds_namespace_users() {
        use crate::state::{Document, WorldState};
        let mut state = WorldState::new();
        let a = Url::parse("file:///ws/a.R").unwrap();
        let b = Url::parse("file:///ws/b.R").unwrap();
        let c = Url::parse("file:///ws/c.R").unwrap();
        // a.R warms `dplyr` via library(); b.R references it via `dplyr::`;
        // c.R references an unrelated package.
        state
            .documents
            .insert(a.clone(), Document::new("library(dplyr)\n", Some(1)));
        state
            .documents
            .insert(b.clone(), Document::new("dplyr::mutate(x)\n", Some(1)));
        state
            .documents
            .insert(c.clone(), Document::new("tidyr::pivot(x)\n", Some(1)));

        let warmed: std::collections::HashSet<String> =
            std::iter::once("dplyr".to_string()).collect();
        let found = open_docs_referencing_packages(&state, &warmed);
        assert!(found.contains(&b), "b.R references dplyr::; got {found:?}");
        assert!(
            !found.contains(&c),
            "c.R references only tidyr::; got {found:?}"
        );
        // a.R has no namespace reference (only a library() call).
        assert!(!found.contains(&a), "a.R has no pkg:: ref; got {found:?}");
    }

    /// Deterministic (R-free) coverage of the issue #429 sysdata fallback gate
    /// `sysdata_r_fallback_needed`, which is otherwise only touched via an
    /// R-gated acceptance test. The predicate fires iff the AST scan found no
    /// sysdata names AND a binary `R/sysdata.rda` exists on disk; this exercises
    /// the true branch and both false branches.
    #[test]
    fn sysdata_r_fallback_needed_gate_branches() {
        use crate::state::WorldState;
        use std::collections::BTreeSet;
        use tempfile::tempdir;

        let tmp = tempdir().unwrap();
        let r_dir = tmp.path().join("R");
        std::fs::create_dir_all(&r_dir).unwrap();

        let mut state = WorldState::new();
        state.package_inputs.workspace_root = Some(tmp.path().to_path_buf());

        // Empty sysdata names but no R/sysdata.rda yet → no fallback.
        state.package_inputs.sysdata_names = BTreeSet::new();
        assert!(
            !sysdata_r_fallback_needed(&state),
            "no R/sysdata.rda on disk → fallback must not fire"
        );

        // Create R/sysdata.rda → empty names + rda present → fallback fires.
        std::fs::write(r_dir.join("sysdata.rda"), b"").unwrap();
        assert!(
            sysdata_r_fallback_needed(&state),
            "empty sysdata names + R/sysdata.rda present → fallback must fire"
        );

        // AST scan already found sysdata names → no R fallback even with the rda.
        state.package_inputs.sysdata_names = BTreeSet::from(["internal_obj".to_string()]);
        assert!(
            !sysdata_r_fallback_needed(&state),
            "non-empty sysdata names → fallback must not fire (AST scan sufficed)"
        );
    }

    /// Regression for review finding #1: `raven.refreshPackages` must
    /// re-discover `.libPaths()`. `rebuild_package_library` reads
    /// `cross_file_config.packages_additional_library_paths` from state, so
    /// mutating those paths between calls must produce a PackageLibrary
    /// whose `lib_paths()` reflects the new configuration.
    #[tokio::test]
    async fn rebuild_picks_up_additional_library_paths_change() {
        use crate::state::WorldState;
        use tempfile::tempdir;
        use tokio::sync::RwLock;

        let t_old = tempdir().unwrap();
        let t_new = tempdir().unwrap();

        let state = Arc::new(RwLock::new(WorldState::new()));

        // Pretend the user started with only `t_old` on the path, then
        // changed `.libPaths()` mid-session so `t_new` is now also present.
        {
            let mut s = state.write().await;
            s.cross_file_config.packages_enabled = true;
            s.cross_file_config.packages_additional_library_paths =
                vec![t_old.path().to_path_buf()];
        }
        let (lib_v1, _ready_v1) = rebuild_package_library(&state).await;
        assert!(
            lib_v1.lib_paths().iter().any(|p| p == t_old.path()),
            "initial library should include t_old ({}), got {:?}",
            t_old.path().display(),
            lib_v1.lib_paths()
        );
        assert!(
            !lib_v1.lib_paths().iter().any(|p| p == t_new.path()),
            "initial library must not contain t_new yet, got {:?}",
            lib_v1.lib_paths()
        );

        {
            let mut s = state.write().await;
            s.cross_file_config.packages_additional_library_paths =
                vec![t_old.path().to_path_buf(), t_new.path().to_path_buf()];
        }
        let (lib_v2, _ready_v2) = rebuild_package_library(&state).await;
        assert!(
            lib_v2.lib_paths().iter().any(|p| p == t_new.path()),
            "rebuilt library must include t_new after config change, got {:?}",
            lib_v2.lib_paths()
        );
    }

    /// Regression: when `raven.refreshPackages` rebuilds the `PackageLibrary`,
    /// the user-visible "cleared N entries" count must reflect the *old*
    /// library's pre-rebuild size rather than `refresh_packages_command_body`'s
    /// internal `before - after` (which is computed against the new empty
    /// library and would always be 0). Mirrors the cleared-count math in
    /// `Backend::execute_command("raven.refreshPackages")`.
    #[tokio::test]
    async fn cleared_count_reflects_pre_rebuild_library_size() {
        use crate::package_library::{PackageInfo, PackageLibrary};
        // Old library populated with three entries — represents the cache
        // state right before the user invokes "Raven: Refresh package cache".
        let old_lib = Arc::new(PackageLibrary::new_empty());
        for name in ["foo", "bar", "baz"] {
            old_lib
                .insert_package(PackageInfo::new(name.into(), HashSet::new()))
                .await;
        }
        let before_count = old_lib.cached_count().await;
        assert_eq!(before_count, 3);

        // Simulate `rebuild_package_library` swapping in a fresh empty
        // library: from this point the command operates on `new_lib`, which
        // starts at 0 entries.
        let new_lib = Arc::new(PackageLibrary::new_empty());

        // Run the standard refresh body on the new library (no prefetch
        // candidates because the test fixture has no documents). This is
        // exactly what `execute_command` does after the swap.
        let body_cleared = refresh_packages_command_body(&new_lib, &[]).await;
        let after_count = new_lib.cached_count().await;
        let cleared_user_visible = before_count.saturating_sub(after_count);

        // The function's own return is meaningless after a rebuild — it
        // sees an empty library and reports 0 evicted.
        assert_eq!(
            body_cleared, 0,
            "refresh_packages_command_body operates on the post-rebuild library, so its return is 0",
        );
        // The user-visible delta must reflect the three entries that vanished
        // when the library was replaced.
        assert_eq!(
            cleared_user_visible, 3,
            "user-visible cleared count must reflect pre-rebuild size",
        );
    }

    /// Regression for review finding #1: when packages are disabled, rebuild
    /// yields an empty library and does not attempt R discovery.
    #[tokio::test]
    async fn rebuild_returns_empty_library_when_packages_disabled() {
        use crate::state::WorldState;
        use tokio::sync::RwLock;

        let state = Arc::new(RwLock::new(WorldState::new()));
        {
            let mut s = state.write().await;
            s.cross_file_config.packages_enabled = false;
        }
        let (lib, ready) = rebuild_package_library(&state).await;
        assert!(!ready);
        assert!(lib.lib_paths().is_empty());
    }

    /// Regression for review finding #3: a `Dropped` event must leave open
    /// documents in a state where the next diagnostic run can republish at
    /// the same document version (force-republish bypasses the monotonic
    /// gate) and must clear the package cache.
    #[tokio::test]
    async fn prepare_dropped_recovery_clears_cache_and_marks_open_docs() {
        use crate::package_library::{PackageInfo, PackageLibrary};
        use crate::state::{Document, WorldState};
        use tokio::sync::RwLock;

        let mut world = WorldState::new();
        let uri_a = Url::parse("file:///workspace/a.R").unwrap();
        let uri_b = Url::parse("file:///workspace/b.R").unwrap();
        world
            .documents
            .insert(uri_a.clone(), Document::new("x <- 1", Some(1)));
        world
            .documents
            .insert(uri_b.clone(), Document::new("y <- 2", Some(1)));

        // Simulate prior publishes at version 1 for both docs.
        world.diagnostics_gate.record_publish(&uri_a, 1);
        world.diagnostics_gate.record_publish(&uri_b, 1);
        // Without force-republish, the gate blocks same-version republishes.
        assert!(!world.diagnostics_gate.can_publish(&uri_a, 1));
        assert!(!world.diagnostics_gate.can_publish(&uri_b, 1));

        // Seed the package cache so we can observe it being cleared.
        let lib = Arc::new(PackageLibrary::new_empty());
        lib.insert_package(PackageInfo::new("foo".into(), HashSet::new()))
            .await;
        world.package_library = lib.clone();

        let state = Arc::new(RwLock::new(world));
        let open = prepare_dropped_recovery(&state).await;

        // Cache cleared.
        assert_eq!(lib.cached_count().await, 0);

        // All open URIs returned.
        let open_set: HashSet<Url> = open.into_iter().collect();
        assert!(open_set.contains(&uri_a));
        assert!(open_set.contains(&uri_b));

        // Same-version republish is now allowed for both documents.
        let state_guard = state.read().await;
        assert!(
            state_guard.diagnostics_gate.can_publish(&uri_a, 1),
            "force-republish must allow the same-version republish for uri_a"
        );
        assert!(
            state_guard.diagnostics_gate.can_publish(&uri_b, 1),
            "force-republish must allow the same-version republish for uri_b"
        );
    }

    /// Regression for issue #107: `build_package_scope_snapshot` must include
    /// closed parent files (reachable via the dependency graph) in the
    /// artifacts/metadata maps, not just open documents. Otherwise inherited
    /// packages from parent `source()` chains are missed during prefetch.
    #[tokio::test]
    async fn snapshot_includes_closed_parent_in_neighborhood() {
        use crate::state::{Document, WorldState};
        use crate::workspace_index::IndexEntry;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let parent_path = temp_dir.path().join("parent.R");
        let child_path = temp_dir.path().join("child.R");
        let parent_code = "library(dplyr)\nhelper <- function(x) x";
        let child_code = "source('parent.R')\nx <- 1";
        std::fs::write(&parent_path, parent_code).unwrap();
        std::fs::write(&child_path, child_code).unwrap();

        let parent_uri = Url::from_file_path(&parent_path).unwrap();
        let child_uri = Url::from_file_path(&child_path).unwrap();
        let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();

        let mut world = WorldState::new();
        world.workspace_folders.push(workspace_root.clone());

        // Parent is a CLOSED file in the workspace index with library(dplyr)
        let parent_meta = crate::cross_file::extract_metadata(parent_code);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        let parent_tree = parser.parse(parent_code, None).unwrap();
        let parent_artifacts = Arc::new(crate::cross_file::scope::compute_artifacts_with_metadata(
            &parent_uri,
            &parent_tree,
            parent_code,
            Some(&parent_meta),
        ));
        let parent_entry = IndexEntry {
            contents: ropey::Rope::from_str(parent_code),
            tree: Some(parent_tree),
            loaded_packages: vec!["dplyr".into()],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 0,
                content_hash: None,
            },
            metadata: Arc::new(parent_meta),
            artifacts: parent_artifacts,
            indexed_at_version: 0,
        };
        world
            .workspace_index_new
            .insert(parent_uri.clone(), parent_entry);

        // Child is an OPEN document that sources the parent
        let child_meta = crate::cross_file::extract_metadata(child_code);
        world
            .documents
            .insert(child_uri.clone(), Document::new(child_code, Some(1)));

        // Add forward edge: child -> parent
        world
            .cross_file_graph
            .update_file(&child_uri, &child_meta, Some(&workspace_root), |_| None);

        // Build snapshot for the child only
        let docs = vec![(child_uri.clone(), 1u32)];
        let snapshot = world.build_package_scope_snapshot(&docs);

        // The parent must be in the snapshot's artifacts_map even though
        // it's not an open document — it's reachable via the dependency graph.
        assert!(
            snapshot.artifacts_map.contains_key(&parent_uri),
            "closed parent must be included in snapshot artifacts_map; \
             keys: {:?}",
            snapshot.artifacts_map.keys().collect::<Vec<_>>()
        );
        assert!(
            snapshot.metadata_map.contains_key(&parent_uri),
            "closed parent must be included in snapshot metadata_map"
        );

        // Verify scope resolution using the snapshot discovers inherited packages.
        let get_artifacts = |u: &Url| snapshot.artifacts_map.get(u).cloned();
        let get_metadata = |u: &Url| snapshot.metadata_map.get(u).cloned();

        // Debug: check child artifacts have source() in timeline
        let child_arts = snapshot.artifacts_map.get(&child_uri);
        assert!(
            child_arts.is_some(),
            "child must have artifacts in snapshot"
        );
        let child_arts = child_arts.unwrap();
        let has_source_event = child_arts
            .timeline
            .iter()
            .any(|e| matches!(e, crate::cross_file::scope::ScopeEvent::Source { .. }));
        assert!(
            has_source_event,
            "child artifacts must have Source event in timeline; timeline has {} events",
            child_arts.timeline.len()
        );

        // Debug: check graph has forward edge from child to parent
        let child_deps = snapshot.graph.get_dependencies(&child_uri);
        assert!(
            !child_deps.is_empty(),
            "child must have forward dependencies in snapshot graph"
        );
        // Debug: check edge call_site matches source event
        for dep in &child_deps {
            assert_eq!(dep.to, parent_uri, "edge must point to parent");
        }

        let empty_base: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Debug: check parent artifacts have PackageLoad event
        let parent_arts = snapshot.artifacts_map.get(&parent_uri).unwrap();
        assert!(
            parent_arts
                .timeline
                .iter()
                .any(|e| { matches!(e, crate::cross_file::scope::ScopeEvent::PackageLoad { .. }) }),
            "parent must have PackageLoad event"
        );
        let scope = crate::cross_file::scope::scope_at_position_with_graph(
            &child_uri,
            1,
            u32::MAX,
            &get_artifacts,
            &get_metadata,
            &snapshot.graph,
            snapshot.workspace_folder.as_ref(),
            snapshot.max_chain_depth,
            &empty_base,
            false,
            snapshot.backward_dependencies,
            &|| false,
            Some(&snapshot.scope_contribution),
            None,
        );
        // dplyr appears in loaded_packages (from forward source() chain),
        // not inherited_packages (which come from backward/parent edges).
        let all_packages: std::collections::HashSet<&String> = scope
            .inherited_packages
            .iter()
            .chain(scope.loaded_packages.iter())
            .collect();
        assert!(
            all_packages.contains(&"dplyr".to_string()),
            "scope must include 'dplyr' from closed parent; \
             inherited={:?}, loaded={:?}",
            scope.inherited_packages,
            scope.loaded_packages
        );
    }

    #[test]
    fn package_scope_snapshot_carries_package_contribution() {
        use crate::package_state::{
            ContentDigest, DescriptionInput, PackageInputDelta, RFileInput, RFileKind,
        };
        use crate::state::{Document, WorldState};
        use std::path::PathBuf;

        let root = PathBuf::from("/work/pkg");
        let uri = Url::from_file_path(root.join("R").join("main.R")).unwrap();
        let mut world = WorldState::new();
        world
            .documents
            .insert(uri.clone(), Document::new("helper()\n", Some(1)));
        world.package_inputs.workspace_root = Some(root.clone());
        world.package_inputs.description = Some(DescriptionInput {
            text: "Package: pkg\n".into(),
        });
        let text: Arc<str> = "helper <- function() 1\n".into();
        world.package_inputs.r_files.insert(
            root.join("R").join("helper.R"),
            RFileInput {
                kind: RFileKind::Source,
                content_digest: ContentDigest::of(&text),
                text,
            },
        );
        world.apply_package_event(&PackageInputDelta::Initial);

        let snapshot = world.build_package_scope_snapshot(&[(uri, 0)]);

        assert!(
            snapshot
                .scope_contribution
                .r_internal_symbols
                .contains("helper"),
            "background scope probes must carry package-internal symbols"
        );
    }

    /// Regression for issue #106: after `rebuild_package_library` swaps in a
    /// fresh (empty-cache) `PackageLibrary`, calling
    /// `prefetch_packages_for_open_documents` must warm the cache so
    /// diagnostics don't flash false-positive "unknown function" errors.
    #[tokio::test]
    async fn prefetch_warms_cache_after_rebuild() {
        use crate::r_subprocess::RSubprocess;
        use crate::state::{Document, WorldState};
        use tokio::sync::RwLock;

        // Skip if R is not available on this machine.
        let Some(r_subprocess) = RSubprocess::new(None) else {
            return;
        };

        // Build a real PackageLibrary with R subprocess.
        let mut lib = crate::package_library::PackageLibrary::with_subprocess(Some(r_subprocess));
        if lib.initialize().await.is_err() || lib.lib_paths().is_empty() {
            return; // R available but initialization failed
        }
        let pkg_lib = Arc::new(lib);

        // Set up a WorldState with an open document that uses `library(stats)`.
        let mut world = WorldState::new();
        world.package_library = pkg_lib.clone();
        world.package_library_ready = true;
        let uri = Url::parse("file:///workspace/test.R").unwrap();
        world.documents.insert(
            uri.clone(),
            Document::new("library(stats)\nmean(c(1,2,3))", Some(1)),
        );
        let state = Arc::new(RwLock::new(world));

        // Prefetch should warm the cache for "stats".
        prefetch_packages_for_open_documents(&state, &pkg_lib).await;
        assert!(
            pkg_lib.is_cached("stats").await,
            "stats must be cached after prefetch"
        );

        // Simulate a rebuild: clear the cache, confirm it's empty, then
        // re-prefetch and confirm it's warm again.
        pkg_lib.clear_cache().await;
        assert!(
            !pkg_lib.is_cached("stats").await,
            "stats must not be cached after clear"
        );
        prefetch_packages_for_open_documents(&state, &pkg_lib).await;
        assert!(
            pkg_lib.is_cached("stats").await,
            "stats must be re-cached after second prefetch"
        );
    }

    /// Regression: in package mode, NAMESPACE `import(pkg)` whole-package
    /// imports (`scope_contribution.full_imports`) must be prefetched
    /// alongside `library()` attaches and inherited packages. Without the
    /// warm-up, value-position references to a full import's exports emit
    /// false "Undefined variable" diagnostics (the "pending" heuristic only
    /// suppresses call-position uses) until something else warms the cache.
    /// R-free: the fake installed package is parsed statically from a temp
    /// library directory.
    #[tokio::test]
    async fn prefetch_covers_namespace_full_imports() {
        use crate::package_state::{DescriptionInput, NamespaceInput, PackageInputDelta};
        use crate::state::{Document, WorldState};
        use tokio::sync::RwLock;

        let libdir = tempfile::tempdir().unwrap();
        let pkgdir = libdir.path().join("fakepkg");
        std::fs::create_dir_all(&pkgdir).unwrap();
        std::fs::write(
            pkgdir.join("DESCRIPTION"),
            "Package: fakepkg\nVersion: 1.0\n",
        )
        .unwrap();
        std::fs::write(pkgdir.join("NAMESPACE"), "export(real_export)\n").unwrap();

        let mut lib = crate::package_library::PackageLibrary::with_subprocess(None);
        lib.set_lib_paths(vec![libdir.path().to_path_buf()]);
        let pkg_lib = Arc::new(lib);

        // Package-mode workspace whose NAMESPACE fully imports fakepkg; the
        // open document attaches nothing itself, so only `full_imports` can
        // drive the prefetch.
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_path_buf();
        let uri = Url::from_file_path(root_path.join("R").join("uses.R")).unwrap();
        let mut world = WorldState::new();
        world.package_library = pkg_lib.clone();
        world.package_library_ready = true;
        world.documents.insert(
            uri.clone(),
            Document::new("f <- function(x) identical(x, real_export)\n", Some(1)),
        );
        world.package_inputs.workspace_root = Some(root_path);
        world.package_inputs.description = Some(DescriptionInput {
            text: "Package: testpkg\nVersion: 1.0\nImports: fakepkg\n".into(),
        });
        world.package_inputs.namespace = Some(NamespaceInput {
            text: "import(fakepkg)\n".into(),
        });
        world.apply_package_event(&PackageInputDelta::Initial);
        assert!(
            world
                .package_state
                .scope_contribution()
                .full_imports
                .contains("fakepkg"),
            "fixture must derive fakepkg as a full import"
        );
        let state = Arc::new(RwLock::new(world));

        prefetch_packages_for_open_documents(&state, &pkg_lib).await;
        assert!(
            pkg_lib.is_cached("fakepkg").await,
            "NAMESPACE import(fakepkg) must be prefetched for open documents"
        );
    }

    /// Integration test for issue #106: exercises the same sequence as
    /// `did_change_configuration`'s `package_settings_changed` branch:
    ///   rebuild_package_library → swap into state → prefetch → verify warm.
    /// Uses MASS (a recommended package that ships with R but is NOT a base
    /// package) so it starts uncached after rebuild — exactly the scenario
    /// that caused transient false-positive diagnostics.
    #[tokio::test]
    async fn settings_change_rebuild_then_prefetch_warms_cache() {
        use crate::r_subprocess::RSubprocess;
        use crate::state::{Document, WorldState};
        use tokio::sync::RwLock;

        let Some(_) = RSubprocess::new(None) else {
            return;
        };

        // 1. Set up state with an open document using library(MASS).
        let mut world = WorldState::new();
        world.cross_file_config.packages_enabled = true;
        let uri = Url::parse("file:///workspace/analysis.R").unwrap();
        world.documents.insert(
            uri.clone(),
            Document::new("library(MASS)\nstepAIC(model)", Some(1)),
        );
        let state = Arc::new(RwLock::new(world));

        // 2. Initial rebuild (simulates initialization).
        let (lib_v1, ready_v1) = rebuild_package_library(&state).await;
        if !ready_v1 {
            return; // R available but lib_paths empty
        }
        // MASS is recommended (ships with R) but not base — skip if missing.
        if lib_v1.find_package_directory("MASS").is_none() {
            return;
        }
        {
            let mut s = state.write().await;
            s.package_library = lib_v1.clone();
            s.package_library_ready = true;
        }
        prefetch_packages_for_open_documents(&state, &lib_v1).await;
        assert!(
            lib_v1.is_cached("MASS").await,
            "MASS must be cached after initial prefetch"
        );

        // 3. Simulate did_change_configuration: rebuild swaps in a fresh library.
        //    Base packages are pre-cached by initialize(), but MASS is NOT base,
        //    so it starts uncached — this is the false-positive window.
        let (lib_v2, ready_v2) = rebuild_package_library(&state).await;
        assert!(ready_v2);
        assert!(
            !lib_v2.is_cached("MASS").await,
            "MASS (non-base) must NOT be cached in fresh library"
        );
        {
            let mut s = state.write().await;
            s.package_library = lib_v2.clone();
            s.package_library_ready = true;
        }

        // 4. Prefetch (the fix from issue #106).
        prefetch_packages_for_open_documents(&state, &lib_v2).await;

        // 5. Cache must be warm BEFORE diagnostics would run.
        assert!(
            lib_v2.is_cached("MASS").await,
            "MASS must be cached after settings-change rebuild + prefetch"
        );
    }

    /// Regression: when the background workspace scan completes and applies
    /// the dependency graph, the post-scan path must call
    /// `prefetch_packages_for_open_documents` BEFORE force-republishing
    /// open documents. Otherwise packages newly visible via inherited scope
    /// from now-indexed parent files stay uncached forever (no later trigger
    /// caches them), and `package_cache_pending` permanently silences
    /// undefined-variable diagnostics for unrelated uninstalled packages.
    ///
    /// Real-world reproduction (worldwide repo): `data.r` has
    /// `library(lme4); lmer()` (lme4 not installed). It is sourced by
    /// `main.r`, which sources `functions.r`, which loads ~20 packages.
    /// After workspace scan, those 20 packages flow into `data.r`'s
    /// inherited scope. Pre-fix, did_open prefetched only `lme4` (no
    /// backward edges yet); apply_workspace_index then ran a republish
    /// without re-prefetching, so the 20 inherited packages were
    /// `is_cached_sync = false` AND `package_exists = true` → pending →
    /// `lmer()` was suppressed permanently.
    #[tokio::test]
    async fn workspace_scan_completion_prefetches_packages_from_closed_parent_chain() {
        use crate::r_subprocess::RSubprocess;
        use crate::state::{Document, WorldState};
        use crate::workspace_index::IndexEntry;
        use tempfile::TempDir;
        use tokio::sync::RwLock;

        // Skip if R is not available.
        let Some(r_subprocess) = RSubprocess::new(None) else {
            return;
        };
        let mut lib = crate::package_library::PackageLibrary::with_subprocess(Some(r_subprocess));
        if lib.initialize().await.is_err() || lib.lib_paths().is_empty() {
            return;
        }
        // MASS ships with R but is NOT base, so it starts uncached after
        // initialize() — exactly mirroring the user's inherited-package state.
        if lib.find_package_directory("MASS").is_none() {
            return;
        }
        let pkg_lib = Arc::new(lib);

        // parent.R is CLOSED (only in workspace_index_new); it loads MASS and
        // then sources child.R. child.R is the OPEN document.
        let temp_dir = TempDir::new().unwrap();
        let parent_path = temp_dir.path().join("parent.R");
        let child_path = temp_dir.path().join("child.R");
        let parent_code = "library(MASS)\nsource('child.R')\n";
        let child_code = "x <- 1\n";
        std::fs::write(&parent_path, parent_code).unwrap();
        std::fs::write(&child_path, child_code).unwrap();

        let parent_uri = Url::from_file_path(&parent_path).unwrap();
        let child_uri = Url::from_file_path(&child_path).unwrap();
        let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();

        let mut world = WorldState::new();
        world.workspace_folders.push(workspace_root);
        world.package_library = pkg_lib.clone();
        world.package_library_ready = true;
        world.cross_file_config.packages_enabled = true;

        // Open child.R only.
        world
            .documents
            .insert(child_uri.clone(), Document::new(child_code, Some(1)));

        // Build the closed parent IndexEntry.
        let parent_meta = crate::cross_file::extract_metadata(parent_code);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        let parent_tree = parser.parse(parent_code, None).unwrap();
        let parent_artifacts = Arc::new(crate::cross_file::scope::compute_artifacts_with_metadata(
            &parent_uri,
            &parent_tree,
            parent_code,
            Some(&parent_meta),
        ));
        let parent_entry = IndexEntry {
            contents: ropey::Rope::from_str(parent_code),
            tree: Some(parent_tree),
            loaded_packages: vec!["MASS".into()],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 0,
                content_hash: None,
            },
            metadata: Arc::new(parent_meta),
            artifacts: parent_artifacts,
            indexed_at_version: 0,
        };

        // Simulate workspace scan completion: insert closed parent and build
        // the dependency graph. This auto-detects the backward edge from the
        // perspective of child.R (parent.R sources it).
        let mut new_entries = std::collections::HashMap::new();
        new_entries.insert(parent_uri.clone(), parent_entry);
        world.apply_workspace_index(
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            new_entries,
        );

        // Sanity: MASS uncached before any prefetch.
        assert!(
            !pkg_lib.is_cached("MASS").await,
            "MASS must NOT be cached before post-scan prefetch"
        );

        let state = Arc::new(RwLock::new(world));

        // The fix: post-workspace-scan prefetch picks up MASS via the
        // newly-built backward edge (parent.R sources child.R, parent.R has
        // `library(MASS)` before that source() call, so MASS is inherited
        // into child.R's scope).
        prefetch_packages_for_open_documents(&state, &pkg_lib).await;

        assert!(
            pkg_lib.is_cached("MASS").await,
            "MASS must be cached after post-workspace-scan prefetch — \
             without this, package_cache_pending would permanently silence \
             undefined-variable diagnostics in child.R for unrelated \
             uninstalled packages"
        );
    }

    // ============================================================================
    // Regression: transitive (>=2-hop) forward source() resolution under the
    // on-demand `did_open` indexing walk (`index_forward_chain`).
    // ============================================================================
    mod forward_chain_inherited_wd {
        use super::make_test_backend;
        use tower_lsp::lsp_types::Url;

        /// A 2-hop forward `source()` chain where the consumer lives in a
        /// DIFFERENT directory than the sourced files (`scripts/` → `R/`), so
        /// each hop's path (`source("R/...")`) resolves only via the
        /// workspace-root fallback.
        ///
        /// The on-demand `index_forward_chain` walk used to propagate the
        /// consumer's effective working directory (`scripts/`) as the
        /// *inherited* working directory of every non-`chdir` forward child,
        /// even though no `# raven: cd` was in effect anywhere. That inherited
        /// WD both (a) re-based the deep hop's `source("R/leaf.R")` under
        /// `scripts/` and (b) suppressed the workspace-root fallback (see
        /// `resolve_path_impl`), so the dependency edge for `R/mid.R` pointed at
        /// the nonexistent `scripts/R/leaf.R` instead of the real `R/leaf.R`.
        /// The workspace scan (`build_dependency_graph_from_workspace`) never
        /// injects an inherited WD for non-directive sources, so it built the
        /// correct edge — hence the LSP-vs-CLI divergence this guards.
        #[tokio::test(flavor = "multi_thread")]
        async fn deep_forward_source_edge_uses_workspace_fallback_not_inherited_wd() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::write(
                root.join("DESCRIPTION"),
                "Package: probe\nTitle: T\nVersion: 0.1.0\n",
            )
            .unwrap();
            std::fs::create_dir_all(root.join("scripts")).unwrap();
            std::fs::create_dir_all(root.join("R")).unwrap();
            std::fs::write(
                root.join("scripts").join("use.R"),
                "source(\"R/mid.R\")\nleaf_fn(1)\n",
            )
            .unwrap();
            std::fs::write(root.join("R").join("mid.R"), "source(\"R/leaf.R\")\n").unwrap();
            std::fs::write(
                root.join("R").join("leaf.R"),
                "leaf_fn <- function(x) x + 1\n",
            )
            .unwrap();

            let ws_uri = Url::from_file_path(root).unwrap();
            let use_uri = Url::from_file_path(root.join("scripts").join("use.R")).unwrap();
            let mid_uri = Url::from_file_path(root.join("R").join("mid.R")).unwrap();
            let leaf_uri = Url::from_file_path(root.join("R").join("leaf.R")).unwrap();

            let backend = make_test_backend();
            backend.state.write().await.workspace_folders = vec![ws_uri.clone()];

            // Drive the exact on-demand walk `did_open` runs for the consumer.
            backend
                .index_forward_chain(&use_uri, 10, Some(&ws_uri))
                .await;

            let state = backend.state.read().await;

            // Hop 1: consumer -> mid (always worked; sanity check).
            let use_deps: Vec<_> = state
                .cross_file_graph
                .get_dependencies(&use_uri)
                .iter()
                .map(|e| e.to.clone())
                .collect();
            assert!(
                use_deps.contains(&mid_uri),
                "consumer should source R/mid.R; got {use_deps:?}"
            );

            // Hop 2: mid -> leaf. This is the regression. The edge MUST point at
            // the real R/leaf.R (resolved via the workspace-root fallback), not
            // at the consumer-dir-rebased scripts/R/leaf.R.
            let mid_deps: Vec<_> = state
                .cross_file_graph
                .get_dependencies(&mid_uri)
                .iter()
                .map(|e| e.to.clone())
                .collect();
            assert!(
                !mid_deps.is_empty(),
                "R/mid.R must have a resolved forward edge for source(\"R/leaf.R\")"
            );
            let to_path = mid_deps[0].to_file_path().unwrap();
            assert!(
                to_path.exists(),
                "deep edge target must be a real file, got {to_path:?} (consumer dir leaked in?)"
            );
            assert_eq!(
                mid_deps[0], leaf_uri,
                "R/mid.R's source(\"R/leaf.R\") must resolve to {leaf_uri} via the \
                 workspace-root fallback, not under the consumer's scripts/ dir; got {mid_deps:?}"
            );
        }

        /// Guard the cd-in-effect branch the deep-source fix gates on: when a
        /// `# raven: cd` IS active on the consumer, the on-demand walk MUST still
        /// propagate the resolved working directory to its non-`chdir` children
        /// (which also keeps the workspace-root fallback disabled under it), so a
        /// transitive `# raven: cd` chain keeps resolving relative to the cd dir.
        #[tokio::test(flavor = "multi_thread")]
        async fn cd_in_effect_still_propagates_inherited_wd_to_children() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir_all(root.join("data")).unwrap();
            // main.R changes wd to data/ then sources helper.R (resolved under
            // data/ thanks to the cd; NOT via the workspace-root fallback).
            std::fs::write(
                root.join("main.R"),
                "# raven: cd data\nsource(\"helper.R\")\n",
            )
            .unwrap();
            std::fs::write(root.join("data").join("helper.R"), "h <- function() 1\n").unwrap();

            let ws_uri = Url::from_file_path(root).unwrap();
            let main_uri = Url::from_file_path(root.join("main.R")).unwrap();
            let helper_uri = Url::from_file_path(root.join("data").join("helper.R")).unwrap();

            let backend = make_test_backend();
            backend.state.write().await.workspace_folders = vec![ws_uri.clone()];
            backend
                .index_forward_chain(&main_uri, 10, Some(&ws_uri))
                .await;

            let state = backend.state.read().await;
            let main_deps: Vec<_> = state
                .cross_file_graph
                .get_dependencies(&main_uri)
                .iter()
                .map(|e| e.to.clone())
                .collect();
            assert!(
                main_deps.contains(&helper_uri),
                "cd-resolved source must point at data/helper.R; got {main_deps:?}"
            );
            let helper_meta = state
                .get_enriched_metadata(&helper_uri)
                .expect("helper.R must be indexed by the walk");
            assert_eq!(
                helper_meta.inherited_working_directory.as_deref(),
                Some(root.join("data").to_string_lossy().as_ref()),
                "an in-effect `# raven: cd` must propagate as the child's inherited \
                 working directory"
            );
        }

        /// Exercise the `should_index` convergence arm: a child first indexed by a
        /// no-cd consumer (stored inherited WD = None) must be RE-indexed when a
        /// later `# raven: cd` consumer reaches the same file, so its stored
        /// inherited WD reflects the cd now in effect. This is the "diamond"
        /// shared-child case the value-based re-index check (`stored != computed`)
        /// was written for — the prior `cd_in_effect_*` test only hits the
        /// fresh-index (`!contains`) arm, never this one.
        #[tokio::test(flavor = "multi_thread")]
        async fn cd_walk_reindexes_shared_child_indexed_plain_first() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir_all(root.join("data")).unwrap();
            // A no-cd consumer that sources the shared file by its workspace path.
            std::fs::write(root.join("plain.R"), "source(\"data/shared.R\")\n").unwrap();
            // A cd consumer whose `# raven: cd data` resolves source("shared.R")
            // to the SAME data/shared.R file.
            std::fs::write(
                root.join("main.R"),
                "# raven: cd data\nsource(\"shared.R\")\n",
            )
            .unwrap();
            std::fs::write(root.join("data").join("shared.R"), "s <- function() 1\n").unwrap();

            let ws_uri = Url::from_file_path(root).unwrap();
            let plain_uri = Url::from_file_path(root.join("plain.R")).unwrap();
            let main_uri = Url::from_file_path(root.join("main.R")).unwrap();
            let shared_uri = Url::from_file_path(root.join("data").join("shared.R")).unwrap();

            let backend = make_test_backend();
            backend.state.write().await.workspace_folders = vec![ws_uri.clone()];

            // Pass 1 (no cd): shared.R is indexed fresh with NO inherited WD.
            backend
                .index_forward_chain(&plain_uri, 10, Some(&ws_uri))
                .await;
            {
                let state = backend.state.read().await;
                let shared_meta = state
                    .get_enriched_metadata(&shared_uri)
                    .expect("shared.R indexed by the plain (no-cd) walk");
                assert_eq!(
                    shared_meta.inherited_working_directory, None,
                    "a no-cd consumer must index the shared child with no inherited WD"
                );
            }

            // Pass 2 (cd in effect): shared.R is already indexed with a stale
            // `None`, while the cd walk computes `Some(data/)`. This drives the
            // `stored != computed` re-index arm, updating the stored WD.
            backend
                .index_forward_chain(&main_uri, 10, Some(&ws_uri))
                .await;
            {
                let state = backend.state.read().await;
                let shared_meta = state
                    .get_enriched_metadata(&shared_uri)
                    .expect("shared.R still indexed after the cd walk");
                assert_eq!(
                    shared_meta.inherited_working_directory.as_deref(),
                    Some(root.join("data").to_string_lossy().as_ref()),
                    "the cd walk must re-index the already-indexed shared child so its \
                     stored inherited working directory reflects the cd now in effect"
                );
            }
        }
    }

    // ============================================================================
    // Unit tests for raven.getHelpHtml execute_command handler.
    // ============================================================================
    mod get_help_html_command {
        use std::sync::Arc;
        use tower_lsp::LanguageServer;
        use tower_lsp::lsp_types::ExecuteCommandParams;

        use super::super::Backend;
        use super::make_test_backend;

        /// Build a `Backend` whose `packages_r_path` is left as `None` (the default,
        /// shared with the other test submodules). Thin alias for the call sites
        /// below that read better as "no R configured".
        fn make_backend_no_r() -> Arc<Backend> {
            make_test_backend()
        }

        /// Build a `Backend` and set `packages_r_path` to the given path.
        fn make_backend_with_r(r_path: std::path::PathBuf) -> Arc<Backend> {
            let backend = make_backend_no_r();
            // Set r_path synchronously — we need a tokio runtime for the write lock.
            // We use `tokio::runtime::Handle::current()` which is available inside
            // the `#[tokio::test]` harness.
            let state = Arc::clone(&backend.state);
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let mut guard = state.write().await;
                    guard.cross_file_config.packages_r_path = Some(r_path);
                })
            });
            backend
        }

        /// Helper: invoke `raven.getHelpHtml` with the given args and return the response value.
        async fn run_command(backend: &Backend, args: Vec<serde_json::Value>) -> serde_json::Value {
            let params = ExecuteCommandParams {
                command: "raven.getHelpHtml".into(),
                arguments: args,
                work_done_progress_params: Default::default(),
            };
            backend
                .execute_command(params)
                .await
                .expect("execute_command must not error")
                .expect("execute_command must return Some")
        }

        fn binary_available_on_path(binary: &str) -> bool {
            std::process::Command::new(binary)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn unset_r_path_falls_back_to_path_lookup() {
            // Regression for the smoke-test bug where leaving raven.packages.rPath
            // unset (the documented default — auto-detect) made the help panel
            // show "r-unavailable: R not configured (set raven.packages.rPath)"
            // even on machines with R on PATH. The handler must mirror the hover
            // path's fallback to PathBuf::from("R") so PATH-resolved R is used.
            //
            // Skip on hosts without R on PATH: the production fallback correctly
            // returns r-unavailable when `Command::new("R")` cannot spawn, so the
            // assertion below would fail spuriously on R-less CI runners. Probe
            // the same plain PATH lookup the handler uses, not broader R
            // autodiscovery locations.
            if !binary_available_on_path("R") {
                eprintln!("skip: no R on PATH");
                return;
            }
            let backend = make_backend_no_r();
            let resp = run_command(
                &backend,
                vec![serde_json::json!("mean"), serde_json::json!("base")],
            )
            .await;
            if resp["ok"] == false {
                assert_ne!(
                    resp["reason"], "r-unavailable",
                    "handler must not bail with r-unavailable when PATH could provide R; got: {resp:?}"
                );
                let msg = resp["message"].as_str().unwrap_or("");
                assert!(
                    !msg.contains("set raven.packages.rPath"),
                    "stale 'set raven.packages.rPath' wording leaked through: {resp:?}"
                );
            }
        }

        #[test]
        fn r_path_skip_probe_uses_plain_path_lookup() {
            let missing = "__raven_missing_r_binary_for_path_probe__";
            assert!(!binary_available_on_path(missing));
        }

        #[tokio::test]
        async fn invalid_topic_returns_invalid_topic_reason() {
            // Even with no R configured, validation runs first — so we get
            // "invalid-topic" rather than "r-unavailable".
            let backend = make_backend_no_r();
            let resp = run_command(
                &backend,
                vec![
                    serde_json::json!("with\nnewline"),
                    serde_json::json!("base"),
                ],
            )
            .await;
            assert_eq!(resp["ok"], false);
            assert_eq!(resp["reason"], "invalid-topic");
        }

        #[tokio::test]
        async fn invalid_package_returns_invalid_topic_reason() {
            let backend = make_backend_no_r();
            let resp = run_command(
                &backend,
                vec![serde_json::json!("mean"), serde_json::json!("bad package!")],
            )
            .await;
            assert_eq!(resp["ok"], false);
            assert_eq!(resp["reason"], "invalid-topic");
        }

        #[tokio::test]
        async fn missing_topic_arg_is_invalid() {
            // Empty args → topic defaults to "" which fails validation.
            let backend = make_backend_no_r();
            let resp = run_command(&backend, vec![]).await;
            assert_eq!(resp["ok"], false);
            assert_eq!(resp["reason"], "invalid-topic");
        }

        /// Cache hit is served without spawning R.
        ///
        /// Positive entries cached from an earlier session must be served
        /// straight from the LRU before the handler resolves an R path or
        /// spawns a subprocess. The cache probe runs after r_path resolution
        /// in the handler but before any fetch, so a hit short-circuits the
        /// PATH-fallback subprocess work.
        #[tokio::test]
        async fn cached_entry_served_without_r() {
            use crate::help::HelpHtml;
            use std::path::PathBuf;

            let backend = make_backend_no_r();

            // Pre-populate the cache with a synthetic positive entry.
            let synthetic = HelpHtml {
                topic: "synthetic".into(),
                package: "fake".into(),
                title: "Synthetic Help".into(),
                html: "<p>cached</p>".into(),
                help_dir: PathBuf::from("/fake/lib/fake/help"),
                lib_paths: vec![PathBuf::from("/fake/lib")],
            };
            {
                let state = backend.state.read().await;
                state
                    .html_help_cache
                    .insert("synthetic", Some("fake"), Ok(synthetic));
            }

            // With no r_path configured, a miss would return r-unavailable.
            // The cache probe must intercept first.
            let resp = run_command(
                &backend,
                vec![serde_json::json!("synthetic"), serde_json::json!("fake")],
            )
            .await;
            assert_eq!(resp["ok"], true, "expected cache hit, got: {resp:?}");
            assert_eq!(resp["topic"], "synthetic");
            assert_eq!(resp["package"], "fake");
            assert_eq!(resp["html"], "<p>cached</p>");
        }

        /// R-required happy-path test: renders `mean` from `base`.
        ///
        /// Skips automatically when no R binary is available on PATH,
        /// following the same `RSubprocess::new(None)` pattern used in
        /// `crates/raven/src/help/html.rs`.
        #[tokio::test(flavor = "multi_thread")]
        async fn happy_path_renders_base_mean_when_r_is_available() {
            let Some(r) = crate::r_subprocess::RSubprocess::new(None).map(|s| s.r_path().clone())
            else {
                eprintln!("skip: no R");
                return;
            };
            let backend = make_backend_with_r(r);
            let resp = run_command(
                &backend,
                vec![serde_json::json!("mean"), serde_json::json!("base")],
            )
            .await;
            assert_eq!(resp["ok"], true, "unexpected error: {resp:?}");
            assert_eq!(resp["package"], "base");
            assert!(
                resp["html"].as_str().is_some_and(|h| !h.is_empty()),
                "html must be non-empty"
            );
            assert!(
                resp["helpDir"].as_str().is_some_and(|s| !s.is_empty()),
                "helpDir must be a non-empty string, got {:?}",
                resp["helpDir"]
            );
            let lib_paths = resp["libPaths"].as_array().expect("libPaths must be array");
            assert!(!lib_paths.is_empty(), "libPaths must be non-empty");
            for p in lib_paths {
                assert!(
                    p.is_string(),
                    "libPaths element must be a string, got {p:?}"
                );
            }
        }
    }

    // ============================================================================
    // Regression tests for the two adjacent `packages_enabled` gates around
    // `rebuild_package_library` (design Decision 4).
    // ============================================================================
    mod package_enabled_gates {
        use std::sync::Arc;
        use tower_lsp::LanguageServer;
        use tower_lsp::lsp_types::{DidChangeConfigurationParams, ExecuteCommandParams};

        use super::make_test_backend;
        use crate::package_library::PackageLibrary;

        /// Build a populated, ready library so a clobber is observable.
        fn ready_library(path: std::path::PathBuf) -> Arc<PackageLibrary> {
            let mut lib = PackageLibrary::new_empty();
            lib.set_lib_paths(vec![path]);
            Arc::new(lib)
        }

        /// Settings-reload site (redundant gate removed): flipping
        /// `packages.enabled` to false must leave the library empty and
        /// not-ready. Locks in that the now-unconditional
        /// `rebuild_package_library` call still produces the disabled outcome.
        #[tokio::test]
        async fn settings_reload_disabled_yields_empty_not_ready() {
            let backend = make_test_backend();
            {
                let mut state = backend.state.write().await;
                state.package_library = ready_library(std::path::PathBuf::from("/tmp/libs"));
                state.package_library_ready = true;
            }

            // packages_enabled defaults to true, so flipping it to false makes
            // `package_settings_changed` fire and routes through the rebuild.
            backend
                .did_change_configuration(DidChangeConfigurationParams {
                    settings: serde_json::json!({ "packages": { "enabled": false } }),
                })
                .await;

            let state = backend.state.read().await;
            assert!(
                !state.package_library_ready,
                "reload with packages disabled must leave the library not-ready"
            );
            assert!(
                state.package_library.lib_paths().is_empty(),
                "reload with packages disabled must swap in an empty library; got {:?}",
                state.package_library.lib_paths()
            );
        }

        /// refreshPackages site: a refresh issued while packages are disabled
        /// must NOT touch the library the user built before disabling. The
        /// command early-returns, so the same `Arc` survives *and* its cache is
        /// left intact — the rest of the command (`clear_cache`, prefetch,
        /// libpath-watcher restart, republish) is skipped entirely.
        #[tokio::test]
        async fn refresh_packages_disabled_keeps_existing_library() {
            let backend = make_test_backend();
            let original = ready_library(std::path::PathBuf::from("/tmp/libs"));
            // Seed a cache entry so a stray `clear_cache()` would be observable.
            original
                .insert_package(crate::package_library::PackageInfo::new(
                    "dplyr".to_string(),
                    std::collections::HashSet::new(),
                ))
                .await;
            {
                let mut state = backend.state.write().await;
                state.cross_file_config.packages_enabled = false;
                state.package_library = Arc::clone(&original);
                state.package_library_ready = true;
            }

            backend
                .execute_command(ExecuteCommandParams {
                    command: "raven.refreshPackages".into(),
                    arguments: vec![],
                    work_done_progress_params: Default::default(),
                })
                .await
                .expect("execute_command must not error");

            let state = backend.state.read().await;
            assert!(
                Arc::ptr_eq(&state.package_library, &original),
                "refresh while disabled must not swap the library Arc"
            );
            assert!(
                !state.package_library.lib_paths().is_empty() && state.package_library_ready,
                "refresh while disabled must leave lib_paths and readiness intact"
            );
            assert!(
                state.package_library.is_cached("dplyr").await,
                "refresh while disabled must not clear the existing library's cache"
            );
        }
    }
}

#[cfg(test)]
mod project_config_initialize_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use tower_lsp::lsp_types::{
        DidChangeConfigurationParams, InitializeParams, Url, WorkspaceFolder,
    };

    async fn initialized_registrations(initialize_params: InitializeParams) -> Vec<Registration> {
        initialized_registrations_with_home(initialize_params, None).await
    }

    async fn initialized_registrations_with_home(
        initialize_params: InitializeParams,
        test_home_dir: Option<&std::path::Path>,
    ) -> Vec<Registration> {
        use futures_util::{SinkExt, StreamExt};
        use std::time::Duration;
        use tower::{Service, ServiceExt};
        use tower_lsp::jsonrpc::{Request, Response};
        use tower_lsp::lsp_types::{InitializedParams, RegistrationParams};

        let (mut svc, socket) = tower_lsp::LspService::new(Backend::new);
        if let Some(home_dir) = test_home_dir {
            svc.inner().set_test_home_dir(home_dir);
        }
        let initialize = Request::build("initialize")
            .id(1)
            .params(serde_json::to_value(initialize_params).unwrap())
            .finish();
        let response = svc.ready().await.unwrap().call(initialize).await.unwrap();
        assert!(response.is_some_and(|response| response.is_ok()));

        let (mut request_stream, mut response_sink) = socket.split();
        let initialized = svc.inner().initialized(InitializedParams {});
        tokio::pin!(initialized);
        let mut registrations = Vec::new();
        let mut initialized_done = false;

        loop {
            let request = if initialized_done {
                match tokio::time::timeout(Duration::from_millis(100), request_stream.next()).await
                {
                    Ok(request) => request,
                    Err(_) => break,
                }
            } else {
                tokio::select! {
                    () = &mut initialized => {
                        initialized_done = true;
                        continue;
                    }
                    request = tokio::time::timeout(Duration::from_secs(5), request_stream.next()) => {
                        request.expect("initialized should send dynamic registration requests")
                    }
                }
            }
            .expect("client socket should remain open");

            if request.method() == "client/registerCapability" {
                let params: RegistrationParams =
                    serde_json::from_value(request.params().expect("registration params").clone())
                        .unwrap();
                registrations.extend(params.registrations);
            }

            if let Some(id) = request.id().cloned() {
                response_sink
                    .send(Response::from_ok(id, serde_json::Value::Null))
                    .await
                    .unwrap();
            }
        }

        registrations
    }

    fn watcher_options(registration: Registration) -> DidChangeWatchedFilesRegistrationOptions {
        assert_eq!(registration.method, "workspace/didChangeWatchedFiles");
        serde_json::from_value(
            registration
                .register_options
                .expect("watch registration options"),
        )
        .unwrap()
    }

    fn assert_has_exact_watcher(watchers: &[FileSystemWatcher], base_uri: &Url, pattern: &str) {
        assert!(
            watchers.iter().any(|watcher| matches!(
                &watcher.glob_pattern,
                GlobPattern::Relative(relative)
                    if relative.base_uri == OneOf::Right(base_uri.clone())
                        && relative.pattern == pattern
            )),
            "expected exact external {pattern} watcher at {base_uri}, got {watchers:?}",
        );
    }

    fn assert_lacks_exact_watcher(watchers: &[FileSystemWatcher], base_uri: &Url, pattern: &str) {
        assert!(
            !watchers.iter().any(|watcher| matches!(
                &watcher.glob_pattern,
                GlobPattern::Relative(relative)
                    if relative.base_uri == OneOf::Right(base_uri.clone())
                        && relative.pattern == pattern
            )),
            "unexpected exact {pattern} watcher at {base_uri}, got {watchers:?}",
        );
    }

    fn assert_all_watchers_cover_config_file_events(watchers: &[FileSystemWatcher]) {
        for watcher in watchers {
            assert_eq!(
                watcher.kind,
                Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete)
            );
        }
    }

    #[tokio::test]
    async fn initialized_external_project_config_watch_does_not_unregister_before_first_register() {
        use futures_util::{SinkExt, StreamExt};
        use std::time::Duration;
        use tower::{Service, ServiceExt};
        use tower_lsp::jsonrpc::{Request, Response};
        use tower_lsp::lsp_types::InitializedParams;

        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("project");
        fs::create_dir(&workspace).unwrap();

        let (mut svc, socket) = tower_lsp::LspService::new(Backend::new);
        let initialize = Request::build("initialize")
            .id(1)
            .params(
                serde_json::to_value(InitializeParams {
                    workspace_folders: Some(vec![WorkspaceFolder {
                        uri: Url::from_file_path(&workspace).unwrap(),
                        name: "t".into(),
                    }]),
                    initialization_options: Some(serde_json::json!({
                        "crossFile": { "indexWorkspace": false },
                        "packages": { "enabled": false },
                    })),
                    ..Default::default()
                })
                .unwrap(),
            )
            .finish();
        let response = svc.ready().await.unwrap().call(initialize).await.unwrap();
        assert!(response.is_some_and(|response| response.is_ok()));

        let (mut request_stream, mut response_sink) = socket.split();
        let initialized = svc.inner().initialized(InitializedParams {});
        tokio::pin!(initialized);
        let mut methods = Vec::new();

        loop {
            tokio::select! {
                () = &mut initialized => break,
                request = tokio::time::timeout(Duration::from_secs(5), request_stream.next()) => {
                    let request = request
                        .expect("initialized should send dynamic registration requests")
                        .expect("client socket should remain open");
                    methods.push(request.method().to_string());
                    if let Some(id) = request.id().cloned() {
                        response_sink
                            .send(Response::from_ok(id, serde_json::Value::Null))
                            .await
                            .unwrap();
                    }
                }
            }
        }

        assert!(
            !methods
                .iter()
                .any(|method| method == "client/unregisterCapability"),
            "initial registration path must not unregister a capability id that was never registered; got {methods:?}"
        );
    }

    /// Regression for #281: an explicit client `enabled = false` must
    /// remain off even when discovery picks up a `.lintr` from the
    /// workspace (or an ancestor — same code path). Pre-fix, the `.lintr`
    /// loader injected `enabled = true` into the project layer, which won
    /// at the merge step over the client value.
    #[tokio::test]
    async fn initialize_client_false_overrides_lintr_discovery() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "linting": { "enabled": false }
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        let state = backend.state.read().await;
        assert!(
            !state.lint_config.enabled,
            "client enabled=false must win over .lintr discovery (#281)"
        );
    }

    /// Default client `"auto"` + a discovered `.lintr` resolves to on,
    /// preserving the implicit opt-in users had before #281.
    #[tokio::test]
    async fn initialize_auto_with_lintr_resolves_on() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "linting": { "enabled": "auto" }
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        let state = backend.state.read().await;
        assert!(
            state.lint_config.enabled,
            "auto + .lintr discovered must resolve to on (#281)"
        );
        assert_eq!(state.lint_config.line_length, 120);
    }

    /// Client `true` + `raven.toml [linting] enabled = "auto"` resolves
    /// to on. `"auto"` in raven.toml is semantically equivalent to omitting
    /// `enabled`, so the client's explicit value wins. Without the
    /// `strip_project_auto_enabled` normalization in `recompute_parsed_configs`,
    /// the project layer's `"auto"` would overwrite the client value at
    /// merge and then resolve to off because raven.toml was discovered (no
    /// `.lintr`). See #281.
    #[tokio::test]
    async fn initialize_client_true_with_raven_toml_auto_resolves_on() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("raven.toml"),
            "[linting]\nenabled = \"auto\"\n",
        )
        .unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "linting": { "enabled": true }
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        let state = backend.state.read().await;
        assert!(
            state.lint_config.enabled,
            "client true with raven.toml enabled=\"auto\" must resolve to on (#281)"
        );
    }

    /// Default client `"auto"` with no project config → off.
    #[tokio::test]
    async fn initialize_auto_no_project_resolves_off() {
        let tmp = TempDir::new().unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "linting": { "enabled": "auto" }
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        let state = backend.state.read().await;
        assert!(
            !state.lint_config.enabled,
            "auto + no project config must resolve to off"
        );
    }

    #[tokio::test]
    async fn initialize_loads_raven_toml_from_workspace_root() {
        let tmp = TempDir::new().unwrap();
        let toml_path = tmp.path().join("raven.toml");
        fs::write(&toml_path, "[linting]\nenabled = true\nlineLength = 123\n").unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        let params = InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root.clone(),
                name: "test".into(),
            }]),
            ..Default::default()
        };
        backend.initialize(params).await.unwrap();
        let state = backend.state.read().await;
        assert!(state.lint_config.enabled);
        assert_eq!(state.lint_config.line_length, 123);
        assert_eq!(
            state.project_config_path.as_deref(),
            Some(toml_path.as_path())
        );
    }

    #[tokio::test]
    async fn initialize_ignores_malformed_ancestor_raven_toml() {
        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        fs::write(home.path().join("raven.toml"), "not valid = = toml [[[\n").unwrap();
        let root = Url::from_file_path(&workspace).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();

        let state = backend.state.read().await;
        assert!(
            state.project_config_path.is_none(),
            "malformed raven.toml must not become the active project config"
        );
    }

    #[tokio::test]
    async fn initialize_uses_init_options_when_no_project_config() {
        let tmp = TempDir::new().unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        let params = InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root.clone(),
                name: "test".into(),
            }]),
            initialization_options: Some(serde_json::json!({
                "linting": { "enabled": true, "lineLength": 90 }
            })),
            ..Default::default()
        };
        backend.initialize(params).await.unwrap();
        let state = backend.state.read().await;
        assert!(state.lint_config.enabled);
        assert_eq!(state.lint_config.line_length, 90);
        assert!(state.project_config_path.is_none());
    }

    #[tokio::test]
    async fn did_change_configuration_read_home_lintr_toggle_loads_and_clears_home_lintr() {
        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let lintr = home.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend.set_test_home_dir(home.path());
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: Url::from_file_path(&workspace).unwrap(),
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "crossFile": { "indexWorkspace": false },
                    "packages": { "enabled": false },
                    "linting": { "enabled": "auto", "readHomeLintr": false }
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        {
            let state = backend.state.read().await;
            assert!(
                state.project_config_path.is_none(),
                "literal home .lintr should be ignored before opt-in"
            );
            assert!(!state.lint_config.enabled);
        }

        backend
            .did_change_configuration(DidChangeConfigurationParams {
                settings: serde_json::json!({
                    "crossFile": { "indexWorkspace": false },
                    "packages": { "enabled": false },
                    "linting": { "enabled": "auto", "readHomeLintr": true }
                }),
            })
            .await;
        {
            let state = backend.state.read().await;
            assert_eq!(state.project_config_path.as_deref(), Some(lintr.as_path()));
            assert!(state.lint_config.enabled);
            assert_eq!(state.lint_config.line_length, 120);
        }

        backend
            .did_change_configuration(DidChangeConfigurationParams {
                settings: serde_json::json!({
                    "crossFile": { "indexWorkspace": false },
                    "packages": { "enabled": false },
                    "linting": { "enabled": "auto", "readHomeLintr": false }
                }),
            })
            .await;
        let state = backend.state.read().await;
        assert!(
            state.project_config_path.is_none(),
            "turning readHomeLintr back off should clear the home .lintr project layer"
        );
        assert!(!state.lint_config.enabled);
        assert_eq!(state.lint_config.line_length, 80);
    }

    #[tokio::test]
    async fn did_change_configuration_read_home_lintr_toggle_notifies_project_config_loaded() {
        use futures_util::{SinkExt, StreamExt};
        use serde_json::json;
        use std::time::Duration;
        use tower::{Service, ServiceExt};
        use tower_lsp::jsonrpc::{Request, Response};

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let lintr = home.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();

        let (mut svc, socket) = tower_lsp::LspService::new(Backend::new);
        svc.inner().set_test_home_dir(home.path());
        let initialize = Request::build("initialize")
            .id(1)
            .params(
                serde_json::to_value(InitializeParams {
                    workspace_folders: Some(vec![WorkspaceFolder {
                        uri: Url::from_file_path(&workspace).unwrap(),
                        name: "t".into(),
                    }]),
                    initialization_options: Some(json!({
                        "crossFile": { "indexWorkspace": false },
                        "packages": { "enabled": false },
                        "linting": { "enabled": "auto", "readHomeLintr": false }
                    })),
                    ..Default::default()
                })
                .unwrap(),
            )
            .finish();
        let response = svc.ready().await.unwrap().call(initialize).await.unwrap();
        assert!(response.is_some_and(|response| response.is_ok()));

        let (mut request_stream, mut response_sink) = socket.split();
        for (read_home_lintr, expected_path, expected_source) in [
            (
                true,
                serde_json::Value::String(lintr.display().to_string()),
                serde_json::Value::String(".lintr".into()),
            ),
            (false, serde_json::Value::Null, serde_json::Value::Null),
        ] {
            let changed = svc
                .inner()
                .did_change_configuration(DidChangeConfigurationParams {
                    settings: json!({
                        "crossFile": { "indexWorkspace": false },
                        "packages": { "enabled": false },
                        "linting": { "enabled": "auto", "readHomeLintr": read_home_lintr }
                    }),
                });
            tokio::pin!(changed);
            let mut changed_done = false;
            let mut saw_notification = false;

            loop {
                if changed_done && saw_notification {
                    break;
                }

                let request = if changed_done {
                    tokio::time::timeout(Duration::from_secs(5), request_stream.next())
                        .await
                        .expect("settings change should send raven/projectConfigLoaded")
                } else {
                    tokio::select! {
                        () = &mut changed => {
                            changed_done = true;
                            continue;
                        }
                        request = tokio::time::timeout(Duration::from_secs(5), request_stream.next()) => {
                            request.expect("settings change should send client requests or project config notification")
                        }
                    }
                }
                .expect("client socket should remain open");

                if request.method() == "raven/projectConfigLoaded" {
                    let payload = request.params().expect("project config payload");
                    assert_eq!(payload["path"], expected_path);
                    assert_eq!(payload["source"], expected_source);
                    saw_notification = true;
                }

                if let Some(id) = request.id().cloned() {
                    response_sink
                        .send(Response::from_ok(id, serde_json::Value::Null))
                        .await
                        .unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn did_change_configuration_reregisters_external_watch_when_read_home_lintr_changes() {
        use futures_util::{SinkExt, StreamExt};
        use serde_json::json;
        use std::time::Duration;
        use tower::{Service, ServiceExt};
        use tower_lsp::jsonrpc::{Request, Response};
        use tower_lsp::lsp_types::{InitializedParams, RegistrationParams, UnregistrationParams};

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let root = Url::from_file_path(&workspace).unwrap();

        let (mut svc, socket) = tower_lsp::LspService::new(Backend::new);
        svc.inner().set_test_home_dir(home.path());
        let initialize = Request::build("initialize")
            .id(1)
            .params(
                serde_json::to_value(InitializeParams {
                    workspace_folders: Some(vec![WorkspaceFolder {
                        uri: root,
                        name: "t".into(),
                    }]),
                    initialization_options: Some(json!({
                        "crossFile": { "indexWorkspace": false },
                        "packages": { "enabled": false },
                        "linting": { "enabled": "auto", "readHomeLintr": false }
                    })),
                    ..Default::default()
                })
                .unwrap(),
            )
            .finish();
        let response = svc.ready().await.unwrap().call(initialize).await.unwrap();
        assert!(response.is_some_and(|response| response.is_ok()));

        let (mut request_stream, mut response_sink) = socket.split();
        let initialized = svc.inner().initialized(InitializedParams {});
        tokio::pin!(initialized);
        loop {
            tokio::select! {
                () = &mut initialized => break,
                request = tokio::time::timeout(Duration::from_secs(5), request_stream.next()) => {
                    let request = request
                        .expect("initialized should send dynamic registration requests")
                        .expect("client socket should remain open");
                    if let Some(id) = request.id().cloned() {
                        response_sink
                            .send(Response::from_ok(id, serde_json::Value::Null))
                            .await
                            .unwrap();
                    }
                }
            }
        }

        let changed = svc
            .inner()
            .did_change_configuration(DidChangeConfigurationParams {
                settings: json!({
                    "crossFile": { "indexWorkspace": false },
                    "packages": { "enabled": false },
                    "linting": { "enabled": "auto", "readHomeLintr": true }
                }),
            });
        tokio::pin!(changed);
        let home_uri = Url::from_file_path(home.path()).unwrap();
        let mut saw_unregister = false;
        let mut saw_register = false;

        loop {
            tokio::select! {
                () = &mut changed => break,
                request = tokio::time::timeout(Duration::from_secs(5), request_stream.next()) => {
                    let request = request
                        .expect("settings change should refresh external watchers")
                        .expect("client socket should remain open");
                    if request.method() == "client/unregisterCapability" {
                        let params: UnregistrationParams = serde_json::from_value(
                            request.params().expect("unregistration params").clone(),
                        )
                        .unwrap();
                        saw_unregister |= params.unregisterations.iter().any(|registration| {
                            registration.id == "raven-active-project-config-file"
                                && registration.method == "workspace/didChangeWatchedFiles"
                        });
                    } else if request.method() == "client/registerCapability" {
                        let params: RegistrationParams = serde_json::from_value(
                            request.params().expect("registration params").clone(),
                        )
                        .unwrap();
                        for registration in params.registrations {
                            if registration.id != "raven-active-project-config-file" {
                                continue;
                            }
                            let options = watcher_options(registration);
                            assert_has_exact_watcher(&options.watchers, &home_uri, ".lintr");
                            saw_register = true;
                        }
                    }

                    if let Some(id) = request.id().cloned() {
                        response_sink
                            .send(Response::from_ok(id, serde_json::Value::Null))
                            .await
                            .unwrap();
                    }
                }
            }
        }

        assert!(
            saw_unregister,
            "readHomeLintr watcher changes must unregister the previous exact watcher set"
        );
        assert!(
            saw_register,
            "readHomeLintr watcher changes must register the new exact watcher set"
        );
    }

    /// When a client clears its linting settings (e.g. user "Reset Setting"
    /// in VS Code), `did_change_configuration` must re-merge against the
    /// project file: project-pinned keys still win, and keys with no project
    /// override fall back to built-in defaults rather than retaining the
    /// previous client value.
    #[tokio::test]
    async fn did_change_configuration_falls_back_to_project_when_client_clears() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("raven.toml"),
            "[linting]\nenabled = true\nlineLength = 100\n",
        )
        .unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "linting": { "enabled": true, "lineLength": 80, "objectLength": 40 }
                })),
                ..Default::default()
            })
            .await
            .unwrap();

        // Sanity: project file wins on lineLength; client wins on objectLength.
        {
            let state = backend.state.read().await;
            assert_eq!(state.lint_config.line_length, 100);
            assert_eq!(state.lint_config.object_length, 40);
        }

        // Client clears all linting settings (e.g. user "Reset Setting" in VS Code).
        backend
            .did_change_configuration(DidChangeConfigurationParams {
                settings: serde_json::json!({ "linting": {} }),
            })
            .await;

        let state = backend.state.read().await;
        // Project still pins lineLength; objectLength falls back to default (30).
        assert_eq!(state.lint_config.line_length, 100);
        assert_eq!(state.lint_config.object_length, 30);
    }

    /// When `did_change_watched_files` reports a change to `raven.toml` on
    /// disk, the backend must re-discover the project config, re-load it,
    /// and recompute parsed configs so the new value is reflected in
    /// `state.lint_config`. This is the live-reload entry point used by
    /// non-VS Code clients that honor dynamic file watch registration.
    #[tokio::test]
    async fn watched_files_reload_picks_up_new_raven_toml() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("raven.toml"),
            "[linting]\nenabled = true\nlineLength = 100\n",
        )
        .unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root.clone(),
                    name: "t".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(backend.state.read().await.lint_config.line_length, 100);

        // Edit raven.toml on disk.
        fs::write(
            tmp.path().join("raven.toml"),
            "[linting]\nenabled = true\nlineLength = 140\n",
        )
        .unwrap();

        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(tmp.path().join("raven.toml")).unwrap(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;

        assert_eq!(backend.state.read().await.lint_config.line_length, 140);
    }

    #[tokio::test]
    async fn watched_files_ignores_home_lintr_event_when_home_lintr_disabled() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let toml_path = workspace.join("raven.toml");
        fs::write(&toml_path, "[linting]\nenabled = true\nlineLength = 100\n").unwrap();
        let lintr = home.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend.set_test_home_dir(home.path());
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: Url::from_file_path(&workspace).unwrap(),
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "linting": { "enabled": "auto", "readHomeLintr": false }
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        {
            let state = backend.state.read().await;
            assert_eq!(
                state.project_config_path.as_deref(),
                Some(toml_path.as_path())
            );
            assert_eq!(state.lint_config.line_length, 100);
        }

        // Change the active config on disk without sending its event. A too-broad
        // config reload triggered by the ignored home .lintr event below would
        // pick this up and expose the bug as lineLength=140.
        fs::write(&toml_path, "[linting]\nenabled = true\nlineLength = 140\n").unwrap();
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(180))\n",
        )
        .unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&lintr).unwrap(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;

        let state = backend.state.read().await;
        assert_eq!(
            state.project_config_path.as_deref(),
            Some(toml_path.as_path())
        );
        assert_eq!(
            state.lint_config.line_length, 100,
            "disabled literal home .lintr events must not trigger project-config rediscovery"
        );
    }

    #[tokio::test]
    async fn watched_files_ignores_second_workspace_config_event() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let first_repo = TempDir::new().unwrap();
        let second_repo = TempDir::new().unwrap();
        let first = first_repo.path().join("project");
        let second = second_repo.path().join("project");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let first_toml = first.join("raven.toml");
        let second_toml = second.join("raven.toml");
        fs::write(&first_toml, "[linting]\nenabled = true\nlineLength = 100\n").unwrap();
        fs::write(
            &second_toml,
            "[linting]\nenabled = true\nlineLength = 180\n",
        )
        .unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![
                    WorkspaceFolder {
                        uri: Url::from_file_path(&first).unwrap(),
                        name: "first".into(),
                    },
                    WorkspaceFolder {
                        uri: Url::from_file_path(&second).unwrap(),
                        name: "second".into(),
                    },
                ]),
                ..Default::default()
            })
            .await
            .unwrap();
        {
            let state = backend.state.read().await;
            assert_eq!(
                state.project_config_path.as_deref(),
                Some(first_toml.as_path())
            );
            assert_eq!(state.lint_config.line_length, 100);
        }

        // Mutate the active first-root config without sending its event. If the
        // second-root event below caused a broad rediscovery, the active config
        // would change to 140.
        fs::write(&first_toml, "[linting]\nenabled = true\nlineLength = 140\n").unwrap();
        fs::write(
            &second_toml,
            "[linting]\nenabled = true\nlineLength = 220\n",
        )
        .unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&second_toml).unwrap(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;
        {
            let state = backend.state.read().await;
            assert_eq!(
                state.project_config_path.as_deref(),
                Some(first_toml.as_path())
            );
            assert_eq!(
                state.lint_config.line_length, 100,
                "second workspace config events must not reload the first workspace project config"
            );
        }

        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&first_toml).unwrap(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;
        assert_eq!(backend.state.read().await.lint_config.line_length, 140);
    }

    /// `.Rprofile` is a live R document: editing it in an open buffer must
    /// refresh the prelude AS YOU TYPE (not only on save), and closing it
    /// WITHOUT saving must revert the prelude to the on-disk content. Uses
    /// `crossFile.indexWorkspace = false` so initialize seeds package inputs
    /// synchronously (no racy background scan task).
    #[tokio::test]
    async fn live_rprofile_buffer_edit_and_close_refresh_prelude() {
        use tower_lsp::lsp_types::{
            DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
            TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
            VersionedTextDocumentIdentifier,
        };

        let tmp = TempDir::new().unwrap();
        let root_path = tmp.path().to_path_buf();
        // Disk `.Rprofile` defines helper_a; the disk file is never changed.
        fs::write(root_path.join(".Rprofile"), "helper_a <- function() 1\n").unwrap();
        let root = Url::from_file_path(&root_path).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                // `indexWorkspace: false` prevents a background workspace-scan
                // task that would otherwise race this test's prelude assertions.
                initialization_options: Some(serde_json::json!({
                    "crossFile": { "indexWorkspace": false }
                })),
                ..Default::default()
            })
            .await
            .unwrap();

        // Seed the package inputs the live refresh depends on, then establish
        // the baseline prelude from the disk `.Rprofile` (helper_a). (initialize
        // seeds package inputs via its background scan in real use; here we set
        // them directly so the test isolates the live-refresh behavior.)
        {
            let mut state = backend.state.write().await;
            state.package_inputs.workspace_root = Some(root_path.clone());
            state.package_inputs.model_rprofile = true;
            let scan = crate::package_state::rprofile::scan_workspace_rprofile(&root_path);
            state.package_inputs.rprofile_symbols = scan.symbols;
            state.package_inputs.rprofile_attached_packages = scan.attached_packages;
            state.package_inputs.rprofile_sourced_files = scan.sourced_files;
        }

        // Sanity: baseline prelude reflects the disk `.Rprofile`.
        {
            let state = backend.state.read().await;
            assert!(
                state.package_inputs.rprofile_symbols.contains("helper_a"),
                "baseline prelude must harvest the disk .Rprofile: {:?}",
                state.package_inputs.rprofile_symbols
            );
        }

        let rprofile_uri = Url::from_file_path(root_path.join(".Rprofile")).unwrap();

        // Open `.Rprofile` as a live R document (content == disk).
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: rprofile_uri.clone(),
                    language_id: "r".into(),
                    version: 1,
                    text: "helper_a <- function() 1\n".into(),
                },
            })
            .await;

        // Edit the buffer (unsaved) to define helper_b instead of helper_a.
        backend
            .did_change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: rprofile_uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "helper_b <- function() 1\n".into(),
                }],
            })
            .await;

        {
            let state = backend.state.read().await;
            assert!(
                state.package_inputs.rprofile_symbols.contains("helper_b"),
                "live buffer edit must refresh the prelude: {:?}",
                state.package_inputs.rprofile_symbols
            );
            assert!(
                !state.package_inputs.rprofile_symbols.contains("helper_a"),
                "stale buffer symbol must be dropped: {:?}",
                state.package_inputs.rprofile_symbols
            );
        }

        // Close without saving: the prelude must revert to the on-disk helper_a.
        backend
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: rprofile_uri },
            })
            .await;

        {
            let state = backend.state.read().await;
            assert!(
                state.package_inputs.rprofile_symbols.contains("helper_a"),
                "close must revert the prelude to disk: {:?}",
                state.package_inputs.rprofile_symbols
            );
            assert!(
                !state.package_inputs.rprofile_symbols.contains("helper_b"),
                "discarded buffer symbol must not linger: {:?}",
                state.package_inputs.rprofile_symbols
            );
        }
    }

    /// `did_open` can carry UNSAVED `.Rprofile` text (e.g. VS Code hot-exit
    /// restoring a dirty buffer) with no following `did_change`. The prelude
    /// must reflect the opened buffer, not just the disk seed.
    #[tokio::test]
    async fn live_rprofile_dirty_open_uses_buffer_not_disk() {
        use tower_lsp::lsp_types::{DidOpenTextDocumentParams, TextDocumentItem};

        let tmp = TempDir::new().unwrap();
        let root_path = tmp.path().to_path_buf();
        // Disk `.Rprofile` defines helper_a.
        fs::write(root_path.join(".Rprofile"), "helper_a <- function() 1\n").unwrap();
        let root = Url::from_file_path(&root_path).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "crossFile": { "indexWorkspace": false }
                })),
                ..Default::default()
            })
            .await
            .unwrap();

        // Baseline: seed the prelude from disk (helper_a).
        {
            let mut state = backend.state.write().await;
            state.package_inputs.workspace_root = Some(root_path.clone());
            state.package_inputs.model_rprofile = true;
            let scan = crate::package_state::rprofile::scan_workspace_rprofile(&root_path);
            state.package_inputs.rprofile_symbols = scan.symbols;
            state.package_inputs.rprofile_attached_packages = scan.attached_packages;
            state.package_inputs.rprofile_sourced_files = scan.sourced_files;
        }

        // Open `.Rprofile` carrying UNSAVED buffer text (helper_x ≠ disk's helper_a),
        // with NO following did_change.
        let rprofile_uri = Url::from_file_path(root_path.join(".Rprofile")).unwrap();
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: rprofile_uri,
                    language_id: "r".into(),
                    version: 1,
                    text: "helper_x <- function() 1\n".into(),
                },
            })
            .await;

        let state = backend.state.read().await;
        assert!(
            state.package_inputs.rprofile_symbols.contains("helper_x"),
            "did_open must refresh the prelude from the (unsaved) buffer: {:?}",
            state.package_inputs.rprofile_symbols
        );
        assert!(
            !state.package_inputs.rprofile_symbols.contains("helper_a"),
            "stale disk symbol must be dropped on dirty open: {:?}",
            state.package_inputs.rprofile_symbols
        );
    }

    /// While `.Rprofile` is OPEN with unsaved edits, a file-watcher event for it
    /// (external disk touch, or the editor's own save) must NOT overwrite the
    /// live-buffer prelude with the on-disk scan — open documents are
    /// authoritative. (Regression for the manifest-block open-doc skip.)
    #[tokio::test]
    async fn watched_rprofile_event_does_not_clobber_open_buffer_prelude() {
        use tower_lsp::lsp_types::{
            DidChangeTextDocumentParams, DidChangeWatchedFilesParams, DidOpenTextDocumentParams,
            FileChangeType, FileEvent, TextDocumentContentChangeEvent, TextDocumentItem,
            VersionedTextDocumentIdentifier,
        };

        let tmp = TempDir::new().unwrap();
        let root_path = tmp.path().to_path_buf();
        fs::write(root_path.join(".Rprofile"), "helper_a <- function() 1\n").unwrap();
        let root = Url::from_file_path(&root_path).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "crossFile": { "indexWorkspace": false }
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        {
            let mut state = backend.state.write().await;
            state.package_inputs.workspace_root = Some(root_path.clone());
            state.package_inputs.model_rprofile = true;
            let scan = crate::package_state::rprofile::scan_workspace_rprofile(&root_path);
            state.package_inputs.rprofile_symbols = scan.symbols;
            state.package_inputs.rprofile_attached_packages = scan.attached_packages;
            state.package_inputs.rprofile_sourced_files = scan.sourced_files;
        }

        let rprofile_uri = Url::from_file_path(root_path.join(".Rprofile")).unwrap();
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: rprofile_uri.clone(),
                    language_id: "r".into(),
                    version: 1,
                    text: "helper_a <- function() 1\n".into(),
                },
            })
            .await;
        // Unsaved edit → buffer defines helper_b; disk still says helper_a.
        backend
            .did_change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: rprofile_uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "helper_b <- function() 1\n".into(),
                }],
            })
            .await;

        // A watcher event arrives for `.Rprofile` (e.g. external disk touch).
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: rprofile_uri,
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;

        let state = backend.state.read().await;
        assert!(
            state.package_inputs.rprofile_symbols.contains("helper_b"),
            "open-buffer prelude must survive a watcher event: {:?}",
            state.package_inputs.rprofile_symbols
        );
        assert!(
            !state.package_inputs.rprofile_symbols.contains("helper_a"),
            "watcher must not revert an open buffer to disk: {:?}",
            state.package_inputs.rprofile_symbols
        );
    }

    /// A live reload of `raven.toml` must reapply `packages.enabled` —
    /// flipping it `false` should drive the package-rebuild path
    /// (replacing `state.package_library` with a fresh empty instance)
    /// without requiring a server restart. The pre-extraction code path
    /// warned the user to restart instead; the shared reconciliation
    /// helper now drives the rebuild from both call sites.
    ///
    /// `package_library_ready` defaults to `false` and stays false until
    /// `rebuild_package_library` runs successfully against an R
    /// subprocess (which we don't have in unit tests). So instead of
    /// asserting on that flag, we capture the `package_library` Arc's
    /// raw pointer before and after the reload — a different pointer is
    /// proof the rebuild path replaced the instance, which is the
    /// specific behavior this PR adds to the watched-files branch.
    #[tokio::test]
    async fn watched_files_reload_disables_packages_live() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("raven.toml"),
            "[packages]\nenabled = true\n",
        )
        .unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root.clone(),
                    name: "t".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        // Sanity: project file initially enables packages.
        assert!(
            backend
                .state
                .read()
                .await
                .cross_file_config
                .packages_enabled
        );
        // Capture the package_library Arc identity so we can prove the
        // reload's rebuild path replaced the instance.
        let library_before = std::sync::Arc::as_ptr(&backend.state.read().await.package_library);

        // Edit raven.toml to disable packages on disk.
        fs::write(
            tmp.path().join("raven.toml"),
            "[packages]\nenabled = false\n",
        )
        .unwrap();

        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(tmp.path().join("raven.toml")).unwrap(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;

        // Effective config reflects the new value (live reload) AND the
        // package_library Arc was replaced — proof the rebuild path ran
        // rather than the prior warn-only behavior.
        let state = backend.state.read().await;
        assert!(!state.cross_file_config.packages_enabled);
        let library_after = std::sync::Arc::as_ptr(&state.package_library);
        assert!(
            !std::ptr::eq(library_before, library_after),
            "expected package_library to be replaced by the reload's rebuild path"
        );
        // The fresh library replaces whatever the initialize-time
        // `PackageLibrary::new_empty()` was — confirm it's the empty
        // variant the disabled path constructs.
        assert!(state.package_library.lib_paths().is_empty());
    }

    /// `.lintr` reload via `did_change_watched_files` must round-trip
    /// through the same reconciliation helper as `raven.toml`. The
    /// `.lintr` load branch lives at `did_change_watched_files`'s step-1
    /// discovery, and its loaded settings feed the same `prev`/recompute
    /// flow — so this exists primarily to guard against drift in either
    /// of those two paths.
    #[tokio::test]
    async fn watched_files_reload_picks_up_new_dotlintr() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let tmp = TempDir::new().unwrap();
        // `lintr_loader` translates `line_length_linter(N)` (under the
        // `linters_with_defaults(...)` wrapper that real `.lintr` files
        // use) into `linting.lineLength = N`. See the loader's own
        // `line_length_param_maps` test for the canonical form.
        fs::write(
            tmp.path().join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root.clone(),
                    name: "t".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        // Initialize picks up the `.lintr` and translates it to the
        // expected line length.
        {
            let state = backend.state.read().await;
            assert_eq!(
                state
                    .project_config_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str()),
                Some(".lintr"),
                "initialize should have picked up the .lintr"
            );
            assert_eq!(
                state.lint_config.line_length, 120,
                ".lintr should have been parsed into lineLength=120 at initialize"
            );
        }

        // Edit the `.lintr` on disk and replay the watched-files event.
        fs::write(
            tmp.path().join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(180))\n",
        )
        .unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(tmp.path().join(".lintr")).unwrap(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;

        // Reload preserves the `.lintr` source discriminator AND reapplies
        // the new setting — proof that the live-reload pipeline survives
        // the `.lintr` branch, not just the raven.toml one.
        let state = backend.state.read().await;
        assert_eq!(
            state
                .project_config_path
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some(".lintr"),
            "reload should have kept the .lintr discriminator"
        );
        assert_eq!(
            state.lint_config.line_length, 180,
            "reload should have re-translated the new .lintr line_length_linter value"
        );
    }

    #[tokio::test]
    async fn watched_files_reload_picks_up_opted_in_home_dotlintr() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let lintr = home.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend.set_test_home_dir(home.path());
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: Url::from_file_path(&workspace).unwrap(),
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "linting": { "enabled": "auto", "readHomeLintr": true }
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        {
            let state = backend.state.read().await;
            assert_eq!(state.project_config_path.as_deref(), Some(lintr.as_path()));
            assert!(state.lint_config.enabled);
            assert_eq!(state.lint_config.line_length, 120);
        }

        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(180))\n",
        )
        .unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&lintr).unwrap(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;
        {
            let state = backend.state.read().await;
            assert_eq!(state.project_config_path.as_deref(), Some(lintr.as_path()));
            assert_eq!(state.lint_config.line_length, 180);
        }

        fs::remove_file(&lintr).unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&lintr).unwrap(),
                    typ: FileChangeType::DELETED,
                }],
            })
            .await;

        let state = backend.state.read().await;
        assert!(
            state.project_config_path.is_none(),
            "deleted opted-in home .lintr must clear the active project layer"
        );
        assert!(!state.lint_config.enabled);
        assert_eq!(state.lint_config.line_length, 80);
    }

    /// Deleting an ancestor `.lintr` must clear the project config layer.
    /// This is the server-side half of the external `.lintr` live-reload path:
    /// once the exact external watcher reports the delete event, rediscovery
    /// from the workspace root must stop treating the deleted ancestor file as
    /// active and must fall back to built-in lint defaults.
    #[tokio::test]
    async fn watched_files_reload_clears_deleted_ancestor_dotlintr() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let lintr = home.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let root = Url::from_file_path(&workspace).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root.clone(),
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "linting": { "enabled": "auto" }
                })),
                ..Default::default()
            })
            .await
            .unwrap();

        {
            let state = backend.state.read().await;
            assert_eq!(state.project_config_path.as_deref(), Some(lintr.as_path()));
            assert!(state.lint_config.enabled);
            assert_eq!(state.lint_config.line_length, 120);
        }

        fs::remove_file(&lintr).unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&lintr).unwrap(),
                    typ: FileChangeType::DELETED,
                }],
            })
            .await;

        let state = backend.state.read().await;
        assert!(
            state.project_config_path.is_none(),
            "deleted ancestor .lintr must clear project_config_path"
        );
        assert!(
            !state.lint_config.enabled,
            "without a discovered .lintr, auto linting should fall back off"
        );
        assert_eq!(state.lint_config.line_length, 80);
        drop(state);

        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(180))\n",
        )
        .unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&lintr).unwrap(),
                    typ: FileChangeType::CREATED,
                }],
            })
            .await;

        let state = backend.state.read().await;
        assert_eq!(
            state.project_config_path.as_deref(),
            Some(lintr.as_path()),
            "recreated ancestor .lintr must become active again"
        );
        assert!(state.lint_config.enabled);
        assert_eq!(state.lint_config.line_length, 180);
    }

    #[tokio::test]
    async fn watched_files_reload_picks_up_external_raven_toml_created_next_to_active_dotlintr() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let lintr = home.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let root = Url::from_file_path(&workspace).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "linting": { "enabled": "auto" }
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            backend.state.read().await.project_config_path.as_deref(),
            Some(lintr.as_path())
        );

        let toml_path = home.path().join("raven.toml");
        fs::write(&toml_path, "[linting]\nenabled = true\nlineLength = 160\n").unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&toml_path).unwrap(),
                    typ: FileChangeType::CREATED,
                }],
            })
            .await;

        let state = backend.state.read().await;
        assert_eq!(
            state.project_config_path.as_deref(),
            Some(toml_path.as_path()),
            "external raven.toml created next to active .lintr must take precedence"
        );
        assert_eq!(state.lint_config.line_length, 160);
    }

    #[tokio::test]
    async fn watched_files_reload_reactivates_external_raven_toml_after_delete_fallback() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let toml_path = home.path().join("raven.toml");
        let lintr = home.path().join(".lintr");
        fs::write(&toml_path, "[linting]\nenabled = true\nlineLength = 120\n").unwrap();
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(140))\n",
        )
        .unwrap();
        let root = Url::from_file_path(&workspace).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                initialization_options: Some(serde_json::json!({
                    "linting": { "enabled": "auto" }
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            backend.state.read().await.project_config_path.as_deref(),
            Some(toml_path.as_path())
        );

        fs::remove_file(&toml_path).unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&toml_path).unwrap(),
                    typ: FileChangeType::DELETED,
                }],
            })
            .await;
        {
            let state = backend.state.read().await;
            assert_eq!(state.project_config_path.as_deref(), Some(lintr.as_path()));
            assert_eq!(state.lint_config.line_length, 140);
        }

        fs::write(&toml_path, "[linting]\nenabled = true\nlineLength = 180\n").unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&toml_path).unwrap(),
                    typ: FileChangeType::CREATED,
                }],
            })
            .await;

        let state = backend.state.read().await;
        assert_eq!(
            state.project_config_path.as_deref(),
            Some(toml_path.as_path()),
            "recreated external raven.toml must take precedence over fallback .lintr"
        );
        assert_eq!(state.lint_config.line_length, 180);
    }

    #[tokio::test]
    async fn initialized_registers_exact_external_project_config_watcher() {
        use serde_json::json;

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let lintr = home.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let root = Url::from_file_path(&workspace).unwrap();

        let registrations = initialized_registrations(InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root.clone(),
                name: "t".into(),
            }]),
            initialization_options: Some(json!({
                "crossFile": { "indexWorkspace": false },
                "packages": { "enabled": false },
                "linting": { "enabled": "auto" }
            })),
            ..Default::default()
        })
        .await;

        let parent_uri = Url::from_file_path(home.path()).unwrap();
        let base_registration = registrations
            .iter()
            .find(|registration| registration.id == "raven-config-files")
            .cloned()
            .expect("initialized must register exact workspace config-file watchers");
        let base_options = watcher_options(base_registration);
        assert_eq!(base_options.watchers.len(), 2);
        assert_all_watchers_cover_config_file_events(&base_options.watchers);
        assert_has_exact_watcher(&base_options.watchers, &root, "raven.toml");
        assert_has_exact_watcher(&base_options.watchers, &root, ".lintr");

        let exact_registration = registrations
            .into_iter()
            .find(|registration| registration.id == "raven-active-project-config-file")
            .expect("initialized must register exact external config-file watchers");
        let exact_options = watcher_options(exact_registration);
        assert_all_watchers_cover_config_file_events(&exact_options.watchers);
        assert_has_exact_watcher(&exact_options.watchers, &parent_uri, "raven.toml");
        assert_has_exact_watcher(&exact_options.watchers, &parent_uri, ".lintr");
    }

    #[tokio::test]
    async fn initialized_project_config_watchers_follow_active_first_workspace_only() {
        use serde_json::json;

        let first_repo = TempDir::new().unwrap();
        let second_repo = TempDir::new().unwrap();
        let first = first_repo.path().join("project");
        let second = second_repo.path().join("project");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let first_uri = Url::from_file_path(&first).unwrap();
        let second_uri = Url::from_file_path(&second).unwrap();

        let registrations = initialized_registrations(InitializeParams {
            workspace_folders: Some(vec![
                WorkspaceFolder {
                    uri: first_uri.clone(),
                    name: "first".into(),
                },
                WorkspaceFolder {
                    uri: second_uri.clone(),
                    name: "second".into(),
                },
            ]),
            initialization_options: Some(json!({
                "crossFile": { "indexWorkspace": false },
                "packages": { "enabled": false },
                "linting": { "enabled": "auto" }
            })),
            ..Default::default()
        })
        .await;

        let base_registration = registrations
            .iter()
            .find(|registration| registration.id == "raven-config-files")
            .cloned()
            .expect("initialized must register exact workspace config-file watchers");
        let base_options = watcher_options(base_registration);
        assert_has_exact_watcher(&base_options.watchers, &first_uri, "raven.toml");
        assert_has_exact_watcher(&base_options.watchers, &first_uri, ".lintr");
        assert_lacks_exact_watcher(&base_options.watchers, &second_uri, "raven.toml");
        assert_lacks_exact_watcher(&base_options.watchers, &second_uri, ".lintr");

        let first_parent_uri = Url::from_file_path(first_repo.path()).unwrap();
        let second_parent_uri = Url::from_file_path(second_repo.path()).unwrap();
        let exact_registration = registrations
            .into_iter()
            .find(|registration| registration.id == "raven-active-project-config-file")
            .expect("initialized must register exact external config-file watchers");
        let exact_options = watcher_options(exact_registration);
        assert_has_exact_watcher(&exact_options.watchers, &first_parent_uri, "raven.toml");
        assert_has_exact_watcher(&exact_options.watchers, &first_parent_uri, ".lintr");
        assert_lacks_exact_watcher(&exact_options.watchers, &second_parent_uri, "raven.toml");
        assert_lacks_exact_watcher(&exact_options.watchers, &second_parent_uri, ".lintr");
    }

    #[tokio::test]
    async fn initialized_registers_external_ancestor_watchers_without_active_config() {
        use serde_json::json;

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let root = Url::from_file_path(&workspace).unwrap();

        let registrations = initialized_registrations(InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: "t".into(),
            }]),
            initialization_options: Some(json!({
                "crossFile": { "indexWorkspace": false },
                "packages": { "enabled": false },
                "linting": { "enabled": "auto" }
            })),
            ..Default::default()
        })
        .await;

        let parent_uri = Url::from_file_path(home.path()).unwrap();
        let registration = registrations
            .into_iter()
            .find(|registration| registration.id == "raven-active-project-config-file")
            .expect("absent external configs should still have exact ancestor watchers");
        let options = watcher_options(registration);
        assert_all_watchers_cover_config_file_events(&options.watchers);
        assert_has_exact_watcher(&options.watchers, &parent_uri, "raven.toml");
        assert_has_exact_watcher(&options.watchers, &parent_uri, ".lintr");
    }

    #[tokio::test]
    async fn initialized_keeps_workspace_home_lintr_watcher_when_home_lintr_disabled() {
        use serde_json::json;

        let home = TempDir::new().unwrap();
        let root = Url::from_file_path(home.path()).unwrap();

        let registrations = initialized_registrations_with_home(
            InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root.clone(),
                    name: "home".into(),
                }]),
                initialization_options: Some(json!({
                    "crossFile": { "indexWorkspace": false },
                    "packages": { "enabled": false },
                    "linting": { "enabled": "auto", "readHomeLintr": false }
                })),
                ..Default::default()
            },
            Some(home.path()),
        )
        .await;

        let base_registration = registrations
            .iter()
            .find(|registration| registration.id == "raven-config-files")
            .cloned()
            .expect("initialized must register exact workspace config-file watchers");
        let base_options = watcher_options(base_registration);
        assert_all_watchers_cover_config_file_events(&base_options.watchers);
        assert_has_exact_watcher(&base_options.watchers, &root, "raven.toml");
        assert_has_exact_watcher(&base_options.watchers, &root, ".lintr");
    }

    #[tokio::test]
    async fn initialized_skips_external_home_lintr_watcher_when_home_lintr_disabled() {
        use serde_json::json;

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let root = Url::from_file_path(&workspace).unwrap();

        let registrations = initialized_registrations_with_home(
            InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "project".into(),
                }]),
                initialization_options: Some(json!({
                    "crossFile": { "indexWorkspace": false },
                    "packages": { "enabled": false },
                    "linting": { "enabled": "auto", "readHomeLintr": false }
                })),
                ..Default::default()
            },
            Some(home.path()),
        )
        .await;

        let home_uri = Url::from_file_path(home.path()).unwrap();
        let registration = registrations
            .into_iter()
            .find(|registration| registration.id == "raven-active-project-config-file")
            .expect("external ancestor raven.toml watcher should still be registered");
        let options = watcher_options(registration);
        assert_all_watchers_cover_config_file_events(&options.watchers);
        assert_has_exact_watcher(&options.watchers, &home_uri, "raven.toml");
        assert_lacks_exact_watcher(&options.watchers, &home_uri, ".lintr");
    }

    #[test]
    fn config_file_event_filter_ignores_nested_configs_outside_discovery_walk() {
        let tmp = TempDir::new().unwrap();
        let configured_home = TempDir::new().unwrap();
        let workspace = tmp.path().join("project");
        let nested = workspace.join("nested");
        fs::create_dir_all(&nested).unwrap();

        let options =
            crate::config_file::DiscoveryOptions::default().with_home_dir(configured_home.path());
        assert!(
            super::config_file_event_affects_project_config(
                &workspace.join("raven.toml"),
                Some(&workspace),
                &options,
            ),
            "workspace-root raven.toml is on the discovery walk"
        );
        assert!(
            !super::config_file_event_affects_project_config(
                &nested.join("raven.toml"),
                Some(&workspace),
                &options,
            ),
            "nested raven.toml is not discovered from the workspace root"
        );
        assert!(
            super::config_file_event_affects_project_config(
                &tmp.path().join(".lintr"),
                Some(&workspace),
                &options,
            ),
            "non-home ancestor .lintr is on the discovery walk"
        );
    }

    #[tokio::test]
    async fn initialized_external_watchers_include_closer_ancestor_than_active_config() {
        use serde_json::json;

        let repo = TempDir::new().unwrap();
        let nearer = repo.path().join("analysis");
        let workspace = nearer.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let lintr = repo.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let root = Url::from_file_path(&workspace).unwrap();

        let registrations = initialized_registrations(InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: "t".into(),
            }]),
            initialization_options: Some(json!({
                "crossFile": { "indexWorkspace": false },
                "packages": { "enabled": false },
                "linting": { "enabled": "auto" }
            })),
            ..Default::default()
        })
        .await;

        let active_parent_uri = Url::from_file_path(repo.path()).unwrap();
        let nearer_uri = Url::from_file_path(&nearer).unwrap();
        let registration = registrations
            .into_iter()
            .find(|registration| registration.id == "raven-active-project-config-file")
            .expect("external ancestor watchers should be registered");
        let options = watcher_options(registration);
        assert_all_watchers_cover_config_file_events(&options.watchers);
        assert_has_exact_watcher(&options.watchers, &active_parent_uri, "raven.toml");
        assert_has_exact_watcher(&options.watchers, &active_parent_uri, ".lintr");
        assert_has_exact_watcher(&options.watchers, &nearer_uri, "raven.toml");
        assert_has_exact_watcher(&options.watchers, &nearer_uri, ".lintr");
    }

    #[tokio::test]
    async fn initialized_registers_external_project_config_watcher_for_load_failed_config() {
        use serde_json::json;

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let toml_path = home.path().join("raven.toml");
        fs::write(&toml_path, "not valid = = toml [[[\n").unwrap();
        let root = Url::from_file_path(&workspace).unwrap();

        let registrations = initialized_registrations(InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: "t".into(),
            }]),
            initialization_options: Some(json!({
                "crossFile": { "indexWorkspace": false },
                "packages": { "enabled": false }
            })),
            ..Default::default()
        })
        .await;

        let parent_uri = Url::from_file_path(home.path()).unwrap();
        let registration = registrations
            .into_iter()
            .find(|registration| registration.id == "raven-active-project-config-file")
            .expect("load-failed external config should still register external watcher");
        let options = watcher_options(registration);

        assert_all_watchers_cover_config_file_events(&options.watchers);
        assert_has_exact_watcher(&options.watchers, &parent_uri, "raven.toml");
        assert_has_exact_watcher(&options.watchers, &parent_uri, ".lintr");
    }

    /// If an external project config becomes malformed, it must stop being
    /// active and reset the parsed settings that came from that project layer.
    /// Separate watcher-registration tests cover keeping external config
    /// directories watched so a later fix is not missed.
    #[tokio::test]
    async fn watched_files_reload_clears_load_failed_external_config_layer() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let toml_path = home.path().join("raven.toml");
        fs::write(&toml_path, "[linting]\nenabled = true\nlineLength = 120\n").unwrap();
        let root = Url::from_file_path(&workspace).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root,
                    name: "t".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();

        {
            let state = backend.state.read().await;
            assert_eq!(
                state.project_config_path.as_deref(),
                Some(toml_path.as_path())
            );
            assert!(state.lint_config.enabled);
            assert_eq!(state.lint_config.line_length, 120);
        }

        fs::write(&toml_path, "not valid = = toml [[[\n").unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&toml_path).unwrap(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;

        let state = backend.state.read().await;
        assert!(
            state.project_config_path.is_none(),
            "load failure must clear the active project config"
        );
        assert!(state.raw_project_settings.is_none());
        assert_eq!(state.lint_config.line_length, 80);
    }

    /// The watched-files reload path must update `state.project_config_path`
    /// so the subsequent `raven/projectConfigLoaded` notification carries
    /// the live value (rather than the path captured at initialize time).
    /// This complements the initialize-time test
    /// `initialize_loads_raven_toml_from_workspace_root`.
    #[tokio::test]
    async fn watched_files_reload_updates_project_config_path_for_notification() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let tmp = TempDir::new().unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root.clone(),
                    name: "t".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        // No config at initialize time.
        assert!(backend.state.read().await.project_config_path.is_none());

        // Create raven.toml; watched_files reload should pick it up and
        // populate project_config_path so the notification helper has a
        // fresh value to emit.
        let toml_path = tmp.path().join("raven.toml");
        fs::write(&toml_path, "[linting]\nenabled = true\n").unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&toml_path).unwrap(),
                    typ: FileChangeType::CREATED,
                }],
            })
            .await;
        assert_eq!(
            backend.state.read().await.project_config_path.as_deref(),
            Some(toml_path.as_path())
        );

        // Delete raven.toml; watched_files reload should clear the path
        // so the notification helper emits the cleared form (`path: null`).
        std::fs::remove_file(&toml_path).unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(&toml_path).unwrap(),
                    typ: FileChangeType::DELETED,
                }],
            })
            .await;
        assert!(backend.state.read().await.project_config_path.is_none());
    }

    #[tokio::test]
    async fn watched_files_reload_notifies_project_config_loaded_over_socket() {
        use futures_util::{SinkExt, StreamExt};
        use serde_json::json;
        use std::time::Duration;
        use tower::{Service, ServiceExt};
        use tower_lsp::jsonrpc::{Request, Response};
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let tmp = TempDir::new().unwrap();
        let root = Url::from_file_path(tmp.path()).unwrap();

        let (mut svc, socket) = tower_lsp::LspService::new(Backend::new);
        let initialize = Request::build("initialize")
            .id(1)
            .params(
                serde_json::to_value(InitializeParams {
                    workspace_folders: Some(vec![WorkspaceFolder {
                        uri: root,
                        name: "t".into(),
                    }]),
                    initialization_options: Some(json!({
                        "crossFile": { "indexWorkspace": false },
                        "packages": { "enabled": false },
                    })),
                    ..Default::default()
                })
                .unwrap(),
            )
            .finish();
        let response = svc.ready().await.unwrap().call(initialize).await.unwrap();
        assert!(response.is_some_and(|response| response.is_ok()));

        let toml_path = tmp.path().join("raven.toml");
        fs::write(&toml_path, "[linting]\nenabled = true\n").unwrap();

        let (mut request_stream, mut response_sink) = socket.split();
        macro_rules! expect_project_config_loaded {
            ($change_type:expr, $expected_path:expr, $expected_source:expr) => {{
                let changed = svc
                    .inner()
                    .did_change_watched_files(DidChangeWatchedFilesParams {
                        changes: vec![FileEvent {
                            uri: Url::from_file_path(&toml_path).unwrap(),
                            typ: $change_type,
                        }],
                    });
                tokio::pin!(changed);
                let mut changed_done = false;
                let mut saw_notification = false;

                loop {
                    if changed_done && saw_notification {
                        break;
                    }

                    let request = if changed_done {
                        tokio::time::timeout(Duration::from_secs(5), request_stream.next())
                            .await
                            .expect("watched-files reload should send raven/projectConfigLoaded")
                    } else {
                        tokio::select! {
                            () = &mut changed => {
                                changed_done = true;
                                continue;
                            }
                            request = tokio::time::timeout(Duration::from_secs(5), request_stream.next()) => {
                                request.expect("watched-files reload should send client requests or project config notification")
                            }
                        }
                    }
                    .expect("client socket should remain open");

                    if request.method() == "raven/projectConfigLoaded" {
                        let payload = request.params().expect("project config payload");
                        assert_eq!(payload["path"], $expected_path);
                        assert_eq!(payload["source"], $expected_source);
                        saw_notification = true;
                    }

                    if let Some(id) = request.id().cloned() {
                        response_sink
                            .send(Response::from_ok(id, serde_json::Value::Null))
                            .await
                            .unwrap();
                    }
                }
            }};
        }

        expect_project_config_loaded!(
            FileChangeType::CREATED,
            serde_json::Value::String(toml_path.display().to_string()),
            serde_json::Value::String("raven.toml".into())
        );

        fs::remove_file(&toml_path).unwrap();
        expect_project_config_loaded!(
            FileChangeType::DELETED,
            serde_json::Value::Null,
            serde_json::Value::Null
        );
    }

    /// `did_change_watched_files` publishes diagnostics for every open
    /// document on a `raven.toml` reload. With the parallel JoinSet
    /// driver, all spawned publish tasks must be joined before the
    /// handler returns — otherwise a later `did_change` could race a
    /// still-running reload publish and corrupt monotonic-version
    /// ordering.
    ///
    /// We exercise 12 open `.R` files (above the concurrency cap of 8).
    /// The reload's reconciliation helper marks every open URI for
    /// force-republish (counter increment); a successful publish
    /// decrements that counter via `try_consume_publish`. Asserting that
    /// every URI's `force_republish_count_for_test == 0` after the
    /// handler returns is a stronger signal than checking
    /// `last_published_version` — the gate's prior-publish state from
    /// `did_open` would already make `can_publish(uri, 1)` return
    /// false, so that observable could pass without the reload doing
    /// anything.
    #[tokio::test]
    async fn watched_files_reload_publishes_all_open_documents_in_parallel() {
        use tower_lsp::lsp_types::{
            DidChangeWatchedFilesParams, DidOpenTextDocumentParams, FileChangeType, FileEvent,
            TextDocumentItem,
        };

        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("raven.toml"),
            "[linting]\nenabled = true\nlineLength = 100\n",
        )
        .unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: Url::from_file_path(tmp.path()).unwrap(),
                    name: "t".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();

        const N: usize = 12;
        let mut uris: Vec<Url> = Vec::with_capacity(N);
        for i in 0..N {
            let p = tmp.path().join(format!("file_{i}.R"));
            fs::write(&p, "x <- 1\n").unwrap();
            let uri = Url::from_file_path(&p).unwrap();
            backend
                .did_open(DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "r".into(),
                        version: 1,
                        text: "x <- 1\n".into(),
                    },
                })
                .await;
            uris.push(uri);
        }

        // Sanity: pre-reload, no outstanding force-republish markers — the
        // reload is the only path under test that will mark and then
        // consume them.
        {
            let state = backend.state.read().await;
            for uri in &uris {
                assert_eq!(
                    state.diagnostics_gate.force_republish_count_for_test(uri),
                    0,
                    "unexpected pre-reload force-republish marker for {uri}"
                );
            }
        }

        // Edit raven.toml and trigger a reload.
        fs::write(
            tmp.path().join("raven.toml"),
            "[linting]\nenabled = true\nlineLength = 140\n",
        )
        .unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(tmp.path().join("raven.toml")).unwrap(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;

        // After the handler returns, every URI's force-republish marker
        // (set by the reconciliation helper) must have been consumed by
        // a successful `try_consume_publish` — a stuck or un-awaited
        // task would leave its URI's counter at 1.
        let state = backend.state.read().await;
        for uri in &uris {
            assert_eq!(
                state.diagnostics_gate.force_republish_count_for_test(uri),
                0,
                "reload's parallel publish driver left an outstanding force-republish marker for {uri}",
            );
        }
    }

    /// Characterization test for the watched-files CREATED/CHANGED async
    /// disk-read pass: a disk edit to a CLOSED file must update the
    /// dependency graph and the cross-file workspace index from disk
    /// content. Written before extracting the shared single-file resync
    /// primitive (issue #558) to pin the current behavior through the
    /// refactor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watched_file_change_updates_graph_and_index_from_disk() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helper.R"), "helper_fn <- function() 1\n").unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        fs::write(
            tmp.path().join("main.R"),
            "source(\"helper.R\")\nhelper_fn()\n",
        )
        .unwrap();
        let (svc, main_uri) =
            open_in_workspace(&tmp, "main.R", "r", "source(\"helper.R\")\nhelper_fn()\n").await;
        let backend = svc.inner();
        let helper_uri = Url::from_file_path(tmp.path().join("helper.R")).unwrap();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();

        // Sanity: opening main.R created the main -> helper edge.
        {
            let state = backend.state.read().await;
            assert!(
                state
                    .cross_file_graph
                    .get_dependencies(&main_uri)
                    .iter()
                    .any(|e| e.to == helper_uri),
                "did_open must add the main -> helper edge"
            );
        }

        // External (closed-file) disk edit: helper.R gains a source() edge.
        fs::write(
            tmp.path().join("helper.R"),
            "source(\"extra.R\")\nhelper_fn <- function() 1\n",
        )
        .unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: helper_uri.clone(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;

        // The graph mutation happens in the spawned async disk-read pass.
        let updated = wait_for_state(backend, 5_000, |state| {
            state
                .cross_file_graph
                .get_dependencies(&helper_uri)
                .iter()
                .any(|e| e.to == extra_uri)
        })
        .await;
        assert!(
            updated,
            "watched CHANGED event must add the helper -> extra edge from disk content"
        );
        let state = backend.state.read().await;
        let meta = state
            .cross_file_workspace_index
            .get_metadata(&helper_uri)
            .expect("workspace index must hold disk-derived metadata for the changed file");
        assert_eq!(
            meta.sources.len(),
            1,
            "index metadata must reflect the new on-disk source() call"
        );
    }

    /// Characterization test for the watched-files DELETED branch: deleting
    /// a closed file must drop its graph edges (including the mirror in an
    /// open parent's forward list) and its workspace-index entry, and the
    /// open dependent's force-republish marker must be consumed by the
    /// synchronous publish before the handler returns. Pins current
    /// behavior ahead of the issue #558 extraction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watched_file_deleted_removes_graph_edges_and_index_entry() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helper.R"), "helper_fn <- function() 1\n").unwrap();
        fs::write(
            tmp.path().join("main.R"),
            "source(\"helper.R\")\nhelper_fn()\n",
        )
        .unwrap();
        let (svc, main_uri) =
            open_in_workspace(&tmp, "main.R", "r", "source(\"helper.R\")\nhelper_fn()\n").await;
        let backend = svc.inner();
        let helper_uri = Url::from_file_path(tmp.path().join("helper.R")).unwrap();

        // The undefined-variable collector defers all diagnostics until the
        // workspace scan completes (Auto backward-dependency mode); the test
        // harness never runs `initialized`, so mark the scan complete.
        backend.state.write().await.workspace_scan_complete = true;

        // Sanity: the edge exists and helper_fn resolves (no diagnostics).
        {
            let state = backend.state.read().await;
            assert!(
                state
                    .cross_file_graph
                    .get_dependencies(&main_uri)
                    .iter()
                    .any(|e| e.to == helper_uri),
                "did_open must add the main -> helper edge"
            );
            assert!(
                snapshot_diagnostics(&state, &main_uri).is_empty(),
                "helper_fn must resolve through the source() edge before deletion"
            );
        }

        fs::remove_file(tmp.path().join("helper.R")).unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: helper_uri.clone(),
                    typ: FileChangeType::DELETED,
                }],
            })
            .await;

        // DELETED-only batches mutate the graph synchronously, so the
        // post-handler state is final: edges gone in both directions, index
        // entry gone, and the dependent's force-republish marker consumed by
        // the synchronous publish loop.
        let state = backend.state.read().await;
        assert!(
            !state
                .cross_file_graph
                .get_dependencies(&main_uri)
                .iter()
                .any(|e| e.to == helper_uri),
            "deletion must remove the main -> helper edge"
        );
        assert!(
            state
                .cross_file_graph
                .get_dependents(&helper_uri)
                .is_empty(),
            "deletion must clear the deleted file's backward edges"
        );
        assert!(
            state
                .cross_file_workspace_index
                .get_metadata(&helper_uri)
                .is_none(),
            "deletion must invalidate the workspace-index entry"
        );
        assert_eq!(
            state
                .diagnostics_gate
                .force_republish_count_for_test(&main_uri),
            0,
            "the dependent's force-republish marker must be consumed before the handler returns"
        );
        assert!(
            !snapshot_diagnostics(&state, &main_uri).is_empty(),
            "helper_fn must be flagged once the sourced file is gone"
        );
    }

    // ====================================================================
    // did_close disk resync (issue #558): closing a buffer must converge
    // the cross-file graph/index back to disk truth, discarding topology
    // derived from unsaved buffer edits.
    // ====================================================================

    /// Issue #558 test 1: an unsaved `source()` edge added in the buffer
    /// must disappear from the graph when the document closes — the resync
    /// reverts to disk topology (which has no such edge).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_after_unsaved_added_source_edge_reverts_to_disk_topology() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helper.R"), "x <- 1\n").unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        let (svc, helper_uri) = open_in_workspace(&tmp, "helper.R", "r", "x <- 1\n").await;
        let backend = svc.inner();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();

        // Unsaved buffer edit adds a source() edge; the open buffer is
        // authoritative, so the graph gains it.
        change_doc(backend, &helper_uri, 2, "source(\"extra.R\")\nx <- 1\n").await;
        {
            let state = backend.state.read().await;
            assert!(
                state
                    .cross_file_graph
                    .get_dependencies(&helper_uri)
                    .iter()
                    .any(|e| e.to == extra_uri),
                "unsaved buffer edit must add the helper -> extra edge"
            );
        }

        close_doc(backend, &helper_uri).await;

        let reverted = wait_for_state(backend, 5_000, |state| {
            !state
                .cross_file_graph
                .get_dependencies(&helper_uri)
                .iter()
                .any(|e| e.to == extra_uri)
        })
        .await;
        assert!(
            reverted,
            "close must revert the buffer-only source() edge to disk topology"
        );
        let state = backend.state.read().await;
        let meta = state
            .cross_file_workspace_index
            .get_metadata(&helper_uri)
            .expect("close resync must install disk-derived index metadata");
        assert!(
            meta.sources.is_empty(),
            "index metadata must reflect the on-disk content (no source() call)"
        );
    }

    /// The resync gate keys off the tracked document's `file_type`, not the
    /// URI extension, so extension-less R documents like `.Rprofile` get the
    /// close-time resync too: a buffer-only source() edge in an unsaved
    /// `.Rprofile` must revert on close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_rprofile_reverts_buffer_only_source_edge() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".Rprofile"), "options(digits = 3)\n").unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        let (svc, rprofile_uri) =
            open_in_workspace(&tmp, ".Rprofile", "r", "options(digits = 3)\n").await;
        let backend = svc.inner();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();

        change_doc(
            backend,
            &rprofile_uri,
            2,
            "source(\"extra.R\")\noptions(digits = 3)\n",
        )
        .await;
        {
            let state = backend.state.read().await;
            assert!(
                state
                    .cross_file_graph
                    .get_dependencies(&rprofile_uri)
                    .iter()
                    .any(|e| e.to == extra_uri),
                "unsaved .Rprofile edit must add the edge"
            );
        }

        close_doc(backend, &rprofile_uri).await;

        let reverted = wait_for_state(backend, 5_000, |state| {
            !state
                .cross_file_graph
                .get_dependencies(&rprofile_uri)
                .iter()
                .any(|e| e.to == extra_uri)
        })
        .await;
        assert!(
            reverted,
            ".Rprofile close must revert the buffer-only source() edge to disk topology"
        );
    }

    /// The resync enriches disk metadata with the parent's inherited working
    /// directory before committing, matching `did_open`/`did_change`: closing
    /// a `# raven: sourced-by` child whose source() paths resolve through the
    /// parent's `# raven: cd` must keep those edges rather than dropping to
    /// file-relative resolution.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_resync_preserves_inherited_working_directory_edges() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("data")).unwrap();
        fs::create_dir(tmp.path().join("scripts")).unwrap();
        fs::write(
            tmp.path().join("parent.R"),
            "# raven: cd data\nsource(\"../scripts/child.R\")\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("data").join("helper.R"),
            "helper_fn <- function() 1\n",
        )
        .unwrap();
        let child_disk = "# raven: sourced-by ../parent.R\nsource(\"helper.R\")\n";
        fs::write(tmp.path().join("scripts").join("child.R"), child_disk).unwrap();

        let (svc, child_uri) = open_in_workspace(&tmp, "scripts/child.R", "r", child_disk).await;
        let backend = svc.inner();
        let helper_uri = Url::from_file_path(tmp.path().join("data").join("helper.R")).unwrap();

        // did_open enriched the buffer's metadata with the parent's `cd`, so
        // the child's source("helper.R") resolves to data/helper.R (never
        // scripts/helper.R, which does not exist).
        {
            let state = backend.state.read().await;
            assert!(
                state
                    .cross_file_graph
                    .get_dependencies(&child_uri)
                    .iter()
                    .any(|e| e.to == helper_uri),
                "open must resolve the child's source() through the inherited working directory"
            );
        }

        close_doc(backend, &child_uri).await;

        // Wait for the resync to install disk-derived index metadata (the
        // edge itself is unchanged here, so it cannot signal completion).
        let resynced = wait_for_state(backend, 5_000, |state| {
            state
                .cross_file_workspace_index
                .get_metadata(&child_uri)
                .is_some()
        })
        .await;
        assert!(
            resynced,
            "close resync must install index metadata for the child"
        );
        let state = backend.state.read().await;
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&child_uri)
                .iter()
                .any(|e| e.to == helper_uri),
            "close resync must keep resolving through the inherited working directory"
        );
    }

    /// Issue #558 test 2: a `source()` edge removed by an unsaved buffer
    /// edit must be restored from disk when the document closes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_after_unsaved_removed_edge_restores_disk_edge() {
        let tmp = TempDir::new().unwrap();
        let disk = "source(\"extra.R\")\nx <- 1\n";
        fs::write(tmp.path().join("helper.R"), disk).unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        let (svc, helper_uri) = open_in_workspace(&tmp, "helper.R", "r", disk).await;
        let backend = svc.inner();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();

        // Unsaved buffer edit removes the source() line; the edge drops.
        change_doc(backend, &helper_uri, 2, "x <- 1\n").await;
        {
            let state = backend.state.read().await;
            assert!(
                !state
                    .cross_file_graph
                    .get_dependencies(&helper_uri)
                    .iter()
                    .any(|e| e.to == extra_uri),
                "unsaved buffer edit must drop the helper -> extra edge"
            );
        }

        close_doc(backend, &helper_uri).await;

        let restored = wait_for_state(backend, 5_000, |state| {
            state
                .cross_file_graph
                .get_dependencies(&helper_uri)
                .iter()
                .any(|e| e.to == extra_uri)
        })
        .await;
        assert!(
            restored,
            "close must restore the on-disk source() edge the unsaved edit removed"
        );
    }

    /// Issue #558 test 3 (deterministic half): when the URI is an open
    /// document, `resync_file_from_disk` must return `Vetoed` and mutate
    /// nothing — the open buffer's state stays authoritative. (With the doc
    /// open for the whole call this exercises the preflight veto; the
    /// commit-time veto is the same check replayed under the write lock, and
    /// the close-then-reopen e2e test covers the racing case.)
    #[tokio::test]
    async fn resync_vetoes_commit_when_document_is_open() {
        let tmp = TempDir::new().unwrap();
        // Disk has a source() edge; the open buffer does NOT.
        fs::write(tmp.path().join("helper.R"), "source(\"extra.R\")\nx <- 1\n").unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        let (svc, helper_uri) = open_in_workspace(&tmp, "helper.R", "r", "x <- 1\n").await;
        let backend = svc.inner();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();

        let outcome = resync_file_from_disk(&backend.state, &helper_uri, None, None, true).await;
        assert!(
            matches!(outcome, ResyncOutcome::Vetoed),
            "resync must veto while the document is open"
        );
        let state = backend.state.read().await;
        assert!(
            !state
                .cross_file_graph
                .get_dependencies(&helper_uri)
                .iter()
                .any(|e| e.to == extra_uri),
            "vetoed resync must not install disk-derived edges over buffer state"
        );
    }

    /// The staleness veto: a resync must never overwrite a NEWER snapshot a
    /// concurrent same-file resync already committed (close resync racing a
    /// watched CHANGED event — both open-document vetoes are blind to
    /// freshness). Seeding the file cache with a future-mtime snapshot
    /// simulates the fresher commit having won the race.
    #[tokio::test]
    async fn resync_skips_when_newer_snapshot_already_committed() {
        let tmp = TempDir::new().unwrap();
        // Disk content would add an edge if committed.
        fs::write(tmp.path().join("helper.R"), "source(\"extra.R\")\nx <- 1\n").unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        fs::write(tmp.path().join("other.R"), "y <- 1\n").unwrap();
        let (svc, _other_uri) = open_in_workspace(&tmp, "other.R", "r", "y <- 1\n").await;
        let backend = svc.inner();
        let helper_uri = Url::from_file_path(tmp.path().join("helper.R")).unwrap();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();

        // Simulate a concurrent resync having committed FRESHER disk state:
        // a cache snapshot whose mtime is ahead of the on-disk file's.
        let newer_content = "newer <- 1\n".to_string();
        {
            let state = backend.state.read().await;
            state.cross_file_file_cache.insert(
                helper_uri.clone(),
                crate::cross_file::file_cache::FileSnapshot {
                    mtime: std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
                    size: newer_content.len() as u64,
                    content_hash: None,
                },
                newer_content.clone(),
            );
        }

        let outcome = resync_file_from_disk(&backend.state, &helper_uri, None, None, true).await;
        assert!(
            matches!(outcome, ResyncOutcome::Skipped),
            "an older disk read must not overwrite a newer committed snapshot"
        );
        let state = backend.state.read().await;
        assert_eq!(
            state.cross_file_file_cache.get(&helper_uri).as_deref(),
            Some(newer_content.as_str()),
            "the fresher cached content must survive the stale resync"
        );
        assert!(
            !state
                .cross_file_graph
                .get_dependencies(&helper_uri)
                .iter()
                .any(|e| e.to == extra_uri),
            "the stale read's edges must not be committed"
        );
    }

    /// Opening a document invalidates its closed-file cache entry, so a
    /// poisoned (newer-mtime) snapshot from before the open cannot wrongly
    /// win the staleness veto at the next close — e.g. a `git checkout`
    /// restoring older mtimes while the buffer was open, when watcher
    /// events (and their invalidation) are skipped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_clears_cached_snapshot_so_close_resync_commits() {
        use tower_lsp::lsp_types::{DidCloseTextDocumentParams, TextDocumentIdentifier};

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helper.R"), "x <- 1\n").unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        fs::write(tmp.path().join("other.R"), "y <- 1\n").unwrap();
        let (svc, _other) = open_in_workspace(&tmp, "other.R", "r", "y <- 1\n").await;
        let backend = svc.inner();
        let helper_uri = Url::from_file_path(tmp.path().join("helper.R")).unwrap();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();

        // Poison the cache with a future-mtime snapshot (as a fresher commit
        // would leave), then open the file — the open must clear it.
        {
            let state = backend.state.read().await;
            state.cross_file_file_cache.insert(
                helper_uri.clone(),
                crate::cross_file::file_cache::FileSnapshot {
                    mtime: std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
                    size: 1,
                    content_hash: None,
                },
                "poisoned <- 1\n".to_string(),
            );
        }
        open_doc(backend, &helper_uri, "r", 1, "x <- 1\n").await;

        // Simulate the checkout: disk now has an edge; its mtime is "older"
        // than the poisoned snapshot would have been.
        fs::write(tmp.path().join("helper.R"), "source(\"extra.R\")\nx <- 1\n").unwrap();
        backend
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: helper_uri.clone(),
                },
            })
            .await;

        let committed = wait_for_state(backend, 5_000, |state| {
            state
                .cross_file_graph
                .get_dependencies(&helper_uri)
                .iter()
                .any(|e| e.to == extra_uri)
        })
        .await;
        assert!(
            committed,
            "the close resync must commit disk state — the pre-open snapshot must not veto it"
        );
    }

    /// A watched CHANGED that reads transiently-invalid UTF-8 (a non-atomic
    /// external write in progress) must NOT remove the file's cross-file
    /// state: removal would strip a closed parent's incoming edge, which
    /// nothing could restore. It skips; the completing write's next CHANGED
    /// converges.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watched_transient_invalid_encoding_does_not_remove_state() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("child.R"), "child_fn <- function() 1\n").unwrap();
        let parent = "source(\"child.R\")\nchild_fn()\n";
        fs::write(tmp.path().join("parent.R"), parent).unwrap();
        let (svc, parent_uri) = open_in_workspace(&tmp, "parent.R", "r", parent).await;
        let backend = svc.inner();
        let child_uri = Url::from_file_path(tmp.path().join("child.R")).unwrap();
        {
            let state = backend.state.read().await;
            assert!(
                state
                    .cross_file_graph
                    .get_dependencies(&parent_uri)
                    .iter()
                    .any(|e| e.to == child_uri),
                "precondition: parent -> child edge exists"
            );
        }

        // Mid-write garbage on disk, then the watcher fires.
        std::fs::write(tmp.path().join("child.R"), [0x78u8, 0x80, 0x79]).unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: child_uri.clone(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;

        // Give the async resync time to (incorrectly) act, then assert the
        // edge survived the transient garbage.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let state = backend.state.read().await;
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&parent_uri)
                .iter()
                .any(|e| e.to == child_uri),
            "transiently-undecodable watched content must not strip the parent's edge"
        );
    }

    /// The missing-file branch has its own veto: a file deleted from disk
    /// whose URI is (re)opened must not have its cross-file state removed —
    /// the open buffer is authoritative even when no disk file backs it.
    #[tokio::test]
    async fn resync_missing_file_vetoes_when_reopened() {
        let tmp = TempDir::new().unwrap();
        let content = "source(\"extra.R\")\nx <- 1\n";
        fs::write(tmp.path().join("helper.R"), content).unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        let (svc, helper_uri) = open_in_workspace(&tmp, "helper.R", "r", content).await;
        let backend = svc.inner();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();

        fs::remove_file(tmp.path().join("helper.R")).unwrap();

        // Call the missing-file branch directly (the resync entry point
        // would preflight-veto before ever reaching it) with the document
        // open, as a racing reopen would leave it.
        let outcome = resync_missing_file(&backend.state, &helper_uri).await;
        assert!(
            matches!(outcome, ResyncOutcome::Vetoed),
            "the missing-file branch must veto while the document is open"
        );
        let state = backend.state.read().await;
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&helper_uri)
                .iter()
                .any(|e| e.to == extra_uri),
            "a vetoed removal must leave the open buffer's edges intact"
        );
    }

    /// Issue #558 test 3 (end-to-end half): close-then-immediate-reopen.
    /// Whatever order the spawned resync and the reopen's `did_open` land
    /// in, the reopened buffer's topology must win — either the resync
    /// commits first and `did_open` overwrites it, or `did_open` lands
    /// first and the resync's commit-time veto fires.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_then_reopen_keeps_reopened_buffer_topology() {
        let tmp = TempDir::new().unwrap();
        // Disk has NO source() edge; the reopened buffer adds one (unsaved).
        fs::write(tmp.path().join("helper.R"), "x <- 1\n").unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        let (svc, helper_uri) = open_in_workspace(&tmp, "helper.R", "r", "x <- 1\n").await;
        let backend = svc.inner();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();

        close_doc(backend, &helper_uri).await;
        // Immediate reopen with different (unsaved) buffer content.
        open_doc(
            backend,
            &helper_uri,
            "r",
            1,
            "source(\"extra.R\")\nx <- 1\n",
        )
        .await;

        // Let the spawned resync settle (commit or veto), then assert the
        // reopened buffer's edge survived.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let state = backend.state.read().await;
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&helper_uri)
                .iter()
                .any(|e| e.to == extra_uri),
            "a stale disk commit must never clobber the reopened buffer's topology"
        );
    }

    /// Issue #558 test 4: transitive dependents (grandparent -> parent ->
    /// helper) are revalidated and republished after the close resync
    /// restores disk content the unsaved buffer had removed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_resync_revalidates_transitive_dependents() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helper.R"), "helper_fn <- function() 1\n").unwrap();
        fs::write(tmp.path().join("parent.R"), "source(\"helper.R\")\n").unwrap();
        fs::write(
            tmp.path().join("grandparent.R"),
            "source(\"parent.R\")\nhelper_fn()\n",
        )
        .unwrap();
        let (svc, grandparent_uri) = open_in_workspace(
            &tmp,
            "grandparent.R",
            "r",
            "source(\"parent.R\")\nhelper_fn()\n",
        )
        .await;
        let backend = svc.inner();
        let helper_uri = Url::from_file_path(tmp.path().join("helper.R")).unwrap();
        open_doc(backend, &helper_uri, "r", 1, "helper_fn <- function() 1\n").await;
        // The undefined-variable collector defers until the workspace scan
        // completes; the harness never runs `initialized`.
        backend.state.write().await.workspace_scan_complete = true;

        // Sanity: helper_fn resolves through the transitive chain.
        {
            let state = backend.state.read().await;
            assert!(
                snapshot_diagnostics(&state, &grandparent_uri).is_empty(),
                "helper_fn must resolve through parent -> helper before the edit"
            );
        }

        // Unsaved buffer edit deletes helper_fn; the grandparent now flags it.
        change_doc(backend, &helper_uri, 2, "x <- 1\n").await;
        {
            let state = backend.state.read().await;
            assert!(
                !snapshot_diagnostics(&state, &grandparent_uri).is_empty(),
                "deleting helper_fn in the buffer must flag the grandparent's call"
            );
        }
        // Let did_change's own dependent fanout drain before closing, so the
        // marker-consumed assertion below observes only the close resync's
        // mark/publish pair (a pending debounced publish cancelled by the
        // resync's schedule() would otherwise strand its marker).
        let drained = wait_for_state(backend, 5_000, |state| {
            state
                .diagnostics_gate
                .force_republish_count_for_test(&grandparent_uri)
                == 0
        })
        .await;
        assert!(drained, "did_change's dependent fanout must drain");

        close_doc(backend, &helper_uri).await;

        // Stage 1: the resync must MARK the transitive dependent (visible
        // during the 200ms publish debounce) — asserting only the drained
        // `== 0` end state would pass even if the publish tail never ran.
        let marked = wait_for_state(backend, 5_000, |state| {
            state
                .diagnostics_gate
                .force_republish_count_for_test(&grandparent_uri)
                >= 1
        })
        .await;
        assert!(
            marked,
            "the close resync must force-mark the transitive dependent"
        );
        // The resync restores disk content (helper_fn exists again) and
        // force-republishes the transitive dependent.
        let recovered = wait_for_state(backend, 5_000, |state| {
            snapshot_diagnostics(state, &grandparent_uri).is_empty()
        })
        .await;
        assert!(
            recovered,
            "close resync must restore disk content and revalidate the transitive dependent"
        );
        // Stage 2: the marker must be consumed by the bounded publish.
        let consumed = wait_for_state(backend, 5_000, |state| {
            state
                .diagnostics_gate
                .force_republish_count_for_test(&grandparent_uri)
                == 0
        })
        .await;
        assert!(
            consumed,
            "the dependent's force-republish marker must be consumed by the publish tail"
        );
    }

    /// Issue #558 test 5: no transient empty-edge window. Between close and
    /// the disk commit, a dependent's diagnostics must never observe the
    /// closed file with its edges stripped — the buffer-derived edges stay
    /// in place until the resync atomically replaces them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_resync_has_no_transient_empty_edge_window() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helper.R"), "helper_fn <- function() 1\n").unwrap();
        fs::write(
            tmp.path().join("parent.R"),
            "source(\"helper.R\")\nhelper_fn()\n",
        )
        .unwrap();
        let (svc, parent_uri) =
            open_in_workspace(&tmp, "parent.R", "r", "source(\"helper.R\")\nhelper_fn()\n").await;
        let backend = svc.inner();
        let helper_uri = Url::from_file_path(tmp.path().join("helper.R")).unwrap();
        open_doc(backend, &helper_uri, "r", 1, "helper_fn <- function() 1\n").await;
        backend.state.write().await.workspace_scan_complete = true;
        {
            let state = backend.state.read().await;
            assert!(
                snapshot_diagnostics(&state, &parent_uri).is_empty(),
                "helper_fn must resolve before the close"
            );
        }

        close_doc(backend, &helper_uri).await;

        // Sample the dependent's diagnostics continuously through the resync
        // window: the parent -> helper edge and helper's resolvable content
        // must be visible at every instant (buffer == disk here, so the
        // final state is identical; only a transient strip could flag).
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
        while std::time::Instant::now() < deadline {
            {
                let state = backend.state.read().await;
                assert!(
                    state
                        .cross_file_graph
                        .get_dependencies(&parent_uri)
                        .iter()
                        .any(|e| e.to == helper_uri),
                    "the parent -> helper edge must never transiently disappear"
                );
                assert!(
                    snapshot_diagnostics(&state, &parent_uri).is_empty(),
                    "the dependent must never transiently flag helper_fn during the resync"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Closing a deleted-from-disk file removes its cross-file state
    /// entirely (graph edges in both directions, index entries) instead of
    /// leaving stale buffer-derived topology behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_of_deleted_file_removes_cross_file_state() {
        let tmp = TempDir::new().unwrap();
        let content = "source(\"extra.R\")\nx <- 1\n";
        fs::write(tmp.path().join("helper.R"), content).unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        let (svc, helper_uri) = open_in_workspace(&tmp, "helper.R", "r", content).await;
        let backend = svc.inner();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();
        {
            let state = backend.state.read().await;
            assert!(
                state
                    .cross_file_graph
                    .get_dependencies(&helper_uri)
                    .iter()
                    .any(|e| e.to == extra_uri),
                "open buffer must contribute the helper -> extra edge"
            );
        }

        // Delete the underlying file while the buffer is still open, then close.
        fs::remove_file(tmp.path().join("helper.R")).unwrap();
        close_doc(backend, &helper_uri).await;

        let removed = wait_for_state(backend, 5_000, |state| {
            state
                .cross_file_graph
                .get_dependencies(&helper_uri)
                .is_empty()
                && state
                    .cross_file_workspace_index
                    .get_metadata(&helper_uri)
                    .is_none()
        })
        .await;
        assert!(
            removed,
            "closing a deleted file must remove its graph edges and index entry"
        );
    }

    /// The close resync masks disk content with the closing document's
    /// editor-language classification, not the path: a file-backed document
    /// WITHOUT an `.Rmd`/`.qmd` extension opened as `languageId: rmd` is
    /// chunk-masked while open, and the resync must not reclassify the same
    /// disk text as plain R (which would mint prose `source()` edges).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_resync_respects_language_mode_chunk_classification() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helpers.R"), "helper_fn <- function() 1\n").unwrap();
        // Both decoy targets exist, so an unmasked (plain-R) parse of the
        // prose line would resolve a real edge.
        fs::write(tmp.path().join("prose_decoy.R"), "d <- 1\n").unwrap();
        // `.Rmarkdown` is NOT matched by the path classifier (only .rmd/.qmd),
        // so only the languageId can classify this document as R Markdown.
        let content = concat!(
            "# Report\n",
            "\n",
            "source(\"prose_decoy.R\")\n",
            "\n",
            "```{r}\n",
            "source(\"helpers.R\")\n",
            "```\n",
        );
        fs::write(tmp.path().join("report.Rmarkdown"), content).unwrap();
        let (svc, report_uri) = open_in_workspace(&tmp, "report.Rmarkdown", "rmd", content).await;
        let backend = svc.inner();
        let helpers_uri = Url::from_file_path(tmp.path().join("helpers.R")).unwrap();
        let decoy_uri = Url::from_file_path(tmp.path().join("prose_decoy.R")).unwrap();

        // Sanity: while open (languageId-classified, masked), only the chunk
        // edge exists.
        {
            let state = backend.state.read().await;
            let deps = state.cross_file_graph.get_dependencies(&report_uri);
            assert!(
                deps.iter().any(|e| e.to == helpers_uri),
                "chunk source() must produce an edge while open"
            );
            assert!(
                !deps.iter().any(|e| e.to == decoy_uri),
                "prose source() must not produce an edge while open"
            );
        }

        close_doc(backend, &report_uri).await;

        // The resync installs disk-derived index metadata; wait for it, then
        // assert the classification survived the close.
        let resynced = wait_for_state(backend, 5_000, |state| {
            state
                .cross_file_workspace_index
                .get_metadata(&report_uri)
                .is_some()
        })
        .await;
        assert!(resynced, "close resync must install index metadata");
        let state = backend.state.read().await;
        let deps = state.cross_file_graph.get_dependencies(&report_uri);
        assert!(
            deps.iter().any(|e| e.to == helpers_uri),
            "the chunk edge must survive the close resync"
        );
        assert!(
            !deps.iter().any(|e| e.to == decoy_uri),
            "the resync must keep the editor-language chunk masking — prose must never become an edge"
        );
    }

    /// An open file that is BOTH a package-visibility sibling and a graph
    /// dependent of the closing document must be force-marked exactly once:
    /// the sibling fanout rides the close resync instead of being marked and
    /// scheduled separately (a second schedule cancels the first pending
    /// debounced task and strands its marker). A second, package-ONLY
    /// sibling (`c.R`, no graph edge to the closed file) pins the fold
    /// itself: it can only be marked through `package_sibling_fanout`, so an
    /// implementation that drops the sibling set from `run_close_resync`
    /// fails its stage-1 assertion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_marks_overlapping_package_sibling_and_dependent_once() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        fs::write(tmp.path().join("R").join("a.R"), "a_fn <- function() 1\n").unwrap();
        let b_disk = "source(\"a.R\")\nb_fn <- function() 2\n";
        fs::write(tmp.path().join("R").join("b.R"), b_disk).unwrap();
        // Package-only sibling: open R/ file with NO edge to a.R.
        let c_disk = "c_fn <- function() 3\n";
        fs::write(tmp.path().join("R").join("c.R"), c_disk).unwrap();
        let (svc, b_uri) = open_in_workspace(&tmp, "R/b.R", "r", b_disk).await;
        let backend = svc.inner();
        let a_uri = Url::from_file_path(tmp.path().join("R").join("a.R")).unwrap();
        let c_uri = Url::from_file_path(tmp.path().join("R").join("c.R")).unwrap();
        open_doc(backend, &a_uri, "r", 1, "a_fn <- function() 1\n").await;
        open_doc(backend, &c_uri, "r", 1, c_disk).await;

        // Activate REAL package mode (a bare workspace_root without a
        // DESCRIPTION never derives a scope contribution, so
        // pkg_visibility_changed would stay false and the sibling fanout
        // would be empty), then make a's close change package visibility
        // (buffer-only symbol discarded on close).
        {
            let mut state = backend.state.write().await;
            state.package_inputs.workspace_root = Some(tmp.path().to_path_buf());
            state.package_inputs.description = Some(crate::package_state::DescriptionInput {
                text: "Package: testpkg\nVersion: 1.0\n".into(),
            });
            state.apply_package_event(&crate::package_state::PackageInputDelta::Initial);
        }
        change_doc(
            backend,
            &a_uri,
            2,
            "a_fn <- function() 1\nbuffer_only_fn <- function() 2\n",
        )
        .await;
        // Drain the open/change fanout so the assertions below observe only
        // the close's marking.
        let drained = wait_for_state(backend, 5_000, |state| {
            state
                .diagnostics_gate
                .force_republish_count_for_test(&b_uri)
                == 0
                && state
                    .diagnostics_gate
                    .force_republish_count_for_test(&c_uri)
                    == 0
        })
        .await;
        assert!(
            drained,
            "the open/change fanout must drain before the close"
        );

        close_doc(backend, &a_uri).await;

        // b.R is in the sibling fanout (open R/ file, package visibility
        // changed) AND in the resync's dependent walk (it sources a.R);
        // c.R is in the sibling fanout ONLY. Stage 1: observe the resync
        // actually MARK both (the publish rides a 200ms debounce after
        // marking, so the counters are visibly >= 1 in between) — without
        // this the final `== 0` would pass vacuously, since the counters
        // were drained before the close. The c.R half fails if the sibling
        // set is dropped from `run_close_resync` (nothing else marks a
        // package-only sibling).
        let marked = wait_for_state(backend, 5_000, |state| {
            state
                .diagnostics_gate
                .force_republish_count_for_test(&b_uri)
                >= 1
        })
        .await;
        assert!(
            marked,
            "the close resync must force-mark the overlapping sibling/dependent"
        );
        let sibling_marked = wait_for_state(backend, 5_000, |state| {
            state
                .diagnostics_gate
                .force_republish_count_for_test(&c_uri)
                >= 1
        })
        .await;
        assert!(
            sibling_marked,
            "the close resync must force-mark the package-only sibling (the fold itself)"
        );
        // Stage 2: each single publish must consume each single marker. A
        // double mark (sibling path + resync path) would leave a counter
        // stuck at 1 after its publish.
        let settled = wait_for_state(backend, 5_000, |state| {
            state
                .diagnostics_gate
                .force_republish_count_for_test(&b_uri)
                == 0
                && state
                    .diagnostics_gate
                    .force_republish_count_for_test(&c_uri)
                    == 0
        })
        .await;
        assert!(
            settled,
            "both siblings must be force-marked exactly once and consumed"
        );
    }

    /// Exists -> missing -> exists: deleting a sourced file strips the open
    /// parent's edge (removal is total), and recreating the file must
    /// restore it — the recreated child's own metadata cannot re-derive an
    /// edge owned by PARENT metadata, so the resync re-runs the open
    /// parents' graph updates.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recreated_file_restores_open_parent_edges() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("child.R"), "child_fn <- function() 1\n").unwrap();
        let parent = "source(\"child.R\")\nchild_fn()\n";
        fs::write(tmp.path().join("parent.R"), parent).unwrap();
        let (svc, parent_uri) = open_in_workspace(&tmp, "parent.R", "r", parent).await;
        let backend = svc.inner();
        let child_uri = Url::from_file_path(tmp.path().join("child.R")).unwrap();
        {
            let state = backend.state.read().await;
            assert!(
                state
                    .cross_file_graph
                    .get_dependencies(&parent_uri)
                    .iter()
                    .any(|e| e.to == child_uri),
                "precondition: the parent -> child edge exists"
            );
        }

        // Delete: the DELETED branch removes ALL of child's edges, including
        // the parent-owned one.
        fs::remove_file(tmp.path().join("child.R")).unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: child_uri.clone(),
                    typ: FileChangeType::DELETED,
                }],
            })
            .await;
        {
            let state = backend.state.read().await;
            assert!(
                !state
                    .cross_file_graph
                    .get_dependencies(&parent_uri)
                    .iter()
                    .any(|e| e.to == child_uri),
                "deletion must strip the parent -> child edge"
            );
        }

        // Recreate: the resync must restore the open parent's edge.
        fs::write(tmp.path().join("child.R"), "child_fn <- function() 1\n").unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: child_uri.clone(),
                    typ: FileChangeType::CREATED,
                }],
            })
            .await;
        let restored = wait_for_state(backend, 5_000, |state| {
            state
                .cross_file_graph
                .get_dependencies(&parent_uri)
                .iter()
                .any(|e| e.to == child_uri)
        })
        .await;
        assert!(
            restored,
            "recreating the file must restore the open parent's source() edge"
        );
    }

    /// The legacy startup-scan document map (`state.workspace_index`) is a
    /// content-provider tier above the file cache; the resync must drop its
    /// entry or stale scan-time content shadows the fresh disk commit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_resync_drops_stale_legacy_workspace_index_entry() {
        use tower_lsp::lsp_types::{DidCloseTextDocumentParams, TextDocumentIdentifier};

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helper.R"), "helper_fn <- function() 1\n").unwrap();
        let (svc, helper_uri) =
            open_in_workspace(&tmp, "helper.R", "r", "helper_fn <- function() 1\n").await;
        let backend = svc.inner();

        // Plant a stale legacy-index entry, as the startup scan would have.
        backend.state.write().await.workspace_index.insert(
            helper_uri.clone(),
            crate::state::Document::new("old_fn <- function() 1\n", None),
        );

        backend
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: helper_uri.clone(),
                },
            })
            .await;

        let dropped = wait_for_state(backend, 5_000, |state| {
            !state.workspace_index.contains_key(&helper_uri)
        })
        .await;
        assert!(
            dropped,
            "the resync commit must drop the stale legacy workspace-index entry"
        );
    }

    /// A file whose disk content became undecodable converges like a missing
    /// file on close: the package input side already removes it
    /// (`on_disk_text: None`), and the cross-file side must not strand the
    /// discarded buffer's graph entry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_of_undecodable_file_removes_cross_file_state() {
        use tower_lsp::lsp_types::{DidCloseTextDocumentParams, TextDocumentIdentifier};

        let tmp = TempDir::new().unwrap();
        let content = "source(\"extra.R\")\nx <- 1\n";
        fs::write(tmp.path().join("helper.R"), content).unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        let (svc, helper_uri) = open_in_workspace(&tmp, "helper.R", "r", content).await;
        let backend = svc.inner();
        {
            let state = backend.state.read().await;
            assert!(
                !state
                    .cross_file_graph
                    .get_dependencies(&helper_uri)
                    .is_empty(),
                "precondition: the buffer's edge exists"
            );
        }

        // Replace the disk content with invalid UTF-8 (no BOM: a lone
        // continuation byte), then close.
        std::fs::write(tmp.path().join("helper.R"), [0x78u8, 0x80, 0x79]).unwrap();
        backend
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: helper_uri.clone(),
                },
            })
            .await;

        let removed = wait_for_state(backend, 5_000, |state| {
            state
                .cross_file_graph
                .get_dependencies(&helper_uri)
                .is_empty()
                && state
                    .cross_file_workspace_index
                    .get_metadata(&helper_uri)
                    .is_none()
        })
        .await;
        assert!(
            removed,
            "undecodable disk content must converge like a missing file"
        );
    }

    /// Watched CHANGED/DELETED events for CLOSED `.Rmd` documents flow
    /// through the resync (the old skip guard pre-dated masked resyncs):
    /// external edits update the chunk-derived edges, and deletion removes
    /// the state the close resync installed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watched_events_update_and_remove_closed_rmd_state() {
        use tower_lsp::lsp_types::{
            DidChangeWatchedFilesParams, DidCloseTextDocumentParams, FileChangeType, FileEvent,
            TextDocumentIdentifier,
        };

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helpers.R"), "helper_fn <- function() 1\n").unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        fs::write(tmp.path().join("prose_decoy.R"), "d <- 1\n").unwrap();
        let rmd_v1 = concat!(
            "# Report\n",
            "\n",
            "source(\"prose_decoy.R\")\n",
            "\n",
            "```{r}\n",
            "source(\"helpers.R\")\n",
            "```\n",
        );
        fs::write(tmp.path().join("analysis.Rmd"), rmd_v1).unwrap();
        let (svc, rmd_uri) = open_in_workspace(&tmp, "analysis.Rmd", "rmd", rmd_v1).await;
        let backend = svc.inner();
        let helpers_uri = Url::from_file_path(tmp.path().join("helpers.R")).unwrap();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();
        let decoy_uri = Url::from_file_path(tmp.path().join("prose_decoy.R")).unwrap();

        // Close (installs disk-derived state), then edit the CLOSED file on
        // disk, re-pointing the chunk edge.
        backend
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: rmd_uri.clone(),
                },
            })
            .await;
        let installed = wait_for_state(backend, 5_000, |state| {
            state
                .cross_file_graph
                .get_dependencies(&rmd_uri)
                .iter()
                .any(|e| e.to == helpers_uri)
        })
        .await;
        assert!(installed, "close resync must install the chunk edge");

        let rmd_v2 = concat!(
            "# Report\n",
            "\n",
            "source(\"prose_decoy.R\")\n",
            "\n",
            "```{r}\n",
            "source(\"extra.R\")\n",
            "```\n",
        );
        fs::write(tmp.path().join("analysis.Rmd"), rmd_v2).unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: rmd_uri.clone(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;
        let updated = wait_for_state(backend, 5_000, |state| {
            let deps = state.cross_file_graph.get_dependencies(&rmd_uri);
            deps.iter().any(|e| e.to == extra_uri) && !deps.iter().any(|e| e.to == helpers_uri)
        })
        .await;
        assert!(
            updated,
            "a watched CHANGED for a closed Rmd must update its chunk edge"
        );
        {
            let state = backend.state.read().await;
            assert!(
                !state
                    .cross_file_graph
                    .get_dependencies(&rmd_uri)
                    .iter()
                    .any(|e| e.to == decoy_uri),
                "prose source() must stay masked through the watched resync"
            );
        }

        // Delete the closed Rmd: its installed state must be removed.
        fs::remove_file(tmp.path().join("analysis.Rmd")).unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: rmd_uri.clone(),
                    typ: FileChangeType::DELETED,
                }],
            })
            .await;
        let state = backend.state.read().await;
        assert!(
            state.cross_file_graph.get_dependencies(&rmd_uri).is_empty(),
            "deleting a closed Rmd must remove its graph state"
        );
        assert!(
            state
                .cross_file_workspace_index
                .get_metadata(&rmd_uri)
                .is_none(),
            "deleting a closed Rmd must remove its index entry"
        );
    }

    /// A close-time package-visibility change must reach `load_all()`
    /// carriers (root-level scripts that are not `is_r_source_path`), the
    /// same widening every watcher-driven visibility fanout applies.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_visibility_fanout_reaches_load_all_carriers() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        fs::write(tmp.path().join("R").join("a.R"), "a_fn <- function() 1\n").unwrap();
        let analysis = "devtools::load_all()\na_fn()\n";
        fs::write(tmp.path().join("analysis.R"), analysis).unwrap();
        let (svc, analysis_uri) = open_in_workspace(&tmp, "analysis.R", "r", analysis).await;
        let backend = svc.inner();
        let a_uri = Url::from_file_path(tmp.path().join("R").join("a.R")).unwrap();
        open_doc(backend, &a_uri, "r", 1, "a_fn <- function() 1\n").await;

        {
            let mut state = backend.state.write().await;
            state.package_inputs.workspace_root = Some(tmp.path().to_path_buf());
            state.package_inputs.description = Some(crate::package_state::DescriptionInput {
                text: "Package: testpkg\nVersion: 1.0\n".into(),
            });
            state.apply_package_event(&crate::package_state::PackageInputDelta::Initial);
        }
        change_doc(
            backend,
            &a_uri,
            2,
            "a_fn <- function() 1\nbuffer_only_fn <- function() 2\n",
        )
        .await;
        let drained = wait_for_state(backend, 5_000, |state| {
            state
                .diagnostics_gate
                .force_republish_count_for_test(&analysis_uri)
                == 0
        })
        .await;
        assert!(drained, "pre-close fanout must drain");

        close_doc(backend, &a_uri).await;

        // The carrier is NOT is_r_source_path and has no graph edge to a.R —
        // only the load_all widening can mark it.
        let marked = wait_for_state(backend, 5_000, |state| {
            state
                .diagnostics_gate
                .force_republish_count_for_test(&analysis_uri)
                >= 1
        })
        .await;
        assert!(
            marked,
            "the close visibility fanout must reach the load_all carrier"
        );
        let settled = wait_for_state(backend, 5_000, |state| {
            state
                .diagnostics_gate
                .force_republish_count_for_test(&analysis_uri)
                == 0
        })
        .await;
        assert!(settled, "the carrier's marker must be consumed");
    }

    /// Closing a dirty package source reverts its package input to DISK
    /// truth: symbols that existed only in the discarded buffer must leave
    /// `package_inputs.r_files` (previously the close event installed the
    /// buffer text, stranding buffer-only symbols until a watcher event that
    /// never comes — disk was never touched).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_reverts_package_input_to_disk_text() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        let disk = "pkg_fn <- function() 1\n";
        fs::write(tmp.path().join("R").join("helper.R"), disk).unwrap();
        let (svc, helper_uri) = open_in_workspace(&tmp, "R/helper.R", "r", disk).await;
        let backend = svc.inner();
        let helper_path = tmp.path().join("R").join("helper.R");

        // Activate package mode after open; the did_change below then routes
        // through the package event pipeline.
        backend.state.write().await.package_inputs.workspace_root = Some(tmp.path().to_path_buf());

        change_doc(
            backend,
            &helper_uri,
            2,
            "pkg_fn <- function() 1\nbuffer_only_fn <- function() 2\n",
        )
        .await;
        {
            let state = backend.state.read().await;
            let input = state
                .package_inputs
                .r_files
                .get(&helper_path)
                .expect("did_change must install the buffer text as a package input");
            assert!(
                input.text.contains("buffer_only_fn"),
                "precondition: the live buffer text carries the buffer-only symbol"
            );
        }

        close_doc(backend, &helper_uri).await;

        // The DidClose translate runs synchronously inside did_close's write
        // lock, so the post-close state is final.
        let state = backend.state.read().await;
        let input = state
            .package_inputs
            .r_files
            .get(&helper_path)
            .expect("the file still exists on disk, so its input must remain");
        assert_eq!(
            input.text.as_ref(),
            disk,
            "close must revert the package input to exactly the disk text"
        );
    }

    /// Closing a package source whose disk file was deleted while the buffer
    /// was open removes it from `package_inputs.r_files` — the watcher's
    /// DELETED event was skipped ("open docs are authoritative"), so the
    /// close is the last chance to converge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_of_deleted_package_source_removes_package_input() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        let disk = "pkg_fn <- function() 1\n";
        fs::write(tmp.path().join("R").join("helper.R"), disk).unwrap();
        let (svc, helper_uri) = open_in_workspace(&tmp, "R/helper.R", "r", disk).await;
        let backend = svc.inner();
        let helper_path = tmp.path().join("R").join("helper.R");

        backend.state.write().await.package_inputs.workspace_root = Some(tmp.path().to_path_buf());
        // Seed the input through the live pipeline (buffer text).
        change_doc(backend, &helper_uri, 2, "pkg_fn <- function() 1\nx <- 1\n").await;
        assert!(
            backend
                .state
                .read()
                .await
                .package_inputs
                .r_files
                .contains_key(&helper_path),
            "precondition: the open buffer's input is tracked"
        );

        fs::remove_file(&helper_path).unwrap();
        close_doc(backend, &helper_uri).await;

        let state = backend.state.read().await;
        assert!(
            !state.package_inputs.r_files.contains_key(&helper_path),
            "closing a disk-deleted package source must drop its package input"
        );
    }

    /// A stale `workspace_index_new` entry (planted by the startup scan or
    /// on-demand indexing) outranks `cross_file_workspace_index` in
    /// `get_enriched_metadata` and the content provider, so the resync's
    /// commit must invalidate it — otherwise the freshly committed disk
    /// state is shadowed and dependents keep resolving the pre-resync
    /// interface.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_resync_overrides_stale_unified_index_entry() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helper.R"), "helper_fn <- function() 1\n").unwrap();
        fs::write(
            tmp.path().join("parent.R"),
            "source(\"helper.R\")\nhelper_fn()\n",
        )
        .unwrap();
        let (svc, parent_uri) =
            open_in_workspace(&tmp, "parent.R", "r", "source(\"helper.R\")\nhelper_fn()\n").await;
        let backend = svc.inner();
        let helper_uri = Url::from_file_path(tmp.path().join("helper.R")).unwrap();
        backend.state.write().await.workspace_scan_complete = true;

        // Plant a STALE unified-index entry for helper.R, simulating a scan
        // snapshot taken before the file's current disk content (it defines
        // old_fn, not helper_fn).
        {
            let stale = "old_fn <- function() 1\n";
            let tree = crate::parser_pool::with_parser(|p| p.parse(stale, None));
            let metadata = std::sync::Arc::new(crate::cross_file::extract_metadata(stale));
            let artifacts = std::sync::Arc::new(match tree.as_ref() {
                Some(t) => crate::cross_file::scope::compute_artifacts_with_metadata(
                    &helper_uri,
                    t,
                    stale,
                    Some(&metadata),
                ),
                None => crate::cross_file::scope::ScopeArtifacts::default(),
            });
            let fs_meta = std::fs::metadata(tmp.path().join("helper.R")).unwrap();
            let entry = crate::workspace_index::IndexEntry {
                contents: ropey::Rope::from_str(stale),
                tree,
                loaded_packages: Vec::new(),
                snapshot: crate::cross_file::file_cache::FileSnapshot::with_content_hash(
                    &fs_meta, stale,
                ),
                metadata,
                artifacts,
                indexed_at_version: 0,
            };
            let state = backend.state.read().await;
            state.workspace_index_new.insert(helper_uri.clone(), entry);
        }
        // Precondition: the stale entry shadows every lower tier, so the
        // parent cannot resolve helper_fn.
        {
            let state = backend.state.read().await;
            assert!(
                !snapshot_diagnostics(&state, &parent_uri).is_empty(),
                "the stale unified-index entry must shadow lower tiers (test precondition)"
            );
        }

        // Open + close helper.R (buffer identical to disk): the close resync
        // must drop the stale unified-index entry so the fresh disk commit
        // becomes visible to the dependent.
        open_doc(backend, &helper_uri, "r", 1, "helper_fn <- function() 1\n").await;
        close_doc(backend, &helper_uri).await;

        let recovered = wait_for_state(backend, 5_000, |state| {
            snapshot_diagnostics(state, &parent_uri).is_empty()
        })
        .await;
        assert!(
            recovered,
            "close resync must invalidate the stale unified-index entry that shadows disk state"
        );
    }

    /// Rmd/Quarto close resync goes through the masking pipeline: disk
    /// metadata comes from chunk bodies only, so an unsaved chunk edit
    /// reverts and prose `source()` mentions never become edges.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_resync_rmd_uses_masked_disk_content() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helpers.R"), "helper_fn <- function() 1\n").unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        // The prose decoy target exists on disk, so an unmasked parse WOULD
        // resolve it into an edge — masking is what keeps it out.
        fs::write(tmp.path().join("prose_decoy.R"), "d <- 1\n").unwrap();
        let rmd_disk = concat!(
            "# Report\n",
            "\n",
            "Prose calls source(\"prose_decoy.R\") but that is not R.\n",
            "\n",
            "```{r}\n",
            "source(\"helpers.R\")\n",
            "```\n",
        );
        fs::write(tmp.path().join("analysis.Rmd"), rmd_disk).unwrap();
        let (svc, rmd_uri) = open_in_workspace(&tmp, "analysis.Rmd", "rmd", rmd_disk).await;
        let backend = svc.inner();
        let helpers_uri = Url::from_file_path(tmp.path().join("helpers.R")).unwrap();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();
        let decoy_uri = Url::from_file_path(tmp.path().join("prose_decoy.R")).unwrap();

        // Unsaved chunk edit swaps the source() target to extra.R.
        let rmd_buffer = concat!(
            "# Report\n",
            "\n",
            "Prose calls source(\"prose_decoy.R\") but that is not R.\n",
            "\n",
            "```{r}\n",
            "source(\"extra.R\")\n",
            "```\n",
        );
        change_doc(backend, &rmd_uri, 2, rmd_buffer).await;
        {
            let state = backend.state.read().await;
            assert!(
                state
                    .cross_file_graph
                    .get_dependencies(&rmd_uri)
                    .iter()
                    .any(|e| e.to == extra_uri),
                "unsaved chunk edit must point the edge at extra.R"
            );
        }

        close_doc(backend, &rmd_uri).await;

        let reverted = wait_for_state(backend, 5_000, |state| {
            let deps = state.cross_file_graph.get_dependencies(&rmd_uri);
            deps.iter().any(|e| e.to == helpers_uri) && !deps.iter().any(|e| e.to == extra_uri)
        })
        .await;
        assert!(
            reverted,
            "close must revert the Rmd's edge to the on-disk chunk target"
        );
        let state = backend.state.read().await;
        assert!(
            !state
                .cross_file_graph
                .get_dependencies(&rmd_uri)
                .iter()
                .any(|e| e.to == decoy_uri),
            "prose source() mentions must never become edges (masked extraction)"
        );
    }

    /// A chunkless Rmd on disk resyncs to empty analysis metadata: prose
    /// `source()` mentions never become edges, and a chunk edge that existed
    /// only in the discarded buffer disappears.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_resync_chunkless_rmd_yields_no_edges() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        fs::write(tmp.path().join("prose_decoy.R"), "d <- 1\n").unwrap();
        // Disk version has NO chunks — only prose that mentions source().
        let rmd_disk = concat!(
            "# Report\n",
            "\n",
            "Prose calls source(\"prose_decoy.R\") but that is not R.\n",
        );
        fs::write(tmp.path().join("analysis.Rmd"), rmd_disk).unwrap();
        let (svc, rmd_uri) = open_in_workspace(&tmp, "analysis.Rmd", "rmd", rmd_disk).await;
        let backend = svc.inner();

        // Unsaved buffer edit adds a chunk with a real source() edge.
        let rmd_buffer = concat!(
            "# Report\n",
            "\n",
            "Prose calls source(\"prose_decoy.R\") but that is not R.\n",
            "\n",
            "```{r}\n",
            "source(\"extra.R\")\n",
            "```\n",
        );
        change_doc(backend, &rmd_uri, 2, rmd_buffer).await;
        {
            let state = backend.state.read().await;
            assert!(
                !state.cross_file_graph.get_dependencies(&rmd_uri).is_empty(),
                "unsaved chunk edit must add an edge"
            );
        }

        close_doc(backend, &rmd_uri).await;

        let emptied = wait_for_state(backend, 5_000, |state| {
            state.cross_file_graph.get_dependencies(&rmd_uri).is_empty()
        })
        .await;
        assert!(
            emptied,
            "chunkless disk content must resync to zero edges (prose never counts)"
        );
    }

    /// A DELETED-only watcher batch with multiple files removes every file's
    /// cross-file state in one pass and settles the open dependent's
    /// force-republish marker before the handler returns (pins/package state
    /// recompute once per batch, after the loop).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watched_files_deleted_batch_removes_all_files() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.R"), "a_fn <- function() 1\n").unwrap();
        fs::write(tmp.path().join("b.R"), "b_fn <- function() 2\n").unwrap();
        let main = "source(\"a.R\")\nsource(\"b.R\")\na_fn()\nb_fn()\n";
        fs::write(tmp.path().join("main.R"), main).unwrap();
        let (svc, main_uri) = open_in_workspace(&tmp, "main.R", "r", main).await;
        let backend = svc.inner();
        let a_uri = Url::from_file_path(tmp.path().join("a.R")).unwrap();
        let b_uri = Url::from_file_path(tmp.path().join("b.R")).unwrap();
        {
            let state = backend.state.read().await;
            assert_eq!(
                state.cross_file_graph.get_dependencies(&main_uri).len(),
                2,
                "both edges must exist before the deletions"
            );
        }

        fs::remove_file(tmp.path().join("a.R")).unwrap();
        fs::remove_file(tmp.path().join("b.R")).unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![
                    FileEvent {
                        uri: a_uri.clone(),
                        typ: FileChangeType::DELETED,
                    },
                    FileEvent {
                        uri: b_uri.clone(),
                        typ: FileChangeType::DELETED,
                    },
                ],
            })
            .await;

        let state = backend.state.read().await;
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&main_uri)
                .is_empty(),
            "both deleted files' edges must be removed in the one batch"
        );
        for uri in [&a_uri, &b_uri] {
            assert!(
                state.cross_file_workspace_index.get_metadata(uri).is_none(),
                "index entry for {uri} must be invalidated"
            );
        }
        assert_eq!(
            state
                .diagnostics_gate
                .force_republish_count_for_test(&main_uri),
            0,
            "the dependent's force-republish marker must be consumed synchronously"
        );
    }

    /// Regression guard for the reopen race the shared-resync extraction
    /// closed: a watched CHANGED event racing a `did_open` of the same file
    /// must never clobber the fresh buffer's topology with stale disk state.
    /// (Pre-extraction, the async pass's graph update had no open-at-commit
    /// check; the workspace index self-guarded but the graph did not.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watched_file_reopen_during_async_read_does_not_clobber_graph() {
        use tower_lsp::lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let tmp = TempDir::new().unwrap();
        // Disk gains a source() edge; the buffer opened mid-flight has none.
        fs::write(tmp.path().join("helper.R"), "source(\"extra.R\")\nx <- 1\n").unwrap();
        fs::write(tmp.path().join("extra.R"), "extra_fn <- function() 2\n").unwrap();
        fs::write(tmp.path().join("other.R"), "y <- 1\n").unwrap();
        // Open an unrelated file so the backend is initialized the usual way.
        let (svc, _other_uri) = open_in_workspace(&tmp, "other.R", "r", "y <- 1\n").await;
        let backend = svc.inner();
        let helper_uri = Url::from_file_path(tmp.path().join("helper.R")).unwrap();
        let extra_uri = Url::from_file_path(tmp.path().join("extra.R")).unwrap();

        // Fire the watched event (async disk read spawns), then immediately
        // open the file with buffer content that has NO edge.
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: helper_uri.clone(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;
        open_doc(backend, &helper_uri, "r", 1, "x <- 1\n").await;

        // Whichever side wins the race, the open buffer's (edge-free)
        // topology must be the settled state.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let state = backend.state.read().await;
        assert!(
            !state
                .cross_file_graph
                .get_dependencies(&helper_uri)
                .iter()
                .any(|e| e.to == extra_uri),
            "a stale watched-file disk commit must never clobber an open buffer's topology"
        );
    }

    /// The `raven/projectConfigLoaded` payload schema is a contract with
    /// the VS Code extension. Lock it down: `path: string` ⇒
    /// `source: "raven.toml" | ".lintr"`; `path: None` ⇒ both fields
    /// JSON `null`. The `path` field uses `Path::display()`, which is
    /// platform-dependent — build the test paths from a tempdir so the
    /// assertion stays correct on both Unix and Windows.
    #[test]
    fn project_config_loaded_payload_shape() {
        let tmp = TempDir::new().unwrap();

        let raven_toml = tmp.path().join("raven.toml");
        let payload = super::build_project_config_loaded_payload(Some(&raven_toml));
        assert_eq!(
            payload["path"].as_str(),
            Some(raven_toml.display().to_string().as_str())
        );
        assert_eq!(payload["source"].as_str(), Some("raven.toml"));

        let lintr = tmp.path().join(".lintr");
        let payload = super::build_project_config_loaded_payload(Some(&lintr));
        assert_eq!(
            payload["path"].as_str(),
            Some(lintr.display().to_string().as_str())
        );
        assert_eq!(payload["source"].as_str(), Some(".lintr"));

        let payload = super::build_project_config_loaded_payload(None);
        assert!(payload["path"].is_null());
        assert!(payload["source"].is_null());
    }

    /// `DiagnosticsSnapshot::build` (the snapshot site in `handlers.rs` that
    /// every diagnostics pass takes) must apply per-document
    /// `[[linting.overrides]]` patches when computing the effective
    /// `LintConfig`. This exercises the handlers-site refactor: two files
    /// under the same project produce snapshots with different
    /// `lint_config.line_length` values depending on which override globs
    /// they match.
    #[tokio::test]
    async fn published_diagnostics_use_per_file_override() {
        use tower_lsp::lsp_types::{DidOpenTextDocumentParams, TextDocumentItem};

        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("raven.toml"),
            r#"
[linting]
enabled = true
lineLength = 30
lineLengthSeverity = "warning"

[[linting.overrides]]
files = ["tests/**/*.R"]
lineLength = 200
"#,
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("tests")).unwrap();
        fs::create_dir_all(tmp.path().join("R")).unwrap();
        let r_path = tmp.path().join("R/a.R");
        let test_path = tmp.path().join("tests/test-a.R");
        // 80-column line: triggers in R/ (line_length = 30), not in tests/ (200).
        let long_line =
            "x_long_identifier <- 'sample value with a longer literal string' ; cat('hi')\n";
        fs::write(&r_path, long_line).unwrap();
        fs::write(&test_path, long_line).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: Url::from_file_path(tmp.path()).unwrap(),
                    name: "t".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();

        let r_uri = Url::from_file_path(&r_path).unwrap();
        let test_uri = Url::from_file_path(&test_path).unwrap();

        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: r_uri.clone(),
                    language_id: "r".into(),
                    version: 1,
                    text: long_line.into(),
                },
            })
            .await;
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: test_uri.clone(),
                    language_id: "r".into(),
                    version: 1,
                    text: long_line.into(),
                },
            })
            .await;

        // Build the snapshots that handlers.rs would build for each URI and
        // assert their effective LintConfig. This is the same struct +
        // build path that `handlers.rs:DiagnosticsSnapshot::build` takes
        // during a diagnostics pass.
        let state = backend.state.read().await;
        let r_snap = crate::handlers::DiagnosticsSnapshot::build(&state, &r_uri)
            .expect("snapshot built for R/a.R");
        let test_snap = crate::handlers::DiagnosticsSnapshot::build(&state, &test_uri)
            .expect("snapshot built for tests/test-a.R");
        assert_eq!(r_snap.lint_config.line_length, 30);
        assert_eq!(test_snap.lint_config.line_length, 200);
    }

    /// Issue #343: an open Rmd document must flow through the publish-path
    /// dispatch (`publish_diagnostics_inner` Phase 1 build + Phase 2 match
    /// guard) and produce chunk diagnostics. The document is opened via the
    /// real `did_open` path, whose DocumentStore entry now stores
    /// masked-derived metadata/tree (#343 Task 5), so the snapshot is
    /// masked-correct: diagnostics fall only on R chunk-body lines, never
    /// prose. This mirrors the exact match arm in `publish_diagnostics_inner`:
    /// build the snapshot, then run `diagnostics_from_snapshot` iff
    /// `file_type == R`.
    #[tokio::test]
    async fn rmd_document_flows_through_publish_path_and_flags_chunk_error() {
        use tower_lsp::lsp_types::{DidOpenTextDocumentParams, TextDocumentItem};

        let tmp = TempDir::new().unwrap();
        let rmd_path = tmp.path().join("report.Rmd");
        // Prose + a chunk with a syntax error (`x <- (`) on document line 5.
        let content = "# Title\n\nSome [prose](url) here.\n\n```{r}\nx <- (\n```\n";
        fs::write(&rmd_path, content).unwrap();

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: Url::from_file_path(tmp.path()).unwrap(),
                    name: "t".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();

        let uri = Url::from_file_path(&rmd_path).unwrap();
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "rmd".into(),
                    version: 1,
                    text: content.into(),
                },
            })
            .await;

        let state = backend.state.read().await;
        // `snapshot_diagnostics` reproduces the publish-path match guard.
        let diagnostics = snapshot_diagnostics(&state, &uri);

        assert!(
            diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR) && d.range.start.line == 5),
            "the chunk syntax error must surface on document line 5 through the publish path, got {:?}",
            diagnostics
                .iter()
                .map(|d| (d.range.start.line, d.message.clone()))
                .collect::<Vec<_>>()
        );
        // Prose lines (heading, markdown link) must not produce diagnostics.
        assert!(
            diagnostics.iter().all(|d| d.range.start.line == 5),
            "no diagnostics may land on prose lines, got {:?}",
            diagnostics
                .iter()
                .map(|d| (d.range.start.line, d.message.clone()))
                .collect::<Vec<_>>()
        );
    }

    /// Open an Rmd document through the real `did_open` path. Returns the owned
    /// `LspService` (keep it alive for the test's duration), the backend's URI,
    /// and the `TempDir` backing the file. Shared by the on-type-formatting
    /// chunk/prose tests.
    async fn open_rmd(content: &str) -> (tower_lsp::LspService<Backend>, Url, TempDir) {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("report.Rmd"), content).unwrap();
        let (svc, uri) = open_in_workspace(&tmp, "report.Rmd", "rmd", content).await;
        (svc, uri, tmp)
    }

    fn on_type_params(
        uri: &Url,
        line: u32,
        character: u32,
    ) -> tower_lsp::lsp_types::DocumentOnTypeFormattingParams {
        use tower_lsp::lsp_types::{
            DocumentOnTypeFormattingParams, FormattingOptions, Position, TextDocumentPositionParams,
        };
        DocumentOnTypeFormattingParams {
            text_document_position: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line, character },
            },
            ch: "\n".into(),
            options: FormattingOptions {
                tab_size: 2,
                insert_spaces: true,
                ..Default::default()
            },
        }
    }

    /// Issue #343 Task 4: on-type-formatting indents inside an R chunk body.
    /// The indentation engine receives the masked analysis text, so a newline
    /// inside a function body produces a body-indent edit at document
    /// coordinates exactly as a plain-R file would.
    #[tokio::test]
    async fn on_type_formatting_indents_inside_rmd_chunk() {
        // Cursor lands on the blank line 3, inside the brace block opened on
        // line 2; the engine should indent it under the block.
        //   0  ```{r}
        //   1  prose-free chunk
        //   2  f <- function() {
        //   3  (cursor here, empty)
        //   4  }
        //   5  ```
        let content = "```{r}\nx <- 1\nf <- function() {\n\n}\n```\n";
        let (svc, uri, _tmp) = open_rmd(content).await;

        let edits = svc
            .inner()
            .on_type_formatting(on_type_params(&uri, 3, 0))
            .await
            .expect("on_type_formatting must not error");
        let edits = edits.expect("on_type_formatting must return edits inside a chunk body");
        assert!(
            !edits.is_empty() && edits.iter().any(|e| e.new_text.contains(' ')),
            "indentation edit must add leading spaces inside the chunk function body, got {:?}",
            edits
        );
    }

    /// The complement: a newline on a prose line must be inert. Without the
    /// prose guard the engine would treat the blank masked line as top-level R
    /// and apply R indentation rules to markdown.
    #[tokio::test]
    async fn on_type_formatting_inert_on_rmd_prose() {
        // Line 2 is prose; an Enter there must yield no edits.
        let content = "```{r}\nx <- 1\n```\n\nSome prose line.\n";
        let (svc, uri, _tmp) = open_rmd(content).await;

        let edits = svc
            .inner()
            .on_type_formatting(on_type_params(&uri, 4, 5))
            .await
            .expect("on_type_formatting must not error");
        assert!(
            edits.is_none() || edits.as_ref().is_some_and(|e| e.is_empty()),
            "on_type_formatting must be inert on a prose line, got {:?}",
            edits
        );
    }

    // ====================================================================
    // Issue #343 Task 5: outgoing-only chunk-aware cross-file metadata.
    //
    // These exercise the production did_open / publish / on-demand paths to
    // prove that for an open `.Rmd`, cross-file metadata feeding the
    // dependency graph, the DocumentStore, and the async missing-file phase
    // is derived from the MASKED analysis text (chunk bodies only), never
    // from prose, while raw content remains available to non-analysis
    // consumers.
    // ====================================================================

    /// Open an arbitrary file (by path under a freshly-initialized workspace)
    /// through the real `did_open` path. Returns the live `LspService`, the
    /// document URI, and the backing `TempDir`. The `TempDir` must already
    /// contain the workspace fixtures; `path` is relative to it.
    async fn open_in_workspace(
        tmp: &TempDir,
        rel_path: &str,
        language_id: &str,
        content: &str,
    ) -> (tower_lsp::LspService<Backend>, Url) {
        use tower_lsp::lsp_types::{DidOpenTextDocumentParams, TextDocumentItem};

        let (svc, _socket) = tower_lsp::LspService::new(Backend::new);
        let backend = svc.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: Url::from_file_path(tmp.path()).unwrap(),
                    name: "t".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();

        let uri = Url::from_file_path(tmp.path().join(rel_path)).unwrap();
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: language_id.into(),
                    version: 1,
                    text: content.into(),
                },
            })
            .await;
        (svc, uri)
    }

    /// Open `uri` on an already-initialized backend (the `did_open` half of
    /// `open_in_workspace`, for second and reopened documents).
    async fn open_doc(backend: &Backend, uri: &Url, language_id: &str, version: i32, text: &str) {
        use tower_lsp::lsp_types::{DidOpenTextDocumentParams, TextDocumentItem};
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: language_id.into(),
                    version,
                    text: text.into(),
                },
            })
            .await;
    }

    /// Replace `uri`'s full buffer text through the real `did_change` path.
    async fn change_doc(backend: &Backend, uri: &Url, version: i32, text: &str) {
        use tower_lsp::lsp_types::{
            DidChangeTextDocumentParams, TextDocumentContentChangeEvent,
            VersionedTextDocumentIdentifier,
        };
        backend
            .did_change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.into(),
                }],
            })
            .await;
    }

    /// Close `uri` through the real `did_close` path.
    async fn close_doc(backend: &Backend, uri: &Url) {
        use tower_lsp::lsp_types::{DidCloseTextDocumentParams, TextDocumentIdentifier};
        backend
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            })
            .await;
    }

    /// Poll `check` against fresh `WorldState` read snapshots until it
    /// returns `true` or `timeout_ms` elapses. Returns whether the condition
    /// was met. For tests that assert on the outcome of fire-and-forget
    /// background work (the watched-files async disk-read pass, the
    /// `did_close` disk resync), which has no join handle to await.
    async fn wait_for_state<F>(backend: &Backend, timeout_ms: u64, mut check: F) -> bool
    where
        F: FnMut(&WorldState) -> bool,
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            {
                let state = backend.state.read().await;
                if check(&state) {
                    return true;
                }
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn snapshot_diagnostics(
        state: &WorldState,
        uri: &Url,
    ) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        let snapshot =
            crate::handlers::DiagnosticsSnapshot::build(state, uri).expect("snapshot must build");
        if snapshot.file_type == crate::file_type::FileType::R {
            crate::handlers::diagnostics_from_snapshot(
                &snapshot,
                uri,
                &crate::handlers::DiagCancelToken::never(),
            )
            .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Flag #1: an open Rmd whose chunk `source()`s an existing helper must
    /// produce an outgoing dependency-graph edge, and a symbol defined in the
    /// helper used in a later chunk must NOT be flagged undefined.
    #[tokio::test]
    async fn rmd_chunk_source_produces_outgoing_edge_and_resolves_symbol() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("helpers.R"), "helper_fn <- function() 1\n").unwrap();
        // Prose mentions `source(\"prose_decoy.R\")` which must be ignored; the
        // real source() lives in an R chunk. A later chunk uses helper_fn().
        let rmd = concat!(
            "# Report\n",
            "\n",
            "Prose calls source(\"prose_decoy.R\") but that is not R.\n",
            "\n",
            "```{r}\n",
            "source(\"helpers.R\")\n",
            "```\n",
            "\n",
            "More prose.\n",
            "\n",
            "```{r}\n",
            "y <- helper_fn()\n",
            "```\n",
        );
        fs::write(tmp.path().join("analysis.Rmd"), rmd).unwrap();

        let (svc, uri) = open_in_workspace(&tmp, "analysis.Rmd", "rmd", rmd).await;
        let backend = svc.inner();
        let state = backend.state.read().await;

        // Outgoing edge analysis.Rmd -> helpers.R exists, and the decoy does not.
        let helpers_uri = Url::from_file_path(tmp.path().join("helpers.R")).unwrap();
        let decoy_uri = Url::from_file_path(tmp.path().join("prose_decoy.R")).unwrap();
        let deps = state.cross_file_graph.get_dependencies(&uri);
        let targets: Vec<&Url> = deps.iter().map(|e| &e.to).collect();
        assert!(
            targets.contains(&&helpers_uri),
            "chunk source() must add an outgoing edge to helpers.R; edges: {:?}",
            targets
        );
        assert!(
            !targets.contains(&&decoy_uri),
            "prose source() must NOT add an edge; edges: {:?}",
            targets
        );

        // helper_fn used in a later chunk must resolve (no undefined-var diag).
        let diags = snapshot_diagnostics(&state, &uri);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("helper_fn is not defined")),
            "helper_fn from the sourced helper must resolve; diagnostics: {:?}",
            diags
                .iter()
                .map(|d| (d.range.start.line, d.message.clone()))
                .collect::<Vec<_>>()
        );
    }

    /// Flag #3: a chunk `source("nonexistent.R")` must yield a missing-file
    /// diagnostic through the ASYNC publish path. The async path strips the
    /// sync missing-file diagnostics and re-adds them from a separately-built
    /// `directive_meta`. Pre-fix, `publish_diagnostics_inner` Phase 1 built
    /// that meta from `parse_directives(doc.text())` — RAW and directives-only,
    /// so it carried NO AST `source()` calls — and the chunk's missing-file
    /// diagnostic was silently lost. The fix sources `directive_meta` from
    /// `snapshot.directive_meta` (full `extract_metadata`, masked for Rmd).
    #[tokio::test]
    async fn rmd_chunk_missing_source_flagged_through_async_publish_path() {
        let tmp = TempDir::new().unwrap();
        let rmd = concat!(
            "# Report\n",
            "\n",
            "```{r}\n",
            "source(\"nonexistent.R\")\n",
            "```\n",
        );
        fs::write(tmp.path().join("analysis.Rmd"), rmd).unwrap();
        let (svc, uri) = open_in_workspace(&tmp, "analysis.Rmd", "rmd", rmd).await;
        let backend = svc.inner();

        // Reproduce publish_diagnostics_inner Phase 1-3 exactly (the production
        // async path), then assert the missing-file diagnostic survives at the
        // chunk's document line (line 3, 0-based).
        let (snapshot, directive_meta, raw_text, workspace_folder, missing_file_severity) = {
            let state = backend.state.read().await;
            let snapshot = crate::handlers::DiagnosticsSnapshot::build(&state, &uri);
            // Production Phase 1 sources directive_meta from the snapshot.
            let directive_meta = snapshot
                .as_ref()
                .map(|s| s.directive_meta.clone())
                .unwrap_or_default();
            let raw_text = state.get_document(&uri).map(|d| d.text()).unwrap();
            let wf = state.workspace_folders.first().cloned();
            let sev = state.cross_file_config.missing_file_severity;
            (snapshot, directive_meta, raw_text, wf, sev)
        };
        let snapshot = snapshot.expect("snapshot must build for Rmd");

        // The production directive_meta carries the chunk's AST source() call...
        assert!(
            directive_meta
                .sources
                .iter()
                .any(|s| s.path == "nonexistent.R"),
            "snapshot.directive_meta (the production async-phase source) must carry the chunk source() call"
        );
        // ...whereas the PRE-FIX source (raw `parse_directives`) does not — this
        // is exactly why the old code lost the diagnostic. Guarding it pins the
        // fix at the seam: a regression to `parse_directives(doc.text())` would
        // feed `diagnostics_async_standalone` a meta with no AST source() call.
        let prefix_meta = crate::cross_file::directive::parse_directives(&raw_text);
        assert!(
            !prefix_meta
                .sources
                .iter()
                .any(|s| s.path == "nonexistent.R"),
            "the pre-fix raw parse_directives must NOT carry the chunk source() — it is directives-only and prose-blind"
        );

        let sync_diagnostics = crate::handlers::diagnostics_from_snapshot(
            &snapshot,
            &uri,
            &crate::handlers::DiagCancelToken::never(),
        )
        .unwrap_or_default();
        let diagnostics = crate::handlers::diagnostics_async_standalone(
            &uri,
            sync_diagnostics,
            &directive_meta,
            workspace_folder.as_ref(),
            missing_file_severity,
            crate::cross_file::CaseMismatchSeverity::Auto,
        )
        .await;

        assert!(
            diagnostics.iter().any(|d| {
                (d.message.starts_with("File not found:")
                    || d.message.starts_with("Cannot resolve path:"))
                    && d.range.start.line == 3
            }),
            "missing-file diagnostic for the chunk source() must survive the async path on line 3; got {:?}",
            diagnostics
                .iter()
                .map(|d| (d.range.start.line, d.message.clone()))
                .collect::<Vec<_>>()
        );
    }

    /// A `# @lsp-cd` directive in Rmd PROSE must be ignored (prose is masked
    /// away), while the same directive inside an R chunk is honored. Asserted
    /// cheaply at the metadata-extraction seam via `extract_metadata_for_path`,
    /// the shared helper every Rmd extraction site routes through.
    #[test]
    fn rmd_lsp_cd_directive_honored_in_chunk_ignored_in_prose() {
        // Prose `# @lsp-cd` — masked away, so no working_directory.
        let prose_cd = "# @lsp-cd /tmp/prose\n\n```{r}\nx <- 1\n```\n";
        let meta = crate::cross_file::extract_metadata_for_path("a.Rmd", prose_cd);
        assert!(
            meta.working_directory.is_none(),
            "a @lsp-cd in prose must not set working_directory; got {:?}",
            meta.working_directory
        );

        // Chunk `# @lsp-cd` — a real directive inside R chunk body.
        let chunk_cd = "# heading\n\n```{r}\n# @lsp-cd /tmp/chunk\nx <- 1\n```\n";
        let meta = crate::cross_file::extract_metadata_for_path("a.Rmd", chunk_cd);
        assert_eq!(
            meta.working_directory.as_deref(),
            Some("/tmp/chunk"),
            "a @lsp-cd inside a chunk must set working_directory"
        );
    }

    /// Flag #2: after did_open of an Rmd, the DocumentStore exposes
    /// chunk-defined symbols via `get_artifacts` (not prose garbage), while
    /// `get_content` still returns the verbatim RAW text.
    #[tokio::test]
    async fn rmd_document_store_artifacts_masked_content_raw() {
        let tmp = TempDir::new().unwrap();
        let rmd = concat!(
            "# Heading is prose, not a symbol\n",
            "\n",
            "Some prose with words like analyze and report.\n",
            "\n",
            "```{r}\n",
            "chunk_symbol <- function() 1\n",
            "```\n",
        );
        fs::write(tmp.path().join("analysis.Rmd"), rmd).unwrap();
        let (svc, uri) = open_in_workspace(&tmp, "analysis.Rmd", "rmd", rmd).await;
        let backend = svc.inner();
        let state = backend.state.read().await;

        use crate::content_provider::ContentProvider;
        let cp = state.content_provider();
        // RAW content is preserved verbatim (prose included).
        let content = cp.get_content(&uri).expect("content available");
        assert_eq!(content, rmd, "get_content must return RAW Rmd text");
        assert!(
            content.contains("Heading is prose"),
            "raw content must still contain prose"
        );

        // Artifacts expose the chunk-defined symbol, derived from masked text.
        let artifacts = cp.get_artifacts(&uri).expect("artifacts available");
        let names: Vec<String> = artifacts
            .exported_interface
            .keys()
            .map(|k| k.to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "chunk_symbol"),
            "DocumentStore artifacts must include the chunk-defined symbol; got {:?}",
            names
        );

        // DocumentStore metadata must be masked-derived (no prose noise).
        let meta = cp.get_metadata(&uri).expect("metadata available");
        assert!(
            meta.sources.is_empty(),
            "no source() in chunks, so metadata.sources must be empty (prose ignored); got {:?}",
            meta.sources
        );
    }

    /// Flag #2 / DocumentState invariant: the DocumentStore's `tree` must be
    /// parsed from the masked analysis text. Pairing the tree with the raw
    /// content would mis-slice; here we assert the tree + analysis_text pair
    /// is self-consistent (the tree's root spans the masked text length).
    #[tokio::test]
    async fn rmd_document_store_tree_paired_with_masked_text() {
        let tmp = TempDir::new().unwrap();
        let rmd = "# prose\n\n```{r}\nz <- 1\n```\n";
        fs::write(tmp.path().join("analysis.Rmd"), rmd).unwrap();
        let (svc, uri) = open_in_workspace(&tmp, "analysis.Rmd", "rmd", rmd).await;
        let backend = svc.inner();
        let state = backend.state.read().await;

        let doc_state = state
            .document_store
            .get_without_touch(&uri)
            .expect("document in store");
        let analysis = doc_state.analysis_text();
        let masked = crate::chunks::mask_to_r(rmd);
        assert_eq!(
            analysis, masked,
            "DocumentState analysis_text must be masked"
        );
        let tree = doc_state.tree.as_ref().expect("tree parsed");
        assert_eq!(
            tree.root_node().end_byte(),
            analysis.len(),
            "tree must be parsed from the masked analysis text"
        );
    }

    /// Flag #4: opening a `.R` helper that declares `# @lsp-sourced-by
    /// analysis.Rmd` triggers on-demand indexing of the (closed) Rmd from disk.
    /// That extraction must be MASKED so `helper_fn` (defined in a chunk) is in
    /// the helper's inherited scope and is not flagged undefined. The Rmd's
    /// indexed content stays RAW while its indexed metadata reflects only chunk
    /// content.
    #[tokio::test]
    async fn r_file_sourced_by_rmd_resolves_chunk_symbol_via_masked_on_demand() {
        let tmp = TempDir::new().unwrap();
        let rmd = concat!(
            "# Analysis\n",
            "\n",
            "Prose source(\"prose_decoy.R\") here.\n",
            "\n",
            "```{r}\n",
            "library(tools)\n",
            "helper_fn <- function() 1\n",
            "```\n",
        );
        fs::write(tmp.path().join("analysis.Rmd"), rmd).unwrap();
        let helper = concat!("# @lsp-sourced-by analysis.Rmd\n", "x <- helper_fn()\n",);
        fs::write(tmp.path().join("helpers.R"), helper).unwrap();

        let (svc, helper_uri) = open_in_workspace(&tmp, "helpers.R", "r", helper).await;
        let backend = svc.inner();
        let state = backend.state.read().await;

        // The Rmd was indexed on demand. Its indexed metadata reflects ONLY
        // chunk content: library(tools) is detected, the prose decoy source()
        // is NOT.
        let rmd_uri = Url::from_file_path(tmp.path().join("analysis.Rmd")).unwrap();
        let rmd_meta = state
            .get_enriched_metadata(&rmd_uri)
            .expect("Rmd indexed metadata available");
        assert!(
            rmd_meta
                .library_calls
                .iter()
                .any(|lc| lc.package == "tools"),
            "masked on-demand extraction must find library(tools) from the chunk; got {:?}",
            rmd_meta.library_calls
        );
        assert!(
            rmd_meta.sources.is_empty(),
            "prose source() decoy must not appear in indexed Rmd metadata; got {:?}",
            rmd_meta.sources
        );

        // The indexed RAW content (file cache) still contains prose.
        let cached = state
            .cross_file_file_cache
            .get(&rmd_uri)
            .expect("Rmd content cached");
        assert!(
            cached.contains("Prose source"),
            "cross_file_file_cache must hold RAW Rmd content (with prose)"
        );

        // helper_fn (chunk-defined in the parent Rmd) resolves in helpers.R.
        let diags = snapshot_diagnostics(&state, &helper_uri);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("helper_fn is not defined")),
            "helper_fn from the parent Rmd chunk must resolve in helpers.R; diagnostics: {:?}",
            diags
                .iter()
                .map(|d| (d.range.start.line, d.message.clone()))
                .collect::<Vec<_>>()
        );
    }
}
