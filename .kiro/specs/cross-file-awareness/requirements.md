# Requirements: Cross-File Awareness

## Overview

This document specifies requirements for adding cross-file awareness to Rlsp, a static R Language Server written in Rust. Cross-file awareness enables the LSP to understand relationships between R source files through `source()` calls and special comment directives, providing accurate symbol resolution, diagnostics, and navigation across file boundaries.

The feature is inspired by the Sight LSP for Stata and adapted for R's specific patterns including `source()`, `sys.source()`, working directory conventions, and R project structures.

## Goals

- Enable the LSP to track dependencies between R files via `source()` calls and comment directives
- Provide accurate symbol resolution across file boundaries for completions, hover, and go-to-definition
- Suppress false-positive "undefined variable" diagnostics for symbols defined in sourced files
- Support real-time updates when any file in a source chain is modified
- Maintain performance through intelligent caching and lazy evaluation

## Non-Goals

- Runtime execution or evaluation of R code
- Dynamic path resolution (variables, expressions, `paste0()` in source paths)
- Package-level dependency management (handled separately by existing workspace indexing)
- Support for non-standard sourcing mechanisms beyond `source()` and `sys.source()`

## Glossary

- **Rlsp**: The R Language Server Protocol implementation being extended
- **Source_Chain**: The ordered sequence of files that contribute symbols to a given file's scope
- **Directive**: A special comment annotation (e.g., `# @lsp-sourced-by`) that provides cross-file metadata to the LSP
- **Backward_Directive**: A directive declaring that the current file is sourced by another file (child declares parent)
- **Forward_Directive**: A directive declaring that the current file sources another file (parent declares child)
- **Call_Site**: The specific location (line number) in a parent file where a source() call occurs
- **Working_Directory**: The directory used for resolving relative paths in source() calls
- **Scope**: The set of symbols (functions, variables) available at a given point in a file
- **Dependency_Graph**: A directed graph representing source relationships between files
- **Symbol_Table**: A mapping of symbol names to their definitions and metadata

## Actors

- **R Developer**: The primary user who writes R code across multiple files and expects accurate LSP features
- **LSP Client**: The editor (VS Code, etc.) that communicates with Rlsp via the Language Server Protocol

## Requirements

### Requirement 0: Real-Time Cross-File Updates (Core)

**User Story:** As an R developer, I want diagnostics, completions, hover, and definition results to update in all affected open files when I edit any file in the chain, so that cross-file analysis stays correct while I edit multiple files concurrently.

#### Acceptance Criteria

1. WHEN a document is opened or changed, THE LSP SHALL update cross-file metadata for that document (directives, detected source calls, and effective working directory).
2. WHEN cross-file metadata for a document changes, THE Dependency_Graph SHALL be updated for that document.
3. WHEN a document’s exported symbol interface changes OR its dependency edges change, THE LSP SHALL invalidate cross-file scope caches for all transitive dependents.
4. WHEN a file is invalidated and is currently open, THE LSP SHALL recompute and publish diagnostics for that file without requiring the user to edit that file.
5. IF multiple changes occur rapidly across files, THEN the LSP SHALL debounce revalidation and SHALL cancel outdated pending revalidations to avoid publishing stale diagnostics.
6. BEFORE publishing diagnostics from any debounced/background task, THE LSP SHALL verify a freshness guard (document version and/or content hash) and SHALL NOT publish if the task is stale.
7. THE LSP SHALL enforce monotonic diagnostic publishing per URI (never publish diagnostics for an older document version than what has already been published).
8. WHEN dependency-driven scope/diagnostic inputs change for an open document without changing its text document version, THE LSP SHALL provide a "force republish" mechanism so updated diagnostics can still be published.
9. WHEN invalidation affects many open documents, THE LSP SHALL prioritize revalidation and SHALL cap the number of revalidations scheduled per trigger (configurable).
   - The trigger document (the one that changed) SHALL be prioritized first.
   - If the client provides active/visible document hints (see Requirement 15), the server SHOULD prioritize: active > visible > other open.
   - Otherwise, the server SHOULD fall back to most-recently-changed/opened ordering.
