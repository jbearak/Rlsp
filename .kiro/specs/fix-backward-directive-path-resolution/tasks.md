# Implementation Plan: Fix Backward Directive Path Resolution

## Overview

This implementation fixes a bug in `collect_ambiguous_parent_diagnostics` where backward directive paths are incorrectly resolved using `PathContext::from_metadata` (respects `@lsp-cd`) instead of `PathContext::new` (ignores `@lsp-cd`). The fix is a single-line change to use the correct PathContext constructor.

## Tasks

- [x] 1. Fix PathContext usage in collect_ambiguous_parent_diagnostics
  - [x] 1.1 Change PathContext::from_metadata to PathContext::new
    - In `crates/raven/src/handlers.rs`, function `collect_ambiguous_parent_diagnostics`
    - Change line ~896 from `PathContext::from_metadata(uri, meta, state.workspace_folders.first())` to `PathContext::new(uri, state.workspace_folders.first())`
    - _Requirements: 1.1, 1.2, 1.3, 4.1_

  - [x] 1.2 Write unit test for bug reproduction
    - Create test with `@lsp-cd ..` and `@lsp-run-by: program.r`
    - Verify backward directive resolves relative to file's directory, not @lsp-cd
    - _Requirements: 1.2, 3.2_

  - [x] 1.3 Write property test for backward directive resolution
    - **Property 1: Backward directive path resolution ignores @lsp-cd**
    - **Validates: Requirements 1.2, 1.3, 3.1**

- [x] 2. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.
  - Run `cargo test -p raven` to verify no regressions

## Notes

- All tasks are required for comprehensive coverage
- The core fix is a single-line change in task 1.1
- Property tests validate the path resolution invariant across many inputs
