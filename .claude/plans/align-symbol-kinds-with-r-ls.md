# Symbol Icon and Color Alignment with Official R Language Server

## Context

The official R language server (r-language-server) uses specific LSP `CompletionItemKind` and `SymbolKind` values that determine how symbols appear in editors like VS Code (icons) and Zed (icons + colors). Raven currently uses different kind assignments, which may cause inconsistent visual representation between the two language servers.

This plan documents the differences and proposes changes to align Raven's behavior with the official R language server for consistency across editors.

## Research Findings

### Official R Language Server Symbol Classification

**CompletionItemKind assignments:**

| Symbol Type | Kind | Value | Detail Field | Context |
|-------------|------|-------|--------------|---------|
| Constants (TRUE, FALSE, NULL, NA, Inf, NaN) | `Constant` | 21 | - | Built-in R constants |
| Packages (installed packages) | `Module` | 9 | - | Package names in library() context |
| Function parameters | `Variable` | 6 | `"parameter"` | Function argument completions |
| Functions (all sources) | `Function` | 3 | `"{pkg}"` or `"[workspace]"` or `"[scope]"` | Callable functions |
| Non-function variables | `Field` | 5 | `"{pkg}"` or `"[workspace]"` or `"[scope]"` | Data objects, variables |
| Lazy data (package datasets) | `Field` | 5 | `"{pkg}"` | Package data objects |
| Text matches (fallback) | `Text` | 1 | - | Token-based completions |

**SymbolKind for document symbols:**

| Symbol Type | Kind | Value | Logic |
|-------------|------|-------|-------|
| Functions | `Function` | 12 | `type == "function"` |
| Booleans | `Boolean` | 17 | `typeof(expr) == "logical"` |
| Numbers | `Number` | 16 | `typeof(expr) in c("integer", "double", "complex")` |
| Integers | `Number` | 16 | `typeof(expr) == "integer"` |
| Doubles | `Number` | 16 | `typeof(expr) == "double"` |
| Complex | `Number` | 16 | `typeof(expr) == "complex"` |
| Strings | `String` | 15 | `typeof(expr) == "character"` |
| Arrays | `Array` | 18 | `type == "array"` (from c(), matrix(), array() calls) |
| Lists | `Struct` | 23 | `type == "list"` |
| NULL | `Null` | 21 | `type == "NULL"` |
| Classes | `Class` | 5 | `grepl("R6Class", func)` |
| Default fallback | `Field` | 8 | All other assignments |

**Detail field conventions:**
- Package symbols: `"{package_name}"` (e.g., `"{dplyr}"`)
- Workspace symbols: `"[workspace]"`
- Local scope symbols: `"[scope]"`
- Parameters: `"parameter"`

**Sort order prefixes (sortText):**
- Function arguments: `"0-"` (highest priority)
- Local scope: `"1-"`
- Workspace: `"2-"`
- Imported objects: `"3-"`
- Package globals: `"4-"`
- Text tokens: `"5-"` (lowest priority)

### Raven's Current Implementation

**CompletionItemKind assignments:**

| Symbol Type | Kind | Value | Detail Field |
|-------------|------|-------|--------------|
| Keywords | `KEYWORD` | 14 | - |
| Package exports (all) | `FUNCTION` | 3 | `"{pkg}"` |
| Cross-file functions | `FUNCTION` | 3 | `"from {path}"` |
| Cross-file variables | `VARIABLE` | 6 | `"from {path}"` |
| Declared variables (@lsp-var) | `VARIABLE` | 6 | - |
| Declared functions (@lsp-func) | `FUNCTION` | 3 | - |

**SymbolKind for document symbols:**

