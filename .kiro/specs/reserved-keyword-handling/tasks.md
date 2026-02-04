# Implementation Plan: Reserved Keyword Handling

## Overview

This implementation adds proper handling of R reserved words to the Raven LSP. The approach is to create a centralized `reserved_words` module and integrate it into four components: definition extraction, undefined variable checking, completion generation, and document symbol collection.

## Tasks

- [x] 1. Create Reserved Word Module
  - [x] 1.1 Create `crates/raven/src/reserved_words.rs` with `RESERVED_WORDS` constant and `is_reserved_word()` function
    - Define constant array with all 19 reserved words
    - Implement `is_reserved_word()` using `matches!` macro for zero-allocation lookup
    - _Requirements: 1.1, 1.2_
  
  - [x] 1.2 Add module declaration to `crates/raven/src/lib.rs`
    - Add `pub mod reserved_words;` to expose the module
    - _Requirements: 1.3_
  
  - [x] 1.3 Write property test for reserved word identification
    - **Property 1: Reserved Word Identification**
    - **Validates: Requirements 1.1, 1.2**

- [x] 2. Modify Definition Extraction
  - [x] 2.1 Update `try_extract_assignment` in `crates/raven/src/cross_file/scope.rs`
    - Add reserved word check before creating `ScopedSymbol`
    - Return `None` if identifier is a reserved word
    - Covers both left-assignment (`<-`, `=`, `<<-`) and right-assignment (`->`)
    - _Requirements: 2.1, 2.2, 2.3, 2.4_
  
  - [x] 2.2 Write property test for definition extraction exclusion
    - **Property 2: Definition Extraction Exclusion**
    - **Validates: Requirements 2.1, 2.2, 2.3, 2.4**

- [x] 3. Modify Undefined Variable Checker
  - [x] 3.1 Update `collect_undefined_variables_position_aware` in `crates/raven/src/handlers.rs`
    - Add reserved word check at the start of the usage loop
    - Skip reserved words before any other checks (builtins, scope, packages)
    - _Requirements: 3.1, 3.2, 3.3, 3.4_
  
  - [x] 3.2 Write property test for undefined variable check exclusion
    - **Property 3: Undefined Variable Check Exclusion**
    - **Validates: Requirements 3.1, 3.2, 3.3**

- [x] 4. Checkpoint - Core functionality complete
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. Modify Completion Provider
  - [x] 5.1 Update `completion` function in `crates/raven/src/handlers.rs`
    - Filter reserved words from identifier completions after aggregation
    - Preserve keyword completions (CompletionItemKind::KEYWORD)
    - _Requirements: 5.1, 5.2, 5.3_
  
  - [x] 5.2 Write property test for completion exclusion
    - **Property 4: Completion Exclusion**
    - **Validates: Requirements 5.1, 5.2, 5.3**

- [x] 6. Modify Document Symbol Provider
  - [x] 6.1 Update `collect_symbols` function in `crates/raven/src/handlers.rs`
    - Add reserved word check before adding symbol to list
    - Skip reserved words but continue recursion
    - _Requirements: 6.1, 6.2_
  
  - [x] 6.2 Write property test for document symbol exclusion
    - **Property 5: Document Symbol Exclusion**
    - **Validates: Requirements 6.1, 6.2**

- [x] 7. Final checkpoint - All tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties
- Unit tests validate specific examples and edge cases
- The `reserved_words` module uses `matches!` macro for zero-allocation, inlineable lookup