10. WHEN the cap is exceeded, THE LSP SHALL log/trace that additional open documents were skipped for that trigger (best-effort). Skipped documents SHALL be revalidated on-demand when they become active/visible (or next time they change).

### Requirement 1: Backward Directive Parsing

**User Story:** As an R developer, I want to annotate files that are sourced by other files, so that the LSP understands the execution context without parsing the parent file.

#### Acceptance Criteria

1. WHEN a file contains `# @lsp-sourced-by <path>`, THE Directive_Parser SHALL extract the path and associate it with the current file
2. WHEN a file contains `# @lsp-run-by <path>`, THE Directive_Parser SHALL treat it equivalently to `@lsp-sourced-by`
3. WHEN a file contains `# @lsp-included-by <path>`, THE Directive_Parser SHALL treat it equivalently to `@lsp-sourced-by`
4. WHEN a backward directive includes `line=<number>`, THE Directive_Parser SHALL record the call site line number.
   - The directive syntax SHALL interpret `line=` as a 1-based line number for user ergonomics.
   - Internally, the LSP SHALL convert to 0-based lines to match LSP positions.
5. WHEN a backward directive includes `match="<pattern>"`, THE Directive_Parser SHALL record the pattern for call site identification
6. WHEN a directive path is relative, THE Path_Resolver SHALL resolve it relative to the current file's directory
7. WHEN a directive path uses `..`, THE Path_Resolver SHALL correctly navigate parent directories
8. IF a backward directive references a non-existent file, THEN THE Diagnostic_Engine SHALL emit a warning diagnostic

### Requirement 2: Forward Directive Parsing

**User Story:** As an R developer, I want to explicitly declare source() calls for the LSP when automatic detection fails, so that I get accurate completions even with dynamic paths.

#### Acceptance Criteria

1. WHEN a file contains `# @lsp-source <path>`, THE Directive_Parser SHALL treat it as an explicit source() declaration at that line
2. WHEN a file contains `# @lsp-ignore`, THE Diagnostic_Engine SHALL suppress diagnostics for that line
3. WHEN a file contains `# @lsp-ignore-next`, THE Diagnostic_Engine SHALL suppress diagnostics for the following line
4. WHEN multiple `@lsp-source` directives exist, THE Directive_Parser SHALL process them in document order
5. IF a forward directive references a non-existent file, THEN THE Diagnostic_Engine SHALL emit a warning diagnostic

### Requirement 3: Working Directory Directives

**User Story:** As an R developer, I want to specify the working directory context for a file, so that relative paths in source() calls resolve correctly.

#### Acceptance Criteria

1. WHEN a file contains `# @lsp-working-directory <path>`, THE Path_Resolver SHALL use that path as the base for relative path resolution
2. WHEN a file contains `# @lsp-wd <path>`, THE Path_Resolver SHALL treat it equivalently to `@lsp-working-directory`
3. WHEN a file contains `# @lsp-cd <path>`, THE Path_Resolver SHALL treat it equivalently to `@lsp-working-directory`
4. WHEN a file contains `# @lsp-current-directory <path>`, THE Path_Resolver SHALL treat it equivalently to `@lsp-working-directory`
5. WHEN no working directory directive exists, THE Path_Resolver SHALL inherit the working directory from the parent file in the source chain
6. WHEN no working directory is inherited and no directive exists, THE Path_Resolver SHALL use the file's own directory as the working directory
7. WHEN a working directory path is relative, THE Path_Resolver SHALL resolve it relative to the file's directory

### Requirement 4: Automatic source() Detection

**User Story:** As an R developer, I want the LSP to automatically detect source() calls in my code, so that I don't need to manually annotate every file relationship.

#### Acceptance Criteria

