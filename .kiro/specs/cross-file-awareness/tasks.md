# Implementation Plan: Cross-File Awareness for Rlsp

## Overview

This implementation plan breaks down the cross-file awareness feature into discrete, actionable tasks. The feature enables Rlsp to understand relationships between R source files through `source()` calls and special comment directives, providing accurate symbol resolution, diagnostics, and navigation across file boundaries.

The implementation follows Rlsp's existing patterns: tree-sitter for parsing, `RwLock` for thread-safe state, and integration with existing `WorldState` and `Document` structures. Per project coding guidelines: use `log::trace!` instead of `log::debug!`, use explicit `return Err(anyhow!(...))` instead of `bail!`, and omit `return` in match expressions.

## Tasks

- [ ] 1. Set up cross-file module structure and core types
  - Create `crates/rlsp/src/cross_file/` directory
  - Create `mod.rs` with module declarations
  - Define `CrossFileMetadata`, `BackwardDirective`, `ForwardSource`, `CallSiteSpec` types
  - Define `CallSitePosition` type with full (line, column) support
  - Define `ForwardSourceKey` for edge deduplication
  - Add UTF-16 column conversion helpers (`byte_offset_to_utf16_column`, `tree_sitter_point_to_lsp_position`)
  - _Requirements: 1.1-1.7, 2.1-2.5, 3.1-3.10, 4.1-4.8_

- [ ] 2. Implement directive parser
  - [ ] 2.1 Create `directive.rs` with `CrossFileExtractor` trait
    - Implement regex-based directive parsing
    - Ensure cross-file call-site resolution (`match=` validation/inference) reads parent content via a File Content Provider (open document > index snapshot > async disk)
    - Support all backward directive synonyms (`@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`)
    - Support all working directory directive synonyms
    - Support forward directives (`@lsp-source`)
    - Support ignore directives (`@lsp-ignore`, `@lsp-ignore-next`)
    - Support global directive syntax rules:
      - optional colon after directive name for all directives
      - optional quoting around paths for path-taking directives
    - Extract `line=` and `match=` parameters from backward directives
    - Convert 1-based `line=` to 0-based internal representation
    - _Requirements: 0a, 1.1-1.8, 2.1-2.5, 3.1-3.10_

  - [ ] 2.2 Write property test for directive parsing
    - **Property 1: Backward Directive Synonym Equivalence**
    - **Validates: Requirements 1.1, 1.2, 1.3**

  - [ ] 2.3 Write property test for working directory directive synonyms
    - **Property 2: Working Directory Directive Synonym Equivalence**
    - **Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5, 3.6**

  - [ ] 2.4 Write property test for directive serialization round-trip
    - **Property 8: Directive Serialization Round-Trip**
    - **Validates: Requirements 14.1, 14.2, 14.3, 14.4**

- [ ] 3. Implement source() call detection
  - [ ] 3.1 Create `source_detect.rs` with `SourceDetector` trait
    - Use tree-sitter to detect `source()` calls with string literal paths
    - Use tree-sitter to detect `sys.source()` calls
    - Extract path from first argument (handle both positional and named `file=`)
    - Extract `local = TRUE/FALSE` parameter
    - Extract `chdir = TRUE/FALSE` parameter
    - Skip calls with non-literal paths (variables, expressions, `paste0()`)
    - Record call site position with full (line, column) using UTF-16 conversion
    - _Requirements: 4.1-4.8_

  - [ ] 3.2 Write property test for source() detection
    - **Property 9: Source Call Detection**
    - **Validates: Requirements 4.1, 4.2, 4.3**

  - [ ] 3.3 Write property test for sys.source() detection
    - **Property 10: Sys.Source Call Detection**
    - **Validates: Requirements 4.4**

  - [ ] 3.4 Write property test for local parameter extraction
    - **Property 11: Local Parameter Extraction**
    - **Validates: Requirements 4.7**

  - [ ] 3.5 Write property test for chdir parameter extraction
    - **Property 12: Chdir Parameter Extraction**
    - **Validates: Requirements 4.8**

