# Requirements Document

## Introduction

This specification defines enhancements to Rlsp's variable definition detection and hover information capabilities. The system will recognize loop iterator variables to eliminate false-positive undefined variable warnings, and provide enhanced hover information showing definition statements with hyperlinked file locations.

## Glossary

- **Rlsp**: The static R Language Server implementation using tree-sitter for parsing
- **Loop_Iterator**: A variable defined in the iterator position of a for loop (e.g., `i` in `for (i in 1:10)`)
- **Definition_Statement**: The source code text that defines a symbol (variable or function)
- **Hover_Provider**: The LSP component that generates hover information when a user hovers over a symbol
- **Scope_Resolver**: The component that determines which symbols are available at a given position
- **Tree_Sitter_Node**: A node in the abstract syntax tree produced by tree-sitter-r parser
- **Cross_File_Definition**: A symbol definition that exists in a different file from the current reference
- **Hyperlink**: A markdown-formatted clickable link following the pattern `[text](uri)`

## Requirements

### Requirement 1: Loop Iterator Variable Detection

**User Story:** As an R developer, I want loop iterator variables to be recognized as defined variables, so that I don't receive false-positive undefined variable warnings.

#### Acceptance Criteria

1. WHEN a for loop defines an iterator variable, THE Scope_Resolver SHALL include that variable in the scope for the loop body
2. WHEN code references a loop iterator variable within the loop body, THE Rlsp SHALL NOT emit an undefined variable diagnostic
3. WHEN a for loop is nested within another for loop, THE Scope_Resolver SHALL correctly track both iterator variables in their respective scopes
4. WHEN a loop iterator variable shadows an outer scope variable, THE Scope_Resolver SHALL prioritize the loop iterator in the loop body scope
5. THE Scope_Resolver SHALL extract loop iterator names from tree-sitter for loop nodes

### Requirement 2: Definition Statement Extraction

**User Story:** As an R developer, I want to see the definition statement when I hover over a symbol, so that I can quickly understand how the symbol is defined without navigating to the definition.

#### Acceptance Criteria

1. WHEN a user hovers over a variable reference, THE Hover_Provider SHALL extract the definition statement from the source text
2. WHEN a user hovers over a function reference, THE Hover_Provider SHALL extract the function definition statement including the signature
3. WHEN extracting a definition statement, THE Hover_Provider SHALL use the tree-sitter node's byte range to extract the exact source text
4. WHEN a definition statement spans multiple lines, THE Hover_Provider SHALL include all lines up to a reasonable limit
5. THE Hover_Provider SHALL format definition statements as R code blocks in markdown

### Requirement 3: Hyperlinked File Location

**User Story:** As an R developer, I want hover information to include a clickable file location, so that I can quickly navigate to the definition.

#### Acceptance Criteria

1. WHEN a symbol is defined in the current file, THE Hover_Provider SHALL display "this file, line N" where N is the definition line number
2. WHEN a symbol is defined in a different file, THE Hover_Provider SHALL display a hyperlink in the format `[relative_path](file:///absolute_path), line N`
3. WHEN generating file URIs, THE Hover_Provider SHALL use the file:// protocol with absolute paths
4. WHEN calculating relative paths, THE Hover_Provider SHALL compute the path relative to the workspace root
5. THE Hover_Provider SHALL use LSP MarkupKind Markdown for all hover content

### Requirement 4: Cross-File Hover Information

**User Story:** As an R developer working with multi-file projects, I want hover information to work for symbols defined in sourced files, so that I can understand definitions regardless of file boundaries.

#### Acceptance Criteria

1. WHEN a symbol is defined in a sourced file, THE Hover_Provider SHALL locate the definition using the cross-file dependency graph
2. WHEN multiple definitions exist for a symbol, THE Hover_Provider SHALL use the scope resolution system to select the correct definition
3. WHEN a definition cannot be located, THE Hover_Provider SHALL return hover information indicating the symbol type without a definition statement
4. THE Hover_Provider SHALL use existing cross-file metadata to resolve definition locations

### Requirement 5: Hover Content Formatting

**User Story:** As an R developer, I want hover information to be clearly formatted and readable, so that I can quickly extract the information I need.

#### Acceptance Criteria

