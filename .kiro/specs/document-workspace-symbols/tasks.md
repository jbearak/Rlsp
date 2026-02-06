# Implementation Plan: Document and Workspace Symbols Enhancement

## Overview

This implementation plan transforms Raven's document symbol and workspace symbol providers from flat `SymbolInformation[]` responses to hierarchical `DocumentSymbol[]` structures with proper range computation, R code section support, richer SymbolKind mapping, and improved workspace symbol search.

## Tasks

- [x] 1. Add DocumentSymbolKind enum and configuration
  - [x] 1.1 Create `DocumentSymbolKind` enum in `handlers.rs` with variants: Function, Variable, Constant, Class, Method, Interface, Module
    - Add `to_lsp_kind()` method for LSP SymbolKind conversion
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5, 5.6, 5.7_
  - [x] 1.2 Add `SymbolConfig` struct to `state.rs` with `workspace_max_results: usize` field (default 1000)
    - Add to `CrossFileConfig` or create separate config struct
    - _Requirements: 11.1, 11.2, 11.3_
  - [x] 1.3 Parse `symbols.workspaceMaxResults` from LSP initialization options in `backend.rs`
    - Validate range 100-10000, clamp to boundaries
    - _Requirements: 11.1, 11.2, 11.3_

- [x] 2. Implement RawSymbol and SymbolExtractor
  - [x] 2.1 Create `RawSymbol` struct with fields: name, kind, range, selection_range, detail, section_level
    - _Requirements: 2.1, 2.2, 6.1_
  - [x] 2.2 Implement `SymbolExtractor::extract_assignments()` to collect function and variable assignments
    - Compute full range (start of assignment to end of RHS)
    - Compute selection_range (identifier only)
    - _Requirements: 2.1, 2.2, 2.3_
  - [x] 2.3 Implement `SymbolExtractor::classify_symbol()` for symbol kind determination
    - ALL_CAPS pattern detection for CONSTANT
    - R6Class/setRefClass detection for CLASS
    - Default FUNCTION/VARIABLE classification
    - _Requirements: 5.1, 5.2, 5.6, 5.7_
  - [x] 2.4 Implement `SymbolExtractor::extract_signature()` for function parameter extraction
    - Format as `(param1, param2, ...)`
    - Truncate at 60 characters with `...`
    - _Requirements: 6.1, 6.2, 6.3_
  - [x] 2.5 Write property test for range containment invariant
    - **Property 3: Range Containment Invariant**
    - **Validates: Requirements 2.1, 2.2, 2.3**
  - [x] 2.6 Write property test for symbol kind classification
    - **Property 6: Symbol Kind Classification**
    - **Validates: Requirements 5.1, 5.2, 5.3, 5.4, 5.5, 5.6, 5.7**

- [x] 3. Implement S4 method detection
  - [x] 3.1 Implement `SymbolExtractor::extract_s4_methods()` to detect setMethod, setClass, setGeneric calls
    - Extract method name from first string argument
    - Assign appropriate SymbolKind (METHOD, CLASS, INTERFACE)
    - _Requirements: 5.3, 5.4, 5.5, 10.1, 10.2, 10.3_
  - [x] 3.2 Write property test for S4 name extraction
    - **Property 13: S4 Name Extraction**
    - **Validates: Requirements 10.1, 10.2, 10.3**

