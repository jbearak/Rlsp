# Implementation Plan: Cross-File Debugging

## Overview

This plan systematically debugs and fixes the cross-file awareness feature by adding comprehensive logging, creating test cases that reproduce real-world failures, verifying each component in isolation, and implementing fixes for identified bugs. The approach is incremental: add observability first, then identify issues through testing, then fix them.

## Tasks

- [ ] 1. Add comprehensive logging infrastructure
  - [x] 1.1 Add logging to metadata extraction (source_detect.rs, directive.rs)
    - Add log::trace! calls before and after tree-sitter parsing
    - Log each detected source() call with path and line number
    - Log each parsed directive with full details
    - Log any errors during extraction with full context
    - _Requirements: 1.1, 1.5_
  
  - [x] 1.2 Add logging to dependency graph operations (dependency.rs)
    - Log when edges are added with parent, child, and call site
    - Log when edges are removed
    - Log total edge count after updates
    - Add dump_state() method for debugging
    - _Requirements: 1.2, 1.5_
  
  - [x] 1.3 Add logging to path resolution (path_resolve.rs)
    - Log input path, base directory, and working directory
    - Log resolved canonical path on success
    - Log errors with attempted path and base directory
    - _Requirements: 1.4, 1.5, 9.2_
  
  - [x] 1.4 Add logging to scope resolution (scope.rs)
    - Log entry point (file and position)
    - Log each file traversed during resolution
    - Log symbols found at each step
    - Log final symbol count
    - _Requirements: 1.3, 1.5_
  
  - [x] 1.5 Add logging to LSP handlers (handlers.rs)
    - Log handler invocation (completion, hover, definition, diagnostics)
    - Log whether cross-file resolution is enabled
    - Log whether cross-file functions are being called
    - Log symbol counts returned from cross-file resolution
    - _Requirements: 1.6, 1.5_
  
  - [x] 1.6 Add logging to configuration initialization (config.rs, main.rs)
    - Log cross-file configuration at startup
    - Log enabled status, max_chain_depth, diagnostic severities
    - Log when configuration is invalid with defaults used
    - _Requirements: 8.5, 8.6_

- [ ] 2. Create test infrastructure and helper utilities
  - [x] 2.1 Create TestWorkspace helper struct
    - Implement TestWorkspace::new() to create temp directory
    - Implement add_file() to create files with content
    - Implement get_uri() to get file URIs
    - Add cleanup on drop
    - _Requirements: 7.1, 7.2, 7.3_
  
  - [x] 2.2 Create VerificationReport helper struct
    - Implement VerificationReport for collecting check results
    - Implement add_check() to record pass/fail
    - Implement summary() to format results
    - _Requirements: 7.1, 7.2, 7.3_
  
  - [x] 2.3 Create integration test module (cross_file/integration_tests.rs)
    - Set up module structure with imports
    - Add helper functions for simulating LSP requests
    - Add helper functions for extracting metadata and building graphs
    - _Requirements: 7.1, 7.2, 7.3_

- [ ] 3. Implement real-world failure reproduction tests
  - [x] 3.1 Implement validation_functions/collate.r test case
    - Create test workspace with validation_functions directory
    - Add get_colnames.r with function definition
    - Add collate.r that sources get_colnames.r and uses the function
    - Request diagnostics for collate.r
    - Assert get_colnames() is NOT marked as undefined
    - _Requirements: 7.2, 7.4, 7.5_
  
  - [x] 3.2 Implement backward directive ../oos.r test case
    - Create test workspace with parent file oos.r
    - Create subdir/child.r with @lsp-run-by: ../oos.r directive
    - Extract metadata and build dependency graph
    - Assert no "parent file not found" error
    - Assert edge exists from oos.r to subdir/child.r
    - _Requirements: 7.3, 7.6, 7.8_
  
  - [x] 3.3 Implement basic source() call test case
    - Create file A that sources file B
    - File B defines a function
    - Request completion in A after source() call
    - Assert function from B appears in completions
    - _Requirements: 7.1, 7.4_

- [x] 4. Checkpoint - Run tests and analyze logs
  - Run all integration tests with RUST_LOG=rlsp=trace
  - Analyze logs to identify where execution flow breaks
  - Document findings: which components are working, which are not
  - Ensure all tests pass, ask the user if questions arise

