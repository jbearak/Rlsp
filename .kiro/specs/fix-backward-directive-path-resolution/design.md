# Design Document: Fix Backward Directive Path Resolution

## Overview

This design addresses a bug in the `collect_ambiguous_parent_diagnostics` function where backward directive paths are incorrectly resolved using `PathContext::from_metadata` (which includes `@lsp-cd` working directory) instead of `PathContext::new` (which ignores `@lsp-cd`).

The fix is straightforward: change the PathContext construction to use `PathContext::new` for backward directive resolution, matching the pattern already established in `collect_missing_file_diagnostics` and other functions.

## Architecture

The cross-file awareness system uses two distinct path resolution strategies:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Path Resolution Strategy                      │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Forward Sources (source() calls)     Backward Directives        │
│  ─────────────────────────────────    ───────────────────────    │
│  PathContext::from_metadata           PathContext::new           │
│  - Respects @lsp-cd                   - Ignores @lsp-cd          │
│  - Runtime behavior modeling          - Static file relationships│
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Components and Interfaces

### PathContext

The `PathContext` struct in `crates/raven/src/cross_file/path_resolve.rs` provides two constructors:

```rust
// For backward directives - ignores @lsp-cd
pub fn new(file_uri: &Url, workspace_root: Option<&Url>) -> Option<Self>

// For forward sources - respects @lsp-cd from metadata
pub fn from_metadata(
    file_uri: &Url,
    metadata: &CrossFileMetadata,
    workspace_root: Option<&Url>,
) -> Option<Self>
```

### collect_ambiguous_parent_diagnostics (Current - Buggy)

```rust
fn collect_ambiguous_parent_diagnostics(
    state: &WorldState,
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // BUG: Uses from_metadata which includes @lsp-cd
    let path_ctx = crate::cross_file::path_resolve::PathContext::from_metadata(
        uri, meta, state.workspace_folders.first()
    );
    // ... rest of function uses path_ctx for backward directive resolution
}
```

### collect_ambiguous_parent_diagnostics (Fixed)

```rust
fn collect_ambiguous_parent_diagnostics(
    state: &WorldState,
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // FIX: Use PathContext::new which ignores @lsp-cd for backward directives
    let path_ctx = crate::cross_file::path_resolve::PathContext::new(
        uri, state.workspace_folders.first()
    );
    // ... rest of function unchanged
}
```

## Data Models

No changes to data models are required. The existing `PathContext` struct already supports both resolution strategies.

## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*



### Property 1: Backward directive path resolution ignores @lsp-cd

*For any* file with an `@lsp-cd` directive and any backward directive (`@lsp-run-by`, `@lsp-sourced-by`, `@lsp-included-by`), the backward directive path SHALL be resolved relative to the file's own directory, producing the same result as if `@lsp-cd` were not present.

**Validates: Requirements 1.2, 1.3, 3.1**

### Property 2: Forward source path resolution respects @lsp-cd

*For any* file with an `@lsp-cd` directive and any `source()` call, the source path SHALL be resolved relative to the working directory set by `@lsp-cd`, producing a different result than resolution relative to the file's directory (when @lsp-cd points elsewhere).

**Validates: Requirements 2.2**

## Error Handling

No new error handling is required. The fix changes which PathContext constructor is used but does not alter error handling behavior. Existing error handling for:
- Missing parent files
- Unresolvable paths
- Paths outside workspace

...remains unchanged.

## Testing Strategy

### Unit Tests

1. **Bug reproduction test**: Create a scenario with `@lsp-cd ..` and `@lsp-run-by: program.r` where the bug would cause incorrect resolution. Verify the fix produces correct resolution.

2. **Regression test**: Ensure forward source resolution still respects `@lsp-cd` after the fix.

### Property-Based Tests

Property-based tests should verify the path resolution invariants across many generated inputs:

1. **Property 1 test**: Generate random file URIs, @lsp-cd values, and backward directive paths. Verify that backward directive resolution produces the same result with and without @lsp-cd.

2. **Property 2 test**: Generate random file URIs, @lsp-cd values, and source() paths. Verify that source() resolution uses the @lsp-cd directory.

**Configuration**: Each property test should run minimum 100 iterations.

**Tag format**: `Feature: fix-backward-directive-path-resolution, Property N: {property_text}`
