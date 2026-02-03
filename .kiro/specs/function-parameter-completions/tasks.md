# Implementation Plan: Function Parameter and Dollar-Sign Completions

## Overview

This implementation adds function parameter completions and dollar-sign completions to Raven's LSP. The work is organized into incremental tasks that build on each other, with property tests validating correctness at each stage.

## Tasks

- [ ] 1. Create completion context detection module
  - [ ] 1.1 Create `crates/raven/src/completion_context.rs` with `CompletionContext` enum
    - Define `FunctionCall`, `DollarSign`, and `Standard` variants
    - Include fields for function name, namespace, existing params, object name, prefix
    - _Requirements: 1.1, 1.2, 1.3, 6.1, 6.2_
  
  - [ ] 1.2 Implement `detect_completion_context()` function
    - Walk AST to find enclosing function call or dollar-sign operator
    - Handle nested function calls by finding innermost
    - Extract existing parameter names from function call arguments
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 6.1, 6.2_
  
  - [ ] 1.3 Write property test for function call context detection
    - **Property 1: Function Call Context Detection**
    - **Validates: Requirements 1.1, 1.2, 1.4**
  
  - [ ] 1.4 Write property test for nested function call resolution
    - **Property 2: Nested Function Call Resolution**
    - **Validates: Requirements 1.3**

- [ ] 2. Checkpoint - Ensure context detection tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 3. Implement parameter resolver module
  - [ ] 3.1 Create `crates/raven/src/parameter_resolver.rs` with data structures
    - Define `FunctionSignature`, `ParameterInfo`, `SignatureSource` structs
    - Define `SignatureCache` with thread-safe storage
    - _Requirements: 9.1, 9.4_
  
  - [ ] 3.2 Implement AST-based parameter extraction for user-defined functions
    - Extract parameters from `function_definition` nodes
    - Handle default values and dots parameter
    - Exclude dots from completion results
    - _Requirements: 4.1, 4.2, 4.3, 4.4_
  
  - [ ] 3.3 Write property test for parameter extraction round-trip
    - **Property 3: Parameter Extraction Round-Trip**
    - **Validates: Requirements 4.1, 4.2**
  
  - [ ] 3.4 Write property test for dots parameter exclusion
    - **Property 7: Dots Parameter Exclusion**
    - **Validates: Requirements 4.4**

- [ ] 4. Extend R subprocess for function formals queries
  - [ ] 4.1 Add `get_function_formals()` method to `RSubprocess`
    - Query R using `formals(func)` or `formals(pkg::func)`
    - Parse output into `Vec<ParameterInfo>`
    - Validate function names to prevent injection
    - _Requirements: 10.1, 10.3, 10.5_
  
  - [ ] 4.2 Write property test for R subprocess input validation
    - **Property 12: R Subprocess Input Validation**
    - **Validates: Requirements 10.3**

- [ ] 5. Checkpoint - Ensure parameter resolver tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 6. Implement full parameter resolution flow
  - [ ] 6.1 Implement `ParameterResolver` with resolution priority
    - Try user-defined functions first (current file, then cross-file scope)
    - Fall back to package functions via R subprocess
    - Use cache for repeated queries
    - _Requirements: 2.1, 2.2, 3.1, 3.2, 3.3_
  
  - [ ] 6.2 Write property test for cache consistency
    - **Property 5: Cache Consistency**
    - **Validates: Requirements 2.5, 3.4, 7.5**
  
  - [ ] 6.3 Write property test for default value preservation
    - **Property 4: Default Value Preservation**
    - **Validates: Requirements 2.3, 4.3, 5.2**

- [ ] 7. Implement dollar-sign resolver module
  - [ ] 7.1 Create `crates/raven/src/dollar_resolver.rs` with data structures
    - Define `ObjectMembers`, `MemberSource`, `DatasetCache` structs
    - _Requirements: 7.1, 8.1_
  
  - [ ] 7.2 Implement AST-based member extraction
    - Extract column names from `data.frame()` calls
    - Extract member names from `list()` calls
    - Track column assignments (`df$col <- value`)
    - _Requirements: 7.2, 7.3, 8.1_
  
  - [ ] 7.3 Write property test for data frame column extraction
    - **Property 9: Data Frame Column Extraction**
    - **Validates: Requirements 7.2**
  
  - [ ] 7.4 Write property test for column assignment tracking
    - **Property 10: Column Assignment Tracking**
    - **Validates: Requirements 7.3**
  
  - [ ] 7.5 Write property test for list member extraction
    - **Property 11: List Member Extraction**
    - **Validates: Requirements 8.1**

- [ ] 8. Extend R subprocess for object names queries
  - [ ] 8.1 Add `get_object_names()` method to `RSubprocess`
    - Query R using `names(obj)` for built-in datasets
    - Validate object names to prevent injection
    - _Requirements: 10.2, 10.3_

- [ ] 9. Checkpoint - Ensure dollar resolver tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 10. Integrate with completion handler
  - [ ] 10.1 Add caches to `WorldState`
    - Add `signature_cache: Arc<SignatureCache>` field
    - Add `dataset_cache: Arc<DatasetCache>` field
    - Initialize in `WorldState::new()`
    - _Requirements: 9.1_
  
  - [ ] 10.2 Modify `completion()` function in handlers.rs
    - Call `detect_completion_context()` first
    - Route to appropriate completion handler based on context
    - Maintain existing behavior for `Standard` context
    - _Requirements: 11.1, 11.2, 11.3, 11.4_
  
  - [ ] 10.3 Implement `get_parameter_completions()` function
    - Use `ParameterResolver` to get function signature
    - Filter out already-specified parameters
    - Format completions with equals-space suffix and default value detail
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5_
  
  - [ ] 10.4 Write property test for already-specified parameter exclusion
    - **Property 6: Already-Specified Parameter Exclusion**
    - **Validates: Requirements 5.5**
  
  - [ ] 10.5 Implement `get_dollar_completions()` function
    - Use `DollarResolver` to get object members
    - Filter by prefix
    - Return empty list for unresolvable objects
    - _Requirements: 6.1, 6.2, 6.4, 7.4, 8.3_
  
  - [ ] 10.6 Write property test for dollar-sign context detection
    - **Property 8: Dollar-Sign Context Detection**
    - **Validates: Requirements 6.1, 6.2**

- [ ] 11. Implement cache invalidation
  - [ ] 11.1 Invalidate user-defined function signatures on file change
    - Hook into `did_change` handler
    - Clear signatures for changed file URI
    - _Requirements: 9.2_

- [ ] 12. Implement graceful degradation
  - [ ] 12.1 Handle R subprocess unavailability
    - Fall back to AST-based extraction when R unavailable
    - Return standard completions when signature unknown
    - Log errors at trace level
    - _Requirements: 2.4, 12.1, 12.2, 12.3, 12.4_
  
  - [ ] 12.2 Write property test for graceful degradation
    - **Property 13: Graceful Degradation**
    - **Validates: Requirements 12.1, 12.2**

- [ ] 13. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 14. Wire up module exports
  - [ ] 14.1 Add module declarations to `lib.rs`
    - Add `pub mod completion_context;`
    - Add `pub mod parameter_resolver;`
    - Add `pub mod dollar_resolver;`
  
  - [ ] 14.2 Export public types from modules
    - Export `CompletionContext` and `detect_completion_context`
    - Export `ParameterResolver`, `SignatureCache`, `FunctionSignature`
    - Export `DollarResolver`, `DatasetCache`, `ObjectMembers`

## Notes

- All tasks including property tests are required for comprehensive coverage
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties
- Unit tests validate specific examples and edge cases
