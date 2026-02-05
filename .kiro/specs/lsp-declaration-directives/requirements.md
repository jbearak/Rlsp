# Requirements Document

## Introduction

This document specifies requirements for adding LSP declaration directives to Raven, an R Language Server. These directives allow users to declare symbols (variables and functions) that cannot be statically detected by the parser, enabling proper IDE support for dynamically created symbols.

R code frequently creates symbols through dynamic mechanisms like `eval()`, `assign()`, `load()`, or external data loading that the static parser cannot detect. Without declaration directives, these symbols produce false-positive "undefined variable" diagnostics and are missing from completions and hover information.

## Glossary

- **Declaration_Directive**: An LSP comment directive that declares a symbol exists in scope without requiring static detection
- **Declared_Symbol**: A symbol (variable or function) introduced via a declaration directive
- **Variable_Directive**: A declaration directive for variables (`@lsp-declare-variable`, `@lsp-var`, etc.)
- **Function_Directive**: A declaration directive for functions (`@lsp-declare-function`, `@lsp-func`, etc.)
- **Directive_Parser**: The module that extracts LSP directives from R source file comments
- **Scope_Resolution**: The process of determining which symbols are available at a given position
- **Timeline**: The ordered sequence of scope-affecting events in a file (definitions, source calls, removals, declarations)

## Requirements

### Requirement 1: Variable Declaration Directive Parsing

**User Story:** As an R developer, I want to declare variables that are created dynamically, so that the LSP does not report them as undefined.

#### Acceptance Criteria

1. WHEN a comment contains `@lsp-declare-variable`, `@lsp-declare-var`, `@lsp-variable`, or `@lsp-var` followed by an identifier, THE Directive_Parser SHALL extract the variable name
2. WHEN a variable directive uses optional colon syntax (e.g., `@lsp-var: myvar`), THE Directive_Parser SHALL correctly parse the variable name
3. WHEN a variable directive uses quoted syntax (e.g., `@lsp-var "my.var"`), THE Directive_Parser SHALL correctly parse the variable name including special characters
4. WHEN multiple variable directives appear on separate lines, THE Directive_Parser SHALL extract all declared variables
5. WHEN a variable directive appears on a specific line, THE Directive_Parser SHALL record the 0-based line number of the directive
6. WHEN a directive does not start with `@` (e.g., `# lsp-var myvar`), THE Directive_Parser SHALL NOT recognize it as a valid directive

### Requirement 2: Function Declaration Directive Parsing

**User Story:** As an R developer, I want to declare functions that are created dynamically, so that the LSP provides proper completions and hover information.

#### Acceptance Criteria

1. WHEN a comment contains `@lsp-declare-function`, `@lsp-declare-func`, `@lsp-function`, or `@lsp-func` followed by an identifier, THE Directive_Parser SHALL extract the function name
2. WHEN a function directive uses optional colon syntax (e.g., `@lsp-func: myfunc`), THE Directive_Parser SHALL correctly parse the function name
3. WHEN a function directive uses quoted syntax (e.g., `@lsp-func "my.func"`), THE Directive_Parser SHALL correctly parse the function name including special characters
4. WHEN multiple function directives appear on separate lines, THE Directive_Parser SHALL extract all declared functions
5. WHEN a function directive appears on a specific line, THE Directive_Parser SHALL record the 0-based line number of the directive
6. WHEN a directive does not start with `@` (e.g., `# lsp-func myfunc`), THE Directive_Parser SHALL NOT recognize it as a valid directive

### Requirement 3: Declared Symbol Storage in Metadata

**User Story:** As a language server component, I want declared symbols stored in cross-file metadata, so that they can be used during scope resolution.

#### Acceptance Criteria

1. THE CrossFileMetadata struct SHALL include a field for declared variables
2. THE CrossFileMetadata struct SHALL include a field for declared functions
3. WHEN metadata is serialized and deserialized, THE declared symbols SHALL be preserved correctly
4. WHEN a file is re-parsed, THE declared symbols SHALL be updated to reflect the current directive state

### Requirement 4: Position-Aware Scope Integration

**User Story:** As an R developer, I want declared symbols to be available only after their declaration line, so that scope resolution is position-accurate.

#### Acceptance Criteria

1. WHEN computing scope at a position, THE Scope_Resolution SHALL include declared symbols from directives appearing before that position
2. WHEN computing scope at a position, THE Scope_Resolution SHALL NOT include declared symbols from directives appearing after that position
3. WHEN a declared variable directive appears at line N, THE declared variable SHALL be available starting from line N+1 (the line after the directive)
4. WHEN a declared function directive appears at line N, THE declared function SHALL be available starting from line N+1 (the line after the directive)
5. THE Timeline SHALL include declaration events in document order alongside other scope events
6. WHEN a directive appears on a line with code (e.g., `x <- 1 # @lsp-var foo`), THE declared symbol SHALL be available starting from line N+1, not on line N