- [x] 4. Implement R code section detection
  - [x] 4.1 Implement `SymbolExtractor::extract_sections()` to detect section comments
    - Match pattern `^\s*#(#*)\s*(%%)?\s*(\S.+?)\s*(#{4,}|\-{4,}|={4,}|\*{4,}|\+{4,})\s*$`
    - Extract section name and heading level (# count)
    - Create RawSymbol with kind=Module and section_level set
    - _Requirements: 4.1, 4.5_
  - [x] 4.2 Write property test for section detection
    - **Property 5: Section Detection and Nesting**
    - **Validates: Requirements 4.1, 4.2, 4.3, 4.4, 4.5**

- [x] 5. Checkpoint - Ensure symbol extraction works
  - Ensure all tests pass, ask the user if questions arise.

- [x] 6. Implement HierarchyBuilder
  - [x] 6.1 Create `HierarchyBuilder` struct that takes `Vec<RawSymbol>` and line count
    - _Requirements: 3.1, 3.2, 3.3, 4.4_
  - [x] 6.2 Implement `HierarchyBuilder::compute_section_ranges()` to set section ranges
    - Section range spans from comment line to line before next section (or EOF)
    - Section selectionRange is the comment line only
    - _Requirements: 4.2, 4.3_
  - [x] 6.3 Implement `HierarchyBuilder::nest_in_sections()` to nest symbols within sections
    - Symbols within section range become children
    - Nested sections based on heading level
    - _Requirements: 4.4, 4.5_
  - [x] 6.4 Implement `HierarchyBuilder::nest_in_functions()` to nest symbols within function bodies
    - Use position-based containment check
    - Support arbitrary nesting depth
    - _Requirements: 3.1, 3.2, 3.3_
  - [x] 6.5 Implement `HierarchyBuilder::build()` to produce `Vec<DocumentSymbol>`
    - Convert RawSymbol to DocumentSymbol with children
    - _Requirements: 1.1, 3.1, 3.2, 3.3_
  - [x] 6.6 Write property test for hierarchical nesting correctness
    - **Property 4: Hierarchical Nesting Correctness**
    - **Validates: Requirements 3.1, 3.2, 3.3**

- [x] 7. Update document_symbol handler
  - [x] 7.1 Add client capability detection for `hierarchicalDocumentSymbolSupport`
    - Store capability in Backend or pass through handler
    - _Requirements: 1.1, 1.2_
  - [x] 7.2 Update `document_symbol()` to use SymbolExtractor and HierarchyBuilder
    - Return `DocumentSymbolResponse::Nested` when hierarchical support available
    - Return `DocumentSymbolResponse::Flat` as fallback with correct URIs
    - _Requirements: 1.1, 1.2, 1.3_
  - [x] 7.3 Integrate reserved word filtering using `is_reserved_word()`
    - Filter before hierarchy building
    - _Requirements: 7.1_
  - [x] 7.4 Write property test for response type selection
    - **Property 1: Response Type Selection**
    - **Validates: Requirements 1.1, 1.2**
  - [x] 7.5 Write property test for reserved word filtering
    - **Property 8: Reserved Word Filtering**
    - **Validates: Requirements 7.1, 7.2**

- [x] 8. Checkpoint - Ensure document symbols work end-to-end
  - Ensure all tests pass, ask the user if questions arise.

- [x] 9. Update workspace_symbol handler
  - [x] 9.1 Update `workspace_symbol()` to use configurable max results
    - Read from `SymbolConfig.workspace_max_results`
    - _Requirements: 9.2, 11.2_
  - [x] 9.2 Add `containerName` extraction from file URI
    - Extract filename without extension
    - Set on all returned SymbolInformation
    - _Requirements: 8.1, 8.2_
  - [x] 9.3 Ensure reserved word filtering applies to workspace symbols
    - Filter in `collect_workspace_symbols_from_artifacts()`
    - _Requirements: 7.2_
  - [x] 9.4 Write property test for workspace containerName
    - **Property 9: Workspace ContainerName**
    - **Validates: Requirements 8.1, 8.2**
  - [x] 9.5 Write property test for workspace query filtering
    - **Property 10: Workspace Query Filtering**
    - **Validates: Requirements 9.1**
  - [x] 9.6 Write property test for workspace result limiting
    - **Property 11: Workspace Result Limiting**
    - **Validates: Requirements 9.2**
  - [x] 9.7 Write property test for workspace deduplication
    - **Property 12: Workspace Deduplication**
    - **Validates: Requirements 9.3**

- [x] 10. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 11. Update user-facing documentation
  - [x] 11.1 Update `docs/configuration.md` with new `symbols.workspaceMaxResults` setting
    - Document default value (1000), valid range (100-10000), and purpose
  - [x] 11.2 Update `README.md` features section to mention hierarchical document symbols
    - Add R code section support (`# Section ----`) to feature list
    - Mention S4/R6 class detection in outline
  - [x] 11.3 Update `editors/vscode/package.json` with new configuration schema
    - Add `rlsp.symbols.workspaceMaxResults` setting with description and default
  - [x] 11.4 Update `AGENTS.md` with new symbol provider architecture
    - Document SymbolExtractor and HierarchyBuilder components
    - Add to "Extension Guide" if relevant patterns emerge

## Notes

- All tasks are required for comprehensive testing
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties
- Unit tests validate specific examples and edge cases
- The implementation builds on existing tree-sitter parsing infrastructure
- Reserved word filtering already exists in `reserved_words.rs` module
