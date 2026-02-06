# Design Document: Document and Workspace Symbols Enhancement

## Overview

This design enhances Raven's document symbol and workspace symbol providers to deliver hierarchical outlines, accurate range information, R code section support, richer SymbolKind mapping, and improved workspace symbol search. The implementation transforms the current flat `SymbolInformation[]` response into a nested `DocumentSymbol[]` structure that supports VS Code's outline view, breadcrumb navigation, and Ctrl+T workspace search.

The design leverages the existing tree-sitter parsing infrastructure and cross-file scope resolution system, extending them with new symbol extraction logic for S4 methods, R6 classes, and R code sections.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         LSP Request Flow                                     │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  textDocument/documentSymbol                workspace/symbol                │
│           │                                        │                        │
│           ▼                                        ▼                        │
│  ┌─────────────────┐                    ┌─────────────────┐                │
│  │ document_symbol │                    │ workspace_symbol│                │
│  │    handler      │                    │    handler      │                │
│  └────────┬────────┘                    └────────┬────────┘                │
│           │                                      │                          │
│           ▼                                      ▼                          │
│  ┌─────────────────┐                    ┌─────────────────┐                │
│  │SymbolExtractor  │                    │ Query Filter    │                │
│  │ (per-document)  │                    │ (case-insens.)  │                │
│  └────────┬────────┘                    └────────┬────────┘                │
│           │                                      │                          │
│           ▼                                      ▼                          │
│  ┌─────────────────────────────────────────────────────────┐               │
│  │              Symbol Collection Pipeline                  │               │
│  │  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────────┐   │               │
│  │  │Sections │ │Assign-  │ │S4/R6    │ │Reserved     │   │               │
│  │  │Detector │ │ments    │ │Methods  │ │Word Filter  │   │               │
│  │  └─────────┘ └─────────┘ └─────────┘ └─────────────┘   │               │
│  └─────────────────────────────────────────────────────────┘               │
│                              │                                              │
│                              ▼                                              │
│  ┌─────────────────────────────────────────────────────────┐               │
│  │              Hierarchy Builder                           │               │
│  │  - Section nesting (by heading level)                   │               │
│  │  - Function body nesting (by position)                  │               │
│  │  - Range/SelectionRange computation                     │               │
│  └─────────────────────────────────────────────────────────┘               │
│                              │                                              │
│                              ▼                                              │
│  ┌─────────────────────────────────────────────────────────┐               │
│  │              Response Builder                            │               │
│  │  - DocumentSymbol[] (hierarchical)                      │               │
│  │  - SymbolInformation[] (flat fallback)                  │               │
│  └─────────────────────────────────────────────────────────┘               │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Components and Interfaces

### 1. DocumentSymbolKind Enum Extension

Extend the internal symbol kind representation to support richer LSP SymbolKind mapping:

```rust
/// Extended symbol kind for document symbols
/// Maps to LSP SymbolKind with richer categorization
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentSymbolKind {
    Function,       // SymbolKind::FUNCTION
    Variable,       // SymbolKind::VARIABLE
    Constant,       // SymbolKind::CONSTANT (ALL_CAPS pattern)
    Class,          // SymbolKind::CLASS (R6Class, setRefClass, setClass)
    Method,         // SymbolKind::METHOD (setMethod)
    Interface,      // SymbolKind::INTERFACE (setGeneric)
    Module,         // SymbolKind::MODULE (R code sections)
}

impl DocumentSymbolKind {
    /// Convert to LSP SymbolKind
    pub fn to_lsp_kind(self) -> SymbolKind {
        match self {
            Self::Function => SymbolKind::FUNCTION,
            Self::Variable => SymbolKind::VARIABLE,
            Self::Constant => SymbolKind::CONSTANT,
            Self::Class => SymbolKind::CLASS,
            Self::Method => SymbolKind::METHOD,
            Self::Interface => SymbolKind::INTERFACE,
            Self::Module => SymbolKind::MODULE,
        }
    }
}
```

### 2. RawSymbol Intermediate Representation

```rust
/// Intermediate symbol representation before hierarchy building
#[derive(Debug, Clone)]
pub struct RawSymbol {
    /// Symbol name
    pub name: String,
    /// Symbol kind
    pub kind: DocumentSymbolKind,
    /// Full range (start of construct to end of value/body)
    pub range: Range,
    /// Selection range (identifier only)
    pub selection_range: Range,
    /// Function parameter signature (for detail field)
    pub detail: Option<String>,
    /// Section heading level (1 = #, 2 = ##, etc.) for sections only
    pub section_level: Option<u32>,
}
```