### Requirement 5: Undefined Variable Diagnostic Suppression

**User Story:** As an R developer, I want declared symbols to suppress "undefined variable" diagnostics, so that I don't see false positives for dynamically created symbols.

#### Acceptance Criteria

1. WHEN a variable is declared via directive and used after the directive line, THE diagnostic collector SHALL NOT emit an "undefined variable" warning
2. WHEN a function is declared via directive and called after the directive line, THE diagnostic collector SHALL NOT emit an "undefined variable" warning
3. WHEN a variable is used before its declaration directive, THE diagnostic collector SHALL emit an "undefined variable" warning
4. WHEN a declared symbol name matches a usage exactly (case-sensitive), THE diagnostic SHALL be suppressed

### Requirement 6: Completion Support for Declared Symbols

**User Story:** As an R developer, I want declared symbols to appear in code completions, so that I can easily reference dynamically created symbols.

#### Acceptance Criteria

1. WHEN requesting completions at a position after a variable declaration directive, THE completion list SHALL include the declared variable
2. WHEN requesting completions at a position after a function declaration directive, THE completion list SHALL include the declared function
3. WHEN a declared function appears in completions, THE completion item SHALL indicate it is a function (with appropriate kind)
4. WHEN a declared variable appears in completions, THE completion item SHALL indicate it is a variable (with appropriate kind)

### Requirement 7: Hover Support for Declared Symbols

**User Story:** As an R developer, I want to see hover information for declared symbols, so that I know they were introduced via directive.

#### Acceptance Criteria

1. WHEN hovering over a declared variable usage, THE hover response SHALL indicate the symbol was declared via directive
2. WHEN hovering over a declared function usage, THE hover response SHALL indicate the symbol was declared via directive
3. THE hover response SHALL include the directive line number where the symbol was declared

### Requirement 8: Go-to-Definition for Declared Symbols

**User Story:** As an R developer, I want go-to-definition to navigate to the declaration directive, so that I can find where a symbol was declared.

#### Acceptance Criteria

1. WHEN invoking go-to-definition on a declared symbol usage, THE response SHALL navigate to the directive line
2. THE definition location SHALL point to the start of the directive comment

### Requirement 9: Cross-File Declaration Inheritance

**User Story:** As an R developer, I want declared symbols from parent files to be available in child files, so that declarations work across source() boundaries.

#### Acceptance Criteria

1. WHEN a parent file declares a symbol before a source() call, THE declared symbol SHALL be available in the sourced child file
2. WHEN a parent file declares a symbol after a source() call, THE declared symbol SHALL NOT be available in the sourced child file
3. WHEN traversing the dependency chain, THE declared symbols SHALL follow the same inheritance rules as regular symbols
4. WHEN a source() call uses `local=TRUE`, THE declared symbols from the parent SHALL still be visible in the child file (declarations describe symbol existence, not export behavior)

### Requirement 10: Interface Hash Update

**User Story:** As a language server component, I want declared symbols to affect the interface hash, so that dependent files are revalidated when declarations change.

#### Acceptance Criteria

1. WHEN a declaration directive is added to a file, THE interface hash SHALL change
2. WHEN a declaration directive is removed from a file, THE interface hash SHALL change
3. WHEN a declaration directive's symbol name changes, THE interface hash SHALL change
4. WHEN only non-declaration content changes, THE interface hash change behavior SHALL remain unchanged

### Requirement 11: Conflicting Declaration Types

**User Story:** As an R developer, I want clear behavior when the same symbol is declared as both a variable and a function.

#### Acceptance Criteria

1. WHEN the same symbol name is declared as both a variable (`@lsp-var`) and a function (`@lsp-func`), THE later declaration SHALL take precedence for symbol kind
2. WHEN conflicting declarations exist, THE diagnostic suppression SHALL apply regardless of kind (the symbol exists)
3. WHEN conflicting declarations exist, THE go-to-definition SHALL navigate to the first declaration by line number
4. WHEN conflicting declarations exist, THE completion item kind SHALL reflect the later declaration's kind

### Requirement 12: Workspace Index Integration

**User Story:** As an R developer, I want declared symbols from indexed (but not open) files to be available when those files are part of the dependency chain.

#### Acceptance Criteria

1. WHEN a closed file is indexed by the workspace indexer, THE declared symbols SHALL be extracted and stored
2. WHEN a dependency chain includes an indexed (closed) file with declarations, THE declared symbols SHALL be available in scope resolution
3. WHEN an indexed file is opened, THE declared symbols SHALL be re-extracted from the live document content
