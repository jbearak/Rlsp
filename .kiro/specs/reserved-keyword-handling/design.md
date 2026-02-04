# Design Document: Reserved Keyword Handling

## Overview

This design addresses the incorrect handling of R reserved words in the Raven LSP. Currently, the LSP has two bugs:

1. **False definitions**: Code like `else <- 1` incorrectly creates a definition for `else`
2. **False undefined variable diagnostics**: Misplaced `else` reports "Undefined variable: else" instead of relying on tree-sitter parse errors

The solution introduces a centralized `Reserved_Word_Module` that provides a constant list of R reserved words and an `is_reserved_word()` function. This module is then used by:
- Definition extraction (to skip reserved words)
- Undefined variable checking (to skip reserved words)
- Completion generation (to exclude reserved words from identifier completions)
- Document symbol collection (to exclude reserved words)

## Architecture

```mermaid
graph TD
    RWM[reserved_words.rs] --> |is_reserved_word| DE[Definition Extractor<br/>scope.rs]
    RWM --> |is_reserved_word| UVC[Undefined Variable Checker<br/>handlers.rs]
    RWM --> |is_reserved_word| CP[Completion Provider<br/>handlers.rs]
    RWM --> |is_reserved_word| DSP[Document Symbol Provider<br/>handlers.rs]
    
    DE --> |ScopeArtifacts| SR[Scope Resolution]
    UVC --> |Diagnostics| D[diagnostics()]
    CP --> |CompletionItems| C[completion()]
    DSP --> |SymbolInformation| DS[document_symbol()]
```

The `reserved_words` module is a simple, stateless utility that can be called from any component. It uses a static `HashSet` for O(1) lookup performance.

## Components and Interfaces

### Reserved Word Module (`reserved_words.rs`)

A new module at `crates/raven/src/reserved_words.rs`:

```rust
use std::collections::HashSet;
use std::sync::OnceLock;

/// R reserved words that cannot be used as identifiers.
/// These are language keywords and special constants.
static RESERVED_WORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();

/// Check if a name is an R reserved word.
/// 
/// Reserved words cannot be used as variable or function names.
/// This includes control flow keywords (if, else, for, while, etc.)
/// and special constants (TRUE, FALSE, NULL, NA variants, Inf, NaN).
pub fn is_reserved_word(name: &str) -> bool {
    let reserved = RESERVED_WORDS.get_or_init(|| {
        let mut set = HashSet::new();
        // Control flow keywords
        set.insert("if");
        set.insert("else");
        set.insert("repeat");
        set.insert("while");
        set.insert("function");
        set.insert("for");
        set.insert("in");
        set.insert("next");
        set.insert("break");
        // Logical constants
        set.insert("TRUE");
        set.insert("FALSE");
        // Null value
        set.insert("NULL");
        // Special numeric values
        set.insert("Inf");
        set.insert("NaN");
        // NA variants
        set.insert("NA");
        set.insert("NA_integer_");
        set.insert("NA_real_");
        set.insert("NA_complex_");
        set.insert("NA_character_");
        set
    });
    reserved.contains(name)
}
```

### Modified Components

#### Definition Extractor (`scope.rs`)

The `try_extract_assignment` function is modified to check for reserved words:

```rust
fn try_extract_assignment(node: Node, content: &str, uri: &Url) -> Option<ScopedSymbol> {
    // ... existing code to extract name ...
    let name = node_text(lhs, content).to_string();
    
    // Skip reserved words - they cannot be valid definitions
    if crate::reserved_words::is_reserved_word(&name) {
        return None;
    }
    
    // ... rest of existing code ...
}
```

#### Undefined Variable Checker (`handlers.rs`)

The `collect_undefined_variables_position_aware` function is modified to skip reserved words early:

```rust
for (name, usage_node) in used {
    // Skip reserved words BEFORE any other checks
    if crate::reserved_words::is_reserved_word(&name) {
        continue;
    }
    
    // ... existing checks for builtins, scope, packages ...
}
```

#### Completion Provider (`handlers.rs`)

The `collect_document_completions` function is modified to skip reserved words:

```rust
fn collect_document_completions(...) {
    // ... existing code to extract name ...
    let name = node_text(lhs, text).to_string();
    
    // Skip reserved words - they shouldn't appear as identifier completions
    if crate::reserved_words::is_reserved_word(&name) {
        // Don't add to completions, but continue recursion
    } else if !seen.contains(&name) {
        // ... existing completion item creation ...
    }
    
    // ... recurse into children ...
}
```

#### Document Symbol Provider (`handlers.rs`)

The `collect_symbols` function is modified to skip reserved words:

```rust
fn collect_symbols(node: Node, text: &str, symbols: &mut Vec<SymbolInformation>) {
    if node.kind() == "binary_operator" {
        // ... existing code to extract name ...
        let name = node_text(lhs, text).to_string();
        
        // Skip reserved words
        if crate::reserved_words::is_reserved_word(&name) {
            // Don't add to symbols, but continue recursion
        } else {
            // ... existing symbol creation ...
        }
    }
    // ... recurse into children ...
}
```

## Data Models

### Reserved Word List

The complete list of reserved words for this feature:

