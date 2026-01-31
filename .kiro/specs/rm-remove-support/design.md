# Design Document: rm()/remove() Support

## Overview

This design adds support for tracking variable removals via `rm()` and `remove()` calls in R code. The implementation extends the existing scope tracking system by adding a new `Removal` event type to the scope timeline. When scope is computed at a position, removal events are processed to exclude symbols that have been removed.

The key insight is that `rm()` is the inverse of assignment—it removes symbols from scope rather than adding them. By treating removals as timeline events (like definitions and source calls), we can correctly compute scope at any position while respecting the order of operations.

## Architecture

### Current Architecture

The current scope system in `scope.rs` tracks three types of events in the timeline:
- `Def`: Symbol definitions (assignments, function definitions)
- `Source`: source() calls that bring in symbols from other files
- `FunctionScope`: Function parameter scope boundaries

### Proposed Architecture

We extend the system with a fourth event type:

```
┌─────────────────────────────────────────────────────────────────┐
│                      ScopeEvent Enum                            │
│                                                                 │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐ │
│  │    Def      │  │   Source    │  │    FunctionScope        │ │
│  │  (existing) │  │  (existing) │  │      (existing)         │ │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘ │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │                    Removal (NEW)                         │   │
│  │  - line: u32                                             │   │
│  │  - column: u32                                           │   │
│  │  - symbols: Vec<String>                                  │   │
│  └─────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

### Detection Pipeline

```
┌─────────────────────────────────────────────────────────────────┐
│                    rm() Detection Pipeline                       │
│                                                                 │
│  ┌─────────────┐    ┌─────────────────┐    ┌─────────────────┐ │
│  │  Parse AST  │───▶│ Find rm/remove  │───▶│ Extract Symbols │ │
│  │             │    │     calls       │    │                 │ │
│  └─────────────┘    └─────────────────┘    └─────────────────┘ │
│                                                   │             │
│                                                   ▼             │
│                     ┌─────────────────────────────────────────┐ │
│                     │         Check envir= argument           │ │
│                     │  - If non-default: skip                 │ │
│                     │  - If default/globalenv: process        │ │
│                     └─────────────────────────────────────────┘ │
│                                                   │             │
│                                                   ▼             │
│                     ┌─────────────────────────────────────────┐ │
│                     │      Create Removal Event               │ │
│                     │  - Position (line, column)              │ │
│                     │  - List of symbol names                 │ │
│                     └─────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

## Components and Interfaces

### New ScopeEvent Variant

```rust
/// A scope-introducing event within a file
#[derive(Debug, Clone)]
pub enum ScopeEvent {
    /// A symbol definition at a specific position
    Def { ... },
    /// A source() call that introduces symbols from another file
    Source { ... },
    /// A function definition that introduces parameter scope
    FunctionScope { ... },
    /// NEW: A removal of symbols from scope via rm()/remove()
    Removal {
        line: u32,
        column: u32,
        symbols: Vec<String>,
    },
}
```

### rm() Detection Function

New function in `source_detect.rs` (or a new `rm_detect.rs` module):

```rust
/// Detected rm()/remove() call with extracted symbol names
#[derive(Debug, Clone)]
pub struct RmCall {
    /// 0-based line of the rm() call
    pub line: u32,
    /// 0-based UTF-16 column
    pub column: u32,
    /// Symbol names to remove
    pub symbols: Vec<String>,
}

/// Detect rm() and remove() calls in R code.
/// Returns calls that should affect scope (excludes those with non-default envir=).
pub fn detect_rm_calls(tree: &Tree, content: &str) -> Vec<RmCall>
```

### Symbol Extraction Logic

The detection function must handle multiple patterns:

```rust
/// Extract symbol names from rm() arguments
fn extract_rm_symbols(args_node: &Node, content: &str) -> Vec<String> {
    let mut symbols = Vec::new();
    
    // 1. Bare symbols: rm(x, y, z)
    for arg in positional_args(args_node) {
        if arg.kind() == "identifier" {
            symbols.push(node_text(arg, content).to_string());
        }
    }
    
    // 2. list= argument: rm(list = "x") or rm(list = c("x", "y"))
    if let Some(list_arg) = find_named_arg(args_node, "list") {
        symbols.extend(extract_list_symbols(list_arg, content));
    }
    
    symbols
}

/// Extract symbols from list= argument value
fn extract_list_symbols(value_node: Node, content: &str) -> Vec<String> {
    match value_node.kind() {
        "string" => {
            // rm(list = "x")
            vec![extract_string_content(value_node, content)]
        }
        "call" if is_c_call(value_node, content) => {
            // rm(list = c("x", "y", "z"))
            extract_c_string_args(value_node, content)
        }
        _ => {
            // Dynamic expression - not supported
            vec![]
        }
    }
}
```

### envir= Argument Checking

```rust
/// Check if rm() call has a non-default envir= argument
fn has_non_default_envir(args_node: &Node, content: &str) -> bool {
    if let Some(envir_arg) = find_named_arg(args_node, "envir") {
        let value = node_text(envir_arg, content).trim();
        // Default-equivalent values
        if value == "globalenv()" || value == ".GlobalEnv" {
            return false;
        }
        // Any other value means non-default
        return true;
    }
    // No envir= argument means default
    false
}
```