| Symbol Type | Kind | Value | Logic |
|-------------|------|-------|-------|
| Functions | `FUNCTION` | 12 | Detected via tree-sitter |
| Variables | `VARIABLE` | 13 | Default for assignments |
| Constants | `CONSTANT` | 14 | ALL_CAPS pattern (min 2 chars) |
| Classes | `CLASS` | 5 | R6Class, setRefClass, setClass |
| Methods | `METHOD` | 6 | setMethod() calls |
| Generics | `INTERFACE` | 11 | setGeneric() calls |
| Sections | `MODULE` | 2 | R code sections (# Section ----) |

**Detail field:**
- Package exports: `"{package_name}"` ✓ (aligned)
- Cross-file symbols: `"from {path}"` (keep this - more informative than R-LS)

**Sort order:**
- No sortText implementation (alphabetical by default)

## Key Differences Causing Visual Inconsistencies

### 1. CompletionItemKind Mismatches

**Critical difference:** Raven uses `VARIABLE` (6) for non-function symbols, while R-LS uses `FIELD` (5).

**Why this matters:**
- Editors like Zed assign colors based on CompletionItemKind
- `Field` is semantically for "object properties/struct fields"
- `Variable` is for "standalone variables"
- R package exports (non-function data) are conceptually "fields" of the package namespace

**Impact:** Different icons and colors for package data objects and variables in completions.

**Example mismatch:**
```r
library(datasets)
mtcars  # R-LS: Field (5), Raven: Variable (6) or Function (3)
```

### 2. Missing R Constant Classification

R-LS uses `Constant` (21) for built-in constants: `TRUE`, `FALSE`, `NULL`, `NA`, `Inf`, `NaN`.

Raven treats these as keywords or doesn't provide special completion for them.

**Impact:** R constants may show as keywords instead of constants, different icon/color.

### 3. Detail Field Format

R-LS uses:
- `"[workspace]"` for symbols from open documents
- `"[scope]"` for local function scope symbols
- `"{package}"` for package symbols

Raven uses:
- `"from {path}"` for cross-file symbols (shows full file path)
- `"{package}"` for package symbols ✓

**Decision:** Keep Raven's current approach (`"from {path}"`). Showing the actual file path and line number is more informative than a generic `"[workspace]"` label, especially in cross-file aware workflows where understanding symbol provenance matters.

### 4. Missing Sort Order Control

R-LS explicitly sets `sortText` with prefixes to control ordering:
1. Parameters (0-)
2. Local scope (1-)
3. Workspace (2-)
4. Imported (3-)
5. Package globals (4-)
6. Text matches (5-)

Raven relies on alphabetical sorting.

**Impact:** Completion list ordering differs between R-LS and Raven. Local symbols don't float to the top.

### 5. Document Symbol Granularity

R-LS uses type-specific SymbolKind values:
- `Boolean`, `Number`, `String`, `Array`, `Struct`, `Null`, `Field`

Raven uses generic:
- `VARIABLE` or `CONSTANT` (ALL_CAPS only)

**Impact:** Document outline icons less granular in Raven.

## Proposed Changes

### Phase 1: Align CompletionItemKind (High Priority)

**Goal:** Match R-LS completion kind assignments to ensure consistent icons/colors.

**Changes:**

1. **Add R constant detection and classification:**
   - Create set of R constants: `["TRUE", "FALSE", "NULL", "NA", "Inf", "NaN", "NA_integer_", "NA_real_", "NA_complex_", "NA_character_"]`
   - Return `CompletionItemKind::CONSTANT` for these in keyword completions
   - File: `crates/raven/src/handlers.rs` in `completion()` handler

2. **Use `Field` for non-function variables:**
   - Package exports (non-function): `CompletionItemKind::FIELD`
   - Cross-file variables: `CompletionItemKind::FIELD`
   - Local variables: `CompletionItemKind::FIELD`
   - Keep `CompletionItemKind::VARIABLE` only for function parameters (future work)
   - Files: `crates/raven/src/handlers.rs` lines ~4579, ~4597-4599

3. **Keep Function kind for functions:**
   - No change needed (already using `FUNCTION`)

4. **Package name completions** (if/when implemented):
   - Use `CompletionItemKind::MODULE` for package names
   - Detail field: package description (optional)

### Phase 2: Add Sort Order Control (Medium Priority)

**Goal:** Float relevant symbols to top of completion list.

**Changes:**

1. **Add sortText field to CompletionItem:**
   - Define sort prefix constants matching R-LS:
     ```rust
     const SORT_PREFIX_SCOPE: &str = "1-";
     const SORT_PREFIX_WORKSPACE: &str = "2-";
     const SORT_PREFIX_PACKAGE: &str = "4-";
     const SORT_PREFIX_KEYWORD: &str = "5-";
     ```

2. **Apply sort prefixes:**
   - Cross-file symbols from same file (local): `"1-{name}"`
   - Cross-file symbols from other files: `"2-{name}"`
   - Package exports: `"4-{name}"`
   - Keywords: `"5-{name}"`
   - File: `crates/raven/src/handlers.rs` in `completion()` handler

3. **Future: Parameter completions:**
   - When implemented, use `"0-{name}"` for function arguments

### Phase 3: Document Symbol Type Granularity (High Priority)

**Goal:** Match R-LS document symbol type granularity through value-based type detection.

**Approach: Option A (Value-Based Type Detection)**

Add analysis of assigned values to determine specific types matching R-LS behavior.

**Implementation:**

1. **Add new DocumentSymbolKind variants:**
   ```rust
   pub enum DocumentSymbolKind {
       Function,      // function(...) { }
       Variable,      // Generic fallback
       Constant,      // ALL_CAPS pattern
       Boolean,       // TRUE, FALSE literals
       Number,        // Numeric literals (integer, double, complex)
       String,        // Character literals
       Null,          // NULL literal
       Array,         // c(), vector(), arrays
       List,          // list() structures
       Class,         // R6Class, setClass
       Method,        // setMethod
       Interface,     // setGeneric
       Module,        // Code sections
   }
   ```

2. **Update LSP kind mapping:**
   ```rust
   impl DocumentSymbolKind {
       pub fn to_lsp_kind(&self) -> SymbolKind {
           match self {
               Self::Function => SymbolKind::FUNCTION,
               Self::Variable => SymbolKind::FIELD,  // Changed from VARIABLE
               Self::Constant => SymbolKind::CONSTANT,
               Self::Boolean => SymbolKind::BOOLEAN,
               Self::Number => SymbolKind::NUMBER,
               Self::String => SymbolKind::STRING,
               Self::Null => SymbolKind::NULL,
               Self::Array => SymbolKind::ARRAY,
               Self::List => SymbolKind::STRUCT,
               Self::Class => SymbolKind::CLASS,
               Self::Method => SymbolKind::METHOD,
               Self::Interface => SymbolKind::INTERFACE,
               Self::Module => SymbolKind::MODULE,
           }
       }
   }
   ```

3. **Add value type detection in SymbolExtractor:**

   **Location:** `crates/raven/src/handlers.rs` in `SymbolExtractor::extract_symbols()`

   **Detection logic:**

   a. **Boolean literals:**
   ```rust
   // Detect TRUE, FALSE in right-hand side of assignment
   if right_node.kind() == "identifier" {
       let text = right_node.utf8_text(source.as_bytes()).unwrap_or("");
       if text == "TRUE" || text == "FALSE" {
           kind = DocumentSymbolKind::Boolean;
       }
   }
   ```

   b. **NULL literal:**
   ```rust
   if right_node.kind() == "null" {
       kind = DocumentSymbolKind::Null;
   }
   ```

   c. **Numeric literals:**
   ```rust
   if matches!(right_node.kind(), "integer" | "float" | "complex") {
       kind = DocumentSymbolKind::Number;
   }
   ```

   d. **String literals:**
   ```rust
   if right_node.kind() == "string" {
       kind = DocumentSymbolKind::String;
   }
   ```

   e. **Array-like calls:**
   ```rust
   // Detect c(), vector(), matrix(), array()
   if right_node.kind() == "call" {
       if let Some(func_node) = right_node.child_by_field_name("function") {
           let func_name = func_node.utf8_text(source.as_bytes()).unwrap_or("");
           if matches!(func_name, "c" | "vector" | "matrix" | "array") {
               kind = DocumentSymbolKind::Array;
           }
       }
   }
   ```

   f. **List calls:**
   ```rust
   // Detect list()
   if right_node.kind() == "call" {
       if let Some(func_node) = right_node.child_by_field_name("function") {
           let func_name = func_node.utf8_text(source.as_bytes()).unwrap_or("");
           if func_name == "list" {
               kind = DocumentSymbolKind::List;
           }
       }
   }
   ```

   g. **Fallback to Variable (mapped to FIELD):**
   ```rust
   // Default for all other assignments
   kind = DocumentSymbolKind::Variable;  // Will map to FIELD in LSP
   ```

4. **Priority order for classification:**
   - Function definitions (highest priority - already detected)
   - Class definitions (R6Class, setClass - already detected)
   - ALL_CAPS constant pattern (already detected)
   - Boolean literals (TRUE, FALSE)
   - NULL literal
   - Numeric literals
   - String literals
   - Array-like calls (c, vector, matrix, array)
   - List calls
   - Variable (default fallback - maps to FIELD)

**Trade-offs:**
- **Pros:**
  - Matches R-LS granularity exactly
  - More informative document outline with specific icons
  - Semantically accurate representation of symbol types
  - Better visual navigation in editors

- **Cons:**
  - Adds complexity to symbol extraction
  - Requires AST value analysis
  - May need special handling for complex expressions
  - Performance consideration (minimal - analysis happens during already-occurring AST walk)

**Edge cases to handle:**
- Multi-line expressions: Use first non-comment node of RHS
- Complex expressions: Fall back to Variable if can't determine type
- Dynamic assignments (assign(), etc.): Fall back to Variable
- Keep existing ALL_CAPS → Constant logic (takes precedence)

### Phase 4: Detail Field Format (No Change)

**Decision:** Keep Raven's current format (`"from {path}"`).

**Rationale:**
- More informative than generic `"[workspace]"` label
- Critical for cross-file aware workflows where symbol provenance matters
- Shows exact file path, enabling quick navigation
- R-LS uses `"[workspace]"` because it doesn't have cross-file tracking
- Raven's cross-file awareness is a differentiating feature

**Keep existing:**
- Cross-file symbols: `"from {path}"`
- Package symbols: `"{package_name}"`

## Implementation Priority

**High Priority (Must-Have for Consistency):**
1. Use `FIELD` for non-function completion items (Phase 1.2)
2. Add `CONSTANT` for R built-in constants (Phase 1.1)
3. Add value-based type detection for document symbols (Phase 3)

**Medium Priority (Nice-to-Have):**
4. Add sortText prefixes for completion ordering (Phase 2)

**Required:**
5. Unit tests for all new functionality (Phase 5)
6. Type checking and all checks pass (Phase 6)

**Low Priority (Future Enhancement):**
7. Parameter completions with `Variable` kind + `"parameter"` detail

## Critical Files to Modify

### Phase 1: CompletionItemKind Alignment

**File:** `crates/raven/src/handlers.rs`

1. **Add R constant set** (near top of file):
   ```rust
   const R_CONSTANTS: &[&str] = &[
       "TRUE", "FALSE", "NULL", "NA", "Inf", "NaN",
       "NA_integer_", "NA_real_", "NA_complex_", "NA_character_"
   ];
   ```

2. **Modify `completion()` function** (~lines 4536-4650):
   - Add constant detection for keyword completions
   - Change Variable → Field for non-functions

3. **Package exports** (~line 4579):
   - Change to Field for non-functions (requires checking if export is function)
   - Keep Function for function exports

4. **Cross-file symbols** (~lines 4597-4599):
   - Change Variable → Field for non-function symbols
   - Keep Function for function symbols

### Phase 2: Sort Order Control

**File:** `crates/raven/src/handlers.rs`

1. **Add sort prefix constants** (near top of file):
   ```rust
   const SORT_PREFIX_SCOPE: &str = "1-";
   const SORT_PREFIX_WORKSPACE: &str = "2-";
   const SORT_PREFIX_PACKAGE: &str = "4-";
   const SORT_PREFIX_KEYWORD: &str = "5-";
   ```

2. **Add sortText field to CompletionItem creation**:
   - Package exports: `sortText: Some(format!("{}{}", SORT_PREFIX_PACKAGE, name))`
   - Cross-file symbols (same file): `sortText: Some(format!("{}{}", SORT_PREFIX_SCOPE, name))`
   - Cross-file symbols (other files): `sortText: Some(format!("{}{}", SORT_PREFIX_WORKSPACE, name))`
   - Keywords: `sortText: Some(format!("{}{}", SORT_PREFIX_KEYWORD, name))`

### Phase 3: Document Symbol Type Detection

**File:** `crates/raven/src/handlers.rs`

1. **Expand `DocumentSymbolKind` enum** (~line 180):
   - Add: Boolean, Number, String, Null, Array, List

2. **Update `to_lsp_kind()` method** (~line 198):
   - Add mappings for new kinds
   - Change Variable → FIELD

3. **Modify `SymbolExtractor::extract_symbols()`** (~line 241):
   - Add value type detection logic
   - Check right-hand side of assignments for type indicators
   - Maintain priority: Function > Class > Constant > Boolean/Null/Number/String/Array/List > Variable

4. **Helper function for RHS type detection**:
   ```rust
   fn detect_value_type(node: &Node, source: &str) -> Option<DocumentSymbolKind> {
       match node.kind() {
           "identifier" => {
               let text = node.utf8_text(source.as_bytes()).ok()?;
               match text {
                   "TRUE" | "FALSE" => Some(DocumentSymbolKind::Boolean),
                   "NULL" => Some(DocumentSymbolKind::Null),
                   "NA" | "Inf" | "NaN" => Some(DocumentSymbolKind::Constant),
                   _ => None,
               }
           }
           "integer" | "float" | "complex" => Some(DocumentSymbolKind::Number),
           "string" => Some(DocumentSymbolKind::String),
           "null" => Some(DocumentSymbolKind::Null),
           "call" => {
               let func_node = node.child_by_field_name("function")?;
               let func_name = func_node.utf8_text(source.as_bytes()).ok()?;
               match func_name {
                   "c" | "vector" | "matrix" | "array" => Some(DocumentSymbolKind::Array),
                   "list" => Some(DocumentSymbolKind::List),
                   _ => None,
               }
           }
           _ => None,
       }
   }
   ```

## Verification

After changes, verify in:

1. **VS Code:**
   - Document outline shows correct icons for different symbol types:
     - Functions: function icon
     - Booleans: boolean icon
     - Numbers: number icon
     - Strings: string icon
     - NULL: null icon
     - Arrays: array icon
     - Lists: struct icon
     - Variables: field icon
     - Constants (ALL_CAPS): constant icon
   - Completion list shows distinct icons:
     - Functions: function icon
     - Variables/data: field icon
     - Constants (TRUE, FALSE, etc.): constant icon
   - Package exports show `{pkg}` in detail field
   - Cross-file symbols show `from {path}` in detail field

2. **Zed:**
   - Completion items have correct colors matching official R-LS
   - Functions from packages show same color as in R-LS
   - Non-function variables/data show same color as in R-LS (field color, not variable color)
   - R constants show constant color

3. **Manual Testing:**
   ```r
   # Test file: test_symbols.R

   # Functions
   my_func <- function(x, y) { x + y }

   # Booleans
   flag <- TRUE
   enabled <- FALSE

   # Numbers
   count <- 42
   ratio <- 3.14
   imaginary <- 1+2i

   # Strings
   name <- "test"

   # NULL
   empty <- NULL

   # Arrays
   nums <- c(1, 2, 3)
   mat <- matrix(1:9, nrow=3)

   # Lists
   data <- list(a=1, b=2)

   # Constants (ALL_CAPS)
   MAX_SIZE <- 100

   # Classes (existing)
   MyClass <- R6Class("MyClass", list(x=1))
   ```

   Verify:
   - Each symbol shows correct icon in document outline
   - Completions show correct kinds and colors
   - Compare side-by-side with official R-LS

### Phase 5: Unit Testing (Required)

**Goal:** Ensure all new functionality is thoroughly tested.

**Test Coverage Required:**

1. **CompletionItemKind tests** (`crates/raven/src/handlers.rs` - add to existing test module):

   a. **R constants completion:**
   ```rust
   #[test]
   fn test_r_constants_completion_kind() {
       // Test that R constants return CONSTANT kind
       // Test cases: TRUE, FALSE, NULL, NA, Inf, NaN, NA_integer_, etc.
   }
   ```

   b. **Field vs Function completion:**
   ```rust
   #[test]
   fn test_completion_kind_field_for_variables() {
       // Test that non-function symbols use FIELD kind
       // Test package exports, cross-file variables
   }

   #[test]
   fn test_completion_kind_function_preserved() {
       // Test that function symbols still use FUNCTION kind
   }
   ```

2. **DocumentSymbolKind tests** (`crates/raven/src/handlers.rs` - add to existing test module):

   a. **Boolean type detection:**
   ```rust
   #[test]
   fn test_boolean_symbol_detection() {
       let code = r#"
           flag <- TRUE
           enabled <- FALSE
       "#;
       // Assert DocumentSymbolKind::Boolean
       // Assert LSP SymbolKind::BOOLEAN
   }
   ```

   b. **Number type detection:**
   ```rust
   #[test]
   fn test_number_symbol_detection() {
       let code = r#"
           count <- 42
           ratio <- 3.14
           imaginary <- 1+2i
       "#;
       // Assert DocumentSymbolKind::Number
       // Assert LSP SymbolKind::NUMBER
   }
   ```

   c. **String type detection:**
   ```rust
   #[test]
   fn test_string_symbol_detection() {
       let code = r#"
           name <- "test"
           path <- 'data.csv'
       "#;
       // Assert DocumentSymbolKind::String
       // Assert LSP SymbolKind::STRING
   }
   ```

   d. **Null type detection:**
   ```rust
   #[test]
   fn test_null_symbol_detection() {
       let code = r#"
           empty <- NULL
       "#;
       // Assert DocumentSymbolKind::Null
       // Assert LSP SymbolKind::NULL
   }
   ```

   e. **Array type detection:**
   ```rust
   #[test]
   fn test_array_symbol_detection() {
       let code = r#"
           nums <- c(1, 2, 3)
           mat <- matrix(1:9, nrow=3)
           arr <- array(1:27, dim=c(3,3,3))
           vec <- vector("numeric", 10)
       "#;
       // Assert DocumentSymbolKind::Array
       // Assert LSP SymbolKind::ARRAY
   }
   ```

   f. **List type detection:**
   ```rust
   #[test]
   fn test_list_symbol_detection() {
       let code = r#"
           data <- list(a=1, b=2)
           config <- list(
               name = "test",
               values = c(1, 2, 3)
           )
       "#;
       // Assert DocumentSymbolKind::List
       // Assert LSP SymbolKind::STRUCT
   }
   ```

   g. **Type precedence:**
   ```rust
   #[test]
   fn test_symbol_type_precedence() {
       let code = r#"
           my_func <- function(x) { x + 1 }  # Function (highest)
           MyClass <- R6Class("MyClass")      # Class
           MAX_SIZE <- 100                    # Constant (ALL_CAPS)
           flag <- TRUE                       # Boolean
           other <- some_call()               # Variable (fallback to FIELD)
       "#;
       // Assert correct precedence for each
   }
   ```

   h. **Fallback to Variable (FIELD):**
   ```rust
   #[test]
   fn test_variable_fallback_mapped_to_field() {
       let code = r#"
           result <- some_function()
           data <- x + y
           obj <- ComplexExpression(a, b, c)
       "#;
       // Assert DocumentSymbolKind::Variable
       // Assert LSP SymbolKind::FIELD (not VARIABLE)
   }
   ```

3. **SortText tests** (`crates/raven/src/handlers.rs`):

   ```rust
   #[test]
   fn test_completion_sort_order() {
       // Test that sortText prefixes are applied correctly:
       // - Scope: "1-"
       // - Workspace: "2-"
       // - Package: "4-"
       // - Keyword: "5-"
   }
   ```

4. **Integration tests** (existing patterns in `crates/raven/src/handlers.rs` integration_tests module):

   ```rust
   #[test]
   fn test_document_symbols_with_type_granularity() {
       // Create a document with various symbol types
       // Request document symbols
       // Verify correct SymbolKind for each
   }

   #[test]
   fn test_completions_with_correct_kinds() {
       // Set up workspace with packages and cross-file symbols
       // Request completions
       // Verify FIELD for variables, FUNCTION for functions, CONSTANT for constants
   }
   ```

5. **Edge case tests:**

   ```rust
   #[test]
   fn test_multiline_assignment_type_detection() {
       let code = r#"
           data <- list(
               a = 1,
               b = 2,
               c = 3
           )
       "#;
       // Should detect List despite multiline
   }

   #[test]
   fn test_complex_rhs_falls_back_to_variable() {
       let code = r#"
           result <- if (x > 0) TRUE else FALSE
       "#;
       // Complex expression → Variable (maps to FIELD)
   }

   #[test]
   fn test_na_variants_detected_as_constants() {
       let code = r#"
           x <- NA_integer_
           y <- NA_real_
           z <- NA_complex_
           w <- NA_character_
       "#;
       // All should be Constant kind
   }
   ```

**Test Utilities:**

Add helper function for symbol extraction testing:
```rust
#[cfg(test)]
fn extract_symbols_from_code(code: &str) -> Vec<(String, DocumentSymbolKind)> {
    // Parse code, extract symbols, return (name, kind) pairs
}
```

### Phase 6: Type Checking and Verification (Required)

**Goal:** Ensure all code compiles, passes type checking, and meets quality standards.

**Verification Steps:**

1. **Cargo check (type checking):**
   ```bash
   cargo check -p raven
   ```
   - Must pass with no errors
   - Verify all new enum variants are handled in match expressions

2. **Cargo clippy (linting):**
   ```bash
   cargo clippy -p raven -- -D warnings
   ```
   - Must pass with no warnings
   - Fix any clippy suggestions (unused variables, unnecessary clones, etc.)

3. **Run all tests:**
   ```bash
   cargo test -p raven
   ```
   - All existing tests must pass
   - All new tests must pass
   - No test failures or panics

4. **Run integration tests specifically:**
   ```bash
   cargo test -p raven --test integration
   ```
   - Verify no regressions in LSP behavior

5. **Format check:**
   ```bash
   cargo fmt -p raven -- --check
   ```
   - Code must be properly formatted

6. **Build release:**
   ```bash
   cargo build --release -p raven
   ```
   - Must build successfully in release mode

7. **Manual LSP test:**
   - Build and run LSP server
   - Open test file with various symbol types
   - Verify document symbols show correct icons
   - Verify completions show correct kinds
   - Test in VS Code and/or Zed if possible

**Acceptance Criteria:**

- ✅ All phases implemented
- ✅ All unit tests pass
- ✅ All integration tests pass
- ✅ `cargo check` passes
- ✅ `cargo clippy` passes with no warnings
- ✅ `cargo test` passes
- ✅ `cargo fmt --check` passes
- ✅ Release build succeeds
- ✅ Manual verification shows correct symbols and icons

## Parser Discoveries (Critical Implementation Details)

During implementation, we discovered critical mismatches between expected and actual tree-sitter-r node kinds. These discoveries were essential for making value-based type detection work.

### Initial Assumptions (WRONG)

When designing the value type detection logic, we initially assumed:
- Boolean literals (`TRUE`, `FALSE`) would be `identifier` nodes
- Numeric literals (e.g., `42`) would be `integer` nodes
- R constants (NA, Inf, NaN) would be `identifier` nodes

**These assumptions were incorrect** and caused all value type detection tests to fail.

### Actual Node Kinds (CORRECT)

By creating a debug test (`test_debug_ast_structure`) that printed the actual AST structure, we discovered the correct node kinds:

| R Code | Expected (Wrong) | Actual (Correct) | Node Kind |
|--------|------------------|------------------|-----------|
| `TRUE` | `identifier` | `true` | Boolean literal |
| `FALSE` | `identifier` | `false` | Boolean literal |
| `42` | `integer` | `float` | Numeric literal |
| `42L` | `integer` | `integer` | Integer literal |
| `3.14` | `float` | `float` | Numeric literal |
| `2i` | `complex` | `complex` | Complex literal |
| `"text"` | `string` | `string` | String literal |
| `NULL` | `null` | `null` | Null literal |
| `NA` | `identifier` | `na` | NA constant |
| `Inf` | `identifier` | `inf` | Infinity constant |
| `NaN` | `identifier` | `nan` | NaN constant |

### Key Insights

1. **Boolean literals are NOT identifiers:** Tree-sitter-r has dedicated node kinds `"true"` and `"false"` for boolean literals. This is semantically correct - they are literals, not identifiers.

2. **Integer literals are actually floats:** Regular numeric literals like `42` parse as `"float"` nodes, not `"integer"`. Only literals with the `L` suffix (e.g., `42L`) parse as `"integer"`. This matches R's internal behavior where unadorned numbers are doubles.

3. **R constants have dedicated node kinds:** `NA`, `Inf`, and `NaN` have their own node kinds (`"na"`, `"inf"`, `"nan"`) rather than being treated as identifiers.

4. **Debug test was critical:** Without printing the actual AST structure, we would have continued debugging the wrong assumptions. The test that revealed this:

```rust
#[test]
fn test_debug_ast_structure() {
    let tests = vec![
        ("x <- 42", "numeric"),
        ("x <- TRUE", "boolean"),
        ("x <- \"hello\"", "string"),
        ("x <- NULL", "null"),
    ];

    for (code, _expected) in tests {
        let tree = parser.parse(code, None).unwrap();
        // Print actual node kinds from RHS
        // This revealed the correct node kind names
    }
}
```

### Implications for Implementation

The correct node kinds meant our `detect_value_type()` method needed to use:

```rust
fn detect_value_type(&self, node: tree_sitter::Node<'a>) -> Option<DocumentSymbolKind> {
    match node.kind() {
        "true" | "false" => Some(DocumentSymbolKind::Boolean),  // NOT "identifier"
        "null" => Some(DocumentSymbolKind::Null),
        "na" | "inf" | "nan" => Some(DocumentSymbolKind::Constant),  // NOT "identifier"
        "integer" | "float" | "complex" => Some(DocumentSymbolKind::Number),  // "float" is common
        "string" => Some(DocumentSymbolKind::String),
        // ... rest of implementation
    }
}
```

### Reference Source

The authoritative source for these node kinds is the tree-sitter-r grammar:
- Repository: https://github.com/r-lib/tree-sitter-r
- Grammar file: `grammar.js` defines all node types
- When in doubt, clone the repo and consult the grammar, or use a debug test to print actual node structures

### Testing Strategy

After discovering the correct node kinds:
1. Updated all `detect_value_type()` logic to use correct node kinds
2. Fixed 27 failing tests that expected old behavior (e.g., `Variable` instead of `Number`)
3. All 2110 tests now pass

This experience reinforces the importance of:
- Never assuming parser node structures without verification
- Using debug tests to inspect actual AST output
- Consulting grammar files when documentation is unclear
- Testing assumptions early before building complex logic on top

## Implementation Notes

1. **Tree-sitter node types for R:**
   - Assignment: `binary_operator` with `<-` or `=` or `->`
   - Right-hand side: second child of binary_operator
   - **Literals (see "Parser Discoveries" section above for CRITICAL corrections):**
     - Boolean: `"true"`, `"false"` (NOT `"identifier"`)
     - Numeric: `"float"` (for `42`), `"integer"` (for `42L`), `"complex"` (for `2i`)
     - String: `"string"`
     - Null: `"null"`
     - R constants: `"na"`, `"inf"`, `"nan"` (NOT `"identifier"`)
   - Identifiers: `identifier`
   - Calls: `call` with `function` field

2. **Precedence handling:**
   - Function detection (existing) takes precedence
   - Class detection (existing) takes precedence
   - ALL_CAPS constant pattern takes precedence over literal types
   - Value type detection for remaining cases
   - Variable as ultimate fallback

3. **Performance considerations:**
   - Value type detection happens during existing AST traversal
   - No additional file reads or parsing required
   - Simple node kind checks are fast
   - Minimal overhead

4. **Backward compatibility:**
   - Existing symbol extraction continues to work
   - New type detection adds granularity without breaking changes
   - Tests should be updated to expect new symbol kinds

5. **Test organization:**
   - Add new tests to existing `#[cfg(test)]` module in handlers.rs
   - Use existing test utilities where possible
   - Follow existing test naming conventions (`test_*`)
   - Group related tests together with comments

## Open Questions

None - all decisions have been made:
- ✓ Phase 4: Use Option A (value-based type detection)
- ✓ Detail field: Keep current format (`"from {path}"`)
- ✓ Plan location: Repository `.claude/plans/` directory

## Summary

**Key changes for alignment with official R language server:**

1. **CompletionItemKind:** Change non-function items to `FIELD` instead of `VARIABLE`
2. **R Constants:** Classify TRUE, FALSE, NULL, NA, Inf, NaN as `CONSTANT`
3. **Document Symbols:** Add value-based type detection (Boolean, Number, String, Null, Array, List)
4. **Sort Order:** Add sortText prefixes for better completion ordering
5. **Detail Field:** Keep current format - more informative than R-LS

These changes ensure Raven shows the same icons and colors as the official R language server in editors like VS Code and Zed, while maintaining Raven's superior cross-file awareness features.