- [ ] 4. Implement path resolution
  - [ ] 4.1 Create `path_resolve.rs` with `PathResolver` trait
    - Implement workspace-root-relative path resolution (paths starting with `/`)
    - Implement file-relative path resolution (paths not starting with `/`)
    - Handle `..` navigation correctly
    - Implement working directory inheritance from parent files
    - Implement effective working directory computation
    - Implement `chdir=TRUE` working-directory override during child traversal (restore parent context after returning)
    - _Requirements: 1.6, 1.7, 3.7, 3.8, 3.9, 3.10, 4.8_

  - [ ] 4.2 Write property test for path resolution
    - **Property 13: Path Resolution Correctness**
    - **Validates: Requirements 1.6, 1.7, 3.7, 3.8**

  - [ ] 4.3 Write property test for working directory inheritance
    - **Property 14: Working Directory Inheritance**
    - **Validates: Requirements 3.9, 3.10**

- [ ] 5. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 6. Implement dependency graph
  - [ ] 6.1 Create `dependency.rs` with `DependencyGraph` struct
    - Implement forward-only edge representation (no `EdgeKind` enum)
    - Store edges with full call site position (line, column)
    - Store `local`, `chdir`, `is_sys_source`, `is_directive` flags on edges
    - Implement `update_file()` to process both forward sources and backward directives
    - Implement edge deduplication by canonical key `(to_uri, call_site_position, local, chdir, is_sys_source)`
    - Implement `get_dependencies()` and `get_dependents()` queries
    - Implement `get_transitive_dependents()` with depth limit
    - Implement `remove_file()` for cleanup
    - Use `HashMap` with forward and backward indexes for efficient queries
    - _Requirements: 6.1-6.7_

  - [ ] 6.2 Write property test for dependency graph operations
    - **Property 23: Dependency Graph Add/Remove**
    - **Validates: Requirements 6.1, 6.2**

  - [ ] 6.3 Write property test for transitive dependencies
    - **Property 24: Transitive Dependency Computation**
    - **Validates: Requirements 6.4, 6.5**

  - [ ] 6.4 Write property test for edge deduplication
    - **Property 50: Edge Deduplication**
    - **Validates: Requirements 6.1, 6.2, 12.5**

  - [ ] 6.5 Write property test for directive-vs-AST conflict resolution
    - **Property 58: Directive Overrides AST For Same (from,to)**
    - Generate cases where both an `@lsp-source` directive and an AST-detected `source()` call reference the same resolved target but disagree on call site or flags.
    - Verify the graph contains exactly one semantic edge for the pair and that it uses the directive's semantics.
    - **Validates: Requirements 6.8**

- [ ] 7. Implement scope resolution
  - [ ] 7.1 Create `scope.rs` with `ScopeResolver` trait
    - Define `ScopedSymbol` (including `defined_column`), `ScopeArtifacts`, `ScopeEvent`, `ScopeAtPosition` types
    - Implement `compute_artifacts()` - non-recursive, file-local only
    - Implement v1 R symbol model extraction for exported interface + timeline Def events (functions + top-level assignments only)
    - Build timeline of `ScopeEvent`s (Def, Source, WorkingDirectory)
    - Compute exported interface hash
    - Implement `scope_at_position()` with full (line, column) precision
    - Walk timeline up to requested position
    - Recursively resolve sourced files (bounded by depth + visited set)
    - Apply call-site filtering for backward directives
    - Implement local symbol shadowing (local definitions win)
    - Handle `local=TRUE` semantics (no inheritance)
    - Detect and break circular dependencies
    - _Requirements: 5.1-5.10_

  - [ ] 7.2 Write property test for local symbol precedence
    - **Property 4: Local Symbol Precedence**
    - **Validates: Requirements 5.4**

  - [ ] 7.3 Write property test for call-site filtering
    - **Property 19: Call-Site Filtering**
    - **Validates: Requirements 5.3, 5.5**

  - [ ] 7.4 Write property test for position-aware symbol availability
    - **Property 40: Position-Aware Symbol Availability**
    - **Validates: Requirements 5.3**

  - [ ] 7.5 Write property test for full position precision
    - **Property 51: Full Position Precision**
    - **Validates: Requirements 5.3, 7.1, 7.4**

  - [ ] 7.6 Write property test for circular dependency detection
    - **Property 7: Circular Dependency Detection**
    - **Validates: Requirements 5.7, 10.6_

  - [ ] 7.7 Write property test for local=TRUE semantics
    - **Property 52: Local=TRUE No Inheritance**
    - **Validates: Requirements 4.7**

  - [ ] 7.8 Write property test for v1 R symbol model
    - Generate files with a mix of:
      - top-level `name <- function(...)` / `name <- <expr>` definitions
      - `name <<- <expr>` and `name <<- function(...)`
      - `assign("name", <expr>)` (string literal)
      - `assign(dynamic_name, <expr>)` and `assign(paste0(...), <expr>)` (non-literal)
      - `set("name", <expr>)` where `set` is and is not recognized
    - Verify only v1-recognized (statically name-resolvable) constructs contribute to exported interface and can suppress undefined-variable diagnostics.
    - Verify non-literal `assign()` does NOT suppress diagnostics.
    - **Validates: Requirements 17.1-17.7**

