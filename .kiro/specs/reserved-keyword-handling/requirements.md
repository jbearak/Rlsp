# Requirements Document

## Introduction

This feature addresses incorrect handling of R **reserved words** in the Raven LSP.

Two related issues currently occur:

**Bug 1 - False definition**: When a user writes `else <- 1`, the LSP incorrectly treats `else` as a valid variable definition. In R, reserved words cannot be used as identifiers, so this code is invalid and should not create a definition.

**Bug 2 - False undefined variable**: When `else` appears in an unexpected position (e.g., on a separate line from an `if`), the LSP reports "Undefined variable: else". This is wrong because:
- `else` is a reserved word, not a variable
- the actual problem is a syntax/parse error (e.g., `else` placement rules)
- the diagnostic should not disappear merely because `else <- 1` was written

The fix requires the LSP to:
1. Never treat reserved words as variable/function definitions
2. Never report reserved words as "undefined variables"
3. Continue surfacing parse errors from tree-sitter without adding keyword-specific heuristics

## Glossary

- **Reserved_Word**: An R reserved word that must not be treated as a user-defined identifier. The list for this feature is: `if`, `else`, `repeat`, `while`, `function`, `for`, `in`, `next`, `break`, `TRUE`, `FALSE`, `NULL`, `Inf`, `NaN`, `NA`, `NA_integer_`, `NA_real_`, `NA_complex_`, `NA_character_`.
- **Reserved_Word_Module**: A small utility that centralizes the reserved-word list and exposes `is_reserved_word`.
- **Definition_Extractor**: The component that identifies variable and function definitions from the R AST.
- **Undefined_Variable_Checker**: The component that reports diagnostics for variables used but not defined.
- **Syntax_Error_Collector**: The component that reports syntax/parse errors from tree-sitter.
- **Scope_Resolver**: The component that determines which symbols are in scope at a given position.

## Requirements

### Requirement 1: Reserved Word List

**User Story:** As a developer, I want the LSP to have a comprehensive list of R reserved words, so that it can correctly identify them throughout the codebase.

#### Acceptance Criteria

1. The Reserved_Word_Module SHALL define a constant list containing all reserved words used by this feature: `if`, `else`, `repeat`, `while`, `function`, `for`, `in`, `next`, `break`, `TRUE`, `FALSE`, `NULL`, `Inf`, `NaN`, `NA`, `NA_integer_`, `NA_real_`, `NA_complex_`, `NA_character_`.
2. The Reserved_Word_Module SHALL provide a function `is_reserved_word(name: &str) -> bool` that returns true if `name` is a reserved word.
3. The Reserved_Word_Module SHALL be usable by any component that performs definition extraction, scope resolution, completion generation, or undefined-variable diagnostics.

### Requirement 2: Exclude Reserved Words from Definitions

**User Story:** As a developer, I want the LSP to never treat reserved words as variable/function definitions, so that invalid code like `else <- 1` does not create a definition for `else`.

#### Acceptance Criteria

1. When the Definition_Extractor encounters an assignment where the left-hand side identifier text matches a reserved word, the Definition_Extractor SHALL NOT add it to the exported interface.
2. When the Definition_Extractor encounters an assignment where the left-hand side identifier text matches a reserved word, the Definition_Extractor SHALL NOT add it to the scope timeline.
3. When the Definition_Extractor encounters `else <- 1`, the Definition_Extractor SHALL NOT create a definition for `else`.
4. When the Definition_Extractor encounters `if <- function() {}`, the Definition_Extractor SHALL NOT create a definition for `if`.

### Requirement 3: Skip Reserved Words in Undefined Variable Checks

**User Story:** As a developer, I want the LSP to never report reserved words as undefined variables, so that misplaced `else` results in parse errors rather than "undefined variable".

#### Acceptance Criteria

1. When the Undefined_Variable_Checker encounters an identifier usage whose text matches a reserved word, the Undefined_Variable_Checker SHALL skip it without reporting an undefined variable diagnostic.
2. When the Undefined_Variable_Checker encounters `else` in any position, it SHALL NOT report "Undefined variable: else".
3. When the Undefined_Variable_Checker encounters `if` in any position, it SHALL NOT report "Undefined variable: if".
4. The Undefined_Variable_Checker SHALL check whether an identifier is a reserved word BEFORE checking whether it is defined in scope.

### Requirement 4: Syntax/Parse Error Reporting for Misplaced Reserved Words

**User Story:** As a developer, I want the LSP to report parse errors when reserved words appear in invalid positions, so that I get accurate error messages.

#### Acceptance Criteria

1. When tree-sitter reports a syntax/parse error for code containing a misplaced reserved word, the Syntax_Error_Collector SHALL report that error.
2. The Syntax_Error_Collector SHALL NOT be modified as part of this feature; it already reports tree-sitter syntax/parse errors.

### Requirement 5: Identifier Completions Exclude Reserved Words

**User Story:** As a developer, I want the LSP to not suggest reserved words as *identifier* completions, so that I don't accidentally try to use them as variable/function names.

#### Acceptance Criteria

1. When generating completions for in-scope identifiers (e.g., variables/functions), the Completion_Provider SHALL NOT include reserved words in the identifier completion list.
2. When a user has defined `el <- 1` and types `el`, the Completion_Provider SHALL suggest `el` but SHALL NOT suggest `else` as an identifier completion.
3. This requirement SHALL NOT prevent keyword-specific completions (if any) from offering reserved words as keywords; it only applies to identifier/symbol-derived completion items.

### Requirement 6: Document Symbols Exclude Reserved Words

**User Story:** As a developer, I want the document symbols list to not include reserved words that appear on the left side of assignments.

#### Acceptance Criteria

1. When collecting document symbols, the Document_Symbol_Provider SHALL NOT include symbols where the name is a reserved word.
2. When the document contains `else <- 1`, the Document_Symbol_Provider SHALL NOT list `else` as a symbol.
