# Design Document: Enhanced Variable Detection and Hover Information

## Overview

This design enhances Rlsp's variable definition detection and hover information capabilities. The system will recognize loop iterator variables and function-local variables to eliminate false-positive undefined variable warnings, and provide enhanced hover information showing definition statements with hyperlinked file locations.

The implementation leverages Rlsp's existing tree-sitter-based parsing and cross-file scope resolution infrastructure. All enhancements are static analysis only - no R runtime is required.

## Architecture

### High-Level Components

1. **Scope Resolution Enhancement** (`crates/rlsp/src/cross_file/scope.rs`)
   - Extend `ScopeEvent` enum to handle loop iterators and function parameters
   - Modify `collect_definitions()` to detect for loop iterators
   - Add function scope boundary tracking

2. **Hover Provider Enhancement** (`crates/rlsp/src/handlers.rs`)
   - Extract definition statements from tree-sitter nodes
   - Format definition statements as R code blocks
   - Generate hyperlinked file locations
   - Handle cross-file definitions

3. **Tree-Sitter Integration**
   - Use tree-sitter-r grammar to identify for loops and function definitions
   - Extract source text from definition nodes
   - Track scope boundaries using node byte ranges

## Components and Interfaces

### 1. Enhanced Scope Events

The existing `ScopeEvent` enum already supports the necessary definition types. Loop iterators should be treated as regular variable definitions, not as special scoped constructs:

```rust
pub enum ScopeEvent {
    /// A symbol definition at a specific position
    Def {
        line: u32,
        column: u32,
        symbol: ScopedSymbol,
    },
    /// A source() call that introduces symbols from another file
    Source {
        line: u32,
        column: u32,
        source: ForwardSource,
    },
    /// A function scope boundary (start and end positions)
    FunctionScope {
        start_line: u32,
        start_column: u32,
        end_line: u32,
        end_column: u32,
        /// Parameters defined in this function
        parameters: Vec<ScopedSymbol>,
    },
}
```

Note: Loop iterators do NOT need a special scope boundary event because in R, they persist after the loop completes.

### 2. Loop Iterator Detection

Add function to detect for loop iterators and treat them as regular variable definitions:

```rust
fn try_extract_for_loop_iterator(node: Node, content: &str, uri: &Url) -> Option<ScopedSymbol> {
    // node.kind() == "for_statement"
    // Extract iterator variable from: for (iterator in sequence)
    // Return ScopedSymbol for the iterator (treated as a regular variable)
    // The iterator is defined at the position of the for statement
}
```

Tree-sitter-r grammar structure for for loops:
- Node kind: `"for_statement"`
- Child fields: `"variable"` (iterator), `"sequence"`, `"body"`

**Important**: In R, loop iterators persist after the loop completes, so they should be added to the timeline as regular `Def` events, not as scoped constructs.

### 3. Function Scope Detection

Add function to detect function definitions and their parameters:

```rust
fn try_extract_function_scope(node: Node, content: &str, uri: &Url) -> Option<ScopeEvent> {
    // node.kind() == "function_definition"
    // Extract parameters from parameters node
    // Return FunctionScope event with parameters and body boundaries
}
```

Tree-sitter-r grammar structure for functions:
- Node kind: `"function_definition"`
- Child fields: `"parameters"`, `"body"`
- Parameters node contains individual `"parameter"` children

### 4. Scope Resolution with Function Boundaries

Modify `scope_at_position_with_graph_recursive()` to handle function scope boundaries:

```rust
// Pseudocode for enhanced scope resolution
for event in timeline {
    match event {
        ScopeEvent::Def { ... } => {
            // Existing logic - includes loop iterators
            // Loop iterators are treated as regular definitions
        }
        ScopeEvent::FunctionScope { start, end, parameters, ... } => {
            // If position is within function body:
            //   - Add parameters to scope
            //   - Track that we're inside a function
            // If position is outside function body:
            //   - Remove function-local symbols from scope
            //   - Remove parameters from scope
        }
    }
}
```

**Key Point**: Loop iterators do NOT need special scope handling because they persist after the loop in R.

### 5. Definition Statement Extraction

Add function to extract definition statements from tree-sitter nodes:

```rust
pub struct DefinitionInfo {
    pub statement: String,
    pub source_uri: Url,
    pub line: u32,
    pub column: u32,
}

fn extract_definition_statement(
    symbol: &ScopedSymbol,
    get_content: impl Fn(&Url) -> Option<String>,
    get_tree: impl Fn(&Url) -> Option<Tree>,
) -> Option<DefinitionInfo> {
    // Get the source file content and tree
    // Find the node at the definition position
    // Extract the complete definition statement
    // Handle multi-line definitions with truncation
}
```