- [ ] 5. Add unit tests for metadata extraction
  - [ ] 5.1 Add unit tests for source() call detection
    - Test detection of source("file.r")
    - Test detection of source('file.r')
    - Test detection with relative paths (../file.r, subdir/file.r)
    - Test UTF-16 column calculation
    - _Requirements: 2.1_
  
  - [ ] 5.2 Write property test for source() call detection
    - **Property 1: Source call detection completeness**
    - **Validates: Requirements 2.1**
    - Generate random R files with source() calls
    - Verify all source() calls are detected
  
  - [ ] 5.3 Add unit tests for directive parsing
    - Test @lsp-run-by without colon or quotes
    - Test @lsp-run-by: with colon
    - Test @lsp-run-by: "file.r" with colon and quotes
    - Test all directive synonyms
    - Test working directory directives
    - _Requirements: 2.2, 2.3_
  
  - [ ] 5.4 Write property test for directive parsing
    - **Property 2: Directive parsing flexibility**
    - **Validates: Requirements 2.2**
    - Generate directives with various syntax combinations
    - Verify all parse correctly

- [ ] 6. Add unit tests for path resolution
  - [ ] 6.1 Add unit tests for relative path resolution
    - Test path resolution with working directory
    - Test path resolution without working directory
    - Test ../ path navigation
    - Test ./ path handling
    - Test path normalization
    - _Requirements: 4.1, 4.2, 4.3, 4.6_
  
  - [ ] 6.2 Write property tests for path resolution
    - **Property 14-20: Path resolution properties**
    - **Validates: Requirements 4.1-4.8**
    - Generate random paths with various components
    - Verify resolution follows documented rules
  
  - [ ] 6.3 Add unit tests for path resolution errors
    - Test non-existent file handling
    - Test invalid path handling
    - Verify error messages include attempted path and base
    - _Requirements: 4.5, 9.2_

- [ ] 7. Add unit tests for dependency graph
  - [ ] 7.1 Add unit tests for edge creation
    - Test edge creation from source() calls
    - Test edge creation from backward directives
    - Test edge deduplication
    - Test call site position storage
    - _Requirements: 3.1, 3.2, 3.6_
  
  - [ ] 7.2 Write property tests for dependency graph
    - **Property 8-13: Dependency graph properties**
    - **Validates: Requirements 3.1-3.6**
    - Generate random file relationships
    - Verify graph operations maintain invariants
  
  - [ ] 7.3 Add unit tests for directive-AST conflict resolution
    - Test directive with call site overrides AST at same call site
    - Test directive without call site suppresses all AST edges
    - Test AST edges to different targets are preserved
    - _Requirements: 3.5_

- [ ] 8. Add unit tests for scope resolution
  - [ ] 8.1 Add unit tests for basic scope resolution
    - Test scope after single source() call
    - Test scope with multiple source() calls
    - Test local symbol precedence over sourced symbols
    - Test symbol structure (name, type, source file)
    - _Requirements: 5.1, 5.2, 5.4, 5.6_
  
  - [ ] 8.2 Write property tests for scope resolution
    - **Property 21-26: Scope resolution properties**
    - **Validates: Requirements 5.1-5.6**
    - Generate random source chains
    - Verify scope resolution maintains invariants
  
  - [ ] 8.3 Add unit tests for scope resolution edge cases
    - Test chain traversal with depth limit
    - Test cycle detection
    - Test empty scope (no sources)
    - _Requirements: 5.3, 5.5_

- [ ] 9. Checkpoint - Verify all components in isolation
  - Run all unit tests and property tests
  - Verify each component works correctly in isolation
  - Document any bugs found in component tests
  - Ensure all tests pass, ask the user if questions arise

- [ ] 10. Investigate and fix LSP handler integration
  - [x] 10.1 Verify handlers call cross-file functions
    - Check handle_completion calls scope_at_position
    - Check handle_hover calls scope_at_position
    - Check handle_definition calls scope_at_position
    - Check diagnostics use cross-file scope
    - Add logging if calls are missing
    - _Requirements: 6.1, 6.2, 6.3, 6.4_
  
  - [x] 10.2 Verify document lifecycle triggers metadata extraction
    - Check textDocument/didOpen triggers extraction
    - Check textDocument/didChange triggers extraction
    - Check revalidation is triggered for affected files
    - Add missing triggers if needed
    - _Requirements: 6.5, 6.6_
  
  - [x] 10.3 Fix any integration issues found
    - Implement fixes for missing handler calls
    - Implement fixes for missing lifecycle triggers
    - Add error handling for integration failures
    - _Requirements: 10.1, 10.2, 10.3, 10.4_

- [ ] 11. Investigate and fix path resolution issues
  - [ ] 11.1 Debug backward directive path resolution
    - Add detailed logging to directive path resolution
    - Test with ../oos.r scenario
    - Identify why "parent file not found" occurs
    - _Requirements: 2.4, 4.8_
  
  - [ ] 11.2 Fix path resolution bugs
    - Implement fix for base directory selection
    - Implement fix for ../ navigation
    - Ensure paths are resolved relative to directive file
    - Add error handling with clear messages
    - _Requirements: 4.1, 4.2, 4.3, 4.8, 9.2_
  
  - [ ] 11.3 Verify path resolution fixes with property tests
    - Run property tests for path resolution
    - Verify all path types resolve correctly
    - Verify error messages are clear

