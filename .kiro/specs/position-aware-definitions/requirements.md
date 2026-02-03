# Requirements Document

## Introduction

This document specifies requirements for fixing go-to-definition behavior and undefined variable diagnostics in Raven LSP to properly handle position-aware symbol resolution. The current implementation has a bug where go-to-definition incorrectly jumps to assignments that occur AFTER the usage position.

R executes sequentially, so a variable must be defined BEFORE it can be used. The LSP should respect this semantic when resolving definitions and emitting undefined variable warnings.

## Glossary

- **Definition_Resolver**: The component responsible for finding where a symbol is defined
- **Undefined_Variable_Checker**: The component that emits diagnostics for variables used before definition
- **Position**: A (line, column) location in a source file
- **Scope_Timeline**: The ordered sequence of scope-affecting events in a file

## Requirements

### Requirement 1: Position-Aware Same-File Go-to-Definition

**User Story:** As a developer, I want go-to-definition to only navigate to definitions that occur before my cursor position, so that I can find where a variable was actually defined when I'm using it.

#### Acceptance Criteria

1. WHEN a user invokes go-to-definition on a variable usage at position P, THE Definition_Resolver SHALL only consider definitions that occur at positions strictly before P
2. WHEN multiple definitions exist before position P, THE Definition_Resolver SHALL return the definition closest to (but before) P
3. WHEN no definition exists before position P, THE Definition_Resolver SHALL return no result for same-file lookup (allowing cross-file resolution to proceed)
4. WHEN a definition occurs on the same line as the usage but at an earlier column, THE Definition_Resolver SHALL consider it a valid definition
5. IF a definition occurs at or after position P, THEN THE Definition_Resolver SHALL NOT return that definition for same-file lookup

### Requirement 2: Position-Aware Undefined Variable Diagnostics

**User Story:** As a developer, I want undefined variable warnings to correctly identify variables used before they are defined, so that I can catch bugs where I reference variables too early.

#### Acceptance Criteria

1. WHEN a variable is used at position P and no definition exists before P, THE Undefined_Variable_Checker SHALL emit an "undefined variable" diagnostic
2. WHEN a variable is used at position P and a definition exists before P, THE Undefined_Variable_Checker SHALL NOT emit an "undefined variable" diagnostic for that usage
3. WHEN a variable is defined after its usage (forward reference), THE Undefined_Variable_Checker SHALL emit an "undefined variable" diagnostic at the usage position
4. WHEN checking undefined variables, THE Undefined_Variable_Checker SHALL use the same position-aware logic as go-to-definition

### Requirement 3: Cross-File Definition Resolution Compatibility

**User Story:** As a developer, I want position-aware definition resolution to work correctly with cross-file symbols from `source()` calls, so that the behavior is consistent across single-file and multi-file projects.

#### Acceptance Criteria

1. WHEN a symbol is defined in a sourced file, THE Definition_Resolver SHALL continue to return that definition regardless of position (cross-file symbols are available from the source() call site)
2. WHEN both a same-file definition (after position) and a cross-file definition exist, THE Definition_Resolver SHALL return the cross-file definition
3. WHEN a same-file definition exists before position P, THE Definition_Resolver SHALL prefer the same-file definition over cross-file definitions
4. THE position-aware filtering SHALL only apply to same-file definitions, not to symbols inherited from parent files or sourced files

### Requirement 4: Function Scope Interaction

**User Story:** As a developer, I want go-to-definition to correctly handle function-local variables, so that variables defined inside a function are not visible outside of it, and go-to-definition works correctly inside functions.

#### Acceptance Criteria

1. WHEN a variable is defined inside a function at position P1 and used at position P2 within the same function, THE Definition_Resolver SHALL return the definition if P1 < P2
2. WHEN a variable is defined inside a function at position P1 and used at position P2 within the same function, THE Definition_Resolver SHALL NOT return the definition if P1 >= P2
3. WHEN a variable is defined inside a function, THE Definition_Resolver SHALL NOT return that definition for usages outside that function (prevent scope leaking)
4. WHEN a global variable shadows a function parameter, THE Definition_Resolver SHALL respect both position and scope rules
