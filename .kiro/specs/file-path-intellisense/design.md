# Design Document: File Path Intellisense

## Overview

This feature adds file path intellisense to Raven, providing completions when typing file paths in `source()` calls and LSP directives, plus go-to-definition navigation for file paths. The implementation integrates with Raven's existing completion and definition handlers, leveraging the established path resolution infrastructure from `cross_file/path_resolve.rs`.

### Key Design Decisions

1. **Context Detection via Tree-Sitter**: Use tree-sitter AST to detect when cursor is inside a string literal in source()/sys.source() calls. Use regex patterns (consistent with existing directive.rs) for directive path contexts.

2. **Reuse Existing Path Resolution**: Leverage `PathContext` and `resolve_path()` from `cross_file/path_resolve.rs` for consistent path handling. This ensures the critical distinction between source() calls (respects @lsp-cd) and directives (ignores @lsp-cd) is maintained.

3. **Workspace-Bounded Completions**: Only show files within the workspace to prevent information leakage and maintain security.

4. **Trigger Character Extension**: Add `/` and `"` as completion trigger characters alongside existing `:`, `$`, `@`.

5. **R File Filtering**: Filter completions to show only `.R`/`.r` files and directories, matching R's source() conventions.

## Architecture

```text
+------------------------------------------------------------------+
|                         LSP Client                                |
+------------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------------+
|                        backend.rs                                 |
|  - Registers trigger characters: ":", "$", "@", "/", "\""        |
|  - Routes completion/definition requests to handlers             |
+------------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------------+
|                       handlers.rs                                 |
|  +---------------------+    +----------------------------------+ |
|  |   completion()      |    |   goto_definition()              | |
|  |   - Check file path |    |   - Check file path context      | |
|  |     context first   |    |   - Delegate to file_path_       | |
|  |   - Delegate to     |    |     definition() if matched      | |
|  |     file_path_      |    |                                  | |
|  |     completions()   |    |                                  | |
|  +---------------------+    +----------------------------------+ |
+------------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------------+
|                  file_path_intellisense.rs (new)                  |
|  +--------------------------------------------------------------+|
|  | Context Detection                                            ||
|  | - detect_file_path_context()                                ||
|  | - is_source_call_string_context()                           ||
|  | - is_directive_path_context()                               ||
|  | - extract_partial_path()                                    ||
|  +--------------------------------------------------------------+|
|  +--------------------------------------------------------------+|
|  | Completions                                                  ||
|  | - file_path_completions()                                   ||
|  | - list_directory_entries()                                  ||
|  | - filter_r_files_and_dirs()                                 ||
|  | - create_path_completion_item()                             ||
|  +--------------------------------------------------------------+|
|  +--------------------------------------------------------------+|
|  | Go-to-Definition                                            ||
|  | - file_path_definition()                                    ||
|  | - extract_file_path_at_position()                           ||
|  +--------------------------------------------------------------+|
+------------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------------+
|              cross_file/path_resolve.rs (existing)                |
|  - PathContext::new() - for directives (ignores @lsp-cd)         |
|  - PathContext::from_metadata() - for source() (uses @lsp-cd)    |
|  - resolve_path()                                                 |
|  - normalize_path_public()                                        |
+------------------------------------------------------------------+
```

## Components and Interfaces

### 1. FilePathContext (enum)

Represents the detected context for file path operations.

```rust
/// Context type for file path intellisense
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePathContext {
    /// Inside string literal in source() or sys.source() call
    SourceCall {
        /// The partial path typed so far (content between opening quote and cursor)
        partial_path: String,
        /// Start position of the string content (after opening quote)
        content_start: Position,
        /// Whether this is sys.source (vs regular source)
        is_sys_source: bool,
    },
    /// After an LSP directive keyword
    Directive {
        /// The directive type (backward or forward)
        directive_type: DirectiveType,
        /// The partial path typed so far
        partial_path: String,
        /// Start position of the path (after directive keyword and optional colon/quote)
        path_start: Position,
    },
    /// Not in a file path context
    None,
}

/// Type of LSP directive for path context
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveType {
    /// Backward directives: @lsp-sourced-by, @lsp-run-by, @lsp-included-by
    /// These declare that the current file is sourced BY another file
    SourcedBy,
    /// Forward directive: @lsp-source
    /// This declares that the current file sources another file
    Source,
}
```


