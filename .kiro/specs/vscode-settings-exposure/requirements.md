# Requirements Document

## Introduction

This feature exposes all Raven LSP server configuration settings through the VS Code extension's settings UI. Currently, the VS Code extension only exposes 2 settings (`raven.server.path` and `raven.editor.dotInWordSeparators`), while the LSP server supports comprehensive configuration for cross-file awareness, diagnostics, and package management. This feature bridges that gap by adding all LSP configuration options to VS Code's `package.json` and wiring them to the LSP client's `initializationOptions`.

## Glossary

- **LSP**: Language Server Protocol - the communication protocol between the VS Code extension (client) and the Raven language server
- **Extension**: The VS Code extension located in `editors/vscode/`
- **Initialization_Options**: JSON configuration passed from the LSP client to the server during the `initialize` request
- **Cross_File_Config**: The Rust struct in the LSP server that holds all cross-file awareness settings
- **Severity**: Diagnostic severity level (error, warning, information, hint)
- **On_Demand_Indexing**: Background indexing of files not currently open in the editor

## Requirements

### Requirement 1: Cross-File Depth Settings

**User Story:** As a developer, I want to configure traversal depth limits for cross-file analysis, so that I can balance analysis thoroughness with performance.

#### Acceptance Criteria

1. THE Extension SHALL expose `raven.crossFile.maxBackwardDepth` as a number setting with default 10
2. THE Extension SHALL expose `raven.crossFile.maxForwardDepth` as a number setting with default 10
3. THE Extension SHALL expose `raven.crossFile.maxChainDepth` as a number setting with default 20
4. WHEN any depth setting is configured, THE Extension SHALL pass it to the LSP server via initializationOptions

### Requirement 2: Call Site Assumption Setting

**User Story:** As a developer, I want to configure the default call site assumption, so that I can control symbol availability behavior when call sites cannot be determined.

#### Acceptance Criteria

1. THE Extension SHALL expose `raven.crossFile.assumeCallSite` as an enum setting with values "start" and "end"
2. THE Extension SHALL use "end" as the default value for assumeCallSite
3. WHEN assumeCallSite is configured, THE Extension SHALL pass it to the LSP server via initializationOptions

### Requirement 3: Workspace Indexing Setting

**User Story:** As a developer, I want to enable or disable workspace indexing, so that I can control whether closed files are indexed for cross-file awareness.

#### Acceptance Criteria

1. THE Extension SHALL expose `raven.crossFile.indexWorkspace` as a boolean setting with default true
2. WHEN indexWorkspace is configured, THE Extension SHALL pass it to the LSP server via initializationOptions

### Requirement 4: Revalidation Settings

**User Story:** As a developer, I want to configure revalidation behavior, so that I can tune the responsiveness and performance of cross-file diagnostics.

#### Acceptance Criteria

1. THE Extension SHALL expose `raven.crossFile.maxRevalidationsPerTrigger` as a number setting with default 10
2. THE Extension SHALL expose `raven.crossFile.revalidationDebounceMs` as a number setting with default 200
3. WHEN any revalidation setting is configured, THE Extension SHALL pass it to the LSP server via initializationOptions

### Requirement 5: Cross-File Diagnostic Severity Settings

**User Story:** As a developer, I want to configure the severity of cross-file diagnostics, so that I can customize how issues are reported based on my workflow.

#### Acceptance Criteria

1. THE Extension SHALL expose `raven.crossFile.missingFileSeverity` as an enum setting with values "error", "warning", "information", "hint" and default "warning"
2. THE Extension SHALL expose `raven.crossFile.circularDependencySeverity` as an enum setting with values "error", "warning", "information", "hint" and default "error"
3. THE Extension SHALL expose `raven.crossFile.outOfScopeSeverity` as an enum setting with values "error", "warning", "information", "hint" and default "warning"
4. THE Extension SHALL expose `raven.crossFile.ambiguousParentSeverity` as an enum setting with values "error", "warning", "information", "hint" and default "warning"
5. THE Extension SHALL expose `raven.crossFile.maxChainDepthSeverity` as an enum setting with values "error", "warning", "information", "hint" and default "warning"
6. WHEN any severity setting is configured, THE Extension SHALL pass it to the LSP server via initializationOptions

### Requirement 6: On-Demand Indexing Settings

**User Story:** As a developer, I want to configure on-demand indexing behavior, so that I can control background indexing of transitive dependencies.

#### Acceptance Criteria

1. THE Extension SHALL expose `raven.crossFile.onDemandIndexing.enabled` as a boolean setting with default true
2. THE Extension SHALL expose `raven.crossFile.onDemandIndexing.maxTransitiveDepth` as a number setting with default 2
3. THE Extension SHALL expose `raven.crossFile.onDemandIndexing.maxQueueSize` as a number setting with default 50
4. WHEN any on-demand indexing setting is configured, THE Extension SHALL pass it to the LSP server via initializationOptions with the nested structure `crossFile.onDemandIndexing`

### Requirement 7: Diagnostics Settings

**User Story:** As a developer, I want to enable or disable undefined variable diagnostics, so that I can control whether undefined variables are reported.

#### Acceptance Criteria

1. THE Extension SHALL expose `raven.diagnostics.undefinedVariables` as a boolean setting with default true
2. WHEN undefinedVariables is configured, THE Extension SHALL pass it to the LSP server via initializationOptions under the `diagnostics` key

### Requirement 8: Package Awareness Settings

**User Story:** As a developer, I want to configure package awareness features, so that I can control how Raven discovers and uses R package information.

#### Acceptance Criteria

1. THE Extension SHALL expose `raven.packages.enabled` as a boolean setting with default true
2. THE Extension SHALL expose `raven.packages.additionalLibraryPaths` as an array of strings setting with default empty array
3. THE Extension SHALL expose `raven.packages.rPath` as a string setting with default empty string
4. THE Extension SHALL expose `raven.packages.missingPackageSeverity` as an enum setting with values "error", "warning", "information", "hint" and default "warning"
5. WHEN any package setting is configured, THE Extension SHALL pass it to the LSP server via initializationOptions under the `packages` key

### Requirement 9: Settings Descriptions

**User Story:** As a developer, I want clear descriptions for each setting, so that I can understand what each option does without consulting external documentation.

#### Acceptance Criteria

1. THE Extension SHALL provide a description for each setting that explains its purpose and effect
2. THE Extension SHALL include the default value in each setting's description where helpful
3. THE Extension SHALL group related settings under logical categories in the VS Code settings UI

### Requirement 10: Initialization Options Transmission

**User Story:** As a developer, I want my configured settings to be sent to the LSP server, so that my preferences take effect.

#### Acceptance Criteria

1. WHEN the LSP client initializes, THE Extension SHALL read all raven.* settings from VS Code configuration
2. WHEN the LSP client initializes, THE Extension SHALL construct an initializationOptions object matching the LSP server's expected JSON structure
3. THE Extension SHALL pass the initializationOptions to the LanguageClient constructor
4. IF a setting is not configured, THE Extension SHALL omit it from initializationOptions to allow server defaults

### Requirement 11: Configuration Change Handling

**User Story:** As a developer, I want settings changes to take effect without manually restarting the extension, so that I can iterate on my configuration quickly.

#### Acceptance Criteria

1. WHEN a raven.* setting changes, THE Extension SHALL detect the configuration change
2. WHEN a configuration change is detected, THE Extension SHALL send a `workspace/didChangeConfiguration` notification to the LSP server with the updated settings
3. THE Extension SHALL NOT require a window reload for configuration changes to take effect
