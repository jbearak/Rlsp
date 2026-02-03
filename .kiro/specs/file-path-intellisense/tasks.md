# Implementation Plan: File Path Intellisense

## Overview

This implementation adds file path intellisense to Raven, providing completions when typing file paths in `source()` calls and LSP directives, plus go-to-definition navigation for file paths. The implementation creates a new `file_path_intellisense.rs` module and integrates with existing completion and definition handlers.

## Tasks

- [x] 1. Set up module structure and core types
  - [x] 1.1 Create `crates/raven/src/file_path_intellisense.rs` with module structure and imports
    - Create the new module file with necessary imports (tree-sitter, lsp-types, std::path, etc.)
    - _Requirements: Foundation for all file path intellisense features_
  
  - [x] 1.2 Define `FilePathContext` enum and `DirectiveType` enum
    - Implement `FilePathContext::SourceCall`, `FilePathContext::Directive`, `FilePathContext::None`
    - Implement `DirectiveType::SourcedBy`, `DirectiveType::Source`
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 1.6_
  
  - [x] 1.3 Add module declaration to `main.rs` or `lib.rs`
    - Export the new module for use by handlers
    - _Requirements: Foundation_

- [x] 2. Implement context detection for source() calls
  - [x] 2.1 Implement `is_source_call_string_context()` using tree-sitter AST
    - Traverse AST to find `call` nodes with `source` or `sys.source` function names
    - Check if cursor position is inside the string argument
    - Return partial path, content start position, and is_sys_source flag
    - _Requirements: 1.1, 1.2_
  
  - [x] 2.2 Implement `extract_partial_path()` helper function
    - Extract text from string start to cursor position
    - Handle escaped characters and quotes
    - _Requirements: 1.1, 1.2_
  
  - [x] 2.3 Write property test for source call context detection
    - **Property 1: Source Call Context Detection**
    - **Validates: Requirements 1.1, 1.2**

- [x] 3. Implement context detection for LSP directives
  - [x] 3.1 Implement `is_directive_path_context()` using regex patterns
    - Match `@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`, `@lsp-source` directives
    - Handle optional colon and quotes syntax variations
    - Return directive type, partial path, and path start position
    - _Requirements: 1.3, 1.4, 1.5, 1.6_
  
  - [x] 3.2 Write property test for backward directive context detection
    - **Property 2: Backward Directive Context Detection**
    - **Validates: Requirements 1.3, 1.4, 1.5**
  
  - [x] 3.3 Write property test for forward directive context detection
    - **Property 3: Forward Directive Context Detection**
    - **Validates: Requirements 1.6**

- [x] 4. Implement unified context detection
  - [x] 4.1 Implement `detect_file_path_context()` combining source call and directive detection
    - Check source call context first, then directive context
    - Return `FilePathContext::None` for non-matching contexts
    - _Requirements: 1.1-1.7_
  
  - [x] 4.2 Write property test for non-source function exclusion
    - **Property 4: Non-Source Function Exclusion**
    - **Validates: Requirements 1.7**

- [x] 5. Checkpoint - Context detection complete
  - Ensure all context detection tests pass, ask the user if questions arise.

- [x] 6. Implement directory listing and filtering
  - [x] 6.1 Implement `list_directory_entries()` function
    - List files and directories in the given base path
    - Exclude hidden files/directories (starting with `.`)
    - Handle filesystem errors gracefully
    - _Requirements: 2.1, 2.2, 2.7_
  
  - [x] 6.2 Implement `filter_r_files()` to filter for .R/.r files and directories
    - Keep files with `.R` or `.r` extensions
    - Keep all directories (for navigation)
    - _Requirements: 2.1, 2.2_
  
  - [x] 6.3 Write property test for R file and directory filtering
    - **Property 5: R File and Directory Filtering**
    - **Validates: Requirements 2.1, 2.2**

- [x] 7. Implement completion item creation
  - [x] 7.1 Implement `create_path_completion_item()` for files and directories
    - Set `CompletionItemKind::FILE` or `CompletionItemKind::FOLDER`
    - Add trailing `/` to directory insert_text
    - Use forward slashes for all path separators
    - _Requirements: 2.6, 4.3_
  
  - [x] 7.2 Write property test for directory completion trailing slash
    - **Property 9: Directory Completion Trailing Slash**
    - **Validates: Requirements 2.6**
  
  - [x] 7.3 Write property test for output path separator
    - **Property 11: Output Path Separator**
    - **Validates: Requirements 4.3**

