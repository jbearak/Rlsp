//
// cross_file/config.rs
//
// Configuration for cross-file awareness
//

use std::path::PathBuf;
use tower_lsp::lsp_types::DiagnosticSeverity;

use super::path_resolve::CaseMismatchRegime;

/// Default maximum number of native Stan/JAGS syntax diagnostics retained for
/// one file. A configured value of `0` disables the limit.
pub const DEFAULT_MAX_SYNTAX_DIAGNOSTICS_PER_FILE: usize = 500;

/// Severity policy for the `source-path-case-mismatch` diagnostic (issue #530;
/// extended to backward directives in #535). Distinct from the plain
/// `Option<DiagnosticSeverity>` other categories use because the default
/// (`Auto`) is host-derived rather than a fixed level. The same policy gates the
/// forward (`source()`/forward-directive) and backward (`# raven: sourced-by`
/// etc.) case-mismatch collectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMismatchSeverity {
    /// Host-derived: information on a case-insensitive filesystem (portability
    /// hazard), warning on a case-sensitive one (for a forward `source()` R would
    /// error; for a backward directive it is a casing/portability mismatch).
    /// Default.
    #[default]
    Auto,
    /// A user-pinned level applied in both regimes.
    Fixed(DiagnosticSeverity),
    /// Diagnostic suppressed entirely (resolution still happens, so no cascade).
    Off,
}

impl CaseMismatchSeverity {
    /// Resolve to the concrete severity for a given mismatch regime, or `None`
    /// when the diagnostic should not be emitted.
    pub fn resolve(self, regime: CaseMismatchRegime) -> Option<DiagnosticSeverity> {
        match self {
            CaseMismatchSeverity::Off => None,
            CaseMismatchSeverity::Fixed(severity) => Some(severity),
            CaseMismatchSeverity::Auto => Some(match regime {
                CaseMismatchRegime::CaseInsensitiveFs => DiagnosticSeverity::INFORMATION,
                CaseMismatchRegime::CaseSensitiveFs => DiagnosticSeverity::WARNING,
            }),
        }
    }
}

/// How backward dependencies (parent files that source this file) are resolved
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BackwardDependencyMode {
    /// Only use backward relationships from explicit `# raven: sourced-by`
    /// directives (`@lsp-sourced-by` is a permanent alias that parses identically).
    /// Diagnostics are not deferred for the workspace scan.
    Explicit,
    /// Automatically infer backward relationships from forward directives and
    /// `source()` calls in other workspace files. Files without explicit backward
    /// directives defer undefined variable diagnostics until the workspace scan
    /// completes. Files with explicit backward directives use only those directives.
    #[default]
    Auto,
}

/// Default call site assumption when not specified
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CallSiteDefault {
    /// Assume call site at end of file (all symbols available)
    #[default]
    End,
    /// Assume call site at start of file (no symbols available)
    Start,
}

