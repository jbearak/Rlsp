# Design: Scope Resolution and Background Indexer Refactor

## Overview

This design describes two related simplifications to the cross-file awareness system:

1. **Consolidate scope resolution** to use only `scope_at_position_with_graph`
2. **Simplify background indexing** by removing priority tiers

## Part 1: Remove scope_at_position_with_backward

### Current State

Two scope resolution entry points exist:

```rust
// PRIMARY - Used by all handlers
pub fn scope_at_position_with_graph<F, G>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    get_metadata: &G,
    graph: &DependencyGraph,
    workspace_root: Option<&Url>,
    max_depth: usize,
    base_exports: &HashSet<String>,
) -> ScopeAtPosition

// LEGACY - Only used in tests
pub fn scope_at_position_with_backward<F, G>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    get_metadata: &G,
    resolve_path: &impl Fn(&str, &Url) -> Option<Url>,
    max_depth: usize,
    parent_call_site: Option<(u32, u32)>,
) -> ScopeAtPosition
```

### Why scope_at_position_with_graph is Superior

1. **Uses DependencyGraph**: The graph already consolidates:
   - AST-detected `source()` calls
   - Directive-declared relationships (`@lsp-sourced-by`, `@lsp-source`)
   - Conflict resolution between directives and AST

2. **Proper edge metadata**: The graph stores:
   - Call site positions (line, column)
   - Local scoping flags (`local=TRUE`)
   - `sys.source` vs `source` distinction
   - Working directory context

3. **Single source of truth**: All handlers use the graph, so tests should too.

### Migration Plan

#### Files to Modify

1. `crates/raven/src/cross_file/scope.rs`:
   - Remove `scope_at_position_with_backward` (lines ~1903-1930)
   - Remove `scope_at_position_with_backward_recursive` (lines ~2562-2800)
   - Update tests that use the backward function

2. `crates/raven/src/cross_file/property_tests.rs`:
   - Remove import of `scope_at_position_with_backward`
   - Migrate property tests to use graph-based approach

#### Test Migration Pattern

**Before:**
```rust
let scope = scope_at_position_with_backward(
    &child_uri, 0, 0, 
    &get_artifacts, &get_metadata, &resolve_path, 
    10, None,
);
```

**After:**
```rust
// Build dependency graph
let mut graph = DependencyGraph::default();
graph.update_file(&parent_uri, &parent_meta, workspace_root.as_ref(), |_| None);
graph.update_file(&child_uri, &child_meta, workspace_root.as_ref(), |u| {
    if u == &parent_uri { Some(parent_content.clone()) } else { None }
});

let scope = scope_at_position_with_graph(
    &child_uri, 0, 0,
    &get_artifacts, &get_metadata, &graph,
    workspace_root.as_ref(), 10, &HashSet::new(),
);
```

## Part 2: Simplify BackgroundIndexer

### Current State

The `BackgroundIndexer` uses a priority queue:

```rust
pub struct IndexTask {
    pub uri: Url,
    pub priority: usize,  // 2 or 3
    pub depth: usize,
    pub submitted_at: Instant,
}
```

Priority ordering:
- Priority 2: Backward directive targets (indexed first)
- Priority 3: Transitive dependencies (indexed after Priority 2)

### Why Priority Tiers Are Unnecessary

1. **Priority 1 is synchronous**: Files directly sourced by open documents are indexed synchronously before diagnostics. This is the critical path.

2. **Priority 2 and 3 are both background**: Both are indexed asynchronously after diagnostics. The ordering between them doesn't significantly impact user experience.

3. **Complexity without benefit**: The priority queue adds code complexity without measurable user benefit.

### Simplified Design

```rust
pub struct IndexTask {
    pub uri: Url,
    pub depth: usize,  // For transitive depth limiting
    pub submitted_at: Instant,
}

impl BackgroundIndexer {
    /// Submit a file for background indexing
    pub fn submit(&self, uri: Url, depth: usize) {
        // Simple FIFO queue, no priority ordering
        let mut queue = self.queue.lock().unwrap();
        
        if queue.iter().any(|task| task.uri == uri) {
            return; // Already queued
        }
        
        if queue.len() >= self.max_queue_size {
            log::warn!("Queue full, dropping task for {}", uri);
            return;
        }
        
        queue.push_back(IndexTask {
            uri,
            depth,
            submitted_at: Instant::now(),
        });
    }
}
```

### Configuration Changes

**Remove:**
- `on_demand_indexing_priority_2_enabled`
- `on_demand_indexing_priority_3_enabled`

**Keep:**
- `on_demand_indexing_enabled`
- `on_demand_indexing_max_transitive_depth`
- `on_demand_indexing_max_queue_size`

### Updated CrossFileConfig

```rust
pub struct CrossFileConfig {
    // ... existing fields ...
    
    // On-demand indexing (simplified)
    pub on_demand_indexing_enabled: bool,
    pub on_demand_indexing_max_transitive_depth: usize,
    pub on_demand_indexing_max_queue_size: usize,
    // REMOVED: on_demand_indexing_priority_2_enabled
    // REMOVED: on_demand_indexing_priority_3_enabled
}
```

## Correctness Properties

### Property 1: Scope Resolution Equivalence
**Statement**: For any file with backward directives, `scope_at_position_with_graph` produces the same symbols as `scope_at_position_with_backward` when the dependency graph is correctly populated.

**Validation**: Migration tests compare results before and after.

### Property 2: Background Indexing Completeness
**Statement**: All files that would have been indexed under the priority system are still indexed under the simplified system, provided the `on_demand_indexing_max_queue_size` is not exceeded (or dropped tasks are re-queued).

**Validation**: Integration tests verify all transitive dependencies are indexed under non-capacity conditions, and verify that queue-full scenarios are handled gracefully.

### Property 3: No Regression in Cross-File Features
**Statement**: All cross-file features (completions, hover, diagnostics, go-to-definition) work identically after the refactor.

**Validation**: Existing integration tests pass.

## Testing Strategy

### Unit Tests
- Verify `scope_at_position_with_graph` handles all cases previously handled by `scope_at_position_with_backward`
- Verify simplified `BackgroundIndexer` queues and processes files correctly

### Property Tests
- Migrate existing property tests to use graph-based scope resolution
- Add property test for queue ordering (FIFO)

### Integration Tests
- Verify cross-file symbol resolution works end-to-end
- Verify backward directives work correctly
- Verify transitive dependencies are indexed

## Migration Checklist

### Phase 1: Remove scope_at_position_with_backward
- [ ] Identify all usages in scope.rs tests
- [ ] Identify all usages in property_tests.rs
- [ ] Create helper function for building test dependency graphs
- [ ] Migrate each test
- [ ] Remove the functions
- [ ] Run all tests

### Phase 2: Simplify BackgroundIndexer
- [ ] Remove priority field from IndexTask
- [ ] Remove priority ordering in submit()
- [ ] Remove priority config options
- [ ] Update config.rs
- [ ] Update backend.rs submit calls
- [ ] Update tests
- [ ] Run all tests

### Phase 3: Update Documentation
- [ ] Update AGENTS.md
- [ ] Update cross-file.md if needed
- [ ] Remove priority-2-3-indexing spec (mark as superseded)

## Risks and Mitigations

### Risk 1: Test Migration Complexity
**Mitigation**: Create a helper function that builds a dependency graph from metadata, reducing boilerplate in tests.

### Risk 2: Subtle Behavioral Differences
**Mitigation**: Run comprehensive integration tests and compare behavior before/after.

### Risk 3: Performance Regression
**Mitigation**: The simplified indexer should be faster (no priority sorting). Monitor performance in integration tests.
