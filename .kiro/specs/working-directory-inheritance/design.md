# Design Document: Working Directory Inheritance

## Overview

This design extends the cross-file awareness system to support working directory inheritance through backward directives. When a child file declares itself as being sourced by a parent file via `@lsp-sourced-by` (or synonyms), it should inherit the parent's effective working directory for resolving paths in its own `source()` calls.

The key insight is that backward directives establish a logical parent-child relationship, and the child should behave as if it were actually sourced by the parent at runtime—including inheriting the parent's working directory context.

### Current Behavior

Currently, working directory inheritance only happens during forward traversal in `scope.rs`:
1. When processing a `source()` call, the system creates a child `PathContext` using `child_context_for_source()`
2. The child context inherits the parent's effective working directory (unless `chdir=TRUE`)
3. This inheritance is used when resolving paths for `source()` calls within the child file

However, when a file uses backward directives (`@lsp-sourced-by`), this inheritance doesn't occur because:
1. Backward directives are processed in `dependency.rs` during graph building
2. The dependency graph only stores edge information, not working directory context
3. When the child file's metadata is computed, it doesn't know about the parent's working directory

### Proposed Solution

Extend the metadata extraction and path resolution system to:
1. Store inherited working directory information in `CrossFileMetadata`
2. Resolve the parent's effective working directory when processing backward directives
3. Use the inherited working directory when building `PathContext` for path resolution

## Architecture

```mermaid
flowchart TD
    subgraph "Current Flow"
        A[Child File with @lsp-sourced-by] --> B[Parse Directives]
        B --> C[Build Dependency Graph]
        C --> D[Resolve source paths]
        D --> E[Use child's directory only]
    end
    
    subgraph "New Flow"
        A2[Child File with @lsp-sourced-by] --> B2[Parse Directives]
        B2 --> C2[Resolve Parent URI]
        C2 --> D2[Get Parent Metadata]
        D2 --> E2[Extract Parent's Effective WD]
        E2 --> F2[Store in Child Metadata]
        F2 --> G2[Build PathContext with Inherited WD]
        G2 --> H2[Resolve source paths correctly]
    end
```

### Data Flow

1. **Directive Parsing** (`directive.rs`): Parse backward directives as before
2. **Parent Resolution** (`dependency.rs`): Resolve parent URI from backward directive path
3. **Metadata Retrieval**: Get parent's metadata to determine its effective working directory
4. **Inheritance Storage**: Store inherited working directory in child's metadata
5. **Path Resolution** (`path_resolve.rs`): Use inherited working directory in `PathContext`

## Components and Interfaces

### Modified Types

#### CrossFileMetadata (types.rs)

Add a new field to store the inherited working directory:

```rust
/// Complete cross-file metadata for a document
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrossFileMetadata {
    /// Backward directives (this file is sourced by others)
    pub sourced_by: Vec<BackwardDirective>,
    /// Forward directives and detected source() calls
    pub sources: Vec<ForwardSource>,
    /// Working directory override (explicit @lsp-cd)
    pub working_directory: Option<String>,
    /// Working directory inherited from parent via backward directive
    pub inherited_working_directory: Option<String>,  // NEW
    /// Lines with @lsp-ignore (0-based)
    pub ignored_lines: HashSet<u32>,
    /// Lines following @lsp-ignore-next (0-based)
    pub ignored_next_lines: HashSet<u32>,
    /// Detected library(), require(), loadNamespace() calls
    pub library_calls: Vec<LibraryCall>,
}
```

### Modified Functions

#### PathContext::from_metadata (path_resolve.rs)

Update to use inherited working directory from metadata:

```rust
/// Create a context from a file URI and its metadata
pub fn from_metadata(
    file_uri: &Url,
    metadata: &CrossFileMetadata,
    workspace_root: Option<&Url>,
) -> Option<Self> {
    let mut ctx = Self::new(file_uri, workspace_root)?;

    // Apply explicit working directory from metadata if present
    if let Some(ref wd_path) = metadata.working_directory {
        ctx.working_directory = resolve_working_directory(wd_path, &ctx);
    }

    // Apply inherited working directory if no explicit one
    // NEW: Handle inherited_working_directory from backward directives
    if ctx.working_directory.is_none() {
        if let Some(ref inherited_wd) = metadata.inherited_working_directory {
            ctx.inherited_working_directory = resolve_working_directory(inherited_wd, &ctx);
        }
    }

    Some(ctx)
}
```

### New Functions

#### resolve_parent_working_directory (dependency.rs or new module)

```rust
/// Resolve the effective working directory of a parent file for inheritance.
/// 
/// Returns the parent's effective working directory as a string path that can
/// be stored in the child's metadata.
pub fn resolve_parent_working_directory<F>(
    parent_uri: &Url,
    get_metadata: F,
    workspace_root: Option<&Url>,
) -> Option<String>
where
    F: Fn(&Url) -> Option<CrossFileMetadata>,
{
    // Get parent's metadata
    let parent_meta = get_metadata(parent_uri)?;
    
    // Build parent's PathContext
    let parent_ctx = PathContext::from_metadata(parent_uri, &parent_meta, workspace_root)?;
    
    // Get effective working directory
    let effective_wd = parent_ctx.effective_working_directory();
    
    // Convert to string for storage
    Some(effective_wd.to_string_lossy().to_string())
}
```