/// Cross-file awareness configuration
#[derive(Debug, Clone, PartialEq)]
pub struct CrossFileConfig {
    /// Master switch for all diagnostics
    /// When false, all diagnostics are suppressed regardless of individual severity settings
    pub diagnostics_enabled: bool,
    /// Whether native JAGS diagnostics are enabled.
    ///
    /// The global [`Self::diagnostics_enabled`] switch still takes precedence.
    pub jags_diagnostics_enabled: bool,
    /// Whether native Stan syntax and semantic diagnostics are enabled.
    ///
    /// The global [`Self::diagnostics_enabled`] switch still takes precedence.
    pub stan_diagnostics_enabled: bool,
    /// Maximum native Tree-sitter syntax diagnostics retained for one Stan or
    /// JAGS file after exact deduplication and source ordering. `0` is
    /// unlimited. This does not cap R diagnostics.
    pub max_syntax_diagnostics_per_file: usize,
    /// Maximum depth for backward directive traversal
    pub max_backward_depth: usize,
    /// Maximum depth for forward source() traversal
    pub max_forward_depth: usize,
    /// Maximum total chain depth. Also bounds the *bidirectional* neighborhood
    /// BFS depth, which can exceed a linear source chain (up to ancestors, back
    /// down through siblings). Default 64 — high enough that realistic graphs
    /// never truncate, while still bounding pathological ones (issue #473).
    pub max_chain_depth: usize,
    /// Default call site assumption when not specified
    pub assume_call_site: CallSiteDefault,
    /// Whether to index workspace files
    pub index_workspace: bool,
    /// Max number of open documents to schedule for diagnostics revalidation per trigger
    pub max_revalidations_per_trigger: usize,
    /// Debounce delay for cross-file diagnostics fanout in milliseconds
    pub revalidation_debounce_ms: u64,
    /// Debounce delay for the actively-edited file in milliseconds.
    /// Lower than revalidation_debounce_ms for near-instant feedback.
    pub edited_file_debounce_ms: u64,
    /// Severity for undefined variable diagnostics (None = disabled)
    pub undefined_variable_severity: Option<DiagnosticSeverity>,
    /// Whether undefined-variable diagnostics descend into ordinary function-call
    /// arguments (`f(...)`). When true (default), the collector resolves each
    /// call's callee and applies a per-call NSE argument policy, so real bugs
    /// like `paste(undefined_var)` are flagged while genuine NSE arguments
    /// (`with(df, col + 1)`, `dplyr::filter(df, col)`) stay suppressed. When
    /// false, every ordinary call argument is blanket-suppressed (an escape
    /// hatch for highly dynamic code). Does NOT affect `[` / `[[`; see
    /// `undefined_variable_in_bracket_indices`.
    pub undefined_variable_in_call_arguments: bool,
    /// Whether undefined-variable diagnostics descend into bracket index
    /// expressions (`df[i, ]`, `lst[[k]]`). When true (default), base `[` / `[[`
    /// indices are checked, except for data.table-style `[` cases (a known
    /// data.table object, or an unresolved object with data.table detectably in
    /// play). When false, every bracket index is blanket-suppressed (an escape
    /// hatch for data.table-heavy code).
    pub undefined_variable_in_bracket_indices: bool,
    /// Severity for missing file diagnostics (None = disabled)
    pub missing_file_severity: Option<DiagnosticSeverity>,
    /// Severity policy for the `source-path-case-mismatch` diagnostic (issue
    /// #530). Independent of `missing_file_severity` so turning off missing-file
    /// diagnostics does not silence this. Default `Auto` (host-derived).
    pub case_mismatch_severity: CaseMismatchSeverity,
    /// Severity for circular dependency diagnostics (None = disabled)
    pub circular_dependency_severity: Option<DiagnosticSeverity>,
    /// Severity for out-of-scope symbol diagnostics (None = disabled)
    pub out_of_scope_severity: Option<DiagnosticSeverity>,
    /// Extend the `unused-suppression` sweep to *every* suppression directive
    /// (`# raven: ignore`, alias `@lsp-ignore`, or `# nolint`-equivalent), not just
    /// `# raven: expect` (F2 Step 3). Pyright-style; default `false`. When
    /// `false`, only `expect` directives that suppressed nothing are reported.
    pub report_unused_suppressions: bool,
    /// Severity for max chain depth exceeded diagnostics (None = disabled)
    pub max_chain_depth_severity: Option<DiagnosticSeverity>,
    /// Whether on-demand indexing is enabled
    pub on_demand_indexing_enabled: bool,
    /// Whether package function awareness is enabled
    pub packages_enabled: bool,
    /// Additional R library paths for package discovery
    pub packages_additional_library_paths: Vec<PathBuf>,
    /// Path to R executable for subprocess calls
    pub packages_r_path: Option<PathBuf>,
    /// Severity for missing package diagnostics (None = disabled)
    pub packages_missing_package_severity: Option<DiagnosticSeverity>,
    /// Whether to emit the generic `package-not-installed` diagnostic.
    ///
    /// The CLI disables this by default because CI commonly omits all package
    /// installation, while retaining the actionable renv-specific child
    /// diagnostic. Editor/LSP operation leaves it enabled.
    pub(crate) packages_report_generic_uninstalled: bool,
    /// Severity for `namespace-member-not-found` diagnostics (None = disabled).
    /// Fires only when a package's *complete* export set lacks the referenced
    /// `pkg::member`. See `namespace_member_status_sync`.
    pub packages_namespace_member_severity: Option<DiagnosticSeverity>,
    /// Watch R library paths (`.libPaths()`) for package install/remove events.
    /// When true, Raven attaches a filesystem watcher and refreshes package
    /// diagnostics automatically.
    pub packages_watch_library_paths: bool,
    /// Debounce window for libpath watcher events, in milliseconds.
    /// Clamped to `[100, 5000]` by the parser.
    pub packages_watch_debounce_ms: u64,
    /// Severity for redundant directive diagnostics (when `# raven: source` without line= targets
    /// same file as earlier source() call)
    /// _Requirements: 6.2_
    pub redundant_directive_severity: Option<DiagnosticSeverity>,
    /// Severity for the mixed-logical rule. `None` disables the rule.
    ///
    /// Flags `|` / `||` binary operators whose immediate operand is a bare
    /// `&` / `&&` (no parentheses), e.g. `a & b | c`. Stops at call/subset
    /// boundaries so vectorized data-mask patterns are not flagged.
    pub mixed_logical_severity: Option<DiagnosticSeverity>,
    /// Severity for the condition-assignment rule. `None` disables the rule.
    ///
    /// Flags `=` used as a binary operator directly inside an `if` or `while`
    /// condition. R rejects `if (x = 1)` as a syntax error at runtime;
    /// tree-sitter-r accepts it silently.
    pub condition_assignment_severity: Option<DiagnosticSeverity>,
    /// Maximum entries in the metadata cache (LRU eviction)
    pub cache_metadata_max_entries: usize,
    /// Maximum entries in the file content cache (LRU eviction)
    pub cache_file_content_max_entries: usize,
    /// Maximum entries in the file existence cache (LRU eviction)
    pub cache_existence_max_entries: usize,
    /// Maximum entries in the cross-file workspace index (LRU eviction)
    pub cache_workspace_index_max_entries: usize,
    /// Whether to hoist global definitions inside function bodies.
    /// When true, all top-level (global) definitions are visible inside function bodies
    /// regardless of position, since by the time any function executes the entire file
    /// has been sourced. Function-local variables remain positional.
    pub hoist_globals_in_functions: bool,
    /// How backward dependencies are resolved.
    /// Controls whether the LSP auto-detects parent files from forward source() calls
    /// in other workspace files, or requires explicit `# raven: sourced-by` directives.
    pub backward_dependencies: BackwardDependencyMode,
    /// Maximum nodes visited during transitive dependent / neighborhood search
    /// (caps traversal in dense graphs). When exceeded, the resolver silently
    /// stops following `source()` edges, surfacing dropped symbols as
    /// false-positive `undefined-variable`. Default 50_000 — far above any real
    /// R workspace (the neighborhood is naturally bounded by file count) while
    /// still bounding a runaway graph (issue #473). `raven check` surfaces a
    /// note when this budget is hit so drops are distinguishable from genuine
    /// undefined variables.
    pub max_transitive_dependents_visited: usize,
    /// Package mode: auto (detect DESCRIPTION), enabled (always), disabled (never).
    pub package_mode: PackageMode,
    /// Model a workspace-root `.Rprofile` as a script-scope prelude
    /// (suppressive-only). See `docs/r-package-dev.md` and
    /// `crates/raven/src/package_state/rprofile.rs`.
    pub model_rprofile: bool,
}

