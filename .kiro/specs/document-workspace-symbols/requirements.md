# Requirements Document

## Introduction

This feature enhances Raven's document symbol and workspace symbol support to provide hierarchical outlines, proper range information, R code section support, richer SymbolKind mapping, and improved workspace symbol search. The goal is to transform the current flat, limited symbol output into a rich, hierarchical representation that supports VS Code's outline view, breadcrumb navigation, and Ctrl+T workspace search.

## Glossary

- **Document_Symbol_Provider**: The LSP handler that responds to `textDocument/documentSymbol` requests, returning symbols for a single file
- **Workspace_Symbol_Provider**: The LSP handler that responds to `workspace/symbol` requests, returning symbols across all indexed files
- **DocumentSymbol**: The hierarchical LSP response type with `range`, `selectionRange`, and optional `children` array
- **SymbolInformation**: The flat LSP response type with `location` and `containerName` (legacy format)
- **Range**: The full extent of a construct (e.g., entire function definition including body)
- **SelectionRange**: The identifier-only range (e.g., just the function name)
- **R_Code_Section**: A comment line matching the pattern `# Section Name ----` that creates a logical grouping in the outline
- **ALL_CAPS_Constant**: An identifier matching `^[A-Z][A-Z0-9_.]+$` (minimum 2 characters) treated as a constant
- **S4_Method**: A method defined via `setMethod()`, `setClass()`, or `setGeneric()` calls
- **R6_Class**: A class defined via `R6Class()` or `setRefClass()` calls
- **Reserved_Word**: R language keywords (if, else, for, while, etc.) that should not appear as symbols

## Requirements

### Requirement 1: Hierarchical DocumentSymbol Response

**User Story:** As a developer, I want the document outline to show hierarchical structure, so that I can see nested functions and assignments within their parent scopes.

#### Acceptance Criteria

1. WHEN a client supports `hierarchicalDocumentSymbolSupport` capability, THE Document_Symbol_Provider SHALL return `DocumentSymbol[]` response type
2. WHEN a client does not support `hierarchicalDocumentSymbolSupport` capability, THE Document_Symbol_Provider SHALL return `SymbolInformation[]` response type as fallback
3. WHEN returning flat `SymbolInformation[]` fallback, THE Document_Symbol_Provider SHALL set the correct document URI in each symbol's location (not placeholder)

### Requirement 2: Correct Range and SelectionRange

**User Story:** As a developer, I want accurate symbol ranges, so that breadcrumb navigation and outline selection work correctly.

#### Acceptance Criteria

1. FOR ALL DocumentSymbol entries, THE Document_Symbol_Provider SHALL set `range` to the full extent of the construct (from start of assignment to end of value/body)
2. FOR ALL DocumentSymbol entries, THE Document_Symbol_Provider SHALL set `selectionRange` to the identifier-only range (just the symbol name)
3. THE selectionRange SHALL always be contained within the range

### Requirement 3: Hierarchical Nesting of Children

**User Story:** As a developer, I want assignments inside function bodies to appear as children of that function, so that the outline reflects the actual code structure.

#### Acceptance Criteria

1. WHEN an assignment occurs inside a function body, THE Document_Symbol_Provider SHALL include it as a child of that function's DocumentSymbol
2. WHEN an assignment occurs at top-level (outside any function), THE Document_Symbol_Provider SHALL include it as a root-level symbol
3. THE Document_Symbol_Provider SHALL support arbitrary nesting depth for nested function definitions

### Requirement 4: R Code Section Support

**User Story:** As a developer, I want `# Section ----` comments to appear in the outline as collapsible groups, so that I can organize and navigate large files by logical sections.

#### Acceptance Criteria

1. WHEN a comment matches the section pattern `^\s*#(#*)\s*(%%)?\s*(\S.+?)\s*(#{4,}|\-{4,}|={4,}|\*{4,}|\+{4,})\s*$`, THE Document_Symbol_Provider SHALL create a DocumentSymbol with SymbolKind MODULE
2. THE section symbol's `range` SHALL span from the section comment to the line before the next section (or end of file)
3. THE section symbol's `selectionRange` SHALL be the section comment line only
4. WHEN symbols are defined within a section's range, THE Document_Symbol_Provider SHALL nest them as children of that section
5. THE Document_Symbol_Provider SHALL support nested sections based on heading level (number of `#` characters)

### Requirement 5: Richer SymbolKind Mapping