#### compute_inherited_working_directory (new function in dependency.rs)

```rust
/// Compute the inherited working directory for a file based on its backward directives.
/// 
/// Uses the first backward directive's parent to determine inheritance.
/// Returns None if no backward directives exist or parent metadata unavailable.
pub fn compute_inherited_working_directory<F>(
    uri: &Url,
    meta: &CrossFileMetadata,
    workspace_root: Option<&Url>,
    get_metadata: F,
) -> Option<String>
where
    F: Fn(&Url) -> Option<CrossFileMetadata>,
{
    // Skip if file has explicit working directory
    if meta.working_directory.is_some() {
        return None;
    }
    
    // Get first backward directive (document order)
    let first_directive = meta.sourced_by.first()?;
    
    // Resolve parent URI (using file-relative resolution, not @lsp-cd)
    let backward_ctx = PathContext::new(uri, workspace_root)?;
    let parent_path = resolve_path(&first_directive.path, &backward_ctx)?;
    let parent_uri = path_to_uri(&parent_path)?;
    
    // Get parent's effective working directory
    resolve_parent_working_directory(&parent_uri, get_metadata, workspace_root)
}
```

### Integration Points

#### handlers.rs - Metadata Extraction

Update the metadata extraction flow to compute inherited working directory:

```rust
// After parsing directives, compute inherited working directory
if !metadata.sourced_by.is_empty() && metadata.working_directory.is_none() {
    metadata.inherited_working_directory = compute_inherited_working_directory(
        &uri,
        &metadata,
        workspace_root,
        |u| get_metadata_for_uri(u),
    );
}
```

#### revalidation.rs - Cache Invalidation

When a parent's working directory changes, invalidate child caches:

```rust
// In revalidation logic
if parent_wd_changed {
    // Find all children with backward directives to this parent
    let children = graph.get_dependents(&parent_uri);
    for child_edge in children {
        if child_edge.is_backward_directive {
            // Invalidate child's metadata cache
            invalidate_metadata_cache(&child_edge.to);
        }
    }
}
```

## Data Models

### Inheritance Resolution Order

The effective working directory is determined by this priority:

1. **Explicit** (`@lsp-cd` in the file itself) - highest priority
2. **Inherited** (from parent via backward directive)
3. **Default** (file's own directory) - lowest priority

### Storage Format

The inherited working directory is stored as a string path in `CrossFileMetadata`:
- Absolute paths are stored as-is
- Workspace-relative paths are resolved before storage
- The stored path is always absolute for consistent resolution

### Transitive Inheritance

For chains like A → B → C (where → means "sources via backward directive"):
1. When computing B's metadata, inherit from A
2. When computing C's metadata, inherit from B (which already has A's WD)
3. This naturally handles transitive inheritance without special logic

## Error Handling

### Parent File Not Found

When the parent file specified in a backward directive cannot be found:
- Log a warning (existing behavior)
- Do not set inherited working directory
- Fall back to file's own directory for path resolution

### Parent Metadata Unavailable

When parent metadata cannot be retrieved (file not indexed):
- Use parent's directory as the inherited working directory
- This provides reasonable default behavior

### Circular Dependencies

When a cycle is detected in backward directive chains:
- Stop inheritance at the cycle point
- Use the file's own directory
- Log a trace message for debugging

## Testing Strategy

### Unit Tests

1. **PathContext inheritance tests** (`path_resolve.rs`)
   - Test `from_metadata` with `inherited_working_directory` set
   - Test priority: explicit > inherited > default
   - Test workspace-relative path resolution

2. **Metadata computation tests** (`dependency.rs`)
   - Test `compute_inherited_working_directory` with various scenarios
   - Test first-directive-wins behavior for multiple backward directives
   - Test skip when explicit `@lsp-cd` present

3. **Integration tests** (`integration_tests.rs`)
   - Test end-to-end path resolution with backward directives
   - Test cache invalidation when parent WD changes

### Property-Based Tests

Property tests will validate the core invariants of working directory inheritance across randomly generated file structures and directive configurations.



## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

Based on the prework analysis, the following properties have been identified and consolidated to eliminate redundancy:

### Property 1: Parent Effective Working Directory Inheritance

*For any* child file with a backward directive pointing to a parent file, when the child has no explicit `@lsp-cd` directive, the child's inherited working directory SHALL equal the parent's effective working directory (whether the parent has an explicit `@lsp-cd` or uses its own directory as default).

**Validates: Requirements 1.1, 2.1, 2.2**

### Property 2: Path Resolution Uses Inherited Working Directory

*For any* `source()` call path in a child file that has inherited a working directory from a parent, resolving that path SHALL produce the same result as if the path were resolved from the parent's effective working directory.

**Validates: Requirements 1.2, 1.3**

