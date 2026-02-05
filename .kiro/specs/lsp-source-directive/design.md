# Design Document: @lsp-source Forward Directive

## Overview

This document describes the design for adding the `@lsp-source` forward directive to Raven, the R Language Server. The `@lsp-source` directive allows developers to explicitly declare that a file sources another file, complementing the existing backward directives (`@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`).

Forward directives are useful when:
- `source()` calls use dynamic paths (variables, expressions)
- `source()` calls are conditional (inside if statements)
- The relationship needs to be declared at a specific line for position-aware scope

### Key Design Decisions

1. **Forward directives use @lsp-cd**: Unlike backward directives which always resolve relative to the file's directory, forward directives respect the `@lsp-cd` working directory. This is because forward directives are semantically equivalent to `source()` calls and describe runtime execution behavior.

2. **Directive wins at same call site**: When both a directive and an AST-detected `source()` call point to the same file at the same call site, the directive takes precedence.

3. **Both edges kept at different call sites**: When a directive and `source()` call point to the same file but at different lines, both edges are preserved (symbols become available at the earliest call site).

## Architecture

The implementation extends the existing cross-file awareness system with minimal changes:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Cross-File Architecture                       │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐       │
│  │  directive.rs │───▶│ dependency.rs│───▶│   scope.rs   │       │
│  │              │    │              │    │              │       │
│  │ Parse @lsp-  │    │ Build edges  │    │ Resolve      │       │
│  │ source/run/  │    │ with conflict│    │ symbols at   │       │
│  │ include      │    │ resolution   │    │ position     │       │
│  └──────────────┘    └──────────────┘    └──────────────┘       │
│         │                   │                   │                │
│         ▼                   ▼                   ▼                │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐       │
│  │path_resolve.rs│    │ handlers.rs  │    │ completions  │       │
│  │              │    │              │    │ hover, goto  │       │
│  │ Resolve paths│    │ Diagnostics  │    │ definition   │       │
│  │ with @lsp-cd │    │ for missing  │    │              │       │
│  └──────────────┘    │ files        │    └──────────────┘       │
│                      └──────────────┘                           │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Components and Interfaces

### 1. Directive Parser (`directive.rs`)

**Current State**: Already parses `@lsp-source` with basic syntax support.

**Changes Required**: 
- Add `@lsp-run` and `@lsp-include` as synonyms
- Add `line=N` parameter parsing for forward directives

#### Updated Regex Pattern

```rust
// Current pattern (basic)
forward: Regex::new(
    r#"#\s*@?lsp-source\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))"#
).unwrap(),

// New pattern (with synonyms and line parameter)
forward: Regex::new(
    r#"#\s*@?lsp-(?:source|run|include)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))(?:\s+line\s*=\s*(\d+))?"#
).unwrap(),
```

#### ForwardSource Structure (existing)

```rust
pub struct ForwardSource {
    pub path: String,
    pub line: u32,           // 0-based line of directive or line=N value
    pub column: u32,         // Always 0 for directives
    pub is_directive: bool,  // true for @lsp-source
    pub local: bool,         // false for directives
    pub chdir: bool,         // false for directives
    pub is_sys_source: bool, // false for directives
    pub sys_source_global_env: bool, // true for directives
}
```

### 2. Path Resolution (`path_resolve.rs`)

**Current State**: Already supports working directory context via `PathContext`.

**Key Behavior**: Forward directives (`@lsp-source`) use `PathContext::from_metadata()` which includes the working directory from `@lsp-cd`. This differs from backward directives which use `PathContext::new()` (no working directory).

```rust
// For forward directives (uses @lsp-cd)
let path_ctx = PathContext::from_metadata(uri, meta, workspace_root)?;
let resolved = resolve_path(&source.path, &path_ctx)?;

// For backward directives (ignores @lsp-cd)
let backward_ctx = PathContext::new(uri, workspace_root)?;
let resolved = resolve_path(&directive.path, &backward_ctx)?;
```

### 3. Dependency Graph (`dependency.rs`)

**Current State**: Already processes forward sources in `update_file()`.

**Changes Required**: 
- Handle `line=N` parameter for forward directives
- Ensure directive-vs-AST conflict resolution works correctly

#### Conflict Resolution Logic

