# Implementation Plan: @lsp-source Forward Directive

## Overview

This plan implements the `@lsp-source` forward directive for Raven, adding synonym support (`@lsp-run`, `@lsp-include`), `line=N` parameter parsing, proper path resolution with `@lsp-cd` support, and conflict resolution between directives and AST-detected `source()` calls.

## Tasks

- [ ] 1. Extend directive parsing with synonyms and line parameter
  - [ ] 1.1 Update forward directive regex in `directive.rs`
    - Add `@lsp-run` and `@lsp-include` as synonyms to the regex pattern
    - Add optional `line=N` parameter capture group
    - Pattern: `#\s*@?lsp-(?:source|run|include)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))(?:\s+line\s*=\s*(\d+))?`
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8, 1.9_

  - [ ] 1.2 Update `parse_forward_directives()` to handle line parameter
    - Parse the `line=N` capture group when present
    - Convert from 1-based user input to 0-based internal (N-1)
    - Use directive's own line when no `line=` parameter
    - Set `column=0` for all directive-based sources
    - _Requirements: 2.1, 2.2, 2.3, 2.4_

  - [ ] 1.3 Write property test for directive parsing completeness
    - **Property 1: Forward Directive Parsing Completeness**
    - Generate random valid syntax variations (with/without @, colon, quotes, line=)
    - Verify ForwardSource has correct path and `is_directive=true`
    - **Validates: Requirements 1.1-1.9**

  - [ ] 1.4 Write property test for synonym equivalence
    - **Property 2: Synonym Equivalence**
    - Generate random paths, verify @lsp-source, @lsp-run, @lsp-include produce identical ForwardSource
    - **Validates: Requirements 1.2, 1.3**

  - [ ] 1.5 Write property test for call-site line conversion
    - **Property 3: Call-Site Line Conversion**
    - Generate random line numbers, verify 1-based to 0-based conversion (N → N-1)
    - **Validates: Requirements 2.1**

  - [ ] 1.6 Write property test for default call-site assignment
    - **Property 4: Default Call-Site Assignment**
    - Generate directives without line= at various lines, verify line matches directive position
    - **Validates: Requirements 2.2**

  - [ ] 1.7 Write property test for multiple directive independence
    - **Property 5: Multiple Directive Independence**
    - Generate files with N directives, verify exactly N ForwardSource entries
    - **Validates: Requirements 2.3**

- [ ] 2. Checkpoint - Verify directive parsing
  - Ensure all directive parsing tests pass, ask the user if questions arise.

- [ ] 3. Implement path resolution with @lsp-cd support
  - [ ] 3.1 Verify forward directives use `PathContext::from_metadata()` in `dependency.rs`
    - Ensure forward directive path resolution includes working directory from @lsp-cd
    - Confirm backward directives continue using `PathContext::new()` (no working directory)
    - Add comments clarifying the distinction
    - _Requirements: 3.1, 3.2, 3.4_

  - [ ] 3.2 Handle non-existent files in forward directive processing
    - Skip edge creation when resolved path doesn't exist
    - Store unresolved paths for diagnostic emission
    - _Requirements: 3.3_

  - [ ] 3.3 Write property test for forward directive working directory usage
    - **Property 6: Forward Directive Uses Working Directory**
    - Generate files with @lsp-cd and @lsp-source, verify path resolution uses working directory
    - **Validates: Requirements 3.4**

- [ ] 4. Implement dependency graph conflict resolution
  - [ ] 4.1 Update `update_file()` in `dependency.rs` for directive-vs-AST conflict resolution
    - When directive and source() point to same file at same line: keep directive edge only
    - When directive and source() point to same file at different lines: keep both edges
    - Track which edges are from directives vs AST for conflict detection
    - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5_

  - [ ] 4.2 Write property test for directive edge creation
    - **Property 7: Directive Edge Creation**
    - Generate forward directives to existing files, verify edge has `is_directive=true`, `is_backward_directive=false`
    - **Validates: Requirements 4.1, 4.2**

  - [ ] 4.3 Write property test for same call-site conflict resolution
    - **Property 8: Same Call-Site Conflict Resolution**
    - Generate files with directive and source() at same line, verify single directive edge
    - **Validates: Requirements 4.3**

  - [ ] 4.4 Write property test for different call-site preservation
    - **Property 9: Different Call-Site Preservation**
    - Generate files with directive at line A and source() at line B (A≠B), verify both edges exist
    - **Validates: Requirements 4.4**