### 3. SymbolExtractor Component

```rust
/// Extracts symbols from a parsed R document
pub struct SymbolExtractor<'a> {
    text: &'a str,
    root: Node<'a>,
}

impl<'a> SymbolExtractor<'a> {
    pub fn new(text: &'a str, root: Node<'a>) -> Self;
    
    /// Extract all raw symbols from the document
    pub fn extract_all(&self) -> Vec<RawSymbol>;
    
    /// Extract R code sections from comments
    fn extract_sections(&self) -> Vec<RawSymbol>;
    
    /// Extract assignments (functions, variables, constants)
    fn extract_assignments(&self, node: Node) -> Vec<RawSymbol>;
    
    /// Extract S4 method definitions (setMethod, setClass, setGeneric)
    fn extract_s4_methods(&self, node: Node) -> Vec<RawSymbol>;
    
    /// Extract R6 class definitions
    fn extract_r6_classes(&self, node: Node) -> Vec<RawSymbol>;
    
    /// Determine symbol kind from assignment RHS and name
    fn classify_symbol(&self, name: &str, rhs: Node) -> DocumentSymbolKind;
    
    /// Extract function parameter signature
    fn extract_signature(&self, func_node: Node) -> Option<String>;
}
```

### 4. HierarchyBuilder Component

```rust
/// Builds hierarchical DocumentSymbol tree from flat symbols
pub struct HierarchyBuilder {
    symbols: Vec<RawSymbol>,
    line_count: u32,
}

impl HierarchyBuilder {
    pub fn new(symbols: Vec<RawSymbol>, line_count: u32) -> Self;
    
    /// Build hierarchical DocumentSymbol tree
    pub fn build(self) -> Vec<DocumentSymbol>;
    
    /// Nest symbols within sections based on position
    fn nest_in_sections(&mut self);
    
    /// Nest symbols within function bodies based on position
    fn nest_in_functions(&mut self);
    
    /// Compute section ranges (from section comment to next section or EOF)
    fn compute_section_ranges(&mut self);
}
```

### 5. Configuration Extension

```rust
/// Symbol provider configuration
pub struct SymbolConfig {
    /// Maximum workspace symbol results (default: 1000)
    pub workspace_max_results: usize,
}

impl Default for SymbolConfig {
    fn default() -> Self {
        Self {
            workspace_max_results: 1000,
        }
    }
}
```

## Data Models

### Section Pattern Regex

The R code section pattern matches RStudio/VS Code conventions:

```text
Pattern: ^\s*#(#*)\s*(%%)?\s*(\S.+?)\s*(#{4,}|\-{4,}|={4,}|\*{4,}|\+{4,})\s*$

Components:
- ^\s*           - Optional leading whitespace
- #(#*)          - One or more # characters (capture group 1 = heading level)
- \s*(%%)?\s*    - Optional %% marker (RStudio style)
- (\S.+?)        - Section name (capture group 3)
- \s*            - Optional whitespace
- (#{4,}|...)    - Trailing delimiter (4+ of #, -, =, *, +)
- \s*$           - Optional trailing whitespace

Examples:
- # Section Name ----
- ## Subsection ####
- # %% Cell Name ----
- ### Deep Section ========
```

### ALL_CAPS Constant Pattern

```text
Pattern: ^[A-Z][A-Z0-9_.]+$

Rules:
- Must start with uppercase letter
- Contains only uppercase letters, digits, dots, underscores
- Minimum 2 characters total

Examples:
- MAX_VALUE (constant)
- PI (constant)
- API_KEY (constant)
- x (not constant - lowercase)
- A (not constant - single char)
```

### S4 Method Call Patterns

```rust
/// S4 method call detection
enum S4CallType {
    SetMethod,   // setMethod("name", ...)
    SetClass,    // setClass("name", ...)
    SetGeneric,  // setGeneric("name", ...)
}

/// Extract name from S4 call's first string argument
fn extract_s4_name(call_node: Node, text: &str) -> Option<(S4CallType, String)>;
```

### R6 Class Detection

