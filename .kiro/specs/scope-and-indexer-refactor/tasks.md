# Implementation Plan: Scope Resolution and Background Indexer Refactor

## Overview

This implementation plan removes the legacy `scope_at_position_with_backward` function and simplifies the `BackgroundIndexer` by removing priority tiers.

## Tasks

### Phase 1: Remove scope_at_position_with_backward

- [x] 1. Create test helper for building dependency graphs
  - [x] 1.1 Add `build_test_graph` helper function in scope.rs test module
    - Takes parent/child URIs, metadata, and content
    - Returns a populated DependencyGraph
    - Reduces boilerplate in migrated tests
    - _Requirements: 4.4, 4.5_

- [x] 2. Migrate scope.rs unit tests
  - [x] 2.1 Identify all tests using `scope_at_position_with_backward` in scope.rs
  - [x] 2.2 Migrate `test_scope_with_backward_directive` to use graph-based approach
  - [x] 2.3 Migrate `test_rm_with_backward_directive_*` tests to use graph-based approach
  - [x] 2.4 Verify all migrated tests pass
    - _Requirements: 4.4_

- [x] 3. Migrate property_tests.rs tests
  - [x] 3.1 Remove import of `scope_at_position_with_backward` from property_tests.rs
  - [x] 3.2 Migrate `prop_backward_directive_provides_parent_scope` test
  - [x] 3.3 Migrate `prop_backward_directive_with_explicit_call_site` test
  - [x] 3.4 Migrate `prop_backward_directive_with_match_call_site` test
  - [x] 3.5 Migrate `prop_backward_directive_default_call_site` test
  - [x] 3.6 Migrate `prop_assume_call_site_config` test
  - [x] 3.7 Verify all migrated property tests pass
    - _Requirements: 4.3, 4.5_

- [x] 4. Remove legacy functions
  - [x] 4.1 Remove `scope_at_position_with_backward` public function from scope.rs
  - [x] 4.2 Remove `scope_at_position_with_backward_recursive` private function from scope.rs
  - [x] 4.3 Remove any unused imports after removal
  - [x] 4.4 Verify compilation succeeds
    - _Requirements: 4.1, 4.2_

- [x] 5. Checkpoint - Verify Phase 1 complete
  - Run `cargo test -p raven` and ensure all tests pass
  - Run `cargo clippy -p raven` and ensure no warnings
    - _Requirements: 1.1, 1.2, 1.3, 1.4_

### Phase 2: Simplify BackgroundIndexer

- [x] 6. Remove priority from IndexTask
  - [x] 6.1 Remove `priority` field from `IndexTask` struct in background_indexer.rs
  - [x] 6.2 Update `IndexTask` creation sites to not include priority
  - [x] 6.3 Remove priority-based insertion logic in `submit()`
  - [x] 6.4 Use simple `push_back()` for FIFO ordering
    - _Requirements: 5.1, 5.2, 5.5_

- [x] 7. Remove priority config options
  - [x] 7.1 Remove `on_demand_indexing_priority_2_enabled` from CrossFileConfig
  - [x] 7.2 Remove `on_demand_indexing_priority_3_enabled` from CrossFileConfig
  - [x] 7.3 Update `from_initialization_options()` to not parse these options
  - [x] 7.4 Update default config
    - _Requirements: 5.3, 5.4_

- [x] 8. Update BackgroundIndexer callers
  - [x] 8.1 Update `submit()` calls in backend.rs to not pass priority
  - [x] 8.2 Update `queue_transitive_deps()` to not use priority
  - [x] 8.3 Simplify logging to not mention priority
    - _Requirements: 5.5, 5.6_

- [x] 9. Update BackgroundIndexer tests
  - [x] 9.1 Remove `test_queue_priority_ordering` test
  - [x] 9.2 Remove `test_priority_2_before_priority_3` test
  - [x] 9.3 Update remaining tests to not use priority
  - [x] 9.4 Add test for simple FIFO ordering
    - _Requirements: 2.3_

- [x] 10. Checkpoint - Verify Phase 2 complete
  - Run `cargo test -p raven` and ensure all tests pass
  - Run `cargo clippy -p raven` and ensure no warnings
    - _Requirements: 2.1, 2.2, 2.3, 2.4_

### Phase 3: Update Documentation

- [x] 11. Update AGENTS.md
  - [x] 11.1 Remove "Priority Levels" section from BackgroundIndexer documentation
  - [x] 11.2 Remove references to Priority 2/3 throughout the file
  - [x] 11.3 Remove `scope_at_position_with_backward` from any examples or references
  - [x] 11.4 Update "On-Demand Background Indexing" section to describe simplified approach
  - [x] 11.5 Update configuration documentation to remove priority options
    - _Requirements: 6.1, 6.2, 6.3, 6.4_

- [x] 12. Final verification
  - [x] 12.1 Run full test suite: `cargo test -p raven`
  - [x] 12.2 Run clippy: `cargo clippy -p raven`
  - [ ] 12.3 Verify cross-file features work in manual testing
    - _Requirements: 3.1, 3.2, 3.3, 3.4_

## Notes

- Phase 1 must be completed before Phase 2 (no dependencies, but logical ordering)
- Each checkpoint ensures incremental validation
- The test helper in task 1.1 is critical for reducing migration complexity
- Property tests may need additional setup for dependency graph construction
