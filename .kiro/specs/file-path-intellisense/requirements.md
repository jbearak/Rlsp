# Requirements Document

## Introduction

This feature adds file path intellisense to Raven (R Language Server), providing two key capabilities:

1. **File path completions** - When typing inside `source("")`, `sys.source("")`, or LSP directives like `# @lsp-sourced-by`, the user gets file path completions showing available R files in the workspace.

2. **Go-to-definition for file paths** - Command-click (Ctrl+click on Windows/Linux) on file paths in `source()` calls and LSP directives navigates to that file.

These features improve developer productivity by reducing manual path typing and enabling quick navigation between related R files in a project.

## Glossary

- **File_Path_Completion**: An LSP completion item representing a file or directory path that can be inserted at the cursor position.
- **Source_Call**: A call to R's `source()` or `sys.source()` function that executes code from another file.
- **LSP_Directive**: A comment-based directive like `# @lsp-sourced-by`, `# @lsp-run-by`, `# @lsp-included-by`, or `# @lsp-source` that declares file relationships.
- **String_Literal_Context**: The cursor position is inside a string literal (between quotes) in R code.
- **Directive_Path_Context**: The cursor position is after an LSP directive keyword where a file path is expected.
- **Trigger_Character**: A character that, when typed, triggers the LSP completion provider to show suggestions.
- **Path_Separator**: The character used to separate directory components in a path (forward slash `/` or backslash `\`).
- **Relative_Path**: A file path relative to the current file's directory (e.g., `../utils.R`, `./helpers/data.R`).
- **Workspace_Root_Relative_Path**: A file path starting with `/` that is relative to the workspace root.

## Requirements

### Requirement 1: Detect File Path Completion Context

**User Story:** As an R developer, I want the LSP to recognize when I'm typing a file path, so that I can receive relevant file path completions.

#### Acceptance Criteria

1. WHEN the cursor is inside a string literal in a `source()` call, THE Completion_Provider SHALL recognize this as a file path context
2. WHEN the cursor is inside a string literal in a `sys.source()` call, THE Completion_Provider SHALL recognize this as a file path context
3. WHEN the cursor is after an `@lsp-sourced-by` directive (with or without colon), THE Completion_Provider SHALL recognize this as a file path context
4. WHEN the cursor is after an `@lsp-run-by` directive (with or without colon), THE Completion_Provider SHALL recognize this as a file path context
5. WHEN the cursor is after an `@lsp-included-by` directive (with or without colon), THE Completion_Provider SHALL recognize this as a file path context
6. WHEN the cursor is after an `@lsp-source` directive (with or without colon), THE Completion_Provider SHALL recognize this as a file path context
7. WHEN the cursor is inside a string literal in a non-source function call, THE Completion_Provider SHALL NOT provide file path completions

### Requirement 2: Provide File Path Completions

**User Story:** As an R developer, I want to see available R files when typing a path, so that I can quickly select the correct file without memorizing paths.

#### Acceptance Criteria

1. WHEN file path completions are triggered, THE Completion_Provider SHALL show files with `.R` or `.r` extensions
2. WHEN file path completions are triggered, THE Completion_Provider SHALL show directories that may contain R files
3. WHEN a partial path is typed (e.g., `../`), THE Completion_Provider SHALL show completions relative to that partial path
4. WHEN the path starts with `/`, THE Completion_Provider SHALL show completions relative to the workspace root
5. WHEN no path prefix is typed, THE Completion_Provider SHALL show completions relative to the current file's directory (or @lsp-cd working directory for source() calls)
6. WHEN a directory is selected from completions, THE Completion_Provider SHALL append a path separator to enable continued path navigation
7. WHEN listing directory contents, THE Completion_Provider SHALL exclude hidden files and directories (those starting with `.`)

### Requirement 3: Configure Trigger Characters for Path Completions

**User Story:** As an R developer, I want path completions to appear automatically when I type path separators, so that I can navigate directories without manually invoking completions.

#### Acceptance Criteria

1. WHEN the user types `/` inside a file path context, THE Completion_Provider SHALL trigger and show completions
2. WHEN the user types `"` to start a string in a source() call, THE Completion_Provider SHALL trigger and show completions
3. WHEN the user types a recognized directive name (e.g., `@lsp-sourced-by`), THE Completion_Provider SHALL trigger and show completions after the directive
4. THE LSP Server SHALL register `/` and `"` as additional trigger characters for completions

### Requirement 4: Support Path Separators

**User Story:** As an R developer working on different operating systems, I want both forward slash and backslash to work in paths, so that my code is portable.

#### Acceptance Criteria

1. WHEN the user types a path with forward slashes, THE Completion_Provider SHALL resolve the path correctly
2. WHEN the user types a path with escaped backslashes (`\\`), THE Completion_Provider SHALL resolve the path correctly
3. WHEN providing completions, THE Completion_Provider SHALL use forward slashes as the path separator (R convention)

### Requirement 5: Go-to-Definition for File Paths in source() Calls

**User Story:** As an R developer, I want to Command-click on a file path in `source()` to open that file, so that I can quickly navigate to sourced files.

#### Acceptance Criteria

1. WHEN the cursor is on a string literal in a `source()` call and go-to-definition is invoked, THE Definition_Provider SHALL navigate to the referenced file
2. WHEN the cursor is on a string literal in a `sys.source()` call and go-to-definition is invoked, THE Definition_Provider SHALL navigate to the referenced file
3. WHEN the referenced file does not exist, THE Definition_Provider SHALL return no definition (no navigation)
4. WHEN the path is relative, THE Definition_Provider SHALL resolve it relative to the current file's directory (or @lsp-cd working directory if set)
5. WHEN go-to-definition navigates to a file, THE Definition_Provider SHALL position the cursor at line 0, column 0

### Requirement 6: Go-to-Definition for File Paths in LSP Directives

**User Story:** As an R developer, I want to Command-click on a file path in LSP directives to open that file, so that I can quickly navigate between related files.

#### Acceptance Criteria

1. WHEN the cursor is on a file path in an `@lsp-sourced-by` directive and go-to-definition is invoked, THE Definition_Provider SHALL navigate to the referenced file
2. WHEN the cursor is on a file path in an `@lsp-run-by` directive and go-to-definition is invoked, THE Definition_Provider SHALL navigate to the referenced file
3. WHEN the cursor is on a file path in an `@lsp-included-by` directive and go-to-definition is invoked, THE Definition_Provider SHALL navigate to the referenced file
4. WHEN the cursor is on a file path in an `@lsp-source` directive and go-to-definition is invoked, THE Definition_Provider SHALL navigate to the referenced file
5. WHEN the path in a directive is resolved, THE Definition_Provider SHALL resolve it relative to the file's directory (ignoring @lsp-cd, per existing path resolution rules)

### Requirement 7: Handle Edge Cases

**User Story:** As an R developer, I want the file path features to handle edge cases gracefully, so that I don't encounter errors or unexpected behavior.

#### Acceptance Criteria

1. WHEN the workspace has no R files, THE Completion_Provider SHALL return an empty completion list
2. WHEN the path points outside the workspace, THE Completion_Provider SHALL NOT show completions for files outside the workspace
3. WHEN the path contains invalid characters, THE Completion_Provider SHALL handle it gracefully without errors
4. WHEN the file path is empty, THE Completion_Provider SHALL show files in the current directory
5. WHEN a quoted path contains spaces, THE Completion_Provider SHALL handle it correctly
