# Requirements Document

## Introduction

This feature adds two new completion types to Raven (an R language server): function parameter completions and dollar-sign completions. When the cursor is inside a function call, the LSP will suggest parameter names for that function. When the cursor follows a `$` operator, the LSP will suggest member/column names from known data structures.

## Glossary

- **Completion_Handler**: The LSP component that processes completion requests and returns completion items
- **Parameter_Resolver**: The component that extracts and caches function parameter information
- **Dollar_Resolver**: The component that resolves member names for objects accessed via the `$` operator
- **Function_Signature**: The parameter list of a function including parameter names and default values
- **R_Subprocess**: The interface for querying R about package information, function signatures, and object structures
- **Package_Library**: The cache of installed R packages and their exports
- **Cross_File_Scope**: The scope resolution system that tracks symbols across sourced files
- **Tree_Sitter**: The incremental parsing library used to analyze R code structure

## Requirements

### Requirement 1: Function Call Context Detection

**User Story:** As a developer, I want the LSP to detect when my cursor is inside a function call, so that I can receive relevant parameter suggestions.

#### Acceptance Criteria

1. WHEN the cursor is positioned after `(` and before `)` in a function call, THE Completion_Handler SHALL detect the function call context
2. WHEN the cursor is positioned after a comma within function arguments, THE Completion_Handler SHALL detect the function call context
3. WHEN the cursor is positioned inside nested function calls, THE Completion_Handler SHALL detect the innermost function call context
4. WHEN the cursor is positioned outside any function call parentheses, THE Completion_Handler SHALL NOT provide parameter completions
5. WHEN the cursor is positioned inside a string literal within function arguments, THE Completion_Handler SHALL NOT provide parameter completions

### Requirement 2: Base R Function Parameter Completions

**User Story:** As a developer, I want to see parameter suggestions for base R functions, so that I can quickly write correct function calls without consulting documentation.

#### Acceptance Criteria

1. WHEN the cursor is inside a base R function call, THE Parameter_Resolver SHALL query R subprocess using `formals()` to get parameter names
2. WHEN R subprocess returns parameter information, THE Completion_Handler SHALL display parameter names as completion items
3. WHEN a parameter has a default value, THE Completion_Handler SHALL include the default value in the completion detail
4. IF R subprocess is unavailable, THEN THE Parameter_Resolver SHALL return an empty parameter list gracefully
5. THE Parameter_Resolver SHALL cache function signatures to avoid repeated R subprocess queries

### Requirement 3: Package Function Parameter Completions

**User Story:** As a developer, I want to see parameter suggestions for functions from loaded packages, so that I can use package functions efficiently.

#### Acceptance Criteria

1. WHEN the cursor is inside a call to a function from a loaded package, THE Parameter_Resolver SHALL resolve the function's package and query its parameters
2. WHEN using namespace-qualified calls (e.g., `dplyr::filter`), THE Parameter_Resolver SHALL query parameters for the specified package's function
3. WHEN multiple packages export the same function name, THE Parameter_Resolver SHALL use the function from the package that was loaded most recently
4. THE Parameter_Resolver SHALL cache package function signatures per package to minimize R subprocess queries

### Requirement 4: User-Defined Function Parameter Completions

**User Story:** As a developer, I want to see parameter suggestions for functions I've defined in my project, so that I can use my own functions consistently.

#### Acceptance Criteria

1. WHEN the cursor is inside a call to a user-defined function in the current file, THE Parameter_Resolver SHALL extract parameters from the function definition AST
2. WHEN the cursor is inside a call to a function defined in a sourced file, THE Parameter_Resolver SHALL use cross-file scope to locate and extract parameters
3. WHEN a user-defined function has parameters with default values, THE Completion_Handler SHALL include the default value in the completion detail
4. WHEN a user-defined function uses `...` (dots), THE Completion_Handler SHALL NOT include `...` as a completion item since dots is a pass-through mechanism, not a named parameter to be specified

### Requirement 5: Parameter Completion Formatting

**User Story:** As a developer, I want parameter completions to be clearly formatted, so that I can distinguish them from other completion types.

#### Acceptance Criteria

1. THE Completion_Handler SHALL display parameter completions with a distinct icon or kind (e.g., `CompletionItemKind::PROPERTY` or `CompletionItemKind::FIELD`)
2. WHEN a parameter has a default value, THE Completion_Handler SHALL show the default in the detail field (e.g., `= TRUE`)
3. WHEN inserting a parameter completion, THE Completion_Handler SHALL append an equals sign followed by a space (`=` + space) after the parameter name
4. THE Completion_Handler SHALL filter parameter completions based on the user's typed prefix
5. THE Completion_Handler SHALL exclude parameters that have already been specified in the current function call

