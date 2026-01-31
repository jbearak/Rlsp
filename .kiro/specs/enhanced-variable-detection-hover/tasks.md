# Implementation Plan: Enhanced Variable Detection and Hover Information

## Overview

This implementation plan breaks down the feature into discrete coding tasks. Each task builds on previous work and includes references to specific requirements. The plan focuses on extending Rlsp's scope resolution to detect loop iterators and function parameters, then enhancing hover information to show definition statements with hyperlinked file locations.

## Tasks

- [x] 1. Add loop iterator detection to scope resolution
  - [x] 1.1 Implement `try_extract_for_loop_iterator()` function in `scope.rs`
    - Parse tree-sitter `for_statement` nodes
    - Extract iterator variable from the `variable` field
    - Create `ScopedSymbol` for the iterator at the for statement position
    - Handle UTF-16 column conversion for iterator position
    - _Requirements: 1.1, 1.5, 6.4_
  
  - [x] 1.2 Write property test for loop iterator detection
    - **Property 1: Loop iterator scope inclusion**
    - **Validates: Requirements 1.1, 6.1**
  
  - [x] 1.3 Integrate loop iterator extraction into `collect_definitions()`
    - Add check for `for_statement` node kind
    - Call `try_extract_for_loop_iterator()` for for loops
    - Add iterator as `Def` event to timeline (not a special scope)
    - Add iterator to exported interface
    - _Requirements: 1.1, 6.1_
  
  - [x] 1.4 Write unit tests for loop iterator extraction
    - Test simple for loop iterator extraction
    - Test nested for loops with multiple iterators
    - Test iterator shadowing of outer variables
    - _Requirements: 1.1, 1.3, 1.4_

- [x] 2. Add function parameter detection to scope resolution
  - [x] 2.1 Implement `try_extract_function_scope()` function in `scope.rs`
    - Parse tree-sitter `function_definition` nodes
    - Extract parameters from `parameters` field
    - Create `ScopedSymbol` for each parameter
    - Determine function body boundaries (start/end positions)
    - Return `FunctionScope` event with parameters and boundaries
    - _Requirements: 8.1, 8.2, 8.3, 8.4_
  
  - [x] 2.2 Write property test for function parameter detection
    - **Property 8: Function parameter scope inclusion**
    - **Validates: Requirements 8.1**
  
  - [x] 2.3 Add `FunctionScope` variant to `ScopeEvent` enum
    - Add fields: start_line, start_column, end_line, end_column, parameters
    - Update all match statements handling `ScopeEvent`
    - _Requirements: 7.4, 7.5_
  
  - [x] 2.4 Integrate function scope extraction into `collect_definitions()`
    - Add check for `function_definition` node kind
    - Call `try_extract_function_scope()` for functions
    - Add `FunctionScope` event to timeline
    - _Requirements: 8.1, 8.4_
  
  - [x] 2.5 Write unit tests for function parameter extraction
    - Test function with multiple parameters
    - Test function with default parameter values
    - Test function with ellipsis (...) parameter
    - Test function with no parameters
    - _Requirements: 8.1, 8.2, 8.3_

- [x] 3. Implement function scope boundary handling
  - [x] 3.1 Extend `scope_at_position_with_graph_recursive()` to handle `FunctionScope` events
    - Check if query position is within function body boundaries
    - If inside: add parameters to scope, track function-local definitions
    - If outside: exclude function-local symbols and parameters
    - Maintain symbol precedence (local > inherited)
    - _Requirements: 7.1, 7.2, 7.4_
  
  - [x] 3.2 Write property test for function scope boundaries
    - **Property 5: Function-local variable scope boundaries**
    - **Validates: Requirements 7.1**
  
  - [x] 3.3 Write property test for function parameter scope boundaries
    - **Property 6: Function parameter scope boundaries**
    - **Validates: Requirements 7.2**
  
  - [x] 3.4 Write unit tests for function scope resolution
    - Test parameters available inside function
    - Test parameters not available outside function
    - Test local variables not available outside function
    - Test nested functions with separate scopes
    - _Requirements: 7.1, 7.2, 8.1_

- [x] 4. Checkpoint - Ensure scope resolution tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. Implement definition statement extraction
  - [x] 5.1 Create `DefinitionInfo` struct in `handlers.rs`
    - Add fields: statement (String), source_uri (Url), line (u32), column (u32)
    - _Requirements: 2.1, 2.2_
  
  - [x] 5.2 Implement `extract_definition_statement()` function
    - Accept symbol, content provider, and tree provider closures
    - Get source file content and tree for the symbol's URI
    - Find tree-sitter node at the definition position
    - Extract complete statement based on symbol kind
    - Handle multi-line definitions with 10-line truncation
    - Preserve original indentation
    - _Requirements: 2.1, 2.2, 2.3, 2.4, 5.3, 5.4_
  
  - [x] 5.3 Implement statement extraction for different symbol types
    - Variables: extract complete assignment statement
    - Functions: extract signature and opening brace
    - Loop iterators: extract for loop header
    - Function parameters: extract function signature
    - Handle all R assignment operators: `<-`, `=`, `<<-`, `->`
    - _Requirements: 10.1, 10.2, 10.3, 10.4, 10.5_
  
  - [x] 5.4 Write unit tests for definition extraction
    - Test variable assignment extraction
    - Test function definition extraction
    - Test multi-line definition truncation
    - Test indentation preservation
    - Test all assignment operators
    - _Requirements: 2.1, 2.2, 2.4, 5.3, 5.4, 10.1, 10.2, 10.3, 10.4_

