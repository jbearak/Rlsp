# AGENTS.md - LLM Guidance for Rlsp

## Project Overview

Rlsp is a static R Language Server extracted from Ark. It provides LSP features without embedding R runtime. Uses tree-sitter for parsing, subprocess calls for help.

## Repository Structure

- `crates/rlsp/`: Main LSP implementation
- `crates/rlsp/src/cross_file/`: Cross-file awareness module
- `editors/vscode/`: VS Code extension
- `Cargo.toml`: Workspace root
- `setup.sh`: Build and install script

## Build Commands

- `cargo build -p rlsp` - Debug build
- `cargo build --release -p rlsp` - Release build
- `cargo test -p rlsp` - Run tests
- `./setup.sh` - Build and install everything

## LSP Architecture

- Static analysis using tree-sitter-r
- Workspace symbol indexing (functions, variables)
- Package awareness (library() calls, NAMESPACE)
- Help via R subprocess (tools::Rd2txt)
- Thread-safe caching (RwLock)
- Cross-file awareness via source() detection and directives

## Cross-File Architecture

### Overview

Cross-file awareness enables Rlsp to understand symbol definitions and relationships across multiple R files connected via `source()` calls and LSP directives. This allows:

- **Symbol resolution**: Functions and variables from sourced files appear in completions, hover, and go-to-definition
- **Diagnostics suppression**: Symbols from sourced files are not marked as "undefined variable"
- **Dependency tracking**: Changes to a file trigger revalidation of dependent files
- **Workspace indexing**: Closed files are indexed and their symbols are available

**Key Features**:
1. **Automatic detection**: Parses `source()` and `sys.source()` calls from R code
2. **Manual directives**: `@lsp-sourced-by`, `@lsp-run-by` for files not explicitly sourced
3. **Working directory support**: `@lsp-cd` directive affects source() path resolution
4. **Position-aware scope**: Symbols only available after their source() call
5. **Cycle detection**: Prevents infinite loops in circular dependencies
6. **Real-time updates**: Changes propagate to dependent files automatically

### Module Structure (`crates/rlsp/src/cross_file/`)

- `background_indexer.rs` - Background indexing queue for Priority 2/3 files
- `types.rs` - Core types (CrossFileMetadata, BackwardDirective, ForwardSource, CallSiteSpec)
- `directive.rs` - Directive parsing (@lsp-sourced-by, @lsp-source, etc.) with optional colon/quotes
- `source_detect.rs` - Tree-sitter based source() call detection with UTF-16 columns
- `path_resolve.rs` - Path resolution with working directory support
- `dependency.rs` - Dependency graph with directive-vs-AST conflict resolution
- `scope.rs` - Scope resolution and symbol extraction
- `config.rs` - Configuration options including severity settings
- `cache.rs` - Caching with interior mutability
- `parent_resolve.rs` - Parent resolution with match= and call-site inference
- `revalidation.rs` - Real-time update system with debouncing
- `workspace_index.rs` - Workspace indexing for closed files
- `file_cache.rs` - Disk file cache with snapshots
- `content_provider.rs` - Unified content provider

### Directive Syntax

All directives support optional colon and quotes:
- `# @lsp-sourced-by ../main.R`
- `# @lsp-sourced-by: ../main.R`
- `# @lsp-sourced-by "../main.R"`
- `# @lsp-sourced-by: "../main.R"`

Backward directive synonyms: `@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`
Working directory synonyms: `@lsp-working-directory`, `@lsp-working-dir`, `@lsp-current-directory`, `@lsp-current-dir`, `@lsp-wd`, `@lsp-cd`

### Path Resolution: Critical Distinction

**IMPORTANT**: Path resolution behaves differently for LSP directives vs. source() statements:

#### LSP Directives (Backward/Forward)
**Always resolve relative to the file's directory, ignoring @lsp-cd:**
- `@lsp-sourced-by: ../parent.R` - Resolved from file's directory
- `@lsp-run-by: ../parent.R` - Resolved from file's directory
- `@lsp-source: utils.R` - Resolved from file's directory
- **Rationale**: Directives describe static file relationships that should not change based on runtime working directory

**Implementation**:
- Uses `PathContext::new()` which excludes working_directory from metadata
- Applied in both `dependency.rs` (graph building) and `handlers.rs` (diagnostics)
- See `do_resolve_backward()` helper in `dependency.rs`

#### source() Statements (AST-detected)
**Resolve using @lsp-cd working directory when present:**
- `source("utils.R")` - Resolved from @lsp-cd directory if specified, else file's directory
- `source("../data.R")` - Resolved from @lsp-cd directory if specified, else file's directory
- **Rationale**: source() calls execute at runtime and are affected by working directory