### Requirement 6: Dollar-Sign Context Detection

**User Story:** As a developer, I want the LSP to detect when I'm accessing object members with `$`, so that I can receive relevant member suggestions.

#### Acceptance Criteria

1. WHEN the cursor is positioned immediately after `$` following an identifier, THE Completion_Handler SHALL detect the dollar-sign context
2. WHEN the cursor is positioned after `$` with a partial member name typed, THE Completion_Handler SHALL detect the dollar-sign context and filter by prefix
3. WHEN the `$` operator is inside a string literal, THE Completion_Handler SHALL NOT provide dollar-sign completions
4. WHEN the expression before `$` is a complex expression (e.g., `func()$`), THE Completion_Handler SHALL return an empty completion list since the return type cannot be determined statically

### Requirement 7: Data Frame Column Completions

**User Story:** As a developer, I want to see column name suggestions when accessing data frames with `$`, so that I can avoid typos in column names.

#### Acceptance Criteria

1. WHEN the object before `$` is a built-in dataset (e.g., `mtcars`, `iris`), THE Dollar_Resolver SHALL query R subprocess for column names using `names()`
2. WHEN the object was created with `data.frame()` with named arguments, THE Dollar_Resolver SHALL extract column names from the AST
3. WHEN the object was assigned new columns via `df$new_col <- value`, THE Dollar_Resolver SHALL include the new column name in completions
4. WHEN the object's columns cannot be determined statically (e.g., `read.csv()`, external data), THE Dollar_Resolver SHALL return an empty completion list
5. THE Dollar_Resolver SHALL cache dataset column information to avoid repeated R subprocess queries

### Requirement 8: List Member Completions

**User Story:** As a developer, I want to see member name suggestions when accessing lists with `$`, so that I can navigate complex data structures.

#### Acceptance Criteria

1. WHEN the object before `$` is a list created with `list()` with named elements, THE Dollar_Resolver SHALL extract member names from the AST
2. WHEN the object is a named list from a package (e.g., model output), THE Dollar_Resolver SHALL attempt to provide known member names
3. WHEN list members cannot be determined statically, THE Dollar_Resolver SHALL return an empty completion list gracefully

### Requirement 9: Signature Cache Management

**User Story:** As a developer, I want function signatures to be cached efficiently, so that completions are fast and responsive.

#### Acceptance Criteria

1. THE Parameter_Resolver SHALL store cached signatures in a thread-safe data structure
2. THE Parameter_Resolver SHALL invalidate user-defined function signatures when the defining file changes
3. THE Parameter_Resolver SHALL persist package function signatures across LSP sessions where possible
4. WHEN cache memory exceeds a configurable threshold, THE Parameter_Resolver SHALL evict least-recently-used entries

### Requirement 10: R Subprocess Query Interface

**User Story:** As a developer, I want the LSP to query R for function information, so that completions are accurate for the installed R version.

#### Acceptance Criteria

1. THE R_Subprocess SHALL provide a method to query function parameters using `formals(func)`
2. THE R_Subprocess SHALL provide a method to query object names using `names(obj)`
3. THE R_Subprocess SHALL validate function and object names to prevent code injection
4. IF a query times out or fails, THEN THE R_Subprocess SHALL return an error without crashing the LSP
5. THE R_Subprocess SHALL support querying parameters for functions in specific packages using `formals(pkg::func)`

### Requirement 11: Integration with Existing Completions

**User Story:** As a developer, I want parameter and dollar-sign completions to integrate seamlessly with existing completion types, so that I have a unified completion experience.

#### Acceptance Criteria

1. WHEN in function call context, THE Completion_Handler SHALL show parameter completions before other completion types
2. WHEN in dollar-sign context, THE Completion_Handler SHALL show member completions exclusively (no keywords or other symbols)
3. THE Completion_Handler SHALL maintain existing completion behavior when not in parameter or dollar-sign context
4. THE Completion_Handler SHALL respect the existing precedence: local definitions > package exports > cross-file symbols

### Requirement 12: Error Handling and Graceful Degradation

**User Story:** As a developer, I want the LSP to handle errors gracefully, so that completions work even when some information is unavailable.

#### Acceptance Criteria

1. IF R subprocess is unavailable, THEN THE Completion_Handler SHALL fall back to AST-based parameter extraction where possible
2. IF function signature cannot be determined, THEN THE Completion_Handler SHALL return standard completions without parameter suggestions
3. IF dollar-sign object cannot be resolved, THEN THE Completion_Handler SHALL return an empty completion list
4. THE Completion_Handler SHALL log errors at trace level without displaying error messages to the user
