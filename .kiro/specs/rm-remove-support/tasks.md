# Implementation Plan: rm()/remove() Support

## Overview

This implementation adds support for tracking variable removals via `rm()` and `remove()` calls. The work is organized into detection, scope integration, cross-file support, and documentation phases.

## Tasks

- [ ] 1. Add Removal event type to scope system
  - [x] 1.1 Add ScopeEvent::Removal variant to scope.rs
    - Add new variant with line, column, and symbols fields
    - Update timeline sorting to include Removal events
    - _Requirements: 1.1, 1.2_
  
  - [x] 1.2 Write unit tests for Removal event creation
    - Test that Removal events are correctly created and sorted in timeline
    - _Requirements: 1.1, 1.2_

- [ ] 2. Implement rm()/remove() call detection
  - [x] 2.1 Create detect_rm_calls function in source_detect.rs
    - Detect calls to rm() and remove() functions
    - Extract bare symbol arguments from positional args
    - Return Vec<RmCall> with position and symbol names
    - _Requirements: 1.1, 1.2, 2.1_
  
  - [x] 2.2 Implement list= argument parsing
    - Handle `list = "name"` (single string literal)
    - Handle `list = c("a", "b", "c")` (character vector)
    - Skip non-literal expressions (variables, function calls)
    - _Requirements: 3.1, 3.2, 3.3, 3.4_
  
  - [x] 2.3 Implement envir= argument filtering
    - Skip rm() calls with non-default envir= argument
    - Allow calls with envir = globalenv() or envir = .GlobalEnv
    - _Requirements: 4.1, 4.2, 4.3_
  
  - [x] 2.4 Write unit tests for rm() detection
    - Test bare symbols: rm(x), rm(x, y, z)
    - Test remove() alias
    - Test list= with strings and c()
    - Test envir= filtering
    - Test edge cases (empty calls, mixed arguments)
    - _Requirements: 1.1, 1.2, 2.1, 3.1, 3.2, 4.1_
  
  - [x] 2.5 Write property test for bare symbol extraction
    - **Property 1: Bare Symbol Extraction**
    - **Validates: Requirements 1.1, 1.2, 1.3**
  
  - [x] 2.6 Write property test for remove() equivalence
    - **Property 2: remove() Equivalence**
    - **Validates: Requirements 2.1, 2.2, 2.3**
  
  - [x] 2.7 Write property test for list= string extraction
    - **Property 3: list= String Literal Extraction**
    - **Validates: Requirements 3.1, 3.2**
  
  - [x] 2.8 Write property test for dynamic expression filtering
    - **Property 4: Dynamic Expression Filtering**
    - **Validates: Requirements 3.3, 3.4**
  
  - [x] 2.9 Write property test for envir= filtering
    - **Property 5: envir= Argument Filtering**
    - **Validates: Requirements 4.1, 4.2, 4.3**

- [x] 3. Checkpoint - Ensure detection tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 4. Integrate rm() detection into scope artifacts
  - [x] 4.1 Modify compute_artifacts to collect rm() calls
    - Call detect_rm_calls and add Removal events to timeline
    - Ensure proper sorting with other event types
    - _Requirements: 1.1, 1.2_
  
  - [x] 4.2 Write integration tests for artifacts with removals
    - Test timeline contains Removal events in correct order
    - Test mixed definitions and removals
    - _Requirements: 1.1, 7.1_

- [ ] 5. Implement scope resolution with removals
  - [x] 5.1 Modify scope_at_position to handle Removal events
    - Remove symbols from scope when processing Removal events
    - Only process removals before the query position
    - _Requirements: 7.3, 7.4_
  
  - [x] 5.2 Implement function scope handling for removals
    - Check if removal is inside a function scope
    - Only apply removal within the same function scope
    - _Requirements: 5.1, 5.2, 5.3_
  
  - [x] 5.3 Update scope_at_position_with_graph_recursive for removals
    - Handle Removal events in the graph-based scope resolution
    - Maintain function scope isolation
    - _Requirements: 5.1, 6.1, 6.2_
  
  - [x] 5.4 Write unit tests for scope resolution with removals
    - Test define-then-remove sequence
    - Test remove-then-define sequence
    - Test define-remove-define sequence
    - Test position-aware queries
    - _Requirements: 7.1, 7.2, 7.3, 7.4_
  
  - [x] 5.5 Write property test for function scope isolation
    - **Property 6: Function Scope Isolation**
    - **Validates: Requirements 5.1, 5.2, 5.3**
  
  - [x] 5.6 Write property test for timeline-based scope resolution
    - **Property 8: Timeline-Based Scope Resolution**
    - **Validates: Requirements 7.1, 7.2, 7.3, 7.4**

- [x] 6. Checkpoint - Ensure scope resolution tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 7. Cross-file scope integration
  - [x] 7.1 Update scope_at_position_with_backward_recursive for removals
    - Handle Removal events in backward directive resolution
    - _Requirements: 6.1, 6.2, 6.3_
  
  - [x] 7.2 Write cross-file integration tests
    - Test source file then remove symbol
    - Test removal affects downstream scope queries
    - _Requirements: 6.1, 6.2_
  
  - [x] 7.3 Write property test for cross-file removal propagation
    - **Property 7: Cross-File Removal Propagation**
    - **Validates: Requirements 6.1, 6.2, 6.3**

- [ ] 8. Documentation
  - [x] 8.1 Update README.md with rm()/remove() support documentation
    - Document supported patterns (bare symbols, list= with literals)
    - Document unsupported patterns (dynamic expressions, envir=)
    - Document limitations
    - _Requirements: 8.1, 8.2, 8.3, 8.4_

- [x] 9. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- All tasks including tests are required for comprehensive coverage
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties
- Unit tests validate specific examples and edge cases
- The implementation follows the existing patterns in source_detect.rs and scope.rs