```
┌─────────────────────────────────────────────────────────────────┐
│              Directive-vs-AST Conflict Resolution                │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Case 1: Same file, same call site                              │
│  ┌─────────────────┐                                            │
│  │ @lsp-source a.R │ line 5                                     │
│  │ source("a.R")   │ line 5                                     │
│  └─────────────────┘                                            │
│  Result: Keep directive edge only (directive wins)              │
│                                                                  │
│  Case 2: Same file, different call sites                        │
│  ┌─────────────────┐                                            │
│  │ @lsp-source a.R │ line 10                                    │
│  │ source("a.R")   │ line 5                                     │
│  └─────────────────┘                                            │
│  Result: Keep both edges (symbols available at line 5)          │
│                                                                  │
│  Case 3: Directive without line=, AST at earlier line           │
│  ┌─────────────────┐                                            │
│  │ source("a.R")   │ line 5                                     │
│  │ @lsp-source a.R │ line 10 (no line= param)                   │
│  └─────────────────┘                                            │
│  Result: Keep AST edge, emit optional redundancy note           │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### 4. Scope Resolution (`scope.rs`)

**Current State**: Already handles `ForwardSource` entries in timeline events.

**No Changes Required**: The existing scope resolution logic treats all `ForwardSource` entries uniformly, making symbols available after the call site line.

### 5. Diagnostics (`handlers.rs`)

**Changes Required**:
- Emit warning for missing files referenced by `@lsp-source`
- Optionally emit note for redundant directives

#### Diagnostic Types

| Condition | Severity | Message |
|-----------|----------|---------|
| File not found | Warning (configurable) | "File 'path.R' referenced by @lsp-source directive not found" |
| Redundant directive | Hint (optional) | "Directive is redundant: source() call to same file exists at earlier line" |

## Data Models

### CrossFileMetadata (existing)

```rust
pub struct CrossFileMetadata {
    pub sourced_by: Vec<BackwardDirective>,  // Backward directives
    pub sources: Vec<ForwardSource>,          // Forward directives + AST source() calls
    pub working_directory: Option<String>,    // @lsp-cd
    pub inherited_working_directory: Option<String>,
    pub ignored_lines: HashSet<u32>,
    pub ignored_next_lines: HashSet<u32>,
    pub library_calls: Vec<LibraryCall>,
}
```

### DependencyEdge (existing)

```rust
pub struct DependencyEdge {
    pub from: Url,                    // Parent file
    pub to: Url,                      // Child file
    pub call_site_line: Option<u32>,  // 0-based line
    pub call_site_column: Option<u32>,// 0-based UTF-16 column
    pub local: bool,
    pub chdir: bool,
    pub is_sys_source: bool,
    pub is_directive: bool,           // true for @lsp-source
    pub is_backward_directive: bool,  // false for @lsp-source
}
```

## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

### Property 1: Forward Directive Parsing Completeness

*For any* valid forward directive syntax variation (with/without @, with/without colon, with single/double/no quotes, with/without line= parameter), the Directive_Parser SHALL produce a ForwardSource entry with the correct path and `is_directive=true`.

**Validates: Requirements 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8, 1.9**

### Property 2: Synonym Equivalence

*For any* path string, parsing `@lsp-source <path>`, `@lsp-run <path>`, and `@lsp-include <path>` SHALL produce identical ForwardSource entries (same path, same flags).

**Validates: Requirements 1.2, 1.3**

### Property 3: Call-Site Line Conversion

*For any* forward directive with `line=N` parameter where N is a positive integer, the resulting ForwardSource SHALL have `line = N - 1` (converting from 1-based user input to 0-based internal representation).

**Validates: Requirements 2.1**

### Property 4: Default Call-Site Assignment

*For any* forward directive without a `line=` parameter appearing at line L (0-based), the resulting ForwardSource SHALL have `line = L`.

**Validates: Requirements 2.2**

### Property 5: Multiple Directive Independence

*For any* file containing N forward directives, the Directive_Parser SHALL produce exactly N ForwardSource entries, each with the correct path and line.

**Validates: Requirements 2.3**

### Property 6: Forward Directive Uses Working Directory

*For any* file with both `@lsp-cd <wd>` and `@lsp-source <path>` directives, the path resolution for the forward directive SHALL use the working directory `<wd>` as the base for relative path resolution.

**Validates: Requirements 3.4**

### Property 7: Directive Edge Creation

*For any* forward directive pointing to an existing file, the Dependency_Graph SHALL contain an edge with `is_directive=true` and `is_backward_directive=false`.

**Validates: Requirements 4.1, 4.2**

### Property 8: Same Call-Site Conflict Resolution

*For any* file containing both a forward directive and a `source()` call pointing to the same target file at the same line, the Dependency_Graph SHALL contain exactly one edge (the directive edge).

**Validates: Requirements 4.3**

### Property 9: Different Call-Site Preservation

*For any* file containing both a forward directive at line A and a `source()` call at line B (where A ≠ B) pointing to the same target file, the Dependency_Graph SHALL contain edges for both call sites.

**Validates: Requirements 4.4**

### Property 10: Scope Availability After Directive

*For any* file with `@lsp-source <path>` at line L, symbols from the sourced file SHALL be available in scope at positions after line L.

**Validates: Requirements 5.1, 5.2**

### Property 11: Missing File Diagnostic

*For any* forward directive referencing a non-existent file, the system SHALL emit a diagnostic at the directive's line.

**Validates: Requirements 6.1**

### Property 12: Revalidation on Directive Change

*For any* change to a forward directive (add, remove, or modify), the dependency graph SHALL be updated to reflect the change.

**Validates: Requirements 7.1, 7.2, 7.3**

## Error Handling

### Missing File

When a forward directive references a file that doesn't exist:
1. No edge is created in the dependency graph
2. A diagnostic warning is emitted at the directive line
3. The diagnostic severity is configurable via `crossFile.missingFileSeverity`

### Invalid Path Syntax

When a path cannot be parsed (e.g., unmatched quotes):
1. The directive is ignored
2. No diagnostic is emitted (silent failure to avoid noise)

### Circular Dependencies

Circular dependencies are detected during scope resolution:
1. The cycle is broken at the point of detection
2. A warning diagnostic is emitted
3. Symbols up to the cycle point are still available

## Testing Strategy

### Unit Tests

Unit tests focus on specific examples and edge cases:

1. **Directive Parsing**
   - Parse each synonym (@lsp-source, @lsp-run, @lsp-include)
   - Parse with/without @ prefix
   - Parse with/without colon
   - Parse with single/double/no quotes
   - Parse with spaces in quoted paths
   - Parse with line=N parameter

2. **Path Resolution**
   - Relative paths with @lsp-cd
   - Workspace-root-relative paths
   - Non-existent files

3. **Conflict Resolution**
   - Directive and source() at same line
   - Directive and source() at different lines
   - Multiple directives to same file

### Property-Based Tests

Property tests verify universal properties across many generated inputs. Each test runs minimum 100 iterations.

**Tag format**: `Feature: lsp-source-directive, Property N: <property_text>`

1. **Property 1 Test**: Generate random valid directive syntax variations, verify parsing produces correct ForwardSource.

2. **Property 2 Test**: Generate random paths, verify all three synonyms produce identical results.

3. **Property 3 Test**: Generate random line numbers, verify 1-based to 0-based conversion.

4. **Property 6 Test**: Generate random working directories and relative paths, verify resolution uses working directory.

5. **Property 8 Test**: Generate files with directive and source() at same line, verify single edge.

6. **Property 9 Test**: Generate files with directive and source() at different lines, verify both edges.

### Integration Tests

1. **End-to-End Scope Resolution**: Verify symbols from @lsp-source files appear in completions.

2. **Diagnostic Generation**: Verify missing file warnings are emitted.

3. **Revalidation**: Verify adding/removing directives triggers appropriate updates.

## Implementation Notes

### Backward Compatibility

The existing `@lsp-source` directive parsing already works. This design adds:
- Synonym support (@lsp-run, @lsp-include)
- `line=N` parameter support
- Explicit documentation of @lsp-cd interaction

### Performance Considerations

- Directive parsing is O(n) where n is the number of lines
- Path resolution is O(1) per directive
- Dependency graph update is O(m) where m is the number of edges

### Configuration Options

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `crossFile.missingFileSeverity` | string | "warning" | Severity for missing file diagnostics |
| `crossFile.redundantDirectiveSeverity` | string | "hint" | Severity for redundant directive notes |
