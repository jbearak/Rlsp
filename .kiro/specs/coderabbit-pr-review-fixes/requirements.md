# Requirements Document

## Introduction

This document specifies the requirements for addressing CodeRabbit PR review feedback on PR #2 for the Rlsp R Language Server project. The feedback includes code quality issues, bug fixes, and documentation improvements identified during automated code review.

## Glossary

- **Rlsp**: R Language Server Protocol implementation extracted from Ark
- **LSP**: Language Server Protocol
- **On-Demand_Indexing**: Background indexing system for files not currently open in the editor
- **Backward_Directive**: LSP directive like `@lsp-sourced-by` that declares a parent file relationship
- **Forward_Source**: A `source()` call or `@lsp-source` directive that includes another file
- **PathContext**: Context object for resolving file paths relative to working directory or file location
- **UTF-16**: Character encoding used by LSP for column positions

## Requirements

### Requirement 1: On-Demand Indexing Global Flag Check

**User Story:** As a developer, I want on-demand indexing to respect the global enabled flag, so that I can completely disable the feature when needed.

#### Acceptance Criteria

1. WHEN `state.cross_file_config.on_demand_indexing_enabled` is false, THE Backend SHALL skip all Priority 1 synchronous indexing
2. WHEN `state.cross_file_config.on_demand_indexing_enabled` is false, THE Backend SHALL skip all Priority 2 background indexing submission
3. WHEN `state.cross_file_config.on_demand_indexing_enabled` is false, THE Backend SHALL skip all Priority 3 transitive dependency queuing
4. THE Backend SHALL check the global flag early in the `did_open` handler before any indexing work

### Requirement 2: Directive Regex Quoted Paths with Spaces

**User Story:** As a developer, I want to use quoted paths with spaces in LSP directives, so that I can reference files in directories with spaces in their names.

#### Acceptance Criteria

1. WHEN a backward directive contains a double-quoted path with spaces, THE Directive_Parser SHALL extract the path correctly
2. WHEN a backward directive contains a single-quoted path with spaces, THE Directive_Parser SHALL extract the path correctly
3. WHEN a forward directive contains a quoted path with spaces, THE Directive_Parser SHALL extract the path correctly
4. WHEN a working directory directive contains a quoted path with spaces, THE Directive_Parser SHALL extract the path correctly
5. THE Directive_Parser SHALL provide a helper function to extract the path from the correct capture group
6. THE Directive_Parser SHALL include unit tests for quoted paths with spaces

### Requirement 3: Parent Resolution Child Path Fix

**User Story:** As a developer, I want parent resolution to use the correct child path, so that match patterns and call-site inference work correctly.

#### Acceptance Criteria

1. WHEN resolving match patterns, THE Parent_Resolver SHALL use the child file path derived from child_uri
2. WHEN inferring call sites from parent content, THE Parent_Resolver SHALL use the child file path derived from child_uri
3. THE Parent_Resolver SHALL NOT use directive.path (the parent path) when the child path is needed

### Requirement 4: Path Normalization ParentDir Fix

**User Story:** As a developer, I want path normalization to handle edge cases correctly, so that absolute paths like "/../a" don't become relative.

#### Acceptance Criteria

1. WHEN normalizing a path with ParentDir (..) component, THE Path_Normalizer SHALL only pop a prior component if it is a Normal segment
2. WHEN normalizing a path with ParentDir after RootDir, THE Path_Normalizer SHALL preserve the RootDir component
3. WHEN normalizing a path with ParentDir after Prefix, THE Path_Normalizer SHALL preserve the Prefix component
4. THE Path_Normalizer SHALL include unit tests for edge cases like "/../a"

### Requirement 5: Diagnostic Range Precision

**User Story:** As a developer, I want diagnostic ranges to precisely highlight the problematic code, so that I can quickly identify issues.

#### Acceptance Criteria

1. WHEN creating a diagnostic range for a source path, THE Diagnostics_Handler SHALL use the actual path length for the end column
2. THE Diagnostics_Handler SHALL NOT use arbitrary offsets like "+ 10" for range calculation

### Requirement 6: Remove Dead Code

**User Story:** As a developer, I want the codebase to be clean without dead code, so that maintenance is easier.

#### Acceptance Criteria

1. THE Handlers module SHALL NOT contain the unused `collect_identifier_usages` function
2. THE Handlers module SHALL retain only the UTF-16 variant `collect_identifier_usages_utf16`

### Requirement 7: IndexEntry Comment Accuracy

**User Story:** As a developer, I want code comments to accurately describe behavior, so that I can understand the code correctly.

#### Acceptance Criteria

1. IF `CrossFileWorkspaceIndex::insert()` does not modify `indexed_at_version`, THEN THE comment SHALL be updated to reflect this
2. THE comment or code SHALL accurately describe when and how `indexed_at_version` is set

### Requirement 8: Non-Blocking File Existence Check

**User Story:** As a developer, I want the LSP server to remain responsive, so that file existence checks don't block request handling.

#### Acceptance Criteria

1. WHEN checking if a file exists for diagnostics, THE Diagnostics_Handler SHALL NOT perform blocking filesystem I/O on the request thread
2. THE Diagnostics_Handler SHALL use async/background task or queued worker for filesystem checks
3. IF a file is not in any cache, THE Diagnostics_Handler SHALL either skip the diagnostic or queue an async check

### Requirement 9: Markdown Code Block Language Tags

**User Story:** As a documentation reader, I want code blocks to have proper language tags, so that syntax highlighting works correctly.

#### Acceptance Criteria

1. THE checkpoint-4-findings.md file SHALL have `text` language tag on the test output code block at line 239
2. THE final-checkpoint-report.md file SHALL have `text` language tags on all four test output code blocks
3. THE task-10.2-findings.md file SHALL have `text` language tag on the output block at line 140
4. THE task-16.3-build-results.md file SHALL have `text` language tags on both code blocks at lines 16 and 61

### Requirement 10: Markdown Heading Format

**User Story:** As a documentation reader, I want consistent heading formatting, so that documents are well-structured.

#### Acceptance Criteria

1. THE task-16.3-build-results.md file SHALL use a proper Markdown heading instead of bold text for "Task 16.3 Build Phase: COMPLETE ✅"

### Requirement 11: Design Document Diagram Language Tag

**User Story:** As a documentation reader, I want diagram blocks to have proper language tags, so that rendering tools can process them correctly.

#### Acceptance Criteria

1. THE priority-2-3-indexing/design.md file SHALL have `text` language tag on the fenced diagram block at line 11