- [x] 8. Implement path resolution for completions
  - [x] 8.1 Implement relative path resolution (including `../` prefixes)
    - Use existing `PathContext` infrastructure
    - For source() calls: respect @lsp-cd working directory
    - For directives: always relative to file's directory
    - _Requirements: 2.3, 2.5_
  
  - [x] 8.2 Implement workspace-root-relative paths (starting with `/`)
    - Resolve `/path` relative to workspace root for both directives and source() calls
    - _Requirements: 2.4_
  
  - [x] 8.3 Implement backslash normalization (convert `\\` to `/`)
    - Normalize escaped backslashes before path resolution
    - _Requirements: 4.1, 4.2_
  
  - [x] 8.5 Write property test for path separator normalization
    - **Property 10: Path Separator Normalization**
    - **Validates: Requirements 4.1, 4.2**

- [x] 9. Implement main completion function
  - [x] 9.1 Implement `file_path_completions()` main function
    - Determine base directory from context and partial path
    - List and filter directory entries
    - Create completion items for each entry
    - Enforce workspace boundary (exclude files outside workspace)
    - _Requirements: 2.1-2.7, 7.2_
  
  - [x] 9.2 Write property test for workspace boundary enforcement
    - **Property 17: Workspace Boundary Enforcement**
    - **Validates: Requirements 7.2**

- [x] 10. Checkpoint - Completions implementation complete
  - Ensure all completion tests pass, ask the user if questions arise.

- [x] 11. Implement go-to-definition for file paths
  - [x] 11.1 Implement `extract_file_path_at_position()` to get full path string at cursor
    - For source() calls: extract string literal content
    - For directives: extract path after directive keyword
    - Return the path string and context type
    - _Requirements: 5.1, 5.2, 6.1-6.4_
  
  - [x] 11.2 Implement `file_path_definition()` main function
    - Detect context type at cursor position
    - Resolve path using appropriate PathContext (with or without @lsp-cd)
    - Return `Location` at line 0, column 0 if file exists
    - Return `None` if file doesn't exist
    - _Requirements: 5.1-5.5, 6.1-6.5_
  
  - [x] 11.3 Write property test for source call go-to-definition
    - **Property 12: Source Call Go-to-Definition**
    - **Validates: Requirements 5.1, 5.2, 5.4**
  
  - [x] 11.4 Write property test for missing file returns no definition
    - **Property 13: Missing File Returns No Definition**
    - **Validates: Requirements 5.3**
  
  - [x] 11.5 Write property test for backward directive go-to-definition
    - **Property 14: Backward Directive Go-to-Definition**
    - **Validates: Requirements 6.1, 6.2, 6.3**
  
  - [x] 11.6 Write property test for forward directive go-to-definition
    - **Property 15: Forward Directive Go-to-Definition**
    - **Validates: Requirements 6.4**
  
  - [x] 11.7 Write property test for backward directives ignore @lsp-cd
    - **Property 16: Backward Directives Ignore @lsp-cd**
    - **Validates: Requirements 6.5**

- [x] 12. Integrate with LSP handlers
  - [x] 12.1 Modify `handlers::completion()` to check for file path context
    - Call `detect_file_path_context()` before other completion logic
    - Return file path completions when in file path context
    - _Requirements: 1.1-1.6, 2.1-2.7_
  
  - [x] 12.2 Add trigger characters `/` and `"` to completion options in backend.rs
    - Register additional trigger characters for path navigation
    - _Requirements: 3.1, 3.2, 3.4_
  
  - [x] 12.3 Modify `handlers::goto_definition()` to check for file path context
    - Call `file_path_definition()` when cursor is on a file path
    - Return file location for valid paths
    - _Requirements: 5.1-5.5, 6.1-6.5_

- [x] 13. Implement edge case handling
  - [x] 13.1 Handle empty workspace (return empty completions)
    - Return empty list when workspace has no R files
    - _Requirements: 7.1_
  
  - [x] 13.2 Handle paths outside workspace (exclude from completions)
    - Check resolved paths against workspace root
    - Exclude paths that escape workspace boundary
    - _Requirements: 7.2_
  
  - [x] 13.3 Handle invalid path characters gracefully
    - Catch filesystem errors for invalid characters
    - Return empty completions without throwing errors
    - _Requirements: 7.3_
  
  - [x] 13.4 Handle empty file path (show current directory contents)
    - When path is empty, list files in current directory
    - _Requirements: 7.4_
  
  - [x] 13.5 Handle paths with spaces in quoted strings
    - Correctly parse paths containing spaces
    - _Requirements: 7.5_
  
  - [x] 13.6 Write property test for invalid character handling
    - **Property 18: Invalid Character Handling**
    - **Validates: Requirements 7.3**
  
  - [x] 13.7 Write property test for space handling in paths
    - **Property 19: Space Handling in Paths**
    - **Validates: Requirements 7.5**

- [x] 14. Final checkpoint - All tests pass
  - Ensure all unit tests and property tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional property-based tests that can be skipped for faster MVP
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties from the design document
- Unit tests validate specific examples and edge cases
- The implementation reuses existing `PathContext` and `resolve_path()` from `cross_file/path_resolve.rs`
- Path resolution differs between source() calls (respects @lsp-cd) and directives (ignores @lsp-cd)