### 2. Context Detection Functions

```rust
/// Detect if cursor is in a file path context for completions
///
/// Checks source() calls first (via tree-sitter), then directive contexts (via regex).
/// Returns FilePathContext indicating the type of context and partial path.
///
/// # Arguments
/// * `tree` - The tree-sitter parse tree for the document
/// * `content` - The document text content
/// * `position` - The cursor position (line, character in UTF-16)
pub fn detect_file_path_context(
    tree: &Tree,
    content: &str,
    position: Position,
) -> FilePathContext;

/// Check if cursor is inside a string literal in a source()/sys.source() call
///
/// Uses tree-sitter AST traversal to find call nodes with function name
/// "source" or "sys.source", then checks if cursor is within the string argument.
///
/// # Returns
/// Some((partial_path, content_start, is_sys_source)) if in source call context
fn is_source_call_string_context(
    tree: &Tree,
    content: &str,
    position: Position,
) -> Option<(String, Position, bool)>;

/// Check if cursor is after an LSP directive where a path is expected
///
/// Uses regex patterns consistent with cross_file/directive.rs to detect
/// @lsp-sourced-by, @lsp-run-by, @lsp-included-by, and @lsp-source directives.
/// Handles optional colon and quotes syntax variations.
///
/// # Returns
/// Some((directive_type, partial_path, path_start)) if in directive context
fn is_directive_path_context(
    content: &str,
    position: Position,
) -> Option<(DirectiveType, String, Position)>;

/// Extract the partial path from string start to cursor position
///
/// Handles escaped characters within the string literal.
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
/// Determines the base directory based on context type:
/// - SourceCall: Uses PathContext::from_metadata() (respects @lsp-cd)
/// - Directive: Uses PathContext::new() (ignores @lsp-cd)
///
/// # Arguments
/// * `context` - The detected file path context
/// * `file_uri` - URI of the current file
/// * `metadata` - Cross-file metadata for the current file
/// * `workspace_root` - Optional workspace root URI
///
/// # Returns
/// Vector of CompletionItem for R files and directories
pub fn file_path_completions(
    context: &FilePathContext,
    file_uri: &Url,
    metadata: &CrossFileMetadata,
    workspace_root: Option<&Url>,
) -> Vec<CompletionItem>;

/// List directory entries, excluding hidden files (starting with '.')
///
/// # Arguments
/// * `base_path` - The directory to list
/// * `workspace_root` - Optional workspace root for boundary checking
///
/// # Returns
/// Vector of DirEntry for non-hidden files and directories
fn list_directory_entries(
    base_path: &Path,
    workspace_root: Option<&Path>,
) -> io::Result<Vec<DirEntry>>;

/// Filter entries to R files (.R, .r) and directories
///
/// Keeps:
/// - Files with .R or .r extension
/// - All directories (for navigation)
fn filter_r_files_and_dirs(entries: Vec<DirEntry>) -> Vec<DirEntry>;

/// Create a completion item for a file or directory
///
/// - Sets CompletionItemKind::FILE or FOLDER
/// - Appends trailing '/' to directory insert_text
/// - Uses forward slashes for all paths (R convention)
fn create_path_completion_item(
    entry: &DirEntry,
    is_directory: bool,
) -> CompletionItem;
```

### 4. Go-to-Definition Functions