1. THE Hover_Provider SHALL format definition statements as markdown code blocks with R syntax highlighting
2. WHEN displaying file locations, THE Hover_Provider SHALL separate the definition statement and location with a blank line
3. WHEN a definition statement exceeds 10 lines, THE Hover_Provider SHALL truncate it and append an ellipsis indicator
4. THE Hover_Provider SHALL preserve the original indentation of definition statements
5. THE Hover_Provider SHALL escape markdown special characters in definition statements

### Requirement 6: Loop Iterator Persistence

**User Story:** As an R developer, I want loop iterator variables to persist after the loop completes, so that the LSP correctly models R's scoping behavior.

#### Acceptance Criteria

1. WHEN a for loop defines an iterator variable, THE Scope_Resolver SHALL include that variable in scope for all code after the loop definition
2. WHEN code references a loop iterator variable after the loop body, THE Scope_Resolver SHALL include the iterator in the available symbols
3. WHEN a loop iterator is defined, THE Scope_Resolver SHALL treat it as a regular variable definition that persists beyond the loop
4. THE Scope_Resolver SHALL recognize loop iterators at the position where the for loop begins

### Requirement 7: Function Scope Boundaries

**User Story:** As an R developer, I want function-local variables to only be available within their function scope, so that I receive warnings when referencing variables outside their scope.

#### Acceptance Criteria

1. WHEN a variable is defined inside a function body, THE Scope_Resolver SHALL NOT include that variable in scopes outside the function
2. WHEN a function parameter is referenced outside the function body, THE Scope_Resolver SHALL NOT include that parameter in the available symbols
3. WHEN code outside a function references a function-local variable, THE Rlsp SHALL emit an undefined variable diagnostic
4. WHEN analyzing scope at a specific position, THE Scope_Resolver SHALL determine if the position is within a function body
5. THE Scope_Resolver SHALL use tree-sitter function definition node byte ranges to determine function scope boundaries

### Requirement 8: Function Parameter Recognition

**User Story:** As an R developer, I want function parameters to be correctly recognized as defined variables within the function body, so that parameter references don't generate false warnings.

#### Acceptance Criteria

1. WHEN a function defines parameters, THE Scope_Resolver SHALL include those parameters in the function body scope
2. WHEN a function parameter has a default value, THE Scope_Resolver SHALL recognize the parameter regardless of the default
3. WHEN a function uses `...` (ellipsis) parameter, THE Scope_Resolver SHALL recognize it as a defined symbol
4. THE Scope_Resolver SHALL extract function parameters from tree-sitter function definition nodes

### Requirement 9: Source Local Parameter Handling

**User Story:** As an R developer, I want the LSP to correctly handle `source(local=TRUE)` vs `source(local=FALSE)`, so that scope resolution respects R's scoping rules for sourced files.

#### Acceptance Criteria

1. WHEN a file is sourced with `local=FALSE`, THE Scope_Resolver SHALL make sourced symbols available in the global scope
2. WHEN a file is sourced with `local=TRUE` inside a function, THE Scope_Resolver SHALL make sourced symbols available only within that function scope
3. WHEN a file is sourced with `local=TRUE` at the top level, THE Scope_Resolver SHALL make sourced symbols available in the current environment scope
4. WHEN the `local` parameter is omitted, THE Scope_Resolver SHALL treat it as `local=FALSE` (R's default behavior)
5. THE Scope_Resolver SHALL use existing cross-file metadata that tracks the `local` flag for source() calls

### Requirement 10: Definition Statement Extraction for Special Cases

**User Story:** As an R developer, I want hover information to handle special R constructs correctly, so that I get accurate information for all symbol types.

#### Acceptance Criteria

1. WHEN a symbol is defined through assignment operators (`<-`, `=`, `<<-`, `->`), THE Hover_Provider SHALL extract the complete assignment statement
2. WHEN a function is defined inline, THE Hover_Provider SHALL extract the function definition including the function keyword
3. WHEN a symbol is defined in a for loop iterator, THE Hover_Provider SHALL extract the for loop header
4. WHEN a symbol is a function parameter, THE Hover_Provider SHALL extract the function signature
5. THE Hover_Provider SHALL handle all R assignment operators correctly