- [x] 6. Implement path utilities for hover
  - [x] 6.1 Implement `compute_relative_path()` function
    - Accept target URI and optional workspace root
    - Compute relative path from workspace root to target
    - If no workspace root, return filename only
    - Handle edge cases (target outside workspace, etc.)
    - _Requirements: 3.4_
  
  - [x] 6.2 Implement `escape_markdown()` function
    - Escape markdown special characters: *, _, [, ], (, ), #, `, \
    - Preserve code readability while ensuring safe rendering
    - _Requirements: 5.5_
  
  - [x] 6.3 Write unit tests for path utilities
    - Test relative path calculation with workspace root
    - Test relative path without workspace root
    - Test markdown character escaping
    - _Requirements: 3.4, 5.5_

- [x] 7. Enhance hover provider with definition statements
  - [x] 7.1 Modify `hover()` function in `handlers.rs` to extract definition statements
    - After finding symbol in cross-file scope, call `extract_definition_statement()`
    - Format definition as R code block with syntax highlighting
    - Add blank line separator between statement and location
    - Escape markdown special characters in definition
    - _Requirements: 2.1, 2.2, 2.5, 5.1, 5.2, 5.5_
  
  - [x] 7.2 Add file location formatting to hover content
    - For same-file definitions: format as "this file, line N" (1-based)
    - For cross-file definitions: format as `[relative_path](file:///absolute_path), line N`
    - Use file:// protocol with absolute paths
    - Compute relative paths from workspace root
    - _Requirements: 3.1, 3.2, 3.3, 3.4_
  
  - [x] 7.3 Ensure hover uses LSP MarkupKind::Markdown
    - Verify all hover responses use MarkupContent with MarkupKind::Markdown
    - _Requirements: 3.5_
  
  - [x] 7.4 Write unit tests for enhanced hover
    - Test hover shows definition statement
    - Test same-file location format
    - Test cross-file hyperlink format
    - Test markdown code block formatting
    - Test blank line separator
    - _Requirements: 2.1, 2.5, 3.1, 3.2, 5.1, 5.2_

- [x] 8. Handle cross-file hover resolution
  - [x] 8.1 Ensure hover uses existing cross-file scope resolution
    - Verify `get_cross_file_symbols()` is called correctly
    - Ensure dependency graph is used for definition lookup
    - Handle multiple definitions with scope-based selection
    - _Requirements: 4.1, 4.2, 4.4_
  
  - [x] 8.2 Add graceful fallback for missing definitions
    - When definition cannot be located, show symbol type without statement
    - Handle built-in functions gracefully
    - Handle undefined symbols gracefully
    - _Requirements: 4.3_
  
  - [x] 8.3 Write integration tests for cross-file hover
    - Test hover on symbol defined in sourced file
    - Test hover with multiple definitions (shadowing)
    - Test hover on built-in function (fallback)
    - _Requirements: 4.1, 4.2, 4.3_

- [x] 9. Checkpoint - Ensure hover tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 10. Handle source() local parameter in scope resolution
  - [x] 10.1 Verify existing `local` flag handling in scope resolution
    - Check that `ForwardSource.local` flag is respected
    - Verify `ForwardSource.inherits_symbols()` is used correctly
    - Ensure local=TRUE sources don't add symbols to global scope
    - _Requirements: 9.1, 9.2, 9.5_
  
  - [x] 10.2 Verify default local=FALSE behavior
    - Ensure source() calls without explicit local parameter default to FALSE
    - Verify symbols are globally available for local=FALSE
    - _Requirements: 9.4_
  
  - [x] 10.3 Write unit tests for source() local parameter
    - Test source() with local=FALSE (global scope)
    - Test source() with local=TRUE inside function (function scope)
    - Test source() without local parameter (defaults to FALSE)
    - _Requirements: 9.1, 9.2, 9.4_