```rust
/// R6 class call detection
fn is_r6_class_call(call_node: Node, text: &str) -> bool {
    // Check for R6Class() or setRefClass() function calls
    let func_name = get_function_name(call_node, text)?;
    matches!(func_name, "R6Class" | "setRefClass")
}
```

## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*



### Property 1: Response Type Selection

*For any* client capability configuration, if `hierarchicalDocumentSymbolSupport` is true, the Document_Symbol_Provider SHALL return `DocumentSymbol[]`; otherwise it SHALL return `SymbolInformation[]`.

**Validates: Requirements 1.1, 1.2**

### Property 2: URI Correctness in Fallback

*For any* document and flat `SymbolInformation[]` response, every symbol's `location.uri` SHALL equal the document's URI.

**Validates: Requirements 1.3**

### Property 3: Range Containment Invariant

*For any* `DocumentSymbol` in the response:
- `range` SHALL span from the start of the construct to the end of its value/body
- `selectionRange` SHALL span only the identifier
- `selectionRange.start >= range.start` AND `selectionRange.end <= range.end`

**Validates: Requirements 2.1, 2.2, 2.3**

### Property 4: Hierarchical Nesting Correctness

*For any* R document with assignments:
- Assignments inside function bodies SHALL appear as children of that function's symbol
- Assignments at top-level SHALL appear as root-level symbols
- Nested function definitions SHALL preserve their nesting depth in the symbol hierarchy

**Validates: Requirements 3.1, 3.2, 3.3**

### Property 5: Section Detection and Nesting

*For any* R document with section comments matching the pattern `^\s*#(#*)\s*(%%)?\s*(\S.+?)\s*(#{4,}|\-{4,}|={4,}|\*{4,}|\+{4,})\s*$`:
- Each matching comment SHALL produce a `DocumentSymbol` with `kind = MODULE`
- Section `range` SHALL span from the comment line to the line before the next section (or EOF)
- Section `selectionRange` SHALL be the comment line only
- Symbols within a section's range SHALL be nested as children
- Sections with more `#` characters SHALL be nested within sections with fewer `#` characters

**Validates: Requirements 4.1, 4.2, 4.3, 4.4, 4.5**

### Property 6: Symbol Kind Classification

*For any* symbol extracted from an R document:
- Identifiers matching `^[A-Z][A-Z0-9_.]+$` (min 2 chars) SHALL have `kind = CONSTANT`
- Assignments with RHS `R6Class()` or `setRefClass()` SHALL have `kind = CLASS`
- Top-level `setMethod()` calls SHALL have `kind = METHOD`
- Top-level `setClass()` calls SHALL have `kind = CLASS`
- Top-level `setGeneric()` calls SHALL have `kind = INTERFACE`
- Other function definitions SHALL have `kind = FUNCTION`
- Other variable assignments SHALL have `kind = VARIABLE`

**Validates: Requirements 5.1, 5.2, 5.3, 5.4, 5.5, 5.6, 5.7**

### Property 7: Function Signature Extraction

*For any* function symbol:
- The `detail` field SHALL contain the parameter list in format `(param1, param2, ...)`
- If the parameter list exceeds 60 characters, it SHALL be truncated with `...` appended

**Validates: Requirements 6.1, 6.2, 6.3**

### Property 8: Reserved Word Filtering

*For any* assignment where the LHS is an R reserved word (if, else, for, while, repeat, in, next, break, TRUE, FALSE, NULL, Inf, NaN, NA, NA_integer_, NA_real_, NA_complex_, NA_character_, function):
- The symbol SHALL NOT appear in document symbol results
- The symbol SHALL NOT appear in workspace symbol results

**Validates: Requirements 7.1, 7.2**

### Property 9: Workspace ContainerName

*For any* workspace symbol from a file with path `/path/to/filename.R`:
- The `containerName` field SHALL equal `filename` (without extension)

**Validates: Requirements 8.1, 8.2**

### Property 10: Workspace Query Filtering

*For any* workspace symbol query with a non-empty query string:
- All returned symbols SHALL have names containing the query (case-insensitive substring match)

**Validates: Requirements 9.1**

### Property 11: Workspace Result Limiting

*For any* workspace symbol query:
- The number of returned symbols SHALL NOT exceed `symbols.workspaceMaxResults` configuration value

**Validates: Requirements 9.2**

### Property 12: Workspace Deduplication

