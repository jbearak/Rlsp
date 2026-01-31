# Design Document: Q Bot PR Review Fixes

## Overview

This design addresses five critical issues identified by Q Bot in PR #2:

1. **Integer Overflow**: Replace `+ 1` with `saturating_add(1)` in priority scoring and depth tracking
2. **Parser Recreation**: Use thread-local parser instances to avoid repeated allocation
3. **O(n²) Complexity**: Replace Vec with HashSet for affected files collection
4. **Deadlock Risk**: Analyze and document lock acquisition patterns in did_open
5. **Sequential File I/O**: Evaluate concurrent execution opportunities in on-demand indexing

The fixes are targeted, minimal changes that improve correctness and performance without altering the overall architecture.

## Architecture

The fixes are localized to specific modules:

- **backend.rs**: Priority score calculation, affected files collection
- **background_indexer.rs**: Depth increment, parser usage
- **cross_file/mod.rs**: Parser usage in extract_metadata

No architectural changes are required. The fixes maintain existing interfaces and behavior while improving implementation details.

## Components and Interfaces

### 1. Thread-Local Parser Pool

**Purpose**: Provide reusable Parser instances per thread to avoid allocation overhead

**Interface**:
```rust
thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into())
            .expect("Failed to set R language");
        parser
    });
}

fn with_parser<F, R>(f: F) -> R
where
    F: FnOnce(&mut Parser) -> R,
{
    PARSER.with(|parser| f(&mut parser.borrow_mut()))
}
```

**Usage Pattern**:
```rust
// Before: Creates new parser every call
let mut parser = Parser::new();
parser.set_language(&tree_sitter_r::LANGUAGE.into())?;
let tree = parser.parse(content, None);

// After: Reuses thread-local parser
let tree = with_parser(|parser| parser.parse(content, None));
```

**Location**: Create new module `crates/rlsp/src/parser_pool.rs`

### 2. Saturating Arithmetic

**Purpose**: Prevent integer overflow in counters and scores

**Changes**:
- `backend.rs` line 607: `activity.priority_score(u).saturating_add(1)`
- `background_indexer.rs` line 346: `current_depth.saturating_add(1)`

**Behavior**: When addition would overflow, result is capped at `usize::MAX` instead of wrapping

### 3. HashSet for Affected Files

**Purpose**: Provide O(1) membership testing for affected files collection

**Interface Change**:
```rust
// Before: Vec with linear search
let mut affected: Vec<Url> = vec![uri.clone()];
for dep in dependents {
    if state.documents.contains_key(&dep) && !affected.contains(&dep) {
        affected.push(dep);
    }
}

// After: HashSet with constant-time insert
let mut affected: HashSet<Url> = HashSet::from([uri.clone()]);
for dep in dependents {
    if state.documents.contains_key(&dep) {
        state.diagnostics_gate.mark_force_republish(&dep);
        affected.insert(dep);
    }
}
```

**Conversion**: Convert HashSet to Vec for sorting and iteration:
```rust
let mut affected: Vec<Url> = affected.into_iter().collect();
affected.sort_by_key(|u| { /* priority logic */ });
```

## Data Models

No new data models are introduced. Existing types are used:

- `thread_local!` macro for Parser storage
- `RefCell<Parser>` for interior mutability
- `HashSet<Url>` instead of `Vec<Url>` for affected files
- `usize` with saturating arithmetic for counters

## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*


### Property 1: Saturating Arithmetic Prevents Overflow

*For any* counter or score value at or near `usize::MAX`, when performing saturating addition, the result should be capped at `usize::MAX` without wrapping to zero.

**Validates: Requirements 1.1, 1.2**

### Property 2: System Stability at Boundary Conditions

*For any* operation involving counters at maximum values (priority scores, depth counters), the system should continue to function correctly without panics or incorrect behavior.

**Validates: Requirements 1.4**

### Property 3: Parser Instance Reuse

*For any* sequence of metadata extraction or file indexing operations on the same thread, the same Parser instance should be reused rather than creating new instances.

**Validates: Requirements 2.1, 2.2**

### Property 4: HashSet Insert Deduplication

*For any* file URI, when inserting into the affected files HashSet, the insert operation should return true for the first insertion and false for subsequent insertions of the same URI.

