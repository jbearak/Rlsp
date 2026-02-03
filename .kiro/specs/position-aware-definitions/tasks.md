# Implementation Plan: Position-Aware Definitions

## Overview

This implementation fixes the go-to-definition bug where definitions occurring after the usage position are incorrectly returned. The fix modifies `find_definition_in_tree` to filter definitions by position and updates undefined variable diagnostics to use the same position-aware logic.

## Tasks

- [x] 1. Refactor `goto_definition` to use `ScopeArtifacts`
  - [x] 1.1 Update `goto_definition` in `handlers.rs`
    - Retrieve `ScopeArtifacts` for the current URI (via `ContentProvider` or `WorldState`)
    - Call `crate::cross_file::scope::scope_at_position` with the cursor position
    - Check if the symbol exists in the resolved scope
    - If found, return the definition location
    - If not found, fall back to existing cross-file/workspace search
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 4.1, 4.2, 4.3_

  - [x] 1.2 Write property test for position-aware definition lookup
    - **Property 1: Position-Aware Definition Lookup**
    - Generate random R code with variable definitions and usages
    - Verify returned definition position is always < query position
    - **Validates: Requirements 1.1, 1.5**

  - [x] 1.3 Write property test for function scope isolation
    - **Property 8: Function-Local Position Awareness**
    - Generate functions with local variable definitions and usages
    - Verify position-aware resolution within function scope
    - Verify local variables are not visible outside
    - **Validates: Requirements 4.1, 4.2, 4.3**

  - [x] 1.4 Write unit tests for updated go-to-definition
    - Test definition before usage returns correct location
    - Test definition after usage returns None (falls through)
    - Test same-line definition at earlier column
    - Test shadowing (local overrides global)
    - _Requirements: 1.1, 1.2, 1.4_

- [x] 2. Checkpoint - Verify go-to-definition fix
  - Ensure all tests pass, ask the user if questions arise.

- [x] 3. Update undefined variable diagnostics
  - [x] 3.1 Modify `collect_undefined_variables_position_aware`
    - Remove the initial `collect_definitions` pass (which was position-ignorant)
    - Rely entirely on `get_cross_file_scope` (which calls `scope_at_position`) to determine symbol validity
    - _Requirements: 2.1, 2.2, 2.3, 2.4_

  - [x] 3.2 Write property test for undefined variable emission
    - **Property 4: Undefined Variable Emission**
    - Generate code with forward references (usage before definition)
    - Verify diagnostic is emitted at usage position
    - **Validates: Requirements 2.1, 2.3**

  - [x] 3.3 Write property test for no false positives
    - **Property 5: No False Positive Diagnostics**
    - Generate code with definitions before usage
    - Verify no "undefined variable" diagnostic is emitted
    - **Validates: Requirements 2.2**

- [x] 4. Checkpoint - Verify undefined variable diagnostics fix
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. Verify cross-file compatibility
  - [x] 5.1 Add integration tests for cross-file scenarios
    - Test that cross-file definitions are still returned when same-file def is after position
    - Test that same-file definition before position takes precedence over cross-file
    - _Requirements: 3.1, 3.2, 3.3_

- [x] 6. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.
  - Run `cargo test -p raven` to verify no regressions
  - Run `cargo clippy -p raven` to check for style issues

## Notes

- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties
- Unit tests validate specific examples and edge cases
- The existing `find_definition_in_tree` function can be kept for backward compatibility or removed if no longer needed
