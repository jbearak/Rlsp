//
// cross_file/config.rs
//
// Configuration for cross-file awareness
//

use std::path::PathBuf;
use tower_lsp::lsp_types::DiagnosticSeverity;

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
#[derive(Debug, Clone)]
pub struct CrossFileConfig {
    /// Maximum depth for backward directive traversal
    pub max_backward_depth: usize,
    /// Maximum depth for forward source() traversal
    pub max_forward_depth: usize,
    /// Maximum total chain depth
    pub max_chain_depth: usize,
    /// Default call site assumption when not specified
    pub assume_call_site: CallSiteDefault,
    /// Whether to index workspace files
    pub index_workspace: bool,
    /// Max number of open documents to schedule for diagnostics revalidation per trigger
    pub max_revalidations_per_trigger: usize,
    /// Debounce delay for cross-file diagnostics fanout in milliseconds
    pub revalidation_debounce_ms: u64,
    /// Whether undefined variable diagnostics are enabled
    pub undefined_variables_enabled: bool,
    /// Severity for missing file diagnostics
    pub missing_file_severity: DiagnosticSeverity,
    /// Severity for circular dependency diagnostics
    pub circular_dependency_severity: DiagnosticSeverity,
    /// Severity for out-of-scope symbol diagnostics
    pub out_of_scope_severity: DiagnosticSeverity,
    /// Severity for ambiguous parent diagnostics
    pub ambiguous_parent_severity: DiagnosticSeverity,
    /// Severity for max chain depth exceeded diagnostics
    pub max_chain_depth_severity: DiagnosticSeverity,
    /// Whether on-demand indexing is enabled
    pub on_demand_indexing_enabled: bool,
    /// Maximum transitive depth for on-demand indexing
    pub on_demand_indexing_max_transitive_depth: usize,
    /// Maximum queue size for background indexing
    pub on_demand_indexing_max_queue_size: usize,
    /// Whether package function awareness is enabled
    pub packages_enabled: bool,
    /// Additional R library paths for package discovery
    pub packages_additional_library_paths: Vec<PathBuf>,
    /// Path to R executable for subprocess calls
    pub packages_r_path: Option<PathBuf>,
    /// Severity for missing package diagnostics
    pub packages_missing_package_severity: DiagnosticSeverity,
}

impl Default for CrossFileConfig {
    /// Creates a CrossFileConfig populated with sensible defaults used by the cross-file awareness subsystem.
    ///
    /// The defaults enable workspace indexing, on-demand indexing, package awareness, and conservative traversal limits
    /// (backward/forward depth: 10, chain depth: 20). Diagnostic severities and debounce/revalidation limits are
    /// initialized to typical values for editor integrations.
    ///
    /// # Examples
    ///
    /// ```
    /// let cfg = CrossFileConfig::default();
    /// assert!(cfg.index_workspace);
    /// assert_eq!(cfg.max_chain_depth, 20);
    /// assert_eq!(cfg.max_backward_depth, 10);
    /// assert_eq!(cfg.max_forward_depth, 10);
    /// assert!(cfg.on_demand_indexing_enabled);
    /// assert!(cfg.packages_enabled);
    /// ```
    fn default() -> Self {
        Self {
            max_backward_depth: 10,
            max_forward_depth: 10,
            max_chain_depth: 20,
            assume_call_site: CallSiteDefault::End,
            index_workspace: true,
            max_revalidations_per_trigger: 10,
            revalidation_debounce_ms: 200,
            undefined_variables_enabled: true,
            missing_file_severity: DiagnosticSeverity::WARNING,
            circular_dependency_severity: DiagnosticSeverity::ERROR,
            out_of_scope_severity: DiagnosticSeverity::WARNING,
            ambiguous_parent_severity: DiagnosticSeverity::WARNING,
            max_chain_depth_severity: DiagnosticSeverity::WARNING,
            on_demand_indexing_enabled: true,
            on_demand_indexing_max_transitive_depth: 2,
            on_demand_indexing_max_queue_size: 50,
            packages_enabled: true,
            packages_additional_library_paths: Vec::new(),
            packages_r_path: None,
            packages_missing_package_severity: DiagnosticSeverity::WARNING,
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
        assert_eq!(config.max_chain_depth, 20);
        assert_eq!(config.assume_call_site, CallSiteDefault::End);
        assert!(config.index_workspace);
        assert_eq!(config.max_revalidations_per_trigger, 10);
        assert_eq!(config.revalidation_debounce_ms, 200);
        assert!(config.undefined_variables_enabled);
        // On-demand indexing defaults
        assert!(config.on_demand_indexing_enabled);
        assert_eq!(config.on_demand_indexing_max_transitive_depth, 2);
        assert_eq!(config.on_demand_indexing_max_queue_size, 50);
        // Package awareness defaults
        assert!(config.packages_enabled);
        assert!(config.packages_additional_library_paths.is_empty());
        assert!(config.packages_r_path.is_none());
        assert_eq!(
            config.packages_missing_package_severity,
            DiagnosticSeverity::WARNING
        );
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
    }

    #[test]
    fn test_non_scope_settings_not_changed() {
        let config1 = CrossFileConfig::default();
        let mut config2 = CrossFileConfig::default();

        // Changing non-scope settings should not trigger scope change
        config2.revalidation_debounce_ms = 500;
        assert!(!config1.scope_settings_changed(&config2));

        config2.undefined_variables_enabled = false;
        assert!(!config1.scope_settings_changed(&config2));
    }
}
