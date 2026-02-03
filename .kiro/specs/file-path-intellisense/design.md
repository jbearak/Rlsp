# Design Document: File Path Intellisense

## Overview

This feature adds file path intellisense to Raven, providing completions when typing file paths in `source()` calls and LSP directives, plus go-to-definition navigation for file paths. The implementation integrates with Raven's existing completion and definition handlers, leveraging the established path resolution infrastructure.

### Key Design Decisions

1. **Context Detection via Tree-Sitter**: Use tree-sitter AST to detect when cursor is inside a string literal in source()/sys.source() calls, and regex for directive path contexts.

2. **Reuse Existing Path Resolution**: Leverage `PathContext` and `resolve_path()` from `cross_file/path_resolve.rs` for consistent path handling.

3. **Workspace-Bounded Completions**: Only show files within the workspace to prevent information leakage and maintain security.

4. **Trigger Character Extension**: Add `/` and `"` as completion trigger characters alongside existing `:`, `$`, `@`.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         LSP Client                               │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                        backend.rs                                │
│  - Registers trigger characters: ":", "$", "@", "/", "\""       │
│  - Routes completion/definition requests to handlers            │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                       handlers.rs                                │
│  ┌─────────────────────┐    ┌─────────────────────────────────┐ │
│  │   completion()      │    │   goto_definition()             │ │
│  │   - Detect context  │    │   - Detect file path context    │ │
│  │   - Delegate to     │    │   - Resolve path                │ │
│  │     file_path_      │    │   - Return file location        │ │
│  │     completions()   │    │                                 │ │
│  └─────────────────────┘    └─────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                  file_path_intellisense.rs (new)                 │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │ Context Detection                                           ││
│  │ - is_source_call_string_context()                          ││
│  │ - is_directive_path_context()                              ││
│  │ - extract_partial_path()                                   ││
│  └─────────────────────────────────────────────────────────────┘│
│  ┌─────────────────────────────────────────────────────────────┐│
│  │ Completions                                                 ││
│  │ - file_path_completions()                                  ││
│  │ - list_directory_entries()                                 ││
│  │ - filter_r_files()                                         ││
│  └─────────────────────────────────────────────────────────────┘│
│  ┌─────────────────────────────────────────────────────────────┐│
│  │ Go-to-Definition                                           ││
│  │ - file_path_definition()                                   ││
│  │ - extract_file_path_at_position()                          ││
│  └─────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│              cross_file/path_resolve.rs (existing)               │
│  - PathContext                                                   │
│  - resolve_path()                                                │
│  - resolve_working_directory()                                   │
└─────────────────────────────────────────────────────────────────┘
```

## Components and Interfaces

### 1. FilePathContext (enum)

Represents the detected context for file path operations.

```rust
/// Context type for file path intellisense
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePathContext {
    /// Inside string literal in source() call
    SourceCall {
        /// The partial path typed so far
        partial_path: String,
        /// Start position of the string content (after opening quote)
        content_start: Position,
        /// Whether this is sys.source (affects envir handling)
        is_sys_source: bool,
    },
    /// After an LSP directive keyword
    Directive {
        /// The directive type
        directive_type: DirectiveType,
        /// The partial path typed so far
        partial_path: String,
        /// Start position of the path
        path_start: Position,
    },
    /// Not in a file path context
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveType {
    SourcedBy,  // @lsp-sourced-by, @lsp-run-by, @lsp-included-by
    Source,     // @lsp-source
}
```

### 2. Context Detection Functions

```rust
/// Detect if cursor is in a file path context for completions
/// 
/// Returns FilePathContext indicating the type of context and partial path.
pub fn detect_file_path_context(
    tree: &Tree,
    content: &str,
    position: Position,
) -> FilePathContext;

/// Check if cursor is inside a string literal in a source()/sys.source() call
fn is_source_call_string_context(
    tree: &Tree,
    content: &str,
    position: Position,
) -> Option<(String, Position, bool)>;

/// Check if cursor is after an LSP directive where a path is expected
fn is_directive_path_context(
    content: &str,
    position: Position,
) -> Option<(DirectiveType, String, Position)>;

/// Extract the partial path from cursor position to start of path
fn extract_partial_path(
    content: &str,
    line: u32,
    start_col: u32,
    cursor_col: u32,
) -> String;
```

### 3. Completion Functions

```rust
/// Generate file path completions for the given context
/// 
/// Returns completion items for R files and directories.
pub fn file_path_completions(
    context: &FilePathContext,
    file_uri: &Url,
    metadata: &CrossFileMetadata,
    workspace_root: Option<&Url>,
) -> Vec<CompletionItem>;

/// List directory entries filtered for R files
fn list_directory_entries(
    base_path: &Path,
    workspace_root: Option<&Path>,
) -> Vec<DirEntry>;

/// Filter entries to R files (.R, .r) and directories
fn filter_r_files(entries: Vec<DirEntry>) -> Vec<DirEntry>;

/// Create a completion item for a file or directory
fn create_path_completion_item(
    entry: &DirEntry,
    is_directory: bool,
) -> CompletionItem;
```

### 4. Go-to-Definition Functions

```rust
/// Get definition location for a file path at the given position
/// 
/// Returns the file URI if the path resolves to an existing file.
pub fn file_path_definition(
    tree: &Tree,
    content: &str,
    position: Position,
    file_uri: &Url,
    metadata: &CrossFileMetadata,
    workspace_root: Option<&Url>,
) -> Option<Location>;

/// Extract the complete file path string at the cursor position
fn extract_file_path_at_position(
    tree: &Tree,
    content: &str,
    position: Position,
) -> Option<(String, FilePathContext)>;
```

## Data Models

### CompletionItem for File Paths

```rust
CompletionItem {
    label: String,           // File or directory name (e.g., "utils.R", "helpers/")
    kind: CompletionItemKind, // FILE or FOLDER
    detail: Option<String>,  // Relative path from current file
    insert_text: Option<String>, // Path to insert (with trailing / for directories)
    sort_text: Option<String>,   // For ordering (directories first, then files)
}
```

### Path Resolution Context

The feature reuses the existing `PathContext` from `cross_file/path_resolve.rs`:

```rust
pub struct PathContext {
    pub file_path: PathBuf,
    pub working_directory: Option<PathBuf>,
    pub inherited_working_directory: Option<PathBuf>,
    pub workspace_root: Option<PathBuf>,
}
```

**Path Resolution Rules** (from existing implementation):
- **source() calls**: Use `PathContext::from_metadata()` which respects @lsp-cd
- **LSP directives**: Use `PathContext::new()` which ignores @lsp-cd (always relative to file's directory)

### Directory Entry

```rust
struct DirEntry {
    name: String,
    path: PathBuf,
    is_directory: bool,
}
```



## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

### Property 1: Source Call Context Detection

*For any* R code containing a `source()` or `sys.source()` call with a string literal argument, and *for any* cursor position inside that string literal, the context detector SHALL return a `SourceCall` context with the correct partial path.

**Validates: Requirements 1.1, 1.2**

### Property 2: Backward Directive Context Detection

*For any* R comment containing an `@lsp-sourced-by`, `@lsp-run-by`, or `@lsp-included-by` directive (with or without colon, with or without quotes), and *for any* cursor position after the directive keyword, the context detector SHALL return a `Directive` context with `DirectiveType::SourcedBy`.

**Validates: Requirements 1.3, 1.4, 1.5**

### Property 3: Forward Directive Context Detection

*For any* R comment containing an `@lsp-source` directive (with or without colon, with or without quotes), and *for any* cursor position after the directive keyword, the context detector SHALL return a `Directive` context with `DirectiveType::Source`.

**Validates: Requirements 1.6**

### Property 4: Non-Source Function Exclusion

*For any* R code containing a function call that is NOT `source()` or `sys.source()`, and *for any* cursor position inside a string argument, the context detector SHALL return `FilePathContext::None`.

**Validates: Requirements 1.7**

### Property 5: R File and Directory Filtering

*For any* directory containing files with various extensions, the completion provider SHALL return only files with `.R` or `.r` extensions, plus all directories.

**Validates: Requirements 2.1, 2.2**

### Property 6: Partial Path Resolution

*For any* partial path prefix (including empty, relative with `../`, or starting with `/`), the completion provider SHALL return completions relative to the correctly resolved base directory.

**Validates: Requirements 2.3, 2.6**

### Property 7: Workspace-Root-Relative Paths in Directives

*For any* LSP directive context where the partial path starts with `/`, the completion provider SHALL resolve the path relative to the workspace root (not filesystem root).

**Validates: Requirements 2.4**

### Property 8: Absolute Paths in Source Calls

*For any* source() call context where the partial path starts with `/`, the completion provider SHALL resolve the path as an absolute filesystem path.

**Validates: Requirements 2.5**

### Property 9: Directory Completion Trailing Slash

*For any* directory entry in completion results, the `insert_text` SHALL end with a forward slash `/`.

**Validates: Requirements 2.7**

### Property 10: Path Separator Normalization

*For any* input path containing escaped backslashes (`\\`), the path resolver SHALL treat them equivalently to forward slashes for resolution purposes.

**Validates: Requirements 4.1, 4.2**

### Property 11: Output Path Separator

*For any* completion item returned, the path separator used in `insert_text` SHALL be a forward slash `/`.

**Validates: Requirements 4.3**

### Property 12: Source Call Go-to-Definition

*For any* `source()` or `sys.source()` call with a string literal path that resolves to an existing file, go-to-definition SHALL return a `Location` pointing to that file at line 0, column 0.

**Validates: Requirements 5.1, 5.2, 5.4**

### Property 13: Missing File Returns No Definition

*For any* file path (in source() call or directive) that does not resolve to an existing file, go-to-definition SHALL return `None`.

**Validates: Requirements 5.3**

### Property 14: Backward Directive Go-to-Definition

*For any* `@lsp-sourced-by`, `@lsp-run-by`, or `@lsp-included-by` directive with a path that resolves to an existing file, go-to-definition SHALL return a `Location` pointing to that file.

**Validates: Requirements 6.1, 6.2, 6.3**

### Property 15: Forward Directive Go-to-Definition

*For any* `@lsp-source` directive with a path that resolves to an existing file, go-to-definition SHALL return a `Location` pointing to that file.

**Validates: Requirements 6.4**

### Property 16: Backward Directives Ignore @lsp-cd

*For any* file with both an `@lsp-cd` directive and a backward directive (`@lsp-sourced-by`, etc.), the backward directive path SHALL be resolved relative to the file's directory, NOT the @lsp-cd directory.

**Validates: Requirements 6.5**

### Property 17: Workspace Boundary Enforcement

*For any* path that would resolve to a location outside the workspace root, the completion provider SHALL NOT include that path in results.

**Validates: Requirements 7.2**

### Property 18: Invalid Character Handling

*For any* path containing invalid filesystem characters, the completion provider SHALL return an empty list without throwing an error.

**Validates: Requirements 7.3**

### Property 19: Space Handling in Paths

*For any* quoted path containing spaces, the path resolver SHALL correctly parse and resolve the complete path including spaces.

**Validates: Requirements 7.5**

## Error Handling

### Invalid Path Characters

When a path contains characters invalid for the filesystem (null bytes, etc.):
- Log a trace message indicating the invalid path
- Return empty completion list
- Return `None` for go-to-definition

### Path Resolution Failures

When path resolution fails (e.g., too many `../` components):
- Return empty completion list for completions
- Return `None` for go-to-definition
- Do not emit diagnostics (this is handled by existing missing file diagnostics)

### Filesystem Access Errors

When directory listing fails (permissions, I/O errors):
- Log a warning with the error details
- Return empty completion list
- Continue processing other paths

### Non-Existent Base Directory

When the base directory for completions doesn't exist:
- Return empty completion list
- This is expected behavior for incomplete paths

## Testing Strategy

### Dual Testing Approach

This feature requires both unit tests and property-based tests:

- **Unit tests**: Verify specific examples, edge cases, and integration points
- **Property tests**: Verify universal properties across all valid inputs using proptest

### Property-Based Testing Configuration

- **Library**: proptest (already used in the codebase)
- **Minimum iterations**: 100 per property test
- **Tag format**: `Feature: file-path-intellisense, Property N: {property_text}`

### Test Categories

#### 1. Context Detection Tests

Property tests for:
- Source call string context detection (Property 1)
- Directive context detection (Properties 2, 3)
- Non-source function exclusion (Property 4)

Unit tests for:
- Edge cases: empty strings, nested quotes, escaped quotes
- Cursor at string boundaries
- Malformed source() calls

#### 2. Completion Tests

Property tests for:
- R file filtering (Property 5)
- Path resolution (Properties 6, 7, 8)
- Directory trailing slash (Property 9)
- Path separator handling (Properties 10, 11)

Unit tests for:
- Empty directory
- Directory with no R files
- Deeply nested paths
- Symlinks (if supported)

#### 3. Go-to-Definition Tests

Property tests for:
- Source call navigation (Property 12)
- Missing file handling (Property 13)
- Directive navigation (Properties 14, 15)
- @lsp-cd isolation (Property 16)

Unit tests for:
- Cursor at different positions within path string
- Paths with special characters
- Case sensitivity on different platforms

#### 4. Edge Case Tests

Property tests for:
- Workspace boundary (Property 17)
- Invalid characters (Property 18)
- Space handling (Property 19)

Unit tests for:
- Empty workspace
- Empty path string
- Very long paths
- Unicode in paths

### Test File Structure

```
crates/raven/src/
├── file_path_intellisense.rs      # Main implementation
├── file_path_intellisense_tests.rs # Unit tests
└── cross_file/
    └── property_tests.rs          # Add property tests here
```

### Generator Strategies for Property Tests

```rust
/// Strategy for generating valid R file names
fn r_filename() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,10}\\.(R|r)".prop_map(|s| s)
}

/// Strategy for generating directory names
fn dirname() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,10}".prop_map(|s| s)
}

/// Strategy for generating relative paths
fn relative_path() -> impl Strategy<Value = String> {
    prop_oneof![
        r_filename(),
        (dirname(), r_filename()).prop_map(|(d, f)| format!("{}/{}", d, f)),
        (r_filename()).prop_map(|f| format!("../{}", f)),
    ]
}

/// Strategy for generating source() call code
fn source_call_code() -> impl Strategy<Value = (String, Position)> {
    relative_path().prop_map(|path| {
        let code = format!("source(\"{}\")", path);
        let cursor_col = 8 + path.len() / 2; // Middle of path
        (code, Position::new(0, cursor_col as u32))
    })
}
```
