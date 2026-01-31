# Implementation Plan: Q Bot PR Review Fixes

## Overview

This plan implements five targeted fixes to address Q Bot's PR review comments. The fixes are independent and can be implemented incrementally, starting with the lowest-risk changes (saturating arithmetic) and progressing to higher-risk changes (thread-local parser).

## Tasks

- [x] 1. Create thread-local parser pool module
  - Create `crates/rlsp/src/parser_pool.rs` with thread-local Parser storage
  - Implement `with_parser` helper function for safe parser access
  - Add module declaration to `crates/rlsp/src/lib.rs`
  - _Requirements: 2.1, 2.2_

- [x] 1.1 Write unit tests for parser pool
  - Test that parser is properly initialized with R language
  - Test that multiple calls on same thread reuse parser instance
  - Test that parser state is reset between uses
  - _Requirements: 2.1, 2.2_

- [x] 1.2 Write property test for parser reuse
  - **Property 3: Parser Instance Reuse**
  - **Validates: Requirements 2.1, 2.2**

- [x] 2. Fix integer overflow in backend.rs
  - Replace `activity.priority_score(u) + 1` with `activity.priority_score(u).saturating_add(1)` at line 607
  - Add comment explaining why saturating arithmetic is used
  - _Requirements: 1.1_

- [x] 2.1 Write unit tests for saturating arithmetic
  - Test that `usize::MAX.saturating_add(1) == usize::MAX`
  - Test that `(usize::MAX - 1).saturating_add(1) == usize::MAX`
  - Test that normal values work correctly
  - _Requirements: 1.1, 1.2_

- [x] 2.2 Write property test for saturating arithmetic
  - **Property 1: Saturating Arithmetic Prevents Overflow**
  - **Validates: Requirements 1.1, 1.2**

- [x] 3. Fix integer overflow in background_indexer.rs
  - Replace `current_depth + 1` with `current_depth.saturating_add(1)` at line 346
  - Add comment explaining why saturating arithmetic is used
  - _Requirements: 1.2_

- [x] 4. Replace Vec with HashSet for affected files in backend.rs
  - [x] 4.1 Update did_open handler (around line 607)
    - Change `affected` from `Vec<Url>` to `HashSet<Url>`
    - Initialize with `HashSet::from([uri.clone()])`
    - Replace `!affected.contains(&dep)` check with direct `affected.insert(dep)`
    - Convert to Vec before sorting: `let mut affected: Vec<Url> = affected.into_iter().collect()`
    - _Requirements: 3.1, 3.3, 3.4_
  
  - [x] 4.2 Update did_change handler (around line 817)
    - Apply same HashSet changes as in did_open
    - Ensure consistent pattern across both handlers
    - _Requirements: 3.1, 3.3, 3.4_

- [x] 4.3 Write unit tests for HashSet behavior
  - Test that first insert returns true
  - Test that duplicate insert returns false
  - Test that affected files collection has no duplicates
  - _Requirements: 3.3_

- [x] 4.4 Write property test for HashSet deduplication
  - **Property 4: HashSet Insert Deduplication**
  - **Validates: Requirements 3.3**

- [x] 5. Update extract_metadata to use thread-local parser
  - Modify `crates/rlsp/src/cross_file/mod.rs` line 62
  - Replace inline parser creation with `parser_pool::with_parser` call
  - Remove local `parser` variable and `set_language` call
  - _Requirements: 2.1_

- [x] 6. Update background_indexer to use thread-local parser
  - Modify `index_file` function in `background_indexer.rs` line 219
  - Replace inline parser creation with `parser_pool::with_parser` call
  - Remove local `parser` variable and `set_language` call
  - _Requirements: 2.2_

- [x] 7. Document deadlock analysis
  - Add documentation comment to `did_open` handler explaining lock acquisition pattern
  - Document why current implementation avoids deadlock (write lock released before indexing)
  - Add note for future maintainers about lock ordering
  - _Requirements: 4.1, 4.3_

- [x] 8. Document sequential file I/O rationale
  - Add documentation comment to `index_file_on_demand` explaining sequential approach
  - Document why sequential is appropriate (dependency graph serialization, fast I/O)
  - Note conditions under which concurrent execution might be beneficial
  - _Requirements: 5.1, 5.2, 5.3, 5.4_

- [x] 9. Write integration tests
  - Test did_open with large dependency graphs (verify no performance regression)
  - Test background indexing with deep transitive dependencies (verify saturating arithmetic)
  - Test that system remains stable with counters at maximum values
  - _Requirements: 1.4_

- [x] 9.1 Write property test for system stability
  - **Property 2: System Stability at Boundary Conditions**
  - **Validates: Requirements 1.4**

- [x] 10. Checkpoint - Ensure all tests pass
  - Run `cargo test -p rlsp` to verify all tests pass
  - Run `cargo clippy -p rlsp` to check for warnings
  - Ensure no performance regressions from changes
  - Ask the user if questions arise

## Notes

- All tasks are required for comprehensive bug fixes and testing
- Each task references specific requirements for traceability
- Fixes are independent and can be implemented in any order
- Saturating arithmetic fixes (tasks 2-3) are lowest risk
- HashSet conversion (task 4) is medium risk - verify performance
- Thread-local parser (tasks 1, 5-6) is highest risk - verify thread safety
- Documentation tasks (7-8) have no code risk