- [ ] 8. Implement configuration
  - [ ] 8.1 Create `config.rs` with `CrossFileConfig` struct
    - Define all configuration fields with defaults from Requirement 11
    - Implement `CallSiteDefault` enum (End, Start)
    - Implement `Default` trait with correct default values
    - Implement `on_configuration_changed()` handler
    - Invalidate caches when scope-affecting settings change
    - Schedule revalidation for all open documents on config change
    - _Requirements: 11.1-11.11_

  - [ ] 8.2 Write property test for configuration change re-resolution
    - **Property 34: Configuration Change Re-resolution**
    - **Validates: Requirements 11.11**

- [ ] 9. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 10. Implement caching with interior mutability
  - [ ] 10.1 Create `cache.rs` with cache structures
    - Implement `ScopeFingerprint` with all required hash components
    - Implement `MetadataCache` with interior mutability (`RwLock<HashMap>`)
    - Implement `ArtifactsCache` with interior mutability
    - Implement `ParentSelectionCache` with interior mutability
    - Implement a bounded `ScopeQueryCache` for `scope_at_position` (keyed by `(uri, position bucket, ScopeFingerprint)`)
    - Implement `get_if_fresh()`, `insert()`, `invalidate()` methods
    - Ensure minimal lock hold-time (compute outside lock, insert under lock)
    - _Requirements: 12.1-12.8_

  - [ ] 10.2 Write property test for interface hash optimization
    - **Property 39: Interface Hash Optimization**
    - **Validates: Requirements 12.8**

- [ ] 11. Implement parent resolution with stability
  - [ ] 11.1 Create `parent_resolve.rs` with parent resolution logic
    - Implement `ParentResolution` enum (Single, Ambiguous, None)
    - Implement `resolve_parent()` with strict precedence order
    - Implement call-site resolution ladder (explicit line → match → reverse deps → inference → default)
    - Implement `resolve_multiple_source_calls()` for earliest call site
    - Compute `ParentCacheKey` with metadata_fingerprint and reverse_edges_hash
    - Use `ParentSelectionCache` for stability during graph convergence
    - Generate ambiguous parent diagnostics when multiple parents found
    - _Requirements: 5.5, 5.6, 5.9, 5.10_

  - [ ] 11.2 Write property test for multiple source calls
    - **Property 37: Multiple Source Calls - Earliest Call Site**
    - **Validates: Requirements 5.9**

  - [ ] 11.3 Write property test for ambiguous parent determinism
    - **Property 38: Ambiguous Parent Determinism**
    - **Validates: Requirements 5.10**

  - [ ] 11.4 Write property test for parent selection stability
    - **Property 57: Parent Selection Stability**
    - **Validates: Requirements 5.10**

