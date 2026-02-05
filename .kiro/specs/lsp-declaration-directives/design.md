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
    /// Column (always 0 for directives)
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

*For any* file with declaration directives at various line positions and any query position (line, column), a declared symbol SHALL appear in scope if and only if its directive line is less than or equal to the query line.

**Validates: Requirements 4.1, 4.2, 4.3, 4.4, 4.5**

### Property 5: Diagnostic Suppression

*For any* file with a declaration directive and a usage of the declared symbol name (case-sensitive match), the undefined variable diagnostic SHALL be suppressed if and only if the usage position is after the declaration directive line.

**Validates: Requirements 5.1, 5.2, 5.3, 5.4**

### Property 6: Completion Inclusion with Correct Kind

*For any* completion request at a position after a declaration directive, the declared symbol SHALL appear in the completion list with `CompletionItemKind::FUNCTION` for function declarations and `CompletionItemKind::VARIABLE` for variable declarations.

**Validates: Requirements 6.1, 6.2, 6.3, 6.4**

### Property 7: Cross-File Declaration Inheritance

*For any* parent file with a declaration directive and a `source()` call, the declared symbol SHALL be available in the sourced child file if and only if the declaration directive appears before the `source()` call in the parent file.

**Validates: Requirements 9.1, 9.2, 9.3**

### Property 8: Interface Hash Sensitivity

*For any* file, the interface hash SHALL change when a declaration directive is added, removed, or when a declared symbol's name changes. The hash SHALL remain stable when only non-declaration content changes.

**Validates: Requirements 10.1, 10.2, 10.3, 10.4**

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

When the same symbol is declared multiple times:
- All declarations are stored in metadata
- The first declaration (by line number) takes precedence for go-to-definition
- All declarations suppress diagnostics for that symbol name

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

3. **Scope Resolution Tests** (`scope.rs`)
   - Test declared symbols appear in scope after directive line
   - Test declared symbols do NOT appear before directive line
   - Test function vs variable kind distinction
   - Test timeline ordering with mixed events

4. **Diagnostic Tests** (`handlers.rs`)
   - Test undefined variable suppression for declared variables
   - Test undefined variable suppression for declared functions
   - Test diagnostics still emitted for undeclared symbols
   - Test diagnostics emitted when usage is before declaration

5. **Completion Tests** (`handlers.rs`)
   - Test declared symbols appear in completions
   - Test correct CompletionItemKind for functions vs variables

6. **Hover Tests** (`handlers.rs`)
   - Test hover shows declaration info
   - Test hover includes directive line number

7. **Go-to-Definition Tests** (`handlers.rs`)
   - Test navigation to directive line

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
   - _Requirements: 9.1, 9.2, 9.3_

8. **Property 8: Interface Hash Sensitivity**
   - Generate files with and without declarations
   - Compute interface hash before and after changes
   - Verify hash changes when declarations change
   - Verify hash stable when non-declaration content changes
   - _Requirements: 10.1, 10.2, 10.3, 10.4_

### Integration Tests

1. **Cross-File Declaration Inheritance**
   - Parent file with declaration before source()
   - Verify child file has access to declared symbol

2. **LSP Feature Integration**
   - Test completion includes declared symbols
   - Test hover shows declaration info
   - Test go-to-definition navigates to directive

## Implementation Notes

### Regex Pattern Design

The regex patterns follow the existing directive pattern style:
- `#\s*` - Comment start with optional whitespace
- `@lsp-(?:...)` - Required `@` prefix with directive name alternatives
- `\s*:?\s*` - Optional colon with surrounding whitespace
- `(?:"([^"]+)"|'([^']+)'|(\S+))` - Quoted or unquoted symbol name

### Timeline Ordering

Declaration events are inserted into the timeline at their directive line position with column 0. This ensures correct ordering relative to other scope events (definitions, source calls, removals).

### Interface Hash Computation

The interface hash must include declared symbols to ensure proper cache invalidation. The hash computation should:
1. Sort declared symbols by name for determinism
2. Include both name and kind (function/variable) in hash
3. Maintain existing hash computation for regular symbols and packages

### Cross-File Propagation

Declared symbols follow the same inheritance rules as regular symbols:
- Available in child files sourced after the declaration
- Not available in child files sourced before the declaration
- Respect `local=TRUE` scoping rules