1. WHEN a file contains `source("path.R")`, THE Source_Detector SHALL extract the path and record the call site
2. WHEN a file contains `source('path.R')`, THE Source_Detector SHALL handle single-quoted strings
3. WHEN a file contains `source(file = "path.R")`, THE Source_Detector SHALL handle named arguments
4. WHEN a file contains `sys.source("path.R", envir = ...)`, THE Source_Detector SHALL extract the path
5. WHEN a source() call uses a variable or expression for the path, THE Source_Detector SHALL skip that call and not emit an error
6. WHEN a source() call uses `paste0()` or string concatenation, THE Source_Detector SHALL skip that call
7. WHEN a source() call includes `local = TRUE`, THE Source_Detector SHALL record this for scope resolution
8. WHEN a source() call includes `chdir = TRUE`, THE Source_Detector SHALL update the working directory context for that sourced file

### Requirement 5: Scope Resolution

**Note:** Scope resolution is *position-aware* (line/character). A file does not have a single global “resolved scope”; instead, the available symbols depend on where the user is in the file.

**User Story:** As an R developer, I want symbols from sourced files to be available for completion and diagnostics, so that I can work with multi-file R projects effectively.

#### Acceptance Criteria

1. WHEN resolving scope for a file at a given position, THE Scope_Resolver SHALL first process backward directives to establish parent context (subject to call-site filtering).
2. WHEN resolving scope for a file at a given position, THE Scope_Resolver SHALL then process forward source() calls in document order.
3. WHEN a symbol is defined in a sourced file, THE Scope_Resolver SHALL make it available only for positions strictly after the source() call site.
4. WHEN a symbol is defined in the current file, THE Scope_Resolver SHALL give it precedence over inherited symbols (shadowing).
5. WHEN a backward directive call site line number is specified, THE Scope_Resolver SHALL only include symbols defined before that call site line in the parent.
6. WHEN a backward directive call site is not specified, THE Scope_Resolver SHALL attempt to identify the call site using:
   - reverse dependency edges (from forward resolution)
   - text inference in the parent (static string-literal source() only)
   - otherwise falling back to the configured default (end-of-file or start-of-file).
7. WHEN circular source dependencies exist, THE Scope_Resolver SHALL detect and break the cycle with a diagnostic.
8. WHEN the source chain exceeds the maximum depth, THE Scope_Resolver SHALL stop traversal and emit a diagnostic.
9. WHEN a file is sourced multiple times at different call sites, THE Scope_Resolver SHALL apply the earliest call site for “introduced symbols” semantics unless the user disambiguates using `line=` or `match=`.
10. WHEN a file has multiple possible parents (multiple backward directives or multiple callers), THE Scope_Resolver SHALL be deterministic (document the precedence), and SHOULD emit an ambiguity diagnostic suggesting `line=`/`match=`.

### Requirement 6: Dependency Graph Management

**Note:** The graph is used for both resolution *and* for invalidation/fanout of diagnostics updates across open documents.

**User Story:** As an R developer, I want the LSP to efficiently track file dependencies, so that changes propagate correctly without excessive recomputation.

#### Acceptance Criteria

1. WHEN a file is opened or changed, THE Dependency_Graph SHALL update edges for that file
2. WHEN a file's directives change, THE Dependency_Graph SHALL invalidate dependent scope caches
3. WHEN a file is deleted, THE Dependency_Graph SHALL remove all edges involving that file
4. WHEN querying dependents of a file, THE Dependency_Graph SHALL return all files that source it directly or indirectly
5. WHEN querying dependencies of a file, THE Dependency_Graph SHALL return all files it sources directly or indirectly
6. THE Dependency_Graph SHALL support concurrent read access from multiple LSP handlers
7. THE Dependency_Graph SHALL serialize write access to prevent data races

### Requirement 7: Enhanced Completions

**User Story:** As an R developer, I want completions to include symbols from my source chain, so that I can use functions defined in other files.

#### Acceptance Criteria