**Validates: Requirements 3.3**

## Error Handling

The fixes maintain existing error handling patterns:

- **Parser initialization**: Thread-local parser initialization uses `expect()` since language setting should never fail
- **Arithmetic overflow**: Saturating arithmetic eliminates overflow errors by design
- **HashSet operations**: Insert and contains operations are infallible for valid Url types

No new error cases are introduced by these fixes.

## Testing Strategy

### Unit Tests

Unit tests will verify specific examples and edge cases:

1. **Saturating arithmetic edge cases**:
   - Test `usize::MAX.saturating_add(1) == usize::MAX`
   - Test `(usize::MAX - 1).saturating_add(1) == usize::MAX`
   - Test normal values work correctly

2. **Parser reuse verification**:
   - Test that multiple calls to `extract_metadata` on same thread reuse parser
   - Test that parser state is properly reset between uses

3. **HashSet behavior**:
   - Test that first insert returns true
   - Test that duplicate insert returns false
   - Test that affected files collection has no duplicates

4. **Integration tests**:
   - Test did_open with large dependency graphs
   - Test background indexing with deep transitive dependencies
   - Verify no performance regression from HashSet conversion

### Property-Based Tests

Property tests will verify universal properties across all inputs:

1. **Property 1 (Saturating Arithmetic)**: Generate random counter values including boundary cases, verify saturating_add never overflows
2. **Property 2 (System Stability)**: Generate scenarios with maximum counter values, verify system continues operating
3. **Property 3 (Parser Reuse)**: Generate sequences of operations, verify parser instance identity
4. **Property 4 (HashSet Deduplication)**: Generate random URI sequences with duplicates, verify insert behavior

All property tests should run with minimum 100 iterations to ensure comprehensive coverage.

### Performance Testing

Verify that the fixes improve performance:

1. **Parser allocation**: Measure allocation count before/after thread-local change
2. **Affected files collection**: Measure time complexity with varying dependency graph sizes
3. **Background indexing**: Measure throughput with deep transitive dependencies

## Implementation Notes

### Parser Thread-Local Implementation

The thread-local parser must be properly initialized with the R language:

```rust
thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into())
            .expect("Failed to set R language");
        parser
    });
}
```

The `expect()` is appropriate here because:
- Language setting should never fail for a valid tree-sitter language
- This is initialization code that runs once per thread
- Failure here indicates a serious configuration problem that should halt execution

### HashSet Conversion Considerations

When converting from Vec to HashSet:

1. **Preserve sorting**: Convert back to Vec before sorting by priority
2. **Maintain iteration order**: Use `into_iter().collect()` for deterministic conversion
3. **Update all call sites**: Both `did_open` and `did_change` use the same pattern

### Deadlock Analysis

The did_open handler currently:
1. Acquires write lock to update state
2. Performs synchronous Priority 1 indexing (which may need read locks)
3. Releases write lock
4. Spawns async tasks for diagnostics

Potential deadlock scenario:
- If synchronous indexing tries to acquire a read lock while holding write lock
- This would deadlock if another task is waiting for write lock

**Mitigation**: The current code releases the write lock before indexing, so no deadlock exists. Document this pattern for future maintainers.

### Sequential vs Concurrent File I/O

Current implementation uses sequential file I/O in `index_file_on_demand`:
- Reads file content
- Computes metadata
- Updates caches
- Updates dependency graph

**Analysis**: Sequential is appropriate because:
- Each file's processing depends on previous state updates
- Dependency graph updates must be serialized
- File I/O is fast relative to parsing and analysis
- Complexity of concurrent coordination outweighs benefits

**Recommendation**: Keep sequential implementation. If profiling shows I/O bottleneck, consider batching multiple independent files.

## Deployment Considerations

These fixes are backward compatible:
- No API changes
- No configuration changes
- No breaking changes to existing behavior

The fixes can be deployed incrementally:
1. Deploy saturating arithmetic fixes (lowest risk)
2. Deploy HashSet conversion (medium risk, verify performance)
3. Deploy thread-local parser (highest risk, verify thread safety)

Rollback strategy: Each fix is independent and can be reverted individually if issues arise.