### Modified compute_artifacts Function

```rust
pub fn compute_artifacts(uri: &Url, tree: &Tree, content: &str) -> ScopeArtifacts {
    let mut artifacts = ScopeArtifacts::default();
    let root = tree.root_node();

    // Existing: Collect definitions from AST
    collect_definitions(root, content, uri, &mut artifacts);

    // Existing: Collect source() calls
    let source_calls = detect_source_calls(tree, content);
    for source in source_calls {
        artifacts.timeline.push(ScopeEvent::Source { ... });
    }

    // NEW: Collect rm()/remove() calls
    let rm_calls = detect_rm_calls(tree, content);
    for rm_call in rm_calls {
        artifacts.timeline.push(ScopeEvent::Removal {
            line: rm_call.line,
            column: rm_call.column,
            symbols: rm_call.symbols,
        });
    }

    // Sort timeline by position
    artifacts.timeline.sort_by_key(|event| match event {
        ScopeEvent::Def { line, column, .. } => (*line, *column),
        ScopeEvent::Source { line, column, .. } => (*line, *column),
        ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
        ScopeEvent::Removal { line, column, .. } => (*line, *column),
    });

    // ... rest of function
}
```

### Modified Scope Resolution

The scope resolution functions need to process `Removal` events:

```rust
fn scope_at_position_with_graph_recursive<F, G>(...) -> ScopeAtPosition {
    // ... existing code ...

    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { ... } => { /* existing */ }
            ScopeEvent::Source { ... } => { /* existing */ }
            ScopeEvent::FunctionScope { ... } => { /* existing */ }
            
            // NEW: Handle removal events
            ScopeEvent::Removal { line: rm_line, column: rm_col, symbols } => {
                // Only process if removal is before the query position
                if (*rm_line, *rm_col) <= (line, column) {
                    // Check function scope - removals inside functions only affect that function
                    let rm_function_scope = artifacts.function_scopes.iter()
                        .filter(|(start_line, start_column, end_line, end_column)| {
                            (*start_line, *start_column) <= (*rm_line, *rm_col) 
                            && (*rm_line, *rm_col) <= (*end_line, *end_column)
                        })
                        .max_by_key(|(start_line, start_column, _, _)| (*start_line, *start_column))
                        .copied();
                    
                    match rm_function_scope {
                        None => {
                            // Global removal - remove from scope
                            for sym in symbols {
                                scope.symbols.remove(sym);
                            }
                        }
                        Some(rm_scope) => {
                            // Function-local removal - only remove if we're in the same function
                            if active_function_scopes.contains(&rm_scope) {
                                for sym in symbols {
                                    scope.symbols.remove(sym);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    scope
}
```

## Data Models

### Tree-Sitter Node Structure for rm() Calls

```
call
├── function: identifier ("rm" or "remove")
└── arguments
    ├── argument (positional - bare symbol)
    │   └── value: identifier
    ├── argument (positional - bare symbol)
    │   └── value: identifier
    └── argument (named - list=)
        ├── name: identifier ("list")
        └── value: string | call
            └── (for c() call)
                └── arguments
                    ├── argument
                    │   └── value: string
                    └── argument
                        └── value: string
```

### Supported Patterns

| Pattern | Extracted Symbols |
|---------|-------------------|
| `rm(x)` | `["x"]` |
| `rm(x, y, z)` | `["x", "y", "z"]` |
| `rm(list = "x")` | `["x"]` |
| `rm(list = c("x", "y"))` | `["x", "y"]` |
| `remove(x)` | `["x"]` |
| `rm(x, list = c("y", "z"))` | `["x", "y", "z"]` |

### Unsupported Patterns (No Symbols Extracted)

| Pattern | Reason |
|---------|--------|
| `rm(list = var)` | Dynamic variable |
| `rm(list = ls())` | Dynamic expression |
| `rm(list = ls(pattern = "..."))` | Pattern-based |
| `rm(x, envir = my_env)` | Non-default environment |



## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

### Property 1: Bare Symbol Extraction

*For any* `rm()` or `remove()` call containing bare symbol arguments, the resulting Removal event SHALL contain exactly those symbol names, regardless of how many symbols are specified or whether they are currently defined in scope.

**Validates: Requirements 1.1, 1.2, 1.3**

### Property 2: remove() Equivalence

*For any* R code using `remove()`, replacing `remove` with `rm` SHALL produce an identical scope timeline (same Removal events with same symbols and positions).

**Validates: Requirements 2.1, 2.2, 2.3**

### Property 3: list= String Literal Extraction

*For any* `rm()` call with a `list=` argument containing string literals (either a single string or a `c()` call with strings), the Removal event SHALL contain exactly those string values as symbol names.

**Validates: Requirements 3.1, 3.2**

### Property 4: Dynamic Expression Filtering

*For any* `rm()` call with a `list=` argument containing a non-literal expression (variable reference, function call other than `c()` with literals, etc.), no Removal event SHALL be created for that call.