Statement extraction logic:
- For variables: Extract the complete assignment statement
- For functions: Extract function signature and opening brace
- For loop iterators: Extract the for loop header
- For function parameters: Extract the function signature
- Truncate at 10 lines with ellipsis indicator

### 6. Hover Content Formatting

Enhance the `hover()` function to include definition statements and hyperlinks:

```rust
pub fn hover(state: &WorldState, uri: &Url, position: Position) -> Option<Hover> {
    // Existing logic to find symbol...
    
    // Get definition info
    let def_info = extract_definition_statement(symbol, ...)?;
    
    // Format hover content
    let mut value = String::new();
    
    // Add definition statement
    value.push_str(&format!("```r\n{}\n```\n\n", def_info.statement));
    
    // Add file location
    if def_info.source_uri == *uri {
        value.push_str(&format!("this file, line {}", def_info.line + 1));
    } else {
        let relative_path = compute_relative_path(&def_info.source_uri, workspace_root);
        let absolute_path = def_info.source_uri.path();
        value.push_str(&format!(
            "[{}](file://{}), line {}",
            relative_path,
            absolute_path,
            def_info.line + 1
        ));
    }
    
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(node_range),
    })
}
```

### 7. Path Utilities

Add helper functions for path manipulation:

```rust
fn compute_relative_path(target_uri: &Url, workspace_root: Option<&Url>) -> String {
    // Compute relative path from workspace root to target
    // If no workspace root, use filename only
}

fn escape_markdown(text: &str) -> String {
    // Escape markdown special characters in definition statements
}
```

## Data Models

### Enhanced ScopedSymbol

The existing `ScopedSymbol` struct already contains the necessary fields:

```rust
pub struct ScopedSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub source_uri: Url,
    pub defined_line: u32,
    pub defined_column: u32,
    pub signature: Option<String>,
}
```

No changes needed to the data model.

### Scope Context

Add a scope context structure to track function scopes:

```rust
struct ScopeContext {
    /// Stack of active function scopes
    function_scopes: Vec<(u32, u32, u32, u32)>, // (start_line, start_col, end_line, end_col)
}

impl ScopeContext {
    fn is_in_function(&self, line: u32, column: u32) -> bool {
        // Check if position is within any function scope
    }
}
```

Note: Loop scopes are not tracked because loop iterators persist after the loop in R.

## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*


### Property Reflection

After analyzing all acceptance criteria, the following redundancies were identified:

- **Property 1.2**: Consequence of 1.1 - if iterator is in scope, no diagnostic will be emitted (redundant)
- **Properties 10.1 and 10.5**: Both test handling of R assignment operators - can be combined  

Note: Requirements 6.1 and 6.2 from the original analysis were removed because they were based on incorrect assumptions about R's scoping rules. In R, loop iterators persist after the loop completes.

### Correctness Properties

Property 1: Loop iterator scope inclusion
*For any* for loop with an iterator variable, when analyzing scope at any position at or after the for statement, the iterator variable should be included in the available symbols (loop iterators persist in R).
**Validates: Requirements 1.1, 6.1**

Property 2: Loop iterator persistence after loop
*For any* for loop with an iterator variable, when analyzing scope at any position after the loop body completes, the iterator variable should still be included in the available symbols.
**Validates: Requirements 6.2**

Property 3: Nested loop iterator tracking
*For any* nested for loop structure, when analyzing scope at a position after both loops complete, both the outer and inner iterator variables should be available in scope.
**Validates: Requirements 1.3**

Property 4: Loop iterator shadowing
*For any* code where a variable is defined and then a for loop uses the same name as its iterator, when analyzing scope after the for statement, the iterator definition should take precedence over the outer variable definition.
**Validates: Requirements 1.4, 6.3**

Property 5: Function-local variable scope boundaries
*For any* function with local variable definitions, when analyzing scope at any position outside the function body, those local variables should NOT be included in the available symbols.
**Validates: Requirements 7.1**

Property 6: Function parameter scope boundaries
*For any* function with parameters, when analyzing scope at any position outside the function body, those parameters should NOT be included in the available symbols.
**Validates: Requirements 7.2**

