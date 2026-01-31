# Implementation Plan: Interval Tree Scope Lookup

## Overview

This plan implements an interval tree data structure for O(log n) function scope lookups, replacing the current O(n) linear scans in the scope resolution code. The implementation follows a bottom-up approach: first implementing the core data structure with tests, then integrating it into the existing scope resolution system.

## Tasks

- [x] 1. Implement core interval tree data structure
  - [x] 1.1 Create Position and FunctionScopeInterval types in scope.rs
    - Add `Position` struct with line/column fields and `Ord` implementation
    - Add `FunctionScopeInterval` struct with start/end positions
    - Implement `contains()`, `from_tuple()`, `to_tuple()` methods
    - _Requirements: 4.1, 1.1_

  - [x] 1.2 Implement FunctionScopeTree structure
    - Create internal `IntervalNode` struct with interval, max_end, left, right fields
    - Create `FunctionScopeTree` struct with root and count fields
    - Implement `new()`, `is_empty()`, `len()` methods
    - _Requirements: 1.1_

  - [x] 1.3 Implement tree construction from sorted intervals
    - Implement `from_scopes()` that builds balanced tree from slice
    - Use recursive median-split approach for balance
    - Compute `max_end` augmentation during construction
    - Filter out invalid intervals (start > end) with warning
    - _Requirements: 1.1, 1.5_

  - [x] 1.4 Implement point query method
    - Implement `query_point()` that returns all containing intervals
    - Use max_end augmentation for subtree pruning
    - Handle empty tree case
    - _Requirements: 1.3, 1.4, 1.6_

  - [x] 1.5 Implement innermost query method
    - Implement `query_innermost()` that returns interval with max start
    - Reuse `query_point()` and select max start from results
    - Return None when no intervals contain the point
    - _Requirements: 2.1, 2.2_

  - [x] 1.6 Write unit tests for interval tree
    - Test empty tree queries
    - Test single interval containment
    - Test boundary positions (inclusive)
    - Test nested intervals for innermost selection
    - Test EOF sentinel positions
    - _Requirements: 1.6, 2.2, 4.4_

- [x] 2. Checkpoint - Verify interval tree implementation
  - Ensure all unit tests pass, ask the user if questions arise.

- [x] 3. Integrate interval tree into ScopeArtifacts
  - [x] 3.1 Update ScopeArtifacts struct
    - Replace `function_scopes: Vec<(u32, u32, u32, u32)>` with `function_scope_tree: FunctionScopeTree`
    - Update `Default` implementation
    - _Requirements: 3.1_

  - [x] 3.2 Update compute_artifacts() to build interval tree
    - Collect function scope tuples from FunctionScope events
    - Build `FunctionScopeTree` using `from_scopes()`
    - Remove old `function_scopes` population code
    - _Requirements: 3.2_

  - [x] 3.3 Update find_containing_function_scope() helper
    - Change signature to accept `&FunctionScopeTree` instead of slice
    - Delegate to `query_innermost()` method
    - Convert result back to tuple format for compatibility
    - _Requirements: 2.1, 3.3_

- [x] 4. Replace linear scans with interval tree queries
  - [x] 4.1 Update scope_at_position() function
    - Replace `artifacts.function_scopes.iter().filter().max_by_key()` with `function_scope_tree.query_point()`
    - Update active_function_scopes collection to use tree query
    - Update def_function_scope lookup to use `query_innermost()`
    - _Requirements: 3.3, 3.4_

  - [x] 4.2 Update scope_at_position_recursive() function
    - Same pattern as 4.1 for the recursive variant
    - Replace all three linear scan locations
    - _Requirements: 3.3, 3.4_

  - [x] 4.3 Update scope_at_position_with_graph_recursive() if present
    - Apply same replacement pattern
    - Ensure all function_scopes.iter() calls are replaced
    - _Requirements: 3.3, 3.4_

- [x] 5. Checkpoint - Verify integration
  - Ensure all existing tests pass, ask the user if questions arise.

- [x] 6. Add property-based tests
  - [x] 6.1 Write property test for point query correctness
    - **Property 1: Point Query Correctness**
    - Generate random intervals and query points
    - Verify all returned intervals contain the point (no false positives)
    - Compare with brute-force to verify no false negatives
    - **Validates: Requirements 1.3, 1.4**

  - [x] 6.2 Write property test for innermost selection
    - **Property 2: Innermost Selection Correctness**
    - Generate random intervals and query points
    - Verify result has maximum start among all containing intervals
    - **Validates: Requirements 2.1, 2.2**

  - [x] 6.3 Write property test for backward compatibility
    - **Property 3: Backward Compatibility**
    - Generate random R code with nested functions
    - Compare scope resolution results: old (linear) vs new (tree)
    - **Validates: Requirements 3.4**

  - [x] 6.4 Write property test for position ordering
    - **Property 4: Position Lexicographic Ordering**
    - Generate random position pairs
    - Verify Ord implementation matches lexicographic definition
    - **Validates: Requirements 4.1**

- [x] 7. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- The interval tree uses a static construction approach (build once from sorted intervals)
- Backward compatibility is critical - existing scope resolution behavior must be preserved
- Property tests validate universal correctness properties across random inputs
