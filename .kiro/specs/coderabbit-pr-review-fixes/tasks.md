# Implementation Plan: CodeRabbit PR Review Fixes

## Overview

This implementation plan addresses the CodeRabbit PR review feedback for PR #2. Tasks are organized by priority: code fixes first (critical), then documentation fixes (minor).

## Tasks

- [x] 1. Fix on-demand indexing global flag check (backend.rs)
  - [x] 1.1 Add early check for `on_demand_indexing_enabled` flag in `did_open` handler
    - Wrap Priority 1 synchronous indexing loop in conditional
    - Wrap Priority 2 submission block in conditional
    - Wrap Priority 3 transitive-queueing block in conditional
    - _Requirements: 1.1, 1.2, 1.3, 1.4_
  - [x] 1.2 Write unit test for disabled on-demand indexing
    - **Property 1: On-demand indexing respects global flag**
    - **Validates: Requirements 1.1, 1.2, 1.3**

- [x] 2. Fix directive regex for quoted paths with spaces (directive.rs)
  - [x] 2.1 Update backward directive regex to handle quoted paths with spaces
    - Change pattern to use alternation: `(?:"([^"]+)"|'([^']+)'|([^\s]+))`
    - _Requirements: 2.1, 2.2_
  - [x] 2.2 Update forward directive regex to handle quoted paths with spaces
    - Apply same pattern change
    - _Requirements: 2.3_
  - [x] 2.3 Update working directory directive regex to handle quoted paths with spaces
    - Apply same pattern change
    - _Requirements: 2.4_
  - [x] 2.4 Add `capture_path()` helper function
    - Extract path from correct capture group (double-quoted, single-quoted, or unquoted)
    - Update `parse_directives()` to use the helper
    - _Requirements: 2.5_
  - [x] 2.5 Write unit tests for quoted paths with spaces
    - Test double-quoted paths with spaces
    - Test single-quoted paths with spaces
    - Test unquoted paths (regression)
    - **Property 2: Quoted path extraction preserves spaces**
    - **Validates: Requirements 2.1, 2.2, 2.3, 2.4, 2.6**

- [x] 3. Fix parent resolution child path (parent_resolve.rs)
  - [x] 3.1 Derive child_path from child_uri in `resolve_parent_with_content`
    - Extract filename from child_uri using `to_file_path()` and `file_name()`
    - Pass child_path to `resolve_match_pattern()` and `infer_call_site_from_parent()`
    - _Requirements: 3.1, 3.2, 3.3_
  - [x] 3.2 Write unit test for child path resolution
    - Test that match patterns work with correct child path
    - Test that call-site inference works with correct child path
    - _Requirements: 3.1, 3.2_

- [x] 4. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. Fix path normalization ParentDir handling (path_resolve.rs)
  - [x] 5.1 Update `normalize_path` to only pop Normal segments
    - Check if last component is `Component::Normal` before popping
    - Preserve RootDir and Prefix components
    - _Requirements: 4.1, 4.2, 4.3_
  - [x] 5.2 Write unit tests for path normalization edge cases
    - Test `/../a` produces `/a`
    - Test `/a/../b` produces `/b`
    - Test `a/../b` produces `b`
    - **Property 3: Path normalization preserves root**
    - **Validates: Requirements 4.1, 4.2, 4.3, 4.4**

- [x] 6. Fix diagnostic range precision (handlers.rs)
  - [x] 6.1 Update diagnostic range calculation to use actual path length
    - Change `source.column + source.path.len() as u32 + 10` to `source.column + source.path.len() as u32`
    - _Requirements: 5.1, 5.2_
  - [x] 6.2 Write unit test for diagnostic range calculation
    - **Property 4: Diagnostic range matches path length**
    - **Validates: Requirements 5.1**

- [x] 7. Remove dead code (handlers.rs)
  - [x] 7.1 Remove unused `collect_identifier_usages` function
    - Keep only `collect_identifier_usages_utf16`
    - _Requirements: 6.1, 6.2_

- [x] 8. Fix IndexEntry comment (state.rs)
  - [x] 8.1 Update misleading comment about `indexed_at_version`
    - Change "Will be updated when inserted" to accurate description
    - _Requirements: 7.1, 7.2_

- [x] 9. Fix non-blocking file existence check (handlers.rs)
  - [x] 9.1 Remove blocking `path.exists()` call from `file_exists` closure
    - Return true for uncached files to avoid false positive diagnostics
    - Add trace log for cache misses
    - _Requirements: 8.1, 8.2, 8.3_

- [x] 10. Checkpoint - Ensure all code tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 11. Fix markdown documentation
  - [x] 11.1 Add `text` language tag to checkpoint-4-findings.md code block
    - Line 239: test output block
    - _Requirements: 9.1_
  - [x] 11.2 Add `text` language tags to final-checkpoint-report.md code blocks
    - Lines 272, 292, 313, 336: test output blocks
    - _Requirements: 9.2_
  - [x] 11.3 Add `text` language tag to task-10.2-findings.md code block
    - Line 140: output block
    - _Requirements: 9.3_
  - [x] 11.4 Add `text` language tags to task-16.3-build-results.md code blocks
    - Lines 16, 61: code blocks
    - _Requirements: 9.4_
  - [x] 11.5 Replace bold text with heading in task-16.3-build-results.md
    - Line 194: Change "**Task 16.3 Build Phase: COMPLETE ✅**" to "## Task 16.3 Build Phase: COMPLETE ✅"
    - _Requirements: 10.1_
  - [x] 11.6 Add `text` language tag to priority-2-3-indexing/design.md diagram block
    - Line 11: diagram block
    - _Requirements: 11.1_

- [x] 12. Final checkpoint - Ensure all tests pass
  - Run `cargo test -p rlsp` to verify all tests pass
  - Run `cargo build -p rlsp` to verify clean compilation
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties
- Unit tests validate specific examples and edge cases