Property 7: Function-local undefined variable diagnostics
*For any* code that references a function-local variable outside the function body, the system should emit an undefined variable diagnostic for that reference.
**Validates: Requirements 7.3**

Property 8: Function parameter scope inclusion
*For any* function with parameters, when analyzing scope at any position within the function body, all parameters should be included in the available symbols.
**Validates: Requirements 8.1**

Property 9: Function parameter with default value recognition
*For any* function parameter with or without a default value, the parameter should be recognized and included in the function body scope regardless of whether a default is specified.
**Validates: Requirements 8.2**

Property 10: Variable hover definition extraction
*For any* variable reference, when hovering over it, the hover content should include the complete definition statement extracted from the source text.
**Validates: Requirements 2.1**

Property 11: Function hover signature extraction
*For any* function reference, when hovering over it, the hover content should include the function signature.
**Validates: Requirements 2.2**

Property 12: Multi-line definition handling
*For any* definition statement spanning multiple lines, when hovering over a reference to it, the hover content should include all lines up to 10 lines, with truncation and ellipsis for longer definitions.
**Validates: Requirements 2.4**

Property 13: Markdown code block formatting
*For any* symbol hover, the definition statement should be formatted as a markdown code block with R syntax highlighting (using ```r markers).
**Validates: Requirements 2.5, 5.1**

Property 14: Same-file location format
*For any* symbol defined in the current file, when hovering over a reference to it, the hover content should display "this file, line N" where N is the 1-based line number.
**Validates: Requirements 3.1**

Property 15: Cross-file hyperlink format
*For any* symbol defined in a different file, when hovering over a reference to it, the hover content should display a hyperlink in the format `[relative_path](file:///absolute_path), line N`.
**Validates: Requirements 3.2**

Property 16: File URI protocol
*For any* cross-file symbol hover, the generated URI should use the file:// protocol with an absolute path.
**Validates: Requirements 3.3**

Property 17: Relative path calculation
*For any* cross-file symbol hover with a workspace root, the relative path should be computed relative to the workspace root.
**Validates: Requirements 3.4**

Property 18: LSP Markdown markup kind
*For any* hover response, the MarkupContent kind should be set to MarkupKind::Markdown.
**Validates: Requirements 3.5**

Property 19: Cross-file definition resolution
*For any* symbol defined in a sourced file, when hovering over a reference to it, the hover provider should locate the definition using the cross-file dependency graph.
**Validates: Requirements 4.1**

Property 20: Scope-based definition selection
*For any* symbol with multiple definitions, when hovering over a reference to it, the hover provider should select the definition that is in scope at the reference position.
**Validates: Requirements 4.2**

Property 21: Definition statement and location separation
*For any* hover response with both definition statement and file location, the two should be separated by a blank line.
**Validates: Requirements 5.2**

Property 22: Definition statement truncation
*For any* definition statement exceeding 10 lines, when hovering over a reference to it, the hover content should truncate the statement and append an ellipsis indicator.
**Validates: Requirements 5.3**

Property 23: Indentation preservation
*For any* definition statement with indentation, when hovering over a reference to it, the hover content should preserve the original indentation.
**Validates: Requirements 5.4**

Property 24: Markdown character escaping
*For any* definition statement containing markdown special characters, when hovering over a reference to it, those characters should be escaped in the hover content.
**Validates: Requirements 5.5**

Property 25: Source local=FALSE global scope
*For any* file sourced with local=FALSE, all symbols defined in that file should be available in the global scope.
**Validates: Requirements 9.1**

Property 26: Source local=TRUE function scope
*For any* file sourced with local=TRUE inside a function, all symbols defined in that file should be available only within that function scope.
**Validates: Requirements 9.2**

Property 27: Source local parameter default
*For any* source() call without an explicit local parameter, the system should treat it as local=FALSE.
**Validates: Requirements 9.4**

Property 28: Assignment operator extraction
*For any* symbol defined through any R assignment operator (`<-`, `=`, `<<-`, `->`), when hovering over a reference to it, the hover content should include the complete assignment statement.
**Validates: Requirements 10.1, 10.5**

Property 29: Inline function extraction
*For any* inline function definition, when hovering over a reference to it, the hover content should include the function keyword and signature.
**Validates: Requirements 10.2**

Property 30: Loop iterator definition extraction
*For any* for loop iterator, when hovering over a reference to it within the loop body, the hover content should include the for loop header.
**Validates: Requirements 10.3**

Property 31: Function parameter definition extraction
*For any* function parameter, when hovering over a reference to it within the function body, the hover content should include the function signature.
**Validates: Requirements 10.4**

## Error Handling

### Tree-Sitter Parsing Failures

- If tree-sitter fails to parse a file, scope resolution should gracefully degrade to existing behavior
- Hover should fall back to showing symbol name without definition statement
- No crashes or panics should occur

### Missing Source Files

- If a sourced file cannot be found, scope resolution should continue with available files
- Hover should indicate when a definition file is not available
- Cross-file resolution should handle broken source() references gracefully

### Invalid UTF-16 Conversions

- All byte offset to UTF-16 column conversions should handle invalid UTF-8 gracefully
- Use existing `byte_offset_to_utf16_column()` utility which handles edge cases
- Clamp positions to valid ranges

### Circular Source Dependencies

- Scope resolution already handles cycles with visited set
- No changes needed - existing cycle detection is sufficient

### Malformed R Code

- Tree-sitter produces partial trees for malformed code
- Scope resolution should extract what it can from partial trees
- Hover should work for well-formed symbols even in partially malformed files

## Testing Strategy

### Dual Testing Approach

This feature requires both unit tests and property-based tests for comprehensive coverage:

**Unit Tests** - Focus on:
- Specific examples of for loops, functions, and nested scopes
- Edge cases like empty loop bodies, functions with no parameters
- Integration between scope resolution and hover provider
- Markdown formatting and escaping
- File path calculations

**Property-Based Tests** - Focus on:
- Universal properties that hold for all R code structures
- Randomized generation of for loops, functions, and variable definitions
- Scope resolution correctness across all valid code patterns
- Hover content format consistency

### Property-Based Testing Configuration

- Use `proptest` crate (already used in Rlsp)
- Minimum 100 iterations per property test
- Each property test must reference its design document property
- Tag format: `// Feature: enhanced-variable-detection-hover, Property N: [property text]`

### Test Organization

**Unit Tests** (`crates/rlsp/src/cross_file/scope.rs`):
```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_for_loop_iterator_in_scope() {
        // Test that iterator is available within loop body
    }
    
    #[test]
    fn test_for_loop_iterator_persists_after_loop() {
        // Test that iterator is still available after loop completes (R behavior)
    }
    
    #[test]
    fn test_function_parameter_in_scope() {
        // Test that parameters are available within function
    }
    
    #[test]
    fn test_function_local_variable_out_of_scope() {
        // Test that local variables are not available outside function
    }
}
```

**Property Tests** (`crates/rlsp/src/cross_file/property_tests.rs`):
```rust
proptest! {
    #[test]
    // Feature: enhanced-variable-detection-hover, Property 1: Loop iterator scope inclusion
    fn prop_loop_iterator_in_scope(
        iterator_name in "[a-z][a-z0-9_]*",
        sequence in "[a-z][a-z0-9_]*",
    ) {
        let code = format!("for ({} in {}) {{ x <- {} }}", iterator_name, sequence, iterator_name);
        // Parse and verify iterator is in scope within body
    }
    
    #[test]
    // Feature: enhanced-variable-detection-hover, Property 2: Loop iterator persistence
    fn prop_loop_iterator_persists(
        iterator_name in "[a-z][a-z0-9_]*",
        sequence in "[a-z][a-z0-9_]*",
    ) {
        let code = format!("for ({} in {}) {{ }}\nx <- {}", iterator_name, sequence, iterator_name);
        // Parse and verify iterator IS in scope after loop (R behavior)
    }
}
```

**Hover Tests** (`crates/rlsp/src/handlers.rs`):
```rust
#[cfg(test)]
mod hover_tests {
    #[test]
    fn test_hover_shows_definition_statement() {
        // Test that hovering shows the definition
    }
    
    #[test]
    fn test_hover_same_file_location() {
        // Test "this file, line N" format
    }
    
    #[test]
    fn test_hover_cross_file_hyperlink() {
        // Test hyperlink format for cross-file definitions
    }
}
```

### Integration Testing

Test complete workflows:
1. Parse R file with for loops → verify scope resolution → verify diagnostics
2. Parse R file with functions → verify scope resolution → verify diagnostics
3. Hover over variable → verify definition extraction → verify formatting
4. Hover over cross-file symbol → verify path resolution → verify hyperlink

### Test Data

Use realistic R code patterns:
- Nested for loops with multiple iterators
- Functions with multiple parameters and local variables
- Cross-file source() calls with local=TRUE/FALSE
- Multi-line function definitions
- Code with markdown special characters in strings