- [ ] 5. Checkpoint - Verify dependency graph integration
  - Ensure all dependency graph tests pass, ask the user if questions arise.

- [ ] 6. Verify scope resolution integration
  - [ ] 6.1 Verify existing scope resolution handles forward directive edges
    - Confirm symbols from sourced files are available after directive line
    - Confirm `line=N` parameter affects scope availability position
    - Add integration test for scope resolution with @lsp-source
    - _Requirements: 5.1, 5.2, 5.3_

  - [ ] 6.2 Write property test for scope availability after directive
    - **Property 10: Scope Availability After Directive**
    - Generate files with @lsp-source at line L, verify symbols available after L
    - **Validates: Requirements 5.1, 5.2**

- [ ] 7. Implement diagnostics for forward directives
  - [ ] 7.1 Add missing file diagnostic in `handlers.rs`
    - Emit warning when @lsp-source references non-existent file
    - Use configurable severity from `crossFile.missingFileSeverity`
    - Position diagnostic at directive line
    - _Requirements: 6.1, 6.3_

  - [ ] 7.2 Add optional redundant directive diagnostic
    - Emit hint when directive without line= targets same file as earlier source() call
    - Use configurable severity from `crossFile.redundantDirectiveSeverity`
    - _Requirements: 6.2_

  - [ ] 7.3 Write property test for missing file diagnostic
    - **Property 11: Missing File Diagnostic**
    - Generate forward directives to non-existent files, verify diagnostic emitted at directive line
    - **Validates: Requirements 6.1**

- [ ] 8. Verify revalidation on directive changes
  - [ ] 8.1 Verify existing revalidation handles forward directive changes
    - Confirm adding @lsp-source triggers dependency graph update
    - Confirm removing @lsp-source removes edge and revalidates
    - Confirm modifying @lsp-source path updates graph
    - _Requirements: 7.1, 7.2, 7.3_

  - [ ] 8.2 Write property test for revalidation on directive change
    - **Property 12: Revalidation on Directive Change**
    - Generate directive add/remove/modify operations, verify graph reflects changes
    - **Validates: Requirements 7.1, 7.2, 7.3**

- [ ] 9. Checkpoint - Verify diagnostics and revalidation
  - Ensure all diagnostic and revalidation tests pass, ask the user if questions arise.

- [ ] 10. Update documentation
  - [ ] 10.1 Update AGENTS.md with forward directive path resolution behavior
    - Clarify forward directives (@lsp-source, @lsp-run, @lsp-include) use @lsp-cd
    - Distinguish from backward directives which ignore @lsp-cd
    - Explain rationale: forward directives describe runtime execution like source()
    - _Requirements: 8.1, 8.2, 8.3_

  - [ ] 10.2 Update docs/cross-file.md with @lsp-source documentation
    - Expand Forward Directives section with @lsp-source and synonyms
    - Document full syntax with examples (quotes, colon, line= parameter)
    - Correct note about @lsp-cd to clarify forward directives DO use it
    - Document line=N parameter for explicit call-site specification
    - _Requirements: 8.4, 8.5, 8.6_

- [ ] 11. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- All tasks including property tests are required for comprehensive coverage
- Property tests use proptest and run minimum 100 iterations each
- Key files: `directive.rs`, `dependency.rs`, `path_resolve.rs`, `handlers.rs`, `scope.rs`
- The existing @lsp-source parsing works; this adds synonyms, line= parameter, and explicit @lsp-cd documentation