1. WHEN providing completions, THE Completion_Handler SHALL include symbols from the resolved scope chain
2. WHEN a completion item comes from a sourced file, THE Completion_Handler SHALL indicate the source file in the detail
3. WHEN multiple definitions exist for a symbol, THE Completion_Handler SHALL prefer the most local definition
4. WHEN completing after a source() call, THE Completion_Handler SHALL include symbols from the newly sourced file

### Requirement 8: Enhanced Hover Information

**User Story:** As an R developer, I want hover information to show where a symbol was defined, so that I can navigate multi-file projects.

#### Acceptance Criteria

1. WHEN hovering over a symbol from a sourced file, THE Hover_Handler SHALL display the source file path
2. WHEN hovering over a symbol from a sourced file, THE Hover_Handler SHALL display the function signature if applicable
3. WHEN a symbol has multiple definitions in the scope chain, THE Hover_Handler SHALL show the effective definition

### Requirement 9: Enhanced Go-to-Definition

**User Story:** As an R developer, I want go-to-definition to navigate to symbols in sourced files, so that I can explore my codebase.

#### Acceptance Criteria

1. WHEN invoking go-to-definition on a symbol from a sourced file, THE Definition_Handler SHALL navigate to that file and location
2. WHEN a symbol is defined in multiple files in the scope chain, THE Definition_Handler SHALL navigate to the effective definition
3. WHEN a symbol is shadowed by a local definition, THE Definition_Handler SHALL navigate to the local definition

### Requirement 10: Enhanced Diagnostics

**User Story:** As an R developer, I want diagnostics to account for symbols from sourced files, so that I don't get false positive "undefined variable" warnings.

#### Acceptance Criteria

1. WHEN checking for undefined variables, THE Diagnostic_Engine SHALL consider symbols from the resolved scope chain
2. WHEN a sourced file is missing, THE Diagnostic_Engine SHALL emit a warning at the source() call or directive
3. WHEN a symbol is used before its source() call site, THE Diagnostic_Engine SHALL emit an "out of scope" warning
4. WHEN `@lsp-ignore` is present, THE Diagnostic_Engine SHALL suppress diagnostics on that line
5. WHEN `@lsp-ignore-next` is present, THE Diagnostic_Engine SHALL suppress diagnostics on the next line
6. WHEN a circular dependency is detected, THE Diagnostic_Engine SHALL emit an error diagnostic

### Requirement 11: Configuration Options

**User Story:** As an R developer, I want to configure cross-file behavior, so that I can tune it for my project's needs.

#### Acceptance Criteria

1. THE Configuration SHALL support `crossFile.maxBackwardDepth` with default value 10
2. THE Configuration SHALL support `crossFile.maxForwardDepth` with default value 10
3. THE Configuration SHALL support `crossFile.maxChainDepth` with default value 20
4. THE Configuration SHALL support `crossFile.assumeCallSite` with values "end" or "start", defaulting to "end"
5. THE Configuration SHALL support `crossFile.indexWorkspace` boolean, defaulting to true
6. THE Configuration SHALL support `crossFile.maxRevalidationsPerTrigger` integer, defaulting to 10
7. THE Configuration SHALL support `crossFile.revalidationDebounceMs` integer, defaulting to 200
8. THE Configuration SHALL support severity settings for cross-file diagnostics
9. THE Configuration SHALL support `diagnostics.undefinedVariables` boolean, defaulting to true, to enable or disable undefined variable diagnostics entirely
10. WHEN `diagnostics.undefinedVariables` is false, THE Diagnostic_Engine SHALL not emit any undefined variable warnings
11. WHEN configuration changes, THE LSP SHALL re-resolve scope chains for open documents

### Requirement 12: Caching and Performance

**Note:** Cached results must be versioned/fingerprinted so concurrent edits across multiple files cannot produce stale cross-file scopes or diagnostics.

**User Story:** As an R developer, I want the LSP to respond quickly even in large projects, so that my editing experience remains smooth.

#### Acceptance Criteria

