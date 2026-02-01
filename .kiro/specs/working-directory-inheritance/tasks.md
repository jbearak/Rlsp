# Implementation Plan: Working Directory Inheritance

## Overview

This implementation plan adds working directory inheritance support for backward directives. The work is organized into incremental tasks that build on each other, with property tests validating correctness at each stage.

## Tasks

- [x] 1. Extend CrossFileMetadata with inherited_working_directory field
  - [x] 1.1 Add `inherited_working_directory: Option<String>` field to `CrossFileMetadata` struct in `types.rs`
    - Add the field with appropriate documentation
    - Update `Default` implementation
    - Update serialization/deserialization (already derived)
    - _Requirements: 6.1_
  - [x] 1.2 Write unit tests for CrossFileMetadata with inherited_working_directory
    - Test serialization round-trip with the new field
    - Test default value is None
    - _Requirements: 6.1_

- [x] 2. Update PathContext to use inherited_working_directory from metadata
  - [x] 2.1 Modify `PathContext::from_metadata` in `path_resolve.rs` to populate `inherited_working_directory`
    - When metadata has `inherited_working_directory` and no explicit `working_directory`, resolve and set `inherited_working_directory` on PathContext
    - Ensure explicit working directory still takes precedence
    - _Requirements: 6.2, 6.3, 3.1_
  - [x] 2.2 Write property test for PathContext metadata round-trip
    - **Property 6: Metadata and PathContext Round-Trip**
    - **Validates: Requirements 6.1, 6.2, 6.3**
  - [x] 2.3 Write property test for explicit working directory precedence
    - **Property 3: Explicit Working Directory Precedence**
    - **Validates: Requirements 3.1, 3.2**

- [x] 3. Implement parent working directory resolution
  - [x] 3.1 Add `resolve_parent_working_directory` function in `dependency.rs`
    - Takes parent URI, metadata getter, and workspace root
    - Returns parent's effective working directory as Option<String>
    - Handles case where parent metadata is unavailable (fallback to parent's directory)
    - _Requirements: 5.1, 5.3_
  - [x] 3.2 Add `compute_inherited_working_directory` function in `dependency.rs`
    - Takes child URI, metadata, workspace root, and metadata getter
    - Returns None if child has explicit @lsp-cd
    - Uses first backward directive to determine parent
    - Calls `resolve_parent_working_directory` to get inherited WD
    - _Requirements: 1.1, 2.1, 7.1_
  - [x] 3.3 Write property test for parent effective WD inheritance
    - **Property 1: Parent Effective Working Directory Inheritance**
    - **Validates: Requirements 1.1, 2.1, 2.2**
  - [x] 3.4 Write property test for fallback when parent metadata unavailable
    - **Property 5: Fallback When Parent Metadata Unavailable**
    - **Validates: Requirements 5.3**
  - [x] 3.5 Write property test for first backward directive wins
    - **Property 7: First Backward Directive Wins**
    - **Validates: Requirements 7.1, 7.2**

- [x] 4. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. Integrate inheritance into metadata extraction flow
  - [x] 5.1 Update metadata extraction in `handlers.rs` to compute inherited working directory
    - After parsing directives, call `compute_inherited_working_directory`
    - Store result in metadata's `inherited_working_directory` field
    - Pass appropriate metadata getter function
    - _Requirements: 5.1, 5.2, 6.1_
  - [x] 5.2 Update `DependencyGraph::update_file` to pass metadata getter for inheritance computation
    - Ensure the get_content closure or a new get_metadata closure is available
    - _Requirements: 5.1_
  - [x] 5.3 Write property test for path resolution using inherited WD
    - **Property 2: Path Resolution Uses Inherited Working Directory**
    - **Validates: Requirements 1.2, 1.3**

- [x] 6. Ensure backward directive paths ignore working directory
  - [x] 6.1 Verify backward directive path resolution uses file-relative PathContext
    - Review existing code in `dependency.rs` that uses `backward_path_ctx`
    - Ensure `inherited_working_directory` is NOT used for backward directive resolution
    - Add comments clarifying this intentional behavior
    - _Requirements: 4.1, 4.2, 4.3_
  - [x] 6.2 Write property test for backward directive path resolution
    - **Property 4: Backward Directive Paths Ignore Working Directory**
    - **Validates: Requirements 4.1, 4.2, 4.3**

- [x] 7. Implement transitive inheritance and cycle handling
  - [x] 7.1 Ensure transitive inheritance works through metadata propagation
    - When computing B's inherited WD from A, B's metadata includes inherited WD
    - When computing C's inherited WD from B, it gets B's effective WD (which includes A's)
    - Add depth tracking to prevent infinite chains
    - _Requirements: 9.1, 9.2_
  - [x] 7.2 Add cycle detection in inheritance computation
    - Track visited URIs during inheritance resolution
    - Stop and use file's directory when cycle detected
    - _Requirements: 9.3_
  - [x] 7.3 Write property test for transitive inheritance
    - **Property 8: Transitive Inheritance**
    - **Validates: Requirements 9.1**
  - [x] 7.4 Write property test for depth limiting
    - **Property 9: Depth Limiting**
    - **Validates: Requirements 9.2**
  - [x] 7.5 Write property test for cycle handling
    - **Property 10: Cycle Handling**
    - **Validates: Requirements 9.3**

- [x] 8. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 9. Implement cache invalidation for working directory changes
  - [x] 9.1 Update revalidation logic to detect parent WD changes
    - Compare old and new working_directory in parent metadata
    - Trigger child revalidation when parent WD changes
    - _Requirements: 8.1, 8.2_
  - [x] 9.2 Add invalidation of child metadata when parent WD changes
    - Find children via dependency graph's backward edges
    - Invalidate their metadata cache entries
    - _Requirements: 8.3_
  - [x] 9.3 Write integration test for cache invalidation
    - Test that changing parent's @lsp-cd triggers child recomputation
    - _Requirements: 8.1, 8.2, 8.3_

- [x] 10. Add integration tests for end-to-end scenarios
  - [x] 10.1 Add integration test for basic inheritance scenario
    - Parent with @lsp-cd, child with @lsp-sourced-by, verify source() resolution
    - _Requirements: 1.1, 1.2_
  - [x] 10.2 Add integration test for implicit inheritance scenario
    - Parent without @lsp-cd, child inherits parent's directory
    - _Requirements: 2.1, 2.2_
  - [x] 10.3 Add integration test for precedence scenario
    - Child with both @lsp-sourced-by and @lsp-cd, verify child's WD wins
    - _Requirements: 3.1_

- [x] 11. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- All tasks are required for comprehensive implementation and testing
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties
- Unit tests validate specific examples and edge cases
- The implementation builds incrementally: types → functions → integration → cache
