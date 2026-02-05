# Design Document: LSP Declaration Directives

## Overview

This design adds declaration directives to Raven that allow users to declare symbols (variables and functions) that cannot be statically detected by the parser. These directives integrate with the existing cross-file awareness system to provide proper IDE support for dynamically created symbols.

The implementation extends the existing directive parsing infrastructure in `directive.rs`, adds new fields to `CrossFileMetadata` in `types.rs`, introduces a new `ScopeEvent::Declaration` variant, and updates scope resolution to include declared symbols.

## Architecture

The declaration directive feature follows the existing directive architecture pattern:

```
┌─────────────────────────────────────────────────────────────┐
│                    Directive Processing Flow                 │
├─────────────────────────────────────────────────────────────┤
│  1. File Content                                            │
│     └── # @lsp-var myvar                                    │
│     └── # @lsp-func myfunc                                  │
│                                                             │
│  2. Directive Parser (directive.rs)                         │
│     └── parse_directives() extracts DeclaredSymbol entries  │
│     └── Stores in CrossFileMetadata.declared_variables      │
│     └── Stores in CrossFileMetadata.declared_functions      │
│                                                             │
│  3. Scope Artifacts (scope.rs)                              │
│     └── compute_artifacts() creates ScopeEvent::Declaration │
│     └── Timeline includes declarations in document order    │
│                                                             │
│  4. Scope Resolution                                        │
│     └── scope_at_position() includes declared symbols       │
│     └── Position-aware: only symbols before query position  │
│                                                             │
│  5. LSP Features                                            │
│     └── Diagnostics: suppress "undefined variable"          │
│     └── Completions: include declared symbols               │
│     └── Hover: show declaration info                        │
│     └── Go-to-definition: navigate to directive             │
└─────────────────────────────────────────────────────────────┘
```

## Components and Interfaces

### 1. Directive Parser Extension (`directive.rs`)

Add new regex patterns and parsing logic for declaration directives:

```rust
/// A declared symbol from an @lsp-var or @lsp-func directive
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclaredSymbol {
    /// The symbol name
    pub name: String,
    /// 0-based line where the directive appears
    pub line: u32,
    /// Whether this is a function (true) or variable (false)
    pub is_function: bool,
}
```

New regex patterns:
- Variable: `#\s*@lsp-(?:declare-variable|declare-var|variable|var)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))`
- Function: `#\s*@lsp-(?:declare-function|declare-func|function|func)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))`

### 2. Metadata Extension (`types.rs`)

Extend `CrossFileMetadata` with declared symbol storage:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrossFileMetadata {
    // ... existing fields ...
    
    /// Variables declared via @lsp-var directives
    pub declared_variables: Vec<DeclaredSymbol>,
    /// Functions declared via @lsp-func directives  
    pub declared_functions: Vec<DeclaredSymbol>,
}
```

### 3. Scope Event Extension (`scope.rs`)

Add a new `ScopeEvent` variant for declarations:

```rust
pub enum ScopeEvent {
    // ... existing variants ...
    
    /// A symbol declared via @lsp-var or @lsp-func directive
    Declaration {
        line: u32,
        column: u32,
        symbol: ScopedSymbol,
    },
}
```

### 4. Interface Updates

#### `parse_directives()` in `directive.rs`
- Input: File content as `&str`
- Output: `CrossFileMetadata` with `declared_variables` and `declared_functions` populated
- Behavior: Scans for declaration directives and extracts symbol names

#### `compute_artifacts()` in `scope.rs`
- Input: URI, Tree, content, and optionally metadata
- Output: `ScopeArtifacts` with `Declaration` events in timeline
- Behavior: Converts declared symbols from metadata into timeline events

#### `scope_at_position()` in `scope.rs`
- Input: Artifacts, line, column
- Output: `ScopeAtPosition` with declared symbols included
- Behavior: Includes declared symbols from `Declaration` events before query position

#### `compute_interface_hash()` in `scope.rs`
- Input: Interface map, packages, and declared symbols
- Output: Hash value
- Behavior: Includes declared symbols in hash computation for cache invalidation

## Data Models

### DeclaredSymbol

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclaredSymbol {
    /// The symbol name (e.g., "myvar", "my.func")
    pub name: String,
    /// 0-based line number where the directive appears
    pub line: u32,
    /// true for @lsp-func, false for @lsp-var
    pub is_function: bool,
}
```

