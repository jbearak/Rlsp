# Requirements Document: Cross-File Debugging

## Introduction

The cross-file awareness feature in Rlsp has been fully implemented according to specification, but symbols from sourced files are not being recognized in practice. This specification defines requirements for systematically debugging the implementation, identifying root causes, and fixing the issues to ensure cross-file symbol resolution works correctly.

## Glossary

- **LSP**: Language Server Protocol - the protocol used for communication between editor and language server
- **Rlsp**: R Language Server Protocol implementation - the static R language server
- **Cross_File_System**: The subsystem responsible for tracking dependencies and resolving symbols across multiple R files
- **Metadata_Extractor**: Component that extracts source() calls and directives from R files
- **Dependency_Graph**: Data structure tracking which files source which other files
- **Scope_Resolver**: Component that determines which symbols are available at a given position
- **Source_Call**: R function call `source("path.r")` that loads another R file
- **Symbol**: A named entity (function, variable) defined in R code
- **Diagnostic**: LSP term for errors, warnings, or information messages shown in the editor
- **WorldState**: The main state container in Rlsp that holds all workspace information

## Requirements

### Requirement 1: Diagnostic Logging Infrastructure

**User Story:** As a developer, I want comprehensive logging throughout the cross-file system, so that I can trace execution flow and identify where the system is failing.

#### Acceptance Criteria

1. WHEN a document is opened or changed, THE Rlsp SHALL log the metadata extraction process with file path and extracted source calls
2. WHEN the dependency graph is updated, THE Rlsp SHALL log all edges added or removed with source and target paths
3. WHEN scope resolution is performed, THE Rlsp SHALL log the position, file path, and number of symbols found
4. WHEN path resolution occurs, THE Rlsp SHALL log the input path, working directory, and resolved canonical path
5. WHEN errors occur in any cross-file component, THE Rlsp SHALL log the error with full context including file paths and positions
6. WHEN LSP handlers are invoked, THE Rlsp SHALL log whether cross-file resolution is being attempted
7. THE Rlsp SHALL use log::trace level for detailed execution flow and log::info for significant events

### Requirement 2: Metadata Extraction Verification

**User Story:** As a developer, I want to verify that metadata extraction runs correctly, so that I can confirm source() calls and directives are being detected.

#### Acceptance Criteria

1. WHEN a file containing source() calls is opened, THE Metadata_Extractor SHALL detect all source() calls with correct paths
2. WHEN a file contains backward directives like `@lsp-run-by: ../oos.r`, THE Metadata_Extractor SHALL parse them correctly with optional colon and quotes
3. WHEN a file contains working directory directives, THE Metadata_Extractor SHALL extract the working directory path
4. WHEN backward directive paths are relative, THE Metadata_Extractor SHALL resolve them relative to the file containing the directive
5. WHEN metadata extraction completes, THE Cross_File_System SHALL store the metadata in the cache
6. IF metadata extraction fails, THEN THE Rlsp SHALL log the error and continue without crashing
7. WHEN a file is modified, THE Metadata_Extractor SHALL re-extract metadata and update the cache
8. WHEN backward directives reference non-existent files, THE Rlsp SHALL log a clear error with the attempted path

### Requirement 3: Dependency Graph Verification

**User Story:** As a developer, I want to verify the dependency graph is populated correctly, so that I can confirm file relationships are tracked.

#### Acceptance Criteria

1. WHEN a file with source() calls has metadata extracted, THE Dependency_Graph SHALL create forward edges from parent to child
2. WHEN a file with backward directives has metadata extracted, THE Dependency_Graph SHALL create or confirm forward edges
3. WHEN querying parents of a file, THE Dependency_Graph SHALL return all files that source it
4. WHEN querying children of a file, THE Dependency_Graph SHALL return all files it sources
5. WHEN edges conflict between directives and AST, THE Dependency_Graph SHALL apply correct conflict resolution rules
6. THE Dependency_Graph SHALL store call site positions in UTF-16 columns for each edge

### Requirement 4: Path Resolution Verification

**User Story:** As a developer, I want to verify path resolution works correctly, so that I can confirm relative paths like "../oos.r" are being resolved to canonical paths.

#### Acceptance Criteria

1. WHEN resolving a relative path with a working directory, THE Path_Resolver SHALL resolve relative to that directory
2. WHEN resolving a relative path without a working directory, THE Path_Resolver SHALL resolve relative to the file's directory
3. WHEN resolving a path starting with "..", THE Path_Resolver SHALL correctly navigate up directory levels
4. WHEN resolving an absolute path, THE Path_Resolver SHALL use it directly after canonicalization
5. WHEN a path cannot be resolved, THE Path_Resolver SHALL return an error with the attempted path and base directory
6. WHEN paths contain ".." or "." components, THE Path_Resolver SHALL normalize them correctly
7. THE Path_Resolver SHALL handle both forward slashes and backslashes on all platforms
8. WHEN resolving paths for backward directives, THE Path_Resolver SHALL use the directive file's directory as the base

### Requirement 5: Scope Resolution Verification

**User Story:** As a developer, I want to verify scope resolution returns correct symbols, so that I can confirm symbols from sourced files are available.

#### Acceptance Criteria

1. WHEN resolving scope at a position after a source() call, THE Scope_Resolver SHALL include symbols from the sourced file
2. WHEN resolving scope with multiple source() calls, THE Scope_Resolver SHALL include symbols from all sourced files
3. WHEN resolving scope with chained source() calls, THE Scope_Resolver SHALL traverse the chain up to max_chain_depth
4. WHEN local symbols conflict with sourced symbols, THE Scope_Resolver SHALL prioritize local symbols
5. WHEN scope resolution encounters a cycle, THE Scope_Resolver SHALL detect it and stop traversal
6. THE Scope_Resolver SHALL return symbols with their names, types, and source file paths