- [ ] 12. Implement real-time update system
  - [ ] 12.1 Create `revalidation.rs` with revalidation logic
    - Implement `CrossFileRevalidationState` with pending task tracking
    - Implement `CrossFileDiagnosticsGate` for monotonic publishing
    - Implement `CrossFileActivityState` for prioritization
    - Implement `revalidate_after_change_locked()` function
    - Extract metadata, update graph, compute interface/edge changes
    - Invalidate dependent caches selectively
    - Return list of affected open documents
    - Ensure no blocking disk I/O is performed while holding the Tokio `WorldState` lock (use async reads or spawn_blocking + re-check freshness)
    - Implement `schedule_diagnostics_debounced()` function
    - Prioritize: trigger first, then active, then visible, then recent
    - Cap revalidations per trigger (configurable)
    - Spawn async tasks with cancellation tokens
    - Implement freshness guards (check before compute AND before publish)
    - Implement monotonic publishing gate
    - Implement force republish for dependency-triggered changes
    - _Requirements: 0.1-0.10, 15.1-15.5_

  - [ ] 12.2 Write property test for diagnostics fanout
    - **Property 35: Diagnostics Fanout to Open Files**
    - **Validates: Requirements 0.4, 13.4**

  - [ ] 12.3 Write property test for debounce cancellation
    - **Property 36: Debounce Cancellation**
    - **Validates: Requirements 0.5**

  - [ ] 12.4 Write property test for freshness guard
    - **Property 41: Freshness Guard Prevents Stale Diagnostics**
    - Verify freshness guard uses both version (when present) AND a content-hash/revision snapshot.
    - **Validates: Requirements 0.6**

  - [ ] 12.5 Write property test for monotonic publishing
    - **Property 47: Monotonic Diagnostic Publishing**
    - **Validates: Requirements 0.7**

  - [ ] 12.6 Write property test for force republish
    - **Property 48: Force Republish on Dependency Change**
    - **Validates: Requirements 0.8**

  - [ ] 12.7 Write property test for revalidation prioritization
    - **Property 42: Revalidation Prioritization**
    - **Validates: Requirements 0.9**

  - [ ] 12.8 Write property test for revalidation cap enforcement
    - **Property 43: Revalidation Cap Enforcement**
    - **Validates: Requirements 0.9, 0.10**

- [ ] 13. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 14. Implement workspace watching and indexing
  - [ ] 14.1 Create `workspace_index.rs` with workspace index
    - Implement `CrossFileWorkspaceIndex` struct
    - Store per-file metadata, artifacts, and file snapshots
    - Implement monotonic version counter
    - Implement `update_from_disk()` with open-docs-authoritative check
    - Implement `get_metadata()`, `get_artifacts()` with open-docs preference
    - Implement debounced index updates
    - _Requirements: 13.1-13.6_

  - [ ] 14.2 Create `file_cache.rs` with disk file cache
    - Implement `CrossFileFileCache` struct
    - Implement `FileSnapshot` with mtime, size, content_hash
    - Read file contents from disk on-demand
    - Cache by absolute path and file metadata
    - Invalidate on file change events
    - _Requirements: 13.1-13.3_

  - [ ] 14.3 Implement file watcher handler in backend
    - Implement `did_change_watched_files()` handler in `backend.rs`
    - Handle CREATED, CHANGED, DELETED events
    - Invalidate caches for changed files
    - Update workspace index (respecting open-docs-authoritative)
    - Schedule diagnostics fanout for affected open documents
    - _Requirements: 13.1-13.4_

  - [ ] 14.4 Write property test for workspace index version monotonicity
    - **Property 44: Workspace Index Version Monotonicity**
    - **Validates: Requirements 13.5**

  - [ ] 14.5 Write property test for watched file cache invalidation
    - **Property 45: Watched File Cache Invalidation**
    - **Validates: Requirements 13.2**

- [ ] 15. Extend WorldState with cross-file support
  - [ ] 15.1 Update `state.rs` to add cross-file fields
    - Add `version: Option<i32>` field to `Document` struct
    - Add `cross_file_config: CrossFileConfig` to `WorldState`
    - Add `cross_file_meta: MetadataCache` to `WorldState`
    - Add `cross_file_graph: DependencyGraph` to `WorldState`
    - Add `cross_file_cache: ArtifactsCache` to `WorldState`
    - Add `cross_file_file_cache: CrossFileFileCache` to `WorldState`
    - Add `cross_file_revalidation: CrossFileRevalidationState` to `WorldState`
    - Add `cross_file_activity: CrossFileActivityState` to `WorldState`
    - Add `cross_file_diagnostics_gate: CrossFileDiagnosticsGate` to `WorldState`
    - Add `cross_file_workspace_index: CrossFileWorkspaceIndex` to `WorldState`
    - Add `cross_file_parent_cache: ParentSelectionCache` to `WorldState`
    - Initialize all fields in `WorldState::new()`
    - _Requirements: 0.1-0.10, 6.1-6.7, 12.1-12.8, 13.1-13.6_