```rust
/// Get definition location for a file path at the given position
///
/// Detects context type and resolves path using appropriate PathContext:
/// - SourceCall: Uses PathContext::from_metadata() (respects @lsp-cd)
/// - Directive: Uses PathContext::new() (ignores @lsp-cd)
///
/// # Returns
/// Some(Location) at line 0, column 0 if file exists, None otherwise
pub fn file_path_definition(
    tree: &Tree,
    content: &str,
    position: Position,
    file_uri: &Url,
    metadata: &CrossFileMetadata,
    workspace_root: Option<&Url>,
) -> Option<Location>;

/// Extract the complete file path string at the cursor position
///
/// For source() calls: extracts the full string literal content
/// For directives: extracts the path after the directive keyword
///
/// # Returns
/// Some((path_string, context_type)) if cursor is on a file path
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
    label: String,              // File or directory name (e.g., "utils.R", "helpers/")
    kind: CompletionItemKind,   // FILE or FOLDER
    detail: Option<String>,     // Relative path from current file
    insert_text: Option<String>, // Path to insert (with trailing / for directories)
    sort_text: Option<String>,  // For ordering (directories first, then files alphabetically)
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

**Critical Path Resolution Rules** (from AGENTS.md):

| Context Type | PathContext Constructor | @lsp-cd Behavior |
|--------------|------------------------|------------------|
| source() calls | `PathContext::from_metadata()` | Respects @lsp-cd |
| LSP directives | `PathContext::new()` | Ignores @lsp-cd |

This distinction is essential for correct behavior:
- `source("utils.R")` resolves from @lsp-cd directory if set
- `# @lsp-sourced-by ../parent.R` always resolves from file's directory

### Directory Entry

```rust
struct DirEntry {
    name: String,       // File or directory name
    path: PathBuf,      // Full path
    is_directory: bool, // Whether this is a directory
}
```


## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

### Property 1: Source Call Context Detection

*For any* R code containing a `source()` or `sys.source()` call with a string literal argument, and *for any* cursor position inside that string literal (between the opening quote and closing quote), the context detector SHALL return a `SourceCall` context with the correct partial path extracted from the string start to cursor position.

**Validates: Requirements 1.1, 1.2**

### Property 2: Backward Directive Context Detection

*For any* R comment containing an `@lsp-sourced-by`, `@lsp-run-by`, or `@lsp-included-by` directive (with or without colon, with or without quotes), and *for any* cursor position after the directive keyword where a path is expected, the context detector SHALL return a `Directive` context with `DirectiveType::SourcedBy` and the correct partial path.

**Validates: Requirements 1.3, 1.4, 1.5**

### Property 3: Forward Directive Context Detection

*For any* R comment containing an `@lsp-source` directive (with or without colon, with or without quotes), and *for any* cursor position after the directive keyword where a path is expected, the context detector SHALL return a `Directive` context with `DirectiveType::Source` and the correct partial path.

**Validates: Requirements 1.6**

### Property 4: Non-Source Function Exclusion

*For any* R code containing a function call that is NOT `source()` or `sys.source()` (e.g., `print()`, `read.csv()`, `library()`), and *for any* cursor position inside a string argument, the context detector SHALL return `FilePathContext::None`.

**Validates: Requirements 1.7**

### Property 5: R File and Directory Filtering

*For any* directory containing files with various extensions, the completion provider SHALL return only:
- Files with `.R` or `.r` extensions
- All directories (regardless of contents)
- No hidden files or directories (those starting with `.`)

**Validates: Requirements 2.1, 2.2, 2.7**

### Property 6: Partial Path Resolution

*For any* partial path prefix (e.g., `../`, `subdir/`, `./`), the completion provider SHALL resolve the base directory by joining the partial path with the appropriate base (file directory or @lsp-cd directory) and return completions from that resolved directory.

**Validates: Requirements 2.3**

### Property 7: Workspace-Root-Relative Paths

*For any* path starting with `/` in a file path context, the completion provider SHALL resolve the path relative to the workspace root (not the filesystem root), and return completions from that workspace-relative directory.

**Validates: Requirements 2.4**

### Property 8: Directory Completion Trailing Slash

*For any* directory entry in completion results, the `insert_text` field SHALL end with a forward slash `/` to enable continued path navigation.

**Validates: Requirements 2.6**

### Property 9: Path Separator Normalization

*For any* input path containing escaped backslashes (`\\`), the path resolver SHALL normalize them to forward slashes before resolution, treating `\\` equivalently to `/` for path component separation.

**Validates: Requirements 4.1, 4.2**

### Property 10: Output Path Separator Convention

*For any* completion item returned by the completion provider, the path separator used in `insert_text` and `label` SHALL be a forward slash `/` (R convention), regardless of the operating system.

**Validates: Requirements 4.3**

### Property 11: Source Call Go-to-Definition

