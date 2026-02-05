# Implementation Plan: LSP Declaration Directives

## Overview

This plan implements declaration directives (`@lsp-var`, `@lsp-func` and synonyms) for Raven, enabling users to declare symbols that cannot be statically detected. The implementation extends the existing directive parsing infrastructure, adds new metadata fields, integrates with scope resolution, and updates LSP features (diagnostics, completions, hover, go-to-definition).

## Tasks

- [ ] 1. Add DeclaredSymbol type and extend CrossFileMetadata
  - [ ] 1.1 Add `DeclaredSymbol` struct to `types.rs`
    - Define struct with `name: String`, `line: u32`, `is_function: bool`
    - Derive `Debug, Clone, Serialize, Deserialize, PartialEq, Eq`
    - _Requirements: 3.1, 3.2_
  
  - [ ] 1.2 Extend `CrossFileMetadata` with declared symbol fields
    - Add `declared_variables: Vec<DeclaredSymbol>` field
    - Add `declared_functions: Vec<DeclaredSymbol>` field
    - Ensure default values are empty vectors
    - _Requirements: 3.1, 3.2, 3.3_
  
  - [ ] 1.3 Write property test for metadata serialization round-trip
    - **Property 3: Metadata Serialization Round-Trip**
    - **Validates: Requirements 3.3**

- [ ] 2. Implement directive parsing for declaration directives
  - [ ] 2.1 Add regex patterns for variable declaration directives
    - Pattern: `#\s*@lsp-(?:declare-variable|declare-var|variable|var)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))`
    - Handle all 4 synonym forms
    - _Requirements: 1.1, 1.2, 1.3_
  
  - [ ] 2.2 Add regex patterns for function declaration directives
    - Pattern: `#\s*@lsp-(?:declare-function|declare-func|function|func)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))`
    - Handle all 4 synonym forms
    - _Requirements: 2.1, 2.2, 2.3_
  
  - [ ] 2.3 Update `parse_directives()` to extract declared symbols
    - Scan content line-by-line for declaration directives
    - Extract symbol name from regex captures (quoted or unquoted)
    - Record 0-based line number for each directive
    - Populate `declared_variables` and `declared_functions` in metadata
    - Skip directives with empty/whitespace-only symbol names
    - _Requirements: 1.4, 1.5, 2.4, 2.5, 3.4_
  
  - [ ] 2.4 Write property test for directive parsing completeness
    - **Property 1: Directive Parsing Completeness**
    - **Validates: Requirements 1.1, 1.2, 1.3, 1.4, 1.5, 2.1, 2.2, 2.3, 2.4, 2.5**
  
  - [ ] 2.5 Write property test for required @ prefix
    - **Property 2: Required @ Prefix**
    - **Validates: Requirements 1.6, 2.6**

- [ ] 3. Checkpoint - Ensure directive parsing tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 4. Integrate declared symbols into scope resolution
  - [ ] 4.1 Add `ScopeEvent::Declaration` variant to `scope.rs`
    - Add variant with `line: u32`, `column: u32`, `symbol: ScopedSymbol`
    - _Requirements: 4.5_
  
  - [ ] 4.2 Update `compute_artifacts()` to create Declaration events
    - Convert `DeclaredSymbol` entries from metadata to `ScopeEvent::Declaration`
    - Set column to 0 for all declaration events
    - Create `ScopedSymbol` with appropriate `SymbolKind` (Function or Variable)
    - Insert events in timeline at correct position (by line number)
    - _Requirements: 4.3, 4.4, 4.5_
  
  - [ ] 4.3 Update `scope_at_position()` to include declared symbols
    - Process `ScopeEvent::Declaration` events in timeline traversal
    - Include declared symbol in scope if event line <= query line
    - Exclude declared symbol if event line > query line
    - _Requirements: 4.1, 4.2_
  
  - [ ] 4.4 Write property test for position-aware scope inclusion
    - **Property 4: Position-Aware Scope Inclusion**
    - **Validates: Requirements 4.1, 4.2, 4.3, 4.4, 4.5**

- [ ] 5. Update interface hash computation
  - [ ] 5.1 Include declared symbols in `compute_interface_hash()`
    - Sort declared symbols by name for deterministic hashing
    - Include symbol name and kind (function/variable) in hash
    - _Requirements: 10.1, 10.2, 10.3, 10.4_
  
  - [ ] 5.2 Write property test for interface hash sensitivity
    - **Property 8: Interface Hash Sensitivity**
    - **Validates: Requirements 10.1, 10.2, 10.3, 10.4**

- [ ] 6. Checkpoint - Ensure scope resolution tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 7. Implement diagnostic suppression for declared symbols
  - [ ] 7.1 Update undefined variable diagnostic collection
    - Check if symbol name matches any declared variable or function in scope
    - Suppress diagnostic if declared symbol is in scope at usage position
    - Maintain case-sensitive matching
    - _Requirements: 5.1, 5.2, 5.3, 5.4_
  
  - [ ] 7.2 Write property test for diagnostic suppression
    - **Property 5: Diagnostic Suppression**
    - **Validates: Requirements 5.1, 5.2, 5.3, 5.4**

- [ ] 8. Implement completion support for declared symbols
  - [ ] 8.1 Update completion handler to include declared symbols
    - Add declared variables with `CompletionItemKind::VARIABLE`
    - Add declared functions with `CompletionItemKind::FUNCTION`
    - Only include symbols in scope at completion position
    - _Requirements: 6.1, 6.2, 6.3, 6.4_
  
  - [ ] 8.2 Write property test for completion inclusion
    - **Property 6: Completion Inclusion with Correct Kind**
    - **Validates: Requirements 6.1, 6.2, 6.3, 6.4**

- [ ] 9. Implement hover support for declared symbols
  - [ ] 9.1 Update hover handler for declared symbols
    - Detect when hover target is a declared symbol
    - Return hover content indicating symbol was declared via directive
    - Include directive line number in hover content
    - _Requirements: 7.1, 7.2, 7.3_

- [ ] 10. Implement go-to-definition for declared symbols
  - [ ] 10.1 Update go-to-definition handler for declared symbols
    - Detect when definition target is a declared symbol
    - Return location pointing to directive line (column 0)
    - Use first declaration if symbol declared multiple times
    - _Requirements: 8.1, 8.2_

- [ ] 11. Checkpoint - Ensure LSP feature tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 12. Implement cross-file declaration inheritance
  - [ ] 12.1 Update cross-file scope traversal for declared symbols
    - Include declared symbols from parent files in child scope
    - Respect position ordering: only include declarations before source() call
    - Follow same inheritance rules as regular symbols
    - _Requirements: 9.1, 9.2, 9.3_
  
  - [ ] 12.2 Write property test for cross-file inheritance
    - **Property 7: Cross-File Declaration Inheritance**
    - **Validates: Requirements 9.1, 9.2, 9.3**

- [ ] 13. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- All tasks including property tests are required
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties
- Unit tests validate specific examples and edge cases
- Implementation follows existing patterns in `directive.rs` and `scope.rs`