- [ ] 16. Integrate cross-file into LSP handlers
  - [ ] 16.1 Update completion handler
    - Query `ScopeResolver::scope_at_position()` for request position
    - Include symbols from resolved scope chain
    - Add source file path to completion detail for cross-file symbols
    - Prefer local definitions over inherited symbols
    - _Requirements: 7.1-7.4_

  - [ ] 16.2 Write property test for completion cross-file symbols
    - **Property 27: Cross-File Symbol Inclusion in Completions**
    - **Validates: Requirements 7.1**

  - [ ] 16.3 Write property test for completion source attribution
    - **Property 28: Completion Source Attribution**
    - **Validates: Requirements 7.2**

  - [ ] 16.4 Update hover handler
    - Query `ScopeResolver::scope_at_position()` for hover position
    - Display source file path for cross-file symbols
    - Display function signature if applicable
    - Show effective definition when multiple definitions exist
    - _Requirements: 8.1-8.3_

  - [ ] 16.5 Write property test for cross-file hover information
    - **Property 29: Cross-File Hover Information**
    - **Validates: Requirements 8.1, 8.2**

  - [ ] 16.6 Update definition handler
    - Query `ScopeResolver::scope_at_position()` for definition request
    - Navigate to definition location in sourced file
    - Handle shadowing (navigate to effective definition)
    - _Requirements: 9.1-9.3_

  - [ ] 16.7 Write property test for cross-file go-to-definition
    - **Property 30: Cross-File Go-to-Definition**
    - **Validates: Requirements 9.1**

  - [ ] 16.8 Update diagnostics handler
    - Query `ScopeResolver::scope_at_position()` for each symbol usage
    - Suppress undefined variable diagnostics for symbols in scope chain
    - Check `CrossFileMetadata::ignored_lines` for suppression
    - Respect `config.undefined_variables_enabled` flag
    - Emit missing file diagnostics for non-existent sourced files
    - Emit out-of-scope diagnostics for symbols used before source() call
    - Emit circular dependency diagnostics
    - Emit ambiguous parent diagnostics
    - Use configurable severity levels
    - _Requirements: 10.1-10.6_

  - [ ] 16.9 Write property test for undefined variable suppression
    - **Property 31: Cross-File Undefined Variable Suppression**
    - **Validates: Requirements 10.1**

  - [ ] 16.10 Write property test for out-of-scope warnings
    - **Property 32: Out-of-Scope Symbol Warning**
    - **Validates: Requirements 10.3**

  - [ ] 16.11 Write property test for undefined variables configuration
    - **Property 33: Undefined Variables Configuration**
    - **Validates: Requirements 11.9, 11.10**

