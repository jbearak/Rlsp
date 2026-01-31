# Design: CodeRabbit PR Review Fixes

## Overview

This design document addresses the code quality issues, bug fixes, and documentation improvements identified by CodeRabbit during PR #2 review. The fixes span multiple modules including backend.rs, directive.rs, parent_resolve.rs, path_resolve.rs, handlers.rs, and state.rs, plus several markdown documentation files.

## Architecture

The fixes are organized into three categories:

1. **Code Fixes** (Requirements 1-8): Bug fixes and code quality improvements in Rust source files
2. **Documentation Fixes** (Requirements 9-11): Markdown formatting improvements

### Affected Components

```
crates/rlsp/src/
├── backend.rs          # Requirement 1: On-demand indexing flag check
├── handlers.rs         # Requirements 5, 6, 8: Diagnostics and dead code
├── state.rs            # Requirement 7: Comment accuracy
└── cross_file/
    ├── directive.rs    # Requirement 2: Quoted paths with spaces
    ├── parent_resolve.rs # Requirement 3: Child path fix
    └── path_resolve.rs # Requirement 4: ParentDir normalization

.kiro/specs/
├── cross-file-debugging/
│   ├── checkpoint-4-findings.md      # Requirement 9
│   ├── final-checkpoint-report.md    # Requirement 9
│   ├── task-10.2-findings.md         # Requirement 9
│   └── task-16.3-build-results.md    # Requirements 9, 10
└── priority-2-3-indexing/
    └── design.md                     # Requirement 11
```

## Components and Interfaces

### Component 1: On-Demand Indexing Flag Check (backend.rs)

**Current Issue**: The on-demand indexing code in `did_open()` checks individual priority flags but doesn't early-check the global `on_demand_indexing_enabled` flag.

**Solution**: Wrap all Priority 1, 2, and 3 indexing blocks in a conditional gated by the global flag.

```rust
// In did_open(), after collecting files_to_index:
if on_demand_enabled {
    // Priority 1: Synchronous indexing
    // Priority 2: Background indexing submission
    // Priority 3: Transitive dependency queuing
}
```

### Component 2: Directive Regex for Quoted Paths (directive.rs)

**Current Issue**: The regex patterns use `[^"'\s]+` which doesn't handle paths with spaces inside quotes.

**Solution**: Update regexes to use alternation for quoted vs unquoted paths:

```rust
// New pattern structure:
// (?:"([^"]+)"|'([^']+)'|([^\s]+))
// Group 1: double-quoted path (may contain spaces)
// Group 2: single-quoted path (may contain spaces)
// Group 3: unquoted path (no spaces)

fn capture_path(caps: &regex::Captures, base_group: usize) -> Option<String> {
    // Try double-quoted (base_group)
    if let Some(m) = caps.get(base_group) {
        return Some(m.as_str().to_string());
    }
    // Try single-quoted (base_group + 1)
    if let Some(m) = caps.get(base_group + 1) {
        return Some(m.as_str().to_string());
    }
    // Try unquoted (base_group + 2)
    if let Some(m) = caps.get(base_group + 2) {
        return Some(m.as_str().to_string());
    }
    None
}
```

### Component 3: Parent Resolution Child Path (parent_resolve.rs)

**Current Issue**: `resolve_match_pattern` and `infer_call_site_from_parent` receive `directive.path` (the parent path) when they need the child path.

**Solution**: Derive `child_path` from `child_uri` and pass it to the helper functions:

```rust
// In resolve_parent_with_content():
let child_path = child_uri.to_file_path()
    .ok()
    .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
    .unwrap_or_default();

// Use child_path instead of directive.path when calling:
resolve_match_pattern(&parent_content, pattern, &child_path)
infer_call_site_from_parent(&parent_content, &child_path)
```

### Component 4: Path Normalization Fix (path_resolve.rs)

**Current Issue**: `normalize_path` pops any previous component on ParentDir, including RootDir/Prefix.

**Solution**: Only pop when the prior component is a Normal segment:

```rust
fn normalize_path(path: &Path) -> Option<PathBuf> {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                // Only pop if the last component is a Normal segment
                if let Some(last) = components.last() {
                    if matches!(last, std::path::Component::Normal(_)) {
                        components.pop();
                    }
                }
                // Otherwise, keep the ParentDir or ignore it
            }
            std::path::Component::CurDir => {}
            c => components.push(c),
        }
    }

    if components.is_empty() {
        return None;
    }

    let mut result = PathBuf::new();
    for c in components {
        result.push(c);
    }
    Some(result)
}
```

