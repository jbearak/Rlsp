# Implementation Plan: VS Code Settings Exposure

## Overview

This implementation adds all Raven LSP configuration settings to the VS Code extension. The work involves updating `package.json` with setting definitions and modifying `extension.ts` to read settings and pass them to the LSP client.

## Tasks

- [x] 1. Add cross-file depth settings to package.json
  - Add `raven.crossFile.maxBackwardDepth` (number, default 10)
  - Add `raven.crossFile.maxForwardDepth` (number, default 10)
  - Add `raven.crossFile.maxChainDepth` (number, default 20)
  - Include descriptions explaining each setting's purpose
  - _Requirements: 1.1, 1.2, 1.3, 9.1_

- [x] 2. Add cross-file behavior settings to package.json
  - Add `raven.crossFile.assumeCallSite` (enum: "start", "end", default "end")
  - Add `raven.crossFile.indexWorkspace` (boolean, default true)
  - Add `raven.crossFile.maxRevalidationsPerTrigger` (number, default 10)
  - Add `raven.crossFile.revalidationDebounceMs` (number, default 200)
  - Include descriptions for each setting
  - _Requirements: 2.1, 2.2, 3.1, 4.1, 4.2, 9.1_

- [x] 3. Add cross-file diagnostic severity settings to package.json
  - Add `raven.crossFile.missingFileSeverity` (enum: error/warning/information/hint, default "warning")
  - Add `raven.crossFile.circularDependencySeverity` (enum, default "error")
  - Add `raven.crossFile.outOfScopeSeverity` (enum, default "warning")
  - Add `raven.crossFile.ambiguousParentSeverity` (enum, default "warning")
  - Add `raven.crossFile.maxChainDepthSeverity` (enum, default "warning")
  - Include descriptions for each severity setting
  - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5, 9.1_

- [x] 4. Add on-demand indexing settings to package.json
  - Add `raven.crossFile.onDemandIndexing.enabled` (boolean, default true)
  - Add `raven.crossFile.onDemandIndexing.maxTransitiveDepth` (number, default 2)
  - Add `raven.crossFile.onDemandIndexing.maxQueueSize` (number, default 50)
  - Include descriptions for each setting
  - _Requirements: 6.1, 6.2, 6.3, 9.1_

- [x] 5. Add diagnostics settings to package.json
  - Add `raven.diagnostics.undefinedVariables` (boolean, default true)
  - Include description explaining the setting
  - _Requirements: 7.1, 9.1_

- [x] 6. Add package awareness settings to package.json
  - Add `raven.packages.enabled` (boolean, default true)
  - Add `raven.packages.additionalLibraryPaths` (array of strings, default [])
  - Add `raven.packages.rPath` (string, default "")
  - Add `raven.packages.missingPackageSeverity` (enum, default "warning")
  - Include descriptions for each setting
  - _Requirements: 8.1, 8.2, 8.3, 8.4, 9.1_

- [x] 7. Checkpoint - Verify package.json schema
  - Ensure all tests pass, ask the user if questions arise.

- [x] 8. Implement getInitializationOptions function in extension.ts
  - [x] 8.1 Create TypeScript interface for RavenInitializationOptions
    - Define interface matching LSP server's expected JSON structure
    - Include optional fields for all settings
    - _Requirements: 10.2_
  
  - [x] 8.2 Implement getInitializationOptions function
    - Read all raven.* settings from VS Code configuration
    - Construct initializationOptions object with correct JSON paths
    - Only include explicitly configured settings (omit undefined)
    - _Requirements: 10.1, 10.4_
  
  - [x] 8.3 Write property test for settings transmission
    - **Property 1: Settings Transmission Correctness**
    - **Validates: Requirements 1.4, 2.3, 3.2, 4.3, 5.6, 6.4, 7.2, 8.5, 10.2**

- [x] 9. Wire initializationOptions to LanguageClient
  - Update LanguageClient constructor to include initializationOptions
  - Call getInitializationOptions() during activation
  - _Requirements: 10.3_

- [x] 10. Implement configuration change handler
  - [x] 10.1 Register onDidChangeConfiguration listener
    - Listen for changes to raven.* settings
    - _Requirements: 11.1_
  
  - [x] 10.2 Send didChangeConfiguration notification
    - When settings change, read new configuration
    - Send workspace/didChangeConfiguration notification to LSP server
    - _Requirements: 11.2, 11.3_

- [x] 11. Checkpoint - Verify end-to-end functionality
  - Ensure all tests pass, ask the user if questions arise.

- [x] 12. Write unit tests for settings reader
  - Test getInitializationOptions with various configurations
  - Test that unconfigured settings are omitted
  - Test nested settings structure (onDemandIndexing)
  - _Requirements: 10.2, 10.4_

## Notes

- The LSP server already supports `workspace/didChangeConfiguration` - no server changes needed
- All settings use the `raven.` prefix for consistency with existing settings
- Settings are organized into logical groups: crossFile, diagnostics, packages