/// Controls whether R package workspace mode is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PackageMode {
    /// Detect automatically from DESCRIPTION file presence.
    #[default]
    Auto,
    /// Always enable package mode (for non-standard layouts).
    Enabled,
    /// Never enable package mode even if DESCRIPTION exists.
    Disabled,
}

impl Default for CrossFileConfig {
    /// Creates a CrossFileConfig populated with sensible defaults used by the cross-file awareness subsystem.
    ///
    /// The defaults enable workspace indexing, on-demand indexing, package awareness, and traversal limits
    /// sized so that realistic workspaces never hit them while a high finite ceiling still bounds pathological
    /// graphs (backward/forward depth: 10, chain depth: 64, transitive-dependents budget: 50_000 — see issue #473).
    /// Diagnostic severities and debounce/revalidation limits are initialized to typical values for editor integrations.
    ///
    /// # Examples
    ///
    /// ```
    /// use raven::cross_file::CrossFileConfig;
    ///
    /// let cfg = CrossFileConfig::default();
    /// assert!(cfg.index_workspace);
    /// assert_eq!(cfg.max_chain_depth, 64);
    /// assert_eq!(cfg.max_backward_depth, 10);
    /// assert_eq!(cfg.max_forward_depth, 10);
    /// assert_eq!(cfg.max_transitive_dependents_visited, 50_000);
    /// assert!(cfg.on_demand_indexing_enabled);
    /// assert!(cfg.packages_enabled);
    /// ```
    fn default() -> Self {
        Self {
            diagnostics_enabled: true,
            jags_diagnostics_enabled: false,
            stan_diagnostics_enabled: false,
            max_syntax_diagnostics_per_file: DEFAULT_MAX_SYNTAX_DIAGNOSTICS_PER_FILE,
            max_backward_depth: 10,
            max_forward_depth: 10,
            max_chain_depth: 64,
            assume_call_site: CallSiteDefault::End,
            index_workspace: true,
            max_revalidations_per_trigger: 10,
            revalidation_debounce_ms: 200,
            edited_file_debounce_ms: 50,
            undefined_variable_severity: Some(DiagnosticSeverity::WARNING),
            undefined_variable_in_call_arguments: true,
            undefined_variable_in_bracket_indices: true,
            missing_file_severity: Some(DiagnosticSeverity::WARNING),
            case_mismatch_severity: CaseMismatchSeverity::Auto,
            circular_dependency_severity: Some(DiagnosticSeverity::ERROR),
            out_of_scope_severity: Some(DiagnosticSeverity::WARNING),
            report_unused_suppressions: false,
            max_chain_depth_severity: Some(DiagnosticSeverity::WARNING),
            on_demand_indexing_enabled: true,
            packages_enabled: true,
            packages_additional_library_paths: Vec::new(),
            packages_r_path: None,
            packages_missing_package_severity: Some(DiagnosticSeverity::WARNING),
            packages_report_generic_uninstalled: true,
            packages_namespace_member_severity: Some(DiagnosticSeverity::WARNING),
            packages_watch_library_paths: true,
            packages_watch_debounce_ms: 500,
            redundant_directive_severity: Some(DiagnosticSeverity::HINT),
            mixed_logical_severity: Some(DiagnosticSeverity::WARNING),
            condition_assignment_severity: Some(DiagnosticSeverity::WARNING),
            cache_metadata_max_entries: 1000,
            cache_file_content_max_entries: 500,
            cache_existence_max_entries: 2000,
            cache_workspace_index_max_entries: 5000,
            hoist_globals_in_functions: true,
            backward_dependencies: BackwardDependencyMode::Auto,
            max_transitive_dependents_visited: 50_000,
            package_mode: PackageMode::Auto,
            model_rprofile: true,
        }
    }
}