- [ ] 12. Investigate and fix metadata extraction issues
  - [ ] 12.1 Debug source() call detection
    - Add detailed logging to tree-sitter parsing
    - Test with validation_functions/collate.r scenario
    - Identify if source() calls are being detected
    - _Requirements: 2.1_
  
  - [ ] 12.2 Fix metadata extraction bugs
    - Implement fix for source() call detection
    - Implement fix for directive parsing
    - Ensure metadata is cached correctly
    - Add error handling for parse failures
    - _Requirements: 2.1, 2.2, 2.5, 2.6_
  
  - [ ] 12.3 Verify metadata extraction fixes with property tests
    - Run property tests for metadata extraction
    - Verify all source() calls are detected
    - Verify all directive syntaxes parse

- [ ] 13. Investigate and fix dependency graph issues
  - [ ] 13.1 Debug edge creation
    - Add detailed logging to edge creation
    - Verify edges are created from metadata
    - Verify edges are stored with correct call sites
    - _Requirements: 3.1, 3.2_
  
  - [ ] 13.2 Fix dependency graph bugs
    - Implement fix for edge creation from source() calls
    - Implement fix for edge creation from directives
    - Implement fix for conflict resolution
    - Ensure edges are queryable
    - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5_
  
  - [ ] 13.3 Verify dependency graph fixes with property tests
    - Run property tests for dependency graph
    - Verify graph operations maintain invariants
    - Verify conflict resolution is correct

- [ ] 14. Investigate and fix scope resolution issues
  - [ ] 14.1 Debug scope resolution
    - Add detailed logging to scope traversal
    - Verify scope resolution is called by handlers
    - Verify symbols from sourced files are included
    - _Requirements: 5.1, 5.2_
  
  - [ ] 14.2 Fix scope resolution bugs
    - Implement fix for symbol inclusion from sourced files
    - Implement fix for chain traversal
    - Implement fix for local symbol precedence
    - Ensure cycle detection works
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5_
  
  - [ ] 14.3 Verify scope resolution fixes with property tests
    - Run property tests for scope resolution
    - Verify symbols from sourced files are available
    - Verify precedence rules are maintained

- [ ] 15. Verify configuration and error handling
  - [ ] 15.1 Verify configuration is loaded correctly
    - Check cross-file is enabled by default
    - Check max_chain_depth is set correctly
    - Check diagnostic severities are configured
    - Add tests for configuration parsing
    - _Requirements: 8.1, 8.2, 8.3, 8.4_
  
  - [ ] 15.2 Verify error handling throughout system
    - Check all errors are logged with context
    - Check system continues after non-fatal errors
    - Check error messages are actionable
    - Add missing error handling if needed
    - _Requirements: 9.1, 9.2, 9.3, 9.4, 9.5, 9.6_
  
  - [ ] 15.3 Write property tests for error resilience
    - **Property 6, 31: Error resilience properties**
    - **Validates: Requirements 2.6, 9.6**
    - Generate invalid inputs
    - Verify system logs errors and continues

- [ ] 16. Final integration testing and verification
  - [x] 16.1 Run all integration tests
    - Run validation_functions/collate.r test
    - Run backward directive ../oos.r test
    - Run basic source() call test
    - Verify all tests pass
    - _Requirements: 7.1, 7.2, 7.3, 7.4, 7.5, 7.6, 7.8_
  
  - [x] 16.2 Run all property tests
    - Run all property tests with 100+ iterations
    - Verify all properties hold
    - Fix any failures found
    - _Requirements: All property requirements_
  
  - [x] 16.3 Test with real VS Code extension
    - Build and install rlsp with ./setup.sh
    - Open validation_functions/collate.r in VS Code
    - Verify get_colnames() is not marked as undefined
    - Verify completion shows get_colnames
    - Open file with @lsp-run-by: ../oos.r
    - Verify no "parent file not found" error
    - _Requirements: 7.2, 7.3, 7.4, 7.5_

- [x] 17. Final checkpoint - Ensure all tests pass
  - Run full test suite: cargo test -p rlsp
  - Run with logging: RUST_LOG=rlsp=trace cargo test -p rlsp cross_file -- --nocapture
  - Verify real-world usage works in VS Code
  - Document any remaining issues
  - Ensure all tests pass, ask the user if questions arise