**User Story:** As a developer, I want different symbol types to have appropriate icons in the outline, so that I can quickly distinguish constants, classes, and methods.

#### Acceptance Criteria

1. WHEN an identifier matches the ALL_CAPS pattern `^[A-Z][A-Z0-9_.]+$` (minimum 2 characters), THE Document_Symbol_Provider SHALL assign SymbolKind CONSTANT
2. WHEN an assignment's RHS is an `R6Class()` or `setRefClass()` call, THE Document_Symbol_Provider SHALL assign SymbolKind CLASS
3. WHEN a `setMethod()` call is detected at top-level, THE Document_Symbol_Provider SHALL create a symbol with SymbolKind METHOD
4. WHEN a `setClass()` call is detected at top-level, THE Document_Symbol_Provider SHALL create a symbol with SymbolKind CLASS
5. WHEN a `setGeneric()` call is detected at top-level, THE Document_Symbol_Provider SHALL create a symbol with SymbolKind INTERFACE
6. FOR ALL other function definitions, THE Document_Symbol_Provider SHALL assign SymbolKind FUNCTION
7. FOR ALL other variable assignments (non-constant, non-function), THE Document_Symbol_Provider SHALL assign SymbolKind VARIABLE

### Requirement 6: Function Parameter Signature in Detail

**User Story:** As a developer, I want to see function parameters in the outline, so that I can quickly identify functions by their signatures.

#### Acceptance Criteria

1. FOR ALL function symbols, THE Document_Symbol_Provider SHALL populate the `detail` field with the function's parameter list
2. WHEN the parameter list exceeds 60 characters, THE Document_Symbol_Provider SHALL truncate it and append `...`
3. THE detail format SHALL be `(param1, param2, ...)` matching the function's formal parameters

### Requirement 7: Reserved Word Filtering

**User Story:** As a developer, I want R reserved words to be excluded from the symbol list, so that the outline only shows meaningful user-defined symbols.

#### Acceptance Criteria

1. WHEN an assignment's LHS is an R reserved word (if, else, for, while, repeat, in, next, break, TRUE, FALSE, NULL, Inf, NaN, NA, NA_integer_, NA_real_, NA_complex_, NA_character_, function), THE Document_Symbol_Provider SHALL NOT include it in the symbol list
2. THE reserved word filtering SHALL apply to both document symbols and workspace symbols

### Requirement 8: Workspace Symbol ContainerName

**User Story:** As a developer, I want workspace symbol search results to show which file each symbol comes from, so that I can distinguish between symbols with the same name in different files.

#### Acceptance Criteria

1. FOR ALL workspace symbols, THE Workspace_Symbol_Provider SHALL set `containerName` to the filename without extension
2. WHEN the file path is `/path/to/analysis.R`, THE containerName SHALL be `analysis`

### Requirement 9: Workspace Symbol Query Filtering

**User Story:** As a developer, I want workspace symbol search to filter results by my query, so that I can quickly find symbols across the codebase.

#### Acceptance Criteria

1. WHEN a query string is provided, THE Workspace_Symbol_Provider SHALL return only symbols whose names contain the query (case-insensitive substring match)
2. THE Workspace_Symbol_Provider SHALL limit results to a configurable maximum (default: 1000 symbols)
3. THE Workspace_Symbol_Provider SHALL search across open documents, workspace index, and legacy indices with proper deduplication

### Requirement 11: Configuration Options

**User Story:** As a developer, I want to configure symbol provider behavior, so that I can tune performance and results for my workspace size.

#### Acceptance Criteria

1. THE server SHALL expose a `symbols.workspaceMaxResults` configuration option with default value 1000
2. WHEN `symbols.workspaceMaxResults` is set, THE Workspace_Symbol_Provider SHALL use that value as the maximum result limit
3. THE configuration SHALL accept integer values between 100 and 10000

### Requirement 10: S4 Method Name Extraction

**User Story:** As a developer, I want S4 methods to appear with meaningful names in the outline, so that I can navigate S4 class hierarchies.

#### Acceptance Criteria

1. WHEN a `setMethod("methodName", ...)` call is detected, THE Document_Symbol_Provider SHALL use `methodName` as the symbol name
2. WHEN a `setClass("ClassName", ...)` call is detected, THE Document_Symbol_Provider SHALL use `ClassName` as the symbol name
3. WHEN a `setGeneric("genericName", ...)` call is detected, THE Document_Symbol_Provider SHALL use `genericName` as the symbol name
