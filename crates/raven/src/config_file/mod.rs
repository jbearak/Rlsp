//! Project-level configuration loader (raven.toml, .lintr).

pub mod discovery;
pub mod discovery_load;
pub mod exclusions;
pub mod lintr_loader;
pub mod merge;
pub mod overrides;
pub mod toml_loader;

pub use discovery::{
    ConfigFileKind, DiscoveredConfig, DiscoveryOptions, find_config, find_config_with_options,
};
pub use discovery_load::{
    DiscoveredLoad, LoadedConfig, discover_and_load, discover_and_load_with_options,
    load_explicit_config, load_explicit_config_from_base, resolve_explicit_config_path,
};
pub use exclusions::{CompiledWorkspaceExclusions, compile_workspace_exclusions};
pub use lintr_loader::load as load_lintr;
pub use merge::merge as merge_settings;
pub use overrides::{
    CompiledLintOverride, compile_lint_overrides, is_skipped_by_overrides,
    resolve_lint_for_document,
};
pub use toml_loader::load as load_toml;

/// Re-run every `parse_*_config` over the merged `(client, project)` JSON
/// and overwrite the parsed configs on `state`. Idempotent.
///
/// Resets each parsed config to its struct default when the corresponding
/// section is absent in the merged JSON. This matches the spec's layered
/// precedence: built-in defaults are the floor; client-supplied settings
/// and project-supplied settings layer on top. Both layers being silent on
/// a section means "fall to default", not "preserve whatever was there".
///
/// One exception: `parse_cross_file_config` returns `Ok(None)` when ALL of
/// `crossFile`, `diagnostics`, `packages` are absent — in that case we still
/// overwrite with `CrossFileConfig::default()`. A validation error
/// (`Err(...)`) is logged and the existing config is preserved (best-effort
/// graceful degradation; same as the existing behavior at
/// `backend.rs:3819-3838`).
///
/// Callers: `backend::initialize`, `backend::did_change_configuration`,
/// `backend::did_change_watched_files` (project-config change).
/// Strip `linting.enabled = "auto"` from a (cloned) project layer so it
/// behaves as if the key were omitted. Without this, the deep merge would
/// overwrite a client-explicit `true`/`false` with the project's `"auto"`
/// and then resolve `Auto → lintr_discovered`, which contradicts the
/// behavior matrix in `docs/linting.md` for `true` + `raven.toml enabled =
/// "auto"`. See #281.
fn strip_project_auto_enabled(project: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let mut cloned = project.cloned()?;
    if let Some(linting) = cloned.get_mut("linting").and_then(|l| l.as_object_mut())
        && linting.get("enabled") == Some(&serde_json::Value::String("auto".into()))
    {
        linting.remove("enabled");
    }
    Some(cloned)
}

