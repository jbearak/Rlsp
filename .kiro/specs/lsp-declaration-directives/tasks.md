# Implementation Plan: LSP Declaration Directives

## Overview

This plan implements declaration directives (`@lsp-var`, `@lsp-func` and synonyms) for Raven, enabling users to declare symbols that cannot be statically detected. The implementation extends the existing directive parsing infrastructure in `directive.rs`, adds new metadata fields to `types.rs`, integrates with scope resolution in `scope.rs`, and updates LSP features (diagnostics, completions, hover, go-to-definition).

## Tasks

- [x] 1. Add DeclaredSymbol type and extend CrossFileMetadata
  - [x] 1.1 Add `DeclaredSymbol` struct to `types.rs`
    - Define struct with `name: String`, `line: u32`, `is_function: bool`
    - Derive `Debug, Clone, Serialize, Deserialize, PartialEq, Eq`
    - _Requirements: 3.1, 3.2_
  
  - [x] 1.2 Extend `CrossFileMetadata` with declared symbol fields
    - Add `declared_variables: Vec<DeclaredSymbol>` field
    - Add `declared_functions: Vec<DeclaredSymbol>` field
    - Ensure default values are empty vectors
    - _Requirements: 3.1, 3.2_
  
  - [x] 1.3 Write property test for metadata serialization round-trip
    - **Property 3: Metadata Serialization Round-Trip**
    - **Validates: Requirements 3.3**

- [x] 2. Implement directive parsing for declaration directives
  - [x] 2.1 Add regex patterns for variable declaration directives in `directive.rs`
    - Pattern: `#\s*@lsp-(?:declare-variable|declare-var|variable|var)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))`
    - Handle all 4 synonym forms: `@lsp-declare-variable`, `@lsp-declare-var`, `@lsp-variable`, `@lsp-var`
    - _Requirements: 1.1, 1.2, 1.3_
  
  - [x] 2.2 Add regex patterns for function declaration directives in `directive.rs`
    - Pattern: `#\s*@lsp-(?:declare-function|declare-func|function|func)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))`
    - Handle all 4 synonym forms: `@lsp-declare-function`, `@lsp-declare-func`, `@lsp-function`, `@lsp-func`
    - _Requirements: 2.1, 2.2, 2.3_
  
  - [x] 2.3 Update `parse_directives()` to extract declared symbols
    - Scan content line-by-line for declaration directives
    - Extract symbol name from regex captures (quoted or unquoted)
    - Record 0-based line number for each directive
    - Populate `declared_variables` and `declared_functions` in metadata
    - Skip directives with empty/whitespace-only symbol names
    - _Requirements: 1.4, 1.5, 2.4, 2.5, 3.4_
  
  - [x] 2.4 Write property test for directive parsing completeness
    - **Property 1: Directive Parsing Completeness**
    - **Validates: Requirements 1.1, 1.2, 1.3, 1.4, 1.5, 2.1, 2.2, 2.3, 2.4, 2.5**
  
  - [x] 2.5 Write property test for required @ prefix
    - **Property 2: Required @ Prefix**
    - **Validates: Requirements 1.6, 2.6**

- [x] 3. Checkpoint - Ensure directive parsing tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 4. Integrate declared symbols into scope resolution
  - [x] 4.1 Add `ScopeEvent::Declaration` variant to `scope.rs`
    - Add variant with `line: u32`, `column: u32`, `symbol: ScopedSymbol`
    - _Requirements: 4.5_
  
  - [x] 4.2 Update `compute_artifacts()` to create Declaration events from metadata
    - Convert `DeclaredSymbol` entries from metadata to `ScopeEvent::Declaration`
    - Set column to `u32::MAX` (end-of-line sentinel) so symbol is available from line+1
    - Create `ScopedSymbol` with appropriate `SymbolKind` (Function or Variable)
    - Insert events in timeline at correct position (by line number)
    - _Requirements: 4.3, 4.4, 4.5, 4.6_
  
  - [x] 4.3 Update `scope_at_position()` to include declared symbols
    - Process `ScopeEvent::Declaration` events in timeline traversal
    - Include declared symbol in scope if event line <= query line
    - Exclude declared symbol if event line > query line
    - _Requirements: 4.1, 4.2_
  
  - [x] 4.4 Write property test for position-aware scope inclusion
    - **Property 4: Position-Aware Scope Inclusion**
    - Verify symbol on line N is NOT available on line N, but IS available on line N+1
    - **Validates: Requirements 4.1, 4.2, 4.3, 4.4, 4.5, 4.6**