### Component 5: Diagnostic Range Precision (handlers.rs)

**Current Issue**: The diagnostic range uses `source.column + 10` as an arbitrary offset.

**Solution**: Use the actual path length:

```rust
// Before:
end: Position::new(source.line, source.column + source.path.len() as u32 + 10),

// After:
end: Position::new(source.line, source.column + source.path.len() as u32),
```

### Component 6: Dead Code Removal (handlers.rs)

**Current Issue**: `collect_identifier_usages` is dead code; only the UTF-16 variant is used.

**Solution**: Remove the unused function entirely.

### Component 7: IndexEntry Comment Fix (state.rs)

**Current Issue**: Comment says "Will be updated when inserted" but `insert()` doesn't modify `indexed_at_version`.

**Solution**: Update the comment to accurately describe the behavior:

```rust
cross_file_entries.insert(uri, crate::cross_file::workspace_index::IndexEntry {
    snapshot,
    metadata: cross_file_meta,
    artifacts,
    indexed_at_version: 0, // Initial version; updated by workspace_index.insert() if needed
});
```

Or verify and fix the `insert()` method if it should update the version.

### Component 8: Non-Blocking File Existence Check (handlers.rs)

**Current Issue**: The `file_exists` closure performs blocking `path.exists()` on the LSP request thread.

**Solution**: For files not in cache, either:
- Skip the diagnostic (conservative approach)
- Queue an async check and emit diagnostic later
- Use a background task

Recommended approach: Skip the diagnostic for uncached files and rely on on-demand indexing to populate the cache:

```rust
let file_exists = |target_uri: &Url| -> bool {
    // Check caches first (fast path)
    if state.documents.contains_key(target_uri) { return true; }
    if state.workspace_index.contains_key(target_uri) { return true; }
    if state.cross_file_workspace_index.contains(target_uri) { return true; }
    if state.cross_file_file_cache.get(target_uri).is_some() { return true; }
    
    // Don't block on filesystem I/O - assume file exists if not in cache
    // On-demand indexing will populate the cache when the file is needed
    log::trace!("file_exists: {} not in cache, skipping blocking check", target_uri);
    true // Assume exists to avoid false positives
};
```

## Data Models

No new data models are introduced. Existing models are unchanged.

## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

### Property 1: On-Demand Indexing Respects Global Flag

*For any* document open event with `on_demand_indexing_enabled` set to false, no Priority 1, 2, or 3 indexing operations should be performed.

**Validates: Requirements 1.1, 1.2, 1.3, 1.4**

### Property 2: Quoted Path Extraction Preserves Spaces

*For any* directive containing a quoted path with spaces, parsing then extracting the path should preserve all characters including spaces.

**Validates: Requirements 2.1, 2.2, 2.3, 2.4**

### Property 3: Path Normalization Preserves Root

*For any* absolute path starting with root or prefix, normalizing with leading ParentDir components should preserve the root/prefix.

**Validates: Requirements 4.1, 4.2, 4.3**

### Property 4: Diagnostic Range Matches Path Length

*For any* source path diagnostic, the range end column should equal start column plus path length.

**Validates: Requirements 5.1, 5.2**

## Error Handling

- **Regex compilation failures**: Handled at initialization with `unwrap()` (patterns are compile-time constants)
- **Path resolution failures**: Logged with context, diagnostic skipped
- **File read failures**: Logged, indexing skipped for that file
- **Invalid URIs**: Logged, operation skipped

## Testing Strategy

### Unit Tests

1. **Directive parsing tests** (directive.rs):
   - Test quoted paths with spaces (double and single quotes)
   - Test unquoted paths (existing behavior preserved)
   - Test mixed scenarios

2. **Path normalization tests** (path_resolve.rs):
   - Test `/../a` produces `/a` not `a`
   - Test `/a/../b` produces `/b`
   - Test `a/../b` produces `b`

3. **Parent resolution tests** (parent_resolve.rs):
   - Test that child_path is correctly derived and used

### Integration Tests

1. **On-demand indexing flag test**:
   - Verify no indexing when flag is disabled
   - Verify indexing works when flag is enabled

### Property Tests

Property-based tests should be written for:
- Quoted path extraction (Property 2)
- Path normalization (Property 3)

Each property test should run minimum 100 iterations.

**Tag format**: Feature: coderabbit-pr-review-fixes, Property {number}: {property_text}

## Notes

- Documentation fixes (Requirements 9-11) are straightforward text edits
- The non-blocking file check (Requirement 8) uses a conservative approach to avoid false positive diagnostics
- All code changes maintain backward compatibility