**Implementation**:
- Uses `PathContext::from_metadata()` which includes working_directory from metadata
- Applied via `do_resolve()` helper in `dependency.rs`

#### Example Scenario
```r
# File: subdir/child.r
# @lsp-cd: /some/other/directory
# @lsp-run-by: ../parent.r

source("utils.r")
```

**Resolution behavior**:
- `@lsp-run-by: ../parent.r` → Resolves to `parent.r` in workspace root (ignores @lsp-cd)
- `source("utils.r")` → Resolves to `/some/other/directory/utils.r` (uses @lsp-cd)

#### Path Types
1. **File-relative**: `utils.R`, `../parent.R`, `./data.R`
   - LSP directives: relative to file's directory
   - source() calls: relative to @lsp-cd or file's directory

2. **Workspace-root-relative**: `/data/utils.R`
   - Both: relative to workspace root (requires workspace_root parameter)

3. **Absolute**: `/absolute/path/to/file.R`
   - Both: used directly after canonicalization

#### PathContext Types
- `PathContext::new(uri, workspace_root)` - For LSP directives (no working_directory)
- `PathContext::from_metadata(uri, metadata, workspace_root)` - For source() calls (includes working_directory)

### Call-Site Resolution

1. `line=N` - Explicit line number (highest precedence)
2. `match="pattern"` - Pattern search in parent file
3. Text inference - Scan parent for source() calls to child
4. Config default - `assumeCallSite` setting ("end" or "start")

### Directive-vs-AST Conflict Resolution

- Directive with known call site: only overrides AST edge at same call site
- Directive without call site: suppresses all AST edges to that target (emits warning)
- AST edges to different targets are always preserved

### Dependency Graph

- Forward edges only (parent sources child)
- Backward directives create/confirm forward edges
- Edges store call site position (line, column in UTF-16)
- Stores local, chdir, is_sys_source flags
- Deduplication by canonical key

### Scope Resolution

- Position-aware: scope depends on (line, character) position
- Two-phase: compute per-file artifacts (non-recursive), then traverse
- Artifacts include: exported interface, timeline of scope events, interface hash
- Timeline contains: symbol definitions, source() calls, working directory changes
- Traversal bounded by max_chain_depth and visited set

### Caching Strategy

- Three caches with interior mutability: MetadataCache, ArtifactsCache, ParentSelectionCache
- Fingerprinted entries: self_hash, edges_hash, upstream_interfaces_hash, workspace_index_version
- Invalidation triggers: interface hash change OR edge set change

### Real-Time Updates

- Metadata extraction on document change
- Dependency graph update
- Selective invalidation based on interface/edge changes
- Debounced diagnostics fanout to affected open files
- Cancellation of outdated pending revalidations
- Freshness guards prevent stale diagnostic publishes
- Monotonic publishing: never publish older version than last published

### On-Demand Background Indexing

The BackgroundIndexer handles asynchronous indexing of files not currently open in the editor:

**Priority Levels**:
- Priority 1: Files directly sourced by open documents (synchronous, before diagnostics)
- Priority 2: Files referenced by backward directives (@lsp-run-by, @lsp-sourced-by)
- Priority 3: Transitive dependencies (files sourced by Priority 2 files)

**Architecture**:
- Single worker thread processes queue sequentially (avoids resource contention)
- Priority queue ensures important files indexed first
- Depth tracking prevents infinite transitive chains
- Duplicate detection avoids redundant work

**Configuration** (via `crossFile.onDemandIndexing.*`):
- `enabled`: Enable/disable on-demand indexing (default: true)
- `maxTransitiveDepth`: Maximum depth for transitive indexing (default: 2)
- `maxQueueSize`: Maximum queue size (default: 50)
- `priority2Enabled`: Enable Priority 2 indexing (default: true)
- `priority3Enabled`: Enable Priority 3 indexing (default: true)

**Flow**:
1. File opened with backward directive → Priority 2 task submitted
2. Worker processes task → reads file, extracts metadata, computes artifacts
3. Updates workspace index and dependency graph
4. Queues transitive dependencies as Priority 3 tasks (if depth allows)

## Learnings