### ScopeEvent::Declaration

```rust
ScopeEvent::Declaration {
    /// 0-based line of the directive
    line: u32,
    /// Column (u32::MAX for end-of-line sentinel, ensuring symbol is available from line+1)
    column: u32,
    /// The declared symbol with full metadata
    symbol: ScopedSymbol,
}
```

### ScopedSymbol for Declared Symbols

When creating a `ScopedSymbol` from a `DeclaredSymbol`:
- `name`: From directive
- `kind`: `SymbolKind::Function` or `SymbolKind::Variable`
- `source_uri`: URI of the file containing the directive
- `defined_line`: Line of the directive
- `defined_column`: 0
- `signature`: `None` (no signature info available)
- `is_declared`: `true` (new field to distinguish declared symbols from parsed ones)

### Hover Content Format

When hovering over a declared symbol, the hover content SHALL use this format:

**For declared variables:**
```
myvar (declared variable)

Declared via @lsp-var directive at line 5
```

**For declared functions:**
```
myfunc (declared function)

Declared via @lsp-func directive at line 10
```

The hover content uses Markdown formatting and includes:
1. Symbol name with kind annotation in parentheses
2. Blank line separator
3. Declaration source with directive type and 1-based line number (for user display)

## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system-essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

### Property 1: Directive Parsing Completeness

*For any* valid symbol name and any directive synonym form (`@lsp-var`, `@lsp-variable`, `@lsp-declare-var`, `@lsp-declare-variable` for variables; `@lsp-func`, `@lsp-function`, `@lsp-declare-func`, `@lsp-declare-function` for functions), with or without optional colon, with or without quotes, parsing SHALL extract the exact symbol name and correct symbol kind (function or variable).

**Validates: Requirements 1.1, 1.2, 1.3, 1.4, 1.5, 2.1, 2.2, 2.3, 2.4, 2.5**

### Property 2: Required @ Prefix

*For any* directive-like comment that does not start with `@` (e.g., `# lsp-var myvar`), the parser SHALL NOT recognize it as a valid declaration directive and SHALL NOT extract any declared symbol.

**Validates: Requirements 1.6, 2.6**

### Property 3: Metadata Serialization Round-Trip

*For any* `CrossFileMetadata` containing declared variables and functions, serializing to JSON and deserializing back SHALL produce an equivalent metadata object with all declared symbols preserved.

**Validates: Requirements 3.3**

### Property 4: Position-Aware Scope Inclusion

*For any* file with declaration directives at various line positions and any query position (line, column), a declared symbol SHALL appear in scope if and only if the query line is strictly greater than the directive line (i.e., directive_line < query_line). A symbol declared on line N is available starting from line N+1.

**Validates: Requirements 4.1, 4.2, 4.3, 4.4, 4.5, 4.6**

### Property 5: Diagnostic Suppression

*For any* file with a declaration directive and a usage of the declared symbol name (case-sensitive match), the undefined variable diagnostic SHALL be suppressed if and only if the usage position is after the declaration directive line.

**Validates: Requirements 5.1, 5.2, 5.3, 5.4**

### Property 6: Completion Inclusion with Correct Kind

*For any* completion request at a position after a declaration directive, the declared symbol SHALL appear in the completion list with `CompletionItemKind::FUNCTION` for function declarations and `CompletionItemKind::VARIABLE` for variable declarations.

**Validates: Requirements 6.1, 6.2, 6.3, 6.4**

### Property 7: Cross-File Declaration Inheritance

*For any* parent file with a declaration directive and a `source()` call, the declared symbol SHALL be available in the sourced child file if and only if the declaration directive appears before the `source()` call in the parent file. This holds regardless of whether `local=TRUE` is used in the source() call.

**Validates: Requirements 9.1, 9.2, 9.3, 9.4**

### Property 8: Interface Hash Sensitivity

*For any* file, the interface hash SHALL change when a declaration directive is added, removed, or when a declared symbol's name changes. The hash SHALL remain stable when only non-declaration content changes.

**Validates: Requirements 10.1, 10.2, 10.3, 10.4**

### Property 9: Conflicting Declaration Resolution

*For any* file where the same symbol name is declared as both a variable and a function, the later declaration (by line number) SHALL determine the symbol kind for completions and hover. Diagnostic suppression SHALL apply regardless of which declaration comes first.

**Validates: Requirements 11.1, 11.2, 11.3, 11.4**