- [x] 11. Add property tests for scope resolution invariants
  - [x] 11.1 Write property test for loop iterator persistence
    - **Property 2: Loop iterator persistence after loop**
    - **Validates: Requirements 6.2**
  
  - [x] 11.2 Write property test for nested loop iterators
    - **Property 3: Nested loop iterator tracking**
    - **Validates: Requirements 1.3**
  
  - [x] 11.3 Write property test for loop iterator shadowing
    - **Property 4: Loop iterator shadowing**
    - **Validates: Requirements 1.4, 6.3**
  
  - [x] 11.4 Write property test for function-local undefined diagnostics
    - **Property 7: Function-local undefined variable diagnostics**
    - **Validates: Requirements 7.3**
  
  - [x] 11.5 Write property test for function parameter with defaults
    - **Property 9: Function parameter with default value recognition**
    - **Validates: Requirements 8.2**

- [x] 12. Add property tests for hover formatting
  - [x] 12.1 Write property test for variable hover extraction
    - **Property 10: Variable hover definition extraction**
    - **Validates: Requirements 2.1**
  
  - [x] 12.2 Write property test for function hover extraction
    - **Property 11: Function hover signature extraction**
    - **Validates: Requirements 2.2**
  
  - [x] 12.3 Write property test for multi-line definition handling
    - **Property 12: Multi-line definition handling**
    - **Validates: Requirements 2.4**
  
  - [x] 12.4 Write property test for markdown formatting
    - **Property 13: Markdown code block formatting**
    - **Validates: Requirements 2.5, 5.1**
  
  - [x] 12.5 Write property test for file location formats
    - **Property 14: Same-file location format**
    - **Property 15: Cross-file hyperlink format**
    - **Validates: Requirements 3.1, 3.2**

- [x] 13. Add property tests for hover content properties
  - [x] 13.1 Write property test for URI protocol
    - **Property 16: File URI protocol**
    - **Validates: Requirements 3.3**
  
  - [x] 13.2 Write property test for relative path calculation
    - **Property 17: Relative path calculation**
    - **Validates: Requirements 3.4**
  
  - [x] 13.3 Write property test for LSP markup kind
    - **Property 18: LSP Markdown markup kind**
    - **Validates: Requirements 3.5**
  
  - [x] 13.4 Write property test for cross-file resolution
    - **Property 19: Cross-file definition resolution**
    - **Validates: Requirements 4.1**
  
  - [x] 13.5 Write property test for scope-based selection
    - **Property 20: Scope-based definition selection**
    - **Validates: Requirements 4.2**

- [x] 14. Add property tests for hover formatting details
  - [x] 14.1 Write property test for statement/location separation
    - **Property 21: Definition statement and location separation**
    - **Validates: Requirements 5.2**
  
  - [x] 14.2 Write property test for truncation
    - **Property 22: Definition statement truncation**
    - **Validates: Requirements 5.3**
  
  - [x] 14.3 Write property test for indentation preservation
    - **Property 23: Indentation preservation**
    - **Validates: Requirements 5.4**
  
  - [x] 14.4 Write property test for markdown escaping
    - **Property 24: Markdown character escaping**
    - **Validates: Requirements 5.5**

- [x] 15. Add property tests for source() scoping
  - [x] 15.1 Write property test for source local=FALSE
    - **Property 25: Source local=FALSE global scope**
    - **Validates: Requirements 9.1**
  
  - [x] 15.2 Write property test for source local=TRUE
    - **Property 26: Source local=TRUE function scope**
    - **Validates: Requirements 9.2**
  
  - [x] 15.3 Write property test for source local default
    - **Property 27: Source local parameter default**
    - **Validates: Requirements 9.4**

- [x] 16. Add property tests for definition extraction
  - [x] 16.1 Write property test for assignment operators
    - **Property 28: Assignment operator extraction**
    - **Validates: Requirements 10.1, 10.5**
  
  - [x] 16.2 Write property test for inline functions
    - **Property 29: Inline function extraction**
    - **Validates: Requirements 10.2**
  
  - [x] 16.3 Write property test for loop iterator extraction
    - **Property 30: Loop iterator definition extraction**
    - **Validates: Requirements 10.3**
  
  - [x] 16.4 Write property test for parameter extraction
    - **Property 31: Function parameter definition extraction**
    - **Validates: Requirements 10.4**

- [x] 17. Final checkpoint - Integration testing
  - [x] 17.1 Write integration test for complete workflow
    - Parse R file with for loops and functions
    - Verify scope resolution includes iterators and parameters
    - Verify no false-positive undefined variable diagnostics
    - Hover over symbols and verify definition statements
    - Verify hyperlinked file locations
    - _Requirements: All_
  
  - [x] 17.2 Test with realistic R code patterns
    - Nested for loops with multiple iterators
    - Functions with multiple parameters and local variables
    - Cross-file source() calls with local=TRUE/FALSE
    - Multi-line function definitions
    - Code with markdown special characters
    - _Requirements: All_

- [x] 18. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties (minimum 100 iterations each)
- Unit tests validate specific examples and edge cases
- The implementation leverages existing Rlsp infrastructure (tree-sitter, cross-file resolution, caching)