- [x] 5. Update interface hash computation
  - [x] 5.1 Include declared symbols in `compute_interface_hash()` in `scope.rs`
    - Sort declared symbols by name for deterministic hashing
    - Include symbol name and kind (function/variable) in hash
    - _Requirements: 10.1, 10.2, 10.3, 10.4_
  
  - [x] 5.2 Write property test for interface hash sensitivity
    - **Property 8: Interface Hash Sensitivity**
    - **Validates: Requirements 10.1, 10.2, 10.3, 10.4**

- [x] 6. Checkpoint - Ensure scope resolution tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 7. Implement diagnostic suppression for declared symbols
  - [x] 7.1 Update undefined variable diagnostic collection in `handlers.rs`
    - Check if symbol name matches any declared variable or function in scope
    - Suppress diagnostic if declared symbol is in scope at usage position
    - Maintain case-sensitive matching
    - _Requirements: 5.1, 5.2, 5.3, 5.4_
  
  - [x] 7.2 Write property test for diagnostic suppression
    - **Property 5: Diagnostic Suppression**
    - **Validates: Requirements 5.1, 5.2, 5.3, 5.4**

- [x] 8. Implement completion support for declared symbols
  - [x] 8.1 Update completion handler in `handlers.rs` to include declared symbols
    - Add declared variables with `CompletionItemKind::VARIABLE`
    - Add declared functions with `CompletionItemKind::FUNCTION`
    - Only include symbols in scope at completion position
    - _Requirements: 6.1, 6.2, 6.3, 6.4_
  
  - [x] 8.2 Write property test for completion inclusion with correct kind
    - **Property 6: Completion Inclusion with Correct Kind**
    - **Validates: Requirements 6.1, 6.2, 6.3, 6.4**

- [x] 9. Implement hover support for declared symbols
  - [x] 9.1 Update hover handler in `handlers.rs` for declared symbols
    - Detect when hover target is a declared symbol
    - Return hover content indicating symbol was declared via directive
    - Include directive line number in hover content
    - _Requirements: 7.1, 7.2, 7.3_

- [x] 10. Implement go-to-definition for declared symbols
  - [x] 10.1 Update go-to-definition handler in `handlers.rs` for declared symbols
    - Detect when definition target is a declared symbol
    - Return location pointing to directive line (column 0)
    - Use first declaration if symbol declared multiple times
    - _Requirements: 8.1, 8.2_

- [x] 11. Checkpoint - Ensure LSP feature tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 12. Implement cross-file declaration inheritance
  - [x] 12.1 Update cross-file scope traversal in `scope.rs` for declared symbols
    - Include declared symbols from parent files in child scope
    - Respect position ordering: only include declarations before source() call
    - Follow same inheritance rules as regular symbols
    - _Requirements: 9.1, 9.2, 9.3_
  
  - [x] 12.2 Write property test for cross-file declaration inheritance
    - **Property 7: Cross-File Declaration Inheritance**
    - Include tests with `local=TRUE` source() calls
    - **Validates: Requirements 9.1, 9.2, 9.3, 9.4**

- [x] 13. Handle conflicting declaration kinds
  - [x] 13.1 Update scope resolution to handle same symbol declared as both variable and function
    - Later declaration (by line number) determines symbol kind for completions/hover
    - First declaration used for go-to-definition
    - Diagnostic suppression applies regardless of kind
    - _Requirements: 11.1, 11.2, 11.3, 11.4_

  - [x] 13.2 Write property test for conflicting declaration resolution
    - **Property 9: Conflicting Declaration Resolution**
    - **Validates: Requirements 11.1, 11.2, 11.3, 11.4**

- [x] 14. Implement workspace index integration for declarations
  - [x] 14.1 Update workspace indexer to extract declared symbols from indexed files
    - Extract declarations when indexing closed files
    - Store in workspace index alongside other metadata
    - _Requirements: 12.1_

  - [x] 14.2 Update scope resolution to include declarations from indexed files
    - When dependency chain includes indexed (closed) file, include its declarations
    - Re-extract from live content when file is opened
    - _Requirements: 12.2, 12.3_

  - [x] 14.3 Write property test for workspace index declaration extraction
    - **Property 10: Workspace Index Declaration Extraction**
    - **Validates: Requirements 12.1, 12.2, 12.3**

- [x] 15. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties
- Unit tests validate specific examples and edge cases
- Implementation follows existing patterns in `directive.rs` and `scope.rs`
- Declaration events use `u32::MAX` column (end-of-line sentinel) to ensure symbols are available from line+1, matching source() semantics
- Conflicting declaration kinds (same symbol as both variable and function) use "later wins for kind, first wins for definition location" rule