- When building a new struct from `&T`, avoid `..*ref`/`..ref` in struct update syntax; clone the base (`..ref.clone()`) or construct fields explicitly.
- In tests that locate identifiers in generated code, avoid substring matches (e.g., `inner_func` inside `outer_func`). Prefer delimiter-aware search or node positions from the AST.
- Don’t recurse on identifier nodes just to “keep traversal going” — they have no children and the extra recursion can be removed for clarity.
- Avoid blocking filesystem I/O on LSP request threads; if a fallback check is needed, do it off-thread and revalidate via cache updates.
- Guard against deadlocks by avoiding nested async lock acquisition (especially around background indexer/state access).
- Use `saturating_add` (or equivalent) for sentinel end-of-line columns to prevent overflow.
- Don’t “paper over” missing files: file-existence checks must preserve accurate diagnostics instead of always returning `true`.
- Validate user-controlled package names/paths and skip suspicious values before using them in indexing or diagnostics.
- Avoid `expect()` in long-lived server paths (e.g., parser init); propagate errors with `Option`/`Result` instead.
- When adding new enum variants used in test match expressions, update all test helpers to keep matches exhaustive.
- Avoid hard-coded line/column positions in property tests; compute stable positions from the generated code or parsed nodes.
- When using `tokio::sync::watch`, guard waiters with in-flight state or revisions so late callers don’t hang on `changed()`.
- When using `u32::MAX` as an EOF sentinel, ensure function-scope filtering treats it as “outside any function” to avoid leaking locals.
- Distinguish “full EOF” (both line and column MAX) from end-of-line sentinels so scope filtering doesn’t drop function-local symbols at call sites.
- In proptest, prefer `prop_assume!` for invalid generated cases instead of returning early with non-unit values.
- Keep requirements docs aligned with the actual API surface (e.g., static construction vs insertion guarantees).
- Convert UTF-16 columns to byte offsets before constructing tree-sitter `Point`s; avoid mixing column units.
- When expanding point windows by a byte, advance to the next UTF-8 boundary to avoid mid-codepoint positions.
- Watch for O(n²) scope detection or duplicate-detection paths in large files; prefer indexed lookups when possible.
- Avoid duplicating local-scoping condition logic across functions; centralize to reduce drift.
- Be careful with hover/definition range calculations at line boundaries to avoid off-by-one bugs or invalid points.
- For removal events, use strict position comparisons (before, not at) to avoid removing symbols at their definition position.
- In hot scope-resolution paths, avoid repeated scans over large lists (e.g., function scopes per event); precompute mappings or cache lookups to prevent O(R·F) regressions.
- Keep doc comments and markdown examples aligned with current behavior (e.g., list= string literals support).
- Normalize markdown table spacing to match project lint expectations when adding spec tables.
- `tree_sitter::Tree` implements `Clone`; preserve ASTs in cloned index entries when reference searches depend on them.
- For intentionally-unused public APIs, either wire them into a caller or add a localized `#[allow(dead_code)]` with a brief comment to avoid warning noise.

### Rust/Clippy Best Practices

- Use `strip_prefix()` instead of manual `starts_with()` + slice indexing (e.g., `path[1..]`); clippy flags this as `manual_strip`.
- On `DoubleEndedIterator`s, use `next_back()` instead of `last()` to avoid iterating the entire collection.
- Name methods `as_*` (not `to_*`) when they return a cheap view/conversion on `Copy` types that take `self` by value.
- Name methods `as_*` (not `from_*`) when they convert `&self` to another type; `from_*` conventionally takes no `self`.
- Use `&Path` instead of `&PathBuf` in function parameters; `&PathBuf` coerces to `&Path` and avoids unnecessary type constraints.
- Use `split_once()` instead of `splitn(2, ...).nth(1)` for cleaner single-split operations.
- Prefer `for item in iter` over `while let Some(item) = iter.next()` unless you need to call other iterator methods mid-loop.
- When a function legitimately needs many parameters (e.g., recursive scope resolution), add `#[allow(clippy::too_many_arguments)]` rather than forcing awkward refactors.
- Mark test-only helper functions with `#[cfg(test)]` to avoid dead-code warnings in non-test builds.
- For struct fields that are set but not yet read (future use), add `#[allow(dead_code)]` with a comment explaining the intent.
- In doc comments, separate list items from following paragraphs with a blank line to avoid `doc_lazy_continuation` warnings.
- Run `cargo clippy` before committing to catch style issues early; many have auto-fix suggestions via `cargo clippy --fix`.

### Thread-Safety

- WorldState protected by Arc<tokio::sync::RwLock>
- Concurrent reads from request handlers
- Serialized writes for state mutations
- Interior-mutable caches allow population during read operations
- Background tasks reacquire locks, never hold borrowed &mut WorldState

### Common Issues and Debugging

#### "Parent file not found" Error
**Symptom**: Backward directive reports parent file not found despite file existing

**Common Causes**:
1. **Incorrect path resolution**: Ensure path is relative to file's directory, not @lsp-cd
2. **File not in workspace**: Parent file must be within workspace or accessible on disk
3. **Typo in path**: Check for correct use of `..` for parent directory navigation

**Debug Steps**:
1. Enable trace logging: `RUST_LOG=rlsp=trace`
2. Check logs for "Resolving path" messages in `path_resolve.rs`
3. Verify file exists at resolved canonical path
4. Ensure backward directive uses separate PathContext (without @lsp-cd)