1. THE Cache SHALL store parsed cross-file metadata per file.
2. THE Cache SHALL store position-aware scope computation artifacts per file (e.g., exported interface + per-file timeline), not just a single flattened map.
3. EACH cached entry SHALL be associated with a stable fingerprint/version that includes:
   - the file’s own document version/content hash
   - the effective dependency edge set and call-site metadata used (including semantics-bearing edge fields like `local` and `chdir`)
   - fingerprints of any upstream “exported interfaces” / upstream interfaces used
   - the workspace index version (directly or indirectly) so cached results cannot outlive workspace index changes
4. WHEN a file changes, THE Cache SHALL invalidate only affected entries.
5. WHEN a dependency edge set changes (add/remove/modify call sites), THE Cache SHALL invalidate scope caches of all transitive dependents.
6. THE Cache SHALL use lazy evaluation to avoid computing unused scopes.
7. The cache design SHALL support concurrent reads and serialized writes without deadlocks.
8. The LSP SHOULD use an interface-hash optimization: IF a file changes but its exported interface hash remains identical and its edge set remains identical, THEN dependent invalidation SHOULD be skipped.

### Requirement 13: Workspace Watching + Indexing (Required)

**User Story:** As an R developer, I want cross-file scope and diagnostics to remain correct even when related files change on disk (including when those files are not open), so that the LSP does not depend on me opening every file to keep analysis fresh.

#### Acceptance Criteria

1. THE LSP SHALL register file watchers for relevant R files (at minimum `**/*.R` and `**/*.r`) so that changes are observed via `workspace/didChangeWatchedFiles`.
2. WHEN a watched file is created or changed, THE LSP SHALL invalidate any disk-backed caches for that file and SHALL schedule a debounced workspace index update for that file.
   - IF the file is currently open in the editor (i.e., present in the in-memory Document store), the in-memory contents MUST remain authoritative and the server MUST NOT overwrite in-memory metadata/artifacts with disk-derived results for that file.
3. WHEN a watched file is deleted, THE Dependency_Graph SHALL remove all edges involving that file and THE LSP SHALL invalidate cross-file scope caches for open dependents.
4. WHEN a watched file change affects the dependency graph (edges added/removed/modified) OR a watched file’s exported interface changes, THE LSP SHALL update diagnostics for affected open files without requiring the user to edit those files.
5. The workspace index SHALL expose a monotonically increasing version counter that increments whenever it changes.
6. The cross-file cache fingerprinting SHALL incorporate the workspace index version (directly or indirectly) so cached results cannot outlive workspace index changes.

### Requirement 14: Directive Serialization

**User Story:** As a developer, I want directive metadata to be serializable, so that it can be stored and transmitted efficiently.

#### Acceptance Criteria

1. THE Directive_Parser SHALL produce a structured representation of all directives in a file
2. THE Directive representation SHALL be serializable to JSON for debugging and testing
3. THE Directive_Parser SHALL produce equivalent output when parsing the same file content (deterministic)
4. FOR ALL valid directive strings, parsing then serializing then parsing again SHALL produce equivalent directive structures (round-trip property)

### Requirement 15: Client Activity Signals (VS Code)

**User Story:** As an R developer, I want cross-file revalidation to prioritize the active/visible editors, so that diagnostics update first where I’m looking.

#### Acceptance Criteria

1. The VS Code client extension SHALL send a custom LSP notification when the active editor changes.
2. The VS Code client extension SHOULD send a custom LSP notification when the set of visible text editors changes.
3. The notification payload SHALL include:
   - `activeUri`: the currently active document URI (or null if none)
   - `visibleUris`: the set/list of currently visible document URIs
   - `timestampMs`: client timestamp for ordering
4. When the server receives these notifications, it SHALL update an in-memory activity model used to prioritize cross-file revalidations (Requirement 0.7).
5. If the client does not support these notifications, the server MUST fall back to trigger-first + most-recently-changed ordering.
