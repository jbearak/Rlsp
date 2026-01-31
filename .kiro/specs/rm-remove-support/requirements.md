# Requirements Document

## Introduction

This feature adds support for tracking when variables are removed from scope via `rm()` or `remove()` calls in R code. Currently, the LSP only tracks when variables are added to scope (via assignments, function definitions, etc.), not when they are removed. This causes false negatives in undefined variable diagnostics when code uses `rm()` to delete variables.

The implementation will detect `rm()` and `remove()` calls, extract the symbol names being removed, and add removal events to the scope timeline. This allows the scope resolution system to correctly determine that a variable is no longer available after an `rm()` call.

## Glossary

- **Rm_Call**: A call to R's `rm()` or `remove()` function that removes objects from an environment. Both functions are aliases with identical behavior.
- **Scope_Timeline**: The ordered sequence of scope-affecting events (definitions, source calls, function scopes, and now removals) tracked for each file.
- **Scope_Event**: An event that affects variable scope, including `Def` (definition), `Source` (sourcing another file), `FunctionScope` (function parameter scope), and the new `Removal` event.
- **Bare_Symbol**: An unquoted identifier passed to `rm()`, e.g., `rm(x)` where `x` is a bare symbol.
- **List_Argument**: The `list=` argument to `rm()` that accepts a character vector of names to remove, e.g., `rm(list = "x")` or `rm(list = c("x", "y"))`.
- **Envir_Argument**: The `envir=` argument to `rm()` that specifies which environment to remove from. When non-default, the removal should be ignored for scope tracking.
- **Cross_File_Scope**: The scope resolution system that tracks symbols across multiple files connected via `source()` calls.

## Requirements

### Requirement 1: Detect rm() Calls with Bare Symbols

**User Story:** As an R developer, I want the LSP to understand that `rm(x)` removes `x` from scope, so that I get accurate undefined variable warnings when I use `x` after removing it.

#### Acceptance Criteria

1. WHEN a call to `rm()` contains a single bare symbol argument, THE Scope_Timeline SHALL include a Removal event for that symbol at the call position
2. WHEN a call to `rm()` contains multiple bare symbol arguments (e.g., `rm(x, y, z)`), THE Scope_Timeline SHALL include Removal events for all specified symbols
3. WHEN a bare symbol in `rm()` is not currently defined in scope, THE Removal event SHALL still be recorded (no error)

### Requirement 2: Detect remove() Calls Identically to rm()

**User Story:** As an R developer, I want `remove()` to be treated the same as `rm()`, so that my code using either function gets accurate scope tracking.

#### Acceptance Criteria

1. WHEN a call to `remove()` is detected, THE system SHALL process it identically to an `rm()` call
2. WHEN `remove()` contains bare symbols, THE Scope_Timeline SHALL include Removal events for those symbols
3. WHEN `remove()` contains a `list=` argument, THE system SHALL process it identically to `rm(list=...)`

### Requirement 3: Support list= Argument with String Literals

**User Story:** As an R developer, I want `rm(list = "x")` and `rm(list = c("x", "y"))` to be recognized, so that common patterns for programmatic removal are tracked.

#### Acceptance Criteria

1. WHEN `rm()` is called with `list = "name"` (single string literal), THE Scope_Timeline SHALL include a Removal event for that name
2. WHEN `rm()` is called with `list = c("a", "b", "c")` (character vector of string literals), THE Scope_Timeline SHALL include Removal events for all specified names
3. WHEN `rm()` is called with `list = variable` (non-literal expression), THE system SHALL NOT create any Removal events (limitation)
4. WHEN `rm()` is called with `list = ls(...)` or other dynamic expressions, THE system SHALL NOT create any Removal events (limitation)

### Requirement 4: Ignore rm() Calls with Non-Default envir= Argument

**User Story:** As an R developer, I want `rm(x, envir = my_env)` to be ignored for scope tracking, so that removals from other environments don't affect my global scope analysis.

#### Acceptance Criteria

1. WHEN `rm()` is called with an `envir=` argument that is not the default, THE system SHALL NOT create any Removal events
2. WHEN `rm()` is called without an `envir=` argument, THE system SHALL process the removal normally
3. WHEN `rm()` is called with `envir = globalenv()` or `envir = .GlobalEnv`, THE system SHALL process the removal normally (these are equivalent to default)

### Requirement 5: Function Scope Handling

**User Story:** As an R developer, I want `rm()` inside a function to only affect that function's local scope, so that global variables aren't incorrectly marked as removed.

#### Acceptance Criteria

1. WHEN `rm()` is called inside a function body, THE Removal event SHALL only affect scope within that function
2. WHEN `rm()` is called at the top level (global scope), THE Removal event SHALL affect global scope
3. WHEN a variable is removed inside a function, THE variable SHALL still be available in the global scope after the function returns

### Requirement 6: Cross-File Scope Integration

**User Story:** As an R developer, I want `rm()` to affect cross-file scope correctly, so that if I source a file that defines a function and then remove it, downstream code sees it as undefined.

#### Acceptance Criteria

1. WHEN a file sources another file that defines symbol `s`, and then calls `rm(s)`, THE symbol `s` SHALL NOT be in scope after the `rm()` call
2. WHEN a file is sourced by another file, and the parent file calls `rm()` on a symbol defined in the child, THE symbol SHALL be removed from scope in the parent's context
3. WHEN computing scope at a position, THE system SHALL respect Removal events in the timeline ordering

### Requirement 7: Scope Resolution with Removals

**User Story:** As an R developer, I want the scope resolution to correctly handle the timeline of definitions and removals, so that re-definitions after removals work correctly.

#### Acceptance Criteria

1. WHEN a symbol is defined, then removed, then defined again, THE symbol SHALL be in scope after the second definition
2. WHEN a symbol is removed before it is defined, THE removal SHALL have no effect (symbol was never in scope)
3. WHEN computing scope at a position between a definition and removal, THE symbol SHALL be in scope
4. WHEN computing scope at a position after a removal, THE symbol SHALL NOT be in scope

### Requirement 8: Documentation

**User Story:** As an R developer, I want to understand the limitations of rm() tracking, so that I know what patterns are and aren't supported.

#### Acceptance Criteria

1. THE README.md SHALL document the rm()/remove() tracking feature
2. THE documentation SHALL list supported patterns: bare symbols, `list=` with string literals
3. THE documentation SHALL list unsupported patterns: dynamic expressions, `list=` with variables, pattern-based removal
4. THE documentation SHALL explain that `envir=` argument causes the call to be ignored