impl CrossFileConfig {
    /// Check if scope-affecting settings changed between two configs
    pub fn scope_settings_changed(&self, other: &Self) -> bool {
        self.assume_call_site != other.assume_call_site
            || self.max_chain_depth != other.max_chain_depth
            || self.max_backward_depth != other.max_backward_depth
            || self.max_forward_depth != other.max_forward_depth
            || self.hoist_globals_in_functions != other.hoist_globals_in_functions
            || self.backward_dependencies != other.backward_dependencies
            || self.max_transitive_dependents_visited != other.max_transitive_dependents_visited
            || self.package_mode != other.package_mode
            || self.model_rprofile != other.model_rprofile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let config = CrossFileConfig::default();
        assert_eq!(config.max_backward_depth, 10);
        assert_eq!(config.max_forward_depth, 10);
        assert_eq!(config.max_chain_depth, 64);
        assert_eq!(config.assume_call_site, CallSiteDefault::End);
        assert!(config.index_workspace);
        assert_eq!(config.max_revalidations_per_trigger, 10);
        assert_eq!(config.revalidation_debounce_ms, 200);
        assert_eq!(config.edited_file_debounce_ms, 50);
        assert!(!config.jags_diagnostics_enabled);
        assert!(!config.stan_diagnostics_enabled);
        assert_eq!(
            config.max_syntax_diagnostics_per_file,
            DEFAULT_MAX_SYNTAX_DIAGNOSTICS_PER_FILE
        );
        assert_eq!(
            config.undefined_variable_severity,
            Some(DiagnosticSeverity::WARNING)
        );
        // Issue #398: call-argument and bracket-index checking default on.
        assert!(config.undefined_variable_in_call_arguments);
        assert!(config.undefined_variable_in_bracket_indices);
        // On-demand indexing defaults
        assert!(config.on_demand_indexing_enabled);
        // Package awareness defaults
        assert!(config.packages_enabled);
        assert!(config.packages_additional_library_paths.is_empty());
        assert!(config.packages_r_path.is_none());
        assert_eq!(
            config.packages_missing_package_severity,
            Some(DiagnosticSeverity::WARNING)
        );
        // Redundant directive severity defaults
        assert_eq!(
            config.redundant_directive_severity,
            Some(DiagnosticSeverity::HINT)
        );
        // Hoist globals in functions default
        assert!(config.hoist_globals_in_functions);
        // Backward dependencies default
        assert_eq!(config.backward_dependencies, BackwardDependencyMode::Auto);
        assert_eq!(config.max_transitive_dependents_visited, 50_000);
        // Libpath watcher defaults — keep in sync with parse_cross_file_config
        // (which clamps debounce to [100, 5000]) and the VS Code extension's
        // initializationOptions.
        assert!(config.packages_watch_library_paths);
        assert_eq!(config.packages_watch_debounce_ms, 500);
    }

