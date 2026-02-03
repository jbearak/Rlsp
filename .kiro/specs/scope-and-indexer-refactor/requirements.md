# Requirements: Scope Resolution and Background Indexer Refactor

## Overview

This document specifies requirements for two related refactors:

1. **Remove `scope_at_position_with_backward`**: Consolidate to use only `scope_at_position_with_graph` which is the superior, graph-based scope resolution system.

2. **Remove Priority Tiers from BackgroundIndexer**: Replace the Priority 2/3 tier system with a simpler, more robust on-demand indexing approach inspired by Sight's architecture.

## Background

### Current State: Dual Scope Resolution

The codebase currently has two scope resolution entry points:

- `scope_at_position_with_graph`: Uses the `DependencyGraph` to resolve parent context. This is the primary function used by all handlers (completions, hover, diagnostics, go-to-definition).

- `scope_at_position_with_backward`: A legacy function that resolves parent context by directly following backward directives without using the dependency graph. Only used in a handful of tests.

The graph-based approach is superior because:
- The `DependencyGraph` consolidates both AST-detected `source()` calls and directive-declared relationships
- It handles directive-vs-AST conflict resolution
- It properly tracks edge metadata (call sites, local scoping, etc.)

### Current State: Priority Tier Indexing

The `BackgroundIndexer` uses a priority queue with three tiers:
- Priority 1: Files directly sourced by open documents (synchronous, before diagnostics)
- Priority 2: Files referenced by backward directives (`@lsp-run-by`, `@lsp-sourced-by`)
- Priority 3: Transitive dependencies (files sourced by Priority 2 files)

This approach has complexity that isn't necessary. Sight's simpler approach:
- Index files on-demand when they're needed for scope resolution
- Use the unified `WorkspaceIndex` and `ContentProvider` for all file access
- Let the dependency graph drive what needs to be indexed

## User Stories

### 1. As a maintainer, I want a single scope resolution system
**Acceptance Criteria:**
- 1.1: All scope resolution uses `scope_at_position_with_graph`
- 1.2: The `scope_at_position_with_backward` function and its recursive helper are removed
- 1.3: All tests that used `scope_at_position_with_backward` are migrated to use `scope_at_position_with_graph`
- 1.4: Property tests in `property_tests.rs` are updated to use the graph-based approach

### 2. As a maintainer, I want simpler background indexing
**Acceptance Criteria:**
- 2.1: The priority tier system (Priority 2/3) is removed from `BackgroundIndexer`
- 2.2: On-demand indexing is triggered when scope resolution needs a file that isn't indexed
- 2.3: The `BackgroundIndexer` becomes a simple async file indexer without priority ordering
- 2.4: Configuration options for priority tiers are removed

### 3. As a developer, I want consistent cross-file behavior
**Acceptance Criteria:**
- 3.1: Cross-file symbol resolution works identically before and after the refactor
- 3.2: Backward directives continue to work correctly
- 3.3: Forward source() calls continue to work correctly
- 3.4: Working directory inheritance continues to work correctly

## Technical Requirements

### 4. Remove scope_at_position_with_backward
**Acceptance Criteria:**
- 4.1: Remove `scope_at_position_with_backward` public function from `scope.rs`
- 4.2: Remove `scope_at_position_with_backward_recursive` private function from `scope.rs`
- 4.3: Remove the import from `property_tests.rs`
- 4.4: Update all tests in `scope.rs` that use `scope_at_position_with_backward`
- 4.5: Update all property tests in `property_tests.rs` that use `scope_at_position_with_backward`

### 5. Simplify BackgroundIndexer
**Acceptance Criteria:**
- 5.1: Remove `priority` field from `IndexTask`
- 5.2: Remove priority-based queue ordering logic
- 5.3: Remove `on_demand_indexing_priority_2_enabled` config option
- 5.4: Remove `on_demand_indexing_priority_3_enabled` config option
- 5.5: Simplify `submit()` to just queue files without priority
- 5.6: Keep depth tracking for transitive indexing limits

### 6. Update AGENTS.md
**Acceptance Criteria:**
- 6.1: Remove references to Priority 2/3 indexing tiers
- 6.2: Remove references to `scope_at_position_with_backward`
- 6.3: Document the simplified indexing approach
- 6.4: Update the "Cross-File Architecture" section

## Non-Goals

- Changing the fundamental scope resolution algorithm
- Changing how the dependency graph works
- Changing how metadata extraction works
- Adding new features

## Success Metrics

- All existing tests pass after migration
- No regression in cross-file symbol resolution
- Reduced code complexity (fewer lines of code)
- Simpler mental model for maintainers

## Dependencies

- Requires existing `DependencyGraph` infrastructure
- Requires existing `WorkspaceIndex` and `ContentProvider` from workspace-index-consolidation

## Migration Strategy

### Phase 1: Remove scope_at_position_with_backward
1. Identify all usages of `scope_at_position_with_backward`
2. Migrate each test to use `scope_at_position_with_graph` with appropriate setup
3. Remove the functions
4. Verify all tests pass

### Phase 2: Simplify BackgroundIndexer
1. Remove priority fields and ordering logic
2. Remove priority-related config options
3. Simplify the submit/process flow
4. Update tests
5. Verify all tests pass

### Phase 3: Update Documentation
1. Update AGENTS.md
2. Update any other documentation referencing the old systems
