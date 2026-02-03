# Design Document: Position-Aware Definitions

## Overview

This design addresses a bug in Raven LSP where go-to-definition incorrectly navigates to variable assignments that occur AFTER the usage position. R executes sequentially, so a variable must be defined before it can be used.

The fix leverages the existing, robust scope resolution system (`scope_at_position` and `ScopeArtifacts`) which already handles:
1.  **Sequential Execution**: Definitions are ordered by position.
2.  **Function Scoping**: `FunctionScopeTree` correctly isolates function-local variables.
3.  **Shadowing**: Local definitions override global ones.

Instead of implementing a new, ad-hoc AST walker for "same-file" lookups, we will unify the `goto_definition` logic to use the same cached artifacts that cross-file resolution uses. This ensures consistency, correctness, and better performance by utilizing cached artifacts.

## Architecture

The fix involves refactoring `goto_definition` to use `ScopeArtifacts` for the current file:

1.  **Retrieve Artifacts**: obtain `ScopeArtifacts` for the current document (from `ContentProvider` / `WorldState`).
2.  **Resolve Scope**: Call `scope_at_position` with the cursor position.
3.  **Lookup Symbol**: Check if the requested symbol exists in the resolved scope.
    *   If yes, return the definition location from the `ScopedSymbol`.
    *   If no, fall back to other search strategies (workspace search for closed files, though `scope_at_position` usually handles most of this via `scope_at_position_with_graph` if we wanted to go full cross-file immediately, but for "same file" strictly, `scope_at_position` on the file's own artifacts is sufficient for the first pass).

```mermaid
flowchart TD
    A[goto_definition request] --> B{Is identifier?}
    B -->|No| C[Return None]
    B -->|Yes| D[Get ScopeArtifacts for current file]
    D --> E[Call scope_at_position(line, col)]
    E --> F{Symbol in Scope?}
    F -->|Yes| G[Return definition location]
    F -->|No| H[Try cross-file symbols]
    H --> I{Found in cross-file scope?}
    I -->|Yes| J[Return cross-file location]
    I -->|No| K[Search other open documents/Workspace]
```

## Components and Interfaces

### Removed Component
The originally proposed `find_definition_in_tree_before_position` is **discarded**. We will not be walking the AST manually for definitions during requests.

### Updated `goto_definition` Flow

The `goto_definition` handler in `handlers.rs` will be updated:

1.  **Preparation**:
    *   Identify the symbol name at the cursor.
    *   Obtain the `ScopeArtifacts` for the current URI. If the document is open and dirty, these might need to be recomputed (or taken from the live `Document` state if maintained there). `ContentProvider` should handle this abstraction.

2.  **Resolution**:
    *   Call `crate::cross_file::scope::scope_at_position(&artifacts, line, col)`.
    *   This function returns a `ScopeAtPosition` containing only symbols visible at that specific (line, col), respecting:
        *   **Position**: Only definitions appearing before (line, col).
        *   **Scope**: Only definitions valid in the current function nesting.
        *   **Removals**: `rm()` calls are respected.

3.  **Result Construction**:
    *   Look up the symbol name in `ScopeAtPosition.symbols`.
    *   If found: Return `Location` constructed from `symbol.defined_line` and `symbol.defined_column`.
    *   If not found: Proceed to existing cross-file/workspace fallback logic.

### Updated Undefined Variable Diagnostics

The `collect_undefined_variables_position_aware` function currently does a hybrid approach. It should be verified that it strictly relies on `scope_at_position` (or `scope_at_position_with_graph` for full context) and does *not* do its own "first pass collect definitions" that ignores position.

*   Current implementation seems to do `collect_definitions` (first pass) then `collect_usages`. This "first pass" is likely the source of the bug for diagnostics.
*   **Change**: Remove the manual `collect_definitions` pass in `collect_undefined_variables_position_aware`. Rely entirely on `scope_at_position` (which internally uses the artifacts' timeline) to determine if a symbol is defined at the usage site.

## Data Models

Re-use existing `ScopedSymbol` and `ScopeArtifacts` from `cross_file/scope.rs`.

### Definition Logic (via `scope.rs`)

*   **Position**: `(line, column)` lexicographical comparison.
*   **Shadowing**: `scope_at_position` iterates the timeline. A later definition overwrites an earlier one in the `HashMap` *unless* it's in a scope that isn't active.
*   **Function Scope**: `FunctionScopeTree` filters out definitions that are inside functions we are not currently inside.

## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

### Property 1: Position-Aware Definition Lookup

*For any* R source file and any variable usage at position P, if `find_definition_in_tree_before_position` returns a definition, that definition's position SHALL be strictly less than P (lexicographically comparing (line, column)).

**Validates: Requirements 1.1, 1.5**

### Property 2: Closest Definition Selection

*For any* R source file with multiple definitions of the same variable before position P, `find_definition_in_tree_before_position` SHALL return the definition with the largest position that is still less than P.

**Validates: Requirements 1.2**

### Property 3: No Definition Returns None

*For any* R source file where a variable is used at position P and no definition of that variable exists at any position less than P, `find_definition_in_tree_before_position` SHALL return None.

**Validates: Requirements 1.3**

### Property 4: Undefined Variable Emission

*For any* R source file and any variable usage at position P where no definition exists before P (and the variable is not a builtin, cross-file symbol, or package export), the undefined variable checker SHALL emit a diagnostic at position P.

**Validates: Requirements 2.1, 2.3**

### Property 5: No False Positive Diagnostics

*For any* R source file and any variable usage at position P where a definition exists before P, the undefined variable checker SHALL NOT emit an "undefined variable" diagnostic for that usage.

**Validates: Requirements 2.2**

### Property 6: Cross-File Definitions Always Available

*For any* multi-file R project where file A sources file B, symbols defined in B SHALL be available in A at positions after the `source()` call, regardless of whether A has a same-file definition of the same symbol after the query position.

**Validates: Requirements 3.1, 3.2**

### Property 7: Same-File Precedence When Before Position

*For any* R source file with both a same-file definition before position P and a cross-file definition of the same symbol, go-to-definition at position P SHALL return the same-file definition.

**Validates: Requirements 3.3**

### Property 8: Function-Local Position Awareness

*For any* R function containing a variable definition at position P1 and a usage at position P2 within the same function scope, go-to-definition at P2 SHALL return the definition if and only if P1 < P2.

**Validates: Requirements 4.1, 4.2**

## Error Handling

### Invalid Positions

If the query position is outside the document bounds, the function should return None gracefully without panicking.

### Malformed AST

If the AST is malformed or parsing failed, the existing behavior of returning None should be preserved.

### UTF-16 Column Handling

Column positions must be converted between byte offsets and UTF-16 code units consistently. The existing `byte_offset_to_utf16_column` function should be used for this conversion.

## Testing Strategy

### Unit Tests

Unit tests should cover:
- Basic case: definition before usage returns correct location
- Basic case: definition after usage returns None
- Same-line case: definition at earlier column on same line
- Multiple definitions: returns closest one before position
- No definitions: returns None
- Function scope: respects function boundaries
- Edge cases: empty file, single-line file, definition at position (0, 0)

### Property-Based Tests

Property-based tests using `proptest` should verify:
- **Property 1**: Generate random R code with definitions and usages, verify returned definitions are always before query position
- **Property 2**: Generate code with multiple definitions, verify closest-before selection
- **Property 3**: Generate code with no definitions before usage, verify None returned
- **Property 5**: Generate code with definitions before usage, verify no false positive diagnostics

Each property test should run a minimum of 100 iterations.

### Integration Tests

Integration tests should verify:
- End-to-end go-to-definition behavior with the LSP protocol
- Undefined variable diagnostics are correctly emitted
- Cross-file resolution still works correctly
- Existing tests continue to pass

### Test Configuration

Property-based tests should be tagged with:
```rust
// Feature: position-aware-definitions, Property N: [property description]
```
