# Implementation Plan: User-Defined Function Hover

## Overview

This plan implements hover information for user-defined functions in rlsp. The implementation modifies the existing hover handler to search for user-defined functions before falling back to R help, and adds functions to extract formatted signatures from the tree-sitter AST.

## Tasks

- [ ] 1. Implement signature extraction functions
  - [ ] 1.1 Add `extract_parameters` function to extract parameter strings from a parameters node
    - Parse each parameter child node
    - Handle simple parameters, parameters with defaults, and `...`
    - Return `Vec<String>` of formatted parameter strings
    - _Requirements: 1.2, 1.4, 4.2, 4.3_
  
  - [ ] 1.2 Add `extract_function_signature` function to format complete signature
    - Take function_definition node and function name
    - Call `extract_parameters` for the parameters child
    - Return formatted string `name(param1, param2, ...)`
    - _Requirements: 1.2, 1.3_
  
  - [ ] 1.3 Write property test for parameter extraction completeness
    - **Property 2: Parameter Extraction Completeness**
    - **Validates: Requirements 1.4, 4.2, 4.3**

- [ ] 2. Implement function definition lookup
  - [ ] 2.1 Add `find_function_definition_node` function to locate function definitions in AST
    - Traverse tree looking for binary_operator with function_definition RHS
    - Match function name on LHS identifier
    - Handle all assignment operators (`<-`, `=`, `<<-`)
    - Return the function_definition node if found
    - _Requirements: 4.4_
  
  - [ ] 2.2 Write property test for assignment operator handling
    - **Property 5: Assignment Operator Handling**
    - **Validates: Requirements 4.4**

- [ ] 3. Implement multi-source function search
  - [ ] 3.1 Add `find_user_function_signature` function to search across sources
    - Search current document first
    - Search open documents if not found
    - Search workspace index if still not found
    - Return formatted signature string if found
    - _Requirements: 2.1, 2.2, 2.3, 2.4_
  
  - [ ] 3.2 Write property test for search priority
    - **Property 3: Current Document Search Priority**
    - **Validates: Requirements 2.1, 2.4**

- [ ] 4. Modify hover handler
  - [ ] 4.1 Update `hover` function to try user-defined functions first
    - Call `find_user_function_signature` before R help lookup
    - Return hover with signature in markdown code block if found
    - Fall back to existing R help behavior if not found
    - _Requirements: 1.1, 3.1, 3.2_
  
  - [ ] 4.2 Write property test for user-defined priority over built-ins
    - **Property 4: User-Defined Function Priority Over Built-ins**
    - **Validates: Requirements 3.2**

- [ ] 5. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 6. Add unit tests for signature extraction
  - [ ] 6.1 Write unit tests for basic signature cases
    - Test simple function: `add <- function(a, b) { a + b }` → `add(a, b)`
    - Test no parameters: `get_pi <- function() { 3.14 }` → `get_pi()`
    - Test with defaults: `greet <- function(name = "World") { }` → `greet(name = "World")`
    - Test with dots: `wrapper <- function(...) { }` → `wrapper(...)`
    - _Requirements: 1.2, 1.3, 1.4, 4.3_
  
  - [ ]* 6.2 Write property test for signature format correctness
    - **Property 1: Signature Format Correctness**
    - **Validates: Requirements 1.1, 1.2**

- [ ] 7. Final checkpoint - Verify VS Code extension test passes
  - Run `cargo test -p rlsp` to verify all Rust tests pass
  - The VS Code extension test "hover information is provided" should now pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional and can be skipped for faster MVP
- Each task references specific requirements for traceability
- The implementation follows existing code patterns in handlers.rs
- Property tests use proptest library (already a project dependency)