### Property 10: Workspace Index Declaration Extraction

*For any* file indexed by the workspace indexer, declared symbols SHALL be extracted and stored such that when the file participates in a dependency chain, its declared symbols are available during scope resolution.

**Validates: Requirements 12.1, 12.2, 12.3**

## Error Handling

### Invalid Directive Syntax

When a directive is malformed (e.g., missing symbol name):
- The directive is silently ignored
- No error diagnostic is emitted (consistent with existing directive behavior)
- Parsing continues with remaining content

### Empty Symbol Names

When a directive has an empty or whitespace-only symbol name:
- The directive is ignored
- No `DeclaredSymbol` is created

### Duplicate Declarations

When the same symbol is declared multiple times with the same kind:
- All declarations are stored in metadata
- The first declaration (by line number) takes precedence for go-to-definition
- All declarations suppress diagnostics for that symbol name

### Conflicting Declaration Kinds

When the same symbol is declared as both a variable and a function:
- Both declarations are stored in metadata
- The later declaration (by line number) determines the symbol kind for completions and hover
- The first declaration (by line number) is used for go-to-definition
- Diagnostic suppression applies regardless of kind (the symbol exists in either case)

### Invalid Characters in Symbol Names

R allows many characters in symbol names when backtick-quoted. For declaration directives:
- Quoted names (double or single quotes) preserve special characters
- Unquoted names are parsed as contiguous non-whitespace
- No validation is performed on symbol name validity (matches R's permissive naming)

## Testing Strategy

### Unit Tests

1. **Directive Parsing Tests** (`directive.rs`)
   - Test all synonym forms for variable directives
   - Test all synonym forms for function directives
   - Test optional colon syntax
   - Test quoted paths with special characters
   - Test multiple directives in one file
   - Test directives without `@` prefix are NOT recognized
   - Test line number recording

2. **Metadata Serialization Tests** (`types.rs`)
   - Test round-trip serialization of declared symbols
   - Test default values for new fields

3. **Metadata Update Tests** (`directive.rs`)
   - Test that re-parsing a file updates declared symbols to reflect current directives
   - Test that removing a directive removes the declared symbol
   - Test that adding a directive adds the declared symbol

4. **Scope Resolution Tests** (`scope.rs`)
   - Test declared symbols appear in scope after directive line (line N+1)
   - Test declared symbols do NOT appear on directive line itself (line N)
   - Test declared symbols do NOT appear before directive line
   - Test function vs variable kind distinction
   - Test timeline ordering with mixed events
   - Test same-line code and directive (symbol available from next line only)

5. **Diagnostic Tests** (`handlers.rs`)
   - Test undefined variable suppression for declared variables
   - Test undefined variable suppression for declared functions
   - Test diagnostics still emitted for undeclared symbols
   - Test diagnostics emitted when usage is before declaration
   - Test diagnostics emitted when usage is on same line as declaration

6. **Completion Tests** (`handlers.rs`)
   - Test declared symbols appear in completions
   - Test correct CompletionItemKind for functions vs variables
   - Test declared symbols do NOT appear in completions on declaration line

7. **Hover Tests** (`handlers.rs`)
   - Test hover shows declaration info with correct format
   - Test hover includes 1-based directive line number
   - Test hover shows "declared variable" vs "declared function" appropriately

8. **Go-to-Definition Tests** (`handlers.rs`)
   - Test navigation to directive line
   - Test navigation to first declaration when symbol declared multiple times
   - Test navigation to first declaration when symbol has conflicting kinds

9. **Conflicting Declaration Tests** (`scope.rs`, `handlers.rs`)
   - Test later declaration determines symbol kind
   - Test diagnostic suppression works regardless of declaration order
   - Test go-to-definition uses first declaration

### Property-Based Tests

Property tests should run minimum 100 iterations each. Each test must be tagged with:
**Feature: lsp-declaration-directives, Property N: [property text]**

1. **Property 1: Directive Parsing Completeness**
   - Generate random valid R symbol names (alphanumeric, dots, underscores)
   - Generate random directive forms (all 4 variable synonyms, all 4 function synonyms)
   - Generate random syntax variants (with/without colon, with/without quotes)
   - Verify parsing extracts correct symbol name and kind
   - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 2.1, 2.2, 2.3, 2.4, 2.5_

2. **Property 2: Required @ Prefix**
   - Generate directive-like comments without @ prefix
   - Verify no declared symbols are extracted
   - _Requirements: 1.6, 2.6_

3. **Property 3: Metadata Serialization Round-Trip**
   - Generate CrossFileMetadata with random declared symbols
   - Serialize to JSON and deserialize
   - Verify equality of declared_variables and declared_functions
   - _Requirements: 3.3_

4. **Property 4: Position-Aware Scope Inclusion**
   - Generate files with declarations at random line positions
   - Generate random query positions
   - Verify scope inclusion follows position rules
   - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5_

5. **Property 5: Diagnostic Suppression**
   - Generate files with declarations and usages at various positions
   - Run diagnostic collection
   - Verify suppression follows position rules
   - _Requirements: 5.1, 5.2, 5.3, 5.4_

6. **Property 6: Completion Inclusion with Correct Kind**
   - Generate files with variable and function declarations
   - Request completions at positions after declarations
   - Verify declared symbols appear with correct CompletionItemKind
   - _Requirements: 6.1, 6.2, 6.3, 6.4_

7. **Property 7: Cross-File Declaration Inheritance**
   - Generate parent files with declarations and source() calls
   - Generate child files
   - Verify declared symbol availability follows source() position
   - Include tests with `local=TRUE` to verify declarations still propagate
   - _Requirements: 9.1, 9.2, 9.3, 9.4_

8. **Property 8: Interface Hash Sensitivity**
   - Generate files with and without declarations
   - Compute interface hash before and after changes
   - Verify hash changes when declarations change
   - Verify hash stable when non-declaration content changes
   - _Requirements: 10.1, 10.2, 10.3, 10.4_

9. **Property 9: Conflicting Declaration Resolution**
   - Generate files with same symbol declared as both variable and function
   - Verify later declaration determines symbol kind for completions
   - Verify first declaration is used for go-to-definition
   - Verify diagnostic suppression applies regardless of declaration order
   - _Requirements: 11.1, 11.2, 11.3, 11.4_

10. **Property 10: Workspace Index Declaration Extraction**
    - Generate files with declarations, index them via workspace indexer
    - Verify declared symbols are available when file is in dependency chain
    - Verify re-opening file updates declarations from live content
    - _Requirements: 12.1, 12.2, 12.3_

### Integration Tests

1. **Cross-File Declaration Inheritance**
   - Parent file with declaration before source()
   - Verify child file has access to declared symbol
   - Test with `local=TRUE` source() calls - declarations should still propagate

2. **Workspace Index Integration**
   - Index file with declarations (without opening)
   - Open dependent file that sources the indexed file
   - Verify declared symbols from indexed file are available

3. **LSP Feature Integration**
   - Test completion includes declared symbols
   - Test hover shows declaration info with correct format
   - Test go-to-definition navigates to directive

## Implementation Notes

### Regex Pattern Design

The regex patterns follow the existing directive pattern style:
- `#\s*` - Comment start with optional whitespace
- `@lsp-(?:...)` - Required `@` prefix with directive name alternatives
- `\s*:?\s*` - Optional colon with surrounding whitespace
- `(?:"([^"]+)"|'([^']+)'|(\S+))` - Quoted or unquoted symbol name

### Timeline Ordering

Declaration events are inserted into the timeline at their directive line position with column set to `u32::MAX` (end-of-line sentinel). This ensures:

1. The declared symbol is NOT available on the directive line itself
2. The declared symbol IS available starting from the next line (line N+1)
3. Correct ordering relative to other scope events on the same line

This "available on next line" semantics matches the behavior of `source()` calls: symbols from a sourced file are available after the source() statement, not on the same line. Using the end-of-line sentinel column achieves this without special-casing the comparison logic.

### Interface Hash Computation

The interface hash must include declared symbols to ensure proper cache invalidation. The hash computation should:
1. Sort declared symbols by name for determinism
2. Include both name and kind (function/variable) in hash
3. Maintain existing hash computation for regular symbols and packages

### Cross-File Propagation

Declared symbols follow position-based inheritance rules:
- Available in child files sourced after the declaration line
- Not available in child files sourced before the declaration line
- **Note on `local=TRUE`**: Unlike regular symbols (which are not exported when `local=TRUE`), declared symbols are always visible in child files regardless of the `local` parameter. This is because declarations describe symbol existence for diagnostic suppression purposes, not runtime scoping behavior. A declared symbol represents "this symbol will exist at runtime" rather than "this symbol should be exported."
