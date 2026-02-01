# Requirements Document

## Introduction

This document specifies the requirements for fixing a bug in the cross-file awareness system where backward directive paths are incorrectly resolved using the `@lsp-cd` working directory. According to the project's design (documented in AGENTS.md), backward directives (`@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`) should ALWAYS resolve relative to the file's own directory, ignoring any `@lsp-cd` directive.

The bug is located in the `collect_ambiguous_parent_diagnostics` function in `crates/raven/src/handlers.rs`, which uses `PathContext::from_metadata` (respects `@lsp-cd`) instead of `PathContext::new` (ignores `@lsp-cd`) for backward directive resolution.

## Glossary

- **Backward_Directive**: An LSP directive (`@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`) that declares a parent file relationship from the child file's perspective
- **Forward_Source**: A `source()` or `sys.source()` call that sources another file from the parent file's perspective
- **PathContext**: A struct that holds path resolution context including file path, working directory, and workspace root
- **PathContext_new**: Constructor that creates a PathContext without working directory (for backward directives)
- **PathContext_from_metadata**: Constructor that creates a PathContext with working directory from metadata (for forward sources)
- **Working_Directory**: The directory set by `@lsp-cd` directive that affects `source()` call path resolution
- **Ambiguous_Parent_Diagnostic**: A diagnostic emitted when multiple parent files could match a backward directive

## Requirements

### Requirement 1: Backward Directive Path Resolution

**User Story:** As a developer using cross-file awareness, I want backward directives to resolve paths relative to the file's directory, so that `@lsp-cd` does not incorrectly affect parent file resolution.

#### Acceptance Criteria

1. WHEN resolving backward directive paths in `collect_ambiguous_parent_diagnostics`, THE System SHALL use `PathContext::new` instead of `PathContext::from_metadata`
2. WHEN a file contains both `@lsp-cd` and `@lsp-run-by` directives, THE System SHALL resolve the `@lsp-run-by` path relative to the file's directory, ignoring `@lsp-cd`
3. WHEN a file contains both `@lsp-cd` and `@lsp-sourced-by` directives, THE System SHALL resolve the `@lsp-sourced-by` path relative to the file's directory, ignoring `@lsp-cd`

### Requirement 2: Forward Source Path Resolution Unchanged

**User Story:** As a developer using cross-file awareness, I want `source()` calls to continue respecting `@lsp-cd`, so that runtime path resolution behavior is correctly modeled.

#### Acceptance Criteria

1. WHEN resolving forward source paths (from `source()` calls), THE System SHALL continue using `PathContext::from_metadata` to respect `@lsp-cd`
2. WHEN a file contains `@lsp-cd` and a `source()` call, THE System SHALL resolve the `source()` path relative to the working directory set by `@lsp-cd`

### Requirement 3: Ambiguous Parent Diagnostic Accuracy

**User Story:** As a developer, I want ambiguous parent diagnostics to be accurate, so that I am not shown false positives when `@lsp-cd` is present.

#### Acceptance Criteria

1. WHEN checking for ambiguous parents with `@lsp-cd` present, THE System SHALL correctly identify the parent file based on the file's directory, not the working directory
2. WHEN the same parent file is found via different resolution paths due to `@lsp-cd`, THE System SHALL NOT emit a false "ambiguous parent" diagnostic

### Requirement 4: Consistency with Other Functions

**User Story:** As a maintainer, I want path resolution to be consistent across all diagnostic functions, so that the codebase follows the documented design pattern.

#### Acceptance Criteria

1. THE `collect_ambiguous_parent_diagnostics` function SHALL follow the same path resolution pattern as `collect_missing_file_diagnostics`
2. THE `collect_ambiguous_parent_diagnostics` function SHALL use separate PathContext instances for forward sources and backward directives