#### Symbols Not Available from Sourced File
**Symptom**: Completions don't show functions from sourced files

**Common Causes**:
1. **Position before source() call**: Symbols only available after source() line
2. **Path resolution failed**: source() path doesn't resolve to actual file
3. **Cycle detected**: Circular dependencies stop traversal
4. **Max depth exceeded**: Chain longer than configured max_chain_depth

**Debug Steps**:
1. Check dependency graph: Look for edge from parent to child
2. Verify metadata extraction: Ensure source() call was detected
3. Check scope resolution logs: Verify traversal reaches sourced file
4. Test with simple two-file case to isolate issue

#### @lsp-cd Not Affecting Directives (Expected Behavior)
**Symptom**: Backward directive path resolution ignores @lsp-cd

**This is correct behavior**: LSP directives always resolve relative to file's directory, ignoring @lsp-cd. Only source() statements use @lsp-cd for path resolution.

**If you need @lsp-cd to affect a path**:
- Use `source()` call instead of directive
- Or specify absolute/workspace-relative path in directive

## VS Code Extension

- TypeScript client in `editors/vscode/src/`
- Bundles platform-specific rlsp binary
- Configuration: `rlsp.server.path`
- Sends activity notifications for revalidation prioritization

## Coding Style

- No `bail!`, use explicit `return Err(anyhow!(...))`
- Omit `return` in match expressions
- Direct formatting: `anyhow!("Message: {err}")`
- Use `log::trace!` instead of `log::debug!`
- Fully qualified result types

## Testing

Property-based tests with proptest, integration tests

## Built-in Functions

`build_builtins.R` generates `src/builtins.rs` with 2,355 R functions

## Release Process

Manual tagging (`git tag vX.Y.Z && git push origin vX.Y.Z`) triggers GitHub Actions

## Extension Guide

### Adding New Directives

1. **Define the directive type** in `types.rs`:
   - Add variant to existing enum or create new struct
   - Include all necessary fields (path, line, parameters)

2. **Parse the directive** in `directive.rs`:
   - Add regex pattern to `DIRECTIVE_PATTERNS` or create new pattern
   - Handle optional colon and quotes: `@name:? "?path"?`
   - Parse any parameters (e.g., `line=N`, `match="pattern"`)
   - Add to `parse_directives()` function

3. **Process in dependency graph** (`dependency.rs`):
   - Handle in `update_file()` method
   - Create appropriate `DependencyEdge` entries
   - Consider directive-vs-AST conflict resolution

4. **Update scope resolution** (`scope.rs`) if directive affects symbol availability

5. **Add tests**:
   - Unit tests in the module's `#[cfg(test)]` section
   - Property tests in `property_tests.rs` for invariants

### Extending Scope Resolution

1. **Modify `ScopeArtifacts`** in `scope.rs`:
   - Add new fields to track additional scope information
   - Update `compute_artifacts()` to populate new fields

2. **Update timeline events** (`ScopeEvent` enum):
   - Add new event types for scope-affecting constructs
   - Handle in `scope_at_position_*` functions

3. **Modify traversal** in `scope_at_position_with_graph_recursive()`:
   - Process new event types
   - Maintain correct symbol precedence (local > inherited)

4. **Update interface hash** if changes affect cross-file invalidation

### Adding Cross-File Diagnostics

1. **Define diagnostic type** in `handlers.rs`:
   - Create `collect_*_diagnostics()` function
   - Use `state.cross_file_config.*_severity` for configurable severity

2. **Add configuration** in `config.rs`:
   - Add severity field to `CrossFileConfig`
   - Add to `from_initialization_options()`

3. **Wire into diagnostics collection** in `handlers.rs`:
   - Call from `collect_diagnostics()` or `publish_diagnostics()`
   - Ensure proper position (line, column) in UTF-16

4. **Add tests** for diagnostic generation and severity configuration

### Testing Strategies

**Unit Tests:**
- Test individual functions in isolation
- Use `#[cfg(test)]` module at end of each file
- Mock dependencies with closures

**Property Tests** (`property_tests.rs`):
- Test invariants that must hold for all inputs
- Use `proptest!` macro with custom strategies
- Focus on: edge deduplication, scope precedence, path resolution

**Integration Tests:**
- Test full LSP request/response cycles
- Use `handlers::integration_tests` module
- Test with realistic R code patterns

**Test Patterns:**
```rust
// Unit test pattern
#[test]
fn test_specific_behavior() {
    let input = /* setup */;
    let result = function_under_test(input);
    assert_eq!(result, expected);
}

// Property test pattern
proptest! {
    #[test]
    fn prop_invariant_holds(input in strategy()) {
        let result = function_under_test(input);
        prop_assert!(invariant(result));
    }
}
```