- [ ] 17. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 18. Update backend.rs with cross-file integration
  - [ ] 18.1 Update did_open handler
    - Pass `text_document.version` to `Document::new()`
    - Extract cross-file metadata and update graph
    - Record document as recently opened in activity state
    - Schedule initial diagnostics with cross-file awareness
    - _Requirements: 0.1, 0.2_

  - [ ] 18.2 Update did_change handler
    - Update `doc.version` from `text_document.version`
    - Call `revalidate_after_change_locked()` to get affected files
    - Call `schedule_diagnostics_debounced()` for affected files
    - Record document as recently changed in activity state
    - _Requirements: 0.1, 0.2, 0.3_

  - [ ] 18.3 Update did_close handler
    - Call `CrossFileDiagnosticsGate::clear()` for closed URI
    - Cancel pending revalidation for closed URI
    - Remove from activity tracking
    - Do NOT remove from dependency graph or metadata cache
    - _Requirements: 0.7, 0.8_

  - [ ] 18.4 Write property test for diagnostics gate cleanup on close
    - **Property 54: Diagnostics Gate Cleanup on Close**
    - **Validates: Requirements 0.7, 0.8**

  - [ ] 18.5 Implement did_change_watched_files handler
    - Handle CREATED, CHANGED, DELETED events
    - Invalidate disk-backed caches for changed files
    - Update workspace index (respecting open-docs-authoritative)
    - Update dependency graph for changed files
    - Schedule diagnostics fanout for affected open documents
    - _Requirements: 13.1-13.4_

  - [ ] 18.6 Implement did_change_configuration handler
    - Parse new cross-file configuration
    - Call `on_configuration_changed()` to invalidate caches
    - Schedule revalidation for all open documents
    - _Requirements: 11.11_

  - [ ] 18.7 Implement custom notification handler for client activity signals
    - Define `ActiveDocumentsChangedParams` struct
    - Implement `handle_active_documents_changed()` handler
    - Update `CrossFileActivityState` with active/visible URIs
    - Log activity updates at trace level
    - _Requirements: 15.1-15.5_

  - [ ] 18.8 Write property test for client activity signal processing
    - **Property 49: Client Activity Signal Processing**
    - **Validates: Requirements 15.4, 15.5**

- [ ] 19. Update VS Code extension for client activity signals
  - [ ] 19.1 Update extension.ts to send activity notifications
    - Register `onDidChangeActiveTextEditor` listener
    - Register `onDidChangeVisibleTextEditors` listener
    - Send `rlsp/activeDocumentsChanged` notification with active/visible URIs
    - Include timestamp for ordering
    - _Requirements: 15.1, 15.2, 15.3_

- [ ] 20. Update documentation
  - [ ] 20.1 Update README.md with cross-file awareness section
    - Document all LSP directives with syntax and examples
    - Document cross-file behavior (source() detection, scope resolution, position-awareness)
    - Document all configuration options with descriptions and defaults
    - Include practical usage examples (basic multi-file, backward directives, forward directives, working directory, circular dependencies)
    - _Requirements: 16.1-16.4_

  - [ ] 20.2 Update AGENTS.md with cross-file architecture
    - Document dependency graph structure and management
    - Document scope resolution algorithm overview
    - Document caching and invalidation strategy
    - Document real-time update mechanism
    - Document thread-safety considerations
    - Document implementation patterns (adding directives, extending scope resolution, adding diagnostics, testing strategies)
    - _Requirements: 16.5-16.7_

- [ ] 21. Final integration testing
  - [ ] 21.1 Create multi-file workspace integration tests
    - Test completions include cross-file symbols
    - Test hover shows source file information
    - Test go-to-definition navigates to sourced files
    - Test diagnostics account for source chains
    - Test real-time updates across multiple files
    - Test workspace watching with disk changes
    - Test configuration changes trigger re-resolution

  - [ ] 21.2 Test concurrent editing scenarios
    - Simulate rapid edits across multiple files
    - Verify debouncing and cancellation work correctly
    - Verify freshness guards prevent stale diagnostics
    - Verify monotonic publishing is enforced
    - Verify force republish works for dependency changes

  - [ ] 21.3 Test edge cases and error handling
    - Test missing file diagnostics
    - Test circular dependency detection
    - Test ambiguous parent warnings
    - Test out-of-scope symbol warnings
    - Test directive suppression (`@lsp-ignore`, `@lsp-ignore-next`)
    - Test maximum depth limits
    - Test Unicode paths and special characters

- [ ] 22. Final checkpoint - Ensure all tests pass
  - Run full test suite including unit tests, property tests, and integration tests
  - Verify all requirements are covered
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties (minimum 100 iterations each)
- Unit tests validate specific examples and edge cases
- Integration tests validate end-to-end LSP behavior
- The implementation follows Rlsp coding style: no `bail!`, use `log::trace!`, explicit error returns
- Thread-safety is critical: use `RwLock` for shared state, interior mutability for caches
- Real-time updates require careful handling of document versions and monotonic publishing
- Open documents are always authoritative over disk-backed caches
