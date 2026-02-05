# Requirements Document

## Introduction

This document specifies the requirements for adding the `@lsp-source` forward directive to Raven, the R Language Server. The `@lsp-source` directive allows developers to explicitly declare that a file sources another file, complementing the existing backward directives (`@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`). This is useful when `source()` calls are conditional, dynamic, or use variables for paths.

## Glossary

- **Directive_Parser**: The module responsible for parsing LSP directives from R file comments
- **Dependency_Graph**: The data structure tracking source relationships between files
- **Forward_Directive**: A directive placed in a parent file that points to a child file it sources
- **Backward_Directive**: A directive placed in a child file that points to a parent file that sources it
- **Call_Site**: The position (line, column) in a parent file where a source relationship originates
- **Path_Resolver**: The module responsible for resolving relative and absolute file paths
- **Scope_Resolver**: The module responsible for determining which symbols are available at a given position

## Requirements

### Requirement 1: Basic Directive Parsing

**User Story:** As a developer, I want to use `@lsp-source` directives to explicitly declare file dependencies, so that the LSP understands relationships that cannot be detected from `source()` calls.

#### Acceptance Criteria

1. WHEN a comment contains `@lsp-source` followed by a path, THE Directive_Parser SHALL extract the path and create a ForwardSource entry with `is_directive=true`
2. WHEN a comment contains `@lsp-run` followed by a path, THE Directive_Parser SHALL parse it identically to `@lsp-source` (synonym)
3. WHEN a comment contains `@lsp-include` followed by a path, THE Directive_Parser SHALL parse it identically to `@lsp-source` (synonym)
4. WHEN a comment contains any of the above without `@` prefix (e.g., `lsp-source`), THE Directive_Parser SHALL parse it identically to the `@`-prefixed version
5. WHEN the directive uses an optional colon separator (`@lsp-source: path`), THE Directive_Parser SHALL parse it correctly
6. WHEN the path is double-quoted (`@lsp-source "path"`), THE Directive_Parser SHALL extract the path without quotes
7. WHEN the path is single-quoted (`@lsp-source 'path'`), THE Directive_Parser SHALL extract the path without quotes
8. WHEN the path contains spaces and is quoted (`@lsp-source "path with spaces/file.R"`), THE Directive_Parser SHALL preserve the spaces in the extracted path
9. WHEN the path is unquoted and contains no spaces, THE Directive_Parser SHALL extract the path correctly

### Requirement 2: Call-Site Specification

**User Story:** As a developer, I want to specify where in the file the source relationship should be considered active, so that position-aware scope resolution works correctly.

#### Acceptance Criteria