/// Whether a discovered `.lintr` is allowed to auto-enable Raven's native
/// linting, per the client-only `linting.autoEnableFromDotLintr` signal.
///
/// `.lintr` is REditorSupport's / `lintr`'s config file; its mere presence
/// only signals "I want lintr-style linting" in a context where that
/// diagnostic path is actually live. The VS Code client clears this flag to
/// `false` when REditorSupport is installed+enabled with its LSP lint path on
/// (`r.lsp.enabled` and `r.lsp.diagnostics`), or when running inside Positron —
/// contexts where a `.lintr` is dormant config for another tool and must not
/// flip Raven's lints on. See #337.
///
/// Read from the CLIENT layer only: this is a VS Code environment signal and
/// must not be overridable by a project `raven.toml`. Absent or malformed
/// (non-VS-Code clients, older clients, the CLI) defaults to `true`,
/// preserving the historical behavior.
fn lintr_auto_enable_allowed(raw_client: &serde_json::Value) -> bool {
    raw_client
        .get("linting")
        .and_then(|l| l.get("autoEnableFromDotLintr"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// Whether a discovered `.lintr` actually expresses linting configuration, as
/// opposed to being blank/empty.
///
/// A `.lintr` auto-enables Raven's native linting only when it carries linting
/// intent — a recognized `linters:` or `exclusions:` directive (including a
/// bare `linters_with_defaults()`, which sets no individual keys). The `.lintr`
/// loader emits a `linting` object exactly when such a directive is present
/// (see `lintr_loader::load_str`), so the presence of that object in the raw
/// `.lintr` layer is the intent signal. A blank, whitespace-only, or
/// unknown-fields-only `.lintr` carries no opt-in and must not flip linting on.
///
/// Note: lintr itself has no enable/disable switch tied to `.lintr` presence —
/// it lints whenever invoked and treats an empty `.lintr` as "use defaults".
/// Raven's presence-based opt-in is a Raven design choice (#281); gating it on
/// expressed intent keeps a stray empty `.lintr` from silently enabling lints.
pub(crate) fn lintr_expresses_linting(raw_project: Option<&serde_json::Value>) -> bool {
    raw_project.and_then(|s| s.get("linting")).is_some()
}

/// The single source of truth for "does this config file opt a project into
/// linting via the `.lintr` path?": it must be a `.lintr` file AND express
/// linting config (see [`lintr_expresses_linting`]). Used by both the LSP server
/// gate ([`recompute_parsed_configs`]) and the CLI
/// (`cli::lint::resolve_lint_config`) so the two surfaces cannot drift on the
/// opt-in rule — if the policy ever tightens, both follow automatically.
pub(crate) fn lintr_path_opts_in(
    path: &std::path::Path,
    raw_project: Option<&serde_json::Value>,
) -> bool {
    ConfigFileKind::is_lintr_path(path) && lintr_expresses_linting(raw_project)
}

pub fn recompute_parsed_configs(state: &mut crate::state::WorldState) {
    let normalized_project = strip_project_auto_enabled(state.raw_project_settings.as_ref());
    let merged = merge_settings(&state.raw_client_settings, normalized_project.as_ref());

    match crate::backend::parse_cross_file_config(&merged) {
        Ok(Some(cfg)) => {
            state.resize_caches(&cfg);
            state.cross_file_config = cfg;
        }
        Ok(None) => {
            let cfg = crate::cross_file::CrossFileConfig::default();
            state.resize_caches(&cfg);
            state.cross_file_config = cfg;
        }
        Err(err) => {
            log::warn!("recompute_parsed_configs: cross_file validation error: {err}");
        }
    }
    state.symbol_config = crate::backend::parse_symbol_config(&merged).unwrap_or_default();
    state.completion_config = crate::backend::parse_completion_config(&merged).unwrap_or_default();
    state.indentation_config =
        crate::backend::parse_indentation_config(&merged).unwrap_or_default();
    // A discovered `.lintr` auto-enables only when it actually expresses
    // linting config (not a blank/empty file) — see `lintr_expresses_linting`.
    let lintr_discovered = state
        .project_config_path
        .as_deref()
        .is_some_and(|p| lintr_path_opts_in(p, state.raw_project_settings.as_ref()));
    // Gate ONLY the `.lintr` auto-enable path on the client environment signal
    // (#337). An explicit client `on`/`off` and `raven.toml enabled = true`
    // flow through `merged` independently and are unaffected, because they
    // resolve via `On`/`Off` and never route through `lintr_discovered`.
    let lintr_auto = lintr_discovered && lintr_auto_enable_allowed(&state.raw_client_settings);
    state.lint_config = crate::backend::parse_lint_config(&merged, lintr_auto).unwrap_or_default();

    // Recompile per-document lint overrides as part of the centralized
    // recompute. Splitting this into a separate caller step (as earlier
    // versions did) was error-prone — a future caller could call
    // `recompute_parsed_configs` and forget to recompile overrides,
    // leaving them stale relative to the new merged settings. Tying
    // them together here is the per-CLAUDE.md invariant: this function
    // is the only place that writes any parsed config field after a
    // settings change.
    let workspace_roots: Vec<std::path::PathBuf> = state
        .workspace_folders
        .iter()
        .filter_map(|u| u.to_file_path().ok())
        .collect();

    if let Some(root) = workspace_roots.first() {
        state.lint_overrides = compile_lint_overrides(&merged, root);
    } else {
        // No workspace root yet — clear any stale overrides so we don't
        // resolve against patches whose globs were computed against a
        // since-removed root.
        state.lint_overrides = Vec::new();
    }
    // Cache the merged `linting` section next to the compiled overrides so
    // per-document resolution (`effective_lint_config_for_document`) never
    // re-merges the raw settings trees on the typing hot path. Deliberately
    // merged from the RAW project layer (not `normalized_project`), exactly
    // as the per-document resolvers always did before the cache existed.
    state.merged_linting_section = merge_settings(
        &state.raw_client_settings,
        state.raw_project_settings.as_ref(),
    )
    .get("linting")
    .cloned()
    .unwrap_or(serde_json::json!({}));
    // Every input of per-document lint resolution just changed; drop the
    // resolved-config cache with them.
    if let Ok(mut cache) = state.effective_lint_config_cache.lock() {
        cache.clear();
    }
    state.workspace_exclusions = compile_workspace_exclusions(&merged, workspace_roots);
    // This is the sole parsed-config writer. Advance the typed authority only
    // after every parsed analysis field and compiled exclusion has been
    // installed, so detached transactions observe either the complete old
    // configuration or the complete new one.
    state.advance_analysis_config_generation();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WorldState;
    use serde_json::json;
    use std::path::PathBuf;

    fn state_with(
        client: serde_json::Value,
        project_config_path: &str,
        project: Option<serde_json::Value>,
    ) -> WorldState {
        let mut state = WorldState::new();
        state.raw_client_settings = client;
        state.raw_project_settings = project;
        state.project_config_path = Some(PathBuf::from(project_config_path));
        state
    }

    #[test]
    fn syntax_diagnostic_cap_layers_project_over_client_and_resets_to_default() {
        let mut state = WorldState::new();
        state.raw_client_settings = json!({
            "diagnostics": { "maxSyntaxDiagnosticsPerFile": 40 }
        });
        state.raw_project_settings = Some(json!({
            "diagnostics": { "maxSyntaxDiagnosticsPerFile": 12 }
        }));
        recompute_parsed_configs(&mut state);
        assert_eq!(state.cross_file_config.max_syntax_diagnostics_per_file, 12);

        state.raw_project_settings = None;
        recompute_parsed_configs(&mut state);
        assert_eq!(state.cross_file_config.max_syntax_diagnostics_per_file, 40);

        state.raw_client_settings = json!({});
        recompute_parsed_configs(&mut state);
        assert_eq!(
            state.cross_file_config.max_syntax_diagnostics_per_file,
            crate::cross_file::config::DEFAULT_MAX_SYNTAX_DIAGNOSTICS_PER_FILE
        );
    }

    #[test]
    fn model_diagnostic_switches_layer_independently_and_reset_to_defaults() {
        let mut state = WorldState::new();
        state.raw_client_settings = json!({
            "diagnostics": { "jags": "on", "stan": "off" }
        });
        state.raw_project_settings = Some(json!({
            "diagnostics": { "stan": "on" }
        }));
        recompute_parsed_configs(&mut state);
        assert!(state.cross_file_config.jags_diagnostics_enabled);
        assert!(state.cross_file_config.stan_diagnostics_enabled);

        state.raw_project_settings = Some(json!({
            "diagnostics": { "jags": "off" }
        }));
        recompute_parsed_configs(&mut state);
        assert!(!state.cross_file_config.jags_diagnostics_enabled);
        assert!(!state.cross_file_config.stan_diagnostics_enabled);

        state.raw_client_settings = json!({});
        state.raw_project_settings = None;
        recompute_parsed_configs(&mut state);
        assert!(!state.cross_file_config.jags_diagnostics_enabled);
        assert!(!state.cross_file_config.stan_diagnostics_enabled);
    }

    /// The per-URI resolved-config cache serves repeated lookups, evicts a
    /// closed document, and is cleared by `recompute_parsed_configs`, so no
    /// stale value survives either lifecycle transition.
    #[test]
    fn effective_lint_config_cache_is_cleared_by_recompute() {
        let root = if cfg!(windows) { "C:\\proj" } else { "/proj" };
        let overrides_settings = |unit: u32| {
            serde_json::json!({
                "linting": {
                    "enabled": true,
                    "indentationUnit": 2,
                    "overrides": [{"files": ["R/**"], "indentationUnit": unit}],
                }
            })
        };
        let mut state = WorldState::new();
        state.workspace_folders = vec![tower_lsp::lsp_types::Url::from_file_path(root).unwrap()];
        state.raw_client_settings = overrides_settings(8);
        recompute_parsed_configs(&mut state);

        let uri = tower_lsp::lsp_types::Url::from_file_path(
            std::path::Path::new(root).join("R").join("a.R"),
        )
        .unwrap();
        state.open_document(uri.clone(), "", Some(1));
        assert_eq!(
            state
                .effective_lint_config_for_document(&uri)
                .indentation_unit,
            8
        );
        // Second lookup is the cache hit; it must return the same answer.
        assert_eq!(
            state
                .effective_lint_config_for_document(&uri)
                .indentation_unit,
            8
        );
        assert!(
            state
                .effective_lint_config_cache
                .lock()
                .unwrap()
                .contains_key(uri.as_str())
        );

        state.close_document(&uri);
        assert!(
            !state
                .effective_lint_config_cache
                .lock()
                .unwrap()
                .contains_key(uri.as_str()),
            "close_document must evict the closed URI's resolved config"
        );
        // A one-shot/non-open lookup (the shape used by `raven check` worker
        // overlays) must not repopulate the shared cache.
        assert_eq!(
            state
                .effective_lint_config_for_document(&uri)
                .indentation_unit,
            8
        );
        assert!(
            !state
                .effective_lint_config_cache
                .lock()
                .unwrap()
                .contains_key(uri.as_str()),
            "non-open document resolution must bypass the shared cache"
        );

        // Reopen and repopulate so the remainder still pins recompute
        // invalidation for the LSP path.
        state.open_document(uri.clone(), "", Some(2));
        assert_eq!(
            state
                .effective_lint_config_for_document(&uri)
                .indentation_unit,
            8
        );
        assert!(
            state
                .effective_lint_config_cache
                .lock()
                .unwrap()
                .contains_key(uri.as_str())
        );

        state.raw_client_settings = overrides_settings(6);
        recompute_parsed_configs(&mut state);
        assert_eq!(
            state
                .effective_lint_config_for_document(&uri)
                .indentation_unit,
            6,
            "recompute_parsed_configs must invalidate the resolved-config cache"
        );
    }

    /// The settings a *configured* `.lintr` contributes — the `linting` object
    /// the loader emits when the file expresses linting intent (e.g. a bare
    /// `linters_with_defaults()`). Used by the #337 gate tests, which are about
    /// a real `.lintr`, not the empty-file case.
    fn configured_lintr_project() -> serde_json::Value {
        json!({ "linting": {} })
    }

    #[test]
    fn dot_lintr_auto_enable_gated_off_disables_lint() {
        // #337: a configured `.lintr` is discovered, but the client signals that
        // REditorSupport's lintr path is live (or we're in Positron). The
        // dormant `.lintr` must not flip Raven's lints on.
        let mut state = state_with(
            json!({ "linting": { "enabled": "auto", "autoEnableFromDotLintr": false } }),
            "/ws/.lintr",
            Some(configured_lintr_project()),
        );
        recompute_parsed_configs(&mut state);
        assert!(!state.lint_config.enabled);
    }

    #[test]
    fn dot_lintr_auto_enable_allowed_enables_lint() {
        // #337: the signal is present and `true` → historical opt-in survives.
        let mut state = state_with(
            json!({ "linting": { "enabled": "auto", "autoEnableFromDotLintr": true } }),
            "/ws/.lintr",
            Some(configured_lintr_project()),
        );
        recompute_parsed_configs(&mut state);
        assert!(state.lint_config.enabled);
    }

    #[test]
    fn dot_lintr_auto_enable_absent_defaults_on() {
        // #337: older clients and the CLI omit the signal entirely. Absent
        // defaults to "allowed" so the pre-#337 behavior is preserved.
        let mut state = state_with(
            json!({ "linting": { "enabled": "auto" } }),
            "/ws/.lintr",
            Some(configured_lintr_project()),
        );
        recompute_parsed_configs(&mut state);
        assert!(state.lint_config.enabled);
    }

    #[test]
    fn blank_dot_lintr_does_not_auto_enable() {
        // A discovered but content-free `.lintr` (loader contributes no
        // `linting` object) carries no opt-in: Auto must resolve to off.
        let mut state = state_with(
            json!({ "linting": { "enabled": "auto", "autoEnableFromDotLintr": true } }),
            "/ws/.lintr",
            Some(json!({})),
        );
        recompute_parsed_configs(&mut state);
        assert!(
            !state.lint_config.enabled,
            "a blank .lintr must not auto-enable linting"
        );
    }

    #[test]
    fn configured_dot_lintr_with_only_exclusions_auto_enables() {
        // An `exclusions:`-only `.lintr` still expresses linting config.
        let mut state = state_with(
            json!({ "linting": { "enabled": "auto" } }),
            "/ws/.lintr",
            Some(
                json!({ "linting": { "overrides": [ { "files": ["x/**"], "enabled": false } ] } }),
            ),
        );
        recompute_parsed_configs(&mut state);
        assert!(state.lint_config.enabled);
    }

    #[test]
    fn lintr_expresses_linting_reads_marker() {
        // Present `linting` object (incl. empty, the linters_with_defaults()
        // marker) → intent; absent → none.
        assert!(lintr_expresses_linting(Some(&json!({ "linting": {} }))));
        assert!(lintr_expresses_linting(Some(
            &json!({ "linting": { "lineLength": 80 } })
        )));
        assert!(!lintr_expresses_linting(Some(&json!({}))));
        assert!(!lintr_expresses_linting(None));
    }

    #[test]
    fn dot_lintr_gate_does_not_touch_raven_toml_enabled_true() {
        // #337: the gate is scoped to the `.lintr` discovery branch only. A
        // discovered `raven.toml` with `enabled = true` keeps linting on even
        // when the client clears the `.lintr` signal — that resolves through
        // `On`, never through `lintr_discovered`.
        let mut state = state_with(
            json!({ "linting": { "autoEnableFromDotLintr": false } }),
            "/ws/raven.toml",
            Some(json!({ "linting": { "enabled": true } })),
        );
        recompute_parsed_configs(&mut state);
        assert!(state.lint_config.enabled);
    }

    #[test]
    fn dot_lintr_gate_does_not_override_explicit_client_on() {
        // #337: an explicit client `enabled = "on"` wins regardless of the
        // `.lintr` gate — the gate only governs `Auto` resolution.
        let mut state = state_with(
            json!({ "linting": { "enabled": "on", "autoEnableFromDotLintr": false } }),
            "/ws/.lintr",
            None,
        );
        recompute_parsed_configs(&mut state);
        assert!(state.lint_config.enabled);
    }

    #[test]
    fn recompute_updates_workspace_exclusions_from_project_layer() {
        let root = tempfile::TempDir::new().unwrap();
        let root_url = tower_lsp::lsp_types::Url::from_file_path(root.path()).unwrap();
        let mut state = state_with(
            json!({}),
            root.path().join("raven.toml").to_str().unwrap(),
            Some(json!({ "workspace": { "exclude": ["generated/**"] } })),
        );
        state.workspace_folders = vec![root_url];

        recompute_parsed_configs(&mut state);
        assert!(
            state
                .workspace_exclusions
                .is_excluded_path(&root.path().join("generated/a.R"))
        );

        state.raw_project_settings = Some(json!({ "workspace": { "exclude": ["archive/**"] } }));
        recompute_parsed_configs(&mut state);
        assert!(
            !state
                .workspace_exclusions
                .is_excluded_path(&root.path().join("generated/a.R"))
        );
        assert!(
            state
                .workspace_exclusions
                .is_excluded_path(&root.path().join("archive/a.R"))
        );
    }

    #[test]
    fn lintr_auto_enable_allowed_reads_client_flag() {
        // Explicit false suppresses; explicit true allows.
        assert!(!lintr_auto_enable_allowed(
            &json!({ "linting": { "autoEnableFromDotLintr": false } })
        ));
        assert!(lintr_auto_enable_allowed(
            &json!({ "linting": { "autoEnableFromDotLintr": true } })
        ));
        // Absent (no key, no section, non-VS-Code clients) → allowed.
        assert!(lintr_auto_enable_allowed(&json!({ "linting": {} })));
        assert!(lintr_auto_enable_allowed(&json!({})));
        assert!(lintr_auto_enable_allowed(&serde_json::Value::Null));
        // Malformed (non-bool) → allowed (defensive default).
        assert!(lintr_auto_enable_allowed(
            &json!({ "linting": { "autoEnableFromDotLintr": "no" } })
        ));
    }
}