| Category | Words |
|----------|-------|
| Control Flow | `if`, `else`, `repeat`, `while`, `function`, `for`, `in`, `next`, `break` |
| Logical Constants | `TRUE`, `FALSE` |
| Null | `NULL` |
| Special Numeric | `Inf`, `NaN` |
| NA Variants | `NA`, `NA_integer_`, `NA_real_`, `NA_complex_`, `NA_character_` |

This list is based on R's official reserved words. Note that `library`, `require`, `return`, `print` are NOT reserved words - they are regular functions that can be redefined.

### Lookup Performance

The `HashSet` provides O(1) average-case lookup. The set is initialized once via `OnceLock` and reused for all subsequent calls, making the check extremely fast.



## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

### Property 1: Reserved Word Identification

*For any* string that is in the set of R reserved words (`if`, `else`, `repeat`, `while`, `function`, `for`, `in`, `next`, `break`, `TRUE`, `FALSE`, `NULL`, `Inf`, `NaN`, `NA`, `NA_integer_`, `NA_real_`, `NA_complex_`, `NA_character_`), `is_reserved_word()` SHALL return `true`. *For any* string that is a valid R identifier but NOT in this set, `is_reserved_word()` SHALL return `false`.

**Validates: Requirements 1.1, 1.2**

### Property 2: Definition Extraction Exclusion

*For any* R code containing an assignment where the left-hand side is a reserved word (e.g., `else <- 1`, `if <- function() {}`), the definition extractor SHALL NOT include that reserved word in either the exported interface or the scope timeline.

**Validates: Requirements 2.1, 2.2, 2.3, 2.4**

### Property 3: Undefined Variable Check Exclusion

*For any* R code containing a reserved word used as an identifier (in any syntactic position), the undefined variable checker SHALL NOT emit an "Undefined variable" diagnostic for that reserved word.

**Validates: Requirements 3.1, 3.2, 3.3, 3.4**

### Property 4: Completion Exclusion

*For any* R code containing assignments to reserved words, when generating identifier completions, the completion provider SHALL NOT include those reserved words in the completion list (though they may still appear as keyword completions from the separate keyword list).

**Validates: Requirements 5.1, 5.2**

### Property 5: Document Symbol Exclusion

*For any* R code containing assignments to reserved words, the document symbol provider SHALL NOT include those reserved words in the symbol list.

**Validates: Requirements 6.1, 6.2**

## Error Handling

### Invalid Input Handling

The `is_reserved_word()` function handles all string inputs gracefully:
- Empty strings return `false` (not a reserved word)
- Strings with special characters return `false` (not a reserved word)
- Case-sensitive matching: `TRUE` is reserved, but `true` is not

### Parse Error Preservation

When reserved words appear in invalid positions (e.g., `else` without preceding `if`), tree-sitter will report parse errors. This feature does NOT suppress or modify those errors. The only change is that we no longer report "Undefined variable: else" for such cases—the parse error is the correct diagnostic.

### Edge Cases

| Input | Expected Behavior |
|-------|-------------------|
| `else <- 1` | No definition created, no undefined variable warning, parse error from tree-sitter |
| `if <- function() {}` | No definition created, no undefined variable warning, parse error from tree-sitter |
| `TRUE <- FALSE` | No definition created, no undefined variable warning |
| `myelse <- 1` | Normal definition created (not a reserved word) |
| `ELSE <- 1` | Normal definition created (case-sensitive, `ELSE` is not reserved) |

## Testing Strategy

### Unit Tests

Unit tests verify specific examples and edge cases:

1. **Reserved word module tests**:
   - Test each reserved word returns `true`
   - Test common non-reserved identifiers return `false`
   - Test edge cases: empty string, case variations, similar names

2. **Definition extraction tests**:
   - Test `else <- 1` produces no definition
   - Test `if <- function() {}` produces no definition
   - Test `myelse <- 1` produces a definition (not reserved)

3. **Undefined variable tests**:
   - Test `else` alone doesn't produce undefined variable diagnostic
   - Test `if` alone doesn't produce undefined variable diagnostic
   - Test actual undefined variables still produce diagnostics

4. **Completion tests**:
   - Test reserved word assignments don't appear in completions
   - Test normal assignments still appear in completions
   - Test keyword completions still include reserved words

5. **Document symbol tests**:
   - Test reserved word assignments don't appear in symbols
   - Test normal assignments still appear in symbols

### Property-Based Tests

Property-based tests verify universal properties across many generated inputs. Each test runs minimum 100 iterations.

**Test Configuration**:
- Framework: `proptest` (Rust property-based testing library)
- Iterations: 100+ per property
- Each test tagged with: **Feature: reserved-keyword-handling, Property N: [property text]**

**Property Test Implementations**:

1. **Property 1 Test**: Generate random strings from the reserved word set and verify `is_reserved_word()` returns `true`. Generate random valid R identifiers not in the set and verify it returns `false`.

2. **Property 2 Test**: Generate R code with assignments to randomly selected reserved words. Parse and extract definitions. Verify the reserved word does not appear in exported interface or timeline.

3. **Property 3 Test**: Generate R code with reserved words used as identifiers. Run undefined variable checking. Verify no "Undefined variable" diagnostic is emitted for reserved words.

4. **Property 4 Test**: Generate R code with assignments to reserved words. Generate completions. Verify reserved words don't appear in identifier completion items.

5. **Property 5 Test**: Generate R code with assignments to reserved words. Collect document symbols. Verify reserved words don't appear in symbol list.