    #[test]
    fn test_scope_settings_changed() {
        let config1 = CrossFileConfig::default();
        let mut config2 = CrossFileConfig::default();

        // Same config should not be changed
        assert!(!config1.scope_settings_changed(&config2));

        // Changing assume_call_site should trigger change
        config2.assume_call_site = CallSiteDefault::Start;
        assert!(config1.scope_settings_changed(&config2));

        // Reset and change max_chain_depth
        config2 = CrossFileConfig::default();
        config2.max_chain_depth = 30;
        assert!(config1.scope_settings_changed(&config2));

        // Reset and change hoist_globals_in_functions
        config2 = CrossFileConfig::default();
        config2.hoist_globals_in_functions = false;
        assert!(config1.scope_settings_changed(&config2));

        // Reset and change backward_dependencies
        config2 = CrossFileConfig::default();
        config2.backward_dependencies = BackwardDependencyMode::Explicit;
        assert!(config1.scope_settings_changed(&config2));

        // Reset and change max_transitive_dependents_visited
        config2 = CrossFileConfig::default();
        config2.max_transitive_dependents_visited = 500;
        assert!(config1.scope_settings_changed(&config2));

        // Reset and change model_rprofile
        config2 = CrossFileConfig::default();
        config2.model_rprofile = false;
        assert!(config1.scope_settings_changed(&config2));
    }

    #[test]
    fn test_non_scope_settings_not_changed() {
        let config1 = CrossFileConfig::default();
        let mut config2 = CrossFileConfig::default();

        // Changing non-scope settings should not trigger scope change
        config2.revalidation_debounce_ms = 500;
        assert!(!config1.scope_settings_changed(&config2));

        config2.undefined_variable_severity = None;
        assert!(!config1.scope_settings_changed(&config2));

        config2 = CrossFileConfig::default();
        config2.jags_diagnostics_enabled = true;
        assert!(!config1.scope_settings_changed(&config2));

        config2 = CrossFileConfig::default();
        config2.stan_diagnostics_enabled = true;
        assert!(!config1.scope_settings_changed(&config2));

        // Non-None -> non-None transitions (e.g. WARNING -> ERROR) also do not
        // affect scope resolution; severity only impacts diagnostic emission.
        config2 = CrossFileConfig::default();
        config2.undefined_variable_severity = Some(DiagnosticSeverity::ERROR);
        assert!(!config1.scope_settings_changed(&config2));
    }

    #[test]
    fn case_mismatch_severity_default_is_auto_and_resolves_host_derived() {
        use super::super::path_resolve::CaseMismatchRegime;
        // Default is Auto.
        assert_eq!(
            CrossFileConfig::default().case_mismatch_severity,
            CaseMismatchSeverity::Auto
        );
        // Auto → information on a case-insensitive FS, warning on a case-sensitive FS.
        assert_eq!(
            CaseMismatchSeverity::Auto.resolve(CaseMismatchRegime::CaseInsensitiveFs),
            Some(DiagnosticSeverity::INFORMATION)
        );
        assert_eq!(
            CaseMismatchSeverity::Auto.resolve(CaseMismatchRegime::CaseSensitiveFs),
            Some(DiagnosticSeverity::WARNING)
        );
        // A pinned level overrides both regimes.
        assert_eq!(
            CaseMismatchSeverity::Fixed(DiagnosticSeverity::ERROR)
                .resolve(CaseMismatchRegime::CaseInsensitiveFs),
            Some(DiagnosticSeverity::ERROR)
        );
        assert_eq!(
            CaseMismatchSeverity::Fixed(DiagnosticSeverity::ERROR)
                .resolve(CaseMismatchRegime::CaseSensitiveFs),
            Some(DiagnosticSeverity::ERROR)
        );
        // Off suppresses in every regime.
        assert_eq!(
            CaseMismatchSeverity::Off.resolve(CaseMismatchRegime::CaseSensitiveFs),
            None
        );
    }

    #[test]
    fn test_diagnostics_enabled_default_is_true() {
        // Validates: Requirements 2.2
        // The diagnostics_enabled field should default to true so that
        // diagnostics are enabled by default when no configuration is provided
        let config = CrossFileConfig::default();
        assert!(
            config.diagnostics_enabled,
            "diagnostics_enabled should default to true"
        );
    }
}