### Requirement 6: LSP Handler Integration Verification

**User Story:** As a developer, I want to verify LSP handlers use cross-file resolution, so that I can confirm the feature is actually being invoked.

#### Acceptance Criteria

1. WHEN a completion request is received, THE LSP_Handler SHALL call cross-file scope resolution
2. WHEN a hover request is received for a symbol, THE LSP_Handler SHALL check cross-file scope for symbol information
3. WHEN a definition request is received, THE LSP_Handler SHALL search cross-file scope for the symbol definition
4. WHEN diagnostics are computed, THE LSP_Handler SHALL use cross-file scope to validate symbol references
5. WHEN a document is opened, THE LSP_Handler SHALL trigger metadata extraction and dependency graph update
6. WHEN a document is changed, THE LSP_Handler SHALL trigger revalidation of affected files

### Requirement 7: Real-World Test Case Reproduction

**User Story:** As a developer, I want test cases that reproduce the real-world failure, so that I can verify fixes work for actual usage scenarios.

#### Acceptance Criteria

1. THE Test_Suite SHALL include a test case with file A sourcing file B and using symbols from B
2. THE Test_Suite SHALL include a test case matching the validation_functions/collate.r scenario with source() calls
3. THE Test_Suite SHALL include a test case with backward directive `@lsp-run-by: ../oos.r` reporting "parent file not found"
4. THE Test_Suite SHALL verify that symbols from sourced files appear in completion results
5. THE Test_Suite SHALL verify that symbols from sourced files do not show diagnostics errors
6. THE Test_Suite SHALL include test cases with relative paths like "validation_functions/get_colnames.r" and "../oos.r"
7. THE Test_Suite SHALL verify behavior with both open and closed sourced files
8. THE Test_Suite SHALL verify backward directives correctly resolve parent file paths

### Requirement 8: Configuration Verification

**User Story:** As a developer, I want to verify cross-file configuration is correct, so that I can confirm the feature is enabled and configured properly.

#### Acceptance Criteria

1. WHEN Rlsp initializes, THE Configuration_System SHALL load cross-file settings from initialization options
2. WHEN cross-file is disabled in configuration, THE Cross_File_System SHALL not perform cross-file resolution
3. WHEN max_chain_depth is configured, THE Scope_Resolver SHALL respect the limit
4. WHEN diagnostic severities are configured, THE Diagnostic_System SHALL use the configured levels
5. THE Configuration_System SHALL log the loaded cross-file configuration at startup
6. IF configuration is invalid, THEN THE Configuration_System SHALL use safe defaults and log a warning

### Requirement 9: Error Handling Verification

**User Story:** As a developer, I want to verify errors are not being silently swallowed, so that I can identify failure points.

#### Acceptance Criteria

1. WHEN any cross-file operation fails, THE Rlsp SHALL log the error with full context
2. WHEN path resolution fails, THE Rlsp SHALL log the attempted path and reason for failure
3. WHEN file reading fails, THE Rlsp SHALL log the file path and IO error
4. WHEN parsing fails, THE Rlsp SHALL log the file path and parse error location
5. WHEN cache operations fail, THE Rlsp SHALL log the cache key and error
6. THE Rlsp SHALL continue operating after non-fatal errors without crashing

### Requirement 10: Integration Point Verification

**User Story:** As a developer, I want to verify all integration points between components work correctly, so that I can confirm data flows through the system.

#### Acceptance Criteria

1. WHEN metadata is extracted, THE Dependency_Graph SHALL receive and process the metadata
2. WHEN the dependency graph is updated, THE Cache_System SHALL invalidate affected entries
3. WHEN scope resolution needs file content, THE Content_Provider SHALL supply it from open documents or disk
4. WHEN revalidation is triggered, THE Revalidation_System SHALL identify affected files and publish diagnostics
5. WHEN workspace indexing runs, THE Workspace_Index SHALL discover closed files and extract their symbols
6. THE Integration SHALL maintain thread-safety with proper locking of WorldState

### Requirement 11: Bug Fix Implementation

**User Story:** As a developer, I want identified bugs to be fixed, so that cross-file awareness works correctly in practice.

#### Acceptance Criteria

1. WHEN bugs are identified through debugging, THE Developer SHALL implement fixes that address root causes
2. WHEN fixes are implemented, THE Test_Suite SHALL verify the fixes resolve the original failure scenarios
3. WHEN fixes are implemented, THE Test_Suite SHALL verify no regressions are introduced
4. WHEN fixes involve algorithm changes, THE Developer SHALL update relevant documentation
5. WHEN fixes involve new error cases, THE Developer SHALL add appropriate error handling and logging
6. THE Fixes SHALL maintain backward compatibility with existing directives and configuration

### Requirement 12: Diagnostic Output Analysis

**User Story:** As a developer, I want to analyze diagnostic output from the LSP, so that I can understand what the server is reporting about symbols.

#### Acceptance Criteria

1. WHEN diagnostics are published, THE Rlsp SHALL log which diagnostics are being sent for which files
2. WHEN a symbol is marked as undefined, THE Diagnostic_System SHALL log why it was not found in scope
3. WHEN cross-file diagnostics are generated, THE Rlsp SHALL log the severity and message
4. WHEN diagnostics are debounced, THE Revalidation_System SHALL log the debounce timing
5. WHEN stale diagnostics are prevented, THE Revalidation_System SHALL log the freshness check result
6. THE Diagnostic_System SHALL distinguish between local-only and cross-file-aware diagnostic checks