**Validates: Requirements 3.3, 3.4**

### Property 5: envir= Argument Filtering

*For any* `rm()` call with an `envir=` argument, a Removal event SHALL be created if and only if the envir value is `globalenv()` or `.GlobalEnv` (or omitted entirely).

**Validates: Requirements 4.1, 4.2, 4.3**

### Property 6: Function Scope Isolation

*For any* `rm()` call inside a function body, the removal SHALL only affect scope queries within that function body. Scope queries outside the function (before or after) SHALL NOT be affected by the removal.

**Validates: Requirements 5.1, 5.2, 5.3**

### Property 7: Cross-File Removal Propagation

*For any* file that sources another file defining symbol `s` and then calls `rm(s)`, scope queries after the `rm()` call SHALL NOT include `s`, while scope queries between the `source()` and `rm()` calls SHALL include `s`.

**Validates: Requirements 6.1, 6.2, 6.3**

### Property 8: Timeline-Based Scope Resolution

*For any* sequence of definitions and removals of a symbol, scope at position P SHALL include the symbol if and only if there exists a definition before P with no removal between that definition and P.

**Validates: Requirements 7.1, 7.2, 7.3, 7.4**

## Error Handling

### Invalid AST Nodes

If the tree-sitter parser produces an error node or missing node within an `rm()` call, the detection should skip that call entirely rather than producing partial results.

### Edge Cases

1. **Empty rm() call**: `rm()` with no arguments should produce no Removal events
2. **Mixed arguments**: `rm(x, list = c("y", "z"))` should extract all three symbols
3. **Duplicate symbols**: `rm(x, x)` or `rm(list = c("x", "x"))` should handle gracefully (deduplicate or allow duplicates - both are acceptable)
4. **Empty strings**: `rm(list = "")` should be handled gracefully (skip empty string or include it)
5. **Whitespace in strings**: `rm(list = " x ")` - the string content should be used as-is (R would fail, but we record it)

### Malformed Patterns

| Pattern | Behavior |
|---------|----------|
| `rm(x + y)` | Skip - not an identifier |
| `rm(1)` | Skip - not an identifier |
| `rm("x")` | Skip - string in positional arg (not list=) |
| `rm(list = 123)` | Skip - not a string or c() call |

## Testing Strategy

### Dual Testing Approach

Both unit tests and property-based tests are required for comprehensive coverage:

- **Unit tests**: Verify specific examples, edge cases, and error conditions
- **Property tests**: Verify universal properties across generated inputs

### Property-Based Testing Configuration

- **Library**: `proptest` (already used in the codebase)
- **Minimum iterations**: 100 per property test
- **Tag format**: `Feature: rm-remove-support, Property N: {property_text}`

### Test Categories

#### Unit Tests

1. **Detection tests** (in `rm_detect.rs` or `source_detect.rs`):
   - `rm(x)` - single bare symbol
   - `rm(x, y, z)` - multiple bare symbols
   - `remove(x)` - alias function
   - `rm(list = "x")` - single string
   - `rm(list = c("x", "y"))` - character vector
   - `rm(x, envir = my_env)` - non-default envir (should skip)
   - `rm(x, envir = globalenv())` - default-equivalent envir

2. **Scope resolution tests** (in `scope.rs`):
   - Define then remove - symbol not in scope after removal
   - Remove then define - symbol in scope after definition
   - Define, remove, define - symbol in scope after second definition
   - Position-aware queries at different points in timeline

3. **Function scope tests**:
   - `rm()` inside function doesn't affect global scope
   - `rm()` at global level affects global scope
   - Nested functions with removals

4. **Cross-file tests**:
   - Source file, remove symbol from sourced file
   - Backward directive with removal in parent

#### Property Tests

Each correctness property (1-8) should have a corresponding property-based test that:
1. Generates random R code matching the property's pattern
2. Parses the code and extracts artifacts/computes scope
3. Verifies the property holds

### Test File Location

- Detection tests: `crates/rlsp/src/cross_file/source_detect.rs` (or new `rm_detect.rs`)
- Scope tests: `crates/rlsp/src/cross_file/scope.rs`
- Property tests: `crates/rlsp/src/cross_file/property_tests.rs`

### Generator Strategies

For property tests, we need generators for:

```rust
/// Generate valid R identifier names
fn r_identifier() -> impl Strategy<Value = String>

/// Generate rm() calls with bare symbols
fn rm_bare_symbols(symbols: Vec<String>) -> String {
    format!("rm({})", symbols.join(", "))
}

/// Generate rm() calls with list= argument
fn rm_list_strings(symbols: Vec<String>) -> String {
    let quoted: Vec<_> = symbols.iter().map(|s| format!("\"{}\"", s)).collect();
    if quoted.len() == 1 {
        format!("rm(list = {})", quoted[0])
    } else {
        format!("rm(list = c({}))", quoted.join(", "))
    }
}

/// Generate code with definition and removal sequence
fn def_rm_sequence(symbol: String, positions: Vec<DefOrRm>) -> String
```