### Property 3: Explicit Working Directory Precedence

*For any* child file that has both a backward directive AND its own explicit `@lsp-cd` directive, the effective working directory for path resolution SHALL equal the child's explicit working directory, ignoring any inherited working directory from the parent.

**Validates: Requirements 3.1, 3.2**

### Property 4: Backward Directive Paths Ignore Working Directory

*For any* backward directive path (e.g., `@lsp-sourced-by: ../parent.R`), resolving that path SHALL always be relative to the child file's directory, regardless of any explicit `@lsp-cd` or inherited working directory settings.

**Validates: Requirements 4.1, 4.2, 4.3**

### Property 5: Fallback When Parent Metadata Unavailable

*For any* child file with a backward directive where the parent file's metadata cannot be retrieved, the system SHALL use the parent file's directory as the inherited working directory.

**Validates: Requirements 5.3**

### Property 6: Metadata and PathContext Round-Trip

*For any* `CrossFileMetadata` with an `inherited_working_directory` field set, constructing a `PathContext` from that metadata and then computing the effective working directory SHALL return the inherited working directory (when no explicit working directory is set).

**Validates: Requirements 6.1, 6.2, 6.3**

### Property 7: First Backward Directive Wins

*For any* child file with multiple backward directives pointing to different parent files with different effective working directories, the inherited working directory SHALL equal the first parent's (in document order) effective working directory.

**Validates: Requirements 7.1, 7.2**

### Property 8: Transitive Inheritance

*For any* chain of files A → B → C connected by backward directives (where → means "is sourced by"), if only A has an explicit `@lsp-cd` and B and C have none, then C's inherited working directory SHALL equal A's explicit working directory.

**Validates: Requirements 9.1**

### Property 9: Depth Limiting

*For any* chain of backward directives exceeding `max_chain_depth`, the system SHALL stop inheritance at the depth limit and use the file's own directory for files beyond the limit.

**Validates: Requirements 9.2**

### Property 10: Cycle Handling

*For any* cycle in backward directive relationships (e.g., A → B → A), the system SHALL detect the cycle, stop inheritance at the cycle point, and use the file's own directory as the effective working directory.

**Validates: Requirements 9.3**

## Error Handling Summary

### Error Categories

| Error Type | Handling Strategy | User Feedback |
| ---------- | ----------------- | ------------- |
| Parent file not found | Log warning, skip inheritance | Diagnostic on backward directive line |
| Parent metadata unavailable | Use parent's directory as fallback | None (silent fallback) |
| Circular dependency | Stop at cycle, use file's directory | Trace log for debugging |
| Invalid path in @lsp-cd | Log warning, ignore directive | Diagnostic on @lsp-cd line |
| Workspace root unavailable | Skip workspace-relative resolution | Warning if workspace-relative path used |

### Graceful Degradation

The system follows a graceful degradation approach:
1. If inheritance fails, fall back to file's own directory
2. If path resolution fails, skip that source() call
3. If metadata retrieval fails, continue with available data

This ensures the LSP remains functional even when some cross-file features cannot be fully resolved.

## Testing Strategy

### Dual Testing Approach

Both unit tests and property-based tests are required for comprehensive coverage:

- **Unit tests**: Verify specific examples, edge cases, and error conditions
- **Property tests**: Verify universal properties across all valid inputs

### Property-Based Testing Configuration

- **Library**: `proptest` (already used in the codebase)
- **Minimum iterations**: 100 per property test
- **Tag format**: `Feature: working-directory-inheritance, Property N: {property_text}`

### Test Categories

#### Unit Tests (Specific Examples)

1. Basic inheritance: Parent with `@lsp-cd: /data`, child inherits `/data`
2. Implicit inheritance: Parent without `@lsp-cd`, child inherits parent's directory
3. Precedence: Child with own `@lsp-cd` ignores parent's WD
4. Backward directive resolution: Always relative to file's directory
5. Multiple backward directives: First one wins
6. Cache invalidation: Parent WD change triggers child recomputation

#### Property Tests (Universal Properties)

Each correctness property (1-10) will have a corresponding property-based test that:
1. Generates random file structures and directive configurations
2. Verifies the property holds for all generated inputs
3. Uses shrinking to find minimal failing examples if violations occur

### Test File Organization

```text
crates/raven/src/cross_file/
├── path_resolve.rs          # Unit tests for PathContext inheritance
├── dependency.rs            # Unit tests for compute_inherited_working_directory
├── property_tests.rs        # Property tests for all 10 properties
└── integration_tests.rs     # End-to-end tests for inheritance scenarios
```

### Example Property Test Structure

```rust
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: working-directory-inheritance, Property 1: Parent Effective Working Directory Inheritance
    #[test]
    fn prop_parent_wd_inheritance(
        parent_dir in "[a-z]{3,8}",
        child_dir in "[a-z]{3,8}",
        parent_has_explicit_wd in prop::bool::ANY,
        explicit_wd in "[a-z]{3,8}",
    ) {
        // Setup parent and child files
        // Verify inheritance behavior
        // Assert property holds
    }
}
```
