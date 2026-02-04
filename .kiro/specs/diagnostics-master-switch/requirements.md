# Requirements Document

## Introduction

This feature adds a master switch configuration option (`raven.diagnostics.enabled`) that allows users to completely enable or disable all diagnostics with a single setting. When disabled, Raven will suppress all diagnostic messages regardless of individual severity settings. This provides a quick way for users to turn off all diagnostics without modifying multiple individual settings.

## Glossary

- **Diagnostics_Master_Switch**: A boolean configuration option that controls whether any diagnostics are published to the editor
- **Raven**: The static R Language Server providing LSP features
- **CrossFileConfig**: The configuration struct that holds all cross-file and diagnostic settings
- **Diagnostics**: Error, warning, information, or hint messages displayed in the editor for code issues

## Requirements

### Requirement 1: Configuration Option

**User Story:** As a user, I want a single configuration option to enable or disable all diagnostics, so that I can quickly toggle diagnostic feedback without changing multiple settings.

#### Acceptance Criteria

1. THE Raven extension SHALL provide a `raven.diagnostics.enabled` configuration option in VS Code settings
2. THE `raven.diagnostics.enabled` option SHALL be a boolean type with a default value of `true`
3. WHEN `raven.diagnostics.enabled` is set to `true`, THE Diagnostics_Master_Switch SHALL allow all diagnostics to be published according to their individual severity settings
4. WHEN `raven.diagnostics.enabled` is set to `false`, THE Diagnostics_Master_Switch SHALL suppress all diagnostics regardless of individual severity settings

### Requirement 2: Server-Side Configuration

**User Story:** As a developer, I want the master switch to be handled in the LSP server configuration, so that it integrates cleanly with existing configuration infrastructure.

#### Acceptance Criteria

1. THE CrossFileConfig struct SHALL include a `diagnostics_enabled` boolean field
2. THE `diagnostics_enabled` field SHALL default to `true`
3. WHEN parsing initialization options, THE configuration parser SHALL read the `diagnostics.enabled` setting from the LSP client
4. IF the `diagnostics.enabled` setting is absent, THEN THE configuration parser SHALL use the default value of `true`

### Requirement 3: Diagnostics Suppression

**User Story:** As a user, I want all diagnostics to be suppressed when the master switch is disabled, so that I can work without any diagnostic noise.

#### Acceptance Criteria

1. WHEN `diagnostics_enabled` is `false`, THE diagnostics function SHALL return an empty vector
2. WHEN `diagnostics_enabled` is `false`, THE publish_diagnostics function SHALL publish an empty diagnostics array to clear existing diagnostics
3. WHEN `diagnostics_enabled` is `true`, THE diagnostics function SHALL compute and return diagnostics normally
4. THE Diagnostics_Master_Switch SHALL take precedence over all individual diagnostic severity settings

### Requirement 4: Runtime Configuration Changes

**User Story:** As a user, I want to toggle the master switch without restarting the language server, so that I can quickly enable or disable diagnostics during my workflow.

#### Acceptance Criteria

1. WHEN the `raven.diagnostics.enabled` setting changes at runtime, THE Raven server SHALL detect the configuration change
2. WHEN the master switch is toggled from enabled to disabled, THE Raven server SHALL clear diagnostics for all open documents
3. WHEN the master switch is toggled from disabled to enabled, THE Raven server SHALL recompute and publish diagnostics for all open documents
4. THE configuration change SHALL take effect without requiring a server restart

### Requirement 5: Logging

**User Story:** As a developer, I want the master switch state to be logged, so that I can debug configuration issues.

#### Acceptance Criteria

1. WHEN the configuration is loaded, THE Raven server SHALL log the `diagnostics_enabled` value
2. WHEN the master switch state changes at runtime, THE Raven server SHALL log the previous and new `diagnostics_enabled` values in the `did_change_configuration` handler