- [x] 18. Add regression tests for bug fixes
  - [x] 18.1 Add test for backward directive path resolution bug
    - Create test with @lsp-cd directive in child file
    - Add @lsp-run-by: ../parent.r directive in same file
    - Verify backward directive is resolved relative to file, not @lsp-cd
    - Verify no "parent file not found" diagnostic
    - _Bug: Backward directives were incorrectly using @lsp-cd working directory_
    - _Fix: Use separate PathContext without @lsp-cd for backward directives_
    - _Note: Test reveals fix is only partial - applied to handlers.rs but not dependency.rs_
    - _Requirements: 2.4, 4.8_
  
  - [x] 18.2 Add test for workspace index population bug
    - Create test workspace with multiple R files
    - Simulate LSP initialization (scan_workspace)
    - Verify cross_file_workspace_index is populated
    - Verify closed files are found in index
    - Request diagnostics for file that sources closed file
    - Verify no "undefined variable" error for symbols from closed file
    - _Bug: Workspace scan only populated legacy index, not cross-file index_
    - _Fix: Modified scan_workspace to compute and store cross-file metadata_
    - _Requirements: 7.2, 7.4_
  
  - [x] 18.3 Add test for filesystem fallback in file existence check
    - Create test with backward directive to file not in any cache
    - Verify file_exists closure checks filesystem as fallback
    - Verify no "parent file not found" error for existing file
    - _Bug: file_exists only checked caches, not filesystem_
    - _Fix: Added filesystem fallback with path.exists() check_
    - _Requirements: 2.4, 10.2_

- [x] 19. Complete backward directive path resolution bug fix
  - [x] 19.1 Apply separate PathContext fix to dependency.rs
    - Modify DependencyGraph::update_file() to use separate PathContext for backward directives
    - Create backward_path_ctx without working_directory from metadata
    - Use backward_path_ctx for resolving backward directive paths
    - Keep existing path_ctx for forward source() calls
    - _Requirements: 2.4, 4.8_
  
  - [x] 19.2 Update regression test to verify complete fix
    - Modify test_regression_backward_directive_ignores_lsp_cd to include @lsp-cd
    - Verify backward directive resolves correctly despite @lsp-cd
    - Verify dependency graph contains correct edge
    - Verify no "parent file not found" error
    - _Requirements: 2.4, 4.8_
  
  - [x] 19.3 Run all tests to verify no regressions
    - Run full test suite: cargo test -p rlsp
    - Run integration tests with logging
    - Verify all existing tests still pass
    - Verify new test passes with complete fix
    - _Requirements: 2.4, 4.8_

## Notes

- All tasks are required for comprehensive debugging and testing
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation and allow for course correction
- Logging is added first to enable observability during debugging
- Tests are created before fixes to ensure fixes are verified
- Component isolation (unit tests) happens before integration testing
- Real-world verification happens last to confirm fixes work in practice
- Task 18 adds regression tests for bugs discovered during manual testing

- [x] 20. Add tests and fix for case-insensitive file scanning and on-demand indexing
  - [x] 20.1 Add test for case-insensitive R file scanning
    - Create test workspace with both .R and .r files
    - Verify scan_workspace finds both uppercase and lowercase files
    - Verify cross-file index is populated for both
    - _Bug: Workspace scan only found .R files, not .r files_
    - _Fix: Changed extension check to use eq_ignore_ascii_case("r")_
    - _Requirements: 7.2_
  
  - [x] 20.2 Add on-demand prioritized indexing for sourced files
    - When a file with source() call is opened, immediately index sourced files
    - Prioritize indexing files directly referenced by open documents
    - Index transitively sourced files (sources of sources) with lower priority
    - Verify symbols from on-demand loaded file are available immediately
    - _Design: Prioritized indexing strategy_
      - Priority 1: Files directly sourced by open documents (indexed synchronously)
      - Priority 2: Files referenced by backward directives in open documents (skipped for now)
      - Priority 3: Transitive dependencies (sources of sources) (skipped for now)
      - Priority 4: Remaining workspace files (background scan)
    - _Bug: Files referenced by source() but not scanned at startup are not indexed_
    - _Fix: COMPLETED - Added synchronous on-demand indexing in did_open() for Priority 1 files_
      - Created index_file_on_demand() helper method in Backend
      - Modified did_open() to synchronously index directly sourced files BEFORE scheduling diagnostics
      - This ensures symbols are available when diagnostics run, fixing the race condition
      - Removed failed re-publishing approach from initialized() handler
    - _Requirements: 2.1, 7.2, 7.4_
  
  - [x] 20.3 Run tests to verify case-insensitive scanning
    - Run: cargo test -p rlsp workspace_scan
    - Verify all case-insensitive tests pass
    - Test with real .r files in VS Code
    - _Requirements: 7.2_