*For any* `source()` or `sys.source()` call with a string literal path that resolves to an existing file within the workspace, go-to-definition SHALL return a `Location` pointing to that file at line 0, column 0. The path SHALL be resolved using `PathContext::from_metadata()` which respects @lsp-cd working directory.

**Validates: Requirements 5.1, 5.2, 5.4, 5.5**

### Property 12: Missing File Returns No Definition

*For any* file path (in source() call or directive) that does not resolve to an existing file, go-to-definition SHALL return `None` (no navigation occurs).

**Validates: Requirements 5.3**

### Property 13: Backward Directive Go-to-Definition

*For any* `@lsp-sourced-by`, `@lsp-run-by`, or `@lsp-included-by` directive with a path that resolves to an existing file, go-to-definition SHALL return a `Location` pointing to that file at line 0, column 0.

**Validates: Requirements 6.1, 6.2, 6.3**

### Property 14: Forward Directive Go-to-Definition

*For any* `@lsp-source` directive with a path that resolves to an existing file, go-to-definition SHALL return a `Location` pointing to that file at line 0, column 0.

**Validates: Requirements 6.4**

### Property 15: Backward Directives Ignore @lsp-cd

*For any* file containing both an `@lsp-cd` directive and a backward directive (`@lsp-sourced-by`, `@lsp-run-by`, or `@lsp-included-by`), the backward directive path SHALL be resolved using `PathContext::new()` (relative to the file's directory), NOT using the @lsp-cd working directory.

**Validates: Requirements 6.5**

### Property 16: Workspace Boundary Enforcement

*For any* path that would resolve to a location outside the workspace root (e.g., excessive `../` components), the completion provider SHALL NOT include that path in results, and go-to-definition SHALL return `None`.

**Validates: Requirements 7.2**

### Property 17: Invalid Character Handling

*For any* path containing invalid filesystem characters (null bytes, etc.), the completion provider SHALL return an empty list without throwing an error, and go-to-definition SHALL return `None` without throwing an error.

**Validates: Requirements 7.3**

### Property 18: Space Handling in Paths

*For any* quoted path containing spaces (e.g., `"path with spaces/file.R"`), the path resolver SHALL correctly parse and resolve the complete path including the spaces.

**Validates: Requirements 7.5**


## Error Handling

### Invalid Path Characters

When a path contains characters invalid for the filesystem (null bytes, etc.):
- Log a trace message indicating the invalid path
- Return empty completion list
- Return `None` for go-to-definition
- Do not propagate errors to the LSP client

### Path Resolution Failures

When path resolution fails (e.g., too many `../` components escaping workspace):
- Return empty completion list for completions
- Return `None` for go-to-definition
- Do not emit diagnostics (handled by existing missing file diagnostics)

### Filesystem Access Errors

When directory listing fails (permissions, I/O errors):
- Log a warning with the error details
- Return empty completion list
- Continue processing other requests

### Non-Existent Base Directory

When the base directory for completions doesn't exist:
- Return empty completion list
- This is expected behavior for incomplete paths being typed

### Malformed AST

When tree-sitter parsing produces an incomplete or error AST:
- Fall back to regex-based detection where possible
- Return `FilePathContext::None` if context cannot be determined
- Do not crash or propagate errors

## Testing Strategy

### Dual Testing Approach

This feature requires both unit tests and property-based tests:

- **Unit tests**: Verify specific examples, edge cases, and integration points
- **Property tests**: Verify universal properties across all valid inputs using proptest

### Property-Based Testing Configuration

- **Library**: proptest (already used in the codebase)
- **Minimum iterations**: 100 per property test
- **Tag format**: `Feature: file-path-intellisense, Property N: {property_text}`
- **Each correctness property MUST be implemented by a SINGLE property-based test**

### Test Categories

#### 1. Context Detection Tests

Property tests for:
- Source call string context detection (Property 1)
- Backward directive context detection (Property 2)
- Forward directive context detection (Property 3)
- Non-source function exclusion (Property 4)

Unit tests for:
- Edge cases: empty strings, nested quotes, escaped quotes
- Cursor at string boundaries (start, end, middle)
- Malformed source() calls (missing quotes, unclosed strings)
- Directive syntax variations (with/without colon, with/without quotes)

#### 2. Completion Tests

Property tests for:
- R file and directory filtering (Property 5)
- Partial path resolution (Property 6)
- Workspace-root-relative paths (Property 7)
- Directory trailing slash (Property 8)
- Path separator handling (Properties 9, 10)

Unit tests for:
- Empty directory
- Directory with no R files
- Deeply nested paths
- Hidden files and directories
- Case sensitivity of .R/.r extensions

#### 3. Go-to-Definition Tests

Property tests for:
- Source call navigation (Property 11)
- Missing file handling (Property 12)
- Backward directive navigation (Property 13)
- Forward directive navigation (Property 14)
- @lsp-cd isolation for directives (Property 15)

Unit tests for:
- Cursor at different positions within path string
- Paths with special characters
- Relative vs absolute paths
- Workspace-root-relative paths (starting with `/`)

#### 4. Edge Case Tests

Property tests for:
- Workspace boundary enforcement (Property 16)
- Invalid character handling (Property 17)
- Space handling in paths (Property 18)

Unit tests for:
- Empty workspace
- Empty path string
- Very long paths
- Unicode characters in paths
- Symlinks (platform-dependent)

### Test File Structure

```text
crates/raven/src/
├── file_path_intellisense.rs           # Main implementation
└── cross_file/
    └── property_tests.rs               # Add property tests here (existing file)
```

### Generator Strategies for Property Tests

```rust
/// Strategy for generating valid R file names
fn r_filename_strategy() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-z][a-z0-9_]{0,10}\\.(R|r)")
        .unwrap()
        .prop_filter("non-empty", |s| !s.is_empty())
}

/// Strategy for generating directory names (no extension)
fn dirname_strategy() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-z][a-z0-9_]{0,10}")
        .unwrap()
        .prop_filter("non-empty and not hidden", |s| !s.is_empty() && !s.starts_with('.'))
}

/// Strategy for generating relative paths
fn relative_path_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        r_filename_strategy(),
        (dirname_strategy(), r_filename_strategy())
            .prop_map(|(d, f)| format!("{}/{}", d, f)),
        r_filename_strategy().prop_map(|f| format!("../{}", f)),
        (dirname_strategy(), dirname_strategy(), r_filename_strategy())
            .prop_map(|(d1, d2, f)| format!("{}/{}/{}", d1, d2, f)),
    ]
}

/// Strategy for generating source() call R code with cursor position
fn source_call_with_cursor_strategy() -> impl Strategy<Value = (String, Position, String)> {
    relative_path_strategy().prop_flat_map(|path| {
        let path_len = path.len();
        (0..=path_len).prop_map(move |cursor_offset| {
            let code = format!("source(\"{}\")", path);
            let cursor_col = 8 + cursor_offset; // 8 = len("source(\"")
            let partial = path[..cursor_offset].to_string();
            (code, Position::new(0, cursor_col as u32), partial)
        })
    })
}

/// Strategy for generating directive comments with cursor position
fn directive_with_cursor_strategy() -> impl Strategy<Value = (String, Position, DirectiveType, String)> {
    let directive_names = prop_oneof![
        Just("@lsp-sourced-by"),
        Just("@lsp-run-by"),
        Just("@lsp-included-by"),
        Just("@lsp-source"),
    ];
    
    (directive_names, relative_path_strategy(), prop::bool::ANY, prop::bool::ANY)
        .prop_flat_map(|(directive, path, use_colon, use_quotes)| {
            let path_len = path.len();
            (0..=path_len).prop_map(move |cursor_offset| {
                let colon = if use_colon { ": " } else { " " };
                let (open_quote, close_quote) = if use_quotes { ("\"", "\"") } else { ("", "") };
                let code = format!("# {}{}{}{}{}", directive, colon, open_quote, path, close_quote);
                
                let directive_type = if directive == "@lsp-source" {
                    DirectiveType::Source
                } else {
                    DirectiveType::SourcedBy
                };
                
                // Calculate cursor column
                let prefix_len = 2 + directive.len() + colon.len() + open_quote.len();
                let cursor_col = prefix_len + cursor_offset;
                let partial = path[..cursor_offset].to_string();
                
                (code, Position::new(0, cursor_col as u32), directive_type, partial)
            })
        })
}
```