*For any* workspace symbol query where the same symbol exists in multiple sources (open documents, workspace index, legacy indices):
- The symbol SHALL appear exactly once in the results
- Open document symbols SHALL take precedence over indexed symbols

**Validates: Requirements 9.3**

### Property 13: S4 Name Extraction

*For any* S4 method call:
- `setMethod("methodName", ...)` SHALL produce a symbol named `methodName`
- `setClass("ClassName", ...)` SHALL produce a symbol named `ClassName`
- `setGeneric("genericName", ...)` SHALL produce a symbol named `genericName`

**Validates: Requirements 10.1, 10.2, 10.3**

### Property 14: Configuration Validation

*For any* `symbols.workspaceMaxResults` configuration value:
- The default value SHALL be 1000
- Values between 100 and 10000 (inclusive) SHALL be accepted
- Values outside this range SHALL be clamped to the nearest boundary

**Validates: Requirements 11.1, 11.2, 11.3**

## Error Handling

### Invalid AST Nodes

When tree-sitter produces an ERROR node or incomplete parse:
- Skip the malformed construct
- Continue processing sibling and child nodes
- Log a trace-level warning for debugging

### Missing Function Bodies

When a function definition lacks a body (syntax error):
- Create the function symbol with range ending at the last valid token
- Do not attempt to nest children within incomplete functions

### Malformed Section Comments

When a comment partially matches the section pattern:
- Do not create a section symbol
- Process as a regular comment (ignored for symbols)

### Unicode and Encoding

- All positions use UTF-16 code units (LSP standard)
- Use existing `byte_offset_to_utf16_column` utility for conversion
- Handle multi-byte characters correctly in range computation

### Large Files

- No explicit file size limit for symbol extraction
- Symbol extraction is O(n) in AST node count
- Hierarchy building is O(n log n) for section nesting

## Testing Strategy

### Unit Tests

Unit tests verify specific examples and edge cases:

1. **Section pattern matching**: Test various valid and invalid section comment formats
2. **ALL_CAPS detection**: Test boundary cases (single char, mixed case, special chars)
3. **S4 method extraction**: Test setMethod/setClass/setGeneric with various argument formats
4. **R6 class detection**: Test R6Class and setRefClass calls
5. **Reserved word filtering**: Test all 19 reserved words
6. **Signature truncation**: Test exact 60-character boundary
7. **Range computation**: Test multi-line functions, single-line assignments
8. **Nested sections**: Test 1-level, 2-level, 3-level section hierarchies

### Property-Based Tests

Property tests verify universal properties across generated inputs. Each test runs minimum 100 iterations.

**Test Configuration**: Use `proptest` crate with custom strategies for R code generation.

**Property Test Tags**:
- **Feature: document-workspace-symbols, Property 1**: Response type selection
- **Feature: document-workspace-symbols, Property 3**: Range containment invariant
- **Feature: document-workspace-symbols, Property 4**: Hierarchical nesting correctness
- **Feature: document-workspace-symbols, Property 6**: Symbol kind classification
- **Feature: document-workspace-symbols, Property 8**: Reserved word filtering
- **Feature: document-workspace-symbols, Property 9**: Workspace containerName
- **Feature: document-workspace-symbols, Property 10**: Workspace query filtering
- **Feature: document-workspace-symbols, Property 12**: Workspace deduplication
- **Feature: document-workspace-symbols, Property 13**: S4 name extraction

### Integration Tests

Integration tests verify end-to-end LSP behavior:

1. **Document symbol request**: Send `textDocument/documentSymbol` and verify response structure
2. **Workspace symbol request**: Send `workspace/symbol` and verify filtering/limiting
3. **Client capability handling**: Test with/without `hierarchicalDocumentSymbolSupport`
4. **Configuration changes**: Test `symbols.workspaceMaxResults` updates

### Test Data Generators

```rust
/// Generate valid R function definitions
fn arb_function_def() -> impl Strategy<Value = String>;

/// Generate valid R section comments
fn arb_section_comment() -> impl Strategy<Value = String>;

/// Generate ALL_CAPS identifiers
fn arb_constant_name() -> impl Strategy<Value = String>;

/// Generate S4 method calls
fn arb_s4_call() -> impl Strategy<Value = String>;

/// Generate complete R documents with mixed constructs
fn arb_r_document() -> impl Strategy<Value = String>;
```