1. WHEN the directive includes `line=N` parameter, THE Directive_Parser SHALL store the call site as line N-1 (converting from 1-based user input to 0-based internal)
2. WHEN the directive includes no call-site parameter, THE Directive_Parser SHALL use the directive's own line as the call site
3. WHEN multiple `@lsp-source` directives exist in a file, THE Directive_Parser SHALL create separate ForwardSource entries for each
4. THE ForwardSource entry SHALL have `column=0` for directive-based sources (directives don't have meaningful column positions)

### Requirement 3: Path Resolution

**User Story:** As a developer, I want `@lsp-source` paths to resolve correctly relative to the file's directory or workspace root, so that the dependency graph is accurate.

#### Acceptance Criteria

1. WHEN the path is relative (e.g., `utils.R`, `../shared/common.R`), THE Path_Resolver SHALL resolve it relative to the directive's file directory
2. WHEN the path starts with `/` (workspace-root-relative), THE Path_Resolver SHALL resolve it relative to the workspace root
3. WHEN the resolved path does not exist, THE Dependency_Graph SHALL NOT create an edge for that directive
4. WHEN the file has an `@lsp-cd` directive, THE Path_Resolver SHALL use the working directory for resolving `@lsp-source` paths (unlike backward directives which ignore `@lsp-cd`)

### Requirement 4: Dependency Graph Integration

**User Story:** As a developer, I want `@lsp-source` directives to create proper edges in the dependency graph, so that cross-file features work correctly.

#### Acceptance Criteria

1. WHEN an `@lsp-source` directive is parsed, THE Dependency_Graph SHALL create a forward edge from the parent file to the child file
2. WHEN the edge is created from a directive, THE DependencyEdge SHALL have `is_directive=true` and `is_backward_directive=false`
3. WHEN both an `@lsp-source` directive and a `source()` call point to the same file at the same call site, THE Dependency_Graph SHALL keep only the directive edge (directive wins)
4. WHEN an `@lsp-source` directive points to the same file as a `source()` call but at different call sites, THE Dependency_Graph SHALL keep both edges (symbols become available at the earliest call site)
5. WHEN an `@lsp-source` directive has no explicit call site and a `source()` call exists to the same file at an earlier line, THE Dependency_Graph SHALL keep the AST edge (earliest call site wins) and MAY emit an informational note that the directive is redundant

### Requirement 5: Scope Resolution

**User Story:** As a developer, I want symbols from files referenced by `@lsp-source` to be available in completions and hover, so that I get accurate IDE features.

#### Acceptance Criteria

1. WHEN a file has an `@lsp-source` directive, THE Scope_Resolver SHALL include symbols from the sourced file in scope after the directive's line
2. WHEN the directive specifies `line=N`, THE Scope_Resolver SHALL include symbols from the sourced file in scope starting at line N
3. WHEN the sourced file defines functions or variables, THE Scope_Resolver SHALL make them available for completions, hover, and go-to-definition

### Requirement 6: Diagnostics

**User Story:** As a developer, I want to see helpful diagnostics when `@lsp-source` directives have issues, so that I can fix problems quickly.

#### Acceptance Criteria

1. WHEN an `@lsp-source` directive references a file that does not exist, THE system SHALL emit a diagnostic warning at the directive line
2. WHEN an `@lsp-source` directive without an explicit `line=N` parameter targets the same file as an AST-detected `source()` call at an earlier line, THE system MAY emit an informational diagnostic noting the directive is redundant
3. THE diagnostic severity for missing files SHALL be configurable via LSP settings

### Requirement 7: Revalidation

**User Story:** As a developer, I want changes to `@lsp-source` directives to trigger appropriate revalidation, so that the IDE stays up-to-date.

#### Acceptance Criteria

1. WHEN an `@lsp-source` directive is added to a file, THE system SHALL update the dependency graph and revalidate affected files
2. WHEN an `@lsp-source` directive is removed from a file, THE system SHALL remove the corresponding edge and revalidate affected files
3. WHEN the target of an `@lsp-source` directive changes, THE system SHALL update the dependency graph accordingly

### Requirement 8: Documentation Update

**User Story:** As a developer reading the codebase documentation, I want AGENTS.md and docs/cross-file.md to accurately describe the path resolution behavior for forward vs backward directives, so that I understand the design correctly.

#### Acceptance Criteria

1. WHEN the `@lsp-source` feature is implemented, THE AGENTS.md file SHALL be updated to clarify that forward directives (`@lsp-source`, `@lsp-run`, `@lsp-include`) use `@lsp-cd` for path resolution
2. THE AGENTS.md file SHALL distinguish between backward directives (which ignore `@lsp-cd`) and forward directives (which use `@lsp-cd`)
3. THE AGENTS.md file SHALL explain the rationale: forward directives are semantically equivalent to `source()` calls and describe runtime execution, while backward directives describe static file relationships from the child's perspective
4. THE docs/cross-file.md file SHALL expand the Forward Directives section to document `@lsp-source` and its synonyms (`@lsp-run`, `@lsp-include`) with full syntax examples
5. THE docs/cross-file.md file SHALL correct the existing note about `@lsp-cd` to clarify that forward directives DO use `@lsp-cd` for path resolution (unlike backward directives which ignore it)
6. THE docs/cross-file.md file SHALL document the `line=N` parameter for forward directives
