# Requirements Document

## Introduction

This feature modifies Rlsp's undefined variable detection to skip checks in contexts where R uses non-standard evaluation (NSE) or data frame column access. This matches Ark's (Posit's R kernel/LSP) approach, which hardcodes these skip rules without configuration settings. The goal is to eliminate false positive "undefined variable" warnings in common R idioms.

Ark's implementation uses three context flags:
1. `in_formula` - set when traversing inside formula expressions (`~`)
2. `in_call_like_arguments` - set when traversing inside function call arguments
3. `ExtractOperator` check - checks if identifier is RHS of `$` or `@` operators

## Glossary

- **NSE (Non-Standard Evaluation)**: R's ability to capture and manipulate unevaluated expressions, commonly used in tidyverse and base R functions like `subset()`, `with()`, `dplyr::filter()`, etc.
- **Extract_Operator**: The `$` and `@` operators in R used to access named elements from lists/data frames and S4 object slots respectively. In tree-sitter-r, these are represented as `extract_operator` nodes.
- **Formula**: R expressions using the `~` operator, commonly used in statistical modeling (e.g., `y ~ x`). In tree-sitter-r, these are `unary_operator` (for `~ x`) or `binary_operator` (for `y ~ x`) nodes with `~` as the operator.
- **Call_Like_Node**: Function calls (`call`), subset operations (`subset`/`subset2` for `[` and `[[`). All have an `arguments` field containing the argument list.
- **Undefined_Variable_Diagnostic**: A warning diagnostic emitted when an identifier is used but not defined in the current scope.
- **Tree_Sitter**: The parsing library used by Rlsp to analyze R code structure.

## Requirements

### Requirement 1: Skip RHS of Extract Operators

**User Story:** As an R developer, I want to use `df$column` and `obj@slot` syntax without seeing false "undefined variable" warnings for the column/slot names, so that I can work with data frames and S4 objects naturally.

#### Acceptance Criteria

1. WHEN an identifier appears as the right-hand side of a `$` operator, THE Undefined_Variable_Diagnostic SHALL NOT be emitted for that identifier
2. WHEN an identifier appears as the right-hand side of an `@` operator, THE Undefined_Variable_Diagnostic SHALL NOT be emitted for that identifier
3. WHEN an identifier appears on the left-hand side of `$` or `@` operators, THE Undefined_Variable_Diagnostic SHALL still be checked normally

### Requirement 2: Skip Variables Inside Call-Like Arguments

**User Story:** As an R developer, I want to use NSE functions like `subset(df, x > 5)` or `dplyr::filter(df, column == value)` without seeing false warnings for variables that will be resolved at runtime, so that I can use idiomatic R code.

#### Acceptance Criteria

1. WHEN an identifier appears inside the arguments of a function call (`call` node), THE Undefined_Variable_Diagnostic SHALL NOT be emitted for that identifier
2. WHEN an identifier appears inside the arguments of a subset operation (`subset` node for `[`), THE Undefined_Variable_Diagnostic SHALL NOT be emitted for that identifier
3. WHEN an identifier appears inside the arguments of a subset2 operation (`subset2` node for `[[`), THE Undefined_Variable_Diagnostic SHALL NOT be emitted for that identifier
4. WHEN an identifier appears as the function name being called (the `function` field of a call-like node), THE Undefined_Variable_Diagnostic SHALL still be checked normally
5. WHEN an identifier appears outside any call-like arguments, THE Undefined_Variable_Diagnostic SHALL still be checked normally

### Requirement 3: Skip Variables Inside Formula Expressions

**User Story:** As an R developer, I want to use formula syntax like `~ x` or `y ~ x + z` without seeing false warnings for the formula variables, so that I can write statistical models naturally.

#### Acceptance Criteria

1. WHEN an identifier appears inside a unary formula expression (`~ x`), THE Undefined_Variable_Diagnostic SHALL NOT be emitted for that identifier
2. WHEN an identifier appears inside a binary formula expression (`y ~ x`), THE Undefined_Variable_Diagnostic SHALL NOT be emitted for that identifier
3. WHEN an identifier appears outside of formula expressions, THE Undefined_Variable_Diagnostic SHALL still be checked normally
4. WHEN formulas are nested inside call arguments, THE Undefined_Variable_Diagnostic SHALL NOT be emitted for identifiers in the formula (both contexts apply)

### Requirement 4: No Configuration Required

**User Story:** As an R developer, I want these NSE-aware skip rules to work automatically without any configuration, so that I get a good experience out of the box.

#### Acceptance Criteria

1. THE Skip_Rules for NSE contexts SHALL be hardcoded and always active
2. THE Skip_Rules SHALL NOT require any configuration settings to enable
3. THE existing `undefined_variables_enabled` configuration SHALL continue to control whether undefined variable checking is performed at all

### Requirement 5: Preserve Existing Skip Rules

**User Story:** As an R developer, I want the existing skip rules for assignment LHS and named arguments to continue working, so that I don't see regressions in diagnostic behavior.

#### Acceptance Criteria

1. WHEN an identifier appears as the left-hand side of an assignment (`<-`, `=`, `<<-`), THE Undefined_Variable_Diagnostic SHALL NOT be emitted for that identifier
2. WHEN an identifier appears as a named argument name (e.g., `n` in `func(n = 1)`), THE Undefined_Variable_Diagnostic SHALL NOT be emitted for that identifier
